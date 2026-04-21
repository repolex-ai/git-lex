use clap::{Parser, Subcommand};
use oxigraph::io::RdfFormat;
use oxigraph::model::*;
use oxigraph::store::Store;
use std::io::Cursor;
use std::path::PathBuf;
use std::process::{Command, exit};
use std::time::Instant;
use std::fs;
use std::collections::{HashMap, HashSet};
use tree_sitter;

// Shared utilities (also used by git-lex-serve)
use git_lex::{find_git_root, store_path, get_kit,
              resolve_kit_spec, kit_install_dir_for_spec, add_prefixes};

// Frontmatter ObjectProperty value resolver. The rules for what is and isn't
// allowed in frontmatter values are codified as tests in this module — read
// the test suite for the definitive spec.
mod resolve;
mod harness;
mod git;
mod nquad;
mod ontology;
mod shacl;
mod kit;
mod extraction;

use crate::git::{auto_commit_snapshot, base_uri, get_repo_id};
use crate::nquad::{build_slug_path_indexes, compile_extraction_log, emit_spo_line_nquads,
                   generate_frontmatter_nquads, generate_git_nquads,
                   load_lex_nquads, nq_escape, uri_encode_path};
use crate::ontology::{get_kit_prefix_name, get_kit_types,
                      get_object_properties, get_property_datatypes,
                      load_ontology_tboxes};
use crate::shacl::{build_shacl_shapes, parse_shacl_hints};
use crate::extraction::{extract_jsonl_sessions, extract_markdown_links, frontmatter_to_turtle,
                        sanitize_uri_segment, short_hash};
use crate::kit::{collect_init_variables, fetch_kit_from_github, install_scaffold_files_from,
                 install_scaffold_files_from_skip_existing,
                 kit_config_bool, kit_config_str, read_repo_yml_fields};

// .spo event stream — git-aware change detector for .spo sidecars. Used by
// orphan cleanup (pre-commit hook), history graph ingest (rebuild +
// incremental), and the `git lex history-spike` debug subcommand. Imported
// 2026-04-11 from the w4r3z/history-spike branch (was src/history_spike.rs).
// See Situation/2026-04-09-history-graph-temporal-ledger.md §11 for the
// phase plan this module is the foundation for.
mod spo_events;

