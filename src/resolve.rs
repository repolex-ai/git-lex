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
//! 1. **No `[[wikilinks]]` in frontmatter.** If an agent writes
//!    `assignedTo: [[w4r3z]]` the resolver rejects it with a clear error.
//!    The correct form is the repo-relative path
//!    (`assignedTo: friend/w4r3z.md`).
//!
//! 2. **No `@mentions` in frontmatter.** The `@` prefix is prose syntax.
//!    `assignedTo: @w4r3z` is rejected — write the repo-relative path
//!    (`assignedTo: friend/w4r3z.md`) instead, per rule 3.
//!
//! 3. **Bare names are REJECTED** (Rob-ruled 2026-07-28). A reference is
//!    the document's repo-relative path (`friend/selkie.md`) or a full
//!    IRI — the graph never guesses which file a name means. The old
//!    bare-name search made history non-deterministic (resolution depended
//!    on which files existed at sync time).
//!
//! 3b. **The authored identifier form is `<namespace/Class/identifier>`**
//!    (Rob's notation ruling) — `<soul/Journal/day-7>`,
//!    `<copia/Texture/deep-water>`. The angle brackets are DELIMITERS, the
//!    same punctuation Turtle and SPARQL use for an address; they are
//!    stripped, never encoded into the id. It resolves against the ONE root
//!    (`git::RESOURCE_ROOT`) and NEVER against the writing document's kit
//!    base — that is the entire point of the form, and it is what makes a
//!    cross-kit reference possible. A bracketed value with no `/` is still a
//!    bare name and is still rejected under rule 3.
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
/// This is the ONLY resolution path for frontmatter ObjectProperty values.
/// Body links are standard markdown links, handled in extraction.rs
/// (emitting `linksTo`); the body wikilink/@mention resolvers this doc
/// once described are retired — `[[...]]` and `@name` in a body are
/// plain prose.
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

    // Rule 3b: the authored identifier form, `<namespace/Class/id>`.
    //
    // The angle brackets are DELIMITERS, not part of the identifier — the same
    // punctuation Turtle and SPARQL use to mean "this is an address". Their
    // whole purpose is that the namespace comes from the VALUE, so this form
    // resolves against the ONE root and NEVER against the writing document's
    // own kit base. Before this rule the brackets were percent-encoded into
    // the identifier and the wreckage glued under the document's kit, so
    // `<copia/Texture/deep-water>` in a soul Note landed at
    // `https://repolex.ai/soul/%3Ccopia/Texture/deep-water%3E` — silently, and
    // as a WRONG join rather than no join, which is the worse failure.
    //
    // This is the same assumption #104 removed from the predicate side —
    // "the document's kit is the right namespace" — still living in the
    // values. A value's namespace comes from the value.
    let trimmed = raw.trim();
    if let Some(inner) = trimmed.strip_prefix('<').and_then(|r| r.strip_suffix('>')) {
        let inner = inner.trim();
        if inner.is_empty() {
            return ResolveResult::Rejected(
                "empty identifier '<>' — write <namespace/Class/identifier>, \
                 e.g. <soul/Journal/day-7>".to_string(),
            );
        }
        // An absolute address inside brackets is just an absolute address.
        if inner.contains("://") {
            return ResolveResult::Iri(format!("<{}>", crate::nquad::uri_encode_url(inner)));
        }
        // A bare name in brackets is still a bare name (rule 3): the brackets
        // promise a namespace and a class, and one token supplies neither.
        if !inner.contains('/') {
            return ResolveResult::Rejected(format!(
                "'<{inner}>' names no namespace or class — write \
                 <namespace/Class/identifier>, e.g. <soul/Journal/day-7>. \
                 The graph never guesses which kit a bare name belongs to.",
            ));
        }
        return ResolveResult::Iri(format!(
            "<{}{}>",
            crate::git::RESOURCE_ROOT,
            crate::nquad::uri_encode_path(inner)
        ));
    }

    // Rule 4: full IRI passthrough — but percent-encode any character that
    // would make the IRI structurally invalid (spaces, parens, etc. that
    // appear in real-world URLs). Without this, oxigraph's strict NQuads
    // parser rejects entire history-walk batches when one external link
    // contains an unencoded character. `uri_encode_url` (not `_path`): an
    // already-encoded URL's `%XX` escapes pass through untouched — the old
    // unconditional `%`→`%25` silently rewrote `Caf%C3%A9` into a
    // different URL than the author wrote.
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return ResolveResult::Iri(format!("<{}>", crate::nquad::uri_encode_url(raw)));
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

    /// Rule 4: an ALREADY-ENCODED URL passes through byte-identical — its
    /// `%XX` escapes are not re-encoded (`%` → `%25` would mint a different
    /// URL than the author wrote). A bare `%` that is NOT a valid escape
    /// still encodes, keeping the emitted IRI parseable.
    #[test]
    fn full_iri_preserves_percent_encoding() {
        let iri = "https://en.wikipedia.org/wiki/Caf%C3%A9";
        assert_eq!(
            resolve_frontmatter_value(iri),
            ResolveResult::Iri(format!("<{}>", iri))
        );
        // Unencoded characters in the same URL still encode; valid escapes
        // stay untouched.
        assert_eq!(
            resolve_frontmatter_value("https://example.com/a%20b and c"),
            ResolveResult::Iri("<https://example.com/a%20b%20and%20c>".to_string())
        );
        // A stray `%` (not followed by two hex digits) is not a valid
        // escape — it encodes so the IRI stays structurally valid.
        assert_eq!(
            resolve_frontmatter_value("https://example.com/100%"),
            ResolveResult::Iri("<https://example.com/100%25>".to_string())
        );
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

#[cfg(test)]
mod authored_identifier_tests {
    use super::*;

    fn iri(raw: &str) -> String {
        match resolve_frontmatter_value(raw) {
            ResolveResult::Iri(s) => s,
            other => panic!("expected an IRI for {raw:?}, got {other:?}"),
        }
    }

    /// The brackets are DELIMITERS. They must not survive into the address.
    #[test]
    fn brackets_are_stripped_not_encoded() {
        let got = iri("<soul/Journal/day-7>");
        assert_eq!(got, "<https://repolex.ai/soul/Journal/day-7>");
        assert!(!got.contains("%3C") && !got.contains("%3E"), "{got}");
    }

    /// THE point of the form, and the bug that prompted it: the namespace
    /// comes from the VALUE, never from the document doing the writing. A
    /// copia Texture referenced from a soul Note is a copia Texture.
    #[test]
    fn the_namespace_comes_from_the_value_not_the_document() {
        assert_eq!(
            iri("<copia/Texture/deep-water>"),
            "<https://repolex.ai/copia/Texture/deep-water>"
        );
        assert_eq!(
            iri("<git-lex/File/Soul/Journal/day-1.md>"),
            "<https://repolex.ai/git-lex/File/Soul/Journal/day-1.md>"
        );
    }

    /// tr1p's invariant: for a document carrying `id`, the resolved id must
    /// be the same string as the document's own Thing subject. If these ever
    /// diverge, the keystone property points at a mangled twin of the very
    /// node it names.
    #[test]
    fn a_resolved_id_equals_the_thing_subject_it_names() {
        // What derive_file_subjects builds for a soul-kit repo: base + Class + id.
        let subject = "https://repolex.ai/soul/Note/thing-plane-first-flight";
        assert_eq!(iri("<soul/Note/thing-plane-first-flight>"), format!("<{subject}>"));
    }

    /// An absolute address inside brackets is just an absolute address.
    #[test]
    fn an_absolute_iri_in_brackets_passes_through() {
        assert_eq!(
            iri("<https://example.org/thing/1>"),
            "<https://example.org/thing/1>"
        );
    }

    /// Brackets promise a namespace and a class. One token supplies neither,
    /// so it stays a bare name and stays rejected (rule 3).
    #[test]
    fn a_bare_name_in_brackets_is_still_rejected() {
        match resolve_frontmatter_value("<w4r3z>") {
            ResolveResult::Rejected(msg) => {
                assert!(msg.contains("<soul/Journal/day-7>"), "should teach the form: {msg}");
            }
            other => panic!("expected rejection, got {other:?}"),
        }
        match resolve_frontmatter_value("<>") {
            ResolveResult::Rejected(_) => {}
            other => panic!("expected rejection of '<>', got {other:?}"),
        }
    }

    /// The unbracketed path form is UNCHANGED — it still resolves against the
    /// document's own repo base, because that is what a repo-relative path
    /// means. Only the bracketed form carries its own namespace.
    #[test]
    fn the_plain_path_form_is_untouched() {
        match resolve_frontmatter_value("Soul/Journal/day-7.md") {
            ResolveResult::Iri(s) => assert!(!s.contains("%3C"), "{s}"),
            other => panic!("expected an IRI, got {other:?}"),
        }
    }
}
