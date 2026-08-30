//! `git lex export-index cottas` — snapshot the SEMANTIC graphs for
//! external readers, in two artifacts written together:
//!
//! - `<synced-commit>.cottas` — COTTAS (Columnar Triple Table Storage): one
//!   Parquet file with s/p/o/g columns, sorted and ZSTD-compressed. The
//!   columnar dictionary/run-length encoding lives inside Parquet itself.
//!   Machine readers scan it with DuckDB or pycottas; no server. Production
//!   is delegated to the `cottas-rs` binary (Rob-ruled 2026-08-29): making
//!   the crate a dependency would bundle DuckDB's C++ build into every fleet
//!   `cargo install --force`, so git-lex shells out to it the way it shells
//!   out to git.
//! - `<synced-commit>.spine.md` — the Tabular Prefix spine (kira's spec,
//!   2026-08-29): `@prefix` header + one pipe-table row per fact, built for
//!   loading straight into an LLM context cache (the semantic KV-cache for
//!   Gemini). Rows are sorted, so unchanged content produces identical
//!   bytes (a consumer can key its cache on the file hash, not just the
//!   commit).
//!
//! BOTH artifacts cover the same two graphs — `now` (current semantic
//! state) and `repo-ontology` (the vocabulary that explains it). The
//! commit/refs/filetree/history plumbing is deliberately out of scope: it
//! is low meaning-per-token for a context window, it is the bulk of a big
//! repo's store (lUX: 911,881 quads total, a 209MB N-Quads intermediate
//! and 3m11s when the export dumped every graph — selkie's measurement,
//! 2026-08-29), and the history graph's RDF 1.2 triple terms cannot exist
//! in a plain triple table anyway. Scoping the export to the semantic
//! graphs is what makes running it on EVERY sync affordable.
//!
//! There is deliberately NO incremental path. Full regeneration each time
//! is the correctness property selkie named: an incremental snapshot that
//! can drift from a full one will drift silently, and nothing downstream
//! can tell. One code path, deterministic sorted output, byte-identical
//! when content is unchanged — drift is designed away, and the
//! determinism test in this module is the tripwire.
//!
//! Snapshots are named by the commit the STORE is synced to (not HEAD —
//! commit-without-sync would otherwise put a fresh name on stale content)
//! and live in the `.lex/_ignore/` worktree pocket beside the oxigraph
//! store and the walkcache. `manifest.json` beside them names the current
//! snapshot so a consumer polls one small file to know whether its cached
//! copy is still good. Retention is ONE generation, decided day one:
//! every successful export prunes predecessor snapshots.
//!
//! `git lex sync` refreshes the snapshot as its last step — but only in a
//! repo that has opted in by running `git lex export-index cottas` once
//! (the pocket dir existing is the opt-in marker). On the sync path a
//! missing cottas-rs binary demotes to a warning and the spine + manifest
//! still refresh: the spine is pure Rust and must never go stale because
//! an unrelated C++ toolchain is absent. The explicit command keeps the
//! hard failure — someone typing `export-index cottas` asked for the
//! .cottas file by name.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use oxigraph::io::{RdfFormat, RdfSerializer};
use oxigraph::model::{GraphName, NamedNode, Term};
use oxigraph::store::Store;

use crate::git::graph_uri;
use crate::require_git_root;

/// The graphs both artifacts cover: current semantic state + the
/// vocabulary that explains it. Everything else is plumbing.
const EXPORT_GRAPHS: [&str; 2] = ["now", "repo-ontology"];

/// The platform base every git-lex IRI is minted under — the `@base` the
/// spine relativizes instance IRIs against.
const SPINE_BASE: &str = "https://repolex.ai/";

/// Pocket dir for index snapshots — same shape as `.lex/_ignore/oxigraph`
/// and `.lex/_ignore/walkcache`. Its existence is also the sync-path
/// opt-in marker.
pub(crate) fn cottas_dir(root: &Path) -> PathBuf {
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

    // Explicit command = strict: a missing cottas-rs binary is fatal here.
    if let Err(e) = run_export(&root, &store, true) {
        eprintln!("fatal: {e}");
        std::process::exit(1);
    }
}

