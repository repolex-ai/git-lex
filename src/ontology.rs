//! Runtime type information — read from SHACL shapes, not OWL.
//!
//! Shapes are the single source of truth at runtime. OWL TTLs are only
//! consulted at kit-install time and during shape generation (see
//! `src/shacl.rs`). Everything here parses `*-shapes.ttl` files.
//!
//! A shape file contains everything runtime needs:
//!   - `@prefix kit: <ns>`         → prefix name + namespace
//!   - `sh:targetClass kit:X`      → class declarations
//!   - `sh:path kit:p`             → property name (scoped to enclosing class)
//!   - `sh:datatype xsd:date`      → xsd typing
//!   - `sh:nodeKind sh:IRI`        → object property (reference)
//!   - `rdfs:comment "…"`          → property doc text (for templates)
//!   - `sh:minCount 1`             → required
//!   - `sh:class kit:Y`            → object-property range (future; see TODO)

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use git_lex::{find_git_root, resolve_kit_spec};

// ─── Shape file discovery ────────────────────────────────────

/// Locate the shapes TTL for a kit. Returns empty string if not found.
///
/// Reuses `all_shape_files()`'s recursive walk over both `.lex/ontology/` and
/// `_ontology/`, then picks the first file named `{short}-shapes.ttl`. This
/// matches old-layout repos that nest shapes a directory deeper (e.g.
/// `.lex/ontology/kit/{short}/`) without forcing a migration.
fn read_kit_shapes(kit: &str) -> String {
    let (_, _, short) = resolve_kit_spec(kit);
    let target = format!("{}-shapes.ttl", short);
    for path in all_shape_files() {
        if path.file_name().and_then(|n| n.to_str()) == Some(target.as_str()) {
            if let Ok(content) = fs::read_to_string(&path) {
                return content;
            }
        }
    }
    String::new()
}

/// Return paths to every shape TTL installed in the repo, across both
/// `.lex/ontology/**/*-shapes.ttl` (kit-provided) and
/// `_ontology/**/*-shapes.ttl` (agent-authored). Used by whole-repo listings.
pub(crate) fn all_shape_files() -> Vec<PathBuf> {
    let root = match find_git_root() {
        Some(r) => r,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for e in entries.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.ends_with("-shapes.ttl"))
                .unwrap_or(false)
            {
                out.push(p);
            }
        }
    }
    let a = root.join(".lex").join("ontology");
    if a.exists() { walk(&a, &mut out); }
    let b = root.join("_ontology");
    if b.exists() { walk(&b, &mut out); }
    out.sort();
    out
}

// ─── Parsed shape representation ─────────────────────────────

/// One property constraint in a shape.
#[derive(Clone, Debug)]
struct ShapeProp {
    /// Local name (e.g. `confidence`).
    name: String,
    /// True if `sh:nodeKind sh:IRI` — treat as reference/object property.
    is_iri: bool,
    /// XSD local name if `sh:datatype` present (e.g. `integer`, `date`).
    datatype: Option<String>,
    /// True if `sh:minCount >= 1`.
    required: bool,
    /// `rdfs:comment` text, if present.
    comment: String,
}

/// One NodeShape targeting a class.
#[derive(Clone, Debug)]
struct ParsedShape {
    /// Local name of the target class (e.g. `Memory`).
    class_name: String,
    props: Vec<ShapeProp>,
}

/// Everything we can know about a shapes TTL.
#[derive(Clone, Debug, Default)]
struct ShapeFile {
    /// Local prefix name (e.g. `soul`).
    prefix_name: String,
    /// Full namespace IRI (e.g. `https://repolex.ai/ontology/kit/soul/`).
    namespace: String,
    shapes: Vec<ParsedShape>,
}

// ─── Shape parser ────────────────────────────────────────────

