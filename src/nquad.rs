//! N-Quad / N-Triple encoding and generation.
//!
//! Low-level escapers (`nq_escape`, `uri_encode_path`) plus the N-Quad
//! generators that produce git-lex's "now" view of the world:
//!
//! - `generate_frontmatter_nquads` — the "now" graph: current-state frontmatter
//!   extraction. (Body links are markdown links, extracted in `extraction.rs`;
//!   the wikilink reader was retired 2026-08-06, Rob-ruled.)
//! - `load_lex_nquads` — slurp any `.lex/**/*.nq` files the user wrote by hand.
//!
//! The git machinery layer lives in `git2_nquads` (library reads, `git2:`
//! vocab); the old shell-out `generate_git_nquads` (`git:` vocab, plus the
//! write-only changeset/blame/language layers nothing consumed) was removed
//! at the git2 cutover.
//!
//! Peeled out of `main.rs` during modularization.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use git_lex::find_git_root;

use crate::git::graph_uri;

use crate::extraction::{flatten_yaml, normalize_wikilink_path};
use crate::ontology::{get_kit_namespaces_all_kits, get_object_properties_all_kits,
                       get_property_datatypes_all_kits};
use crate::resolve;

/// Write a sidecar (or its `.meta`) file, failing the process on error.
/// A silently unwritten sidecar is a permanent history gap: the committed
/// sidecar diff is the one graph's ONLY event source, so "couldn't write,
/// carried on" means facts that never happened as far as history is
/// concerned (review finding A8).
pub(crate) fn write_sidecar_loud(path: &std::path::Path, content: &str) {
    // Already byte-identical → nothing to do. The walk regenerates EVERY
    // sidecar on EVERY run, so on a repo where one file changed this was
    // thousands of writes of bytes already on disk (5,840 of them per sync
    // on the fleet's largest repo — 2026-08-23 measurement).
    //
    // Skipping is safe precisely BECAUSE the test is on content: the end
    // state is identical either way, and the file's own bytes are the
    // instrument — no mtime, no cache, nothing to go stale.
    //
    // A read error is NOT a decision. Fall through and write, so a
    // permissions or encoding problem surfaces at the loud write below
    // instead of being silently mistaken for "unchanged".
    if let Ok(existing) = fs::read_to_string(path) {
        if existing == content {
            return;
        }
    }
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!("fatal: failed to create sidecar dir {}: {e}", parent.display());
            std::process::exit(1);
        }
    }
    if let Err(e) = fs::write(path, content) {
        eprintln!("fatal: failed to write sidecar {}: {e}", path.display());
        std::process::exit(1);
    }
}

/// Remove a stale sidecar, failing the process on error (already-gone is
/// fine — that's the desired end state). A stale sidecar that survives
/// removal keeps its facts alive forever: the sync diff never sees the
/// lines vanish, so the retraction events never exist (review finding A3).
pub(crate) fn remove_sidecar_loud(path: &std::path::Path) {
    if let Err(e) = fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            eprintln!("fatal: failed to remove stale sidecar {}: {e}", path.display());
            std::process::exit(1);
        }
    }
}

/// Escape a string for use in N-Quads literals.
pub(crate) fn nq_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Percent-encode one character into `out` (the shared table for both
/// encoders below).
fn push_uri_encoded(c: char, out: &mut String) {
    match c {
        '%' => out.push_str("%25"),
        '"' => out.push_str("%22"),
        '\\' => out.push_str("%5C"),
        ' ' => out.push_str("%20"),
        '<' => out.push_str("%3C"),
        '>' => out.push_str("%3E"),
        '{' => out.push_str("%7B"),
        '}' => out.push_str("%7D"),
        '|' => out.push_str("%7C"),
        '^' => out.push_str("%5E"),
        '`' => out.push_str("%60"),
        '[' => out.push_str("%5B"),
        ']' => out.push_str("%5D"),
        c if !c.is_ascii() => {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{:02X}", b));
            }
        }
        c => out.push(c),
    }
}

/// Percent-encode a path for use in URIs (spaces, special chars, non-ASCII).
pub(crate) fn uri_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        push_uri_encoded(c, &mut out);
    }
    out
}

/// Percent-encode a full http(s) URL for IRI use — like `uri_encode_path`,
/// but an EXISTING `%XX` escape passes through untouched. Rule-4 passthrough
/// values often arrive already encoded (`Caf%C3%A9`); re-encoding the `%`
/// mints a DIFFERENT URL (`Caf%25C3%25A9`) than the author wrote. A bare `%`
/// not followed by two hex digits still encodes, so oxigraph's strict
/// N-Quads parser never sees a structurally invalid IRI.
pub(crate) fn uri_encode_url(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '%'
            && i + 2 < chars.len()
            && chars[i + 1].is_ascii_hexdigit()
            && chars[i + 2].is_ascii_hexdigit()
        {
            out.push('%');
            i += 1;
            continue;
        }
        push_uri_encoded(chars[i], &mut out);
        i += 1;
    }
    out
}

/// Split a frontmatter ObjectProperty value into its list items.
///
/// Values are comma-separated lists — EXCEPT that a comma inside a URL is
/// part of the URL (`https://en.wikipedia.org/wiki/Washington,_D.C.` is ONE
/// value, not a wrong IRI plus a rejected `_D.C.`). When the value starts
/// with `http(s)://`, a comma only starts a new item where the next item
/// itself begins a new `http(s)://` URL; everything else keeps the plain
/// comma split. Used by the emitter, the validate path, and the identity
/// gate — ONE splitter, so validation judges exactly what sync will emit.
pub(crate) fn split_object_values(object: &str) -> Vec<String> {
    let is_url = |s: &str| s.starts_with("http://") || s.starts_with("https://");
    if !is_url(object.trim()) {
        return object
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    let mut items: Vec<String> = Vec::new();
    let mut current = String::new();
    for piece in object.split(',') {
        if is_url(piece.trim()) && !current.is_empty() {
            items.push(current.trim().to_string());
            current.clear();
        }
        if !current.is_empty() {
            current.push(',');
        }
        current.push_str(piece);
    }
    if !current.trim().is_empty() {
        items.push(current.trim().to_string());
    }
    items.retain(|s| !s.is_empty());
    items
}

/// Load .lex/*.nq files and return their contents.
pub(crate) fn load_lex_nquads() -> String {
    let root = match find_git_root() {
        Some(r) => r,
        None => return String::new(),
    };

    let mut nq = String::new();
    let lex_dir = root.join(".lex");

    // Recursively find all .nq files
    fn walk_nq(dir: &std::path::Path, nq: &mut String) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_dir() {
                    walk_nq(&path, nq);
                } else if path.extension().is_some_and(|e| e == "nq") {
                    if let Ok(content) = fs::read_to_string(&path) {
                        nq.push_str(&content);
                        if !content.ends_with('\n') {
                            nq.push('\n');
                        }
                    }
                }
            }
        }
    }

    if lex_dir.exists() {
        walk_nq(&lex_dir, &mut nq);
    }

    nq
}

/// THE repo document walker. Every consumer walks the same file set —
/// `.md` and `.txt`, dot-entries skipped — through this one function.
/// Call sites that need a narrower set filter EXPLICITLY (see
/// `is_template`); the four hand-rolled walkers this replaces had drifted
/// into three different file policies.
pub(crate) fn walk_repo_docs(root: &std::path::Path) -> Vec<PathBuf> {
    fn walk(dir: &std::path::Path, files: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') { continue; }
                if path.is_dir() {
                    walk(&path, files);
                } else if name.ends_with(".md") || name.ends_with(".txt") {
                    files.push(path);
                }
            }
        }
    }
    let mut files = Vec::new();
    walk(root, &mut files);
    files
}

/// Is this a kit-derived `__Class.md` scaffold template?
pub(crate) fn is_template(path: &std::path::Path) -> bool {
    path.file_name()
        .map(|n| n.to_string_lossy().starts_with("__"))
        .unwrap_or(false)
}

/// Everything the shared emitter resolves against, built ONCE per run:
/// the slug/path indexes over the repo's documents and the all-kits
/// ontology tables. Sync used to build the indexes twice per run (once
/// inside frontmatter generation, again for the history walk).
pub(crate) struct ResolverContext {
    pub files: Vec<PathBuf>,
    pub path_index: HashSet<String>,
    pub obj_props: HashSet<String>,
    pub prop_datatypes: HashMap<String, String>,
    /// Every "{kit}/{Class}/{prop}" any installed kit declares — datatype-
    /// unconditional. The undeclared-key warning consults THIS set;
    /// `prop_datatypes` (typed-literal emission only) misses every
    /// xsd:string property by design and must never gate the warning.
    pub declared_props: HashSet<String>,
    pub kit_namespaces: HashMap<String, String>,
    /// Law-6 reference ranges: property IRI → range class IRI, from
    /// installed kit TTLs (owl:ObjectProperty + non-XSD rdfs:range).
    /// IRI-keyed (2026-08-20) so INHERITED properties join — the authoring
    /// kit's `{kit}/{prop}` key missed ranges declared where the property
    /// lives. A declared range makes the property's authored value a
    /// TARGET ID, resolved to `<range-app>/<RangeClass>/<id>` at emission;
    /// range git-lex:Thing (THING_CLASS_IRI) is the angle-bracket lane
    /// (identifier form only); without any range, the legacy path/IRI
    /// resolver applies (resolve.rs).
    pub ref_ranges: HashMap<String, String>,
    /// "{kit}/{Class}/{prop}" → the property's DECLARED IRI, from the
    /// generated shapes. The predicate used to be built by gluing the
    /// document's own kit namespace onto the key's property segment, which was
    /// right only while every property a class carried came from that class's
    /// own kit. Inheritance ended that (#104): `soul.Note.title` would have
    /// emitted `.../soul/title` for a property declared at
    /// `.../git-lex/title`. The ontology says where a property lives; the
    /// emitter now asks instead of assuming.
    pub prop_iris: HashMap<String, String>,
    /// "{kit}/{prop}" → optional replacement for owl:deprecated properties.
    /// Retired-by-deprecation keys are DECLARED (history stays replayable);
    /// the save-time note teaches the deprecation instead of falsely
    /// claiming the key does not exist.
    pub deprecated_props: HashMap<String, Option<String>>,
    /// "{kit}/{prop}" → domain-open properties (no rdfs:domain — usable on
    /// any class), read straight from the ontology TTLs. Shapes are
    /// per-class, so every shapes-derived table above is blind to these by
    /// construction — which is how a genuinely declared key was false-warned
    /// as nonexistent (#82, soul:relatedTo). No class in the key on purpose:
    /// no domain means every class is in scope.
    pub domain_open_props: HashMap<String, crate::ontology::DomainOpenProp>,
}

impl ResolverContext {
    pub(crate) fn build(root: &std::path::Path) -> ResolverContext {
        let files = walk_repo_docs(root);
        let path_index = build_path_index(root, &files);
        ResolverContext {
            files,
            path_index,
            obj_props: get_object_properties_all_kits(),
            prop_datatypes: get_property_datatypes_all_kits(),
            declared_props: crate::ontology::get_declared_properties_all_kits(),
            kit_namespaces: get_kit_namespaces_all_kits(),
            ref_ranges: crate::ontology::get_reference_ranges_all_kits(),
            prop_iris: crate::ontology::get_property_iris_all_kits(),
            deprecated_props: crate::ontology::get_deprecated_properties_all_kits(),
            domain_open_props: crate::ontology::get_domain_open_properties_all_kits(),
        }
    }
}

