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
        // Converge the spine even on the fast path: a no-op when current,
        // and it heals a spine that failed to write on an earlier sync.
        crate::export_spine::refresh_after_sync(&root, &store);
        crate::context::refresh(&root);
        return;
    }

    let onegraph_resume = validated_resume(&root, resume_point(&store, &root));
    // No resume point = the one graph is rebuilt from the first commit.
    let full_rebuild = onegraph_resume.is_none();

    clear_derived_graphs(&store);
    let now_present = store
        .contains_named_graph(&oxigraph::model::NamedNode::new_unchecked(NOW_GRAPH_IRI))
        .unwrap_or(false);

    heal_ontology_graph(&store);


    // Regenerate the git2 machinery layer (commits/signatures/refs/filetree).
    // An append loads it here, whole, as it always has. A full rebuild loads
    // it AFTER the one graph and the now view (see below): the rebuild
    // writes in batches (#15), and the git2 layer carries the sync marker.
    let mut git_count = 0;
    if !full_rebuild {
        git_count = load_git2_layer(&store, Git2Load::Whole);
    }

    // Extraction: the ONE working-tree walk WRITES both sidecar families
    // (.fm.spo + .md.spo — the one graph's source) and derives the
    // working-tree now view. The now view is NO LONGER loaded into the
    // store (Rob-ruled: the now graph died as a store product — the one
    // graph's base layer is current state), so its text is not built: the
    // sync report's fact count comes back from the walk as a number.
    let resolver_ctx = crate::nquad::ResolverContext::build(&root);
    let walk = crate::nquad::generate_frontmatter_nquads_with(
        &root,
        &resolver_ctx,
        crate::nquad::NowWalkOpts { write_sidecars: true, build_nquads: false },
    );
    let fm_errors = walk.errors;
    if fm_errors > 0 {
        eprintln!(
            "warning: {fm_errors} live document(s) carry values the data rules reject (each is listed above with its file). \
These are in your WORKING FILES, not history — fix the listed files and the warning goes away for good."
        );
    }
    let fm_count = walk.facts;

    // ─── One-graph phase: append new commits' statement events.
    // Shares the SAME resolver context, so one-graph facts resolve
    // identically to now-view facts (and the indexes build once per sync,
    // not twice). ───
    let onegraph = sync_onegraph_walk(&store, &root, onegraph_resume, &resolver_ctx);

    // ─── Stale graph cleanup ───
    // Subsumed by the Phase-1 clear filter: every graph not on the keep-list
    // (the one graph + repo-ontology) is removed each sync — including the
    // RETIRED families (sync/<sha>, history, meta, changeset/, blame/) and
    // all legacy urn:soul:* names. Migration off every old layout is
    // automatic on the first new-binary sync.

    // The now view follows the base layer. A full rebuild (or a store that
    // has no now view yet) copies all of it; otherwise only the subjects
    // this walk changed can differ, and only they are refreshed.
    //
    // ── Full rebuild: everything in batches, the marker LAST (#15) ──
    // Nothing is stored to say how far a sync got: the commits graph is the
    // marker (goodlux-ruled; see resume_point). A rebuild held in one store
    // transaction cost memory that grew with the whole of history, so the
    // rebuild writes the one graph, the now view and the git2 layer in
    // bounded batches — and the commit ordinals, which every "is this store
    // synced?" reader keys on, go in last, in one transaction. Killed at any
    // point before that, the store has no marker: the next sync finds no
    // resume point and rebuilds from the first commit.
    if full_rebuild {
        materialize_now_view_in_batches(&store);
        git_count = load_git2_layer(&store, Git2Load::BatchedMarkerLast);
    }

    // Every sync proves the store coherent or aborts. The proof joins
    // events to commit ordinals, so it follows the git2 layer.
    verify_onegraph(&store);

    if !full_rebuild {
        if now_present {
            refresh_now_view(&store, &onegraph.changed_subjects);
        } else {
            materialize_now_view(&store);
        }
    }

    store.flush().expect("failed to flush store");

    let elapsed = start.elapsed();

    println!(
        "Synced in {:.1}ms:",
        elapsed.as_secs_f64() * 1000.0
    );
    println!("  git2 layer: {} quads; extracted: {} now-view facts", git_count, fm_count);
    println!("Store: {}", store_path().unwrap().display());

    // Spine refresh — every sync, every repo, no gate (Rob-ruled
    // 2026-08-29). Kept AFTER the sync report: sync's own success is
    // already printed, and any failure here demotes to a warning, so a
    // cache artifact can never fail or mask a sync.
    crate::export_spine::refresh_after_sync(&root, &store);
    // The agent context is a function of the installed kits only; this is
    // the safety net behind kit-add/kit-remove/kit-update. Written only
    // when its bytes change.
    crate::context::refresh(&root);
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
    // The ordinal, not "any fact about HEAD": the ordinals are the sync
    // marker (SYNC_MARKER_PREDICATE), written last by a batched rebuild.
    let probe = format!(
        "ASK {{ GRAPH <{}> {{ <https://repolex.ai/git-lex/git2/Commit/{}> <{}> ?o }} }}",
        graph_uri("commits"), head_sha, SYNC_MARKER_PREDICATE
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
               ?c <{SYNC_MARKER_PREDICATE}> ?o ; \
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
    // We remove EVERY graph that is not on the keep-list below (the one
    // graph, repo-ontology, the now view), then reload. The old sync/<sha>
    // family is retired and is swept like any other graph.

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

    for graph_uri in &existing_graphs {
        // Keep-list: the one graph (persistent, append-only — incremental
        // appends; full rebuild only via the spike command or an
        // invalid-resume fallback), the repo-ontology graph (loaded at
        // init/kit-update, "stays put") and the now view (refreshed after
        // the walk from what the walk changed). EVERYTHING else is derived and
        // regenerated — including the retired sync/<sha>, history, and meta
        // families, which this sweep removes from pre-cutover stores.
        if graph_uri != "https://repolex.ai/git-lex/NamedGraph/repo-ontology"
            && graph_uri != spo_events::LEXHISTORY_GRAPH_IRI
            && graph_uri != NOW_GRAPH_IRI
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
const NOW_GRAPH_IRI: &str = "https://repolex.ai/git-lex/NamedGraph/now";

/// The now view restricted to some subjects: exactly what
/// `materialize_now_view` would copy for them. That query filters per
/// subject (no SpoEvent subjects, no rdf:reifies), so the view for every
/// subject the walk did not change is already right, and each changed
/// subject's facts are replaced with its current base-layer facts.
fn refresh_now_view(store: &Store, subjects: &std::collections::HashSet<String>) {
    use oxigraph::model::{GraphNameRef, NamedNodeRef, NamedOrBlankNodeRef, Quad};
    let fail = |e: String| -> ! {
        // A stale now view silently lies to every downstream consumer.
        eprintln!("ERROR: now-view refresh failed: {e}");
        std::process::exit(1);
    };
    let now = NamedNodeRef::new_unchecked(NOW_GRAPH_IRI);
    let one = NamedNodeRef::new_unchecked(spo_events::LEXHISTORY_GRAPH_IRI);
    let rdf_type = NamedNodeRef::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#type");
    let reifies = NamedNodeRef::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies");
    let spo_event = NamedNodeRef::new_unchecked("https://repolex.ai/ontology/git-lex/SpoEvent");
    for term in subjects {
        let iri = term.trim_start_matches('<').trim_end_matches('>');
        let Ok(subject) = NamedNodeRef::new(iri) else {
            fail(format!("changed subject is not an IRI: {term}"));
        };
        let subject_ref = NamedOrBlankNodeRef::from(subject);
        let stale: Vec<Quad> = store
            .quads_for_pattern(Some(subject_ref), None, None, Some(GraphNameRef::from(now)))
            .collect::<Result<_, _>>()
            .unwrap_or_else(|e| fail(e.to_string()));
        for q in &stale {
            store.remove(q).unwrap_or_else(|e| fail(e.to_string()));
        }
        let is_event = store
            .contains(oxigraph::model::QuadRef::new(subject_ref, rdf_type, spo_event, one))
            .unwrap_or_else(|e| fail(e.to_string()));
        if is_event {
            continue;
        }
        let current: Vec<Quad> = store
            .quads_for_pattern(Some(subject_ref), None, None, Some(GraphNameRef::from(one)))
            .collect::<Result<_, _>>()
            .unwrap_or_else(|e| fail(e.to_string()));
        for q in current {
            if q.predicate.as_ref() == reifies {
                continue;
            }
            let copy = Quad::new(q.subject, q.predicate, q.object, now);
            store.insert(&copy).unwrap_or_else(|e| fail(e.to_string()));
        }
    }
}

/// The sync marker: git2 commit ordinals in the commits graph.
///
/// Nothing is stored to say how far a sync got (goodlux-ruled) — the
/// persisted commit data IS the marker. Every reader that asks "is this
/// store synced, and to where?" keys on the ordinal: the fast path, the
/// resume point, and the spine's newest-synced-commit. So a rebuild that
/// writes in batches must write the ordinals LAST, in one transaction: a
/// store without them is a store no reader mistakes for a finished one.
const SYNC_MARKER_PREDICATE: &str = "https://repolex.ai/ontology/git-lex/git2/ordinalDerived";

/// How the git2 layer goes into the store.
enum Git2Load {
    /// One transaction — an append's load, unchanged.
    Whole,
    /// A full rebuild's load: bounded batches, the sync marker last (#15).
    /// One transaction held the whole layer in memory at about 3 KB a quad
    /// (measured: 1.03M quads, +2.9 GB), most of it the file tree.
    BatchedMarkerLast,
}

/// Regenerate the git2 layer (commits/signatures/refs/filetree) and load
/// it. Returns the number of quads.
fn load_git2_layer(store: &Store, how: Git2Load) -> usize {
    match how {
        Git2Load::Whole => {
            let git_nq = crate::git2_nquads::generate_git2_nquads();
            store
                .load_from_reader(RdfFormat::NQuads, Cursor::new(git_nq.as_bytes()))
                .expect("failed to load git triples");
            git_nq.lines().count()
        }
        Git2Load::BatchedMarkerLast => {
            let mut loader = MarkerLastLoader::new(store, spo_events::REBUILD_BATCH_QUADS);
            crate::git2_nquads::emit_git2_nquads(&mut loader);
            loader.finish().expect("failed to load git triples")
        }
    }
}

/// Is this N-Quads line a sync-marker quad? (`<s> <p> o <g> .` — the
/// subject is an IRI, so the predicate is the second space-separated term.)
fn is_sync_marker_line(line: &str) -> bool {
    line.split_once(' ')
        .and_then(|(_, rest)| rest.strip_prefix('<'))
        .and_then(|rest| rest.strip_prefix(SYNC_MARKER_PREDICATE))
        .is_some_and(|rest| rest.starts_with("> "))
}

/// Takes N-Quads text as it is produced and loads it `batch_quads` lines per
/// store transaction, holding back the sync-marker quads for one final
/// transaction ([`MarkerLastLoader::finish`]). The git2 layer carries no
/// blank nodes, so where a batch ends changes nothing that is stored.
struct MarkerLastLoader<'a> {
    store: &'a Store,
    batch_quads: usize,
    /// A line still waiting for its newline.
    carry: String,
    batch: String,
    held: usize,
    marker: String,
    lines: usize,
    /// The first load failure; nothing is loaded after it.
    error: Option<String>,
}

impl<'a> MarkerLastLoader<'a> {
    fn new(store: &'a Store, batch_quads: usize) -> Self {
        MarkerLastLoader {
            store,
            batch_quads: batch_quads.max(1),
            carry: String::new(),
            batch: String::new(),
            held: 0,
            marker: String::new(),
            lines: 0,
            error: None,
        }
    }

    fn line(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        self.lines += 1;
        if is_sync_marker_line(line) {
            self.marker.push_str(line);
            return;
        }
        self.batch.push_str(line);
        self.held += 1;
        if self.held >= self.batch_quads {
            self.load_batch();
        }
    }

    fn load_batch(&mut self) {
        if self.error.is_none()
            && let Err(e) = self
                .store
                .load_from_reader(RdfFormat::NQuads, Cursor::new(self.batch.as_bytes()))
        {
            self.error = Some(e.to_string());
        }
        self.batch.clear();
        self.held = 0;
    }

    /// Load what is left, then — only if every batch loaded — the marker.
    /// Returns the number of quad lines taken.
    fn finish(mut self) -> Result<usize, String> {
        let last = std::mem::take(&mut self.carry);
        if !last.is_empty() {
            self.line(&format!("{last}\n"));
        }
        self.load_batch();
        if let Some(e) = self.error {
            return Err(e);
        }
        self.store
            .load_from_reader(RdfFormat::NQuads, Cursor::new(self.marker.as_bytes()))
            .map_err(|e| e.to_string())?;
        Ok(self.lines)
    }
}

impl crate::git2_nquads::NqSink for MarkerLastLoader<'_> {
    fn push_str(&mut self, text: &str) {
        let mut rest = text;
        while let Some(end) = rest.find('\n') {
            let (head, tail) = rest.split_at(end + 1);
            if self.carry.is_empty() {
                self.line(head);
            } else {
                self.carry.push_str(head);
                let whole = std::mem::take(&mut self.carry);
                self.line(&whole);
            }
            rest = tail;
        }
        self.carry.push_str(rest);
    }
}

