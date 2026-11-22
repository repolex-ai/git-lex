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
use std::path::Path;

use super::claude::record_harness_failure;

/// Set up Antigravity / Gemini substrate: copy hooks, rules, and configurations
/// shipped by installed kits under `.lex/kit/*/*/harness/.agents/`.
pub fn setup_substrate_gemini(root: &Path, _agent_name: &str) {
    let agents_dir = root.join(".agents");
    let hooks_dir = agents_dir.join("hooks");
    let rules_dir = agents_dir.join("rules");
    let _ = fs::create_dir_all(&hooks_dir);
    let _ = fs::create_dir_all(&rules_dir);

    // `.agents/hooks.json` itself is composed from every installed kit by
    // `git_lex::kit::compose_agents_hooks`, before this runs (#40).
    let lex_kit = git_lex::layout::kits_dir(root);
    if let Ok(entries) = fs::read_dir(&lex_kit) {
        for org_entry in entries.flatten() {
            if let Ok(kit_entries) = fs::read_dir(org_entry.path()) {
                for kit_entry in kit_entries.flatten() {
                    let kit_agents = kit_entry.path().join("harness").join(".agents");
                    if !kit_agents.exists() {
                        continue;
                    }
                    // Copy hooks scripts
                    let src_hooks = kit_agents.join("hooks");
                    if let Ok(hook_files) = fs::read_dir(&src_hooks) {
                        for hf in hook_files.flatten() {
                            let dst = hooks_dir.join(hf.file_name());
                            let _ = fs::copy(hf.path(), dst);
                        }
                    }
                    // Copy rules
                    let src_rules = kit_agents.join("rules");
                    if let Ok(rule_files) = fs::read_dir(&src_rules) {
                        for rf in rule_files.flatten() {
                            let dst = rules_dir.join(rf.file_name());
                            let _ = fs::copy(rf.path(), dst);
                        }
                    }
                }
            }
        }
    }

    println!("Gemini: reconciled .agents/ customization tree");
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
    let Some(sources) = super::claude::class_sources(root, "Skill") else { return };

    let mut synced = 0;
    let mut failed = 0;
    let mut source_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    // A name two folders both hold is deployed from neither (see
    // `class_sources`); it still counts as a source, so nothing is pruned.
    for (name, paths) in &sources.clashes {
        let files: Vec<String> = paths.iter().map(|p| p.strip_prefix(root).unwrap_or(p).display().to_string()).collect();
        eprintln!("harness: {name} is the name of more than one file: {}. Rename one; neither is deployed until then.", files.join(", "));
        failed += 1;
        source_names.insert(name.clone());
    }
    for (name, path) in &sources.files {
        let name = name.as_str();
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

    let Some(sources) = super::claude::class_sources(root, "Subagent") else { return };

    let mut synced = 0;
    let mut failed = 0;
    let mut source_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    // A name two folders both hold is deployed from neither (see
    // `class_sources`); it still counts as a source, so nothing is pruned.
    for (name, paths) in &sources.clashes {
        let files: Vec<String> = paths.iter().map(|p| p.strip_prefix(root).unwrap_or(p).display().to_string()).collect();
        eprintln!("harness: {name} is the name of more than one file: {}. Rename one; neither is deployed until then.", files.join(", "));
        failed += 1;
        source_names.insert(name.clone());
    }
    for (name, path) in &sources.files {
        let name = name.as_str();
        let fname = format!("{name}.md");
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
        if description.is_empty()
            && let Some(val) = strip_fm_key(line, "description") {
                description = val;
            }
    }
    // The class's own key wins over a bare `description:` line.
    if let Some(val) = super::claude::skill_value(&soul_fm, "description", "skillDescription") {
        description = val;
    }
    let description = super::claude::with_cue(description, super::claude::skill_value(&soul_fm, "cue", "cue"));

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