/// The universal Thing class (git-lex.ttl). As a DECLARED RANGE it is the
/// "angle-bracket field" switch (Rob-ruled 2026-08-20): the target could be
/// any class in any kit, so the authored value must carry its own namespace
/// and class — the identifier form `<namespace/Class/id>`, resolved by
/// `resolve::resolve_thing_reference`, everything else rejected at save.
pub(crate) const THING_CLASS_IRI: &str = "https://repolex.ai/ontology/git-lex/Thing";

/// A Thing IRI derived from a range CLASS IRI + a bare target id:
/// `https://repolex.ai/ontology/copia/Being` + `lux`
/// → `https://repolex.ai/copia/Being/lux` (universal instance law; the
/// class's own namespace decides the application, so cross-kit ranges
/// resolve into the right id-space). Returns the bracketed IRI.
pub(crate) fn thing_iri_from_range(range_class_iri: &str, id: &str) -> Option<String> {
    let split = range_class_iri.rfind('/')?;
    let (ns, class) = range_class_iri.split_at(split + 1);
    if class.is_empty() {
        return None;
    }
    Some(format!(
        "<{}{}/{}>",
        app_base_from_kit_ns(ns),
        class,
        uri_encode_path(id)
    ))
}

/// The per-file subject anchors of the two identity planes (identity model
/// Laws 1–5, Rob-ruled 2026-07-30). Derived ONCE per file from its full
/// sidecar line set, then every line emits against the right plane:
///
/// - `file_uri` — the File node `git-lex/File/<path>`: linksTo edges,
///   markdown/fm facts, git facts. Always present; a no-kit repo is a
///   File-only graph and that is the bare-markdown tier working.
/// - `thing_uri` — the Thing node `<kit-app>/<Class>/<id>`: kit-declared
///   facts + rdf:type. Present only when the file's class has a known id
///   property AND the sidecar carries its value. The Thing→File
///   connection is the derived `fileId` edge (Law 5).
///
/// All IRIs carry angle brackets (ready to print into N-Quads).
pub(crate) struct FileSubjects {
    pub file_uri: String,
    pub thing_uri: Option<String>,
    /// (kit short name, canonical class) of the anchoring Thing.
    pub thing_key: Option<(String, String)>,
}

/// A kit's a-box (application) base from its ontology namespace:
/// `https://repolex.ai/ontology/soul/` → `https://repolex.ai/soul/` — the
/// universal instance law (`<application>/<Class>/<id>`, Rob Day-50),
/// derived from the installed TTL's own declaration, never hardcoded.
fn app_base_from_kit_ns(kit_ns: &str) -> String {
    match kit_ns.find("/ontology/") {
        Some(i) => format!("{}/{}", &kit_ns[..i], &kit_ns[i + "/ontology/".len()..]),
        None => kit_ns.to_string(),
    }
}

/// Derive both plane anchors for one file from its full sidecar line set.
///
/// Thing-anchor derivation: the file's class is the FIRST kit-classed line's
/// class (resolved against the ontology, same rule as type emission); its id
/// property IS `<className>Id` — CONVENTION-AS-LAW, Rob-ruled 2026-08-02:
/// "shouldn't the id property always be the classNameId?" Every
/// file-expressed class conforms (soul 17/17, copia, File→fileId); the
/// pattern-breakers (pool's cid, git2's sha) are machine-derived classes
/// that never pass through frontmatter, documented non-cases. The name is
/// validated against the class's DECLARED properties, so a class that
/// ships no `<class>Id` property anchors nothing and says so — the
/// lintable guarantee, now enforced at the kit level rather than by a
/// per-class annotation. A classed file whose id VALUE is absent anchors
/// NOTHING (facts stay on the File node) and warns: pre-migration corpora
/// hit this constantly, and the warning is the Phase-4 work list.
/// (Flipping that warning to a save-time reject is the post-migration
/// step — see the identity model doc §2.3.)
pub(crate) fn derive_file_subjects(
    spo_lines: &[String],
    relpath_str: &str,
    declared_props: &HashSet<String>,
    obj_props: &HashSet<String>,
    kit_namespaces: &HashMap<String, String>,
    warn: bool,
) -> FileSubjects {
    let file_uri = format!("<{}>", crate::git::file_iri(&uri_encode_path(relpath_str)));

    // First kit-classed line decides the file's class (existing single-class
    // rule; other classes' lines warn at emission as before).
    let mut anchor: Option<(String, String)> = None; // (kit, canonical class)
    for line in spo_lines {
        let parts: Vec<&str> = line.splitn(3, " | ").collect();
        if parts.len() != 3 || parts[1] != "hasValue" {
            continue;
        }
        let segments: Vec<&str> = parts[0].splitn(3, '.').collect();
        if segments.len() != 3 {
            continue;
        }
        if let Ok(canonical) =
            crate::ontology::resolve_class_segment(segments[0], segments[1], relpath_str, warn)
        {
            anchor = Some((segments[0].to_string(), canonical));
            break;
        }
    }
    let Some((kit, class)) = anchor else {
        return FileSubjects { file_uri, thing_uri: None, thing_key: None };
    };

    // ═══ THE UNIVERSAL id LANE (Rob-ruled 2026-08-21) — tried FIRST. ═══
    // `{kit}.{Class}.id` (git-lex:id, inherited onto every Thing subclass)
    // carries the Thing's FULL address: `<namespace/Class/identifier>`.
    // The id value is THE identity authority — namespace, class, and
    // identifier all come from inside it; the folder, the filename, and
    // the key prefix are just where the Thing is parked (which is what
    // buys subfolder freedom: Soul/Note/archive/x.md can still BE
    // <soul/Note/x>). On any disagreement with the key prefix, the id
    // wins — the file's kit-line facts still anchor to the id's Thing.
    //
    // The per-class convention lane below (`noteId` = lowerFirst(Class) +
    // "Id") is the TRANSITION fallback: the unmigrated corpus anchors
    // exactly as before; `create` scaffolds BOTH during the window
    // (Rob-ruled); the one-swoop removal comes after tr1p's deprecation.
    let universal_key = format!("{}.{}.id", kit, class);
    let universal_value = spo_lines.iter().find_map(|line| {
        let parts: Vec<&str> = line.splitn(3, " | ").collect();
        if parts.len() == 3
            && parts[1] == "hasValue"
            && parts[0] == universal_key
            && !parts[2].trim().is_empty()
        {
            Some(parts[2].trim().to_string())
        } else {
            None
        }
    });
    if let Some(raw) = universal_value {
        match parse_universal_id(&raw) {
            Ok((id_ns, id_class, _identifier, inner)) => {
                if (id_ns.as_str(), id_class.as_str()) != (kit.as_str(), class.as_str()) && warn {
                    eprintln!(
                        "note: {relpath_str}: the id `{raw}` declares `{id_ns}/{id_class}` while \
                         the file's keys say `{kit}.{class}` — the id is the identity authority \
                         and wins; align the keys (or the folder) when convenient."
                    );
                }
                let thing_uri = format!(
                    "<{}{}>",
                    crate::git::RESOURCE_ROOT,
                    uri_encode_path(&inner)
                );
                return FileSubjects {
                    file_uri,
                    thing_uri: Some(thing_uri),
                    thing_key: Some((id_ns, id_class)),
                };
            }
            Err(msg) => {
                if warn {
                    eprintln!(
                        "warning: {relpath_str}: `{universal_key}` — {msg} The per-class id \
                         (if present) anchors this file for now."
                    );
                }
                // Fall through to the convention lane: a malformed .id
                // must not cost the file the identity it already had.
            }
        }
    }

    // Convention-as-law: id property = lowerFirst(Class) + "Id", valid
    // only if the class actually declares it.
    let id_prop = {
        let mut conv = class.clone();
        if let Some(first) = conv.get_mut(0..1) {
            let low = first.to_lowercase();
            conv.replace_range(0..1, &low);
        }
        conv.push_str("Id");
        conv
    };
    let prop_key = format!("{}/{}/{}", kit, class, id_prop);
    if !declared_props.contains(&prop_key) && !obj_props.contains(&prop_key) {
        if warn {
            eprintln!(
                "warning: {relpath_str}: the class `{kit}.{class}` has no `{id_prop}` \
                 key in its ontology, so documents of this class cannot get their own \
                 identity in the graph. You cannot fix this by editing this file — \
                 report it to the `{kit}` ontology owner. (The document's facts still \
                 save, attached to the file itself.)"
            );
        }
        return FileSubjects { file_uri, thing_uri: None, thing_key: None };
    }

    // Find the id value in this file's own lines.
    let id_line_key = format!("{}.{}.{}", kit, class, id_prop);
    let id_value = spo_lines.iter().find_map(|line| {
        let parts: Vec<&str> = line.splitn(3, " | ").collect();
        if parts.len() == 3
            && parts[1] == "hasValue"
            && parts[0] == id_line_key
            && !parts[2].trim().is_empty()
        {
            Some(parts[2].trim().to_string())
        } else {
            None
        }
    });
    let Some(id_value) = id_value else {
        if warn {
            let stem = std::path::Path::new(relpath_str)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| relpath_str.to_string());
            eprintln!(
                "warning: {relpath_str}: this {kit}.{class} document has no id. Fix: \
                 add this line to the YAML block at the top of the file: \
                 {id_line_key}: \"{stem}\""
            );
        }
        return FileSubjects { file_uri, thing_uri: None, thing_key: None };
    };

    let kit_ns = kit_namespaces
        .get(&kit)
        .cloned()
        .unwrap_or_else(|| git_lex::conventional_kit_namespace(&kit));
    let thing_uri = format!(
        "<{}{}/{}>",
        app_base_from_kit_ns(&kit_ns),
        class,
        uri_encode_path(&id_value)
    );
    FileSubjects {
        file_uri,
        thing_uri: Some(thing_uri),
        thing_key: Some((kit, class)),
    }
}

/// Parse a universal-id value: `<namespace/Class/identifier>`. Returns
/// `(namespace, Class, identifier, inner)` or a teaching message.
///
/// The identifier may itself contain slashes — the address is what it is;
/// namespace and Class are the first two segments, everything after is the
/// identifier. (File-side subfolders never appear here: the id is the
/// Thing's address, not the file's path.)
pub(crate) fn parse_universal_id(raw: &str) -> Result<(String, String, String, String), String> {
    let trimmed = raw.trim();
    let Some(inner) = trimmed.strip_prefix('<').and_then(|r| r.strip_suffix('>')) else {
        return Err(format!(
            "the value `{raw}` has no angle brackets — the universal id is the Thing's \
             full address, written <namespace/Class/identifier>, e.g. \
             <soul/Note/20260821-my-note>."
        ));
    };
    let inner = inner.trim();
    if inner.contains("://") {
        return Err(
            "the id is written relative to the one root — <namespace/Class/identifier>, \
             never a full URL."
                .to_string(),
        );
    }
    let segs: Vec<&str> = inner.split('/').collect();
    if segs.len() < 3 || segs.iter().take(3).any(|s| s.is_empty()) {
        return Err(format!(
            "`<{inner}>` does not name namespace, Class, AND identifier — three segments, \
             e.g. <soul/Note/20260821-my-note>."
        ));
    }
    Ok((
        segs[0].to_string(),
        segs[1].to_string(),
        segs[2..].join("/"),
        inner.to_string(),
    ))
}

