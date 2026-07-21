//! ╔══════════════════════════════════════════════════════════════════════╗
//! ║  DEPRECATED LEGACY-HISTORY SHIM — QUARANTINED. DO NOT EXTEND.        ║
//! ╚══════════════════════════════════════════════════════════════════════╝
//!
//! Recognition of RETIRED `.spo` sidecar formats that exist only in the git
//! HISTORY of the ~10 repos that were alive while git-lex itself was being
//! built (our squad's souls). New repos never contain these — no current
//! extractor writes them — but git history is immutable, so the one-graph
//! walker meets them forever when walking those repos' early commits.
//!
//! This is an IMPLEMENTATION DETAIL of our own history, not a feature
//! (Rob-ruled 2026-07-21): "clearly marked, DEPRECATED, and eventually
//! removed from the codebase." When the last pre-cutover soul repo is
//! re-walked for the final time (or its early history is no longer walked),
//! delete this module and the walker's call into it.
//!
//! What it recognizes:
//!   - The retired `.md.spo` BODY-EXTRACT format (~ April 2026): extracted
//!     body content keyed by an `@<filename>` first field, e.g.
//!         `@SOUL.md | mentions | 1ux`
//!     Resolving these through the modern emitter fabricates junk predicates
//!     (`git-lex:fm/@SOUL.md` …) — triage BUG 3. The walker SKIPS these
//!     lines, COUNTED in the completeness accounting, never silently.

/// Is this sidecar line in the retired `@<filename>`-keyed body-extract
/// format? (First pipe-field starts with `@`.)
pub(crate) fn is_retired_body_extract_line(line: &str) -> bool {
    line.trim_start().starts_with('@')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_retired_body_extract() {
        assert!(is_retired_body_extract_line("@SOUL.md | mentions | 1ux"));
        assert!(is_retired_body_extract_line("  @note/w4r3z-identity.md | tag | repolex"));
    }

    #[test]
    fn passes_modern_lines() {
        assert!(!is_retired_body_extract_line("soul.Journal.soulDay | hasValue | 53"));
        assert!(!is_retired_body_extract_line("md.externalLink | hasValue | https://x.com"));
        assert!(!is_retired_body_extract_line(""));
    }
}
