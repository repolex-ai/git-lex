//! Claude Code harness adapter.
//!
//! Syncs skills from Skill/ into .claude/skills/. The Skill/ directory is
//! the source of truth — files in .claude/skills/ are overwritten every sync.

use std::fs;
use std::path::Path;

/// Sync all skills from Skill/ into .claude/skills/.
/// Each Skill/{name}/SKILL.md gets copied to .claude/skills/{name}/SKILL.md.
/// Overwrites existing files — Skill/ is the source of truth.
pub fn sync_skills(root: &Path) {
    let skill_dir = root.join("Skill");
    let target_dir = root.join(".claude").join("skills");

    if !skill_dir.exists() {
        return;
    }

    let entries = match fs::read_dir(&skill_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut synced = 0;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();

        // Skip non-directories and the class template (__Skill.md)
        if !path.is_dir() {
            continue;
        }

        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        let source = path.join("SKILL.md");
        if !source.exists() {
            continue;
        }

        let dest_dir = target_dir.join(&name);
        fs::create_dir_all(&dest_dir).ok();

        let dest = dest_dir.join("SKILL.md");
        if let Err(e) = fs::copy(&source, &dest) {
            eprintln!("harness: failed to sync skill {}: {}", name, e);
            continue;
        }
        synced += 1;
    }

    if synced > 0 {
        println!("Claude: synced {} skill(s) to .claude/skills/", synced);
    }
}
