//! Harness adapters — push soul documents into substrate-specific locations.
//!
//! Soul directories (Skill/, Subagent/, …) are the source of truth; harness
//! targets (.claude/skills/, .claude/agents/, …) are derived artifacts that
//! get overwritten on every sync.
//!
//! ## Substrate selection
//!
//! The active substrate list is, in order of precedence:
//!
//! 1. Explicit `substrates:` list in `.lex/repo.yml` (declarative override)
//! 2. Auto-detection from on-disk markers (.claude/, .hermes/, .gemini/)
//! 3. Back-compat fallback: `[Claude]` if nothing detected
//!
//! Multiple substrates can be active in the same repo — the adapter runs
//! each in turn. This is how a soul gets to be usable from both Claude Code
//! and Hermes against the same Skill/ source files.

pub mod claude;
pub mod gemini;

use std::path::Path;

use crate::kit::read_repo_yml_substrates;

/// A specific agent harness git-lex knows how to write to.
///
/// To add a new substrate: add the variant here, implement a new module
/// (e.g. `harness::hermes`), wire it into `Substrate::sync` and
/// `Substrate::auto_detect`, and add the on-disk marker check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Substrate {
    Claude,
    Hermes,
    Gemini,
}

impl Substrate {
    /// Parse a substrate name from `repo.yml`. Returns None on unknown
    /// (caller decides whether to warn or skip silently).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "claude" | "claude-code" => Some(Substrate::Claude),
            "hermes" => Some(Substrate::Hermes),
            "gemini" | "antigravity" => Some(Substrate::Gemini),
            _ => None,
        }
    }

    /// On-disk presence check. Substrate-specific files/dirs at the repo
    /// root that signal this substrate is in use in this repo. Used by
    /// `auto_detect`. Not by itself authoritative — `repo.yml`'s
    /// `substrates:` list takes precedence.
    fn is_present_on_disk(&self, root: &Path) -> bool {
        match self {
            Substrate::Claude => {
                root.join(".claude").is_dir()
                    || root.join(".claude").join("settings.json").is_file()
            }
            Substrate::Hermes => {
                root.join(".hermes").is_dir()
                    || root.join("hermes-config.yaml").is_file()
                    || root.join("hermes-config.yml").is_file()
            }
            Substrate::Gemini => {
                root.join(".agents").is_dir()
                    || root.join(".gemini").is_dir()
                    || root.join(".antigravity").is_dir()
            }
        }
    }

    /// Run this substrate's sync adapter. Soul → harness translation.
    pub fn sync(&self, root: &Path) {
        match self {
            Substrate::Claude => claude::sync_all(root),
            Substrate::Hermes => {
                eprintln!("harness: hermes adapter not yet implemented (Soul/Skill, Soul/Subagent untouched on the hermes side)");
            }
            Substrate::Gemini => gemini::sync_all(root),
        }
    }

    /// Every substrate variant. Used for auto-detect iteration. If you add
    /// a variant above, add it here too.
    fn all() -> &'static [Substrate] {
        &[Substrate::Claude, Substrate::Hermes, Substrate::Gemini]
    }
}

/// Detect substrates from on-disk markers. Returns every substrate that
/// has its signature files at the repo root. Order is canonical
/// (Claude/Hermes/Gemini) for stable iteration.
pub fn auto_detect(root: &Path) -> Vec<Substrate> {
    Substrate::all()
        .iter()
        .copied()
        .filter(|s| s.is_present_on_disk(root))
        .collect()
}

/// Resolve the active substrate list for a repo:
///
/// 1. If `.lex/repo.yml` has a non-empty `substrates:` list, use it.
/// 2. Otherwise auto-detect from on-disk markers.
/// 3. If detection finds nothing, fall back to `[Claude]` for back-compat
///    (every git-lex repo prior to this commit was Claude-only).
///
/// Unknown substrate names in `repo.yml` are warned-on-stderr and skipped.
pub fn active_substrates(root: &Path) -> Vec<Substrate> {
    let repo_yml = root.join(".lex").join("repo.yml");
    let declared = read_repo_yml_substrates(&repo_yml);
    if !declared.is_empty() {
        let mut out = Vec::new();
        for name in &declared {
            match Substrate::parse(name) {
                Some(s) if !out.contains(&s) => out.push(s),
                Some(_) => { /* dedup */ }
                None => {
                    eprintln!(
                        "warning: unknown substrate '{}' in .lex/repo.yml substrates: — skipping",
                        name
                    );
                }
            }
        }
        if !out.is_empty() {
            return out;
        }
    }

    let detected = auto_detect(root);
    if !detected.is_empty() {
        return detected;
    }
    // Back-compat: every pre-multi-substrate repo was Claude.
    vec![Substrate::Claude]
}

/// Run every active substrate's sync adapter. Called from `git lex save`
/// and `git lex init`. Replaces the old hardcoded `sync(&root, "claude")`.
pub fn sync_all(root: &Path) {
    for substrate in active_substrates(root) {
        substrate.sync(root);
    }
}

