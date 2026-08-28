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
//!
//! Peeled out of `main.rs` during modularization. No behavior changes.

use oxigraph::model::Term;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use git_lex::resolve_kit_spec;

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
             SELECT ?prop ?path ?nodeKind ?minCount ?inList ?datatype ?minIncl ?maxIncl WHERE {
                 ?prop sh:path ?path .
                 OPTIONAL { ?prop sh:nodeKind ?nodeKind }
                 OPTIONAL { ?prop sh:minCount ?minCount }
                 OPTIONAL { ?prop sh:in ?inList }
                 OPTIONAL { ?prop sh:datatype ?datatype }
                 OPTIONAL { ?prop sh:minInclusive ?minIncl }
                 OPTIONAL { ?prop sh:maxInclusive ?maxIncl }
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

        // The declared datatype and its bounds, so the template teaches the
        // real type instead of calling every literal a string (#100).
        let datatype = match s.get("datatype") {
            Some(Term::NamedNode(n)) => n.as_str().to_string(),
            _ => String::new(),
        };
        let lit_value = |name: &str| -> Option<String> {
            match s.get(name) {
                Some(Term::Literal(l)) => Some(l.value().to_string()),
                _ => None,
            }
        };

        let hint = build_shacl_hint(
            &in_values,
            &node_kind,
            min_count,
            &datatype,
            lit_value("minIncl"),
            lit_value("maxIncl"),
        );
        if !hint.is_empty() {
            // An INHERITED property keeps its own (foreign) namespace in
            // sh:path, so its key here is the bracketed IRI — but the template
            // emitter looks properties up as `{thisKitPrefix}:{localName}`,
            // because that is what the author's frontmatter key resolves to
            // (Rob's own-class ruling: soul.Note.title, not git-lex.Thing.title).
            // Index it under BOTH so an inherited field keeps its type hint
            // instead of silently rendering blank (#104).
            //
            // `or_insert` not `insert`: if this kit declares its own property
            // of the same local name, that one is the author's and wins.
            if key.starts_with('<') {
                if let Some(local) = path.as_str().rsplit(['/', '#']).next() {
                    hints.entry(format!("{}:{}", prefix_name, local))
                        .or_insert_with(|| hint.clone());
                }
            }
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

/// Every class `class_iri` inherits from, nearest parent first, transitively.
///
/// Named parents only: `rdfs:subClassOf` also carries the blank-node
/// `owl:Restriction` axioms that declare cardinality, and those are not
/// classes whose properties anyone inherits — Query 4 reads them separately.
/// Cycle-guarded, because an ontology that says A is a B is an A should
/// produce a wrong shape, not a hung command.
fn ancestor_chain(store: &oxigraph::store::Store, class_iri: &str) -> Vec<String> {
    const SUB_CLASS_OF: &str = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
    let Ok(pred) = oxigraph::model::NamedNodeRef::new(SUB_CLASS_OF) else { return Vec::new() };

    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut frontier: Vec<String> = vec![class_iri.to_string()];
    seen.insert(class_iri.to_string());

    while let Some(current) = frontier.pop() {
        let Ok(subject) = oxigraph::model::NamedNode::new(&current) else { continue };
        for q in store
            .quads_for_pattern(Some((&subject).into()), Some(pred), None, None)
            .flatten()
        {
            if let Term::NamedNode(parent) = q.object {
                let iri = parent.as_str().to_string();
                if seen.insert(iri.clone()) {
                    out.push(iri.clone());
                    frontier.push(iri);
                }
            }
        }
    }
    out
}

/// Render an XSD datatype IRI as the word an author should see in a template.
///
/// The template is the surface people copy when creating a document, often
/// without reading the ontology at all — so calling a boolean "str" teaches
/// the wrong type on exactly the fields where getting it wrong is easiest
/// (#100, tr1p's copia specimen: `lookAnatomyReject: # optional, str` for a
/// hard boolean gate). Unknown types fall back to their own local name rather
/// than to "str": naming a type we don't have a friendly word for is honest;
/// calling it a string is not.
fn friendly_datatype(datatype_iri: &str) -> Option<String> {
    const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
    let local = datatype_iri.strip_prefix(XSD)?;
    Some(match local {
        "boolean" => "bool".to_string(),
        "integer" | "int" | "long" | "short" | "nonNegativeInteger" | "positiveInteger"
        | "negativeInteger" | "nonPositiveInteger" | "unsignedInt" | "unsignedLong" => {
            "int".to_string()
        }
        "decimal" | "double" | "float" => "number".to_string(),
        "date" => "date".to_string(),
        "dateTime" => "datetime".to_string(),
        "time" => "time".to_string(),
        "anyURI" => "url".to_string(),
        "string" => "str".to_string(),
        other => other.to_string(),
    })
}

fn build_shacl_hint(
    in_values: &[String],
    node_kind: &str,
    min_count: Option<u32>,
    datatype_iri: &str,
    min_inclusive: Option<String>,
    max_inclusive: Option<String>,
) -> String {
    let required = min_count.map_or("optional", |n| if n > 0 { "required" } else { "optional" });
    if !in_values.is_empty() {
        format!("{}, enum: {}", required, in_values.join(", "))
    } else if node_kind == "sh:IRI" {
        format!("{}, IRI", required)
    } else {
        // Type word from the declared datatype; "str" only when that is what
        // the ontology actually says (or when it says nothing at all).
        let type_word = friendly_datatype(datatype_iri).unwrap_or_else(|| "str".to_string());
        let range = match (min_inclusive, max_inclusive) {
            (Some(lo), Some(hi)) => format!(" {}-{}", lo, hi),
            (Some(lo), None) => format!(" min {}", lo),
            (None, Some(hi)) => format!(" max {}", hi),
            (None, None) => String::new(),
        };
        format!("{}, {}{}", required, type_word, range)
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

    // Query 4b: QUALIFIED cardinality — "at least/exactly N of this property's
    // values are of class K". Standard OWL 2 (owl:onClass +
    // owl:minQualifiedCardinality / owl:qualifiedCardinality), sitting on the
    // same owl:Restriction node Query 4 already walks. Rob's build order via
    // @tr1p, 2026-08-27: it replaces the relatedTo{Class}Id twins, every one of
    // which had rdfs:range git-lex:Thing and therefore never constrained type
    // at all — the class lived only in the property NAME.
    //
    // ROB'S DEFAULT, encoded here rather than special-cased: NO RESTRICTION =
    // NO ENFORCEMENT. A class that declares nothing about relatedToId gets no
    // shape emitted for it and anything may go in. Silence in the ontology is
    // permission, not prohibition.
    struct QualRestriction {
        class_iri: String,
        prop_iri: String,
        on_class: String,
        min: u32,
        exact: bool,
    }
    let mut qualified: Vec<QualRestriction> = Vec::new();
    {
        let q = "PREFIX owl: <http://www.w3.org/2002/07/owl#>
                 PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
                 SELECT ?class ?prop ?onClass ?min ?exactCard WHERE {
                     ?class rdfs:subClassOf ?restriction .
                     ?restriction a owl:Restriction ;
                                  owl:onProperty ?prop ;
                                  owl:onClass ?onClass .
                     { ?restriction owl:minQualifiedCardinality ?min }
                     UNION
                     { ?restriction owl:qualifiedCardinality ?exactCard }
                 }";
        if let Ok(oxigraph::sparql::QueryResults::Solutions(sols)) = git_lex::eval_query(store, q) {
            for s in sols.flatten() {
                let iri_of = |k: &str| s.get(k).and_then(|t| match t {
                    Term::NamedNode(n) => Some(n.as_str().to_string()),
                    _ => None,
                });
                let num_of = |k: &str| s.get(k).and_then(|t| match t {
                    Term::Literal(l) => l.value().parse::<u32>().ok(),
                    _ => None,
                });
                let (Some(class_iri), Some(prop_iri), Some(on_class)) =
                    (iri_of("class"), iri_of("prop"), iri_of("onClass")) else { continue };
                let exact_card = num_of("exactCard");
                let min = exact_card.or_else(|| num_of("min")).unwrap_or(0);
                if min == 0 { continue }
                qualified.push(QualRestriction {
                    class_iri, prop_iri, on_class, min, exact: exact_card.is_some(),
                });
            }
        }
    }

    // Query 5: Find BOUNDED custom datatypes — `rdfs:Datatype` declared with
    // `owl:onDatatype` (the base type) plus `owl:withRestrictions` (an RDF list
    // of facet nodes, e.g. `[ xsd:minInclusive 1 ]`). This is the formally
    // correct way to declare "an integer between 1 and 5", and before this
    // query such a range produced NO constraint at all — not even the base
    // datatype — because the emitter only recognized bare xsd types. Declaring
    // the MORE precise type therefore left the data LESS protected than plain
    // xsd:integer, silently (tr1p, copia LookScoreValue, 2026-08-11).
    struct BoundedDatatype {
        base: String,
        // (facet local name, lexical value)
        facets: Vec<(String, String)>,
    }
    let mut bounded_datatypes: HashMap<String, BoundedDatatype> = HashMap::new();
    {
        let q = "PREFIX owl: <http://www.w3.org/2002/07/owl#>
                 PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
                 PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
                 SELECT ?dtype ?base ?facet ?value WHERE {
                     ?dtype a rdfs:Datatype ;
                            owl:onDatatype ?base ;
                            owl:withRestrictions ?list .
                     ?list rdf:rest*/rdf:first ?restriction .
                     ?restriction ?facet ?value .
                 } ORDER BY ?dtype ?facet";
        if let Ok(oxigraph::sparql::QueryResults::Solutions(sols)) = git_lex::eval_query(store, q) {
            for s in sols.flatten() {
                let iri_of = |name: &str| -> String {
                    s.get(name).map(|t| match t {
                        Term::NamedNode(n) => n.as_str().to_string(),
                        _ => String::new(),
                    }).unwrap_or_default()
                };
                let dtype = iri_of("dtype");
                let base = iri_of("base");
                let facet = iri_of("facet");
                let value = s.get("value").map(|t| match t {
                    Term::Literal(l) => l.value().to_string(),
                    Term::NamedNode(n) => n.as_str().to_string(),
                    _ => String::new(),
                }).unwrap_or_default();
                if dtype.is_empty() || base.is_empty() || facet.is_empty() {
                    continue;
                }
                let entry = bounded_datatypes.entry(dtype).or_insert_with(|| BoundedDatatype {
                    base: base.clone(),
                    facets: Vec::new(),
                });
                let facet_local = facet.rsplit('#').next().unwrap_or(&facet).to_string();
                if !value.is_empty() {
                    entry.facets.push((facet_local, value));
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

        // Properties of this class AND of every class it inherits from
        // (#104). This used to be an exact IRI match on the domain, so a
        // property declared on a parent reached no child at all.
        //
        // Not theoretical and not new: copia:Group is an abstract parent
        // declaring groupTitle, groupDepictedBy and fromNocturneId, and its
        // subclasses copia:Set and copia:Sequence received none of them —
        // shipped, unnoticed only because nobody authors a Group. The
        // git-lex:Thing properties are simply the first case where it had to
        // work.
        //
        // Own properties first, then inherited, so the generated shape reads
        // in the order the author thinks in. A child re-declaring a parent's
        // property wins, because its own domain already placed it.
        let ancestors = ancestor_chain(store, class_iri);
        let mut class_props: Vec<&PropInfo> = properties.iter()
            .filter(|p| p.domain == *class_iri)
            .collect();
        for ancestor in &ancestors {
            for p in properties.iter().filter(|p| p.domain == *ancestor) {
                if !class_props.iter().any(|existing| existing.iri == p.iri) {
                    class_props.push(p);
                }
            }
        }

        // Qualified restrictions for this class OR any ancestor — a parent
        // that declares "at least one Place" constrains its children too, the
        // same inheritance Query 4's required-ness already follows.
        let class_quals: Vec<&QualRestriction> = qualified.iter()
            .filter(|q| q.class_iri == *class_iri || ancestors.contains(&q.class_iri))
            .collect();

        if class_props.is_empty() && class_quals.is_empty() {
            shacl.push_str(" .\n");
            continue;
        }

        for (i, prop) in class_props.iter().enumerate() {
            let prop_name = local_name(&prop.iri);
            // The class no longer necessarily ends at the last PROPERTY —
            // qualified blocks may follow it.
            let is_last = i == class_props.len() - 1 && class_quals.is_empty();
            // A required-ness restriction can sit on the class OR on any
            // ancestor — an inherited property that a parent declares required
            // is required here too (#104).
            let is_required = required_props.contains(&(class_iri.clone(), prop.iri.clone()))
                || ancestors.iter().any(|a| required_props.contains(&(a.clone(), prop.iri.clone())));

            shacl.push_str(" ;\n    sh:property [\n");
            // An INHERITED property usually lives in another kit's namespace
            // (git-lex:title on a soul class), where the local prefix would
            // name a different IRI entirely. Full bracketed IRI in that case —
            // always valid Turtle, and parse_shacl_hints already handles the
            // bracketed form.
            match prop.iri.strip_prefix(namespace) {
                Some(local) => shacl.push_str(&format!("        sh:path {}:{} ;\n", prefix_name, local)),
                None => shacl.push_str(&format!("        sh:path <{}> ;\n", prop.iri)),
            }

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
            } else if let Some(bounded) = bounded_datatypes.get(&prop.range) {
                // A bounded custom datatype: emit the base type AND the bounds.
                let xsd_prefix = "http://www.w3.org/2001/XMLSchema#";
                let base_local = if bounded.base.starts_with(xsd_prefix) {
                    let t = &bounded.base[xsd_prefix.len()..];
                    shacl.push_str(&format!("        sh:datatype xsd:{} ;\n", t));
                    t.to_string()
                } else {
                    // A base we cannot express as an xsd type. Say so — a
                    // constraint we silently dropped is the whole defect class.
                    eprintln!(
                        "warning: {} declares owl:onDatatype <{}>, which is not an XSD type — \
no sh:datatype emitted for properties ranged at it. Range them at an XSD base type, \
or the values save ungoverned.",
                        local_name(&prop.range), bounded.base
                    );
                    String::new()
                };

                let mut described: Vec<String> = Vec::new();
                for (facet, value) in &bounded.facets {
                    // XSD facet -> SHACL constraint. Numeric facets take a bare
                    // literal; pattern takes a quoted string.
                    let emitted = match facet.as_str() {
                        "minInclusive" => Some(("sh:minInclusive", true, format!("at least {}", value))),
                        "maxInclusive" => Some(("sh:maxInclusive", true, format!("at most {}", value))),
                        "minExclusive" => Some(("sh:minExclusive", true, format!("greater than {}", value))),
                        "maxExclusive" => Some(("sh:maxExclusive", true, format!("less than {}", value))),
                        "minLength"    => Some(("sh:minLength",    true, format!("at least {} characters", value))),
                        "maxLength"    => Some(("sh:maxLength",    true, format!("at most {} characters", value))),
                        "pattern"      => Some(("sh:pattern",      false, format!("matching {}", value))),
                        _ => None,
                    };
                    match emitted {
                        Some((sh_name, bare, description)) => {
                            if bare {
                                shacl.push_str(&format!("        {} {} ;\n", sh_name, value));
                            } else {
                                let esc = value.replace('\\', "\\\\").replace('"', "\\\"");
                                shacl.push_str(&format!("        {} \"{}\" ;\n", sh_name, esc));
                            }
                            described.push(description);
                        }
                        None => {
                            // NOT silently skipped — an untranslated facet is a
                            // bound the author declared and the data will not carry.
                            eprintln!(
                                "warning: {} declares the XSD facet '{}' ({}), which git-lex does not \
translate to a SHACL constraint — that bound is NOT enforced. Report it so the \
generator learns it, or express the bound with a facet git-lex knows \
(minInclusive, maxInclusive, minExclusive, maxExclusive, minLength, maxLength, pattern).",
                                local_name(&prop.range), facet, value
                            );
                        }
                    }
                }

                let msg = if described.is_empty() {
                    format!("Expected datatype: xsd:{}.", base_local)
                } else if base_local.is_empty() {
                    format!("{} must be {}.", prop_name, described.join(", "))
                } else {
                    format!("{} must be an xsd:{} {}.", prop_name, base_local, described.join(", "))
                };
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

        // QUALIFIED BLOCKS. One sh:property per restriction, in the form
        // proved by `probe_pattern_inside_qualified_value_shape`.
        //
        // The pattern is DERIVED from owl:onClass's local name — the author
        // declares semantics ("at least one Place") and never writes a regex.
        //
        // WHY A PATTERN AND NOT sh:class: the save gate builds a fresh graph
        // per DOCUMENT, so a referenced Thing's rdf:type is never present and
        // sh:class would fail every document on every save (probed:
        // `probe_qualified_value_shape_capability`). The pattern reads the
        // class out of the IRI path instead and resolves nothing.
        //
        // THE FLIP CONDITION, named so the upgrade is an edit and not a
        // rediscovery: the day validation builds ONE graph over MORE THAN ONE
        // document, swap the sh:pattern line for `sh:class <onClass>`. That is
        // strictly better — it checks what a thing IS rather than what its
        // name looks like — and needs no ontology change.
        //
        // LOAD-BEARING DEPENDENCY (@tr1p's words, kept deliberately): this
        // reads the CLASS OUT OF THE IRI PATH, sound only because instance
        // IRIs are <namespace/Class/id> by the naming law Rob ruled
        // 2026-07-16. It does NOT resolve the target node. If that law ever
        // softens, this check silently weakens and nothing here will say so.
        for (i, qr) in class_quals.iter().enumerate() {
            let is_last = i == class_quals.len() - 1;
            let on_local = local_name(&qr.on_class);
            shacl.push_str(" ;\n    sh:property [\n");
            match qr.prop_iri.strip_prefix(namespace) {
                Some(local) => shacl.push_str(&format!("        sh:path {}:{} ;\n", prefix_name, local)),
                None => shacl.push_str(&format!("        sh:path <{}> ;\n", qr.prop_iri)),
            }
            // NO sh:nodeKind here, deliberately. The literal hole is real —
            // resolve.rs rule 7 keeps an UNRESOLVED reference as a string
            // literal, and a bare pattern would match it and count a broken
            // reference as satisfied — but it is ALREADY closed one shape up:
            // relatedToId is an owl:ObjectProperty with rdfs:domain
            // git-lex:Thing, so every Thing class already gets an unconditional
            // `sh:nodeKind sh:IRI` property shape on this path. Rob's
            // "must actually point at something" rule is shipped behaviour, not
            // new work (verified against copia-shapes.ttl, 2026-08-27).
            //
            // Emitting it twice would be two sources for one fact. Instead the
            // baseline is PINNED by `object_properties_always_get_nodekind_iri`
            // — if it ever stops being emitted, that test fails loudly rather
            // than this block quietly losing its guard.
            shacl.push_str("        sh:qualifiedValueShape [ sh:pattern \"/");
            shacl.push_str(&on_local);
            shacl.push_str("/\" ] ;\n");
            shacl.push_str(&format!("        sh:qualifiedMinCount {} ;\n", qr.min));
            if qr.exact {
                shacl.push_str(&format!("        sh:qualifiedMaxCount {} ;\n", qr.min));
            }
            let how_many = if qr.exact {
                format!("exactly {}", qr.min)
            } else if qr.min == 1 {
                "at least one".to_string()
            } else {
                format!("at least {}", qr.min)
            };
            shacl.push_str(&format!(
                "        sh:message \"{} must reference {} {} — <.../{}/&lt;id&gt;>.\" ;\n",
                local_name(class_iri), how_many, on_local, on_local
            ));
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
    // EVERY installed vocabulary, not just this kit's (#104) — a class whose
    // parent lives in another kit (rdfs:subClassOf git-lex:Thing) cannot have
    // its chain walked if the parent was never loaded. Class emission stays
    // namespace-filtered below, so the extra vocabulary resolves parents
    // without leaking other kits' shapes into this file.
    let Some(store) = crate::kit::load_all_kit_ontologies_into_store(kit)? else { return Ok(None) };
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
/// Output location lives alongside the source TTL:
/// `.lex/ontology/{short}/{short}-shapes.ttl`.
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



#[cfg(test)]
mod bounded_datatype_tests {
    use super::*;

    /// tr1p's exact copia specimen (2026-08-11): a score declared as an integer
    /// bounded 1..5 via `owl:withRestrictions`. Before this, the generator
    /// emitted NOTHING for such a property — not even the base datatype — so
    /// declaring the more precise type left the data less protected than plain
    /// `xsd:integer`, silently.
    const BOUNDED_TTL: &str = r#"
@prefix owl:  <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .
@prefix t:    <https://repolex.ai/ontology/t/> .

t:LookScoreValue a rdfs:Datatype ;
    owl:onDatatype xsd:integer ;
    owl:withRestrictions ( [ xsd:minInclusive 1 ] [ xsd:maxInclusive 5 ] ) .

t:Look a owl:Class .

t:lookTechnicalScore a owl:DatatypeProperty ;
    rdfs:domain t:Look ;
    rdfs:range t:LookScoreValue .
"#;

    fn shapes_for(ttl: &str) -> String {
        let store = crate::kit::load_ttl_str(ttl, "test").expect("ttl loads");
        generate_shapes_from_store(&store, "t", "https://repolex.ai/ontology/t/", "test")
            .expect("shapes generate")
    }

    /// Rob's build order via @tr1p, 2026-08-27: standard OWL 2 qualified
    /// cardinality replaces the relatedTo{Class}Id twins. The author declares
    /// SEMANTICS; the generator picks the enforceable form.
    #[test]
    fn qualified_cardinality_becomes_a_qualified_shape() {
        let ttl = r#"
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix t: <https://repolex.ai/ontology/t/> .
@prefix gl: <https://repolex.ai/ontology/git-lex/> .

t:Place a owl:Class .
t:Being a owl:Class .

# Mirrors reality: git-lex.ttl declares relatedToId as an ObjectProperty with
# rdfs:domain git-lex:Thing, which is what produces Rob's ALWAYS-ENFORCED
# baseline (sh:nodeKind sh:IRI) on every Thing class. The qualified shapes below
# depend on that baseline for the broken-reference case, so the fixture has to
# carry it or the test is checking a world we do not ship.
t:Thing a owl:Class .
gl:relatedToId a owl:ObjectProperty ; rdfs:domain t:Thing ; rdfs:range t:Thing .

t:ScenarioTake a owl:Class ;
    rdfs:subClassOf t:Thing ;
    rdfs:subClassOf [ a owl:Restriction ;
        owl:onProperty gl:relatedToId ; owl:onClass t:Place ;
        owl:qualifiedCardinality 1 ] ;
    rdfs:subClassOf [ a owl:Restriction ;
        owl:onProperty gl:relatedToId ; owl:onClass t:Being ;
        owl:minQualifiedCardinality 1 ] .

t:takeName a owl:DatatypeProperty ; rdfs:domain t:ScenarioTake ; rdfs:range <http://www.w3.org/2001/XMLSchema#string> .
"#;
        let out = shapes_for(ttl);

        // The pattern is DERIVED from owl:onClass — the author writes no regex.
        assert!(out.contains(r#"sh:qualifiedValueShape [ sh:pattern "/Place/" ]"#),
            "Place restriction must become a qualified pattern shape:\n{out}");
        assert!(out.contains(r#"sh:qualifiedValueShape [ sh:pattern "/Being/" ]"#),
            "Being restriction must become a qualified pattern shape:\n{out}");

        // Exact cardinality gets a MAX too; min-only does not.
        assert!(out.contains("sh:qualifiedMaxCount 1"),
            "owl:qualifiedCardinality is EXACTLY n, so a max must be emitted:\n{out}");
        assert_eq!(out.matches("sh:qualifiedMaxCount").count(), 1,
            "owl:minQualifiedCardinality is a floor and must NOT gain a ceiling:\n{out}");

        // THE LITERAL HOLE, closed one shape up rather than here. An unresolved
        // reference stays a string literal (resolve.rs rule 7) and a bare
        // pattern would match it — but relatedToId is an ObjectProperty on
        // Thing, so every Thing class already carries an unconditional
        // sh:nodeKind sh:IRI on this path. Rob's "must actually point at
        // something" rule is shipped behaviour, and the qualified shapes lean
        // on it. Asserted HERE so the qualified form cannot silently lose its
        // guard if the baseline ever stops being emitted.
        assert!(out.contains("sh:nodeKind sh:IRI"),
            "a qualified pattern without sh:nodeKind counts a BROKEN reference as satisfied:\n{out}");

        assert!(out.contains("must reference exactly 1 Place"), "message names the requirement:\n{out}");
        assert!(out.contains("must reference at least one Being"), "min-1 reads naturally:\n{out}");
    }

    /// ROB'S DEFAULT, and it is the half that must not regress: silence in the
    /// ontology is PERMISSION. A class that says nothing about relatedToId gets
    /// no shape for it, and anything may go in.
    #[test]
    fn no_restriction_means_no_enforcement() {
        let ttl = r#"
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix t: <https://repolex.ai/ontology/t/> .
t:Plain a owl:Class .
t:plainName a owl:DatatypeProperty ; rdfs:domain t:Plain ; rdfs:range <http://www.w3.org/2001/XMLSchema#string> .
"#;
        let out = shapes_for(ttl);
        assert!(!out.contains("sh:qualifiedValueShape"),
            "a class declaring no restriction must get NO qualified shape:\n{out}");
    }

    #[test]
    fn bounded_datatype_emits_base_type_and_both_bounds() {
        let out = shapes_for(BOUNDED_TTL);
        assert!(out.contains("sh:datatype xsd:integer"),
            "base type must survive the custom datatype:\n{out}");
        assert!(out.contains("sh:minInclusive 1"), "lower bound missing:\n{out}");
        assert!(out.contains("sh:maxInclusive 5"), "upper bound missing:\n{out}");
    }

    /// The regression that motivated the fix: the property must not come out
    /// bare. Before, this shape had a path and a comment and nothing else.
    #[test]
    fn bounded_datatype_property_is_not_constraint_free() {
        let out = shapes_for(BOUNDED_TTL);
        let block = out
            .split("sh:path t:lookTechnicalScore")
            .nth(1)
            .expect("property shape present");
        let block = block.split("] ;").next().unwrap_or(block);
        assert!(
            block.contains("sh:datatype") || block.contains("sh:minInclusive"),
            "property shape carries NO constraint — the exact defect:\n{block}"
        );
    }

    /// The message should teach the bound, not just name a type.
    #[test]
    fn bounded_datatype_message_states_the_range() {
        let out = shapes_for(BOUNDED_TTL);
        assert!(out.contains("at least 1"), "message omits lower bound:\n{out}");
        assert!(out.contains("at most 5"), "message omits upper bound:\n{out}");
    }

    /// A plain xsd range must keep working exactly as before.
    #[test]
    fn plain_xsd_range_unchanged() {
        let ttl = r#"
@prefix owl:  <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .
@prefix t:    <https://repolex.ai/ontology/t/> .
t:Look a owl:Class .
t:plainScore a owl:DatatypeProperty ;
    rdfs:domain t:Look ;
    rdfs:range xsd:integer .
"#;
        let out = shapes_for(ttl);
        assert!(out.contains("sh:datatype xsd:integer"), "plain xsd regressed:\n{out}");
    }
}

#[cfg(test)]
mod class_annotation_tests {
    use super::*;

    /// tr1p's pin (authoringGuidance review, 2026-08-24): an annotation ON A
    /// CLASS has no business producing a `sh:property`, and "it didn't" is
    /// worth a test rather than an assumption. The generator's property
    /// discovery filters on owl:DatatypeProperty/owl:ObjectProperty; this
    /// holds that line for git-lex:authoringGuidance and git-lex:foldered
    /// (both owl:AnnotationProperty since kit-base 0.10.3).
    #[test]
    fn class_annotations_never_reach_the_shapes() {
        let ttl = r###"
@prefix owl:  <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .
@prefix t:    <https://repolex.ai/ontology/t/> .
@prefix git-lex: <https://repolex.ai/ontology/git-lex/> .

git-lex:foldered a owl:AnnotationProperty .
git-lex:authoringGuidance a owl:AnnotationProperty ;
    rdfs:domain owl:Class ;
    rdfs:range xsd:string .

t:Journal a owl:Class ;
    git-lex:foldered true ;
    git-lex:authoringGuidance """## Sections
One line each.""" .

t:journalId a owl:DatatypeProperty ;
    rdfs:domain t:Journal ;
    rdfs:range xsd:string .
"###;
        let store = crate::kit::load_ttl_str(ttl, "test").expect("ttl loads");
        let out =
            generate_shapes_from_store(&store, "t", "https://repolex.ai/ontology/t/", "test")
                .expect("shapes generate");
        assert!(out.contains("sh:path t:journalId"),
            "the real property must still shape:\n{out}");
        assert!(!out.contains("authoringGuidance"),
            "class annotation leaked into the shapes:\n{out}");
        assert!(!out.contains("foldered"),
            "foldered leaked into the shapes:\n{out}");
    }
}

#[cfg(test)]
mod template_hint_tests {
    use super::*;

    /// tr1p's copia specimen end to end (2026-08-11): ontology -> generated
    /// shapes -> template hints. Before, BOTH ends dropped type information —
    /// the shapes emitted nothing for a bounded datatype (#99) and the hint
    /// called every literal "str" (#100). This walks the whole path so the two
    /// fixes are pinned together: if either regresses, the hint goes wrong.
    const KIT_TTL: &str = r#"
@prefix owl:  <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .
@prefix t:    <https://repolex.ai/ontology/t/> .

t:LookScoreValue a rdfs:Datatype ;
    owl:onDatatype xsd:integer ;
    owl:withRestrictions ( [ xsd:minInclusive 1 ] [ xsd:maxInclusive 5 ] ) .

t:Look a owl:Class .

t:lookTechnicalScore a owl:DatatypeProperty ;
    rdfs:domain t:Look ;
    rdfs:range t:LookScoreValue .

t:lookAnatomyReject a owl:DatatypeProperty ;
    rdfs:domain t:Look ;
    rdfs:range xsd:boolean .

t:lookNote a owl:DatatypeProperty ;
    rdfs:domain t:Look ;
    rdfs:range xsd:string .

t:lookTakenOn a owl:DatatypeProperty ;
    rdfs:domain t:Look ;
    rdfs:range xsd:date .
"#;

    fn hints_for(ttl: &str) -> HashMap<String, String> {
        let store = crate::kit::load_ttl_str(ttl, "test").expect("ttl loads");
        let shapes =
            generate_shapes_from_store(&store, "t", "https://repolex.ai/ontology/t/", "test")
                .expect("shapes generate");
        parse_shacl_hints(&shapes, "t")
    }

    #[test]
    fn boolean_is_taught_as_bool_not_str() {
        let h = hints_for(KIT_TTL);
        let hint = h.get("t:lookAnatomyReject").expect("boolean prop has a hint");
        assert!(hint.contains("bool"), "boolean taught as `{hint}` — a hard gate must not read as text");
        assert!(!hint.contains("str"), "boolean still says str: {hint}");
    }

    #[test]
    fn bounded_integer_is_taught_with_its_range() {
        let h = hints_for(KIT_TTL);
        let hint = h.get("t:lookTechnicalScore").expect("score prop has a hint");
        assert!(hint.contains("int"), "score not taught as int: {hint}");
        assert!(hint.contains("1-5"), "score omits its declared bound: {hint}");
    }

    #[test]
    fn date_keeps_its_own_word() {
        let h = hints_for(KIT_TTL);
        let hint = h.get("t:lookTakenOn").expect("date prop has a hint");
        assert!(hint.contains("date"), "date taught as `{hint}`");
    }

    /// A genuine string must still say str — the fix is about accuracy, not
    /// about never saying "str".
    #[test]
    fn genuine_string_still_says_str() {
        let h = hints_for(KIT_TTL);
        let hint = h.get("t:lookNote").expect("string prop has a hint");
        assert!(hint.contains("str"), "real string lost its word: {hint}");
    }

    #[test]
    fn unknown_datatype_is_named_not_called_str() {
        assert_eq!(
            friendly_datatype("http://www.w3.org/2001/XMLSchema#gYear"),
            Some("gYear".to_string()),
            "an unfamiliar xsd type should name itself rather than pose as a string"
        );
    }
}

#[cfg(test)]
mod qualified_value_shape_support_tests {
    use rudof_rdf::rdf_core::RDFFormat;
    use rudof_rdf::rdf_impl::{InMemoryGraph, ReaderMode};
    use sparql_service::RdfData;
    use shacl_rdf::ShaclParser;
    use shacl_ir::compiled::schema_ir::SchemaIR as ShaclSchemaIR;
    use shacl_validation::shacl_processor::{GraphValidation, ShaclProcessor, ShaclValidationMode};
    use shacl_validation::store::Graph;

    const SHAPES: &str = r#"
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.org/> .
ex:SceneShape a sh:NodeShape ;
    sh:targetClass ex:Scene ;
    sh:property [ sh:path ex:relatedToId ; sh:minCount 1 ;
                  sh:qualifiedValueShape [ sh:class ex:Place ] ; sh:qualifiedMinCount 1 ] ;
    sh:property [ sh:path ex:relatedToId ;
                  sh:qualifiedValueShape [ sh:class ex:Being ] ; sh:qualifiedMinCount 1 ] .
"#;

    fn validate(data: &str) -> Result<usize, String> {
        let sg = InMemoryGraph::from_reader(&mut SHAPES.as_bytes(), "s", &RDFFormat::Turtle, None, &ReaderMode::Lax)
            .map_err(|e| format!("shapes parse: {e}"))?;
        let sr = RdfData::from_graph(sg).map_err(|e| format!("shapes load: {e}"))?;
        let schema = ShaclParser::new(sr).parse().map_err(|e| format!("shacl parse: {e}"))?;
        let compiled = ShaclSchemaIR::compile(&schema).map_err(|e| format!("compile: {e}"))?;
        let dg = InMemoryGraph::from_reader(&mut data.as_bytes(), "d", &RDFFormat::Turtle, None, &ReaderMode::Strict)
            .map_err(|e| format!("data parse: {e}"))?;
        let dr = RdfData::from_graph(dg).map_err(|e| format!("data load: {e}"))?;
        let store = Graph::from_data(dr);
        let mut p = GraphValidation::from_graph(store, ShaclValidationMode::Native);
        let report = p.validate(&compiled).map_err(|e| format!("validate: {e}"))?;
        Ok(report.results().len())
    }

    const PATTERN_SHAPES: &str = r#"
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.org/> .
ex:SceneShape a sh:NodeShape ;
    sh:targetClass ex:Scene ;
    sh:property [ sh:path ex:relatedToId ; sh:minCount 1 ] ;
    sh:property [ sh:path ex:relatedToId ;
                  sh:qualifiedValueShape [ sh:pattern "/Place/" ] ; sh:qualifiedMinCount 1 ] ;
    sh:property [ sh:path ex:relatedToId ;
                  sh:qualifiedValueShape [ sh:pattern "/Being/" ] ; sh:qualifiedMinCount 1 ] .
"#;

    fn validate_with(shapes: &str, data: &str) -> Result<usize, String> {
        let sg = InMemoryGraph::from_reader(&mut shapes.as_bytes(), "s", &RDFFormat::Turtle, None, &ReaderMode::Lax)
            .map_err(|e| format!("shapes parse: {e}"))?;
        let sr = RdfData::from_graph(sg).map_err(|e| format!("shapes load: {e}"))?;
        let schema = ShaclParser::new(sr).parse().map_err(|e| format!("shacl parse: {e}"))?;
        let compiled = ShaclSchemaIR::compile(&schema).map_err(|e| format!("compile: {e}"))?;
        let dg = InMemoryGraph::from_reader(&mut data.as_bytes(), "d", &RDFFormat::Turtle, None, &ReaderMode::Strict)
            .map_err(|e| format!("data parse: {e}"))?;
        let dr = RdfData::from_graph(dg).map_err(|e| format!("data load: {e}"))?;
        let store = Graph::from_data(dr);
        let mut p = GraphValidation::from_graph(store, ShaclValidationMode::Native);
        let report = p.validate(&compiled).map_err(|e| format!("validate: {e}"))?;
        Ok(report.results().len())
    }

    /// @tr1p's Q(B): sh:pattern INSIDE sh:qualifiedValueShape. Legal SHACL, but
    /// he asked me to finish the way I started — by probing, not assuming. This
    /// is the form the spec actually ships, so it is the one that must work.
    #[test]
    fn probe_pattern_inside_qualified_value_shape() {
        // IRIs carry the class in the path (Rob's naming law, 2026-07-16).
        let ok = r#"
@prefix ex: <http://example.org/> .
ex:s1 a ex:Scene ; ex:relatedToId <https://repolex.ai/copia/Place/greenhouse>,
                                  <https://repolex.ai/copia/Being/selkie> .
"#;
        let no_being = r#"
@prefix ex: <http://example.org/> .
ex:s2 a ex:Scene ; ex:relatedToId <https://repolex.ai/copia/Place/greenhouse> .
"#;
        assert_eq!(validate_with(PATTERN_SHAPES, ok), Ok(0),
            "sh:pattern inside sh:qualifiedValueShape IS supported — this is the form \
             @tr1p's spec ships, so it had to be probed rather than assumed");
        assert_eq!(validate_with(PATTERN_SHAPES, no_being), Ok(1),
            "a Scene with a Place and no Being must violate exactly the Being shape");

        // NOTE the dependency this rests on, which the shape itself cannot show:
        // it reads the CLASS OUT OF THE IRI PATH, sound only because instance
        // IRIs are <namespace/Class/id> by the naming law Rob ruled 2026-07-16.
        // No target node is resolved. If that law softens, this check silently
        // weakens and nothing here will say so. (@tr1p's words, kept.)
    }

    /// tr1p's Q1: does the stack we SHIP support sh:qualifiedValueShape at all?
    /// Everything else is theory until this passes.
    #[test]
    fn probe_qualified_value_shape_capability() {
        // Satisfied: one Place, one Being, both typed IN THIS GRAPH.
        let ok = r#"
@prefix ex: <http://example.org/> .
ex:s1 a ex:Scene ; ex:relatedToId ex:p1, ex:b1 .
ex:p1 a ex:Place . ex:b1 a ex:Being .
"#;
        // Violated: Place present, Being MISSING entirely.
        let bad = r#"
@prefix ex: <http://example.org/> .
ex:s2 a ex:Scene ; ex:relatedToId ex:p1 .
ex:p1 a ex:Place .
"#;
        // tr1p's Q2: the referenced node exists but its TYPE is not in the
        // graph — the cross-repo case. Pass or fail?
        let unresolvable = r#"
@prefix ex: <http://example.org/> .
ex:s3 a ex:Scene ; ex:relatedToId ex:elsewhere .
"#;
        assert_eq!(validate(ok), Ok(0),
            "the shipped SHACL stack DOES support sh:qualifiedValueShape + \
             sh:qualifiedMinCount — two property shapes on the same sh:path, each \
             constraining a different qualified subset (@tr1p's design)");
        assert_eq!(validate(bad), Ok(1),
            "a Scene with a Place but no Being must violate exactly the Being shape");

        // THE ANSWER THAT DECIDES THE DESIGN. When the referenced node's TYPE
        // is absent from the graph being validated, sh:class does not quietly
        // pass — it FAILS BOTH qualified constraints, because neither a Place
        // nor a Being can be found. Not a silent gate; a blocking one.
        //
        // Combined with the fact that `cmd_validate` builds one graph PER
        // DOCUMENT, this means the referenced Thing is never present at save
        // time — not even for a same-repo reference. So sh:class can never
        // resolve at the save gate as it is built today, and every Scene would
        // fail every save. That is why the IRI-pattern fallback is not merely
        // the cross-repo option; it is the only one that can work here now.
        assert_eq!(validate(unresolvable), Ok(2),
            "an unresolvable target FAILS both qualified shapes — loud, not silent");
    }
}
