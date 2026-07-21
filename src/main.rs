use clap::{Parser, Subcommand};
use oxigraph::io::RdfFormat;
use oxigraph::model::*;
use oxigraph::store::Store;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Command, exit};
use std::time::Instant;
use std::fs;
use std::collections::{HashMap, HashSet};
use tree_sitter;

// Shared utilities (also used by git-lex-serve)
use git_lex::{find_git_root, store_path, get_kit,
              resolve_kit_spec, add_prefixes,
              registry_add, registry_remove};

// Frontmatter ObjectProperty value resolver. The rules for what is and isn't
// allowed in frontmatter values are codified as tests in this module — read
// the test suite for the definitive spec.
mod resolve;
mod harness;
mod git;
mod hooks;
mod git2_nquads;
mod legacy_spo;
mod nquad;
mod ontology;
mod shacl;
mod kit;
mod extraction;

use crate::git::{auto_commit_snapshot, graph_uri, resource_uri};
use crate::nquad::{build_slug_path_indexes, emit_spo_line_nquads,
                   generate_frontmatter_nquads,
                   load_lex_nquads, nq_escape, uri_encode_path};
use crate::ontology::{get_kit_prefix_name, get_kit_types,
                      get_object_properties, get_property_datatypes};
use crate::shacl::{build_shacl_shapes, parse_shacl_hints};
use crate::extraction::{extract_jsonl_sessions, extract_markdown_links, frontmatter_to_turtle,
                        sanitize_uri_segment, short_hash};