#[derive(Parser)]
#[command(name = "git-lex", about = "Git extensions for knowledge graphs")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum LlmCommands {
    /// List files needing LLM extraction (new, changed, fresh)
    List,
    /// Extract entities and relationships from a file (two-step)
    Extract {
        /// File path to extract
        file: String,
        /// Model identifier for the .spo filename (e.g., claude-haiku-4-5-20251001)
        #[arg(long)]
        model: String,
    },
    /// Re-check extraction after file changed (uses old extraction + diff)
    Recheck {
        /// File path to recheck
        file: String,
        /// Model identifier for the .spo filename
        #[arg(long)]
        model: String,
    },
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
        /// Dev mode: skip the GitHub fetch and use the kit already at
        /// .lex/kit/{org}/{repo}/. Preserves .lex/ state across re-init so
        /// the kit you are developing is not nuked. Regenerates SHACL
        /// shapes and class templates from the local kit TTL.
        #[arg(long)]
        dev: bool,
    },
    /// Query the knowledge graph.
    ///
    /// By default, queries act on the union of all named graphs, meaning
    /// `SELECT * WHERE { ?s ?p ?o }` will find everything across commits, files,
    /// and extracted metrics automatically.
    ///
    /// Examples:
    ///   git lex query "SELECT * WHERE { ?s ?p ?o } LIMIT 10"
    ///   git lex query "SELECT ?file WHERE { ?file git:language 'markdown' }"
    Query {
        /// The SPARQL query string
        query: String,
    },
    /// Extract frontmatter from .md files → write .spo sidecars + compile log
    Extract,
    /// Validate documents against SHACL shapes from the kit ontology
    Validate,
    /// Dump all generated N-Quads to stdout (debug)
    Dump,
    /// Resolve extraction log into RDF N-Quads (mechanical transformation)
    Resolve {
        /// Rebuild from scratch instead of diffing
        #[arg(long)]
        full: bool,
    },
    /// Sync git data + .lex/*.nq into the persistent store
    Sync,
    /// LLM agent tools
    Llm {
        #[command(subcommand)]
        command: LlmCommands,
    },
    /// Create a new document from the kit ontology
    Create {
        /// Document type (e.g., decision, agent, task)
        doctype: String,
        /// Title for the document
        #[arg(long)]
        title: Option<String>,
    },
    /// Save changes (add + commit + sync in one command)
    Save {
        /// Commit message
        #[arg(default_value = "git lex save")]
        message: String,
    },
    /// Show status of .lex/ in the current repo
    Status,
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
    /// Remove .lex/ entirely. Content files and git history are preserved.
    Nuke,
    /// Re-download and reinstall the kit without touching content or extractions
    KitUpdate {
        /// Kit to update (e.g., repolex-ai/git-lex-kit-squad). If omitted,
        /// uses the kit from .lex/repo.yml.
        kit: Option<String>,
        /// Dev mode: use the kit already at .lex/kit/{org}/{repo}/ instead
        /// of fetching from GitHub. Regenerates shapes and templates.
        #[arg(long)]
        dev: bool,
        /// Force reinstall scaffold files even if they already exist in the
        /// repo. Without this, kit-update only installs scaffold files that
        /// are missing — preserving any local customizations. Use this when
        /// developing a kit and you want the latest shipped files to
        /// overwrite whatever is currently in the repo.
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
    /// Start servers (viz, listen). Delegates to git-lex-serve.
    Serve {
        /// Arguments passed through to git-lex-serve
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// (dev) Walk git history and print .spo line additions/removals per commit
    HistorySpike {
        /// Limit to the most recent N commits (0 = all)
        #[arg(long, default_value = "0")]
        limit: usize,
        /// Only show commits that actually touched .spo files
        #[arg(long)]
        only_changes: bool,
        /// Collapse extraction-id hash-prefix churn (drop first field when deduping)
        #[arg(long)]
        dedup: bool,
        /// Write inconsistency reports to this file (default: stderr)
        #[arg(long)]
        inconsistency_log: Option<String>,
        /// Print canonical URIs alongside event lines
        #[arg(long)]
        canonical: bool,
    },
    /// Build the history graph: walk git history, diff .spo files per
    /// commit, wrap each change in an RDF 1.2 triple-term annotation,
    /// load into oxigraph. Writes to <base/history> and records the
    /// lastHistorySync marker in <base/meta>.
    HistoryBuild {
        /// Limit to the most recent N commits (0 = all)
        #[arg(long, default_value = "0")]
        limit: usize,
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
}


// ─── git lex init ──────────────────────────────────────────────

// Base ontologies (git.ttl, fm.ttl, lex.ttl) are no longer embedded in the
// binary. They ship in the base kit scaffold at scaffold/.lex/ontology/ and
// are installed to .lex/ontology/ by the scaffold installer during init.
// Kit ontologies are fetched from GitHub at init time — no embedded fallback.

const BASE_KIT: &str = "repolex-ai/git-lex-kit-base";

fn cmd_init(directory: Option<String>, kit: Option<String>, dev: bool) {
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

    // --dev requires --kit: we need to know which kit dir to use locally.
    if dev && kit.is_none() {
        eprintln!("--dev requires --kit to specify which local kit to use.");
        eprintln!("Example: git lex init --kit goodlex/mytestkit --dev");
        exit(1);
    }

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

    // If .lex/ already exists, this is a re-initialization. In dev mode we
    // PRESERVE .lex/ entirely (including the local kit you are developing)
    // and just regenerate derived artifacts. In normal mode we ask the user,
    // then refresh only the kit-derived subdirs (kit/, ontology/). User
    // data (extract/, extraction.log.spo, tickets/) is preserved.
    if lex_dir.exists() && !dev {
        carryover = read_repo_yml_fields(&lex_dir.join("repo.yml"));

        eprint!(
            "This repo is already initialized at {}.\n\
             Re-initializing will refresh the kit and ontology files and overwrite scaffold files.\n\
             Extractions, extraction log, tickets, and repo.yml fields are preserved.\n\
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
    } else if dev && lex_dir.exists() {
        // Dev mode: read existing carryover from repo.yml, but don't nuke.
        carryover = read_repo_yml_fields(&lex_dir.join("repo.yml"));
        println!("Dev mode: preserving .lex/ and regenerating from local kit.");
    }

    // Create .lex/ structure (idempotent — safe in dev mode too)
    fs::create_dir_all(lex_dir.join("extract")).ok();

    // Ontologies are installed from the base kit scaffold (scaffold/.lex/ontology/)
    // by the scaffold installer below — no hardcoded ontology block needed.

    // Install kit(s). Every repo gets the base kit. If --kit specifies a
    // domain kit (squad, soul, etc.), that's installed alongside base in
    // the same .lex/kit/ directory.
    //
    // In dev mode, don't fetch — verify the local kit dir already exists.
    {
        let lex_kit_root = lex_dir.join("kit");
        if !dev {
            let _ = fs::remove_dir_all(&lex_kit_root);
        }

        // Always install base kit (unless dev mode where it may already exist).
        let (base_org, base_repo, _) = resolve_kit_spec(BASE_KIT);
        let base_dir = lex_kit_root.join(&base_org).join(&base_repo);
        if !dev || !base_dir.exists() {
            fs::create_dir_all(&base_dir).ok();
            println!("Downloading base kit {}/{}...", base_org, base_repo);
            if fetch_kit_from_github(BASE_KIT, &base_dir) {
                println!("Base kit installed.");
            } else {
                eprintln!("Failed to fetch base kit from GitHub.");
                eprintln!("Check network access to https://github.com/{}/{}", base_org, base_repo);
                exit(1);
            }
        }

        // Install the domain kit (if different from base).
        let kit_dir = lex_kit_root.join(&org).join(&repo);
        if kit_spec != format!("{}/{}", base_org, base_repo) {
            if dev {
                if !kit_dir.exists() {
                    eprintln!("--dev: kit directory not found at {}", kit_dir.display());
                    eprintln!("Populate the kit locally first, then re-run with --dev.");
                    exit(1);
                }
                println!("Dev mode: using local kit at {}", kit_dir.display());
            } else {
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
    }

    // .lex/.gitignore — universal: ignore the local oxigraph store. This is
    // a nested .gitignore scoped to .lex/, so it doesn't pollute the repo
    // root's .gitignore file.
    fs::write(lex_dir.join(".gitignore"), "oxigraph/\n").ok();

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

    // repo.yml — create if missing, otherwise update the kit: field to
    // match the spec passed to this init run. This matters for dev mode
    // and for re-initialization: if the user ran init once without --kit
    // and then runs again with --kit X, the kit: field needs to change
    // from "none" to the new spec.
    let repo_yml_path = lex_dir.join("repo.yml");
    if !repo_yml_path.exists() {
        let repo_name = root.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let today = Command::new("date").args(["+%Y-%m-%d"]).output().ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        fs::write(&repo_yml_path, format!(
            "name: {}\nkit: {}\ncreated: {}\n",
            repo_name, kit_spec, today
        )).ok();
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
            fs::write(&repo_yml_path, content).ok();
        }
    }

    // README
    let readme_path = lex_dir.join("README.md");
    if !readme_path.exists() {
        fs::write(&readme_path, format!(
            "# .lex/\n\nKnowledge graph managed by git-lex.\nKit: {}\n\n\
             - `extract/` — extraction sidecars (.spo)\n\
             - `ontology/` — ontology definitions\n\
             - `oxigraph/` — local SPARQL store (gitignored)\n",
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

    // Create type folders from kit ontology.
    // Reads from kit.yml: "install folders", "folder base", "folder ontology".
    // Falls back to legacy "createTypeFolders" for pre-migration kits.
    {
        let create_folders = kit_config_bool(kit_name, "install folders", false)
            || kit_config_bool(kit_name, "createTypeFolders", false);
        let folder_base = kit_config_str(kit_name, "folder base");
        let kit_types = get_kit_types(kit_name);
        if create_folders {
            for (type_name, _) in &kit_types {
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
            }
            if !kit_types.is_empty() {
                let type_names: Vec<String> = kit_types.iter().map(|(n, _)| n.clone()).collect();
                let prefix = folder_base.as_deref().unwrap_or("");
                if prefix.is_empty() {
                    println!("Created type folders: {}", type_names.join(", "));
                } else {
                    println!("Created type folders: {}/{{{}}}", prefix, type_names.join(", "));
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
                doc.push_str("git lex status            # Show extraction status\n");
                doc.push_str("```\n\n");

                doc.push_str("## Commands\n\n");
                doc.push_str("| Command | What it does |\n");
                doc.push_str("|---|---|\n");
                doc.push_str(&format!("| `git lex create <type>` | Scaffold a new document. Valid types: {} |\n", type_names.join(", ")));
                doc.push_str("| `git lex save \"msg\"` | Stage all changes, commit, extract frontmatter |\n");
                doc.push_str("| `git lex sync` | Build the SPARQL knowledge graph from git + extractions |\n");
                doc.push_str("| `git lex query \"...\"` | Run a SPARQL query against the knowledge graph |\n");
                doc.push_str("| `git lex status` | Show which files have been extracted |\n");
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
                doc.push_str("Your content here. Use @mentions and [[wikilinks]] for relationships.\n");
                doc.push_str("```\n\n");
                doc.push_str("See `__ClassName.md` files in each folder for available properties and SHACL-derived constraints.\n\n");

                doc.push_str("## @mentions and [[wikilinks]]\n\n");
                doc.push_str("Reference other agents and documents naturally in your text:\n\n");
                doc.push_str("- `@agentname` — creates a `lex:mentions` relationship\n");
                doc.push_str("- `[[document-title]]` — creates a `lex:linksTo` relationship\n\n");
                doc.push_str("These are extracted automatically from document bodies and commit messages.\n\n");

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

    // Generate SHACL shapes from ontology, then class templates
    if let Some(shapes_path) = build_shacl_shapes(kit_name) {
        println!("SHACL shapes generated: {}", shapes_path.file_name().unwrap_or_default().to_string_lossy());
    }
    {
        let kit_types = get_kit_types(kit_name);
        let shapes_content = {
            let r = find_git_root().unwrap();
            let (_, _, short) = resolve_kit_spec(kit_name);
            let shapes_path = r.join(".lex").join("ontology").join(&short).join(format!("{}-shapes.ttl", short));
            fs::read_to_string(&shapes_path).unwrap_or_default()
        };
        let shacl_hints = parse_shacl_hints(&shapes_content);

        let tmpl_folder_base = kit_config_str(kit_name, "folder base");
        for (type_name, properties) in &kit_types {
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
            fs::write(&repo_yml_path, &updated).ok();
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
        // Currently: Claude Code (via settings.local.json env block).
        // Future: Gemini, OpenAI Codex, etc. — stub those as needed after
        // researching how each model's CLI handles per-project identity.

        let agent_name = vars.get("agent_name").cloned().unwrap_or_default();
        if !agent_name.is_empty() {
            // Claude Code: write git identity env vars into
            // .claude/settings.local.json (gitignored, per-machine).
            // These get injected into every Bash tool call automatically.
            setup_substrate_claude(&root, &agent_name);

            // TODO: Gemini (Aistudio CLI / Project IDX)
            // Needs research on how Gemini's agent substrate handles
            // per-project env injection. Likely a similar local config
            // file, but the path and format are different.
            // setup_substrate_gemini(&root, &agent_name);

            // TODO: OpenAI (Codex CLI)
            // Same — needs research on the OpenAI agent substrate's
            // local config mechanism.
            // setup_substrate_openai(&root, &agent_name);
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

    // Pre-commit hook: extract changed files → write sidecars → stage
    let hooks_dir = root.join(".git").join("hooks");
    fs::create_dir_all(&hooks_dir).ok();

    let pre_commit_path = hooks_dir.join("pre-commit");
    let pre_commit_content = "#!/bin/sh\ngit-lex extract\ngit add .lex/extract/ 2>/dev/null\ngit-lex validate || exit 1\n";
    if !pre_commit_path.exists() || !fs::read_to_string(&pre_commit_path).unwrap_or_default().contains("git-lex extract") {
        fs::write(&pre_commit_path, pre_commit_content).ok();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&pre_commit_path, fs::Permissions::from_mode(0o755)).ok();
        }
    }
    println!("Installed pre-commit hook (extract on commit)");

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
                let _ = Command::new("git").args(["commit", "-m", "Initial content"]).status();
                println!("Committed existing content.");
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
            fs::write(&repo_yml_path, &updated).ok();
            let _ = Command::new("git").args(["add", ".lex/repo.yml"]).status();
            let _ = Command::new("git").args(["commit", "-m", "git lex identity"]).status();
            println!("Identity: {}", first_sha);
        }
    }
}


// ─── git lex status ────────────────────────────────────────────

fn cmd_status() {
    let root = match find_git_root() {
        Some(r) => r,
        None => {
            eprintln!("fatal: not a git repository (or any parent up to mount point /)");
            exit(1);
        }
    };

    let lex_dir = root.join(".lex");

    if !lex_dir.exists() {
        println!("No .lex/ directory found.");
        println!("Run 'git lex init' to initialize.");
        return;
    }

    println!("git-lex status for {}", root.display());
    println!();

    for subdir in &["graph", "ontology"] {
        let dir = lex_dir.join(subdir);
        if dir.exists() {
            let count = fs::read_dir(&dir)
                .map(|entries| entries.filter_map(|e| e.ok()).count())
                .unwrap_or(0);
            println!("  .lex/{}/  — {} files", subdir, count);
        } else {
            println!("  .lex/{}/  — (not created)", subdir);
        }
    }

    // Document lexification status
    // Get file list with blob hashes
    let output = Command::new("git")
        .args(["ls-tree", "-r", "--format=%(objectname)\t%(path)", "HEAD"])
        .output();
    if let Ok(o) = output {
        if o.status.success() {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let docs: Vec<(&str, &str)> = stdout
                .lines()
                .filter_map(|line| {
                    let parts: Vec<&str> = line.splitn(2, '\t').collect();
                    if parts.len() < 2 { return None; }
                    let (hash, path) = (parts[0], parts[1]);
                    let pl = path.to_lowercase();
                    if (pl.ends_with(".md") || pl.ends_with(".txt"))
                        && !pl.starts_with(".lex/")
                        && !pl.starts_with(".git") {
                        Some((hash, path))
                    } else {
                        None
                    }
                })
                .collect();

            if !docs.is_empty() {
                let lex_nq = load_lex_nquads();

                let mut lexified = Vec::new();
                let mut stale = Vec::new();
                let mut unlexified = Vec::new();

                for (hash, path) in &docs {
                    let path_mentioned = lex_nq.contains(path);
                    let blob_mentioned = lex_nq.contains(hash);

                    if path_mentioned && blob_mentioned {
                        lexified.push(*path);
                    } else if path_mentioned && !blob_mentioned {
                        stale.push(*path);
                    } else {
                        unlexified.push(*path);
                    }
                }

                println!();
                println!("  Documents:");
                for doc in &lexified {
                    println!("    {}  — lexified", doc);
                }
                for doc in &stale {
                    println!("    {}  — stale (content changed since lexification)", doc);
                }
                for doc in &unlexified {
                    println!("    {}  — unlexified", doc);
                }
                println!();
                let total = lexified.len() + stale.len() + unlexified.len();
                println!("  {}/{} lexified, {} stale", lexified.len(), total, stale.len());
            }
        }
    }
}

// ─── git lex query ─────────────────────────────────────────────


/// Get the persistent store path.
// store_path and open_store_read_only imported from git_lex lib

/// Open the persistent store, or None if it doesn't exist.
fn open_store() -> Option<Store> {
    let path = store_path()?;
    if path.exists() {
        Store::open(&path).ok()
    } else {
        None
    }
}

/// Create or open the persistent store.
fn open_or_create_store() -> Store {
    let path = store_path().expect("not in a git repo");
    fs::create_dir_all(&path).expect("failed to create .lex/oxigraph/");
    Store::open(&path).expect("failed to open store")
}


// ─── git lex llm ───────────────────────────────────────────────

fn cmd_llm_list() {
    let root = match find_git_root() {
        Some(r) => r,
        None => {
            eprintln!("fatal: not a git repository");
            exit(1);
        }
    };

    let repo = match git2::Repository::discover(".") {
        Ok(r) => r,
        Err(_) => {
            eprintln!("fatal: cannot open git repository");
            exit(1);
        }
    };

    // Get current file list with blob hashes from index
    let index = repo.index().expect("failed to read index");
    let mut current_files: HashMap<String, String> = HashMap::new();
    for entry in index.iter() {
        let path = String::from_utf8_lossy(&entry.path).to_string();
        let pl = path.to_lowercase();
        if (pl.ends_with(".md") || pl.ends_with(".txt"))
            && !pl.starts_with(".lex/")
            && !pl.starts_with(".git")
        {
            let hash = entry.id.to_string();
            let short_hash = hash[..8.min(hash.len())].to_string();
            current_files.insert(path, short_hash);
        }
    }

    // Check which files have .llm.spo sidecars and what blob hash they contain
    let extract_dir = root.join(".lex").join("extract");
    let mut new_files = Vec::new();
    let mut changed_files = Vec::new();
    let mut fresh_files = Vec::new();

    for (path, current_hash) in &current_files {
        // Check for any non-fm .spo sidecar for this file
        let spo_dir = extract_dir.join(std::path::Path::new(path).parent().unwrap_or(std::path::Path::new("")));
        let fname = std::path::Path::new(path).file_name().unwrap_or_default().to_string_lossy();
        let has_llm_spo = spo_dir.exists() && fs::read_dir(&spo_dir)
            .map(|entries| entries.filter_map(|e| e.ok()).any(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.starts_with(&format!("{}.", fname)) && n.ends_with(".spo") && !n.ends_with(".fm.spo")
            }))
            .unwrap_or(false);
        let spo_path = if has_llm_spo {
            // Find the actual spo file to check blob hash
            fs::read_dir(&spo_dir)
                .ok()
                .and_then(|entries| entries.filter_map(|e| e.ok()).find(|e| {
                    let n = e.file_name().to_string_lossy().to_string();
                    n.starts_with(&format!("{}.", fname)) && n.ends_with(".spo") && !n.ends_with(".fm.spo")
                }))
                .map(|e| e.path())
                .unwrap_or_else(|| extract_dir.join("nonexistent"))
        } else {
            extract_dir.join("nonexistent")
        };
        if !has_llm_spo {
            new_files.push(path.as_str());
        } else {
            // Check extraction log for this file's current blob hash
            let log_content = fs::read_to_string(&root.join(".lex").join("extraction.log.spo")).unwrap_or_default();
            let file_id_prefix = format!("{}/{}", current_hash, path);
            // If the current hash appears in the log, extraction is fresh
            // If a different hash appears for this path, it's changed
            let has_current = log_content.lines().any(|l| l.starts_with(&file_id_prefix));
            if has_current {
                fresh_files.push(path.as_str());
            } else {
                changed_files.push(path.as_str());
            }
        }
    }

    new_files.sort();
    changed_files.sort();
    fresh_files.sort();

    if !new_files.is_empty() {
        println!("New ({} files — never extracted):", new_files.len());
        for f in &new_files {
            println!("  {}", f);
        }
        println!();
    }

    if !changed_files.is_empty() {
        println!("Changed ({} files — blob hash differs, needs re-extraction):", changed_files.len());
        for f in &changed_files {
            println!("  {}", f);
        }
        println!();
    }

    if !fresh_files.is_empty() {
        println!("Fresh ({} files — up to date):", fresh_files.len());
        for f in &fresh_files {
            println!("  {}", f);
        }
        println!();
    }

    println!("Summary: {} new, {} changed, {} fresh", new_files.len(), changed_files.len(), fresh_files.len());
}

fn cmd_llm_extract(file: &str, model: &str) {
    let root = match find_git_root() {
        Some(r) => r,
        None => {
            eprintln!("fatal: not a git repository");
            exit(1);
        }
    };

    // Read the file content
    let filepath = root.join(file);
    let content = match fs::read_to_string(&filepath) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Cannot read {}: {}", file, e);
            exit(1);
        }
    };

    let spo_path = format!(".lex/extract/{}.{}.spo", file, model);

    println!("Extract entities and relationships from this document.");
    println!();
    println!("Step 1: Identify all entities (things, concepts, technologies, people, systems, components).");
    println!("Step 2: For those entities, output triples in this format, one per line:");
    println!("  subject | predicate | object");
    println!();
    println!("Include: isA (type), properties (attributes), relationships between entities.");
    println!("Use lowercase-with-dashes for names. Stay grounded to the actual text.");
    println!("Sort output alphabetically by subject, then predicate, then object.");
    println!();
    println!("--- FILE: {} ---", file);
    println!("{}", content);
    println!("--- END FILE ---");
    println!();
    println!("Write the output to: {}", spo_path);
}

fn cmd_llm_recheck(file: &str, model: &str) {
    let root = match find_git_root() {
        Some(r) => r,
        None => {
            eprintln!("fatal: not a git repository");
            exit(1);
        }
    };

    // Get current blob hash
    let repo = git2::Repository::discover(".").expect("failed to open repo");
    let index = repo.index().expect("failed to read index");
    let entry = index.get_path(std::path::Path::new(file), 0);
    let blob_hash = match entry {
        Some(e) => {
            let hash = e.id.to_string();
            hash[..8.min(hash.len())].to_string()
        }
        None => {
            eprintln!("File not found in git index: {}", file);
            exit(1);
        }
    };

    let file_id = format!("{}/{}", blob_hash, file);

    // Read old extraction
    let spo_path = root.join(".lex").join("extract").join(format!("{}.{}.spo", file, model));
    let old_extraction = fs::read_to_string(&spo_path).unwrap_or_default();

    if old_extraction.is_empty() {
        eprintln!("No existing extraction for {} by {}. Use 'git lex llm extract' instead.", file, model);
        exit(1);
    }

    // Read the current file content
    let filepath = root.join(file);
    let content = fs::read_to_string(&filepath).unwrap_or_default();

    // Get the diff
    let diff_output = Command::new("git")
        .args(["diff", "HEAD", "--", file])
        .output();

    let diff = diff_output
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    println!("Re-check extraction after file change.");
    println!();
    println!("Previous extraction:");
    println!("{}", old_extraction);
    println!();
    if !diff.is_empty() {
        println!("Changes since last extraction:");
        println!("{}", diff);
    } else {
        println!("--- FILE: {} ---", file);
        println!("{}", content);
        println!("--- END FILE ---");
    }
    println!();
    println!("Update the triples. Keep unchanged ones, add/remove/modify based on changes.");
    println!("Sort output alphabetically by subject, then predicate, then object.");
    println!();
    println!("Write the output to: .lex/extract/{}.{}.spo", file, model);
}

// ─── git lex resolve ────────────────────────────────────────────

fn cmd_resolve(full: bool) {
    let start = Instant::now();

    let root = match find_git_root() {
        Some(r) => r,
        None => {
            eprintln!("fatal: not a git repository");
            exit(1);
        }
    };

    let base = base_uri();
    let log_path = root.join(".lex").join("extraction.log.spo");
    let knowledge_path = root.join(".lex").join("graph").join("knowledge.nq");

    // Get current commit hash for named graph
    let commit_hash = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let graph = format!("<{}/commit/{}>", base, commit_hash);

    // Read current extraction log
    let current_log = fs::read_to_string(&log_path).unwrap_or_default();
    let current_lines: HashSet<&str> = current_log.lines().filter(|l| !l.is_empty()).collect();

    // Read previous version (from last commit) for diff
    let previous_log = if full {
        String::new() // Full rebuild — treat everything as new
    } else {
        Command::new("git")
            .args(["show", "HEAD~1:.lex/extraction.log.spo"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default()
    };
    let previous_lines: HashSet<&str> = previous_log.lines().filter(|l| !l.is_empty()).collect();

    // Compute diff
    let new_lines: Vec<&str> = current_lines.difference(&previous_lines).copied().collect();
    let removed_lines: Vec<&str> = previous_lines.difference(&current_lines).copied().collect();

    if new_lines.is_empty() && removed_lines.is_empty() {
        println!("Nothing to resolve (no changes in extraction log).");
        return;
    }

    let mut nq = String::new();

    // Process new assertions
    for line in &new_lines {
        // Parse: blobhash/filepath | subject | predicate | object
        let parts: Vec<&str> = line.splitn(2, " | ").collect();
        if parts.len() < 2 {
            continue;
        }
        let file_id = parts[0]; // blobhash/filepath
        let spo_part = parts[1]; // subject | predicate | object

        let spo_fields: Vec<&str> = spo_part.splitn(3, " | ").collect();
        if spo_fields.len() < 3 {
            continue;
        }
        let (subject, predicate, object) = (spo_fields[0], spo_fields[1], spo_fields[2]);

        // Extract blob hash and filepath from file_id
        let (blob_hash, filepath) = if let Some(pos) = file_id.find('/') {
            (&file_id[..pos], &file_id[pos + 1..])
        } else {
            (file_id, "")
        };

        // Build entity URIs (sanitized for valid IRI)
        let subject_uri = format!("<{}/entity/{}~{}>", base, sanitize_uri_segment(subject), blob_hash);

        // Handle predicate: isA → rdf:type, hasValue → fm:key, others → unresolved URI
        let predicate_nq = if predicate == "isA" {
            "<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>".to_string()
        } else if predicate == "hasValue" {
            // For frontmatter, the subject IS the key name — use fm: namespace
            format!("<https://repolex.ai/ontology/git-lex/fm/{}>", uri_encode_path(subject))
        } else {
            // Unresolved predicate — wrap in a namespace so it's a valid IRI
            format!("<https://repolex.ai/r/{}/predicate/{}>", get_repo_id(), sanitize_uri_segment(predicate))
        };

        // Handle object based on property type:
        // - isA → literal (class name)
        // - hasValue with -link/-links subject → resolve as entity URI
        // - hasValue otherwise → literal
        // - other predicates → entity from same file
        let object_nq = if predicate == "isA" {
            format!("\"{}\"", nq_escape(object))
        } else if predicate == "hasValue" {
            if subject.ends_with("-link") || subject.ends_with("-links") {
                // Resolve -link values as entity URIs
                let slug = object.trim().trim_start_matches('@').to_lowercase()
                    .replace(' ', "-")
                    .replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '/' && c != '.', "");
                if slug.contains('/') || slug.ends_with(".md") {
                    format!("<{}/file/{}>", base, uri_encode_path(&slug))
                } else if !slug.is_empty() {
                    format!("<{}/entity/{}>", base, uri_encode_path(&slug))
                } else {
                    format!("\"{}\"", nq_escape(object))
                }
            } else {
                format!("\"{}\"", nq_escape(object))
            }
        } else {
            format!("<{}/entity/{}~{}>", base, sanitize_uri_segment(object), blob_hash)
        };

        // Write the assertion triple
        nq.push_str(&format!("{} {} {} {} .\n", subject_uri, predicate_nq, object_nq, graph));

        // Write name triple for subject (if we haven't seen it yet)
        nq.push_str(&format!(
            "{} <https://repolex.ai/ontology/git-lex/lex/name> \"{}\" {} .\n",
            subject_uri, nq_escape(subject), graph
        ));

        // Generate annotation with triple term
        let spo_key = format!("{}|{}|{}|{}", file_id, subject, predicate, object);
        let ann_hash = short_hash(&spo_key);
        let ann_uri = format!("<{}/ann/{}>", base, ann_hash);

        // Triple term annotation
        nq.push_str(&format!(
            "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> <<( {} {} {} )>> {} .\n",
            ann_uri, subject_uri, predicate_nq, object_nq, graph
        ));
        nq.push_str(&format!(
            "{} <https://repolex.ai/ontology/git-lex/git/filePath> \"{}\" {} .\n",
            ann_uri, nq_escape(filepath), graph
        ));
        nq.push_str(&format!(
            "{} <https://repolex.ai/ontology/git-lex/git/blobHash> \"{}\" {} .\n",
            ann_uri, nq_escape(blob_hash), graph
        ));
    }

    // Process retractions
    for line in &removed_lines {
        let parts: Vec<&str> = line.splitn(2, " | ").collect();
        if parts.len() < 2 {
            continue;
        }
        let file_id = parts[0];
        let spo_part = parts[1];

        let spo_fields: Vec<&str> = spo_part.splitn(3, " | ").collect();
        if spo_fields.len() < 3 {
            continue;
        }
        let (subject, predicate, object) = (spo_fields[0], spo_fields[1], spo_fields[2]);

        let spo_key = format!("{}|{}|{}|{}", file_id, subject, predicate, object);
        let ann_hash = short_hash(&spo_key);
        let ann_uri = format!("<{}/ann/{}>", base, ann_hash);

        // Retraction annotation
        nq.push_str(&format!(
            "{} <https://repolex.ai/ontology/git-lex/git/retracted> \"true\"^^<http://www.w3.org/2001/XMLSchema#boolean> {} .\n",
            ann_uri, graph
        ));
    }

    // Write knowledge graph
    fs::create_dir_all(root.join(".lex").join("graph")).ok();
    if full {
        fs::write(&knowledge_path, &nq).expect("failed to write knowledge.nq");
    } else {
        // Append to existing
        let mut existing = fs::read_to_string(&knowledge_path).unwrap_or_default();
        existing.push_str(&nq);
        fs::write(&knowledge_path, &existing).expect("failed to write knowledge.nq");
    }

    let elapsed = start.elapsed();
    let triple_count = nq.lines().filter(|l| !l.is_empty()).count();
    println!(
        "Resolved {} new + {} retracted → {} quads in {:.1}ms",
        new_lines.len(),
        removed_lines.len(),
        triple_count,
        elapsed.as_secs_f64() * 1000.0
    );
    println!("Written to: {}", knowledge_path.display());
}

// ─── git lex create ─────────────────────────────────────────────

// get_kit, resolve_kit_spec, kit_install_dir_for_spec imported from git_lex lib


// ─── Ontology Builder ──────���───────────────────────────────────
// Loads kit TTL into oxigraph, queries OWL constraints, generates SHACL shapes.
// Single source of truth: the TTL. Shapes are derived artifacts.



fn cmd_create(doctype: &str, title: Option<&str>) {
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
            eprintln!("No kit configured. Run 'git lex init --kit <name>' first.");
            exit(1);
        }
    };

    // Find valid types
    let kit_types = get_kit_types(&kit);
    // Match case-insensitively so `git lex create task` and `git lex create Task` both work.
    let doctype_lower = doctype.to_lowercase();
    let matching_type = kit_types.iter().find(|(name, _)| name.to_lowercase() == doctype_lower);

    let (class_name, properties) = match matching_type {
        Some((name, props)) => (name.clone(), props.clone()),
        None => {
            let valid: Vec<String> = kit_types.iter().map(|(n, _)| n.clone()).collect();
            eprintln!(
                "Unknown document type '{}'. Valid types for kit '{}': {}",
                doctype, kit, valid.join(", ")
            );
            exit(1);
        }
    };

    // Generate filename in type-specific folder (folder name matches ontology class exactly)
    let title_str = title.unwrap_or("untitled");
    let slug = title_str
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
        eprintln!("File already exists: {}", display_path);
        exit(1);
    }

    // Auto-generate agent email for Agent type
    let agent_email = format!("{}@lex.local", slug);

    // Build frontmatter — flat dot notation: kit.class.property using the
    // short kit name, not the full org/repo spec.
    let (_, _, short) = resolve_kit_spec(&kit);
    let mut fm = String::new();
    fm.push_str("---\n");

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

        // Auto-fill agentEmail for Agent type
        if prop_name == "agentEmail" && class_name == "Agent" {
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
    fm.push_str(&format!("# {}\n\n", title_str));
    fm.push_str("<!-- Write your content here -->\n");

    fs::write(&filepath, &fm).expect("failed to create document");
    println!("Created: {}", display_path);
    println!("Type: {}:{}", short, class_name);
    if class_name == "Agent" {
        println!("Agent ID: {}", agent_email);
        println!("Use this as your git author: git -c user.email=\"{}\"", agent_email);
    }
    println!("Edit the file, then run 'git lex save' to commit.");
}

// ─── git lex save ──────────────────────────────────────────────

fn cmd_save(message: &str) {
    // Sync skills/subagents into substrate harness.
    // The harness scans for Skill/ and Subagent/ under any namespace folder.
    if let Some(root) = find_git_root() {
        harness::sync(&root, "claude");
    }

    // Add everything, commit, let hooks handle extract + sync
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
        .args(["commit", "-m", message])
        .status();
    match status {
        Ok(s) if s.success() => {
            println!("Saved: {}", message);
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

    // Load SHACL shapes TTL from .lex/ontology/{short}/
    // Shapes are self-contained — no ontology TTL needed for validation.
    let (_, _, short) = resolve_kit_spec(&kit);
    let ontology_dir = root.join(".lex").join("ontology").join(&short);
    let shapes_ttl = {
        let shapes_path = ontology_dir.join(format!("{}-shapes.ttl", short));
        fs::read_to_string(&shapes_path).ok()
    };

    let shapes_ttl = match shapes_ttl {
        Some(s) => s,
        None => {
            println!("No SHACL shapes found for kit '{}' — skipping validation.", kit);
            return true;
        }
    };

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

    // Initialize rudof
    let config = match rudof_lib::RudofConfig::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to create rudof config: {}", e);
            return true;
        }
    };
    let mut rudof = match rudof_lib::Rudof::new(&config) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to create rudof instance: {}", e);
            return true;
        }
    };

    // Load shapes once
    if let Err(e) = rudof.read_shacl(
        &mut shapes_ttl.as_bytes(),
        "shapes",
        Some(&rudof_lib::ShaclFormat::Turtle),
        None,
        Some(&rudof_lib::ReaderMode::Lax),
    ) {
        eprintln!("Failed to load SHACL shapes: {}", e);
        return true;
    }

    let mut total_files = 0;
    let mut total_violations = 0;
    let mut failed_files = Vec::new();

    for filepath in &files {
        let ttl = match frontmatter_to_turtle(filepath, &root, &kit) {
            Some(t) => t,
            None => continue,
        };
        total_files += 1;

        // Reset data, keep shapes cached
        rudof.reset_data();

        if let Err(e) = rudof.read_data(
            &mut ttl.as_bytes(),
            &filepath.to_string_lossy(),
            Some(&rudof_lib::RDFFormat::Turtle),
            None,
            Some(&rudof_lib::ReaderMode::Strict),
            Some(false),
        ) {
            eprintln!("  Parse error in {}: {}", filepath.display(), e);
            continue;
        }

        match rudof.validate_shacl(
            Some(&rudof_lib::ShaclValidationMode::Native),
            Some(&rudof_lib::ShapesGraphSource::CurrentSchema),
        ) {
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
    generate_frontmatter_nquads();

    // Run markdown link extraction via tree-sitter
    extract_markdown_links();

    // Run JSONL extraction for claude-code kit
    extract_jsonl_sessions();

    // Compile the extraction log
    compile_extraction_log();

    let elapsed = start.elapsed();
    eprintln!("Extracted in {:.1}ms", elapsed.as_secs_f64() * 1000.0);
}

fn cmd_sync() {
    let start = Instant::now();

    let root = find_git_root().expect("not a git repo");
    let base = base_uri();
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

    // ─── Phase 1: Clear and regenerate virtual graphs ───
    // Virtual graphs are ephemeral — rebuilt from git every sync.
    // We clear ALL graphs that aren't /sync/ graphs, then reload.
    // Sync graphs are persistent — never touched.

    // Find all existing graph names
    let existing_graphs: Vec<String> = {
        let query = "SELECT DISTINCT ?g WHERE { GRAPH ?g { ?s ?p ?o } }";
        let results = oxigraph::sparql::SparqlEvaluator::new()
            .parse_query(query)
            .ok()
            .and_then(|q| q.on_store(&store).execute().ok());
        match results {
            Some(oxigraph::sparql::QueryResults::Solutions(solutions)) => {
                solutions.filter_map(|s| {
                    s.ok().and_then(|s| {
                        s.get("g").map(|t| match t {
                            Term::NamedNode(n) => n.as_str().to_string(),
                            _ => String::new(),
                        })
                    })
                }).collect()
            }
            _ => Vec::new(),
            None => Vec::new(),
        }
    };

    // Clear non-sync, non-history graphs (virtual graphs get regenerated).
    // History and meta graphs are persistent — managed by Phase 4.
    for graph_uri in &existing_graphs {
        if !graph_uri.contains("/sync/")
            && !graph_uri.ends_with("/history")
            && !graph_uri.ends_with("/meta")
        {
            if let Ok(graph) = oxigraph::model::NamedNode::new(graph_uri) {
                store.clear_graph(&oxigraph::model::GraphName::from(graph)).ok();
            }
        }
    }

    // Load ontology TBoxes (upper + installed kits) into named graphs.
    // Drop-and-replace on every sync — the TTL files on disk are the source
    // of truth, the store should always match. Parse errors fail loudly.
    let tbox_count = load_ontology_tboxes(&store, &root);

    // Regenerate git virtual triples
    let git_nq = generate_git_nquads();
    let git_count = git_nq.lines().count();
    store
        .load_from_reader(RdfFormat::NQuads, Cursor::new(git_nq.as_bytes()))
        .expect("failed to load git triples");

    // Regenerate frontmatter + mention + wikilink triples
    let fm_nq = generate_frontmatter_nquads();
    let fm_count = fm_nq.lines().filter(|l| !l.is_empty()).count();
    if !fm_nq.is_empty() {
        store
            .load_from_reader(RdfFormat::NQuads, Cursor::new(fm_nq.as_bytes()))
            .expect("failed to load frontmatter triples");
    }

    // ─── Phase 2: Sync graph — diff sidecars since last sync ───

    // Find last sync commit (latest /sync/ graph in store)
    let last_sync_commit: Option<String> = {
        let query = format!(
            "SELECT ?g WHERE {{ GRAPH ?g {{ ?s ?p ?o }} FILTER(CONTAINS(STR(?g), '/sync/')) }} ORDER BY DESC(STR(?g)) LIMIT 1"
        );
        let results = oxigraph::sparql::SparqlEvaluator::new()
            .parse_query(&query)
            .ok()
            .and_then(|q| q.on_store(&store).execute().ok());
        match results {
            Some(oxigraph::sparql::QueryResults::Solutions(solutions)) => {
                solutions.filter_map(|s| {
                    s.ok().and_then(|s| {
                        s.get("g").and_then(|t| match t {
                            Term::NamedNode(n) => {
                                // Extract commit SHA from /sync/{sha}/
                                let uri = n.as_str();
                                uri.rfind("/sync/").map(|pos| {
                                    uri[pos + 6..].trim_end_matches('/').to_string()
                                })
                            }
                            _ => None,
                        })
                    })
                }).next()
            }
            _ => None,
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
        let output = Command::new("git")
            .args(["ls-tree", "-r", "--name-only", last_sha, ".lex/extract/"])
            .output();
        let mut prev = HashMap::new();
        if let Ok(o) = output {
            if o.status.success() {
                let stdout = String::from_utf8_lossy(&o.stdout);
                for file_path in stdout.lines() {
                    if file_path.ends_with(".spo") {
                        let content = Command::new("git")
                            .args(["show", &format!("{}:{}", last_sha, file_path)])
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
    let sync_graph = format!("<{}/sync/{}>", base, head_sha);
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
            let subject_uri = format!("<{}/entity/{}~{}>", base, sanitize_uri_segment(subject), blob_hash);
            let predicate_uri = if predicate == "isA" {
                "<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>".to_string()
            } else if predicate == "hasValue" {
                format!("<https://repolex.ai/ontology/git-lex/fm/{}>", uri_encode_path(subject))
            } else if predicate == "mentions" {
                "<https://repolex.ai/ontology/git-lex/lex/mentions>".to_string()
            } else if predicate == "linksTo" {
                "<https://repolex.ai/ontology/git-lex/lex/linksTo>".to_string()
            } else {
                format!("<{}/predicate/{}>", base, sanitize_uri_segment(predicate))
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
                format!("<{}/entity/{}~{}>", base, sanitize_uri_segment(object), blob_hash)
            };

            // The assertion
            sync_nq.push_str(&format!("{} {} {} {} .\n", subject_uri, predicate_uri, object_nq, sync_graph));

            // Name triple
            sync_nq.push_str(&format!(
                "{} <https://repolex.ai/ontology/git-lex/lex/name> \"{}\" {} .\n",
                subject_uri, nq_escape(subject), sync_graph
            ));

            // Triple term annotation
            let spo_key = format!("{}|{}|{}|{}", source_file, subject, predicate, object);
            let ann_hash = short_hash(&spo_key);
            let ann_uri = format!("<{}/ann/{}>", base, ann_hash);

            sync_nq.push_str(&format!(
                "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> <<( {} {} {} )>> {} .\n",
                ann_uri, subject_uri, predicate_uri, object_nq, sync_graph
            ));
            sync_nq.push_str(&format!(
                "{} <https://repolex.ai/ontology/git-lex/lex/filePath> \"{}\" {} .\n",
                ann_uri, nq_escape(source_file), sync_graph
            ));
            sync_nq.push_str(&format!(
                "{} <https://repolex.ai/ontology/git-lex/lex/blobHash> \"{}\" {} .\n",
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
            let ann_uri = format!("<{}/ann/{}>", base, ann_hash);

            sync_nq.push_str(&format!(
                "{} <https://repolex.ai/ontology/git-lex/lex/retracted> \"true\"^^<http://www.w3.org/2001/XMLSchema#boolean> {} .\n",
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
                let ann_uri = format!("<{}/ann/{}>", base, ann_hash);

                sync_nq.push_str(&format!(
                    "{} <https://repolex.ai/ontology/git-lex/lex/retracted> \"true\"^^<http://www.w3.org/2001/XMLSchema#boolean> {} .\n",
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
    // Class graphs (`<base>/class/{Name}`) used to be a "current-state data
    // table" projection — but they were a duplicate of the `now` graph born
    // from a misunderstanding (we thought `now` was fm-namespace-only). They
    // are gone. The `now` graph is the single source of current state.
    //
    // Also sweeps the legacy `<base>/frontmatter` graph (renamed to `now` in
    // a prior commit) so existing repos drop the stale snapshot on next sync.
    //
    // We sweep both on every sync — cheap, idempotent, handles old data left
    // over from before the rename + class-graph deletion shipped.
    let class_prefix = format!("{}/class/", base);
    let legacy_frontmatter = format!("{}/frontmatter", base);
    let stale_graphs: Vec<String> = {
        let q = "SELECT DISTINCT ?g WHERE { GRAPH ?g { ?s ?p ?o } }";
        match oxigraph::sparql::Query::parse(q, None) {
            Ok(mut parsed) => {
                parsed.dataset_mut().set_default_graph_as_union();
                match store.query(parsed) {
                    Ok(oxigraph::sparql::QueryResults::Solutions(sols)) => {
                        sols.flatten().filter_map(|s| {
                            s.get("g").and_then(|t| match t {
                                Term::NamedNode(n) => {
                                    let uri = n.as_str().to_string();
                                    if uri.starts_with(&class_prefix) || uri == legacy_frontmatter {
                                        Some(uri)
                                    } else {
                                        None
                                    }
                                }
                                _ => None,
                            })
                        }).collect()
                    }
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    };
    for graph_uri in &stale_graphs {
        if let Ok(graph_node) = oxigraph::model::NamedNode::new(graph_uri) {
            store.clear_graph(&oxigraph::model::GraphName::from(graph_node)).ok();
        }
    }

    // ─── Phase 4: History graph — incremental update ───
    //
    // Read the lastHistorySync marker from <base/meta>. If it exists and
    // is an ancestor of HEAD, walk only commits since the marker (append).
    // Otherwise fall back to a full rebuild (clear + walk all).

    let history_graph_uri = format!("<{}/history>", base);
    let meta_graph_uri = format!("<{}/meta>", base);

    // Query the marker
    let marker_query = format!(
        "SELECT ?commit WHERE {{ GRAPH {} {{ <{}/meta> <https://repolex.ai/ontology/spo/lastHistorySync> ?commit }} }}",
        meta_graph_uri, base
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
                                uri.rfind("/commit/").map(|pos| uri[pos + 8..].to_string())
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

    let history_summary = if history_commits.is_empty() {
        "up to date".to_string()
    } else {
        let stats = spo_events::history_walk_engine(
            &history_commits,
            &store,
            &base,
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
        format!(
            "{} commit(s), {} events, {} annotations",
            history_commits.len(), stats.events_seen, stats.events_emitted,
        )
    };

    store.flush().expect("failed to flush store");

    let elapsed = start.elapsed();

    // Count total sync graph triples
    let total_sync: usize = existing_graphs.iter()
        .filter(|g| g.contains("/sync/"))
        .count();

    println!(
        "Synced in {:.1}ms:",
        elapsed.as_secs_f64() * 1000.0
    );
    println!("  Virtual: {} git + {} now + {} TBox files", git_count, fm_count, tbox_count);
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
    println!("  History: {}", history_summary);
    println!("  Total sync graphs: {}", total_sync + if sync_count > 0 { 1 } else { 0 });
    if !stale_graphs.is_empty() {
        println!("  Cleaned up {} stale graph(s)", stale_graphs.len());
    }
    println!("Store: {}", store_path().unwrap().display());
}

// add_prefixes imported from git_lex lib

#[allow(deprecated)]
fn run_query(store: &Store, query: &str, store_type: &str) {
    let start = Instant::now();
    let prefixed = add_prefixes(query);

    let mut parsed_query = match oxigraph::sparql::Query::parse(&prefixed, None) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("SPARQL parse error: {}", e);
            exit(1);
        }
    };
    parsed_query.dataset_mut().set_default_graph_as_union();

    let results = match store.query(parsed_query) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("SPARQL evaluation error: {}", e);
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
        oxigraph::sparql::QueryResults::Boolean(b) => {
            println!("{}", b);
            count = 1;
        }
        oxigraph::sparql::QueryResults::Graph(_) => {
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

fn cmd_query(query: String) {
    // Try persistent store first
    if let Some(store) = open_store() {
        run_query(&store, &query, "persistent store");
        return;
    }

    // Fall back to in-memory
    eprintln!("No persistent store found, building in-memory (run 'git lex sync' for faster queries)");
    let start = Instant::now();
    let store = Store::new().expect("failed to create in-memory store");

    let git_nq = generate_git_nquads();
    let git_count = git_nq.lines().count();
    store
        .load_from_reader(RdfFormat::NQuads, Cursor::new(git_nq.as_bytes()))
        .expect("failed to load git triples");

    let lex_nq = load_lex_nquads();
    let lex_count = lex_nq.lines().filter(|l| !l.is_empty()).count();
    if !lex_nq.is_empty() {
        store
            .load_from_reader(RdfFormat::NQuads, Cursor::new(lex_nq.as_bytes()))
            .expect("failed to load .lex/ triples");
    }

    let load_ms = start.elapsed().as_secs_f64() * 1000.0;
    run_query(
        &store,
        &query,
        &format!("in-memory, loaded {} git + {} lex triples in {:.1}ms", git_count, lex_count, load_ms),
    );
}

// ─── main ──────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init { directory, kit, dev } => cmd_init(directory, kit, dev),
        Commands::Status => cmd_status(),
        Commands::Create { doctype, title } => cmd_create(&doctype, title.as_deref()),
        Commands::Save { message } => cmd_save(&message),
        Commands::Query { query } => cmd_query(query),
        Commands::Dump => {
            let git_nq = generate_git_nquads();
            let fm_nq = generate_frontmatter_nquads();
            let lex_nq = load_lex_nquads();
            print!("{}{}{}", git_nq, fm_nq, lex_nq);
        }
        Commands::Extract => cmd_extract(),
        Commands::Validate => {
            if !cmd_validate() {
                exit(1);
            }
        }
        Commands::Join { squad_path } => cmd_join(&squad_path),
        Commands::Parse { file } => cmd_parse(&file),
        Commands::Nuke => cmd_nuke(),
        Commands::KitUpdate { kit, dev, force } => cmd_kit_update(kit, dev, force),
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
        Commands::HistorySpike {
            limit,
            only_changes,
            dedup,
            inconsistency_log,
            canonical,
        } => {
            spo_events::run(spo_events::Options {
                limit,
                only_changes,
                dedup,
                inconsistency_log,
                canonical,
            });
        }
        Commands::HistoryBuild { limit } => {
            spo_events::spike_history_walk(limit);
        }
        Commands::HistoryVerify { show } => {
            cmd_history_verify(show);
        }
        Commands::Llm { command } => match command {
            LlmCommands::List => cmd_llm_list(),
            LlmCommands::Extract { file, model } => cmd_llm_extract(&file, &model),
            LlmCommands::Recheck { file, model } => cmd_llm_recheck(&file, &model),
        },
        Commands::Resolve { full } => cmd_resolve(full),
        Commands::Sync => cmd_sync(),
    }
}

// ─── nuke ──────────────────────────────────────────────────────

/// Set up Claude Code substrate: write git identity env vars and register
/// any hooks into .claude/settings.local.json (gitignored, per-machine).
fn setup_substrate_claude(root: &std::path::Path, agent_name: &str) {
    let settings_path = root.join(".claude").join("settings.local.json");
    fs::create_dir_all(settings_path.parent().unwrap()).ok();

    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = fs::read_to_string(&settings_path).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    // Git identity env vars — injected into every Bash tool call.
    let email = format!("{}@lex.local", agent_name.to_lowercase());
    if !settings.get("env").is_some() {
        settings["env"] = serde_json::json!({});
    }
    let env = settings["env"].as_object_mut().unwrap();
    env.insert("GIT_AUTHOR_NAME".to_string(), serde_json::json!(agent_name));
    env.insert("GIT_AUTHOR_EMAIL".to_string(), serde_json::json!(email));
    env.insert("GIT_COMMITTER_NAME".to_string(), serde_json::json!(agent_name));
    env.insert("GIT_COMMITTER_EMAIL".to_string(), serde_json::json!(email));

    // Register SessionStart hook if the scaffold provided one.
    let hook_script = root.join(".claude").join("hooks").join("SessionStart.sh");
    if hook_script.exists() {
        register_hook_in_settings(&mut settings, "SessionStart",
            r#"bash "$CLAUDE_PROJECT_DIR/.claude/hooks/SessionStart.sh""#);
    }

    let json_str = serde_json::to_string_pretty(&settings).unwrap();
    fs::write(&settings_path, json_str + "\n").ok();
    println!("Claude Code: identity and hooks written to .claude/settings.local.json");
}

/// Add a hook entry to a settings JSON value (in-memory merge, no file I/O).
/// Avoids duplicates by checking if the command is already registered.
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
/// which includes extras like `fm:path` / `git:blobHash` / unconditional
/// `rdf:type <lex:Document>` that the history walker never sees. Instead we
/// regenerate the "pure .spo emission" set live and compare against that.
/// Both sides go through the same emitter → symmetric difference should be
/// empty if the history walker is faithful.
fn cmd_history_verify(show: usize) {
    let start = Instant::now();

    let root = find_git_root().expect("not in a git repo");
    let base = base_uri();
    let history_graph = format!("<{}/history>", base);

    let store_path_buf = root.join(".lex").join("oxigraph");
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
            eprintln!("history-verify: reconstruct query failed (is the history graph populated? try `git lex history-build` first)");
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
        let doc_uri = match spo_events::doc_uri_from_sidecar(&sidecar_rel, &base) {
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
            emit_spo_line_nquads(
                line,
                &doc_uri,
                &emit_graph,
                &base,
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
    eprintln!("║  this repo by deleting the .lex/ directory.             ║");
    eprintln!("║                                                         ║");
    eprintln!("║  DELETED:                                               ║");
    eprintln!("║    • .lex/oxigraph/    (SPARQL store)                   ║");
    eprintln!("║    • .lex/extract/     (extraction sidecars)            ║");
    eprintln!("║    • .lex/kit/         (installed kit)                  ║");
    eprintln!("║    • .lex/ontology/    (ontology files)                 ║");
    eprintln!("║    • .lex/repo.yml     (configuration)                  ║");
    eprintln!("║    • Everything else in .lex/                           ║");
    eprintln!("║                                                         ║");
    eprintln!("║  NOT DELETED:                                           ║");
    eprintln!("║    • Your content files (markdown, etc.)                ║");
    eprintln!("║    • Git history (all commits preserved)                ║");
    eprintln!("║    • .git/ directory                                    ║");
    eprintln!("║                                                         ║");
    eprintln!("║  You can re-initialize with: git lex init               ║");
    eprintln!("║  To also remove git tracking:                           ║");
    eprintln!("║    rm -rf .git                                          ║");
    eprintln!("╚══════════════════════════════════════════════════════════╝");
    eprint!("\nType 'nuke' to confirm: ");

    let mut input = String::new();
    std::io::stdin().read_line(&mut input).unwrap_or_default();
    if input.trim() != "nuke" {
        println!("Aborted.");
        return;
    }

    // Auto-commit any uncommitted work first so nothing is lost.
    auto_commit_snapshot("pre-nuke");

    match fs::remove_dir_all(&lex_dir) {
        Ok(_) => println!(".lex/ removed. git-lex is no longer active in this repo."),
        Err(e) => {
            eprintln!("Failed to remove .lex/: {}", e);
            exit(1);
        }
    }
}

// ─── kit-update ────────────────────────────────────────────────

fn cmd_kit_update(kit_arg: Option<String>, dev: bool, force: bool) {
    let root = find_git_root().expect("not in a git repo");
    let lex_dir = root.join(".lex");

    if !lex_dir.exists() {
        eprintln!("Not a git-lex repo. Run 'git lex init' first.");
        exit(1);
    }

    // Determine which kit to update: from the argument, or from repo.yml.
    // This is the *domain* kit (soul, squad, lab, etc.). The base kit is
    // implicit and always refreshed alongside it, mirroring init's behavior.
    let kit_name = match kit_arg {
        Some(ref k) => k.clone(),
        None => {
            // Read kit from repo.yml
            let repo_yml = lex_dir.join("repo.yml");
            let content = fs::read_to_string(&repo_yml).unwrap_or_default();
            let mut found = None;
            for line in content.lines() {
                if let Some(rest) = line.strip_prefix("kit:") {
                    let val = rest.trim().trim_matches('"').to_string();
                    if val != "none" && !val.is_empty() {
                        found = Some(val);
                    }
                }
            }
            match found {
                Some(k) => k,
                None => {
                    eprintln!("No kit configured in .lex/repo.yml. Specify one: git lex kit-update org/repo");
                    exit(1);
                }
            }
        }
    };

    let (org, repo, short) = resolve_kit_spec(&kit_name);
    let kit_dir = lex_dir.join("kit").join(&org).join(&repo);

    // Resolve the base kit too — we fetch and install its scaffold (which
    // ships core infrastructure like .lex/www/ and .lex/ontology/) on every
    // kit-update, matching init's base+domain pattern.
    let (base_org, base_repo, _) = resolve_kit_spec(BASE_KIT);
    let base_kit_dir = lex_dir.join("kit").join(&base_org).join(&base_repo);
    let base_is_same_as_domain = kit_name == BASE_KIT
        || (org == base_org && repo == base_repo);

    if dev {
        // Dev mode: verify local kit exists, regenerate derived artifacts.
        // Don't refetch base kit either — agent is iterating locally.
        if !kit_dir.exists() {
            eprintln!("--dev: kit directory not found at {}", kit_dir.display());
            eprintln!("Populate the kit locally first, then re-run with --dev.");
            exit(1);
        }
        println!("Dev mode: using local kit at {}", kit_dir.display());
    } else {
        // Normal mode: refresh the base kit first (always), then the domain kit.

        // Base kit.
        println!("Updating base kit '{}/{}' from GitHub...", base_org, base_repo);
        let _ = fs::remove_dir_all(&base_kit_dir);
        fs::create_dir_all(&base_kit_dir).ok();
        if fetch_kit_from_github(BASE_KIT, &base_kit_dir) {
            println!("Base kit '{}/{}' fetched.", base_org, base_repo);
        } else {
            eprintln!("Failed to fetch base kit '{}' from GitHub.", BASE_KIT);
            eprintln!("Check network access to https://github.com/{}/{}", base_org, base_repo);
            exit(1);
        }

        // Domain kit (if different from base).
        if !base_is_same_as_domain {
            println!("Updating kit '{}/{}' from GitHub...", org, repo);
            let _ = fs::remove_dir_all(&kit_dir);
            fs::create_dir_all(&kit_dir).ok();
            if fetch_kit_from_github(&kit_name, &kit_dir) {
                println!("Kit '{}/{}' fetched.", org, repo);
            } else {
                eprintln!("Failed to fetch kit '{}' from GitHub.", kit_name);
                exit(1);
            }
        }
    }

    // Install scaffold files from both kits. This is the new behavior that
    // mirrors init: base kit ships .lex/www/, .lex/ontology/, and domain kit
    // ships its own scaffold (.claude/, AGENTS.md, etc.). Without --force,
    // existing files in the repo are preserved — only missing files are
    // installed. With --force, everything is clobbered (kit-development mode).
    let (base_installed, base_skipped) =
        install_scaffold_files_from_skip_existing(&base_kit_dir, force);
    let (domain_installed, domain_skipped) = if !base_is_same_as_domain {
        install_scaffold_files_from_skip_existing(&kit_dir, force)
    } else {
        (0, 0)
    };
    let total_installed = base_installed + domain_installed;
    let total_skipped = base_skipped + domain_skipped;
    if total_installed > 0 || total_skipped > 0 {
        if force {
            println!(
                "Scaffold: {} file(s) installed (--force: overwrote existing)",
                total_installed
            );
        } else {
            println!(
                "Scaffold: {} file(s) installed, {} preserved (already existed — use --force to overwrite)",
                total_installed, total_skipped
            );
        }
    }

    // Regenerate SHACL shapes from the (possibly updated) kit ontology.
    if let Some(shapes_path) = build_shacl_shapes(&kit_name) {
        println!("SHACL shapes regenerated: {}",
            shapes_path.file_name().unwrap_or_default().to_string_lossy());
    }

    // Regenerate class templates from the kit.
    let kit_types = get_kit_types(&kit_name);
    let shapes_content = {
        let shapes_path = root.join(".lex").join("ontology").join(&short)
            .join(format!("{}-shapes.ttl", short));
        fs::read_to_string(&shapes_path).unwrap_or_default()
    };
    let shacl_hints = parse_shacl_hints(&shapes_content);

    let mut templates_updated = 0usize;
    let update_folder_base = kit_config_str(&kit_name, "folder base");
    for (type_name, properties) in &kit_types {
        let type_dir = if let Some(ref base) = update_folder_base {
            root.join(base).join(type_name)
        } else {
            root.join(type_name)
        };
        fs::create_dir_all(&type_dir).ok();
        let template_path = type_dir.join(format!("__{}.md", type_name));

        // Always overwrite templates on kit-update — they're derived artifacts.
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

    // ─── Folder audit: check disk matches ontology ───
    if let Some(ref base) = update_folder_base {
        let expected: std::collections::HashSet<String> =
            kit_types.iter().map(|(name, _)| name.clone()).collect();
        let base_dir = root.join(base);

        // Check for missing folders (in ontology but not on disk)
        let mut missing = Vec::new();
        for name in &expected {
            if !base_dir.join(name).exists() {
                missing.push(name.clone());
            }
        }

        // Check for extra folders (on disk but not in ontology)
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
        if missing.is_empty() && extra.is_empty() {
            println!("  Folders: {}/{} match ontology ✓", expected.len(), expected.len());
        }
    }

    println!("Kit update complete: {} class templates regenerated.", templates_updated);
}


