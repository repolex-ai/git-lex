//! SHACL shape parsing and generation.
//!
//! - `parse_shacl_hints` reads a shapes TTL and pulls out inline hints
//!   (enum values, IRI node-kind, required/optional) that get embedded in
//!   class-template comments by `cmd create`.
//! - `generate_shacl_shapes` runs SPARQL against a loaded kit ontology to
//!   derive SHACL shapes automatically (owl:oneOf → sh:in, ObjectProperty
//!   → sh:nodeKind sh:IRI, owl:Restriction minCard → sh:minCount).
//! - `build_shacl_shapes` writes the generated shapes to
//!   `.lex/ontology/{short}/{short}-shapes.ttl`.
//! - `build_adaptive_shapes` scans `_ontology/` for agent-authored TTLs,
//!   generates shapes alongside them.
//!
//! Peeled out of `main.rs` during modularization. No behavior changes.

use oxigraph::model::Term;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use git_lex::{find_git_root, resolve_kit_spec};

/// Parse SHACL shapes TTL to extract inline hints for class template comments.
/// Returns a map of property qname (`soul:confidence`) → hint string
/// (e.g. "optional, enum: certain, likely, hypothesis, hunch").
///
/// Real Turtle parse + SPARQL over the same in-memory-store approach shape
/// GENERATION already uses — one Turtle-reading policy, no line scanning.
/// `short` names the kit so the key prefix can be derived from the file's
/// own `@prefix` declaration (the ONE shared scanner).
///
/// `sh:in` members are walked down the RDF list (rdf:first/rdf:rest), so
/// enum values keep their declaration order.
pub(crate) fn parse_shacl_hints(shapes_ttl: &str, short: &str) -> HashMap<String, String> {
    let mut hints: HashMap<String, String> = HashMap::new();
    let (prefix_name, namespace) = git_lex::extract_kit_prefix(shapes_ttl, short)
        .unwrap_or_else(|| (short.to_string(), git_lex::conventional_kit_namespace(short)));

    let store = match crate::kit::load_ttl_str(shapes_ttl, &format!("{} shapes", short)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: {} — no template hints for kit '{}'", e, short);
            return hints;
        }
    };

    let q = "PREFIX sh: <http://www.w3.org/ns/shacl#>
             SELECT ?prop ?path ?nodeKind ?minCount ?inList WHERE {
                 ?prop sh:path ?path .
                 OPTIONAL { ?prop sh:nodeKind ?nodeKind }
                 OPTIONAL { ?prop sh:minCount ?minCount }
                 OPTIONAL { ?prop sh:in ?inList }
             } ORDER BY ?path";
    let Ok(oxigraph::sparql::QueryResults::Solutions(sols)) = git_lex::eval_query(&store, q)
    else { return hints };

    for s in sols.flatten() {
        let Some(Term::NamedNode(path)) = s.get("path") else { continue };
        // Key mirrors what the template emitter looks up: `{prefix}:{local}`.
        // A path outside the kit namespace keeps its full bracketed IRI
        // (never looked up, but never collides either).
        let key = match path.as_str().strip_prefix(&namespace) {
            Some(local) => format!("{}:{}", prefix_name, local),
            None => format!("<{}>", path.as_str()),
        };

        let in_values: Vec<String> = match s.get("inList") {
            Some(list) => rdf_list_literals(&store, list),
            None => Vec::new(),
        };
        let node_kind = match s.get("nodeKind") {
            Some(Term::NamedNode(nk)) if nk.as_str() == "http://www.w3.org/ns/shacl#IRI" => {
                "sh:IRI".to_string()
            }
            _ => String::new(),
        };
        let min_count: Option<u32> = match s.get("minCount") {
            Some(Term::Literal(l)) => l.value().parse().ok(),
            _ => None,
        };

        let hint = build_shacl_hint(&in_values, &node_kind, min_count);
        if !hint.is_empty() {
            hints.insert(key, hint);
        }
    }

    hints
}