/// Emit the per-file anchor facts shared by BOTH graph paths:
/// the File node's rdf:type, and — when the file expresses a Thing — the
/// Thing's rdf:type plus the derived `fileId` edge (Law 5,
/// Thing → File). In the one-graph walk these participate in resolved-set
/// diffing, so a file move produces exactly the honest fileId
/// retract+assert pair and nothing else.
pub(crate) fn emit_file_anchor_nquads(
    subjects: &FileSubjects,
    kit_namespaces: &HashMap<String, String>,
    graph: &str,
    emitted_types: &mut HashSet<String>,
    out: &mut String,
) {
    out.push_str(&format!(
        "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/File> {} .\n",
        subjects.file_uri, graph
    ));
    let (Some(thing_uri), Some((kit, class))) = (&subjects.thing_uri, &subjects.thing_key) else {
        return;
    };
    let kit_ns = kit_namespaces
        .get(kit)
        .cloned()
        .unwrap_or_else(|| git_lex::conventional_kit_namespace(kit));
    if emitted_types.insert(format!("{}.{}", kit, class)) {
        out.push_str(&format!(
            "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{}{}> {} .\n",
            thing_uri, kit_ns, class, graph
        ));
    }
    out.push_str(&format!(
        "{} <https://repolex.ai/ontology/git-lex/fileId> {} {} .\n",
        thing_uri, subjects.file_uri, graph
    ));
}

/// What each caller needs from the ONE working-tree walk. Three callers,
/// three shapes — save wants the sidecars and gates but discards the text,
/// sync wants both, query wants the text and must not write (a read-only
/// command dirtying the tree was the old behavior, not a feature). A struct
/// rather than two positional bools: identical adjacent types are how the
/// emitter's argument-swap bug compiled clean (review #15).
#[derive(Clone, Copy)]
pub(crate) struct NowWalkOpts {
    /// Write/refresh the `.fm.spo` and `.md.spo` sidecars (and remove stale
    /// ones). False on the query path — query never touches the tree.
    pub write_sidecars: bool,
    /// Accumulate and return the now-graph N-Quads text. False on the save
    /// path, which used to build the full string only to drop it on the
    /// floor. Resolution, warnings, and the returned error COUNT are
    /// identical either way — the gates run in full regardless.
    pub build_nquads: bool,
}

/// Extract frontmatter from all .md/.txt files in the repo into the "now"
/// graph. Sidecar writing (`.fm.spo` + `.md.spo`) and N-Quads emission are
/// selected per caller via [`NowWalkOpts`]. Body linking is markdown links
/// (`linksTo`), extracted in the SAME pass — one read + one tree-sitter
/// parse per document; the wikilink reader and commit-message scanning this
/// doc once promised are retired (Rob-ruled 2026-08-06 — `[[...]]` in a
/// body is plain prose).
pub(crate) fn generate_frontmatter_nquads(opts: NowWalkOpts) -> (String, u32) {
    let root = match find_git_root() {
        Some(r) => r,
        None => return (String::new(), 0),
    };
    let ctx = ResolverContext::build(&root);
    generate_frontmatter_nquads_with(&root, &ctx, opts)
}

/// [`generate_frontmatter_nquads`] against a caller-built context — sync
/// builds ONE `ResolverContext` and shares it with the history walk.
pub(crate) fn generate_frontmatter_nquads_with(
    root: &std::path::Path,
    ctx: &ResolverContext,
    opts: NowWalkOpts,
) -> (String, u32) {
    let root = root.to_path_buf();

    // The "now" graph is the canonical view of current state: extracted
    // frontmatter plus the git-layer facts derived from the working tree
    // as it exists right now. Contrasts with the ONE graph
    // (LexHistoryGraph), which holds the full event history and its
    // materialized base layer. (The old sync/<sha> snapshot family this
    // comment once contrasted against is retired and swept at every sync;
    // wikilinks/mentions likewise no longer exist anywhere.)
    let graph = format!("<{}>", graph_uri("now"));
    let mut nq = String::new();
    let mut total_errors: u32 = 0;

    // Build ObjectProperty + datatype lookup across ALL installed kits (base
    // + domain + optionals). Frontmatter triples from any kit's properties
    // need correct IRI-vs-literal classification and typed-literal tags
    // (xsd:integer, xsd:date, etc.) regardless of which kit declared the
    // property. The previous single-kit lookup hid optional-kit datatypes —
    // e.g. copia:firstVisited (xsd:date) emitted as untyped string.

    // Open git repo for blob hash lookups
    let repo = git2::Repository::discover(".").ok();

    let files = ctx.files.as_slice();
    let (obj_props, kit_namespaces) = (&ctx.obj_props, &ctx.kit_namespaces);

    // Markdown-link lane (th34 #5): the now view used to carry ZERO
    // document-to-document edges — linksTo lived only in the synced store,
    // so `git lex query` proved "git-lex discards links" to anyone who
    // checked. Same extractor, same md-only index as cmd_extract (a
    // different index here would mean two resolution policies — the A5
    // disease). Lines are emitted into the graph only, never written to
    // the .fm.spo sidecar (that stays frontmatter-only; .md.spo is the
    // link sidecar and cmd_extract owns it).
    let mut md_parser = tree_sitter_md::MarkdownParser::default();
    let mut total_links: usize = 0;
    let md_index: HashSet<String> = files.iter()
        .filter(|p| p.extension().is_some_and(|x| x == "md") && !is_template(p))
        .filter_map(|p| p.strip_prefix(&root).ok())
        .map(|p| p.to_string_lossy().to_string())
        .collect();

    // entity_classes was used by the old range-aware resolver, which has been
    // replaced by src/resolve.rs. The range-check approach (matching class IRIs
    // across kits) had a cross-kit identity bug (squad:Agent ≠ soul:Agent) and
    // is deferred until cross-kit class equivalence is designed. For now the
    // resolver trusts bare-slug + full-IRI resolution without range filtering.

    // Ensure extract dir exists
    let extract_dir = root.join(".lex").join("extract");
    fs::create_dir_all(&extract_dir).ok();

    // The walk cache (incremental-sync spec §4.3, Rob-approved 2026-08-26):
    // per-file finished fragments keyed on CONTENT IDENTITY (working-tree
    // blob hash + index blob hash), under a context hash that carries the
    // two total gates — the installed ontology's bytes and the document
    // existence set. Either gate trips → the context hash changes → the
    // cache refuses to load → this run IS the full walk, which is exactly
    // today's behavior. GIT_LEX_FULL_WALK=1 forces that path by hand.
    let ctx_hash = crate::walkcache::context_hash(&root, files);
    let force_full = std::env::var_os("GIT_LEX_FULL_WALK").is_some();
    let mut cache = if force_full {
        crate::walkcache::WalkCache::empty(&root, &ctx_hash)
    } else {
        crate::walkcache::WalkCache::load(&root, &ctx_hash)
            .unwrap_or_else(|| crate::walkcache::WalkCache::empty(&root, &ctx_hash))
    };
    // The tampered-sidecar belt: a sidecar dirty in git while its source
    // file is unchanged means the on-disk sidecar diverged from what the
    // last commit pinned — send its source through the full pipeline so
    // the sidecar write converges it. One `git status` for the whole run.
    let forced_sources = dirty_sidecar_sources(&root);
    let mut cache_hits: usize = 0;

    for filepath in files {
        // Unreadable docs are LOUD and counted (review #23): skipping one
        // bypasses the stale-sidecar removal below, so its existing sidecar
        // keeps asserting facts the sync diff never sees vanish.
        let content = match fs::read_to_string(filepath) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "error: cannot read {} for extraction ({e}) — its \
                     existing sidecar (if any) is NOT updated; fix the file \
                     (permissions / invalid UTF-8) or delete it",
                    filepath.display()
                );
                total_errors += 1;
                continue;
            }
        };

        let relpath = filepath.strip_prefix(&root).unwrap_or(filepath);
        let relpath_str = relpath.to_string_lossy().to_string();

        // Blob hash from the git index (staging area) — feeds the emitted
        // `git/blobHash` quad AND the cache identity, so it is computed on
        // every path now (it is one index lookup; the cache it enables
        // skips a YAML parse, a tree-sitter parse and the quad emission).
        let blob_hash = repo.as_ref().and_then(|r| {
            if let Ok(index) = r.index() {
                if let Some(entry) = index.get_path(std::path::Path::new(&relpath_str), 0) {
                    return Some(entry.id.to_string());
                }
            }
            let head = r.head().ok()?;
            let tree = head.peel_to_tree().ok()?;
            let entry = tree.get_path(std::path::Path::new(&relpath_str)).ok()?;
            Some(entry.id().to_string())
        }).unwrap_or_default();

        // Cache hit: the file's bytes and index state are exactly what
        // produced the stored fragment, and no belt forces it through.
        // Its quads append verbatim; its sidecars are already right (same
        // bytes → same extraction). Warnings for unchanged files go quiet
        // until the file is next edited — deliberate; they fired at the
        // save that introduced them and fire again on any change.
        let bytes_hash = crate::walkcache::blob_hash_of(content.as_bytes());
        if !force_full && !forced_sources.contains(&relpath_str) {
            if let Some((frag, links)) =
                cache.hit(&relpath_str, &bytes_hash, &blob_hash, opts.build_nquads)
            {
                cache_hits += 1;
                total_links += links;
                if opts.build_nquads {
                    nq.push_str(&frag);
                }
                continue;
            }
        }
        let file_errors_start = total_errors;
        let file_nq_start = nq.len();
        let mut file_links: usize = 0;

        // --- Frontmatter extraction ---
        // Only the YAML block is read here. The BODY is deliberately not
        // parsed: wikilink extraction retired (Rob-ruled 2026-08-06) —
        // markdown links are the linking story and extraction.rs emits
        // their `linksTo` lines; `[[...]]` in a body is plain prose.
        // Historical `linksTo` sidecar lines still replay through the quad
        // emitter below — history doesn't un-happen.
        let mut spo_lines = Vec::new();

        // ONE frontmatter parser (review #9): the shared fence rule in lib.rs.
        if let (Some(yaml_str), _) = git_lex::split_frontmatter(&content) {
            // ONE frontmatter YAML parser (#101): the shared duplicate-key
            // gate in lib.rs. This path used to deserialize into a HashMap,
            // which accepts a repeated key and keeps only the last value — so
            // a walk silently re-emitted the same loss the save made.
            match git_lex::parse_frontmatter_map(yaml_str) {
                Ok(yaml) => {
                    for (key_node, value) in &yaml {
                        if let Some(key) = key_node.as_str() {
                            flatten_yaml(key, value, &mut spo_lines);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("error: {}: {}", relpath_str, e);
                    total_errors += 1;
                }
            }
        }

        // Sort and dedup
        spo_lines.sort();
        spo_lines.dedup();

        // Write .spo sidecar; when a doc's extractable content goes away its
        // existing sidecar must go away too, so the sync diff sees the lines
        // vanish and records retractions (the one graph's only
        // signal — the now graph rebuilds from files and never notices).
        let spo_path = extract_dir.join(format!("{}.fm.spo", relpath_str));
        if opts.write_sidecars {
            if !spo_lines.is_empty() {
                let spo_content = spo_lines.join("\n") + "\n";
                write_sidecar_loud(&spo_path, &spo_content);
            } else if spo_path.exists() {
                remove_sidecar_loud(&spo_path);
            }
        }

        // Markdown links join the emission stream AFTER the sidecar write —
        // the .fm.spo sidecar carries frontmatter only (th34 #5; see the
        // md_index comment above the loop). This is THE md walk: the same
        // parse also writes the `.md.spo` sidecar (link lines only,
        // sorted+deduped — the bytes the retired second walk in
        // extraction.rs produced), so each document is read and
        // tree-sitter-parsed exactly once per run.
        if md_index.contains(&relpath_str) {
            let fm_len = spo_lines.len();
            match md_parser.parse(content.as_bytes(), None) {
                Some(tree) => {
                    crate::extraction::extract_md_link_lines(
                        &tree, &content, &relpath_str, &md_index, &mut spo_lines,
                    );
                    let mut md_lines: Vec<String> = spo_lines[fm_len..].to_vec();
                    md_lines.sort();
                    md_lines.dedup();
                    file_links = md_lines.len();
                    if opts.write_sidecars {
                        let md_path =
                            extract_dir.join(format!("{}.md.spo", relpath_str));
                        if !md_lines.is_empty() {
                            write_sidecar_loud(&md_path, &(md_lines.join("\n") + "\n"));
                            total_links += md_lines.len();
                        } else if md_path.exists() {
                            remove_sidecar_loud(&md_path);
                        }
                    }
                }
                None => {
                    // Same contract as the read-failure above: skipping
                    // bypasses the sidecar-removal branch, so the doc's
                    // existing sidecar keeps asserting links the doc may no
                    // longer carry — be LOUD and count it.
                    eprintln!(
                        "error: tree-sitter could not parse {} — its existing \
                         sidecar (if any) is NOT updated",
                        filepath.display()
                    );
                    total_errors += 1;
                }
            }
        }

        // --- Generate N-Quads for oxigraph (now graph) ---
        // Identity model (Rob-ruled 2026-07-30, re-anchor 2026-08-02): two
        // plane anchors per file. The File node `git-lex/File/<path>` (the
        // path IS the id — Law 4) carries the git layer, links, and fm
        // facts; when the file expresses a Thing with an authored id, the
        // Thing node `<kit-app>/<Class>/<id>` (Law 2) carries the
        // kit-declared facts, connected by the derived fileId edge (Law 5).
        // Classes come from the ontology and explicit dot-notation, never
        // folder-name guessing.
        //
        // File location is git:path — a git-lex-authored synthetic fact from
        // the on-disk path, NOT a user frontmatter key; fm: carries ONLY what
        // the user wrote (the fm firewall).
        let subjects = derive_file_subjects(
            &spo_lines,
            &relpath_str,
            &ctx.declared_props,
            &obj_props,
            &kit_namespaces,
            true, // the now path is the save/sync moment — warn here
        );

        nq.push_str(&format!(
            "{} <https://repolex.ai/ontology/git-lex/git/path> \"{}\" {} .\n",
            subjects.file_uri, nq_escape(&relpath_str), graph
        ));
        // Omit the triple when the hash is unknown (review #51): an
        // untracked new doc (or a failed repo discovery) used to assert
        // `git/blobHash ""` — a false fact all untracked files SHARED,
        // enabling spurious joins. Absence is the honest statement; the
        // post-save sync fills it in.
        if !blob_hash.is_empty() {
            nq.push_str(&format!(
                "{} <https://repolex.ai/ontology/git-lex/git/blobHash> \"{}\" {} .\n",
                subjects.file_uri, blob_hash, graph
            ));
        }

        // Track which kit types we've seen for rdf:type emission (dedup)
        let mut emitted_types: HashSet<String> = HashSet::new();

        // File rdf:type + (when anchored) Thing rdf:type + fileId edge.
        emit_file_anchor_nquads(&subjects, &kit_namespaces, &graph, &mut emitted_types, &mut nq);

        for line in &spo_lines {
            total_errors += emit_spo_line_nquads(
                line,
                &subjects,
                &graph,
                &relpath_str,
                ctx,
                true, // the now path is the save/sync moment — warn here
                &mut emitted_types,
                &mut nq,
            );
        }

        // Cache what this file produced — but NEVER a file whose extraction
        // errored: errors must stay loud on every run, and a cached error
        // would read as clean forever.
        if total_errors == file_errors_start {
            cache.store(
                &relpath_str,
                &bytes_hash,
                &blob_hash,
                &nq[file_nq_start..],
                file_links,
            );
        }

        // Emission ran for its gates (resolution errors, warnings); when the
        // caller discards the text, drop this file's quads now — the buffer's
        // capacity is reused instead of accumulating the whole repo's worth.
        if !opts.build_nquads {
            nq.clear();
        }
    }
    cache.save();
    if opts.write_sidecars && cache_hits > 0 {
        eprintln!(
            "Walk: {} unchanged (cached), {} extracted",
            cache_hits,
            files.len() - cache_hits
        );
    }

    if opts.write_sidecars && total_links > 0 {
        eprintln!("Markdown links: {} from {} files", total_links, md_index.len());
    }

    // Commit-message [[wikilink]] scanning: RETIRED with the wikilink reader
    // (Rob-ruled 2026-08-06). git-lex reads no wikilinks anywhere; a
    // bracketed name in a commit subject is prose.

    (nq, total_errors)
}

