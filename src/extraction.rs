//! Extraction helpers — slug/link resolution, YAML flattening, IRI sanitation,
//! and the frontmatter-to-Turtle converter used by `cmd create` / `cmd save`.
//!
//! The big N-Quad *generators* (`generate_git_nquads`, `generate_frontmatter_nquads`,
//! `load_lex_nquads`, `compile_extraction_log`) stay in main.rs for now — they
//! will move in a follow-up phase once their store-access shape settles.
//!
//! Peeled out of `main.rs` during modularization. No behavior changes.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;

use git_lex::{kit_install_dir_for_spec, resolve_kit_spec};

use crate::nquad::uri_encode_path;
use crate::ontology::{get_object_properties, get_property_datatypes};

/// Resolve a slug to an IRI, using the provided base URI and slug index.
/// Falls back to an entity URI if the slug is not in the index.
pub(crate) fn resolve_slug_to_uri(slug: &str, base: &str, slug_index: &HashMap<String, String>) -> String {
    if let Some(rel_path) = slug_index.get(slug) {
        format!("<{}/{}>", base, uri_encode_path(rel_path))
    } else {
        // No matching file — fall back to entity URI
        format!("<{}/entity/{}>", base, uri_encode_path(slug))
    }
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

/// True if the byte position `start` in `text` is preceded by a non-word
/// character (or is at the start of `text`). Used to reject `@mention`
/// matches that are actually the local-part separator of an email address
/// (`rob@repolex.ai` should not produce a mention `@repolex.ai`).
///
/// "Word char" here means ASCII alphanumeric or `_`, matching the usual
/// `\b` semantics. We walk back to the previous char boundary so this is
/// safe on UTF-8 input.
pub(crate) fn is_word_boundary_before(text: &str, start: usize) -> bool {
    if start == 0 {
        return true;
    }
    // Step back to the previous char boundary.
    let mut i = start - 1;
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    let prev = text[i..].chars().next();
    match prev {
        Some(c) => !(c.is_ascii_alphanumeric() || c == '_'),
        None => true,
    }
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

/// True if the given string parses as a syntactically valid IRI.
pub(crate) fn is_valid_iri(iri: &str) -> bool {
    oxiri::Iri::parse(iri).is_ok()
}

/// Sanitize a string for use in a URI path segment.
/// Removes/replaces characters that would make an invalid IRI.
pub(crate) fn sanitize_uri_segment(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            ' ' | ':' | '/' | '\\' | '<' | '>' | '{' | '}' | '|' | '^' | '`' | '[' | ']' | '#' | '?' | '@' => '-',
            _ if c.is_alphanumeric() => c,
            _ => '-',
        })
        .collect::<String>()
        .replace("--", "-")
        .trim_matches('-')
        .to_string()
}

/// Generate a short deterministic hash from a string (16 hex chars).
pub(crate) fn short_hash(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..8]) // 16 hex chars
}