/// Walk an RDF list (rdf:first/rdf:rest … rdf:nil) collecting literal member
/// values IN LIST ORDER — SPARQL property paths can't guarantee order, the
/// chain itself does. Non-literal members are skipped (enum hints are about
/// literal values). Cycle-guarded.
fn rdf_list_literals(store: &oxigraph::store::Store, head: &Term) -> Vec<String> {
    use oxigraph::model::{NamedNodeRef, NamedOrBlankNodeRef};
    const RDF_FIRST: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#first";
    const RDF_REST: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#rest";
    const RDF_NIL: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#nil";
    let first = NamedNodeRef::new(RDF_FIRST).unwrap();
    let rest = NamedNodeRef::new(RDF_REST).unwrap();

    let mut out = Vec::new();
    let mut node = head.clone();
    let mut seen: HashSet<String> = HashSet::new();
    loop {
        let subject = match &node {
            Term::NamedNode(n) if n.as_str() == RDF_NIL => break,
            Term::NamedNode(n) => NamedOrBlankNodeRef::from(n.as_ref()),
            Term::BlankNode(b) => NamedOrBlankNodeRef::from(b.as_ref()),
            _ => break,
        };
        if !seen.insert(node.to_string()) { break; } // cycle guard
        if let Some(Ok(q)) = store
            .quads_for_pattern(Some(subject), Some(first), None, None)
            .next()
        {
            if let Term::Literal(l) = q.object {
                out.push(l.value().to_string());
            }
        }
        match store
            .quads_for_pattern(Some(subject), Some(rest), None, None)
            .next()
        {
            Some(Ok(q)) => node = q.object,
            _ => break,
        }
    }
    out
}

fn build_shacl_hint(in_values: &[String], node_kind: &str, min_count: Option<u32>) -> String {
    let required = min_count.map_or("optional", |n| if n > 0 { "required" } else { "optional" });
    if !in_values.is_empty() {
        format!("{}, enum: {}", required, in_values.join(", "))
    } else if node_kind == "sh:IRI" {
        format!("{}, IRI", required)
    } else {
        format!("{}, str", required)
    }
}

// ─── Core shape generation ────────────────────────────────────