/// The whole export, callable from sync as well as the CLI. `strict`
/// controls the missing-cottas-rs policy: the explicit command fails hard,
/// the sync tail-step writes spine + manifest anyway and warns once.
///
/// Errors are returned, never exited on — the sync caller demotes them to
/// warnings because a stale cache must not fail a sync.
pub(crate) fn run_export(root: &Path, store: &Store, strict: bool) -> Result<(), String> {
    let Some(synced_sha) = newest_synced_commit(store) else {
        return Err(
            "the store holds no synced commits, so there is nothing to snapshot.\n\
             Type: git lex sync — then re-run this command."
                .to_string(),
        );
    };
    let short = &synced_sha[..8.min(synced_sha.len())];

    let dir = cottas_dir(root);
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    let cottas_path = dir.join(format!("{synced_sha}.cottas"));
    let spine_path = dir.join(format!("{synced_sha}.spine.md"));
    if cottas_path.exists() && spine_path.exists() {
        // Re-converge the manifest anyway: it is derived, and a half-done
        // earlier run may have died between snapshot and manifest.
        write_manifest(&dir, &synced_sha, &cottas_path, &spine_path);
        println!(
            "Index: already current at {short} ({})",
            rel(root, &spine_path)
        );
        return Ok(());
    }

    let have_cottas_rs = cottas_binary_present();
    if strict && !have_cottas_rs {
        return Err(
            "cottas-rs is not installed — it is the tool that turns the dump \
             into a COTTAS file.\n\
             Type: cargo install cottas-rs --locked\n\
             (one-time install; the build is slow because it bundles DuckDB)"
                .to_string(),
        );
    }

    let cleanup = |paths: &[&PathBuf]| {
        for p in paths {
            let _ = fs::remove_file(p);
        }
    };

    // ── COTTAS snapshot (skipped with a warning on the sync path when the
    // binary is absent — the spine below is pure Rust and always writes) ──
    let mut wrote_cottas = false;
    let mut exported_quads: u64 = 0;
    if have_cottas_rs {
        // Dump the export graphs to a temp N-Quads file in the same
        // directory (same filesystem, so the final rename is atomic). The
        // `.nq` suffix is load-bearing: cottas-rs picks its parser from the
        // file extension.
        let tmp_nq = dir.join(format!("{synced_sha}.export-tmp.nq"));
        let tmp_cottas = dir.join(format!("{synced_sha}.export-tmp.cottas"));

        let (written, skipped) = dump_export_graphs(store, &tmp_nq).map_err(|e| {
            cleanup(&[&tmp_nq]);
            format!("dumping the store to N-Quads failed: {e}")
        })?;
        exported_quads = written;

        let converted = Command::new("cottas-rs")
            .arg("rdf2-cottas")
            .arg(&tmp_nq)
            .arg(&tmp_cottas)
            .arg("spo")
            .output();
        match converted {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                cleanup(&[&tmp_nq, &tmp_cottas]);
                return Err(format!(
                    "cottas-rs failed to convert the dump:\n{}",
                    String::from_utf8_lossy(&out.stderr).trim_end()
                ));
            }
            Err(e) => {
                cleanup(&[&tmp_nq, &tmp_cottas]);
                return Err(format!("could not run cottas-rs: {e}"));
            }
        }
        cleanup(&[&tmp_nq]);

        fs::rename(&tmp_cottas, &cottas_path).map_err(|e| {
            cleanup(&[&tmp_cottas]);
            format!(
                "cannot move the finished snapshot into place ({} -> {}): {e}",
                tmp_cottas.display(),
                cottas_path.display()
            )
        })?;
        wrote_cottas = true;

        if skipped > 0 {
            // The export graphs should never hold RDF 1.2 triple terms; if
            // one appears it is excluded (a triple table cannot hold it)
            // and reported — never silently.
            println!(
                "Excluded: {skipped} annotation quad(s) — RDF 1.2 triple terms, \
                 which a COTTAS triple table cannot hold"
            );
        }
    } else {
        println!(
            "Index: .cottas skipped — cottas-rs is not installed \
             (cargo install cottas-rs --locked). Spine still refreshed."
        );
    }

    // ── Tabular Prefix spine (pure Rust, always written) ─────────────────
    // Capture the OUTGOING spine first: the incremental delta below is the
    // set-difference of consecutive generations, so the previous file is
    // read before anything replaces or prunes it.
    let prev_spine: Option<(String, String)> = previous_spine(&dir, &synced_sha);

    let tmp_spine = dir.join(format!("{synced_sha}.export-tmp.spine.md"));
    let (spine_header, spine_body) = build_spine_content(root, store)
        .map_err(|e| format!("building the spine table failed: {e}"))?;
    let spine_rows = spine_body.len() as u64;
    write_spine_file(&tmp_spine, &spine_header, &spine_body).map_err(|e| {
        cleanup(&[&tmp_spine]);
        format!("writing the spine table failed: {e}")
    })?;
    fs::rename(&tmp_spine, &spine_path).map_err(|e| {
        cleanup(&[&tmp_spine]);
        format!("cannot move the spine into place: {e}")
    })?;

    // ── Incremental delta (Rob, 2026-08-29): a small "what changed" file a
    // consumer applies to its cached spine instead of re-ingesting the
    // whole thing. Derived as the set-difference of the two FULL spines —
    // one derivation path, so the delta can never drift from what a full
    // re-ingest would see (selkie's correctness property, held by
    // construction). No delta when there is no predecessor to diff against
    // (first export, or the pocket was cleared): the manifest simply shows
    // no chain and the consumer ingests the full spine.
    if let Some((from_sha, old_text)) = &prev_spine {
        let old_rows: Vec<String> = old_text
            .lines()
            .filter(|l| l.starts_with("| ") && !l.starts_with("| SUBJECT"))
            .map(str::to_string)
            .collect();
        let (removed, added) = sorted_diff(&old_rows, &spine_body);
        if !removed.is_empty() || !added.is_empty() {
            let old_header: Vec<String> = old_text
                .lines()
                .filter(|l| l.starts_with('@'))
                .map(str::to_string)
                .collect();
            let delta_path = dir.join(format!("{from_sha}-{synced_sha}.delta.md"));
            if let Err(e) = write_delta_file(
                &delta_path, from_sha, &synced_sha, &old_header, &spine_header, &removed, &added,
            ) {
                eprintln!("warning: could not write the delta file: {e} (full spine is intact)");
            } else {
                println!(
                    "Delta:    {} (-{} +{} facts, {})",
                    rel(root, &delta_path),
                    removed.len(),
                    added.len(),
                    human_bytes(file_len(&delta_path)),
                );
            }
        }
    }

    // Retention, decided day one: ONE generation. Prune every snapshot
    // that is not the current sha — including a stale .cottas when the
    // binary was absent this run (wrong-but-present is worse than absent).
    // The manifest is the pointer readers use, so pruning after the
    // renames can never strand a reader mid-swap.
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

    // Delta retention: the chain is kept until its total size exceeds the
    // current full spine — past that point, replaying deltas costs a
    // consumer more than re-ingesting the full file, so the oldest links
    // stop earning their bytes and are dropped first.
    prune_delta_chain(&dir, file_len(&spine_path));

    write_manifest(&dir, &synced_sha, &cottas_path, &spine_path);

    if wrote_cottas {
        println!(
            "Exported: {} ({} quads, {}, store synced to {short})",
            rel(root, &cottas_path),
            exported_quads,
            human_bytes(file_len(&cottas_path)),
        );
    }
    println!(
        "Spine:    {} ({} facts, {}) — now + repo-ontology graphs, for context loading",
        rel(root, &spine_path),
        spine_rows,
        human_bytes(file_len(&spine_path)),
    );
    println!("Manifest: {}", rel(root, &dir.join("manifest.json")));
    Ok(())
}

