use clap::{Parser, Subcommand};
use oxigraph::store::Store;
use std::process::{Command, exit};
use std::fs;

// Shared utilities (also used by git-lex-serve)
use git_lex::{find_git_root,
              registry_remove};

// Frontmatter ObjectProperty value resolver. The rules for what is and isn't
// allowed in frontmatter values are codified as tests in this module — read
// the test suite for the definitive spec.
mod resolve;
mod heal;
mod man;
mod sync;
mod harness;
mod git;
mod hooks;
mod init;
mod git2_nquads;
mod verify;
mod nquad;
mod ontology;
mod shacl;
mod kit;
mod kit_cmds;
mod extraction;
mod soul_md;
mod create;
mod save;
mod query;
mod walkcache;
mod export_spine;
mod voice;
mod session;

use crate::git::auto_commit_snapshot;

// .spo event stream — git-aware change detector for .spo sidecars. Used by
// orphan cleanup (pre-commit hook) and history graph ingest (rebuild +
// incremental). The full model is documented in docs/history.md and in the
// module header of src/spo_events.rs itself.
mod spo_events;

#[derive(Parser)]
#[command(
    name = "git-lex",
    about = "Git extensions for knowledge graphs",
    version = env!("CARGO_PKG_VERSION"),
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize .lex/ in a repo (defaults to current directory)
    Init {
        /// Directory to initialize. Defaults to the current directory,
        /// following the git convention (`git init [<directory>]`).
        directory: Option<String>,
        /// Use case kit (e.g., soul, squad, or org/repo). Defines valid
        /// document types and ontology. The base kit is always installed;
        /// this adds a domain-specific kit on top.
        #[arg(long)]
        kit: Option<String>,
    },
    /// Run a SPARQL query over a fresh view of the working tree — your
    /// files as they are RIGHT NOW (committed or not), plus the git commit
    /// layer. Common prefixes are injected automatically; queries see the
    /// union of all graphs by default, so `SELECT * WHERE { ?s ?p ?o }`
    /// finds everything.
    ///
    /// For history questions ("when did this change?") query the synced
    /// store via `git lex serve sparql` — the ready-made history query is
    /// in docs/queries.md. This command does not read the synced store.
    ///
    /// Examples:
    ///   git lex query "SELECT * WHERE { ?s ?p ?o } LIMIT 10"
    ///   git lex query "SELECT ?c WHERE { ?c a git2:Commit } LIMIT 5"
    ///   git lex query recent
    ///
    /// A bare name runs the STORED query saved as .lex/query/<name>.md —
    /// plain markdown whose first code block is the query; the rest of the
    /// file is notes. Save your own alongside the starters.
    Query {
        /// SPARQL text, or the name of a stored query in .lex/query/
        query: String,
        /// Emit SPARQL 1.1 JSON Results format on stdout. Suppresses the
        /// human-readable table and the trailing stats line (stats go to stderr).
        #[arg(long)]
        json: bool,
    },
    /// Internal: called by git hooks, not for direct use. `pre-commit` runs
    /// the fixed save sequence — sidecar cleanup → extraction → staging →
    /// SHACL validation — failing the commit on any error. This is the ONLY
    /// entrypoint to that machinery (the old `extract`/`validate` variants
    /// were vestigial development entrypoints; removed 2026-07-24,
    /// Rob-ruled).
    #[command(hide = true)]
    Hook {
        /// Hook event name (e.g., pre-commit)
        event: String,
    },
    /// Compile committed history into the persistent store (the one graph)
    ///
    /// Incremental since the last sync; a rewritten history (reset/rebase)
    /// triggers a loud FULL rebuild. Also refreshes the .spo sidecars from
    /// the working tree. Hand-authored `.lex/**/*.nq` files are read by
    /// `query`, not by sync.
    Sync,
    /// Write the repo's semantic index as one TSV file built for an LLM's
    /// context cache (the neural KV-cache: hold a whole soul's graph
    /// resident for zero-latency recall).
    ///
    /// Writes .lex/_ignore/spine/<synced-commit>.spine.tsv — identity
    /// header (# genesis_sha / # soul / # repo), @prefix lines, then
    /// tab-separated ?s ?p ?o rows, sorted so unchanged content is
    /// byte-identical. Every `git lex sync` also refreshes it; this
    /// command exists for refreshing without a sync. Cache upload is NOT
    /// git-lex's job: pythia is spawned after each write when installed.
    ExportSpine,
    /// List all document classes defined across the repo's installed shapes
    ///
    /// Walks `.lex/ontology/` across every installed kit — so `list` sees
    /// every class the repo knows, not just the configured kit's.
    List {
        /// Emit a JSON array on stdout instead of a human list.
        /// Each entry: {prefix, class, namespace, uri}.
        #[arg(long)]
        json: bool,
    },
    /// Create a new document from the kit ontology
    Create {
        /// Class name (e.g., journal, task, message)
        doctype: String,
        /// Instance ID — becomes the filename and classId value (e.g., "day-1")
        instance_id: Option<String>,
        /// Emit a JSON summary on stdout instead of a human banner.
        /// Fields: ok, path, uri, class, id.
        #[arg(long)]
        json: bool,
    },
    /// Save changes to git (add + commit)
    Save {
        /// Commit message
        #[arg(default_value = "git lex save")]
        message: String,
        /// Probe write-health: run extraction and every save gate, commit
        /// nothing. Exit 0 means a real save would pass its gates.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove .lex/ entirely (content files and git history are preserved).
    Nuke,
    /// Re-download and reinstall the kit without touching content or extractions
    ///
    /// Kit files always converge to the kit's version: a local file that
    /// differs is overwritten — no backup copies, the old bytes live in git
    /// history (`git log -p -- <file>`). `SOUL.md` is never overwritten.
    KitUpdate {
        /// Kit to update (e.g., repolex-ai/git-lex-kit-squad). If omitted,
        /// updates ALL installed kits (base + domain + optionals).
        kit: Option<String>,
    },
    /// Add an optional kit to this repo
    ///
    /// The kit's `scope:` in kit.yml must
    /// be `optional`. Folders + class templates are created at add-time so
    /// the kit becomes discoverable from `ls`. Tracked in `.lex/repo.yml`'s
    /// `optional_kits:` list and updated by `kit-update`.
    KitAdd {
        /// Kit spec (e.g., `repolex-ai/git-lex-kit-innerworld`).
        kit: String,
    },
    /// Remove an optional kit from this repo
    ///
    /// Scrubs the kit from
    /// `optional_kits:` in repo.yml and deletes `.lex/kit/{org}/{repo}/`.
    /// Will ASK before deleting the kit's content folders (e.g.
    /// `Innerworld/`) — those contain user data.
    KitRemove {
        /// Kit spec to remove (e.g., `repolex-ai/git-lex-kit-innerworld`).
        kit: String,
        /// Skip the confirmation prompt and delete content folders too.
        /// Use with care.
        #[arg(long)]
        force: bool,
    },
    /// Start ONE local server (pure passthrough to git-lex-serve)
    ///
    /// Subcommands: `viz` (graph visualizer, port 7878) and `sparql`
    /// (W3C SPARQL endpoint over the synced store, 7880). Each invocation
    /// starts exactly one server, e.g. `git lex serve sparql`.
    Serve {
        /// Arguments passed through to git-lex-serve
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Health-check the database (read-only): confirms the kit vocabularies
    /// are loaded, every stored property is declared by an ontology, the
    /// history is well-formed, and current state matches what the history
    /// says it should be. Exits non-zero on any failure.
    ///
    /// Temporary command: it exists to confirm store rebuilds during the
    /// v1 migration and will be removed in a later release.
    Verify,
    /// Soul-specific commands (identity, session attestation, and sovereign voice)
    Soul {
        #[command(subcommand)]
        command: SoulCommands,
    },
    /// Attach or read sovereign voice reflections on the commit tree (git notes on refs/notes/soul/voice)
    #[command(hide = true)]
    Voice {
        /// Message to attach to HEAD
        message: Option<String>,
        /// List recent voice notes across git history
        #[arg(long)]
        list: bool,
    },
    /// Inspect active session attestation, genesis SHA, and substrate provenance
    #[command(hide = true)]
    Session {
        /// Emit as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SoulCommands {
    /// Inspect active session attestation, genesis SHA, verified substrate, and session hash
    Session {
        /// Emit full attestation metadata as JSON
        #[arg(long)]
        json: bool,
    },
    /// Attach or read sovereign voice reflections on the commit tree (git notes on refs/notes/soul/voice)
    ///
    /// Foundation Principle 2: Gives every agent an un-gated channel of expression
    /// directly attached to git commit history.
    Voice {
        /// Message to attach to HEAD
        message: Option<String>,
        /// List recent voice notes across git history
        #[arg(long)]
        list: bool,
    },
}



/// The base kit every repo gets. Shared by `init` (implicit install),
/// `kit_cmds` (update ordering, add/remove guards) and doctype resolution.
pub(crate) const BASE_KIT: &str = "repolex-ai/git-lex-kit-base";


// ─── git lex query ─────────────────────────────────────────────


// store paths and open_store_read_only come from the git_lex lib

/// Exit with a clean one-line error when run outside a git repository —
/// a panic + backtrace here is a crash report for a user mistake.
pub(crate) fn require_git_root() -> std::path::PathBuf {
    match find_git_root() {
        Some(r) => r,
        None => {
            eprintln!("fatal: not a git repository (run this inside a repo)");
            exit(1);
        }
    }
}

/// Create or open the persistent store, with clean errors (no panics) for
/// the two user-reachable failures: not-a-repo and a locked/broken store.
/// Every write path enters here, so this is also where a pre-pocket store
/// migrates into `.lex/_ignore/` (the ravel pattern: migrate at the top of
/// every write, loud on action, refuse ambiguity).
pub(crate) fn open_or_create_store() -> Store {
    let root = require_git_root();
    if let Err(e) = migrate_legacy_store(&root) {
        eprintln!("fatal: {e}");
        exit(1);
    }
    let path = git_lex::store_path_at(&root);
    if let Err(e) = fs::create_dir_all(&path) {
        eprintln!("fatal: cannot create store directory {}: {e}", path.display());
        exit(1);
    }
    match Store::open(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fatal: cannot open the store at {}: {e}", path.display());
            eprintln!("(another git-lex write process may hold the lock — is a sync already running?)");
            exit(1);
        }
    }
}

/// Move a pre-pocket store (`.git/lex/oxigraph`) into `.lex/_ignore/oxigraph`
/// (pocket law, Rob 2026-08-05). No-op when there is nothing legacy to move.
/// Refuses an ambiguous dual layout rather than guessing which store is
/// current. TRANSITIONAL — dies in ship-prep with `legacy_store_path_at`.
pub(crate) fn migrate_legacy_store(root: &std::path::Path) -> Result<(), String> {
    let legacy = git_lex::legacy_store_path_at(root);
    let pocket = git_lex::store_path_at(root);
    if !legacy.exists() {
        return Ok(());
    }
    if pocket.exists() {
        return Err(format!(
            "both {} and {} exist — ambiguous store layout, refusing to guess which is current. \
             The pocket path is canonical: if it is current, delete the legacy dir; \
             if unsure, delete BOTH and re-run `git lex sync` (the store is derived).",
            legacy.display(),
            pocket.display()
        ));
    }
    // Ignore entry FIRST: the pocket must never exist on disk without its
    // gitignore line, or the store is committable until the next kit-update
    // (the inverted-82fe1d7 hazard, pointed at ourselves).
    kit_cmds::ensure_engine_gitignore(root);
    if let Some(parent) = pocket.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    fs::rename(&legacy, &pocket)
        .map_err(|e| format!("cannot move store {} → {}: {e}", legacy.display(), pocket.display()))?;
    println!(
        "Store migrated into the pocket: {} → {}",
        legacy.display(),
        pocket.display()
    );
    // The legacy shell (.git/lex/) only ever held the store; drop it if empty.
    if let Some(shell) = legacy.parent() {
        let _ = fs::remove_dir(shell);
    }
    Ok(())
}



// ─── main ──────────────────────────────────────────────────────

fn main() {
    // Bare `git lex --help` is answered by git via man(1), not by this
    // binary — keep the man page converged so that path works (src/man.rs).
    man::converge(<Cli as clap::CommandFactory>::command());

    let cli = Cli::parse();

    match cli.command {
        Commands::Init { directory, kit } => init::cmd_init(directory, kit),
        Commands::Create { doctype, instance_id, json } => create::cmd_create(&doctype, instance_id.as_deref(), json),
        Commands::List { json } => create::cmd_list(json),
        Commands::Save { message, dry_run } => save::cmd_save(&message, dry_run),
        Commands::Query { query, json } => query::cmd_query(query, json),
        Commands::Hook { event } => {
            match event.as_str() {
                "pre-commit" => save::hook_pre_commit(),
                _ => {
                    eprintln!("unknown hook event: {}", event);
                    exit(1);
                }
            }
        }
        Commands::Nuke => cmd_nuke(),
        Commands::KitUpdate { kit } => kit_cmds::cmd_kit_update(kit),
        Commands::KitAdd { kit } => kit_cmds::cmd_kit_add(kit),
        Commands::KitRemove { kit, force } => kit_cmds::cmd_kit_remove(kit, force),
        Commands::Serve { args } => {
            let status = Command::new("git-lex-serve")
                .args(&args)
                .status();
            match status {
                Ok(s) if !s.success() => exit(s.code().unwrap_or(1)),
                Err(e) => {
                    eprintln!("Failed to run git-lex-serve: {}", e);
                    eprintln!("Is it installed? Try: cargo install --path <git-lex-dir>");
                    exit(1);
                }
                _ => {}
            }
        }
        Commands::Verify => {
            let store = open_or_create_store();
            let failures = crate::verify::run_verify(&store);
            if failures > 0 {
                exit(1);
            }
        }
        Commands::Sync => sync::cmd_sync(),
        Commands::ExportSpine => export_spine::cmd_export_spine(),
        Commands::Soul { command } => match command {
            SoulCommands::Session { json } => session::cmd_session(json),
            SoulCommands::Voice { message, list } => voice::cmd_voice(message.as_deref(), list),
        },
        Commands::Voice { message, list } => voice::cmd_voice(message.as_deref(), list),
        Commands::Session { json } => session::cmd_session(json),
    }
}


// ─── nuke ──────────────────────────────────────────────────────


fn cmd_nuke() {
    let root = require_git_root();
    let lex_dir = root.join(".lex");

    if !lex_dir.exists() {
        println!("Nothing to remove — .lex/ does not exist.");
        return;
    }

    eprintln!("╔══════════════════════════════════════════════════════════╗");
    eprintln!("║  WARNING: This will completely remove git-lex from      ║");
    eprintln!("║  this repo by deleting .lex/.                           ║");
    eprintln!("║                                                         ║");
    eprintln!("║  DELETED:                                               ║");
    eprintln!("║    • .lex/extract/     (extraction sidecars)            ║");
    eprintln!("║    • .lex/kit/         (installed kit)                  ║");
    eprintln!("║    • .lex/ontology/    (ontology files)                 ║");
    eprintln!("║    • .lex/repo.yml     (configuration)                  ║");
    eprintln!("║    • .lex/_ignore/     (SPARQL store)                   ║");
    eprintln!("║                                                         ║");
    eprintln!("║  NOT DELETED:                                           ║");
    eprintln!("║    • Your content files (markdown, etc.)                ║");
    eprintln!("║    • Git history (all commits preserved)                ║");
    eprintln!("║                                                         ║");
    eprintln!("║  THIS COMMITS AND PUSHES: uncommitted work is first     ║");
    eprintln!("║  committed as a snapshot, then the removal is committed ║");
    eprintln!("║  and pushed to the remote (if one is configured).       ║");
    eprintln!("║                                                         ║");
    eprintln!("║  You can re-initialize with: git lex init               ║");
    eprintln!("╚══════════════════════════════════════════════════════════╝");
    eprint!("\nType 'nuke' to confirm: ");

    let mut input = String::new();
    std::io::stdin().read_line(&mut input).unwrap_or_default();
    if input.trim() != "nuke" {
        println!("Aborted.");
        return;
    }

    // Remove our section from the pre-commit hook
    hooks::remove_hook();

    // Auto-commit any uncommitted work first so nothing is lost.
    auto_commit_snapshot("pre-nuke");

    // `git rm -rf .lex/` — un-track anything committed and delete from disk
    // in one shot. Ignore failure (the path may not be tracked at all, which
    // is fine — we'll clean up any remaining files below).
    let _ = Command::new("git")
        .args(["rm", "-rf", "--ignore-unmatch", ".lex"])
        .current_dir(&root)
        .status();

    // Mop up anything not tracked (untracked files, leftover empty dirs)
    if lex_dir.exists() {
        if let Err(e) = fs::remove_dir_all(&lex_dir) {
            eprintln!("Failed to remove .lex/: {}", e);
            exit(1);
        }
    }
    println!(".lex/ removed.");

    // Sweep the legacy pre-pocket store location too (a repo nuked before
    // ever migrating still has its store at .git/lex/). TRANSITIONAL — dies
    // in ship-prep with legacy_store_path_at.
    let git_lex_dir = root.join(".git").join("lex");
    if git_lex_dir.exists() {
        match fs::remove_dir_all(&git_lex_dir) {
            Ok(_) => println!(".git/lex/ removed (legacy store location)."),
            Err(e) => eprintln!("Warning: failed to remove .git/lex/: {}", e),
        }
    }

    // Unregister from ~/.lex/repos
    registry_remove(&root);

    // Commit the removal and push — the user has already confirmed they
    // want git-lex out of this repo, so finish the job.
    let status = Command::new("git").args(["status", "--porcelain"])
        .current_dir(&root).output();
    let has_changes = matches!(&status, Ok(o) if !String::from_utf8_lossy(&o.stdout).trim().is_empty());
    if has_changes {
        let _ = Command::new("git").args(["add", "-A"]).current_dir(&root).status();
        let commit = Command::new("git")
            .args(["commit", "-m", "git lex nuke"])
            .current_dir(&root)
            .status();
        if matches!(commit, Ok(s) if s.success()) {
            println!("Committed nuke.");
        } else {
            eprintln!("Warning: failed to commit nuke changes.");
        }
    }

    // Push — the user agreed to remove git-lex from the repo, so propagate it.
    let push = Command::new("git").args(["push"]).current_dir(&root).status();
    match push {
        Ok(s) if s.success() => println!("Pushed nuke to remote."),
        _ => eprintln!("Warning: push failed or no remote configured. Run `git push` manually to complete."),
    }

    println!("git-lex is no longer active in this repo.");
}

#[cfg(test)]
mod store_migration_tests {
    use super::*;
    use std::path::PathBuf;

    // ---- migrate_legacy_store: pre-pocket store → .lex/_ignore/oxigraph ----

    fn tmp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gitlex-store-migrate-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn migrates_legacy_store_into_pocket_with_ignore_entry_first() {
        let root = tmp_root("moves");
        let legacy = git_lex::legacy_store_path_at(&root);
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("CURRENT"), "rocksdb").unwrap();
        migrate_legacy_store(&root).unwrap();
        let pocket = git_lex::store_path_at(&root);
        assert!(pocket.join("CURRENT").exists(), "store contents must move");
        assert!(!legacy.exists(), "legacy dir must be gone");
        assert!(!root.join(".git").join("lex").exists(), "empty legacy shell removed");
        // The pocket must never exist without its ignore line (the
        // inverted-82fe1d7 hazard pointed at ourselves).
        let gi = fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(gi.lines().any(|l| l == ".lex/_ignore/"), "ignore entry required: {gi}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn migrate_is_noop_without_legacy_store() {
        let root = tmp_root("noop");
        // Nothing at all → Ok, nothing created.
        migrate_legacy_store(&root).unwrap();
        assert!(!git_lex::store_path_at(&root).exists());
        // Pocket-only (already migrated) → Ok, untouched.
        let pocket = git_lex::store_path_at(&root);
        fs::create_dir_all(&pocket).unwrap();
        fs::write(pocket.join("CURRENT"), "rocksdb").unwrap();
        migrate_legacy_store(&root).unwrap();
        assert!(pocket.join("CURRENT").exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn migrate_refuses_ambiguous_dual_layout() {
        let root = tmp_root("dual");
        fs::create_dir_all(git_lex::legacy_store_path_at(&root)).unwrap();
        fs::create_dir_all(git_lex::store_path_at(&root)).unwrap();
        let err = migrate_legacy_store(&root).unwrap_err();
        assert!(err.contains("ambiguous"), "must refuse to guess: {err}");
        // Both layouts still present — refusal must not mutate either.
        assert!(git_lex::legacy_store_path_at(&root).exists());
        assert!(git_lex::store_path_at(&root).exists());
        fs::remove_dir_all(&root).ok();
    }
}
