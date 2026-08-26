//! `git lex query` — SPARQL over the working + history stores. Extracted
//! from main.rs (#39, task #92).

use std::time::Instant;
use std::io::Cursor;
use std::process::exit;
use oxigraph::io::RdfFormat;
use oxigraph::model::*;
use oxigraph::store::Store;
use crate::nquad::{generate_frontmatter_nquads, load_lex_nquads};
use git_lex::add_prefixes;

pub(crate) fn run_query(store: &Store, query: &str, store_type: &str, json: bool) {
    let start = Instant::now();
    let prefixed = add_prefixes(query);

    // Shared parse/execute with the deliberate union-default-graph
    // semantics (review #8): this surface explores the whole store. The
    // parse-vs-eval error identity survives for the JSON error object.
    let results = match git_lex::eval_query_union(store, &prefixed) {
        Ok(r) => r,
        Err(git_lex::W3cQueryError::Parse(e)) => {
            if json {
                eprintln!("{}", serde_json::json!({"error": "parse", "message": e}));
            } else {
                eprintln!("SPARQL parse error: {}", e);
            }
            exit(1);
        }
        Err(git_lex::W3cQueryError::Eval(e)) => {
            if json {
                eprintln!("{}", serde_json::json!({"error": "eval", "message": e}));
            } else {
                eprintln!("SPARQL evaluation error: {}", e);
            }
            exit(1);
        }
    };

    let mut count = 0;
    match results {
        oxigraph::sparql::QueryResults::Solutions(solutions) => {
            if json {
                // W3C envelope through the ONE shared assembler (review
                // #8) — the CLI's --json and the protocol endpoint emit
                // the same shape by construction.
                match git_lex::solutions_to_w3c_json(solutions) {
                    Ok(out) => {
                        count = out["results"]["bindings"]
                            .as_array()
                            .map(|b| b.len())
                            .unwrap_or(0);
                        println!("{}", serde_json::to_string(&out).unwrap());
                    }
                    Err(e) => {
                        eprintln!("{}", serde_json::json!({"error": "eval", "message": e}));
                        exit(1);
                    }
                }
            } else {
                let vars: Vec<String> = solutions
                    .variables()
                    .iter()
                    .map(|v| v.as_str().to_string())
                    .collect();
                let mut all_rows = Vec::new();
                for solution in solutions {
                    let solution = match solution {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("Error reading solution: {}", e);
                            continue;
                        }
                    };
                    count += 1;
                    let mut row = Vec::new();
                    for var in &vars {
                        let val = solution
                            .get(var.as_str())
                            .map(|t| match t {
                                Term::NamedNode(n) => n.as_str().to_string(),
                                Term::Literal(l) => l.value().to_string(),
                                Term::BlankNode(b) => format!("_:{}", b.as_str()),
                                Term::Triple(t) => format!("<< {} {} {} >>", t.subject, t.predicate, t.object),
                            })
                            .unwrap_or_default();
                        row.push(val);
                    }
                    all_rows.push(row);
                }

                if !all_rows.is_empty() {
                    // Compute column widths
                    let mut widths = vec![0; vars.len()];
                    for (i, var) in vars.iter().enumerate() {
                        widths[i] = var.len();
                    }
                    for row in &all_rows {
                        for (i, val) in row.iter().enumerate() {
                            if val.len() > widths[i] {
                                widths[i] = val.len();
                            }
                        }
                    }

                    // Print header
                    let mut header = String::new();
                    for (i, var) in vars.iter().enumerate() {
                        header.push_str(&format!(" {:width$} |", var, width = widths[i]));
                    }
                    println!("|{} \n|{}", header, "-".repeat(header.len().saturating_sub(1)));

                    // Print rows
                    for row in &all_rows {
                        let mut row_str = String::new();
                        for (i, val) in row.iter().enumerate() {
                            row_str.push_str(&format!(" {:width$} |", val, width = widths[i]));
                        }
                        println!("|{}", row_str);
                    }
                } else {
                    // (review #43: the old "run `git lex sync`" hint here
                    // was unreachable — this command always queries the
                    // live working-tree view — and wrong advice besides.)
                    println!("(No results found)");
                }
            }
        }
        oxigraph::sparql::QueryResults::Boolean(b) => {
            if json {
                println!("{}", serde_json::json!({"head": {}, "boolean": b}));
            } else {
                println!("{}", b);
            }
            count = 1;
        }
        oxigraph::sparql::QueryResults::Graph(_) => {
            if json {
                eprintln!("{}", serde_json::json!({
                    "error": "unsupported",
                    "message": "CONSTRUCT/DESCRIBE JSON output not yet supported"
                }));
                exit(1);
            }
            println!("CONSTRUCT/DESCRIBE queries not yet supported in output");
        }
    }

    let elapsed = start.elapsed();
    eprintln!(
        "\n{} results in {:.1}ms ({})",
        count,
        elapsed.as_secs_f64() * 1000.0,
        store_type
    );
}