/// Sync's tail-step: refresh the snapshot ONLY in a repo that opted in by
/// exporting once (the pocket dir is the marker). Every failure demotes to
/// a warning — a stale cache must never fail a sync.
pub(crate) fn refresh_after_sync(root: &Path, store: &Store) {
    if !cottas_dir(root).is_dir() {
        return;
    }
    if let Err(e) = run_export(root, store, false) {
        eprintln!("warning: index snapshot not refreshed: {e}");
        eprintln!("(sync itself succeeded; run `git lex export-index cottas` to retry)");
    }
}

/// Dump the EXPORT_GRAPHS to `path` as N-Quads. Returns (written, skipped)
/// where skipped counts quads whose object is an RDF 1.2 triple term —
/// not expected in these graphs, but excluded defensively and loudly if
/// one appears (a plain triple table has nowhere to put it).
fn dump_export_graphs(store: &Store, path: &Path) -> Result<(u64, u64), Box<dyn std::error::Error>> {
    let file = fs::File::create(path)?;
    let mut serializer = RdfSerializer::from_format(RdfFormat::NQuads).for_writer(file);
    let mut written: u64 = 0;
    let mut skipped: u64 = 0;
    for graph in EXPORT_GRAPHS {
        let g = GraphName::NamedNode(NamedNode::new(graph_uri(graph))?);
        for quad in store.quads_for_pattern(None, None, None, Some(g.as_ref())) {
            let quad = quad?;
            if matches!(quad.object, Term::Triple(_)) {
                skipped += 1;
                continue;
            }
            serializer.serialize_quad(&quad)?;
            written += 1;
        }
    }
    serializer.finish()?;
    Ok((written, skipped))
}

