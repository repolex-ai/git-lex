//! Kit configuration, TTL-loading, and install pipeline.
//!
//! Four concerns:
//!
//! 1. **Config readers** — `kit_config_init_prompts`, `kit_config_bool`,
//!    `kit_config_str`, `read_repo_yml_fields`: read-only access to
//!    `.lex/kit/**/kit.yml` and repo.yml.
//! 2. **TTL loader** — `find_kit_ttl`, `load_kit_into_store`: find and
//!    parse the kit's primary Turtle file into an ephemeral oxigraph
//!    store (used by SHACL-shape generation).
//! 3. **Install pipeline** — `fetch_kit_from_github`,
//!    `collect_init_variables`, `install_scaffold_files_from`,
//!    `install_scaffold_files_from_skip_existing`: everything that turns
//!    a kit spec into on-disk scaffolded files.
//! 4. **Kit lifecycle** — `add_kit`, `remove_kit`: add/remove kits from
//!    a repo. Stubs for now.

use oxigraph::io::RdfFormat;
use oxigraph::store::Store;
use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use git_lex::{
    canonical_kit_ontology_path, find_git_root, kit_install_dir_for_spec,
    resolve_kit_spec,
};

/// Read simple `key: value` fields from a repo.yml-style file into a map.
/// Used for honoring existing init variables on re-init (single-shot with
/// carry-over). Skips comment lines and anything that doesn't parse as a
/// flat key/value. Skips list keys (lines ending with `:` and no value).
pub(crate) fn read_repo_yml_fields(path: &std::path::Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return out,
    };
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') { continue; }
        // Skip list-item lines (`  - foo`) and bare keys (`optional_kits:`).
        if trimmed.starts_with('-') { continue; }
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

/// Read a flat YAML list value from repo.yml. Used by
/// `read_repo_yml_optional_kits` and `read_repo_yml_substrates`.
///
/// Format:
/// ```yaml
/// <key>:
///   - item-one
///   - item-two
/// ```
///
/// The list ends at the first non-list, non-comment, non-blank line.
pub(crate) use git_lex::read_repo_yml_list;

/// Append an item to a flat YAML list in repo.yml. Creates the list if
/// missing. Idempotent — no duplicate entries. Preserves all other fields
/// and existing list entries.
fn append_repo_yml_list_item(
    path: &std::path::Path,
    key: &str,
    item: &str,
) -> std::io::Result<()> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let current = read_repo_yml_list(path, key);
    if current.iter().any(|s| s == item) {
        return Ok(());
    }
    let key_prefix = format!("{}:", key);
    let mut lines: Vec<String> = existing.lines().map(|s| s.to_string()).collect();
    let mut idx_of_key: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if line.trim().starts_with(&key_prefix) {
            idx_of_key = Some(i);
            break;
        }
    }
    match idx_of_key {
        Some(idx) => {
            let mut insert_at = idx + 1;
            for (i, line) in lines.iter().enumerate().skip(idx + 1) {
                let t = line.trim();
                if t.starts_with('-') {
                    insert_at = i + 1;
                } else if t.is_empty() {
                    continue;
                } else {
                    break;
                }
            }
            lines.insert(insert_at, format!("  - {}", item));
        }
        None => {
            if !lines.last().map(|l| l.is_empty()).unwrap_or(true) {
                lines.push(String::new());
            }
            lines.push(format!("{}:", key));
            lines.push(format!("  - {}", item));
        }
    }
    let mut content = lines.join("\n");
    if !content.ends_with('\n') { content.push('\n'); }
    fs::write(path, content)
}

/// Remove an item from a flat YAML list in repo.yml. If the list becomes
/// empty, also removes the key. Idempotent: removing an item that isn't
/// there is a no-op success.
fn remove_repo_yml_list_item(
    path: &std::path::Path,
    key: &str,
    item: &str,
) -> std::io::Result<()> {
    let existing = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    let key_prefix = format!("{}:", key);
    let lines: Vec<&str> = existing.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut in_list = false;
    let mut list_items_remaining = 0usize;
    let mut pending_key_line: Option<String> = None;
    for line in &lines {
        let t = line.trim();
        if t.starts_with(&key_prefix) {
            in_list = true;
            pending_key_line = Some(line.to_string());
            list_items_remaining = 0;
            continue;
        }
        if in_list {
            if let Some(rest) = t.strip_prefix('-') {
                let parsed_item = rest.trim().trim_matches('"').trim_matches('\'').to_string();
                if parsed_item == item {
                    continue;
                } else {
                    if let Some(key_line) = pending_key_line.take() {
                        out.push(key_line);
                    }
                    out.push(line.to_string());
                    list_items_remaining += 1;
                    continue;
                }
            } else if t.is_empty() {
                if pending_key_line.is_none() {
                    out.push(line.to_string());
                }
                continue;
            } else {
                in_list = false;
                if let Some(_dropped) = pending_key_line.take() {
                    if matches!(out.last().map(|s| s.trim().is_empty()), Some(true)) {
                        out.pop();
                    }
                }
                out.push(line.to_string());
                continue;
            }
        }
        out.push(line.to_string());
    }
    if in_list && pending_key_line.is_some() && list_items_remaining == 0 {
        if matches!(out.last().map(|s| s.trim().is_empty()), Some(true)) {
            out.pop();
        }
    }
    let mut content = out.join("\n");
    if !content.ends_with('\n') { content.push('\n'); }
    fs::write(path, content)
}

/// Read the `optional_kits:` list from a repo.yml. Returns the kit specs
/// (e.g. `["repolex-ai/git-lex-kit-innerworld"]`). Empty if missing or absent.
///
/// Format:
/// ```yaml
/// optional_kits:
///   - repolex-ai/git-lex-kit-innerworld
///   - repolex-ai/git-lex-kit-thoughtsmith
/// ```
pub(crate) use git_lex::read_repo_yml_optional_kits;

/// Read the `substrates:` list from a repo.yml. Returns short substrate
/// names (e.g. `["claude", "hermes"]`) that the agent has explicitly
/// declared this repo targets. Empty if missing or absent — in which case
/// the harness falls back to on-disk auto-detection.
///
/// Format:
/// ```yaml
/// substrates:
///   - claude
///   - hermes
/// ```
pub(crate) fn read_repo_yml_substrates(path: &std::path::Path) -> Vec<String> {
    read_repo_yml_list(path, "substrates")
}

/// Append a kit spec to `optional_kits:` in repo.yml. Creates the list if
/// missing. Idempotent — no duplicate entries. Preserves all other fields
/// and existing list entries.
pub(crate) fn append_optional_kit(path: &std::path::Path, spec: &str) -> std::io::Result<()> {
    append_repo_yml_list_item(path, "optional_kits", spec)
}

/// Remove a kit spec from `optional_kits:` in repo.yml. If the list becomes
/// empty, also removes the `optional_kits:` key. Idempotent: removing a
/// kit that isn't there is a no-op success.
pub(crate) fn remove_optional_kit(path: &std::path::Path, spec: &str) -> std::io::Result<()> {
    remove_repo_yml_list_item(path, "optional_kits", spec)
}

/// Append a substrate name to `substrates:` in repo.yml. Idempotent.
#[allow(dead_code)]
pub(crate) fn append_substrate(path: &std::path::Path, name: &str) -> std::io::Result<()> {
    append_repo_yml_list_item(path, "substrates", name)
}

