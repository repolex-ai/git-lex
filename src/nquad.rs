//! N-Quad / N-Triple encoding helpers.
//!
//! Peeled out of `main.rs` during modularization. Currently just the
//! escapers; the larger N-Quad generators (`generate_git_nquads`,
//! `generate_frontmatter_nquads`) will move here once their ontology-reader
//! dependencies are also split out.

/// Escape a string for use in N-Quads literals.
pub(crate) fn nq_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Percent-encode a path for use in URIs (spaces, special chars).
pub(crate) fn uri_encode_path(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '<' => "%3C".to_string(),
            '>' => "%3E".to_string(),
            '{' => "%7B".to_string(),
            '}' => "%7D".to_string(),
            '|' => "%7C".to_string(),
            '^' => "%5E".to_string(),
            '`' => "%60".to_string(),
            '[' => "%5B".to_string(),
            ']' => "%5D".to_string(),
            _ => c.to_string(),
        })
        .collect()
}