/// Build the Tabular Prefix spine as (header lines, sorted rows): `@base`
/// + `@prefix` lines for every binding actually used, then
/// `| SUBJECT | PREDICATE | OBJECT |` rows. IRIs are shortened by
/// longest-namespace match against the repo's own prefix bindings (the
/// same set `git lex query` injects); an IRI under neither a binding nor
/// the base stays in full `<angle>` form.
fn build_spine_content(
    root: &Path,
    store: &Store,
) -> Result<(Vec<String>, Vec<String>), Box<dyn std::error::Error>> {
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
    for graph in EXPORT_GRAPHS {
        let g = GraphName::NamedNode(NamedNode::new(graph_uri(graph))?);
        for quad in store.quads_for_pattern(None, None, None, Some(g.as_ref())) {
            let quad = quad?;
            // Blank-node rows are excluded — TWO reasons, both load-bearing.
            // Stability: blank labels are minted fresh every time the
            // ontology graph reloads, so semantically identical facts
            // render as different strings — the first real delta was 780
            // phantom changes of exactly this kind (2026-08-29). Value:
            // these rows are OWL structural encoding (restriction shells,
            // list links) that mean nothing without chasing the blank
            // labels a context window can't chase. Named facts only.
            if matches!(quad.subject, oxigraph::model::NamedOrBlankNode::BlankNode(_))
                || matches!(quad.object, Term::BlankNode(_))
            {
                continue;
            }
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

    let mut header: Vec<String> = Vec::new();
    if base_used {
        header.push(format!("@base <{SPINE_BASE}>"));
    }
    used.sort();
    for i in used {
        let (name, ns) = &bindings[i];
        header.push(format!("@prefix {name} <{ns}>"));
    }
    Ok((header, rows))
}

fn write_spine_file(
    path: &Path,
    header: &[String],
    rows: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    let mut out = std::io::BufWriter::new(fs::File::create(path)?);
    for line in header {
        writeln!(out, "{line}")?;
    }
    writeln!(out)?;
    writeln!(out, "| SUBJECT | PREDICATE | OBJECT |")?;
    for row in rows {
        writeln!(out, "{row}")?;
    }
    out.flush()?;
    Ok(())
}

/// The one previous spine generation in the pocket, as (sha, full text) —
/// the diff base for the incremental delta. Skips the file being written
/// (same sha), temp files, and anything not shaped like `<sha>.spine.md`.
fn previous_spine(dir: &Path, current_sha: &str) -> Option<(String, String)> {
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(sha) = name.strip_suffix(".spine.md") else { continue };
        if sha == current_sha || sha.contains('.') || sha.contains('-') {
            continue; // current, temp, or a delta-shaped name — not a base
        }
        if let Ok(text) = fs::read_to_string(entry.path()) {
            return Some((sha.to_string(), text));
        }
    }
    None
}

/// Set-difference of two SORTED, DEDUPED row lists: (removed, added) —
/// rows only in `old`, rows only in `new`. One merge walk, exact.
fn sorted_diff(old: &[String], new: &[String]) -> (Vec<String>, Vec<String>) {
    let (mut removed, mut added) = (Vec::new(), Vec::new());
    let (mut i, mut j) = (0usize, 0usize);
    while i < old.len() && j < new.len() {
        match old[i].cmp(&new[j]) {
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => {
                removed.push(old[i].clone());
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                added.push(new[j].clone());
                j += 1;
            }
        }
    }
    removed.extend(old[i..].iter().cloned());
    added.extend(new[j..].iter().cloned());
    (removed, added)
}

/// The delta file: `@delta from <sha> to <sha>`, the union of both
/// generations' prefix headers (rows from each side were shortened under
/// their own bindings), then a removed section and an added section.
/// Applying it to the `from` spine's rows — delete the removed strings,
/// insert the added ones — reproduces the `to` spine's rows exactly.
fn write_delta_file(
    path: &Path,
    from_sha: &str,
    to_sha: &str,
    old_header: &[String],
    new_header: &[String],
    removed: &[String],
    added: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    let mut out = std::io::BufWriter::new(fs::File::create(path)?);
    writeln!(out, "@delta from {from_sha} to {to_sha}")?;
    let mut seen: Vec<&String> = Vec::new();
    for line in old_header.iter().chain(new_header.iter()) {
        if !seen.contains(&line) {
            seen.push(line);
            writeln!(out, "{line}")?;
        }
    }
    writeln!(out)?;
    writeln!(out, "## removed ({})", removed.len())?;
    writeln!(out, "| SUBJECT | PREDICATE | OBJECT |")?;
    for row in removed {
        writeln!(out, "{row}")?;
    }
    writeln!(out)?;
    writeln!(out, "## added ({})", added.len())?;
    writeln!(out, "| SUBJECT | PREDICATE | OBJECT |")?;
    for row in added {
        writeln!(out, "{row}")?;
    }
    out.flush()?;
    Ok(())
}

/// Drop the oldest delta links while the chain's total bytes exceed the
/// current full spine's — past that point a full re-ingest is cheaper
/// than replaying, so old links no longer earn their keep.
fn prune_delta_chain(dir: &Path, spine_bytes: u64) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut deltas: Vec<(std::time::SystemTime, PathBuf, u64)> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".delta.md"))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            Some((meta.modified().ok()?, e.path(), meta.len()))
        })
        .collect();
    deltas.sort_by_key(|(t, _, _)| *t);
    let mut total: u64 = deltas.iter().map(|(_, _, b)| b).sum();
    for (_, path, bytes) in &deltas {
        if total <= spine_bytes {
            break;
        }
        let _ = fs::remove_file(path);
        total -= bytes;
    }
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