/// Generate SHACL shapes from an oxigraph store containing an OWL ontology.
/// Takes the store, prefix name, namespace IRI, and a label for the comment header.
/// Returns SHACL Turtle string.
fn generate_shapes_from_store(
    store: &oxigraph::store::Store,
    prefix_name: &str,
    namespace: &str,
    source_label: &str,
) -> Option<String> {
    // Helper: extract local name from full IRI
    let local_name = |iri: &str| -> String {
        iri.rsplit('/').next().unwrap_or(iri).to_string()
    };

    // Query 1: Find all classes
    let classes: Vec<String> = {
        let q = "PREFIX owl: <http://www.w3.org/2002/07/owl#>
                 SELECT ?class WHERE { ?class a owl:Class }";
        match git_lex::eval_query(store, q) {
            Ok(oxigraph::sparql::QueryResults::Solutions(sols)) => {
                sols.filter_map(|s| s.ok().and_then(|s| {
                    s.get("class").map(|t| match t {
                        Term::NamedNode(n) => n.as_str().to_string(),
                        _ => String::new(),
                    })
                })).filter(|s| s.starts_with(namespace)).collect()
            }
            _ => Vec::new(),
        }
    };

    // Query 2: Find properties with domains, types, ranges, and comments
    struct PropInfo {
        iri: String,
        is_object_prop: bool,
        domain: String,
        range: String,
        comment: String,
    }
    let properties: Vec<PropInfo> = {
        let q = "PREFIX owl: <http://www.w3.org/2002/07/owl#>
                 PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
                 SELECT ?prop ?propType ?domain ?range ?comment WHERE {
                     ?prop rdfs:domain ?domain .
                     ?prop a ?propType .
                     FILTER(?propType IN (owl:DatatypeProperty, owl:ObjectProperty))
                     OPTIONAL { ?prop rdfs:range ?range }
                     OPTIONAL { ?prop rdfs:comment ?comment }
                 } ORDER BY ?domain ?prop";
        match git_lex::eval_query(store, q) {
            Ok(oxigraph::sparql::QueryResults::Solutions(sols)) => {
                sols.filter_map(|s| s.ok().map(|s| {
                    let term_str = |name: &str| -> String {
                        s.get(name).map(|t| match t {
                            Term::NamedNode(n) => n.as_str().to_string(),
                            Term::Literal(l) => l.value().to_string(),
                            _ => String::new(),
                        }).unwrap_or_default()
                    };
                    PropInfo {
                        iri: term_str("prop"),
                        is_object_prop: term_str("propType").contains("ObjectProperty"),
                        domain: term_str("domain"),
                        range: term_str("range"),
                        comment: term_str("comment"),
                    }
                })).collect()
            }
            _ => Vec::new(),
        }
    };

    // Query 3: Find enum values (rdfs:Datatype with owl:oneOf)
    let mut enum_values: HashMap<String, Vec<String>> = HashMap::new();
    {
        let q = "PREFIX owl: <http://www.w3.org/2002/07/owl#>
                 PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
                 PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
                 SELECT ?dtype ?value WHERE {
                     ?dtype a rdfs:Datatype ;
                            owl:oneOf ?list .
                     ?list rdf:rest*/rdf:first ?value .
                 } ORDER BY ?dtype ?value";
        if let Ok(oxigraph::sparql::QueryResults::Solutions(sols)) = git_lex::eval_query(store, q) {
            for s in sols.flatten() {
                let dtype = s.get("dtype").map(|t| match t {
                    Term::NamedNode(n) => n.as_str().to_string(),
                    _ => String::new(),
                }).unwrap_or_default();
                let value = s.get("value").map(|t| match t {
                    Term::Literal(l) => l.value().to_string(),
                    _ => String::new(),
                }).unwrap_or_default();
                if !dtype.is_empty() && !value.is_empty() {
                    enum_values.entry(dtype).or_default().push(value);
                }
            }
        }
    }

    // Query 4: Find required fields (owl:Restriction with minCardinality or cardinality)
    let mut required_props: HashSet<(String, String)> = HashSet::new(); // (class_iri, prop_iri)
    {
        let q = "PREFIX owl: <http://www.w3.org/2002/07/owl#>
                 PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
                 SELECT ?class ?prop WHERE {
                     ?class rdfs:subClassOf ?restriction .
                     ?restriction a owl:Restriction ;
                                  owl:onProperty ?prop .
                     { ?restriction owl:minCardinality ?card }
                     UNION
                     { ?restriction owl:cardinality ?card }
                     FILTER(?card >= 1)
                 }";
        if let Ok(oxigraph::sparql::QueryResults::Solutions(sols)) = git_lex::eval_query(store, q) {
            for s in sols.flatten() {
                let class = s.get("class").map(|t| match t {
                    Term::NamedNode(n) => n.as_str().to_string(),
                    _ => String::new(),
                }).unwrap_or_default();
                let prop = s.get("prop").map(|t| match t {
                    Term::NamedNode(n) => n.as_str().to_string(),
                    _ => String::new(),
                }).unwrap_or_default();
                if !class.is_empty() && !prop.is_empty() {
                    required_props.insert((class, prop));
                }
            }
        }
    }

    // Build the SHACL Turtle output
    let mut shacl = String::new();
    shacl.push_str("@prefix sh:    <http://www.w3.org/ns/shacl#> .\n");
    shacl.push_str(&format!("@prefix {}: <{}> .\n", prefix_name, namespace));
    shacl.push_str("@prefix xsd:   <http://www.w3.org/2001/XMLSchema#> .\n");
    shacl.push_str("@prefix rdfs:  <http://www.w3.org/2000/01/rdf-schema#> .\n\n");
    shacl.push_str(&format!("# Auto-generated SHACL shapes from {} ontology.\n", source_label));
    shacl.push_str("# Do not hand-edit — regenerate with: git lex kit-update\n\n");

    for class_iri in &classes {
        let class_name = local_name(class_iri);
        let shape_name = format!("{}Shape", class_name);

        shacl.push_str(&format!("\n# --- {} ---\n\n", class_name));
        shacl.push_str(&format!("{}:{} a sh:NodeShape ;\n", prefix_name, shape_name));
        shacl.push_str(&format!("    sh:targetClass {}:{}", prefix_name, class_name));

        // Collect properties for this class
        let class_props: Vec<&PropInfo> = properties.iter()
            .filter(|p| p.domain == *class_iri)
            .collect();

        if class_props.is_empty() {
            shacl.push_str(" .\n");
            continue;
        }

        for (i, prop) in class_props.iter().enumerate() {
            let prop_name = local_name(&prop.iri);
            let is_last = i == class_props.len() - 1;
            let is_required = required_props.contains(&(class_iri.clone(), prop.iri.clone()));

            shacl.push_str(" ;\n    sh:property [\n");
            shacl.push_str(&format!("        sh:path {}:{} ;\n", prefix_name, prop_name));

            if !prop.comment.is_empty() {
                let escaped = prop.comment.replace('\\', "\\\\").replace('"', "\\\"");
                shacl.push_str(&format!("        rdfs:comment \"{}\" ;\n", escaped));
            }

            if prop.is_object_prop {
                shacl.push_str("        sh:nodeKind sh:IRI ;\n");
                let msg = format!("{} must be an IRI reference.", prop_name);
                shacl.push_str(&format!("        sh:message \"{}\" ;\n", msg));
            } else if let Some(values) = enum_values.get(&prop.range) {
                let quoted: Vec<String> = values.iter().map(|v| format!("\"{}\"", v)).collect();
                shacl.push_str(&format!("        sh:in ( {} ) ;\n", quoted.join(" ")));
                let msg = format!("{} must be {}.",
                    prop_name,
                    values.iter().map(|v| format!("'{}'", v)).collect::<Vec<_>>().join(", "));
                shacl.push_str(&format!("        sh:message \"{}\" ;\n", msg));
            } else {
                let xsd_prefix = "http://www.w3.org/2001/XMLSchema#";
                if prop.range.starts_with(xsd_prefix) && prop.range != format!("{}string", xsd_prefix) {
                    let xsd_type = &prop.range[xsd_prefix.len()..];
                    shacl.push_str(&format!("        sh:datatype xsd:{} ;\n", xsd_type));
                    let msg = format!("Expected datatype: xsd:{}.", xsd_type);
                    shacl.push_str(&format!("        sh:message \"{}\" ;\n", msg));
                }
            }

            if is_required {
                shacl.push_str("        sh:minCount 1 ;\n");
            }

            if is_last {
                shacl.push_str("    ] .\n");
            } else {
                shacl.push_str("    ]");
            }
        }
    }

    Some(shacl)
}