use crate::kit::{collect_init_variables, fetch_kit_from_github, install_scaffold_files_from,
                 install_scaffold_files_from_skip_existing,
                 kit_config_bool, kit_config_str, read_repo_yml_fields,
                 read_repo_yml_optional_kits, append_optional_kit, remove_optional_kit,
                 fetch_and_validate_optional_kit, remove_kit_install_dir,
                 KitFetchOutcome};

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
    /// Query the knowledge graph.
    ///
    /// By default, queries act on the union of all named graphs, meaning
    /// `SELECT * WHERE { ?s ?p ?o }` will find everything across commits, files,
    /// and extracted metrics automatically.
    ///
    /// Examples:
    ///   git lex query "SELECT * WHERE { ?s ?p ?o } LIMIT 10"
    ///   git lex query "SELECT ?path WHERE { ?f git2:path ?path }"
    Query {
        /// The SPARQL query string
        query: String,
        /// Emit SPARQL 1.1 JSON Results format on stdout. Suppresses the
        /// human-readable table and the trailing stats line (stats go to stderr).
        #[arg(long)]
        json: bool,
    },
    /// Extract frontmatter from .md files → write .spo sidecars + compile log
    #[command(hide = true)]
    Extract,
    /// Validate documents against SHACL shapes from the kit ontology
    #[command(hide = true)]
    Validate,
    /// Internal: called by git hooks, not for direct use
    #[command(hide = true)]
    Hook {
        /// Hook event name (e.g., pre-commit)
        event: String,
    },
    /// Dump all generated N-Quads to stdout (debug)
    Dump,
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
    /// Join a squad repo (creates mutual identity binding)
    Join {
        /// Path to the squad repo to join
        squad_path: String,
    },
    /// Parse a markdown file and show the syntax tree (debug)
    Parse {
        /// File to parse
        file: String,
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
    /// Run a SPARQL CONSTRUCT query and push the result to the local viz server
    Display {
        /// SPARQL CONSTRUCT query (uses viz: namespace for rendering hints)
        query: String,
        /// Port the viz server is running on
        #[arg(long, default_value = "7878")]
        port: u16,
    },
    /// Start ONE local server (pure passthrough to git-lex-serve)
    ///
    /// Subcommands: `viz` (graph visualizer, port 7878), `listen` (SSE
    /// relay, 7879), `sparql` (W3C SPARQL endpoint over the synced store,
    /// 7880). Each invocation starts exactly one server, e.g.
    /// `git lex serve sparql`.
    Serve {
        /// Arguments passed through to git-lex-serve
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Verify the history==now equivalence invariant.
    ///
    /// Reconstructs the set of "live at HEAD" triples from the history
    /// graph (addedIn minus removedIn per reified triple term) and compares
    /// it against the set produced by evaluating the current .spo files
    /// through the same emitter. Reports symmetric difference. A clean run
    /// means the history walker is a faithful mirror of the now-graph
    /// emission pipeline.
    HistoryVerify {
        /// Print the first N mismatched triples on each side
        #[arg(long, default_value = "10")]
        show: usize,
    },
    /// [SPIKE] Build the experimental "one graph" temporal model and print
    /// sample output.
    ///
    /// EXPERIMENTAL — this is a spike, not a shipped feature. It exists to
    /// evaluate a candidate replacement for the current history subsystem. It
    /// is never invoked by `git lex save` or `git lex sync`.
    ///
    /// The one-graph model collapses history + sync + now into a single graph
    /// (`<.../NamedGraph/one>`) using RDF 1.2 triple-term reification:
    ///
    ///     <reifier> rdf:reifies         <<( s p o )>> .
    ///     <reifier> git-lex:assertedIn  <Commit/SHA> .   (line added)
    ///     <reifier> git-lex:retractedIn <Commit/SHA> .   (line removed)
    ///
    /// "What is true now" is a DERIVED query (a fact whose latest event is an
    /// assert with no later retract). Facts JOIN to their commit's author/date
    /// via the existing command-faithful `git:Commit` nodes. Every `.spo` line
    /// is resolved through the same emitter the now-graph uses — no bespoke
    /// resolution.
    ///
    /// The `assertedIn`/`retractedIn` predicate names are PLACEHOLDERS: the
    /// final vocabulary is a decision to be made and DECLARED in the ontology
    /// before any of this ships.
    ///
    /// Writes only `<NamedGraph/one>`, which the real sync clears anyway. Run
    /// with `--clear` to drop that graph and do nothing else.
    SpikeOnegraph {
        /// Drop the SPIKE one-graph and exit without rebuilding.
        #[arg(long)]
        clear: bool,
        /// Max rows to print per demonstration query.
        #[arg(long, default_value = "5")]
        limit: usize,
    },
}



// ─── git lex init ──────────────────────────────────────────────

// Base ontologies (git.ttl, fm.ttl, lex.ttl) are no longer embedded in the
// binary. They ship in the base kit scaffold at scaffold/.lex/ontology/ and
// are installed to .lex/ontology/ by the scaffold installer during init.
// Kit ontologies are fetched from GitHub at init time — no embedded fallback.

const BASE_KIT: &str = "repolex-ai/git-lex-kit-base";

fn cmd_init(directory: Option<String>, kit: Option<String>) {
    // Follow git convention: `git lex init [<directory>]`
    // If a directory is given, cd into it (creating it if necessary).
    if let Some(ref dir) = directory {
        let path = std::path::Path::new(dir);
        if !path.exists() {
            fs::create_dir_all(path).expect("failed to create directory");
        }
        std::env::set_current_dir(path).expect("failed to cd into directory");
    }

    let root = match find_git_root() {
        Some(r) => r,
        None => {
            let cwd = std::env::current_dir().expect("failed to get current directory");
            eprint!("Not a git repository. Initialize one in {}? [Y/n] ", cwd.display());
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).unwrap_or_default();
            let input = input.trim().to_lowercase();
            if input.is_empty() || input == "y" || input == "yes" {
                let status = Command::new("git").args(["init"]).status();
                match status {
                    Ok(s) if s.success() => { println!(); }
                    _ => { eprintln!("fatal: failed to initialize git repository"); exit(1); }
                }
                cwd
            } else {
                eprintln!("Aborted.");
                exit(1);
            }
        }
    };

    // Every repo gets the base kit. If --kit is specified, that kit is
    // installed alongside base (not instead of it). The kit_spec in repo.yml
    // records the domain kit; base is implicit and always present.
    let kit_name = kit.as_deref().unwrap_or(BASE_KIT);
    let (org, repo, kit_short) = resolve_kit_spec(kit_name);
    let kit_spec = format!("{}/{}", org, repo);

    let lex_dir = root.join(".lex");

    // Carry-over on re-init: if repo.yml already exists, stash its fields so
    // we can reuse previously-collected init variables (agent_name, etc.)
    // without re-prompting.
    let mut carryover: HashMap<String, String> = HashMap::new();

    // If .lex/ already exists, this is a re-initialization. We ask the user,
    // then refresh only the kit-derived subdirs (kit/, ontology/). User
    // data (extract/, tickets/) is preserved.
    if lex_dir.exists() {
        carryover = read_repo_yml_fields(&lex_dir.join("repo.yml"));

        eprint!(
            "This repo is already initialized at {}.\n\
             Re-initializing will refresh the kit and ontology files and overwrite scaffold files.\n\
             Extractions, tickets, and repo.yml fields are preserved.\n\
             Continue? [y/N] ",
            lex_dir.display()
        );
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).unwrap_or_default();
        let input = input.trim().to_lowercase();
        if input != "y" && input != "yes" {
            println!("Aborted.");
            return;
        }

        // Auto-commit any uncommitted work before the destructive step.
        // Git history is the safety net — if anything in the working tree
        // would be lost, we want it in a commit first.
        auto_commit_snapshot("re-initialization");

        // Remove only kit-derived subdirs. Everything else in .lex/ stays.
        let _ = fs::remove_dir_all(lex_dir.join("kit"));
        let _ = fs::remove_dir_all(lex_dir.join("ontology"));
    }

    // Create .lex/ structure (idempotent)
    fs::create_dir_all(lex_dir.join("extract")).ok();

    // Ontologies are installed from the base kit scaffold (scaffold/.lex/ontology/)
    // by the scaffold installer below — no hardcoded ontology block needed.

    // Install kit(s). Every repo gets the base kit. If --kit specifies a
    // domain kit (squad, soul, etc.), that's installed alongside base in
    // the same .lex/kit/ directory.
    {
        let lex_kit_root = lex_dir.join("kit");
        let _ = fs::remove_dir_all(&lex_kit_root);

        // Always install base kit.
        let (base_org, base_repo, _) = resolve_kit_spec(BASE_KIT);
        let base_dir = lex_kit_root.join(&base_org).join(&base_repo);
        fs::create_dir_all(&base_dir).ok();
        println!("Downloading base kit {}/{}...", base_org, base_repo);
        if fetch_kit_from_github(BASE_KIT, &base_dir) {
            println!("Base kit installed.");
        } else {
            eprintln!("Failed to fetch base kit from GitHub.");
            eprintln!("Check network access to https://github.com/{}/{}", base_org, base_repo);
            exit(1);
        }

        // Install the domain kit (if different from base).
        let kit_dir = lex_kit_root.join(&org).join(&repo);
        if kit_spec != format!("{}/{}", base_org, base_repo) {
            fs::create_dir_all(&kit_dir).ok();
            println!("Downloading additional kit {}/{}...", org, repo);
            if fetch_kit_from_github(kit_name, &kit_dir) {
                println!("Additional kit installed.");
            } else {
                eprintln!("Failed to fetch kit '{}' from GitHub.", kit_name);
                eprintln!("Check that https://github.com/{}/{} exists and you have network access.", org, repo);
                exit(1);
            }
        }
    }

    // Create .git/lex/ for derived data (oxigraph store, etc.)
    let git_lex_dir = root.join(".git").join("lex");
    fs::create_dir_all(&git_lex_dir).ok();

    // Claude Code kit needs a whitelist-style root .gitignore because it
    // runs against an existing project directory and only wants to track
    // specific patterns. This is the one kit-specific exception until kits
    // ship their own .gitignore assets.
    if kit_name == "claude-code" {
        let gitignore = root.join(".gitignore");
        let cc_ignore = "*\n\
                         !.gitignore\n\
                         !.lex/\n\
                         !.lex/**\n\
                         !README.lex.md\n\
                         !*/\n\
                         !*/*.jsonl\n\
                         !*/memory/\n\
                         !*/memory/**\n\
                         !agents/\n\
                         !agents/**\n\
                         !plans/\n\
                         !plans/**\n\
                         !CLAUDE.md\n\
                         !history.jsonl\n";
        if gitignore.exists() {
            let existing = fs::read_to_string(&gitignore).unwrap_or_default();
            if !existing.contains("!history.jsonl") {
                fs::write(&gitignore, format!("{}\n{}", existing.trim_end(), cc_ignore)).ok();
            }
        } else {
            fs::write(&gitignore, cc_ignore).ok();
        }
    }

    // Every soul gets the engine runtime dirs (.pool/ .copia/ .weave/) ignored.
    // These are the per-soul LOCAL state of the three Subtexture engines (Pool,
    // CoPIA, Weave) — index stores, embeddings, media roots — that must never be
    // committed. Pushed by git-lex so souls don't hand-maintain it and miss a new
    // dir (the leak: lUX committed 155 .pool/ files, W4R3Z 11 Pool/oxigraph/, both
    // because their hand-written .gitignore predated .pool/). Idempotent.
    ensure_engine_gitignore(&root);

    // repo.yml — create if missing, otherwise update the kit: field to
    // match the spec passed to this init run. This matters for re-init:
    // if the user ran init once without --kit and then runs again with
    // --kit X, the kit: field needs to change from "none" to the new spec.
    let repo_yml_path = lex_dir.join("repo.yml");
    if !repo_yml_path.exists() {
        let repo_name = root.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let today = Command::new("date").args(["+%Y-%m-%d"]).output().ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        fs::write(&repo_yml_path, format!(
            "name: {}\nkit: {}\ncreated: {}\n\
             # build_history_on_sync: true  # builds the temporal history graph on every sync; can be slow on first sync with large repos\n",
            repo_name, kit_spec, today
        )).unwrap_or_else(|e| {
            eprintln!("fatal: could not write .lex/repo.yml: {}", e);
            exit(1);
        });
    } else {
        // Rewrite the kit: line in the existing repo.yml to match the
        // current spec. Preserves all other fields (name, created,
        // agent_name, first_commit, etc.) untouched.
        if let Ok(existing) = fs::read_to_string(&repo_yml_path) {
            let mut updated_lines: Vec<String> = Vec::new();
            let mut saw_kit_line = false;
            for line in existing.lines() {
                if line.starts_with("kit: ") || line.starts_with("kit:") {
                    updated_lines.push(format!("kit: {}", kit_spec));
                    saw_kit_line = true;
                } else {
                    updated_lines.push(line.to_string());
                }
            }
            if !saw_kit_line {
                updated_lines.push(format!("kit: {}", kit_spec));
            }
            let mut content = updated_lines.join("\n");
            if !content.ends_with('\n') { content.push('\n'); }
            fs::write(&repo_yml_path, content).unwrap_or_else(|e| {
                eprintln!("fatal: could not update .lex/repo.yml kit binding: {}", e);
                exit(1);
            });
        }
    }

    // README
    let readme_path = lex_dir.join("README.md");
    if !readme_path.exists() {
        fs::write(&readme_path, format!(
            "# .lex/\n\nKnowledge graph managed by git-lex.\nKit: {}\n\n\
             - `extract/` — extraction sidecars (.spo)\n\
             - `ontology/` — ontology definitions\n\
             - `.git/lex/oxigraph/` — local SPARQL store\n",
            kit_name
        )).ok();
    }

    // ─── Install scaffold files BEFORE type folders ───
    // Scaffold copy puts kit ontologies into .lex/ontology/ which
    // get_kit_types() needs to read for folder/template creation.
    {
        let (base_org, base_repo, _) = resolve_kit_spec(BASE_KIT);
        let base_kit_dir = lex_dir.join("kit").join(&base_org).join(&base_repo);
        let mut scaffold_count = install_scaffold_files_from(&base_kit_dir);

        let domain_kit_dir = lex_dir.join("kit").join(&org).join(&repo);
        if domain_kit_dir != base_kit_dir {
            scaffold_count += install_scaffold_files_from(&domain_kit_dir);
        }
        if scaffold_count > 0 {
            println!("Installed {} scaffold file(s) from kit(s)", scaffold_count);
        }
    }

    // Generate SHACL shapes from kit ontology now — both the type-folder
    // loop below and the README generator read shapes via get_kit_types,
    // so shapes must exist before either runs.
    match build_shacl_shapes(kit_name) {
        Ok(Some(shapes_path)) => println!("SHACL shapes generated: {}", shapes_path.file_name().unwrap_or_default().to_string_lossy()),
        Ok(None) => {} // kit ships no ontology — nothing to generate
        Err(e) => {
            eprintln!("fatal: SHACL shapes generation failed for '{}': {}", kit_name, e);
            eprintln!("       a broken kit ontology must not install silently — validation would be skipped and object properties would degrade to literals");
            exit(1);
        }
    }

    // Create type folders from kit ontology.
    // Reads from kit.yml: "install folders", "folder base", "folder ontology".
    // Falls back to legacy "createTypeFolders" for pre-migration kits.
    {
        let create_folders = kit_config_bool(kit_name, "install folders", false)
            || kit_config_bool(kit_name, "createTypeFolders", false);
        let folder_base = kit_config_str(kit_name, "folder base");
        let kit_types = get_kit_types(kit_name);
        if create_folders {
            let mut created: Vec<String> = Vec::new();
            for (type_name, _) in &kit_types {
                // Foldered gate (git-lex:foldered, opt-IN — Rob's ruling,
                // replaces lex-o:instantiation): a class earns a scaffolded
                // folder ONLY when tagged `git-lex:foldered true`. Untagged =
                // graph-only, no folder.
                if !ontology::get_class_foldered(kit_name, type_name) {
                    continue;
                }
                let type_dir = if let Some(ref base) = folder_base {
                    root.join(base).join(type_name)
                } else {
                    root.join(type_name)
                };
                fs::create_dir_all(&type_dir).ok();
                // Add a .gitkeep so empty dirs are tracked
                let gitkeep = type_dir.join(".gitkeep");
                if !gitkeep.exists() {
                    fs::write(&gitkeep, "").ok();
                }
                created.push(type_name.clone());
            }
            if !created.is_empty() {
                let prefix = folder_base.as_deref().unwrap_or("");
                if prefix.is_empty() {
                    println!("Created type folders: {}", created.join(", "));
                } else {
                    println!("Created type folders: {}/{{{}}}", prefix, created.join(", "));
                }
            }
        }
        let type_names: Vec<String> = kit_types.iter().map(|(n, _)| n.clone()).collect();
        if !kit_types.is_empty() {

            // Generate README.lex.md
            let readme_lex = root.join("README.lex.md");
            if !readme_lex.exists() {
                let mut doc = String::new();
                doc.push_str(&format!("# git-lex — {} kit\n\n", kit_short));
                doc.push_str("This repo is managed by [git-lex](https://github.com/repolex-ai/git-lex) — git extensions for knowledge graphs.\n\n");

                doc.push_str("## Quick Start\n\n");
                doc.push_str("```bash\n");
                doc.push_str(&format!("git lex create <type>    # Create a new document (types: {})\n", type_names.join(", ")));
                doc.push_str("git lex save \"message\"   # Add + commit (extracts automatically)\n");
                doc.push_str("git lex sync              # Build/update the knowledge graph\n");
                doc.push_str("git lex query \"SPARQL...\" # Query the knowledge graph\n");
                doc.push_str("```\n\n");

                doc.push_str("## Commands\n\n");
                doc.push_str("| Command | What it does |\n");
                doc.push_str("|---|---|\n");
                doc.push_str(&format!("| `git lex create <type>` | Scaffold a new document. Valid types: {} |\n", type_names.join(", ")));
                doc.push_str("| `git lex save \"msg\"` | Stage all changes, commit, extract frontmatter |\n");
                doc.push_str("| `git lex sync` | Build the SPARQL knowledge graph from git + extractions |\n");
                doc.push_str("| `git lex query \"...\"` | Run a SPARQL query against the knowledge graph |\n");
                doc.push_str("| `git lex log` | Show commit history |\n");
                doc.push_str("| `git lex llm list` | Show files needing LLM extraction |\n");
                doc.push_str("| `git lex llm extract <file> --model <id>` | Extract entities via LLM |\n\n");

                doc.push_str("## Writing Documents\n\n");
                doc.push_str("Documents use YAML frontmatter with flat dot notation: `kit.class.property`\n\n");
                doc.push_str("```yaml\n");
                doc.push_str("---\n");
                doc.push_str(&format!("{}.memory.confidence: \"certain\"\n", kit_short));
                doc.push_str(&format!("{}.memory.source: \"observation\"\n", kit_short));
                doc.push_str(&format!("{}.memory.category: \"fact\"\n", kit_short));
                doc.push_str("---\n\n");
                doc.push_str("Your content here. Use [[wikilinks]] for relationships between documents.\n");
                doc.push_str("```\n\n");
                doc.push_str("See `__ClassName.md` files in each folder for available properties and SHACL-derived constraints.\n\n");

                doc.push_str("## [[wikilinks]]\n\n");
                doc.push_str("Reference other documents naturally in your text:\n\n");
                doc.push_str("- `[[Class/id]]` — creates a `md:linksTo` relationship to that document\n");
                doc.push_str("- bare `[[some-doc]]` is also accepted (resolved via slug)\n\n");
                doc.push_str("Wikilinks are extracted automatically from document bodies and commit messages.\n\n");

                // Kit-specific section
                doc.push_str(&format!("## {} Kit — Document Types\n\n", kit_short));
                for (type_name, properties) in &kit_types {
                    doc.push_str(&format!("### {}\n\n", type_name));
                    doc.push_str(&format!("Create: `git lex create {}`\n\n", type_name));
                    if !properties.is_empty() {
                        let has_comments = properties.iter().any(|(_, _, _, c)| !c.is_empty());
                        if has_comments {
                            doc.push_str("| Property | Type | Description |\n");
                            doc.push_str("|---|---|---|\n");
                        } else {
                            doc.push_str("| Property | Type |\n");
                            doc.push_str("|---|---|\n");
                        }
                        for (prop_name, prop_type, _, comment) in properties {
                            if comment.is_empty() {
                                doc.push_str(&format!("| {} | {} |\n", prop_name, prop_type));
                            } else {
                                doc.push_str(&format!("| {} | {} | {} |\n", prop_name, prop_type, comment));
                            }
                        }
                        doc.push_str("\n");
                    }
                }

                doc.push_str("## Querying\n\n");
                doc.push_str("Auto-injected prefixes: `git:`, `fm:`, `lex:`");
                if kit_short != "none" {
                    doc.push_str(&format!(", `{}:`", kit_short));
                }
                doc.push_str("\n\n");
                doc.push_str("```sparql\n");
                doc.push_str("# List all documents by type\n");
                doc.push_str(&format!("SELECT ?name ?type WHERE {{\n  GRAPH ?g {{ ?doc fm:{}.type ?type ; fm:title ?name }}\n}}\n", kit_short));
                doc.push_str("```\n");

                fs::write(&readme_lex, &doc).ok();
                println!("Created README.lex.md");
            }
        }
    }

    // Class templates (shapes were already generated above before the
    // type-folder loop).
    {
        let kit_types = get_kit_types(kit_name);
        // Read shapes from wherever build_shacl_shapes wrote them (next to
        // the source TTL — static or adaptive). Try both locations.
        let shapes_content = {
            let r = find_git_root().unwrap();
            let (_, _, short) = resolve_kit_spec(kit_name);
            let static_p = r.join(".lex").join("ontology").join(&short).join(format!("{}-shapes.ttl", short));
            let adaptive_p = r.join("_ontology").join(&short).join(format!("{}-shapes.ttl", short));
            fs::read_to_string(&static_p)
                .or_else(|_| fs::read_to_string(&adaptive_p))
                .unwrap_or_default()
        };
        let shacl_hints = parse_shacl_hints(&shapes_content);

        let tmpl_folder_base = kit_config_str(kit_name, "folder base");
        for (type_name, properties) in &kit_types {
            // Foldered gate (git-lex:foldered, opt-IN): only foldered
            // classes get a `__ClassName.md` template — untagged classes
            // have no authored .md files at all.
            if !ontology::get_class_foldered(kit_name, type_name) {
                continue;
            }
            let type_dir = if let Some(ref base) = tmpl_folder_base {
                root.join(base).join(type_name)
            } else {
                root.join(type_name)
            };
            let template_path = type_dir.join(format!("__{}.md", type_name));

            if !template_path.exists() {
                let mut tmpl = String::new();
                tmpl.push_str("---\n");

                for (prop_name, prop_type, _required, _comment) in properties {
                    // Property names pass through as-is from the ontology (camelCase).
                    // Class name is capitalized to match the ontology exactly.
                    let key = format!("{}.{}.{}", kit_short, type_name, prop_name);

                    // Look up SHACL hint for this property
                    let prefix_name = get_kit_prefix_name(&kit_short);
                    let hint = shacl_hints.get(&format!("{}:{}", prefix_name, prop_name));

                    let type_hint = if let Some(h) = hint {
                        h.clone()
                    } else {
                        let base_hint = match prop_type.as_str() {
                            "reference" => format!("IRI -> {}", type_name),
                            _ => "str".to_string(),
                        };
                        format!("optional, {}", base_hint)
                    };

                    tmpl.push_str(&format!("{}: # {}\n", key, type_hint));
                }

                tmpl.push_str("---\n");
                fs::write(&template_path, &tmpl).ok();
            }
        }
        println!("Created class templates");
    }

    // Collect kit-declared init variables (agent name, etc.) by prompting
    // the user. On re-init, reuse values carried over from the previous
    // repo.yml so the user doesn't re-answer the same prompts.
    //
    // Then install files from the kit in two passes:
    //   1. scaffold/ — raw byte-for-byte copy, always clobber. Kit
    //      infrastructure: .claude/, AGENTS.md, hooks, etc.
    //   2. assets/ — template-processed (substitute `{varname}` placeholders
    //      from the variable map). Skip if destination exists (default);
    //      under --force, prompt to overwrite with clear warning. No backup
    //      files — git history is the safety net.
    {
        let vars = collect_init_variables(kit_name, &carryover);

        // Persist collected variables into repo.yml (one line per var, append
        // if not already present). Skip the `kit` key — already in repo.yml.
        if let Ok(existing) = fs::read_to_string(&repo_yml_path) {
            let mut updated = existing.clone();
            for (k, v) in &vars {
                if k == "kit" { continue; }
                if !existing.lines().any(|l| l.starts_with(&format!("{}:", k))) {
                    if !updated.ends_with('\n') { updated.push('\n'); }
                    updated.push_str(&format!("{}: {}\n", k, v));
                }
            }
            fs::write(&repo_yml_path, &updated).unwrap_or_else(|e| {
                eprintln!("fatal: could not persist init variables to .lex/repo.yml: {}", e);
                exit(1);
            });
        }

        // Scaffold files already installed above (before type folder creation).

        // ── Substrate setup ──────────────────────────────────────────
        //
        // After scaffold files are installed and the agent identifier is
        // known (from the init prompts above, stored in `vars`), configure
        // each supported LLM substrate so the agent's identity is wired
        // into the session.
        //
        // Each substrate gets its own setup function. The agent_name
        // (collected via init prompts from the kit's kit.yml) is injected
        // into the substrate's local config so commits, hooks, and tool
        // calls all use the correct identity.
        //
        // Currently: Claude Code (via settings.json env block).
        // Future: Gemini, OpenAI Codex, etc. — stub those as needed after
        // researching how each model's CLI handles per-project identity.

        let agent_name = vars.get("agent_name").cloned().unwrap_or_default();
        if !agent_name.is_empty() {
            // Per-substrate identity injection. Souls are portable across
            // machines via git, so identity travels with the repo —
            // committed to a substrate-specific config file. Each active
            // substrate gets its own setup pass.
            for substrate in harness::active_substrates(&root) {
                match substrate {
                    harness::Substrate::Claude => setup_substrate_claude(&root, &agent_name),
                    harness::Substrate::Hermes => {
                        // TODO: Hermes per-project identity. Hermes uses
                        // hermes-config.yaml + in-process Python lifecycle;
                        // needs research on how it surfaces per-project git
                        // identity vs global config.
                    }
                    harness::Substrate::Gemini => {
                        // TODO: Gemini / Antigravity CLI per-project config.
                    }
                }
            }
        }

    }

    // Print summary
    println!("Initialized .lex/ in {}", root.display());
    println!();
    println!("  .lex/repo.yml     — repo config (kit: {})", kit_spec);
    println!("  .lex/extract/     — extraction sidecars");
    println!("  .lex/ontology/    — upper ontologies");
    println!("  .lex/kit/         — installed kit");
    println!();

    // Pre-commit hook: extract + validate on every commit. This IS the
    // enforcement gate — if it can't be installed, saying so loudly beats
    // a repo that silently never validates.
    // Respects core.hooksPath (husky, lefthook, etc.)
    match hooks::install_hook() {
        Ok(()) => println!("Installed pre-commit hook (extract + validate on commit)"),
        Err(e) => {
            eprintln!("fatal: could not install the pre-commit hook: {}", e);
            eprintln!("       commits would silently skip extraction + validation — fix and re-run init");
            exit(1);
        }
    }

    // NO post-commit hook — sync is manual/background

    // Commit setup files
    let has_commits = Command::new("git").args(["rev-parse", "HEAD"]).output()
        .map(|o| o.status.success()).unwrap_or(false);

    let _ = Command::new("git").args(["add", ".lex/"]).status();
    if Command::new("git").args(["commit", "-m", "git lex init"]).output()
        .map(|o| o.status.success()).unwrap_or(false) {
        println!("\nCommitted git-lex setup files.");
    }

    // Offer to commit existing content (only for brand-new repos)
    if !has_commits {
        let untracked = Command::new("git").args(["status", "--porcelain"]).output().ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        if !untracked.is_empty() {
            eprint!("Commit existing files to the repository? [Y/n] ");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).unwrap_or_default();
            let input = input.trim().to_lowercase();
            if input.is_empty() || input == "y" || input == "yes" {
                let _ = Command::new("git").args(["add", "."]).status();
                let committed = Command::new("git").args(["commit", "-m", "Initial content"]).status()
                    .map(|s| s.success()).unwrap_or(false);
                if committed {
                    println!("Committed existing content.");
                } else {
                    eprintln!("warning: initial content commit failed — see git output above");
                }
            }
        }
    }

    // Capture first commit SHA — the cryptographic anchor — and append it
    // to repo.yml as `first_commit:`. This replaces the old .lex/identity.yml
    // file; one file holds all repo metadata now.
    let first_sha = Command::new("git")
        .args(["rev-list", "--max-parents=0", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    if !first_sha.is_empty() {
        let existing = fs::read_to_string(&repo_yml_path).unwrap_or_default();
        if !existing.contains("first_commit:") {
            let updated = format!("{}first_commit: {}\n", existing, first_sha);
            fs::write(&repo_yml_path, &updated).unwrap_or_else(|e| {
                eprintln!("fatal: could not record first_commit identity in .lex/repo.yml: {}", e);
                exit(1);
            });
            let _ = Command::new("git").args(["add", ".lex/repo.yml"]).status();
            let _ = Command::new("git").args(["commit", "-m", "git lex identity"]).status();
            println!("Identity: {}", first_sha);
        }
    }

    // t-box: load installed kit ontologies into the persistent ontology graph
    // (init, kit-add, kit-update; it stays put across syncs).
    {
        let store = open_or_create_store();
        let n = crate::nquad::load_ontology_graph(&store);
        println!("Ontology graph: {} kit ttl file(s) loaded", n);
    }

    // Register this repo in the machine-level registry (~/.lex/repos)
    registry_add(&root);
}


// ─── git lex query ─────────────────────────────────────────────


/// Get the persistent store path.
// store_path and open_store_read_only imported from git_lex lib

/// Create or open the persistent store.
fn open_or_create_store() -> Store {
    let path = store_path().expect("not in a git repo");
    fs::create_dir_all(&path).expect("failed to create .git/lex/oxigraph/");
    Store::open(&path).expect("failed to open store")
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
    let installed = collect_kits_for_update(root, None);

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
            eprintln!("Couldn't resolve GIT_AUTHOR_NAME / GIT_AUTHOR_EMAIL from either:");
            eprintln!("  - process environment (your Claude Code session should inject these)");
            eprintln!("  - {}/.claude/settings.json", root.display());
            eprintln!();
            eprintln!("If this is your soul repo, run `git lex kit-update` to refresh identity.");
            eprintln!("If this is a squad repo, your soul session should be injecting env vars —");
            eprintln!("check that your soul's .claude/settings.json has the env block.");
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

// ─── SHACL validation via rudof ────────────────────────────────


// ─── git lex identity ──────────────────────────────────────────

fn read_identity(root: &std::path::Path) -> Option<String> {
    let content = fs::read_to_string(root.join(".lex").join("repo.yml")).ok()?;
    for line in content.lines() {
        if let Some(sha) = line.strip_prefix("first_commit: ") {
            return Some(sha.trim().to_string());
        }
    }
    None
}

// ─── git lex join ──────────────────────────────────────────────

fn cmd_join(squad_path: &str) {
    let root = match find_git_root() {
        Some(r) => r,
        None => {
            eprintln!("fatal: not a git repository");
            exit(1);
        }
    };

    let squad_root = PathBuf::from(squad_path);
    if !squad_root.join(".lex").join("repo.yml").exists() {
        eprintln!("Not a git-lex repo: {}", squad_path);
        exit(1);
    }

    // Read this agent's identity
    let agent_sha = match read_identity(&root) {
        Some(sha) => sha,
        None => {
            eprintln!("No identity found. Run 'git lex init' first.");
            exit(1);
        }
    };

    // Read squad's identity
    let squad_sha = match read_identity(&squad_root) {
        Some(sha) => sha,
        None => {
            eprintln!("Squad repo has no identity: {}", squad_path);
            exit(1);
        }
    };

    // Read squad name from repo.yml or directory name
    let squad_name = squad_root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Read agent name from identity or directory name
    let agent_name = root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let now = Command::new("date").args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output().ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // --- Write ticket to agent's soul repo ---
    let tickets_dir = root.join(".lex").join("tickets");
    fs::create_dir_all(&tickets_dir).ok();

    let ticket_slug = squad_name.to_lowercase()
        .replace(|c: char| !c.is_alphanumeric() && c != '-', "-");
    let ticket_path = tickets_dir.join(format!("{}.ticket", ticket_slug));

    if ticket_path.exists() {
        println!("Already a member of {} — ticket exists at .lex/tickets/{}.ticket",
            squad_name, ticket_slug);
        return;
    }

    let ticket_content = format!(
        "# Squad membership ticket — do not edit\n\
         # Mutual binding: this agent is a verified member of this squad.\n\
         squad_name: {}\n\
         squad_path: {}\n\
         squad_identity: {}\n\
         agent_name: {}\n\
         agent_identity: {}\n\
         joined: {}\n",
        squad_name, squad_path, squad_sha,
        agent_name, agent_sha, now
    );
    fs::write(&ticket_path, &ticket_content).expect("failed to write ticket");

    // --- Write member entry to squad repo ---
    let members_dir = squad_root.join(".lex").join("members");
    fs::create_dir_all(&members_dir).ok();

    let member_slug = agent_name.to_lowercase()
        .replace(|c: char| !c.is_alphanumeric() && c != '-', "-");
    let member_path = members_dir.join(format!("{}.yml", member_slug));

    let member_content = format!(
        "# Squad member — do not edit\n\
         # This agent has joined this squad via 'git lex join'.\n\
         agent_name: {}\n\
         agent_identity: {}\n\
         agent_repo: {}\n\
         joined: {}\n",
        agent_name, agent_sha,
        root.to_string_lossy(), now
    );
    fs::write(&member_path, &member_content).expect("failed to write member entry");

    println!("Joined squad: {}", squad_name);
    println!("  Agent:  {} ({})", agent_name, &agent_sha[..12]);
    println!("  Squad:  {} ({})", squad_name, &squad_sha[..12]);
    println!("  Ticket: .lex/tickets/{}.ticket", ticket_slug);
    println!("  Member: {} .lex/members/{}.yml", squad_path, member_slug);
    println!("\nCommit both repos to finalize the binding.");
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

    // Find all .md files in the repo
    fn walk_md(dir: &std::path::Path, files: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') { continue; }
                if path.is_dir() { walk_md(&path, files); }
                else if name.ends_with(".md") { files.push(path); }
            }
        }
    }

    let mut files = Vec::new();
    walk_md(&root, &mut files);

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

    for filepath in &files {
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


fn cmd_parse(file: &str) {
    let root = find_git_root().unwrap_or_else(|| std::env::current_dir().unwrap());
    let filepath = root.join(file);
    let content = match fs::read_to_string(&filepath) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Cannot read {}: {}", file, e);
            exit(1);
        }
    };

    let mut parser = tree_sitter_md::MarkdownParser::default();

    let tree = match parser.parse(content.as_bytes(), None) {
        Some(t) => t,
        None => {
            eprintln!("Failed to parse {}", file);
            exit(1);
        }
    };

    let root_node = tree.block_tree().root_node();

    fn print_node(node: tree_sitter::Node, source: &str, depth: usize) {
        let indent = "  ".repeat(depth);
        let text = &source[node.start_byte()..node.end_byte()];
        let preview = {
            let escaped = text.replace('\n', "\\n");
            if escaped.chars().count() > 80 {
                format!("{}...", escaped.chars().take(77).collect::<String>())
            } else {
                escaped
            }
        };
        println!("{}{}  [{}:{}–{}:{}]  \"{}\"",
            indent, node.kind(),
            node.start_position().row + 1, node.start_position().column,
            node.end_position().row + 1, node.end_position().column,
            preview);

        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                print_node(cursor.node(), source, depth + 1);
                if !cursor.goto_next_sibling() { break; }
            }
        }
    }

    println!("Tree-sitter parse: {}", file);
    println!();
    print_node(root_node, &content, 0);

    // Summary stats
    let mut node_count = 0;
    let mut type_counts: HashMap<String, usize> = HashMap::new();
    fn count_nodes(node: tree_sitter::Node, count: &mut usize, types: &mut HashMap<String, usize>) {
        *count += 1;
        *types.entry(node.kind().to_string()).or_insert(0) += 1;
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                count_nodes(cursor.node(), count, types);
                if !cursor.goto_next_sibling() { break; }
            }
        }
    }
    count_nodes(root_node, &mut node_count, &mut type_counts);

    println!();
    println!("Total nodes: {}", node_count);
    let mut sorted: Vec<_> = type_counts.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1));
    for (kind, count) in sorted.iter().take(20) {
        println!("  {:30} {}", kind, count);
    }
}

