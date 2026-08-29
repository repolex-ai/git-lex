//! `git lex export-index cottas` — snapshot the synced store for external
//! readers, in two artifacts written together:
//!
//! - `<synced-commit>.cottas` — COTTAS (Columnar Triple Table Storage): one
//!   Parquet file with s/p/o/g columns, sorted and ZSTD-compressed. The
//!   columnar dictionary/run-length encoding lives inside Parquet itself.
//!   Machine readers scan it with DuckDB or pycottas; no server. Production
//!   is delegated to the `cottas-rs` binary (Rob-ruled 2026-08-29): making
//!   the crate a dependency would bundle DuckDB's C++ build into every fleet
//!   `cargo install --force`, so git-lex shells out to it the way it shells
//!   out to git — installed once, failed loudly when missing.
//! - `<synced-commit>.spine.md` — the Tabular Prefix spine (kira's spec,
//!   2026-08-29): `@prefix` header + one pipe-table row per fact, built for
//!   pasting straight into an LLM context cache. Covers the `now` and
//!   `repo-ontology` graphs only — current semantic state plus the
//!   vocabulary that explains it; commits/refs/filetree/history are plumbing
//!   with too little meaning per token for a context window. Rows are
//!   sorted, so unchanged content produces identical bytes (a consumer can
//!   key its cache on the file hash, not just the commit).
//!
//! COTTAS is an RDF 1.1 triple table: it structurally cannot hold RDF 1.2
//! triple terms, so the history graph's annotation quads are excluded from
//! the .cottas dump — LOUDLY, with a count (never a silent drop).
//!
//! Snapshots are named by the commit the STORE is synced to (not HEAD —
//! commit-without-sync would otherwise put a fresh name on stale content)
//! and live in the `.lex/_ignore/` worktree pocket beside the oxigraph
//! store and the walkcache. `manifest.json` beside them names the current
//! snapshot so a consumer polls one small file to know whether its cached
//! copy is still good. One snapshot generation is kept; predecessors are
//! pruned after the new one is in place.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use oxigraph::io::{RdfFormat, RdfSerializer};
use oxigraph::model::{GraphName, NamedNode, Term};
use oxigraph::store::Store;

use crate::git::graph_uri;
use crate::require_git_root;

/// Pocket dir for index snapshots — same shape as `.lex/_ignore/oxigraph`
/// and `.lex/_ignore/walkcache`.
fn cottas_dir(root: &Path) -> PathBuf {
    root.join(".lex").join("_ignore").join("cottas")
}

