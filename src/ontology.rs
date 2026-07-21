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
    let (_, _, prefix) = resolve_kit_spec(kit); // the SHORT kit name — the segment frontmatter keys carry
    let mut out = HashSet::new();
    for shape in &parsed.shapes {
        for p in &shape.props {
            if p.is_iri {
                // Kit+class-qualified key (Rob-ruled 2026-07-21):
                // "{kit}/{Class}/{prop}". Each kit's and class's OWN
                // declaration governs its values — a bare-name pool let one
                // kit's declaration silently rewrite another's (soul:source
                // prose was comma-split as copia:source lineage edges).
                out.insert(format!("{}/{}/{}", prefix, shape.class_name, p.name));
            }
        }
    }
    out
}

/// Map of property local-name → full XSD datatype IRI, for typed literals.
/// Only non-string datatypes are included.
pub(crate) fn get_property_datatypes(kit: &str) -> HashMap<String, String> {
    let parsed = parse_kit_shapes(kit);
    let (_, _, prefix) = resolve_kit_spec(kit); // the SHORT kit name — the segment frontmatter keys carry
    let mut out = HashMap::new();
    for shape in &parsed.shapes {
        for p in &shape.props {
            if let Some(dt) = &p.datatype {
                let full = format!("http://www.w3.org/2001/XMLSchema#{}", dt);
                // Kit+class-qualified key — see get_object_properties.
                out.insert(format!("{}/{}/{}", prefix, shape.class_name, p.name), full);
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
/// Keys are kit+class-qualified — "{kit}/{Class}/{prop}" (Rob-ruled
/// 2026-07-21) — so same-named properties in different kits/classes can
/// never collide. (The old bare-name pool was last-writer-wins; the
/// collision it "documented before introducing" arrived with copia:source
/// v0.15 and silently rewrote soul:source's behavior.)
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
        let prefix = short.clone(); // the SHORT kit name — the segment frontmatter keys carry
        for shape in &parsed.shapes {
            for p in &shape.props {
                if let Some(dt) = &p.datatype {
                    let full = format!("http://www.w3.org/2001/XMLSchema#{}", dt);
                    out.insert(format!("{}/{}/{}", prefix, shape.class_name, p.name), full);
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
        let prefix = short.clone(); // the SHORT kit name — the segment frontmatter keys carry
        for shape in &parsed.shapes {
            for p in &shape.props {
                if p.is_iri {
                    out.insert(format!("{}/{}/{}", prefix, shape.class_name, p.name));
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

/// Resolve a frontmatter class segment against the kit's declared classes.
///
/// This is the single validation point that closes the B1 casing footgun
/// (Day 38): two type-emitters (`nquad.rs` and `extraction.rs`) used to
/// disagree on case — one passed the segment through verbatim, the other
/// capitalized the first letter as a *guess* (`cameraangle` → `Cameraangle`,
/// not the real `CameraAngle`). A lowercase `soul.memory.*` frontmatter thus
/// emitted a phantom `soul:memory` type and the canonical query
/// `?m a soul:Memory` silently returned zero rows. Both emitters now call
/// THIS, so there is one rule, anchored to the ontology the code already
/// parses (`get_kit_types`).
///
/// Returns:
/// - `Ok(canonical)` on an exact, case-correct hit (the common path).
/// - `Ok(canonical)` on a case-ONLY mismatch, after emitting a warning to
///   stderr — we recover to the real class name rather than emit a phantom
///   type, but we tell the author so they fix the frontmatter.
/// - `Err(message)` when the segment matches no class in the kit (a real
///   typo, not just casing) — fail loud, per the soft-release bar. The
///   message lists the kit's known classes so the fix is obvious.
///
/// When the kit declares no classes at all (`get_kit_types` empty — e.g. a
/// kit with only properties, or shapes not yet generated), validation is
/// skipped and the segment passes through unchanged, preserving prior
/// behavior for kits this check can't speak to.
pub(crate) fn resolve_class_segment(kit: &str, class_seg: &str) -> Result<String, String> {
    let classes: Vec<String> = get_kit_types(kit).into_iter().map(|(name, _)| name).collect();
    match resolve_class_against(&classes, class_seg) {
        ClassMatch::Exact(name) | ClassMatch::PassThrough(name) => Ok(name),
        ClassMatch::CaseOnly { canonical, given } => {
            // Recover to the canonical name, but warn loudly so the author
            // corrects the frontmatter (and so this never silently masks a
            // future real typo that happens to differ only in case).
            eprintln!(
                "warning: frontmatter class segment `{kit}.{given}.…` does not match \
                 the ontology class casing; using canonical `{kit}.{canonical}.…`. \
                 Fix the frontmatter prefix to `{canonical}` (class names are \
                 case-sensitive in the graph)."
            );
            Ok(canonical)
        }
        ClassMatch::NoMatch => Err(format!(
            "frontmatter class segment `{class_seg}` is not a class in kit `{kit}`. \
             Known classes: {}. (Class names are case-sensitive; check the prefix.)",
            classes.join(", ")
        )),
    }
}

/// The pure decision behind `resolve_class_segment`, split out so the casing
/// rule is unit-testable without touching disk (B1 regression, Day 38).
#[derive(Debug, PartialEq)]
enum ClassMatch {
    /// Exact, case-correct hit — the common, healthy path.
    Exact(String),
    /// Kit declares no classes (shapes absent / property-only kit); we can't
    /// validate, so pass the segment through unchanged (prior behavior).
    PassThrough(String),
    /// Case-only mismatch — recover to `canonical`, warn about `given`.
    CaseOnly { canonical: String, given: String },
    /// No class matches even case-insensitively — a real typo.
    NoMatch,
}

fn resolve_class_against(classes: &[String], class_seg: &str) -> ClassMatch {
    if classes.is_empty() {
        return ClassMatch::PassThrough(class_seg.to_string());
    }
    if classes.iter().any(|c| c == class_seg) {
        return ClassMatch::Exact(class_seg.to_string());
    }
    if let Some(canonical) = classes.iter().find(|c| c.eq_ignore_ascii_case(class_seg)) {
        return ClassMatch::CaseOnly {
            canonical: canonical.clone(),
            given: class_seg.to_string(),
        };
    }
    ClassMatch::NoMatch
}

/// Read the `git-lex:foldered` flag for a single class out of the kit's
/// source ontology TTL. **Opt-IN**: a class gets a scaffolded folder +
/// `__ClassName.md` template ONLY when tagged `git-lex:foldered true`.
/// Absent (or false) = graph-only, no folder — the quiet default never
/// litters empty folders for vocabulary-only classes (Rob's ruling; the
/// old lex-o:instantiation opt-out list under-covered copia by 10 classes).
///
/// Reads from the source `.ttl` (e.g. `.lex/ontology/copia/copia.ttl`),
/// NOT the derived `-shapes.ttl` — the flag is authored OWL-side.
///
/// Parser is intentionally string-level (not a full Turtle parse), same
/// stanza-scan shape as the type-label lookup below.
pub(crate) fn get_class_foldered(kit: &str, class_name: &str) -> bool {
    let Some(root) = find_git_root() else { return false };
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
        return false;
    }
    parse_class_foldered(&content, &short, class_name)
}

/// Look up the display type label for a class — used at `git lex create` time to
/// emit the `type:` field at the top of the YAML frontmatter.
///
/// Two-fallback chain: `rdfs:label` → local-name of the class. Returns a
/// string in every case; never panics. (The lex-o:okfType head of the old
/// chain retired with lex-o — Rob's ruling; labels are correct everywhere.)
pub(crate) fn get_class_type_label(kit: &str, class_name: &str) -> String {
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
    parse_class_type_label(&content, &short, class_name)
}

/// Pure parser for the class type-label lookup (feeds `git lex create`'s
/// top-of-frontmatter `type:` field). Separated from filesystem I/O so it
/// can be unit-tested directly.
///
/// Two-fallback chain: the stanza's `rdfs:label "..."`; if absent (or the
/// class isn't declared in this file at all), the class's local-name
/// unchanged. (Formerly a three-step chain headed by `lex-o:okfType` — OKF
/// was adopted speculatively and retired with lex-o, Rob's ruling; the
/// label is the correct value for every class.)
fn parse_class_type_label(content: &str, short: &str, class_name: &str) -> String {
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

/// Pure parser for the `git-lex:foldered` flag lookup.
/// Separated from filesystem I/O so it can be unit-tested directly.
///
/// Returns true ONLY for an explicit `git-lex:foldered true` (bare Turtle
/// boolean literal) inside the class's stanza. Anything else — absent flag,
/// `false`, missing class, empty file — is false (opt-in).
fn parse_class_foldered(content: &str, short: &str, class_name: &str) -> bool {
    let kit_prefix = find_kit_prefix(content, short);
    let class_qname = format!("{}:{}", kit_prefix, class_name);

    // Walk lines looking for the class declaration's stanza. Stanza
    // terminator is a line whose trimmed end is `.` (Turtle's
    // statement terminator). Within the stanza, look for
    // `git-lex:foldered true`.
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
        // In-stanza: look for the flag. Turtle booleans are bare tokens
        // (`true`/`false`), not quoted strings.
        if let Some(idx) = t.find("git-lex:foldered") {
            let after = t[idx + "git-lex:foldered".len()..].trim_start();
            return after.starts_with("true");
        }
        // Stanza terminator — a `.` at end of trimmed line, not preceded
        // by `;` (which is the predicate-list continuation marker).
        if t.ends_with('.') && !t.ends_with(" ;") {
            return false;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIT_COPIA_SAMPLE: &str = r#"
@prefix copia: <https://repolex.ai/ontology/kit/copia/> .
@prefix git-lex: <https://repolex.ai/ontology/git-lex/> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

copia:Place a owl:Class ;
    git-lex:foldered true ;
    rdfs:label "Place" ;
    rdfs:comment "Authored — earns a folder." .

copia:Moment a owl:Class ;
    rdfs:label "Moment" ;
    rdfs:comment "Untagged — graph-only, no folder." .

copia:Depictable a owl:Class ;
    git-lex:foldered false ;
    rdfs:label "Depictable" ;
    rdfs:comment "Explicit false — same as untagged." .
"#;

    #[test]
    fn foldered_true_scaffolds() {
        assert!(parse_class_foldered(KIT_COPIA_SAMPLE, "copia", "Place"));
    }

    #[test]
    fn untagged_is_graph_only() {
        // Opt-IN: absent flag means NO folder (the inverted default —
        // forgetting the tag never litters empty folders).
        assert!(!parse_class_foldered(KIT_COPIA_SAMPLE, "copia", "Moment"));
    }

    #[test]
    fn explicit_false_is_graph_only() {
        assert!(!parse_class_foldered(KIT_COPIA_SAMPLE, "copia", "Depictable"));
    }

    #[test]
    fn missing_class_is_graph_only() {
        assert!(!parse_class_foldered(KIT_COPIA_SAMPLE, "copia", "Nonexistent"));
    }

    #[test]
    fn empty_content_is_graph_only() {
        assert!(!parse_class_foldered("", "copia", "Moment"));
    }

    #[test]
    fn does_not_leak_flag_across_stanzas() {
        // Place (foldered true) comes first; Moment follows untagged.
        // Moment must NOT pick up Place's flag.
        assert!(!parse_class_foldered(KIT_COPIA_SAMPLE, "copia", "Moment"));
    }

    #[test]
    fn handles_kit_prefix_via_kit_path() {
        // Some namespaces use /kit/{short}/ (the actual convention for
        // copia). Make sure the prefix-detect handles that form too.
        let ttl = r#"
@prefix soul: <https://repolex.ai/ontology/kit/soul/> .
@prefix git-lex: <https://repolex.ai/ontology/git-lex/> .

soul:Memory a owl:Class ;
    git-lex:foldered true ;
    rdfs:label "Memory" .
"#;
        assert!(parse_class_foldered(ttl, "soul", "Memory"));
    }

    // ── type-label lookup — label → local-name chain ──

    const KIT_WITH_LABELS: &str = r#"
@prefix copia: <https://repolex.ai/ontology/kit/copia/> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <https://www.w3.org/2000/01/rdf-schema#> .

# Label differs from local-name — label wins
copia:Place a owl:Class ;
    rdfs:label "Canon Location" ;
    rdfs:comment "A canon location." .

# Plain label
copia:Outfit a owl:Class ;
    rdfs:label "Outfit Item" ;
    rdfs:comment "What someone is wearing." .

# No label — falls back to local-name
copia:Bag a owl:Class ;
    rdfs:comment "A grouping." .

# Multi-word label (the NocturneActivity case)
copia:NocturneActivity a owl:Class ;
    rdfs:label "Nocturne Activity" .
"#;

    #[test]
    fn type_label_uses_rdfs_label() {
        let v = parse_class_type_label(KIT_WITH_LABELS, "copia", "Place");
        assert_eq!(v, "Canon Location");
    }

    #[test]
    fn type_label_falls_back_to_local_name_when_no_label() {
        let v = parse_class_type_label(KIT_WITH_LABELS, "copia", "Bag");
        assert_eq!(v, "Bag");
    }

    #[test]
    fn type_label_falls_back_to_local_name_when_class_missing() {
        let v = parse_class_type_label(KIT_WITH_LABELS, "copia", "Nonexistent");
        assert_eq!(v, "Nonexistent");
    }

    #[test]
    fn type_label_falls_back_to_local_name_when_content_empty() {
        let v = parse_class_type_label("", "copia", "Memory");
        assert_eq!(v, "Memory");
    }

    #[test]
    fn type_label_multiword_preserved() {
        let v = parse_class_type_label(KIT_WITH_LABELS, "copia", "NocturneActivity");
        assert_eq!(v, "Nocturne Activity");
    }

    #[test]
    fn type_label_does_not_leak_label_across_stanzas() {
        // Outfit has rdfs:label. Bag (next stanza, no label) must NOT pick
        // up Outfit's label — must fall back to local-name.
        let v = parse_class_type_label(KIT_WITH_LABELS, "copia", "Bag");
        assert_eq!(v, "Bag");
    }

    #[test]
    fn type_label_parses_real_kit_soul() {
        // Receipt check against the live kit-soul ontology — labels hold
        // whether or not the retired lex-o annotations are still present
        // (chain is label → local-name; lex-o is invisible to it).
        let path = std::path::PathBuf::from("/Users/rob/repos/repolex-ai/git-lex-kit-soul/ontology/soul/soul.ttl");
        let Ok(content) = fs::read_to_string(&path) else { return };
        assert_eq!(parse_class_type_label(&content, "soul", "Memory"), "Memory");
        assert_eq!(parse_class_type_label(&content, "soul", "Decision"), "Decision");
        assert_eq!(parse_class_type_label(&content, "soul", "Note"), "Note");
        assert_eq!(parse_class_type_label(&content, "soul", "Journal"), "Journal");
    }

    #[test]
    fn type_label_parses_real_kit_copia_multiword() {
        // Receipt for the NocturneActivity case — multi-word labels
        // survive the round-trip through the real ontology file.
        let path = std::path::PathBuf::from("/Users/rob/repos/repolex-ai/git-lex-kit-copia/ontology/copia/copia.ttl");
        let Ok(content) = fs::read_to_string(&path) else { return };
        assert_eq!(parse_class_type_label(&content, "copia", "Place"), "Place");
        assert_eq!(parse_class_type_label(&content, "copia", "NocturneActivity"), "Nocturne Activity");
        assert_eq!(parse_class_type_label(&content, "copia", "NocturneFeed"), "Nocturne Feed");
    }

    #[test]
    fn type_label_parses_real_kit_pool() {
        let path = std::path::PathBuf::from("/Users/rob/repos/repolex-ai/git-lex-kit-pool/ontology/pool/pool.ttl");
        let Ok(content) = fs::read_to_string(&path) else { return };
        assert_eq!(parse_class_type_label(&content, "pool", "Image"), "Image");
        assert_eq!(parse_class_type_label(&content, "pool", "Document"), "Document");
    }

    // B1 regression (Day 38): the class-casing footgun. Two emitters used to
    // disagree on case — one passed through verbatim (`soul.memory` →
    // phantom `soul:memory`), one capitalized-first-letter as a guess
    // (`cameraangle` → `Cameraangle`, not the real `CameraAngle`). Now both
    // call `resolve_class_segment`, whose pure decision is tested here.
    #[test]
    fn class_segment_exact_hit_passes() {
        let classes = vec!["Memory".to_string(), "Journal".to_string()];
        assert_eq!(
            resolve_class_against(&classes, "Memory"),
            ClassMatch::Exact("Memory".to_string())
        );
    }

    #[test]
    fn class_segment_case_only_mismatch_recovers_to_canonical() {
        // THE bug: lowercase `memory` must resolve to canonical `Memory`,
        // not emit a phantom `soul:memory` that `?m a soul:Memory` misses.
        let classes = vec!["Memory".to_string(), "Journal".to_string()];
        assert_eq!(
            resolve_class_against(&classes, "memory"),
            ClassMatch::CaseOnly { canonical: "Memory".to_string(), given: "memory".to_string() }
        );
    }

    #[test]
    fn class_segment_capitalize_guess_would_have_been_wrong() {
        // The OTHER emitter's old guess: capitalize-first-letter turns
        // `cameraangle` into `Cameraangle`, never the real `CameraAngle`.
        // Validation against the class set fixes the casing properly.
        let classes = vec!["CameraAngle".to_string()];
        assert_eq!(
            resolve_class_against(&classes, "cameraangle"),
            ClassMatch::CaseOnly {
                canonical: "CameraAngle".to_string(),
                given: "cameraangle".to_string(),
            }
        );
    }

    #[test]
    fn class_segment_real_typo_is_no_match() {
        let classes = vec!["Memory".to_string(), "Journal".to_string()];
        assert_eq!(resolve_class_against(&classes, "Memmory"), ClassMatch::NoMatch);
    }

    #[test]
    fn class_segment_empty_classes_passes_through() {
        // A kit with no declared classes (property-only / shapes absent):
        // we can't validate, so don't break — pass through unchanged.
        let classes: Vec<String> = vec![];
        assert_eq!(
            resolve_class_against(&classes, "whatever"),
            ClassMatch::PassThrough("whatever".to_string())
        );
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
