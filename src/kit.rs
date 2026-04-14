//! Kit configuration and TTL-loading helpers.
//!
//! Read-only access to `.lex/kit/**/kit.yml` (init-prompts, feature flags,
//! arbitrary string config) and to the kit's primary TTL file (loaded into
//! an ephemeral oxigraph store for SHACL-shape generation, etc).
//!
//! The install/fetch pipeline (fetch_kit_from_github, install_scaffold_*,
//! install_asset_files) stays in main.rs for now — those are tangled with
//! cmd_init and the AssetInstallReport struct. They will move here in a
//! follow-up phase.

use oxigraph::io::RdfFormat;
use oxigraph::store::Store;
use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::path::PathBuf;

use git_lex::{find_git_root, kit_install_dir_for_spec, resolve_kit_spec};

/// Read simple `key: value` fields from a repo.yml-style file into a map.
/// Used for honoring existing init variables on re-init (single-shot with
/// carry-over). Skips comment lines and anything that doesn't parse as a
/// flat key/value.
pub(crate) fn read_repo_yml_fields(path: &std::path::Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return out,
    };
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') { continue; }
        if let Some(colon) = trimmed.find(':') {
            let key = trimmed[..colon].trim().to_string();
            let value = trimmed[colon + 1..].trim().to_string();
            if !key.is_empty() && !value.is_empty() {
                out.insert(key, value);
            }
        }
    }
    out
}

/// Read the `init_prompts:` list from a kit's kit.yml. Returns the variable
/// names the kit wants init to prompt for. Empty list if missing or absent.
pub(crate) fn kit_config_init_prompts(kit_name: &str) -> Vec<String> {
    let root = match find_git_root() {
        Some(r) => r,
        None => return Vec::new(),
    };
    let config_path = kit_install_dir_for_spec(&root, kit_name).join("kit.yml");
    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let parsed: serde_yaml::Value = match serde_yaml::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    parsed
        .get("init_prompts")
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Read a boolean config value from the kit's kit.yml. Recognizes `true`
/// and `yes` as true; everything else (including missing) returns `default`.
pub(crate) fn kit_config_bool(kit: &str, key: &str, default: bool) -> bool {
    let root = match find_git_root() {
        Some(r) => r,
        None => return default,
    };
    let config_path = kit_install_dir_for_spec(&root, kit).join("kit.yml");
    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return default,
    };
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') { continue; }
        if let Some(val) = line.strip_prefix(&format!("{}:", key)) {
            let val = val.trim();
            return val == "true" || val == "yes";
        }
    }
    default
}

/// Read a string config value from the kit's kit.yml file.
pub(crate) fn kit_config_str(kit: &str, key: &str) -> Option<String> {
    let root = find_git_root()?;
    let config_path = kit_install_dir_for_spec(&root, kit).join("kit.yml");
    let content = fs::read_to_string(&config_path).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') { continue; }
        if let Some(val) = line.strip_prefix(&format!("{}:", key)) {
            let val = val.trim();
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

/// Find the kit TTL file path. Tries {kit}.ttl first, then any .ttl in the kit dir.
pub(crate) fn find_kit_ttl(kit: &str) -> Option<PathBuf> {
    let root = find_git_root()?;
    let kit_dir = kit_install_dir_for_spec(&root, kit);
    let (_, _, short_name) = resolve_kit_spec(kit);
    let primary = kit_dir.join(format!("{}.ttl", short_name));
    if primary.exists() {
        return Some(primary);
    }
    fs::read_dir(&kit_dir).ok()
        .and_then(|entries| entries.filter_map(|e| e.ok())
            .find(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.ends_with(".ttl") && !name.contains("shapes")
            })
            .map(|e| e.path()))
}

/// Load a kit TTL into an in-memory oxigraph store for SPARQL querying.
pub(crate) fn load_kit_into_store(kit: &str) -> Option<Store> {
    let ttl_path = find_kit_ttl(kit)?;
    let content = fs::read_to_string(&ttl_path).ok()?;
    let store = Store::new().ok()?;
    store.load_from_reader(RdfFormat::Turtle, Cursor::new(content.as_bytes())).ok()?;
    Some(store)
}
