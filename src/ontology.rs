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

use oxigraph::model::Term;

use git_lex::{find_git_root, resolve_kit_spec};

// ─── Shape file discovery ────────────────────────────────────

/// Locate the shapes TTL for a kit. Returns empty string if not found.
///
/// Resolved by canonical path, NOT by glob-walk. The canonical install
/// location is `.lex/ontology/{short}/{short}-shapes.ttl`.
///
/// Previous behavior glob-walked `all_shape_files()` and picked the FIRST
/// file matching by name. That was first-wins-by-sort-order, which made
/// stale fossils invisible to ls but visible to the loader — a 2-month-old
/// `.lex/ontology/kit/{short}/{short}-shapes.ttl` (from a pre-multi-kit
/// layout) sorted alphabetically before `.lex/ontology/{short}/...` and
/// shadowed the current shapes. See task #29 (TR1P.L3X repro Day 22).
///
/// Now: only the canonical path is read. Anywhere else is ignored —
/// `kit-update` sweeps the legacy `.lex/ontology/kit/` directory.
fn read_kit_shapes(kit: &str) -> String {
    let Some(root) = find_git_root() else { return String::new() };
    let (_, _, short) = resolve_kit_spec(kit);
    let target = format!("{}-shapes.ttl", short);

    let canonical_path = root.join(".lex").join("ontology").join(&short).join(&target);
    let canonical_content = fs::read_to_string(&canonical_path).ok();

    // Audit: surface ANY extra `{short}-shapes.ttl` found outside the
    // canonical path. Catches old-layout stragglers like
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
            if path == canonical_path { continue; }
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

    canonical_content.unwrap_or_default()
}

/// Return paths to every shape TTL installed in the repo
/// (`.lex/ontology/**/*-shapes.ttl`). Used by whole-repo listings.
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

/// Local name of an IRI relative to a kit namespace: strip the namespace
/// when it matches, otherwise fall back to the last path segment (the same
/// local-name rule shape generation uses in `shacl.rs`).
fn local_name_in(namespace: &str, iri: &str) -> String {
    iri.strip_prefix(namespace)
        .unwrap_or_else(|| iri.rsplit('/').next().unwrap_or(iri))
        .to_string()
}

