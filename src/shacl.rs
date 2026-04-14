//! SHACL shape parsing and generation.
//!
//! - `parse_shacl_hints` reads a shapes TTL and pulls out inline hints
//!   (enum values, IRI node-kind, required/optional) that get embedded in
//!   class-template comments by `cmd create`.
//! - `generate_shacl_shapes` runs SPARQL against a loaded kit ontology to
//!   derive SHACL shapes automatically (owl:oneOf → sh:in, ObjectProperty
//!   → sh:nodeKind sh:IRI, owl:Restriction minCard → sh:minCount).
//! - `build_shacl_shapes` writes the generated shapes to
//!   `.lex/kit/.../{kit}-shapes.ttl`.
//!
//! Peeled out of `main.rs` during modularization. No behavior changes.

use oxigraph::model::Term;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use git_lex::{find_git_root, kit_install_dir_for_spec, resolve_kit_spec};

/// Parse SHACL shapes TTL to extract inline hints for class template comments.
/// Returns a map of property name → hint string (e.g. "enum: certain, likely, hypothesis, hunch")
pub(crate) fn parse_shacl_hints(shapes_ttl: &str) -> HashMap<String, String> {
    let mut hints: HashMap<String, String> = HashMap::new();
    let mut current_path = String::new();
    let mut current_in_values: Vec<String> = Vec::new();
    let mut current_node_kind = String::new();
    let mut current_min_count: Option<u32> = None;

    for line in shapes_ttl.lines() {
        let trimmed = line.trim();

        // sh:path soul:confidence ;
        if trimmed.starts_with("sh:path ") {
            // Flush previous property
            if !current_path.is_empty() {
                let hint = build_shacl_hint(&current_in_values, &current_node_kind, current_min_count);
                if !hint.is_empty() {
                    hints.insert(current_path.clone(), hint);
                }
            }
            current_path = trimmed
                .strip_prefix("sh:path ").unwrap_or("")
                .trim_end_matches(|c: char| c == ' ' || c == ';')
                .to_string();
            current_in_values.clear();
            current_node_kind.clear();
            current_min_count = None;
        }

        // sh:in ( "certain" "likely" "hypothesis" "hunch" ) ;
        if trimmed.starts_with("sh:in") {
            // Extract values between ( and )
            if let Some(start) = trimmed.find('(') {
                if let Some(end) = trimmed.find(')') {
                    let values_str = &trimmed[start + 1..end];
                    current_in_values = values_str
                        .split('"')
                        .filter(|s| !s.trim().is_empty())
                        .map(|s| s.to_string())
                        .collect();
                }
            }
        }

        // sh:nodeKind sh:IRI ;
        if trimmed.starts_with("sh:nodeKind") {
            current_node_kind = trimmed
                .strip_prefix("sh:nodeKind ").unwrap_or("")
                .trim_end_matches(|c: char| c == ' ' || c == ';')
                .to_string();
        }

        // sh:minCount 1 ;
        if trimmed.starts_with("sh:minCount") {
            if let Some(num_str) = trimmed.split_whitespace().nth(1) {
                current_min_count = num_str.trim_end_matches(|c: char| c == ' ' || c == ';').parse().ok();
            }
        }
    }

    // Flush last property
    if !current_path.is_empty() {
        let hint = build_shacl_hint(&current_in_values, &current_node_kind, current_min_count);
        if !hint.is_empty() {
            hints.insert(current_path, hint);
        }
    }

    hints
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

/// Generate SHACL shapes TTL from a kit ontology using SPARQL queries.
/// Reads OWL constraints (owl:oneOf, owl:Restriction, owl:ObjectProperty, rdfs:range)
/// and emits equivalent SHACL shapes.
pub(crate) fn generate_shacl_shapes(kit: &str) -> Option<String> {
    let store = crate::kit::load_kit_into_store(kit)?;
    let ttl_path = crate::kit::find_kit_ttl(kit)?;
    let ttl_content = fs::read_to_string(&ttl_path).ok()?;

    // Find the kit prefix name and namespace from the TTL. Namespace uses
    // the short kit name, not the full org/repo spec.
    let (_, _, short) = resolve_kit_spec(kit);
    let kit_ns_pattern = format!("/kit/{}/", short);
    let mut prefix_name = short.clone();
    let mut namespace = format!("https://repolex.ai/ontology/kit/{}/", short);
    for line in ttl_content.lines() {
        if line.starts_with("@prefix ") && line.contains(&kit_ns_pattern) {
            if let Some(colon_pos) = line[8..].find(':') {
                prefix_name = line[8..8 + colon_pos].trim().to_string();
            }
            if let Some(start) = line.find('<') {
                if let Some(end) = line.find('>') {
                    namespace = line[start + 1..end].to_string();
                }
            }
            break;
        }
    }

    // Helper: extract local name from full IRI
    let local_name = |iri: &str| -> String {
        iri.rsplit('/').next().unwrap_or(iri).to_string()
    };

    // Query 1: Find all classes
    let classes: Vec<String> = {
        let q = "PREFIX owl: <http://www.w3.org/2002/07/owl#>
                 SELECT ?class WHERE { ?class a owl:Class }";
        match store.query(q) {
            Ok(oxigraph::sparql::QueryResults::Solutions(sols)) => {
                sols.filter_map(|s| s.ok().and_then(|s| {
                    s.get("class").map(|t| match t {
                        Term::NamedNode(n) => n.as_str().to_string(),
                        _ => String::new(),
                    })
                })).filter(|s| s.starts_with(&namespace)).collect()
            }
            _ => Vec::new(),
        }
    };

    // Query 2: Find properties with domains, types, and ranges
    struct PropInfo {
        iri: String,
        is_object_prop: bool,
        domain: String,
        range: String,
    }
    let properties: Vec<PropInfo> = {
        let q = "PREFIX owl: <http://www.w3.org/2002/07/owl#>
                 PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
                 SELECT ?prop ?propType ?domain ?range WHERE {
                     ?prop rdfs:domain ?domain .
                     ?prop a ?propType .
                     FILTER(?propType IN (owl:DatatypeProperty, owl:ObjectProperty))
                     OPTIONAL { ?prop rdfs:range ?range }
                 } ORDER BY ?domain ?prop";
        match store.query(q) {
            Ok(oxigraph::sparql::QueryResults::Solutions(sols)) => {
                sols.filter_map(|s| s.ok().map(|s| {
                    let term_str = |name: &str| -> String {
                        s.get(name).map(|t| match t {
                            Term::NamedNode(n) => n.as_str().to_string(),
                            _ => String::new(),
                        }).unwrap_or_default()
                    };
                    PropInfo {
                        iri: term_str("prop"),
                        is_object_prop: term_str("propType").contains("ObjectProperty"),
                        domain: term_str("domain"),
                        range: term_str("range"),
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
        if let Ok(oxigraph::sparql::QueryResults::Solutions(sols)) = store.query(q) {
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

    // Query 4: Find required fields (owl:Restriction with minCardinality)
    let mut required_props: HashSet<(String, String)> = HashSet::new(); // (class_iri, prop_iri)
    {
        let q = "PREFIX owl: <http://www.w3.org/2002/07/owl#>
                 PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
                 SELECT ?class ?prop WHERE {
                     ?class rdfs:subClassOf ?restriction .
                     ?restriction a owl:Restriction ;
                                  owl:onProperty ?prop ;
                                  owl:minCardinality ?minCard .
                     FILTER(?minCard >= 1)
                 }";
        if let Ok(oxigraph::sparql::QueryResults::Solutions(sols)) = store.query(q) {
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
    shacl.push_str(&format!("# Auto-generated SHACL shapes from {} ontology.\n", kit));
    shacl.push_str("# Do not hand-edit — regenerate with: git lex kit update\n\n");

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

/// Generate and write SHACL shapes for the current kit.
/// Returns the path to the generated shapes file.
pub(crate) fn build_shacl_shapes(kit: &str) -> Option<PathBuf> {
    let root = find_git_root()?;
    let shacl = generate_shacl_shapes(kit)?;
    let kit_dir = kit_install_dir_for_spec(&root, kit);
    let (_, _, short) = resolve_kit_spec(kit);
    let shapes_path = kit_dir.join(format!("{}-shapes.ttl", short));
    fs::write(&shapes_path, &shacl).ok()?;
    Some(shapes_path)
}
