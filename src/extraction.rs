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
    let kit_prefix = format!("{}.", kit);
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
    let kit_dir = kit_install_dir_for_spec(root, kit);
    let (_, _, short) = resolve_kit_spec(kit);
    let ttl_path = {
        let primary = kit_dir.join(format!("{}.ttl", short));
        if primary.exists() { primary } else {
            fs::read_dir(&kit_dir).ok()?
                .filter_map(|e| e.ok())
                .find(|e| e.path().extension().is_some_and(|ext| ext == "ttl") && !e.file_name().to_string_lossy().contains("shapes"))?
                .path()
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
