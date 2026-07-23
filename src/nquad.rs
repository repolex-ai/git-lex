//! N-Quad / N-Triple encoding and generation.
//!
//! Low-level escapers (`nq_escape`, `uri_encode_path`) plus the N-Quad
//! generators that produce git-lex's "now" view of the world:
//!
//! - `generate_frontmatter_nquads` — the "now" graph: current-state frontmatter
//!   extraction, body wikilinks.
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
use std::process::Command;

use git_lex::find_git_root;

use crate::git::{graph_uri, resource_uri};
use crate::extraction::{flatten_yaml, normalize_wikilink_path,
                        resolve_slug_to_uri};
use crate::ontology::{get_object_properties_all_kits, get_property_datatypes_all_kits};
use crate::resolve;

/// Write a sidecar (or its `.meta`) file, failing the process on error.
/// A silently unwritten sidecar is a permanent history gap: the committed
/// sidecar diff is the one graph's ONLY event source, so "couldn't write,
/// carried on" means facts that never happened as far as history is
/// concerned (review finding A8).
pub(crate) fn write_sidecar_loud(path: &std::path::Path, content: &str) {
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

/// Percent-encode a path for use in URIs (spaces, special chars, non-ASCII).
pub(crate) fn uri_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
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
    out
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

/// Extract frontmatter and body wikilinks from all .md/.txt files in the
/// repo into the "now" graph. Also writes `.fm.spo` sidecars and scans
/// commit messages for wikilinks.
pub(crate) fn generate_frontmatter_nquads() -> (String, u32) {
    let root = match find_git_root() {
        Some(r) => r,
        None => return (String::new(), 0),
    };

    // The "now" graph is the canonical view of current state — extracted
    // frontmatter, body wikilinks/mentions, and any triples derived from the
    // working tree as it exists right now. Contrasts with sync/<sha>/ graphs,
    // which hold historical snapshots at past commits. Previously named
    // "frontmatter" but that was misleading: this graph holds more than the
    // fm: namespace (wikilinks, mentions, etc are also here).
    let graph = format!("<{}>", graph_uri("now"));
    let mut nq = String::new();
    let mut total_errors: u32 = 0;

    // Build ObjectProperty + datatype lookup across ALL installed kits (base
    // + domain + optionals). Frontmatter triples from any kit's properties
    // need correct IRI-vs-literal classification and typed-literal tags
    // (xsd:integer, xsd:date, etc.) regardless of which kit declared the
    // property. The previous single-kit lookup hid optional-kit datatypes —
    // e.g. copia:firstVisited (xsd:date) emitted as untyped string.
    let obj_props = get_object_properties_all_kits();
    let prop_datatypes = get_property_datatypes_all_kits();

    // Open git repo for blob hash lookups
    let repo = git2::Repository::discover(".").ok();

    // Walk all .md files in the repo (skip .lex/ and .git/)
    fn walk_md(dir: &std::path::Path, files: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                if path.is_dir() {
                    walk_md(&path, files);
                } else if name.ends_with(".md") || name.ends_with(".txt") {
                    files.push(path);
                }
            }
        }
    }

    let mut files = Vec::new();
    walk_md(&root, &mut files);

    let (slug_index, path_index) = build_slug_path_indexes(&root, &files);

    // entity_classes was used by the old range-aware resolver, which has been
    // replaced by src/resolve.rs. The range-check approach (matching class IRIs
    // across kits) had a cross-kit identity bug (squad:Agent ≠ soul:Agent) and
    // is deferred until cross-kit class equivalence is designed. For now the
    // resolver trusts bare-slug + full-IRI resolution without range filtering.

    // Ensure extract dir exists
    let extract_dir = root.join(".lex").join("extract");
    fs::create_dir_all(&extract_dir).ok();

    // Regex pattern for [[wikilinks]].
    // @mentions removed — they were blog inheritance with no job in a system
    // where everything is a document. Canonical direction: [[Class/id]].
    let wikilink_re = regex::Regex::new(r"\[\[([^\]]+)\]\]").unwrap();

    for filepath in &files {
        let content = match fs::read_to_string(filepath) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let relpath = filepath.strip_prefix(&root).unwrap_or(filepath);
        let relpath_str = relpath.to_string_lossy().to_string();

        // Get blob hash from git index (staging area)
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

        // --- Frontmatter extraction ---
        let mut spo_lines = Vec::new();
        let body_text;

        if content.starts_with("---\n") || content.starts_with("---\r\n") {
            let rest = &content[4..];
            if let Some(end) = rest.find("\n---") {
                let yaml_str = &rest[..end];
                match serde_yaml::from_str::<HashMap<String, serde_yaml::Value>>(yaml_str) {
                    Ok(yaml) => {
                        for (key, value) in &yaml {
                            flatten_yaml(key, value, &mut spo_lines);
                        }
                    }
                    Err(e) => {
                        eprintln!("error: {}: malformed YAML frontmatter: {}", relpath_str, e);
                        total_errors += 1;
                    }
                }
                // Body is everything after the closing ---
                let after_fm = &rest[end + 4..]; // skip "\n---"
                body_text = after_fm.to_string();
            } else {
                body_text = content.clone();
            }
        } else {
            body_text = content.clone();
        }

        // --- [[wikilink]] extraction ---
        let mut links_seen = HashSet::new();
        for cap in wikilink_re.captures_iter(&body_text) {
            let link = cap[1].to_string();
            if links_seen.insert(link.clone()) {
                spo_lines.push(format!("{} | linksTo | {}", relpath_str, link));
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
        if !spo_lines.is_empty() {
            let spo_content = spo_lines.join("\n") + "\n";
            write_sidecar_loud(&spo_path, &spo_content);
        } else if spo_path.exists() {
            remove_sidecar_loud(&spo_path);
        }

        // --- Generate N-Quads for oxigraph (now graph) ---
        // IRI scheme: https://repolex.ai/soul/{path} (Soul/ root maps onto
        // the namespace root; no soul identity in the subject — Day-50).
        // The IRI mirrors the file path verbatim — no folder capitalization,
        // no folder→class derivation. Classes come from the ontology and from
        // explicit dot-notation in frontmatter (kit.class.property), never
        // from folder name guessing. Honors "ontology is the single source
        // of truth" — sync stops inventing types the schema does not declare.
        //
        // No-kit repos get `git-lex:Document` only (plus the git layer).
        // Kit repos get classes their ontology declares, via frontmatter.
        //
        // File location is git:path — a git-lex-authored synthetic fact from
        // the on-disk path, NOT a user frontmatter key; fm: carries ONLY what
        // the user wrote (the fm firewall).
        let doc_uri = format!("<{}>", resource_uri(&uri_encode_path(&relpath_str)));

        nq.push_str(&format!(
            "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/Document> {} .\n",
            doc_uri, graph
        ));
        nq.push_str(&format!(
            "{} <https://repolex.ai/ontology/git-lex/git/path> \"{}\" {} .\n",
            doc_uri, nq_escape(&relpath_str), graph
        ));
        nq.push_str(&format!(
            "{} <https://repolex.ai/ontology/git-lex/git/blobHash> \"{}\" {} .\n",
            doc_uri, blob_hash, graph
        ));

        // Track which kit types we've seen for rdf:type emission (dedup)
        let mut emitted_types: HashSet<String> = HashSet::new();

        for line in &spo_lines {
            total_errors += emit_spo_line_nquads(
                line,
                &doc_uri,
                &graph,
                &relpath_str,
                &slug_index,
                &path_index,
                &obj_props,
                &prop_datatypes,
                &mut emitted_types,
                &mut nq,
            );
        }
    }

    // --- Scan commit messages for [[wikilinks]] ---
    let commit_output = Command::new("git")
        .args(["log", "--all", "--format=%H%x00%s"])
        .output();
    if let Ok(o) = commit_output {
        if o.status.success() {
            let stdout = String::from_utf8_lossy(&o.stdout);
            for line in stdout.lines() {
                let parts: Vec<&str> = line.splitn(2, '\x00').collect();
                if parts.len() < 2 { continue; }
                let (sha, message) = (parts[0], parts[1]);
                let commit_uri = format!("<{}>", crate::git2_nquads::git2_uri(&format!("Commit/{}", sha)));

                for cap in wikilink_re.captures_iter(message) {
                    let link = &cap[1];
                    nq.push_str(&format!(
                        "{} <https://repolex.ai/ontology/git-lex/md/linksTo> \"{}\" {} .\n",
                        commit_uri, nq_escape(link), graph
                    ));
                }
            }
        }
    }

    (nq, total_errors)
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
/// - `doc_uri`: IRI of the containing document (with angle brackets)
/// - `graph`: target graph IRI (with angle brackets)
/// - `relpath_str`: source document path relative to repo root (for warnings)
/// - `slug_index` / `path_index`: doc lookup tables
/// - `obj_props` / `prop_datatypes`: ontology-derived property metadata
/// - `emitted_types`: in/out dedup set — the caller must zero this per doc
///   so each document emits its `rdf:type` assertions at most once
/// - `out`: the N-Quad buffer being appended to
pub(crate) fn emit_spo_line_nquads(
    line: &str,
    doc_uri: &str,
    graph: &str,
    relpath_str: &str,
    slug_index: &HashMap<String, String>,
    path_index: &HashSet<String>,
    obj_props: &HashSet<String>,
    prop_datatypes: &HashMap<String, String>,
    emitted_types: &mut HashSet<String>,
    out: &mut String,
) -> u32 {
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

    // Hard-fail: [[wikilinks]] in frontmatter values corrupt the graph
    if predicate != "linksTo" && (object.contains("[[") || object.contains("]]")) {
        eprintln!(
            "error: {}: {} — wikilink syntax [[...]] is not allowed in frontmatter values. Write the bare slug instead.",
            relpath_str, subject
        );
        return 1;
    }

    if predicate == "linksTo" {
        // [[wikilink]] → md:linksTo (resolved) or literal fallback (broken).
        //
        // Three resolution strategies, tried in order:
        //   1. Path-style — if the target contains `/`, treat it as a path
        //      relative to the source file's directory. Normalize `..`, look
        //      up against the path index.
        //   2. Trailing-segment fallback — if path resolution fails, take the
        //      last segment of the target and try the bare-wikilink path.
        //   3. Bare wikilink — slugify (lowercase, hyphens, alnum-only) and
        //      look up in the slug index keyed by file stem.
        //
        // Falls through to a literal linksTo only when all three miss.
        let source_dir = std::path::Path::new(relpath_str)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        let resolved_path: Option<String> = if object.contains('/') {
            normalize_wikilink_path(object, &source_dir)
                .filter(|p| path_index.contains(p))
        } else {
            None
        };

        let link_uri: Option<String> = if let Some(p) = resolved_path {
            Some(format!("<{}>", resource_uri(&uri_encode_path(&p))))
        } else {
            // Strategy 2: trailing-segment fallback if the target had a `/`.
            let candidate = if let Some(idx) = object.rfind('/') {
                &object[idx + 1..]
            } else {
                object
            };
            // Strip trailing .md if present so the stem matches the index.
            let stem = candidate.strip_suffix(".md").unwrap_or(candidate);
            // Strategy 3: slugify and look up in slug_index.
            let link_slug = stem.to_lowercase()
                .replace(' ', "-")
                .replace(|c: char| !c.is_alphanumeric() && c != '-', "");
            if !link_slug.is_empty() && slug_index.contains_key(&link_slug) {
                Some(resolve_slug_to_uri(&link_slug, slug_index))
            } else {
                None
            }
        };

        if let Some(uri) = link_uri {
            out.push_str(&format!(
                "{} <https://repolex.ai/ontology/git-lex/md/linksTo> {} {} .\n",
                doc_uri, uri, graph
            ));
        } else {
            // Unresolved wikilink → flat literal on md:linksTo.
            out.push_str(&format!(
                "{} <https://repolex.ai/ontology/git-lex/md/linksTo> \"{}\" {} .\n",
                doc_uri, nq_escape(object), graph
            ));
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
                match crate::ontology::resolve_class_segment(kit_name, class_seg) {
                    Ok(canonical) => Some(canonical),
                    Err(msg) => {
                        eprintln!("warning: {msg} (skipping type emission for this doc)");
                        None
                    }
                };
            if let Some(canonical) = &canonical_class {
                let type_key = format!("{}.{}", kit_name, canonical);
                if emitted_types.insert(type_key) {
                    let type_uri = format!(
                        "<https://repolex.ai/ontology/kit/{}/{}>", kit_name, canonical);
                    out.push_str(&format!(
                        "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> {} {} .\n",
                        doc_uri, type_uri, graph
                    ));
                }
            }

            // Property name passes through as-is (camelCase from ontology)
            let kit_predicate = format!("<https://repolex.ai/ontology/kit/{}/{}>", kit_name, prop_seg);

            // Kit+class-qualified lookup (Rob-ruled 2026-07-21): the tables
            // key "{kit}/{Class}/{prop}", so THIS kit's and class's own
            // declaration governs how the value is processed. The old
            // bare-name lookup let any installed kit's same-named property
            // rewrite the behavior (copia:source, a lineage ObjectProperty,
            // was comma-splitting soul:source prose citations).
            let lookup_key = canonical_class
                .as_ref()
                .map(|c| format!("{}/{}/{}", kit_name, c, prop_seg));

            // Check if this is an ObjectProperty (from ontology) → resolve as IRI
            if lookup_key.as_ref().is_some_and(|k| obj_props.contains(k)) {
                // ObjectProperty: split on commas, resolve each value
                // via the canonical resolver (see src/resolve.rs).
                let values: Vec<&str> = object.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
                for val in values {
                    if val.is_empty() { continue; }
                    match resolve::resolve_frontmatter_value(val, slug_index) {
                        resolve::ResolveResult::Iri(uri) => {
                            out.push_str(&format!(
                                "{} {} {} {} .\n",
                                doc_uri, kit_predicate, uri, graph
                            ));
                        }
                        resolve::ResolveResult::Unresolved(literal) => {
                            out.push_str(&format!(
                                "{} {} \"{}\" {} .\n",
                                doc_uri, kit_predicate, nq_escape(&literal), graph
                            ));
                        }
                        resolve::ResolveResult::Rejected(msg) => {
                            eprintln!(
                                "error: {}: {} — {}",
                                relpath_str, prop_seg, msg
                            );
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
                    if !prop_datatypes.contains_key(key) {
                        let kit_scope = format!("{}/", kit_name);
                        let prop_tail = format!("/{}", prop_seg);
                        let declared_elsewhere_in_kit = obj_props
                            .iter()
                            .chain(prop_datatypes.keys())
                            .any(|k| k.starts_with(&kit_scope) && k.ends_with(&prop_tail));
                        if declared_elsewhere_in_kit {
                            eprintln!(
                                "warning: {}: `{}.{}.{}` — property `{}` is not declared on class `{}` in kit `{}` (declared on another class); emitted as a plain literal until the shape or frontmatter is aligned",
                                relpath_str, kit_name, class_seg, prop_seg, prop_seg,
                                canonical_class.as_deref().unwrap_or(class_seg), kit_name
                            );
                        }
                    }
                }
                // DatatypeProperty: typed literal if ontology specifies a non-string range.
                if let Some(datatype) = lookup_key.as_ref().and_then(|k| prop_datatypes.get(k)) {
                    out.push_str(&format!(
                        "{} {} \"{}\"^^<{}> {} .\n",
                        doc_uri, kit_predicate, nq_escape(object), datatype, graph
                    ));
                } else {
                    out.push_str(&format!(
                        "{} {} \"{}\" {} .\n",
                        doc_uri, kit_predicate, nq_escape(object), graph
                    ));
                }
            }
        } else {
            // Legacy or non-kit frontmatter (title, tags, etc.) — use fm: namespace
            let fm_predicate = format!("<https://repolex.ai/ontology/git-lex/fm/{}>", uri_encode_path(subject));

            if subject.ends_with("-link") || subject.ends_with("-links") {
                let values: Vec<&str> = if subject.ends_with("-links") {
                    object.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect()
                } else {
                    vec![object.trim()]
                };
                for val in values {
                    if val.is_empty() { continue; }
                    let slug = val.trim_start_matches('@').to_lowercase()
                        .replace(' ', "-")
                        .replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '/' && c != '.', "");
                    if slug.is_empty() { continue; }
                    let object_uri = if slug.contains('/') || slug.ends_with(".md") {
                        format!("<{}>", resource_uri(&uri_encode_path(&slug)))
                    } else {
                        resolve_slug_to_uri(&slug, slug_index)
                    };
                    out.push_str(&format!(
                        "{} {} {} {} .\n",
                        doc_uri, fm_predicate, object_uri, graph
                    ));
                }
            } else {
                out.push_str(&format!(
                    "{} {} \"{}\" {} .\n",
                    doc_uri, fm_predicate, nq_escape(object), graph
                ));
            }
        }
    }
    errors
}

/// Build the slug→path and path indexes used for `[[wikilink]]` resolution.
///
/// Takes the repo root and a list of `.md` / `.txt` file paths, returns:
/// - `slug_index`: lowercase filename stem → relative path (with a dot-stripped
///   alias key for handles like `spaceg.o.a.t.` → `spacegoat`). Template files
///   (prefix `__`) are excluded.
/// - `path_index`: set of relative paths, for path-style wikilink resolution.
///
/// Extracted from `generate_frontmatter_nquads` so the history-graph walker
/// can build the same indexes and produce byte-identical triples when replaying
/// historical `.spo` line events through `emit_spo_line_nquads`.
///
/// Known fragility: slug_index is inherently collision-prone (two files with
/// the same stem in different folders). The canonical direction is to prefer
/// full-path wikilinks `[[Class/id]]` going forward and retain slug_index only
/// as a shim for bare `[[name]]` references.
pub(crate) fn build_slug_path_indexes(
    root: &std::path::Path,
    files: &[PathBuf],
) -> (HashMap<String, String>, HashSet<String>) {
    let mut slug_index: HashMap<String, String> = HashMap::new();
    let mut path_index: HashSet<String> = HashSet::new();
    for f in files {
        if let Ok(rel) = f.strip_prefix(root) {
            let rel_str = rel.to_string_lossy().to_string();
            path_index.insert(rel_str.clone());
            if let Some(file_name) = f.file_stem() {
                let slug = file_name.to_string_lossy().to_lowercase();
                if slug.starts_with("__") { continue; }
                slug_index.insert(slug.clone(), rel_str.clone());
                let dotless = slug.replace('.', "");
                if dotless != slug {
                    slug_index.entry(dotless).or_insert(rel_str);
                }
            }
        }
    }
    (slug_index, path_index)
}



/// Load every installed kit ontology TTL (`.lex/ontology/**/*.ttl`, EXCLUDING
/// `-shapes.ttl` SHACL files — Rob: shapes are validation, not vocabulary)
/// into the self-describing ontology graph
/// `<https://repolex.ai/git-lex/NamedGraph/repo-ontology>` of `store`.
///
/// Runs at INIT and KIT-UPDATE only (Rob Day-50): the graph persists in the
/// store ("stays put") — sync does not touch it, query does not rebuild it.
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
