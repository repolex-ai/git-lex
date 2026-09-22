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

/// A kit's vocabulary TTL (`.lex/ontology/{short}/{short}.ttl`), or None when
/// the kit is not installed. Installed means listed in repo.yml
/// (`git_lex::installed_kit_specs`), never "a folder exists".
fn kit_ttl_path(kit: &str) -> Option<PathBuf> {
    let root = find_git_root()?;
    let dir = git_lex::installed_kit_ontology_dir(&root, kit)?;
    let name = dir.file_name()?.to_string_lossy().to_string();
    Some(dir.join(format!("{}.ttl", name)))
}

/// (folder name, vocabulary TTL path) for every installed kit, sorted.
fn installed_kit_ttls() -> Vec<(String, PathBuf)> {
    let Some(root) = find_git_root() else { return Vec::new() };
    git_lex::installed_ontology_dirs(&root)
        .into_iter()
        .map(|(name, dir)| { let ttl = dir.join(format!("{}.ttl", name)); (name, ttl) })
        .collect()
}

/// Read the generated shapes TTL for an installed kit: only the canonical
/// `.lex/ontology/{short}/{short}-shapes.ttl`. Empty string when the kit is
/// not installed or ships no shapes.
pub(crate) fn read_kit_shapes(kit: &str) -> String {
    kit_shapes_path(kit).and_then(|p| fs::read_to_string(p).ok()).unwrap_or_default()
}

/// Every generated shapes TTL of the installed kits. Used by whole-repo
/// listings.
pub(crate) fn all_shape_files() -> Vec<PathBuf> {
    find_git_root().map(|root| git_lex::installed_shape_files(&root)).unwrap_or_default()
}

// ─── Parsed shape representation ─────────────────────────────

/// One property constraint in a shape.
#[derive(Clone, Debug)]
struct ShapeProp {
    /// Local name (e.g. `confidence`).
    name: String,
    /// The FULL property IRI from `sh:path`.
    ///
    /// Not the same as `{kit namespace}{name}` once inheritance exists (#104):
    /// an inherited property keeps its declaring namespace, so
    /// `soul.Note.title` has the local name `title` but the IRI
    /// `https://repolex.ai/ontology/git-lex/title`. The emitter must write the
    /// declared IRI, not one glued together from the document's own kit.
    iri: String,
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
    // `sh:path` is one IRI, or an alternative path over the spellings of ONE
    // property bridged by owl:equivalentProperty (pan:id, subtexture:id,
    // git-lex:id). The generator writes the class's own spelling FIRST in
    // that list, and that is the IRI this class's documents carry — so the
    // head of the list is the property's written IRI here.
    let q = "PREFIX sh: <http://www.w3.org/ns/shacl#>
             PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
             PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
             SELECT ?class ?prop ?path ?nodeKind ?datatype ?minCount ?comment WHERE {
                 ?shape sh:targetClass ?class .
                 OPTIONAL {
                     ?shape sh:property ?prop .
                     ?prop sh:path ?pathNode .
                     OPTIONAL { ?pathNode sh:alternativePath/rdf:first ?head }
                     BIND(COALESCE(?head, ?pathNode) AS ?path)
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
                iri: path.as_str().to_string(),
                is_iri: false,
                datatype: None,
                required: false,
                comment: String::new(),
            });
            last_prop_key = Some(prop_key);
        }
        let prop = shape.props.last_mut().unwrap();

        if let Some(Term::NamedNode(nk)) = s.get("nodeKind")
            && nk.as_str() == SH_IRI { prop.is_iri = true; }
        if let Some(Term::NamedNode(dt)) = s.get("datatype") {
            // xsd:integer → "integer"; non-XSD datatypes stay untyped,
            // matching the old scanner.
            if let Some(local) = dt.as_str().strip_prefix(XSD_NS) {
                prop.datatype = Some(local.to_string());
            }
        }
        if let Some(Term::Literal(n)) = s.get("minCount")
            && n.value().parse::<u32>().map(|n| n >= 1).unwrap_or(false) {
                prop.required = true;
            }
        if let Some(Term::Literal(c)) = s.get("comment") {
            prop.comment = c.value().to_string();
        }
    }
    out
}