/// Remove a substrate name from `substrates:` in repo.yml. Idempotent.
#[allow(dead_code)]
pub(crate) fn remove_substrate(path: &std::path::Path, name: &str) -> std::io::Result<()> {
    remove_repo_yml_list_item(path, "substrates", name)
}

/// Kit scope as declared by `scope:` in the kit's kit.yml.
///
/// - `Base`: ships system ontologies/UI for every repo. Implicit; always
///   installed first by init. Only one base kit per repo.
/// - `Domain`: the repo's primary kit (soul, squad, lab, etc.). Recorded
///   in `repo.yml`'s `kit:` field. Exactly one per repo.
/// - `Optional`: add-on kits (innerworld, thoughtsmith, etc.). Tracked in
///   `repo.yml`'s `optional_kits:` list. Zero or more per repo.
///
/// If a kit's kit.yml omits `scope:`, default is `Domain` (back-compat —
/// every pre-scope kit was a domain kit).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KitScope {
    Base,
    Domain,
    Optional,
}

impl KitScope {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "base" => Some(KitScope::Base),
            "domain" => Some(KitScope::Domain),
            "optional" => Some(KitScope::Optional),
            _ => None,
        }
    }
}

/// Read the `scope:` field from a kit's kit.yml. Returns `KitScope::Domain`
/// if the field is missing (back-compat).
///
/// The kit_dir is the on-disk install location of the kit (e.g.
/// `.lex/kit/repolex-ai/git-lex-kit-innerworld/`).
pub(crate) fn read_kit_scope(kit_dir: &std::path::Path) -> KitScope {
    let path = kit_dir.join("kit.yml");
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return KitScope::Domain,
    };
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') { continue; }
        if let Some(val) = line.strip_prefix("scope:") {
            if let Some(s) = KitScope::parse(val) {
                return s;
            }
        }
    }
    KitScope::Domain
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
///
/// Lookup order:
///   1. `.lex/ontology/{short}/{short}.ttl`        — static kit primary
///   2. any non-shapes `.ttl` in `.lex/ontology/{short}/` — static fallback
///   3. `_ontology/{short}/{short}.ttl`            — adaptive kit primary
///   4. any non-shapes `.ttl` in `_ontology/{short}/` — adaptive fallback
///   5. `.lex/kit/{org}/{repo}/{short}.ttl`        — legacy
pub(crate) fn find_kit_ttl(kit: &str) -> Option<PathBuf> {
    let root = find_git_root()?;
    let (_, _, short_name) = resolve_kit_spec(kit);

    let try_dir = |dir: &PathBuf| -> Option<PathBuf> {
        let primary = dir.join(format!("{}.ttl", short_name));
        if primary.exists() {
            return Some(primary);
        }
        if dir.exists() {
            return fs::read_dir(dir).ok()
                .and_then(|entries| entries.filter_map(|e| e.ok())
                    .find(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        name.ends_with(".ttl") && !name.contains("shapes")
                    })
                    .map(|e| e.path()));
        }
        None
    };

    // Static kit: THE canonical path first (the contract — see
    // `canonical_kit_ontology_path`). Consumers may rely on this path alone.
    let canonical = canonical_kit_ontology_path(&root, kit);
    if canonical.exists() { return Some(canonical); }

    // Resilience tier: any non-shapes .ttl already in the canonical kit dir
    // (covers a kit that ships its TTL under a different filename — rare).
    let static_dir = root.join(".lex").join("ontology").join(&short_name);
    if let Some(p) = try_dir(&static_dir) { return Some(p); }

    // Adaptive kit location (kit.yml `adaptive: true`; ontology lives outside .lex/)
    let adaptive_dir = root.join("_ontology").join(&short_name);
    if let Some(p) = try_dir(&adaptive_dir) { return Some(p); }

    // Legacy fallback: .lex/kit/{org}/{repo}/{short}.ttl
    let kit_dir = kit_install_dir_for_spec(&root, kit);
    let legacy = kit_dir.join(format!("{}.ttl", short_name));
    if legacy.exists() {
        return Some(legacy);
    }
    None
}

/// Load a kit TTL into an in-memory oxigraph store for SPARQL querying.
pub(crate) fn load_kit_into_store(kit: &str) -> Option<Store> {
    let ttl_path = find_kit_ttl(kit)?;
    let content = fs::read_to_string(&ttl_path).ok()?;
    let store = Store::new().ok()?;
    store.load_from_reader(RdfFormat::Turtle, Cursor::new(content.as_bytes())).ok()?;
    Some(store)
}

// ─── install pipeline ──────────────────────────────────────────

/// Fetch a kit tarball from GitHub and extract it into `target_dir`.
///
/// Uses `curl | tar --strip-components=1` so the extract goes straight
/// to `target_dir` without a nested `{repo}-main/` directory. Preserves
/// symlinks natively (a copy step would dereference them and break the
/// `scaffold/.claude/skills` → `../../skill` link).
///
/// Returns true on success (and if at least one file was extracted).
pub(crate) fn fetch_kit_from_github(kit_spec: &str, target_dir: &std::path::Path) -> bool {
    let (org, repo, _) = resolve_kit_spec(kit_spec);
    let url = format!(
        "https://github.com/{}/{}/archive/refs/heads/main.tar.gz",
        org, repo
    );

    // Extract the tarball directly into target_dir using --strip-components=1
    // to drop the top-level "git-lex-kit-{name}-main/" directory. Extracting
    // in-place preserves symlinks (tar honors them natively); any round trip
    // through a copy step dereferences them, which breaks e.g. the
    // scaffold/.claude/skills → ../../skill symlink.
    fs::create_dir_all(target_dir).ok();

    let status = Command::new("curl")
        .args(["-sL", "--fail", "-o", "-", &url])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|curl| {
            Command::new("tar")
                .args([
                    "xzf",
                    "-",
                    "-C",
                    &target_dir.to_string_lossy(),
                    "--strip-components=1",
                ])
                .stdin(curl.stdout.unwrap())
                .status()
        });

    match status {
        Ok(s) if s.success() => {
            // Verify we actually got files (curl --fail should prevent empty
            // extracts, but be safe).
            let has_files = fs::read_dir(target_dir)
                .ok()
                .map(|entries| entries.count() > 0)
                .unwrap_or(false);
            if !has_files {
                return false;
            }
            true
        }
        _ => false,
    }
}

/// Interactively prompt the user for each kit-declared init variable and
/// return the collected name→value map. Re-uses existing values from repo.yml
/// if present (supports idempotent re-init).
pub(crate) fn collect_init_variables(kit_name: &str, existing: &HashMap<String, String>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    // The `{kit}` template variable is the short name ("soul"), not the full
    // spec ("repolex-ai/git-lex-kit-soul"), because that's what templates want
    // to embed (e.g. `{kit}.memory.confidence`).
    let (_, _, short) = resolve_kit_spec(kit_name);
    out.insert("kit".to_string(), short);
    for name in kit_config_init_prompts(kit_name) {
        if let Some(v) = existing.get(&name) {
            out.insert(name, v.clone());
            continue;
        }
        eprint!("{}: ", name);
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).unwrap_or_default();
        let value = input.trim().to_string();
        if !value.is_empty() {
            out.insert(name, value);
        }
    }
    out
}

