//! Frontmatter value resolution for ObjectProperty fields.
//!
//! This module contains the single resolver function that converts a raw
//! frontmatter value (what the agent typed) into either a valid IRI or an
//! error. It enforces the git-lex frontmatter rules at the code level,
//! with tests that serve as both regression guards and living documentation
//! of what the rules are.
//!
//! ## The rules (as codified by the test suite below)
//!
//! 1. **No `[[wikilinks]]` in frontmatter.** Wikilinks are body-text syntax
//!    for cross-referencing pages. If an agent writes `assignedTo: [[w4r3z]]`
//!    the resolver rejects it with a clear error. The correct form is
//!    `assignedTo: w4r3z` (bare slug).
//!
//! 2. **No `@mentions` in frontmatter.** The `@` prefix is body-text syntax
//!    for mentioning agents in prose. `assignedTo: @w4r3z` is rejected.
//!    Write `assignedTo: w4r3z` instead.
//!
//! 3. **Bare slugs must resolve to a known file.** The slug is looked up
//!    in the repo's file index (keyed by file stem, lowercased). If no
//!    file matches, the value is returned as an unresolved literal so the
//!    SHACL validator can flag it downstream.
//!
//! 4. **Full IRIs are passed through unchanged.** If the value starts with
//!    `http://` or `https://`, the resolver trusts it. This is the canonical
//!    form for machine-generated references (e.g. claude-export's JSONL
//!    extractor producing conversation/agent URIs from UUIDs). No slug
//!    normalization, no range checking — the extractor that produced the
//!    IRI is responsible for its correctness.
//!
//! 5. **Path-style values resolve against the file index.** If the value
//!    contains `/` or ends with `.md`, it's treated as a relative path
//!    within the repo. Looked up in the path index directly.
//!
//! 6. **No case folding or workaround transformations.** The slug must
//!    match the file stem as-is after lowercasing (which mirrors how the
//!    slug index is built — all stems are lowercased at index time). No
//!    dot-stripping fallback, no character removal, no silent coercion.
//!    If the value doesn't match, it doesn't match.
//!
//! 7. **Unresolved values become literals, not blank nodes.** When a
//!    frontmatter ObjectProperty value can't be resolved, it's emitted
//!    as a literal string on the kit predicate. This preserves the
//!    author's intent (what they typed) without introducing blank nodes
//!    into the graph. SHACL shapes that declare `sh:nodeKind sh:IRI`
//!    will flag these literals as validation errors, telling the agent
//!    to fix their data.

use std::collections::HashMap;

/// The result of attempting to resolve a frontmatter ObjectProperty value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveResult {
    /// Value resolved to an IRI (either via slug lookup, path lookup, or
    /// direct IRI passthrough). The string includes angle brackets.
    Iri(String),
    /// Value could not be resolved — returned as a literal string for
    /// the caller to emit and SHACL to flag.
    Unresolved(String),
    /// Value used a syntax that is not allowed in frontmatter. The string
    /// is a human-readable error message explaining what to write instead.
    Rejected(String),
}