pub(crate) fn cmd_export_index(format: &str) {
    if format != "cottas" {
        eprintln!(
            "fatal: unknown index format '{format}'.\n\
             The only format today is: cottas\n\
             Type: git lex export-index cottas"
        );
        std::process::exit(1);
    }

    let root = require_git_root();

    let Some(store) = git_lex::open_store_read_only_at(&root) else {
        // open_store_read_only_at already printed the corrupt/locked case;
        // the None we act on here is the genuinely-missing store.
        eprintln!(
            "fatal: no synced store to export.\n\
             Type: git lex sync — then re-run this command."
        );
        std::process::exit(1);
    };

    let Some(synced_sha) = newest_synced_commit(&store) else {
        eprintln!(
            "fatal: the store exists but holds no synced commits, so there is \
             nothing to snapshot.\n\
             Type: git lex sync — then re-run this command."
        );
        std::process::exit(1);
    };
    let short = &synced_sha[..8.min(synced_sha.len())];

    let dir = cottas_dir(&root);
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("fatal: cannot create {}: {e}", dir.display());
        std::process::exit(1);
    }

    let cottas_path = dir.join(format!("{synced_sha}.cottas"));
    let spine_path = dir.join(format!("{synced_sha}.spine.md"));
    if cottas_path.exists() && spine_path.exists() {
        // Re-converge the manifest anyway: it is derived, and a half-done
        // earlier run may have died between snapshot and manifest.
        write_manifest(&dir, &synced_sha, &store, &cottas_path, &spine_path);
        println!(
            "Already current: {} + {} (store synced to {short})",
            rel(&root, &cottas_path),
            rel(&root, &spine_path)
        );
        return;
    }

    require_cottas_binary();

    // ── COTTAS snapshot ──────────────────────────────────────────────────
    // Dump every graph to a temp N-Quads file in the same directory (same
    // filesystem, so the final rename is atomic). The `.nq` suffix is
    // load-bearing: cottas-rs picks its parser from the file extension.
    let tmp_nq = dir.join(format!("{synced_sha}.export-tmp.nq"));
    let tmp_cottas = dir.join(format!("{synced_sha}.export-tmp.cottas"));
    let cleanup = |paths: &[&PathBuf]| {
        for p in paths {
            let _ = fs::remove_file(p);
        }
    };

    let annotation_quads = match dump_plain_quads(&store, &tmp_nq) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("fatal: dumping the store to N-Quads failed: {e}");
            cleanup(&[&tmp_nq]);
            std::process::exit(1);
        }
    };

    let converted = Command::new("cottas-rs")
        .arg("rdf2-cottas")
        .arg(&tmp_nq)
        .arg(&tmp_cottas)
        .arg("spo")
        .output();
    match converted {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            eprintln!(
                "fatal: cottas-rs failed to convert the dump:\n{}",
                String::from_utf8_lossy(&out.stderr).trim_end()
            );
            cleanup(&[&tmp_nq, &tmp_cottas]);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("fatal: could not run cottas-rs: {e}");
            cleanup(&[&tmp_nq, &tmp_cottas]);
            std::process::exit(1);
        }
    }
    cleanup(&[&tmp_nq]);

    if let Err(e) = fs::rename(&tmp_cottas, &cottas_path) {
        eprintln!(
            "fatal: cannot move the finished snapshot into place ({} -> {}): {e}",
            tmp_cottas.display(),
            cottas_path.display()
        );
        cleanup(&[&tmp_cottas]);
        std::process::exit(1);
    }

    // ── Tabular Prefix spine ─────────────────────────────────────────────
    let tmp_spine = dir.join(format!("{synced_sha}.export-tmp.spine.md"));
    let spine_rows = match write_spine(&root, &store, &tmp_spine) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("fatal: writing the spine table failed: {e}");
            cleanup(&[&tmp_spine]);
            std::process::exit(1);
        }
    };
    if let Err(e) = fs::rename(&tmp_spine, &spine_path) {
        eprintln!("fatal: cannot move the spine into place: {e}");
        cleanup(&[&tmp_spine]);
        std::process::exit(1);
    }

    // One snapshot generation kept: prune predecessors so the pocket never
    // accretes one file per sync forever. The manifest is the pointer
    // readers use, so pruning after the renames can never strand a reader
    // mid-swap.
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p == cottas_path || p == spine_path {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".cottas") || name.ends_with(".spine.md") {
                let _ = fs::remove_file(&p);
            }
        }
    }

    write_manifest(&dir, &synced_sha, &store, &cottas_path, &spine_path);

    println!(
        "Exported: {} ({} quads, {}, store synced to {short})",
        rel(&root, &cottas_path),
        (store.len().unwrap_or(0) as u64).saturating_sub(annotation_quads),
        human_bytes(file_len(&cottas_path)),
    );
    if annotation_quads > 0 {
        println!(
            "Excluded: {annotation_quads} history annotation quad(s) — RDF 1.2 \
             triple terms, which a COTTAS triple table cannot hold"
        );
    }
    println!(
        "Spine:    {} ({} facts, {}) — now + repo-ontology graphs, for context loading",
        rel(&root, &spine_path),
        spine_rows,
        human_bytes(file_len(&spine_path)),
    );
    println!("Manifest: {}", rel(&root, &dir.join("manifest.json")));
}

/// Dump every quad whose object is NOT an RDF 1.2 triple term to `path` as
/// N-Quads, returning how many annotation quads were left out. (In RDF 1.2
/// only the object position can hold a triple term, and COTTAS — like any
/// plain triple table — has nowhere to put one.)
fn dump_plain_quads(store: &Store, path: &Path) -> Result<u64, Box<dyn std::error::Error>> {
    let file = fs::File::create(path)?;
    let mut serializer = RdfSerializer::from_format(RdfFormat::NQuads).for_writer(file);
    let mut skipped: u64 = 0;
    for quad in store.iter() {
        let quad = quad?;
        if matches!(quad.object, Term::Triple(_)) {
            skipped += 1;
            continue;
        }
        serializer.serialize_quad(&quad)?;
    }
    serializer.finish()?;
    Ok(skipped)
}

/// The graphs the spine covers: current semantic state + the vocabulary
/// that explains it. Deliberately NOT commits/refs/filetree/history —
/// plumbing facts carry too little meaning per context token.
const SPINE_GRAPHS: [&str; 2] = ["now", "repo-ontology"];

/// The platform base every git-lex IRI is minted under — the `@base` the
/// spine relativizes instance IRIs against.
const SPINE_BASE: &str = "https://repolex.ai/";