fn cottas_binary_present() -> bool {
    matches!(
        Command::new("cottas-rs").arg("--version").output(),
        Ok(out) if out.status.success()
    )
}

/// `manifest.json` — the one small file a consumer polls to learn which
/// snapshot is current. Written last, after the files it names exist. The
/// `file`/`bytes` fields describe the .cottas and appear only when that
/// file exists at the current sha (the sync path skips it when cottas-rs
/// is absent); `spine`/`spine_bytes` are always present.
fn write_manifest(dir: &Path, sha: &str, cottas: &Path, spine: &Path) {
    let mut body = String::from("{\n  \"format\": \"cottas\",\n");
    body.push_str(&format!("  \"commit\": \"{sha}\",\n"));
    if cottas.exists() {
        body.push_str(&format!("  \"file\": \"{sha}.cottas\",\n"));
        body.push_str(&format!("  \"bytes\": {},\n", file_len(cottas)));
    }
    body.push_str(&format!("  \"spine\": \"{sha}.spine.md\",\n"));
    body.push_str(&format!("  \"spine_bytes\": {},\n", file_len(spine)));

    // The delta chain, oldest first: each entry a change-file a consumer
    // can apply to its cached spine instead of re-ingesting the full one.
    // A gap in the chain (or an empty list) means: ingest the full spine.
    let mut deltas: Vec<(std::time::SystemTime, String, u64)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".delta.md") {
                return None;
            }
            let meta = e.metadata().ok()?;
            Some((meta.modified().ok()?, name, meta.len()))
        })
        .collect();
    deltas.sort_by_key(|(t, _, _)| *t);
    body.push_str("  \"deltas\": [");
    for (i, (_, name, bytes)) in deltas.iter().enumerate() {
        let (from, to) = name
            .strip_suffix(".delta.md")
            .and_then(|s| s.split_once('-'))
            .unwrap_or(("", ""));
        if i > 0 {
            body.push(',');
        }
        body.push_str(&format!(
            "\n    {{\"from\": \"{from}\", \"to\": \"{to}\", \"file\": \"{name}\", \"bytes\": {bytes}}}"
        ));
    }
    body.push_str(if deltas.is_empty() { "]\n}\n" } else { "\n  ]\n}\n" });

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

    /// Selkie's correctness property as a tripwire (2026-08-29): the spine
    /// must be byte-deterministic — the same store content must produce
    /// the identical file, every time. There is deliberately no
    /// incremental path to drift, and this test fails the moment someone
    /// adds nondeterminism (an unsorted iteration, a timestamp, a
    /// hash-ordered map) to the writer.
    #[test]
    fn spine_is_byte_deterministic() {
        use oxigraph::model::Quad;
        let store = Store::new().unwrap();
        let g = NamedNode::new(graph_uri("now")).unwrap();
        for (s, p, o) in [
            ("https://repolex.ai/soul/Note/a", "https://repolex.ai/ontology/soul/title", "A note"),
            ("https://repolex.ai/soul/Note/b", "https://repolex.ai/ontology/soul/title", "B | pipe"),
            ("https://repolex.ai/soul/Note/a", "https://repolex.ai/ontology/soul/topic", "z"),
        ] {
            store
                .insert(&Quad::new(
                    NamedNode::new(s).unwrap(),
                    NamedNode::new(p).unwrap(),
                    oxigraph::model::Literal::new_simple_literal(o),
                    g.clone(),
                ))
                .unwrap();
        }
        let dir = std::env::temp_dir().join(format!("glx-spine-det-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p1 = dir.join("one.spine.md");
        let p2 = dir.join("two.spine.md");
        // root: no repo needed — the temp dir has no .lex, so the binding
        // set is just the standard prefixes.
        let (h1, r1) = build_spine_content(&dir, &store).unwrap();
        let (h2, r2) = build_spine_content(&dir, &store).unwrap();
        write_spine_file(&p1, &h1, &r1).unwrap();
        write_spine_file(&p2, &h2, &r2).unwrap();
        assert_eq!(r1.len(), 3);
        assert_eq!(fs::read(&p1).unwrap(), fs::read(&p2).unwrap(), "spine bytes must be identical");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The incremental delta's whole contract: applying (remove `removed`,
    /// insert `added`) to the old row set reproduces the new row set
    /// EXACTLY. Because the delta is computed as the set-difference of the
    /// two full spines, this holds by construction — this test is the
    /// tripwire against anyone recomputing the delta some other way.
    #[test]
    fn delta_reconstructs_new_rows_from_old() {
        let old: Vec<String> = ["| a | p | 1 |", "| b | p | 2 |", "| c | p | 3 |"]
            .iter().map(|s| s.to_string()).collect();
        let new: Vec<String> = ["| b | p | 2 |", "| b | p | 9 |", "| c | p | 3 |", "| d | p | 4 |"]
            .iter().map(|s| s.to_string()).collect();
        let (removed, added) = sorted_diff(&old, &new);
        assert_eq!(removed, vec!["| a | p | 1 |".to_string()]);
        assert_eq!(added, vec!["| b | p | 9 |".to_string(), "| d | p | 4 |".to_string()]);
        // reconstruct
        let mut rebuilt: Vec<String> =
            old.iter().filter(|r| !removed.contains(r)).cloned().collect();
        rebuilt.extend(added.iter().cloned());
        rebuilt.sort();
        assert_eq!(rebuilt, new);
        // unchanged content → empty delta, both directions
        let (r2, a2) = sorted_diff(&new, &new);
        assert!(r2.is_empty() && a2.is_empty());
    }

    /// The export dumps ONLY the semantic graphs — a quad in the commits
    /// graph (or any other plumbing graph) must never leak into the
    /// snapshot. This is the scope decision that makes per-sync export
    /// affordable at lUX scale; leaking plumbing back in would silently
    /// re-grow the 3-minute export.
    #[test]
    fn dump_covers_only_export_graphs() {
        use oxigraph::model::Quad;
        let store = Store::new().unwrap();
        let mk = |g: &str| NamedNode::new(graph_uri(g)).unwrap();
        let quad = |g: NamedNode, s: &str| {
            Quad::new(
                NamedNode::new(s).unwrap(),
                NamedNode::new("https://repolex.ai/ontology/soul/title").unwrap(),
                oxigraph::model::Literal::new_simple_literal("x"),
                g,
            )
        };
        store.insert(&quad(mk("now"), "https://repolex.ai/soul/Note/in-now")).unwrap();
        store.insert(&quad(mk("repo-ontology"), "https://repolex.ai/soul/Note/in-onto")).unwrap();
        store.insert(&quad(mk("commits"), "https://repolex.ai/soul/Note/in-commits")).unwrap();
        store.insert(&quad(mk("filetree/abc"), "https://repolex.ai/soul/Note/in-tree")).unwrap();

        let dir = std::env::temp_dir().join(format!("glx-scope-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let nq = dir.join("dump.nq");
        let (written, skipped) = dump_export_graphs(&store, &nq).unwrap();
        assert_eq!(written, 2, "exactly the now + repo-ontology quads");
        assert_eq!(skipped, 0);
        let text = fs::read_to_string(&nq).unwrap();
        assert!(text.contains("in-now") && text.contains("in-onto"));
        assert!(!text.contains("in-commits") && !text.contains("in-tree"));
        let _ = fs::remove_dir_all(&dir);
    }
}
