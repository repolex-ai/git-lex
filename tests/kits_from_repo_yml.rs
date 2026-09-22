//! Issue #17: `.lex/repo.yml` is the only answer to "which kits are
//! installed". A kit folder left in `.lex/ontology/` that repo.yml does not
//! list must have no effect on anything, and `kit-remove` must not leave one.
//!
//! The fixture reproduces the 2026-09-16 failure: a foreign kit declares its
//! own `id` on `git-lex:Thing`, so every other kit's documents inherit it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const GIT_LEX_TTL: &str = r#"
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix git-lex: <https://repolex.ai/ontology/git-lex/> .
git-lex:Thing a owl:Class .
git-lex:id a owl:ObjectProperty ; rdfs:domain git-lex:Thing ; rdfs:range git-lex:Thing .
"#;

const T_TTL: &str = r#"
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
@prefix git-lex: <https://repolex.ai/ontology/git-lex/> .
@prefix t: <https://repolex.ai/ontology/t/> .
t:Journal a owl:Class ; rdfs:subClassOf git-lex:Thing .
t:title a owl:DatatypeProperty ; rdfs:domain t:Journal ; rdfs:range xsd:string .
"#;

const GHOST_TTL: &str = r#"
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix git-lex: <https://repolex.ai/ontology/git-lex/> .
@prefix ghost: <https://repolex.ai/ontology/ghost/> .
ghost:Spook a owl:Class ; rdfs:subClassOf git-lex:Thing .
ghost:id a owl:ObjectProperty ; rdfs:domain git-lex:Thing ; rdfs:range git-lex:Thing .
"#;

const GHOST_SHAPES: &str = r#"
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ghost: <https://repolex.ai/ontology/ghost/> .
ghost:SpookShape a sh:NodeShape ; sh:targetClass ghost:Spook .
"#;

