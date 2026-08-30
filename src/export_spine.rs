//! `git lex export-spine` — write the repo's semantic index as one TSV
//! file built for loading into an LLM context cache (the neural KV-cache
//! project, Rob + kira, 2026-08-29).
//!
//! THE ARTIFACT: `.lex/_ignore/spine/<synced-commit>.spine.tsv` — the full
//! index, rewritten complete on every sync. Layout:
//!
//!   # genesis_sha: 495d8c70          ← identity header: which soul this IS
//!   # soul: W4R3Z                       (repo.yml agent_name)
//!   # repo: 7R1PL3F0RC3/W4R3Z           (org/repo, from the root path)
//!   @base <https://repolex.ai/>
//!   @prefix soul: <...>              ← only the prefixes actually used
//!
//!   ?s	?p	?o                        ← SPARQL 1.1 TSV header row
//!   <soul/Note/kira>	git-lex:title	"Kira"
//!
//! Tab-separated, no pipes, no padding (kira's ruling: tabs are 1 byte,
//! near-zero collision risk, native to Unix/SQLite tooling, and W3C
//! SPARQL-TSV shaped). Rows are sorted, so unchanged content is
//! byte-identical — consumers can key caches on the file hash. Covers the
//! `now` + `repo-ontology` graphs only: current semantic state plus the
//! vocabulary that explains it. Plumbing graphs (commits/refs/filetree/
//! history) are out of scope — low meaning per token, and the history
//! graph alone would blow past any context window at fleet scale.
//! Blank-node rows are excluded (unstable labels, structural OWL shells);
//! RDF 1.2 triple-term objects likewise (annotation plumbing).
//!
//! There is NO delta machinery and NO opt-in gate (Rob-ruled 2026-08-29,
//! after a full evening of learning it the hard way): every sync writes
//! the complete file, full stop. Deriving the rows is the work and it is
//! sub-second; the write is noise.
//!
//! CLOUD HANDOFF: git-lex never talks to any cloud. After writing the
//! spine it spawns `pythia cache update` (detached, cwd = repo root,
//! output discarded) IF a pythia binary is on PATH — pythia owns the
//! Gemini context-cache upload and writes its own keys (e.g. cache_id)
//! into manifest.json. Absent pythia = silent skip: an optional consumer,
//! not a warning. Because two writers share manifest.json, git-lex
//! PRESERVES every key it does not own when rewriting it.
//!
//! Named by the commit the STORE is synced to (not HEAD — commit-without-
//! sync must not put a fresh name on stale content). One generation kept;
//! predecessors and the retired `.lex/_ignore/cottas/` pocket are cleaned
//! up on the way through.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use oxigraph::model::{GraphName, NamedNode, Term};
use oxigraph::store::Store;

use crate::git::graph_uri;
use crate::require_git_root;

/// The graphs the spine covers: current semantic state + the vocabulary
/// that explains it. Everything else is plumbing.
const EXPORT_GRAPHS: [&str; 2] = ["now", "repo-ontology"];

/// The platform base every git-lex IRI is minted under — the `@base` the
/// spine relativizes instance IRIs against.
const SPINE_BASE: &str = "https://repolex.ai/";

/// Pocket dir for the spine — same shape as `.lex/_ignore/oxigraph` and
/// `.lex/_ignore/walkcache`.
pub(crate) fn spine_dir(root: &Path) -> PathBuf {
    root.join(".lex").join("_ignore").join("spine")
}

pub(crate) fn cmd_export_spine() {
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

    if let Err(e) = run_export(&root, &store) {
        eprintln!("fatal: {e}");
        std::process::exit(1);
    }
}