/// Read a markdown file with `kit.class.property` frontmatter and emit Turtle
/// for the document, using the kit's ontology to distinguish ObjectProperty
/// (→ IRI) from typed/plain literal ranges.
pub(crate) fn frontmatter_to_turtle(filepath: &std::path::Path, root: &std::path::Path, kit: &str) -> Option<String> {
    let content = fs::read_to_string(filepath).ok()?;

    if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
        return None;
    }

    let rest = &content[4..];
    let end = rest.find("\n---")?;
    let yaml_str = &rest[..end];

    let yaml: HashMap<String, serde_yaml::Value> = serde_yaml::from_str(yaml_str).ok()?;

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

                // Infer doc type from class segment (capitalize)
                if doc_type.is_none() {
                    let mut c = class_seg.chars();
                    doc_type = Some(match c.next() {
                        None => class_seg.to_string(),
                        Some(f) => f.to_uppercase().to_string() + c.as_str(),
                    });
                }

                // Handle all YAML value types (string, number, bool)
                let val_str = match value {
                    serde_yaml::Value::String(s) if !s.is_empty() => Some(s.clone()),
                    serde_yaml::Value::Number(n) => Some(n.to_string()),
                    serde_yaml::Value::Bool(b) => Some(b.to_string()),
                    _ => None,
                };
                if let Some(s) = val_str {
                    kit_props.push((prop_name.to_string(), s));
                }
            }
        }
    }

    let doc_type = doc_type?;
    if kit_props.is_empty() {
        return None;
    }

    // Read the kit ontology to find the prefix name and namespace
    let (_, _, short) = resolve_kit_spec(kit);
    let ontology_dir = root.join(".lex").join("ontology").join(&short);
    let ttl_path = {
        let primary = ontology_dir.join(format!("{}.ttl", short));
        if primary.exists() { primary } else {
            // Fallback: any non-shapes .ttl in ontology dir
            let fallback = fs::read_dir(&ontology_dir).ok()
                .and_then(|entries| entries
                    .filter_map(|e| e.ok())
                    .find(|e| e.path().extension().is_some_and(|ext| ext == "ttl")
                        && !e.file_name().to_string_lossy().contains("shapes"))
                    .map(|e| e.path()));
            match fallback {
                Some(p) => p,
                None => {
                    // Legacy: try .lex/kit/
                    let kit_dir = kit_install_dir_for_spec(root, kit);
                    let legacy = kit_dir.join(format!("{}.ttl", short));
                    if legacy.exists() { legacy } else { return None; }
                }
            }
        }
    };
    let kit_ttl = fs::read_to_string(&ttl_path).ok()?;

    // Find prefix name and namespace from TTL — uses short kit name
    let kit_ns_pattern = format!("/kit/{}/", short);
    let mut prefix_name = short.clone();
    let mut namespace = format!("https://repolex.ai/ontology/kit/{}/", short);
    for line in kit_ttl.lines() {
        if line.starts_with("@prefix ") && line.contains(&kit_ns_pattern) {
            if let Some(colon_pos) = line[8..].find(':') {
                prefix_name = line[8..8 + colon_pos].trim().to_string();
            }
            if let Some(start) = line.find('<') {
                if let Some(end) = line.find('>') {
                    namespace = line[start + 1..end].to_string();
                }
            }
            break;
        }
    }

    // Build ObjectProperty set and datatype map for proper literal emission
    let obj_props = get_object_properties(kit);
    let prop_datatypes = get_property_datatypes(kit);

    // Build Turtle RDF for this document
    let relpath = filepath.strip_prefix(root).ok()?;
    let doc_id = relpath.to_string_lossy().replace('/', "_").replace('.', "_");

    let mut ttl = String::new();
    ttl.push_str(&format!("@prefix {}: <{}> .\n", prefix_name, namespace));
    ttl.push_str("@prefix sh: <http://www.w3.org/ns/shacl#> .\n");
    ttl.push_str("@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\n");

    // Declare the document as an instance of the type
    ttl.push_str(&format!("<urn:doc:{}> a {}:{} .\n", doc_id, prefix_name, doc_type));

    // Add properties
    for (prop_name, value) in &kit_props {
        if obj_props.contains(prop_name.as_str()) {
            // ObjectProperty — resolve each comma-separated value as IRI
            let values: Vec<&str> = value.split(',').map(|v| v.trim()).filter(|v| !v.is_empty()).collect();
            for val in values {
                let slug = val.trim_start_matches('@').to_lowercase()
                    .replace(' ', "-")
                    .replace(|c: char| !c.is_alphanumeric() && c != '-', "");
                if !slug.is_empty() {
                    ttl.push_str(&format!(
                        "<urn:doc:{}> {}:{} <urn:entity:{}> .\n",
                        doc_id, prefix_name, prop_name, slug
                    ));
                }
            }
        } else if let Some(datatype) = prop_datatypes.get(prop_name.as_str()) {
            // Typed literal (xsd:integer, xsd:date, etc.)
            ttl.push_str(&format!(
                "<urn:doc:{}> {}:{} \"{}\"^^<{}> .\n",
                doc_id, prefix_name, prop_name, value.replace('"', "\\\""), datatype
            ));
        } else {
            // Plain string literal
            ttl.push_str(&format!(
                "<urn:doc:{}> {}:{} \"{}\" .\n",
                doc_id, prefix_name, prop_name, value.replace('"', "\\\"")
            ));
        }
    }

    Some(ttl)
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
        fs::create_dir_all(spo_path.parent().unwrap()).ok();
        fs::write(&spo_path, spo_lines.join("\n") + "\n").ok();

        // Write meta for incremental
        fs::create_dir_all(meta_path.parent().unwrap()).ok();
        fs::write(&meta_path, format!("last_line: {}\nlast_sync: {}\n", total_lines, last_timestamp)).ok();
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

    // Walk all .md files
    fn walk_md(dir: &std::path::Path, files: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') { continue; }
                if path.is_dir() { walk_md(&path, files); }
                else if name.ends_with(".md") && !name.starts_with("__") { files.push(path); }
            }
        }
    }

    let mut files = Vec::new();
    walk_md(&root, &mut files);

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

        // Write .md.spo sidecar
        if !spo_lines.is_empty() {
            spo_lines.sort();
            spo_lines.dedup();
            let spo_path = extract_dir.join(format!("{}.md.spo", relpath_str));
            fs::create_dir_all(spo_path.parent().unwrap()).ok();
            fs::write(&spo_path, spo_lines.join("\n") + "\n").ok();
            total_links += spo_lines.len();
        }
    }

    if total_links > 0 {
        eprintln!("Markdown links: {} from {} files", total_links, files.len());
    }
}
