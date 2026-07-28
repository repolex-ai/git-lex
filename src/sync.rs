//! `git lex sync` — build/refresh the derived knowledge graphs from git.
//!
//! Regenerates the ephemeral virtual graphs (git2 layer, adaptive shapes),
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
    // Identity: resolve + persist the genesis SHA ONCE per sync (identity.yml
    // is what Pool's boot-skip and federation readers consume). IRIs no longer
    // carry it — see git.rs Task-2 IRI families.
    crate::git::ensure_identity_yml();
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

    // ─── Always: regenerate adaptive shapes before the fast-path check ───
    // Adaptive shapes are derived from `_ontology/*.ttl` (agent-authored,
    // can change at any time). Regenerating is cheap and idempotent. We
    // do it BEFORE the fast-path so that when an agent edits an ontology
    // without committing, the shapes file refreshes even if HEAD hasn't
    // moved. Adaptive shapes are also a precondition for `git lex create`
    // / `git lex list` finding adaptive-kit doctypes.
    let (adaptive_ok, adaptive_fail) = crate::shacl::build_adaptive_shapes();
    for (ttl, err) in &adaptive_fail {
        eprintln!("warning: adaptive shapes failed for {}: {}", ttl.display(), err);
    }

    // ─── Fast path: already-synced no-op ───
    // If the commits graph already contains HEAD (the previous sync reached
    // this commit) AND the extract dir is clean (no uncommitted .spo
    // changes), every phase of sync would rebuild identical state. Skip.
    //
    // Contract this depends on: the oxigraph store is derived. If you've
    // manually mutated it, rebuild via `rm -rf .git/lex/oxigraph`.
    {
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

        if already_synced && onegraph_present {
            // Check .lex/extract/ for uncommitted .spo changes
            let dirty = Command::new("git")
                .args(["status", "--porcelain", "--", ".lex/extract/"])
                .current_dir(&root)
                .output()
                .ok()
                .map(|o| !o.stdout.is_empty())
                .unwrap_or(true); // on error, fall through to full sync

            if !dirty {
                let elapsed = start.elapsed();
                println!(
                    "Already synced at {} ({:.1}ms).",
                    &head_sha[..8.min(head_sha.len())],
                    elapsed.as_secs_f64() * 1000.0
                );
                return;
            }
        }
    }

    // ─── One-graph resume point: read BEFORE Phase 1 clears the commits
    // graph. The resume commit = the commit carrying MAX git2:ordinalDerived
    // in the PREVIOUS sync's commits graph. No stored marker (Rob-ruled):
    // the persisted commit data IS the marker — a no-change commit still
    // lands in the commits graph, so "newest in store" is always the true
    // high-water mark.
    let onegraph_resume: Option<String> = {
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
            .and_then(|q| q.on_store(&store).execute().ok())
            .and_then(|r| match r {
                oxigraph::sparql::QueryResults::Solutions(mut sols) => {
                    sols.next().and_then(|s| s.ok()).and_then(|s| {
                        s.get("sha").map(|t| match t {
                            oxigraph::model::Term::Literal(l) => l.value().to_string(),
                            other => other.to_string(),
                        })
                    })
                }
                _ => None,
            })
    };

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

    // (adaptive shapes already built at top of cmd_sync, before fast-path check)

    // Regenerate the git2 machinery layer (commits/signatures/refs/filetree)
    let git_nq = crate::git2_nquads::generate_git2_nquads();
    let git_count = git_nq.lines().count();
    store
        .load_from_reader(RdfFormat::NQuads, Cursor::new(git_nq.as_bytes()))
        .expect("failed to load git triples");

    // Extraction: generate_frontmatter_nquads WRITES the .spo sidecars (the
    // one graph's source) and derives the working-tree now view. The now
    // view is NO LONGER loaded into the store (Rob-ruled: the now graph
    // died as a store product — the one graph's base layer is current
    // state). The derived text is discarded; the extraction side effect is
    // what sync needs. (Splitting extraction from emission is a refactor
    // deferred until the direct query path's disposition is ruled — the
    // same function serves `git lex query`.)
    let resolver_ctx = crate::nquad::ResolverContext::build(&root);
    let (fm_nq, fm_errors) = crate::nquad::generate_frontmatter_nquads_with(&root, &resolver_ctx);
    if fm_errors > 0 {
        eprintln!("warning: {} frontmatter error(s) during sync — extraction may be incomplete", fm_errors);
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

    // ─── Materialize the now VIEW ───
    // NamedGraph/now = the one graph's base layer (current facts), copied
    // out as a standalone graph each sync. This is a VIEW in the ruled sense
    // ("'now' is a view — a query, OR A MATERIALIZED GRAPH, derived from the
    // one graph"): derived, disposable, rebuilt every sync, never edited.
    // It exists so downstream consumers (Syrinx, viz, agents) can query
    // current state as plain triples without filtering event machinery.
    {
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

    store.flush().expect("failed to flush store");

    let elapsed = start.elapsed();

    println!(
        "Synced in {:.1}ms:",
        elapsed.as_secs_f64() * 1000.0
    );
    println!("  git2 layer: {} quads; extracted: {} now-view facts", git_count, fm_count);
    if !adaptive_ok.is_empty() || !adaptive_fail.is_empty() {
        println!("  Adaptive shapes: {} built, {} failed", adaptive_ok.len(), adaptive_fail.len());
    }
    println!("Store: {}", store_path().unwrap().display());
}

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

    let (shas, full_rebuild) = match &resume_sha {
        Some(sha) if commit_exists(sha) => {
            let exclude = format!("^{sha}");
            (rev_list(&[exclude.as_str(), "HEAD"]), false)
        }
        Some(sha) => {
            eprintln!(
                "warning: one-graph resume commit {sha} no longer exists (history rewritten?) — FULL one-graph rebuild"
            );
            (rev_list(&["HEAD"]), true)
        }
        None => (rev_list(&["HEAD"]), true),
    };

    if !shas.is_empty() {
        let commits = match spo_events::collect_commits_from_shas(&shas) {
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
}
