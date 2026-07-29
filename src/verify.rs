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
    SparqlEvaluator::new()
        .parse_query(q)
        .ok()
        .and_then(|q| q.on_store(store).execute().ok())
        .map(|r| matches!(r, QueryResults::Boolean(true)))
        .unwrap_or(false)
}

fn select_strings(store: &Store, q: &str, var: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(parsed) = SparqlEvaluator::new().parse_query(q) {
        if let Ok(QueryResults::Solutions(sols)) = parsed.on_store(store).execute() {
            for sol in sols.flatten() {
                if let Some(term) = sol.get(var) {
                    let s = term.to_string();
                    out.push(s.trim_matches(|c| c == '<' || c == '>' || c == '"').to_string());
                }
            }
        }
    }
    out
}

fn count(store: &Store, q: &str) -> u64 {
    select_strings(store, q, "n")
        .first()
        .and_then(|v| v.split("\"").nth(0).map(|x| x.to_string()))
        .and_then(|v| v.split("^^").next().map(|x| x.to_string()))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Run the full suite. Returns the number of FAILED checks (0 = clean).
/// Every violation prints; nothing is silent.
pub(crate) fn run_verify(store: &Store) -> usize {
    let mut failures = 0usize;
    println!("git lex verify");
    println!("──────────────────────────────────────────────────");

    // ── Check 0: the ontology graph itself ──────────────────────────────
    let ont_count = count(
        store,
        &format!("SELECT (COUNT(*) AS ?n) WHERE {{ GRAPH <{ONTOLOGY_GRAPH}> {{ ?s ?p ?o }} }}"),
    );
    let vocab_checks_possible = ont_count > 0;
    if vocab_checks_possible {
        println!("✓ ontology graph present ({ont_count} triples)");
    } else {
        println!("✗ ontology graph EMPTY — installed kit vocabularies are not loaded");
        println!("    (run `git lex init` / kit-update; vocabulary checks 1–2 SKIPPED — this is");
        println!("     the kit-version-skew condition: the store cannot vouch for its own vocab)");
        failures += 1;
    }

    // Governed data graphs: the one graph + the git2 machinery layer.
    let data_graph_filter = "FILTER(?g = <https://repolex.ai/git-lex/LexHistoryGraph> \
         || STRSTARTS(STR(?g), \"https://repolex.ai/git-lex/NamedGraph/commits\") \
         || STRSTARTS(STR(?g), \"https://repolex.ai/git-lex/NamedGraph/refs\") \
         || STRSTARTS(STR(?g), \"https://repolex.ai/git-lex/NamedGraph/filetree/\") \
         || ?g = <https://repolex.ai/git-lex/NamedGraph/repo>)";

    // ── Check 1: every governed predicate is declared ───────────────────
    if vocab_checks_possible {
        let preds = select_strings(
            store,
            &format!(
                "SELECT DISTINCT ?p WHERE {{ GRAPH ?g {{ ?s ?p ?o }} {data_graph_filter} }}"
            ),
            "p",
        );
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

    // ── Check 2: every rdf:type object is a declared class ──────────────
    if vocab_checks_possible {
        let types = select_strings(
            store,
            &format!(
                "SELECT DISTINCT ?t WHERE {{ GRAPH ?g {{ ?s a ?t }} {data_graph_filter} }}"
            ),
            "t",
        );
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

    // ── Check 3: one-graph structural integrity ─────────────────────────
    // 3a: exactly one statement + one direction per SpoEvent (id collision /
    //     emitter bug detector — also runs at every sync; here on demand).
    let bad_events = count(
        store,
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
    );
    if bad_events == 0 {
        println!("✓ check 3a: every SpoEvent has one statement + one direction");
    } else {
        println!("✗ check 3a: {bad_events} SpoEvent(s) violate one-statement/one-direction");
        failures += 1;
    }

    // 3b: no dangling commit joins — every event's commit exists in the
    //     commits graph.
    let dangling = count(
        store,
        "SELECT (COUNT(DISTINCT ?c) AS ?n) WHERE { \
           GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> { \
             { ?e <https://repolex.ai/ontology/git-lex/assertedIn> ?c } UNION \
             { ?e <https://repolex.ai/ontology/git-lex/retractedIn> ?c } } \
           FILTER NOT EXISTS { GRAPH <https://repolex.ai/git-lex/NamedGraph/commits> { ?c ?p ?o } } \
        }",
    );
    if dangling == 0 {
        println!("✓ check 3b: every event's commit exists in the commits graph");
    } else {
        println!("✗ check 3b: {dangling} event commit(s) missing from the commits graph (dangling join)");
        failures += 1;
    }

    // 3c: every SpoEvent is typed.
    let untyped = count(
        store,
        "SELECT (COUNT(DISTINCT ?e) AS ?n) WHERE { GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> { \
           ?e <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> ?t . \
           FILTER NOT EXISTS { ?e a <https://repolex.ai/ontology/git-lex/SpoEvent> } } }",
    );
    if untyped == 0 {
        println!("✓ check 3c: every SpoEvent carries its class");
    } else {
        println!("✗ check 3c: {untyped} event node(s) missing rdf:type git-lex:SpoEvent");
        failures += 1;
    }

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
    if base_count == derived_count {
        println!("✓ check 3d: base layer == derived now ({base_count} facts, exact parity)");
    } else {
        println!("✗ check 3d: base layer ({base_count}) != derived now ({derived_count}) — current state disagrees with what the history says it should be");
        failures += 1;
    }

    // ── Check 5: no known-junk families ─────────────────────────────────
    // (Check 4 — the completeness accounting — is walk-side: every walk
    //  prints lines-in / dropped-by-reason and can never drop silently.)
    let junk = count(
        store,
        "SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } \
           FILTER(CONTAINS(STR(?p), \"/fm/@\") || CONTAINS(STR(?p), \"/fm/%40\")) }",
    );
    if junk == 0 {
        println!("✓ check 5a: zero retired @filename junk predicates");
    } else {
        println!("✗ check 5a: {junk} quad(s) with retired @filename junk predicates");
        failures += 1;
    }

    // 5b: cased-duplicate predicate pairs within one namespace (the
    //     lowercase→Capital migration artifacts, e.g. contact_type vs
    //     contactType).
    let preds = select_strings(
        store,
        "SELECT DISTINCT ?p WHERE { GRAPH ?g { ?s ?p ?o } \
           FILTER(STRSTARTS(STR(?p), \"https://repolex.ai/ontology/\")) }",
        "p",
    );
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

    println!("──────────────────────────────────────────────────");
    if failures == 0 {
        println!("ALL CHECKS PASSED");
    } else {
        println!("{failures} CHECK(S) FAILED — the store carries drift; see above");
    }
    failures
}