/// Stored queries (Rob-ruled 2026-08-26): `git lex query <name>` runs the
/// query saved in `.lex/query/<name>.md`. A stored query is plain markdown
/// — prose anywhere in it is the details section, and the query itself is
/// the first fenced code block (or, with no fence, the whole body). One
/// door, the same one inline queries use. Returns None when the argument
/// is not a stored-query name, in which case it runs as SPARQL text.
fn resolve_stored_query(arg: &str) -> Option<String> {
    let root = git_lex::find_git_root()?;
    let path = root.join(".lex").join("query").join(format!("{}.md", arg));
    let md = std::fs::read_to_string(&path).ok()?;
    eprintln!("Stored query: .lex/query/{}.md", arg);
    Some(stored_query_text(&md))
}

/// Pure extraction of the query from a stored-query markdown file: skip
/// YAML frontmatter if present, then the FIRST fenced code block wins
/// (its info string — ```sparql or bare ``` — is ignored); a file with no
/// fence is all query. A parse error downstream names the file, so a
/// malformed stored query fails exactly like a malformed inline one.
fn stored_query_text(md: &str) -> String {
    // Strip frontmatter.
    let body = if let Some(rest) = md.strip_prefix("---\n") {
        match rest.find("\n---\n") {
            Some(i) => &rest[i + 5..],
            None => md,
        }
    } else {
        md
    };
    // First fenced block, if any.
    let mut in_fence = false;
    let mut fence_lines: Vec<&str> = Vec::new();
    for line in body.lines() {
        let t = line.trim_start();
        if t.starts_with("```") {
            if in_fence {
                return fence_lines.join("\n");
            }
            in_fence = true;
            continue;
        }
        if in_fence {
            fence_lines.push(line);
        }
    }
    if in_fence {
        // Unclosed fence: everything after the opener is the query.
        return fence_lines.join("\n");
    }
    body.trim().to_string()
}

/// The miss surface: the argument looked like a stored-query NAME (no
/// whitespace, no SPARQL braces) but no such file exists. Say what IS
/// available instead of handing the name to the SPARQL parser, whose
/// "parse error at 'recent'" would teach nothing.
fn stored_query_miss(arg: &str) -> bool {
    if arg.contains(char::is_whitespace) || arg.contains('{') {
        return false;
    }
    let Some(root) = git_lex::find_git_root() else { return false };
    let dir = root.join(".lex").join("query");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    name.strip_suffix(".md").map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    if names.is_empty() {
        eprintln!(
            "No stored query named '{}' (.lex/query/ is empty). Save one as \
             .lex/query/{}.md — the first code block in it is the query.",
            arg, arg
        );
    } else {
        eprintln!(
            "No stored query named '{}'. Available: {}",
            arg,
            names.join(", ")
        );
    }
    true
}

pub(crate) fn cmd_query(query: String, json: bool) {
    // Stored-query resolution first: a name that matches .lex/query/<name>.md
    // runs that file's query; anything else runs as SPARQL text. A name-like
    // miss gets the available list instead of a SPARQL parse error.
    let query = match resolve_stored_query(&query) {
        Some(q) => q,
        None => {
            if stored_query_miss(&query) {
                exit(1);
            }
            query
        }
    };
    // B2 FIX (w4r3z, Day 40): `query` now builds the "now" view from the WORKING
    // TREE every time, so the documented `create → save → query` flow surfaces a
    // doc's own frontmatter immediately — no `git lex sync` required first.
    //
    // The old code queried the persistent store first when it existed. But `save`
    // writes .spo sidecars WITHOUT recompiling the store (only `sync` does that),
    // so a fresh doc's facts were invisible until `sync` ran — the README's
    // headline query returned 0. (The in-memory fallback also missed them: it read
    // compiled .nq files, which `save` doesn't write either.)
    //
    // Fix: always extract the current working tree (git blobs + frontmatter) into a
    // fresh in-memory store. generate_frontmatter_nquads() reads the live .md files
    // directly — so this reflects exactly what's on disk now. The persistent store
    // remains a SYNC/HISTORY artifact (sync/<sha> graphs); the "now" view is always
    // derived fresh here, trading a little speed for a correct, surprise-free flow.
    let start = Instant::now();
    let store = Store::new().expect("failed to create in-memory store");

    let git_nq = crate::git2_nquads::generate_git2_nquads();
    let git_count = git_nq.lines().count();
    store
        .load_from_reader(RdfFormat::NQuads, Cursor::new(git_nq.as_bytes()))
        .expect("failed to load git triples");

    // The live "now" graph: extract frontmatter + markdown links straight
    // from the working-tree .md files (this is what `save` would extract,
    // computed fresh). write_sidecars is OFF — query is a READ command; it
    // used to rewrite the .spo sidecars as a side effect, dirtying the tree
    // from a question.
    let (fm_nq, _errs) = generate_frontmatter_nquads(crate::nquad::NowWalkOpts {
        write_sidecars: false,
        build_nquads: true,
    });
    let lex_count = fm_nq.lines().filter(|l| !l.is_empty()).count();
    if !fm_nq.is_empty() {
        store
            .load_from_reader(RdfFormat::NQuads, Cursor::new(fm_nq.as_bytes()))
            .expect("failed to load frontmatter triples");
    }

    // Also fold in any hand-authored `.lex/**/*.nq` files a user dropped in.
    // (`sync` does NOT write .nq — it writes the persistent oxigraph store;
    // this is purely for user-supplied static N-Quads.)
    let lex_nq = load_lex_nquads();
    if !lex_nq.is_empty() {
        store
            .load_from_reader(RdfFormat::NQuads, Cursor::new(lex_nq.as_bytes()))
            .expect("failed to load .lex/ triples");
    }

    let load_ms = start.elapsed().as_secs_f64() * 1000.0;
    run_query(
        &store,
        &query,
        &format!(
            "live working-tree view: {} git + {} frontmatter triples in {:.1}ms",
            git_count, lex_count, load_ms
        ),
        json,
    );
}