/// The whole export, callable from sync as well as the CLI. Errors are
/// returned, never exited on — the sync caller demotes them to warnings
/// because a cache artifact must not fail a sync.
pub(crate) fn run_export(root: &Path, store: &Store) -> Result<(), String> {
    let Some(synced_sha) = newest_synced_commit(store) else {
        return Err(
            "the store holds no synced commits, so there is nothing to export.\n\
             Type: git lex sync — then re-run this command."
                .to_string(),
        );
    };
    let short = &synced_sha[..8.min(synced_sha.len())];

    let dir = spine_dir(root);
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    let spine_path = dir.join(format!("{synced_sha}.spine.tsv"));
    if spine_path.exists() {
        write_manifest(&dir, &synced_sha, &spine_path);
        println!("Spine: already current at {short} ({})", rel(root, &spine_path));
        return Ok(());
    }

    let tmp = dir.join(format!("{synced_sha}.export-tmp.spine.tsv"));
    let rows = write_spine_file(root, store, &tmp).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("writing the spine failed: {e}")
    })?;
    fs::rename(&tmp, &spine_path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("cannot move the spine into place: {e}")
    })?;

    // One generation kept: prune predecessors after the new file is in
    // place. The manifest is the pointer readers use, so pruning after the
    // rename can never strand a reader mid-swap.
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p != spine_path
                && entry.file_name().to_string_lossy().ends_with(".spine.tsv")
            {
                let _ = fs::remove_file(&p);
            }
        }
    }

    // The retired Parquet-era pocket: derived data whose format died
    // (Rob-ruled 2026-08-29). Clean it up once, loudly.
    let old = root.join(".lex").join("_ignore").join("cottas");
    if old.is_dir() && fs::remove_dir_all(&old).is_ok() {
        println!("Cleaned: .lex/_ignore/cottas/ (retired format; the spine replaced it)");
    }

    write_manifest(&dir, &synced_sha, &spine_path);

    println!(
        "Spine:    {} ({} facts, {}, store synced to {short})",
        rel(root, &spine_path),
        rows,
        human_bytes(file_len(&spine_path)),
    );

    // Cloud handoff — pythia owns the context-cache upload. Detached spawn,
    // cwd = repo root, output discarded; a missing pythia is a silent skip
    // (optional per-machine tooling, not a fleet requirement). CONTRACT
    // OFFERED TO PYTHIA: it is invoked as `pythia cache update` with the
    // repo as working directory after every spine write, and it may write
    // its own keys into .lex/_ignore/spine/manifest.json — git-lex
    // preserves keys it does not own.
    let _ = Command::new("pythia")
        .args(["cache", "update"])
        .current_dir(root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn();

    Ok(())
}

/// Sync's tail-step: every sync, every repo, no gate. Failures demote to
/// warnings — a cache artifact must never fail a sync.
pub(crate) fn refresh_after_sync(root: &Path, store: &Store) {
    if let Err(e) = run_export(root, store) {
        eprintln!("warning: spine not refreshed: {e}");
        eprintln!("(sync itself succeeded; run `git lex export-spine` to retry)");
    }
}

/// Build and write the spine. Returns the row count.
fn write_spine_file(
    root: &Path,
    store: &Store,
    path: &Path,
) -> Result<u64, Box<dyn std::error::Error>> {
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
        // T-box/A-box split). Turtle's own answer, no minted prefix:
        // relativize against @base — `<copia/Being/w4r3z>` reads back to
        // the full IRI by the standard rule.
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
            // Blank-node rows: unstable labels minted fresh on every
            // ontology reload — semantically identical facts would render
            // as different strings each generation — and they are OWL
            // structural shells with no standalone meaning. Named facts
            // only. (No soul DATA ever produces a blank node; these come
            // only from the kit TTLs' `[ owl:Restriction ... ]` syntax.)
            if matches!(quad.subject, oxigraph::model::NamedOrBlankNode::BlankNode(_))
                || matches!(quad.object, Term::BlankNode(_))
            {
                continue;
            }
            let s = shorten(quad.subject.to_string());
            let p = shorten(quad.predicate.to_string());
            let o = match &quad.object {
                Term::Triple(_) => continue, // annotation plumbing; not spine content
                other => escape_tsv(shorten_object(other.to_string(), &mut shorten)),
            };
            rows.push(format!("{s}\t{p}\t{o}"));
        }
    }
    rows.sort();
    rows.dedup();

    let mut out = std::io::BufWriter::new(fs::File::create(path)?);

    // Identity header (Rob-ruled 2026-08-29): which soul this file IS, so
    // a cache holding many souls' spines can attribute every fact and
    // reconstruct real file paths (repo + the fileId rows).
    let ry = git_lex::RepoYml::load(root);
    if let Some(sha) = crate::git::genesis_sha() {
        writeln!(out, "# genesis_sha: {}", &sha[..8.min(sha.len())])?;
    }
    if let Some(name) = &ry.agent_name {
        writeln!(out, "# soul: {name}")?;
    }
    writeln!(out, "# repo: {}", org_repo(root))?;

    if base_used {
        writeln!(out, "@base <{SPINE_BASE}>")?;
    }
    used.sort();
    for i in used {
        let (name, ns) = &bindings[i];
        writeln!(out, "@prefix {name} <{ns}>")?;
    }
    writeln!(out)?;
    writeln!(out, "?s\t?p\t?o")?;
    let n = rows.len() as u64;
    for row in rows {
        writeln!(out, "{row}")?;
    }
    out.flush()?;
    Ok(n)
}

/// `org/repo` from the root path's last two components — the piece a
/// reader combines with a fileId row to reconstruct a real path.
fn org_repo(root: &Path) -> String {
    let repo = root.file_name().map(|s| s.to_string_lossy().into_owned());
    let org = root
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned());
    match (org, repo) {
        (Some(o), Some(r)) => format!("{o}/{r}"),
        (None, Some(r)) => r,
        _ => String::new(),
    }
}

