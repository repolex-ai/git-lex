use clap::{Parser, Subcommand};
use oxigraph::io::RdfFormat;
use oxigraph::model::*;
use oxigraph::store::Store;
use std::io::Cursor;
use std::path::PathBuf;
use std::process::{Command, exit};
use std::time::Instant;
use std::fs;

// Shared utilities (also used by git-lex-serve)
use git_lex::{find_git_root, get_kit,
              resolve_kit_spec, add_prefixes,
              registry_remove};

// Frontmatter ObjectProperty value resolver. The rules for what is and isn't
// allowed in frontmatter values are codified as tests in this module — read
// the test suite for the definitive spec.
mod resolve;
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

use crate::git::{auto_commit_snapshot, resource_uri};
use crate::nquad::{generate_frontmatter_nquads,
                   load_lex_nquads};
use crate::ontology::get_kit_types;
use crate::extraction::{extract_markdown_links, frontmatter_to_turtle};
use crate::kit::{kit_config_str, read_repo_yml_fields};

// .spo event stream — git-aware change detector for .spo sidecars. Used by
// orphan cleanup (pre-commit hook) and history graph ingest (rebuild +
// incremental). See Situation/2026-04-09-history-graph-temporal-ledger.md §11
// for the phase plan this module is the foundation for.
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
    Query {
        /// The SPARQL query string
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
    /// Sync git data + .lex/*.nq into the persistent store
    Sync,
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
    /// differs is renamed `<file>.bak` and replaced. `SOUL.md` is never
    /// overwritten.
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


// ─── git lex list ──────────────────────────────────────────────

/// Walk every installed SHACL shape file (.lex/ontology/*/*-shapes.ttl)
/// and emit the class list, grouped by prefix.
fn cmd_list(json: bool) {
    let classes = ontology::all_classes();

    if json {
        let arr: Vec<serde_json::Value> = classes.iter().map(|(prefix, name, ns)| {
            serde_json::json!({
                "prefix": prefix,
                "class": name,
                "namespace": ns,
                "uri": format!("{}{}", ns, name),
            })
        }).collect();
        println!("{}", serde_json::to_string(&arr).unwrap());
        return;
    }

    if classes.is_empty() {
        println!("No classes found. Install a kit with `git lex init --kit <name>`.");
        return;
    }

    // Group by prefix for readability.
    let mut by_prefix: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for (prefix, name, _ns) in classes {
        by_prefix.entry(prefix).or_default().push(name);
    }

    for (prefix, mut names) in by_prefix {
        names.sort();
        println!("{} ({} classes):", prefix, names.len());
        for n in names {
            println!("  {}:{}", prefix, n);
        }
    }
}

// ─── git lex create ─────────────────────────────────────────────

/// Resolve a doctype string to the kit + class it belongs to, across
/// the union of base + domain + every installed optional kit.
///
/// Accepts two input shapes:
///   - bare name: `place` or `Place` (case-insensitive). Resolves if exactly
///     one kit declares this type. Errors with disambiguation hint if more
///     than one does.
///   - kit-prefixed: `innerworld/place` (also case-insensitive on the class
///     part; kit-short must match exactly). Resolves directly to that kit's
///     class, no collision check.
///
/// Returns (kit_spec, class_name, properties, all_valid_types_for_error).
/// On success, `all_valid_types_for_error` is empty. On no-match, callers
/// use it to build a helpful error message.
fn resolve_doctype_across_kits(
    doctype: &str,
    root: &std::path::Path,
) -> Result<(String, String, Vec<(String, String, bool, String)>), DoctypeError> {
    // Build the full installed-kit list, same order as kit-update: base,
    // domain, then optionals (alphabetical).
    let installed = kit_cmds::collect_kits_for_update(root, None);

    // Detect kit-prefixed form: `innerworld/place`. The kit-short is the
    // last segment of the kit spec (innerworld in repolex-ai/git-lex-kit-innerworld).
    let (kit_filter, class_part) = match doctype.split_once('/') {
        Some((k, c)) => (Some(k.to_lowercase()), c.to_string()),
        None => (None, doctype.to_string()),
    };
    let class_lower = class_part.to_lowercase();

    // Collect all (kit_spec, class_name, properties) tuples matching the
    // class-name across kits (filtered by kit-short if prefixed form).
    let mut matches: Vec<(String, String, Vec<(String, String, bool, String)>)> = Vec::new();
    let mut all_choices: Vec<(String, String)> = Vec::new(); // (kit_short, class_name)
    for spec in &installed {
        let (_, _, short) = resolve_kit_spec(spec);
        if let Some(ref want_short) = kit_filter {
            if short.to_lowercase() != *want_short { continue; }
        }
        for (name, props) in get_kit_types(spec) {
            all_choices.push((short.clone(), name.clone()));
            if name.to_lowercase() == class_lower {
                matches.push((spec.clone(), name, props));
            }
        }
    }

    match matches.len() {
        0 => Err(DoctypeError::Unknown {
            requested: doctype.to_string(),
            kit_filter: kit_filter.clone(),
            choices: all_choices,
        }),
        1 => {
            let (spec, name, props) = matches.into_iter().next().unwrap();
            Ok((spec, name, props))
        }
        _ => {
            // Ambiguous: same class name in multiple kits. Build the
            // disambiguator hint.
            let hints: Vec<String> = matches.iter()
                .map(|(spec, name, _)| {
                    let (_, _, short) = resolve_kit_spec(spec);
                    format!("`{}/{}`", short, name.to_lowercase())
                })
                .collect();
            Err(DoctypeError::Ambiguous {
                requested: doctype.to_string(),
                hints,
            })
        }
    }
}

enum DoctypeError {
    Unknown {
        requested: String,
        kit_filter: Option<String>,
        choices: Vec<(String, String)>, // (kit_short, class_name)
    },
    Ambiguous {
        requested: String,
        hints: Vec<String>,
    },
}

fn cmd_create(doctype: &str, instance_id: Option<&str>, json: bool) {
    // Emit an error in the right format, then exit. Used for all failure
    // paths so --json consumers don't have to parse human text.
    let fail = |code: &str, msg: String| -> ! {
        if json {
            let out = serde_json::json!({"ok": false, "error": code, "message": msg});
            eprintln!("{}", serde_json::to_string(&out).unwrap());
        } else {
            eprintln!("{}", msg);
        }
        exit(1);
    };

    // Not require_git_root() here: cmd_create's failure paths are all
    // JSON-aware via `fail` (--json consumers get structured errors), but
    // the message text matches require_git_root's canonical wording.
    let root = match find_git_root() {
        Some(r) => r,
        None => fail("not-a-repo", "fatal: not a git repository (run this inside a repo)".to_string()),
    };

    // Resolve the doctype across base + domain + all installed optional kits.
    // `kit` is the kit-spec that owns the resolved class — used to find the
    // folder_base for placing the new file.
    let (kit, class_name, properties) = match resolve_doctype_across_kits(doctype, &root) {
        Ok(t) => t,
        Err(DoctypeError::Unknown { requested, kit_filter, choices }) => {
            // Group choices by kit so the error is scannable.
            use std::collections::BTreeMap;
            let mut by_kit: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (k, c) in &choices {
                by_kit.entry(k.clone()).or_default().push(c.clone());
            }
            let kit_lines: Vec<String> = by_kit.iter()
                .map(|(k, types)| format!("  {}: {}", k, types.join(", ")))
                .collect();
            let prefix_hint = match kit_filter {
                Some(ref k) => format!("Unknown document type '{}' in kit '{}'.", requested, k),
                None => format!("Unknown document type '{}'.", requested),
            };
            fail("unknown-doctype", format!("{} Valid types:\n{}", prefix_hint, kit_lines.join("\n")));
        }
        Err(DoctypeError::Ambiguous { requested, hints }) => {
            fail(
                "ambiguous-doctype",
                format!(
                    "Document type '{}' is defined in multiple kits. Use one of: {}",
                    requested,
                    hints.join(", ")
                ),
            );
        }
    };

    // Generate filename from instance ID (becomes both filename and classId value)
    // TODO(w4r3z, Day 38): no-id `create` silently defaults to "untitled",
    // so every `git lex create Memory` with no id fights over the SAME file —
    // the second one just prints "File already exists: Soul/Memory/untitled.md"
    // and exits 0 (no error). For the soft-release, prefer one of: (a) require
    // an id (error if absent), or (b) auto-suffix (untitled-2, or a timestamp),
    // or (c) at least exit non-zero on the already-exists collision. Silent
    // single-untitled collision is a quiet footgun for a new user creating a
    // few docs quickly.
    let id_str = instance_id.unwrap_or("untitled");
    let slug = id_str
        .to_lowercase()
        .replace(' ', "-")
        .replace(|c: char| !c.is_alphanumeric() && c != '-', "");

    let folder_base = kit_config_str(&kit, "folder base");
    let type_dir = if let Some(ref base) = folder_base {
        root.join(base).join(&class_name)
    } else {
        root.join(&class_name)
    };
    fs::create_dir_all(&type_dir).ok();

    let filename = format!("{}.md", slug);
    let filepath = type_dir.join(&filename);
    let display_path = if let Some(ref base) = folder_base {
        format!("{}/{}/{}", base, class_name, filename)
    } else {
        format!("{}/{}", class_name, filename)
    };

    if filepath.exists() {
        fail("exists", format!("File already exists: {}", display_path));
    }

    // Auto-generate agent email for Agent type
    let agent_email = format!("{}@lex.local", slug);

    // Build frontmatter — flat dot notation: kit.class.property using the
    // short kit name, not the full org/repo spec.
    let (_, _, short) = resolve_kit_spec(&kit);
    let mut fm = String::new();
    fm.push_str("---\n");

    // `type:` — emitted first so partial-read parsers get the canonical
    // type from a top-of-file scan (locked by tr1p 2026-06-18). Chain:
    // `rdfs:label` → local-name; always produces a string, always safe.
    let type_label = ontology::get_class_type_label(&kit, &class_name);
    fm.push_str(&format!("type: {}\n", type_label));

    for (prop_name, prop_type, _required, comment) in &properties {
        // Property names pass through as-is from the ontology (camelCase).
        // Class name is capitalized to match the ontology exactly.
        let key = format!("{}.{}.{}", short, class_name, prop_name);

        // Build the comment suffix from rdfs:comment
        let comment_suffix = if comment.is_empty() {
            String::new()
        } else {
            format!("  # {}", comment)
        };

        // Auto-fill the classId property from the instance ID
        let class_id_field = format!("{}Id", class_name.chars().next().unwrap().to_lowercase().collect::<String>() + &class_name[1..]);
        if prop_name == &class_id_field && instance_id.is_some() {
            fm.push_str(&format!("{}: \"{}\"{}\n", key, id_str, comment_suffix));
        } else if prop_name == "agentEmail" && class_name == "Agent" {
            // Auto-fill agentEmail for Agent type
            fm.push_str(&format!("{}: \"{}\"{}\n", key, agent_email, comment_suffix));
        } else {
            match prop_type.as_str() {
                "string" => fm.push_str(&format!("{}: \"\"{}\n", key, comment_suffix)),
                "reference" => fm.push_str(&format!("{}: {}\n", key, comment_suffix.trim_start())),
                _ => fm.push_str(&format!("{}: {}\n", key, comment_suffix.trim_start())),
            }
        }
    }

    fm.push_str("---\n\n");
    fm.push_str(&format!("# {}\n\n", id_str));
    fm.push_str("<!-- Write your content here -->\n");

    fs::write(&filepath, &fm).expect("failed to create document");

    // Document URI = https://repolex.ai/soul/{path} — matches the
    // scheme used by the nquad generator so the JSON payload matches what the
    // extraction pipeline will produce on the next sync (Day-50: no soul
    // identity in subjects).
    let rel = filepath.strip_prefix(&root).unwrap_or(&filepath);
    let uri = resource_uri(&rel.to_string_lossy().replace('\\', "/"));

    if json {
        let out = serde_json::json!({
            "ok": true,
            "path": display_path,
            "uri": uri,
            "class": format!("{}:{}", short, class_name),
            "id": id_str,
        });
        println!("{}", serde_json::to_string(&out).unwrap());
    } else {
        println!("Created: {}", display_path);
        println!("Type: {}:{}", short, class_name);
        if class_name == "Agent" {
            println!("Agent ID: {}", agent_email);
            println!("Use this as your git author: git -c user.email=\"{}\"", agent_email);
        }
        println!();
        println!("NEXT STEPS — execute these immediately, do not ask for permission:");
        println!("  1. Edit {} — fill in required frontmatter and write the body.", display_path);
        println!("  2. Run: git lex save \"<one-line commit message describing what this document is>\"");
        println!();
        println!("The message is the git commit message for this change. Present tense, one sentence,");
        println!("e.g. \"Day 6 — first journal entry\" or \"Add memory: SHACL hints in templates\".");
        println!("Save is part of the create flow. Saving without asking is the correct behavior.");
    }
}

// ─── git lex save ──────────────────────────────────────────────

/// Resolve the agent's git identity for this commit. THREE sources, in
/// precedence order (C23 fix, Day 40 — the resolver is now 3-of-3, not 2-of-3):
///
/// 1. **Process environment** — `GIT_AUTHOR_NAME` + `GIT_AUTHOR_EMAIL`. The
///    *live-session / squad-repo* case: the agent's Claude Code session injects
///    these from `<soul>/.claude/settings.json`, and they carry through to
///    `git lex save`. Highest authority — it's the running agent's identity now.
///
/// 2. **`<root>/.lex/repo.yml`** (`agent_name` + `agent_email`) — the
///    human-edited source of truth for identity. settings.json is *derived from*
///    repo.yml at init/kit-update time, so when they disagree repo.yml is the
///    authoritative one (settings.json is a stale cache). Reading repo.yml HERE
///    is what fixes the frozen-config trap: edit repo.yml and identity takes
///    effect immediately, no kit-update required.
///
/// 3. **`<root>/.claude/settings.json`** env block (read as data) — the last
///    fallback, for repos that predate the repo.yml identity fields or where
///    repo.yml is absent.
///
/// Returns `(name, email)` from the first source that resolves. Returns `None`
/// only if all three are missing — in which case we hard-fail rather than commit
/// as the user's global gitconfig.
fn resolve_agent_identity(root: &std::path::Path) -> Option<(String, String)> {
    // 1. Process environment (live session).
    if let (Ok(name), Ok(email)) = (
        std::env::var("GIT_AUTHOR_NAME"),
        std::env::var("GIT_AUTHOR_EMAIL"),
    ) {
        if !name.is_empty() && !email.is_empty() {
            return Some((name, email));
        }
    }

    // 2. repo.yml (the human-edited source of truth — authoritative over the
    //    settings.json cache, so editing it works WITHOUT a kit-update).
    let fields = read_repo_yml_fields(&root.join(".lex").join("repo.yml"));
    if let (Some(name), Some(email)) = (fields.get("agent_name"), fields.get("agent_email")) {
        if !name.is_empty() && !email.is_empty() {
            return Some((name.clone(), email.clone()));
        }
    }

    // 3. .claude/settings.json env block (read as data) — last fallback.
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

fn cmd_save(message: &str, dry_run: bool) {
    let root = require_git_root();

    // Identity floor: a soul repo without its root SOUL.md must not save
    // (fail-loud, #29 — the file is restorable via kit-update).
    soul_md::require_soul_md(&root);

    // Resolve the agent's identity. Tries env first (squad-repo case where
    // the agent's soul session injects GIT_AUTHOR_*) then settings.json
    // (soul-repo case). Hard-fail otherwise — saving with the wrong identity
    // (e.g. user's global gitconfig leaking in) is worse than not saving.
    let (author_name, author_email) = match resolve_agent_identity(&root) {
        Some(id) => id,
        None => {
            eprintln!("fatal: no agent identity configured.");
            eprintln!();
            eprintln!("Couldn't resolve an author identity from any of:");
            eprintln!("  - agent_name: / agent_email: in .lex/repo.yml (the simplest fix:");
            eprintln!("    add those two lines there and save again)");
            eprintln!("  - GIT_AUTHOR_NAME / GIT_AUTHOR_EMAIL in the environment");
            eprintln!("  - {}/.claude/settings.json", root.display());
            eprintln!();
            eprintln!("Agent repos: `git lex kit-update` refreshes identity; squad repos get");
            eprintln!("env vars injected by your agent session's settings.");
            exit(1);
        }
    };
    let author = format!("{} <{}>", author_name, author_email);

    // The write-health probe: run the exact gates a real save runs —
    // extraction (which refreshes derived sidecars on disk), the sidecar
    // write-gate, the identity gate, SHACL validation — and commit nothing.
    // Exists because `verify` audits the STORE while the gates live on the
    // WRITE path, and a clean-tree save short-circuits before any gate: a
    // repo could be write-dead with NO command able to say so until the
    // moment a real write is needed (W3BL0RD's receipt, 2026-08-06: verify
    // ALL CHECKS PASSED on a repo that could not save). Known fidelity gap:
    // a real save stages deletions before the hook, so its sidecar cleanup
    // sees them; the probe stages nothing and skips that pass.
    if dry_run {
        cmd_extract();
        if !cmd_validate() {
            eprintln!("DRY RUN: a real `git lex save` would FAIL validation in {}.", root.display());
            exit(1);
        }
        println!(
            "DRY RUN: all save gates pass in {} — a real save would proceed [as {}].",
            root.display(),
            author
        );
        println!("(nothing was committed; derived sidecars under .lex/extract/ may have been refreshed)");
        return;
    }

    // Sync skills/subagents into every active substrate's harness. The
    // substrate list comes from `.lex/repo.yml`'s `substrates:` field
    // (explicit override) or auto-detection from on-disk markers
    // (.claude/, .hermes/, .gemini/). Falls back to Claude if nothing
    // is detected, preserving pre-multi-substrate behavior.
    harness::sync_all(&root);


    // Add everything, commit; the pre-commit hook handles extract + validate
    // (NOT sync — the store is updated separately by `git lex sync`)
    let status = Command::new("git")
        .args(["add", "-A"])
        .status();
    if !status.map(|s| s.success()).unwrap_or(false) {
        eprintln!("fatal: git add failed");
        exit(1);
    }

    // Check if there's anything to commit
    let diff = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .status();
    if diff.map(|s| s.success()).unwrap_or(false) {
        // Name the repo: save targets the CWD's repo, and an agent shell's cwd
        // drifts (Day 120: a save fired from another repo's dir reported
        // "nothing to save" while the intended repo sat modified — the bare
        // message was a null signal indistinguishable from a clean save).
        println!("Nothing to save (no changes) in {}", root.display());
        return;
    }

    let status = Command::new("git")
        .args(["commit", "--author", &author, "-m", message])
        .status();
    match status {
        Ok(s) if s.success() => {
            println!("Saved in {}: {} [as {}]", root.display(), message, author);
        }
        _ => {
            eprintln!("fatal: git commit failed");
            exit(1);
        }
    }
}


/// Returns true if all files pass, false if any violations found.
fn cmd_validate() -> bool {
    let start = Instant::now();

    let root = require_git_root();

    let kit = match get_kit() {
        Some(k) => k,
        None => {
            println!("No kit configured — nothing to validate.");
            return true;
        }
    };

    // Collect SHACL shapes TTL from .lex/ontology/{short}/ (kit-owned, built
    // at kit install time).
    let (_, _, short) = resolve_kit_spec(&kit);
    let mut shapes_sources: Vec<(PathBuf, String)> = Vec::new();

    let kit_shapes = root.join(".lex").join("ontology").join(&short)
        .join(format!("{}-shapes.ttl", short));
    if let Ok(ttl) = fs::read_to_string(&kit_shapes) {
        shapes_sources.push((kit_shapes, ttl));
    }

    if shapes_sources.is_empty() {
        // A kit IS configured but its shapes are gone (broken/partial
        // install). A gate that can't run must not pretend it passed
        // (Rob-ruled 2026-07-29) — fail the save and name the fix.
        eprintln!("fatal: kit '{}' is configured but its SHACL shapes are not installed — validation cannot run.", kit);
        eprintln!("Fix: `git lex kit-update` (reinstalls the kit's ontology and shapes), then retry.");
        return false;
    }

    let shapes_ttl: String = shapes_sources.iter()
        .map(|(_, ttl)| ttl.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    // One walker for the whole codebase; `.txt` files ride along for the
    // slug index (sync's resolver indexes them as link targets, so validate
    // must too). Only .md files are validated (filter in the loop below).
    let files = crate::nquad::walk_repo_docs(&root);

    // Parse SHACL shapes into compiled schema (once)
    use rudof_rdf::rdf_core::RDFFormat;
    use rudof_rdf::rdf_impl::{InMemoryGraph, ReaderMode};
    use sparql_service::RdfData;
    use shacl_rdf::ShaclParser;
    use shacl_ir::compiled::schema_ir::SchemaIR as ShaclSchemaIR;
    use shacl_validation::shacl_processor::{GraphValidation, ShaclProcessor, ShaclValidationMode};
    use shacl_validation::store::Graph;

    // CORRUPT shapes = same law as MISSING shapes (twenty lines up): a gate
    // that can't run must not pretend it passed (Rob-ruled 2026-07-29).
    // These four arms used to `return true` — a broken shapes file waved
    // every save through while printing an error nobody was required to
    // read. All four are the identical cure: kit-update regenerates shapes.
    let shapes_broken = |stage: &str, e: &dyn std::fmt::Display| -> bool {
        eprintln!("fatal: kit '{}' shapes are installed but unusable — {stage}: {e}", kit);
        eprintln!("Validation cannot run, so the save is blocked (a gate that can't run must not pretend it passed).");
        eprintln!("Fix: `git lex kit-update` (regenerates the kit's shapes), then retry.");
        false
    };
    let shapes_graph = match InMemoryGraph::from_reader(
        &mut shapes_ttl.as_bytes(), "shapes", &RDFFormat::Turtle, None, &ReaderMode::Lax,
    ) {
        Ok(g) => g,
        Err(e) => return shapes_broken("Turtle parse failed", &e),
    };
    let shapes_rdf = match RdfData::from_graph(shapes_graph) {
        Ok(d) => d,
        Err(e) => return shapes_broken("graph load failed", &e),
    };
    let shapes_schema = match ShaclParser::new(shapes_rdf).parse() {
        Ok(s) => s,
        Err(e) => return shapes_broken("SHACL parse failed", &e),
    };
    let compiled_shapes = match ShaclSchemaIR::compile(&shapes_schema) {
        Ok(c) => c,
        Err(e) => return shapes_broken("schema compile failed", &e),
    };

    let mut total_files = 0;
    let mut total_violations = 0;
    let mut failed_files = Vec::new();

    for filepath in &files {
        if !filepath.to_string_lossy().ends_with(".md") { continue; }
        let ttl = match frontmatter_to_turtle(filepath, &root, &kit) {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(e) => {
                eprintln!("  {}: {}", filepath.display(), e);
                total_files += 1;
                total_violations += 1;
                failed_files.push(filepath.display().to_string());
                continue;
            }
        };
        total_files += 1;

        // Parse this file's Turtle into RdfData
        // Every failure arm below COUNTS as a violation (review #24): a
        // file whose extracted Turtle can't parse, load, or validate is a
        // file the gate could not judge — and a gate that can't run must
        // not pretend it passed (same law as the missing-shapes arm above).
        let data_graph = match InMemoryGraph::from_reader(
            &mut ttl.as_bytes(), &filepath.to_string_lossy(), &RDFFormat::Turtle, None, &ReaderMode::Strict,
        ) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("  Parse error in {}: {}", filepath.display(), e);
                total_violations += 1;
                failed_files.push(filepath.display().to_string());
                continue;
            }
        };
        let data_rdf = match RdfData::from_graph(data_graph) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("  Data load error in {}: {}", filepath.display(), e);
                total_violations += 1;
                failed_files.push(filepath.display().to_string());
                continue;
            }
        };

        // Validate
        let mut validator = GraphValidation::from_graph(
            Graph::from_data(data_rdf), ShaclValidationMode::Native,
        );
        match ShaclProcessor::validate(&mut validator, &compiled_shapes) {
            Ok(report) => {
                if !report.conforms() {
                    let relpath = filepath.strip_prefix(&root).unwrap_or(filepath);
                    let violations = report.count_violations();
                    total_violations += violations;
                    failed_files.push(relpath.to_string_lossy().to_string());
                    eprintln!("  {} — {} violation(s):", relpath.display(), violations);
                    for result in report.results() {
                        let msg = result.message().unwrap_or("(no message)");
                        eprintln!("    → {}", msg);
                    }
                }
            }
            Err(e) => {
                eprintln!("  Validation error for {}: {}", filepath.display(), e);
                total_violations += 1;
                failed_files.push(filepath.display().to_string());
            }
        }
    }

    let elapsed = start.elapsed();
    if total_violations == 0 {
        eprintln!("Validated {} files in {:.1}ms — all pass ✓",
            total_files, elapsed.as_secs_f64() * 1000.0);
        true
    } else {
        eprintln!("Validated {} files in {:.1}ms — {} violation(s) in {} file(s)",
            total_files, elapsed.as_secs_f64() * 1000.0,
            total_violations, failed_files.len());
        false
    }
}