/// Source documents whose SIDECARS are dirty in git — the on-disk sidecar
/// diverged from the committed pair (a revert after sync, a hand edit, a
/// hookless-clone commit). Those sources are forced through the full
/// pipeline so the sidecar write converges them; everything else may trust
/// the cache. One subprocess for the whole walk.
fn dirty_sidecar_sources(root: &std::path::Path) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(o) = std::process::Command::new("git")
        .args(["status", "--porcelain", "--", ".lex/extract/"])
        .current_dir(root)
        .output()
    else {
        return out;
    };
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        if line.len() < 4 {
            continue;
        }
        // "XY path" (rename rows: "XY old -> new" — the new side is live).
        let path = line[3..].split(" -> ").last().unwrap_or("").trim_matches('"');
        let Some(side) = path.strip_prefix(".lex/extract/") else { continue };
        for suffix in [".fm.spo", ".md.spo"] {
            if let Some(src) = side.strip_suffix(suffix) {
                out.insert(src.to_string());
            }
        }
    }
    out
}

/// Emit N-Quads for a single `.spo` line (`subject | predicate | object`).
///
/// This is the shared triple-emitter used by both `generate_frontmatter_nquads`
/// (the "now" graph builder) and the history-graph walker — so byte-identical
/// triples come out of both paths. Extracted from `generate_frontmatter_nquads`
/// as a behavior-preserving refactor; no logic changes.
///
/// Arguments:
/// - `line`: raw `.spo` line in `subject | predicate | object` form
/// - `subjects`: the file's two plane anchors (see `derive_file_subjects`);
///   each line lands on the plane its nature dictates — kit-declared facts
///   on the Thing node, links/markdown/fm facts on the File node. Kit lines
///   with no Thing anchor (missing id, non-anchor class) fall back to the
///   File node so nothing is dropped; the drift is surfaced elsewhere.
/// - `graph`: target graph IRI (with angle brackets)
/// - `relpath_str`: source document path relative to repo root (for warnings)
/// - `ctx`: the shared resolver context (path index + ontology tables),
///   built once per run — the old signature exploded seven of its fields
///   into positional params of identical types, so an argument swap
///   compiled clean and silently broke the emitter (review #15)
/// - `warn`: true on the live save/sync path (the moment the author can
///   act); false on the history walk, which revisits every commit — replay
///   must not repeat live to-dos (#73). Emission and the returned error
///   COUNT are identical either way; only the printing differs.
/// - `emitted_types`: in/out dedup set — the caller must zero this per doc
///   so each document emits its `rdf:type` assertions at most once
/// - `out`: the N-Quad buffer being appended to
/// Detect the documented identifier form `<namespace/Class/id>` written
/// WITHOUT its angle brackets (tr1p's 2026-08-18 finding, lUX field data:
/// 104 such values, every one resolving to an address nothing describes).
///
/// An unbracketed value falls into the rule-5 path lane (resolve.rs), which
/// glues the WRITING repo's namespace onto it — `soul/Note/x` in a soul repo
/// lands at `…/soul/soul/Note/x`. The wrong form produces no signal, which is
/// exactly why a careful author wrote it 104 times against a correct
/// rdfs:comment. This helper is DETECTION ONLY — resolution is untouched
/// (changing it is a Rob ruling, options B/C in the 2026_08_18 bug doc).
///
/// Fires when the first path segment case-insensitively names an installed
/// kit AND the value is not a tracked file path. The existence check is what
/// keeps legitimate File-plane references quiet: `Soul/Note/x.md` pointing at
/// a real file is the path lane's designed input; the same string pointing at
/// nothing gets the note (and the suggested Thing form is correct either way,
/// since the `Soul/` scaffold folder maps onto the namespace root).
/// Returns the suggested bracketed identifier (first segment lowercased,
/// `.md` dropped — a Thing IRI carries no extension).
pub(crate) fn bare_kit_reference_suggestion(
    val: &str,
    kit_namespaces: &HashMap<String, String>,
    path_index: &HashSet<String>,
) -> Option<String> {
    if val.starts_with('<') || val.contains("://") {
        return None;
    }
    let (first, rest) = val.split_once('/')?;
    if rest.is_empty() {
        return None;
    }
    let first_lower = first.to_lowercase();
    if !kit_namespaces.contains_key(&first_lower) {
        return None;
    }
    if path_index.contains(val) {
        return None;
    }
    let tail = rest.strip_suffix(".md").unwrap_or(rest);
    Some(format!("{first_lower}/{tail}"))
}

