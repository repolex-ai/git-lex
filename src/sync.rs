//! `git lex sync` — build/refresh the derived knowledge graphs from git.
//!
//! Regenerates the ephemeral virtual graphs (git2 layer),
//! appends new commits' statement events to the persistent one graph
//! (with resume-or-full-rebuild logic and a structural integrity check),
//! and materializes the `now` view from the one graph's base layer.

use std::io::Cursor;
use std::process::Command;
use std::time::Instant;

use oxigraph::io::RdfFormat;
use oxigraph::store::Store;

use git_lex::store_path;

use crate::git::graph_uri;
use crate::spo_events;
use crate::{open_or_create_store, require_git_root};

pub(crate) fn cmd_sync() {
    let start = Instant::now();

    let root = require_git_root();

    // Identity floor: wake (sync) fails loud on a soul repo missing its
    // root SOUL.md (#29 — restorable via kit-update).
    crate::soul_md::require_soul_md(&root);

    gate_default_branch(&root);


    // Identity: resolve + record the genesis SHA ONCE per sync. Authority
    // is repo.yml `genesis_sha:` (legacy `first_commit:` self-migrates);
    // identity.yml still written for Pool's boot-skip until its read cuts
    // over. IRIs no longer carry it — see git.rs Task-2 IRI families.
    crate::git::ensure_genesis_recorded();
    let store = open_or_create_store();

    // Get current HEAD commit
    let head_sha = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    if head_sha.is_empty() {
        println!("No commits yet. Nothing to sync.");
        return;
    }

    if fast_path_hit(&store, &root, &head_sha) {
        let elapsed = start.elapsed();
        println!(
            "Already synced at {} ({:.1}ms).",
            &head_sha[..8.min(head_sha.len())],
            elapsed.as_secs_f64() * 1000.0
        );
        return;
    }

    let onegraph_resume = resume_point(&store, &root);

    clear_derived_graphs(&store);

    heal_ontology_graph(&store);


    // Regenerate the git2 machinery layer (commits/signatures/refs/filetree)
    let git_nq = crate::git2_nquads::generate_git2_nquads();
    let git_count = git_nq.lines().count();
    store
        .load_from_reader(RdfFormat::NQuads, Cursor::new(git_nq.as_bytes()))
        .expect("failed to load git triples");

    // Extraction: the ONE working-tree walk WRITES both sidecar families
    // (.fm.spo + .md.spo — the one graph's source) and derives the
    // working-tree now view. The now view is NO LONGER loaded into the
    // store (Rob-ruled: the now graph died as a store product — the one
    // graph's base layer is current state); the text is still built here
    // because the sync report counts its facts.
    let resolver_ctx = crate::nquad::ResolverContext::build(&root);
    let (fm_nq, fm_errors) = crate::nquad::generate_frontmatter_nquads_with(
        &root,
        &resolver_ctx,
        crate::nquad::NowWalkOpts { write_sidecars: true, build_nquads: true },
    );
    if fm_errors > 0 {
        eprintln!(
            "warning: {fm_errors} live document(s) carry values the data rules reject (each is listed above with its file). \
These are in your WORKING FILES, not history — fix the listed files and the warning goes away for good."
        );
    }
    let fm_count = fm_nq.lines().filter(|l| !l.is_empty()).count();

    // ─── One-graph phase: append new commits' statement events.
    // Shares the SAME resolver context, so one-graph facts resolve
    // identically to now-view facts (and the indexes build once per sync,
    // not twice). ───
    sync_onegraph_phase(&store, &root, onegraph_resume, &resolver_ctx);

    // ─── Stale graph cleanup ───
    // Subsumed by the Phase-1 clear filter: every graph not on the keep-list
    // (the one graph + repo-ontology) is removed each sync — including the
    // RETIRED families (sync/<sha>, history, meta, changeset/, blame/) and
    // all legacy urn:soul:* names. Migration off every old layout is
    // automatic on the first new-binary sync.

    materialize_now_view(&store);

    store.flush().expect("failed to flush store");

    let elapsed = start.elapsed();

    println!(
        "Synced in {:.1}ms:",
        elapsed.as_secs_f64() * 1000.0
    );
    println!("  git2 layer: {} quads; extracted: {} now-view facts", git_count, fm_count);
    println!("Store: {}", store_path().unwrap().display());
}