/// [`materialize_now_view`] for a full rebuild: the same copy, a bounded
/// number of quads per store transaction (the single SPARQL update holds
/// the whole view in one transaction — memory that grows with the repo).
/// Same selection, stated the way `refresh_now_view` states it: every
/// base-layer quad of the one graph — not an SpoEvent's, not `rdf:reifies`.
/// Not all-or-nothing, so only a full rebuild may use it: there the sync
/// marker is still unwritten, and a partial view is never read as finished.
fn materialize_now_view_in_batches(store: &Store) {
    now_view_in_batches(store, spo_events::REBUILD_BATCH_QUADS).unwrap_or_else(|e| {
        // A stale now view silently lies to every downstream consumer
        // (Syrinx, viz, agents) — fail the sync.
        eprintln!("ERROR: now-view materialization failed: {e}");
        std::process::exit(1);
    })
}

fn now_view_in_batches(store: &Store, batch_quads: usize) -> Result<(), String> {
    use oxigraph::model::{GraphNameRef, NamedNode, NamedNodeRef, NamedOrBlankNode, Quad, QuadRef};
    let now = NamedNode::new_unchecked(NOW_GRAPH_IRI);
    let one = NamedNodeRef::new_unchecked(spo_events::LEXHISTORY_GRAPH_IRI);
    let rdf_type = NamedNodeRef::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#type");
    let reifies = NamedNodeRef::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies");
    let spo_event = NamedNodeRef::new_unchecked("https://repolex.ai/ontology/git-lex/SpoEvent");

    // DROP SILENT GRAPH, bounded: empty it, then drop its registration
    // (the first copied quad registers it again).
    spo_events::clear_graph_in_batches(store, &now, batch_quads)?;
    store.remove_named_graph(&now).map_err(|e| e.to_string())?;

    let mut batch: Vec<Quad> = Vec::new();
    // The one graph is read in subject order, so one remembered answer
    // covers each subject's whole run of quads.
    let mut last_subject: Option<(NamedOrBlankNode, bool)> = None;
    for quad in store.quads_for_pattern(None, None, None, Some(GraphNameRef::from(one))) {
        let quad = quad.map_err(|e| e.to_string())?;
        if quad.predicate.as_ref() == reifies {
            continue;
        }
        let is_event = match &last_subject {
            Some((subject, is_event)) if *subject == quad.subject => *is_event,
            _ => {
                let is_event = store
                    .contains(QuadRef::new(quad.subject.as_ref(), rdf_type, spo_event, one))
                    .map_err(|e| e.to_string())?;
                last_subject = Some((quad.subject.clone(), is_event));
                is_event
            }
        };
        if is_event {
            continue;
        }
        batch.push(Quad::new(quad.subject, quad.predicate, quad.object, now.clone()));
        if batch.len() >= batch_quads.max(1) {
            store.extend(batch.drain(..)).map_err(|e| e.to_string())?;
        }
    }
    store.extend(batch).map_err(|e| e.to_string())
}

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

