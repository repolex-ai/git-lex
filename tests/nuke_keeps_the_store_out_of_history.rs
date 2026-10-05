//! `git lex nuke` snapshots uncommitted work before it removes git-lex.
//! The snapshot must not commit the derived store under `.lex/_ignore/`
//! (#51: it did, because .gitignore was cleaned first).

use std::process::{Command, Stdio};
use std::io::Write;

fn git(dir: &std::path::Path, args: &[&str]) {
    let ok = Command::new("git").current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)
        .status().unwrap().success();
    assert!(ok, "git {args:?}");
}

#[test]
fn nuke_never_commits_the_store() {
    let base = std::env::temp_dir().join(format!("glx-nuke-store-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let repo = base.join("repo");
    let home = base.join("home");
    std::fs::create_dir_all(repo.join(".lex/_ignore/oxigraph")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join(".lex/repo.yml"), "name: t\n").unwrap();
    git_lex::kit::ensure_engine_gitignore(&repo);
    std::fs::write(repo.join("note.md"), "hello\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "one"]);
    // The derived store, ignored, plus some uncommitted work to snapshot.
    std::fs::write(repo.join(".lex/_ignore/oxigraph/000001.sst"), "store bytes").unwrap();
    std::fs::write(repo.join("draft.md"), "unsaved\n").unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_git-lex"))
        .arg("nuke")
        .current_dir(&repo)
        .env("HOME", &home)
        .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
        .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null())
        .spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"nuke\n").unwrap();
    child.wait().unwrap();

    let log = Command::new("git").current_dir(&repo)
        .args(["log", "--all", "--name-only", "--format="]).output().unwrap();
    let names = String::from_utf8_lossy(&log.stdout);
    assert!(names.contains("draft.md"), "the snapshot keeps uncommitted work:\n{names}");
    assert!(!names.contains(".lex/_ignore"), "the store reached history:\n{names}");
    assert!(!repo.join(".lex").exists());
    let _ = std::fs::remove_dir_all(&base);
}