fn gate_default_branch(root: &std::path::Path) {
    // ══ DESIGN DECISION (Rob-ruled 2026-07-28): git-lex tracks the DEFAULT
    // BRANCH, full stop. The semantic history is the history of the project
    // as a whole — branches earn their place in it by merging, which is
    // what git branches are for. This deliberately breaks from "track
    // whatever git state you're in":
    //   - "what is true now" is never ambiguous (no branch-dependent state);
    //   - the resume point can never be poisoned by commits from refs the
    //     walk never visits (the silent-skip failure the adversarial review
    //     demonstrated);
    //   - the model fits in one sentence for the docs.
    // NOT-CHOSEN alternative, recorded for future revisiting: per-branch
    // walking with an ancestor-filtered resume (one extra git call). It
    // prevents the skip bug but NOT the deeper ambiguity — after syncing
    // two diverged branches, the base layer reflects whichever synced
    // last. If real branch-tracking demand appears, that ambiguity is the
    // problem to solve first.
    let current = git_current_branch(root);
    let default = git_default_branch(root);
    match &current {
        Some(b) if *b == default => {}
        Some(b) => {
            eprintln!("sync tracks the default branch ('{default}') only — you are on '{b}'.");
            eprintln!("git-lex records the project's merged history; merge your branch, then sync.");
            std::process::exit(1);
        }
        None => {
            eprintln!("sync tracks the default branch ('{default}') only — HEAD is detached.");
            eprintln!("check out '{default}' and re-run.");
            std::process::exit(1);
        }
    }
}

    // ─── Fast path: already-synced no-op ───
    // If the commits graph already contains HEAD (the previous sync reached
    // this commit) AND the extract dir is clean (no uncommitted .spo
    // changes), every phase of sync would rebuild identical state. Skip.
    //
    // Contract this depends on: the oxigraph store is derived. If you've
    // manually mutated it, rebuild via `rm -rf .lex/_ignore/oxigraph`.
fn fast_path_hit(store: &Store, root: &std::path::Path, head_sha: &str) -> bool {
    let probe = format!(
        "ASK {{ GRAPH <{}> {{ <https://repolex.ai/git-lex/git2/Commit/{}> ?p ?o }} }}",
        graph_uri("commits"), head_sha
    );
    let already_synced = oxigraph::sparql::SparqlEvaluator::new()
        .parse_query(&probe)
        .ok()
        .and_then(|q| q.on_store(&store).execute().ok())
        .map(|r| matches!(r, oxigraph::sparql::QueryResults::Boolean(true)))
        .unwrap_or(false);

    // The fast path also requires the one graph to EXIST — an
    // already-synced store from before the one-graph era (or one whose
    // graph was cleared) must fall through so the phase builds it.
    let onegraph_present = {
        let probe = format!(
            "ASK {{ GRAPH <{}> {{ ?s ?p ?o }} }}",
            spo_events::LEXHISTORY_GRAPH_IRI
        );
        oxigraph::sparql::SparqlEvaluator::new()
            .parse_query(&probe)
            .ok()
            .and_then(|q| q.on_store(&store).execute().ok())
            .map(|r| matches!(r, oxigraph::sparql::QueryResults::Boolean(true)))
            .unwrap_or(false)
    };

    // The fast path must also be format-current: an old-subject-model
    // store (pre-re-anchor) with no new commits would otherwise report
    // "already synced" forever and never take the one-time cutover
    // rebuild. Same probe as the resume check below. A repo with no
    // sidecar-bearing files never has File facts and so never fast-
    // paths — a full sync of an empty extract tree is cheap.
    let reanchored = {
        let probe = format!(
            "ASK {{ GRAPH <{}> {{ ?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> \
             <https://repolex.ai/ontology/git-lex/File> }} }}",
            spo_events::LEXHISTORY_GRAPH_IRI
        );
        oxigraph::sparql::SparqlEvaluator::new()
            .parse_query(&probe)
            .ok()
            .and_then(|q| q.on_store(&store).execute().ok())
            .map(|r| matches!(r, oxigraph::sparql::QueryResults::Boolean(true)))
            .unwrap_or(false)
    };

    if already_synced && onegraph_present && reanchored {
        // Check .lex/extract/ for uncommitted .spo changes
        let dirty = Command::new("git")
            .args(["status", "--porcelain", "--", ".lex/extract/"])
            .current_dir(root)
            .output()
            .ok()
            .map(|o| !o.stdout.is_empty())
            .unwrap_or(true); // on error, fall through to full sync
        if dirty {
            return false;
        }

        // Rewind probe (#107): "HEAD is in the commits graph" is satisfied
        // by ANY previously-synced ancestor — after `git reset --hard`,
        // HEAD is exactly that, and this fast path would print "Already
        // synced" over a one graph still carrying the rewound-away
        // commits' events. Fall through; resume_point prints the loud
        // line and forces the full rebuild.
        return rewound_event_commits(store, root).is_empty();
    }
    false
}

