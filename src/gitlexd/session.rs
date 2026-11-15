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
//! 2. Under Antigravity there is no such variable. The harness marks its
//!    shells with `ANTIGRAVITY_CONVERSATION_ID`, and the harness process
//!    (`agy`) is an ancestor of the shell (its direct parent when read on
//!    2026-11-15, agy 1.5.5). The nearest ancestor named `agy` is taken, and
//!    its working directory is the soul home.
//! 3. With no harness marker (a person at a terminal), the process that
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

/// How a session with no harness marker is described: a person at a
/// terminal, or a harness git-lex does not recognise.
pub const TERMINAL: &str = "this terminal";

impl SessionSoul {
    /// Was the session placed by a harness marker, as opposed to falling
    /// back to the shell's own directory?
    pub fn from_harness(&self) -> bool {
        self.how != TERMINAL
    }
}

/// The working directory of the process that started this session.
pub fn session_home() -> Result<(PathBuf, &'static str), String> {
    if let Ok(pid) = std::env::var("CLAUDE_PID")
        && let Ok(pid) = pid.trim().parse::<u32>() {
            return process_cwd(pid)
                .map(|p| (p, "the Claude Code session"))
                .ok_or_else(|| format!("CLAUDE_PID is {pid} but that process's working directory could not be read"));
        }
    if std::env::var_os("ANTIGRAVITY_CONVERSATION_ID").is_some() {
        let pid = ancestor_named(std::process::id(), "agy").ok_or_else(|| {
            "this is an Antigravity session (ANTIGRAVITY_CONVERSATION_ID is set) but no process named agy \
             was found above this one, so the session's home could not be read"
                .to_string()
        })?;
        return process_cwd(pid)
            .map(|p| (p, "the Antigravity session"))
            .ok_or_else(|| format!("the Antigravity process is {pid} but its working directory could not be read"));
    }
    std::env::current_dir()
        .map(|p| (p, TERMINAL))
        .map_err(|e| format!("cannot read the current directory: {e}"))
}

/// A process's parent and its program name (the last path component, with
/// the leading `-` of a login shell removed), from `ps`.
pub fn parent_and_name(pid: u32) -> Option<(u32, String)> {
    let out = std::process::Command::new("ps")
        .args(["-o", "ppid=,comm=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?.trim();
    let (ppid, comm) = line.split_once(char::is_whitespace)?;
    let name = comm.trim().rsplit('/').next()?.trim_start_matches('-').to_string();
    Some((ppid.trim().parse().ok()?, name))
}

/// The nearest process above `pid` whose program name is `name`.
pub fn ancestor_named(pid: u32, name: &str) -> Option<u32> {
    let mut at = parent_and_name(pid)?.0;
    // A process tree is shallow; the bound only stops a loop on bad data.
    for _ in 0..64 {
        if at <= 1 {
            return None;
        }
        let (parent, comm) = parent_and_name(at)?;
        if comm == name {
            return Some(at);
        }
        at = parent;
    }
    None
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
    fn a_process_reports_its_parent_and_no_ancestor_has_a_made_up_name() {
        let me = std::process::id();
        let (parent, name) = parent_and_name(me).expect("own row from ps");
        assert_eq!(parent, unsafe { libc::getppid() } as u32);
        assert!(!name.is_empty() && !name.contains('/'), "{name}");
        assert_eq!(ancestor_named(me, "no-such-program-name"), None);
        let (_, parent_name) = parent_and_name(parent).expect("the parent's row");
        assert_eq!(ancestor_named(me, &parent_name), Some(parent));
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
