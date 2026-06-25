//! Git operations and repo-identity helpers.
//!
//! Peeled out of `main.rs` during modularization. Everything here either
//! shells out to `git` or derives identity from the remote URL.

use std::fs;
use std::process::Command;

use git_lex::find_git_root;

/// Parse the git remote URL into (host, org, repo) components.
/// Falls back to ("localhost", "local", directory_name) if no remote.
pub(crate) fn get_repo_parts() -> (String, String, String) {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output();
    if let Ok(o) = output {
        if o.status.success() {
            let url = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let stripped = url.strip_suffix(".git").unwrap_or(&url);

            // HTTPS: https://github.com/org/repo
            if stripped.starts_with("https://") || stripped.starts_with("http://") {
                let without_scheme = stripped.split("://").nth(1).unwrap_or(stripped);
                let parts: Vec<&str> = without_scheme.splitn(4, '/').collect();
                if parts.len() >= 3 {
                    return (parts[0].to_string(), parts[1].to_string(), parts[2].to_string());
                }
            }

            // SSH: git@github.com:org/repo
            if let Some(at_pos) = stripped.find('@') {
                let after_at = &stripped[at_pos + 1..];
                if let Some(colon_pos) = after_at.find(':') {
                    let host = &after_at[..colon_pos];
                    let path = &after_at[colon_pos + 1..];
                    let parts: Vec<&str> = path.splitn(2, '/').collect();
                    if parts.len() == 2 {
                        return (host.to_string(), parts[0].to_string(), parts[1].to_string());
                    }
                }
            }
        }
    }
    // Fallback
    let dir_name = find_git_root()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "unknown".to_string());
    ("localhost".to_string(), "local".to_string(), dir_name)
}

/// Get the repo identifier (org/name) from the git remote, or fall back to directory name.
pub(crate) fn get_repo_id() -> String {
    let (_, org, repo) = get_repo_parts();
    format!("{}/{}", org, repo)
}

/// Build the repo-level RDF base namespace.
///
/// **URN-emitting form (preferred):** `urn:soul:<first-commit-sha>` —
/// anchored on the soul-repo's genesis commit SHA (immutable across
/// re-clones, host migrations, SSH alias changes). The same anchor the
/// `o:` ontology prefix already uses (`lib.rs:222-229`).
///
/// **Legacy fallback:** `https://<host>/<org>/<repo>` — for pre-genesis
/// repos (no first commit yet) or non-git contexts (tests, fixtures).
/// Squaddies in production stores should never see this form once the
/// repo has its first commit.
///
/// Three-tier SHA resolution, ordered by cost:
/// 1. `.lex/identity.yml` cache (write-once at init/sync, fastest read)
/// 2. `.lex/repo.yml` cache (already there for the `o:` ontology prefix)
/// 3. `git rev-list --max-parents=0 HEAD` (live query — slowest)
///
/// Callers MUST use [`iri_join`] (or `<{}/path>` for the legacy form) to
/// concatenate a path to the base — the URN form requires `:` as the
/// separator, the legacy form uses `/`. See [`iri_join`] for the rule.
// QUESTION(w4r3z, Day 38): base_uri() is a READ that WRITES — it calls
// write_identity_yml_sha() on every invocation (3 sites below). base_uri() is
// hot (every nquad emit, every query prefix). Two concerns: (1) a save and a
// query running concurrently could race on identity.yml writes; (2) a read
// fn with a filesystem side-effect is surprising. Consider writing identity.yml
// ONCE at init/sync and making base_uri() pure-read. The write-back was likely
// added to self-heal a missing/legacy identity.yml, but that healing belongs in
// sync, not in every base_uri() call.
pub(crate) fn base_uri() -> String {
    if let Some(sha) = sha_from_identity_yml() {
        let _ = write_identity_yml_sha(&sha);
        return format!("urn:soul:{}", sha);
    }
    if let Some(sha) = sha_from_repo_yml() {
        let _ = write_identity_yml_sha(&sha);
        return format!("urn:soul:{}", sha);
    }
    if let Some(sha) = sha_from_git() {
        let _ = write_identity_yml_sha(&sha);
        return format!("urn:soul:{}", sha);
    }
    legacy_host_derived_base()
}

