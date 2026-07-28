//! Git hook management.
//!
//! git-lex needs to run extraction + validation before every commit. Rather
//! than exposing `extract` and `validate` as CLI commands that agents misuse,
//! we install a managed section in the git pre-commit hook.
//!
//! Respects `core.hooksPath` (used by husky, lefthook, etc.) — if set, we
//! write into that directory instead of `.git/hooks/`.
//!
//! NOTE: this module is POSIX-only. The managed hook is a `#!/bin/sh` script
//! and the executable bit is set via unix PermissionsExt under `#[cfg(unix)]`
//! (no Windows branch), so the commit-time extract+validate gate does not
//! install on Windows. Documented as a platform limitation (macOS/Linux);
//! a Windows hook path would be additive if ever needed.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use git_lex::find_git_root;

const MARKER_START: &str = "# --- git-lex managed (do not edit this section) ---";
const MARKER_END: &str = "# --- end git-lex managed ---";

// The pre-commit hook runs from the user's shell environment, not from
// Claude Code's plugin-augmented PATH. So we can't assume `git-lex` is on
// PATH — judges and other users may have only the bundled binary from the
// subtext plugin installed. The hook resolves git-lex in this order:
//
//   1. `$PATH` — for users with a system install (cargo, brew, etc.)
//   2. Newest `~/.claude/plugins/cache/repolex/subtext/*/bin/git-lex`
//      symlink — populated by the plugin's host-binaries setup on MCP start
//   3. Newest `~/.claude/plugins/cache/repolex/subtext/*/bin/.platforms/*/git-lex`
//      direct — covers the case where the symlink hasn't been created yet
//      because no Claude Code session has booted the plugin in this version
//      directory
//
// On miss: print a clear install-path message and fail the commit.
const MANAGED_SECTION: &str = "\
# --- git-lex managed (do not edit this section) ---
_glx=\"\"
if command -v git-lex >/dev/null 2>&1; then
    _glx=git-lex