/// A repo with kit `t`, a `ghost` ontology folder on disk, and whichever
/// optional kits the caller lists in repo.yml.
fn fixture(tag: &str, optional_kits: &[&str]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("glx-17-{}-{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let ont = root.join(".lex").join("ontology");
    for (dir, file, body) in [
        ("git-lex", "git-lex.ttl", GIT_LEX_TTL),
        ("t", "t.ttl", T_TTL),
        ("ghost", "ghost.ttl", GHOST_TTL),
        ("ghost", "ghost-shapes.ttl", GHOST_SHAPES),
    ] {
        fs::create_dir_all(ont.join(dir)).unwrap();
        fs::write(ont.join(dir).join(file), body).unwrap();
    }
    let mut yml = String::from("name: fixture\nkit: repolex-ai/git-lex-kit-t\noptional_kits:\n");
    for k in optional_kits {
        yml.push_str(&format!("  - repolex-ai/git-lex-kit-{}\n", k));
    }
    fs::write(root.join(".lex").join("repo.yml"), yml).unwrap();
    fs::create_dir_all(root.join("Journal")).unwrap();
    fs::write(
        root.join("Journal").join("x.md"),
        "---\nt.Journal.id: <t/Journal/x>\nt.Journal.title: \"hello\"\n---\n\nbody\n",
    )
    .unwrap();
    assert!(Command::new("git").args(["init", "-q"]).current_dir(&root).status().unwrap().success());
    root
}

/// Run git-lex in the fixture; stdout and stderr together.
fn git_lex(root: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_git-lex"))
        .args(args)
        .current_dir(root)
        .env("HOME", root.join(".home"))
        .output()
        .unwrap();
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

fn t_shapes(root: &Path) -> String {
    fs::read_to_string(root.join(".lex/ontology/t/t-shapes.ttl")).unwrap_or_default()
}

/// kit-remove regenerates every remaining kit's shapes, offline, so removing
/// an unrelated kit is how these tests get `t-shapes.ttl` generated.
fn remove_kit(root: &Path, short: &str) -> String {
    git_lex(root, &["kit-remove", &format!("repolex-ai/git-lex-kit-{}", short), "--force"])
}

#[test]
fn the_fixture_bites_when_the_foreign_kit_is_installed() {
    let root = fixture("bites", &["dummy", "ghost"]);
    remove_kit(&root, "dummy");
    assert!(
        t_shapes(&root).contains("ontology/ghost/id"),
        "an INSTALLED kit declaring id on git-lex:Thing reaches t's shapes — the premise of #17"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_leftover_ontology_folder_has_no_effect() {
    let root = fixture("leftover", &["dummy"]);
    remove_kit(&root, "dummy");

    let shapes = t_shapes(&root);
    assert!(shapes.contains("ontology/git-lex/id"), "t inherits the universal id: {shapes}");
    assert!(!shapes.contains("ghost"), "an uninstalled kit reached t's shapes: {shapes}");

    let listing = git_lex(&root, &["list"]);
    assert!(listing.contains("t:Journal"), "{listing}");
    assert!(!listing.contains("Spook"), "an uninstalled kit's shapes were listed: {listing}");

    let id_pred = git_lex(&root, &["direct", "SELECT ?p WHERE { ?s ?p <https://repolex.ai/t/Journal/x> }"]);
    assert!(id_pred.contains("ontology/git-lex/id"), "{id_pred}");
    assert!(!id_pred.contains("ontology/ghost/id"), "`t.Journal.id` was captured: {id_pred}");

    let unbound = git_lex(&root, &["direct", "SELECT ?s WHERE { ?s a ghost:Spook }"]);
    assert!(unbound.contains("ghost:"), "{unbound}");
    assert!(unbound.to_lowercase().contains("prefix"), "ghost: must not be a bound prefix: {unbound}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn kit_remove_leaves_no_ontology_folder_and_frees_the_id() {
    let root = fixture("remove", &["ghost"]);
    remove_kit(&root, "ghost");

    assert!(!root.join(".lex/ontology/ghost").exists(), "kit-remove left .lex/ontology/ghost/ behind");
    let yml = fs::read_to_string(root.join(".lex/repo.yml")).unwrap();
    assert!(!yml.contains("ghost"), "{yml}");
    let shapes = t_shapes(&root);
    assert!(shapes.contains("ontology/git-lex/id") && !shapes.contains("ghost"), "{shapes}");
    let _ = fs::remove_dir_all(&root);
}

/// kit-add fetches with `curl | tar`. Put a stand-in `curl` first on PATH that
/// serves a tarball of the ghost kit, so the real command runs offline.
#[cfg(unix)]
fn kit_add_ghost_offline(root: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let stage = root.join(".stage");
    let kit = stage.join("git-lex-kit-ghost-main");
    fs::create_dir_all(kit.join("ontology").join("ghost")).unwrap();
    fs::write(kit.join("kit.yml"), "scope: optional\nname: ghost\n").unwrap();
    fs::write(kit.join("ontology/ghost/ghost.ttl"), GHOST_TTL).unwrap();
    let tarball = stage.join("ghost.tar.gz");
    assert!(Command::new("tar")
        .args(["czf", &tarball.to_string_lossy(), "-C", &stage.to_string_lossy(), "git-lex-kit-ghost-main"])
        .status().unwrap().success());
    let bin = stage.join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("curl"), format!("#!/bin/sh\ncat '{}'\n", tarball.display())).unwrap();
    fs::set_permissions(bin.join("curl"), fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let out = Command::new(env!("CARGO_BIN_EXE_git-lex"))
        .args(["kit-add", "repolex-ai/git-lex-kit-ghost"])
        .current_dir(root)
        .env("HOME", root.join(".home"))
        .env("PATH", path)
        .output()
        .unwrap();
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

/// goodlux, 2026-09-16: kit-add regenerates EVERY installed kit's shapes. A
/// property the new kit declares on git-lex:Thing reaches kit t's shapes at
/// kit-add, with no kit-update in between.
#[cfg(unix)]
#[test]
fn kit_add_regenerates_the_other_kits_shapes() {
    let root = fixture("add", &["dummy"]);
    fs::remove_dir_all(root.join(".lex/ontology/ghost")).unwrap();
    remove_kit(&root, "dummy");
    assert!(!t_shapes(&root).contains("ghost"), "premise: t's shapes start without ghost");

    let out = kit_add_ghost_offline(&root);
    let yml = fs::read_to_string(root.join(".lex/repo.yml")).unwrap();
    assert!(yml.contains("git-lex-kit-ghost"), "kit-add did not record the kit: {out}");
    assert!(
        t_shapes(&root).contains("ontology/ghost/id"),
        "kit-add left kit t's shapes stale until the next kit-update: {out}"
    );
    let _ = fs::remove_dir_all(&root);
}