/// The ONE substrate-setup pass (review #11): resolve the agent name and
/// run every active substrate's identity/hook injection — the pass that
/// registers hooks and writes the identity env block into settings.json,
/// i.e. the thing that makes hooks FIRE. The gate used to be triplicated
/// across init, kit-add, and kit-update; the #67 loud-skip fix landed in
/// two of the three copies while init's kept skipping in total silence —
/// exactly how triplication bites. `agent_name`: Some(name) when the
/// caller just collected it (init — repo.yml may not carry the line yet);
/// None reads .lex/repo.yml.
pub fn run_substrate_setup(root: &Path, agent_name: Option<&str>) {
    let name = match agent_name {
        Some(n) => n.trim().to_string(),
        None => git_lex::RepoYml::load(root)
            .agent_name
            .unwrap_or_default()
            .trim()
            .to_string(),
    };
    if name.is_empty() {
        eprintln!(
            "warning: no `agent_name:` in .lex/repo.yml — SKIPPED substrate setup \
             (settings.json hooks + identity env were NOT written/reconciled).\n\
             Your hooks will not fire and kit hook changes will not converge until \
             this is fixed. Add a line to .lex/repo.yml:\n\
             \x20   agent_name: <your-name>\n\
             then re-run `git lex kit-update`."
        );
        return;
    }
    for substrate in active_substrates(root) {
        match substrate {
            Substrate::Claude => claude::setup_substrate_claude(root, &name),
            Substrate::Gemini => gemini::setup_substrate_gemini(root, &name),
            Substrate::Hermes => {
                // Per-substrate identity injection not yet implemented.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_tmp_root() -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        // Mix in a coarse epoch-second from SystemTime so concurrent test
        // processes don't collide across runs.
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let dir = std::env::temp_dir()
            .join(format!("git-lex-substrate-test-{}-{}-{}", pid, secs, n));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn substrate_parse_accepts_canonical_and_aliases() {
        assert_eq!(Substrate::parse("claude"), Some(Substrate::Claude));
        assert_eq!(Substrate::parse("Claude"), Some(Substrate::Claude));
        assert_eq!(Substrate::parse("claude-code"), Some(Substrate::Claude));
        assert_eq!(Substrate::parse("hermes"), Some(Substrate::Hermes));
        assert_eq!(Substrate::parse("gemini"), Some(Substrate::Gemini));
        assert_eq!(Substrate::parse("antigravity"), Some(Substrate::Gemini));
        assert_eq!(Substrate::parse("nonsense"), None);
    }

    #[test]
    fn auto_detect_empty_root_returns_nothing() {
        let root = unique_tmp_root();
        assert!(auto_detect(&root).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn auto_detect_finds_claude_via_dir() {
        let root = unique_tmp_root();
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        assert_eq!(auto_detect(&root), vec![Substrate::Claude]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn auto_detect_finds_hermes_via_config_file() {
        let root = unique_tmp_root();
        std::fs::write(root.join("hermes-config.yaml"), "").unwrap();
        assert_eq!(auto_detect(&root), vec![Substrate::Hermes]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn auto_detect_finds_multiple_in_canonical_order() {
        let root = unique_tmp_root();
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::create_dir_all(root.join(".hermes")).unwrap();
        std::fs::create_dir_all(root.join(".gemini")).unwrap();
        assert_eq!(
            auto_detect(&root),
            vec![Substrate::Claude, Substrate::Hermes, Substrate::Gemini]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn active_substrates_falls_back_to_claude_when_nothing_present() {
        let root = unique_tmp_root();
        // No .lex/repo.yml, no on-disk markers. Should fall back to Claude
        // for back-compat with every pre-multi-substrate repo.
        assert_eq!(active_substrates(&root), vec![Substrate::Claude]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn active_substrates_uses_repo_yml_when_present() {
        let root = unique_tmp_root();
        // Detection would only find Claude (.claude/ exists), but repo.yml
        // says hermes — the explicit declaration should win.
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::create_dir_all(root.join(".lex")).unwrap();
        std::fs::write(
            root.join(".lex").join("repo.yml"),
            "name: TEST\nsubstrates:\n  - hermes\n",
        ).unwrap();
        assert_eq!(active_substrates(&root), vec![Substrate::Hermes]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn active_substrates_skips_unknown_names() {
        let root = unique_tmp_root();
        std::fs::create_dir_all(root.join(".lex")).unwrap();
        std::fs::write(
            root.join(".lex").join("repo.yml"),
            "name: TEST\nsubstrates:\n  - nonsense\n  - claude\n",
        ).unwrap();
        // The "nonsense" line is warned-and-skipped; claude survives.
        assert_eq!(active_substrates(&root), vec![Substrate::Claude]);
        std::fs::remove_dir_all(&root).ok();
    }
}
