//! Git hook management.
//!
//! git-lex needs to run extraction + validation before every commit. Rather
//! than exposing `extract` and `validate` as CLI commands that agents misuse,
//! we install a managed section in the git pre-commit hook.
//!
//! Respects `core.hooksPath` (used by husky, lefthook, etc.) — if set, we
//! write into that directory instead of `.git/hooks/`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use git_lex::find_git_root;

const MARKER_START: &str = "# --- git-lex managed (do not edit this section) ---";
const MARKER_END: &str = "# --- end git-lex managed ---";

const MANAGED_SECTION: &str = "\
# --- git-lex managed (do not edit this section) ---
git-lex hook pre-commit || exit 1
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
pub(crate) fn install_hook() {
    let dir = match hooks_dir() {
        Some(d) => d,
        None => return,
    };
    fs::create_dir_all(&dir).ok();

    let hook_path = dir.join("pre-commit");
    let existing = fs::read_to_string(&hook_path).unwrap_or_default();

    let new_content = if existing.is_empty() {
        // No existing hook — create fresh
        format!("#!/bin/sh\n{}\n", MANAGED_SECTION)
    } else if existing.contains(MARKER_START) {
        // Already has our section — replace it
        replace_managed_section(&existing, MANAGED_SECTION)
    } else {
        // Existing hook without our section — append
        let mut content = existing.clone();
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push('\n');
        content.push_str(MANAGED_SECTION);
        content.push('\n');
        content
    };

    fs::write(&hook_path, &new_content).ok();

    // Ensure executable
    #[cfg(unix)]
    {
        if let Ok(meta) = fs::metadata(&hook_path) {
            let mut perms = meta.permissions();
            perms.set_mode(perms.mode() | 0o111);
            fs::set_permissions(&hook_path, perms).ok();
        }
    }
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