else
    for d in \"$HOME\"/.claude/plugins/cache/repolex/subtext/*/bin; do
        if [ -x \"$d/git-lex\" ]; then _glx=\"$d/git-lex\"; fi
    done
    if [ -z \"$_glx\" ]; then
        for d in \"$HOME\"/.claude/plugins/cache/repolex/subtext/*/bin/.platforms/*; do
            if [ -x \"$d/git-lex\" ]; then _glx=\"$d/git-lex\"; fi
        done
    fi
fi
if [ -z \"$_glx\" ]; then
    echo \"git-lex not found on PATH or in ~/.claude/plugins/cache/repolex/subtext/*/bin/.\" >&2
    echo \"Install the subtext plugin (claude code: /plugin install subtext@repolex) or build from source.\" >&2
    exit 1
fi
\"$_glx\" hook pre-commit || exit 1
# --- end git-lex managed ---";

/// Find where git looks for hooks. Checks `core.hooksPath` first,
/// falls back to `.git/hooks/`.
fn hooks_dir() -> Option<PathBuf> {
    let root = find_git_root()?;

    // Check if core.hooksPath is set
    let output = Command::new("git")
        .args(["config", "core.hooksPath"])
        .output()
        .ok()?;

    if output.status.success() {
        let custom = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !custom.is_empty() {
            let path = PathBuf::from(&custom);
            // Could be absolute or relative to repo root
            if path.is_absolute() {
                return Some(path);
            } else {
                return Some(root.join(path));
            }
        }
    }

    Some(root.join(".git").join("hooks"))
}

/// Install or update the git-lex managed section in the pre-commit hook.
/// Preserves any existing user content in the file.
///
/// Returns Err when the hook could not be installed — the hook IS the
/// extract+validate enforcement gate, so a swallowed failure here means
/// every later commit silently skips extraction and SHACL validation.
/// Callers must surface the error instead of printing success.
/// Hook lines written by pre-marker-era git-lex (before the managed
/// section existed). Scrubbed on install so upgrading an old repo REPLACES
/// them — otherwise the broken legacy lines (`git-lex extract` /
/// `git-lex validate` are no longer subcommands) would keep failing every
/// commit above a perfectly good managed section.
const LEGACY_HOOK_LINES: &[&str] = &[
    "git-lex extract",
    "git lex extract",
    "git add .lex/extract/ 2>/dev/null",
    "git add .lex/extract/",
    "git-lex validate || exit 1",
    "git lex validate || exit 1",
];

/// Remove exact legacy git-lex lines from a hook script, preserving
/// everything else (a user's own hook content is never touched).
fn scrub_legacy_lines(content: &str) -> String {
    content
        .lines()
        .filter(|l| !LEGACY_HOOK_LINES.contains(&l.trim()))
        .map(|l| format!("{l}\n"))
        .collect()
}

pub(crate) fn install_hook() -> std::io::Result<()> {
    let dir = hooks_dir().ok_or_else(|| std::io::Error::other(
        "could not locate .git/hooks (not a git repo?)",
    ))?;
    fs::create_dir_all(&dir)?;

    let hook_path = dir.join("pre-commit");
    let existing = fs::read_to_string(&hook_path).unwrap_or_default();

    // Markers are detected as EXACT lines, never substrings: a commented-out
    // marker ('## --- git-lex managed …') must not count as "section
    // present" — that combination made install report success while
    // installing nothing, silently disabling the extract+validate gate
    // (adversarial finding 2b).
    let starts = existing.lines().filter(|l| l.trim() == MARKER_START).count();
    let ends = existing.lines().filter(|l| l.trim() == MARKER_END).count();
    if starts != ends || starts > 1 {
        // Mangled managed section (deleted marker line, merge damage,
        // duplicated section). Rewriting through it risks destroying user
        // hook content (finding 2a) — refuse loudly instead.
        return Err(std::io::Error::other(format!(
            "pre-commit hook at {} has a damaged git-lex managed section \
             ({starts} start marker(s), {ends} end marker(s)); fix or delete \
             the hook file and re-run",
            hook_path.display()
        )));
    }

    let new_content = if existing.is_empty() {
        // No existing hook — create fresh
        format!("#!/bin/sh\n{}\n", MANAGED_SECTION)
    } else if starts == 1 {
        // Already has our section — replace it
        replace_managed_section(&existing, MANAGED_SECTION)
    } else {
        // Existing hook without our section: scrub any pre-marker-era
        // git-lex lines, then append the managed section.
        let scrubbed = scrub_legacy_lines(&existing);
        let rest = scrubbed.trim();
        if rest.is_empty() || rest == "#!/bin/sh" || rest == "#!/bin/bash" {
            // The old hook was entirely ours — start fresh.
            format!("#!/bin/sh\n{}\n", MANAGED_SECTION)
        } else {
            let mut content = scrubbed;
            if !content.ends_with('\n') {
                content.push('\n');
            }
            content.push('\n');
            content.push_str(MANAGED_SECTION);
            content.push('\n');
            content
        }
    };

    fs::write(&hook_path, &new_content)?;

    // Ensure executable
    #[cfg(unix)]
    {
        let meta = fs::metadata(&hook_path)?;
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o111);
        fs::set_permissions(&hook_path, perms)?;
    }
    Ok(())
}

/// Remove the git-lex managed section from the pre-commit hook.
/// If the file only contained our section (plus shebang), removes the file entirely.
pub(crate) fn remove_hook() {
    let dir = match hooks_dir() {
        Some(d) => d,
        None => return,
    };

    let hook_path = dir.join("pre-commit");
    let existing = match fs::read_to_string(&hook_path) {
        Ok(s) => s,
        Err(_) => return,
    };

    if !existing.contains(MARKER_START) {
        return; // nothing to remove
    }

    let cleaned = replace_managed_section(&existing, "");
    let trimmed = cleaned.trim();

    // If only the shebang remains, remove the file entirely
    if trimmed.is_empty() || trimmed == "#!/bin/sh" || trimmed == "#!/bin/bash" {
        fs::remove_file(&hook_path).ok();
    } else {
        fs::write(&hook_path, &cleaned).ok();
    }
}

/// Replace the managed section between markers with new content.
/// If replacement is empty, removes the section entirely.
fn replace_managed_section(content: &str, replacement: &str) -> String {
    let mut result = String::new();
    let mut in_section = false;

    for line in content.lines() {
        if line.trim() == MARKER_START {
            in_section = true;
            if !replacement.is_empty() {
                result.push_str(replacement);
                result.push('\n');
            }
            continue;
        }
        if line.trim() == MARKER_END {
            in_section = false;
            continue;
        }
        if !in_section {
            result.push_str(line);
            result.push('\n');
        }
    }

    result
}

#[cfg(test)]
mod hook_convergence_tests {
    use super::*;

    /// The exact pre-marker-era hook found live in W4R3Z's repo on
    /// 2026-07-28 — the shape that broke every save after the binary
    /// upgrade removed the extract/validate subcommands.
    const ANCIENT_HOOK: &str = "#!/bin/sh\n\
        git-lex extract\n\
        git add .lex/extract/ 2>/dev/null\n\
        git-lex validate || exit 1\n";

    #[test]
    fn ancient_hook_lines_are_scrubbed_entirely() {
        let scrubbed = scrub_legacy_lines(ANCIENT_HOOK);
        assert!(!scrubbed.contains("git-lex extract"));
        assert!(!scrubbed.contains("git-lex validate"));
        assert!(!scrubbed.contains("git add .lex/extract/"));
        assert_eq!(scrubbed.trim(), "#!/bin/sh");
    }

    #[test]
    fn user_content_survives_the_scrub() {
        let mixed = "#!/bin/sh\nmy-own-linter --check\ngit-lex extract\n";
        let scrubbed = scrub_legacy_lines(mixed);
        assert!(scrubbed.contains("my-own-linter --check"));
        assert!(!scrubbed.contains("git-lex extract"));
    }

    #[test]
    fn managed_section_replacement_is_idempotent() {
        let once = format!("#!/bin/sh\n{}\n", MANAGED_SECTION);
        let twice = replace_managed_section(&once, MANAGED_SECTION);
        assert_eq!(once.trim(), twice.trim());
    }
}

#[cfg(test)]
mod hook_marker_damage_tests {
    use super::*;

    #[test]
    fn commented_out_marker_is_not_a_section() {
        // Substring detection used to see this as "section present" and
        // install nothing while reporting success (finding 2b). Exact-line
        // counting sees zero real markers → the append path runs instead.
        let hook = format!("#!/bin/sh\n## {}\nmy-linter\n## {}\n", MARKER_START, MARKER_END);
        let starts = hook.lines().filter(|l| l.trim() == MARKER_START).count();
        assert_eq!(starts, 0);
    }

    #[test]
    fn replace_preserves_user_content_when_markers_are_intact() {
        let hook = format!("#!/bin/sh\nuser-before\n{}\nold body\n{}\nuser-after\n",
            MARKER_START, MARKER_END);
        let out = replace_managed_section(&hook, MANAGED_SECTION);
        assert!(out.contains("user-before"));
        assert!(out.contains("user-after"));
        assert!(!out.contains("old body"));
    }
}
