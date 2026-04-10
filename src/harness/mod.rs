//! Harness adapters — push soul documents into substrate-specific locations.
//! Soul directories (Skill/, Subagent/) are the source of truth; harness
//! targets (.claude/skills/, .claude/agents/, etc.) are derived artifacts
//! that get overwritten on every sync.

pub mod claude;

use std::path::Path;

/// Sync soul documents into the active substrate's harness directories.
/// Called from `git lex save` and `git lex init`.
pub fn sync(root: &Path, substrate: &str) {
    match substrate {
        "claude" => claude::sync_all(root),
        "gemini" => eprintln!("harness: gemini adapter not yet implemented"),
        "openai" => eprintln!("harness: openai adapter not yet implemented"),
        _ => {}
    }
}