// ─── viz/serve (moved to git-lex-serve binary) ─────────────────

// Viz server, SPARQL endpoint, WebSocket handler, and listen server
// have all moved to src/bin/git-lex-serve.rs

#[tokio::main(flavor = "current_thread")]
async fn cmd_display(query: &str, port: u16) {
    // Send the query to the running viz server — the server runs it against its
    // own (already-open) oxigraph store and broadcasts the result. We don't try
    // to open the store here because RocksDB is exclusive-locked by the server.
    let payload = serde_json::json!({ "query": query });

    let url = format!("http://127.0.0.1:{}/api/run-and-push", port);
    let client = reqwest::Client::new();
    match client.post(&url).json(&payload).send().await {
        Ok(resp) if resp.status().is_success() => {
            println!("Pushed scene to {}", url);
        }
        Ok(resp) => {
            eprintln!("Push failed: HTTP {}", resp.status());
            if let Ok(body) = resp.text().await {
                eprintln!("{}", body);
            }
            exit(1);
        }
        Err(e) => {
            eprintln!("Push failed: {}", e);
            eprintln!("Is the viz server running? Try: git lex-serve viz --port {}", port);
            exit(1);
        }
    }
}

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

fn cmd_sync() {
    let start = Instant::now();

    let root = find_git_root().expect("not a git repo");
    // Identity: resolve + persist the genesis SHA ONCE per sync (identity.yml
    // is what Pool's boot-skip and federation readers consume). IRIs no longer
    // carry it — see git.rs Task-2 IRI families.
    crate::git::ensure_identity_yml();
    let store = open_or_create_store();

    // Get current HEAD commit
    let head_sha = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    if head_sha.is_empty() {
        println!("No commits yet. Nothing to sync.");
        return;
    }

    // ─── Always: regenerate adaptive shapes before the fast-path check ───
    // Adaptive shapes are derived from `_ontology/*.ttl` (agent-authored,
    // can change at any time). Regenerating is cheap and idempotent. We
    // do it BEFORE the fast-path so that when an agent edits an ontology
    // without committing, the shapes file refreshes even if HEAD hasn't
    // moved. Adaptive shapes are also a precondition for `git lex create`
    // / `git lex list` finding adaptive-kit doctypes.
    let (adaptive_ok, adaptive_fail) = crate::shacl::build_adaptive_shapes();
    for (ttl, err) in &adaptive_fail {
        eprintln!("warning: adaptive shapes failed for {}: {}", ttl.display(), err);
    }

    // ─── Fast path: already-synced no-op ───
    // If a /sync/{HEAD_SHA}/ graph already exists AND the extract dir is
    // clean (no uncommitted .spo changes), every phase of sync would be a
    // no-op that rebuilds identical state. Skip the whole thing.
    //
    // Contract this depends on: the oxigraph store is derived. If you've
    // manually mutated it, rebuild via `rm -rf .git/lex/oxigraph`.
    {
        let sync_graph_uri = graph_uri(&format!("sync/{}", head_sha));
        let probe = format!(
            "ASK {{ GRAPH <{}> {{ ?s ?p ?o }} }}",
            sync_graph_uri
        );
        let already_synced = oxigraph::sparql::SparqlEvaluator::new()
            .parse_query(&probe)
            .ok()
            .and_then(|q| q.on_store(&store).execute().ok())
            .map(|r| matches!(r, oxigraph::sparql::QueryResults::Boolean(true)))
            .unwrap_or(false);

        // The fast path also requires the one graph to EXIST — an
        // already-synced store from before the one-graph era (or one whose
        // graph was cleared) must fall through so the phase builds it.
        let onegraph_present = {
            let probe = format!(
                "ASK {{ GRAPH <{}> {{ ?s ?p ?o }} }}",
                spo_events::LEXHISTORY_GRAPH_IRI
            );
            oxigraph::sparql::SparqlEvaluator::new()
                .parse_query(&probe)
                .ok()
                .and_then(|q| q.on_store(&store).execute().ok())
                .map(|r| matches!(r, oxigraph::sparql::QueryResults::Boolean(true)))
                .unwrap_or(false)
        };

        if already_synced && onegraph_present {
            // Check .lex/extract/ for uncommitted .spo changes
            let dirty = Command::new("git")
                .args(["status", "--porcelain", "--", ".lex/extract/"])
                .current_dir(&root)
                .output()
                .ok()
                .map(|o| !o.stdout.is_empty())
                .unwrap_or(true); // on error, fall through to full sync

            if !dirty {
                let elapsed = start.elapsed();
                println!(
                    "Already synced at {} ({:.1}ms).",
                    &head_sha[..8.min(head_sha.len())],
                    elapsed.as_secs_f64() * 1000.0
                );
                return;
            }
        }
    }

    // ─── One-graph resume point: read BEFORE Phase 1 clears the commits
    // graph. The resume commit = the commit carrying MAX git2:ordinalDerived
    // in the PREVIOUS sync's commits graph. No stored marker (Rob-ruled):
    // the persisted commit data IS the marker — a no-change commit still
    // lands in the commits graph, so "newest in store" is always the true
    // high-water mark.
    let onegraph_resume: Option<String> = {
        let q = format!(
            "SELECT ?sha WHERE {{ GRAPH <{}> {{ \
               ?c <https://repolex.ai/ontology/git-lex/git2/ordinalDerived> ?o ; \
                  <https://repolex.ai/ontology/git-lex/git2/id> ?sha }} \
             }} ORDER BY DESC(?o) LIMIT 1",
            graph_uri("commits")
        );
        oxigraph::sparql::SparqlEvaluator::new()
            .parse_query(&q)
            .ok()
            .and_then(|q| q.on_store(&store).execute().ok())
            .and_then(|r| match r {
                oxigraph::sparql::QueryResults::Solutions(mut sols) => {
                    sols.next().and_then(|s| s.ok()).and_then(|s| {
                        s.get("sha").map(|t| match t {
                            oxigraph::model::Term::Literal(l) => l.value().to_string(),
                            other => other.to_string(),
                        })
                    })
                }
                _ => None,
            })
    };

    // ─── Phase 1: Clear and regenerate virtual graphs ───
    // Virtual graphs are ephemeral — rebuilt from git every sync.
    // We clear ALL graphs that aren't /sync/ graphs, then reload.
    // Sync graphs are persistent — never touched.

    // Find all existing graph names
    // Enumerate via named_graphs(), NOT a GRAPH ?g pattern — a pattern query
    // only sees graphs holding at least one triple, so an already-empty legacy
    // graph would linger registered forever.
    let existing_graphs: Vec<String> = store
        .named_graphs()
        .filter_map(|g| g.ok())
        .map(|g| match g {
            oxigraph::model::NamedOrBlankNode::NamedNode(n) => n.as_str().to_string(),
            other => other.to_string(),
        })
        .collect();

    // Clear non-sync, non-history graphs (virtual graphs get regenerated).
    // History and meta graphs are persistent — managed by Phase 4.
    for graph_uri in &existing_graphs {
        if !graph_uri.starts_with("https://repolex.ai/git-lex/NamedGraph/sync/")
            && graph_uri != "https://repolex.ai/git-lex/NamedGraph/history"
            && graph_uri != "https://repolex.ai/git-lex/NamedGraph/meta"
            && graph_uri != "https://repolex.ai/git-lex/NamedGraph/repo-ontology"
            // The one graph is PERSISTENT + append-only — never cleared by
            // sync (incremental appends; full rebuild only via the spike
            // command or an invalid-resume fallback).
            && graph_uri != spo_events::LEXHISTORY_GRAPH_IRI
        {
            if let Ok(graph) = oxigraph::model::NamedNode::new(graph_uri) {
                // remove (not clear): drops the graph's registration too, so a
                // one-time legacy name (urn:soul:*) doesn't linger as an empty
                // graph in the store forever.
                if let Err(e) = store.remove_named_graph(&graph) {
                    eprintln!("warning: failed to clear graph {}: {} — stale triples may mix with the regeneration", graph_uri, e);
                }
            }
        }
    }

    // (adaptive shapes already built at top of cmd_sync, before fast-path check)

    // Regenerate the git2 machinery layer (commits/signatures/refs/filetree)
    let git_nq = crate::git2_nquads::generate_git2_nquads();
    let git_count = git_nq.lines().count();
    store
        .load_from_reader(RdfFormat::NQuads, Cursor::new(git_nq.as_bytes()))
        .expect("failed to load git triples");

    // Regenerate frontmatter + wikilink triples
    let (fm_nq, fm_errors) = generate_frontmatter_nquads();
    if fm_errors > 0 {
        eprintln!("warning: {} frontmatter error(s) during sync — graph may be incomplete", fm_errors);
    }
    let fm_count = fm_nq.lines().filter(|l| !l.is_empty()).count();
    if !fm_nq.is_empty() {
        store
            .load_from_reader(RdfFormat::NQuads, Cursor::new(fm_nq.as_bytes()))
            .expect("failed to load frontmatter triples");
    }

    // ─── One-graph phase: append new commits' statement events ───
    sync_onegraph_phase(&store, &root, onegraph_resume);

    // ─── Phase 2: Sync graph — diff sidecars since last sync ───

    // Find last sync commit: the sync graph whose commit is the NEAREST
    // ancestor of HEAD. Graph names end in commit SHAs, which carry no
    // ordering — recency must come from git, not string sort (a string-DESC
    // pick is an arbitrary member of the set, and diffing against an old
    // base silently loses retractions).
    let last_sync_commit: Option<String> = {
        let sync_shas: HashSet<String> = store
            .named_graphs()
            .filter_map(|g| g.ok())
            .filter_map(|g| match g {
                oxigraph::model::NamedOrBlankNode::NamedNode(n) => {
                    let uri = n.as_str();
                    uri.strip_prefix("https://repolex.ai/git-lex/NamedGraph/sync/")
                        .map(|sha| sha.trim_end_matches('/').to_string())
                }
                _ => None,
            })
            .collect();
        if sync_shas.is_empty() {
            None
        } else {
            // Walk HEAD's history newest-first; the first synced commit we
            // meet is the most recent sync base. Sync graphs from other
            // branches (non-ancestors) are correctly ignored.
            Command::new("git")
                .args(["rev-list", "HEAD"])
                .current_dir(&root)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .map(|l| l.trim().to_string())
                        .find(|sha| sync_shas.contains(sha))
                })
        }
    };

    // Get all current .spo sidecars
    let extract_dir = root.join(".lex").join("extract");
    let mut current_spo: HashMap<String, String> = HashMap::new(); // filepath → content

    fn collect_spo(dir: &std::path::Path, base_dir: &std::path::Path, map: &mut HashMap<String, String>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_dir() {
                    collect_spo(&path, base_dir, map);
                } else if path.extension().is_some_and(|e| e == "spo") {
                    if let Ok(content) = fs::read_to_string(&path) {
                        let rel = path.strip_prefix(base_dir).unwrap_or(&path);
                        map.insert(rel.to_string_lossy().to_string(), content);
                    }
                }
            }
        }
    }
    if extract_dir.exists() {
        collect_spo(&extract_dir, &extract_dir, &mut current_spo);
    }

    // Get sidecars at last sync point (from git history)
    let previous_spo: HashMap<String, String> = if let Some(ref last_sha) = last_sync_commit {
        // List .spo files at that commit
        // current_dir(&root) on every git call here: ls-tree pathspecs are
        // cwd-relative, so running sync from a subdirectory without it would
        // silently see NO previous sidecars and re-assert everything as new.
        let output = Command::new("git")
            .args(["ls-tree", "-r", "--name-only", last_sha, ".lex/extract/"])
            .current_dir(&root)
            .output();
        let mut prev = HashMap::new();
        if let Ok(o) = output {
            if o.status.success() {
                let stdout = String::from_utf8_lossy(&o.stdout);
                for file_path in stdout.lines() {
                    if file_path.ends_with(".spo") {
                        let content = Command::new("git")
                            .args(["show", &format!("{}:{}", last_sha, file_path)])
                            .current_dir(&root)
                            .output();
                        if let Ok(c) = content {
                            if c.status.success() {
                                let rel = file_path.strip_prefix(".lex/extract/").unwrap_or(file_path);
                                prev.insert(rel.to_string(), String::from_utf8_lossy(&c.stdout).to_string());
                            }
                        }
                    }
                }
            }
        }
        prev
    } else {
        HashMap::new() // First sync — everything is new
    };

    // Compute delta
    let sync_graph = format!("<{}>", graph_uri(&format!("sync/{}", head_sha)));
    let mut sync_nq = String::new();
    let mut new_assertions = 0;
    let mut retracted = 0;

    // New/changed assertions
    for (spo_file, content) in &current_spo {
        let prev_content = previous_spo.get(spo_file).map(|s| s.as_str()).unwrap_or("");
        let current_lines: HashSet<&str> = content.lines().filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
        let prev_lines: HashSet<&str> = prev_content.lines().filter(|l| !l.is_empty() && !l.starts_with('#')).collect();

        // Derive source file from spo path
        let source_file = spo_file
            .strip_suffix(".fm.spo")
            .or_else(|| {
                // For model-named files: strip .{model}.spo
                if let Some(pos) = spo_file.rfind(".spo") {
                    let without_spo = &spo_file[..pos];
                    if let Some(ext_pos) = without_spo.rfind('.') {
                        let maybe_ext = &without_spo[ext_pos + 1..];
                        if maybe_ext.len() > 3 || maybe_ext == "fm" {
                            return Some(&without_spo[..ext_pos]);
                        }
                    }
                }
                None
            })
            .unwrap_or(spo_file);

        // Get blob hash for this source file
        let blob_hash = Command::new("git")
            .args(["rev-parse", &format!("HEAD:{}", source_file)])
            .current_dir(&root)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                let h = String::from_utf8_lossy(&o.stdout).trim().to_string();
                h[..8.min(h.len())].to_string()
            })
            .unwrap_or_default();

        // New lines = new assertions
        for line in current_lines.difference(&prev_lines) {
            let parts: Vec<&str> = line.splitn(3, " | ").collect();
            if parts.len() < 3 { continue; }
            let (subject, predicate, object) = (parts[0], parts[1], parts[2]);

            // Build entity URIs
            let subject_uri = format!("<{}>", resource_uri(&format!("entity/{}~{}", sanitize_uri_segment(subject), blob_hash)));
            let predicate_uri = if predicate == "isA" {
                "<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>".to_string()
            } else if predicate == "hasValue" {
                format!("<https://repolex.ai/ontology/git-lex/fm/{}>", uri_encode_path(subject))
            } else if predicate == "mentions" {
                "<https://repolex.ai/ontology/git-lex/md/mentions>".to_string()
            } else if predicate == "linksTo" {
                "<https://repolex.ai/ontology/git-lex/md/linksTo>".to_string()
            } else {
                format!("<{}>", resource_uri(&format!("predicate/{}", sanitize_uri_segment(predicate))))
            };
            // Determine if object is a literal or entity reference
            // Literals: isA, hasValue, mentions, linksTo, and any predicate where
            // the object looks like a value (numbers, timestamps, paths, URLs, or
            // contains special chars that aren't entity-name-like)
            let is_literal = predicate == "isA"
                || predicate == "hasValue"
                || predicate == "mentions"
                || predicate == "linksTo"
                || object.contains('/')
                || object.contains(':')
                || object.contains(' ')
                || object.parse::<f64>().is_ok()
                || object.starts_with("20")  // timestamps like 2026-03-26T...
                || predicate.ends_with("Count")
                || predicate.ends_with("Time")
                || predicate.ends_with("Date")
                || predicate.ends_with("Version")
                || predicate.ends_with("Id")
                || predicate.ends_with("Status")
                || predicate.ends_with("Branch")
                || predicate == "cwd"
                || predicate == "ccVersion"
                || predicate == "sessionId"
                || predicate == "toolUsage"
                || predicate == "project"
                || object.starts_with("-")  // dash-encoded paths like -Users-rob-repos
                || object.starts_with("http");
            let object_nq = if is_literal {
                format!("\"{}\"", nq_escape(object))
            } else {
                format!("<{}>", resource_uri(&format!("entity/{}~{}", sanitize_uri_segment(object), blob_hash)))
            };

            // The assertion
            sync_nq.push_str(&format!("{} {} {} {} .\n", subject_uri, predicate_uri, object_nq, sync_graph));

            // Name triple
            sync_nq.push_str(&format!(
                "{} <https://repolex.ai/ontology/git-lex/name> \"{}\" {} .\n",
                subject_uri, nq_escape(subject), sync_graph
            ));

            // Triple term annotation (a git:Annotation node — the sync-diff
            // record of this asserted fact)
            let spo_key = format!("{}|{}|{}|{}", source_file, subject, predicate, object);
            let ann_hash = short_hash(&spo_key);
            let ann_uri = format!("<{}>", resource_uri(&format!("ann/{}", ann_hash)));

            sync_nq.push_str(&format!(
                "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/git/Annotation> {} .\n",
                ann_uri, sync_graph
            ));
            sync_nq.push_str(&format!(
                "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> <<( {} {} {} )>> {} .\n",
                ann_uri, subject_uri, predicate_uri, object_nq, sync_graph
            ));
            sync_nq.push_str(&format!(
                "{} <https://repolex.ai/ontology/git-lex/git/path> \"{}\" {} .\n",
                ann_uri, nq_escape(source_file), sync_graph
            ));
            sync_nq.push_str(&format!(
                "{} <https://repolex.ai/ontology/git-lex/git/blobHash> \"{}\" {} .\n",
                ann_uri, nq_escape(&blob_hash), sync_graph
            ));
            sync_nq.push_str(&format!(
                "{} <https://repolex.ai/ontology/git-lex/git/commitId> \"{}\" {} .\n",
                ann_uri, nq_escape(&head_sha), sync_graph
            ));

            new_assertions += 1;
        }

        // Removed lines = retractions
        for line in prev_lines.difference(&current_lines) {
            let parts: Vec<&str> = line.splitn(3, " | ").collect();
            if parts.len() < 3 { continue; }
            let (subject, predicate, object) = (parts[0], parts[1], parts[2]);

            let spo_key = format!("{}|{}|{}|{}", source_file, subject, predicate, object);
            let ann_hash = short_hash(&spo_key);
            let ann_uri = format!("<{}>", resource_uri(&format!("ann/{}", ann_hash)));

            sync_nq.push_str(&format!(
                "{} <https://repolex.ai/ontology/git-lex/git/retracted> \"true\"^^<http://www.w3.org/2001/XMLSchema#boolean> {} .\n",
                ann_uri, sync_graph
            ));
            retracted += 1;
        }
    }

    // Handle deleted files (in previous but not in current)
    for (spo_file, content) in &previous_spo {
        if !current_spo.contains_key(spo_file) {
            // Entire file deleted — retract all its assertions
            let source_file = spo_file
                .strip_suffix(".fm.spo")
                .unwrap_or(spo_file);

            for line in content.lines().filter(|l| !l.is_empty() && !l.starts_with('#')) {
                let parts: Vec<&str> = line.splitn(3, " | ").collect();
                if parts.len() < 3 { continue; }
                let (subject, predicate, object) = (parts[0], parts[1], parts[2]);

                let spo_key = format!("{}|{}|{}|{}", source_file, subject, predicate, object);
                let ann_hash = short_hash(&spo_key);
                let ann_uri = format!("<{}>", resource_uri(&format!("ann/{}", ann_hash)));

                sync_nq.push_str(&format!(
                    "{} <https://repolex.ai/ontology/git-lex/git/retracted> \"true\"^^<http://www.w3.org/2001/XMLSchema#boolean> {} .\n",
                    ann_uri, sync_graph
                ));
                retracted += 1;
            }
        }
    }

    // Load sync graph into oxigraph
    let sync_count = sync_nq.lines().filter(|l| !l.is_empty()).count();
    if !sync_nq.is_empty() {
        store
            .load_from_reader(RdfFormat::NQuads, Cursor::new(sync_nq.as_bytes()))
            .expect("failed to load sync graph");
    }

    // ─── Phase 3: Stale graph cleanup ───
    // Subsumed by the Phase-2 clear filter: every graph whose name is not a
    // current https://repolex.ai/git-lex/... keep-name (sync/history/meta) is
    // cleared on each sync — including all legacy urn:soul:* graphs and the
    // old `<base>/class/*` + `<base>/frontmatter` projections. Migration off
    // the old naming is therefore automatic on the first new-binary sync.

    // ─── Phase 4: History graph (config-gated, OFF by default) ───
    let history_summary: Option<String> = if build_history_on_sync(&root) {
        Some(sync_history_phase(&root, &store, &head_sha))
    } else {
        None
    };

    store.flush().expect("failed to flush store");

    let elapsed = start.elapsed();

    // Count total sync graph triples
    let total_sync: usize = existing_graphs.iter()
        .filter(|g| g.starts_with("https://repolex.ai/git-lex/NamedGraph/sync/"))
        .count();

    println!(
        "Synced in {:.1}ms:",
        elapsed.as_secs_f64() * 1000.0
    );
    println!("  Virtual: {} git + {} now", git_count, fm_count);
    if !adaptive_ok.is_empty() || !adaptive_fail.is_empty() {
        println!("  Adaptive shapes: {} built, {} failed", adaptive_ok.len(), adaptive_fail.len());
    }
    if new_assertions > 0 || retracted > 0 {
        println!(
            "  Sync /sync/{}/: +{} assertions, -{} retracted ({} quads)",
            &head_sha[..8.min(head_sha.len())], new_assertions, retracted, sync_count
        );
    } else if last_sync_commit.is_some() {
        println!("  No new assertions since last sync");
    } else {
        println!("  First sync — no previous state");
    }
    if let Some(ref history_summary) = history_summary {
        println!("  History: {}", history_summary);
    }
    println!("  Total sync graphs: {}", total_sync + if sync_count > 0 { 1 } else { 0 });
    println!("Store: {}", store_path().unwrap().display());
}