// ─── Kit-based shapes ─────────────────────────────────────────

/// Generate SHACL shapes TTL from a kit ontology using SPARQL queries.
///
/// `Ok(None)` = kit has no ontology (nothing to generate). `Err` = the
/// ontology exists but is broken — callers must be LOUD (finding #22).
pub(crate) fn generate_shacl_shapes(kit: &str) -> Result<Option<String>, String> {
    let Some(store) = crate::kit::load_kit_into_store(kit)? else { return Ok(None) };
    let Some(ttl_path) = crate::kit::find_kit_ttl(kit) else { return Ok(None) };
    let ttl_content = fs::read_to_string(&ttl_path)
        .map_err(|e| format!("cannot read {}: {}", ttl_path.display(), e))?;

    // Kit prefix name + namespace come from the TTL's own declaration
    // (matched by prefix NAME via the shared scanner — namespace migrations
    // are a TTL edit, not a code change). Conventional fallback otherwise.
    let (_, _, short) = resolve_kit_spec(kit);
    let (prefix_name, namespace) = git_lex::extract_kit_prefix(&ttl_content, &short)
        .unwrap_or_else(|| (short.clone(), git_lex::conventional_kit_namespace(&short)));

    Ok(generate_shapes_from_store(&store, &prefix_name, &namespace, kit))
}

/// Generate and write SHACL shapes for a kit.
/// Returns the path to the generated shapes file.
///
/// Output location is chosen to live alongside the source TTL:
///   - static kit  → `.lex/ontology/{short}/{short}-shapes.ttl`
///   - adaptive kit → `_ontology/{short}/{short}-shapes.ttl`
pub(crate) fn build_shacl_shapes(kit: &str) -> Result<Option<PathBuf>, String> {
    let Some(shacl) = generate_shacl_shapes(kit)? else { return Ok(None) };
    let (_, _, short) = resolve_kit_spec(kit);
    // Locate the source TTL so we can drop the shapes file next to it.
    let Some(source_ttl) = crate::kit::find_kit_ttl(kit) else { return Ok(None) };
    let Some(ontology_dir) = source_ttl.parent().map(|p| p.to_path_buf()) else { return Ok(None) };
    fs::create_dir_all(&ontology_dir)
        .map_err(|e| format!("cannot create {}: {}", ontology_dir.display(), e))?;
    let shapes_path = ontology_dir.join(format!("{}-shapes.ttl", short));
    fs::write(&shapes_path, &shacl)
        .map_err(|e| format!("cannot write {}: {}", shapes_path.display(), e))?;
    Ok(Some(shapes_path))
}

