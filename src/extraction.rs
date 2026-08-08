//! Extraction helpers — slug/link resolution, YAML flattening, IRI sanitation,
//! and the frontmatter-to-Turtle converter (`frontmatter_to_turtle`, called by
//! `cmd_validate` to build the per-file graph SHACL validates).
//!
//! The big N-Quad *generators* (`generate_git_nquads`, `generate_frontmatter_nquads`,
//! `load_lex_nquads`) live in `nquad.rs`.
//!
//! Peeled out of `main.rs` during modularization. No behavior changes.
//!
//! NOTE(w4r3z, Day 48): two type-emitters in two files have DRIFTED apart and are
//! the STRUCTURAL ROOT of the B1 class-casing bug: `frontmatter_to_turtle` (here,
//! capitalizes first letter) and the nquad-path emitter (`nquad.rs:~749`, no
//! transform). Unifying them — one emitter, one casing rule — would dissolve the
//! bug. Update or remove this comment when that lands.

use std::collections::HashMap;
use std::fs;

use git_lex::resolve_kit_spec;


/// Escape a string for a Turtle double-quoted literal. Backslash FIRST,
/// then quote — escaping only the quote produced invalid Turtle for any
/// value containing a backslash, which made rudof's parse fail and the
/// whole file silently skip validation (adversarial finding 4b).
fn turtle_escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"")
}
use crate::ontology::{get_object_properties, get_property_datatypes};

/// Normalize a path-style wikilink target into a relpath that can be matched
/// against the file index. Resolves the target relative to `source_dir`,
/// collapses `.` and `..` segments, strips a leading `/`, and appends `.md`
/// if no extension is present.
///
/// Returns None if the target tries to escape the repo root (more `..`
/// segments than the source path can absorb).
pub(crate) fn normalize_wikilink_path(target: &str, source_dir: &str) -> Option<String> {
    // Leading `/` means "from repo root"; otherwise relative to source_dir.
    let combined = if let Some(rest) = target.strip_prefix('/') {
        rest.to_string()
    } else if source_dir.is_empty() {
        target.to_string()
    } else {
        format!("{}/{}", source_dir, target)
    };

    // Walk segments, collapsing . and ..
    let mut stack: Vec<&str> = Vec::new();
    for seg in combined.split('/') {
        match seg {
            "" | "." => continue,
            ".." => {
                if stack.pop().is_none() { return None; }
            }
            other => stack.push(other),
        }
    }
    if stack.is_empty() { return None; }
    let mut joined = stack.join("/");
    // Append .md if there is no file extension on the trailing segment
    if !stack.last().map(|s| s.contains('.')).unwrap_or(false) {
        joined.push_str(".md");
    }
    Some(joined)
}

/// Decode `%XX` escapes in a markdown link destination — `[x](my%20file.md)`
/// authors an on-disk path containing a space, and the file index holds the
/// raw filename. An invalid escape (no two hex digits) passes through
/// unchanged.
pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                16,
            ) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Render a YAML string value as ONE physical sidecar line. The `.spo`