/// Rewind probe (#107): commits the one graph WITNESSED that are no longer
/// on the default branch's line.
///
/// After `git reset --hard` (or a rebase), HEAD is an ancestor the store
/// already synced — so the fast path's "is HEAD in the commits graph?"
/// answers yes, and the resume point (newest stored ancestor-of-HEAD) is
/// HEAD itself, so the append phase appends nothing. Both checks look only
/// at commits that ARE on the line; neither can see events from commits
/// that no longer are. Result before this probe: rewound-away commits'
/// statement events stayed in the one graph forever, and the now view kept
/// describing a history the branch no longer has.
///
/// The one graph itself is the honest instrument: every event names the
/// commit it was witnessed in (assertedIn/retractedIn), and the walk only
/// ever follows the default branch — so every witnessed commit MUST be an
/// ancestor of HEAD. Any that isn't means the line was rewritten, and the
/// graph must be rebuilt from the line that exists now (same law as fetch
/// scope vs rebuild scope: a total change to the source needs a total
/// rebuild of the derivation).
///
/// One SPARQL DISTINCT + one `git rev-list HEAD` set. On git failure this
/// returns empty (no forced rebuild): the sync phases run their own
/// rev-list with a loud exit, so a broken repo fails there, not silently
/// here.
fn rewound_event_commits(store: &Store, root: &std::path::Path) -> Vec<String> {
    let q = format!(
        "SELECT DISTINCT ?c WHERE {{ GRAPH <{}> {{ \
           {{ ?e <{}> ?c }} UNION {{ ?e <{}> ?c }} }} }}",
        spo_events::LEXHISTORY_GRAPH_IRI,
        spo_events::ONEGRAPH_ASSERTED_IN,
        spo_events::ONEGRAPH_RETRACTED_IN
    );
    let commit_prefix = crate::git2_nquads::git2_uri("Commit/");
    let witnessed: Vec<String> = oxigraph::sparql::SparqlEvaluator::new()
        .parse_query(&q)
        .ok()
        .and_then(|q| q.on_store(store).execute().ok())
        .map(|r| match r {
            oxigraph::sparql::QueryResults::Solutions(sols) => sols
                .flatten()
                .filter_map(|s| match s.get("c") {
                    Some(oxigraph::model::Term::NamedNode(n)) => n
                        .as_str()
                        .strip_prefix(commit_prefix.as_str())
                        .map(|sha| sha.to_string()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        })
        .unwrap_or_default();
    if witnessed.is_empty() {
        return Vec::new();
    }
    let out = Command::new("git")
        .args(["rev-list", "HEAD"])
        .current_dir(root)
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let on_line: std::collections::HashSet<&str> =
        stdout.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
    witnessed
        .into_iter()
        .filter(|sha| !on_line.contains(sha.as_str()))
        .collect()
}

fn resume_point(store: &Store, root: &std::path::Path) -> Option<String> {
    // ─── Rewind check FIRST (#107): if the one graph witnessed commits
    // that are no longer on the default branch's line, no resume point is
    // valid — the graph describes a history the branch no longer has, and
    // appending onto it would keep the phantom events forever. Full
    // rebuild (the walk engine clears the one graph when resume is None),
    // so the store equals what a fresh clone would derive from the line
    // that exists now.
    let rewound = rewound_event_commits(store, root);
    if !rewound.is_empty() {
        println!(
            "One graph: history rewind detected — {} commit(s) it witnessed are no longer \
             on the default branch (git reset/rebase). FULL rebuild from the current line; \
             the rewound commits' events are dropped with their commits.",
            rewound.len()
        );
        return None;
    }

    // ─── One-graph resume point: read BEFORE Phase 1 clears the commits
    // graph. The resume commit = the NEWEST commit in the PREVIOUS sync's
    // commits graph that is an ANCESTOR OF HEAD. No stored marker
    // (Rob-ruled): the persisted commit data IS the marker — a no-change
    // commit still lands in the commits graph, so "newest in store" is the
    // true high-water mark. The ancestor gate matters (review-HIGH): the
    // commits graph is built from ALL refs (branches, tags, remotes —
    // git2_nquads push_glob("*")) while the walk covers only HEAD's line;
    // taking the bare max ordinal let a feature-branch or fetched-ahead
    // remote tip become the resume point, silently skipping the HEAD
    // commits between the fork and now.
    let onegraph_resume: Option<String> = {
        let q = format!(
            "SELECT ?sha WHERE {{ GRAPH <{}> {{ \
               ?c <https://repolex.ai/ontology/git-lex/git2/ordinalDerived> ?o ; \
                  <https://repolex.ai/ontology/git-lex/git2/id> ?sha }} \
             }} ORDER BY DESC(?o)",
            graph_uri("commits")
        );
        let is_head_ancestor = |sha: &str| -> bool {
            std::process::Command::new("git")
                .args(["merge-base", "--is-ancestor", sha, "HEAD"])
                .current_dir(root)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        oxigraph::sparql::SparqlEvaluator::new()
            .parse_query(&q)
            .ok()
            .and_then(|q| q.on_store(&store).execute().ok())
            .and_then(|r| match r {
                oxigraph::sparql::QueryResults::Solutions(sols) => sols
                    .flatten()
                    .filter_map(|s| {
                        s.get("sha").map(|t| match t {
                            oxigraph::model::Term::Literal(l) => l.value().to_string(),
                            other => other.to_string(),
                        })
                    })
                    .find(|sha| is_head_ancestor(sha)),
                _ => None,
            })
    };

    // ─── Re-anchor format probe (identity model cutover, 2026-08-02) ───
    // A one graph built by the pre-re-anchor emitter carries path-family
    // subjects and ZERO `git-lex:File` type facts (the re-anchored emitter
    // asserts one per sidecar-bearing file). Resuming onto such a store
    // would mix two subject models in one graph — force the full rebuild
    // instead. Derived probe, no stored marker: same ethos as the resume
    // point ("the persisted data IS the marker").
    let onegraph_resume: Option<String> = match onegraph_resume {
        Some(sha) => {
            let probe = format!(
                "ASK {{ GRAPH <{}> {{ ?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> \
                 <https://repolex.ai/ontology/git-lex/File> }} }}",
                spo_events::LEXHISTORY_GRAPH_IRI
            );
            let file_typed = oxigraph::sparql::SparqlEvaluator::new()
                .parse_query(&probe)
                .ok()
                .and_then(|q| q.on_store(&store).execute().ok())
                .map(|r| matches!(r, oxigraph::sparql::QueryResults::Boolean(true)))
                .unwrap_or(false);
            if file_typed {
                Some(sha)
            } else {
                println!(
                    "One graph: pre-re-anchor subject model detected — FULL rebuild under \
                     the identity-model emitter (one-time cutover)."
                );
                None
            }
        }
        None => None,
    };
    onegraph_resume
}

fn clear_derived_graphs(store: &Store) {
    // ─── Phase 1: Clear and regenerate virtual graphs ───
    // Virtual graphs are ephemeral — rebuilt from git every sync.
    // We clear ALL graphs that aren't /sync/ graphs, then reload.
    // Sync graphs are persistent — never touched.

    // Find all existing graph names
    // Enumerate via named_graphs(), NOT a GRAPH ?g pattern — a pattern query
    // only sees graphs holding at least one triple, so an already-empty legacy
    // graph would linger registered forever.
    let existing_graphs: Vec<String> = store
        .named_graphs()
        .filter_map(|g| g.ok())
        .map(|g| match g {
            oxigraph::model::NamedOrBlankNode::NamedNode(n) => n.as_str().to_string(),
            other => other.to_string(),
        })
        .collect();

    // Clear non-sync, non-history graphs (virtual graphs get regenerated).
    // History and meta graphs are persistent — managed by Phase 4.
    for graph_uri in &existing_graphs {
        // Keep-list: the one graph (persistent, append-only — incremental
        // appends; full rebuild only via the spike command or an
        // invalid-resume fallback) and the repo-ontology graph (loaded at
        // init/kit-update, "stays put"). EVERYTHING else is derived and
        // regenerated — including the retired sync/<sha>, history, and meta
        // families, which this sweep removes from pre-cutover stores.
        if graph_uri != "https://repolex.ai/git-lex/NamedGraph/repo-ontology"
            && graph_uri != spo_events::LEXHISTORY_GRAPH_IRI
        {
            if let Ok(graph) = oxigraph::model::NamedNode::new(graph_uri) {
                // remove (not clear): drops the graph's registration too, so a
                // one-time legacy name (urn:soul:*) doesn't linger as an empty
                // graph in the store forever.
                if let Err(e) = store.remove_named_graph(&graph) {
                    eprintln!("warning: failed to clear graph {}: {} — stale triples may mix with the regeneration", graph_uri, e);
                }
            }
        }
    }
}

fn heal_ontology_graph(store: &Store) {
    // t-box self-heal (#81): the repo-ontology graph persists and is loaded
    // at init/kit-update ("stays put", Rob Day-50) — but a fresh store
    // (deleted for a rebuild) starts EMPTY, which forced the cure sequence
    // "kit-update → rm store → sync → kit-update": the second update only
    // reloaded vocabulary already sitting on disk. If the graph is empty
    // and installed TTLs exist, load them now. Verify's empty-graph refusal
    // still stands when no kits are installed (nothing on disk to load).
    let ont_empty = match oxigraph::model::NamedNode::new(
        "https://repolex.ai/git-lex/NamedGraph/repo-ontology",
    ) {
        Ok(g) => store
            .quads_for_pattern(None, None, None, Some(g.as_ref().into()))
            .next()
            .is_none(),
        Err(_) => false,
    };
    if ont_empty {
        let n = crate::nquad::load_ontology_graph(&store);
        if n > 0 {
            println!(
                "Ontology graph was empty (fresh store) — loaded {} kit ttl file(s) from disk",
                n
            );
        }
    }
}

    // ─── Materialize the now VIEW ───
    // NamedGraph/now = the one graph's base layer (current facts), copied
    // out as a standalone graph each sync. This is a VIEW in the ruled sense
    // ("'now' is a view — a query, OR A MATERIALIZED GRAPH, derived from the
    // one graph"): derived, disposable, rebuilt every sync, never edited.
    // It exists so downstream consumers (Syrinx, viz, agents) can query
    // current state as plain triples without filtering event machinery.
fn materialize_now_view(store: &Store) {
    let update = "PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>              PREFIX gl: <https://repolex.ai/ontology/git-lex/>              DROP SILENT GRAPH <https://repolex.ai/git-lex/NamedGraph/now> ;              INSERT { GRAPH <https://repolex.ai/git-lex/NamedGraph/now> { ?s ?p ?o } }              WHERE { GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> { ?s ?p ?o .                        FILTER NOT EXISTS { ?s a gl:SpoEvent }                        FILTER(?p != rdf:reifies) } }";
    match oxigraph::sparql::SparqlEvaluator::new().parse_update(update) {
        Ok(u) => {
            if let Err(e) = u.on_store(&store).execute() {
                // A stale now view silently lies to every downstream
                // consumer (Syrinx, viz, agents) — fail the sync.
                eprintln!("ERROR: now-view materialization failed: {e}");
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("ERROR: now-view update did not parse (binary bug): {e}");
            std::process::exit(1);
        }
    }
}


/// How many facts the HISTORY says are true right now — the derived half of
/// the state-parity check, compared against `base_count` (what the base layer
/// actually holds). A disagreement means the store is corrupt.
///
/// ── Why this shape (2026-08-23) ──────────────────────────────────────────
/// A statement is live when its LATEST assertion is later than its LATEST
/// retraction (and trivially live when it was never retracted). Two grouped
/// MAX aggregates and one comparison — each side scanned once.
///
/// The previous formulation asked the equivalent question the other way:
/// "does SOME assertion of this statement have no retraction at-or-after
/// it?", as a correlated FILTER NOT EXISTS carrying a two-graph join. The
/// planner ran that inner join once per candidate assertion, so cost grew
/// with events SQUARED while the aggregate form grows linearly. Measured
/// head-to-head on real stores, same answer both ways:
///
///     W4R3Z (24k quads,  7,237 events):   4,322 ms →     139 ms   (31x)
///     lUX (479k quads, 132,456 events): 844,446 ms →   1,560 ms  (541x)
///
/// On lUX that one query WAS a one-commit sync: 14m04s of a 14m44s run.
///
/// EQUIVALENCE (the claim the tests below pin, including the boundary):
///   - maxAssert > maxRetract → the assertion at maxAssert has nothing
///     at-or-after it, so the old query counts it. Live both ways.
///   - maxAssert <= maxRetract → EVERY assertion has the retraction at
///     maxRetract at-or-after it, so the old query counts none of them.
///     Dead both ways — including maxAssert == maxRetract, i.e. asserted
///     and retracted in the SAME commit, which both forms treat as dead.
///   - Never asserted → neither form counts it (both are driven by asserts).
const DERIVED_COUNT_Q: &str = "\
PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> \
PREFIX gl: <https://repolex.ai/ontology/git-lex/> \
PREFIX g2: <https://repolex.ai/ontology/git-lex/git2/> \
SELECT (COUNT(*) AS ?n) WHERE { \
  { SELECT ?tt (MAX(?oa) AS ?maxA) WHERE { \
      GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> { ?a rdf:reifies ?tt ; gl:assertedIn ?ca } \
      GRAPH <https://repolex.ai/git-lex/NamedGraph/commits> { ?ca g2:ordinalDerived ?oa } \
    } GROUP BY ?tt } \
  OPTIONAL { SELECT ?tt (MAX(?orr) AS ?maxR) WHERE { \
      GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> { ?r rdf:reifies ?tt ; gl:retractedIn ?cr } \
      GRAPH <https://repolex.ai/git-lex/NamedGraph/commits> { ?cr g2:ordinalDerived ?orr } \
    } GROUP BY ?tt } \
  FILTER(!BOUND(?maxR) || ?maxR < ?maxA) }";

fn sync_onegraph_phase(store: &Store, root: &std::path::Path, resume_sha: Option<String>, ctx: &crate::nquad::ResolverContext) {
    let one_graph_uri = format!("<{}>", spo_events::LEXHISTORY_GRAPH_IRI);

    // Which commits are new?
    let commit_exists = |sha: &str| -> bool {
        Command::new("git")
            .args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
            .current_dir(root)
            .status()
            .map(|st| st.success())
            .unwrap_or(false)
    };
    // A rev-list failure must NOT read as "no new commits" — that would make
    // sync print "up to date" over a range it never walked. Fail the sync.
    let rev_list = |range: &[&str]| -> Vec<String> {
        let mut args = vec!["rev-list", "--topo-order", "--reverse"];
        args.extend_from_slice(range);
        let out = Command::new("git")
            .args(&args)
            .current_dir(root)
            .output()
            .unwrap_or_else(|e| {
                eprintln!("ERROR: git rev-list spawn failed: {e}");
                std::process::exit(1);
            });
        if !out.status.success() {
            eprintln!(
                "ERROR: git rev-list {:?} failed ({}): {}",
                range,
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
            std::process::exit(1);
        }
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    };

    // Belt-and-braces on top of the main-only gate: a resume commit that
    // is not an ancestor of HEAD can only mean external interference
    // (manual store surgery, a force-push that kept the sha alive on
    // another ref). Never walk past it — fall back to a full rebuild.
    let is_ancestor_of_head = |sha: &str| -> bool {
        Command::new("git")
            .current_dir(root)
            .args(["merge-base", "--is-ancestor", sha, "HEAD"])
            .status()
            .map(|st| st.success())
            .unwrap_or(false)
    };

    let (mut shas, full_rebuild) = match &resume_sha {
        Some(sha) if commit_exists(sha) && is_ancestor_of_head(sha) => {
            let exclude = format!("^{sha}");
            (rev_list(&[exclude.as_str(), "HEAD"]), false)
        }
        Some(sha) => {
            eprintln!(
                "warning: one-graph resume commit {sha} is gone or not an ancestor of HEAD (history rewritten?) — FULL one-graph rebuild"
            );
            (rev_list(&["HEAD"]), true)
        }
        None => (rev_list(&["HEAD"]), true),
    };

    // DEV-ONLY horizon (see resolve_dev_horizon): on a full rebuild, drop
    // everything before the horizon commit; it becomes the walk's first
    // commit and diffs against the empty tree (the whole tree asserts as
    // of the horizon).
    let mut horizon_start: Option<String> = None;
    if full_rebuild {
        if let Some(h) = resolve_dev_horizon(root) {
            if let Some(pos) = shas.iter().position(|s| *s == h) {
                let dropped = pos;
                shas.drain(..pos);
                horizon_start = Some(h);
                println!(
                    "One graph: dev_history_horizon active — {dropped} pre-horizon commit(s) excluded from the walk."
                );
            }
        }
    }

    if !shas.is_empty() {
        let commits = match spo_events::collect_commits_from_shas(&shas, horizon_start.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("ERROR: could not read commit diffs: {e}");
                eprintln!("Sync aborted; the one graph was not updated. A failing diff usually means repository corruption — run `git fsck`.");
                std::process::exit(1);
            }
        };

        let (seen, emitted) = match spo_events::onegraph_walk_engine(
            &commits,
            store,
            &one_graph_uri,
            ctx,
            false, // show_progress — sync prints its own phase summary
            full_rebuild, // clear_first only on a full rebuild
        ) {
            Ok(counts) => counts,
            Err(e) => {
                // The resume point is unchanged (events load at the end of the
                // walk), so the next sync retries this same commit range.
                eprintln!("ERROR: one-graph build failed: {e}");
                eprintln!("Sync aborted; the one graph was not updated for this commit range. Fix the cause and re-run `git lex sync`.");
                std::process::exit(1);
            }
        };
        println!(
            "One graph: {} {} commit(s), {} event(s) seen, {} emitted.",
            if full_rebuild { "full rebuild —" } else { "appended" },
            commits.len(),
            seen,
            emitted
        );
    } else {
        println!("One graph: up to date.");
    }

    // Discovery typing (default graph, idempotent): the graph's NamedGraph
    // object, dual-typed — the store does no inference, so both the class and
    // its NamedGraph parent are stated explicitly.
    let typing = format!(
        "<{g}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/LexHistoryGraph> .\n\
         <{g}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/NamedGraph> .\n",
        g = spo_events::LEXHISTORY_GRAPH_IRI
    );
    if let Err(e) = store.load_from_reader(RdfFormat::NQuads, Cursor::new(typing.as_bytes())) {
        eprintln!("ERROR: one-graph discovery typing failed to load: {e}");
        std::process::exit(1);
    }

    // Structural integrity (runs EVERY build): each SpoEvent has exactly one
    // statement (rdf:reifies) and exactly one direction. A violation means a
    // 16-hex id collision or an emitter bug — LOUD, never silently deduped.
    let integrity = format!(
        "SELECT (COUNT(DISTINCT ?e) AS ?bad) WHERE {{ GRAPH <{}> {{ \
           {{ ?e <https://repolex.ai/ontology/git-lex/assertedIn> ?a ; \
                <https://repolex.ai/ontology/git-lex/retractedIn> ?r }} \
           UNION \
           {{ ?e <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> ?t1 , ?t2 . FILTER(?t1 != ?t2) }} \
           UNION \
           {{ ?e <https://repolex.ai/ontology/git-lex/assertedIn> ?c1 , ?c2 . FILTER(?c1 != ?c2) }} \
           UNION \
           {{ ?e <https://repolex.ai/ontology/git-lex/retractedIn> ?d1 , ?d2 . FILTER(?d1 != ?d2) }} \
        }} }}",
        spo_events::LEXHISTORY_GRAPH_IRI
    );
    // The check itself failing to run is ALSO a failure — an unverified graph
    // must not report a successful sync (`unwrap_or(0)` here used to turn a
    // broken query into a silent pass).
    let bad = oxigraph::sparql::SparqlEvaluator::new()
        .parse_query(&integrity)
        .ok()
        .and_then(|q| q.on_store(store).execute().ok())
        .and_then(|r| match r {
            oxigraph::sparql::QueryResults::Solutions(mut sols) => sols
                .next()
                .and_then(|s| s.ok())
                .and_then(|s| s.get("bad").map(|t| t.to_string())),
            _ => None,
        })
        .and_then(|v| v.split('"').nth(1).and_then(|n| n.parse::<u64>().ok()));
    match bad {
        None => {
            eprintln!("ERROR: one-graph integrity check could not run (query failed) — the graph is unverified.");
            std::process::exit(1);
        }
        Some(bad) if bad > 0 => {
            eprintln!(
                "ERROR: one-graph integrity check FAILED — {bad} SpoEvent node(s) violate one-statement/one-direction (16-hex id collision or emitter bug). The graph is NOT trustworthy until this is resolved."
            );
            std::process::exit(1);
        }
        _ => {}
    }
    // ── Commit joins + state-parity (promoted from `verify` before its
    // removal — Rob-ruled 2026-07-29: every sync proves the store coherent
    // or aborts; the strongest corruption detector runs on every build).
    let count_q = |q: &str| -> Option<u64> {
        match git_lex::eval_query(store, q) {
            Ok(oxigraph::sparql::QueryResults::Solutions(mut sols)) => sols
                .next()
                .and_then(|r| r.ok())
                .and_then(|r| r.get("n").map(|t| t.to_string()))
                .and_then(|v| v.split('"').nth(1).and_then(|x| x.parse().ok())),
            _ => None,
        }
    };
    let dangling = count_q(
        "SELECT (COUNT(DISTINCT ?c) AS ?n) WHERE { \
           GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> { \
             { ?e <https://repolex.ai/ontology/git-lex/assertedIn> ?c } UNION \
             { ?e <https://repolex.ai/ontology/git-lex/retractedIn> ?c } } \
           FILTER NOT EXISTS { GRAPH <https://repolex.ai/git-lex/NamedGraph/commits> { ?c ?p ?o } } \
        }",
    );
    let base_count = count_q(
        "SELECT (COUNT(*) AS ?n) WHERE { GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> { \
           ?s ?p ?o . \
           FILTER NOT EXISTS { ?s a <https://repolex.ai/ontology/git-lex/SpoEvent> } \
           FILTER(?p != <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies>) } }",
    );
    let derived_count = count_q(DERIVED_COUNT_Q);
    match (dangling, base_count, derived_count) {
        (Some(0), Some(b), Some(d)) if b == d => {}
        (None, _, _) | (_, None, _) | (_, _, None) => {
            eprintln!("ERROR: store coherence checks could not run — the graph is unverified.");
            std::process::exit(1);
        }
        (Some(dg), _, _) if dg > 0 => {
            eprintln!("ERROR: {dg} history event commit(s) missing from the commits graph — the store is incoherent.");
            std::process::exit(1);
        }
        (_, Some(b), Some(d)) => {
            eprintln!("ERROR: current state ({b} facts) disagrees with what the history derives ({d}) — the store is corrupt. Delete .lex/_ignore/oxigraph and re-run `git lex sync` to rebuild.");
            std::process::exit(1);
        }
    }
}

/// The branch HEAD is on, or None when detached.
fn git_current_branch(root: &std::path::Path) -> Option<String> {
    let out = Command::new("git")
        .current_dir(root)
        .args(["symbolic-ref", "--short", "-q", "HEAD"])
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

/// The repo's default branch: `main` if it exists, else `master`, else
/// whatever branch HEAD is on (single-branch repos with custom names keep
/// working — there is nothing to diverge from).
fn git_default_branch(root: &std::path::Path) -> String {
    for cand in ["main", "master"] {
        let ok = Command::new("git")
            .current_dir(root)
            .args(["show-ref", "--verify", "--quiet", &format!("refs/heads/{cand}")])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return cand.to_string();
        }
    }
    git_current_branch(root).unwrap_or_else(|| "main".to_string())
}

/// Resolve `dev_history_horizon:` (a DATE in repo.yml) to the first commit
/// after it. DEV-ONLY: a stopgap so the ~10 squad repos that predate the
/// v1 data rules can exclude their pre-spec development history from the
/// graph without touching git. Normal repos never set this. The first
/// walked commit diffs against the EMPTY tree, so the whole tree asserts
/// as of the horizon — untouched old documents keep their facts; only the
/// pre-horizon CHURN is excluded.
fn resolve_dev_horizon(root: &std::path::Path) -> Option<String> {
    let date = git_lex::RepoYml::load(root).dev_history_horizon?;
    let out = Command::new("git")
        .current_dir(root)
        .args(["rev-list", "--reverse", "--after", date.trim(), "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let first = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty());
    if first.is_none() {
        eprintln!("warning: dev_history_horizon '{date}' matches no commit — walking full history");
    }
    first
}

#[cfg(test)]
mod derived_count_tests {
    use super::*;
    use oxigraph::store::Store;

    /// The formulation `DERIVED_COUNT_Q` replaced (2026-08-23). Kept HERE, in
    /// the tests only, as the ORACLE: every fixture asserts new == old, so the
    /// rewrite is proved equivalent rather than pinned to a number someone
    /// later "fixes" to match a regression.
    const OLD_DERIVED_COUNT_Q: &str = "\
PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> \
PREFIX gl: <https://repolex.ai/ontology/git-lex/> \
PREFIX g2: <https://repolex.ai/ontology/git-lex/git2/> \
SELECT (COUNT(DISTINCT ?tt) AS ?n) WHERE { \
  GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> { ?a rdf:reifies ?tt ; gl:assertedIn ?ca } \
  GRAPH <https://repolex.ai/git-lex/NamedGraph/commits> { ?ca g2:ordinalDerived ?oa } \
  FILTER NOT EXISTS { \
    GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> { ?r rdf:reifies ?tt ; gl:retractedIn ?cr } \
    GRAPH <https://repolex.ai/git-lex/NamedGraph/commits> { ?cr g2:ordinalDerived ?or } \
    FILTER(?or >= ?oa) } }";

    fn count(store: &Store, q: &str) -> u64 {
        match git_lex::eval_query(store, q) {
            Ok(oxigraph::sparql::QueryResults::Solutions(mut sols)) => sols
                .next()
                .and_then(|r| r.ok())
                .and_then(|r| r.get("n").map(|t| t.to_string()))
                .and_then(|v| v.split('"').nth(1).and_then(|x| x.parse().ok()))
                .expect("count query returned no usable row"),
            other => panic!("count query failed: {:?}", other.is_ok()),
        }
    }

    /// Build a store from a list of `(statement_id, direction, commit_ordinal)`
    /// events. Each statement is a distinct reified triple; each ordinal is a
    /// distinct commit in the commits graph. `dir` is "a" (assert) or "r".
    fn store_with(events: &[(&str, &str, i64)]) -> Store {
        let store = Store::new().unwrap();
        let lh = "https://repolex.ai/git-lex/LexHistoryGraph";
        let cg = "https://repolex.ai/git-lex/NamedGraph/commits";
        let mut nq = String::new();
        let mut ordinals: Vec<i64> = events.iter().map(|(_, _, o)| *o).collect();
        ordinals.sort_unstable();
        ordinals.dedup();
        for o in &ordinals {
            nq.push_str(&format!(
                "<https://ex/c{o}> <https://repolex.ai/ontology/git-lex/git2/ordinalDerived> \
                 \"{o}\"^^<http://www.w3.org/2001/XMLSchema#integer> <{cg}> .\n"
            ));
        }
        for (i, (stmt, dir, ord)) in events.iter().enumerate() {
            let pred = if *dir == "a" { "assertedIn" } else { "retractedIn" };
            // One event node per event; all events for a statement reify the
            // SAME triple term, which is what makes them the same statement.
            nq.push_str(&format!(
                "<https://ex/e{i}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> \
                 <<( <https://ex/s{stmt}> <https://ex/p> \"v{stmt}\" )>> <{lh}> .\n\
                 <https://ex/e{i}> <https://repolex.ai/ontology/git-lex/{pred}> \
                 <https://ex/c{ord}> <{lh}> .\n"
            ));
        }
        store
            .load_from_reader(RdfFormat::NQuads, Cursor::new(nq.as_bytes()))
            .expect("fixture n-quads failed to load");
        store
    }

    fn assert_agree(events: &[(&str, &str, i64)], expected_live: u64) {
        let store = store_with(events);
        let new = count(&store, DERIVED_COUNT_Q);
        let old = count(&store, OLD_DERIVED_COUNT_Q);
        assert_eq!(new, old, "rewrite disagrees with the oracle on {events:?}");
        assert_eq!(new, expected_live, "wrong live count for {events:?}");
    }

    #[test]
    fn asserted_never_retracted_is_live() {
        assert_agree(&[("1", "a", 1), ("2", "a", 2)], 2);
    }

    #[test]
    fn retracted_after_assert_is_dead() {
        assert_agree(&[("1", "a", 1), ("1", "r", 2)], 0);
    }

    /// THE BOUNDARY: asserted and retracted in the SAME commit. The old form
    /// kills it via `?or >= ?oa`; the new form via `maxR < maxA` being false
    /// on equality. Both say dead — this is the case a careless rewrite to
    /// `>` / `<=` would silently flip.
    #[test]
    fn assert_and_retract_in_same_commit_is_dead() {
        assert_agree(&[("1", "a", 5), ("1", "r", 5)], 0);
    }

    #[test]
    fn re_asserted_after_retract_is_live_again() {
        assert_agree(&[("1", "a", 1), ("1", "r", 2), ("1", "a", 3)], 1);
    }

    /// Retract lands BETWEEN two asserts: latest assert (5) beats latest
    /// retract (3), so live. The old form finds the assert at 5 has nothing
    /// at-or-after it; the new form compares 5 > 3.
    #[test]
    fn interleaved_events_follow_the_latest() {
        assert_agree(&[("1", "a", 1), ("1", "r", 3), ("1", "a", 5)], 1);
    }

    /// Same shape, but the last event is the retraction — dead both ways.
    #[test]
    fn interleaved_ending_in_retract_is_dead() {
        assert_agree(&[("1", "a", 1), ("1", "a", 3), ("1", "r", 5)], 0);
    }

    #[test]
    fn mixed_population_counts_only_the_live() {
        assert_agree(
            &[
                ("1", "a", 1),                              // live
                ("2", "a", 1), ("2", "r", 2),               // dead
                ("3", "a", 1), ("3", "r", 2), ("3", "a", 4), // live again
                ("4", "a", 7), ("4", "r", 7),               // dead, same commit
            ],
            2,
        );
    }

    #[test]
    fn empty_graph_counts_zero() {
        assert_agree(&[], 0);
    }

    /// A retraction with no assertion anywhere is not a live fact — neither
    /// form is driven by retractions, so both ignore it.
    #[test]
    fn retract_without_assert_is_not_live() {
        assert_agree(&[("1", "r", 2)], 0);
    }
}
