//! `git lex query` — SPARQL over the working + history stores. Extracted
//! from main.rs (#39, task #92).

use std::time::Instant;
use std::io::Cursor;
use git_lex::{Failure, Outcome};
use oxigraph::io::RdfFormat;
use oxigraph::model::*;
use oxigraph::store::Store;
use git_lex::nquad::{generate_frontmatter_nquads, load_lex_nquads};
use git_lex::add_prefixes;

pub(crate) fn run_query(store: &Store, query: &str, store_type: &str, json: bool) -> Outcome {
    let start = Instant::now();
    let prefixed = add_prefixes(query);

    // Shared parse/execute with the deliberate union-default-graph
    // semantics (review #8): this surface explores the whole store. The
    // parse-vs-eval error identity survives for the JSON error object.
    let root = git_lex::find_git_root();
    let results = match git_lex::eval_query_union_at(root.as_deref(), store, &prefixed) {
        Ok(r) => r,
        Err(git_lex::W3cQueryError::Parse(e)) => {
            // An unbound prefix gets a sentence instead of the parser's
            // five-line unicode-class dump (@selkie, 2026-08-27). Everything
            // else still gets the raw message — a wrong explanation would be
            // worse than a noisy true one.
            let friendly = git_lex::explain_unbound_prefix(&prefixed, &e);
            if json {
                eprintln!("{}", serde_json::json!({
                    "error": "parse",
                    "message": e,
                    "explanation": friendly,
                }));
            } else if let Some(msg) = friendly {
                eprintln!("{}", msg);
            } else {
                eprintln!("SPARQL parse error: {}", e);
            }
            return Err(Failure::new(""));
        }
        Err(git_lex::W3cQueryError::Eval(e)) => {
            if json {
                eprintln!("{}", serde_json::json!({"error": "eval", "message": e}));
            } else {
                eprintln!("SPARQL evaluation error: {}", e);
            }
            return Err(Failure::new(""));
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
                        return Err(Failure::new(format!("{}", serde_json::json!({"error": "eval", "message": e}))));
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
                        // A row that cannot be read is a failed query, not
                        // a shorter answer: stop, never print "0 results".
                        Err(e) => {
                            eprintln!("SPARQL evaluation error: {}", git_lex::service_hint(e.to_string()));
                            return Err(Failure::new(""));
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
                return Err(Failure::new(format!("{}", serde_json::json!({
                    "error": "unsupported",
                    "message": "CONSTRUCT/DESCRIBE JSON output not yet supported"
                }))));
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
    Ok(())
}

/// Stored queries: `git lex query '<soul/Query/recent>'` runs a Query document.
/// The lookup and the format are `git_lex::stored_query`: the installed
/// kits' `Query/` folders first, `.lex/query/` after. One door, the same one
/// inline queries use.
///
/// `Ok(Some(text))` is the query to run; `Ok(None)` means the argument is
/// not a saved query's name and runs as SPARQL text; `Err` has already told
/// the person what is wrong.
/// The query text of a `.lex/query/` file, the earlier format: skip YAML
/// frontmatter, then the first fenced block; a file with no fence is all
/// query.
#[cfg(test)]
fn stored_query_text(md: &str) -> String {
    let (_, body) = git_lex::split_frontmatter(md);
    git_lex::stored_query::first_fenced_block(body).unwrap_or_else(|| body.trim().to_string())
}

pub(crate) fn cmd_direct(query: String, params: Vec<String>, json: bool) -> Outcome {
    // Stored-query resolution first: a name that matches .lex/query/<name>.md
    // runs that file's query; anything else runs as SPARQL text. A name-like
    // miss gets the available list instead of a SPARQL parse error.
    if !params.is_empty() || query.trim_start().starts_with('<') {
        return Err(Failure::new("`git lex direct` runs SPARQL text. A stored query runs with `git lex query '<soul/Query/recent>'`."));
    }
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
    // remains a SYNC artifact (the one graph of history plus the graphs sync
    // regenerates each run; the sync/<sha> family is retired); the "now" view is always
    // derived fresh here, trading a little speed for a correct, surprise-free flow.
    let start = Instant::now();
    let store = Store::new().expect("failed to create in-memory store");

    let git_nq = git_lex::git2_nquads::generate_git2_nquads();
    let git_count = git_nq.lines().count();
    store
        .load_from_reader(RdfFormat::NQuads, Cursor::new(git_nq.as_bytes()))
        .expect("failed to load git triples");

    // The live "now" graph: extract frontmatter + markdown links straight
    // from the working-tree .md files (this is what `save` would extract,
    // computed fresh). write_sidecars is OFF — query is a READ command; it
    // used to rewrite the .spo sidecars as a side effect, dirtying the tree
    // from a question.
    let walk = generate_frontmatter_nquads(git_lex::nquad::NowWalkOpts {
        write_sidecars: false,
        build_nquads: true,
    })?;
    let fm_nq = walk.nquads;
    let lex_count = walk.facts;
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
    )?;
    Ok(())
}

// ─── git lex query: the soul's graph, through gitlexd ─────────────────

/// Ask gitlexd for the soul this session is bound to. The soul is resolved
/// from the process that started the session (gitlexd::session), never from
/// a flag, a setting or the shell's current directory.
pub(crate) fn cmd_query(name: Option<String>, params: Vec<String>, json: bool) -> Outcome {
    use git_lex::stored_query::{self, GIT_LEX_SERVICE};
    let soul = git_lex::gitlexd::session::session_soul().map_err(|e| Failure::new(e.to_string()))?;
    let Some(id) = name else {
        let all = stored_query::list(&soul.root);
        if json {
            println!("{}", serde_json::json!({ "queries": all }));
        } else if all.is_empty() {
            println!("This soul has no stored queries yet. Make one with `git lex create query <name>`.");
        } else {
            for q in &all {
                println!("{:<36} {:<9} {}", q.id, q.service.as_deref().unwrap_or("(none)"), q.title.as_deref().unwrap_or(""));
            }
        }
        return Ok(());
    };
    // Anything with whitespace or a brace is SPARQL text, never a name.
    if id.contains(char::is_whitespace) || id.contains('{') {
        return Err(Failure::new(
            "That is SPARQL text. `git lex query` runs a stored query by name; run text with `git lex sparql \"...\"`.",
        ));
    }
    let given = git_lex::query_params::read_given(&params).map_err(Failure::new)?;
    let found = stored_query::resolve(&soul.root, &id, &given).map_err(Failure::new)?;
    if found.service != GIT_LEX_SERVICE {
        return Err(Failure::new(format!(
            "This query runs against {}. git-lex runs only git-lex queries.\nRun it with: syrinx query '{}'",
            found.service, found.id
        )));
    }
    eprintln!("Stored query: {}", found.file);
    run_through_gitlexd(&soul, found.sparql, json)
}

/// `git lex sparql "<text>"`: SPARQL typed on the command line, asked of the
/// soul's own store through gitlexd.
pub(crate) fn cmd_sparql(query: String, json: bool) -> Outcome {
    let soul = git_lex::gitlexd::session::session_soul().map_err(|e| Failure::new(e.to_string()))?;
    run_through_gitlexd(&soul, query, json)
}

fn run_through_gitlexd(soul: &git_lex::gitlexd::session::SessionSoul, query: String, json: bool) -> Outcome {
    if let Err(e) = git_lex::gitlexd::client::ensure_running() {
        return Err(Failure::new([e.to_string(), String::from("(`git lex direct \"...\"` reads the working tree without gitlexd.)")].join("\n")));
    }
    let start = Instant::now();
    let response = match git_lex::gitlexd::client::query(&soul.root, &query) {
        Ok(r) => r,
        Err(e) => {
            let friendly = e
                .strip_prefix("SPARQL parse error: ")
                .and_then(|inner| git_lex::explain_unbound_prefix(&git_lex::add_prefixes_at(Some(&soul.root), &query), inner));
            if json {
                eprintln!("{}", serde_json::json!({ "error": "gitlexd", "message": e, "explanation": friendly }));
            } else if let Some(msg) = friendly {
                eprintln!("{msg}");
            } else {
                eprintln!("{e}");
            }
            return Err(Failure::new(""));
        }
    };
    let count = if response.content_type.starts_with("application/n-triples") {
        print!("{}", response.body);
        response.body.lines().count()
    } else if json {
        println!("{}", response.body.trim_end());
        count_bindings(&response.body)
    } else {
        print_w3c_table(&response.body)?
    };
    eprintln!(
        "\n{} results in {:.1}ms (gitlexd: soul {} at {})",
        count,
        start.elapsed().as_secs_f64() * 1000.0,
        &soul.genesis[..8.min(soul.genesis.len())],
        soul.root.display()
    );
    Ok(())
}

fn count_bindings(body: &str) -> usize {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .map(|v| {
            if v.get("boolean").is_some() {
                1
            } else {
                v["results"]["bindings"].as_array().map(|b| b.len()).unwrap_or(0)
            }
        })
        .unwrap_or(0)
}

/// One W3C JSON term as the table shows it: an IRI bare, a literal by its
/// value, a blank node as `_:x`, a triple term as `<< s p o >>`.
fn w3c_term_text(t: &serde_json::Value) -> String {
    match t.get("type").and_then(|x| x.as_str()) {
        Some("bnode") => format!("_:{}", t["value"].as_str().unwrap_or_default()),
        Some("triple") => {
            let inner = &t["value"];
            format!(
                "<< {} {} {} >>",
                w3c_term_text(&inner["subject"]),
                w3c_term_text(&inner["predicate"]),
                w3c_term_text(&inner["object"])
            )
        }
        _ => t["value"].as_str().map(str::to_string).unwrap_or_else(|| t["value"].to_string()),
    }
}

/// Print a W3C SPARQL JSON result the way `git lex direct` prints its rows.
/// Returns the row count.
pub(crate) fn print_w3c_table(body: &str) -> Outcome<usize> {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            return Err(Failure::new(format!("gitlexd answered with something that is not a SPARQL result: {e}")));
        }
    };
    if let Some(b) = v.get("boolean").and_then(|b| b.as_bool()) {
        println!("{b}");
        return Ok(1);
    }
    let vars: Vec<String> = v["head"]["vars"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let rows: Vec<Vec<String>> = v["results"]["bindings"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|b| vars.iter().map(|var| b.get(var).map(w3c_term_text).unwrap_or_default()).collect())
                .collect()
        })
        .unwrap_or_default();
    if rows.is_empty() {
        println!("(No results found)");
        return Ok(0);
    }
    let mut widths: Vec<usize> = vars.iter().map(|v| v.chars().count()).collect();
    for row in &rows {
        for (i, val) in row.iter().enumerate() {
            widths[i] = widths[i].max(val.chars().count());
        }
    }
    let mut header = String::new();
    for (i, var) in vars.iter().enumerate() {
        header.push_str(&format!(" {:width$} |", var, width = widths[i]));
    }
    println!("|{} \n|{}", header, "-".repeat(header.len().saturating_sub(1)));
    for row in &rows {
        let mut line = String::new();
        for (i, val) in row.iter().enumerate() {
            line.push_str(&format!(" {:width$} |", val, width = widths[i]));
        }
        println!("|{line}");
    }
    Ok(rows.len())
}


#[cfg(test)]
mod w3c_table_tests {
    use super::w3c_term_text;

    #[test]
    fn every_term_kind_renders_as_direct_prints_it() {
        let iri = serde_json::json!({"type": "uri", "value": "https://repolex.ai/soul/Journal/day-1"});
        let lit = serde_json::json!({"type": "literal", "value": "hello", "datatype": "http://www.w3.org/2001/XMLSchema#string"});
        let bn = serde_json::json!({"type": "bnode", "value": "b0"});
        let tt = serde_json::json!({"type": "triple", "value": {"subject": iri, "predicate": {"type": "uri", "value": "p"}, "object": lit}});
        assert_eq!(w3c_term_text(&iri), "https://repolex.ai/soul/Journal/day-1");
        assert_eq!(w3c_term_text(&lit), "hello");
        assert_eq!(w3c_term_text(&bn), "_:b0");
        assert_eq!(w3c_term_text(&tt), "<< https://repolex.ai/soul/Journal/day-1 p hello >>");
    }
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