/// Write the Tabular Prefix spine: `@prefix` lines for every prefix
/// actually used, then `| SUBJECT | PREDICATE | OBJECT |` rows, sorted.
/// IRIs are shortened by longest-namespace match against the repo's own
/// prefix bindings (the same set `git lex query` injects); an IRI no
/// binding covers stays in full `<angle>` form rather than guessing.
fn write_spine(root: &Path, store: &Store, path: &Path) -> Result<u64, Box<dyn std::error::Error>> {
    use std::io::Write;

    // Longest namespace first, so the most specific binding wins (git2:
    // and md: nest inside git-lex:'s namespace).
    let mut bindings = git_lex::prefix_bindings_at(Some(root));
    bindings.sort_by_key(|(_, ns)| std::cmp::Reverse(ns.len()));

    let mut used: Vec<usize> = Vec::new();
    let mut base_used = false;
    let mut shorten = |term: String| -> String {
        // term arrives in N-Triples form: <iri>, _:blank, or "literal"...
        let Some(iri) = term.strip_prefix('<').and_then(|t| t.strip_suffix('>')) else {
            return term;
        };
        for (i, (name, ns)) in bindings.iter().enumerate() {
            if let Some(local) = iri.strip_prefix(ns.as_str()) {
                if local.is_empty() {
                    continue; // the namespace root itself has no local name
                }
                if !used.contains(&i) {
                    used.push(i);
                }
                return format!("{name}{local}");
            }
        }
        // Instance IRIs live outside every ontology namespace (the known
        // T-box/A-box split), and spelling each one in full is where the
        // tokens go. Turtle's own answer, no minted prefix: relativize
        // against @base. `<copia/Being/w4r3z>` reads back to the full IRI
        // by the standard rule.
        if let Some(rel) = iri.strip_prefix(SPINE_BASE) {
            if !rel.is_empty() {
                base_used = true;
                return format!("<{rel}>");
            }
        }
        term
    };

    let mut rows: Vec<String> = Vec::new();
    for graph in SPINE_GRAPHS {
        let g = GraphName::NamedNode(NamedNode::new(graph_uri(graph))?);
        for quad in store.quads_for_pattern(None, None, None, Some(g.as_ref())) {
            let quad = quad?;
            let s = shorten(quad.subject.to_string());
            let p = shorten(quad.predicate.to_string());
            let o = match &quad.object {
                Term::Triple(_) => continue, // not expected in these graphs
                other => shorten_object(other.to_string(), &mut shorten),
            };
            rows.push(format!("| {s} | {p} | {o} |"));
        }
    }
    rows.sort();
    rows.dedup();

    let mut out = std::io::BufWriter::new(fs::File::create(path)?);
    if base_used {
        writeln!(out, "@base <{SPINE_BASE}>")?;
    }
    used.sort();
    for i in used {
        let (name, ns) = &bindings[i];
        writeln!(out, "@prefix {name} <{ns}>")?;
    }
    writeln!(out)?;
    writeln!(out, "| SUBJECT | PREDICATE | OBJECT |")?;
    let n = rows.len() as u64;
    for row in rows {
        writeln!(out, "{row}")?;
    }
    out.flush()?;
    Ok(n)
}

/// Objects need two extra touches beyond IRI shortening: a typed literal's
/// datatype IRI gets shortened too ("..."^^xsd:date), and a pipe inside a
/// literal is escaped so every fact stays one table row. Pipe is the ONLY
/// escape added here: terms arrive N-Quads-serialized, where newlines,
/// quotes and backslashes are already escaped and only `|` survives raw.
fn shorten_object(term: String, shorten: &mut impl FnMut(String) -> String) -> String {
    let shortened = if term.starts_with('"') {
        match term.rfind("\"^^<") {
            Some(i) => {
                let (lex, dt) = term.split_at(i + 3); // keep `"^^`, shorten `<iri>`
                format!("{lex}{}", shorten(dt.to_string()))
            }
            None => term,
        }
    } else {
        shorten(term)
    };
    shortened.replace('|', "\\|")
}