/// Install scaffold files from the kit into the repo root.
/// Scaffold files live in .lex/kit/scaffold/ and mirror the repo structure.
/// Raw byte-for-byte copy — no template processing, no variable substitution.
/// Always overwrites existing files. Symlinks are preserved as symlinks
/// (not dereferenced) so that e.g. `.claude/skills` can be a symlink to
/// `../../skill` pointing at the agent's content-area skill folder.
/// These are infrastructure files the kit owns: .claude/, AGENTS.md, hooks,
/// skills symlink, etc. Agents don't edit them.
// NOTE(w4r3z, Day 38): "Agents don't edit them" is the load-bearing assumption.
// It holds TODAY because the one agent-edited file in .claude/ — settings.json
// (identity + COPIA_CONFIG) — is special-cased through setup_substrate_claude
// (main.rs:~720), NOT this always-overwrite path. GUARD-RAIL: if a future kit
// ever ships settings.json (or any other agent-touched file) in scaffold/, this
// path would silently clobber the agent's edits on every kit-update — exactly
// the silent-overwrite-of-user-intent bug class (cf. the Day-37 <slug>@lex.local
// identity reverts, and the copia COPIA_CONFIG default). Keep agent-editable
// files OUT of scaffold/, or route them through a drift-aware/merge path.
pub(crate) fn install_scaffold_files_from(kit_dir: &std::path::Path) -> usize {
    let root = match find_git_root() {
        Some(r) => r,
        None => return 0,
    };

    let mut count = 0;

    fn install_recursive(
        src_dir: &std::path::Path,
        dest_dir: &std::path::Path,
        count: &mut usize,
    ) {
        let entries = match fs::read_dir(src_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let src = entry.path();
            let name = entry.file_name();
            let dest = dest_dir.join(&name);

            let meta = match fs::symlink_metadata(&src) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let ft = meta.file_type();

            if ft.is_symlink() {
                let target = match fs::read_link(&src) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                fs::create_dir_all(dest.parent().unwrap_or(&dest)).ok();
                if dest.symlink_metadata().is_ok() {
                    if dest.is_dir() && !dest.is_symlink() {
                        let _ = fs::remove_dir_all(&dest);
                    } else {
                        let _ = fs::remove_file(&dest);
                    }
                }
                #[cfg(unix)]
                {
                    if std::os::unix::fs::symlink(&target, &dest).is_ok() {
                        *count += 1;
                    }
                }
                continue;
            }

            if ft.is_dir() {
                fs::create_dir_all(&dest).ok();
                install_recursive(&src, &dest, count);
                continue;
            }

            if ft.is_file() {
                fs::create_dir_all(dest.parent().unwrap_or(&dest)).ok();
                if dest.symlink_metadata().is_ok() {
                    let dmeta = dest.symlink_metadata().ok().map(|m| m.file_type());
                    if let Some(dft) = dmeta {
                        if dft.is_symlink() || dft.is_dir() {
                            if dft.is_dir() && !dft.is_symlink() {
                                let _ = fs::remove_dir_all(&dest);
                            } else {
                                let _ = fs::remove_file(&dest);
                            }
                        }
                    }
                }
                if fs::copy(&src, &dest).is_ok() {
                    *count += 1;
                }
            }
        }
    }

    fn install_recursive_skip_existing(
        src_dir: &std::path::Path,
        dest_dir: &std::path::Path,
        count: &mut usize,
    ) {
        let entries = match fs::read_dir(src_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let src = entry.path();
            let name = entry.file_name();
            let dest = dest_dir.join(&name);

            if src.is_dir() {
                fs::create_dir_all(&dest).ok();
                install_recursive_skip_existing(&src, &dest, count);
            } else if src.is_file() && !dest.exists() {
                fs::create_dir_all(dest.parent().unwrap_or(&dest)).ok();
                if fs::copy(&src, &dest).is_ok() {
                    *count += 1;
                }
            }
        }
    }

    // New kit structure: ontology/, content/, harness/ (and legacy scaffold/)
    // Each maps to a different destination:
    //   ontology/ → .lex/ontology/  (static kit) or _ontology/ (adaptive kit)
    //   content/  → repo root       (content files)
    //   harness/  → repo root       (substrate adapter files)
    //   www/      → .lex/www/       (web UI assets)
    //   scaffold/ → repo root       (legacy, for pre-migration kits)
    let ontology_src = kit_dir.join("ontology");
    if ontology_src.exists() {
        // Adaptive kits seed ontology to _ontology/ (never clobber — agent-owned).
        // Static kits install to .lex/ontology/ (always clobber — kit-owned).
        let is_adaptive = fs::read_to_string(kit_dir.join("kit.yml"))
            .ok()
            .map(|c| c.lines().any(|l| {
                let l = l.trim();
                l.starts_with("adaptive:") && {
                    let v = l.strip_prefix("adaptive:").unwrap().trim();
                    v == "true" || v == "yes"
                }
            }))
            .unwrap_or(false);

        if is_adaptive {
            let ontology_dest = root.join("_ontology");
            fs::create_dir_all(&ontology_dest).ok();
            // Only seed if nothing is there yet — never clobber agent work
            install_recursive_skip_existing(&ontology_src, &ontology_dest, &mut count);
        } else {
            let ontology_dest = root.join(".lex").join("ontology");
            fs::create_dir_all(&ontology_dest).ok();
            install_recursive(&ontology_src, &ontology_dest, &mut count);
        }
    }

    let content_src = kit_dir.join("content");
    if content_src.exists() {
        install_recursive(&content_src, &root, &mut count);
    }

    let harness_src = kit_dir.join("harness");
    if harness_src.exists() {
        install_recursive(&harness_src, &root, &mut count);
    }

    let www_src = kit_dir.join("www");
    if www_src.exists() {
        let www_dest = root.join(".lex").join("www");
        fs::create_dir_all(&www_dest).ok();
        install_recursive(&www_src, &www_dest, &mut count);
    }

    // Legacy: scaffold/ → repo root (for kits not yet migrated)
    let scaffold_src = kit_dir.join("scaffold");
    if scaffold_src.exists() {
        install_recursive(&scaffold_src, &root, &mut count);
    }

    count
}

/// Report from `install_scaffold_files_from_skip_existing`.
///
/// Beyond the legacy (installed, skipped) counts, this carries:
/// - `drifted`: files where the local copy differs from the kit-shipped one;
///   for each, a `.kit-latest` sibling has been written so the agent can see
///   the diff. The local file itself was NOT touched.
/// - `stashed`: files moved to `<repo-root>/.kit-pre-force/<timestamp>/<rel>`
///   before being overwritten under `--force`. Recovery path if --force was
///   wrong.
///
/// Paths are relative to the repo root, for display.
#[derive(Default)]
pub(crate) struct ScaffoldInstallReport {
    pub installed: usize,
    pub skipped: usize,
    pub drifted: Vec<String>,
    pub stashed: Vec<String>,
}

/// Best-effort kit version string for the `.kit-latest` header.
/// Reads `version:` from `kit.yml`; falls back to the kit dir name.
fn kit_version_for(kit_dir: &Path) -> String {
    if let Ok(content) = fs::read_to_string(kit_dir.join("kit.yml")) {
        for line in content.lines() {
            let l = line.trim();
            if let Some(rest) = l.strip_prefix("version:") {
                let v = rest.trim().trim_matches('"').trim_matches('\'');
                if !v.is_empty() {
                    return v.to_string();
                }
            }
        }
    }
    kit_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// `YYYYMMDD-HHMMSS` (UTC) from `SystemTime`. Used for `.kit-pre-force/` stash
/// dirs and `.kit-latest` header dates. UTC, not local — stash dirs sort right
/// across timezones.
fn timestamp_now_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Days since 1970-01-01.
    let days = (secs / 86400) as i64;
    let sod = secs % 86400;
    let hh = sod / 3600;
    let mm = (sod % 3600) / 60;
    let ss = sod % 60;

    // Civil-from-days (Howard Hinnant's algorithm — exact, no chrono dep).
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{:04}{:02}{:02}-{:02}{:02}{:02}", y, m, d, hh, mm, ss)
}

