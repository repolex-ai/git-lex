//! Git operations and repo-identity helpers.
//!
//! Peeled out of `main.rs` during modularization. Everything here either
//! shells out to `git` or derives identity from the remote URL.

use std::fs;
use std::process::Command;

use git_lex::find_git_root;

/// Resolve the soul's genesis SHA (three tiers, ordered by cost) and make
/// sure `.lex/identity.yml` records it. Called ONCE per sync — identity.yml
/// is the machine-readable identity file downstream consumers (Pool's
/// boot-skip, federation readers) rely on. This replaces the old
/// `base_uri()` read-that-writes: IRIs no longer carry the SHA (Day-50);
/// identity lives here and as a `git:genesisSha` FACT on the repo node.
pub(crate) fn ensure_identity_yml() -> Option<String> {
    let sha = genesis_sha()?;
    // identity.yml is a CROSS-SYSTEM contract file (Pool boot-skip,
    // federation readers). git-lex itself would recover via the fallback
    // tiers, so a swallowed write error is invisible in-process while an
    // external consumer reads a missing/stale file — warn loudly.
    if let Err(e) = write_identity_yml_sha(&sha) {
        eprintln!("warning: could not write .lex/identity.yml: {} — downstream consumers (Pool, federation) read this file", e);
    }
    Some(sha)
}

/// The soul's genesis (first-commit) SHA, if resolvable.
pub(crate) fn genesis_sha() -> Option<String> {
    sha_from_identity_yml()
        .or_else(sha_from_repo_yml)
        .or_else(sha_from_git)
}

// ---------------------------------------------------------------------
// Task-2 IRI families (Day-50 decisions): graph names + instance subjects
// carry NO soul identity. The store is the scope; one query works against
// every soul's oxigraph.
// ---------------------------------------------------------------------

/// Graph-container names: soul-independent ABSOLUTE IRIs, identical across
/// every soul's store — `GRAPH <https://repolex.ai/git-lex/NamedGraph/now>` is the same
/// query everywhere. (A literally-bare name like `now` is not a legal IRI:
/// oxigraph rejects it at the model level and SPARQL won't parse `<now>` —
/// probed Day-50. Absolute-and-identical is the standard shape that delivers
/// the portability requirement.)
pub(crate) const GRAPH_BASE: &str = "https://repolex.ai/git-lex/NamedGraph/";

/// The a-box (instance) base for soul-repo subjects. Subtexture-wide shape
/// (Rob, Day-50): `https://repolex.ai/<application>/<Class>/<instanceId>` —
/// no base word; vocabulary stays under `https://repolex.ai/ontology/...`.
pub(crate) const SOUL_RESOURCE_BASE: &str = "https://repolex.ai/soul";

/// Mint a graph name: `https://repolex.ai/git-lex/NamedGraph/<name>`.
/// Graphs are instances of `git-lex:NamedGraph` (⊑ sd:NamedGraph, kit-base
/// 72be113) under the universal instance law — the git-lex application's
/// NamedGraph objects. The ontology graph's instance name is
/// `repo-ontology` (renamed from `ontology`, Rob Day-50).
pub(crate) fn graph_uri(name: &str) -> String {
    format!("{GRAPH_BASE}{name}")
}

/// Mint an instance-subject IRI under the soul a-box base.
///
/// - Empty path = the Self node: the namespace root itself
///   (`https://repolex.ai/soul`). Soul identity (genesis SHA) is a
///   FACT about the Self node, never part of any IRI.
/// - A tracked path under the `Soul/` scaffold root maps onto the namespace
///   root (`Soul/Memory/foo.md` → `…/soul/Memory/foo.md`) — the
///   `Soul/` folder IS the soul namespace, so it doesn't repeat.
/// - Everything else joins verbatim (`journal/day-1.md`, `commit/<sha>`,
///   `entity/<x>~<hash>`, …).
///
/// NOTE: file-derived subjects keep their extension (`.md`) — joins are by
/// filename (nquad.rs wikilink resolution + downstream resolvers exact-match
/// on it); see the Task-2 spec's ON HOLD ruling before ever changing that.
pub(crate) fn resource_uri(path: &str) -> String {
    if path.is_empty() {
        return SOUL_RESOURCE_BASE.to_string();
    }
    let tail = path.strip_prefix("Soul/").unwrap_or(path);
    format!("{SOUL_RESOURCE_BASE}/{tail}")
}
// ---------------------------------------------------------------------
// SHA resolution (three tiers, ordered by cost)
// ---------------------------------------------------------------------

/// Read first-commit SHA from `.lex/identity.yml` if present.
///
/// `.lex/identity.yml` is a write-once cache of the soul's genesis-SHA.
/// System-owned (NOT squaddie-editable like `repo.yml`). Path-level
/// separation makes the invariant visible.
fn sha_from_identity_yml() -> Option<String> {
    let root = find_git_root()?;
    let content = fs::read_to_string(root.join(".lex").join("identity.yml")).ok()?;
    for line in content.lines() {
        if let Some(rest) = line.trim().strip_prefix("genesis_sha:") {
            let sha = rest.trim();
            if is_valid_sha(sha) {
                return Some(sha.to_string());
            }
        }
    }
    None
}