/// Phase 4 of sync: the history graph (the context-graph lane). Reads the
/// lastHistorySync marker; if it is an ancestor of HEAD, walks only newer
/// commits (append), otherwise falls back to a full rebuild. Returns the
/// one-line summary for sync output.
///
/// Config-gated by `build_history_on_sync: true` in `.lex/repo.yml` —
/// OFF by default. Safe to gate: the history graph is derived entirely
/// from git history and can be rebuilt any time by enabling the switch
/// (slow on the first sync of a large repo, by design).
fn sync_history_phase(root: &std::path::Path, store: &Store, head_sha: &str) -> String {

    let history_graph_uri = format!("<{}>", graph_uri("history"));
    let meta_graph_uri = format!("<{}>", graph_uri("meta"));

    // Query the marker
    let marker_query = format!(
        "SELECT ?commit WHERE {{ GRAPH {} {{ <{}> <https://repolex.ai/ontology/spo/lastHistorySync> ?commit }} }}",
        meta_graph_uri,
        resource_uri("meta")
    );
    let marker_sha: Option<String> = {
        match oxigraph::sparql::Query::parse(&marker_query, None) {
            Ok(parsed) => match store.query(parsed) {
                Ok(oxigraph::sparql::QueryResults::Solutions(solutions)) => {
                    solutions.flatten().filter_map(|s| {
                        s.get("commit").and_then(|t| match t {
                            Term::NamedNode(n) => {
                                // URI is <base/commit/SHA> — extract the SHA
                                let uri = n.as_str();
                                uri.rfind("/Commit/").map(|pos| uri[pos + 8..].to_string())
                            }
                            _ => None,
                        })
                    }).next()
                }
                _ => None,
            },
            _ => None,
        }
    };

    // Decide full rebuild vs incremental
    let (history_commits, history_clear) = if let Some(ref marker) = marker_sha {
        // Check if marker is an ancestor of HEAD
        let is_ancestor = Command::new("git")
            .args(["merge-base", "--is-ancestor", marker, "HEAD"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        if marker == &head_sha {
            // Already up to date
            (Vec::new(), false)
        } else if is_ancestor {
            // Incremental: walk only new commits
            let new_shas = spo_events::rev_list_range(marker);
            if new_shas.is_empty() {
                (Vec::new(), false)
            } else {
                eprintln!("  History: incremental — {} new commit(s) since {}", new_shas.len(), &marker[..8.min(marker.len())]);
                let commits = spo_events::collect_commits_from_shas(&new_shas);
                (commits, false) // append, do not clear
            }
        } else {
            // Marker not reachable from HEAD (rebase/amend) — full rebuild
            eprintln!("  History: lastHistorySync marker {} is not an ancestor of HEAD — falling back to full rebuild. This may take a moment.", &marker[..8.min(marker.len())]);
            let all_shas = {
                let out = Command::new("git")
                    .args(["rev-list", "--topo-order", "--reverse", "HEAD"])
                    .output()
                    .expect("git rev-list failed");
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            };
            let commits = spo_events::collect_commits_from_shas(&all_shas);
            (commits, true) // clear first
        }
    } else {
        // No marker — first-time history build
        eprintln!("  History: no lastHistorySync marker found — falling back to full rebuild. This may take a moment.");
        let all_shas = {
            let out = Command::new("git")
                .args(["rev-list", "--topo-order", "--reverse", "HEAD"])
                .output()
                .expect("git rev-list failed");
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        };
        let commits = spo_events::collect_commits_from_shas(&all_shas);
        (commits, true) // clear first
    };

    // Load ontology helpers for the history emitter
    let kit_name = get_kit().unwrap_or_default();
    let hist_obj_props = get_object_properties(&kit_name);
    let hist_prop_datatypes = get_property_datatypes(&kit_name);

    // Build slug/path indexes (same as now-graph builder)
    fn walk_md_for_history(dir: &std::path::Path, files: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') { continue; }
                if path.is_dir() {
                    walk_md_for_history(&path, files);
                } else if name.ends_with(".md") || name.ends_with(".txt") {
                    files.push(path);
                }
            }
        }
    }
    let mut md_files = Vec::new();
    walk_md_for_history(&root, &mut md_files);
    let (hist_slug_index, hist_path_index) = build_slug_path_indexes(&root, &md_files);

    if history_commits.is_empty() {
        "up to date".to_string()
    } else {
        let stats = spo_events::history_walk_engine(
            &history_commits,
            &store,
            &history_graph_uri,
            &meta_graph_uri,
            &head_sha,
            &hist_slug_index,
            &hist_path_index,
            &hist_obj_props,
            &hist_prop_datatypes,
            history_clear,
            true, // show_progress
        );
        match stats.failed {
            Some(ref e) => format!(
                "FAILED ({}) — {} commit(s) not recorded, will retry next sync",
                e, history_commits.len(),
            ),
            None => format!(
                "{} commit(s), {} events, {} annotations",
                history_commits.len(), stats.events_seen, stats.events_emitted,
            ),
        }
    }
}