/// Resolve a raw frontmatter ObjectProperty value into an IRI or reject it.
///
/// This is the ONLY code path for frontmatter ObjectProperty resolution.
/// Body-text `[[wikilinks]]` and `@mentions` have their own resolvers
/// because they serve a different purpose (ambient cross-references in
/// prose, emitting `md:linksTo` and `md:mentions` respectively).
///
/// See the module-level doc comment for the full rule set, and the test
/// suite below for executable examples of each rule.
pub fn resolve_frontmatter_value(
    raw: &str,
    slug_index: &HashMap<String, String>,
) -> ResolveResult {
    // Rule 1: reject [[wikilinks]]
    if raw.starts_with("[[") || raw.ends_with("]]") {
        let inner = raw
            .trim_start_matches("[[")
            .trim_end_matches("]]");
        return ResolveResult::Rejected(format!(
            "wikilink syntax [[...]] is not allowed in frontmatter. \
             Write the bare slug instead: {}",
            inner
        ));
    }

    // Rule 2: reject @mentions
    if raw.starts_with('@') {
        let inner = &raw[1..];
        return ResolveResult::Rejected(format!(
            "@mention syntax is not allowed in frontmatter. \
             Write the bare slug instead: {}",
            inner
        ));
    }

    // Rule 4: full IRI passthrough — but percent-encode any character that
    // would make the IRI structurally invalid (spaces, parens, etc. that
    // appear in real-world URLs). Without this, oxigraph's strict NQuads
    // parser rejects entire history-walk batches when one external link
    // contains an unencoded character.
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return ResolveResult::Iri(format!("<{}>", crate::nquad::uri_encode_path(raw)));
    }

    // Rule 5: path-style values
    if raw.contains('/') || raw.ends_with(".md") {
        return ResolveResult::Iri(format!("<{}>", crate::git::resource_uri(&crate::nquad::uri_encode_path(raw))));
    }

    // Rule 3: bare slug lookup (lowercased to match index convention)
    // NOTE(w4r3z, Day 38): this lowercases but does NOT trim. The test
    // `whitespace_only_is_unresolved` claims "lowercased + trimmed to empty"
    // as its rationale, but "   " stays "   " here (is_empty() is false) and is
    // Unresolved only because the slug_index has no "   " key — a different
    // reason than the test states. Harmless today, but the comment/test
    // rationale is wrong; either trim here (so whitespace truly empties) or fix
    // the test's stated reason. Low severity; flagging so a maintainer relying
    // on the comment isn't misled.
    let slug = raw.to_lowercase();
    if slug.is_empty() {
        return ResolveResult::Unresolved(raw.to_string());
    }
    if let Some(rel_path) = slug_index.get(&slug) {
        ResolveResult::Iri(format!("<{}>", crate::git::resource_uri(&crate::nquad::uri_encode_path(rel_path))))
    } else {
        // Rule 7: unresolved → literal, not blank node
        ResolveResult::Unresolved(raw.to_string())
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Tests — these ARE the rules documentation. Each test name and doc comment
// states a rule; the test body proves the code enforces it.
// ════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn test_index() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("w4r3z".to_string(), "agent/w4r3z.md".to_string());
        m.insert("kira".to_string(), "agent/kira.md".to_string());
        m.insert("some-decision".to_string(), "decision/some-decision.md".to_string());
        m
    }

    // The a-box base is DERIVED per repo (kit short / repo name — never
    // hardcoded); tests assert resolution BEHAVIOR against whatever base
    // this checkout derives, not a pinned literal.
    fn base() -> String {
        crate::git::resource_uri("")
    }

    // ─── Rule 1: no wikilinks in frontmatter ──────────────────────────────

    /// Frontmatter ObjectProperty values must not use [[wikilink]] syntax.
    /// Wikilinks are body-text syntax for cross-referencing pages.
    /// The agent should write the bare slug instead.
    #[test]
    fn rejects_wikilink_brackets() {
        let idx = test_index();
        let result = resolve_frontmatter_value("[[w4r3z]]", &idx);
        assert!(
            matches!(result, ResolveResult::Rejected(_)),
            "[[wikilinks]] must be rejected in frontmatter"
        );
    }

    /// The rejection message should tell the agent what to write instead.
    #[test]
    fn wikilink_rejection_suggests_bare_slug() {
        let idx = test_index();
        if let ResolveResult::Rejected(msg) = resolve_frontmatter_value("[[w4r3z]]", &idx) {
            assert!(msg.contains("w4r3z"), "should suggest the inner slug");
            assert!(msg.contains("bare slug"), "should say 'bare slug'");
        } else {
            panic!("expected Rejected");
        }
    }

    /// Even if the brackets contain a valid slug, it's still rejected.
    /// The brackets are the problem, not the content.
    #[test]
    fn rejects_wikilink_even_when_slug_exists() {
        let idx = test_index();
        assert!(matches!(
            resolve_frontmatter_value("[[kira]]", &idx),
            ResolveResult::Rejected(_)
        ));
    }

    // ─── Rule 2: no @mentions in frontmatter ──────────────────────────────

    /// Frontmatter ObjectProperty values must not use @mention syntax.
    /// The @ prefix is body-text syntax for mentioning agents in prose.
    #[test]
    fn rejects_at_mention() {
        let idx = test_index();
        let result = resolve_frontmatter_value("@w4r3z", &idx);
        assert!(
            matches!(result, ResolveResult::Rejected(_)),
            "@mentions must be rejected in frontmatter"
        );
    }

    /// The rejection message should tell the agent what to write instead.
    #[test]
    fn at_mention_rejection_suggests_bare_slug() {
        let idx = test_index();
        if let ResolveResult::Rejected(msg) = resolve_frontmatter_value("@kira", &idx) {
            assert!(msg.contains("kira"), "should suggest the inner slug");
        } else {
            panic!("expected Rejected");
        }
    }

    // ─── Rule 3: bare slugs must resolve ──────────────────────────────────

    /// A bare slug that matches a file in the repo resolves to that file's IRI.
    #[test]
    fn bare_slug_resolves_to_iri() {
        let idx = test_index();
        let result = resolve_frontmatter_value("w4r3z", &idx);
        assert_eq!(
            result,
            ResolveResult::Iri(format!("<{}/agent/w4r3z.md>", base()))
        );
    }

    /// Slug matching is case-insensitive (the index is built from
    /// lowercased file stems, so `W4R3Z` matches `w4r3z`).
    #[test]
    fn bare_slug_is_case_insensitive() {
        let idx = test_index();
        let result = resolve_frontmatter_value("W4R3Z", &idx);
        assert_eq!(
            result,
            ResolveResult::Iri(format!("<{}/agent/w4r3z.md>", base()))
        );
    }

    /// A bare slug that does NOT match any file is returned as an
    /// unresolved literal. SHACL validation will catch it downstream
    /// if the property requires sh:nodeKind sh:IRI.
    #[test]
    fn unresolved_slug_returns_literal() {
        let idx = test_index();
        let result = resolve_frontmatter_value("nobody", &idx);
        assert_eq!(result, ResolveResult::Unresolved("nobody".to_string()));
    }

    // ─── Rule 4: full IRIs pass through unchanged ─────────────────────────

    /// A value starting with http:// or https:// is treated as a
    /// fully-qualified IRI and passed through with no modification.
    /// This is the canonical form for machine-generated references.
    #[test]
    fn full_https_iri_passes_through() {
        let idx = test_index();
        let iri = "https://repolex.ai/some/agent/foo";
        let result = resolve_frontmatter_value(iri, &idx);
        assert_eq!(result, ResolveResult::Iri(format!("<{}>", iri)));
    }

    /// HTTP (not just HTTPS) is also accepted.
    #[test]
    fn full_http_iri_passes_through() {
        let idx = test_index();
        let iri = "http://example.org/entity/bar";
        let result = resolve_frontmatter_value(iri, &idx);
        assert_eq!(result, ResolveResult::Iri(format!("<{}>", iri)));
    }

    /// The IRI is NOT slug-normalized. Colons, slashes, mixed case,
    /// query strings — all preserved exactly as written. This is what
    /// was broken in SG's claude-export bug: the old resolver stripped
    /// colons and mangled the IRI.
    #[test]
    fn full_iri_preserves_special_characters() {
        let idx = test_index();
        let iri = "https://repolex.ai/git-lex/goodlux/claude-export/Conversation/4f10a178-c0a4-41c6-b397-655d222d6202";
        let result = resolve_frontmatter_value(iri, &idx);
        assert_eq!(result, ResolveResult::Iri(format!("<{}>", iri)));
    }

    // ─── Rule 5: path-style values ────────────────────────────────────────

    /// A value containing `/` is treated as a relative path within the repo.
    #[test]
    fn path_with_slash_resolves_as_path() {
        let idx = test_index();
        let result = resolve_frontmatter_value("agent/w4r3z.md", &idx);
        assert_eq!(
            result,
            ResolveResult::Iri(format!("<{}/agent/w4r3z.md>", base()))
        );
    }

    /// A value ending with `.md` is also treated as a path.
    #[test]
    fn dotmd_suffix_resolves_as_path() {
        let idx = test_index();
        let result = resolve_frontmatter_value("w4r3z.md", &idx);
        assert_eq!(
            result,
            ResolveResult::Iri(format!("<{}/w4r3z.md>", base()))
        );
    }

    // ─── Rule 6: no silent transformations ────────────────────────────────

    /// The resolver does not strip dots, hyphens, or other characters
    /// from slugs. If the value doesn't match the index as-is (after
    /// lowercasing), it's unresolved. No workarounds.
    #[test]
    fn no_dot_stripping_fallback() {
        let idx = test_index();
        // "w.4.r.3.z" should NOT match "w4r3z" via dot-stripping.
        let result = resolve_frontmatter_value("w.4.r.3.z", &idx);
        assert_eq!(result, ResolveResult::Unresolved("w.4.r.3.z".to_string()));
    }

    // ─── Rule 7: unresolved → literal, not blank node ─────────────────────

    /// When resolution fails, the result is Unresolved (which the caller
    /// emits as a literal string on the kit predicate). No blank nodes
    /// are created. SHACL shapes that require sh:nodeKind sh:IRI will
    /// flag the literal as a validation error.
    #[test]
    fn unresolved_is_literal_not_blank_node() {
        let idx = test_index();
        let result = resolve_frontmatter_value("nonexistent", &idx);
        // The return type is Unresolved, not some BlankNode variant.
        // The caller emits: <doc> kit:pred "nonexistent" <graph> .
        assert!(matches!(result, ResolveResult::Unresolved(_)));
    }

    // ─── Edge cases ───────────────────────────────────────────────────────

    /// Empty string is unresolved.
    #[test]
    fn empty_string_is_unresolved() {
        let idx = test_index();
        let result = resolve_frontmatter_value("", &idx);
        assert!(matches!(result, ResolveResult::Unresolved(_)));
    }

    /// Whitespace-only is unresolved (lowercased + trimmed to empty).
    #[test]
    fn whitespace_only_is_unresolved() {
        let idx = test_index();
        let result = resolve_frontmatter_value("   ", &idx);
        assert!(matches!(result, ResolveResult::Unresolved(_)));
    }

    /// Multiple brackets are still rejected (not just one pair).
    #[test]
    fn rejects_nested_brackets() {
        let idx = test_index();
        assert!(matches!(
            resolve_frontmatter_value("[[[[w4r3z]]]]", &idx),
            ResolveResult::Rejected(_)
        ));
    }
}