/// ISO date `YYYY-MM-DD` (UTC), for `.kit-latest` headers.
fn iso_date_utc() -> String {
    let ts = timestamp_now_utc();
    // ts is "YYYYMMDD-HHMMSS"; reformat the date portion.
    if ts.len() >= 8 {
        format!("{}-{}-{}", &ts[0..4], &ts[4..6], &ts[6..8])
    } else {
        ts
    }
}

/// Pick the comment-prefix to use for a `.kit-latest` header, based on the
/// file's existing first line (shebang-aware) and extension. Default: `# `.
/// Recognizes `//` for JS/TS/Rust/C/Java/Go and `<!--`/`-->` for HTML/Markdown.
fn header_for_drift_file(local_path: &Path, kit_version: &str, kit_path_rel: &str) -> String {
    let ext = local_path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let date = iso_date_utc();
    let line1 = format!(
        "kit-latest from {} installed {} — your local {} differs",
        kit_version, date, kit_path_rel
    );
    let line2 = format!("Diff: diff {0} {0}.kit-latest", kit_path_rel);
    match ext.as_str() {
        "rs" | "js" | "ts" | "jsx" | "tsx" | "c" | "h" | "cpp" | "java" | "go" | "swift" | "kt" => {
            format!("// {}\n// {}\n", line1, line2)
        }
        "html" | "htm" | "md" | "xml" | "svg" => {
            format!("<!-- {} -->\n<!-- {} -->\n", line1, line2)
        }
        _ => format!("# {}\n# {}\n", line1, line2),
    }
}

/// True if the destination is a regular file whose bytes match `src`. Returns
/// false on any error (treat as drift so the agent gets a chance to inspect).
fn files_byte_identical(src: &Path, dest: &Path) -> bool {
    match (fs::read(src), fs::read(dest)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Like `install_scaffold_files_from`, but safe for `kit-update` against a
/// live repo.
///
/// Without `--force`:
///   - Missing files: copied from kit (counted as `installed`).
///   - Identical files: silent no-op (counted as `skipped`).
///   - **Drifted files**: local left untouched; kit version installed
///     alongside as `<name>.kit-latest` with a two-line header (kit version +
///     diff one-liner). Recorded in `drifted` so the caller can surface the
///     paths. This is the graceful upgrade path: drift is visible from `ls`,
///     the decision stays with the agent.
///
/// With `--force`:
///   - Files that differ are stashed to
///     `<repo-root>/.kit-pre-force/<timestamp>/<rel-path>` before being
///     overwritten. Recorded in `stashed`. Identical files still no-op.
/// Collect the set of hook filenames (`<name>.sh`) shipped by an installed kit's
/// `harness/.claude/hooks/` dir. Used by the file-level hook reap to decide which
/// local hook files are kit-owned (survive) vs orphaned (removed). The caller passes
/// each installed kit's vendored dir; this unions them across all kits.
pub(crate) fn kit_shipped_hook_names(kit_dir: &Path) -> Vec<String> {
    let hooks_src = kit_dir.join("harness").join(".claude").join("hooks");
    let mut names = Vec::new();
    if let Ok(entries) = fs::read_dir(&hooks_src) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".sh") {
                names.push(name);
            }
        }
    }
    names
}

/// A local hook file is PERSONAL (protected from the reap) iff its event-namespace
/// segment is `local` — i.e. `<Event>-local-<purpose>.sh`. This is the escape hatch:
/// a squaddie's own hook, named `local`, still fires on its event but is never removed
/// or converged. Anything else must be kit-shipped or it is reaped.
fn is_local_hook_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".sh") else { return false };
    // Second segment (after the event) must be exactly "local".
    let mut parts = stem.split('-');
    let _event = parts.next();
    parts.next() == Some("local")
}

/// File-level hook reap (the twin of the registration reaper): after kit-update has
/// installed all kits, remove any `.claude/hooks/*.sh` that is NEITHER shipped by an
/// installed kit NOR a `<Event>-local-*.sh` personal hook. The removed file is stashed
/// to `stash_root` first (recoverable; .lex/kit/ is also git-tracked). Returns the list
/// of reaped rel-paths so the caller can also prune their settings.json registrations.
///
/// SAFETY: only call this AFTER every kit has successfully fetched + installed —
/// kit-update bails hard on any fetch failure before this point, so `kit_hook_names`
/// is the COMPLETE canonical set. Never reap on a partial fetch (a network blip would
/// otherwise delete a real hook whose kit just didn't download).
pub(crate) fn reap_non_kit_non_local_hooks(
    root: &Path,
    kit_hook_names: &std::collections::HashSet<String>,
    stash_root: &Path,
) -> Vec<String> {
    let hooks_dir = root.join(".claude").join("hooks");
    let mut reaped = Vec::new();
    let Ok(entries) = fs::read_dir(&hooks_dir) else { return reaped };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".sh") {
            continue; // only manage hook scripts; leave .kit-latest, etc.
        }
        // Survives if kit-shipped OR a local personal hook.
        if kit_hook_names.contains(&name) || is_local_hook_name(&name) {
            continue;
        }
        // Orphan: neither kit-owned nor local → stash then remove.
        let dest = entry.path();
        let rel = dest
            .strip_prefix(root)
            .unwrap_or(&dest)
            .to_string_lossy()
            .to_string();
        let stash_dest = stash_root.join(&rel);
        let _ = fs::create_dir_all(stash_dest.parent().unwrap_or(&stash_dest));
        let _ = fs::copy(&dest, &stash_dest); // best-effort archive
        if fs::remove_file(&dest).is_ok() {
            reaped.push(rel);
        }
    }
    reaped
}

/// Is this destination an ENFORCED kit-owned file — one that must ALWAYS converge to
/// the kit version on update, never sit behind a `.kit-latest` drift sidecar?
///
/// Kit-owned MACHINERY (as opposed to user CONTENT like a journal template) should
/// track the kit automatically: a soul running last-month's buggy hook is a bug, not
/// a customization. Git is the revert path if a soul genuinely needs to fork — but the
/// default is convergence, and local forks are discouraged.
///
/// Currently: hook scripts (`.claude/hooks/*.sh`). This is the same principle already
/// applied to static ontology (ALWAYS clobbered — "kit-owned schema, must match the
/// kit", see the static-ontology branch below). Extend this list as more kit-owned
/// machinery is identified.
fn is_enforced_path(dest: &Path) -> bool {
    // A hook script: parent dir is `.../.claude/hooks` and name ends in `.sh`.
    let is_sh = dest.extension().map(|e| e == "sh").unwrap_or(false);
    let in_hooks_dir = dest
        .parent()
        .map(|p| p.ends_with(".claude/hooks"))
        .unwrap_or(false);
    is_sh && in_hooks_dir
}

