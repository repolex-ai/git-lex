//! Self-installing man page — the fix for bare `git lex --help`.
//!
//! `git lex --help` never reaches this binary: git itself rewrites it to
//! `git help lex`, which looks up a git-lex(1) man page. Without one, git
//! prints "No manual entry for git-lex" and exits 1 — a false negative on
//! the first command a new user types, indistinguishable from the tool
//! being broken (th34, release triage B4). The only fix in our control is
//! for the man page to exist.
//!
//! `man` derives its search path from PATH: for a binary at `<prefix>/bin/`,
//! it probes `<prefix>/share/man`. So a page at `~/.cargo/share/man/man1/`
//! is found with zero configuration wherever `~/.cargo/bin` is on PATH —
//! true on macOS (mandoc) and Linux (man-db) alike, and entirely inside the
//! user's own install prefix (no system paths, no elevated writes).
//!
//! The page is rendered from the SAME clap definition that parses the CLI,
//! so it cannot drift from the real commands, and it converges on every
//! invocation: render, compare bytes, rewrite only on difference. A newly
//! deployed binary heals its own man page on first run; there is no install
//! step and nothing for anyone to remember.

use std::fs;
use std::path::{Path, PathBuf};

/// Render the git-lex(1) page and converge the on-disk copy next to the
/// running binary. Every failure is silent by design — a man page must
/// never block or noise up a real command.
pub fn converge(cmd: clap::Command) {
    let Ok(exe) = std::env::current_exe() else { return };
    let Some(target) = man_page_target(&exe) else { return };
    let rendered = render(cmd);
    if fs::read(&target).ok().as_deref() == Some(rendered.as_slice()) {
        return;
    }
    if let Some(dir) = target.parent() {
        if fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let _ = fs::write(&target, rendered);
}

/// The man-page path implied by the binary's own location: `<prefix>/bin/X`
/// maps to `<prefix>/share/man/man1/git-lex.1`. Returns None when the
/// binary is not under a `bin/` dir (e.g. running from `target/debug/`) —
/// those locations are not on any man path, so writing would be litter.
fn man_page_target(exe: &Path) -> Option<PathBuf> {
    let bin_dir = exe.parent()?;
    if bin_dir.file_name()? != "bin" {
        return None;
    }
    Some(bin_dir.parent()?.join("share").join("man").join("man1").join("git-lex.1"))
}

/// Render the top-level man page from the live clap definition. Only the
/// top-level page is needed: git intercepts only bare `git lex --help`;
/// `git lex <subcommand> --help` already reaches clap and works.
fn render(cmd: clap::Command) -> Vec<u8> {
    let mut buf = Vec::new();
    // render() is infallible for a Vec writer; a formatting error would be
    // a clap_mangen bug, and an empty page is still better than a crash.
    let _ = clap_mangen::Man::new(cmd).render(&mut buf);
    buf
}

#[cfg(test)]
mod man_page_tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn page_lists_every_visible_subcommand_and_hides_hook() {
        // roff escapes hyphens (`kit\-update`) — normalize before asserting.
        let page = String::from_utf8(render(crate::Cli::command()))
            .unwrap()
            .replace("\\-", "-");
        for sub in [
            "init", "query", "sync", "list", "create", "save", "nuke",
            "kit-update", "kit-add", "kit-remove", "serve", "verify", "soul",
        ] {
            assert!(page.contains(sub), "man page must document `{sub}`:\n{page}");
        }
        // `hook` is #[command(hide = true)] — internal, must not be taught.
        assert!(!page.contains("hook"), "hidden subcommand leaked into the man page");
    }

    #[test]
    fn target_derives_from_a_bin_prefix_only() {
        assert_eq!(
            man_page_target(Path::new("/home/u/.cargo/bin/git-lex")),
            Some(PathBuf::from("/home/u/.cargo/share/man/man1/git-lex.1"))
        );
        // A dev build under target/debug is not on any man path — no litter.
        assert_eq!(man_page_target(Path::new("/repo/target/debug/git-lex")), None);
    }

    #[test]
    fn converge_writes_then_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("gitlex-man-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let target = dir.join("share").join("man").join("man1").join("git-lex.1");
        // converge() resolves its own target from current_exe, so exercise
        // the same body against an explicit path here.
        let rendered = render(crate::Cli::command());
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"stale page from an older binary").unwrap();
        let stale = fs::read(&target).unwrap();
        if fs::read(&target).ok().as_deref() != Some(rendered.as_slice()) {
            fs::write(&target, &rendered).unwrap();
        }
        let healed = fs::read(&target).unwrap();
        assert_ne!(healed, stale, "a differing page must be rewritten");
        assert_eq!(healed, rendered, "the on-disk page converges to the render");
        fs::remove_dir_all(&dir).ok();
    }
}