/// Parse a SHACL shapes TTL into `ShapeFile` — a real Turtle parse (in-memory
/// oxigraph store) queried with SPARQL, the ONE Turtle-reading policy.
///
/// Ordering: properties come out ORDER BY path IRI, which is exactly the
/// order shape generation writes them (`shacl.rs` orders property blocks by
/// prop IRI), so template prop order is unchanged. Classes come out ORDER BY
/// class IRI (alphabetical); the old line scanner returned file order, which
/// was itself arbitrary store-iteration order at generation time.
fn parse_shape_file(content: &str, short_hint: &str) -> ShapeFile {
    let mut out = ShapeFile::default();

    // Kit prefix + namespace come from the file's own declaration — matched
    // by prefix NAME via the single shared scanner (git_lex::extract_kit_prefix),
    // so a kit's namespace can migrate with a one-line TTL edit and this
    // parser follows. Conventional fallback only when nothing declares.
    // (SPARQL cannot see @prefix declarations — the scanner stays.)
    match git_lex::extract_kit_prefix(content, short_hint) {
        Some((name, ns)) => {
            out.prefix_name = name;
            out.namespace = ns;
        }
        None => {
            out.prefix_name = short_hint.to_string();
            out.namespace = git_lex::conventional_kit_namespace(short_hint);
        }
    }

    let store = match crate::kit::load_ttl_str(content, &format!("{} shapes", short_hint)) {
        Ok(s) => s,
        Err(e) => {
            // Loud, not silent: a shapes file that doesn't parse means no
            // runtime type info for the kit — say so instead of limping.
            eprintln!("warning: {} — kit '{}' shapes ignored", e, short_hint);
            return out;
        }
    };

    // One row per (shape, property block). OPTIONAL keeps property-less
    // shapes (e.g. copia:Pose) as classes with zero props.
    let q = "PREFIX sh: <http://www.w3.org/ns/shacl#>
             PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
             SELECT ?class ?prop ?path ?nodeKind ?datatype ?minCount ?comment WHERE {
                 ?shape sh:targetClass ?class .
                 OPTIONAL {
                     ?shape sh:property ?prop .
                     ?prop sh:path ?path .
                     OPTIONAL { ?prop sh:nodeKind ?nodeKind }
                     OPTIONAL { ?prop sh:datatype ?datatype }
                     OPTIONAL { ?prop sh:minCount ?minCount }
                     OPTIONAL { ?prop rdfs:comment ?comment }
                 }
             } ORDER BY ?class ?path";
    let Ok(oxigraph::sparql::QueryResults::Solutions(sols)) = git_lex::eval_query(&store, q)
    else { return out };

    const XSD_NS: &str = "http://www.w3.org/2001/XMLSchema#";
    const SH_IRI: &str = "http://www.w3.org/ns/shacl#IRI";
    // Track the previous row's property node so a block with several values
    // for one field (several comments, say) updates one ShapeProp instead of
    // duplicating it.
    let mut last_prop_key: Option<(String, String)> = None;
    for s in sols.flatten() {
        let class_iri = match s.get("class") {
            Some(Term::NamedNode(n)) => n.as_str().to_string(),
            _ => continue,
        };
        let class_name = local_name_in(&out.namespace, &class_iri);
        if out.shapes.last().map(|sh: &ParsedShape| sh.class_name != class_name).unwrap_or(true) {
            out.shapes.push(ParsedShape { class_name, props: Vec::new() });
            last_prop_key = None;
        }
        let shape = out.shapes.last_mut().unwrap();

        let Some(Term::NamedNode(path)) = s.get("path") else { continue };
        let prop_key = (class_iri, s.get("prop").map(|t| t.to_string()).unwrap_or_default());
        if last_prop_key.as_ref() != Some(&prop_key) {
            shape.props.push(ShapeProp {
                name: local_name_in(&out.namespace, path.as_str()),
                is_iri: false,
                datatype: None,
                required: false,
                comment: String::new(),
            });
            last_prop_key = Some(prop_key);
        }
        let prop = shape.props.last_mut().unwrap();

        if let Some(Term::NamedNode(nk)) = s.get("nodeKind") {
            if nk.as_str() == SH_IRI { prop.is_iri = true; }
        }
        if let Some(Term::NamedNode(dt)) = s.get("datatype") {
            // xsd:integer → "integer"; non-XSD datatypes stay untyped,
            // matching the old scanner.
            if let Some(local) = dt.as_str().strip_prefix(XSD_NS) {
                prop.datatype = Some(local.to_string());
            }
        }
        if let Some(Term::Literal(n)) = s.get("minCount") {
            if n.value().parse::<u32>().map(|n| n >= 1).unwrap_or(false) {
                prop.required = true;
            }
        }
        if let Some(Term::Literal(c)) = s.get("comment") {
            prop.comment = c.value().to_string();
        }
    }
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

/// The set of EVERY property declared by any installed kit's shapes,
/// keyed "{kit}/{Class}/{prop}" — the same key shape as
/// `get_property_datatypes_all_kits`, WITHOUT the sh:datatype condition.
///
/// This is the index the undeclared-key warning must consult. The datatype
/// map cannot serve that job: the shapes generator deliberately omits
/// `sh:datatype` for xsd:string properties, so every declared string
/// property (soulId, journalId, emojimood — most of every kit) was
/// invisible to a `prop_datatypes` membership test and false-warned as
/// "not declared" on every save (412 warnings in one W4R3Z run,
/// found 2026-08-01).
pub(crate) fn get_declared_properties_all_kits() -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
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
                out.insert(format!("{}/{}/{}", short, shape.class_name, p.name));
            }
        }
    }
    out
}