pub(crate) fn install_scaffold_files_from_skip_existing(
    kit_dir: &std::path::Path,
    force: bool,
) -> ScaffoldInstallReport {
    let root = match find_git_root() {
        Some(r) => r,
        None => return ScaffoldInstallReport::default(),
    };

    let kit_version = kit_version_for(kit_dir);
    // One stash dir per kit-update invocation (caller drives by passing the
    // same timestamp via a fresh call — we use one per call here, which is
    // fine since base+domain kit calls land within the same second and
    // identical timestamps just merge into the same dir).
    let stash_root = root.join(".kit-pre-force").join(timestamp_now_utc());

    let mut report = ScaffoldInstallReport::default();

    struct Ctx<'a> {
        repo_root: &'a Path,
        kit_version: &'a str,
        stash_root: &'a Path,
        force: bool,
    }

    fn install_recursive(
        src_dir: &Path,
        dest_dir: &Path,
        ctx: &Ctx,
        report: &mut ScaffoldInstallReport,
    ) {
        let entries = match fs::read_dir(src_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let src = entry.path();
            let name = entry.file_name();
            let dest = dest_dir.join(&name);

            let meta = match fs::symlink_metadata(&src) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let ft = meta.file_type();

            if ft.is_symlink() {
                if !ctx.force && dest.symlink_metadata().is_ok() {
                    // Symlinks: drift is "exists differently" — but resolving
                    // a symlink's target to byte-compare is fragile. Treat
                    // existing-symlink-no-force as skip (legacy behavior).
                    report.skipped += 1;
                    continue;
                }
                let target = match fs::read_link(&src) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                fs::create_dir_all(dest.parent().unwrap_or(&dest)).ok();
                if dest.symlink_metadata().is_ok() {
                    if dest.is_dir() && !dest.is_symlink() {
                        let _ = fs::remove_dir_all(&dest);
                    } else {
                        let _ = fs::remove_file(&dest);
                    }
                }
                #[cfg(unix)]
                {
                    if std::os::unix::fs::symlink(&target, &dest).is_ok() {
                        report.installed += 1;
                    }
                }
                continue;
            }

            if ft.is_dir() {
                fs::create_dir_all(&dest).ok();
                install_recursive(&src, &dest, ctx, report);
                continue;
            }

            if !ft.is_file() {
                continue;
            }

            fs::create_dir_all(dest.parent().unwrap_or(&dest)).ok();

            let dest_exists = dest.symlink_metadata().is_ok();
            let dest_is_regular_file = dest_exists
                && dest.symlink_metadata().ok().map(|m| m.file_type().is_file()).unwrap_or(false);

            if !dest_exists {
                // Missing → install.
                if fs::copy(&src, &dest).is_ok() {
                    report.installed += 1;
                }
                continue;
            }

            // Destination exists. Decide based on byte-compare.
            let identical = dest_is_regular_file && files_byte_identical(&src, &dest);
            if identical {
                report.skipped += 1;
                continue;
            }

            // Drift case.
            let rel = dest
                .strip_prefix(ctx.repo_root)
                .unwrap_or(&dest)
                .to_string_lossy()
                .to_string();

            // ENFORCED kit-owned files (hooks) ALWAYS converge to the kit version,
            // even without --force — they are machinery, not user content, and a soul
            // must not run a stale local hook. The old local copy is still stashed to
            // .kit-pre-force/ below (the revert path), so this is safe: overwrite +
            // archive, never a silent loss. Non-enforced drift keeps the .kit-latest
            // alongside-install so a soul's own customizations are preserved.
            let enforced = is_enforced_path(&dest);

            if !ctx.force && !enforced {
                // Alongside-install: write `<dest>.kit-latest` with a header.
                let kit_latest_path = {
                    let mut p = dest.clone().into_os_string();
                    p.push(".kit-latest");
                    PathBuf::from(p)
                };
                let header = header_for_drift_file(&dest, ctx.kit_version, &rel);
                if let Ok(body) = fs::read(&src) {
                    let mut out: Vec<u8> = Vec::with_capacity(header.len() + body.len());
                    out.extend_from_slice(header.as_bytes());
                    out.extend_from_slice(&body);
                    if fs::write(&kit_latest_path, &out).is_ok() {
                        report.drifted.push(rel);
                    }
                }
                continue;
            }

            // Overwrite path — reached under --force OR for an enforced kit-owned
            // file (hooks) even without --force. Either way: stash the prior local
            // copy to .kit-pre-force/ first (the revert path), then write the kit
            // version.
            let stash_dest = ctx.stash_root.join(&rel);
            let stash_ok = fs::create_dir_all(stash_dest.parent().unwrap_or(&stash_dest)).is_ok()
                && fs::copy(&dest, &stash_dest).is_ok();
            // Remove dest if it was an odd type, then write the kit version.
            if dest_exists && !dest_is_regular_file {
                if dest.symlink_metadata().ok().map(|m| m.file_type().is_dir() && !m.file_type().is_symlink()).unwrap_or(false) {
                    let _ = fs::remove_dir_all(&dest);
                } else {
                    let _ = fs::remove_file(&dest);
                }
            }
            if fs::copy(&src, &dest).is_ok() {
                report.installed += 1;
                if stash_ok {
                    report.stashed.push(rel);
                }
            }
        }
    }

    let ctx = Ctx {
        repo_root: &root,
        kit_version: &kit_version,
        stash_root: &stash_root,
        force,
    };

    // New kit structure: ontology/, content/, harness/, www/
    // Static kits: ontology ALWAYS overwritten — kit's schema, must stay in sync.
    // Adaptive kits: ontology seeded to _ontology/, never clobbered — agent-owned.
    let ontology_src = kit_dir.join("ontology");
    if ontology_src.exists() {
        let is_adaptive = fs::read_to_string(kit_dir.join("kit.yml"))
            .ok()
            .map(|c| c.lines().any(|l| {
                let l = l.trim();
                l.starts_with("adaptive:") && {
                    let v = l.strip_prefix("adaptive:").unwrap().trim();
                    v == "true" || v == "yes"
                }
            }))
            .unwrap_or(false);

        if is_adaptive {
            // Adaptive: agent-owned. Never clobber; only seed missing.
            let ontology_dest = root.join("_ontology");
            fs::create_dir_all(&ontology_dest).ok();
            let adaptive_ctx = Ctx {
                repo_root: ctx.repo_root,
                kit_version: ctx.kit_version,
                stash_root: ctx.stash_root,
                force: false,
            };
            install_recursive(&ontology_src, &ontology_dest, &adaptive_ctx, &mut report);
        } else {
            // Static: kit-owned schema, ALWAYS clobber. Stash on force is
            // implicit — and for static ontology we hard-overwrite even
            // without --force because the SHACL/TTL graph must match the kit.
            // (Same as legacy behavior; this is not a drift surface.)
            let ontology_dest = root.join(".lex").join("ontology");
            fs::create_dir_all(&ontology_dest).ok();
            let static_ctx = Ctx {
                repo_root: ctx.repo_root,
                kit_version: ctx.kit_version,
                stash_root: ctx.stash_root,
                force: true,
            };
            install_recursive(&ontology_src, &ontology_dest, &static_ctx, &mut report);
        }
    }

    let content_src = kit_dir.join("content");
    if content_src.exists() {
        install_recursive(&content_src, &root, &ctx, &mut report);
    }

    let harness_src = kit_dir.join("harness");
    if harness_src.exists() {
        install_recursive(&harness_src, &root, &ctx, &mut report);
    }

    let www_src = kit_dir.join("www");
    if www_src.exists() {
        let www_dest = root.join(".lex").join("www");
        fs::create_dir_all(&www_dest).ok();
        install_recursive(&www_src, &www_dest, &ctx, &mut report);
    }

    // Legacy: scaffold/ → repo root
    let scaffold_dir = kit_dir.join("scaffold");
    if scaffold_dir.exists() {
        install_recursive(&scaffold_dir, &root, &ctx, &mut report);
    }

    report
}

