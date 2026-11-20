//! Real Git topology tests. One test owns cwd; no process-global state races.
#[path = "../src/hooks.rs"]
#[allow(dead_code)]
mod hooks;

use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .trim_end_matches('\n')
        .to_owned()
}

fn check(cwd: &Path, expected: &Path) {
    std::env::set_current_dir(cwd).unwrap();
    fs::create_dir_all(expected).unwrap();
    let path = expected.join("pre-commit");
    let user = "#!/bin/sh\n# user hook stays\n";
    fs::write(&path, user).unwrap();
    assert!(hooks::converge_hook().unwrap(), "{cwd:?}");
    let installed = fs::read_to_string(&path).unwrap();
    assert!(installed.starts_with(user));
    assert!(installed.contains("\"$_glx\" hook pre-commit || exit 1"));
    assert_ne!(fs::metadata(&path).unwrap().permissions().mode() & 0o111, 0);
    assert!(!hooks::converge_hook().unwrap());
    fs::write(
        &path,
        "#!/bin/sh\n# --- git-lex managed (do not edit this section) ---\nforeign-content\n",
    )
    .unwrap();
    let damaged = fs::read(&path).unwrap();
    assert!(hooks::converge_hook().is_err());
    assert_eq!(fs::read(&path).unwrap(), damaged);
    fs::write(&path, installed).unwrap();
    assert!(hooks::remove_hook());
    assert_eq!(fs::read_to_string(&path).unwrap().trim(), user.trim());
}

#[test]
fn effective_hooks_follow_git_topology_and_commit_cwd() {
    let original = std::env::current_dir().unwrap();
    let base = std::env::temp_dir().join(format!("glx-hooks-paths-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    let ordinary = base.join("ordinary");
    fs::create_dir_all(&ordinary).unwrap();
    git(&ordinary, &["init", "-q"]);
    check(&ordinary, &ordinary.join(".git/hooks"));
    println!("ordinary: PASS");
    let linked = base.join("linked");
    git(
        &ordinary,
        &[
            "worktree",
            "add",
            "--orphan",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ],
    );
    assert!(linked.join(".git").is_file());
    check(&linked, &ordinary.join(".git/hooks"));
    println!("linked .git file: PASS");
    let separate = base.join("separate");
    fs::create_dir_all(&separate).unwrap();
    let gitdir = base.join("separate.git");
    git(
        &separate,
        &["init", "-q", "--separate-git-dir", gitdir.to_str().unwrap()],
    );
    check(&separate, &gitdir.join("hooks"));
    println!("separate git-dir: PASS");
    for repo in [&ordinary, &linked, &separate] {
        let nested = repo.join("a/b");
        fs::create_dir_all(&nested).unwrap();
        let discovered = PathBuf::from(git(repo, &["rev-parse", "--git-path", "hooks"]));
        let default = if discovered.is_absolute() {
            discovered
        } else {
            repo.join(discovered)
        };
        check(&nested, &default);
        git(repo, &["config", "core.hooksPath", "custom hooks"]);
        check(&nested, &repo.join("custom hooks"));
        let absolute = base.join(format!(
            "absolute hooks {}",
            repo.file_name().unwrap().to_str().unwrap()
        ));
        git(
            repo,
            &["config", "core.hooksPath", absolute.to_str().unwrap()],
        );
        check(&nested, &absolute);
        git(repo, &["config", "--unset", "core.hooksPath"]);
    }
    println!("relative/absolute hooksPath from nested cwd: PASS");
    std::env::set_current_dir(&original).unwrap();
    fs::remove_dir_all(base).unwrap();
}
