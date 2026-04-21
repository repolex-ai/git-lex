//! Shared utilities for the git-lex crate.
//!
//! Used by both `git-lex` (the CLI) and `git-lex-serve` (the server binary).

use oxigraph::store::Store;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

pub const KIT_GITHUB_ORG: &str = "repolex-ai";

/// Find the root of the current git repo.
pub fn find_git_root() -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Some(PathBuf::from(path))
    } else {
        None
    }
}

/// Path to the oxigraph store directory: `{repo_root}/.git/lex/oxigraph/`.
/// Lives under .git/ because the store is derived data (rebuildable from
/// .spo sidecars) and should never be version-controlled.
pub fn store_path() -> Option<PathBuf> {
    find_git_root().map(|r| r.join(".git").join("lex").join("oxigraph"))
}

/// Open the persistent store in read-only mode. Does not acquire the
/// RocksDB write lock, so writers (`git lex sync`, `git lex save`) can run
/// concurrently. The view is a snapshot from open-time and will not reflect
/// later writes until the store is reopened.
pub fn open_store_read_only() -> Option<Store> {
    let path = store_path()?;
    if path.exists() {
        Store::open_read_only(&path).ok()
    } else {
        None
    }
}

/// Read the kit spec from `.lex/repo.yml`. Returns None if no kit or kit is "none".
pub fn get_kit() -> Option<String> {
    let root = find_git_root()?;
    let repo_yml = root.join(".lex").join("repo.yml");
    let content = fs::read_to_string(&repo_yml).ok()?;
    for line in content.lines() {
        if let Some(kit) = line.strip_prefix("kit: ") {
            let kit = kit.trim();
            if kit != "none" {
                return Some(kit.to_string());
            }
        }
    }
    None
}

/// Resolve a kit spec into (org, repo, short_name). Accepts either a short
/// form (`soul`) which is sugar for `repolex-ai/git-lex-kit-{name}`, or a
/// full `org/repo` form.
pub fn resolve_kit_spec(spec: &str) -> (String, String, String) {
    if let Some((org, repo)) = spec.split_once('/') {
        let short = repo
            .strip_prefix("git-lex-kit-")
            .unwrap_or(repo)
            .to_string();
        (org.to_string(), repo.to_string(), short)
    } else {
        (
            KIT_GITHUB_ORG.to_string(),
            format!("git-lex-kit-{}", spec),
            spec.to_string(),
        )
    }
}

/// Install dir for a given kit spec, relative to the repo root.
/// `.lex/kit/{org}/{repo}/`.
pub fn kit_install_dir_for_spec(root: &std::path::Path, spec: &str) -> PathBuf {
    let (org, repo, _) = resolve_kit_spec(spec);
    root.join(".lex").join("kit").join(&org).join(&repo)
}

/// Auto-inject SPARQL prefixes into a query string. Adds standard prefixes
/// (git:, lex:, fm:, rdf:, rdfs:, owl:, xsd:) plus the content ontology
/// prefix (o:) and the kit prefix if one is configured.
pub fn add_prefixes(query: &str) -> String {
    // Get first commit SHA for content ontology prefix
    let first_commit = Command::new("git")
        .args(["rev-list", "--max-parents=0", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim()[..8].to_string())
        .unwrap_or_default();
    let o_prefix = format!("PREFIX o: <https://repolex.ai/ont/{}/>", first_commit);

    // Also read kit from repo.yml and get the actual prefix from the TTL
    let kit_prefix = find_git_root().and_then(|r| {
        let content = fs::read_to_string(r.join(".lex").join("repo.yml")).ok()?;
        for line in content.lines() {
            if let Some(kit) = line.strip_prefix("kit: ") {
                let kit = kit.trim();
                if kit == "none" { return None; }
                let kit_dir = kit_install_dir_for_spec(&r, kit);
                let (_, _, short) = resolve_kit_spec(kit);
                let ttl_path = kit_dir.join(format!("{}.ttl", short));
                let ttl_path = if ttl_path.exists() { ttl_path } else {
                    fs::read_dir(&kit_dir).ok()
                        .and_then(|entries| entries.filter_map(|e| e.ok())
                            .find(|e| e.path().extension().is_some_and(|ext| ext == "ttl"))
                            .map(|e| e.path()))
                        .unwrap_or(ttl_path)
                };
                let kit_ns_pattern = format!("/kit/{}/", short);
                if let Ok(ttl) = fs::read_to_string(&ttl_path) {
                    for tline in ttl.lines() {
                        if tline.starts_with("@prefix ") && tline.contains(&kit_ns_pattern) {
                            if let Some(colon_pos) = tline[8..].find(':') {
                                let pname = tline[8..8 + colon_pos].trim();
                                let ns = format!("https://repolex.ai/ontology/kit/{}/", short);
                                return Some((
                                    format!("{}:", pname),
                                    format!("PREFIX {}: <{}>", pname, ns),
                                ));
                            }
                        }
                    }
                }
                // Fallback: use short kit name as prefix
                return Some((
                    format!("{}:", short),
                    format!("PREFIX {}: <https://repolex.ai/ontology/kit/{}/>", short, short),
                ));
            }
        }
        None
    });

    let mut defaults = vec![
        ("git:".to_string(), "PREFIX git: <https://repolex.ai/ontology/git-lex/git/>".to_string()),
        ("lex:".to_string(), "PREFIX lex: <https://repolex.ai/ontology/git-lex/lex/>".to_string()),
        ("fm:".to_string(), "PREFIX fm: <https://repolex.ai/ontology/git-lex/fm/>".to_string()),
        ("o:".to_string(), o_prefix),
        ("rdf:".to_string(), "PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>".to_string()),
        ("rdfs:".to_string(), "PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>".to_string()),
        ("owl:".to_string(), "PREFIX owl: <http://www.w3.org/2002/07/owl#>".to_string()),
        ("xsd:".to_string(), "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>".to_string()),
    ];
    if let Some((short, full)) = kit_prefix {
        defaults.push((short, full));
    }
    let defaults = defaults;
    let upper = query.to_uppercase();
    let mut prefix_block = String::new();
    for (short, full) in &defaults {
        if query.contains(short) && !upper.contains(&format!("PREFIX {}", short.to_uppercase())) {
            prefix_block.push_str(full);
            prefix_block.push('\n');
        }
    }
    if !upper.contains("PREFIX") {
        for (_, full) in &defaults {
            prefix_block.push_str(full);
            prefix_block.push('\n');
        }
    }
    format!("{}{}", prefix_block, query)
}