/// Parse the kit's own shapes file.
///
/// Memoized (#90). `frontmatter_to_turtle` asks four separate questions of
/// this per FILE — prefix, namespace, object properties, datatypes — and each
/// one used to re-read and re-parse the shapes TTL from disk.
///
/// Fingerprinted on the shapes file's (len, mtime) rather than cached
/// outright: `kit-update` REGENERATES shapes inside a single process and then
/// reads them back, so a plain cache would hand the pre-regeneration shapes to
/// everything downstream. That is the stale-derived-state failure #102 was
/// about, and it is not worth re-introducing to save a stat call.
fn parse_kit_shapes(kit: &str) -> std::sync::Arc<ShapeFile> {
    use std::sync::{Arc, Mutex, OnceLock};
    type Fingerprint = Option<(u64, Option<std::time::SystemTime>)>;
    // Shared, not copied: the emitters ask for a kit's classes once per
    // sidecar line, and cloning every parsed shape on each ask was most of
    // a full walk's time (#15).
    type Memo = HashMap<String, (Fingerprint, Arc<ShapeFile>)>;
    static MEMO: OnceLock<Mutex<Memo>> = OnceLock::new();

    let fingerprint: Fingerprint = kit_shapes_path(kit)
        .and_then(|p| fs::metadata(p).ok())
        .map(|m| (m.len(), m.modified().ok()));

    let memo = MEMO.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((seen, cached)) = memo.lock().unwrap().get(kit)
        && *seen == fingerprint {
            return cached.clone();
        }

    let content = read_kit_shapes(kit);
    let parsed = Arc::new(if content.is_empty() {
        ShapeFile::default()
    } else {
        let (_, _, short) = resolve_kit_spec(kit);
        parse_shape_file(&content, &short)
    });
    memo.lock().unwrap().insert(kit.to_string(), (fingerprint, Arc::clone(&parsed)));
    parsed
}

/// Canonical on-disk location of a kit's generated shapes, or None outside a
/// repo. Split out so the memo above fingerprints exactly the file
/// [`read_kit_shapes`] will open.
fn kit_shapes_path(kit: &str) -> Option<PathBuf> {
    let root = find_git_root()?;
    let dir = git_lex::installed_kit_ontology_dir(&root, kit)?;
    let name = dir.file_name()?.to_string_lossy().to_string();
    Some(dir.join(format!("{}-shapes.ttl", name)))
}

// ─── Public API (runtime reads) ──────────────────────────────

/// Get the TTL prefix name for a kit, preferring the actual prefix declared
/// in the shapes file. Falls back to a built-in alias, then the short name.
pub(crate) fn get_kit_prefix_name(kit_name: &str) -> String {
    let parsed = parse_kit_shapes(kit_name);
    if !parsed.prefix_name.is_empty() {
        return parsed.prefix_name.clone();
    }
    match kit_name {
        "claude-code" => "cc".to_string(),
        "lex-lab" => "lab".to_string(),
        other => other.to_string(),
    }
}