pub(crate) fn emit_spo_line_nquads(
    line: &str,
    subjects: &FileSubjects,
    graph: &str,
    relpath_str: &str,
    ctx: &ResolverContext,
    warn: bool,
    emitted_types: &mut HashSet<String>,
    out: &mut String,
) -> u32 {
    let ResolverContext {
        path_index,
        obj_props,
        prop_datatypes,
        declared_props,
        kit_namespaces,
        ref_ranges,
        prop_iris,
        deprecated_props,
        domain_open_props,
        ..
    } = ctx;
    let mut errors: u32 = 0;
    let parts: Vec<&str> = line.splitn(3, " | ").collect();
    if parts.len() != 3 {
        return 0;
    }
    let subject = parts[0];
    let predicate = parts[1];
    let object = parts[2];

    // Hard-fail: empty values produce empty literal triples — skip entirely
    if object.trim().is_empty() {
        return 0;
    }

    // Hard-fail: [[wikilinks]] in frontmatter values corrupt the graph.
    //
    // The advice here used to say "write the repo-relative path (e.g.
    // friend/selkie.md)" and that was actively wrong twice over (@m4rq,
    // 2026-08-27): for a Thing-valued property the path form is rejected by
    // the very next check, which tells you to use the angle-bracket address —
    // so the two messages contradicted each other. And `friend/` is not even a
    // real folder; class folders are capitalised and live under the kit's base
    // (Soul/Friend/). It steered people to a value that could not work.
    if predicate != "linksTo" && (object.contains("[[") || object.contains("]]")) {
        if warn {
            eprintln!(
                "error: {}: {} — wikilink syntax [[...]] is not allowed in frontmatter \
                 values. A frontmatter reference is a Thing ADDRESS: \
                 <namespace/Class/identifier>, e.g. <soul/Note/my-note>.",
                relpath_str, subject
            );
        }
        return 1;
    }

    if predicate == "linksTo" {
        // md:linksTo — ONE law (Rob-ruled 2026-08-08): the sidecar object is
        // a repo-ROOT-relative path, used as-is. The extractor already
        // resolved the markdown link against its document's folder when it
        // wrote the sidecar, so resolving again here was the double-join
        // that minted `Soul/Note/Soul/Note/b.md` File IRIs (review-HIGH).
        // A leading `/` (the retired 2026-07-28 repo-rooted form, present
        // in historical sidecars) names the same root-relative path — it is
        // stripped, not rejected, so all eras replay under the one law.
        // Deterministic at every commit whether or not the target exists
        // (forward links are legal; dangling ones warn at save). `.md` is
        // appended when the target has no extension.
        //
        // History note: this lane once dispatched two semantics on a
        // repo.yml `link_semantics` stamp (the wikilink-era migration
        // fence). The wikilink reader retired 2026-08-06; the fence itself
        // retired with the one-law ruling. Old-era bare targets that were
        // authored source-folder-relative re-derive as root-relative — the
        // accepted data change that bought one law for all history.
        match normalize_wikilink_path(object.trim_start_matches('/'), "") {
            Some(p) => {
                if graph == format!("<{}>", crate::git::graph_uri("now"))
                    && !path_index.contains(&p)
                {
                    eprintln!(
                        "warning: {relpath_str}: link target {p} does not exist (yet) — forward link, or fix the path"
                    );
                }
                // Prose links follow documents (Law 6): File → File, both
                // ends in the File-plane family. A dangling target still
                // derives its IRI — the dangle is true data about the text.
                out.push_str(&format!(
                    "{} <https://repolex.ai/ontology/git-lex/md/linksTo> <{}> {} .\n",
                    subjects.file_uri, crate::git::file_iri(&uri_encode_path(&p)), graph
                ));
            }
            None => {
                if warn {
                    eprintln!(
                        "error: {relpath_str}: link target {object:?} escapes the repo root — links stay inside the repo"
                    );
                }
                errors += 1;
            }
        }
    } else {
        // Check for three-segment dot notation: kit.class.property
        let segments: Vec<&str> = subject.splitn(3, '.').collect();

        if segments.len() == 3 {
            // New dot notation: kit.class.property
            let kit_name = segments[0];
            let class_seg = segments[1];
            let prop_seg = segments[2];

            // Emit rdf:type from class segment (once per class).
            //
            // B1 fix (Day 38): validate the class segment against the kit's
            // declared classes (the graph path is the one users query, so a
            // phantom type here is what makes `?m a soul:Memory` return 0).
            // `resolve_class_segment` returns the canonical class name on an
            // exact or case-only-mismatch hit (warning on the latter), and an
            // Err on a real typo. On the graph path we DON'T panic mid-sync —
            // we warn loudly and SKIP the type emission, so we never write the
            // phantom `a soul:memory`. The doc's properties still emit; only
            // the bad type is withheld until the frontmatter is fixed.
            let canonical_class: Option<String> =
                match crate::ontology::resolve_class_segment(kit_name, class_seg, relpath_str, warn) {
                    Ok(canonical) => Some(canonical),
                    Err(msg) => {
                        if warn {
                            eprintln!("warning: {relpath_str}: {msg}");
                        }
                        None
                    }
                };
            // The kit's namespace comes from its installed TTL declaration
            // (get_kit_namespaces_all_kits); the conventional pattern is only
            // the no-declaration fallback. This is what lets a kit's
            // namespace migrate with a TTL edit and no emitter change.
            let kit_ns = kit_namespaces
                .get(kit_name)
                .cloned()
                .unwrap_or_else(|| git_lex::conventional_kit_namespace(kit_name));

            // The line's subject: the Thing anchor when this line's class IS
            // the file's anchoring class; otherwise the File node (kit lines
            // with no Thing anchor — missing id, second class — fall back so
            // no fact is dropped; the type lands on the same subject, which
            // preserves today's queryability for the unmigrated corpus and
            // relocates Thing-ward per file as ids get authored).
            let line_subject: &str = match (&subjects.thing_uri, &subjects.thing_key, &canonical_class) {
                (Some(t), Some((ak, ac)), Some(c)) if ak == kit_name && ac == c => t,
                _ => &subjects.file_uri,
            };

            if let Some(canonical) = &canonical_class {
                let type_key = format!("{}.{}", kit_name, canonical);
                if emitted_types.insert(type_key) {
                    let type_uri = format!("<{}{}>", kit_ns, canonical);
                    out.push_str(&format!(
                        "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> {} {} .\n",
                        line_subject, type_uri, graph
                    ));
                }
            }

            // Property name passes through as-is (camelCase from ontology)
            // Kit+class-qualified lookup (Rob-ruled 2026-07-21): the tables
            // key "{kit}/{Class}/{prop}", so THIS kit's and class's own
            // declaration governs how the value is processed. The old
            // bare-name lookup let any installed kit's same-named property
            // rewrite the behavior (copia:source, a lineage ObjectProperty,
            // was comma-splitting soul:source prose citations).
            let lookup_key = canonical_class
                .as_ref()
                .map(|c| format!("{}/{}/{}", kit_name, c, prop_seg));

            // Domain-open lookup (#82): a property declared with no
            // rdfs:domain is on NO class's shape by construction, so the
            // class-qualified tables above can never hold it. Its key
            // carries no class — no domain means every class is in scope.
            let open_key = format!("{}/{}", kit_name, prop_seg);
            let domain_open = domain_open_props.get(&open_key);

            // The predicate IRI comes from what the ontology DECLARED, via the
            // generated shapes — not from gluing this document's kit namespace
            // onto the key's property segment (#104).
            //
            // The glue was correct only while every property a class carried
            // was declared by that class's own kit. `git-lex:Thing` ended that:
            // the ruled key `soul.Note.title` would have emitted
            // `.../ontology/soul/title` for a property declared at
            // `.../ontology/git-lex/title` — a fact on an IRI no ontology
            // declares, in every repo, breaking the one rule the rest of this
            // pipeline exists to enforce.
            //
            // Fallback to the old construction when no kit declares the key:
            // those are precisely the undeclared keys the save-time warning
            // already reports, and inventing a different IRI for them here
            // would change what unmigrated corpora replay to.
            let kit_predicate = lookup_key
                .as_ref()
                .and_then(|k| prop_iris.get(k))
                .map(|iri| format!("<{}>", iri))
                .or_else(|| domain_open.map(|d| format!("<{}>", d.iri)))
                .unwrap_or_else(|| format!("<{}{}>", kit_ns, prop_seg));

            // Check if this is an ObjectProperty (from ontology) → resolve as IRI.
            // Domain-open ObjectProperties (soul:relatedTo) qualify too: the
            // declaration says reference, the absent domain says on-any-class.
            if lookup_key.as_ref().is_some_and(|k| obj_props.contains(k))
                || domain_open.is_some_and(|d| d.is_object)
            {
                // Law 6 (identity model): a DECLARED RANGE makes the
                // authored value the TARGET'S ID — resolution is declared,
                // never guessed: id → the range class's id-space → one
                // Thing IRI. Deterministic at every commit, dangling or
                // not (existence is the save gate's job, not derivation's).
                //
                // Looked up by the DECLARED predicate IRI, not a key rebuilt
                // from the authoring kit — an inherited property authors
                // under the subclass's kit (`soul.Note.relatedToId`) while
                // its range is declared where the property lives (git-lex).
                // The `{kit}/{prop}` key missed exactly those (#82's
                // key-mismatch class; table re-keyed 2026-08-20).
                let range =
                    ref_ranges.get(kit_predicate.trim_start_matches('<').trim_end_matches('>'));
                // URL-aware split (review #26): a comma INSIDE a URL is part
                // of the value, not a list separator.
                let values = split_object_values(object);
                for val in &values {
                    let val = val.as_str();
                    if val.is_empty() { continue; }
                    // Range git-lex:Thing (Rob-ruled 2026-08-20): "any
                    // Thing, any class" — the bare-id derivation below
                    // cannot apply (there is no one id-space), so the value
                    // must be the full identifier form <namespace/Class/id>
                    // and ONLY that form. The angle-bracket lane.
                    if range.map(String::as_str) == Some(THING_CLASS_IRI) {
                        match resolve::resolve_thing_reference(val) {
                            Ok(target) => {
                                out.push_str(&format!(
                                    "{} {} {} {} .\n",
                                    line_subject, kit_predicate, target, graph
                                ));
                            }
                            Err(msg) => {
                                if warn {
                                    // Enrich with the concrete fix when the
                                    // value is recognizably the identifier
                                    // form minus brackets. NO tracked-file
                                    // veto here: under a Thing range even a
                                    // real path is invalid.
                                    let empty = HashSet::new();
                                    let hint = bare_kit_reference_suggestion(
                                        val,
                                        kit_namespaces,
                                        &empty,
                                    )
                                    .map(|s| format!(" Did you mean `<{s}>`?"))
                                    .unwrap_or_default();
                                    eprintln!(
                                        "error: {}: {} — {}{}",
                                        relpath_str, prop_seg, msg, hint
                                    );
                                }
                                errors += 1;
                            }
                        }
                        continue;
                    }
                    if let Some(range_iri) = range {
                        match thing_iri_from_range(range_iri, val) {
                            Some(target) => {
                                out.push_str(&format!(
                                    "{} {} {} {} .\n",
                                    line_subject, kit_predicate, target, graph
                                ));
                            }
                            None => {
                                if warn {
                                    eprintln!(
                                        "error: {}: {} — declared range `{}` is not a resolvable class IRI",
                                        relpath_str, prop_seg, range_iri
                                    );
                                }
                                errors += 1;
                            }
                        }
                        continue;
                    }
                    // No declared range: the legacy path/IRI resolver.
                    // But first, tr1p's 2026-08-18 finding: the documented
                    // identifier form minus its brackets is the attractive
                    // error, and the path lane swallows it silently. Note
                    // (not error) — resolution below is unchanged.
                    if warn {
                        if let Some(suggested) =
                            bare_kit_reference_suggestion(val, kit_namespaces, path_index)
                        {
                            eprintln!(
                                "note: {}: `{}` on `{}` has no angle brackets, so it \
                                 resolves as a repo-relative path to `{}` — an address \
                                 nothing in the graph describes. Did you mean `<{}>`?",
                                relpath_str,
                                val,
                                prop_seg,
                                crate::git::resource_uri(&uri_encode_path(val)),
                                suggested
                            );
                        }
                    }
                    match resolve::resolve_frontmatter_value(val) {
                        resolve::ResolveResult::Iri(uri) => {
                            out.push_str(&format!(
                                "{} {} {} {} .\n",
                                line_subject, kit_predicate, uri, graph
                            ));
                        }
                        resolve::ResolveResult::Unresolved(literal) => {
                            out.push_str(&format!(
                                "{} {} \"{}\" {} .\n",
                                line_subject, kit_predicate, nq_escape(&literal), graph
                            ));
                        }
                        resolve::ResolveResult::Rejected(msg) => {
                            if warn {
                                eprintln!(
                                    "error: {}: {} — {}",
                                    relpath_str, prop_seg, msg
                                );
                            }
                            errors += 1;
                        }
                    }
                }
            } else {
                // Used-on-undeclared-class is LOUD, never silent: the property
                // is declared somewhere in THIS kit but not on this class's
                // shape — it still emits (as a plain literal), and the drift
                // is surfaced so the shape or the frontmatter gets fixed.
                if let Some(key) = &lookup_key {
                    // Membership test against the DECLARED set — datatype-
                    // unconditional. Testing prop_datatypes here false-warned
                    // every xsd:string property in every kit (the shapes
                    // generator omits sh:datatype for strings): 412 bogus
                    // "not declared" warnings per save in W4R3Z alone
                    // (found 2026-08-01).
                    // The whole block below is teaching, no emission — one
                    // `warn` gate covers the wrong-class, deprecated-note,
                    // and does-not-exist branches together.
                    //
                    // Deprecated check FIRST, before the declared test (#83):
                    // whether a deprecated prop still sits in the generated
                    // shapes depends on whether its CLASS survived (a class
                    // deprecated whole keeps its shape + props; a bare
                    // appendix prop lands on no shape) — so gating the note
                    // behind not-declared made the Texture family silently
                    // invisible while writtenFrom whispered. Deprecated
                    // whispers regardless of shapes state: one note, the
                    // line still saves, history replays.
                    if warn {
                        if let Some(replaced) =
                            deprecated_props.get(&format!("{}/{}", kit_name, prop_seg))
                        {
                            let repl = replaced
                                .as_ref()
                                .map(|r| format!(" — replacement: `{}`", r))
                                .unwrap_or_default();
                            eprintln!(
                                "note: {}: the key `{}.{}.{}` is deprecated (the \
                                 `{}` ontology retired it{}). The line still saves \
                                 and history replays; don't use it in new writing — \
                                 migrate or delete when you next edit this file.",
                                relpath_str, kit_name, class_seg, prop_seg,
                                kit_name, repl
                            );
                        } else if !declared_props.contains(key)
                            && !obj_props.contains(key)
                            // #82: domain-open props are declared — telling
                            // the author otherwise was the false warning.
                            && domain_open.is_none()
                        {
                        let kit_scope = format!("{}/", kit_name);
                        let prop_tail = format!("/{}", prop_seg);
                        let class_for_msg = canonical_class.as_deref().unwrap_or(class_seg);
                        let owners: std::collections::BTreeSet<String> = obj_props
                            .iter()
                            .chain(declared_props.iter())
                            .filter(|k| k.starts_with(&kit_scope) && k.ends_with(&prop_tail))
                            .filter_map(|k| k.split('/').nth(1).map(str::to_string))
                            .collect();
                        if !owners.is_empty() {
                            // #85: owners that are deprecated classes get
                            // tagged — "exists on class Texture" read as
                            // Texture being live vocabulary, and it isn't.
                            let dep_classes =
                                crate::ontology::get_deprecated_classes(kit_name);
                            let owner_list = owners
                                .into_iter()
                                .map(|c| match dep_classes.get(&c) {
                                    Some(Some(succ)) => {
                                        format!("{c} (deprecated → {succ})")
                                    }
                                    Some(None) => format!("{c} (deprecated)"),
                                    None => c,
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            eprintln!(
                                "warning: {}: the key `{}.{}.{}` — `{}` exists in the \
                                 `{}` ontology, but on class {}, not on {}. Fix, pick \
                                 one: (a) this line belongs in a {} document — move it \
                                 there; (b) this key genuinely belongs on {} too — \
                                 keep the line and report it to the `{}` ontology \
                                 owner; (c) the line no longer matters — delete it. \
                                 Until fixed, the value saves as plain ungoverned data.",
                                relpath_str, kit_name, class_seg, prop_seg, prop_seg,
                                kit_name, owner_list, class_for_msg, owner_list,
                                class_for_msg, kit_name
                            );
                        } else if !kit_namespaces.contains_key(kit_name)
                            || obj_props.iter().chain(prop_datatypes.keys())
                                .any(|k| k.starts_with(&kit_scope))
                        {
                            // The kit-qualified prefix CLAIMS ontology
                            // vocabulary; a property the ontology has never
                            // heard of used to sail through silently — how
                            // months of junk keys (writtenFrom, soul.Note.
                            // title, …) accumulated invisibly (Rob-ruled
                            // 2026-07-29: warn at save). Bare keys (title:)
                            // stay free — the open fm: lane is one line up.
                            //
                            // did-you-mean: declared keys on THIS class that
                            // plausibly mean the same thing (the renamed-by-
                            // the-ontology case, e.g. kind → textureKind) —
                            // case-insensitive containment either way, 4+
                            // chars so single letters never match. (The
                            // deprecated-note lane moved ABOVE the declared
                            // test — #83 — so this branch only sees keys the
                            // ontology has truly never heard of.)
                            let class_prefix =
                                format!("{}/{}/", kit_name, class_for_msg);
                            let prop_lower = prop_seg.to_lowercase();
                            let mut candidates: Vec<String> = obj_props
                                .iter()
                                .chain(declared_props.iter())
                                .filter(|k| k.starts_with(&class_prefix))
                                .filter_map(|k| k.split('/').nth(2))
                                .filter(|cand| {
                                    let cl = cand.to_lowercase();
                                    cl != prop_lower
                                        && ((prop_lower.len() >= 4
                                            && cl.contains(&prop_lower))
                                            || (cl.len() >= 4
                                                && prop_lower.contains(&cl)))
                                })
                                // #85: never SUGGEST a deprecated key —
                                // did-you-mean is a destination menu for
                                // new writing.
                                .filter(|cand| {
                                    !deprecated_props.contains_key(&format!(
                                        "{}/{}",
                                        kit_name, cand
                                    ))
                                })
                                .map(str::to_string)
                                .collect();
                            candidates.sort();
                            candidates.dedup();
                            candidates.truncate(3);
                            let hint = if candidates.is_empty() {
                                format!(
                                    " (the `__{}.md` template in this class's folder \
                                     lists every declared key)",
                                    class_for_msg
                                )
                            } else {
                                format!(
                                    " — closest declared keys on {}: {}",
                                    class_for_msg,
                                    candidates.join(", ")
                                )
                            };
                            eprintln!(
                                "warning: {}: the key `{}.{}.{}` does not exist in \
                                 the `{}` ontology. Fix, pick one: (a) the ontology \
                                 may use a different name for this{} — if one means \
                                 the same thing, edit this line to use it; (b) no \
                                 current key fits and the information matters — keep \
                                 the line and report the missing key to the `{}` \
                                 ontology owner; (c) the line no longer matters — \
                                 delete it. Until fixed, the value saves as plain \
                                 ungoverned data.",
                                relpath_str, kit_name, class_seg, prop_seg, kit_name,
                                hint, kit_name
                            );
                            }
                        }
                    }
                }
                // DatatypeProperty: typed literal if ontology specifies a non-string range.
                // Domain-open datatype props carry their range in the ontology
                // record directly — the shapes-derived table can't see them (#82).
                if let Some(datatype) = lookup_key
                    .as_ref()
                    .and_then(|k| prop_datatypes.get(k))
                    .or_else(|| domain_open.and_then(|d| d.datatype.as_ref()))
                {
                    out.push_str(&format!(
                        "{} {} \"{}\"^^<{}> {} .\n",
                        line_subject, kit_predicate, nq_escape(object), datatype, graph
                    ));
                } else {
                    out.push_str(&format!(
                        "{} {} \"{}\" {} .\n",
                        line_subject, kit_predicate, nq_escape(object), graph
                    ));
                }
            }
        } else if subject == "md.externalLink" || subject == "md.unresolvedLink" {
            // #97 (B6): these are the markdown-link extractor's OWN lines,
            // not user frontmatter — and they were falling through to the
            // fm: lane, landing on `fm:md.externalLink`, an IRI nothing
            // declares, while the DECLARED md:externalLink sat with zero
            // instances. The ontology-first rule broken by our own code.
            // Sidecar lines replay from history too, so mapping at the
            // reader heals old lines on the next rebuild.
            let local = subject.strip_prefix("md.").unwrap_or(subject);
            out.push_str(&format!(
                "{} <https://repolex.ai/ontology/git-lex/md/{}> \"{}\" {} .\n",
                subjects.file_uri, local, nq_escape(object), graph
            ));
        } else {
            // Legacy or non-kit frontmatter (title, tags, etc.) — use fm: namespace
            let fm_predicate = format!("<https://repolex.ai/ontology/git-lex/fm/{}>", uri_encode_path(subject));

            if subject.ends_with("-link") || subject.ends_with("-links") {
                let values: Vec<&str> = if subject.ends_with("-links") {
                    object.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect()
                } else {
                    vec![object.trim()]
                };
                // Pre-dot-notation-era keys (no current kit emits them;
                // they exist only in old souls' history). Same law as
                // everywhere else: paths derive IRIs, anything else stays
                // a literal — no index lookup, no guessing.
                for val in values {
                    if val.is_empty() { continue; }
                    if val.contains('/') || val.ends_with(".md") {
                        out.push_str(&format!(
                            "{} {} <{}> {} .\n",
                            subjects.file_uri, fm_predicate, crate::git::file_iri(&uri_encode_path(val)), graph
                        ));
                    } else {
                        out.push_str(&format!(
                            "{} {} \"{}\" {} .\n",
                            subjects.file_uri, fm_predicate, nq_escape(val), graph
                        ));
                    }
                }
            } else {
                out.push_str(&format!(
                    "{} {} \"{}\" {} .\n",
                    subjects.file_uri, fm_predicate, nq_escape(object), graph
                ));
            }
        }
    }
    errors
}

/// Build the slug→path and path indexes used for `[[wikilink]]` resolution.
///
/// Repo-relative paths of every walked document — used to warn on
/// dangling links. (The slug index that used to live here — lowercase
/// stem → path, collision-prone by construction — died with the
/// bare-name resolution rule, Rob-ruled 2026-07-28.)
pub(crate) fn build_path_index(
    root: &std::path::Path,
    files: &[PathBuf],
) -> HashSet<String> {
    let mut path_index: HashSet<String> = HashSet::new();
    for f in files {
        if let Ok(rel) = f.strip_prefix(root) {
            path_index.insert(rel.to_string_lossy().to_string());
        }
    }
    path_index
}



/// Load every installed kit ontology TTL (`.lex/ontology/**/*.ttl`, EXCLUDING
/// `-shapes.ttl` SHACL files — Rob: shapes are validation, not vocabulary)
/// into the self-describing ontology graph
/// `<https://repolex.ai/git-lex/NamedGraph/repo-ontology>` of `store`.
///
/// Runs at INIT and KIT-UPDATE (Rob Day-50): the graph persists in the
/// store ("stays put") — sync does not rebuild it, query does not touch it.
/// One exception (#81): sync self-heals an EMPTY graph (fresh store after a
/// deliberate delete) by loading the TTLs already installed on disk, so a
/// store rebuild doesn't require a second kit-update.
/// The graph is cleared first so a kit-update fully refreshes the vocabulary.
/// LOUD but not fatal on a broken TTL. Returns the number of files loaded.
pub(crate) fn load_ontology_graph(store: &oxigraph::store::Store) -> usize {
    let Some(root) = find_git_root() else { return 0 };
    let ontology_graph = match oxigraph::model::NamedNode::new(graph_uri("repo-ontology")) {
        Ok(g) => g,
        Err(_) => return 0,
    };
    if let Err(e) = store.remove_named_graph(&ontology_graph) {
        eprintln!("warning: failed to clear the repo-ontology graph before reload: {} — retired vocabulary may linger", e);
    }
    fn walk_ttl(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let p = entry.path();
                if p.is_dir() {
                    walk_ttl(&p, out);
                } else if p.extension().is_some_and(|e| e == "ttl")
                    && !p.file_name().is_some_and(|n| n.to_string_lossy().ends_with("-shapes.ttl"))
                {
                    out.push(p);
                }
            }
        }
    }
    let mut ttls: Vec<PathBuf> = Vec::new();
    let ont_root = root.join(".lex").join("ontology");
    if ont_root.exists() {
        walk_ttl(&ont_root, &mut ttls);
    }
    ttls.sort();
    let mut loaded = 0usize;
    for ttl in &ttls {
        match fs::read(ttl) {
            Ok(bytes) => {
                let parser = oxigraph::io::RdfParser::from_format(oxigraph::io::RdfFormat::Turtle)
                    .with_default_graph(ontology_graph.clone());
                match store.load_from_reader(parser, std::io::Cursor::new(bytes)) {
                    Ok(_) => loaded += 1,
                    Err(e) => eprintln!("  warning: ontology load failed for {}: {}", ttl.display(), e),
                }
            }
            Err(e) => eprintln!("  warning: ontology read failed for {}: {}", ttl.display(), e),
        }
    }
    loaded
}

