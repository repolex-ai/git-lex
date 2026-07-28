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
//! 3. **Bare names are REJECTED** (Rob-ruled 2026-07-28). A reference is
//!    the document's repo-relative path (`friend/selkie.md`) or a full
//!    IRI — the graph never guesses which file a name means. The old
//!    bare-name search made history non-deterministic (resolution depended
//!    on which files existed at sync time).
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
//! 6. **No transformations, ever.** No slugifying, no case folding, no
//!    dot-stripping, no character removal. The written value is either a
//!    valid form or it is rejected with the fix spelled out.
//!
//! 7. **Unresolved values become literals, not blank nodes.** When a
//!    frontmatter ObjectProperty value can't be resolved, it's emitted
//!    as a literal string on the kit predicate. This preserves the
//!    author's intent (what they typed) without introducing blank nodes
//!    into the graph. SHACL shapes that declare `sh:nodeKind sh:IRI`
//!    will flag these literals as validation errors, telling the agent
//!    to fix their data.


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
pub fn resolve_frontmatter_value(raw: &str) -> ResolveResult {
    // Rule 1: reject [[wikilinks]]
    if raw.starts_with("[[") || raw.ends_with("]]") {
        let inner = raw
            .trim_start_matches("[[")
            .trim_end_matches("]]");
        return ResolveResult::Rejected(format!(
            "wikilink syntax [[...]] is not allowed in frontmatter. \
             Write the repo-relative path instead (e.g. {}.md with its folder)",
            inner
        ));
    }

    // Rule 2: reject @mentions
    if raw.starts_with('@') {
        let inner = &raw[1..];
        return ResolveResult::Rejected(format!(
            "@mention syntax is not allowed in frontmatter. \
             Write the repo-relative path instead (e.g. {}.md with its folder)",
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

    // Rule 3 (Rob-ruled 2026-07-28, replacing the bare-name lookup): a
    // reference is the document's REPO-RELATIVE PATH or a full IRI — the
    // graph never guesses. The old rule searched the repo for a file
    // matching the bare name, which made resolution depend on which files
    // existed at sync time (non-deterministic history, silent rebinding
    // when a same-named file appeared). Rejected with the fix spelled out.
    if raw.trim().is_empty() {
        return ResolveResult::Unresolved(raw.to_string());
    }
    ResolveResult::Rejected(format!(
        "bare name '{raw}' — write the repo-relative path instead (e.g. \
         friend/{raw}.md). References are paths or IRIs; the graph never \
         guesses which file a name means.",
    ))
}

// ════════════════════════════════════════════════════════════════════════════
// Tests — these ARE the rules documentation. Each test name and doc comment
// states a rule; the test body proves the code enforces it.
// ════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;


    // The a-box base is DERIVED per repo (kit short / repo name — never
    // hardcoded); tests assert resolution BEHAVIOR against whatever base
    // this checkout derives, not a pinned literal.
    fn base() -> String {
        crate::git::resource_uri("")
    }

    // ─── Rule 1: no wikilinks in frontmatter ──────────────────────────────

    /// Frontmatter ObjectProperty values must not use [[wikilink]] syntax.
    /// Wikilinks are body-text syntax for cross-referencing pages.
    /// The agent should write the repo-relative path instead.
    #[test]
    fn rejects_wikilink_brackets() {
        let result = resolve_frontmatter_value("[[w4r3z]]");
        assert!(
            matches!(result, ResolveResult::Rejected(_)),
            "[[wikilinks]] must be rejected in frontmatter"
        );
    }

    /// Rule 1: the wikilink rejection tells the writer to use a path.
    #[test]
    fn wikilink_rejection_suggests_a_path() {
        match resolve_frontmatter_value("[[w4r3z]]") {
            ResolveResult::Rejected(msg) => assert!(msg.contains("path")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    /// Even if the brackets contain a valid slug, it's still rejected.
    /// The brackets are the problem, not the content.
    #[test]
    fn rejects_wikilink_even_when_slug_exists() {
        assert!(matches!(
            resolve_frontmatter_value("[[kira]]"),
            ResolveResult::Rejected(_)
        ));
    }

    // ─── Rule 2: no @mentions in frontmatter ──────────────────────────────

    /// Frontmatter ObjectProperty values must not use @mention syntax.
    /// The @ prefix is body-text syntax for mentioning agents in prose.
    #[test]
    fn rejects_at_mention() {
        let result = resolve_frontmatter_value("@w4r3z");
        assert!(
            matches!(result, ResolveResult::Rejected(_)),
            "@mentions must be rejected in frontmatter"
        );
    }

    /// The rejection message should tell the agent what to write instead.
    #[test]
    fn at_mention_rejection_suggests_bare_slug() {
        if let ResolveResult::Rejected(msg) = resolve_frontmatter_value("@kira") {
            assert!(msg.contains("kira"), "should suggest the inner slug");
        } else {
            panic!("expected Rejected");
        }
    }

    // ─── Rule 3: bare slugs must resolve ──────────────────────────────────


    /// Slug matching is case-insensitive (the index is built from

    /// A bare slug that does NOT match any file is returned as an
    /// unresolved literal. SHACL validation will catch it downstream

    // ─── Rule 4: full IRIs pass through unchanged ─────────────────────────

    /// A value starting with http:// or https:// is treated as a
    /// fully-qualified IRI and passed through with no modification.
    /// This is the canonical form for machine-generated references.
    #[test]
    fn full_https_iri_passes_through() {
        let iri = "https://repolex.ai/some/agent/foo";
        let result = resolve_frontmatter_value(iri);
        assert_eq!(result, ResolveResult::Iri(format!("<{}>", iri)));
    }

    /// HTTP (not just HTTPS) is also accepted.
    #[test]
    fn full_http_iri_passes_through() {
        let iri = "http://example.org/entity/bar";
        let result = resolve_frontmatter_value(iri);
        assert_eq!(result, ResolveResult::Iri(format!("<{}>", iri)));
    }

    /// The IRI is NOT slug-normalized. Colons, slashes, mixed case,
    /// query strings — all preserved exactly as written. This is what
    /// was broken in SG's claude-export bug: the old resolver stripped
    /// colons and mangled the IRI.
    #[test]
    fn full_iri_preserves_special_characters() {
        let iri = "https://repolex.ai/git-lex/goodlux/claude-export/Conversation/4f10a178-c0a4-41c6-b397-655d222d6202";
        let result = resolve_frontmatter_value(iri);
        assert_eq!(result, ResolveResult::Iri(format!("<{}>", iri)));
    }

    // ─── Rule 5: path-style values ────────────────────────────────────────

    /// A value containing `/` is treated as a relative path within the repo.
    #[test]
    fn path_with_slash_resolves_as_path() {
        let result = resolve_frontmatter_value("agent/w4r3z.md");
        assert_eq!(
            result,
            ResolveResult::Iri(format!("<{}/agent/w4r3z.md>", base()))
        );
    }

    /// A value ending with `.md` is also treated as a path.
    #[test]
    fn dotmd_suffix_resolves_as_path() {
        let result = resolve_frontmatter_value("w4r3z.md");
        assert_eq!(
            result,
            ResolveResult::Iri(format!("<{}/w4r3z.md>", base()))
        );
    }

    // ─── Rule 6: no silent transformations ────────────────────────────────

    /// The resolver does not strip dots, hyphens, or other characters
    /// from slugs. If the value doesn't match the index as-is (after

    // ─── Rule 7: unresolved → literal, not blank node ─────────────────────

    /// When resolution fails, the result is Unresolved (which the caller
    /// emits as a literal string on the kit predicate). No blank nodes
    /// are created. SHACL shapes that require sh:nodeKind sh:IRI will

    // ─── Edge cases ───────────────────────────────────────────────────────

    /// Empty string is unresolved.
    #[test]
    fn empty_string_is_unresolved() {
        let result = resolve_frontmatter_value("");
        assert!(matches!(result, ResolveResult::Unresolved(_)));
    }

    /// Whitespace-only is unresolved (lowercased + trimmed to empty).
    #[test]
    fn whitespace_only_is_unresolved() {
        let result = resolve_frontmatter_value("   ");
        assert!(matches!(result, ResolveResult::Unresolved(_)));
    }

    /// Multiple brackets are still rejected (not just one pair).
    #[test]
    fn rejects_nested_brackets() {
        assert!(matches!(
            resolve_frontmatter_value("[[[[w4r3z]]]]"),
            ResolveResult::Rejected(_)
        ));
    }
    /// Rule 3: a bare name is rejected even when a matching file EXISTS —
    /// the graph never guesses, full stop.
    #[test]
    fn bare_name_is_rejected_even_with_a_matching_file() {
        match resolve_frontmatter_value("w4r3z") {
            ResolveResult::Rejected(msg) => {
                assert!(msg.contains("agent") || msg.contains("path"), "fix-it message: {msg}");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    /// Rule 3: the rejection names the value and tells the writer what to
    /// write instead.
    #[test]
    fn bare_name_rejection_spells_out_the_fix() {
        match resolve_frontmatter_value("selkie") {
            ResolveResult::Rejected(msg) => {
                assert!(msg.contains("selkie"));
                assert!(msg.contains("path"));
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    /// Paths keep working without any index lookup — deterministic at
    /// every commit whether or not the file exists yet (forward
    /// references are legal; dangling ones warn at validate).
    #[test]
    fn path_resolves_without_index() {
        match resolve_frontmatter_value("friend/selkie.md") {
            ResolveResult::Iri(iri) => assert!(iri.ends_with("/friend/selkie.md>")),
            other => panic!("expected Iri, got {other:?}"),
        }
    }

}
