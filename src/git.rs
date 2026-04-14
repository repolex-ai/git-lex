//! Git operations and repo-identity helpers.
//!
//! Peeled out of `main.rs` during modularization. Everything here either
//! shells out to `git` or derives identity from the remote URL.

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

/// Build the repo-level RDF base namespace: `https://host/org/repo`.
pub(crate) fn base_uri() -> String {
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