// ════════════════════════════════════════════════════════════════════════════
// Tests
// ════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    /// tr1p's 2026-08-18 finding, all five probe rows: the documented
    /// identifier form written WITHOUT brackets must be detected (it
    /// resolves to an address nothing describes), while the bracketed
    /// form and legitimate File-plane paths stay quiet.
    #[test]
    fn bare_kit_reference_detection_matches_probe_table() {
        let mut kits: HashMap<String, String> = HashMap::new();
        kits.insert("soul".into(), "https://repolex.ai/ontology/soul/".into());
        kits.insert("copia".into(), "https://repolex.ai/ontology/copia/".into());
        let mut paths: HashSet<String> = HashSet::new();
        paths.insert("Soul/Note/probe-delta.md".to_string());

        // ✅ bracketed identifier form: never reaches this helper unbracketed,
        // but if handed one, it must stay quiet.
        assert_eq!(
            bare_kit_reference_suggestion("<copia/Place/probe-alpha>", &kits, &paths),
            None
        );
        // ⚠️→quiet: capitalized scaffold path to a REAL file is the path
        // lane's designed input.
        assert_eq!(
            bare_kit_reference_suggestion("Soul/Note/probe-delta.md", &kits, &paths),
            None
        );
        // ❌ capitalized kit folder, no such file → note, .md dropped.
        assert_eq!(
            bare_kit_reference_suggestion("Copia/Place/probe-echo.md", &kits, &paths),
            Some("copia/Place/probe-echo".to_string())
        );
        // ❌ bare lowercase kit form (the 104-value attractor).
        assert_eq!(
            bare_kit_reference_suggestion("copia/Place/probe-bravo", &kits, &paths),
            Some("copia/Place/probe-bravo".to_string())
        );
        // ❌ the visible soul/soul/ doubling case.
        assert_eq!(
            bare_kit_reference_suggestion("soul/Note/probe-charlie", &kits, &paths),
            Some("soul/Note/probe-charlie".to_string())
        );
        // Quiet: ordinary repo paths, URLs, bare names, kit name alone.
        assert_eq!(
            bare_kit_reference_suggestion("Harness/Memory/foo.md", &kits, &paths),
            None
        );
        assert_eq!(
            bare_kit_reference_suggestion("https://repolex.ai/soul/Note/x", &kits, &paths),
            None
        );
        assert_eq!(bare_kit_reference_suggestion("probe", &kits, &paths), None);
        assert_eq!(bare_kit_reference_suggestion("soul/", &kits, &paths), None);
    }

    /// The emitter's predicate/class namespace comes from the kit's own TTL
    /// declaration (via the kit_namespaces map), NOT a hardcoded pattern —
    /// so a kit namespace migration is a TTL edit, no emitter change. A kit
    /// with no installed declaration falls back to the conventional pattern.
    #[test]
    fn emitter_follows_declared_kit_namespace() {
        let mut namespaces: HashMap<String, String> = HashMap::new();
        // The flip case: soul declares the migrated (kit-less) namespace.
        namespaces.insert("soul".into(), "https://repolex.ai/ontology/soul/".into());
        let ctx = ResolverContext {
            files: Vec::new(),
            path_index: HashSet::new(),
            obj_props: HashSet::new(),
            prop_datatypes: HashMap::new(),
            declared_props: HashSet::new(),
            kit_namespaces: namespaces,
            ref_ranges: HashMap::new(),
            prop_iris: HashMap::new(),
            deprecated_props: HashMap::new(),
            domain_open_props: HashMap::new(),
        };

        let mut types = HashSet::new();
        let mut out = String::new();
        let subjects = FileSubjects {
            file_uri: "<https://repolex.ai/git-lex/File/Journal/day-1.md>".into(),
            thing_uri: None,
            thing_key: None,
        };
        emit_spo_line_nquads(
            "soul.Journal.soulDay | hasValue | 55",
            &subjects,
            "<https://repolex.ai/git-lex/NamedGraph/now>",
            "Journal/day-1.md",
            &ctx, true,
            &mut types, &mut out,
        );
        assert!(
            out.contains("<https://repolex.ai/ontology/soul/soulDay>"),
            "predicate must follow the declared namespace, got: {out}"
        );
        assert!(!out.contains("/ontology/kit/soul/"), "retired pattern leaked: {out}");

        // No declaration for this kit → conventional fallback.
        let mut out2 = String::new();
        let subjects2 = FileSubjects {
            file_uri: "<https://repolex.ai/git-lex/File/friend/selkie.md>".into(),
            thing_uri: None,
            thing_key: None,
        };
        emit_spo_line_nquads(
            "copia.Being.beingName | hasValue | selkie",
            &subjects2,
            "<https://repolex.ai/git-lex/NamedGraph/now>",
            "friend/selkie.md",
            &ctx, true,
            &mut types, &mut out2,
        );
        assert!(
            out2.contains("<https://repolex.ai/ontology/copia/beingName>"),
            "undeclared kit must use the conventional (app-tier) fallback, got: {out2}"
        );
    }

    /// Range git-lex:Thing = the angle-bracket lane (Rob-ruled 2026-08-20),
    /// exercised through the INHERITED-property join that motivated the
    /// IRI-keyed range table: `soul.Note.relatedToId` authors under soul,
    /// the range is declared on the git-lex property — the lookup must go
    /// through the declared predicate IRI, and a concrete range must keep
    /// working through the same key.
    #[test]
    fn thing_range_is_the_angle_bracket_lane() {
        let mut obj_props = HashSet::new();
        obj_props.insert("soul/Note/relatedToId".to_string());
        obj_props.insert("copia/Look/lookBeingId".to_string());
        let mut prop_iris = HashMap::new();
        prop_iris.insert(
            "soul/Note/relatedToId".to_string(),
            "https://repolex.ai/ontology/git-lex/relatedToId".to_string(),
        );
        prop_iris.insert(
            "copia/Look/lookBeingId".to_string(),
            "https://repolex.ai/ontology/copia/lookBeingId".to_string(),
        );
        let mut ref_ranges = HashMap::new();
        ref_ranges.insert(
            "https://repolex.ai/ontology/git-lex/relatedToId".to_string(),
            THING_CLASS_IRI.to_string(),
        );
        ref_ranges.insert(
            "https://repolex.ai/ontology/copia/lookBeingId".to_string(),
            "https://repolex.ai/ontology/copia/Being".to_string(),
        );
        let ctx = ResolverContext {
            files: Vec::new(),
            path_index: HashSet::new(),
            obj_props,
            prop_datatypes: HashMap::new(),
            declared_props: HashSet::new(),
            kit_namespaces: HashMap::new(),
            ref_ranges,
            prop_iris,
            deprecated_props: HashMap::new(),
            domain_open_props: HashMap::new(),
        };
        let subjects = FileSubjects {
            file_uri: "<https://repolex.ai/git-lex/File/Soul/Note/a.md>".into(),
            thing_uri: None,
            thing_key: None,
        };

        // The identifier form resolves against the ONE root, on the
        // DECLARED (git-lex) predicate — through the soul-authored key.
        let mut types = HashSet::new();
        let mut out = String::new();
        let errs = emit_spo_line_nquads(
            "soul.Note.relatedToId | hasValue | <copia/Place/ocean-park-room>",
            &subjects,
            "<https://repolex.ai/git-lex/NamedGraph/now>",
            "Soul/Note/a.md",
            &ctx, false,
            &mut types, &mut out,
        );
        assert_eq!(errs, 0);
        assert!(
            out.contains(
                "<https://repolex.ai/ontology/git-lex/relatedToId> \
                 <https://repolex.ai/copia/Place/ocean-park-room>"
            ),
            "bracketed identifier must resolve under the Thing range: {out}"
        );

        // Everything else REJECTS under the Thing range: the bare form,
        // a real-looking file path, and a URL. No fact emitted for any.
        for bad in [
            "copia/Place/ocean-park-room",
            "Soul/Note/other.md",
            "https://example.com/thing",
        ] {
            let mut out_bad = String::new();
            let errs = emit_spo_line_nquads(
                &format!("soul.Note.relatedToId | hasValue | {bad}"),
                &subjects,
                "<https://repolex.ai/git-lex/NamedGraph/now>",
                "Soul/Note/a.md",
                &ctx, false,
                &mut types, &mut out_bad,
            );
            assert_eq!(errs, 1, "`{bad}` must reject under range Thing");
            assert!(
                !out_bad.contains("relatedToId"),
                "`{bad}` must emit NO fact, got: {out_bad}"
            );
        }

        // A concrete range still resolves the bare id through the same
        // IRI-keyed table (Law 6 unchanged).
        let mut out_fk = String::new();
        let errs = emit_spo_line_nquads(
            "copia.Look.lookBeingId | hasValue | lux",
            &subjects,
            "<https://repolex.ai/git-lex/NamedGraph/now>",
            "looks/l1.md",
            &ctx, false,
            &mut types, &mut out_fk,
        );
        assert_eq!(errs, 0);
        assert!(
            out_fk.contains("<https://repolex.ai/copia/Being/lux>"),
            "concrete range must keep Law-6 id resolution: {out_fk}"
        );
    }

    /// #82: a domain-open property (no rdfs:domain — soul:relatedTo) is on
    /// no class's shape by construction, so the shapes-derived tables all
    /// miss it. It must still behave as DECLARED: predicate from its own
    /// declared IRI, ObjectProperty values resolved as references, non-string
    /// ranges emitted as typed literals — and never the "does not exist"
    /// false warning (the gate consults the same map these assertions do).
    #[test]
    fn domain_open_property_behaves_as_declared() {
        let mut open: HashMap<String, crate::ontology::DomainOpenProp> = HashMap::new();
        open.insert("soul/relatedTo".into(), crate::ontology::DomainOpenProp {
            is_object: true,
            datatype: None,
            iri: "https://repolex.ai/ontology/soul/relatedTo".into(),
        });
        open.insert("soul/openDate".into(), crate::ontology::DomainOpenProp {
            is_object: false,
            datatype: Some("http://www.w3.org/2001/XMLSchema#date".into()),
            iri: "https://repolex.ai/ontology/soul/openDate".into(),
        });
        let ctx = ResolverContext {
            files: Vec::new(),
            path_index: HashSet::new(),
            obj_props: HashSet::new(),
            prop_datatypes: HashMap::new(),
            declared_props: HashSet::new(),
            kit_namespaces: HashMap::new(),
            ref_ranges: HashMap::new(),
            prop_iris: HashMap::new(),
            deprecated_props: HashMap::new(),
            domain_open_props: open,
        };
        let subjects = FileSubjects {
            file_uri: "<https://repolex.ai/git-lex/File/Soul/Note/a.md>".into(),
            thing_uri: None,
            thing_key: None,
        };

        // ObjectProperty lane: the bracketed identifier resolves to a Thing
        // IRI, on the property's DECLARED predicate.
        let mut types = HashSet::new();
        let mut out = String::new();
        emit_spo_line_nquads(
            "soul.Note.relatedTo | hasValue | <copia/Texture/deep-water>",
            &subjects,
            "<https://repolex.ai/git-lex/NamedGraph/now>",
            "Soul/Note/a.md",
            &ctx, false,
            &mut types, &mut out,
        );
        assert!(
            out.contains(
                "<https://repolex.ai/ontology/soul/relatedTo> \
                 <https://repolex.ai/copia/Texture/deep-water>"
            ),
            "domain-open reference must resolve as an IRI on its declared predicate: {out}"
        );

        // Datatype lane: the declared non-string range types the literal.
        let mut out2 = String::new();
        emit_spo_line_nquads(
            "soul.Note.openDate | hasValue | 2026-08-12",
            &subjects,
            "<https://repolex.ai/git-lex/NamedGraph/now>",
            "Soul/Note/a.md",
            &ctx, false,
            &mut types, &mut out2,
        );
        assert!(
            out2.contains("\"2026-08-12\"^^<http://www.w3.org/2001/XMLSchema#date>"),
            "domain-open datatype must emit typed: {out2}"
        );
    }

    /// Law-6 id→IRI derivation: range class IRI + bare target id → the
    /// Thing IRI in the range class's own application id-space. Pinned to
    /// tr1p's staged copia shapes (train/re-anchor c4b325f).
    #[test]
    fn thing_iri_from_range_derives_target_id_space() {
        assert_eq!(
            thing_iri_from_range("https://repolex.ai/ontology/copia/Being", "lux").as_deref(),
            Some("<https://repolex.ai/copia/Being/lux>")
        );
        assert_eq!(
            thing_iri_from_range("https://repolex.ai/ontology/copia/Moment", "abc123").as_deref(),
            Some("<https://repolex.ai/copia/Moment/abc123>")
        );
        // ids get IRI-encoded; malformed range yields None, never a panic.
        assert_eq!(
            thing_iri_from_range("https://repolex.ai/ontology/soul/Being", "a b").as_deref(),
            Some("<https://repolex.ai/soul/Being/a%20b>")
        );
        assert!(thing_iri_from_range("no-slashes", "x").is_none());
    }

    /// The a-box base derives from the kit's ontology namespace by dropping
    /// the /ontology/ tier — the universal instance law
    /// (`<application>/<Class>/<id>`), never a second hardcoded pattern.
    #[test]
    fn app_base_drops_the_ontology_tier() {
        assert_eq!(
            app_base_from_kit_ns("https://repolex.ai/ontology/soul/"),
            "https://repolex.ai/soul/"
        );
        assert_eq!(
            app_base_from_kit_ns("https://repolex.ai/ontology/git-lex/"),
            "https://repolex.ai/git-lex/"
        );
        // A namespace without the tier passes through untouched.
        assert_eq!(
            app_base_from_kit_ns("https://example.org/vocab/"),
            "https://example.org/vocab/"
        );
    }

    /// A file with no kit-classed lines anchors NO Thing: File node only —
    /// the bare-markdown tier. The File IRI is the path verbatim under the
    /// git-lex File family (no scaffold stripping).
    #[test]
    fn derive_subjects_no_kit_lines_is_file_only() {
        let lines = vec![
            "README.md | linksTo | docs/intro.md".to_string(),
            "title | hasValue | hello".to_string(),
        ];
        let declared = HashSet::new();
        let obj_props = HashSet::new();
        let namespaces = HashMap::new();
        let s = derive_file_subjects(
            &lines, "README.md", &declared, &obj_props, &namespaces, false,
        );
        assert_eq!(s.file_uri, "<https://repolex.ai/git-lex/File/README.md>");
        assert!(s.thing_uri.is_none());
        assert!(s.thing_key.is_none());
    }

    /// The universal-id value form (Rob-ruled 2026-08-21): full address in
    /// brackets, three segments minimum, identifier may carry slashes,
    /// URLs and bare stems reject with teaching.
    #[test]
    fn universal_id_parses_the_full_address_form() {
        let (ns, class, ident, inner) =
            parse_universal_id("<soul/Note/20260821-abc123>").unwrap();
        assert_eq!(
            (ns.as_str(), class.as_str(), ident.as_str(), inner.as_str()),
            ("soul", "Note", "20260821-abc123", "soul/Note/20260821-abc123")
        );
        // Identifier with its own slashes: address is what it is.
        let (_, _, ident, _) = parse_universal_id("<copia/Place/rooms/attic>").unwrap();
        assert_eq!(ident, "rooms/attic");
        for bad in ["20260821-abc123", "<soul/Note>", "<https://repolex.ai/soul/Note/x>", "<>", "<//x>"] {
            assert!(parse_universal_id(bad).is_err(), "`{bad}` must reject");
        }
    }

    /// Anchor priority (Rob-ruled 2026-08-21): the universal `.id` is the
    /// identity authority — tried first, wins over the per-class field
    /// when both are present (the transition window's normal state); the
    /// per-class convention still anchors alone (unmigrated corpus); a
    /// malformed `.id` falls back instead of costing the file its
    /// existing identity.
    #[test]
    fn universal_id_anchors_first_convention_is_fallback() {
        let mut declared = HashSet::new();
        declared.insert("soul/Note/noteId".to_string());
        let obj_props = HashSet::new();
        let namespaces = HashMap::new();

        // Both present, deliberately different stems: the .id wins.
        let lines = vec![
            "soul.Note.noteId | hasValue | old-stem".to_string(),
            "soul.Note.id | hasValue | <soul/Note/20260821-new-stem>".to_string(),
        ];
        let s = derive_file_subjects(&lines, "Soul/Note/x.md", &declared, &obj_props, &namespaces, false);
        assert_eq!(
            s.thing_uri.as_deref(),
            Some("<https://repolex.ai/soul/Note/20260821-new-stem>")
        );
        assert_eq!(s.thing_key, Some(("soul".to_string(), "Note".to_string())));

        // Universal only — the end state after the one-swoop removal.
        let lines = vec![
            "soul.Note.id | hasValue | <soul/Note/solo>".to_string(),
        ];
        let s = derive_file_subjects(&lines, "Soul/Note/y.md", &declared, &obj_props, &namespaces, false);
        assert_eq!(s.thing_uri.as_deref(), Some("<https://repolex.ai/soul/Note/solo>"));

        // Malformed universal id: falls back to the per-class anchor.
        let lines = vec![
            "soul.Note.id | hasValue | no-brackets-here".to_string(),
            "soul.Note.noteId | hasValue | fallback-stem".to_string(),
        ];
        let s = derive_file_subjects(&lines, "Soul/Note/z.md", &declared, &obj_props, &namespaces, false);
        let thing = s.thing_uri.expect("convention lane must still anchor");
        assert!(thing.contains("fallback-stem"), "{thing}");

        // The id is the authority even against its own key prefix: an id
        // declaring another namespace/class carries the Thing there.
        let lines = vec![
            "soul.Note.id | hasValue | <copia/Place/parked-elsewhere>".to_string(),
        ];
        let s = derive_file_subjects(&lines, "Soul/Note/w.md", &declared, &obj_props, &namespaces, false);
        assert_eq!(
            s.thing_uri.as_deref(),
            Some("<https://repolex.ai/copia/Place/parked-elsewhere>")
        );
        assert_eq!(s.thing_key, Some(("copia".to_string(), "Place".to_string())));
    }

    /// Review #26: a comma INSIDE a URL is part of the value; a comma that
    /// starts a new URL splits. Non-URL values keep the plain comma split.
    #[test]
    fn split_object_values_is_url_aware() {
        // Single URL with a comma in it — ONE value, intact.
        assert_eq!(
            split_object_values("https://en.wikipedia.org/wiki/Washington,_D.C."),
            vec!["https://en.wikipedia.org/wiki/Washington,_D.C."]
        );
        // A list of URLs still splits at the item boundaries.
        assert_eq!(
            split_object_values("https://a.com/x, https://b.com/y"),
            vec!["https://a.com/x", "https://b.com/y"]
        );
        // A list of URLs where one ITEM contains a comma: the comma that
        // does not start a new URL stays inside its item.
        assert_eq!(
            split_object_values("https://a.com/w,x, https://b.com/y"),
            vec!["https://a.com/w,x", "https://b.com/y"]
        );
        // Plain (non-URL) values: unchanged comma-split semantics.
        assert_eq!(
            split_object_values("friend/a.md, friend/b.md"),
            vec!["friend/a.md", "friend/b.md"]
        );
        assert_eq!(split_object_values("  , ,"), Vec::<String>::new());
    }

    #[test]
    fn uri_encode_url_preserves_existing_escapes() {
        assert_eq!(
            uri_encode_url("https://en.wikipedia.org/wiki/Caf%C3%A9"),
            "https://en.wikipedia.org/wiki/Caf%C3%A9"
        );
        // Stray % (no two hex digits after) still encodes.
        assert_eq!(uri_encode_url("https://x.com/100%"), "https://x.com/100%25");
        // Non-escape chars keep the path-encoder table.
        assert_eq!(uri_encode_url("https://x.com/a b"), "https://x.com/a%20b");
    }

    /// '%', '"', and '\' are legal in filenames but illegal (or
    /// escape-significant) in IRIs — unencoded they made a file like
    /// `100%.md` panic every sync (deep-review HIGH #3). '%' must be
    /// encoded FIRST so already-encoded output is never double-mangled.
    #[test]
    fn uri_encode_path_covers_iri_breaking_chars() {
        assert_eq!(uri_encode_path("100%.md"), "100%25.md");
        assert_eq!(uri_encode_path("he\"llo.md"), "he%22llo.md");
        assert_eq!(uri_encode_path("back\\slash.md"), "back%5Cslash.md");
        assert_eq!(uri_encode_path("a b.md"), "a%20b.md");
        // untouched safe chars
        assert_eq!(uri_encode_path("Soul/Memory/plain-file.md"), "Soul/Memory/plain-file.md");
    }

    /// Every encoded output must parse as an IRI path segment: round-trip
    /// through oxigraph's NamedNode to prove the class of panic is closed.
    #[test]
    fn uri_encode_path_output_is_valid_iri() {
        for name in ["100%.md", "he\"llo.md", "back\\slash.md", "sp ace.md", "a{b}|c^d`e[f].md"] {
            let iri = format!("https://repolex.ai/soul/{}", uri_encode_path(name));
            oxigraph::model::NamedNode::new(&iri)
                .unwrap_or_else(|e| panic!("{} → {} not a valid IRI: {}", name, iri, e));
        }
    }
}