// ─── Adaptive shapes (_ontology/) ─────────────────────────────

/// Scan `_ontology/` for agent-authored TTL files. For each, generate SHACL
/// shapes and write them alongside the source TTL. Returns a list of
/// (ttl_path, shapes_path) for successes and (ttl_path, error) for failures.
pub(crate) fn build_adaptive_shapes() -> (Vec<(PathBuf, PathBuf)>, Vec<(PathBuf, String)>) {
    let root = match find_git_root() {
        Some(r) => r,
        None => return (vec![], vec![]),
    };

    let adaptive_dir = root.join("_ontology");
    if !adaptive_dir.exists() {
        return (vec![], vec![]);
    }

    let mut successes: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut failures: Vec<(PathBuf, String)> = Vec::new();

    // Walk _ontology/{name}/{name}.ttl — same structure as .lex/ontology/
    let entries = match fs::read_dir(&adaptive_dir) {
        Ok(e) => e,
        Err(_) => return (vec![], vec![]),
    };

    for entry in entries.flatten() {
        if !entry.path().is_dir() { continue; }
        let subdir = entry.path();
        let ttl_files: Vec<PathBuf> = fs::read_dir(&subdir)
            .into_iter()
            .flat_map(|e| e.flatten())
            .filter(|e| {
                let p = e.path();
                p.extension().is_some_and(|ext| ext == "ttl")
                    && !p.file_name().unwrap_or_default().to_string_lossy().ends_with("-shapes.ttl")
            })
            .map(|e| e.path())
            .collect();

        for ttl_path in ttl_files {
            let ttl_content = match fs::read_to_string(&ttl_path) {
                Ok(c) => c,
                Err(e) => {
                    failures.push((ttl_path, format!("read error: {}", e)));
                    continue;
                }
            };

            // Load into temp store — the ONE TTL-text loader.
            let store = match crate::kit::load_ttl_str(
                &ttl_content,
                &ttl_path.display().to_string(),
            ) {
                Ok(s) => s,
                Err(e) => {
                    failures.push((ttl_path, e));
                    continue;
                }
            };

            // Detect prefix and namespace from the TTL.
            //
            // The convention is `_ontology/{short}/{short}.ttl`, so the filename
            // stem IS the kit short name. Prefer a `@prefix` line whose
            // namespace contains `/kit/{short}/` — that's how non-adaptive
            // build does it (generate_shacl_shapes line 325). Fall back to
            // the first non-system prefix only if no match is found.
            //
            // Without this, a TTL that imports an upper ontology (e.g.
            // `@prefix lex-o:` declared before `@prefix autoknow:`) caused
            // the upper-ontology prefix to be picked, and shape generation
            // then queried the wrong namespace and produced an empty file.
            let label = ttl_path.file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();

            // ONE scanner (git_lex::extract_kit_prefix) — the inline copy
            // this replaces still keyed on the retired /kit/ pattern and
            // would have mis-derived every adaptive TTL after the
            // namespace flip (review finding M2).
            let Some((prefix_name, namespace)) = git_lex::extract_kit_prefix(&ttl_content, &label) else {
                failures.push((ttl_path, "no prefix declaration found".to_string()));
                continue;
            };

            match generate_shapes_from_store(&store, &prefix_name, &namespace, &label) {
                Some(shacl) => {
                    let shapes_path = ttl_path.with_file_name(
                        format!("{}-shapes.ttl", label)
                    );
                    match fs::write(&shapes_path, &shacl) {
                        Ok(_) => successes.push((ttl_path, shapes_path)),
                        Err(e) => failures.push((ttl_path, format!("write error: {}", e))),
                    }
                }
                None => {
                    failures.push((ttl_path, "shape generation produced no output".to_string()));
                }
            }
        }
    }

    (successes, failures)
}