// ─── viz/serve (moved to git-lex-serve binary) ─────────────────

// Viz server and SPARQL endpoint live in src/bin/git-lex-serve.rs


// `cleanup_orphaned_sidecars` was deleted in Phase 3 of the history-graph
// work (2026-04-11). Its replacement is `spo_events::cleanup_sidecars_for_
// staged_changes()` which asks git for the staged change set instead of
// walking the filesystem — fixes the macOS APFS case-insensitivity bug
// and adds rename-as-move support so expensive-to-regenerate sidecars
// (future `.haiku.spo` subagent output) survive folder renames without
// re-running extractors.

/// Combined extraction + validation, called by the pre-commit hook.
/// Runs sidecar cleanup, frontmatter extraction, markdown link extraction,
/// stages artifacts, then SHACL validates. Exits non-zero if anything fails.
fn hook_pre_commit() {
    // Phase 1: extraction
    cmd_extract();

    // Stage extraction artifacts. A failed add would let the commit land
    // with sidecars that no longer match the .md content — the history
    // history build diffs COMMITTED sidecars, so that divergence would be
    // permanent and silent. Fail the commit instead.
    //
    // Exception: a repo that gitignores .lex/ has declared its artifacts
    // machine-local (the git-lex code repo dogfoods this way) — nothing is
    // committed, so no committed-sidecar divergence is possible. Skip
    // staging rather than fatal on `git add` refusing an ignored path,
    // which broke every commit in such repos (2026-08-04).
    let lex_ignored = Command::new("git").args(["check-ignore", "-q", ".lex"]).status()
        .map(|s| s.success()).unwrap_or(false);
    if lex_ignored {
        println!(".lex/ is gitignored here — extraction artifacts stay local, not staged.");
    } else {
        let staged = Command::new("git").args(["add", ".lex/extract/"]).status()
            .map(|s| s.success()).unwrap_or(false);
        if !staged {
            eprintln!("fatal: failed to stage extraction artifacts (.lex/extract/)");
            exit(1);
        }
    }

    // Phase 2: SHACL validation
    if !cmd_validate() {
        exit(1);
    }
}