/// First-commit SHA from `.lex/repo.yml`, via the ONE reader.
fn sha_from_repo_yml() -> Option<String> {
    let sha = git_lex::RepoYml::load(&find_git_root()?).first_commit?;
    is_valid_sha(&sha).then_some(sha)
}

/// Query git directly for the first commit SHA.
fn sha_from_git() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-list", "--max-parents=0", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    // Multiple root commits (rare) → take the LAST printed line, matching
    // git rev-list's default reverse-chronological order — earliest commit.
    let sha = raw.lines().last()?.trim();
    if is_valid_sha(sha) {
        Some(sha.to_string())
    } else {
        None
    }
}

/// Write the genesis SHA to `.lex/identity.yml`.
///
/// Three cases:
/// 1. File doesn't exist → write it fresh.
/// 2. File exists, has `genesis_sha:` line → no-op (already correct).
/// 3. File exists but has the legacy `identity:` key (from git-lex's
///    pre-7df99ce git-as-PKI era) → rewrite in place under the canonical
///    `genesis_sha:` key. Same SHA bytes, new label. The legacy `identity:`
///    line is dropped; everything else is preserved.
///
/// The "write once never modify" header in the file body refers to the
/// SHA *value*, not the file's existence — the genesis SHA is immutable,
/// but rewriting the key name when we discover the file's stuck on the
/// legacy schema is a fix, not a violation. Souls initialized pre-7df99ce
/// would otherwise be permanently stuck under sync-from / federation
/// readers that only know the new key.
fn write_identity_yml_sha(sha: &str) -> std::io::Result<()> {
    let root = match find_git_root() {
        Some(r) => r,
        None => return Ok(()),
    };
    let path = root.join(".lex").join("identity.yml");
    if path.exists() {
        let existing = fs::read_to_string(&path).unwrap_or_default();
        match identity_yml_rewrite_decision(&existing, sha) {
            None => Ok(()), // case 2: already correct
            Some(new_body) => fs::write(path, new_body), // case 3: legacy rewrite
        }
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, canonical_identity_yml(sha))
    }
}

/// Pure helper: given the existing identity.yml body and the canonical SHA,
/// return `Some(new_body)` if the file needs to be rewritten (legacy key
/// present, canonical missing), or `None` if it's already correct.
///
/// Preserves non-`identity:` lines from the original file so any comments
/// or future fields a soul added survive the migration.
fn identity_yml_rewrite_decision(existing: &str, sha: &str) -> Option<String> {
    let has_canonical = existing
        .lines()
        .any(|l| l.trim().starts_with("genesis_sha:"));
    if has_canonical {
        return None;
    }
    let mut body = canonical_identity_yml(sha);
    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("identity:") {
            continue; // drop legacy key
        }
        if body.contains(line) {
            continue; // don't double-write our own header lines
        }
        body.push_str(line);
        body.push('\n');
    }
    Some(body)
}

/// The canonical body for a fresh `.lex/identity.yml`.
fn canonical_identity_yml(sha: &str) -> String {
    format!(
        "# WRITE ONCE. NEVER MODIFY. Soul identity anchor.\n\
         # If you accidentally delete this file, the next `git lex sync`\n\
         # will recompute and rewrite it from the genesis commit.\n\
         genesis_sha: {}\n",
        sha
    )
}