#[cfg(test)]
mod stored_query_tests {
    use super::stored_query_text;

    #[test]
    fn first_fence_wins_prose_is_details() {
        let md = "---\ntitle: x\n---\n\n# Recent things\n\nWhat changed lately.\n\n```sparql\nSELECT ?s WHERE { ?s ?p ?o }\n```\n\nMore notes below.\n\n```\nnot the query\n```\n";
        assert_eq!(stored_query_text(md), "SELECT ?s WHERE { ?s ?p ?o }");
    }

    #[test]
    fn bare_fence_label_is_fine() {
        let md = "```\nASK { ?s ?p ?o }\n```\n";
        assert_eq!(stored_query_text(md), "ASK { ?s ?p ?o }");
    }

    #[test]
    fn no_fence_means_whole_body_is_the_query() {
        let md = "---\nk: v\n---\nSELECT * WHERE { ?s ?p ?o } LIMIT 5\n";
        assert_eq!(stored_query_text(md), "SELECT * WHERE { ?s ?p ?o } LIMIT 5");
    }

    #[test]
    fn no_frontmatter_no_fence() {
        assert_eq!(stored_query_text("ASK { ?s ?p ?o }\n"), "ASK { ?s ?p ?o }");
    }

    #[test]
    fn unclosed_fence_takes_the_tail() {
        let md = "notes\n```sparql\nSELECT ?s WHERE { ?s ?p ?o }\n";
        assert_eq!(stored_query_text(md), "SELECT ?s WHERE { ?s ?p ?o }");
    }
}

/// Scaffold the default stored queries into `.lex/query/` — ONLY when the
/// folder does not exist yet. A folder that exists is the operator's,
/// whatever is or isn't in it; re-running init/kit-update never overwrites
/// or re-adds. (Soul-kit override — `Soul/Query/` replacing this folder
/// wholesale — is the kit's move, not built here.)
pub(crate) fn scaffold_default_queries(root: &std::path::Path) {
    let dir = root.join(".lex").join("query");
    if dir.exists() {
        return;
    }
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let starters: &[(&str, &str)] = &[
        (
            "things",
            "# Everything in this repo, by class\n\n\
             Counts every typed thing in the live view — documents, commits,\n\
             files, facts of every class. A quick shape-of-the-repo check.\n\n\
             ```sparql\n\
             SELECT ?class (COUNT(?s) AS ?count)\n\
             WHERE { ?s a ?class }\n\
             GROUP BY ?class\n\
             ORDER BY DESC(?count)\n\
             ```\n",
        ),
        (
            "recent",
            "# What changed lately\n\n\
             Documents by their last change, newest first. The date is\n\
             maintained by git-lex at commit time (dateUpdated) — documents\n\
             that predate the stamping appear once they are next saved.\n\n\
             ```sparql\n\
             SELECT ?doc ?date\n\
             WHERE { ?doc <https://repolex.ai/ontology/git-lex/dateUpdated> ?date }\n\
             ORDER BY DESC(?date)\n\
             LIMIT 20\n\
             ```\n",
        ),
    ];
    let mut written = 0;
    for (name, body) in starters {
        if std::fs::write(dir.join(format!("{}.md", name)), body).is_ok() {
            written += 1;
        }
    }
    if written > 0 {
        println!(
            "Stored queries: .lex/query/ created with {} starter(s) — run one \
             with `git lex query <name>`, add your own as .lex/query/<name>.md",
            written
        );
    }
}