/// Map of short kit name → declared namespace, for EVERY installed kit.
/// This is what the emitters consult so predicate/class IRIs follow each
/// kit's own `@prefix` declaration (namespace migrations = TTL edit only).
/// Kits with no readable declaration fall back to the conventional pattern.
pub(crate) fn get_kit_namespaces_all_kits() -> HashMap<String, String> {
    let mut out = HashMap::new();
    for path in all_shape_files() {
        let Ok(content) = fs::read_to_string(&path) else { continue };
        let short = path.file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix("-shapes"))
            .unwrap_or("")
            .to_string();
        if short.is_empty() { continue; }
        let (_, ns) = git_lex::extract_kit_prefix(&content, &short)
            .unwrap_or_else(|| (short.clone(), git_lex::conventional_kit_namespace(&short)));
        out.insert(short, ns);
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
pub(crate) fn resolve_class_segment(
    kit: &str,
    class_seg: &str,
    context: &str,
    warn: bool,
) -> Result<String, String> {
    let classes: Vec<String> = get_kit_types(kit).into_iter().map(|(name, _)| name).collect();
    match resolve_class_against(&classes, class_seg) {
        ClassMatch::Exact(name) | ClassMatch::PassThrough(name) => Ok(name),
        ClassMatch::CaseOnly { canonical, given } => {
            // Recover to the canonical name, but warn loudly so the author
            // corrects the frontmatter (and so this never silently masks a
            // future real typo that happens to differ only in case).
            // `warn: false` = the history walk, which revisits every commit:
            // the save path already taught this once, at the moment the
            // author could act on it (#73 — replay must not repeat live
            // to-dos). Recovery behavior is identical either way.
            if warn {
                eprintln!(
                    "warning: {context}: the key prefix `{kit}.{given}.` has the wrong \
                     capitalization. Fix: edit it to `{kit}.{canonical}.` exactly \
                     (capitalization matters). Auto-corrected for this run only."
                );
            }
            Ok(canonical)
        }
        ClassMatch::NoMatch => Err(format!(
            "`{class_seg}` is not a class in kit `{kit}` (declared classes: {}). \
             Fix, pick one: (a) this document really is one of the declared classes \
             — edit its keys to use that class name; (b) its class belongs to a kit \
             that is not installed in this repo — leave the file as-is and report it \
             to the kit owner. Until fixed, this document's facts are skipped, \
             not lost.",
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

    let path = root.join(".lex").join("ontology").join(&short).join(&target);
    let content = fs::read_to_string(&path).unwrap_or_default();
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

    let path = root.join(".lex").join("ontology").join(&short).join(&target);
    let content = fs::read_to_string(&path).unwrap_or_default();
    if content.is_empty() {
        return class_name.to_string();
    }
    parse_class_type_label(&content, &short, class_name)
}

/// Pure parser for the class type-label lookup (feeds `git lex create`'s
/// top-of-frontmatter `type:` field). Separated from filesystem I/O so it
/// can be unit-tested directly.
///
/// Two-fallback chain: the class's `rdfs:label "..."`; if absent (or the
/// class isn't declared in this file at all), the class's local-name
/// unchanged. (Formerly a three-step chain headed by `lex-o:okfType` — OKF
/// was adopted speculatively and retired with lex-o, Rob's ruling; the
/// label is the correct value for every class.)
///
/// Real Turtle parse + SPARQL, not a stanza scan. The class IRI is derived
/// from the file's own kit `@prefix` declaration (the ONE shared scanner) —
/// never a hardcoded namespace pattern.
fn parse_class_type_label(content: &str, short: &str, class_name: &str) -> String {
    let class_iri = format!("{}{}", kit_namespace_of(content, short), class_name);
    let store = match crate::kit::load_ttl_str(content, &format!("{} ontology", short)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: {} — falling back to class local-name", e);
            return class_name.to_string();
        }
    };
    let q = format!(
        "SELECT ?label WHERE {{ <{}> <http://www.w3.org/2000/01/rdf-schema#label> ?label }}
         ORDER BY ?label LIMIT 1",
        class_iri
    );
    if let Ok(oxigraph::sparql::QueryResults::Solutions(sols)) = git_lex::eval_query(&store, &q) {
        for s in sols.flatten() {
            if let Some(Term::Literal(l)) = s.get("label") {
                return l.value().to_string();
            }
        }
    }
    class_name.to_string()
}

/// Kit namespace declared in TTL content, via the ONE shared `@prefix`
/// scanner; conventional pattern only when nothing declares.
fn kit_namespace_of(content: &str, short: &str) -> String {
    git_lex::extract_kit_prefix(content, short)
        .map(|(_name, ns)| ns)
        .unwrap_or_else(|| git_lex::conventional_kit_namespace(short))
}

/// Law-6 reference ranges: `"{kit}/{prop}"` → the range CLASS IRI, for
/// every `owl:ObjectProperty` with a non-XSD `rdfs:range` in every
/// installed kit ontology TTL. This is what turns a declared reference
/// (copia:lookBeingId, range copia:Being) into id→IRI resolution at
/// emission: the authored value is the TARGET'S id; the emitter derives
/// `<range-app>/<RangeClass>/<id>`. Property-level (ranges live on the
/// property, not the shape) — the emitter pairs it with the class-
/// qualified obj_props membership test it already does.
pub(crate) fn get_reference_ranges_all_kits() -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(root) = find_git_root() else { return out };
    let ont_root = root.join(".lex").join("ontology");
    let Ok(entries) = fs::read_dir(&ont_root) else { return out };
    for e in entries.filter_map(|e| e.ok()) {
        let dir = e.path();
        if !dir.is_dir() { continue }
        let Some(short) = dir.file_name().and_then(|n| n.to_str()).map(String::from) else { continue };
        let ttl = dir.join(format!("{}.ttl", short));
        let Ok(content) = fs::read_to_string(&ttl) else { continue };
        for (prop, range) in parse_reference_ranges(&content, &short) {
            out.insert(format!("{}/{}", short, prop), range);
        }
    }
    out
}

/// `"{kit}/{prop}"` → optional replacement (dcterms:isReplacedBy) for every
/// property any installed kit declares with `owl:deprecated true`. The
/// deprecated-key note at save consults this: a retired key EXISTS in the
/// ontology (deprecate-never-delete keeps history replayable — the 0.9.0
/// Friend incident), so telling the author it "does not exist" is a lie;
/// it gets the deprecation teaching instead. Replacement values in the
/// kit's own namespace are shortened to the local name.
pub(crate) fn get_deprecated_properties_all_kits() -> HashMap<String, Option<String>> {
    let mut out = HashMap::new();
    let Some(root) = find_git_root() else { return out };
    let ont_root = root.join(".lex").join("ontology");
    let Ok(entries) = fs::read_dir(&ont_root) else { return out };
    for e in entries.filter_map(|e| e.ok()) {
        let dir = e.path();
        if !dir.is_dir() { continue }
        let Some(short) = dir.file_name().and_then(|n| n.to_str()).map(String::from) else { continue };
        let ttl = dir.join(format!("{}.ttl", short));
        let Ok(content) = fs::read_to_string(&ttl) else { continue };
        for (prop, replaced) in parse_deprecated_properties(&content, &short) {
            out.insert(format!("{}/{}", short, prop), replaced);
        }
    }
    out
}

/// Pure parser for `owl:deprecated true` properties in one kit TTL.
/// Returns `(property_local_name, Option<replacement>)` pairs; properties
/// outside the kit's own namespace are skipped.
fn parse_deprecated_properties(content: &str, short: &str) -> Vec<(String, Option<String>)> {
    let kit_ns = kit_namespace_of(content, short);
    let store = match crate::kit::load_ttl_str(content, &format!("{} ontology", short)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: {} — deprecated properties unreadable", e);
            return Vec::new();
        }
    };
    let q = "SELECT ?p ?r WHERE { \
             ?p <http://www.w3.org/2002/07/owl#deprecated> true . \
             OPTIONAL { ?p <http://purl.org/dc/terms/isReplacedBy> ?r } }";
    let mut out = Vec::new();
    if let Ok(oxigraph::sparql::QueryResults::Solutions(sols)) = git_lex::eval_query(&store, q) {
        for s in sols.flatten() {
            let Some(Term::NamedNode(p)) = s.get("p") else { continue };
            let Some(prop) = p.as_str().strip_prefix(kit_ns.as_str()) else { continue };
            if prop.is_empty() {
                continue;
            }
            let replaced = match s.get("r") {
                Some(Term::NamedNode(r)) => Some(
                    r.as_str()
                        .strip_prefix(kit_ns.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| r.as_str().to_string()),
                ),
                Some(Term::Literal(l)) => Some(l.value().to_string()),
                _ => None,
            };
            out.push((prop.to_string(), replaced));
        }
    }
    out
}

/// Class local-names this kit declares with `owl:deprecated true`. The
/// folder audit consults this (#74): a deprecated class keeps resolving —
/// that's what lets its history replay — but it must NOT demand a folder;
/// creating one would invite new writing into retired vocabulary. Fleet
/// receipt 2026-08-08: after the soul 0.9.x deprecation appendix, every
/// repo's kit-update printed phantom missing-folder lines for the
/// deprecated classes.
pub(crate) fn get_deprecated_classes(kit: &str) -> std::collections::HashSet<String> {
    let Some(root) = find_git_root() else { return Default::default() };
    let (_, _, short) = resolve_kit_spec(kit);
    let path = root
        .join(".lex")
        .join("ontology")
        .join(&short)
        .join(format!("{}.ttl", short));
    let Ok(content) = fs::read_to_string(&path) else { return Default::default() };
    parse_deprecated_classes(&content, &short)
}

/// Pure parser for `owl:deprecated true` classes in one kit TTL. Classes
/// outside the kit's own namespace are skipped.
fn parse_deprecated_classes(content: &str, short: &str) -> std::collections::HashSet<String> {
    let kit_ns = kit_namespace_of(content, short);
    let store = match crate::kit::load_ttl_str(content, &format!("{} ontology", short)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: {} — deprecated classes unreadable", e);
            return Default::default();
        }
    };
    let q = "SELECT ?c WHERE { \
             ?c <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://www.w3.org/2002/07/owl#Class> . \
             ?c <http://www.w3.org/2002/07/owl#deprecated> true }";
    let mut out = std::collections::HashSet::new();
    if let Ok(oxigraph::sparql::QueryResults::Solutions(sols)) = git_lex::eval_query(&store, q) {
        for s in sols.flatten() {
            let Some(Term::NamedNode(c)) = s.get("c") else { continue };
            if let Some(name) = c.as_str().strip_prefix(kit_ns.as_str()) {
                if !name.is_empty() {
                    out.insert(name.to_string());
                }
            }
        }
    }
    out
}

