//! Harness adapters — push skills and configuration into substrate-specific
//! locations. The Skill/ directory is the source of truth; harness targets
//! (.claude/skills/, etc.) are derived artifacts that get overwritten on sync.

pub mod claude;

use std::path::Path;

/// Sync skills from the repo's Skill/ directory into the active substrate's
/// harness directory. Called from `git lex save` and `git lex init`.
pub fn sync_skills(root: &Path, substrate: &str) {
    match substrate {
        "claude" => claude::sync_skills(root),
        "gemini" => eprintln!("harness: gemini adapter not yet implemented"),
        "openai" => eprintln!("harness: openai adapter not yet implemented"),
        _ => {}
    }
}