/// SPIKE — build the experimental "one graph" temporal model and print a
/// sample of its output. See `Commands::SpikeOnegraph` for the full writeup.
///
/// This is a self-contained, side-effect-light exploration command:
///   1. It builds the one-graph into the persistent store's `NamedGraph/one`
///      graph (a graph the real pipeline never touches), doing a full rebuild
///      from all of HEAD's history every run.
///   2. It runs a handful of demonstration queries and prints them so we can
///      "try the model on for size" against real data.
///
/// It does NOT write a sync marker, does NOT alter the `history`/`now`/`sync`
/// graphs, and is never invoked by `git lex save` or `git lex sync`. The
/// `one` graph it writes is harmless (any real sync clears non-sync graphs),
/// but you can drop it explicitly with `--clear`.
/// Sync phase: build the one graph (https://repolex.ai/git-lex/LexHistoryGraph)
/// — base facts + SpoEvents, appended INCREMENTALLY.
///
/// `resume_sha` = the newest commit in the store's previous commits graph
/// (by git2:ordinalDerived), read before Phase 1 cleared it. Commits newer
/// than it (`rev-list ^resume HEAD`) get their .spo events appended. No
/// resume (first build) or an invalid resume (history rewritten) = LOUD
/// full rebuild.
///
/// Every run ends with the structural integrity check (Rob-ruled: id
/// collisions and XOR violations fail loud, never silently dedup).
fn sync_onegraph_phase(store: &Store, root: &std::path::Path, resume_sha: Option<String>) {
    let one_graph_uri = format!("<{}>", spo_events::LEXHISTORY_GRAPH_IRI);

    // Which commits are new?
    let commit_exists = |sha: &str| -> bool {
        Command::new("git")
            .args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
            .current_dir(root)
            .status()
            .map(|st| st.success())
            .unwrap_or(false)
    };
    let rev_list = |range: &[&str]| -> Vec<String> {
        let mut args = vec!["rev-list", "--topo-order", "--reverse"];
        args.extend_from_slice(range);
        Command::new("git")
            .args(&args)
            .current_dir(root)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    };

    let (shas, full_rebuild) = match &resume_sha {
        Some(sha) if commit_exists(sha) => {
            let exclude = format!("^{sha}");
            (rev_list(&[exclude.as_str(), "HEAD"]), false)
        }
        Some(sha) => {
            eprintln!(
                "warning: one-graph resume commit {sha} no longer exists (history rewritten?) — FULL one-graph rebuild"
            );
            (rev_list(&["HEAD"]), true)
        }
        None => (rev_list(&["HEAD"]), true),
    };

    if !shas.is_empty() {
        let commits = spo_events::collect_commits_from_shas(&shas);

        // Same resolver inputs the now-graph emitter uses — ALL installed
        // kits (base + optionals), so one-graph facts resolve identically to
        // now-view facts (single-kit lookups were the old drift source).
        let obj_props = crate::ontology::get_object_properties_all_kits();
        let prop_datatypes = crate::ontology::get_property_datatypes_all_kits();
        let mut md_files = Vec::new();
        fn walk_md(dir: &std::path::Path, files: &mut Vec<PathBuf>) {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with('.') { continue; }
                    if path.is_dir() { walk_md(&path, files); }
                    else if name.ends_with(".md") || name.ends_with(".txt") { files.push(path); }
                }
            }
        }
        walk_md(root, &mut md_files);
        let (slug_index, path_index) = build_slug_path_indexes(root, &md_files);

        let (seen, emitted) = spo_events::onegraph_walk_engine(
            &commits,
            store,
            &one_graph_uri,
            &slug_index,
            &path_index,
            &obj_props,
            &prop_datatypes,
            false, // show_progress — sync prints its own phase summary
            full_rebuild, // clear_first only on a full rebuild
        );
        println!(
            "One graph: {} {} commit(s), {} event(s) seen, {} emitted.",
            if full_rebuild { "full rebuild —" } else { "appended" },
            commits.len(),
            seen,
            emitted
        );
    } else {
        println!("One graph: up to date.");
    }

    // Discovery typing (default graph, idempotent): the graph's NamedGraph
    // object, dual-typed — the store does no inference, so both the class and
    // its NamedGraph parent are stated explicitly.
    let typing = format!(
        "<{g}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/LexHistoryGraph> .\n\
         <{g}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/NamedGraph> .\n",
        g = spo_events::LEXHISTORY_GRAPH_IRI
    );
    if let Err(e) = store.load_from_reader(RdfFormat::NQuads, Cursor::new(typing.as_bytes())) {
        eprintln!("warning: one-graph discovery typing failed to load: {e}");
    }

    // Structural integrity (runs EVERY build): each SpoEvent has exactly one
    // statement (rdf:reifies) and exactly one direction. A violation means a
    // 16-hex id collision or an emitter bug — LOUD, never silently deduped.
    let integrity = format!(
        "SELECT (COUNT(DISTINCT ?e) AS ?bad) WHERE {{ GRAPH <{}> {{ \
           {{ ?e <https://repolex.ai/ontology/git-lex/assertedIn> ?a ; \
                <https://repolex.ai/ontology/git-lex/retractedIn> ?r }} \
           UNION \
           {{ ?e <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> ?t1 , ?t2 . FILTER(?t1 != ?t2) }} \
           UNION \
           {{ ?e <https://repolex.ai/ontology/git-lex/assertedIn> ?c1 , ?c2 . FILTER(?c1 != ?c2) }} \
           UNION \
           {{ ?e <https://repolex.ai/ontology/git-lex/retractedIn> ?d1 , ?d2 . FILTER(?d1 != ?d2) }} \
        }} }}",
        spo_events::LEXHISTORY_GRAPH_IRI
    );
    let bad = oxigraph::sparql::SparqlEvaluator::new()
        .parse_query(&integrity)
        .ok()
        .and_then(|q| q.on_store(store).execute().ok())
        .and_then(|r| match r {
            oxigraph::sparql::QueryResults::Solutions(mut sols) => sols
                .next()
                .and_then(|s| s.ok())
                .and_then(|s| s.get("bad").map(|t| t.to_string())),
            _ => None,
        })
        .and_then(|v| v.split('"').nth(1).and_then(|n| n.parse::<u64>().ok()))
        .unwrap_or(0);
    if bad > 0 {
        eprintln!(
            "ERROR: one-graph integrity check FAILED — {bad} SpoEvent node(s) violate one-statement/one-direction (16-hex id collision or emitter bug). The graph is NOT trustworthy until this is resolved."
        );
    }
}

fn cmd_spike_onegraph(clear: bool, limit: usize) {
    let root = find_git_root().expect("not a git repo");
    let store = open_or_create_store();
    let one_graph_uri = format!("<{}>", spo_events::LEXHISTORY_GRAPH_IRI);

    if clear {
        if let Ok(g) = oxigraph::model::NamedNode::new(spo_events::LEXHISTORY_GRAPH_IRI) {
            let _ = store.clear_graph(&g);
        }
        println!("Cleared the one graph (<{}>).", spo_events::LEXHISTORY_GRAPH_IRI);
        return;
    }

    eprintln!("╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║  git lex spike-onegraph — EXPERIMENTAL temporal-model spike   ║");
    eprintln!("║  Not part of sync/save. Writes only <NamedGraph/one>.        ║");
    eprintln!("╚══════════════════════════════════════════════════════════════╝");

    // Ensure the git2 machinery layer (commits/signatures/refs) is present in
    // the store, so the one-graph's assertedIn/retractedIn commit IRIs have
    // real `git2:Commit` nodes to JOIN to. `git lex query`/`sync` clears
    // non-sync graphs, so we regenerate it here rather than assume a prior
    // sync's copy survived. This is the SAME producer `sync` uses.
    {
        let git_nq = crate::git2_nquads::generate_git2_nquads();
        if !git_nq.is_empty() {
            let parser = oxigraph::io::RdfParser::from_format(oxigraph::io::RdfFormat::NQuads);
            if let Err(e) = store.load_from_reader(parser, std::io::Cursor::new(git_nq.as_bytes())) {
                eprintln!("  one-graph (SPIKE): git-layer load failed (JOIN queries will be empty): {}", e);
            }
        }
    }

    // Full history, oldest→newest (topological). The one-graph spike always
    // does a full rebuild — no incremental path.
    let all_shas = {
        let out = Command::new("git")
            .args(["rev-list", "--topo-order", "--reverse", "HEAD"])
            .current_dir(&root)
            .output()
            .expect("git rev-list failed");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
    };
    if all_shas.is_empty() {
        println!("No commits yet. Nothing to build.");
        return;
    }
    let commits = spo_events::collect_commits_from_shas(&all_shas);

    // Same resolver inputs the now-graph and history walker use.
    let kit_name = get_kit().unwrap_or_default();
    let obj_props = get_object_properties(&kit_name);
    let prop_datatypes = get_property_datatypes(&kit_name);
    let mut md_files = Vec::new();
    fn walk_md(dir: &std::path::Path, files: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') { continue; }
                if path.is_dir() { walk_md(&path, files); }
                else if name.ends_with(".md") || name.ends_with(".txt") { files.push(path); }
            }
        }
    }
    walk_md(&root, &mut md_files);
    let (slug_index, path_index) = build_slug_path_indexes(&root, &md_files);

    let (seen, emitted) = spo_events::onegraph_walk_engine(
        &commits,
        &store,
        &one_graph_uri,
        &slug_index,
        &path_index,
        &obj_props,
        &prop_datatypes,
        true, // show_progress
        true, // clear_first — the spike command is always a full rebuild
    );

    println!(
        "\nBuilt SPIKE one-graph: {} commit(s), {} event(s) seen, {} reified event(s) emitted.",
        commits.len(), seen, emitted
    );

    spike_onegraph_report(&store, spo_events::LEXHISTORY_GRAPH_IRI, limit);
}

/// SPIKE — run demonstration queries over the one-graph and print them.
/// Kept separate so the query set is easy to read and revise as we evaluate
/// the model. All queries are read-only.
fn spike_onegraph_report(store: &Store, one_graph: &str, limit: usize) {
    let reifies = "http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies";
    let asserted = "https://repolex.ai/ontology/git-lex/assertedIn";
    let retracted = "https://repolex.ai/ontology/git-lex/retractedIn";

    let run = |q: &str| -> Vec<Vec<String>> {
        let mut rows = Vec::new();
        if let Ok(oxigraph::sparql::QueryResults::Solutions(sols)) = store.query(q) {
            let vars: Vec<String> = sols.variables().iter().map(|v| v.as_str().to_string()).collect();
            for s in sols.flatten() {
                rows.push(vars.iter().map(|v| s.get(v.as_str()).map(|t| t.to_string()).unwrap_or_default()).collect());
            }
        }
        rows
    };
    let short = |s: &str| -> String {
        s.replace("https://repolex.ai/soul/", "soul:")
            .replace("https://repolex.ai/ontology/git-lex/git/", "git:")
            .replace("https://repolex.ai/ontology/git-lex/", "git-lex:")
            .replace("https://repolex.ai/git-lex/git/Commit/", "commit:")
    };

    println!("\n─── sample assertedIn events (fact → commit) ───");
    for r in run(&format!(
        "SELECT ?s ?p ?o ?c WHERE {{ GRAPH <{one_graph}> {{ ?a <{reifies}> <<( ?s ?p ?o )>> . ?a <{asserted}> ?c }} }} LIMIT {limit}"
    )) {
        println!("  <<( {} {} {} )>>  assertedIn  {}", short(&r[0]), short(&r[1]), short(&r[2]), short(&r[3]));
    }

    println!("\n─── sample retractedIn events (removed lines / retired tags) ───");
    for r in run(&format!(
        "SELECT ?s ?p ?o ?c WHERE {{ GRAPH <{one_graph}> {{ ?a <{reifies}> <<( ?s ?p ?o )>> . ?a <{retracted}> ?c }} }} LIMIT {limit}"
    )) {
        println!("  <<( {} {} {} )>>  retractedIn  {}", short(&r[0]), short(&r[1]), short(&r[2]), short(&r[3]));
    }

    println!("\n─── NOW view (asserted, never later retracted) — a DERIVED query ───");
    for r in run(&format!(
        "SELECT (COUNT(DISTINCT ?tt) AS ?n) WHERE {{ GRAPH <{one_graph}> {{ ?a <{reifies}> ?tt . ?a <{asserted}> ?c . FILTER NOT EXISTS {{ ?b <{reifies}> ?tt . ?b <{retracted}> ?r }} }} }}"
    )) {
        println!("  live facts (asserted, no retract): {}", short(&r[0]));
    }
    for r in run(&format!(
        "SELECT (COUNT(DISTINCT ?tt) AS ?n) WHERE {{ GRAPH <{one_graph}> {{ ?a <{reifies}> ?tt }} }}"
    )) {
        println!("  distinct facts ever asserted:      {}", short(&r[0]));
    }

    println!("\n─── JOIN: a fact → its commit's author + date (rides the git2 layer) ───");
    // The commit's time lives on its author Signature (git2:author → git2:when)
    // in the `commits` graph, not the one-graph, so the join pattern must be
    // scoped with GRAPH ?g (not left in the default graph, which is empty).
    for r in run(&format!(
        "SELECT ?p ?o ?date WHERE {{ \
           GRAPH <{one_graph}> {{ ?a <{reifies}> <<( ?s ?p ?o )>> . ?a <{asserted}> ?c }} \
           GRAPH ?g {{ ?c <https://repolex.ai/ontology/git-lex/git2/author> ?sig . \
                       ?sig <https://repolex.ai/ontology/git-lex/git2/xsdDateTimeDerived> ?date }} \
         }} ORDER BY DESC(?date) LIMIT {limit}"
    )) {
        println!("  {} = {}  @ {}", short(&r[0]), short(&r[1]), short(&r[2]));
    }

    println!("\n─── a fact that CHANGED value across commits (the RDF-1.2 raison d'être) ───");
    for r in run(&format!(
        "SELECT ?s ?p (COUNT(DISTINCT ?o) AS ?values) WHERE {{ GRAPH <{one_graph}> {{ ?a <{reifies}> <<( ?s ?p ?o )>> . ?a <{asserted}> ?c }} }} GROUP BY ?s ?p HAVING (COUNT(DISTINCT ?o) > 1) ORDER BY DESC(?values) LIMIT {limit}"
    )) {
        println!("  {} {} took {} distinct values over time", short(&r[0]), short(&r[1]), short(&r[2]));
    }

    println!("\n(SPIKE output — the model is a PROPOSAL, predicate names are placeholders pending Rob's ruling + ontology declaration.)");
}