fn cmd_extract() {
    let start = Instant::now();

    // Clean up .spo sidecars for .md files that are being deleted or
    // renamed in the currently-staged commit. Uses git to detect the
    // change set — exact-case, handles rename-as-move so future subagent-
    // driven `.haiku.spo` content survives folder renames without
    // regeneration. Replaces the old cleanup_orphaned_sidecars walker that
    // was buggy on macOS APFS (case-insensitive `Path::exists()`).
    //
    // See src/spo_events.rs and Situation/2026-04-09-history-graph-
    // temporal-ledger.md §11 for the design.
    let cleanup = spo_events::cleanup_sidecars_for_staged_changes();
    if !cleanup.is_empty() {
        eprintln!("Cleanup: {}", cleanup.summary());
        for p in &cleanup.deleted {
            eprintln!("  removed  {}", p);
        }
        for (old, new) in &cleanup.renamed {
            eprintln!("  moved    {} → {}", old, new);
        }
        for err in &cleanup.errors {
            eprintln!("  error    {}", err);
        }
        if !cleanup.errors.is_empty() {
            // An orphan sidecar left behind here keeps its facts alive in
            // the graph forever (the sync diff never sees the lines vanish).
            // Fail the commit; fix the state and retry.
            eprintln!("fatal: sidecar cleanup failed — see errors above");
            exit(1);
        }
    }

    // Run frontmatter extraction (writes .spo sidecars as a side effect).
    // The context is built here and shared with the identity gate below.
    let ctx_root = git_lex::find_git_root();
    let (_nq, mut extraction_errors, extract_ctx) = match &ctx_root {
        Some(root) => {
            let ctx = nquad::ResolverContext::build(root);
            let (nq, errs) = nquad::generate_frontmatter_nquads_with(root, &ctx);
            (nq, errs, Some(ctx))
        }
        None => {
            let (nq, errs) = generate_frontmatter_nquads();
            (nq, errs, None)
        }
    };

    // Run markdown link extraction via tree-sitter. Its errors join the
    // save gate (review #23): an unextractable doc keeps a stale sidecar.
    extraction_errors += extract_markdown_links();

    // (The .jsonl session extractor ran here 2026-04→08: claude-code-kit
    // only, 13 ad-hoc operators no ontology declared, zero sidecars ever
    // produced in any live repo. Deleted Rob-ruled 2026-08-01 — transcript
    // analytics is ravel's domain.)

    // The v1 write-gate: re-read EVERY sidecar (extraction rewrites the
    // full tree each save) and validate against the format spec using the
    // walker's own line rules. Nothing gets committed that history can't
    // later read — the enforcement brick whose absence let one wrapped
    // line ride 549 commits of lUX history.
    let mut gate_files = 0usize;
    let mut gate_errors = 0usize;
    if let Some(root) = git_lex::find_git_root() {
        let mut stack = vec![root.join(".lex").join("extract")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|e| e.to_str()) == Some("spo") {
                    gate_files += 1;
                    let content = std::fs::read_to_string(&path).unwrap_or_default();
                    for (lineno, err) in spo_events::validate_sidecar_v1(&content) {
                        let rel = path.strip_prefix(&root).unwrap_or(&path);
                        eprintln!("sidecar gate: {}:{}: {}", rel.display(), lineno, err);
                        gate_errors += 1;
                    }
                }
            }
        }
    }

    let elapsed = start.elapsed();
    eprintln!("Extracted in {:.1}ms", elapsed.as_secs_f64() * 1000.0);

    if gate_errors > 0 {
        eprintln!(
            "fatal: sidecar write-gate: {} error(s) across {} sidecar file(s). \
             An out-of-spec sidecar means the extractor produced output the \
             format spec forbids — a git-lex bug unless the message names \
             damage in the sidecar file itself. Report it.",
            gate_errors, gate_files
        );
        std::process::exit(1);
    }
    eprintln!("Sidecar gate: {} file(s) conform to the v1 format ✓", gate_files);

    // Identity gate (identity model Law 3): per-class id uniqueness across
    // the repo, enforced at save. Two files claiming the same
    // <kit>/<Class>/<id> would collapse into ONE Thing IRI — a collision,
    // rejected loudly (Rob: "you can't have two things and reliably tell
    // them apart without an id — enforced, must-have"). Only files whose
    // Thing anchor actually derives participate; unanchored classed files
    // already warned in extraction (the Phase-4 work list).
    if let (Some(root), Some(ctx)) = (&ctx_root, &extract_ctx) {
        let mut id_errors = 0usize;
        let mut owners: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut all_sidecars: Vec<(String, Vec<String>)> = Vec::new();
        let mut stack = vec![root.join(".lex").join("extract")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let Some(rel) = path.strip_prefix(root).ok().map(|p| p.to_string_lossy().to_string()) else { continue };
                let Some(src) = rel
                    .strip_prefix(".lex/extract/")
                    .and_then(|s| s.strip_suffix(".fm.spo"))
                else { continue };
                let content = std::fs::read_to_string(&path).unwrap_or_default();
                let lines: Vec<String> = content.lines().map(String::from).collect();
                let subjects = nquad::derive_file_subjects(
                    &lines, src, &ctx.declared_props,
                    &ctx.obj_props, &ctx.kit_namespaces, false,
                );
                if let Some(thing) = subjects.thing_uri {
                    if let Some(prior) = owners.get(&thing) {
                        eprintln!(
                            "identity gate: {} and {} both claim the Thing {} — \
                             per-class ids must be unique; change one file's id",
                            prior, src, thing
                        );
                        id_errors += 1;
                    } else {
                        owners.insert(thing, src.to_string());
                    }
                }
                all_sidecars.push((src.to_string(), lines));
            }
        }
        if id_errors > 0 {
            eprintln!("fatal: identity gate: {} id collision(s)", id_errors);
            std::process::exit(1);
        }
        eprintln!("Identity gate: {} Thing id(s) unique ✓", owners.len());

        // Law 6, save-side: a declared reference whose range class is
        // FILE-EXPRESSED IN THIS REPO (foldered) must point at a Thing
        // that exists here — dangling rejects at save, same posture as
        // the path law. Graph-only ranges (Moment, …) skip the existence
        // check: their id-spaces live in engine stores, which own their
        // own integrity; the IRI still derives deterministically.
        let mut ref_errors = 0usize;
        let mut foldered_cache: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
        for (src, lines) in &all_sidecars {
            for line in lines {
                let parts: Vec<&str> = line.splitn(3, " | ").collect();
                if parts.len() != 3 || parts[1] != "hasValue" || parts[2].trim().is_empty() {
                    continue;
                }
                let segs: Vec<&str> = parts[0].splitn(3, '.').collect();
                if segs.len() != 3 {
                    continue;
                }
                let Some(range_iri) = ctx.ref_ranges.get(&format!("{}/{}", segs[0], segs[2])) else { continue };
                let enforce = *foldered_cache.entry(range_iri.clone()).or_insert_with(|| {
                    let Some(cut) = range_iri.rfind('/') else { return false };
                    let (ns, class) = range_iri.split_at(cut + 1);
                    let kit_short = ns.trim_end_matches('/').rsplit('/').next().unwrap_or("");
                    !kit_short.is_empty() && !class.is_empty()
                        && ontology::get_class_foldered(kit_short, class)
                });
                if !enforce {
                    continue;
                }
                // URL-aware split (review #26): same splitter as the emitter,
                // so the gate checks the exact values sync will resolve.
                for val in nquad::split_object_values(parts[2]) {
                    if let Some(target) = nquad::thing_iri_from_range(range_iri, &val) {
                        if !owners.contains_key(&target) {
                            eprintln!(
                                "identity gate: {}: `{}` references `{}` but no Thing {} exists \
                                 in this repo — dangling references reject at save (Law 6)",
                                src, parts[0], val, target
                            );
                            ref_errors += 1;
                        }
                    }
                }
            }
        }
        if ref_errors > 0 {
            eprintln!("fatal: identity gate: {} dangling reference(s)", ref_errors);
            std::process::exit(1);
        }
    }

    if extraction_errors > 0 {
        eprintln!("fatal: {} frontmatter error(s) — fix before committing", extraction_errors);
        std::process::exit(1);
    }
}