/// What the one-graph walk did, for the now-view step after it.
struct OnegraphPhase {
    changed_subjects: std::collections::HashSet<String>,
}

/// The resume point, or None when it cannot be resumed from.
///
/// Belt-and-braces on top of the main-only gate: a resume commit that is
/// gone, or is not an ancestor of HEAD, can only mean external interference
/// (manual store surgery, a force-push that kept the sha alive on another
/// ref). Never walk past it — fall back to a full rebuild. Decided here,
/// before any store write, because a full rebuild orders its writes
/// differently from an append (see cmd_sync).
fn validated_resume(root: &std::path::Path, resume_sha: Option<String>) -> Option<String> {
    let sha = resume_sha?;
    let commit_exists = Command::new("git")
        .args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
        .current_dir(root)
        .status()
        .map(|st| st.success())
        .unwrap_or(false);
    let is_ancestor_of_head = commit_exists
        && Command::new("git")
            .current_dir(root)
            .args(["merge-base", "--is-ancestor", &sha, "HEAD"])
            .status()
            .map(|st| st.success())
            .unwrap_or(false);
    if is_ancestor_of_head {
        return Some(sha);
    }
    eprintln!(
        "warning: one-graph resume commit {sha} is gone or not an ancestor of HEAD (history rewritten?) — FULL one-graph rebuild"
    );
    None
}