/// Pure parser for object-property ranges in one kit TTL. Returns
/// `(property_local_name, range_class_iri)` pairs; XSD ranges and
/// properties outside the kit's own namespace are skipped.
fn parse_reference_ranges(content: &str, short: &str) -> Vec<(String, String)> {
    let kit_ns = kit_namespace_of(content, short);
    let store = match crate::kit::load_ttl_str(content, &format!("{} ontology", short)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: {} — reference ranges unreadable", e);
            return Vec::new();
        }
    };
    let q = "SELECT ?p ?r WHERE { \
             ?p <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> \
                <http://www.w3.org/2002/07/owl#ObjectProperty> ; \
                <http://www.w3.org/2000/01/rdf-schema#range> ?r }";
    let mut out = Vec::new();
    if let Ok(oxigraph::sparql::QueryResults::Solutions(sols)) = git_lex::eval_query(&store, q) {
        for s in sols.flatten() {
            let (Some(Term::NamedNode(p)), Some(Term::NamedNode(r))) = (s.get("p"), s.get("r")) else { continue };
            let Some(prop) = p.as_str().strip_prefix(kit_ns.as_str()) else { continue };
            if prop.is_empty() || r.as_str().starts_with("http://www.w3.org/2001/XMLSchema#") {
                continue;
            }
            out.push((prop.to_string(), r.as_str().to_string()));
        }
    }
    out
}