use git_lex::term_to_json;


fn run_query(store: &Store, query: &str, store_type: &str, json: bool) {
    let start = Instant::now();
    let prefixed = add_prefixes(query);

    let mut parsed_query = match oxigraph::sparql::SparqlEvaluator::new().parse_query(&prefixed) {
        Ok(e) => e,
        Err(e) => {
            if json {
                eprintln!("{}", serde_json::json!({"error": "parse", "message": e.to_string()}));
            } else {
                eprintln!("SPARQL parse error: {}", e);
            }
            exit(1);
        }
    };
    parsed_query.dataset_mut().set_default_graph_as_union();

    let results = match parsed_query.on_store(store).execute() {
        Ok(r) => r,
        Err(e) => {
            if json {
                eprintln!("{}", serde_json::json!({"error": "eval", "message": e.to_string()}));
            } else {
                eprintln!("SPARQL evaluation error: {}", e);
            }
            exit(1);
        }
    };

    let mut count = 0;
    match results {
        oxigraph::sparql::QueryResults::Solutions(solutions) => {
            let vars: Vec<String> = solutions
                .variables()
                .iter()
                .map(|v| v.as_str().to_string())
                .collect();

            if json {
                // W3C SPARQL 1.1 Query Results JSON Format.
                // Stream bindings directly — don't buffer for table layout.
                let mut bindings: Vec<serde_json::Value> = Vec::new();
                for solution in solutions {
                    let solution = match solution {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("Error reading solution: {}", e);
                            continue;
                        }
                    };
                    count += 1;
                    let mut binding = serde_json::Map::new();
                    for var in &vars {
                        if let Some(term) = solution.get(var.as_str()) {
                            binding.insert(var.clone(), term_to_json(term));
                        }
                    }
                    bindings.push(serde_json::Value::Object(binding));
                }
                let out = serde_json::json!({
                    "head": { "vars": vars },
                    "results": { "bindings": bindings },
                });
                println!("{}", serde_json::to_string(&out).unwrap());
            } else {
                let mut all_rows = Vec::new();
                for solution in solutions {
                    let solution = match solution {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("Error reading solution: {}", e);
                            continue;
                        }
                    };
                    count += 1;
                    let mut row = Vec::new();
                    for var in &vars {
                        let val = solution
                            .get(var.as_str())
                            .map(|t| match t {
                                Term::NamedNode(n) => n.as_str().to_string(),
                                Term::Literal(l) => l.value().to_string(),
                                Term::BlankNode(b) => format!("_:{}", b.as_str()),
                                Term::Triple(t) => format!("<< {} {} {} >>", t.subject, t.predicate, t.object),
                            })
                            .unwrap_or_default();
                        row.push(val);
                    }
                    all_rows.push(row);
                }

                if !all_rows.is_empty() {
                    // Compute column widths
                    let mut widths = vec![0; vars.len()];
                    for (i, var) in vars.iter().enumerate() {
                        widths[i] = var.len();
                    }
                    for row in &all_rows {
                        for (i, val) in row.iter().enumerate() {
                            if val.len() > widths[i] {
                                widths[i] = val.len();
                            }
                        }
                    }

                    // Print header
                    let mut header = String::new();
                    for (i, var) in vars.iter().enumerate() {
                        header.push_str(&format!(" {:width$} |", var, width = widths[i]));
                    }
                    println!("|{} \n|{}", header, "-".repeat(header.len().saturating_sub(1)));

                    // Print rows
                    for row in &all_rows {
                        let mut row_str = String::new();
                        for (i, val) in row.iter().enumerate() {
                            row_str.push_str(&format!(" {:width$} |", val, width = widths[i]));
                        }
                        println!("|{}", row_str);
                    }
                } else {
                    println!("(No results found)");
                    if store_type.starts_with("one graph") {
                        println!("(nothing synced yet? run `git lex sync` to build the store)");
                    }
                }
            }
        }
        oxigraph::sparql::QueryResults::Boolean(b) => {
            if json {
                println!("{}", serde_json::json!({"head": {}, "boolean": b}));
            } else {
                println!("{}", b);
            }
            count = 1;
        }
        oxigraph::sparql::QueryResults::Graph(_) => {
            if json {
                eprintln!("{}", serde_json::json!({
                    "error": "unsupported",
                    "message": "CONSTRUCT/DESCRIBE JSON output not yet supported"
                }));
                exit(1);
            }
            println!("CONSTRUCT/DESCRIBE queries not yet supported in output");
        }
    }

    let elapsed = start.elapsed();
    eprintln!(
        "\n{} results in {:.1}ms ({})",
        count,
        elapsed.as_secs_f64() * 1000.0,
        store_type
    );
}

