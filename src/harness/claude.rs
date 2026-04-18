//! Claude Code harness adapter.
//!
//! Syncs soul documents into Claude Code's `.claude/` directory structure.
//! Soul directories are the source of truth — harness targets are always
//! overwritten on sync.
//!
//! ## Skill mapping
//!   Source: Skill/{name}.md  (soul frontmatter + markdown body)
//!   Target: .claude/skills/{name}/SKILL.md  (Claude frontmatter + markdown body)
//!
//!   soul.Skill.skillDescription  → description
//!   soul.Skill.skillInvocability → user-invocable (both/user → true, agent → false)
//!   soul.Skill.skillAllowedTools → allowed-tools
//!   soul.Skill.skillArgumentHint → argument-hint
//!   filename stem                → name
//!
//! ## Subagent mapping
//!   Source: Subagent/{name}.md  (soul frontmatter + markdown body)
//!   Target: .claude/agents/{name}.md  (Claude frontmatter + markdown body)
//!
//!   soul.Subagent.subagentDescription → description
//!   soul.Subagent.subagentTools       → tools
//!   soul.Subagent.subagentModel       → model
//!   soul.Subagent.subagentMaxTurns    → maxTurns
//!   filename stem                     → name

use std::fs;
use std::path::{Path, PathBuf};

/// Find a class directory (e.g., "Skill") under any namespace folder.
/// Scans top-level directories for a matching subfolder.
/// Returns the first match (e.g., Soul/Skill/).
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
        if !path.is_dir() || name.starts_with('.') { continue; }
        let candidate = path.join(class_name);
        if candidate.exists() && candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

/// Sync all soul documents into Claude Code's harness directories.
pub fn sync_all(root: &Path) {
    sync_skills(root);
    sync_subagents(root);
}

/// Sync all skills from {Namespace}/Skill/ into .claude/skills/.
/// Each {Namespace}/Skill/{name}.md gets transformed into .claude/skills/{name}/SKILL.md
/// with Claude Code frontmatter derived from soul frontmatter.
/// Scans all top-level directories for a Skill/ subfolder.
fn sync_skills(root: &Path) {
    let target_dir = root.join(".claude").join("skills");

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
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let claude_content = transform_skill(&content, name);

        let dest_dir = target_dir.join(name);
        fs::create_dir_all(&dest_dir).ok();

        let dest = dest_dir.join("SKILL.md");
        if let Err(e) = fs::write(&dest, &claude_content) {
            eprintln!("harness: failed to sync skill {}: {}", name, e);
            continue;
        }
        synced += 1;
    }

    if synced > 0 {
        println!("Claude: synced {} skill(s) to .claude/skills/", synced);
    }
}

/// Sync all subagents from {Namespace}/Subagent/ into .claude/agents/.
/// Each {Namespace}/Subagent/{name}.md gets transformed into .claude/agents/{name}.md
/// with Claude Code frontmatter derived from soul frontmatter.
/// Scans all top-level directories for a Subagent/ subfolder.
fn sync_subagents(root: &Path) {
    let target_dir = root.join(".claude").join("agents");

    let subagent_dir = match find_class_dir(root, "Subagent") {
        Some(d) => d,
        None => return,
    };

    let entries = match fs::read_dir(&subagent_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut synced = 0;
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
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let claude_content = transform_subagent(&content, name);

        fs::create_dir_all(&target_dir).ok();

        let dest = target_dir.join(&fname);
        if let Err(e) = fs::write(&dest, &claude_content) {
            eprintln!("harness: failed to sync subagent {}: {}", name, e);
            continue;
        }
        synced += 1;
    }

    if synced > 0 {
        println!("Claude: synced {} subagent(s) to .claude/agents/", synced);
    }
}

/// Transform a soul Skill document into a Claude Code SKILL.md.
fn transform_skill(content: &str, name: &str) -> String {
    let (soul_fm, body) = split_frontmatter(content);

    let mut description = String::new();
    let mut invocability = "both".to_string();
    let mut allowed_tools = String::new();
    let mut argument_hint = String::new();

    for line in soul_fm.lines() {
        let line = line.trim();
        if let Some(val) = strip_fm_key(line, "soul.Skill.skillDescription") {
            description = val;
        } else if let Some(val) = strip_fm_key(line, "soul.Skill.skillInvocability") {
            invocability = val;
        } else if let Some(val) = strip_fm_key(line, "soul.Skill.skillAllowedTools") {
            allowed_tools = val;
        } else if let Some(val) = strip_fm_key(line, "soul.Skill.skillArgumentHint") {
            argument_hint = val;
        }
    }

    let user_invocable = match invocability.as_str() {
        "agent" => "false",
        _ => "true",
    };

    let mut fm = format!("---\nname: {}\n", name);
    if !description.is_empty() {
        fm.push_str(&format!("description: {}\n", description));
    }
    fm.push_str(&format!("user-invocable: {}\n", user_invocable));
    if !allowed_tools.is_empty() {
        fm.push_str(&format!("allowed-tools: {}\n", allowed_tools));
    }
    if !argument_hint.is_empty() {
        fm.push_str(&format!("argument-hint: \"{}\"\n", argument_hint));
    }
    fm.push_str("---\n");

    format!("{}{}", fm, body)
}

/// Transform a soul Subagent document into a Claude Code agent definition.
fn transform_subagent(content: &str, name: &str) -> String {
    let (soul_fm, body) = split_frontmatter(content);

    let mut description = String::new();
    let mut tools = String::new();
    let mut model = String::new();
    let mut max_turns = String::new();

    for line in soul_fm.lines() {
        let line = line.trim();
        if let Some(val) = strip_fm_key(line, "soul.Subagent.subagentDescription") {
            description = val;
        } else if let Some(val) = strip_fm_key(line, "soul.Subagent.subagentTools") {
            tools = val;
        } else if let Some(val) = strip_fm_key(line, "soul.Subagent.subagentModel") {
            model = val;
        } else if let Some(val) = strip_fm_key(line, "soul.Subagent.subagentMaxTurns") {
            max_turns = val;
        }
    }

    let mut fm = format!("---\nname: {}\n", name);
    if !description.is_empty() {
        fm.push_str(&format!("description: {}\n", description));
    }
    if !tools.is_empty() {
        fm.push_str(&format!("tools: {}\n", tools));
    }
    if !model.is_empty() {
        fm.push_str(&format!("model: {}\n", model));
    }
    if !max_turns.is_empty() {
        fm.push_str(&format!("maxTurns: {}\n", max_turns));
    }
    fm.push_str("---\n");

    format!("{}{}", fm, body)
}

/// Split YAML frontmatter from body.
fn split_frontmatter(content: &str) -> (String, String) {
    if !content.starts_with("---\n") {
        return (String::new(), content.to_string());
    }
    if let Some(end) = content[4..].find("\n---\n") {
        let fm = content[4..4 + end].to_string();
        let body = content[4 + end + 4..].to_string();
        (fm, body)
    } else if let Some(end) = content[4..].find("\n---") {
        let fm = content[4..4 + end].to_string();
        let body = content.get(4 + end + 4..).unwrap_or("").to_string();
        (fm, body)
    } else {
        (String::new(), content.to_string())
    }
}

/// Extract a value from a YAML frontmatter line like `key: "value"` or `key: value`.
fn strip_fm_key(line: &str, key: &str) -> Option<String> {
    let prefix = format!("{}:", key);
    if !line.starts_with(&prefix) {
        return None;
    }
    let val = line[prefix.len()..].trim();
    let val = val.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(val);
    Some(val.to_string())
}
