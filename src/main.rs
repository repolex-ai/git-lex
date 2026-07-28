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
use git_lex::{find_git_root, store_path, get_kit,
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
mod legacy_spo;
mod verify;
mod nquad;
mod ontology;
mod shacl;
mod kit;
mod kit_cmds;
mod extraction;

use crate::git::{auto_commit_snapshot, resource_uri};
use crate::nquad::{build_slug_path_indexes, generate_frontmatter_nquads,
                   load_lex_nquads};
use crate::ontology::get_kit_types;
use crate::extraction::{extract_jsonl_sessions, extract_markdown_links, frontmatter_to_turtle};
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
    /// Walks both `.lex/ontology/` (kit-installed) and `_ontology/`
    /// (agent-authored/adaptive) — so `list` sees every class the repo knows,
    /// not just the classes from the configured kit.
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
    },
    /// Remove .lex/ entirely (content files and git history are preserved).
    Nuke,
    /// Re-download and reinstall the kit without touching content or extractions
    KitUpdate {
        /// Kit to update (e.g., repolex-ai/git-lex-kit-squad). If omitted,
        /// updates ALL installed kits (base + domain + optionals).
        kit: Option<String>,
        /// Overwrite local files that differ from the kit. Without this,
        /// drifted files are left untouched and the kit version is installed
        /// alongside as `<file>.kit-latest` so you can diff and decide. With
        /// --force, prior locals are stashed under
        /// `.kit-pre-force/<timestamp>/` before being overwritten.
        #[arg(long)]
        force: bool,
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


/// Get the persistent store path.
// store_path and open_store_read_only imported from git_lex lib

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
pub(crate) fn open_or_create_store() -> Store {
    let Some(path) = store_path() else {
        eprintln!("fatal: not a git repository (run this inside a repo)");
        exit(1);
    };
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


// ─── git lex list ──────────────────────────────────────────────

/// Walk every installed SHACL shape file and emit the class list.
/// Covers both kit-installed shapes (.lex/ontology/*/*-shapes.ttl) and
/// adaptive shapes (_ontology/**/*-shapes.ttl). Output is grouped by prefix.
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
        println!("No classes found. Install a kit with `git lex init --kit <name>` or add shapes under _ontology/.");
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

    let root = match find_git_root() {
        Some(r) => r,
        None => fail("not-a-repo", "fatal: not a git repository".to_string()),
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

fn cmd_save(message: &str) {
    let root = match find_git_root() {
        Some(r) => r,
        None => {
            eprintln!("fatal: not in a git repository");
            exit(1);
        }
    };

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
        println!("Nothing to save (no changes).");
        return;
    }

    let status = Command::new("git")
        .args(["commit", "--author", &author, "-m", message])
        .status();
    match status {
        Ok(s) if s.success() => {
            println!("Saved: {} [as {}]", message, author);
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

    let root = match find_git_root() {
        Some(r) => r,
        None => {
            eprintln!("fatal: not a git repository");
            exit(1);
        }
    };

    let kit = match get_kit() {
        Some(k) => k,
        None => {
            println!("No kit configured — nothing to validate.");
            return true;
        }
    };

    // Collect SHACL shapes TTL from both .lex/ontology/{short}/ (kit-owned,
    // built at kit install time) and _ontology/ (agent-owned, built at sync
    // time by build_adaptive_shapes). Concatenated into one shapes graph —
    // each TTL carries its own @prefix declarations, so merging is safe.
    let (_, _, short) = resolve_kit_spec(&kit);
    let mut shapes_sources: Vec<(PathBuf, String)> = Vec::new();

    // Kit shapes
    let kit_shapes = root.join(".lex").join("ontology").join(&short)
        .join(format!("{}-shapes.ttl", short));
    if let Ok(ttl) = fs::read_to_string(&kit_shapes) {
        shapes_sources.push((kit_shapes, ttl));
    }

    // Adaptive shapes from _ontology/{name}/{name}-shapes.ttl
    let adaptive_root = root.join("_ontology");
    if adaptive_root.exists() {
        if let Ok(entries) = fs::read_dir(&adaptive_root) {
            for entry in entries.flatten() {
                let subdir = entry.path();
                if !subdir.is_dir() { continue; }
                if let Ok(files) = fs::read_dir(&subdir) {
                    for f in files.flatten() {
                        let p = f.path();
                        if p.file_name()
                            .is_some_and(|n| n.to_string_lossy().ends_with("-shapes.ttl"))
                        {
                            if let Ok(ttl) = fs::read_to_string(&p) {
                                shapes_sources.push((p, ttl));
                            }
                        }
                    }
                }
            }
        }
    }

    if shapes_sources.is_empty() {
        println!("No SHACL shapes found for kit '{}' — skipping validation.", kit);
        return true;
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

    let shapes_graph = match InMemoryGraph::from_reader(
        &mut shapes_ttl.as_bytes(), "shapes", &RDFFormat::Turtle, None, &ReaderMode::Lax,
    ) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("Failed to parse SHACL shapes: {}", e);
            return true;
        }
    };
    let shapes_rdf = match RdfData::from_graph(shapes_graph) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Failed to load SHACL shapes: {}", e);
            return true;
        }
    };
    let shapes_schema = match ShaclParser::new(shapes_rdf).parse() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to parse SHACL schema: {}", e);
            return true;
        }
    };
    let compiled_shapes = match ShaclSchemaIR::compile(&shapes_schema) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to compile SHACL shapes: {}", e);
            return true;
        }
    };

    let mut total_files = 0;
    let mut total_violations = 0;
    let mut failed_files = Vec::new();

    // The same slug index sync's emitter resolves against — validate must
    // judge the exact triples sync will emit (review finding A5).
    let (slug_index, _path_index) = build_slug_path_indexes(&root, &files);

    for filepath in &files {
        if !filepath.to_string_lossy().ends_with(".md") { continue; }
        let ttl = match frontmatter_to_turtle(filepath, &root, &kit, &slug_index) {
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
        let data_graph = match InMemoryGraph::from_reader(
            &mut ttl.as_bytes(), &filepath.to_string_lossy(), &RDFFormat::Turtle, None, &ReaderMode::Strict,
        ) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("  Parse error in {}: {}", filepath.display(), e);
                continue;
            }
        };
        let data_rdf = match RdfData::from_graph(data_graph) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("  Data load error in {}: {}", filepath.display(), e);
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
    let staged = Command::new("git").args(["add", ".lex/extract/"]).status()
        .map(|s| s.success()).unwrap_or(false);
    if !staged {
        eprintln!("fatal: failed to stage extraction artifacts (.lex/extract/)");
        exit(1);
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

    // Run frontmatter extraction (writes .spo sidecars as a side effect)
    let (_nq, extraction_errors) = generate_frontmatter_nquads();

    // Run markdown link extraction via tree-sitter
    extract_markdown_links();

    // Run JSONL extraction for claude-code kit
    extract_jsonl_sessions();

    let elapsed = start.elapsed();
    eprintln!("Extracted in {:.1}ms", elapsed.as_secs_f64() * 1000.0);

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
        Commands::Save { message } => cmd_save(&message),
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
        Commands::KitUpdate { kit, force } => kit_cmds::cmd_kit_update(kit, force),
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
    eprintln!("║  this repo by deleting .lex/ and .git/lex/.             ║");
    eprintln!("║                                                         ║");
    eprintln!("║  DELETED:                                               ║");
    eprintln!("║    • .lex/extract/     (extraction sidecars)            ║");
    eprintln!("║    • .lex/kit/         (installed kit)                  ║");
    eprintln!("║    • .lex/ontology/    (ontology files)                 ║");
    eprintln!("║    • .lex/repo.yml     (configuration)                  ║");
    eprintln!("║    • .git/lex/         (SPARQL store)                   ║");
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

    // Remove .git/lex/ (oxigraph store and other derived data, never tracked)
    let git_lex_dir = root.join(".git").join("lex");
    if git_lex_dir.exists() {
        match fs::remove_dir_all(&git_lex_dir) {
            Ok(_) => println!(".git/lex/ removed."),
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
