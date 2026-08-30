//! Gemini / Antigravity (AGY) harness adapter.
//!
//! Syncs soul documents into Antigravity's `.agents/` directory structure.
//! Soul directories are the source of truth — harness targets are always
//! overwritten on sync.
//!
//! ## Skill mapping
//!   Source: Skill/{name}.md  (soul frontmatter + markdown body)
//!   Target: .agents/skills/{name}/SKILL.md  (Antigravity frontmatter + markdown body)
//!
//!   soul.Skill.skillDescription  → description
//!   filename stem                → name
//!
//! ## Subagent mapping
//!   Source: Subagent/{name}.md  (soul frontmatter + markdown body)
//!   Target: .agents/subagents/{name}.md  (Antigravity subagent definition)

use std::fs;
use std::path::{Path, PathBuf};

use super::claude::record_harness_failure;

fn find_class_dir(root: &Path, class_name: &str) -> Option<PathBuf> {
    // Check root first (legacy flat layout)
    let flat = root.join(class_name);
    if flat.exists() && flat.is_dir() {
        return Some(flat);
    }
    // Scan namespace folders
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !path.is_dir() || name.starts_with('.') {
            continue;
        }
        let candidate = path.join(class_name);
        if candidate.exists() && candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

/// Sync all soul documents into Antigravity's `.agents/` directory structure.
pub fn sync_all(root: &Path) {
    sync_skills(root);
    sync_subagents(root);
}

/// Sync all skills from {Namespace}/Skill/ into .agents/skills/.
/// Each {Namespace}/Skill/{name}.md gets transformed into .agents/skills/{name}/SKILL.md
/// with Antigravity frontmatter derived from soul frontmatter.
fn sync_skills(root: &Path) {
    let target_dir = root.join(".agents").join("skills");

    // Find Skill/ under any namespace folder (e.g., Soul/Skill/)
    let skill_dir = match find_class_dir(root, "Skill") {
        Some(d) => d,
        None => return,
    };

    let entries = match fs::read_dir(&skill_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut synced = 0;
    let mut failed = 0;
    let mut source_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();

        if path.is_dir() {
            continue;
        }
        let fname = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !fname.ends_with(".md") || fname.starts_with("__") || fname.starts_with('.') {
            continue;
        }

        let name = fname.strip_suffix(".md").unwrap();
        source_names.insert(name.to_string());
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let agy_content = transform_skill(&content, name);

        let dest_dir = target_dir.join(name);
        if let Err(e) = fs::create_dir_all(&dest_dir) {
            eprintln!("harness: cannot create {}: {}", dest_dir.display(), e);
            failed += 1;
            continue;
        }

        let dest = dest_dir.join("SKILL.md");
        if let Err(e) = fs::write(&dest, &agy_content) {
            eprintln!("harness: failed to sync skill {}: {}", name, e);
            failed += 1;
            continue;
        }
        synced += 1;
    }

    let total = synced + failed;
    if failed > 0 {
        eprintln!(
            "harness: synced {} of {} skill(s) to .agents/skills/ — {} FAILED.",
            synced, total, failed
        );
        record_harness_failure(failed);
    } else if synced > 0 {
        println!("Gemini: synced {} skill(s) to .agents/skills/", synced);
    }

    prune_orphan_skill_dirs(root, &target_dir, &source_names);
}

/// Sync all subagents from {Namespace}/Subagent/ into .agents/subagents/.
fn sync_subagents(root: &Path) {
    let target_dir = root.join(".agents").join("subagents");

    let subagent_dir = match find_class_dir(root, "Subagent") {
        Some(d) => d,
        None => return,
    };

    let entries = match fs::read_dir(&subagent_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut synced = 0;
    let mut failed = 0;
    let mut source_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();

        if path.is_dir() {
            continue;
        }
        let fname = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !fname.ends_with(".md") || fname.starts_with("__") || fname.starts_with('.') {
            continue;
        }

        let name = fname.strip_suffix(".md").unwrap();
        source_names.insert(name.to_string());
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        if let Err(e) = fs::create_dir_all(&target_dir) {
            eprintln!("harness: cannot create {}: {}", target_dir.display(), e);
            failed += 1;
            continue;
        }

        let dest = target_dir.join(&fname);
        if let Err(e) = fs::write(&dest, &content) {
            eprintln!("harness: failed to sync subagent {}: {}", name, e);
            failed += 1;
            continue;
        }
        synced += 1;
    }

    let total = synced + failed;
    if failed > 0 {
        eprintln!(
            "harness: synced {} of {} subagent(s) to .agents/subagents/ — {} FAILED.",
            synced, total, failed
        );
        record_harness_failure(failed);
    } else if synced > 0 {
        println!("Gemini: synced {} subagent(s) to .agents/subagents/", synced);
    }
}

/// Transform a soul Skill document into an Antigravity SKILL.md.
fn transform_skill(content: &str, name: &str) -> String {
    let (soul_fm, body) = split_frontmatter(content);

    let mut description = String::new();

    for line in soul_fm.lines() {
        let line = line.trim();
        if let Some(val) = strip_fm_key(line, "soul.Skill.skillDescription") {
            description = val;
        } else if description.is_empty() {
            if let Some(val) = strip_fm_key(line, "git-lex.Skill.description") {
                description = val;
            } else if let Some(val) = strip_fm_key(line, "description") {
                description = val;
            }
        }
    }

    let mut fm = format!("---\nname: {}\n", name);
    if !description.is_empty() {
        fm.push_str(&format!("description: {}\n", description));
    }
    fm.push_str("---\n");

    format!("{}{}", fm, body)
}

fn split_frontmatter(content: &str) -> (&str, &str) {
    if !content.starts_with("---") {
        return ("", content);
    }
    let rest = &content[3..];
    let rest = rest.strip_prefix('\r').unwrap_or(rest);
    let rest = rest.strip_prefix('\n').unwrap_or(rest);

    if let Some(end) = rest.find("\n---") {
        let fm = &rest[..end];
        let after = &rest[end + 4..];
        let after = after.strip_prefix('\r').unwrap_or(after);
        let after = after.strip_prefix('\n').unwrap_or(after);
        (fm, after)
    } else {
        ("", content)
    }
}

fn strip_fm_key(line: &str, key: &str) -> Option<String> {
    let prefix = format!("{}:", key);
    if line.starts_with(&prefix) {
        let val = line[prefix.len()..].trim();
        let val = val.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(val);
        Some(val.to_string())
    } else {
        None
    }
}

fn prune_orphan_skill_dirs(
    _root: &Path,
    target_dir: &Path,
    source_names: &std::collections::HashSet<String>,
) {
    let Ok(entries) = fs::read_dir(target_dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || !path.join("SKILL.md").is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if source_names.contains(&name) {
            continue;
        }
        let rel = format!(".agents/skills/{}", name);
        if let Err(e) = fs::remove_dir_all(&path) {
            eprintln!("harness: could not prune {rel}: {e}");
        } else {
            println!("Pruned: {rel}/ — its Skill/ source is gone.");
        }
    }
}