/// A raw tab inside a literal is legal N-Quads and would break the column
/// structure; escape it. Newlines/quotes/backslashes arrive already
/// escaped by the N-Quads serializer and pass through untouched.
fn escape_tsv(term: String) -> String {
    if term.contains('\t') {
        term.replace('\t', "\\t")
    } else {
        term
    }
}

/// Objects need one extra touch beyond IRI shortening: a typed literal's
/// datatype IRI gets shortened too ("..."^^xsd:date).
fn shorten_object(term: String, shorten: &mut impl FnMut(String) -> String) -> String {
    if term.starts_with('"') {
        match term.rfind("\"^^<") {
            Some(i) => {
                let (lex, dt) = term.split_at(i + 3); // keep `"^^`, shorten `<iri>`
                format!("{lex}{}", shorten(dt.to_string()))
            }
            None => term,
        }
    } else {
        shorten(term)
    }
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

/// `manifest.json` — the one small file a consumer polls to learn which
/// spine is current. TWO WRITERS share this file (pythia adds cache keys),
/// so git-lex rewrites ONLY the keys it owns and preserves the rest.
fn write_manifest(dir: &Path, sha: &str, spine: &Path) {
    let path = dir.join("manifest.json");
    let mut doc: serde_json::Value = fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(obj) = doc.as_object_mut() {
        obj.insert("format".into(), serde_json::json!("spine-tsv"));
        obj.insert("commit".into(), serde_json::json!(sha));
        obj.insert("spine".into(), serde_json::json!(format!("{sha}.spine.tsv")));
        obj.insert("spine_bytes".into(), serde_json::json!(file_len(spine)));
        // Parquet-era keys, retired with the format.
        obj.remove("file");
        obj.remove("bytes");
        obj.remove("deltas");
    }
    let tmp = dir.join("manifest.json.tmp");
    let body = format!("{}\n", serde_json::to_string_pretty(&doc).unwrap_or_default());
    if fs::write(&tmp, &body).and_then(|_| fs::rename(&tmp, &path)).is_err() {
        eprintln!(
            "warning: could not write {} — the spine itself is fine",
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

    #[test]
    fn tsv_escaping_and_datatype_shortening() {
        // Raw tabs are legal inside N-Quads literals and must not break
        // the column structure; everything else arrives pre-escaped.
        assert_eq!(escape_tsv("\"a\tb\"".into()), "\"a\\tb\"");
        assert_eq!(escape_tsv("\"plain\"".into()), "\"plain\"");
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
    }

    /// The spine must be byte-deterministic — same store content, same
    /// bytes — so cache keys on the file hash are trustworthy. Fails the
    /// moment someone adds nondeterminism (unsorted iteration, a
    /// timestamp, a hash-ordered map) to the writer.
    #[test]
    fn spine_is_byte_deterministic_and_scoped() {
        use oxigraph::model::{Literal, Quad};
        let store = Store::new().unwrap();
        let mk = |g: &str| NamedNode::new(graph_uri(g)).unwrap();
        let quad = |g: NamedNode, s: &str, o: &str| {
            Quad::new(
                NamedNode::new(s).unwrap(),
                NamedNode::new("https://repolex.ai/ontology/soul/title").unwrap(),
                Literal::new_simple_literal(o),
                g,
            )
        };
        store.insert(&quad(mk("now"), "https://repolex.ai/soul/Note/a", "A")).unwrap();
        store.insert(&quad(mk("now"), "https://repolex.ai/soul/Note/b", "tab\there")).unwrap();
        store.insert(&quad(mk("repo-ontology"), "https://repolex.ai/soul/Note/c", "C")).unwrap();
        store.insert(&quad(mk("commits"), "https://repolex.ai/soul/Note/plumbing", "X")).unwrap();

        let dir = std::env::temp_dir().join(format!("glx-spine-tsv-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p1 = dir.join("one.spine.tsv");
        let p2 = dir.join("two.spine.tsv");
        let n1 = write_spine_file(&dir, &store, &p1).unwrap();
        let n2 = write_spine_file(&dir, &store, &p2).unwrap();
        assert_eq!(n1, 3, "now + repo-ontology rows only — plumbing excluded");
        assert_eq!(n1, n2);
        assert_eq!(fs::read(&p1).unwrap(), fs::read(&p2).unwrap(), "bytes must be identical");
        let text = fs::read_to_string(&p1).unwrap();
        assert!(text.contains("?s\t?p\t?o"), "SPARQL-TSV header row");
        assert!(text.contains("\"tab\\there\""), "literal tab escaped");
        assert!(!text.contains("plumbing"), "commits graph must not leak");
        let _ = fs::remove_dir_all(&dir);
    }
}