/// The one-graph walk: append the new commits' statement events (or, with
/// no resume point, rebuild the graph from the first commit). `resume_sha`
/// has been through [`validated_resume`].
fn sync_onegraph_walk(store: &Store, root: &std::path::Path, resume_sha: Option<String>, ctx: &crate::nquad::ResolverContext) -> OnegraphPhase {
    let one_graph_uri = format!("<{}>", spo_events::LEXHISTORY_GRAPH_IRI);

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

    let (mut shas, full_rebuild) = match &resume_sha {
        Some(sha) => {
            let exclude = format!("^{sha}");
            (rev_list(&[exclude.as_str(), "HEAD"]), false)
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

    let mut changed_subjects = std::collections::HashSet::new();
    if !shas.is_empty() {
        let commits = match spo_events::collect_commits_from_shas(&shas, horizon_start.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("ERROR: could not read commit diffs: {e}");
                eprintln!("Sync aborted; the one graph was not updated. A failing diff usually means repository corruption — run `git fsck`.");
                std::process::exit(1);
            }
        };

        let outcome = match spo_events::onegraph_walk_engine(
            &commits,
            store,
            &one_graph_uri,
            ctx,
            false, // show_progress — sync prints its own phase summary
            full_rebuild, // clear_first only on a full rebuild
        ) {
            Ok(outcome) => outcome,
            Err(e) if full_rebuild => {
                // A rebuild writes in batches, so the graph holds the part
                // built before the failure. It carries no sync marker (the
                // git2 layer is not loaded yet), so the next sync rebuilds.
                eprintln!("ERROR: one-graph rebuild failed: {e}");
                eprintln!("Sync aborted part-way through a full rebuild: the store is INCOMPLETE until a sync succeeds. Fix the cause and re-run `git lex sync` — it will rebuild from the first commit again.");
                std::process::exit(1);
            }
            Err(e) => {
                // An append loads its events once, at the end of the walk,
                // so nothing of this commit range was written.
                eprintln!("ERROR: one-graph build failed: {e}");
                eprintln!("Sync aborted; the one graph was not updated for this commit range. Fix the cause and re-run `git lex sync`.");
                std::process::exit(1);
            }
        };
        println!(
            "One graph: {} {} commit(s), {} event(s) seen, {} emitted.",
            if full_rebuild { "full rebuild —" } else { "appended" },
            commits.len(),
            outcome.events_seen,
            outcome.events_emitted
        );
        changed_subjects = outcome.changed_subjects;
    } else {
        println!("One graph: up to date.");
    }
    OnegraphPhase { changed_subjects }
}

/// Type the one graph for discovery, then prove the store coherent — every
/// sync, or the sync aborts. Reads the commits graph, so it runs after the
/// git2 layer is loaded.
fn verify_onegraph(store: &Store) {
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
    // Aggregate arms (2026-08-26 rewrite; oracle in integrity_query_tests):
    // "more than one X" as GROUP BY ?e HAVING(COUNT > 1) instead of a
    // pairwise self-join per arm — same COUNT(DISTINCT ?e), one scan per
    // arm. The store dedups quads, so COUNT(?t) counts DISTINCT objects by
    // construction.
    let integrity = format!(
        "SELECT (COUNT(DISTINCT ?e) AS ?bad) WHERE {{ \
           {{ GRAPH <{g}> {{ ?e <https://repolex.ai/ontology/git-lex/assertedIn> ?a ; \
                             <https://repolex.ai/ontology/git-lex/retractedIn> ?r }} }} \
           UNION \
           {{ SELECT ?e WHERE {{ GRAPH <{g}> {{ ?e <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> ?t }} }} GROUP BY ?e HAVING(COUNT(?t) > 1) }} \
           UNION \
           {{ SELECT ?e WHERE {{ GRAPH <{g}> {{ ?e <https://repolex.ai/ontology/git-lex/assertedIn> ?c }} }} GROUP BY ?e HAVING(COUNT(?c) > 1) }} \
           UNION \
           {{ SELECT ?e WHERE {{ GRAPH <{g}> {{ ?e <https://repolex.ai/ontology/git-lex/retractedIn> ?d }} }} GROUP BY ?e HAVING(COUNT(?d) > 1) }} \
        }}",
        g = spo_events::LEXHISTORY_GRAPH_IRI
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
    // DISTINCT-first (2026-08-26 rewrite; oracle in coherence_query_tests):
    // the NOT EXISTS probe runs once per DISTINCT commit (~2k) instead of
    // once per event binding (~265k on lUX).
    let dangling = count_q(
        "SELECT (COUNT(*) AS ?n) WHERE { \
           { SELECT DISTINCT ?c WHERE { \
               GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> { \
                 { ?e <https://repolex.ai/ontology/git-lex/assertedIn> ?c } UNION \
                 { ?e <https://repolex.ai/ontology/git-lex/retractedIn> ?c } } } } \
           FILTER NOT EXISTS { GRAPH <https://repolex.ai/git-lex/NamedGraph/commits> { ?c ?p ?o } } \
        }",
    );
    // MINUS anti-join (2026-08-26 rewrite; oracle in coherence_query_tests):
    // one hash anti-join on ?s instead of a correlated NOT EXISTS probe per
    // triple (479k on lUX). Equivalent because the right side binds exactly
    // the shared ?s and nothing else.
    let base_count = count_q(
        "SELECT (COUNT(*) AS ?n) WHERE { GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> { \
           ?s ?p ?o . \
           FILTER(?p != <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies>) \
           MINUS { ?s a <https://repolex.ai/ontology/git-lex/SpoEvent> } } }",
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
    let first = first_commit_on_or_after(root, date.trim());
    if first.is_none() {
        eprintln!("warning: dev_history_horizon '{date}' matches no commit — walking full history");
    }
    first
}

/// The first commit on HEAD's line at or after 00:00:00 (local time) on
/// `date`. Git reads a BARE date as that date at the CURRENT time of day, so
/// `--after 2026-05-29` run at 17:00 skips that day's morning commits and
/// the horizon moved with the clock (#24). The midnight is spelled out
/// unless the value already carries a time.
fn first_commit_on_or_after(root: &std::path::Path, date: &str) -> Option<String> {
    let after = if date.contains(':') {
        date.to_string()
    } else {
        format!("{date} 00:00:00")
    };
    let out = Command::new("git")
        .current_dir(root)
        .args(["rev-list", "--reverse", "--after", &after, "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
}

#[cfg(test)]
mod dev_horizon_tests {
    use super::*;

    fn git(root: &std::path::Path, date: &str, args: &[&str]) {
        let ok = Command::new("git")
            .current_dir(root)
            .args(args)
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?} failed");
    }

    /// Two commits on the horizon date, one just after midnight and one just
    /// before the next. Whatever time of day this test runs, the horizon is
    /// the early one. With a bare date it was the early one only when the
    /// test ran before 00:30.
    #[test]
    fn horizon_is_the_days_first_commit_at_any_time_of_day() {
        let dir = std::env::temp_dir().join(format!("gitlex-horizon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.as_path();
        git(root, "2026-06-30T12:00:00", &["init", "-q", "."]);
        for t in ["2026-06-30T12:00:00", "2026-07-01T00:30:00", "2026-07-01T23:30:00"] {
            git(root, t, &["commit", "-q", "--allow-empty", "-m", t]);
        }
        let early = Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "HEAD~1"])
            .output()
            .unwrap();
        let early = String::from_utf8_lossy(&early.stdout).trim().to_string();
        let got = first_commit_on_or_after(root, "2026-07-01");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got, Some(early));
    }
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

#[cfg(test)]
mod coherence_query_tests {
    use oxigraph::io::RdfFormat;
    use oxigraph::store::Store;
    use std::io::Cursor;

    const LH: &str = "https://repolex.ai/git-lex/LexHistoryGraph";
    const CG: &str = "https://repolex.ai/git-lex/NamedGraph/commits";
    const GL: &str = "https://repolex.ai/ontology/git-lex/";
    const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

    /// The three formulations replaced 2026-08-26, kept as ORACLES —
    /// every fixture asserts new == old (same discipline as
    /// derived_count_tests; the rewrite is proved, not pinned to numbers).
    fn old_dangling() -> String {
        format!(
            "SELECT (COUNT(DISTINCT ?c) AS ?n) WHERE {{ GRAPH <{LH}> {{ \
               {{ ?e <{GL}assertedIn> ?c }} UNION {{ ?e <{GL}retractedIn> ?c }} }} \
               FILTER NOT EXISTS {{ GRAPH <{CG}> {{ ?c ?p ?o }} }} }}"
        )
    }
    fn new_dangling() -> String {
        format!(
            "SELECT (COUNT(*) AS ?n) WHERE {{ \
               {{ SELECT DISTINCT ?c WHERE {{ GRAPH <{LH}> {{ \
                    {{ ?e <{GL}assertedIn> ?c }} UNION {{ ?e <{GL}retractedIn> ?c }} }} }} }} \
               FILTER NOT EXISTS {{ GRAPH <{CG}> {{ ?c ?p ?o }} }} }}"
        )
    }
    fn old_base() -> String {
        format!(
            "SELECT (COUNT(*) AS ?n) WHERE {{ GRAPH <{LH}> {{ ?s ?p ?o . \
               FILTER NOT EXISTS {{ ?s a <{GL}SpoEvent> }} \
               FILTER(?p != <{RDF}reifies>) }} }}"
        )
    }
    fn new_base() -> String {
        format!(
            "SELECT (COUNT(*) AS ?n) WHERE {{ GRAPH <{LH}> {{ ?s ?p ?o . \
               FILTER(?p != <{RDF}reifies>) \
               MINUS {{ ?s a <{GL}SpoEvent> }} }} }}"
        )
    }
    fn old_integrity() -> String {
        format!(
            "SELECT (COUNT(DISTINCT ?e) AS ?bad) WHERE {{ GRAPH <{LH}> {{ \
               {{ ?e <{GL}assertedIn> ?a ; <{GL}retractedIn> ?r }} UNION \
               {{ ?e <{RDF}reifies> ?t1 , ?t2 . FILTER(?t1 != ?t2) }} UNION \
               {{ ?e <{GL}assertedIn> ?c1 , ?c2 . FILTER(?c1 != ?c2) }} UNION \
               {{ ?e <{GL}retractedIn> ?d1 , ?d2 . FILTER(?d1 != ?d2) }} }} }}"
        )
    }
    fn new_integrity() -> String {
        format!(
            "SELECT (COUNT(DISTINCT ?e) AS ?bad) WHERE {{ \
               {{ GRAPH <{LH}> {{ ?e <{GL}assertedIn> ?a ; <{GL}retractedIn> ?r }} }} UNION \
               {{ SELECT ?e WHERE {{ GRAPH <{LH}> {{ ?e <{RDF}reifies> ?t }} }} GROUP BY ?e HAVING(COUNT(?t) > 1) }} UNION \
               {{ SELECT ?e WHERE {{ GRAPH <{LH}> {{ ?e <{GL}assertedIn> ?c }} }} GROUP BY ?e HAVING(COUNT(?c) > 1) }} UNION \
               {{ SELECT ?e WHERE {{ GRAPH <{LH}> {{ ?e <{GL}retractedIn> ?d }} }} GROUP BY ?e HAVING(COUNT(?d) > 1) }} }}"
        )
    }

    fn store_from(nq: &str) -> Store {
        let store = Store::new().unwrap();
        store.load_from_reader(RdfFormat::NQuads, Cursor::new(nq.as_bytes())).unwrap();
        store
    }

    fn n(store: &Store, q: &str) -> u64 {
        match git_lex::eval_query(store, q) {
            Ok(oxigraph::sparql::QueryResults::Solutions(mut sols)) => sols
                .next().and_then(|r| r.ok())
                .and_then(|r| r.iter().next().map(|(_, t)| t.to_string()))
                .and_then(|v| v.split('"').nth(1).and_then(|x| x.parse().ok()))
                .expect("no count row"),
            _ => panic!("query failed"),
        }
    }

    fn agree(store: &Store, old: &str, new: &str, expected: u64, what: &str) {
        let o = n(store, old);
        let nw = n(store, new);
        assert_eq!(nw, o, "{what}: rewrite disagrees with oracle");
        assert_eq!(nw, expected, "{what}: wrong count");
    }

    #[test]
    fn dangling_counts_only_commitless_commits() {
        let nq = format!(
            "<https://ex/e1> <{GL}assertedIn> <https://ex/c1> <{LH}> .\n\
             <https://ex/e2> <{GL}retractedIn> <https://ex/cX> <{LH}> .\n\
             <https://ex/e3> <{GL}assertedIn> <https://ex/cX> <{LH}> .\n\
             <https://ex/c1> <{GL}git2/ordinalDerived> \"1\" <{CG}> .\n"
        );
        let s = store_from(&nq);
        // cX referenced twice but counted ONCE; c1 present in commits → 0.
        agree(&s, &old_dangling(), &new_dangling(), 1, "dangling");
    }

    #[test]
    fn dangling_zero_when_all_commits_known() {
        let nq = format!(
            "<https://ex/e1> <{GL}assertedIn> <https://ex/c1> <{LH}> .\n\
             <https://ex/c1> <{GL}git2/ordinalDerived> \"1\" <{CG}> .\n"
        );
        agree(&store_from(&nq), &old_dangling(), &new_dangling(), 0, "dangling-clean");
    }

    #[test]
    fn base_count_excludes_event_triples_and_reifies() {
        let nq = format!(
            "<https://ex/doc> <https://ex/p> \"base fact\" <{LH}> .\n\
             <https://ex/doc> <https://ex/q> \"another\" <{LH}> .\n\
             <https://ex/e1> <{RDF}type> <{GL}SpoEvent> <{LH}> .\n\
             <https://ex/e1> <{GL}assertedIn> <https://ex/c1> <{LH}> .\n\
             <https://ex/e1> <{RDF}reifies> <<( <https://ex/doc> <https://ex/p> \"base fact\" )>> <{LH}> .\n"
        );
        // Only the two base facts count: event's own triples excluded by
        // subject, reifies excluded by predicate.
        agree(&store_from(&nq), &old_base(), &new_base(), 2, "base_count");
    }

    #[test]
    fn integrity_arms_agree_and_dedup_the_violator() {
        let nq = format!(
            // eBoth: both directions (arm 1) AND two asserts (arm 3) — ONE event.
            "<https://ex/eBoth> <{GL}assertedIn> <https://ex/c1> <{LH}> .\n\
             <https://ex/eBoth> <{GL}assertedIn> <https://ex/c2> <{LH}> .\n\
             <https://ex/eBoth> <{GL}retractedIn> <https://ex/c1> <{LH}> .\n\
             # eTwoReify: two different statements (arm 2).\n\
             <https://ex/eTwoReify> <{RDF}reifies> <<( <https://ex/s1> <https://ex/p> \"a\" )>> <{LH}> .\n\
             <https://ex/eTwoReify> <{RDF}reifies> <<( <https://ex/s2> <https://ex/p> \"b\" )>> <{LH}> .\n\
             # eClean: one statement, one direction.\n\
             <https://ex/eClean> <{RDF}reifies> <<( <https://ex/s3> <https://ex/p> \"c\" )>> <{LH}> .\n\
             <https://ex/eClean> <{GL}assertedIn> <https://ex/c1> <{LH}> .\n"
        );
        // Two distinct violators; the double-violator counts once.
        agree(&store_from(&nq), &old_integrity(), &new_integrity(), 2, "integrity");
    }

    #[test]
    fn integrity_zero_on_clean_events() {
        let nq = format!(
            "<https://ex/e1> <{RDF}reifies> <<( <https://ex/s1> <https://ex/p> \"a\" )>> <{LH}> .\n\
             <https://ex/e1> <{GL}assertedIn> <https://ex/c1> <{LH}> .\n\
             <https://ex/e2> <{RDF}reifies> <<( <https://ex/s1> <https://ex/p> \"a\" )>> <{LH}> .\n\
             <https://ex/e2> <{GL}retractedIn> <https://ex/c2> <{LH}> .\n"
        );
        agree(&store_from(&nq), &old_integrity(), &new_integrity(), 0, "integrity-clean");
    }
}

/// A full rebuild writes the git2 layer and the now view in batches (#15).
/// The batches must not change what is stored, and a load that stops early
/// must leave a store that no reader takes for a synced one.
#[cfg(test)]
mod batched_rebuild_tests {
    use super::*;
    use crate::git2_nquads::NqSink;

    const CG: &str = "https://repolex.ai/git-lex/NamedGraph/commits";
    const FT: &str = "https://repolex.ai/git-lex/NamedGraph/filetree/abc";
    const LH: &str = spo_events::LEXHISTORY_GRAPH_IRI;
    const G2: &str = "https://repolex.ai/ontology/git-lex/git2/";
    const GL: &str = "https://repolex.ai/ontology/git-lex/";
    const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
    const XSD_INT: &str = "http://www.w3.org/2001/XMLSchema#integer";

    /// A small git2 layer in the producer's own order: per commit, facts
    /// with the ordinal in the middle; then the file tree, whose
    /// commit → file links land back in the commits graph.
    fn git2_layer() -> String {
        let mut nq = String::new();
        for (n, sha) in ["aaa", "bbb", "ccc"].iter().enumerate() {
            let c = format!("<https://repolex.ai/git-lex/git2/Commit/{sha}>");
            nq.push_str(&format!("{c} <{RDF}type> <{G2}Commit> <{CG}> .\n"));
            nq.push_str(&format!("{c} <{G2}id> \"{sha}\" <{CG}> .\n"));
            nq.push_str(&format!("{c} <{G2}ordinalDerived> \"{}\"^^<{XSD_INT}> <{CG}> .\n", n + 1));
            nq.push_str(&format!("{c} <{G2}summary> \"a | b <{G2}ordinalDerived> c\" <{CG}> .\n"));
        }
        for path in ["x.md", "y.md"] {
            let e = format!("<https://repolex.ai/git-lex/git2/IndexEntry/ccc/{path}>");
            nq.push_str(&format!("{e} <{RDF}type> <{G2}IndexEntry> <{FT}> .\n"));
            nq.push_str(&format!("{e} <{G2}path> \"{path}\" <{FT}> .\n"));
            nq.push_str(&format!("<https://repolex.ai/git-lex/git2/Commit/ccc> <{G2}file> {e} <{CG}> .\n"));
        }
        nq
    }

    fn quads(store: &Store) -> Vec<String> {
        let mut all: Vec<String> = store.iter().map(|q| q.unwrap().to_string()).collect();
        all.sort();
        all
    }

    fn marker_quads(store: &Store) -> usize {
        quads(store).iter().filter(|q| q.contains(&format!("<{SYNC_MARKER_PREDICATE}> \""))).count()
    }

    #[test]
    fn only_the_ordinal_predicate_is_the_marker() {
        let layer = git2_layer();
        let marker: Vec<&str> = layer.lines().filter(|l| is_sync_marker_line(l)).collect();
        assert_eq!(marker.len(), 3, "one ordinal per commit, and the summary that only MENTIONS the predicate is not one");
        assert!(marker.iter().all(|l| l.contains("/Commit/")));
    }

    #[test]
    fn batches_store_what_one_load_stores() {
        let layer = git2_layer();
        let whole = Store::new().unwrap();
        whole.load_from_reader(RdfFormat::NQuads, Cursor::new(layer.as_bytes())).unwrap();
        for batch_quads in [1, 2, 5, 1000] {
            let store = Store::new().unwrap();
            let mut loader = MarkerLastLoader::new(&store, batch_quads);
            // Hand the text over in pieces that ignore line ends, as any
            // producer is free to.
            for piece in layer.as_bytes().chunks(37) {
                loader.push_str(std::str::from_utf8(piece).unwrap());
            }
            assert_eq!(loader.finish().unwrap(), layer.lines().count());
            assert_eq!(quads(&store), quads(&whole), "batch of {batch_quads}");
        }
    }

    /// Killed before `finish`: everything but the marker may be in the store,
    /// and every "is this store synced?" reader must still say no.
    #[test]
    fn a_load_that_never_finished_reads_as_unsynced() {
        let layer = git2_layer();
        let store = Store::new().unwrap();
        // A one graph that would pass the fast path's other two probes.
        let one = format!(
            "<https://repolex.ai/git-lex/File/x.md> <{RDF}type> <{GL}File> <{LH}> .\n"
        );
        store.load_from_reader(RdfFormat::NQuads, Cursor::new(one.as_bytes())).unwrap();
        let mut loader = MarkerLastLoader::new(&store, 1);
        loader.push_str(&layer);
        drop(loader); // never finished

        assert!(quads(&store).len() > 10, "the batches themselves were written");
        assert_eq!(marker_quads(&store), 0);
        let nowhere = std::path::Path::new("/nonexistent-git-lex-test-root");
        assert!(!fast_path_hit(&store, nowhere, "ccc"), "HEAD has facts, but no ordinal: not synced");
        assert_eq!(resume_point(&store, nowhere), None);

        // Finished, the marker is there.
        let mut loader = MarkerLastLoader::new(&store, 1);
        loader.push_str(&layer);
        loader.finish().unwrap();
        assert_eq!(marker_quads(&store), 3);
    }

    /// Base facts, events about them, and an event whose statement has left
    /// the base layer.
    fn one_graph_fixture() -> String {
        let mut nq = String::new();
        for i in 0..7 {
            let s = format!("<https://repolex.ai/soul/Note/n{i}>");
            nq.push_str(&format!("{s} <{RDF}type> <https://repolex.ai/ontology/soul/Note> <{LH}> .\n"));
            nq.push_str(&format!("{s} <{GL}title> \"note {i}\" <{LH}> .\n"));
            let e = format!("<https://repolex.ai/git-lex/SpoEvent/e{i}>");
            nq.push_str(&format!("{e} <{RDF}type> <{GL}SpoEvent> <{LH}> .\n"));
            nq.push_str(&format!("{e} <{RDF}reifies> <<( {s} <{GL}title> \"note {i}\" )>> <{LH}> .\n"));
            nq.push_str(&format!("{e} <{GL}assertedIn> <https://repolex.ai/git-lex/git2/Commit/aaa> <{LH}> .\n"));
        }
        nq.push_str(&format!("<https://e/elsewhere> <{GL}title> \"not the one graph\" <{CG}> .\n"));
        nq
    }

    #[test]
    fn the_batched_now_view_is_the_now_view() {
        let build = |stale_now: bool| {
            let store = Store::new().unwrap();
            store.load_from_reader(RdfFormat::NQuads, Cursor::new(one_graph_fixture().as_bytes())).unwrap();
            if stale_now {
                let stale: String = ["gone", "gone2", "gone3"]
                    .iter()
                    .map(|s| format!("<https://e/{s}> <{GL}title> \"stale\" <{NOW_GRAPH_IRI}> .\n"))
                    .collect();
                store.load_from_reader(RdfFormat::NQuads, Cursor::new(stale.as_bytes())).unwrap();
            }
            store
        };
        let whole = build(true);
        materialize_now_view(&whole);
        let expected = quads(&whole);
        assert_eq!(expected.iter().filter(|q| q.ends_with(&format!("<{NOW_GRAPH_IRI}>"))).count(), 14);

        for batch_quads in [1, 3, 1000] {
            let store = build(true);
            now_view_in_batches(&store, batch_quads).unwrap();
            assert_eq!(quads(&store), expected, "batch of {batch_quads}");
        }
        // No now view to drop first: same answer.
        let store = build(false);
        now_view_in_batches(&store, 2).unwrap();
        assert_eq!(quads(&store), expected);
    }
}