/// Get the namespace IRI declared for the kit prefix in its shapes file.
pub(crate) fn get_kit_namespace(kit_name: &str) -> String {
    parse_kit_shapes(kit_name).namespace.clone()
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

/// `"{kit}/{Class}/{prop}"` → the property's DECLARED IRI, for every installed
/// kit.
///
/// The emitter used to build a predicate by gluing the document's own kit
/// namespace onto the key's property segment. That was correct for exactly as
/// long as every property a class carried was declared by that class's own kit
/// — true until `git-lex:Thing` (#104). After it, the ruled key
/// `soul.Note.title` would have emitted `.../ontology/soul/title` while the
/// property is declared at `.../ontology/git-lex/title`: a fact on an IRI no
/// ontology declares, fleet-wide, breaking the one rule everything else here
/// enforces.
///
/// So the predicate IRI now comes from the SHAPES, which record what the
/// ontology actually declared (including the inheritance walk), instead of
/// from a naming convention re-derived at the call site. Declare once, derive
/// the rest. Keys that no kit declares are absent here and fall back to the
/// old construction, which is what the undeclared-key warning already covers.
pub(crate) fn get_property_iris_all_kits() -> HashMap<String, String> {
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
                out.insert(
                    format!("{}/{}/{}", short, shape.class_name, p.name),
                    p.iri.clone(),
                );
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
    parsed.shapes.iter().map(|s| {
        let props = s.props.iter().map(|p| {
            let kind = if p.is_iri { "reference".to_string() } else { "string".to_string() };
            (p.name.clone(), kind, p.required, p.comment.clone())
        }).collect();
        (s.class_name.clone(), props)
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
/// INFALLIBLE by design. It answers "what class does this line name?", which
/// is a question about the line, not about the ontology. Every caller either
/// reads recorded data (the emitters, the history walk) or is about to write
/// it, and in both cases the class segment is the record. Casing is the one
/// thing worth correcting, because a case slip means the author and the
/// ontology meant the same class.
///
/// Returns:
/// - the segment as given on an exact, case-correct hit (the common path).
/// - the canonical name on a case-ONLY mismatch, warning when `warn` — we
///   recover to the real class name rather than emit a phantom type, but we
///   tell the author so they fix the frontmatter.
/// - the segment as given when the kit declares no such class, warning when
///   `warn`. A typo, an uninstalled kit and a retired class are
///   indistinguishable from here, and only the author can tell them apart —
///   so this teaches and keeps the fact rather than silently dropping it.
///
/// When the kit declares no classes at all (`get_kit_types` empty — e.g. a
/// kit with only properties, or shapes not yet generated), the segment
/// passes through with no warning: there is nothing to compare it to.
pub(crate) fn resolve_class_segment(
    kit: &str,
    class_seg: &str,
    context: &str,
    warn: bool,
) -> String {
    let shapes = parse_kit_shapes(kit);
    let classes: Vec<&str> = shapes.shapes.iter().map(|s| s.class_name.as_str()).collect();
    match resolve_class_against(&classes, class_seg) {
        ClassMatch::Exact(name) | ClassMatch::PassThrough(name) => name,
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
            canonical
        }
        ClassMatch::NoMatch => {
            // The class the line RECORDS is the class the line gets. The
            // installed ontology says what may be written today; it does not
            // get to rewrite what was written before. A class the kit has
            // retired, or one from a kit this repo does not install, still
            // names the type its documents were authored under, so it passes
            // through verbatim and its facts land on the Thing plane like any
            // other. Refusing here is what used to erase a retired class's
            // whole history at rebuild, and what forced kits to keep dead
            // classes declared to buy replay back.
            if warn {
                eprintln!(
                    "warning: {context}: the key prefix `{kit}.{class_seg}.` names a \
                     class the `{kit}` ontology does not declare (declared: {}). The \
                     facts still save and still replay under `{class_seg}`. Fix, pick \
                     one: (a) it is a typo — edit the keys to the class you meant; \
                     (b) the class belongs to a kit this repo does not install — leave \
                     the file alone and install the kit; (c) the kit retired the class \
                     — migrate the document when you next edit it.",
                    classes.join(", ")
                );
            }
            class_seg.to_string()
        }
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

fn resolve_class_against<S: AsRef<str>>(classes: &[S], class_seg: &str) -> ClassMatch {
    if classes.is_empty() {
        return ClassMatch::PassThrough(class_seg.to_string());
    }
    if classes.iter().any(|c| c.as_ref() == class_seg) {
        return ClassMatch::Exact(class_seg.to_string());
    }
    if let Some(canonical) = classes.iter().find(|c| c.as_ref().eq_ignore_ascii_case(class_seg)) {
        return ClassMatch::CaseOnly {
            canonical: canonical.as_ref().to_string(),
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
    let (_, _, short) = resolve_kit_spec(kit);
    let Some(path) = kit_ttl_path(kit) else { return false };
    let content = fs::read_to_string(&path).unwrap_or_default();
    if content.is_empty() {
        return false;
    }
    parse_class_foldered(&content, &short, class_name)
}

/// THE folder-contract predicate (#74; tr1p field-notes §2j): a class gets
/// a folder — and templates emitted into it — iff it is `git-lex:foldered
/// true`. The folder audit, the template emitter, init's folder scaffolding,
/// and the cross-kit folder registry all dispatch through THIS function, so
/// they cannot drift apart. A class a kit has retired is simply gone from the
/// TTL, taking its `foldered` flag with it, so nothing extra is needed to
/// stop rebuilding its folder.
pub(crate) fn class_gets_folder(kit: &str, class_name: &str) -> bool {
    get_class_foldered(kit, class_name)
}

/// Look up the display type label for a class — used at `git lex create` time to
/// emit the `type:` field at the top of the YAML frontmatter.
///
/// Two-fallback chain: `rdfs:label` → local-name of the class. Returns a
/// string in every case; never panics. (The lex-o:okfType head of the old
/// chain retired with lex-o — Rob's ruling; labels are correct everywhere.)
pub(crate) fn get_class_type_label(kit: &str, class_name: &str) -> String {
    let (_, _, short) = resolve_kit_spec(kit);
    let Some(path) = kit_ttl_path(kit) else { return class_name.to_string() };
    let content = fs::read_to_string(&path).unwrap_or_default();
    if content.is_empty() {
        return class_name.to_string();
    }
    parse_class_type_label(&content, &short, class_name)
}

/// Class-level authoring annotations: the class's `rdfs:comment` (what a
/// document of this class IS) and its `git-lex:authoringGuidance` (what
/// belongs in the body — sections and one line each, never a tutorial).
/// Read by `git lex create` for its terminal output and by the template
/// emitter for `__<Class>.md`. NEVER enforced: gates no save, raises no
/// warning, absent from verify — declared law in the property's own
/// rdfs:comment (kit-base 0.10.3), not merely convention here.
pub(crate) struct ClassAuthoring {
    pub comment: Option<String>,
    pub guidance: Option<String>,
}

/// Look up both class-level authoring annotations in one pass over the
/// kit's source ontology TTL — same read path as `get_class_foldered` and
/// `get_class_type_label` above: the authored `.ttl`, never the derived
/// shapes (class annotations don't reach the shapes at all).
pub(crate) fn get_class_authoring(kit: &str, class_name: &str) -> ClassAuthoring {
    let none = ClassAuthoring { comment: None, guidance: None };
    let (_, _, short) = resolve_kit_spec(kit);
    let Some(path) = kit_ttl_path(kit) else { return none };
    let content = fs::read_to_string(&path).unwrap_or_default();
    if content.is_empty() {
        return none;
    }
    parse_class_authoring(&content, &short, class_name)
}

/// Pure parser for the class authoring lookup. Real Turtle parse + SPARQL —
/// guidance is authored as a `"""…"""` literal and may carry `#`, quotes,
/// backticks and markdown links; a stanza scan would mis-terminate on all
/// of them. Both fields None when the class is undeclared, the file is
/// unparseable, or the annotation simply isn't there — absence is the
/// quiet default, exactly like an unfoldered class.
fn parse_class_authoring(content: &str, short: &str, class_name: &str) -> ClassAuthoring {
    let none = ClassAuthoring { comment: None, guidance: None };
    let class_iri = format!("{}{}", kit_namespace_of(content, short), class_name);
    // Name-exact resolution only, same trap as parse_class_foldered:
    // extract_kit_prefix's fallback rule could hand back the KIT's prefix.
    let gitlex_ns = match git_lex::extract_kit_prefix(content, "git-lex") {
        Some((name, ns)) if name == "git-lex" => ns,
        _ => git_lex::conventional_kit_namespace("git-lex"),
    };
    let store = match crate::kit::load_ttl_str(content, &format!("{} ontology", short)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: {} — class description and authoring guidance unavailable", e);
            return none;
        }
    };
    let one_literal = |predicate: String| -> Option<String> {
        let q = format!(
            "SELECT ?v WHERE {{ <{}> <{}> ?v }} ORDER BY ?v LIMIT 1",
            class_iri, predicate
        );
        if let Ok(oxigraph::sparql::QueryResults::Solutions(sols)) = git_lex::eval_query(&store, &q)
        {
            for s in sols.flatten() {
                if let Some(Term::Literal(l)) = s.get("v") {
                    return Some(l.value().to_string());
                }
            }
        }
        None
    };
    ClassAuthoring {
        comment: one_literal("http://www.w3.org/2000/01/rdf-schema#comment".to_string()),
        guidance: one_literal(format!("{}authoringGuidance", gitlex_ns)),
    }
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

/// Law-6 reference ranges: property IRI → the range CLASS IRI, for
/// every `owl:ObjectProperty` with a non-XSD `rdfs:range` in every
/// installed kit ontology TTL. This is what turns a declared reference
/// (copia:lookBeingId, range copia:Being) into id→IRI resolution at
/// emission: the authored value is the TARGET'S id; the emitter derives
/// `<range-app>/<RangeClass>/<id>`. Property-level (ranges live on the
/// property, not the shape) — the emitter pairs it with the class-
/// qualified obj_props membership test it already does.
///
/// Keyed by the property's FULL IRI (2026-08-20): a `{kit}/{prop}` key
/// reconstructed from the AUTHORING side misses inherited properties —
/// `soul.Note.relatedToId` authors under soul while git-lex declares the
/// range. Consumers look up with the declared predicate IRI they already
/// hold. The special range `git-lex:Thing` (see nquad.rs THING_CLASS_IRI)
/// means "any Thing, any class" — the value must be the identifier form
/// `<namespace/Class/id>`, not a bare id (Rob-ruled 2026-08-20).
pub(crate) fn get_reference_ranges_all_kits() -> HashMap<String, String> {
    // Memoized (#90). This reads and regex-parses EVERY installed kit's TTL,
    // and `frontmatter_to_turtle` called it once per file — re-parsing the
    // whole vocabulary set (280KB on a four-kit seat) for every document in
    // the repo. Same fingerprint discipline as parse_kit_shapes: the memo is
    // keyed on every TTL's (path, len, mtime), so any ontology change during
    // the process invalidates it instead of going stale.
    use std::sync::{Mutex, OnceLock};
    type Fingerprint = Vec<(PathBuf, u64, Option<std::time::SystemTime>)>;
    static MEMO: OnceLock<Mutex<Option<(Fingerprint, HashMap<String, String>)>>> = OnceLock::new();

    let mut out = HashMap::new();
    // Collect the TTLs once, then decide whether the parse can be skipped.
    let ttls = installed_kit_ttls();
    let fingerprint: Fingerprint = ttls.iter()
        .map(|(_, p)| {
            let meta = fs::metadata(p).ok();
            (
                p.clone(),
                meta.as_ref().map(|m| m.len()).unwrap_or(0),
                meta.and_then(|m| m.modified().ok()),
            )
        })
        .collect();

    let memo = MEMO.get_or_init(|| Mutex::new(None));
    if let Some((seen, cached)) = memo.lock().unwrap().as_ref()
        && *seen == fingerprint {
            return cached.clone();
        }

    for (short, ttl) in &ttls {
        let Ok(content) = fs::read_to_string(ttl) else { continue };
        for (prop_iri, range) in parse_reference_ranges(&content, short) {
            out.insert(prop_iri, range);
        }
    }
    *memo.lock().unwrap() = Some((fingerprint, out.clone()));
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
    for (short, ttl) in installed_kit_ttls() {
        let Ok(content) = fs::read_to_string(&ttl) else { continue };
        for (prop, replaced) in parse_deprecated_properties(&content, &short) {
            out.insert(format!("{}/{}", short, prop), replaced);
        }
    }
    out
}

/// Shorten a successor IRI (dcterms:isReplacedBy) for teaching output:
/// the kit's own namespace → bare local name; another kit's app-tier
/// namespace (the `conventional_kit_namespace` pattern,
/// `https://repolex.ai/ontology/<kit>/<Name>`) → `kit:Name`; anything
/// else stays a full IRI. A successor that moved kits (soul:Texture →
/// copia:Texture) printed as a raw URL otherwise.
fn shorten_successor(iri: &str, kit_ns: &str) -> String {
    if let Some(local) = iri.strip_prefix(kit_ns)
        && !local.is_empty() {
            return local.to_string();
        }
    if let Some(rest) = iri.strip_prefix("https://repolex.ai/ontology/") {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            return format!("{}:{}", parts[0], parts[1]);
        }
    }
    iri.to_string()
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
                Some(Term::NamedNode(r)) => Some(shorten_successor(r.as_str(), &kit_ns)),
                Some(Term::Literal(l)) => Some(l.value().to_string()),
                _ => None,
            };
            out.push((prop.to_string(), replaced));
        }
    }
    out
}

/// A property the ontology declares with NO `rdfs:domain` — deliberately
/// usable on any class (soul:relatedTo: "Domain left open so any doc-type
/// may relate"). Shapes are generated per class, so a domain-open property
/// can never appear in ANY shape — every shapes-derived table
/// (declared_props, obj_props, prop_datatypes, prop_iris) is blind to it
/// by construction. That blindness is #82: a key the ontology genuinely
/// declares was reported to its author as nonexistent, with an instruction
/// to report it to the kit owner. This record is read straight from the
/// ontology TTL instead, and the emitter consults it wherever the shapes
/// tables miss.
pub(crate) struct DomainOpenProp {
    /// `owl:ObjectProperty` → values resolve as references, not literals.
    pub is_object: bool,
    /// Full XSD datatype IRI when a non-string `rdfs:range` is declared
    /// (same non-string convention as the shapes-derived datatype table).
    pub datatype: Option<String>,
    /// The property's declared IRI — the predicate to emit (#104's rule:
    /// the ontology says where a property lives; ask, don't glue).
    pub iri: String,
}

/// `"{kit}/{prop}"` → [`DomainOpenProp`] for every domain-open property in
/// every installed kit ontology TTL. The key deliberately carries no class:
/// no domain means every class is in scope, so a class-qualified key would
/// re-invent the restriction the ontology chose not to declare.
pub(crate) fn get_domain_open_properties_all_kits() -> HashMap<String, DomainOpenProp> {
    let mut out = HashMap::new();
    for (short, ttl) in installed_kit_ttls() {
        let Ok(content) = fs::read_to_string(&ttl) else { continue };
        for (prop, rec) in parse_domain_open_properties(&content, &short) {
            out.insert(format!("{}/{}", short, prop), rec);
        }
    }
    out
}

/// Pure parser for domain-open properties in one kit TTL: every
/// `owl:ObjectProperty` / `owl:DatatypeProperty` with no `rdfs:domain`.
/// Properties outside the kit's own namespace are skipped. Deprecated
/// properties are INCLUDED — deprecation is a separate axis (the save-time
/// note fires first), and history replay needs their emission unchanged.
fn parse_domain_open_properties(content: &str, short: &str) -> Vec<(String, DomainOpenProp)> {
    let kit_ns = kit_namespace_of(content, short);
    let store = match crate::kit::load_ttl_str(content, &format!("{} ontology", short)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: {} — domain-open properties unreadable", e);
            return Vec::new();
        }
    };
    let q = "SELECT ?p ?t ?r WHERE { \
             ?p <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> ?t . \
             FILTER (?t = <http://www.w3.org/2002/07/owl#ObjectProperty> \
                  || ?t = <http://www.w3.org/2002/07/owl#DatatypeProperty>) \
             FILTER NOT EXISTS { ?p <http://www.w3.org/2000/01/rdf-schema#domain> ?d } \
             OPTIONAL { ?p <http://www.w3.org/2000/01/rdf-schema#range> ?r } }";
    let mut out = Vec::new();
    if let Ok(oxigraph::sparql::QueryResults::Solutions(sols)) = git_lex::eval_query(&store, q) {
        for s in sols.flatten() {
            let (Some(Term::NamedNode(p)), Some(Term::NamedNode(t))) = (s.get("p"), s.get("t")) else { continue };
            let Some(prop) = p.as_str().strip_prefix(kit_ns.as_str()) else { continue };
            if prop.is_empty() {
                continue;
            }
            let is_object = t.as_str() == "http://www.w3.org/2002/07/owl#ObjectProperty";
            let datatype = match s.get("r") {
                Some(Term::NamedNode(r))
                    if r.as_str().starts_with("http://www.w3.org/2001/XMLSchema#")
                        && r.as_str() != "http://www.w3.org/2001/XMLSchema#string" =>
                {
                    Some(r.as_str().to_string())
                }
                _ => None,
            };
            out.push((
                prop.to_string(),
                DomainOpenProp { is_object, datatype, iri: p.as_str().to_string() },
            ));
        }
    }
    out
}

/// Pure parser for object-property ranges in one kit TTL. Returns
/// `(property_IRI, range_class_iri)` pairs; XSD ranges and properties
/// outside the kit's own namespace are skipped.
///
/// Keyed by the property's FULL IRI (2026-08-20, the range=Thing build):
/// the old `(local_name, …)` shape forced consumers to reconstruct a
/// `{kit}/{prop}` key from the AUTHORING kit — which misses every
/// INHERITED property (`soul.Note.relatedToId` authors under soul, but
/// git-lex declares the range). The ontology speaks in IRIs; consumers
/// already hold the declared predicate IRI (via shapes/prop_iris), so the
/// IRI is the one join that cannot mis-attribute. (#82's key-mismatch
/// class, fixed at the table instead of per-consumer.)
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
            // Own-namespace FILTER only — the emitted key is the full IRI.
            let Some(prop) = p.as_str().strip_prefix(kit_ns.as_str()) else { continue };
            if prop.is_empty() || r.as_str().starts_with("http://www.w3.org/2001/XMLSchema#") {
                continue;
            }
            out.push((p.as_str().to_string(), r.as_str().to_string()));
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
                (
                    "https://repolex.ai/ontology/copia/lookBeingId".to_string(),
                    "https://repolex.ai/ontology/copia/Being".to_string()
                ),
                (
                    "https://repolex.ai/ontology/copia/lookMomentId".to_string(),
                    "https://repolex.ai/ontology/copia/Moment".to_string()
                ),
            ]
        );
    }

    /// The range=Thing declaration (Rob-ruled 2026-08-20) parses like any
    /// other non-XSD range — the SPECIAL meaning (identifier-form-only
    /// values) is the emitter's, not the parser's.
    #[test]
    fn parse_reference_ranges_accepts_thing_range() {
        let ttl = r#"
@prefix git-lex: <https://repolex.ai/ontology/git-lex/> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

git-lex: a owl:Ontology .
git-lex:relatedToId a owl:ObjectProperty ; rdfs:range git-lex:Thing .
"#;
        assert_eq!(
            parse_reference_ranges(ttl, "git-lex"),
            vec![(
                "https://repolex.ai/ontology/git-lex/relatedToId".to_string(),
                "https://repolex.ai/ontology/git-lex/Thing".to_string()
            )]
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

    // ── equivalent spellings (goodlux, 2026-09-17) ──

    /// A merged path lists the class's OWN spelling first; the runtime reads
    /// that head as the property's written IRI, so a pan:Node document keeps
    /// emitting pan:id while the inherited rule is checked over every
    /// spelling. The other spellings must not surface as extra properties.
    #[test]
    fn alternative_path_head_is_the_written_iri() {
        let shapes = r#"
@prefix sh:   <http://www.w3.org/ns/shacl#> .
@prefix t:    <https://repolex.ai/ontology/t/> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

t:NodeShape a sh:NodeShape ;
    sh:targetClass t:Node ;
    sh:property [
        sh:path [ sh:alternativePath ( <https://repolex.ai/ontology/p/id> t:id ) ] ;
        sh:nodeKind sh:IRI ;
        sh:minCount 1 ;
    ] ;
    sh:property [
        sh:path t:plainName ;
    ] .
"#;
        let parsed = parse_shape_file(shapes, "t");
        let node = parsed.shapes.iter().find(|s| s.class_name == "Node").expect("Node parsed");
        assert_eq!(node.props.len(), 2, "one property per shape block, not per spelling: {:?}", node.props);
        let id = node.props.iter().find(|p| p.name == "id").expect("id parsed");
        assert_eq!(id.iri, "https://repolex.ai/ontology/p/id", "the head of the list is the written IRI");
        assert!(id.is_iri && id.required, "typing and required-ness survive the alternative path");
        assert!(node.props.iter().any(|p| p.name == "plainName" && p.iri == "https://repolex.ai/ontology/t/plainName"));
    }

    // ── domain-open properties (#82) ──

    /// A property declared with NO rdfs:domain is deliberately usable on
    /// any class — shapes (per-class) can never see it, so it must come
    /// straight from the TTL. Domained props and foreign-namespace props
    /// stay out; string ranges carry no datatype (typed-literal convention).
    #[test]
    fn domain_open_parser_reads_the_declaration_not_the_shapes() {
        let ttl = r#"
@prefix soul: <https://repolex.ai/ontology/soul/> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

# The specimen: reference, domain left open so any doc-type may relate.
soul:relatedTo a owl:ObjectProperty ;
    rdfs:comment "Other documents this document references." .

# Domain-open datatype property with a non-string range → typed.
soul:openDate a owl:DatatypeProperty ;
    rdfs:range xsd:date .

# Domain-open string range → declared, but untyped (string convention).
soul:openText a owl:DatatypeProperty ;
    rdfs:range xsd:string .

# Domained → the shapes tables own it; NOT domain-open.
soul:noteId a owl:DatatypeProperty ;
    rdfs:domain soul:Note ;
    rdfs:range xsd:string .

# Foreign namespace → skipped (each kit records only its own).
<https://example.org/other/stray> a owl:ObjectProperty .
"#;
        let parsed: HashMap<String, DomainOpenProp> =
            parse_domain_open_properties(ttl, "soul").into_iter().collect();
        let related = parsed.get("relatedTo").expect("relatedTo is domain-open");
        assert!(related.is_object, "ObjectProperty must resolve as reference");
        assert_eq!(related.datatype, None);
        assert_eq!(related.iri, "https://repolex.ai/ontology/soul/relatedTo");
        let date = parsed.get("openDate").expect("openDate is domain-open");
        assert!(!date.is_object);
        assert_eq!(date.datatype.as_deref(), Some("http://www.w3.org/2001/XMLSchema#date"));
        let text = parsed.get("openText").expect("openText is domain-open");
        assert_eq!(text.datatype, None, "string ranges stay untyped");
        assert!(!parsed.contains_key("noteId"), "a domained prop is not domain-open");
        assert!(!parsed.contains_key("stray"), "foreign namespaces are skipped");
    }

    // ── class authoring lookup — rdfs:comment + authoringGuidance ──

    /// Fixture mirrors kit-base 0.10.3's declared shape: authoringGuidance
    /// as a `"""…"""` literal. The guidance body deliberately carries every
    /// character class a stanza scan would trip on — `#`, `-`, backticks,
    /// quotes, a markdown link — because this parser is a real Turtle
    /// parse and must not care.
    const KIT_AUTHORING_SAMPLE: &str = r###"
@prefix soul: <https://repolex.ai/ontology/soul/> .
@prefix git-lex: <https://repolex.ai/ontology/git-lex/> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

soul:Journal a owl:Class ;
    git-lex:foldered true ;
    rdfs:label "Journal" ;
    rdfs:comment "One day of the soul's life, written at rest." ;
    git-lex:authoringGuidance """## What I Did Today
## What I Learned
What surprised you — and what you got "wrong".
## Thoughts
Yours. No audience. `code`, [a link](/SOUL.md), # a hash.
## Tomorrow
- Your message to your future self.""" .

soul:Note a owl:Class ;
    rdfs:label "Note" ;
    rdfs:comment "Anything with no other home." .

soul:Bare a owl:Class .
"###;

    #[test]
    fn authoring_reads_both_annotations() {
        let a = parse_class_authoring(KIT_AUTHORING_SAMPLE, "soul", "Journal");
        assert_eq!(
            a.comment.as_deref(),
            Some("One day of the soul's life, written at rest.")
        );
        let g = a.guidance.expect("Journal declares guidance");
        assert!(g.starts_with("## What I Did Today"), "guidance mangled:\n{g}");
        assert!(g.contains("[a link](/SOUL.md), # a hash"),
            "embedded markdown specials must survive the parse:\n{g}");
        assert!(g.ends_with("future self."), "long literal mis-terminated:\n{g}");
    }

    #[test]
    fn authoring_comment_without_guidance() {
        let a = parse_class_authoring(KIT_AUTHORING_SAMPLE, "soul", "Note");
        assert_eq!(a.comment.as_deref(), Some("Anything with no other home."));
        assert_eq!(a.guidance, None, "no declaration must read as no guidance");
    }

    #[test]
    fn authoring_absent_annotations_are_none() {
        let a = parse_class_authoring(KIT_AUTHORING_SAMPLE, "soul", "Bare");
        assert_eq!(a.comment, None);
        assert_eq!(a.guidance, None);
    }

    #[test]
    fn authoring_missing_class_is_none() {
        let a = parse_class_authoring(KIT_AUTHORING_SAMPLE, "soul", "Nonexistent");
        assert_eq!(a.comment, None);
        assert_eq!(a.guidance, None);
    }

    #[test]
    fn authoring_empty_content_is_none() {
        let a = parse_class_authoring("", "soul", "Journal");
        assert_eq!(a.comment, None);
        assert_eq!(a.guidance, None);
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
    fn type_label_reads_soul_shaped_classes() {
        // Labels hold whether or not the retired lex-o annotations are
        // present (chain is label → local-name; lex-o is invisible to it).
        // Inline fixture in the soul kit's shape: one class with a label,
        // one with a stale lex-o annotation beside it, one with no label.
        const TTL: &str = r#"@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix lex-o: <https://repolex.ai/ontology/lex-o/> .
@prefix soul: <https://repolex.ai/ontology/soul/> .
soul: a owl:Ontology .
soul:Memory a owl:Class ; rdfs:label "Memory" .
soul:Note a owl:Class ; rdfs:label "Note" ; lex-o:typeLabel "note" .
soul:Journal a owl:Class ; rdfs:comment "One entry per waking." .
"#;
        assert_eq!(parse_class_type_label(TTL, "soul", "Memory"), "Memory");
        assert_eq!(parse_class_type_label(TTL, "soul", "Note"), "Note");
        assert_eq!(parse_class_type_label(TTL, "soul", "Journal"), "Journal");
    }

    #[test]
    fn type_label_keeps_multiword_labels() {
        // Multi-word labels survive the round-trip. Inline fixture, not a
        // live kit file: kits delete classes, and a test that reads one
        // breaks on every such release.
        const TTL: &str = r#"@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix copia: <https://repolex.ai/ontology/copia/> .
copia: a owl:Ontology .
copia:Place a owl:Class ; rdfs:label "Place" .
copia:CameraAngle a owl:Class ; rdfs:label "Camera Angle" .
"#;
        assert_eq!(parse_class_type_label(TTL, "copia", "Place"), "Place");
        assert_eq!(parse_class_type_label(TTL, "copia", "CameraAngle"), "Camera Angle");
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
    fn undeclared_class_keeps_its_name() {
        // The decision NoMatch feeds: a class the installed ontology does
        // not declare — a kit that retired it, a kit that is not installed,
        // or a typo — still names the class its documents were written
        // under. resolve_class_segment hands it back verbatim so the type
        // and the Thing-plane facts replay. Losing this is what erased a
        // retired class's entire history at rebuild, and what made kits
        // keep dead classes declared to buy replay back.
        let classes = vec!["Note".to_string(), "Journal".to_string()];
        assert_eq!(resolve_class_against(&classes, "Friend"), ClassMatch::NoMatch);
        // Casing still wins where the ontology can speak to it, so a case
        // slip never reaches the graph as a second, phantom class.
        assert_eq!(
            resolve_class_against(&classes, "journal"),
            ClassMatch::CaseOnly {
                canonical: "Journal".to_string(),
                given: "journal".to_string()
            }
        );
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
