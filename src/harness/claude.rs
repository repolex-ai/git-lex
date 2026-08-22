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
use std::process::exit;

use crate::kit::read_repo_yml_fields;

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
///
/// After syncing, PRUNES deployed skill dirs whose source is gone (selkie's
/// 2026-08-22 find: the sync wrote but never removed, so deleted skills
/// stayed deployed — and invocable — forever). The prune arms ONLY when a
/// Skill/ source dir exists: a repo without one (skills authored directly
/// in .claude/skills/, the pre-Skill-class era) has no source of truth to
/// converge to, and its deployed skills are not orphans.
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
    // Every eligible source stem, whether or not its transform succeeds —
    // a present-but-unreadable source must still protect its deployed dir
    // from the prune (a read blip is not a deletion).
    let mut source_names: std::collections::HashSet<String> =
        std::collections::HashSet::new();
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

    prune_orphan_skill_dirs(root, &target_dir, &source_names);
}

/// Sync all subagents from {Namespace}/Subagent/ into .claude/agents/.
/// Each {Namespace}/Subagent/{name}.md gets transformed into .claude/agents/{name}.md
/// with Claude Code frontmatter derived from soul frontmatter.
/// Scans all top-level directories for a Subagent/ subfolder.
///
/// Prunes like the skills lane, under the same arming rule: no Subagent/
/// source dir → no prune. That rule is load-bearing here — a repo that
/// retired the Subagent class and keeps `.claude/agents/*.md` as the single
/// home (W4R3Z's researcher.md, Day-55 ruling) never enters this function's
/// prune at all.
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
    let mut source_names: std::collections::HashSet<String> =
        std::collections::HashSet::new();
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

    prune_orphan_agent_files(root, &target_dir, &source_names);
}

/// Union of deploy names an installed kit ships under
/// `harness/.claude/<last_seg>/` across ALL kits the repo lists — the
/// full-name-set discipline from the hook reap (#28: a partial set reaps
/// other kits' files). Returns None when any listed kit's vendored dir is
/// missing: we then can't know what it ships, so the caller must SKIP the
/// prune this run rather than guess (the hook reap's `reap_safe` rule).
fn kit_shipped_deploy_names(
    root: &Path,
    last_seg: &str,
) -> Option<std::collections::HashSet<String>> {
    let mut names = std::collections::HashSet::new();
    let lex_kit = root.join(".lex").join("kit");
    for spec in crate::kit_cmds::collect_kits_for_update(root, None) {
        let (org, repo, _) = git_lex::resolve_kit_spec(&spec);
        let kit_dir = lex_kit.join(&org).join(&repo);
        if !kit_dir.exists() {
            return None;
        }
        let deploy_src = kit_dir.join("harness").join(".claude").join(last_seg);
        if let Ok(entries) = fs::read_dir(&deploy_src) {
            for entry in entries.flatten() {
                names.insert(entry.file_name().to_string_lossy().to_string());
            }
        }
    }
    Some(names)
}

/// Is every file under `path` tracked by git, with nothing untracked
/// (ignored counts as untracked — an ignored file is just as unrecoverable
/// after deletion)? Only a fully-tracked target may be auto-pruned: its
/// bytes live in git history, so the prune is a convergence, not a loss.
fn fully_tracked(root: &Path, path: &Path) -> bool {
    let rel = match path.strip_prefix(root) {
        Ok(r) => r.to_string_lossy().to_string(),
        Err(_) => return false,
    };
    let run = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).to_string())
    };
    let tracked = match run(&["ls-files", "--", &rel]) {
        Some(o) => !o.trim().is_empty(),
        None => return false,
    };
    let untracked = match run(&["ls-files", "--others", "--", &rel]) {
        Some(o) => !o.trim().is_empty(),
        None => return true, // can't prove clean → not prunable
    };
    tracked && !untracked
}

/// Remove deployed skill dirs whose source is gone (the sync wrote but
/// never pruned — deleted skills stayed deployed and invocable forever).
/// A deployed dir survives when its name is source-backed OR shipped by an
/// installed kit's harness tree. Orphans are removed only when git fully
/// tracks them (recoverable — the bytes live in history); anything with
/// never-committed content is refused LOUDLY instead: whose move it is and
/// the one action, never a silent unrecoverable delete.
fn prune_orphan_skill_dirs(
    root: &Path,
    target_dir: &Path,
    source_names: &std::collections::HashSet<String>,
) {
    let Some(kit_names) = kit_shipped_deploy_names(root, "skills") else {
        eprintln!(
            "harness: a listed kit has no install dir — skipping the skill \
             prune this run (can't know which skills it ships). Run \
             `git lex kit-update` to repair."
        );
        return;
    };
    let Ok(entries) = fs::read_dir(target_dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        // Only dirs shaped like a deployed skill (the shape this adapter
        // writes). Loose files and asset dirs are not ours to judge.
        if !path.is_dir() || !path.join("SKILL.md").is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if source_names.contains(&name) || kit_names.contains(&name) {
            continue;
        }
        let rel = format!(".claude/skills/{}", name);
        if fully_tracked(root, &path) {
            match fs::remove_dir_all(&path) {
                Ok(_) => println!(
                    "Pruned: {rel}/ — its Skill/ source is gone; the deployed \
                     copy lives in git history (git log -- {rel})."
                ),
                Err(e) => eprintln!("harness: could not prune {rel}: {e}"),
            }
        } else {
            eprintln!(
                "warning: {rel}/ has no Skill/ source but contains files never \
                 committed to git — NOT pruned (deleting them would be \
                 unrecoverable). To keep it, author Skill/{name}.md; to drop \
                 it, delete the directory yourself."
            );
        }
    }
}