/// Join a path to a base IRI using the correct separator.
///
/// For URN-emitting `urn:soul:<sha>` bases, paths join with `:` to preserve
/// the bare-IRI-is-Self recursion (`urn:soul:<sha>:Soul/Note/foo`). For
/// the legacy host-derived form, paths join with `/` (the historical
/// `https://host/org/repo/Soul/Note/foo` shape).
///
/// Both call sites that use this helper get the right thing without
/// knowing which mode is active. This is the difference between subjects
/// being addressable across re-clones (URN form) vs identity changing
/// every time a remote alias changes (legacy).
///
/// Empty `path` returns the base verbatim (caller wants the bare Self).
pub(crate) fn iri_join(base: &str, path: &str) -> String {
    if path.is_empty() {
        return base.to_string();
    }
    if base.starts_with("urn:") {
        format!("{}:{}", base, path)
    } else {
        format!("{}/{}", base, path)
    }
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

/// Read first-commit SHA from `.lex/repo.yml`. Already persisted there for
/// the `o:` ontology prefix path. Cheap to keep reading.
fn sha_from_repo_yml() -> Option<String> {
    let root = find_git_root()?;
    let content = fs::read_to_string(root.join(".lex").join("repo.yml")).ok()?;
    for line in content.lines() {
        if let Some(rest) = line.trim().strip_prefix("first_commit:") {
            let sha = rest.trim();
            if is_valid_sha(sha) {
                return Some(sha.to_string());
            }
        }
    }
    None
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

/// Validate SHA-1 hex shape. Accept full (40-char) and short (>=4) — but
/// the URN form WRITES full. Validation is shape-only.
// FIXME(w4r3z, Day 38): accepting short SHAs here is an IDENTITY-SPLIT risk.
// base_uri() builds `urn:soul:<sha>` from whatever this validates. If repo.yml
// holds a truncated `first_commit:` (short SHA) while identity.yml holds the
// full one, the three-tier resolution can emit `urn:soul:<short>` from one tier
// and `urn:soul:<full>` from another → the SAME soul gets TWO different base
// IRIs, and its subjects silently split across them (queries miss half). For an
// identity anchor, validation should require EXACTLY 40 hex chars (full SHA-1)
// — short forms are fine for display but must never become the urn: base.
// (Also note: this fn is flagged unused by the compiler in some builds — confirm
// it's actually on the live path before relying on it as the gate.)
fn is_valid_sha(s: &str) -> bool {
    !s.is_empty() && s.len() <= 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Pre-genesis / non-git fallback. Returns the OLD `https://host/org/repo`
/// form. Mostly hit during early init (before the first commit lands) or
/// in non-repo contexts (tests, kit fixtures).
fn legacy_host_derived_base() -> String {
    let (host, org, repo) = get_repo_parts();
    format!("https://{}/{}/{}", host, org, repo)
}

/// Unescape a git-quoted path.
/// Git wraps paths with non-ASCII chars in double quotes and uses octal escapes.
/// e.g. "message/list_messages-\342\200\224-foo.md" → message/list_messages-—-foo.md
pub(crate) fn git_unescape_path(s: &str) -> String {
    let s = if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        &s[1..s.len() - 1]
    } else {
        return s.to_string();
    };
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'n' => { result.push(b'\n'); i += 2; }
                b't' => { result.push(b'\t'); i += 2; }
                b'r' => { result.push(b'\r'); i += 2; }
                b'\\' => { result.push(b'\\'); i += 2; }
                b'"' => { result.push(b'"'); i += 2; }
                // Octal escape: \NNN
                d if d.is_ascii_digit() && i + 3 < bytes.len()
                    && bytes[i + 2].is_ascii_digit()
                    && bytes[i + 3].is_ascii_digit() =>
                {
                    let octal = (d - b'0') as u32 * 64
                        + (bytes[i + 2] - b'0') as u32 * 8
                        + (bytes[i + 3] - b'0') as u32;
                    result.push(octal as u8);
                    i += 4;
                }
                _ => { result.push(bytes[i]); i += 1; }
            }
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&result).into_owned()
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