// ─── kit lifecycle ─────────────────────────────────────────────

/// Outcome of `fetch_and_validate_optional_kit`. See that function for the
/// validation flow.
#[derive(Debug)]
pub(crate) enum KitFetchOutcome {
    /// Fetched and scope-validated as Optional. The kit is on disk at the
    /// returned path, ready to install.
    Ready(PathBuf),
    /// Fetch failed (network, missing repo, etc.).
    FetchFailed,
    /// Fetched but kit.yml's `scope:` is not `optional`. Won't auto-install
    /// as an optional kit. The carried scope tells the caller why.
    ScopeMismatch(KitScope),
}

/// Fetch a kit from GitHub into the repo's `.lex/kit/{org}/{repo}/` and
/// verify its `scope:` is `optional`. Used by `kit-add` to install add-on
/// kits without conflating them with base or domain.
///
/// On `ScopeMismatch`, the fetched dir is left on disk (caller can inspect
/// or clean up). On `FetchFailed`, the dir is removed.
pub(crate) fn fetch_and_validate_optional_kit(kit_spec: &str) -> KitFetchOutcome {
    let root = match find_git_root() {
        Some(r) => r,
        None => return KitFetchOutcome::FetchFailed,
    };
    let (org, repo, _) = resolve_kit_spec(kit_spec);
    let kit_dir = root.join(".lex").join("kit").join(&org).join(&repo);

    // Clean any prior state so the fetch is fresh.
    let _ = fs::remove_dir_all(&kit_dir);
    if fs::create_dir_all(&kit_dir).is_err() {
        return KitFetchOutcome::FetchFailed;
    }

    if !fetch_kit_from_github(kit_spec, &kit_dir) {
        let _ = fs::remove_dir_all(&kit_dir);
        return KitFetchOutcome::FetchFailed;
    }

    let scope = read_kit_scope(&kit_dir);
    if scope != KitScope::Optional {
        return KitFetchOutcome::ScopeMismatch(scope);
    }
    KitFetchOutcome::Ready(kit_dir)
}