/// Validate a SHA-1 as a SOUL IDENTITY ANCHOR: exactly 40 hex chars.
///
/// C6 fix (Day 38): this is the gate inside all three `sha_from_*` readers.
/// The genesis SHA is identity data — `.lex/identity.yml` + the
/// `git:genesisSha` fact (it no longer appears in any IRI, Day-50). A short
/// SHA here is still an IDENTITY-SPLIT risk across the Pool seam:
/// `pool sync-from` builds the Moments named-graph IRI from identity.yml's
/// genesis_sha, so a length disagreement between tiers would orphan a
/// soul's Moments in a differently-named graph (see the git-lex×Pool
/// compare, EDGE-2 / Pool #110). The anchor MUST be the full 40-hex SHA-1;
/// short forms are fine for display only.
fn is_valid_sha(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}
/// Stage and commit the working tree as a snapshot before a destructive
/// operation (e.g. `nuke`). The commit message includes `reason`.
///
/// No-op if there's nothing to commit (clean working tree) or if git is
/// unavailable. Errors are reported but not fatal — the destructive
/// operation can still proceed; we just won't have a snapshot.
pub(crate) fn auto_commit_snapshot(reason: &str) {
    // Check if we're in a git repo with commits
    let has_head = Command::new("git").args(["rev-parse", "HEAD"]).output()
        .map(|o| o.status.success()).unwrap_or(false);
    if !has_head {
        return; // No commits yet, nothing to snapshot
    }

    // Check if there's anything to commit
    let status = Command::new("git").args(["status", "--porcelain"]).output();
    let has_changes = match status {
        Ok(o) if o.status.success() => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        _ => return,
    };
    if !has_changes {
        return; // Clean working tree, nothing to snapshot
    }

    // Stage everything and commit
    let _ = Command::new("git").args(["add", "-A"]).status();
    let msg = format!("git lex auto-snapshot: {}", reason);
    let commit = Command::new("git").args(["commit", "-m", &msg, "--allow-empty"]).output();
    match commit {
        Ok(o) if o.status.success() => {
            println!("Auto-committed working tree before {}.", reason);
        }
        _ => {
            // Not fatal — maybe pre-commit hook failed, maybe nothing
            // actually staged (hidden files?). Just report and continue.
            eprintln!("Warning: auto-commit before {} did not succeed. Continuing.", reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SHA: &str = "700c5bd401abcd02abcd03abcd04abcd05abcd06";

    // C6 regression (Day 38): the identity anchor must be EXACTLY 40 hex —
    // a short SHA must NOT validate, or the same soul can get two base IRIs
    // (and Pool would orphan its Moments in a differently-named graph).
    #[test]
    fn full_40_hex_sha_is_valid() {
        assert!(is_valid_sha(TEST_SHA));
        assert_eq!(TEST_SHA.len(), 40);
    }

    #[test]
    fn short_sha_is_rejected_as_identity_anchor() {
        assert!(!is_valid_sha("700c5bd"));        // 7-char short form
        assert!(!is_valid_sha("700c5bd401abcd")); // 14-char
    }

    #[test]
    fn non_hex_and_overlong_are_rejected() {
        assert!(!is_valid_sha(""));                                          // empty
        assert!(!is_valid_sha("700c5bd401abcd02abcd03abcd04abcd05abcd0g"));  // 40 with 'g'
        assert!(!is_valid_sha("700c5bd401abcd02abcd03abcd04abcd05abcd06a")); // 41 hex
    }

    #[test]
    fn canonical_file_is_a_noop() {
        // Case 2: file already has the canonical key. No rewrite needed.
        let existing = canonical_identity_yml(TEST_SHA);
        let decision = identity_yml_rewrite_decision(&existing, TEST_SHA);
        assert!(decision.is_none(), "canonical file should not be rewritten");
    }

    #[test]
    fn legacy_identity_key_gets_migrated() {
        // Case 3: pre-7df99ce souls have `identity: <sha>` instead of
        // `genesis_sha: <sha>`. Must rewrite to canonical form.
        let legacy = format!("identity: {}\n", TEST_SHA);
        let decision = identity_yml_rewrite_decision(&legacy, TEST_SHA);
        let rewritten = decision.expect("legacy file must be rewritten");
        assert!(
            rewritten.contains(&format!("genesis_sha: {}", TEST_SHA)),
            "rewrite must contain canonical key: {}",
            rewritten
        );
        assert!(
            !rewritten.contains("identity:"),
            "rewrite must NOT contain legacy `identity:` line: {}",
            rewritten
        );
    }

    #[test]
    fn migration_preserves_extra_lines() {
        // If a soul or future git-lex added other fields (comments, metadata),
        // they survive the migration. Only `identity:` is dropped.
        let mixed = format!(
            "# my custom comment\n\
             identity: {}\n\
             custom_field: hello\n",
            TEST_SHA
        );
        let rewritten = identity_yml_rewrite_decision(&mixed, TEST_SHA)
            .expect("should rewrite");
        assert!(rewritten.contains("custom_field: hello"), "{}", rewritten);
        assert!(rewritten.contains("# my custom comment"), "{}", rewritten);
        assert!(rewritten.contains(&format!("genesis_sha: {}", TEST_SHA)));
        assert!(!rewritten.contains("identity:"));
    }

    #[test]
    fn empty_or_garbage_file_gets_canonical_body() {
        // If someone deleted the file body but the file exists, treat as
        // legacy and rewrite — better to write the canonical body than to
        // leave a soul stuck.
        let empty = "";
        let rewritten = identity_yml_rewrite_decision(empty, TEST_SHA)
            .expect("empty file must be rewritten");
        assert!(rewritten.contains(&format!("genesis_sha: {}", TEST_SHA)));
    }

    #[test]
    fn both_keys_present_is_a_noop() {
        // Defensive: if a file somehow has BOTH keys (manual edit by a
        // confused squaddie), trust the canonical one and leave it alone.
        // We don't want to thrash if both already exist.
        let both = format!(
            "identity: {sha}\n\
             genesis_sha: {sha}\n",
            sha = TEST_SHA
        );
        let decision = identity_yml_rewrite_decision(&both, TEST_SHA);
        assert!(decision.is_none(), "both-keys present should be no-op");
    }
}
