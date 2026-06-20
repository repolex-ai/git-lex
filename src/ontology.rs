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
/// Resolved by canonical path, NOT by glob-walk. The two canonical
/// install locations are:
///   - `.lex/ontology/{short}/{short}-shapes.ttl`  (static kit)
///   - `_ontology/{short}/{short}-shapes.ttl`      (adaptive kit)
///
/// Previous behavior glob-walked `all_shape_files()` and picked the FIRST
/// file matching by name. That was first-wins-by-sort-order, which made
/// stale fossils invisible to ls but visible to the loader — a 2-month-old
/// `.lex/ontology/kit/{short}/{short}-shapes.ttl` (from a pre-multi-kit
/// layout) sorted alphabetically before `.lex/ontology/{short}/...` and
/// shadowed the current shapes. See task #29 (TR1P.L3X repro Day 22).
///
/// Now: try the static path; if missing, try adaptive. Anywhere else is
/// ignored — `kit-update` sweeps the legacy `.lex/ontology/kit/` directory.
fn read_kit_shapes(kit: &str) -> String {
    let Some(root) = find_git_root() else { return String::new() };
    let (_, _, short) = resolve_kit_spec(kit);
    let target = format!("{}-shapes.ttl", short);

    // Static kit: .lex/ontology/{short}/{short}-shapes.ttl
    let static_path = root.join(".lex").join("ontology").join(&short).join(&target);
    let static_content = fs::read_to_string(&static_path).ok();

    // Adaptive kit: _ontology/{short}/{short}-shapes.ttl
    let adaptive_path = root.join("_ontology").join(&short).join(&target);
    let adaptive_content = if static_content.is_none() {
        fs::read_to_string(&adaptive_path).ok()
    } else { None };

    // Audit: surface ANY extra `{short}-shapes.ttl` found outside the two
    // canonical paths. Catches old-layout stragglers like
    // `.lex/ontology/kit/{short}/{short}-shapes.ttl` that were silently
    // shadowing canonical shapes prior to this resolver. Warning only —
    // we ignore them either way. Once-per-process per kit-short so a
    // file-per-call caller (e.g. cmd_validate) doesn't spam.
    use std::sync::Mutex;
    use std::sync::OnceLock;
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let warned = WARNED.get_or_init(|| Mutex::new(HashSet::new()));
    let mut should_warn = false;
    {
        let mut lock = warned.lock().unwrap();
        if !lock.contains(&short) {
            lock.insert(short.clone());
            should_warn = true;
        }
    }
    if should_warn {
        let mut stragglers: Vec<PathBuf> = Vec::new();
        for path in all_shape_files() {
            if path.file_name().and_then(|n| n.to_str()) != Some(target.as_str()) { continue; }
            if path == static_path || path == adaptive_path { continue; }
            stragglers.push(path);
        }
        if !stragglers.is_empty() {
            eprintln!("warning: stale '{}' found outside the canonical install location(s):", target);
            for p in &stragglers {
                let rel = p.strip_prefix(&root).unwrap_or(p);
                eprintln!("  {}", rel.display());
            }
            eprintln!("  These are ignored; `git lex kit-update` will sweep the legacy `.lex/ontology/kit/` location.");
        }
    }

    static_content.or(adaptive_content).unwrap_or_default()
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

/// Read the `lex-o:instantiation` annotation for a single class out of
/// the kit's source ontology TTL. Returns one of `"authored"`,
/// `"graph-only"`, or `"abstract"`, defaulting to `"authored"` when the
/// annotation is absent (which preserves pre-annotation backward
/// compatibility — older kits without instantiation annotations behave
/// as if every class were authored, the historical default).
///
/// Reads from the source `.ttl` (e.g. `.lex/ontology/copia/copia.ttl`),
/// NOT the derived `-shapes.ttl`. The instantiation annotation lives in
/// OWL; SHACL shapes don't carry it.
///
/// Parser is intentionally string-level (not a full Turtle parse). It
/// scans for the class declaration line — either `kit:ClassName a owl:Class`
/// or just `kit:ClassName a` followed by a class-y token — and looks for
/// `lex-o:instantiation "<value>"` inside the same stanza (until the next
/// statement-terminating `.`). This matches the conventional shape used
/// across kit-copia, kit-pool, etc.
pub(crate) fn get_class_instantiation(kit: &str, class_name: &str) -> String {
    let default = "authored".to_string();
    let Some(root) = find_git_root() else { return default };
    let (_, _, short) = resolve_kit_spec(kit);
    let target = format!("{}.ttl", short);

    // Static kit: .lex/ontology/{short}/{short}.ttl
    let static_path = root.join(".lex").join("ontology").join(&short).join(&target);
    // Adaptive kit: _ontology/{short}/{short}.ttl
    let adaptive_path = root.join("_ontology").join(&short).join(&target);

    let content = fs::read_to_string(&static_path)
        .or_else(|_| fs::read_to_string(&adaptive_path))
        .unwrap_or_default();
    if content.is_empty() {
        return default;
    }
    parse_class_instantiation(&content, &short, class_name)
}

/// Look up the OKF type label for a class — used at `git lex create` time to
/// emit the `type:` field at the top of the YAML frontmatter (per OKF spec
/// v0.1; the only REQUIRED frontmatter field for OKF compliance).
///
/// Three-fallback chain (per the OKF spec, locked by tr1p 2026-06-18):
///   1. `lex-o:okfType "<value>"` annotation on the class — kit author's
///      explicit choice.
///   2. `rdfs:label "<value>"` — soft-launch path so unannotated kits still
///      emit a sensible `type:` value from their existing labels.
///   3. Local-name of the class — final fallback; always exists.
///
/// Returns a string in every case (the bottom of the chain is the local-name
/// itself, which the caller passes in). Never panics.
pub(crate) fn get_class_okf_type(kit: &str, class_name: &str) -> String {
    let Some(root) = find_git_root() else { return class_name.to_string() };
    let (_, _, short) = resolve_kit_spec(kit);
    let target = format!("{}.ttl", short);

    // Static kit: .lex/ontology/{short}/{short}.ttl
    let static_path = root.join(".lex").join("ontology").join(&short).join(&target);
    // Adaptive kit: _ontology/{short}/{short}.ttl
    let adaptive_path = root.join("_ontology").join(&short).join(&target);

    let content = fs::read_to_string(&static_path)
        .or_else(|_| fs::read_to_string(&adaptive_path))
        .unwrap_or_default();
    if content.is_empty() {
        return class_name.to_string();
    }
    parse_class_okf_type(&content, &short, class_name)
}

/// Pure parser for the `lex-o:okfType` lookup with full three-fallback chain.
/// Separated from filesystem I/O so it can be unit-tested directly.
///
/// Walks the class's stanza looking for `lex-o:okfType "..."` first; if not
/// present, falls back to the stanza's `rdfs:label "..."`; if neither is
/// present (or the class isn't declared in this file at all), returns the
/// class's local-name unchanged.
fn parse_class_okf_type(content: &str, short: &str, class_name: &str) -> String {
    let kit_prefix = find_kit_prefix(content, short);
    let class_qname = format!("{}:{}", kit_prefix, class_name);

    let mut in_stanza = false;
    let mut label: Option<String> = None;
    for line in content.lines() {
        let t = line.trim();
        if !in_stanza {
            if let Some(rest) = t.strip_prefix(&class_qname) {
                let rest = rest.trim_start();
                if rest.starts_with("a ") || rest == "a" {
                    in_stanza = true;
                }
            }
            continue;
        }
        // okfType wins.
        if let Some(idx) = t.find("lex-o:okfType") {
            let after = &t[idx + "lex-o:okfType".len()..];
            let after = after.trim_start();
            if let Some(rest) = after.strip_prefix('"') {
                if let Some(end) = rest.find('"') {
                    return rest[..end].to_string();
                }
            }
        }
        // Capture the label for the fallback — but keep scanning in case
        // okfType appears later in the stanza.
        if label.is_none() {
            if let Some(idx) = t.find("rdfs:label") {
                let after = &t[idx + "rdfs:label".len()..];
                let after = after.trim_start();
                if let Some(rest) = after.strip_prefix('"') {
                    if let Some(end) = rest.find('"') {
                        label = Some(rest[..end].to_string());
                    }
                }
            }
        }
        // Stanza terminator — a `.` at end of trimmed line, not preceded
        // by `;` (predicate-list continuation).
        if t.ends_with('.') && !t.ends_with(" ;") {
            break;
        }
    }

    label.unwrap_or_else(|| class_name.to_string())
}

/// Shared kit-prefix detector — finds the `@prefix <name>: <ns>` line whose
/// namespace ends in `/{short}/` (or `/kit/{short}/`). Falls back to the
/// short name itself if no `@prefix` line matches.
fn find_kit_prefix(content: &str, short: &str) -> String {
    for line in content.lines() {
        let t = line.trim();
        if !t.starts_with("@prefix") { continue; }
        if let Some(rest) = t.strip_prefix("@prefix") {
            let rest = rest.trim();
            if let Some((name_part, ns_part)) = rest.split_once(':') {
                let name = name_part.trim().to_string();
                let ns_part = ns_part.trim();
                if ns_part.contains(&format!("/{}/", short))
                    || ns_part.contains(&format!("/kit/{}/", short))
                {
                    return name;
                }
            }
        }
    }
    short.to_string()
}

/// Pure parser for the `lex-o:instantiation` annotation lookup.
/// Separated from filesystem I/O so it can be unit-tested directly.
fn parse_class_instantiation(content: &str, short: &str, class_name: &str) -> String {
    let default = "authored".to_string();
    let kit_prefix = find_kit_prefix(content, short);
    let class_qname = format!("{}:{}", kit_prefix, class_name);

    // Walk lines looking for the class declaration's stanza. Stanza
    // terminator is a line whose trimmed end is `.` (Turtle's
    // statement terminator). Within the stanza, look for
    // `lex-o:instantiation "<value>"`.
    let mut in_stanza = false;
    for line in content.lines() {
        let t = line.trim();
        if !in_stanza {
            // Stanza start: line begins with the class qname followed by
            // whitespace + `a` (the rdf:type abbreviation).
            if let Some(rest) = t.strip_prefix(&class_qname) {
                let rest = rest.trim_start();
                if rest.starts_with("a ") || rest == "a" {
                    in_stanza = true;
                }
            }
            continue;
        }
        // In-stanza: look for the annotation.
        if let Some(idx) = t.find("lex-o:instantiation") {
            let after = &t[idx + "lex-o:instantiation".len()..];
            let after = after.trim_start();
            if let Some(rest) = after.strip_prefix('"') {
                if let Some(end) = rest.find('"') {
                    return rest[..end].to_string();
                }
            }
        }
        // Stanza terminator — a `.` at end of trimmed line, not preceded
        // by `;` (which is the predicate-list continuation marker).
        if t.ends_with('.') && !t.ends_with(" ;") {
            return default;
        }
    }

    default
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIT_COPIA_SAMPLE: &str = r#"
@prefix copia: <https://repolex.ai/ontology/kit/copia/> .
@prefix lex-o: <https://repolex.ai/ontology/lex-o/> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

copia:Place a owl:Class ;
    rdfs:label "Place" ;
    rdfs:comment "A canon location." .

copia:Moment a owl:Class ;
    lex-o:instantiation "graph-only" ;
    rdfs:label "Moment" ;
    rdfs:comment "Graph-only — never authored as a file." .

copia:Depictable a owl:Class ;
    lex-o:instantiation "abstract" ;
    rdfs:label "Depictable" ;
    rdfs:comment "Abstract — never instantiated directly." .
"#;

    #[test]
    fn default_when_annotation_absent() {
        let v = parse_class_instantiation(KIT_COPIA_SAMPLE, "copia", "Place");
        assert_eq!(v, "authored");
    }

    #[test]
    fn reads_graph_only() {
        let v = parse_class_instantiation(KIT_COPIA_SAMPLE, "copia", "Moment");
        assert_eq!(v, "graph-only");
    }

    #[test]
    fn reads_abstract() {
        let v = parse_class_instantiation(KIT_COPIA_SAMPLE, "copia", "Depictable");
        assert_eq!(v, "abstract");
    }

    #[test]
    fn default_when_class_missing() {
        let v = parse_class_instantiation(KIT_COPIA_SAMPLE, "copia", "Nonexistent");
        assert_eq!(v, "authored");
    }

    #[test]
    fn default_when_content_empty() {
        let v = parse_class_instantiation("", "copia", "Moment");
        assert_eq!(v, "authored");
    }

    #[test]
    fn does_not_leak_annotation_across_stanzas() {
        // Place comes first with NO annotation; Moment comes after with
        // graph-only. Place must NOT pick up Moment's annotation.
        let v = parse_class_instantiation(KIT_COPIA_SAMPLE, "copia", "Place");
        assert_eq!(v, "authored");
    }

    #[test]
    fn handles_kit_prefix_via_kit_path() {
        // Some namespaces use /kit/{short}/ (the actual convention for
        // copia). Make sure the prefix-detect handles that form too.
        let ttl = r#"
@prefix soul: <https://repolex.ai/ontology/kit/soul/> .
@prefix lex-o: <https://repolex.ai/ontology/lex-o/> .

soul:Self a owl:Class ;
    lex-o:instantiation "graph-only" ;
    rdfs:label "Self" .
"#;
        let v = parse_class_instantiation(ttl, "soul", "Self");
        assert_eq!(v, "graph-only");
    }

    // ── OKF type lookup — three-fallback chain ──

    const KIT_WITH_OKF_TYPES: &str = r#"
@prefix copia: <https://repolex.ai/ontology/kit/copia/> .
@prefix lex-o: <https://repolex.ai/ontology/lex-o/> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

# Explicit annotation — wins
copia:Place a owl:Class ;
    lex-o:okfType "Place" ;
    rdfs:label "Canon Location" ;
    rdfs:comment "A canon location." .

# No annotation — falls back to rdfs:label
copia:Outfit a owl:Class ;
    rdfs:label "Outfit Item" ;
    rdfs:comment "What someone is wearing." .

# Neither annotation nor label — falls back to local-name
copia:Bag a owl:Class ;
    rdfs:comment "A grouping." .

# Annotation with a multi-word value (the NocturneActivity case)
copia:NocturneActivity a owl:Class ;
    lex-o:okfType "Nocturne Activity" ;
    rdfs:label "Nocturne Activity" .
"#;

    #[test]
    fn okf_explicit_annotation_wins_over_label() {
        let v = parse_class_okf_type(KIT_WITH_OKF_TYPES, "copia", "Place");
        // Annotation says "Place"; rdfs:label says "Canon Location".
        // Annotation wins per the three-fallback chain.
        assert_eq!(v, "Place");
    }

    #[test]
    fn okf_falls_back_to_rdfs_label_when_no_annotation() {
        let v = parse_class_okf_type(KIT_WITH_OKF_TYPES, "copia", "Outfit");
        assert_eq!(v, "Outfit Item");
    }

    #[test]
    fn okf_falls_back_to_local_name_when_neither() {
        let v = parse_class_okf_type(KIT_WITH_OKF_TYPES, "copia", "Bag");
        assert_eq!(v, "Bag");
    }

    #[test]
    fn okf_falls_back_to_local_name_when_class_missing() {
        let v = parse_class_okf_type(KIT_WITH_OKF_TYPES, "copia", "Nonexistent");
        assert_eq!(v, "Nonexistent");
    }

    #[test]
    fn okf_falls_back_to_local_name_when_content_empty() {
        let v = parse_class_okf_type("", "copia", "Memory");
        assert_eq!(v, "Memory");
    }

    #[test]
    fn okf_multiword_annotation_preserved() {
        // The NocturneActivity case from the spec — multi-word OKF type
        // labels are valid (open vocabulary).
        let v = parse_class_okf_type(KIT_WITH_OKF_TYPES, "copia", "NocturneActivity");
        assert_eq!(v, "Nocturne Activity");
    }

    #[test]
    fn okf_does_not_leak_annotation_across_stanzas() {
        // Place has okfType. Outfit (next stanza, no okfType) must NOT
        // pick up Place's annotation — must fall back to its own label.
        let v = parse_class_okf_type(KIT_WITH_OKF_TYPES, "copia", "Outfit");
        assert_eq!(v, "Outfit Item");
    }

    #[test]
    fn okf_does_not_leak_label_across_stanzas() {
        // Outfit has rdfs:label. Bag (next stanza, no label) must NOT pick
        // up Outfit's label — must fall back to local-name.
        let v = parse_class_okf_type(KIT_WITH_OKF_TYPES, "copia", "Bag");
        assert_eq!(v, "Bag");
    }

    #[test]
    fn okf_parses_real_lex_o_seed_ontology() {
        // Receipt check that our parser handles the actual lex-o.ttl
        // shape. lex-o doesn't declare okfType on its own classes (it's the
        // ontology that DEFINES the annotation property), so this exercises
        // the rdfs:label fallback.
        let path = std::path::PathBuf::from("/Users/rob/repos/repolex-ai/lex-o-seed/lex-o.ttl");
        let Ok(content) = fs::read_to_string(&path) else {
            // CI / detached test runs may not have this path. Skip cleanly.
            return;
        };
        let v = parse_class_okf_type(&content, "lex-o", "Agent");
        assert_eq!(v, "Agent", "lex-o:Agent has rdfs:label \"Agent\"");
    }

    #[test]
    fn okf_parses_real_kit_soul_with_annotations() {
        // Receipt check against the live kit-soul ontology that just had
        // okfType annotations added across all 17 classes. Verifies the
        // annotation wins over the label (they happen to match for Memory,
        // but the parser logic is exercised regardless).
        let path = std::path::PathBuf::from("/Users/rob/repos/repolex-ai/git-lex-kit-soul/ontology/soul/soul.ttl");
        let Ok(content) = fs::read_to_string(&path) else { return };
        assert_eq!(parse_class_okf_type(&content, "soul", "Memory"), "Memory");
        assert_eq!(parse_class_okf_type(&content, "soul", "Decision"), "Decision");
        assert_eq!(parse_class_okf_type(&content, "soul", "Note"), "Note");
        assert_eq!(parse_class_okf_type(&content, "soul", "Journal"), "Journal");
    }

    #[test]
    fn okf_parses_real_kit_copia_with_multiword_annotation() {
        // Receipt for the NocturneActivity case — multi-word OKF type
        // labels survive the round-trip through the real ontology file.
        let path = std::path::PathBuf::from("/Users/rob/repos/repolex-ai/git-lex-kit-copia/ontology/copia/copia.ttl");
        let Ok(content) = fs::read_to_string(&path) else { return };
        assert_eq!(parse_class_okf_type(&content, "copia", "Place"), "Place");
        assert_eq!(parse_class_okf_type(&content, "copia", "NocturneActivity"), "Nocturne Activity");
        assert_eq!(parse_class_okf_type(&content, "copia", "NocturneFeed"), "Nocturne Feed");
    }

    #[test]
    fn okf_parses_real_kit_pool_with_annotations() {
        let path = std::path::PathBuf::from("/Users/rob/repos/repolex-ai/git-lex-kit-pool/ontology/pool/pool.ttl");
        let Ok(content) = fs::read_to_string(&path) else { return };
        assert_eq!(parse_class_okf_type(&content, "pool", "Image"), "Image");
        assert_eq!(parse_class_okf_type(&content, "pool", "Document"), "Document");
    }
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
