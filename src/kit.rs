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
use std::path::PathBuf;
use std::process::Command;

use git_lex::{find_git_root, get_kit, kit_install_dir_for_spec, resolve_kit_spec};

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
    let (_, _, short_name) = resolve_kit_spec(kit);

    // Primary: .lex/ontology/{short}/{short}.ttl
    let ontology_dir = root.join(".lex").join("ontology").join(&short_name);
    let primary = ontology_dir.join(format!("{}.ttl", short_name));
    if primary.exists() {
        return Some(primary);
    }

    // Fallback: any non-shapes .ttl in .lex/ontology/{short}/
    if ontology_dir.exists() {
        if let Some(p) = fs::read_dir(&ontology_dir).ok()
            .and_then(|entries| entries.filter_map(|e| e.ok())
                .find(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    name.ends_with(".ttl") && !name.contains("shapes")
                })
                .map(|e| e.path()))
        {
            return Some(p);
        }
    }

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

/// Install dir for the current repo's kit. Reads repo.yml to find the kit
/// spec, resolves it, and returns the path. Returns None if no kit is
/// configured.
pub(crate) fn kit_install_dir() -> Option<PathBuf> {
    let root = find_git_root()?;
    let kit = get_kit()?;
    Some(kit_install_dir_for_spec(&root, &kit))
}

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

/// Substitute `{varname}` template placeholders in text using a variable map.
pub(crate) fn substitute_vars(text: &str, vars: &HashMap<String, String>) -> String {
    let mut out = text.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{}}}", k), v);
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

/// Like `install_scaffold_files_from`, but safe for `kit-update` against a
/// live repo. If `force` is false, files that already exist in the repo are
/// left alone (preserving any local customizations an agent has made). If
/// `force` is true, behaves exactly like `install_scaffold_files_from` —
/// clobbers everything. Used by `kit-update` to refresh base-kit scaffold
/// pieces like `.lex/www/` without blowing away an agent's `.claude/`
/// customizations.
///
/// Returns `(installed, skipped)` counts.
pub(crate) fn install_scaffold_files_from_skip_existing(
    kit_dir: &std::path::Path,
    force: bool,
) -> (usize, usize) {
    let root = match find_git_root() {
        Some(r) => r,
        None => return (0, 0),
    };

    let mut installed = 0usize;
    let mut skipped = 0usize;

    fn install_recursive(
        src_dir: &std::path::Path,
        dest_dir: &std::path::Path,
        force: bool,
        installed: &mut usize,
        skipped: &mut usize,
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
                if !force && dest.symlink_metadata().is_ok() {
                    *skipped += 1;
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
                        *installed += 1;
                    }
                }
                continue;
            }

            if ft.is_dir() {
                fs::create_dir_all(&dest).ok();
                install_recursive(&src, &dest, force, installed, skipped);
                continue;
            }

            if ft.is_file() {
                if !force && dest.symlink_metadata().is_ok() {
                    *skipped += 1;
                    continue;
                }
                fs::create_dir_all(dest.parent().unwrap_or(&dest)).ok();
                if dest.symlink_metadata().is_ok() {
                    let dft = dest.symlink_metadata().ok().map(|m| m.file_type());
                    if let Some(dft) = dft {
                        if dft.is_symlink() || (dft.is_dir() && !dft.is_symlink()) {
                            if dft.is_dir() && !dft.is_symlink() {
                                let _ = fs::remove_dir_all(&dest);
                            } else {
                                let _ = fs::remove_file(&dest);
                            }
                        }
                    }
                }
                if fs::copy(&src, &dest).is_ok() {
                    *installed += 1;
                }
            }
        }
    }

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
            // Never clobber agent-owned ontology — only seed missing files
            let ontology_dest = root.join("_ontology");
            fs::create_dir_all(&ontology_dest).ok();
            // Use force=false so existing files are preserved
            install_recursive(&ontology_src, &ontology_dest, false, &mut installed, &mut skipped);
        } else {
            let ontology_dest = root.join(".lex").join("ontology");
            fs::create_dir_all(&ontology_dest).ok();
            install_recursive(&ontology_src, &ontology_dest, true, &mut installed, &mut skipped);
        }
    }

    let content_src = kit_dir.join("content");
    if content_src.exists() {
        install_recursive(&content_src, &root, force, &mut installed, &mut skipped);
    }

    let harness_src = kit_dir.join("harness");
    if harness_src.exists() {
        install_recursive(&harness_src, &root, force, &mut installed, &mut skipped);
    }

    let www_src = kit_dir.join("www");
    if www_src.exists() {
        let www_dest = root.join(".lex").join("www");
        fs::create_dir_all(&www_dest).ok();
        install_recursive(&www_src, &www_dest, force, &mut installed, &mut skipped);
    }

    // Legacy: scaffold/ → repo root
    let scaffold_dir = kit_dir.join("scaffold");
    if scaffold_dir.exists() {
        install_recursive(&scaffold_dir, &root, force, &mut installed, &mut skipped);
    }

    (installed, skipped)
}

