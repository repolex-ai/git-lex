//! gitlexd — the git-lex query and sync service.
//!
//! One gitlexd per machine. It holds every registered soul's store
//! (`~/.lex/repos.json`), keys each soul by its genesis (first-commit) hash,
//! runs the sync when a soul's HEAD moves, and answers SPARQL over HTTP on
//! localhost. Spec: subtexture/docs/git-lex/2026_09_22_LEXD_SPEC.md
//! (goodlux, 2026-09-22).
//!
//! `daemon` is the process state and the sync loop, `http` the endpoint,
//! `client` what the git-lex command line uses to reach a running gitlexd.

pub mod client;
pub mod daemon;
pub mod http;
pub mod session;

use std::path::PathBuf;

/// The port gitlexd listens on. localhost only. Kept at the port
/// `git lex serve sparql` used, pending a fleet-wide port plan
/// (goodlux, 2026-09-22).
pub const PORT: u16 = 7880;

pub fn base_url() -> String {
    format!("http://127.0.0.1:{PORT}")
}

/// `~/.lex` — the machine-level git-lex directory the registry lives in.
pub fn lex_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(|h| PathBuf::from(h).join(".lex"))
}

/// `~/.lex/logs/gitlexd.log`, appended across starts.
pub fn log_path() -> Option<PathBuf> {
    lex_home().map(|h| h.join("logs").join("gitlexd.log"))
}