/// Parse a SHACL shapes TTL into `ShapeFile`. Line-oriented parser — shapes
/// are auto-generated, so the format is predictable.
fn parse_shape_file(content: &str, short_hint: &str) -> ShapeFile {
    let mut out = ShapeFile::default();

    // Find the kit namespace prefix. Prefer `/kit/{short}/` if we have a
    // hint; otherwise take the first non-standard prefix we encounter.
    let kit_ns_pattern = format!("/kit/{}/", short_hint);
    for line in content.lines() {
        let t = line.trim();
        if !t.starts_with("@prefix ") { continue; }
        // Match either the hinted kit namespace, or any non-boilerplate one.
        let is_kit = t.contains(&kit_ns_pattern);
        let is_boilerplate = t.contains("/shacl#")
            || t.contains("XMLSchema")
            || t.contains("rdf-schema")
            || t.contains("22-rdf-syntax-ns");
        if is_kit || (!is_boilerplate && out.prefix_name.is_empty()) {
            if let Some(colon) = t[8..].find(':') {
                out.prefix_name = t[8..8 + colon].trim().to_string();
            }
            if let Some(start) = t.find('<') {
                if let Some(end) = t.find('>') {
                    out.namespace = t[start + 1..end].to_string();
                }
            }
            if is_kit { break; }
        }
    }
    if out.prefix_name.is_empty() {
        out.prefix_name = short_hint.to_string();
        out.namespace = format!("https://repolex.ai/ontology/kit/{}/", short_hint);
    }

    // Walk shapes. State machine: when we see `sh:targetClass`, start a new
    // shape. Each `sh:path` starts a new property block that accumulates
    // constraints until the next `sh:path` or end-of-shape (`] .`).
    let mut current_shape: Option<ParsedShape> = None;
    let mut current_prop: Option<ShapeProp> = None;
    let prefix_colon = format!("{}:", out.prefix_name);

    let flush_prop = |shape: &mut ParsedShape, prop: &mut Option<ShapeProp>| {
        if let Some(p) = prop.take() {
            shape.props.push(p);
        }
    };
    let flush_shape = |out: &mut ShapeFile, shape: &mut Option<ParsedShape>, prop: &mut Option<ShapeProp>| {
        if let Some(mut s) = shape.take() {
            if let Some(p) = prop.take() { s.props.push(p); }
            out.shapes.push(s);
        }
    };

    for line in content.lines() {
        let t = line.trim();

        // New class target
        if let Some(rest) = t.strip_prefix("sh:targetClass ") {
            // Close prior shape first.
            flush_shape(&mut out, &mut current_shape, &mut current_prop);
            let iri = rest.trim_end_matches(|c: char| c == ' ' || c == ';' || c == '.');
            let class_name = iri
                .strip_prefix(&prefix_colon)
                .unwrap_or(iri)
                .to_string();
            current_shape = Some(ParsedShape {
                class_name,
                props: Vec::new(),
            });
            continue;
        }

        let Some(shape) = current_shape.as_mut() else { continue };

        // New property block inside current shape
        if let Some(rest) = t.strip_prefix("sh:path ") {
            flush_prop(shape, &mut current_prop);
            let iri = rest.trim_end_matches(|c: char| c == ' ' || c == ';' || c == '.');
            let name = iri
                .strip_prefix(&prefix_colon)
                .unwrap_or(iri)
                .to_string();
            current_prop = Some(ShapeProp {
                name,
                is_iri: false,
                datatype: None,
                required: false,
                comment: String::new(),
            });
            continue;
        }

        // Close shape when we hit `.` at end of a blank-surrounded block.
        // SHACL shape blocks end with either `]` followed by `.`, or just `.`.
        if t == "." || t.ends_with("] .") {
            flush_shape(&mut out, &mut current_shape, &mut current_prop);
            continue;
        }

        let Some(prop) = current_prop.as_mut() else { continue };

        if t.starts_with("sh:nodeKind ") && t.contains("sh:IRI") {
            prop.is_iri = true;
        } else if let Some(rest) = t.strip_prefix("sh:datatype ") {
            let dt = rest.trim_end_matches(|c: char| c == ' ' || c == ';' || c == '.');
            // xsd:integer → "integer"
            if let Some(local) = dt.strip_prefix("xsd:") {
                prop.datatype = Some(local.to_string());
            }
        } else if let Some(rest) = t.strip_prefix("sh:minCount ") {
            let n: Option<u32> = rest
                .trim_end_matches(|c: char| c == ' ' || c == ';' || c == '.')
                .parse().ok();
            if n.unwrap_or(0) >= 1 {
                prop.required = true;
            }
        } else if let Some(rest) = t.strip_prefix("rdfs:comment ") {
            // `rdfs:comment "text" ;` — strip quotes.
            if let Some(start) = rest.find('"') {
                if let Some(end) = rest[start + 1..].find('"') {
                    prop.comment = rest[start + 1..start + 1 + end].to_string();
                }
            }
        }
    }
    // Flush tail in case file doesn't end with `.`.
    flush_shape(&mut out, &mut current_shape, &mut current_prop);
    out
}

/// Parse the kit's own shapes file.
fn parse_kit_shapes(kit: &str) -> ShapeFile {
    let content = read_kit_shapes(kit);
    if content.is_empty() { return ShapeFile::default(); }
    let (_, _, short) = resolve_kit_spec(kit);
    parse_shape_file(&content, &short)
}

// ─── Public API (runtime reads) ──────────────────────────────

/// Get the TTL prefix name for a kit, preferring the actual prefix declared
/// in the shapes file. Falls back to a built-in alias, then the short name.
pub(crate) fn get_kit_prefix_name(kit_name: &str) -> String {
    let parsed = parse_kit_shapes(kit_name);
    if !parsed.prefix_name.is_empty() {
        return parsed.prefix_name;
    }
    match kit_name {
        "claude-code" => "cc".to_string(),
        "lex-lab" => "lab".to_string(),
        other => other.to_string(),
    }
}