/// Pure parser for the `git-lex:foldered` flag lookup.
/// Separated from filesystem I/O so it can be unit-tested directly.
///
/// Returns true ONLY for an explicit `git-lex:foldered true` (Turtle boolean
/// literal) on the class. Anything else — absent flag, `false`, missing
/// class, empty/unparseable file — is false (opt-in).
///
/// The `git-lex:` namespace is resolved from the file's own declaration
/// (name-exact via the ONE shared scanner); the conventional base namespace
/// is the fallback when the file doesn't declare it.
fn parse_class_foldered(content: &str, short: &str, class_name: &str) -> bool {
    let class_iri = format!("{}{}", kit_namespace_of(content, short), class_name);
    // extract_kit_prefix's primary rule is name-exact, so short="git-lex"
    // finds `@prefix git-lex:`; its fallback rule could hand back the KIT's
    // prefix, so only trust a name-exact hit.
    let gitlex_ns = match git_lex::extract_kit_prefix(content, "git-lex") {
        Some((name, ns)) if name == "git-lex" => ns,
        _ => git_lex::conventional_kit_namespace("git-lex"),
    };
    let store = match crate::kit::load_ttl_str(content, &format!("{} ontology", short)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: {} — treating classes as graph-only (no folder)", e);
            return false;
        }
    };
    let q = format!("ASK {{ <{}> <{}foldered> true }}", class_iri, gitlex_ns);
    matches!(
        git_lex::eval_query(&store, &q),
        Ok(oxigraph::sparql::QueryResults::Boolean(true))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Law-6 range parsing, pinned to tr1p's staged copia FK flip
    /// (repolex-ai/copia train/re-anchor c4b325f): ObjectProperties with a
    /// kit-class range parse; XSD ranges and other kits' properties skip.
    #[test]
    fn parse_reference_ranges_pins_staged_copia_shapes() {
        let ttl = r#"
@prefix copia: <https://repolex.ai/ontology/copia/> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

copia: a owl:Ontology .
copia:lookBeingId a owl:ObjectProperty ; rdfs:range copia:Being .
copia:lookMomentId a owl:ObjectProperty ; rdfs:range copia:Moment .
copia:firstVisited a owl:DatatypeProperty ; rdfs:range xsd:date .
"#;
        let mut pairs = parse_reference_ranges(ttl, "copia");
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                ("lookBeingId".to_string(), "https://repolex.ai/ontology/copia/Being".to_string()),
                ("lookMomentId".to_string(), "https://repolex.ai/ontology/copia/Moment".to_string()),
            ]
        );
    }

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
        // (owl/rdfs prefixes declared — the fixture predates the real
        // Turtle parse, which rightly rejects undeclared prefixes.)
        let ttl = r#"