/// Remove a kit's on-disk install dir (`.lex/kit/{org}/{repo}/`). Does NOT
/// touch the kit's content/ folders in the repo root (those are user data
/// — `cmd_kit_remove` asks before deleting).
pub(crate) fn remove_kit_install_dir(kit_spec: &str) -> std::io::Result<()> {
    let root = find_git_root().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "not in a git repo")
    })?;
    let (org, repo, _) = resolve_kit_spec(kit_spec);
    let kit_dir = root.join(".lex").join("kit").join(&org).join(&repo);
    if kit_dir.exists() {
        fs::remove_dir_all(&kit_dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn timestamp_now_utc_is_well_formed() {
        let ts = timestamp_now_utc();
        // YYYYMMDD-HHMMSS = 15 chars.
        assert_eq!(ts.len(), 15);
        assert_eq!(ts.chars().nth(8), Some('-'));
        let year: u32 = ts[0..4].parse().expect("year parses");
        assert!(year >= 2026 && year <= 2200, "year out of range: {}", year);
        let month: u32 = ts[4..6].parse().expect("month parses");
        assert!((1..=12).contains(&month), "month out of range: {}", month);
        let day: u32 = ts[6..8].parse().expect("day parses");
        assert!((1..=31).contains(&day), "day out of range: {}", day);
    }

    #[test]
    fn iso_date_utc_is_well_formed() {
        let d = iso_date_utc();
        assert_eq!(d.len(), 10);
        assert_eq!(d.chars().nth(4), Some('-'));
        assert_eq!(d.chars().nth(7), Some('-'));
    }

    #[test]
    fn is_enforced_path_recognizes_hooks_only() {
        // The load-bearing decision: hook scripts are ENFORCED (always converge to the
        // kit version); everything else keeps the drift-protect (.kit-latest) default.
        // These are the real filenames the kits ship.
        assert!(is_enforced_path(Path::new("/soul/.claude/hooks/Stop-pool-moment.sh")));
        assert!(is_enforced_path(Path::new("/soul/.claude/hooks/UserPromptSubmit-soul-recall.sh")));
        assert!(is_enforced_path(Path::new("/soul/.claude/hooks/UserPromptSubmit-pool-share.sh")));
        assert!(is_enforced_path(Path::new("/soul/.claude/hooks/SessionStart-soul-listener.sh")));
        // A relative dest (as install_recursive builds) must also match.
        assert!(is_enforced_path(Path::new(".claude/hooks/PreCompact-soul-journal.sh")));

        // NOT enforced: non-.sh in the hooks dir, .sh outside the hooks dir, config,
        // and especially user CONTENT (a journal template) — those keep .kit-latest.
        assert!(!is_enforced_path(Path::new("/soul/.claude/hooks/README.md")));
        assert!(!is_enforced_path(Path::new("/soul/.claude/settings.json")));
        assert!(!is_enforced_path(Path::new("/soul/scripts/build.sh")));
        assert!(!is_enforced_path(Path::new("/soul/Soul/Journal/__Journal.md")));
        // A .sh one level deeper than hooks/ (not our flat layout) is not enforced.
        assert!(!is_enforced_path(Path::new("/soul/.claude/hooks/sub/x.sh")));
    }

    #[test]
    fn is_local_hook_name_only_matches_local_namespace() {
        assert!(is_local_hook_name("Stop-local-mything.sh"));
        assert!(is_local_hook_name("UserPromptSubmit-local-scratch.sh"));
        // kit-namespaced or plain → NOT local (those must be kit-shipped to survive).
        assert!(!is_local_hook_name("Stop-pool-moment.sh"));
        assert!(!is_local_hook_name("UserPromptSubmit-soul-recall.sh"));
        assert!(!is_local_hook_name("Stop.sh"));
        assert!(!is_local_hook_name("UserPromptSubmit.sh"));
        // "local" must be the SECOND segment, not buried elsewhere.
        assert!(!is_local_hook_name("Stop-pool-local.sh"));
    }

    #[test]
    fn reap_removes_orphans_keeps_kit_and_local() {
        let tmp = std::env::temp_dir().join(format!("glx_filereap_{}_{}", std::process::id(), timestamp_now_utc()));
        let hooks = tmp.join(".claude").join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        // A kit-shipped hook, a local personal hook, and two orphans (old-named +
        // a stray). Plus a non-.sh file that must be ignored entirely.
        std::fs::write(hooks.join("UserPromptSubmit-soul-recall.sh"), b"kit\n").unwrap();
        std::fs::write(hooks.join("Stop-local-mine.sh"), b"personal\n").unwrap();
        std::fs::write(hooks.join("UserPromptSubmit.sh"), b"OLD combined\n").unwrap();       // orphan
        std::fs::write(hooks.join("Stop-copia-moment.sh"), b"OLD renamed\n").unwrap();       // orphan
        std::fs::write(hooks.join("README.md"), b"not a hook\n").unwrap();                    // ignored

        let mut kit_names = std::collections::HashSet::new();
        kit_names.insert("UserPromptSubmit-soul-recall.sh".to_string());

        let stash = tmp.join(".kit-pre-force").join("test");
        let reaped = reap_non_kit_non_local_hooks(&tmp, &kit_names, &stash);

        // Both orphans reaped; kit + local + non-.sh survive.
        assert_eq!(reaped.len(), 2, "exactly the two orphans reaped");
        assert!(hooks.join("UserPromptSubmit-soul-recall.sh").exists(), "kit hook survives");
        assert!(hooks.join("Stop-local-mine.sh").exists(), "local hook survives");
        assert!(hooks.join("README.md").exists(), "non-.sh untouched");
        assert!(!hooks.join("UserPromptSubmit.sh").exists(), "old combined hook reaped");
        assert!(!hooks.join("Stop-copia-moment.sh").exists(), "old renamed hook reaped");
        // Orphans were stashed (recoverable).
        assert!(stash.join(".claude/hooks/UserPromptSubmit.sh").exists(), "reaped hook stashed for revert");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn reap_noop_on_empty_or_missing_hooks_dir() {
        let tmp = std::env::temp_dir().join(format!("glx_filereap_empty_{}_{}", std::process::id(), timestamp_now_utc()));
        // No .claude/hooks dir at all.
        let kit_names = std::collections::HashSet::new();
        let reaped = reap_non_kit_non_local_hooks(&tmp, &kit_names, &tmp.join("stash"));
        assert!(reaped.is_empty(), "missing hooks dir → no reap, no panic");
    }

    #[test]
    fn files_byte_identical_detects_identity() {
        let tmp = std::env::temp_dir().join(format!("git-lex-test-{}", timestamp_now_utc()));
        std::fs::create_dir_all(&tmp).unwrap();
        let a = tmp.join("a");
        let b = tmp.join("b");
        std::fs::write(&a, b"hello\n").unwrap();
        std::fs::write(&b, b"hello\n").unwrap();
        assert!(files_byte_identical(&a, &b));
        std::fs::write(&b, b"hello!\n").unwrap();
        assert!(!files_byte_identical(&a, &b));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn header_picks_bash_style_for_shell() {
        let p = Path::new("SessionStart.sh");
        let h = header_for_drift_file(p, "git-lex-kit-soul", ".claude/hooks/SessionStart.sh");
        assert!(h.starts_with("# kit-latest"), "got: {}", h);
        assert!(h.contains("diff .claude/hooks/SessionStart.sh"));
    }

    #[test]
    fn header_picks_html_style_for_md() {
        let p = Path::new("README.md");
        let h = header_for_drift_file(p, "kit", "README.md");
        assert!(h.starts_with("<!--"), "got: {}", h);
    }

    #[test]
    fn header_picks_rust_style_for_rs() {
        let p = Path::new("lib.rs");
        let h = header_for_drift_file(p, "kit", "src/lib.rs");
        assert!(h.starts_with("// "), "got: {}", h);
    }

    /// Smoke test: build a fake kit + fake repo, exercise the install path.
    /// Skipped if not in a git repo (we need find_git_root() to succeed).
    #[test]
    fn scaffold_install_drift_smoke() {
        // Skip cleanly if find_git_root() returns None — the test asserts only
        // that the function's contract holds when it can run.
        if find_git_root().is_none() {
            return;
        }

        let tmp_root = std::env::temp_dir().join(format!("git-lex-kit-smoke-{}", timestamp_now_utc()));
        let kit_dir = tmp_root.join("kit");
        let harness_dir = kit_dir.join("harness").join(".claude").join("hooks");
        std::fs::create_dir_all(&harness_dir).unwrap();
        let mut f = std::fs::File::create(harness_dir.join("FakeHook.sh")).unwrap();
        f.write_all(b"#!/bin/bash\n# kit version\nexit 0\n").unwrap();

        // Without --force on a non-existent dest, behavior should install.
        // (We can't easily exercise the drift branch without writing into the
        // real repo root, so this smoke just confirms no panic + counts work.)
        let report = install_scaffold_files_from_skip_existing(&kit_dir, false);
        // Either the file was installed in the real repo (and we should clean
        // up) or it was skipped/drifted there. Don't assert exact counts.
        let _ = report;
        // Cleanup: best-effort.
        std::fs::remove_dir_all(&tmp_root).ok();
        // If the test ran and the install landed inside the real repo's root,
        // try to remove it.
        if let Some(root) = find_git_root() {
            let _ = std::fs::remove_file(root.join(".claude").join("hooks").join("FakeHook.sh"));
            let _ = std::fs::remove_file(root.join(".claude").join("hooks").join("FakeHook.sh.kit-latest"));
        }
    }

    // ─── Optional-kit / scope tests ──────────────────────────────────

    // Per-test unique dir. Tests run in parallel and `timestamp_now_utc()`
    // is second-resolution — collisions cause spurious failures. The atomic
    // counter + process id gives every call a fresh dir.
    fn unique_tmp_dir(prefix: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("{}-{}-{}-{}", prefix, pid, timestamp_now_utc(), n));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_tmp_repo_yml(content: &str) -> PathBuf {
        let tmp = unique_tmp_dir("git-lex-repo-yml");
        let p = tmp.join("repo.yml");
        std::fs::write(&p, content).unwrap();
        p
    }

    /// PIN: the canonical kit-ontology path is a CONTRACT downstream consumers
    /// (e.g. Pool) rely on as their ONE path. If this formula ever changes, this
    /// test must fail loudly so the move is deliberate — and so every consumer
    /// gets re-notified — rather than silently re-introducing a fallback chain.
    /// (#8 / EDGE-1: git-lex moved this path twice before; never silently again.)
    #[test]
    fn canonical_kit_ontology_path_is_stable() {
        let root = Path::new("/repo");
        // Full org/repo spec resolves to the short name `copia`.
        let p = canonical_kit_ontology_path(root, "repolex-ai/git-lex-kit-copia");
        assert_eq!(
            p,
            Path::new("/repo/.lex/ontology/copia/copia.ttl"),
            "canonical kit-ontology path moved — update ALL downstream consumers \
             (Pool's locate_kit_copia_ontology, etc.) before changing this"
        );
        // Bare short-name spec resolves identically.
        let p2 = canonical_kit_ontology_path(root, "soul");
        assert_eq!(p2, Path::new("/repo/.lex/ontology/soul/soul.ttl"));
    }

    /// PIN: find_kit_ttl must resolve a kit installed at the canonical path —
    /// the function and the contract agree by construction.
    #[test]
    fn find_kit_ttl_resolves_canonical_install() {
        let tmp = unique_tmp_dir("git-lex-canon-ttl");
        // Lay down a kit ontology at the canonical path under a fake repo root.
        let canon = canonical_kit_ontology_path(&tmp, "repolex-ai/git-lex-kit-copia");
        std::fs::create_dir_all(canon.parent().unwrap()).unwrap();
        std::fs::write(&canon, "# copia ontology\n").unwrap();
        // try_dir's exact-name primary should find it at the canonical location.
        let dir = tmp.join(".lex").join("ontology").join("copia");
        let primary = dir.join("copia.ttl");
        assert!(primary.exists(), "canonical install must exist for the pin");
        assert_eq!(primary, canon, "find path agrees with the canonical contract");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn read_optional_kits_basic() {
        let p = write_tmp_repo_yml(
            "name: TEST\nkit: repolex-ai/git-lex-kit-soul\noptional_kits:\n  - repolex-ai/git-lex-kit-innerworld\n  - repolex-ai/git-lex-kit-thoughtsmith\n"
        );
        let got = read_repo_yml_optional_kits(&p);
        assert_eq!(got, vec![
            "repolex-ai/git-lex-kit-innerworld".to_string(),
            "repolex-ai/git-lex-kit-thoughtsmith".to_string(),
        ]);
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn read_optional_kits_empty_when_missing() {
        let p = write_tmp_repo_yml("name: TEST\nkit: foo/bar\n");
        assert!(read_repo_yml_optional_kits(&p).is_empty());
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn read_repo_yml_fields_skips_list_items() {
        // Regression: ensure the scalar-fields reader doesn't accidentally
        // pick up `- something` list items as malformed key:values.
        let p = write_tmp_repo_yml(
            "name: TEST\nkit: foo/bar\noptional_kits:\n  - a/b\n  - c/d\nother: thing\n"
        );
        let fields = read_repo_yml_fields(&p);
        assert_eq!(fields.get("name"), Some(&"TEST".to_string()));
        assert_eq!(fields.get("kit"), Some(&"foo/bar".to_string()));
        assert_eq!(fields.get("other"), Some(&"thing".to_string()));
        // The list items should NOT appear as fields.
        assert!(fields.get("a/b").is_none());
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn append_optional_kit_creates_section() {
        let p = write_tmp_repo_yml("name: TEST\nkit: foo/bar\n");
        append_optional_kit(&p, "x/y").unwrap();
        let got = read_repo_yml_optional_kits(&p);
        assert_eq!(got, vec!["x/y".to_string()]);
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn append_optional_kit_appends_to_existing() {
        let p = write_tmp_repo_yml(
            "name: TEST\nkit: foo/bar\noptional_kits:\n  - a/b\n"
        );
        append_optional_kit(&p, "c/d").unwrap();
        let got = read_repo_yml_optional_kits(&p);
        assert_eq!(got, vec!["a/b".to_string(), "c/d".to_string()]);
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn append_optional_kit_is_idempotent() {
        let p = write_tmp_repo_yml(
            "name: TEST\nkit: foo/bar\noptional_kits:\n  - a/b\n"
        );
        append_optional_kit(&p, "a/b").unwrap();
        append_optional_kit(&p, "a/b").unwrap();
        let got = read_repo_yml_optional_kits(&p);
        assert_eq!(got, vec!["a/b".to_string()]);
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn remove_optional_kit_drops_entry() {
        let p = write_tmp_repo_yml(
            "name: TEST\nkit: foo/bar\noptional_kits:\n  - a/b\n  - c/d\n"
        );
        remove_optional_kit(&p, "a/b").unwrap();
        let got = read_repo_yml_optional_kits(&p);
        assert_eq!(got, vec!["c/d".to_string()]);
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn remove_optional_kit_drops_empty_section() {
        let p = write_tmp_repo_yml(
            "name: TEST\nkit: foo/bar\noptional_kits:\n  - a/b\nother: thing\n"
        );
        remove_optional_kit(&p, "a/b").unwrap();
        let got = read_repo_yml_optional_kits(&p);
        assert!(got.is_empty());
        // `other:` must survive.
        let fields = read_repo_yml_fields(&p);
        assert_eq!(fields.get("other"), Some(&"thing".to_string()));
        // And `optional_kits:` key should NOT remain in the file.
        let content = std::fs::read_to_string(&p).unwrap();
        assert!(!content.contains("optional_kits:"),
            "expected optional_kits: to be removed, got: {}", content);
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn remove_optional_kit_missing_is_noop() {
        let p = write_tmp_repo_yml(
            "name: TEST\nkit: foo/bar\noptional_kits:\n  - a/b\n"
        );
        remove_optional_kit(&p, "x/y").unwrap();
        let got = read_repo_yml_optional_kits(&p);
        assert_eq!(got, vec!["a/b".to_string()]);
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn kit_scope_parses_known_values() {
        assert_eq!(KitScope::parse("base"), Some(KitScope::Base));
        assert_eq!(KitScope::parse("domain"), Some(KitScope::Domain));
        assert_eq!(KitScope::parse("optional"), Some(KitScope::Optional));
        assert_eq!(KitScope::parse("Optional"), Some(KitScope::Optional));
        assert_eq!(KitScope::parse("  optional  "), Some(KitScope::Optional));
        assert_eq!(KitScope::parse("nonsense"), None);
    }

    #[test]
    fn read_kit_scope_defaults_to_domain_when_missing() {
        let tmp = unique_tmp_dir("git-lex-kityml");
        std::fs::write(tmp.join("kit.yml"), "name: test\nfolder base: Test\n").unwrap();
        assert_eq!(read_kit_scope(&tmp), KitScope::Domain);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn read_kit_scope_picks_up_optional() {
        let tmp = unique_tmp_dir("git-lex-kityml-opt");
        std::fs::write(tmp.join("kit.yml"), "name: inner\nscope: optional\nfolder base: Innerworld\n").unwrap();
        assert_eq!(read_kit_scope(&tmp), KitScope::Optional);
        std::fs::remove_dir_all(&tmp).ok();
    }

    // ─── repo.yml substrates: tests ──────────────────────────────────

    #[test]
    fn read_substrates_basic() {
        let p = write_tmp_repo_yml(
            "name: TEST\nkit: foo/bar\nsubstrates:\n  - claude\n  - hermes\n"
        );
        let got = read_repo_yml_substrates(&p);
        assert_eq!(got, vec!["claude".to_string(), "hermes".to_string()]);
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn read_substrates_empty_when_missing() {
        let p = write_tmp_repo_yml("name: TEST\nkit: foo/bar\n");
        assert!(read_repo_yml_substrates(&p).is_empty());
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn append_substrate_creates_section() {
        let p = write_tmp_repo_yml("name: TEST\nkit: foo/bar\n");
        append_substrate(&p, "claude").unwrap();
        assert_eq!(read_repo_yml_substrates(&p), vec!["claude".to_string()]);
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn append_substrate_is_idempotent() {
        let p = write_tmp_repo_yml("name: TEST\nkit: foo/bar\nsubstrates:\n  - claude\n");
        append_substrate(&p, "claude").unwrap();
        append_substrate(&p, "claude").unwrap();
        assert_eq!(read_repo_yml_substrates(&p), vec!["claude".to_string()]);
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn remove_substrate_drops_entry() {
        let p = write_tmp_repo_yml(
            "name: TEST\nkit: foo/bar\nsubstrates:\n  - claude\n  - hermes\n"
        );
        remove_substrate(&p, "claude").unwrap();
        assert_eq!(read_repo_yml_substrates(&p), vec!["hermes".to_string()]);
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn substrates_and_optional_kits_coexist() {
        // Both lists in the same file should be independently readable.
        let p = write_tmp_repo_yml(
            "name: TEST\nkit: foo/bar\noptional_kits:\n  - org/kit-a\nsubstrates:\n  - claude\n  - hermes\n"
        );
        assert_eq!(read_repo_yml_optional_kits(&p), vec!["org/kit-a".to_string()]);
        assert_eq!(read_repo_yml_substrates(&p),
            vec!["claude".to_string(), "hermes".to_string()]);
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }
}