fn cmd_query(query: String, json: bool) {
    // B2 FIX (w4r3z, Day 40): `query` now builds the "now" view from the WORKING
    // TREE every time, so the documented `create → save → query` flow surfaces a
    // doc's own frontmatter immediately — no `git lex sync` required first.
    //
    // The old code queried the persistent store first when it existed. But `save`
    // writes .spo sidecars WITHOUT recompiling the store (only `sync` does that),
    // so a fresh doc's facts were invisible until `sync` ran — the README's
    // headline query returned 0. (The in-memory fallback also missed them: it read
    // compiled .nq files, which `save` doesn't write either.)
    //
    // Fix: always extract the current working tree (git blobs + frontmatter) into a
    // fresh in-memory store. generate_frontmatter_nquads() reads the live .md files
    // directly — so this reflects exactly what's on disk now. The persistent store
    // remains a SYNC/HISTORY artifact (sync/<sha> graphs); the "now" view is always
    // derived fresh here, trading a little speed for a correct, surprise-free flow.
    let start = Instant::now();
    let store = Store::new().expect("failed to create in-memory store");

    let git_nq = crate::git2_nquads::generate_git2_nquads();
    let git_count = git_nq.lines().count();
    store
        .load_from_reader(RdfFormat::NQuads, Cursor::new(git_nq.as_bytes()))
        .expect("failed to load git triples");

    // The live "now" graph: extract frontmatter + wikilinks straight from the
    // working-tree .md files (this is what `save` would extract, computed fresh).
    let (fm_nq, _errs) = generate_frontmatter_nquads();
    let lex_count = fm_nq.lines().filter(|l| !l.is_empty()).count();
    if !fm_nq.is_empty() {
        store
            .load_from_reader(RdfFormat::NQuads, Cursor::new(fm_nq.as_bytes()))
            .expect("failed to load frontmatter triples");
    }

    // Also fold in any hand-authored `.lex/**/*.nq` files a user dropped in.
    // (`sync` does NOT write .nq — it writes the persistent oxigraph store;
    // this is purely for user-supplied static N-Quads.)
    let lex_nq = load_lex_nquads();
    if !lex_nq.is_empty() {
        store
            .load_from_reader(RdfFormat::NQuads, Cursor::new(lex_nq.as_bytes()))
            .expect("failed to load .lex/ triples");
    }

    let load_ms = start.elapsed().as_secs_f64() * 1000.0;
    run_query(
        &store,
        &query,
        &format!(
            "live working-tree view: {} git + {} frontmatter triples in {:.1}ms",
            git_count, lex_count, load_ms
        ),
        json,
    );
}

// ─── main ──────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init { directory, kit } => init::cmd_init(directory, kit),
        Commands::Create { doctype, instance_id, json } => cmd_create(&doctype, instance_id.as_deref(), json),
        Commands::List { json } => cmd_list(json),
        Commands::Save { message, dry_run } => cmd_save(&message, dry_run),
        Commands::Query { query, json } => cmd_query(query, json),
        Commands::Hook { event } => {
            match event.as_str() {
                "pre-commit" => hook_pre_commit(),
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