#[cfg(test)]
mod sidecar_write_tests {
    use super::write_sidecar_loud;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gitlex-sidecar-write-{}-{}",
            std::process::id(),
            name
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Missing file → written. The skip must never swallow a first write.
    #[test]
    fn writes_when_absent() {
        let d = tmp("absent");
        let p = d.join("a.fm.spo");
        write_sidecar_loud(&p, "one | hasValue | 1\n");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one | hasValue | 1\n");
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// Differing content → overwritten. This is the case that MUST still
    /// write: a sidecar that silently keeps stale bytes is a permanent
    /// history gap (the committed diff is the one graph's only event source).
    #[test]
    fn overwrites_when_content_differs() {
        let d = tmp("differs");
        let p = d.join("a.fm.spo");
        std::fs::write(&p, "old | hasValue | 1\n").unwrap();
        write_sidecar_loud(&p, "new | hasValue | 2\n");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new | hasValue | 2\n");
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// Identical content → the file is not touched. Proved by mtime: the
    /// end state is the same either way, so content alone cannot show that
    /// the write was skipped.
    #[test]
    fn skips_the_write_when_content_is_identical() {
        let d = tmp("identical");
        let p = d.join("a.fm.spo");
        let body = "same | hasValue | 1\n";
        write_sidecar_loud(&p, body);
        let before = std::fs::metadata(&p).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_sidecar_loud(&p, body);
        let after = std::fs::metadata(&p).unwrap().modified().unwrap();
        assert_eq!(before, after, "identical content must not re-write the file");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), body);
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// Parent directory missing → still created. The early return must not
    /// jump over create_dir_all for a genuinely new sidecar tree.
    #[test]
    fn creates_missing_parent_dirs() {
        let d = tmp("nested");
        let p = d.join("deep/deeper/a.fm.spo");
        write_sidecar_loud(&p, "x | hasValue | 1\n");
        assert!(p.exists());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
