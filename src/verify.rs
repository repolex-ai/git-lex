//! Data-quality verification — the Part-4.5 suite (one-graph build plan).
//!
//! Read-only checks against the PERSISTENT store. The ontology is the source
//! of truth: every predicate and class the store carries in GOVERNED
//! namespaces must be declared in the store's own self-describing ontology
//! graph (`NamedGraph/repo-ontology`, loaded at init/kit-update — "stays
//! put", Rob Day-50). An empty ontology graph is itself a finding: it means
//! the installed kits are stale or never loaded (kit-version-skew), and the
//! vocabulary checks cannot run.
//!
//! Namespace governance (Rob-ruled 2026-07-20):
//!   - `ontology/kit/<kit>/*`, `ontology/git-lex/*`, `ontology/git-lex/git2/*`
//!     — GOVERNED: must be declared.
//!   - `ontology/git-lex/fm/*` — OPEN BY DESIGN (user frontmatter catchall,
//!     documented in fm.ttl): exempt from declaredness, but the known-junk
//!     `fm/@<filename>` family must be ZERO (triage BUG 3).
//!   - `ontology/git-lex/md/*` — placement unruled; treated as exempt until
//!     ruled (tracked in the build plan).
//!   - W3C namespaces (rdf:) — exempt.
//!
//! Structural contract of the one graph (git-lex.ttl): every SpoEvent has
//! exactly one rdf:reifies and exactly one direction (assertedIn XOR
//! retractedIn), and every event's commit EXISTS in the commits graph.
//!
//! Failure posture (review #20): a check that cannot RUN is a FAILED check.
//! The helpers return Result; a store/query error prints as ✗, never as a
//! zero that reads like a pass — the same law sync's coherence gate follows
//! ("a gate that can't run must not pretend it passed", Rob-ruled
//! 2026-07-29; sync.rs fixed this exact `unwrap_or(0)` class already).

use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;

const ONTOLOGY_GRAPH: &str = "https://repolex.ai/git-lex/NamedGraph/repo-ontology";
// The ENTIRE ontology root is governed: anything the store carries under
// https://repolex.ai/ontology/ must be declared. This covers both the
// app-tier kit namespaces (ontology/soul/, ontology/copia/, … — the
// 2026-07-24 flip off the ruled-dead kit/ tier) and whatever kit/-tier
// vocab remains quoted in history until each repo's full rebuild.
const GOVERNED_PREFIXES: &[&str] = &[
    "https://repolex.ai/ontology/",
];
const EXEMPT_PREFIXES: &[&str] = &[
    "https://repolex.ai/ontology/git-lex/fm/", // open by design (fm.ttl)
    "https://repolex.ai/ontology/git-lex/md/", // placement unruled — exempt until ruled
];

fn ask(store: &Store, q: &str) -> bool {
    // Fails CLOSED: an error reads as false, which the callers surface as a
    // loud finding (an undeclared term), never as a silent pass.
    SparqlEvaluator::new()
        .parse_query(q)
        .ok()
        .and_then(|q| q.on_store(store).execute().ok())
        .map(|r| matches!(r, QueryResults::Boolean(true)))
        .unwrap_or(false)
}

fn select_strings(store: &Store, q: &str, var: &str) -> Result<Vec<String>, String> {
    let parsed = SparqlEvaluator::new()
        .parse_query(q)
        .map_err(|e| format!("query parse failed: {e}"))?;
    match parsed.on_store(store).execute() {
        Ok(QueryResults::Solutions(sols)) => {
            let mut out = Vec::new();
            for sol in sols {
                // A per-solution error is a store error, not an empty row.
                let sol = sol.map_err(|e| format!("query evaluation failed: {e}"))?;
                if let Some(term) = sol.get(var) {
                    let s = term.to_string();
                    out.push(s.trim_matches(|c| c == '<' || c == '>' || c == '"').to_string());
                }
            }
            Ok(out)
        }
        Ok(_) => Err("query returned a non-SELECT result".to_string()),
        Err(e) => Err(format!("query execution failed: {e}")),
    }
}

