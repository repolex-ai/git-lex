//! Which soul a `git lex query` belongs to.
//!
//! The rule (goodlux, 2026-09-22): a query is tied to the soul the agent's
//! session started in. The agent passes nothing, sets nothing and cannot
//! change it through ordinary means. So the answer comes from the process
//! that started the session, not from the shell's current directory:
//!
//! 1. Under Claude Code, `CLAUDE_PID` names the harness process. Its
//!    working directory is the soul home, whatever directory the shell has
//!    since moved to. Read from the operating system: `lsof` on macOS,
//!    `/proc/<pid>/cwd` on Linux.
//! 2. With no harness marker (a person at a terminal), the process that
//!    started the session is the shell, and its working directory is this
//!    process's own.
//!
//! From that directory: the git root, then the first commit hash, which is
//! how gitlexd keys the soul.

use std::path::{Path, PathBuf};

/// The soul a session is bound to.
#[derive(Debug)]
pub struct SessionSoul {
    pub root: PathBuf,
    pub genesis: String,
    /// Where the binding came from, for the message a person reads.
    pub how: &'static str,
}

/// The working directory of the process that started this session.
pub fn session_home() -> Result<(PathBuf, &'static str), String> {
    if let Ok(pid) = std::env::var("CLAUDE_PID")
        && let Ok(pid) = pid.trim().parse::<u32>() {
            return process_cwd(pid)
                .map(|p| (p, "the Claude Code session"))
                .ok_or_else(|| format!("CLAUDE_PID is {pid} but that process's working directory could not be read"));
        }
    std::env::current_dir()
        .map(|p| (p, "this terminal"))
        .map_err(|e| format!("cannot read the current directory: {e}"))
}

/// A process's working directory, from the operating system.
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(p) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
            return Some(p);
        }
    }
    let out = std::process::Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix('n').map(PathBuf::from))
}

/// The git-lex repository at or above `dir`, with its first commit.
pub fn soul_at(dir: &Path, how: &'static str) -> Result<SessionSoul, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(format!("{} is not inside a git repository ({how}), so there is no soul to query", dir.display()));
    }
    let root = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    if !crate::layout::lex_dir(&root).is_dir() {
        return Err(format!(
            "{} is a git repository but not a git-lex one ({how}): it has no .lex/ directory",
            root.display()
        ));
    }
    let genesis = crate::git::genesis_sha_at(&root)
        .ok_or_else(|| format!("{} has no commits yet, so it has no first commit to identify it by", root.display()))?;
    Ok(SessionSoul { root, genesis, how })
}

/// The soul this session is bound to.
pub fn session_soul() -> Result<SessionSoul, String> {
    let (home, how) = session_home()?;
    soul_at(&home, how)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_reports_its_own_working_directory() {
        let me = std::process::id();
        let got = process_cwd(me).expect("own cwd");
        let want = std::env::current_dir().unwrap();
        assert_eq!(got.canonicalize().unwrap(), want.canonicalize().unwrap());
    }

    #[test]
    fn a_directory_outside_git_is_refused_and_says_so() {
        let dir = std::env::temp_dir().join(format!("git-lex-session-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let err = soul_at(&dir, "a test").unwrap_err();
        assert!(err.contains("not inside a git repository"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_git_repository_without_lex_is_refused_and_says_so() {
        let dir = std::env::temp_dir().join(format!("git-lex-session-nolex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(std::process::Command::new("git").arg("-C").arg(&dir).arg("init").arg("-q").status().unwrap().success());
        let err = soul_at(&dir, "a test").unwrap_err();
        assert!(err.contains("no .lex/ directory"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