/// Report from install_asset_files — lists what was installed, what was
/// skipped because it already existed, and what was overwritten under --force.
#[derive(Default)]
pub(crate) struct AssetInstallReport {
    pub installed: Vec<String>,  // freshly written (destination did not exist)
    pub skipped: Vec<String>,    // destination already existed, --force not set
    pub overwritten: Vec<String>, // destination existed and was overwritten under --force
}

/// Install asset files from the kit into the repo root.
/// Assets live in .lex/kit/assets/ and mirror the repo structure. They can
/// target paths anywhere under the repo, including inside .lex/ itself
/// (e.g. `assets/.lex/www/mykitpage/index.html`).
///
/// Behavior differs from scaffold:
///   - Template processing: `{varname}` placeholders are substituted from
///     the variable map before writing.
///   - Default safe mode: if the destination file already exists, skip it
///     and add to the skipped list. User can re-run with --force to
///     overwrite.
///   - Force mode: overwrite existing files. No backup file is written —
///     git history is the safety net for any lost local edits.
pub(crate) fn install_asset_files(vars: &HashMap<String, String>, force: bool) -> AssetInstallReport {
    let mut report = AssetInstallReport::default();
    let root = match find_git_root() {
        Some(r) => r,
        None => return report,
    };

    let kit_dir = match kit_install_dir() {
        Some(d) => d,
        None => return report,
    };
    let asset_dir = kit_dir.join("assets");
    if !asset_dir.exists() {
        return report;
    }

    fn install_recursive(
        src_dir: &std::path::Path,
        dest_dir: &std::path::Path,
        vars: &HashMap<String, String>,
        force: bool,
        repo_root: &std::path::Path,
        report: &mut AssetInstallReport,
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
                install_recursive(&src, &dest, vars, force, repo_root, report);
                continue;
            }
            if !src.is_file() {
                continue;
            }

            let rel = dest
                .strip_prefix(repo_root)
                .unwrap_or(&dest)
                .to_string_lossy()
                .to_string();

            if dest.exists() && !force {
                report.skipped.push(rel);
                continue;
            }

            let content = match fs::read_to_string(&src) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let processed = substitute_vars(&content, vars);
            fs::create_dir_all(dest.parent().unwrap_or(&dest)).ok();

            let was_present = dest.exists();
            if fs::write(&dest, &processed).is_ok() {
                if was_present {
                    report.overwritten.push(rel);
                } else {
                    report.installed.push(rel);
                }
            }
        }
    }

    install_recursive(&asset_dir, &root, vars, force, &root, &mut report);
    report
}

/// Recursively copy a directory tree, preserving symlinks.
pub(crate) fn copy_dir_recursive(src: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());

        // Inspect without following symlinks so we can preserve them.
        let meta = fs::symlink_metadata(&src_path)?;
        let ft = meta.file_type();

        if ft.is_symlink() {
            let target = fs::read_link(&src_path)?;
            // Remove anything existing at dest so we can create the symlink.
            if dest_path.symlink_metadata().is_ok() {
                if dest_path.is_dir() && !dest_path.is_symlink() {
                    let _ = fs::remove_dir_all(&dest_path);
                } else {
                    let _ = fs::remove_file(&dest_path);
                }
            }
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&target, &dest_path)?;
            }
            #[cfg(not(unix))]
            {
                // Windows: fall back to copying the target (best effort).
                if src_path.exists() && src_path.is_file() {
                    fs::copy(&src_path, &dest_path)?;
                }
            }
        } else if ft.is_dir() {
            copy_dir_recursive(&src_path, &dest_path)?;
        } else if ft.is_file() {
            fs::copy(&src_path, &dest_path)?;
        }
    }
    Ok(())
}

// ─── kit lifecycle ─────────────────────────────────────────────

/// Add a kit to an existing repo. Downloads the kit, installs ontology
/// and scaffold files, updates repo.yml.
///
/// Not yet implemented — currently handled inline by cmd_init.
pub(crate) fn add_kit(_kit_spec: &str) {
    unimplemented!("kit::add_kit — not yet extracted from cmd_init");
}

/// Remove a kit from a repo. Removes kit-installed files (ontology,
/// shapes, scaffold), updates repo.yml.
///
/// Not yet implemented.
pub(crate) fn remove_kit(_kit_spec: &str) {
    unimplemented!("kit::remove_kit — not yet implemented");
}