fn count(store: &Store, q: &str) -> Result<u64, String> {
    let rows = select_strings(store, q, "n")?;
    // A COUNT query always returns exactly one row, so "no row" can only
    // mean the evaluation errored. The old code mapped that to 0 — a
    // corrupted store printed ✓ on every structural check.
    let raw = rows
        .first()
        .ok_or_else(|| "COUNT returned no row — evaluation error".to_string())?;
    let num = raw.split('"').next().unwrap_or(raw);
    let num = num.split("^^").next().unwrap_or(num);
    num.parse::<u64>()
        .map_err(|e| format!("COUNT value `{raw}` did not parse: {e}"))
}

/// Run the full suite. Returns the number of FAILED checks (0 = clean).
/// Every violation prints; nothing is silent — including a check that
/// could not run, which counts as failed.
pub(crate) fn run_verify(store: &Store) -> usize {
    let mut failures = 0usize;
    println!("git lex verify");
    println!("──────────────────────────────────────────────────");

    // ── Check 0: the ontology graph itself ──────────────────────────────
    let mut vocab_checks_possible = false;
    match count(
        store,
        &format!("SELECT (COUNT(*) AS ?n) WHERE {{ GRAPH <{ONTOLOGY_GRAPH}> {{ ?s ?p ?o }} }}"),
    ) {
        Ok(ont_count) if ont_count > 0 => {
            vocab_checks_possible = true;
            println!("✓ ontology graph present ({ont_count} triples)");
        }
        Ok(_) => {
            println!("✗ ontology graph EMPTY — installed kit vocabularies are not loaded");
            println!("    (run `git lex init` / kit-update; vocabulary checks 1–2 SKIPPED — this is");
            println!("     the kit-version-skew condition: the store cannot vouch for its own vocab)");
            failures += 1;
        }
        Err(e) => {
            println!("✗ check 0 could not run ({e}) — the store is unverified, not verified");
            failures += 1;
        }
    }

    // Governed data graphs: the one graph + the git2 machinery layer.
    let data_graph_filter = "FILTER(?g = <https://repolex.ai/git-lex/LexHistoryGraph> \
         || STRSTARTS(STR(?g), \"https://repolex.ai/git-lex/NamedGraph/commits\") \
         || STRSTARTS(STR(?g), \"https://repolex.ai/git-lex/NamedGraph/refs\") \
         || STRSTARTS(STR(?g), \"https://repolex.ai/git-lex/NamedGraph/filetree/\") \
         || ?g = <https://repolex.ai/git-lex/NamedGraph/repo>)";

    // ── Check 1: every governed predicate is declared ───────────────────
    if vocab_checks_possible {
        match select_strings(
            store,
            &format!(
                "SELECT DISTINCT ?p WHERE {{ GRAPH ?g {{ ?s ?p ?o }} {data_graph_filter} }}"
            ),
            "p",
        ) {
            Ok(preds) => {
                let mut undeclared = Vec::new();
                for p in &preds {
                    if !GOVERNED_PREFIXES.iter().any(|g| p.starts_with(g)) {
                        continue; // W3C etc.
                    }
                    if EXEMPT_PREFIXES.iter().any(|e| p.starts_with(e)) {
                        continue;
                    }
                    let declared = ask(
                        store,
                        &format!("ASK {{ GRAPH <{ONTOLOGY_GRAPH}> {{ <{p}> ?x ?y }} }}"),
                    );
                    if !declared {
                        undeclared.push(p.clone());
                    }
                }
                if undeclared.is_empty() {
                    println!("✓ check 1: every governed predicate is declared ({} checked)", preds.len());
                } else {
                    println!("✗ check 1: {} UNDECLARED predicate(s) in governed namespaces:", undeclared.len());
                    for p in &undeclared {
                        println!("    {p}");
                    }
                    failures += 1;
                }
            }
            Err(e) => {
                println!("✗ check 1 could not run ({e})");
                failures += 1;
            }
        }
    }

    // ── Check 2: every rdf:type object is a declared class ──────────────
    if vocab_checks_possible {
        match select_strings(
            store,
            &format!(
                "SELECT DISTINCT ?t WHERE {{ GRAPH ?g {{ ?s a ?t }} {data_graph_filter} }}"
            ),
            "t",
        ) {
            Ok(types) => {
                let mut undeclared = Vec::new();
                for t in &types {
                    if !GOVERNED_PREFIXES.iter().any(|g| t.starts_with(g)) {
                        continue;
                    }
                    let declared = ask(
                        store,
                        &format!("ASK {{ GRAPH <{ONTOLOGY_GRAPH}> {{ <{t}> ?x ?y }} }}"),
                    );
                    if !declared {
                        undeclared.push(t.clone());
                    }
                }
                if undeclared.is_empty() {
                    println!("✓ check 2: every rdf:type object is a declared class ({} checked)", types.len());
                } else {
                    println!("✗ check 2: {} UNDECLARED class(es) in use:", undeclared.len());
                    for t in &undeclared {
                        println!("    {t}");
                    }
                    failures += 1;
                }
            }
            Err(e) => {
                println!("✗ check 2 could not run ({e})");
                failures += 1;
            }
        }
    }

    // One reporter for every count-based check: Ok(0) passes, Ok(n) fails
    // with the check's own message, Err fails as could-not-run.
    let count_check = |failures: &mut usize, label: &str, q: &str, pass: &str, fail: &dyn Fn(u64) -> String| {
        match count(store, q) {
            Ok(0) => println!("✓ {label}: {pass}"),
            Ok(n) => {
                println!("✗ {label}: {}", fail(n));
                *failures += 1;
            }
            Err(e) => {
                println!("✗ {label} could not run ({e})");
                *failures += 1;
            }
        }
    };

    // ── Check 3: one-graph structural integrity ─────────────────────────
    // 3a: exactly one statement + one direction per SpoEvent (id collision /
    //     emitter bug detector — also runs at every sync; here on demand).
    count_check(
        &mut failures,
        "check 3a",
        "SELECT (COUNT(DISTINCT ?e) AS ?n) WHERE { GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> { \
           { ?e <https://repolex.ai/ontology/git-lex/assertedIn> ?a ; \
                <https://repolex.ai/ontology/git-lex/retractedIn> ?r } \
           UNION \
           { ?e <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> ?t1 , ?t2 . FILTER(?t1 != ?t2) } \
           UNION \
           { ?e <https://repolex.ai/ontology/git-lex/assertedIn> ?c1 , ?c2 . FILTER(?c1 != ?c2) } \
           UNION \
           { ?e <https://repolex.ai/ontology/git-lex/retractedIn> ?d1 , ?d2 . FILTER(?d1 != ?d2) } \
        } }",
        "every SpoEvent has one statement + one direction",
        &|n| format!("{n} SpoEvent(s) violate one-statement/one-direction"),
    );

    // 3b: no dangling commit joins — every event's commit exists in the
    //     commits graph.
    count_check(
        &mut failures,
        "check 3b",
        "SELECT (COUNT(DISTINCT ?c) AS ?n) WHERE { \
           GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> { \
             { ?e <https://repolex.ai/ontology/git-lex/assertedIn> ?c } UNION \
             { ?e <https://repolex.ai/ontology/git-lex/retractedIn> ?c } } \
           FILTER NOT EXISTS { GRAPH <https://repolex.ai/git-lex/NamedGraph/commits> { ?c ?p ?o } } \
        }",
        "every event's commit exists in the commits graph",
        &|n| format!("{n} event commit(s) missing from the commits graph (dangling join)"),
    );

    // 3c: every SpoEvent is typed.
    count_check(
        &mut failures,
        "check 3c",
        "SELECT (COUNT(DISTINCT ?e) AS ?n) WHERE { GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> { \
           ?e <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> ?t . \
           FILTER NOT EXISTS { ?e a <https://repolex.ai/ontology/git-lex/SpoEvent> } } }",
        "every SpoEvent carries its class",
        &|n| format!("{n} event node(s) missing rdf:type git-lex:SpoEvent"),
    );

    // ── Check 3d: base layer == derived now (THE contract check) ────────
    // The one graph's plain-triple layer is the materialized now; deriving
    // "now" from the events (latest event per statement, by commit ordinal,
    // is an assert) must give EXACTLY the same set. Divergence means the
    // walk engine's base-layer maintenance and its event stream disagree.
    let base_count = count(
        store,
        "SELECT (COUNT(*) AS ?n) WHERE { GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> {            ?s ?p ?o .            FILTER NOT EXISTS { ?s a <https://repolex.ai/ontology/git-lex/SpoEvent> }            FILTER(?p != <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies>) } }",
    );
    let derived_count = count(
        store,
        "PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>          PREFIX gl: <https://repolex.ai/ontology/git-lex/>          PREFIX g2: <https://repolex.ai/ontology/git-lex/git2/>          SELECT (COUNT(DISTINCT ?tt) AS ?n) WHERE {            GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> { ?a rdf:reifies ?tt ; gl:assertedIn ?ca }            GRAPH <https://repolex.ai/git-lex/NamedGraph/commits> { ?ca g2:ordinalDerived ?oa }            FILTER NOT EXISTS {              GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> { ?r rdf:reifies ?tt ; gl:retractedIn ?cr }              GRAPH <https://repolex.ai/git-lex/NamedGraph/commits> { ?cr g2:ordinalDerived ?or }              FILTER(?or >= ?oa) } }",
    );
    match (base_count, derived_count) {
        (Ok(b), Ok(d)) if b == d => {
            println!("✓ check 3d: base layer == derived now ({b} facts, exact parity)");
        }
        (Ok(b), Ok(d)) => {
            println!("✗ check 3d: base layer ({b}) != derived now ({d}) — current state disagrees with what the history says it should be");
            failures += 1;
        }
        (Err(e), _) | (_, Err(e)) => {
            println!("✗ check 3d could not run ({e})");
            failures += 1;
        }
    }

    // ── Check 5: no known-junk families ─────────────────────────────────
    // (Check 4 — the completeness accounting — is walk-side: every walk
    //  prints lines-in / dropped-by-reason and can never drop silently.)
    count_check(
        &mut failures,
        "check 5a",
        "SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } \
           FILTER(CONTAINS(STR(?p), \"/fm/@\") || CONTAINS(STR(?p), \"/fm/%40\")) }",
        "zero retired @filename junk predicates",
        &|n| format!("{n} quad(s) with retired @filename junk predicates"),
    );

    // 5b: cased-duplicate predicate pairs within one namespace (the
    //     lowercase→Capital migration artifacts, e.g. contact_type vs
    //     contactType).
    match select_strings(
        store,
        "SELECT DISTINCT ?p WHERE { GRAPH ?g { ?s ?p ?o } \
           FILTER(STRSTARTS(STR(?p), \"https://repolex.ai/ontology/\")) }",
        "p",
    ) {
        Ok(preds) => {
            let mut canon_map: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
            for p in &preds {
                let canon = p.to_lowercase().replace('_', "");
                canon_map.entry(canon).or_default().push(p.clone());
            }
            let dupes: Vec<&Vec<String>> = canon_map.values().filter(|v| v.len() > 1).collect();
            if dupes.is_empty() {
                println!("✓ check 5b: zero cased/snake-camel duplicate predicate pairs");
            } else {
                println!("✗ check 5b: {} duplicate predicate famil(ies):", dupes.len());
                for family in &dupes {
                    println!("    {}", family.join("  ≠  "));
                }
                failures += 1;
            }
        }
        Err(e) => {
            println!("✗ check 5b could not run ({e})");
            failures += 1;
        }
    }

    println!("──────────────────────────────────────────────────");
    if failures == 0 {
        println!("ALL CHECKS PASSED");
    } else {
        println!("{failures} CHECK(S) FAILED — the store carries drift; see above");
    }
    failures
}