/// The newest commit the one graph has witnessed — the same "the persisted
/// commit data IS the marker" rule sync's resume point uses (no ancestor
/// gate here: we are naming what the store contains, not choosing where to
/// resume).
fn newest_synced_commit(store: &Store) -> Option<String> {
    let q = format!(
        "SELECT ?sha WHERE {{ GRAPH <{}> {{ \
           ?c <https://repolex.ai/ontology/git-lex/git2/ordinalDerived> ?o ; \
              <https://repolex.ai/ontology/git-lex/git2/id> ?sha }} \
         }} ORDER BY DESC(?o) LIMIT 1",
        graph_uri("commits")
    );
    oxigraph::sparql::SparqlEvaluator::new()
        .parse_query(&q)
        .ok()
        .and_then(|q| q.on_store(store).execute().ok())
        .and_then(|r| match r {
            oxigraph::sparql::QueryResults::Solutions(sols) => sols
                .flatten()
                .filter_map(|s| {
                    s.get("sha").map(|t| match t {
                        Term::Literal(l) => l.value().to_string(),
                        other => other.to_string(),
                    })
                })
                .next(),
            _ => None,
        })
}

fn require_cottas_binary() {
    match Command::new("cottas-rs").arg("--version").output() {
        Ok(out) if out.status.success() => {}
        _ => {
            eprintln!(
                "fatal: cottas-rs is not installed — it is the tool that turns \
                 the dump into a COTTAS file.\n\
                 Type: cargo install cottas-rs --locked\n\
                 (one-time install; the build is slow because it bundles DuckDB)"
            );
            std::process::exit(1);
        }
    }
}

/// `manifest.json` — the one small file a consumer polls to learn which
/// snapshot is current. Written last, after the files it names exist.
fn write_manifest(dir: &Path, sha: &str, store: &Store, cottas: &Path, spine: &Path) {
    // Count what the FILE holds, not what the store holds: the RDF 1.2
    // annotation quads are excluded from the dump, so store.len() would
    // overstate the snapshot.
    let quads: u64 = store
        .iter()
        .filter(|q| !matches!(q, Ok(q) if matches!(q.object, Term::Triple(_))))
        .count() as u64;
    let body = format!(
        "{{\n  \"format\": \"cottas\",\n  \"commit\": \"{sha}\",\n  \
         \"file\": \"{sha}.cottas\",\n  \"spine\": \"{sha}.spine.md\",\n  \
         \"quads\": {quads},\n  \"bytes\": {},\n  \"spine_bytes\": {}\n}}\n",
        file_len(cottas),
        file_len(spine),
    );
    let tmp = dir.join("manifest.json.tmp");
    let path = dir.join("manifest.json");
    if fs::write(&tmp, &body).and_then(|_| fs::rename(&tmp, &path)).is_err() {
        eprintln!(
            "warning: could not write {} — the snapshot files themselves are fine",
            path.display()
        );
    }
}

fn file_len(p: &Path) -> u64 {
    fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

fn human_bytes(b: u64) -> String {
    if b >= 1_048_576 {
        format!("{:.1} MB", b as f64 / 1_048_576.0)
    } else if b >= 1024 {
        format!("{:.1} KB", b as f64 / 1024.0)
    } else {
        format!("{b} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_shorten() -> impl FnMut(String) -> String {
        |t| t
    }

    #[test]
    fn object_escaping_keeps_one_fact_per_row() {
        // A raw pipe inside a literal would otherwise break the table's
        // row/column structure — the whole point of the spine. Everything
        // else (newlines, quotes, backslashes) is already escaped by the
        // N-Quads serializer the terms arrive through, and must pass
        // through untouched.
        let mut s = no_shorten();
        assert_eq!(shorten_object("\"a|b\"".into(), &mut s), "\"a\\|b\"");
        assert_eq!(shorten_object("\"line1\\nline2\"".into(), &mut s), "\"line1\\nline2\"");
        assert_eq!(shorten_object("\"say \\\"hi\\\"\"".into(), &mut s), "\"say \\\"hi\\\"\"");
    }

    #[test]
    fn typed_literal_datatype_is_shortened_not_the_lexical_form() {
        let mut fake = |t: String| {
            if t == "<http://www.w3.org/2001/XMLSchema#date>" {
                "xsd:date".to_string()
            } else {
                t
            }
        };
        assert_eq!(
            shorten_object(
                "\"2026-08-29\"^^<http://www.w3.org/2001/XMLSchema#date>".into(),
                &mut fake
            ),
            "\"2026-08-29\"^^xsd:date"
        );
        // A literal whose TEXT contains ^^< must not have its body mangled:
        // rfind splits at the LAST occurrence, which is the real datatype.
        assert_eq!(
            shorten_object(
                "\"quote \\\"^^<fake>\\\" inside\"^^<http://www.w3.org/2001/XMLSchema#date>".into(),
                &mut fake
            ),
            "\"quote \\\"^^<fake>\\\" inside\"^^xsd:date"
        );
    }
}