@prefix soul: <https://repolex.ai/ontology/kit/soul/> .
@prefix git-lex: <https://repolex.ai/ontology/git-lex/> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

soul:Memory a owl:Class ;
    git-lex:foldered true ;
    rdfs:label "Memory" .
"#;
        assert!(parse_class_foldered(ttl, "soul", "Memory"));
    }

    // ── type-label lookup — label → local-name chain ──

    // (Fixture typo fixed with the SPARQL port: rdfs was declared as
    // `https://www.w3.org/...` — not the real rdfs namespace. The old
    // token-level scanner matched `rdfs:label` by prefix NAME and never
    // noticed; a real RDF parse resolves IRIs, so the standard namespace
    // is required — which is what every real kit TTL declares.)
    const KIT_WITH_LABELS: &str = r#"
@prefix copia: <https://repolex.ai/ontology/kit/copia/> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

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
        // Decision deprecated at soul 0.9.0 (isReplacedBy soul:Note) — the
        // label honestly says so; deprecate-never-delete keeps the stanza.
        assert_eq!(
            parse_class_type_label(&content, "soul", "Decision"),
            "Decision (deprecated)"
        );
        assert_eq!(parse_class_type_label(&content, "soul", "Note"), "Note");
        assert_eq!(parse_class_type_label(&content, "soul", "Journal"), "Journal");
    }

    #[test]
    fn deprecated_classes_parse_real_kit_soul() {
        // Receipt check against the live kit-soul ontology: the 0.9.x
        // appendix re-declared retired classes with owl:deprecated true
        // (deprecate-never-delete). The folder audit (#74) keys off this —
        // deprecated classes must not demand folders.
        let path = std::path::PathBuf::from("/Users/rob/repos/repolex-ai/git-lex-kit-soul/ontology/soul/soul.ttl");
        let Ok(content) = fs::read_to_string(&path) else { return };
        let dep = parse_deprecated_classes(&content, "soul");
        assert!(dep.contains("Decision"), "Decision deprecated at 0.9.0: {dep:?}");
        assert!(dep.contains("Friend"), "Friend deprecated at 0.9.1: {dep:?}");
        assert!(!dep.contains("Note"), "Note is live vocabulary: {dep:?}");
        assert!(!dep.contains("Journal"), "Journal is live vocabulary: {dep:?}");
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
/// Each entry: (prefix_name, class_name, namespace).
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
