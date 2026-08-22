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

pub(crate) fn cmd_query(query: String, json: bool) {
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
