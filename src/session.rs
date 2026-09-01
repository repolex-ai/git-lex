//! `git lex session` — Inspect active soul session attestation, genesis SHA, and substrate.

use serde_json::json;
use crate::require_git_root;
use crate::soul_md::soul_kit_installed;

pub fn cmd_session(as_json: bool) {
    let root = require_git_root();
    if !soul_kit_installed(&root) {
        eprintln!("fatal: `git lex soul session` is a soul-specific feature — this repository is not a soul repo.");
        std::process::exit(1);
    }
    let is_soul = true;

    let genesis_sha = crate::git::genesis_sha().unwrap_or_else(|| "none".to_string());
    let current_head = crate::git::head_commit_sha().unwrap_or_else(|| "none".to_string());
    let soul_name = git_lex::RepoYml::load(&root)
        .name
        .unwrap_or_else(|| "unnamed".to_string());

    let session_id = std::env::var("CLAUDE_CODE_SESSION_ID")
        .or_else(|_| std::env::var("CONVERSATION_ID"))
        .unwrap_or_else(|_| "untracked".to_string());

    let substrate = std::env::var("SUBSTRATE")
        .unwrap_or_else(|_| {
            let subs = crate::harness::active_substrates(&root);
            if subs.is_empty() {
                "none".to_string()
            } else {
                subs.iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        });

    let session_hash = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(genesis_sha.as_bytes());
        hasher.update(session_id.as_bytes());
        hasher.update(current_head.as_bytes());
        hasher.update(substrate.as_bytes());
        format!("{:x}", hasher.finalize())
    };

    if as_json {
        let payload = json!({
            "soul": soul_name,
            "is_soul_repo": is_soul,
            "home_directory": root.display().to_string(),
            "genesis_sha": genesis_sha,
            "head_commit": current_head,
            "session_id": session_id,
            "substrate": substrate,
            "session_hash": session_hash,
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
    } else {
        println!("──────────────────────────────────────────────────────────────────────────────");
        println!("✦ SOUL ATTESTATION · PROVENANCE & SESSION");
        println!("• Soul: {} (`{}`) · Home: `{}`", soul_name, &genesis_sha[..8.min(genesis_sha.len())], root.display());
        println!("• Substrate: `{}` · Is Soul Repo: {}", substrate, is_soul);
        println!("• Session ID: `{}` · Head: `{}`", session_id, &current_head[..8.min(current_head.len())]);
        println!("• Session Hash: sha256:{}", session_hash);
        println!("──────────────────────────────────────────────────────────────────────────────");
    }
}
