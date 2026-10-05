//! Installing a kit's files never replaces an existing root SOUL.md, and
//! still installs the template where there is none (#51: nuke then init
//! reset a soul's identity to the blank template). Its own test binary:
//! the installer reads the repository from the working directory.

use std::process::Command;

fn repo_at(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    assert!(Command::new("git").current_dir(dir).args(["init", "-q"]).status().unwrap().success());
}

#[test]
fn soul_md_is_installed_once_and_never_replaced() {
    let base = std::env::temp_dir().join(format!("glx-keep-soul-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let kit = base.join("kit");
    std::fs::create_dir_all(kit.join("content")).unwrap();
    std::fs::write(kit.join("content/SOUL.md"), "template\n").unwrap();
    std::fs::write(kit.join("content/AGENTS.md"), "agents\n").unwrap();

    // A soul that already has its identity document.
    let existing = base.join("existing");
    repo_at(&existing);
    std::fs::write(existing.join("SOUL.md"), "mine\n").unwrap();
    std::env::set_current_dir(&existing).unwrap();
    git_lex::kit::install_scaffold_files_from(&kit);
    assert_eq!(std::fs::read_to_string(existing.join("SOUL.md")).unwrap(), "mine\n");
    assert_eq!(std::fs::read_to_string(existing.join("AGENTS.md")).unwrap(), "agents\n");

    // A new repository gets the template.
    let fresh = base.join("fresh");
    repo_at(&fresh);
    std::env::set_current_dir(&fresh).unwrap();
    git_lex::kit::install_scaffold_files_from(&kit);
    assert_eq!(std::fs::read_to_string(fresh.join("SOUL.md")).unwrap(), "template\n");

    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    let _ = std::fs::remove_dir_all(&base);
}
