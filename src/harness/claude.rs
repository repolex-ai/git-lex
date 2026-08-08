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

    // Warn if a stale .claude/settings.local.json exists. Older versions
    // wrote identity to that file (gitignored), but souls are portable so
    // identity now lives in committed settings.json. Claude Code load order
    // is user → project → local, so a stale local file silently overrides
    // the new committed one. Don't auto-delete (user may have hand-edited
    // it) — just flag it loudly.
    let local_path = root.join(".claude").join("settings.local.json");
    if local_path.exists() {
        eprintln!();
        eprintln!("warning: .claude/settings.local.json still exists.");
        eprintln!("Identity now lives in committed settings.json. The local file");
        eprintln!("(gitignored) overrides settings.json in Claude Code load order,");
        eprintln!("so its env block (if any) will silently win. Review and delete");
        eprintln!("if you do not need it: rm .claude/settings.local.json");
    }
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