/// Read the `build_history_on_sync` switch from `.lex/repo.yml`.
/// Absent, or any value other than `true`, means OFF.
fn build_history_on_sync(root: &std::path::Path) -> bool {
    fs::read_to_string(root.join(".lex").join("repo.yml"))
        .map(|c| c.lines().any(|l| {
            l.trim()
                .strip_prefix("build_history_on_sync:")
                .map(|v| v.trim() == "true")
                .unwrap_or(false)
        }))
        .unwrap_or(false)
}

// add_prefixes imported from git_lex lib

#[allow(deprecated)]
/// Serialize a SPARQL term to W3C SPARQL JSON binding format.
/// https://www.w3.org/TR/sparql11-results-json/#select-encode-terms
use git_lex::term_to_json;

fn run_query(store: &Store, query: &str, store_type: &str, json: bool) {
    let start = Instant::now();
    let prefixed = add_prefixes(query);

    let mut parsed_query = match oxigraph::sparql::Query::parse(&prefixed, None) {
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

    let results = match store.query(parsed_query) {
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
        Commands::Init { directory, kit } => cmd_init(directory, kit),
        Commands::Create { doctype, instance_id, json } => cmd_create(&doctype, instance_id.as_deref(), json),
        Commands::List { json } => cmd_list(json),
        Commands::Save { message } => cmd_save(&message),
        Commands::Query { query, json } => cmd_query(query, json),
        Commands::Dump => {
            let git_nq = crate::git2_nquads::generate_git2_nquads();
            let (fm_nq, _) = generate_frontmatter_nquads();
            let lex_nq = load_lex_nquads();
            print!("{}{}{}", git_nq, fm_nq, lex_nq);
        }
        Commands::Extract => cmd_extract(),
        Commands::Validate => {
            if !cmd_validate() {
                exit(1);
            }
        }
        Commands::Hook { event } => {
            match event.as_str() {
                "pre-commit" => hook_pre_commit(),
                _ => {
                    eprintln!("unknown hook event: {}", event);
                    exit(1);
                }
            }
        }
        Commands::Join { squad_path } => cmd_join(&squad_path),
        Commands::Parse { file } => cmd_parse(&file),
        Commands::Nuke => cmd_nuke(),
        Commands::KitUpdate { kit, force } => cmd_kit_update(kit, force),
        Commands::KitAdd { kit } => cmd_kit_add(kit),
        Commands::KitRemove { kit, force } => cmd_kit_remove(kit, force),
        Commands::Display { query, port } => cmd_display(&query, port),
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
        Commands::HistoryVerify { show } => {
            cmd_history_verify(show);
        }
        Commands::SpikeOnegraph { clear, limit } => cmd_spike_onegraph(clear, limit),
        Commands::Sync => cmd_sync(),
    }
}


// ─── nuke ──────────────────────────────────────────────────────

/// Read the `agent_name:` field from `.lex/repo.yml`, if present. Returns
/// `None` if the file is missing or the field isn't set. Used by both
/// `cmd_init` (carrying the value across re-init) and `cmd_kit_update`
/// (rewiring substrate identity without re-prompting).
fn read_agent_name(root: &std::path::Path) -> Option<String> {
    let content = fs::read_to_string(root.join(".lex").join("repo.yml")).ok()?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("agent_name:") {
            let val = rest.trim().trim_matches('"').to_string();
            if !val.is_empty() {
                return Some(val);
            }
        }
    }
    None
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
fn setup_substrate_claude(root: &std::path::Path, agent_name: &str) {
    let settings_path = root.join(".claude").join("settings.json");
    fs::create_dir_all(settings_path.parent().unwrap()).ok();

    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = fs::read_to_string(&settings_path).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
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
    fs::write(&settings_path, json_str + "\n").ok();
    println!("Claude Code: identity and hooks written to .claude/settings.json");

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

/// Verify the history==now equivalence invariant.
///
/// Reconstructs "live at HEAD" from the history graph by counting addedIn
/// minus removedIn per reified triple term, then compares that set against
/// the triples produced by running the current `.spo` sidecars through
/// `emit_spo_line_nquads` (the same function the now-graph builder uses).
///
/// The fair-comparison trick: we don't compare against the full now-graph,
/// which includes extras like `git:path` / `git:blobHash` / unconditional
/// `rdf:type git-lex:Document` that the history walker never sees. Instead we
/// regenerate the "pure .spo emission" set live and compare against that.
/// Both sides go through the same emitter → symmetric difference should be
/// empty if the history walker is faithful.
fn cmd_history_verify(show: usize) {
    let start = Instant::now();

    let root = find_git_root().expect("not in a git repo");
    let history_graph = format!("<{}>", graph_uri("history"));

    let store_path_buf = root.join(".git").join("lex").join("oxigraph");
    let store = match Store::open(&store_path_buf) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("history-verify: failed to open store at {}: {}", store_path_buf.display(), e);
            exit(1);
        }
    };

    // ─── Step 1: Reconstruct "live at HEAD" from history graph ───
    //
    // Each reified triple (S,P,O) may have multiple annotations across commits:
    // `addedIn` events put it into the working set, `removedIn` events take it
    // back out. A triple is live at HEAD if (count of addedIn) > (count of
    // removedIn). We aggregate per (S,P,O) and keep the net-positive ones.
    //
    // The SPARQL: iterate annotations, pull the triple-term's S/P/O out, plus
    // the addedIn/removedIn predicate. GROUP BY (S,P,O) and sum.
    let reconstruct_query = format!(r#"
        SELECT ?s ?p ?o
               (SUM(IF(?op = <https://repolex.ai/ontology/spo/addedIn>, 1, 0)) AS ?added)
               (SUM(IF(?op = <https://repolex.ai/ontology/spo/removedIn>, 1, 0)) AS ?removed)
        WHERE {{
            GRAPH {} {{
                ?ann <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> <<( ?s ?p ?o )>> .
                ?ann ?op ?commit .
                FILTER(?op IN (<https://repolex.ai/ontology/spo/addedIn>,
                              <https://repolex.ai/ontology/spo/removedIn>))
            }}
        }}
        GROUP BY ?s ?p ?o
    "#, history_graph);

    let mut history_live: HashSet<String> = HashSet::new();
    let results = oxigraph::sparql::SparqlEvaluator::new()
        .parse_query(&reconstruct_query)
        .ok()
        .and_then(|q| q.on_store(&store).execute().ok());
    match results {
        Some(oxigraph::sparql::QueryResults::Solutions(sols)) => {
            for sol in sols.flatten() {
                let s = sol.get("s").map(term_to_nq).unwrap_or_default();
                let p = sol.get("p").map(term_to_nq).unwrap_or_default();
                let o = sol.get("o").map(term_to_nq).unwrap_or_default();
                let added = sol.get("added").and_then(term_int).unwrap_or(0);
                let removed = sol.get("removed").and_then(term_int).unwrap_or(0);
                if added > removed {
                    history_live.insert(format!("{} {} {}", s, p, o));
                }
            }
        }
        _ => {
            eprintln!("history-verify: reconstruct query failed (is the history graph populated? run `git lex sync` first)");
            exit(1);
        }
    }

    // ─── Step 2: Regenerate the "pure .spo emission" set ───
    //
    // Walk all .md/.txt files like generate_frontmatter_nquads does, build the
    // slug/path indexes, then for each current `.spo` sidecar run each line
    // through emit_spo_line_nquads and collect the resulting triples.
    fn walk_md(dir: &std::path::Path, files: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') { continue; }
                if path.is_dir() {
                    walk_md(&path, files);
                } else if name.ends_with(".md") || name.ends_with(".txt") {
                    files.push(path);
                }
            }
        }
    }
    let mut md_files = Vec::new();
    walk_md(&root, &mut md_files);
    let (slug_index, path_index) = build_slug_path_indexes(&root, &md_files);

    let kit = match get_kit() {
        Some(k) => k,
        None => {
            eprintln!("history-verify: no kit configured in .lex/repo.yml");
            exit(1);
        }
    };
    let obj_props = get_object_properties(&kit);
    let prop_datatypes = get_property_datatypes(&kit);

    // Graph tag used when emitting — must match what the walker uses so the
    // triple-term contents compare byte-for-byte. The walker uses history_graph
    // as the scratch graph, and the reified triple-term drops the graph name
    // when it reifies (triple terms are 3-tuples, not 4). So the graph tag
    // here is irrelevant to set membership; we just need SOMETHING syntactically
    // valid. Use history_graph for consistency.
    let emit_graph = history_graph.clone();

    let extract_dir = root.join(".lex").join("extract");
    let mut current_spo_triples: HashSet<String> = HashSet::new();
    fn walk_spo(dir: &std::path::Path, found: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let p = entry.path();
                if p.is_dir() {
                    walk_spo(&p, found);
                } else if p.extension().is_some_and(|e| e == "spo") {
                    found.push(p);
                }
            }
        }
    }
    let mut spo_files: Vec<PathBuf> = Vec::new();
    if extract_dir.exists() {
        walk_spo(&extract_dir, &mut spo_files);
    }

    for spo_path in &spo_files {
        // Derive sidecar relpath (from repo root) and doc URI.
        let sidecar_rel = match spo_path.strip_prefix(&root) {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => continue,
        };
        // Reuse the same derivation the walker uses (via spo_events).
        let doc_uri = match spo_events::doc_uri_from_sidecar(&sidecar_rel) {
            Some(u) => u,
            None => continue,
        };
        // Derive the source document relpath for the emitter (for source_dir
        // in wikilink resolution).
        let source_rel = spo_events::derive_source_document(&sidecar_rel).unwrap_or_default();

        let content = match fs::read_to_string(spo_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut emitted_types: HashSet<String> = HashSet::new();
        for line in content.lines() {
            // Do NOT trim — `.spo` lines with empty object values end in
            // " | " (trailing space), and trimming would chop that to " |"
            // which fails the 3-field split inside emit_spo_line_nquads.
            // Blank/comment filtering uses the trimmed view only for the check.
            let check = line.trim_start();
            if check.is_empty() || check.starts_with('#') { continue; }
            let mut buf = String::new();
            let _errs = emit_spo_line_nquads(
                line,
                &doc_uri,
                &emit_graph,
                &source_rel,
                &slug_index,
                &path_index,
                &obj_props,
                &prop_datatypes,
                &mut emitted_types,
                &mut buf,
            );
            for out_line in buf.lines() {
                let out_line = out_line.trim();
                if out_line.is_empty() { continue; }
                // Drop the graph tag and trailing period to get "S P O".
                // N-Quad shape is "S P O G ." — we want the first three terms.
                let trimmed = out_line.trim_end_matches('.').trim();
                let without_graph = match trimmed.rsplit_once(' ') {
                    Some((rest, _g)) => rest.trim().to_string(),
                    None => continue,
                };
                current_spo_triples.insert(without_graph);
            }
        }
    }

    // ─── Step 3: Symmetric difference ───
    let only_history: Vec<&String> = history_live.difference(&current_spo_triples).collect();
    let only_current: Vec<&String> = current_spo_triples.difference(&history_live).collect();
    let matched = history_live.intersection(&current_spo_triples).count();

    let elapsed = start.elapsed();
    println!("history-verify — equivalence report");
    println!("───────────────────────────────────");
    println!("history graph:     {} triples reconstructed as live at HEAD", history_live.len());
    println!("current .spo:      {} triples emitted from sidecars", current_spo_triples.len());
    println!("matched:           {}", matched);
    println!("only in history:   {}", only_history.len());
    println!("only in current:   {}", only_current.len());
    println!("elapsed:           {:?}", elapsed);

    if !only_history.is_empty() {
        println!("\nonly-in-history (first {}):", show.min(only_history.len()));
        for t in only_history.iter().take(show) {
            println!("  - {}", t);
        }
    }
    if !only_current.is_empty() {
        println!("\nonly-in-current (first {}):", show.min(only_current.len()));
        for t in only_current.iter().take(show) {
            println!("  + {}", t);
        }
    }

    if only_history.is_empty() && only_current.is_empty() {
        println!("\n✓ history == now. the equivalence invariant holds.");
    } else {
        println!("\n✗ history and current differ by {} triple(s).",
                 only_history.len() + only_current.len());
    }
}

/// Serialize an oxigraph Term to its canonical N-Quad form.
fn term_to_nq(t: &Term) -> String {
    match t {
        Term::NamedNode(n) => format!("<{}>", n.as_str()),
        Term::BlankNode(b) => format!("_:{}", b.as_str()),
        Term::Literal(l) => {
            let value = l.value();
            let escaped = nq_escape(value);
            if let Some(lang) = l.language() {
                format!("\"{}\"@{}", escaped, lang)
            } else {
                let dt = l.datatype();
                if dt.as_str() == "http://www.w3.org/2001/XMLSchema#string" {
                    format!("\"{}\"", escaped)
                } else {
                    format!("\"{}\"^^<{}>", escaped, dt.as_str())
                }
            }
        }
        Term::Triple(t) => format!("<< {} {} {} >>", t.subject, t.predicate, t.object),
    }
}

/// Parse a Literal Term as an integer (for SPARQL SUM results).
fn term_int(t: &Term) -> Option<i64> {
    match t {
        Term::Literal(l) => l.value().parse().ok(),
        _ => None,
    }
}

fn cmd_nuke() {
    let root = find_git_root().expect("not in a git repo");
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

// ─── kit-update ────────────────────────────────────────────────

/// Determine the primary domain kit from repo.yml. Returns None if the
/// `kit:` field is missing or `none`.
fn read_domain_kit_from_repo_yml(root: &std::path::Path) -> Option<String> {
    let repo_yml = root.join(".lex").join("repo.yml");
    let content = fs::read_to_string(&repo_yml).unwrap_or_default();
    for line in content.lines() {
        let trimmed = line.trim();
        // Skip list items so a `- kit: ...` shape inside optional_kits doesn't match.
        if trimmed.starts_with('-') { continue; }
        if let Some(rest) = trimmed.strip_prefix("kit:") {
            let val = rest.trim().trim_matches('"').to_string();
            if val != "none" && !val.is_empty() {
                return Some(val);
            }
        }
    }
    None
}

/// Fetch a single kit into its install dir. Caller decides whether to
/// remove-and-replace (cleanest for update) or skip-if-present.
/// Returns true on success.
fn fetch_kit_for_update(kit_spec: &str) -> bool {
    let root = match find_git_root() {
        Some(r) => r,
        None => return false,
    };
    let (org, repo, _) = resolve_kit_spec(kit_spec);
    let kit_dir = root.join(".lex").join("kit").join(&org).join(&repo);
    let _ = fs::remove_dir_all(&kit_dir);
    if fs::create_dir_all(&kit_dir).is_err() {
        return false;
    }
    fetch_kit_from_github(kit_spec, &kit_dir)
}

/// Regenerate one kit's derived artifacts: SHACL shapes, class folders +
/// __ClassName.md templates, and the folder-vs-ontology audit.
///
/// Used by both `cmd_kit_update` (in a loop over all kits) and
/// `cmd_kit_add` (single-kit). Stays silent if the kit has no types.
fn regenerate_kit_artifacts(kit_name: &str, root: &std::path::Path, create_folders: bool) {
    let (_, _, short) = resolve_kit_spec(kit_name);

    match build_shacl_shapes(kit_name) {
        Ok(Some(shapes_path)) => println!("  SHACL shapes regenerated: {}",
            shapes_path.file_name().unwrap_or_default().to_string_lossy()),
        Ok(None) => {} // kit ships no ontology — nothing to regenerate
        Err(e) => {
            eprintln!("fatal: SHACL shapes generation failed for '{}': {}", kit_name, e);
            eprintln!("       a broken kit ontology must not install silently — fix the kit TTL and re-run");
            exit(1);
        }
    }

    let kit_types = get_kit_types(kit_name);
    let shapes_content = {
        let static_p = root.join(".lex").join("ontology").join(&short)
            .join(format!("{}-shapes.ttl", short));
        let adaptive_p = root.join("_ontology").join(&short)
            .join(format!("{}-shapes.ttl", short));
        fs::read_to_string(&static_p)
            .or_else(|_| fs::read_to_string(&adaptive_p))
            .unwrap_or_default()
    };
    let shacl_hints = parse_shacl_hints(&shapes_content);

    let folder_base = kit_config_str(kit_name, "folder base");
    let mut templates_updated = 0usize;
    for (type_name, properties) in &kit_types {
        // Foldered gate (git-lex:foldered, opt-IN — Rob's ruling, replaces
        // lex-o:instantiation): classes exist in the ontology / SHACL
        // surface but get a folder + `__ClassName.md` template ONLY when
        // tagged `git-lex:foldered true`. The quiet default is graph-only,
        // so vocabulary classes never litter empty folders.
        if !ontology::get_class_foldered(kit_name, type_name) {
            continue;
        }

        let type_dir = if let Some(ref base) = folder_base {
            root.join(base).join(type_name)
        } else {
            root.join(type_name)
        };
        // Create the folder if (a) caller wants it (kit-add / kit-update) and
        // (b) it's missing. Templates land in here either way.
        if create_folders {
            fs::create_dir_all(&type_dir).ok();
            let gitkeep = type_dir.join(".gitkeep");
            if !gitkeep.exists() {
                fs::write(&gitkeep, "").ok();
            }
        } else if !type_dir.exists() {
            // No folder + no create → skip template emit so we don't litter
            // a __ClassName.md in a parent that doesn't have the kit folder.
            continue;
        }
        let template_path = type_dir.join(format!("__{}.md", type_name));

        let mut tmpl = String::new();
        tmpl.push_str("---\n");
        for (prop_name, prop_type, _required, _comment) in properties {
            let key = format!("{}.{}.{}", short, type_name, prop_name);
            let prefix_name = get_kit_prefix_name(&short);
            let hint = shacl_hints.get(&format!("{}:{}", prefix_name, prop_name));
            let comment = match hint {
                Some(h) => format!(" # {}", h),
                None => match prop_type.as_str() {
                    "reference" => " # IRI — bare slug or full URI".to_string(),
                    _ => String::new(),
                },
            };
            tmpl.push_str(&format!("{}: {}\n", key, comment.trim_start()));
        }
        tmpl.push_str("---\n");
        fs::write(&template_path, &tmpl).ok();
        templates_updated += 1;
    }

    // Folder audit — only meaningful when the kit declares a folder_base.
    if let Some(ref base) = folder_base {
        let expected: std::collections::HashSet<String> =
            kit_types.iter().map(|(name, _)| name.clone()).collect();
        let base_dir = root.join(base);

        let mut missing = Vec::new();
        for name in &expected {
            if !base_dir.join(name).exists() {
                missing.push(name.clone());
            }
        }
        let mut extra = Vec::new();
        if let Ok(entries) = fs::read_dir(&base_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().to_string();
                if entry.path().is_dir() && !expected.contains(&name) {
                    extra.push(name);
                }
            }
        }
        if !missing.is_empty() {
            eprintln!("  ⚠ Missing folders (in ontology but not on disk): {}", missing.join(", "));
        }
        if !extra.is_empty() {
            eprintln!("  ⚠ Extra folders (on disk but not in ontology): {}", extra.join(", "));
        }
        if missing.is_empty() && extra.is_empty() && !expected.is_empty() {
            println!("  Folders: {}/{} match ontology ✓", expected.len(), expected.len());
        }
    }

    if templates_updated > 0 {
        println!("  {} class template(s) regenerated.", templates_updated);
    }
}

/// Build the ordered list of kits a `kit-update` should refresh.
/// Order matters: base first (carries shared scaffold/ontology), then
/// domain, then optionals (alphabetical for determinism in output).
///
/// If `target` is provided, returns only that one kit (still validated
/// against installed-kit list — refuses to update a kit that isn't here).
fn collect_kits_for_update(root: &std::path::Path, target: Option<&str>) -> Vec<String> {
    let mut all = vec![BASE_KIT.to_string()];
    if let Some(domain) = read_domain_kit_from_repo_yml(root) {
        if domain != BASE_KIT { all.push(domain); }
    }
    let mut optionals = read_repo_yml_optional_kits(&root.join(".lex").join("repo.yml"));
    optionals.sort();
    optionals.dedup();
    for o in optionals {
        if !all.contains(&o) { all.push(o); }
    }
    match target {
        None => all,
        Some(t) => {
            // Exact match against installed list. Allow short or long form by
            // resolving both sides to canonical (org, repo) tuples.
            let (t_org, t_repo, _) = resolve_kit_spec(t);
            let matched: Vec<String> = all.into_iter()
                .filter(|k| {
                    let (o, r, _) = resolve_kit_spec(k);
                    o == t_org && r == t_repo
                })
                .collect();
            if matched.is_empty() {
                eprintln!("Kit '{}' is not installed in this repo. Use `git lex kit-add` first.", t);
                exit(1);
            }
            matched
        }
    }
}

fn cmd_kit_update(kit_arg: Option<String>, force: bool) {
    let root = find_git_root().expect("not in a git repo");
    let lex_dir = root.join(".lex");

    if !lex_dir.exists() {
        eprintln!("Not a git-lex repo. Run 'git lex init' first.");
        exit(1);
    }

    // The list of kits to update. Without a target arg, this is ALL installed
    // kits: base + domain + optionals. With a target, just that one (still
    // must be present in the installed list).
    let kits_to_update = collect_kits_for_update(&root, kit_arg.as_deref());

    // Fetch every kit fresh. Bail on any fetch failure — partial state is
    // worse than no state, and the only way to fail here is network/auth
    // (since the spec was validated against the installed list).
    for spec in &kits_to_update {
        let (org, repo, _) = resolve_kit_spec(spec);
        println!("Updating kit '{}/{}' from GitHub...", org, repo);
        if !fetch_kit_for_update(spec) {
            eprintln!("Failed to fetch kit '{}' from GitHub.", spec);
            eprintln!("Check network access to https://github.com/{}/{}", org, repo);
            exit(1);
        }
    }

    // Install scaffold files from each kit, accumulating drift/stash reports.
    //
    // Without --force:
    //   - Missing files: installed.
    //   - Identical files: silent no-op.
    //   - Drifted files: local left untouched, kit version installed alongside
    //     as `<file>.kit-latest` so the agent can diff and decide.
    // With --force:
    //   - Drifted files are stashed to `.kit-pre-force/<timestamp>/<rel>`
    //     before being overwritten — recovery path if --force was wrong.
    let mut total_installed = 0usize;
    let mut total_skipped = 0usize;
    let mut all_drifted: Vec<String> = Vec::new();
    let mut all_stashed: Vec<String> = Vec::new();
    let mut kit_hook_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for spec in &kits_to_update {
        let (org, repo, _) = resolve_kit_spec(spec);
        let kit_dir = lex_dir.join("kit").join(&org).join(&repo);
        for name in crate::kit::kit_shipped_hook_names(&kit_dir) {
            kit_hook_names.insert(name);
        }
        let report = install_scaffold_files_from_skip_existing(&kit_dir, force);
        total_installed += report.installed;
        total_skipped += report.skipped;
        all_drifted.extend(report.drifted);
        all_stashed.extend(report.stashed);
    }

    // File-level hook reap (twin of the registration reap): now that ALL kits fetched
    // successfully (we bailed above otherwise) and installed, kit_hook_names is the
    // COMPLETE canonical set. Remove any .claude/hooks/*.sh that is neither kit-shipped
    // nor a `<Event>-local-*.sh` personal hook — this kills old-named hooks left behind
    // by a rename (the exact tangle a migrating soul hits: old + new both present +
    // firing). Removed files are stashed to .kit-pre-force/; their now-dangling
    // settings.json registrations get pruned by reap_orphan_hook_registrations inside
    // the setup_substrate_claude pass below (the file is gone → its registration reaps).
    let hook_stash_root = root.join(".kit-pre-force").join("hooks-reap");
    let reaped_hooks = crate::kit::reap_non_kit_non_local_hooks(&root, &kit_hook_names, &hook_stash_root);
    if !reaped_hooks.is_empty() {
        println!(
            "Reaped {} non-kit, non-local hook file(s) (must be kit-shipped or named <Event>-local-*.sh); stashed under .kit-pre-force/:",
            reaped_hooks.len()
        );
        for path in &reaped_hooks {
            println!("  {}", path);
        }
    }

    if total_installed > 0 || total_skipped > 0 || !all_drifted.is_empty() || !all_stashed.is_empty() {
        if force {
            println!("Scaffold: {} file(s) installed (--force)", total_installed);
            if !all_stashed.is_empty() {
                println!("Stashed {} local file(s) under .kit-pre-force/ before overwriting:", all_stashed.len());
                for path in &all_stashed {
                    println!("  {}", path);
                }
            }
        } else {
            println!("Scaffold: {} file(s) installed, {} unchanged", total_installed, total_skipped);
            // Enforced kit-owned files (hooks) converge even without --force; their
            // prior local copies are stashed. Surface them so the overwrite is never
            // silent — the soul sees exactly what was replaced and where the backup is.
            if !all_stashed.is_empty() {
                println!(
                    "Converged {} kit-owned file(s) to the kit version (prior local stashed under .kit-pre-force/):",
                    all_stashed.len()
                );
                for path in &all_stashed {
                    println!("  {}", path);
                }
            }
            if !all_drifted.is_empty() {
                println!(
                    "Drift: {} file(s) differ from kit — kit version available as .kit-latest sibling:",
                    all_drifted.len()
                );
                for path in &all_drifted {
                    println!("  {} (see {}.kit-latest)", path, path);
                }
                println!("Run `diff <file> <file>.kit-latest` to inspect; rm the .kit-latest to dismiss, or mv it over the local to adopt.");
            }
        }
    }

    // Refresh substrate identity for every active substrate. Identity is
    // per-repo, not per-kit, so this runs once after all kit scaffolds are
    // in place. Each substrate gets its own injection pass.
    //
    // This pass is what registers hooks + writes the identity env block into
    // settings.json. It is GATED on read_agent_name — and a soul whose
    // .lex/repo.yml has no parseable `agent_name:` line (e.g. a repo
    // hand-maintained since before that field existed) silently gets NONE of
    // it: kit files converge (separate code path above), but settings.json is
    // never touched, so deleted hooks stay registered and new hooks never do.
    // That's a well-dressed-dead: "kit update complete" with a dead hook layer.
    // The None branch below makes the skip LOUD (prefer-the-crash: a silent
    // skip of the thing that makes hooks FIRE is exactly the R11 ghost). Found
    // by w3bl0rd's flinch-audit on the convergence rollout, Day 50.
    match read_agent_name(&root) {
        Some(agent_name) => {
            for substrate in harness::active_substrates(&root) {
                match substrate {
                    harness::Substrate::Claude => setup_substrate_claude(&root, &agent_name),
                    harness::Substrate::Hermes | harness::Substrate::Gemini => {
                        // Per-substrate identity injection not yet implemented.
                        // The substrate's sync adapter will surface what shape
                        // it needs (see harness/<substrate>.rs).
                    }
                }
            }
        }
        None => {
            eprintln!(
                "warning: no `agent_name:` in .lex/repo.yml — SKIPPED substrate setup \
                 (settings.json hooks + identity env were NOT written/reconciled).\n\
                 Your hooks will not fire and kit hook changes will not converge until \
                 this is fixed. Add a line to .lex/repo.yml:\n\
                 \x20   agent_name: <your-name>\n\
                 then re-run `git lex kit-update`."
            );
        }
    }

    // Remove legacy .env if present. Older souls used .env + SessionStart
    // hook to inject identity; identity now lives in .claude/settings.json
    // and the .env path silently wins over settings.json when both exist
    // (the hook appended .env after settings.json's env block). Sweeping
    // it on every kit-update guarantees one source of truth.
    let legacy_env = root.join(".env");
    if legacy_env.exists() {
        if fs::remove_file(&legacy_env).is_ok() {
            println!("Removed legacy .env — identity now lives in .claude/settings.json");
        }
    }

    // Remove legacy `.lex/ontology/kit/` directory. Pre-multi-kit repos
    // installed shapes at `.lex/ontology/kit/{short}/`; the current layout
    // is `.lex/ontology/{short}/`. Stale shapes files in the old location
    // sort alphabetically BEFORE the new location (`k` < `s`) and used to
    // shadow current shapes via `read_kit_shapes`'s glob-walk. The resolver
    // is now canonical-path-based and ignores them — but stale fossils on
    // disk are still confusing, so sweep them. See task #29.
    let legacy_ontology = root.join(".lex").join("ontology").join("kit");
    if legacy_ontology.exists() {
        if fs::remove_dir_all(&legacy_ontology).is_ok() {
            println!("Removed legacy .lex/ontology/kit/ — shapes now resolve via canonical .lex/ontology/<short>/ path");
        }
    }

    // Converge the engine runtime-dir gitignore on every existing soul. Souls
    // that predate the `.pool/`/`.copia/`/`.weave/` standard hand-wrote their
    // .gitignore and never got these lines — so their engine index stores leaked
    // into git (lUX: 155 .pool/ files; W4R3Z: 11 Pool/oxigraph/). Idempotent: adds
    // the sentinel block once, reports already-tracked files that now match so the
    // soul can `git rm --cached` them deliberately (never auto-mutates the index).
    ensure_engine_gitignore(&root);

    // Regenerate derived artifacts (shapes, class templates, folder audit)
    // for each kit. Order matches kits_to_update so base goes first.
    for spec in &kits_to_update {
        let (org, repo, _) = resolve_kit_spec(spec);
        println!("Regenerating artifacts for '{}/{}'...", org, repo);
        regenerate_kit_artifacts(spec, &root, true);
    }

    println!("Kit update complete: {} kit(s) refreshed.", kits_to_update.len());

    // t-box refresh: reload kit ontologies into the persistent ontology graph
    // (kit vocab may have changed; the graph stays put until the next update).
    {
        let store = open_or_create_store();
        let n = crate::nquad::load_ontology_graph(&store);
        println!("Ontology graph: {} kit ttl file(s) loaded", n);
    }
}

/// The engine runtime dirs every soul must gitignore: the per-soul LOCAL state
/// of the three Subtexture engines (Pool, CoPIA, Weave). These hold index stores,
/// embeddings, HNSW indexes, and media roots — heavy, high-churn, machine-local,
/// never committed. Mirrors the `.pool`/`.copia`/`.weave` standard.
const ENGINE_GITIGNORE_DIRS: &[&str] = &[".pool/", ".copia/", ".weave/"];

const ENGINE_GITIGNORE_BEGIN: &str = "# >>> git-lex engine runtime (managed) >>>";
const ENGINE_GITIGNORE_END: &str = "# <<< git-lex engine runtime (managed) <<<";

/// Idempotently ensure the soul repo's root `.gitignore` ignores the engine
/// runtime dirs (`.pool/`, `.copia/`, `.weave/`). Wrapped in a sentinel block so
/// re-runs replace-in-place (never duplicate) and a future dir can be added by
/// editing `ENGINE_GITIGNORE_DIRS` — the next `git lex kit-update` re-emits the
/// block. Reports (does NOT auto-remove) files already tracked that now match, so
/// the soul can `git rm --cached` them deliberately — git-lex never mutates the
/// index on the soul's behalf (Rob's call, Day 51).
fn ensure_engine_gitignore(root: &Path) {
    let gitignore = root.join(".gitignore");
    let existing = fs::read_to_string(&gitignore).unwrap_or_default();

    // Build the managed block.
    let mut block = String::from(ENGINE_GITIGNORE_BEGIN);
    block.push('\n');
    for dir in ENGINE_GITIGNORE_DIRS {
        block.push_str(dir);
        block.push('\n');
    }
    block.push_str(ENGINE_GITIGNORE_END);

    // Replace an existing managed block in place, or append a fresh one.
    let new_contents = if let (Some(start), Some(end_idx)) = (
        existing.find(ENGINE_GITIGNORE_BEGIN),
        existing.find(ENGINE_GITIGNORE_END),
    ) {
        let end = end_idx + ENGINE_GITIGNORE_END.len();
        let mut s = String::with_capacity(existing.len());
        s.push_str(&existing[..start]);
        s.push_str(&block);
        s.push_str(&existing[end..]);
        s
    } else if existing.trim().is_empty() {
        format!("{block}\n")
    } else {
        format!("{}\n\n{}\n", existing.trim_end(), block)
    };

    if new_contents != existing {
        if fs::write(&gitignore, &new_contents).is_ok() {
            println!("Ensured engine runtime dirs are gitignored (.pool/ .copia/ .weave/).");
        }
    }

    // Report — but never auto-remove — files already tracked that now match. A
    // soul that committed its engine state before this ran needs a deliberate
    // `git rm --cached` (history retained, files stay on disk).
    report_tracked_engine_paths(root);
}

/// Print a warning for any git-tracked paths that fall under the engine runtime
/// dirs, with the exact `git rm --cached` line to untrack them. Read-only: this
/// never touches the index.
fn report_tracked_engine_paths(root: &Path) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output();
    let stdout = match out {
        Ok(o) if o.status.success() => o.stdout,
        _ => return,
    };
    // Engine dir prefixes to match against tracked paths. Include the legacy
    // capitalized `Pool/` tree — the pre-`.pool` layout W4R3Z is still on — so
    // the report catches it too (that whole tree is migrating to `.pool/`).
    let prefixes: &[&str] = &[".pool/", ".copia/", ".weave/", "Pool/"];
    let mut hits: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for path in stdout.split(|b| *b == 0) {
        if path.is_empty() {
            continue;
        }
        let p = String::from_utf8_lossy(path);
        for pre in prefixes {
            if p.starts_with(pre) {
                *hits.entry(pre).or_insert(0) += 1;
                break;
            }
        }
    }
    if hits.is_empty() {
        return;
    }
    let total: usize = hits.values().sum();
    eprintln!(
        "\nwarning: {total} tracked file(s) match engine runtime dirs and should NOT be committed:"
    );
    for (pre, n) in &hits {
        eprintln!("    {pre} ({n} file(s))");
    }
    eprintln!("  To untrack (history retained, files stay on disk):");
    for pre in hits.keys() {
        eprintln!("    git rm -r --cached {}", pre.trim_end_matches('/'));
    }
    eprintln!("  Then commit the removal. (`Pool/` is legacy — migrate it to `.pool/` first.)\n");
}

// ─── kit-add ─────────────────────────────────────────────────────

/// Add an optional kit to the repo. Validates `scope: optional`, installs
/// scaffold via the drift-handler, creates class folders + templates, and
/// records the kit in `repo.yml`'s `optional_kits:` list.
fn cmd_kit_add(kit_spec: String) {
    let root = find_git_root().expect("not in a git repo");
    let lex_dir = root.join(".lex");
    if !lex_dir.exists() {
        eprintln!("Not a git-lex repo. Run 'git lex init' first.");
        exit(1);
    }
    let (org, repo, _) = resolve_kit_spec(&kit_spec);
    let canonical_spec = format!("{}/{}", org, repo);

    // Refuse to re-add an already-installed kit; the right move is kit-update.
    let already: Vec<String> = read_repo_yml_optional_kits(&lex_dir.join("repo.yml"));
    let already_present = already.iter()
        .any(|s| {
            let (o, r, _) = resolve_kit_spec(s);
            o == org && r == repo
        });
    if already_present {
        eprintln!("Kit '{}' is already installed. Use `git lex kit-update {}` to refresh it.", canonical_spec, canonical_spec);
        exit(1);
    }

    // Also refuse if it's the domain or base kit — those install via init,
    // not kit-add.
    if canonical_spec == BASE_KIT {
        eprintln!("Kit '{}' is the base kit — installed implicitly by `git lex init`. Cannot kit-add.", canonical_spec);
        exit(1);
    }
    if let Some(domain) = read_domain_kit_from_repo_yml(&root) {
        let (d_org, d_repo, _) = resolve_kit_spec(&domain);
        if d_org == org && d_repo == repo {
            eprintln!("Kit '{}' is this repo's domain kit. Cannot kit-add a domain kit.", canonical_spec);
            exit(1);
        }
    }

    println!("Fetching '{}' from GitHub...", canonical_spec);
    let kit_dir = match fetch_and_validate_optional_kit(&canonical_spec) {
        KitFetchOutcome::Ready(p) => p,
        KitFetchOutcome::FetchFailed => {
            eprintln!("Failed to fetch kit '{}' from GitHub.", canonical_spec);
            eprintln!("Check that https://github.com/{}/{} exists and is reachable.", org, repo);
            exit(1);
        }
        KitFetchOutcome::ScopeMismatch(found_scope) => {
            eprintln!(
                "Kit '{}' has scope `{:?}`, not `Optional`. Use `git lex init --kit {}` for a domain kit.",
                canonical_spec, found_scope, canonical_spec
            );
            // Leave the fetched dir for inspection but back out of the install.
            exit(1);
        }
    };
    println!("Kit fetched at {}.", kit_dir.strip_prefix(&root).unwrap_or(&kit_dir).display());

    // Install scaffold (drift-aware). For a new optional kit nothing should
    // exist locally yet, so this is almost entirely fresh-install — but if
    // the agent has already hand-authored folders matching the kit's class
    // names, the drift-handler will surface that as `.kit-latest` siblings.
    let report = install_scaffold_files_from_skip_existing(&kit_dir, false);
    if report.installed > 0 || report.skipped > 0 || !report.drifted.is_empty() {
        println!(
            "Scaffold: {} file(s) installed, {} unchanged",
            report.installed, report.skipped
        );
        if !report.drifted.is_empty() {
            println!("Drift: {} file(s) differ from kit:", report.drifted.len());
            for path in &report.drifted {
                println!("  {} (see {}.kit-latest)", path, path);
            }
        }
    }

    // Regenerate derived artifacts for this kit. create_folders=true so the
    // class folders show up on disk immediately — lux's call: discoverability.
    println!("Regenerating artifacts for '{}/{}'...", org, repo);
    regenerate_kit_artifacts(&canonical_spec, &root, true);

    // Record in repo.yml.
    let repo_yml = lex_dir.join("repo.yml");
    if let Err(e) = append_optional_kit(&repo_yml, &canonical_spec) {
        eprintln!("Warning: failed to update .lex/repo.yml: {}", e);
        eprintln!("The kit is installed but won't be tracked by `git lex kit-update`.");
        eprintln!("Add this line manually under `optional_kits:`:");
        eprintln!("  - {}", canonical_spec);
    } else {
        println!("Recorded '{}' under optional_kits in .lex/repo.yml.", canonical_spec);
    }

    // Register the kit's hooks (and reap any orphans) in the substrate config.
    // install_scaffold_files_from_skip_existing above copies the hook *files*
    // to .claude/hooks/, but a hook does nothing until it's registered under
    // its event in settings.json. setup_substrate_claude is that pass — same
    // one kit-update runs. Without this, kit-add lands the files but Claude
    // Code never fires them (the pool-kit gap, Day 50). Identity is per-repo,
    // not per-kit, so this re-derives the whole hook set from all installed
    // kits — exactly the convergent behavior we want.
    if let Some(agent_name) = read_agent_name(&root) {
        for substrate in harness::active_substrates(&root) {
            match substrate {
                harness::Substrate::Claude => setup_substrate_claude(&root, &agent_name),
                harness::Substrate::Hermes | harness::Substrate::Gemini => {}
            }
        }
    }

    println!("Kit '{}' added.", canonical_spec);

    // t-box: the new kit's ontology joins the persistent ontology graph.
    {
        let store = open_or_create_store();
        let n = crate::nquad::load_ontology_graph(&store);
        println!("Ontology graph: {} kit ttl file(s) loaded", n);
    }
}

// ─── kit-remove ──────────────────────────────────────────────────

/// Remove an optional kit. Scrubs from repo.yml's optional_kits list and
/// deletes `.lex/kit/{org}/{repo}/`. Asks before deleting content folders
/// (e.g. `Innerworld/`) unless --force.
fn cmd_kit_remove(kit_spec: String, force: bool) {
    let root = find_git_root().expect("not in a git repo");
    let lex_dir = root.join(".lex");
    if !lex_dir.exists() {
        eprintln!("Not a git-lex repo. Run 'git lex init' first.");
        exit(1);
    }
    let (org, repo, _) = resolve_kit_spec(&kit_spec);
    let canonical_spec = format!("{}/{}", org, repo);

    // Refuse to remove the base or domain kit.
    if canonical_spec == BASE_KIT {
        eprintln!("Cannot remove the base kit.");
        exit(1);
    }
    if let Some(domain) = read_domain_kit_from_repo_yml(&root) {
        let (d_org, d_repo, _) = resolve_kit_spec(&domain);
        if d_org == org && d_repo == repo {
            eprintln!("Cannot remove the domain kit ('{}'). To switch domain kits, re-init.", canonical_spec);
            exit(1);
        }
    }

    // Verify it's in the optional_kits list. If not, nothing to do — but
    // still try to remove the on-disk dir in case of a half-removed state.
    let in_optionals = read_repo_yml_optional_kits(&lex_dir.join("repo.yml"))
        .iter()
        .any(|s| {
            let (o, r, _) = resolve_kit_spec(s);
            o == org && r == repo
        });
    if !in_optionals {
        eprintln!("Kit '{}' is not in optional_kits. Nothing to remove.", canonical_spec);
        exit(0);
    }

    // Identify the kit's content folder for the prompt. read folder_base
    // from the kit's kit.yml before we delete the install dir.
    let folder_base = kit_config_str(&canonical_spec, "folder base");
    let kit_types = get_kit_types(&canonical_spec);

    // Prompt before deleting content folders.
    let content_dir = folder_base.as_ref().map(|b| root.join(b));
    let content_exists = content_dir.as_ref().map(|p| p.exists()).unwrap_or(false);
    let mut delete_content = false;
    if content_exists {
        if force {
            delete_content = true;
        } else {
            eprint!(
                "Kit '{}' has a content folder at `{}/` with {} class folder(s). \
                 Delete it (with all your authored content inside)? [y/N] ",
                canonical_spec,
                folder_base.as_deref().unwrap_or("?"),
                kit_types.len()
            );
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).unwrap_or_default();
            let input = input.trim().to_lowercase();
            delete_content = input == "y" || input == "yes";
        }
    }

    // Scrub repo.yml.
    let repo_yml = lex_dir.join("repo.yml");
    if let Err(e) = remove_optional_kit(&repo_yml, &canonical_spec) {
        eprintln!("Failed to update repo.yml: {}", e);
        exit(1);
    }

    // Delete the kit install dir.
    if let Err(e) = remove_kit_install_dir(&canonical_spec) {
        eprintln!("Warning: failed to delete .lex/kit/{}/{}/: {}", org, repo, e);
    }

    // Delete content folder if confirmed.
    if delete_content {
        if let Some(cd) = content_dir {
            if let Err(e) = fs::remove_dir_all(&cd) {
                eprintln!("Warning: failed to delete content folder '{}': {}",
                    cd.strip_prefix(&root).unwrap_or(&cd).display(), e);
            } else {
                println!("Deleted content folder '{}/'.", folder_base.as_deref().unwrap_or("?"));
            }
        }
    } else if content_exists {
        println!("Content folder '{}/' kept on disk (you said no).", folder_base.as_deref().unwrap_or("?"));
    }

    println!("Kit '{}' removed.", canonical_spec);
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

    // ---- ensure_engine_gitignore: the .pool/.copia/.weave runtime-dir push ----

    fn tmp_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gitlex-engine-ignore-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn engine_gitignore_appends_to_existing_blocklist() {
        let dir = tmp_repo("append");
        fs::write(dir.join(".gitignore"), ".lex/oxigraph/\ncustom/\n").unwrap();
        ensure_engine_gitignore(&dir);
        let got = fs::read_to_string(dir.join(".gitignore")).unwrap();
        // Original lines preserved.
        assert!(got.contains(".lex/oxigraph/"), "must keep existing entries");
        assert!(got.contains("custom/"));
        // Engine dirs added under the sentinel.
        assert!(got.contains(ENGINE_GITIGNORE_BEGIN));
        assert!(got.contains(".pool/"));
        assert!(got.contains(".copia/"));
        assert!(got.contains(".weave/"));
        assert!(got.contains(ENGINE_GITIGNORE_END));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn engine_gitignore_is_idempotent() {
        let dir = tmp_repo("idempotent");
        fs::write(dir.join(".gitignore"), ".lex/oxigraph/\n").unwrap();
        ensure_engine_gitignore(&dir);
        let once = fs::read_to_string(dir.join(".gitignore")).unwrap();
        ensure_engine_gitignore(&dir);
        ensure_engine_gitignore(&dir);
        let thrice = fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(once, thrice, "re-running must not duplicate the block");
        // Exactly one sentinel pair.
        assert_eq!(thrice.matches(ENGINE_GITIGNORE_BEGIN).count(), 1);
        assert_eq!(thrice.matches(ENGINE_GITIGNORE_END).count(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn engine_gitignore_replaces_block_in_place_on_dir_change() {
        let dir = tmp_repo("replace");
        // Simulate an OLD managed block missing a future dir (e.g. only .pool/).
        let old = format!(
            "keepme/\n\n{}\n.pool/\n{}\ntail/\n",
            ENGINE_GITIGNORE_BEGIN, ENGINE_GITIGNORE_END
        );
        fs::write(dir.join(".gitignore"), &old).unwrap();
        ensure_engine_gitignore(&dir);
        let got = fs::read_to_string(dir.join(".gitignore")).unwrap();
        // The block is rewritten in place (still one pair), now with all dirs,
        // and the surrounding non-managed lines are untouched.
        assert_eq!(got.matches(ENGINE_GITIGNORE_BEGIN).count(), 1);
        assert!(got.contains(".copia/") && got.contains(".weave/"));
        assert!(got.contains("keepme/"), "content before the block is preserved");
        assert!(got.contains("tail/"), "content after the block is preserved");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn engine_gitignore_creates_file_when_absent() {
        let dir = tmp_repo("create");
        // No .gitignore at all.
        ensure_engine_gitignore(&dir);
        let got = fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(got.contains(".pool/") && got.contains(".copia/") && got.contains(".weave/"));
        assert_eq!(got.matches(ENGINE_GITIGNORE_BEGIN).count(), 1);
        fs::remove_dir_all(&dir).ok();
    }
}