/// Get the namespace IRI declared for the kit prefix in its shapes file.
pub(crate) fn get_kit_namespace(kit_name: &str) -> String {
    let parsed = parse_kit_shapes(kit_name);
    parsed.namespace
}

/// Property local-names that are object properties (`sh:nodeKind sh:IRI`).
pub(crate) fn get_object_properties(kit: &str) -> HashSet<String> {
    let parsed = parse_kit_shapes(kit);
    let mut out = HashSet::new();
    for shape in &parsed.shapes {
        for p in &shape.props {
            if p.is_iri {
                out.insert(p.name.clone());
            }
        }
    }
    out
}

/// Map of property local-name → full XSD datatype IRI, for typed literals.
/// Only non-string datatypes are included.
pub(crate) fn get_property_datatypes(kit: &str) -> HashMap<String, String> {
    let parsed = parse_kit_shapes(kit);
    let mut out = HashMap::new();
    for shape in &parsed.shapes {
        for p in &shape.props {
            if let Some(dt) = &p.datatype {
                let full = format!("http://www.w3.org/2001/XMLSchema#{}", dt);
                out.insert(p.name.clone(), full);
            }
        }
    }
    out
}

/// Like `get_property_datatypes`, but unions across every installed shapes
/// file (base + domain + optional kits). The extractor uses this when emitting
/// frontmatter triples so a property declared in an optional kit (e.g.
/// `copia:firstVisited` typed `xsd:date`) still gets the typed-literal tag.
///
/// Property-name collisions across kits: last-writer-wins (whichever
/// shapes file `all_shape_files()` returns later in sorted order). Today
/// no collisions exist; document this contract before introducing one.
pub(crate) fn get_property_datatypes_all_kits() -> HashMap<String, String> {
    let mut out = HashMap::new();
    for path in all_shape_files() {
        let Ok(content) = fs::read_to_string(&path) else { continue };
        let short = path.file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix("-shapes"))
            .unwrap_or("")
            .to_string();
        let parsed = parse_shape_file(&content, &short);
        for shape in &parsed.shapes {
            for p in &shape.props {
                if let Some(dt) = &p.datatype {
                    let full = format!("http://www.w3.org/2001/XMLSchema#{}", dt);
                    out.insert(p.name.clone(), full);
                }
            }
        }
    }
    out
}

/// Like `get_object_properties`, but unions across every installed shapes
/// file. Returns the set of property local-names that are `sh:nodeKind sh:IRI`
/// (object properties / references), so the extractor can emit them as IRIs
/// instead of literals.
pub(crate) fn get_object_properties_all_kits() -> HashSet<String> {
    let mut out = HashSet::new();
    for path in all_shape_files() {
        let Ok(content) = fs::read_to_string(&path) else { continue };
        let short = path.file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix("-shapes"))
            .unwrap_or("")
            .to_string();
        let parsed = parse_shape_file(&content, &short);
        for shape in &parsed.shapes {
            for p in &shape.props {
                if p.is_iri {
                    out.insert(p.name.clone());
                }
            }
        }
    }
    out
}

/// Types defined by the kit.
/// Returns `Vec<(ClassName, Vec<(prop_name, prop_kind, required, comment)>)>`
/// where `prop_kind` is `"reference"` for object properties, `"string"` for
/// everything else (consumers only care about reference-vs-other).
pub(crate) fn get_kit_types(kit: &str) -> Vec<(String, Vec<(String, String, bool, String)>)> {
    let parsed = parse_kit_shapes(kit);
    parsed.shapes.into_iter().map(|s| {
        let props = s.props.into_iter().map(|p| {
            let kind = if p.is_iri { "reference".to_string() } else { "string".to_string() };
            (p.name, kind, p.required, p.comment)
        }).collect();
        (s.class_name, props)
    }).collect()
}

/// Every class declared across all installed shape files — kit and
/// adaptive. Each entry: (prefix_name, class_name, namespace).
/// Used by `list` / `create` when they need a whole-repo view, not just one
/// kit.
pub(crate) fn all_classes() -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for path in all_shape_files() {
        let Ok(content) = fs::read_to_string(&path) else { continue };
        // Derive short name from the filename stem (`soul-shapes.ttl` → `soul`).
        let short = path.file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix("-shapes"))
            .unwrap_or("")
            .to_string();
        let parsed = parse_shape_file(&content, &short);
        for shape in parsed.shapes {
            out.push((
                parsed.prefix_name.clone(),
                shape.class_name,
                parsed.namespace.clone(),
            ));
        }
    }
    out
}