/// The agents-lane twin of `prune_orphan_skill_dirs`: deployed
/// `.claude/agents/*.md` files whose Subagent/ source is gone.
fn prune_orphan_agent_files(
    root: &Path,
    target_dir: &Path,
    source_names: &std::collections::HashSet<String>,
) {
    let Some(kit_names) = kit_shipped_deploy_names(root, "agents") else {
        eprintln!(
            "harness: a listed kit has no install dir — skipping the agent \
             prune this run (can't know which agents it ships). Run \
             `git lex kit-update` to repair."
        );
        return;
    };
    let Ok(entries) = fs::read_dir(target_dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let fname = entry.file_name().to_string_lossy().to_string();
        if !path.is_file() || !fname.ends_with(".md") {
            continue;
        }
        let stem = fname.strip_suffix(".md").unwrap_or(&fname).to_string();
        if source_names.contains(&stem) || kit_names.contains(&fname) {
            continue;
        }
        let rel = format!(".claude/agents/{}", fname);
        if fully_tracked(root, &path) {
            match fs::remove_file(&path) {
                Ok(_) => println!(
                    "Pruned: {rel} — its Subagent/ source is gone; the deployed \
                     copy lives in git history (git log -- {rel})."
                ),
                Err(e) => eprintln!("harness: could not prune {rel}: {e}"),
            }
        } else {
            eprintln!(
                "warning: {rel} has no Subagent/ source but was never committed \
                 to git — NOT pruned (deleting it would be unrecoverable). To \
                 keep it, author Subagent/{stem}.md; to drop it, delete the \
                 file yourself."
            );
        }
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

/// Read the agent identity from the settings.json env block this module
/// WRITES (setup_substrate_claude). Reader and writer of the env schema
/// live in one file (review #38) — which file, which keys — so the schema
/// cannot drift across a module boundary; resolve_agent_identity calls
/// this as its last-fallback tier 3.
pub(crate) fn read_identity_env(root: &Path) -> Option<(String, String)> {
    let path = root.join(".claude").join("settings.json");
    let content = fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    let env = v.get("env")?.as_object()?;
    let name = env.get("GIT_AUTHOR_NAME")?.as_str()?.to_string();
    let email = env.get("GIT_AUTHOR_EMAIL")?.as_str()?.to_string();
    if name.is_empty() || email.is_empty() {
        return None;
    }
    Some((name, email))
}

/// Split YAML frontmatter from body — thin adapter over the crate-wide
/// fence parser (git_lex::split_frontmatter, review #9). The old local
/// scanner rejected CRLF openers, so a CRLF Skill/Subagent doc silently
/// lost all its soul.* keys and dumped the raw `---` block into the
/// generated .claude/ file body. This module's callers want owned strings
/// and an empty string for "no frontmatter".
fn split_frontmatter(content: &str) -> (String, String) {
    let (fm, body) = git_lex::split_frontmatter(content);
    (fm.unwrap_or("").to_string(), body.to_string())
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

/// The canonical set of Claude Code hook event names, per the docs
/// (https://code.claude.com/docs/en/hooks.md, verified Day 48). A hook file's
/// event is the segment of its filename BEFORE the first '-' (or the whole stem if
/// there is no '-'), and it MUST be one of these or kit-update hard-errors — a
/// filename that strips to a non-event silently never fires (the R11 ghost). CC
/// events are CamelCase with no internal hyphen, which is what makes "split on
/// first '-'" unambiguous forever (see hook_event_for).
///
/// This is the FULL documented set, not just the events we currently ship — a kit
/// shipping a legitimate `PostCompact-*.sh` or `PreToolUse-*.sh` must register, not
/// be rejected. Rejecting a real event would be a worse failure than the ghost we
/// fix. Keep in sync with the docs if CC adds events.
const CC_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "Setup",
    "UserPromptSubmit",
    "UserPromptExpansion",
    "PreToolUse",
    "PermissionRequest",
    "PermissionDenied",
    "PostToolUse",
    "PostToolUseFailure",
    "PostToolBatch",
    "Notification",
    "MessageDisplay",
    "SubagentStart",
    "SubagentStop",
    "TaskCreated",
    "TaskCompleted",
    "Stop",
    "StopFailure",
    "TeammateIdle",
    "InstructionsLoaded",
    "ConfigChange",
    "CwdChanged",
    "FileChanged",
    "WorktreeCreate",
    "WorktreeRemove",
    "PreCompact",
    "PostCompact",
    "Elicitation",
    "ElicitationResult",
    "SessionEnd",
];

/// Parse a hook filename into its Claude Code event, per the §3.2a naming standard.
///
/// A hook file is named `<Event>-<kit>-<purpose>.sh`. We split on the FIRST '-':
/// the part before it is the event; everything after is a free, kit-owned
/// namespace whose only job is to make the filename unique so N kits can each ship
/// a hook for the same event (e.g. `UserPromptSubmit-soul-recall.sh` +
/// `UserPromptSubmit-pool-share.sh` both register under `UserPromptSubmit`). A file
/// with no '-' (e.g. `SessionStart.sh`) has the whole stem as the event.
///
/// "First '-'" is unambiguous because every CC event is CamelCase with no internal
/// hyphen (see CC_HOOK_EVENTS).
///
/// Returns:
///   Ok(Some(event))  — a real CC event; register under it.
///   Ok(None)         — not a `.sh` file, or a dotfile; skip silently.
///   Err(msg)         — a `.sh` hook whose leading segment is NOT a CC event. The
///                      caller HARD-ERRORS with this message (prefer-the-crash: a
///                      misnamed hook that never fires is the R11 silent failure).
fn hook_event_for(filename: &str) -> Result<Option<&'static str>, String> {
    let Some(stem) = filename.strip_suffix(".sh") else {
        return Ok(None); // not a hook script
    };
    if stem.is_empty() || stem.starts_with('.') {
        return Ok(None); // empty or dotfile (e.g. ".gitkeep.sh" edge)
    }
    // The event is the segment before the first '-', or the whole stem if none.
    let candidate = stem.split('-').next().unwrap_or(stem);
    match CC_HOOK_EVENTS.iter().find(|&&e| e == candidate) {
        Some(&event) => Ok(Some(event)),
        None => Err(format!(
            "hook '{}': '{}' is not a Claude Code event. \
             Hook files must be named <Event>-<kit>-<purpose>.sh where <Event> is \
             one of: {}. Refusing to register a hook that would never fire.",
            filename,
            candidate,
            CC_HOOK_EVENTS.join(", ")
        )),
    }
}

/// Set up Claude Code substrate: write git identity env vars and register
/// any hooks into .claude/settings.json (committed). Souls are portable
/// across machines via git — checking identity in keeps it traveling with
/// the repo. Anyone running a Claude Code session in this soul commits as
/// this soul, which is the correct semantics: the soul *is* the agent.
pub(crate) fn setup_substrate_claude(root: &std::path::Path, agent_name: &str) {
    let settings_path = root.join(".claude").join("settings.json");
    if let Err(e) = fs::create_dir_all(settings_path.parent().unwrap()) {
        eprintln!(
            "ERROR: could not create .claude/ under {}: {e} — substrate setup cannot proceed.",
            root.display()
        );
        std::process::exit(1);
    }

    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = fs::read_to_string(&settings_path).unwrap_or_default();
        // A file that exists but doesn't parse is the USER'S config in a
        // damaged state — replacing it wholesale would silently wipe every
        // setting they hand-authored. Refuse and teach instead.
        match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "ERROR: {} exists but is not valid JSON ({e}).\n\
                     Refusing to overwrite it — that would wipe your hand-authored \
                     settings. Fix the JSON (or move the file aside), then re-run \
                     `git lex kit-update`.",
                    settings_path.display()
                );
                std::process::exit(1);
            }
        }
    } else {
        serde_json::json!({})
    };

    // Kit-managed banner. JSON has no comments, but Claude Code ignores unknown
    // top-level keys (like `$schema`), so a `_comment` key survives as a visible
    // in-file warning. It's the sign on the door; the real lock is convergence —
    // git-lex reconciles the env + hooks blocks on every kit-update, so a hand-edit
    // gets reverted next update anyway. Re-asserted here on every write.
    settings["_comment"] = serde_json::json!(
        "MANAGED BY git-lex — do not hand-edit the env or hooks blocks. They are \
         converged from your installed kits on every `git lex kit-update` (which runs \
         automatically at compaction), so local edits will be reverted. Add personal \
         hooks as `<Event>-local-<purpose>.sh` and configure them in settings.local.json. \
         To DISABLE a kit-managed hook locally, add its basename (no .sh) to \
         `soul.disabledHooks` in settings.local.json (e.g. \
         {\"soul\":{\"disabledHooks\":[\"UserPromptSubmit-soul-recall\"]}}) — the hook \
         stays registered but no-ops, and settings.local.json is never converged. \
         Edit this file and you will be eaten by a GRUE. 🦖"
    );

    // Git identity env vars — injected into every Bash tool call.
    // Email source of truth: optional `agent_email:` in .lex/repo.yml
    // (so a soul can use a real public address like their GitHub email).
    // Falls back to the generated `<slug>@lex.local` form for souls who
    // never set one. Without this, every `git lex kit-update` would silently
    // clobber a custom-set email in settings.json with the @lex.local default.
    let repo_yml = read_repo_yml_fields(&root.join(".lex").join("repo.yml"));
    let email = repo_yml
        .get("agent_email")
        .cloned()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{}@lex.local", agent_name.to_lowercase()));
    if !settings.get("env").is_some() {
        settings["env"] = serde_json::json!({});
    }
    let env = settings["env"].as_object_mut().unwrap();
    env.insert("GIT_AUTHOR_NAME".to_string(), serde_json::json!(agent_name));
    env.insert("GIT_AUTHOR_EMAIL".to_string(), serde_json::json!(email));
    env.insert("GIT_COMMITTER_NAME".to_string(), serde_json::json!(agent_name));
    env.insert("GIT_COMMITTER_EMAIL".to_string(), serde_json::json!(email));

    // Auto-memory home (Rob-ruled 2026-08-02): Claude Code's auto-memory
    // lives IN the soul repo — `Harness/Memory/`, committed, visible to
    // git-lex as Files with full history — instead of the harness-default
    // `~/.claude/projects/<cwd-slug>/memory/`. Soul repos only (the soul
    // kit ships the folder scaffold; a squad/work repo keeps the default).
    // CC accepts only absolute or `~`-prefixed values (no variables, no
    // relative paths); the `~`-form is preferred so committed settings
    // survive a different home dir, and since this converges on every
    // kit-update, a MOVED repo self-heals on its next update.
    if crate::soul_md::soul_kit_installed(root) {
        settings["autoMemoryDirectory"] =
            serde_json::json!(auto_memory_dir_value(root, std::env::var("HOME").ok().as_deref()));
    }

    // Auto-register any hook scripts the kit's harness/.claude/hooks/ shipped.
    // Each hook file is named `<Event>-<kit>-<purpose>.sh` (§3.2a naming standard);
    // hook_event_for parses it to its CC event (split on first '-'). This lets N
    // kits each ship a hook for the same event (e.g. UserPromptSubmit-soul-recall.sh
    // + UserPromptSubmit-pool-share.sh) — CC merges the registered entries.
    //
    // First RECONCILE: prune any git-lex-managed registration whose target .sh no
    // longer exists (task #90 orphan reap — a renamed/removed hook must not leave a
    // ghost). Then register the current files. A file whose leading segment is not a
    // real CC event is a HARD ERROR (prefer-the-crash: a hook that would never fire
    // is the R11 silent failure).
    let hooks_dir = root.join(".claude").join("hooks");
    reap_orphan_hook_registrations(&mut settings, &hooks_dir);
    if let Ok(entries) = fs::read_dir(&hooks_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            let event = match hook_event_for(&name) {
                Ok(Some(event)) => event,
                Ok(None) => continue, // not a hook script / dotfile
                Err(msg) => {
                    eprintln!("error: {}", msg);
                    exit(1);
                }
            };
            let cmd = format!(
                r#"bash "$CLAUDE_PROJECT_DIR/.claude/hooks/{}""#,
                name
            );
            register_hook_in_settings(&mut settings, event, &cmd);
        }
    }

    let json_str = serde_json::to_string_pretty(&settings).unwrap();
    // A swallowed failure here is the worst well-dressed-dead in the crate:
    // this write is what makes commits attribute correctly and hooks FIRE.
    // Printing success over a failed write left an installed-LOOKING repo
    // with a dead hook layer (the #67 disease, at the writer itself).
    match fs::write(&settings_path, json_str + "\n") {
        Ok(()) => {
            println!("Claude Code: identity and hooks written to .claude/settings.json");
        }
        Err(e) => {
            eprintln!(
                "ERROR: could not write {}: {e}\n\
                 Identity env and hook registrations did NOT land — commits may \
                 attribute to the wrong author and kit hooks will not fire. Fix the \
                 underlying problem (permissions/disk), then re-run `git lex kit-update`.",
                settings_path.display()
            );
            std::process::exit(1);
        }
    }

    // Older git-lex versions wrote the agent's git author env into
    // .claude/settings.local.json (gitignored); it moved to committed
    // settings.json (souls are portable). Claude Code loads local AFTER
    // project, so a leftover GIT_* env block silently outvotes the file we
    // just wrote and saves attribute to the wrong author. Warn ONLY on an
    // actual CONFLICT — a local key whose value differs from the identity
    // just written. settings.local.json itself is a healthy, live file
    // (Claude Code keeps permission grants there; `soul.disabledHooks`
    // lives there by design), and a local block that agrees with the
    // committed one changes nothing, so neither is a finding. A warning
    // someone sees fifty times on a healthy repo teaches them to ignore
    // warnings (Rob, 2026-08-22 — the old exists-check did exactly that).
    let local_path = root.join(".claude").join("settings.local.json");
    if let Ok(txt) = fs::read_to_string(&local_path) {
        let conflicts = local_settings_identity_conflicts(&txt, agent_name, &email);
        if !conflicts.is_empty() {
            eprintln!();
            eprintln!(
                "warning: .claude/settings.local.json overrides your git identity: {}.\n\
                 Claude Code loads that file AFTER .claude/settings.json, so the \
                 LOCAL values win: `git lex save` will sign commits with them, not \
                 with the identity kit-update just wrote to settings.json (from \
                 .lex/repo.yml). If that is not intentional, remove those key(s) \
                 from the env block of .claude/settings.local.json — keep the rest \
                 of the file, Claude Code stores permissions and hook opt-outs \
                 there.",
                conflicts.join(", ")
            );
        }
    }
}

