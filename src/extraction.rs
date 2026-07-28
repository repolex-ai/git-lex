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

use crate::nquad::uri_encode_path;

/// Escape a string for a Turtle double-quoted literal. Backslash FIRST,
/// then quote — escaping only the quote produced invalid Turtle for any
/// value containing a backslash, which made rudof's parse fail and the
/// whole file silently skip validation (adversarial finding 4b).
fn turtle_escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"")
}
use crate::ontology::{get_object_properties, get_property_datatypes};

/// Resolve a slug to an IRI under the soul a-box base (Day-50: no soul
/// identity in subjects). Callers only invoke this on a slug-index HIT —
/// there is no fallback IRI policy (unresolved values stay literals,
/// resolve.rs rule 7; the old `entity/<slug>` fallback was the retired
/// minting policy and was unreachable from every graph-path caller).
pub(crate) fn resolve_slug_to_uri(slug: &str, slug_index: &HashMap<String, String>) -> String {
    let rel_path = slug_index
        .get(slug)
        .expect("resolve_slug_to_uri called without a slug_index hit (caller bug)");
    format!("<{}>", crate::git::resource_uri(&uri_encode_path(rel_path)))
}

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

/// Recursively flatten a YAML value into dot-notation `key | hasValue | val` lines.
/// Used by the frontmatter extractor to produce .spo-compatible rows for nested
/// YAML mappings and sequences.
pub(crate) fn flatten_yaml(prefix: &str, value: &serde_yaml::Value, lines: &mut Vec<String>) {
    match value {
        serde_yaml::Value::String(s) => {
            lines.push(format!("{} | hasValue | {}", prefix, s));
        }
        serde_yaml::Value::Sequence(seq) => {
            for item in seq {
                if let Some(s) = item.as_str() {
                    lines.push(format!("{} | hasValue | {}", prefix, s));
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
    slug_index: &HashMap<String, String>,
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
                    match crate::ontology::resolve_class_segment(kit, class_seg) {
                        Ok(canonical) => doc_type = Some(canonical),
                        Err(msg) => {
                            eprintln!("warning: {msg} (skipping {})", filepath.display());
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
            let values: Vec<&str> = value.split(',').map(|v| v.trim()).filter(|v| !v.is_empty()).collect();
            for val in values {
                match crate::resolve::resolve_frontmatter_value(val, slug_index) {
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

// ─── JSONL session extractor (for claude-code kit) ─────────────

/// Extract structural metadata from .jsonl conversation files.
/// Only runs for the `claude-code` kit. Writes `.cc.spo` sidecars with
/// session-level metadata (sessionId, timestamps, message counts, tool
/// usage) and a `.meta` file tracking the last-processed line for
/// incremental runs.
pub(crate) fn extract_jsonl_sessions() {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use git_lex::{find_git_root, get_kit};

    let root = match find_git_root() {
        Some(r) => r,
        None => return,
    };

    // Only run for claude-code kit
    let kit = get_kit();
    if kit.as_deref() != Some("claude-code") {
        return;
    }

    let extract_dir = root.join(".lex").join("extract");
    fs::create_dir_all(&extract_dir).ok();

    // Find all .jsonl files (not in .lex/)
    let mut jsonl_files = Vec::new();
    fn walk_jsonl(dir: &std::path::Path, files: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') { continue; }
                if path.is_dir() {
                    walk_jsonl(&path, files);
                } else if name.ends_with(".jsonl") {
                    files.push(path);
                }
            }
        }
    }
    walk_jsonl(&root, &mut jsonl_files);

    for filepath in &jsonl_files {
        let relpath = filepath.strip_prefix(&root).unwrap_or(filepath);
        let relpath_str = relpath.to_string_lossy().to_string();

        // Check for incremental: read meta file for last processed line
        let meta_path = extract_dir.join(format!("{}.meta", relpath_str));
        let last_line: usize = fs::read_to_string(&meta_path)
            .ok()
            .and_then(|s| {
                for line in s.lines() {
                    if let Some(n) = line.strip_prefix("last_line: ") {
                        return n.trim().parse().ok();
                    }
                }
                None
            })
            .unwrap_or(0);

        // Read the file
        let content = match fs::read_to_string(filepath) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        // Skip if no new lines
        if last_line >= total_lines {
            continue;
        }

        // Parse all lines (or just new ones for incremental)
        let mut session_id = String::new();
        let mut project_path = String::new();
        let mut first_timestamp = String::new();
        let mut last_timestamp = String::new();
        let mut cwd = String::new();
        let mut version = String::new();
        let mut git_branch = String::new();
        let mut user_count: usize = 0;
        let mut assistant_count: usize = 0;
        let mut system_count: usize = 0;
        let mut channel_count: usize = 0;
        let mut tool_counts: HashMap<String, usize> = HashMap::new();

        for (_i, line) in lines.iter().enumerate() {
            let line = line.trim();
            if line.is_empty() { continue; }

            let obj: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let msg_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

            // Extract session metadata from first valid message
            if session_id.is_empty() {
                if let Some(sid) = obj.get("sessionId").and_then(|v| v.as_str()) {
                    session_id = sid.to_string();
                }
            }
            if cwd.is_empty() {
                if let Some(c) = obj.get("cwd").and_then(|v| v.as_str()) {
                    cwd = c.to_string();
                }
            }
            if version.is_empty() {
                if let Some(v) = obj.get("version").and_then(|v| v.as_str()) {
                    version = v.to_string();
                }
            }
            if git_branch.is_empty() {
                if let Some(b) = obj.get("gitBranch").and_then(|v| v.as_str()) {
                    git_branch = b.to_string();
                }
            }

            // Track timestamps
            if let Some(ts) = obj.get("timestamp").and_then(|v| v.as_str()) {
                if first_timestamp.is_empty() {
                    first_timestamp = ts.to_string();
                }
                last_timestamp = ts.to_string();
            }

            // Count message types
            match msg_type {
                "user" => user_count += 1,
                "assistant" => {
                    assistant_count += 1;
                    // Count tool usage
                    if let Some(content) = obj.get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_array())
                    {
                        for item in content {
                            if item.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                                if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                                    *tool_counts.entry(name.to_string()).or_insert(0) += 1;
                                }
                            }
                        }
                    }
                }
                "system" => system_count += 1,
                "queue-operation" => channel_count += 1,
                _ => {}
            }
        }
        let _ = system_count; // parsed but not emitted; keep for future use

        // Derive project from parent directory name
        if project_path.is_empty() {
            if let Some(parent) = relpath.parent() {
                project_path = parent.to_string_lossy().to_string();
            }
        }

        // Generate .spo
        let session_name = if !session_id.is_empty() {
            format!("session-{}", &session_id[..8.min(session_id.len())])
        } else {
            let stem = filepath.file_stem().unwrap_or_default().to_string_lossy();
            format!("session-{}", &stem[..8.min(stem.len())])
        };

        let mut spo_lines = Vec::new();
        spo_lines.push(format!("{} | isA | session", session_name));

        if !session_id.is_empty() {
            spo_lines.push(format!("{} | sessionId | {}", session_name, session_id));
        }
        if !project_path.is_empty() {
            spo_lines.push(format!("{} | project | {}", session_name, project_path));
        }
        if !first_timestamp.is_empty() {
            spo_lines.push(format!("{} | startTime | {}", session_name, first_timestamp));
        }
        if !last_timestamp.is_empty() {
            spo_lines.push(format!("{} | endTime | {}", session_name, last_timestamp));
        }
        if !cwd.is_empty() {
            spo_lines.push(format!("{} | cwd | {}", session_name, cwd));
        }
        if !version.is_empty() {
            spo_lines.push(format!("{} | ccVersion | {}", session_name, version));
        }
        if !git_branch.is_empty() {
            spo_lines.push(format!("{} | gitBranch | {}", session_name, git_branch));
        }

        spo_lines.push(format!("{} | messageCount | {}", session_name, total_lines));
        spo_lines.push(format!("{} | userMessageCount | {}", session_name, user_count));
        spo_lines.push(format!("{} | assistantMessageCount | {}", session_name, assistant_count));

        if channel_count > 0 {
            spo_lines.push(format!("{} | channelMessageCount | {}", session_name, channel_count));
        }

        // Tool usage
        let mut tools: Vec<(&String, &usize)> = tool_counts.iter().collect();
        tools.sort_by(|a, b| b.1.cmp(a.1));
        for (tool, count) in &tools {
            spo_lines.push(format!("{} | toolUsage | {}:{}", session_name, tool, count));
        }

        // Sort and write sidecar
        spo_lines.sort();
        spo_lines.dedup();

        let spo_path = extract_dir.join(format!("{}.cc.spo", relpath_str));
        crate::nquad::write_sidecar_loud(&spo_path, &(spo_lines.join("\n") + "\n"));

        // Write meta for incremental
        crate::nquad::write_sidecar_loud(
            &meta_path,
            &format!("last_line: {}\nlast_sync: {}\n", total_lines, last_timestamp),
        );
    }
}

// ─── Tree-sitter markdown link extractor ───────────────────────

/// Extract markdown links from body text using tree-sitter.
/// Writes `.md.spo` sidecars with link type (internal/external/unresolved)
/// and destination.
pub(crate) fn extract_markdown_links() {
    use std::collections::HashSet;
    use std::path::PathBuf;
    use git_lex::find_git_root;

    let root = match find_git_root() {
        Some(r) => r,
        None => return,
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
        let content = match fs::read_to_string(filepath) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let relpath = filepath.strip_prefix(&root).unwrap_or(filepath);
        let relpath_str = relpath.to_string_lossy().to_string();

        let tree = match parser.parse(content.as_bytes(), None) {
            Some(t) => t,
            None => continue,
        };

        let mut spo_lines: Vec<String> = Vec::new();

        // Walk inline trees for links
        for inline_tree in tree.inline_trees() {
            let inline_root = inline_tree.root_node();

            fn extract_links(node: tree_sitter::Node, source: &str, lines: &mut Vec<String>, file_index: &HashSet<String>, doc_dir: &str) {
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
                            // Internal link — resolve relative to doc's directory
                            let resolved = if dest.starts_with('/') {
                                dest[1..].to_string()
                            } else if !doc_dir.is_empty() {
                                format!("{}/{}", doc_dir, dest)
                            } else {
                                dest.clone()
                            };

                            if file_index.contains(&resolved) {
                                lines.push(format!("md.internalLink | hasValue | {}", resolved));
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
                        extract_links(cursor.node(), source, lines, file_index, doc_dir);
                        if !cursor.goto_next_sibling() { break; }
                    }
                }
            }

            let doc_dir = relpath.parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();

            extract_links(inline_root, &content, &mut spo_lines, &file_index, &doc_dir);
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
}