/// format is line-based (one triple = one physical line — the sync walker
/// hard-fails on violations), so interior newlines in a multiline YAML
/// value (`description: |` blocks, wrapped strings) normalize to a single
/// space. Defined serialization, not munging: the sidecar stores the
/// single-line rendering of the value.
fn one_line(s: &str) -> String {
    if !s.contains('\n') && !s.contains('\r') {
        return s.to_string();
    }
    s.split(['\n', '\r'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Recursively flatten a YAML value into dot-notation `key | hasValue | val` lines.
/// Used by the frontmatter extractor to produce .spo-compatible rows for nested
/// YAML mappings and sequences.
pub(crate) fn flatten_yaml(prefix: &str, value: &serde_yaml::Value, lines: &mut Vec<String>) {
    match value {
        serde_yaml::Value::String(s) => {
            lines.push(format!("{} | hasValue | {}", prefix, one_line(s)));
        }
        serde_yaml::Value::Sequence(seq) => {
            for item in seq {
                if let Some(s) = item.as_str() {
                    lines.push(format!("{} | hasValue | {}", prefix, one_line(s)));
                } else if let Some(n) = item.as_f64() {
                    lines.push(format!("{} | hasValue | {}", prefix, n));
                } else if let Some(b) = item.as_bool() {
                    lines.push(format!("{} | hasValue | {}", prefix, b));
                }
            }
        }
        serde_yaml::Value::Bool(b) => {
            lines.push(format!("{} | hasValue | {}", prefix, b));
        }
        serde_yaml::Value::Number(n) => {
            lines.push(format!("{} | hasValue | {}", prefix, n));
        }
        serde_yaml::Value::Mapping(map) => {
            for (k, v) in map {
                if let Some(key_str) = k.as_str() {
                    let nested_prefix = format!("{}.{}", prefix, key_str);
                    flatten_yaml(&nested_prefix, v, lines);
                }
            }
        }
        _ => {}
    }
}
/// Read a markdown file with `kit.class.property` frontmatter and emit Turtle
/// for the document, using the kit's ontology to distinguish ObjectProperty
/// (→ IRI) from typed/plain literal ranges.
/// Returns `Ok(None)` for files that simply aren't kit documents (no
/// frontmatter, no kit-prefixed keys). Malformed YAML is `Err` — a doc that
/// TRIED to carry frontmatter and failed must fail validation, not skip it.
pub(crate) fn frontmatter_to_turtle(
    filepath: &std::path::Path,
    root: &std::path::Path,
    kit: &str,
) -> Result<Option<String>, String> {
    let content = fs::read_to_string(filepath)
        .map_err(|e| format!("cannot read file: {}", e))?;

    if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
        return Ok(None);
    }

    let rest = &content[4..];
    let end = match rest.find("\n---") {
        Some(e) => e,
        None => return Ok(None),
    };
    let yaml_str = &rest[..end];

    let yaml: HashMap<String, serde_yaml::Value> = serde_yaml::from_str(yaml_str)
        .map_err(|e| format!("malformed YAML frontmatter: {}", e))?;

    // Find dot notation keys matching this kit: kit.class.property
    // Use the short kit name (e.g., "soul") not the full spec
    // (e.g., "repolex-ai/git-lex-kit-soul") — frontmatter keys are
    // written as soul.Journal.journalId, not repolex-ai/git-lex-kit-soul.Journal.journalId.
    let (_, _, short) = git_lex::resolve_kit_spec(kit);
    let kit_prefix = format!("{}.", short);
    let mut doc_type: Option<String> = None;
    let mut kit_props: Vec<(String, String)> = Vec::new(); // (property_name, value)

    for (key, value) in &yaml {
        if let Some(rest) = key.strip_prefix(&kit_prefix) {
            let segments: Vec<&str> = rest.splitn(2, '.').collect();
            if segments.len() == 2 {
                let class_seg = segments[0];
                let prop_name = segments[1];

                // Infer doc type from class segment, validated against the
                // ontology (B1 fix, Day 38). This path used to capitalize the
                // first letter as a GUESS (`soul.cameraangle` → `Cameraangle`,
                // not the real `CameraAngle`), while nquad.rs passed the segment
                // through verbatim — the two emitters disagreed, and the graph
                // path's phantom `a soul:memory` made `?m a soul:Memory` miss.
                // Now BOTH call `resolve_class_segment`, the single rule anchored
                // to the kit's declared classes: exact/case-only hit → canonical
                // name (warn on case-only), real typo → Err. On Err this doc has
                // a bad class prefix; we warn and emit nothing for it (return
                // None) rather than stamp a phantom type.
                if doc_type.is_none() {
                    match crate::ontology::resolve_class_segment(
                        kit,
                        class_seg,
                        &filepath.display().to_string(),
                        true, // extraction runs at save — the author can act
                    ) {
                        Ok(canonical) => doc_type = Some(canonical),
                        Err(msg) => {
                            eprintln!("warning: {}: {msg}", filepath.display());
                            return Ok(None);
                        }
                    }
                }

                // Handle all YAML value types. Sequences produce one
                // entry per item — EXACTLY what flatten_yaml feeds the
                // emitter. The old code had no Sequence arm, so a doc
                // whose only kit properties were lists was skipped from
                // SHACL entirely: it committed cleanly while violating
                // its shape (adversarial finding 4a).
                match value {
                    serde_yaml::Value::String(s) if !s.is_empty() => {
                        kit_props.push((prop_name.to_string(), s.clone()));
                    }
                    serde_yaml::Value::Number(n) => {
                        kit_props.push((prop_name.to_string(), n.to_string()));
                    }
                    serde_yaml::Value::Bool(b) => {
                        kit_props.push((prop_name.to_string(), b.to_string()));
                    }
                    serde_yaml::Value::Sequence(seq) => {
                        for item in seq {
                            if let Some(s) = item.as_str() {
                                kit_props.push((prop_name.to_string(), s.to_string()));
                            } else if let Some(n) = item.as_f64() {
                                kit_props.push((prop_name.to_string(), n.to_string()));
                            } else if let Some(b) = item.as_bool() {
                                kit_props.push((prop_name.to_string(), b.to_string()));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let doc_type = match doc_type {
        Some(t) => t,
        None => return Ok(None),
    };
    if kit_props.is_empty() {
        return Ok(None);
    }

    // Prefix name + namespace come from the kit's SHACL shapes file (the
    // single runtime source of truth). If shapes aren't installed yet, fall
    // back to the short kit name with the conventional namespace.
    let prefix_name = crate::ontology::get_kit_prefix_name(kit);
    let mut namespace = crate::ontology::get_kit_namespace(kit);
    if namespace.is_empty() {
        let (_, _, short) = resolve_kit_spec(kit);
        namespace = git_lex::conventional_kit_namespace(&short);
    }

    // Build ObjectProperty set and datatype map for proper literal emission
    let obj_props = get_object_properties(kit);
    let prop_datatypes = get_property_datatypes(kit);
    let ref_ranges = crate::ontology::get_reference_ranges_all_kits();

    // Build Turtle RDF for this document. Subjects use the same minting
    // authority as the now-graph (resource_uri) — no urn: anywhere. SHACL
    // targets by rdf:type, so validation is unaffected; the IRIs just stop
    // lying about the identity scheme.
    let relpath = filepath.strip_prefix(root)
        .map_err(|_| format!("file {} is outside repo root", filepath.display()))?;
    let doc_iri = crate::git::resource_uri(
        &crate::nquad::uri_encode_path(&relpath.to_string_lossy()),
    );

    let mut ttl = String::new();
    ttl.push_str(&format!("@prefix {}: <{}> .\n", prefix_name, namespace));
    ttl.push_str("@prefix sh: <http://www.w3.org/ns/shacl#> .\n");
    ttl.push_str("@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\n");

    // Declare the document as an instance of the type
    ttl.push_str(&format!("<{}> a {}:{} .\n", doc_iri, prefix_name, doc_type));

    // Add properties
    for (prop_name, value) in &kit_props {
        // Kit+class-qualified lookup — tables key "{kit}/{Class}/{prop}"
        // (Rob-ruled 2026-07-21; see ontology.rs get_object_properties).
        let lookup_key = format!("{}/{}/{}", short, doc_type, prop_name);
        if obj_props.contains(lookup_key.as_str()) {
            // ObjectProperty — resolve each comma-separated value through
            // the SAME resolver sync's emitter uses (resolve.rs), so
            // validate judges the exact triples sync will emit. The old
            // logic here stripped `@`, slugified, and invented `entity/`
            // IRIs — so an `@mention` PASSED validation and then errored
            // at save time (review finding A5: two resolution policies).
            // URL-aware split (review #26) — the SAME splitter the emitter
            // uses, so validate judges exactly the values sync will emit.
            let values = crate::nquad::split_object_values(value);
            // Law 6 (identity model, 2026-07-30): a DECLARED RANGE makes the
            // authored value the target's ID — the range names the class, the
            // id names the Thing, nothing is guessed. This mirrors the sync
            // emitter's range branch in nquad.rs EXACTLY (the A5 rule this
            // comment block cites: one resolution policy, judged here as it
            // will be emitted there). Path-law resolution applies only to
            // ObjectProperties with no declared class range.
            let range = ref_ranges.get(&format!("{}/{}", short, prop_name));
            for val in &values {
                let val = val.as_str();
                if let Some(range_iri) = range {
                    match crate::nquad::thing_iri_from_range(range_iri, val) {
                        Some(target) => {
                            ttl.push_str(&format!(
                                "<{}> {}:{} {} .\n",
                                doc_iri, prefix_name, prop_name, target
                            ));
                        }
                        None => {
                            return Err(format!(
                                "{}: declared range `{}` is not a resolvable class IRI",
                                prop_name, range_iri
                            ));
                        }
                    }
                    continue;
                }
                match crate::resolve::resolve_frontmatter_value(val) {
                    crate::resolve::ResolveResult::Iri(uri) => {
                        // `uri` arrives in `<...>` form, valid Turtle as-is.
                        ttl.push_str(&format!(
                            "<{}> {}:{} {} .\n",
                            doc_iri, prefix_name, prop_name, uri
                        ));
                    }
                    crate::resolve::ResolveResult::Unresolved(lit) => {
                        // Unresolved stays a LITERAL (resolve.rs rule 7) so
                        // a sh:nodeKind sh:IRI shape flags it — validation
                        // surfaces the problem instead of inventing an IRI.
                        ttl.push_str(&format!(
                            "<{}> {}:{} \"{}\" .\n",
                            doc_iri, prefix_name, prop_name, turtle_escape(&lit)
                        ));
                    }
                    crate::resolve::ResolveResult::Rejected(msg) => {
                        return Err(format!("{}: {}", prop_name, msg));
                    }
                }
            }
        } else if let Some(datatype) = prop_datatypes.get(lookup_key.as_str()) {
            // Typed literal (xsd:integer, xsd:date, etc.)
            ttl.push_str(&format!(
                "<{}> {}:{} \"{}\"^^<{}> .\n",
                doc_iri, prefix_name, prop_name, turtle_escape(value), datatype
            ));
        } else {
            // Plain string literal
            ttl.push_str(&format!(
                "<{}> {}:{} \"{}\" .\n",
                doc_iri, prefix_name, prop_name, turtle_escape(value)
            ));
        }
    }

    // Diagnostic: dump the in-memory TTL that SHACL will validate against.
    // Enable with `GIT_LEX_DEBUG_TTL=1`. Useful when SHACL flags violations
    // that a plain query doesn't show — the in-memory TTL and the /now
    // graph are produced by different code paths and can diverge if one of
    // them isn't consulting the kit ontology correctly.
    if std::env::var("GIT_LEX_DEBUG_TTL").is_ok() {
        eprintln!("=== TTL for {} ===\n{}", filepath.display(), ttl);
    }
    Ok(Some(ttl))
}

// ─── Tree-sitter markdown link extractor ───────────────────────

/// Extract markdown links from body text using tree-sitter.
/// Writes `.md.spo` sidecars with link type (internal/external/unresolved)
/// and destination. Returns the number of extraction ERRORS (unreadable or
/// unparseable docs) — the caller folds them into the save gate, because a
/// doc that can't be re-extracted keeps its stale sidecar and a stale
/// sidecar keeps dead links alive in the graph forever (review #23).
pub(crate) fn extract_markdown_links() -> u32 {
    use std::collections::HashSet;
    use std::path::PathBuf;
    use git_lex::find_git_root;

    let mut errors: u32 = 0;
    let root = match find_git_root() {
        Some(r) => r,
        None => return 0,
    };

    let extract_dir = root.join(".lex").join("extract");
    fs::create_dir_all(&extract_dir).ok();

    let mut parser = tree_sitter_md::MarkdownParser::default();

    // One walker for the whole codebase; this consumer narrows EXPLICITLY:
    // markdown only (tree-sitter md parser) and no `__Class.md` templates
    // (kit scaffolds aren't content — their example links would pollute
    // the graph).
    let files: Vec<PathBuf> = crate::nquad::walk_repo_docs(&root)
        .into_iter()
        .filter(|p| p.extension().is_some_and(|x| x == "md") && !crate::nquad::is_template(p))
        .collect();

    // Build file index for resolving internal links
    let mut file_index: HashSet<String> = HashSet::new();
    for f in &files {
        if let Ok(rel) = f.strip_prefix(&root) {
            file_index.insert(rel.to_string_lossy().to_string());
        }
    }

    let mut total_links = 0;

    for filepath in &files {
        // Unreadable/unparseable docs are LOUD and counted (review #23):
        // the skip bypasses the sidecar-removal branch below, so the doc's
        // existing sidecar keeps asserting facts the doc may no longer
        // carry — and the sync diff never sees them vanish.
        let content = match fs::read_to_string(filepath) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "error: cannot read {} for link extraction ({e}) — its \
                     existing sidecar (if any) is NOT updated; fix the file \
                     (permissions / invalid UTF-8) or delete it",
                    filepath.display()
                );
                errors += 1;
                continue;
            }
        };

        let relpath = filepath.strip_prefix(&root).unwrap_or(filepath);
        let relpath_str = relpath.to_string_lossy().to_string();

        let tree = match parser.parse(content.as_bytes(), None) {
            Some(t) => t,
            None => {
                eprintln!(
                    "error: tree-sitter could not parse {} — its existing \
                     sidecar (if any) is NOT updated",
                    filepath.display()
                );
                errors += 1;
                continue;
            }
        };

        let mut spo_lines: Vec<String> = Vec::new();

        // Walk inline trees for links
        for inline_tree in tree.inline_trees() {
            let inline_root = inline_tree.root_node();

            fn extract_links(node: tree_sitter::Node, source: &str, lines: &mut Vec<String>, file_index: &HashSet<String>, doc_dir: &str, relpath: &str) {
                if node.kind() == "inline_link" {
                    let dest = node.children(&mut node.walk())
                        .find(|c| c.kind() == "link_destination")
                        .map(|c| source[c.start_byte()..c.end_byte()].to_string())
                        .unwrap_or_default();

                    if !dest.is_empty() {
                        if dest.starts_with("http://") || dest.starts_with("https://") {
                            // External link
                            lines.push(format!("md.externalLink | hasValue | {}", dest));
                        } else {
                            // Internal link. Strip any #fragment (a section
                            // link still targets the file), percent-decode
                            // (`my%20file.md` authors an on-disk space), then
                            // resolve against the doc's directory with `./`
                            // and `../` collapsed (review #27: the old naive
                            // string join made every `../` link silently
                            // unresolvable — the same file already shipped
                            // the collapser; this lane just never called it).
                            let target = percent_decode(dest.split('#').next().unwrap_or(""));
                            if target.is_empty() {
                                // pure same-page anchor (`#section`) — not a
                                // document reference, no line at all
                            } else if let Some(resolved) =
                                normalize_wikilink_path(&target, doc_dir)
                                    .filter(|r| file_index.contains(r))
                            {
                                // Markdown links are THE document-reference
                                // edge (Rob-ruled 2026-08-06): they emit
                                // `linksTo`, the name the graph and viz
                                // already consume. md.internalLink retired
                                // with the wikilink reader.
                                lines.push(format!("{} | linksTo | {}", relpath, resolved));
                            } else {
                                lines.push(format!("md.unresolvedLink | hasValue | {}", dest));
                            }
                        }
                    }
                }

                // Also catch bare autolinks
                if node.kind() == "uri_autolink" {
                    let text = &source[node.start_byte()..node.end_byte()];
                    let url = text.trim_matches(|c| c == '<' || c == '>');
                    if !url.is_empty() {
                        lines.push(format!("md.externalLink | hasValue | {}", url));
                    }
                }

                let mut cursor = node.walk();
                if cursor.goto_first_child() {
                    loop {
                        extract_links(cursor.node(), source, lines, file_index, doc_dir, relpath);
                        if !cursor.goto_next_sibling() { break; }
                    }
                }
            }

            let doc_dir = relpath.parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();

            extract_links(inline_root, &content, &mut spo_lines, &file_index, &doc_dir, &relpath_str);
        }

        // Write .md.spo sidecar; when a doc's last link goes away its
        // sidecar must go away too, so the sync diff sees the lines vanish
        // and records retractions — same contract as the `.fm.spo` path
        // (a stale sidecar keeps dead links alive in the graph forever).
        let spo_path = extract_dir.join(format!("{}.md.spo", relpath_str));
        if !spo_lines.is_empty() {
            spo_lines.sort();
            spo_lines.dedup();
            crate::nquad::write_sidecar_loud(&spo_path, &(spo_lines.join("\n") + "\n"));
            total_links += spo_lines.len();
        } else if spo_path.exists() {
            crate::nquad::remove_sidecar_loud(&spo_path);
        }
    }

    if total_links > 0 {
        eprintln!("Markdown links: {} from {} files", total_links, files.len());
    }
    errors
}

#[cfg(test)]
mod link_resolution_tests {
    use super::*;

    /// Review #27: the markdown-link lane resolves `../` and `./` segments
    /// through the same collapser the frontmatter lane uses — a valid
    /// relative link must never silently fail the index lookup.
    #[test]
    fn relative_segments_collapse_before_lookup() {
        assert_eq!(
            normalize_wikilink_path("../Pursuit/thread.md", "Soul/Journal"),
            Some("Soul/Pursuit/thread.md".to_string())
        );
        assert_eq!(
            normalize_wikilink_path("./sibling.md", "Soul/Note"),
            Some("Soul/Note/sibling.md".to_string())
        );
        // Escaping the repo root is a real failure, not a silent guess.
        assert_eq!(normalize_wikilink_path("../../up.md", "Soul"), None);
    }

    #[test]
    fn percent_decode_covers_authored_escapes() {
        assert_eq!(percent_decode("my%20file.md"), "my file.md");
        assert_eq!(percent_decode("plain.md"), "plain.md");
        // Invalid escape passes through unchanged.
        assert_eq!(percent_decode("100%zz.md"), "100%zz.md");
        // Encoded UTF-8 decodes to the real character.
        assert_eq!(percent_decode("Caf%C3%A9.md"), "Café.md");
    }
}

#[cfg(test)]
mod one_line_tests {
    use super::*;

    /// PIN: one triple = one physical sidecar line. A multiline YAML value
    /// (block scalar, wrapped string) must never put a raw newline into the
    /// sidecar — that splits the triple across two lines and hard-fails sync
    /// (Selkie's day-10 transcript class of failure, 2026-07-30).
    #[test]
    fn multiline_yaml_value_becomes_one_sidecar_line() {
        let yaml: serde_yaml::Value =
            serde_yaml::from_str("desc: |\n  first line\n  second line\n").unwrap();
        let mut lines = Vec::new();
        flatten_yaml("soul.Note", yaml.get("desc").unwrap(), &mut lines);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "soul.Note | hasValue | first line second line");
        assert!(!lines[0].contains('\n'));
    }

    #[test]
    fn single_line_values_pass_through_untouched() {
        assert_eq!(one_line("plain value"), "plain value");
        assert_eq!(one_line("keeps  interior  spaces"), "keeps  interior  spaces");
    }
}