/// The git author/committer env keys in a `.claude/settings.local.json`
/// whose values CONFLICT with the identity being written to committed
/// settings.json — the one thing in that file that changes who signs a
/// save. Keys that agree with the committed identity are not conflicts
/// (redundant, but they change nothing; the moment repo.yml changes, they
/// stop agreeing and this fires — exactly when it matters). Pure so the
/// gate is testable; unparseable JSON returns empty — a broken local
/// settings file breaks Claude Code visibly on its own.
fn local_settings_identity_conflicts(txt: &str, name: &str, email: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(txt) else {
        return Vec::new();
    };
    let Some(env) = v.get("env").and_then(|e| e.as_object()) else {
        return Vec::new();
    };
    let expected = [
        ("GIT_AUTHOR_NAME", name),
        ("GIT_AUTHOR_EMAIL", email),
        ("GIT_COMMITTER_NAME", name),
        ("GIT_COMMITTER_EMAIL", email),
    ];
    expected
        .iter()
        .filter_map(|(k, want)| {
            let got = env.get(*k)?.as_str().unwrap_or("");
            if got != *want {
                Some(format!("{k}=\"{got}\" (committed: \"{want}\")"))
            } else {
                None
            }
        })
        .collect()
}

/// The `autoMemoryDirectory` value for a soul repo: `<root>/Harness/Memory`,
/// `~`-shortened when the repo lives under the given home dir. Pure — the
/// caller supplies `$HOME` (None in tests / exotic environments → absolute).
fn auto_memory_dir_value(root: &std::path::Path, home: Option<&str>) -> String {
    let abs = root.join("Harness").join("Memory");
    let abs_str = abs.to_string_lossy().to_string();
    match home {
        Some(h) if !h.is_empty() => match abs_str.strip_prefix(h) {
            Some(rest) if rest.starts_with('/') => format!("~{}", rest),
            _ => abs_str,
        },
        _ => abs_str,
    }
}

/// Extract the hook-script BASENAME from a git-lex-managed hook command, if it is
/// one. We only recognize the exact shape we emit:
///   `bash "$CLAUDE_PROJECT_DIR/.claude/hooks/<name>.sh"`
/// Returns Some("<name>.sh") for our commands, None for anything hand-authored
/// (so the reaper never touches a user's own hook). The marker is the literal
/// prefix + suffix; a command that doesn't match both is left alone.
fn managed_hook_basename(command: &str) -> Option<&str> {
    const PREFIX: &str = r#"bash "$CLAUDE_PROJECT_DIR/.claude/hooks/"#;
    const SUFFIX: &str = r#"""#;
    let inner = command.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    // Guard against a nested path or an empty name — we only manage flat *.sh files.
    if inner.is_empty() || inner.contains('/') || !inner.ends_with(".sh") {
        return None;
    }
    Some(inner)
}

/// Reconcile git-lex-managed hook registrations against the files actually on disk
/// (task #90 orphan reap). Removes any registration WE emitted whose target
/// `.claude/hooks/<name>.sh` no longer exists — this is what kills the
/// `Stop-copia-moment` ghost when a hook is renamed/removed, and makes a kit
/// renaming a hook Just Work on the next update. Hand-authored hook entries (any
/// command not matching our exact emit shape) are NEVER touched. Empty event
/// arrays left behind are removed so settings.json stays clean.
fn reap_orphan_hook_registrations(settings: &mut serde_json::Value, hooks_dir: &std::path::Path) {
    let Some(hooks_obj) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return; // no hooks block yet — nothing to reap
    };
    let mut empty_events: Vec<String> = Vec::new();
    for (event, entries) in hooks_obj.iter_mut() {
        let Some(arr) = entries.as_array_mut() else { continue };
        arr.retain(|entry| {
            // An entry is `{"hooks": [{"type":"command","command":"..."}]}`. Keep it
            // unless EVERY command in it is a managed hook pointing at a missing file.
            let commands = entry
                .get("hooks")
                .and_then(|h| h.as_array())
                .map(|hooks| {
                    hooks
                        .iter()
                        .filter_map(|h| h.get("command").and_then(|c| c.as_str()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if commands.is_empty() {
                return true; // malformed / not ours — leave it
            }
            // Drop the entry only if it is fully ours AND all its targets are gone.
            let all_managed_and_missing = commands.iter().all(|cmd| {
                match managed_hook_basename(cmd) {
                    Some(name) => !hooks_dir.join(name).exists(), // ours + file gone → orphan
                    None => false,                                // hand-authored → keep
                }
            });
            !all_managed_and_missing
        });
        if arr.is_empty() {
            empty_events.push(event.clone());
        }
    }
    for event in empty_events {
        hooks_obj.remove(&event);
    }
}

/// Add a hook entry to a settings JSON value (in-memory merge, no file I/O).
/// Avoids duplicates by checking if the command is already registered. The
/// companion `reap_orphan_hook_registrations` (called first in setup_substrate_claude)
/// handles removal of stale registrations, so add + reap together give convergent
/// (not merely additive) hook reconciliation.
fn register_hook_in_settings(settings: &mut serde_json::Value, event: &str, command: &str) {
    let hook_entry = serde_json::json!({
        "hooks": [{"type": "command", "command": command}]
    });

    if !settings.get("hooks").is_some() {
        settings["hooks"] = serde_json::json!({});
    }
    let hooks_obj = settings["hooks"].as_object_mut().unwrap();
    if !hooks_obj.contains_key(event) {
        hooks_obj.insert(event.to_string(), serde_json::json!([]));
    }
    let event_hooks = hooks_obj.get_mut(event).unwrap().as_array_mut().unwrap();
    let already = event_hooks.iter().any(|entry| {
        entry.get("hooks")
            .and_then(|h| h.as_array())
            .map(|arr| arr.iter().any(|h| h.get("command").and_then(|c| c.as_str()) == Some(command)))
            .unwrap_or(false)
    });
    if !already {
        event_hooks.push(hook_entry);
    }
}

#[cfg(test)]
mod hook_registration_tests {
    use super::*;
    use std::fs;

    // ---- hook_event_for: the §3.2a naming-standard parser ----

    #[test]
    fn plain_event_filename_parses() {
        assert_eq!(hook_event_for("SessionStart.sh"), Ok(Some("SessionStart")));
        assert_eq!(hook_event_for("Stop.sh"), Ok(Some("Stop")));
        assert_eq!(hook_event_for("UserPromptSubmit.sh"), Ok(Some("UserPromptSubmit")));
    }

    #[test]
    fn namespaced_filename_parses_to_leading_event() {
        // The whole point: two kits, same event, distinct filenames — both register.
        assert_eq!(
            hook_event_for("UserPromptSubmit-soul-recall.sh"),
            Ok(Some("UserPromptSubmit"))
        );
        assert_eq!(
            hook_event_for("UserPromptSubmit-pool-share.sh"),
            Ok(Some("UserPromptSubmit"))
        );
        assert_eq!(
            hook_event_for("Stop-copia-moment.sh"),
            Ok(Some("Stop"))
        );
    }

    #[test]
    fn r11_ghost_is_now_a_hard_error() {
        // The historical R11 failure: this filename used to strip to the fake event
        // "UserPromptSubmit-copia-moment" and silently never fire. Now it PARSES
        // (leading segment IS a real event) — this is the fix, it registers under
        // UserPromptSubmit. Proven by namespaced_filename_parses_to_leading_event;
        // here we assert the genuinely-bad case errors loud.
        let err = hook_event_for("Uzerprompt-foo.sh");
        assert!(err.is_err(), "a non-CC leading segment must hard-error");
        let msg = err.unwrap_err();
        assert!(msg.contains("Uzerprompt"), "error names the offending segment");
        assert!(msg.contains("not a Claude Code event"));
    }

    #[test]
    fn bad_event_with_no_hyphen_also_errors() {
        // A whole-stem non-event (someone typos the event itself) must not slip
        // through as a ghost — hard error.
        assert!(hook_event_for("Sessionstart.sh").is_err()); // wrong casing
        assert!(hook_event_for("preToolUse.sh").is_err());   // wrong casing
    }

    #[test]
    fn non_sh_and_dotfiles_are_skipped_not_errors() {
        assert_eq!(hook_event_for("README.md"), Ok(None));
        assert_eq!(hook_event_for(".gitkeep"), Ok(None));
        assert_eq!(hook_event_for("notascript"), Ok(None));
    }

    #[test]
    fn shared_library_sh_in_hooks_dir_is_rejected_so_the_optout_guard_stays_inline() {
        // DESIGN LOCK (soul.disabledHooks opt-out, §3.2c): the kit-hook opt-out guard is
        // duplicated verbatim into every hook script rather than sourced from a shared
        // `hook-common.sh` / `_hook-guard.sh`. The reason is mechanical, and this test
        // pins it: any `.sh` in `.claude/hooks/` whose leading segment is not a CC event
        // is a HARD ERROR here (it would register a hook that never fires — the R11 silent
        // failure), and is also reaped as a non-kit file. A sourced library can't live in
        // that dir. So the guard is inlined; do not "DRY it up" into a shared script — that
        // would crash every kit-update. If you ever DO want a shared lib, it must live
        // OUTSIDE .claude/hooks/ and both hook_event_for AND the reaper must learn to skip it.
        assert!(hook_event_for("hook-common.sh").is_err());
        assert!(hook_event_for("_hook-guard.sh").is_err());
        // and the error names the offending non-event segment
        assert!(hook_event_for("hook-common.sh")
            .unwrap_err()
            .contains("not a Claude Code event"));
    }

    #[test]
    fn every_documented_event_is_accepted_plain_and_namespaced() {
        for &event in CC_HOOK_EVENTS {
            let plain = format!("{event}.sh");
            assert_eq!(
                hook_event_for(&plain),
                Ok(Some(event)),
                "plain {plain} should parse to {event}"
            );
            let namespaced = format!("{event}-somekit-purpose.sh");
            assert_eq!(
                hook_event_for(&namespaced),
                Ok(Some(event)),
                "namespaced {namespaced} should parse to {event}"
            );
        }
    }

    // ---- managed_hook_basename: only OUR emit shape is recognized ----

    #[test]
    fn managed_basename_matches_our_emit_shape_only() {
        assert_eq!(
            managed_hook_basename(r#"bash "$CLAUDE_PROJECT_DIR/.claude/hooks/Stop.sh""#),
            Some("Stop.sh")
        );
        // Hand-authored / different shapes are NOT ours — reaper must never touch them.
        assert_eq!(managed_hook_basename("/usr/local/bin/my-hook.sh"), None);
        assert_eq!(
            managed_hook_basename(r#"bash "$CLAUDE_PROJECT_DIR/.claude/hooks/nested/x.sh""#),
            None
        );
        assert_eq!(managed_hook_basename(r#"echo "not a bash hook""#), None);
    }

    // ---- reap_orphan_hook_registrations: convergent removal ----

    #[test]
    fn reaper_removes_orphan_keeps_live_and_handauthored() {
        let tmp = std::env::temp_dir().join(format!("glx_reap_test_{}", std::process::id()));
        let hooks_dir = tmp.join(".claude").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        // Only Stop.sh exists on disk; the copia-moment one is "removed".
        fs::write(hooks_dir.join("Stop.sh"), "#!/bin/bash\n").unwrap();

        let mut settings = serde_json::json!({
            "hooks": {
                "Stop": [
                    { "hooks": [{"type":"command","command":"bash \"$CLAUDE_PROJECT_DIR/.claude/hooks/Stop.sh\""}] },
                    { "hooks": [{"type":"command","command":"bash \"$CLAUDE_PROJECT_DIR/.claude/hooks/Stop-copia-moment.sh\""}] }
                ],
                "UserPromptSubmit": [
                    { "hooks": [{"type":"command","command":"/usr/local/bin/my-personal-hook.sh"}] }
                ]
            }
        });

        reap_orphan_hook_registrations(&mut settings, &hooks_dir);

        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "orphan Stop-copia-moment registration should be pruned");
        assert!(
            stop[0]["hooks"][0]["command"].as_str().unwrap().contains("Stop.sh"),
            "the live Stop.sh registration survives"
        );
        // Hand-authored entry untouched, its event array intact.
        assert!(
            settings["hooks"]["UserPromptSubmit"].as_array().unwrap().len() == 1,
            "hand-authored personal hook must never be reaped"
        );

        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn reaper_drops_emptied_event_arrays() {
        let tmp = std::env::temp_dir().join(format!("glx_reap_empty_{}", std::process::id()));
        let hooks_dir = tmp.join(".claude").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        // No .sh files on disk at all — every managed registration is an orphan.
        let mut settings = serde_json::json!({
            "hooks": {
                "PreCompact": [
                    { "hooks": [{"type":"command","command":"bash \"$CLAUDE_PROJECT_DIR/.claude/hooks/PreCompact.sh\""}] }
                ]
            }
        });
        reap_orphan_hook_registrations(&mut settings, &hooks_dir);
        assert!(
            settings["hooks"].as_object().unwrap().get("PreCompact").is_none(),
            "an event whose only registration was an orphan is removed entirely"
        );
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn reaper_noop_when_no_hooks_block() {
        let mut settings = serde_json::json!({"env": {"GIT_AUTHOR_NAME": "w4r3z"}});
        reap_orphan_hook_registrations(&mut settings, std::path::Path::new("/nonexistent"));
        assert!(settings.get("hooks").is_none(), "must not fabricate a hooks block");
    }

    /// autoMemoryDirectory derives from the repo root: `~`-shortened under
    /// $HOME (portable committed settings), absolute otherwise, and never a
    /// false-prefix mangle (`/Users/rob2` is NOT under `/Users/rob`).
    #[test]
    fn auto_memory_dir_value_forms() {
        use std::path::Path;
        assert_eq!(
            auto_memory_dir_value(Path::new("/Users/rob/repos/X"), Some("/Users/rob")),
            "~/repos/X/Harness/Memory"
        );
        assert_eq!(
            auto_memory_dir_value(Path::new("/srv/repos/X"), Some("/Users/rob")),
            "/srv/repos/X/Harness/Memory"
        );
        assert_eq!(
            auto_memory_dir_value(Path::new("/Users/rob2/repos/X"), Some("/Users/rob")),
            "/Users/rob2/repos/X/Harness/Memory"
        );
        assert_eq!(
            auto_memory_dir_value(Path::new("/Users/rob/repos/X"), None),
            "/Users/rob/repos/X/Harness/Memory"
        );
    }
}

#[cfg(test)]
mod prune_tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn unique_tmp_root(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        std::env::temp_dir().join(format!("gitlex-prune-{tag}-{}-{nanos}", std::process::id()))
    }

    fn git(root: &Path, args: &[&str]) {
        let st = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .expect("git runs");
        assert!(st.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&st.stderr));
    }

    fn git_repo(tag: &str) -> PathBuf {
        let root = unique_tmp_root(tag);
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "test@test"]);
        git(&root, &["config", "user.name", "test"]);
        // The vendored base kit dir must exist or the prune (correctly)
        // refuses to run — it can't know what a missing kit ships.
        fs::create_dir_all(root.join(".lex/kit/repolex-ai/git-lex-kit-base")).unwrap();
        root
    }

    /// The full four-way judgment on one tree: source-backed survives,
    /// kit-shipped survives, tracked orphan is pruned (recoverable),
    /// never-committed orphan is refused (unrecoverable), and a dir
    /// without SKILL.md is not a skill and is never touched.
    #[test]
    fn skill_prune_is_source_or_kit_backed_and_tracked_only() {
        let root = git_repo("skills");
        fs::create_dir_all(root.join("Soul/Skill")).unwrap();
        fs::write(root.join("Soul/Skill/alive.md"), "---\nsoul.Skill.skillDescription: x\n---\nbody\n").unwrap();
        for name in ["alive", "orphan-tracked", "kitskill"] {
            fs::create_dir_all(root.join(".claude/skills").join(name)).unwrap();
            fs::write(root.join(".claude/skills").join(name).join("SKILL.md"), "old\n").unwrap();
        }
        fs::create_dir_all(root.join(".lex/kit/repolex-ai/git-lex-kit-base/harness/.claude/skills/kitskill")).unwrap();
        fs::write(root.join(".lex/kit/repolex-ai/git-lex-kit-base/harness/.claude/skills/kitskill/SKILL.md"), "kit\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "fixture"]);
        // After the commit: a deployed-only skill that was never committed,
        // and an asset dir that is not skill-shaped.
        fs::create_dir_all(root.join(".claude/skills/orphan-untracked")).unwrap();
        fs::write(root.join(".claude/skills/orphan-untracked/SKILL.md"), "mine\n").unwrap();
        fs::create_dir_all(root.join(".claude/skills/notes")).unwrap();
        fs::write(root.join(".claude/skills/notes/scratch.txt"), "not a skill\n").unwrap();

        sync_skills(&root);

        assert!(root.join(".claude/skills/alive/SKILL.md").exists(), "source-backed survives");
        assert!(!root.join(".claude/skills/orphan-tracked").exists(), "tracked orphan pruned");
        assert!(root.join(".claude/skills/orphan-untracked/SKILL.md").exists(), "never-committed refused");
        assert!(root.join(".claude/skills/kitskill/SKILL.md").exists(), "kit-shipped survives");
        assert!(root.join(".claude/skills/notes/scratch.txt").exists(), "non-skill dir untouched");
        fs::remove_dir_all(&root).ok();
    }

    /// The arming rule: no Skill/ source dir → no prune, ever. A repo whose
    /// skills live only in .claude/skills/ (the pre-Skill-class era, e.g.
    /// W4R3Z's own) has no source of truth to converge to.
    #[test]
    fn no_source_dir_means_no_prune() {
        let root = git_repo("noskilldir");
        fs::create_dir_all(root.join(".claude/skills/deployed-only")).unwrap();
        fs::write(root.join(".claude/skills/deployed-only/SKILL.md"), "keep me\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "fixture"]);

        sync_skills(&root);

        assert!(root.join(".claude/skills/deployed-only/SKILL.md").exists());
        fs::remove_dir_all(&root).ok();
    }

    /// The agents lane makes the same four-way judgment on .md files.
    #[test]
    fn agent_prune_mirrors_the_skill_lane() {
        let root = git_repo("agents");
        fs::create_dir_all(root.join("Soul/Subagent")).unwrap();
        fs::write(root.join("Soul/Subagent/keep.md"), "---\nsoul.Subagent.subagentDescription: x\n---\nbody\n").unwrap();
        fs::create_dir_all(root.join(".claude/agents")).unwrap();
        for name in ["keep.md", "orphan.md", "kitagent.md"] {
            fs::write(root.join(".claude/agents").join(name), "old\n").unwrap();
        }
        fs::create_dir_all(root.join(".lex/kit/repolex-ai/git-lex-kit-base/harness/.claude/agents")).unwrap();
        fs::write(root.join(".lex/kit/repolex-ai/git-lex-kit-base/harness/.claude/agents/kitagent.md"), "kit\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "fixture"]);
        fs::write(root.join(".claude/agents/stray.md"), "mine\n").unwrap();

        sync_subagents(&root);

        assert!(root.join(".claude/agents/keep.md").exists(), "source-backed survives");
        assert!(!root.join(".claude/agents/orphan.md").exists(), "tracked orphan pruned");
        assert!(root.join(".claude/agents/stray.md").exists(), "never-committed refused");
        assert!(root.join(".claude/agents/kitagent.md").exists(), "kit-shipped survives");
        fs::remove_dir_all(&root).ok();
    }
}

#[cfg(test)]
mod local_settings_warning_tests {
    use super::*;

    /// The false-positive Selkie hit (2026-08-22): a healthy local settings
    /// file — permissions, hook opt-outs, a non-git env var, even a GIT_*
    /// block that AGREES with the committed identity — must never warn. A
    /// warning seen fifty times on a healthy repo teaches people to ignore
    /// warnings; only a value that would change who signs a save fires.
    #[test]
    fn healthy_local_settings_do_not_warn() {
        let quiet = |txt: &str| local_settings_identity_conflicts(txt, "selkie", "selkie@repolex.ai");
        assert!(quiet(r#"{"permissions":{"allow":["Bash"]}}"#).is_empty());
        assert!(quiet(r#"{"soul.disabledHooks":["x"],"env":{"MY_VAR":"1"}}"#).is_empty());
        assert!(quiet("not json at all").is_empty());
        assert!(quiet(r#"{}"#).is_empty());
        // Agreeing override: redundant, changes nothing, stays silent.
        assert!(quiet(
            r#"{"env":{"GIT_AUTHOR_NAME":"selkie","GIT_AUTHOR_EMAIL":"selkie@repolex.ai"}}"#
        ).is_empty());
    }

    #[test]
    fn conflicting_override_names_local_and_committed_values() {
        let conflicts = local_settings_identity_conflicts(
            r#"{"env":{"GIT_AUTHOR_NAME":"old-me","GIT_AUTHOR_EMAIL":"selkie@repolex.ai"}}"#,
            "selkie",
            "selkie@repolex.ai",
        );
        assert_eq!(conflicts, vec![r#"GIT_AUTHOR_NAME="old-me" (committed: "selkie")"#]);
    }
}
