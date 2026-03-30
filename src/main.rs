use clap::{Parser, Subcommand};
use oxigraph::io::RdfFormat;
use oxigraph::model::*;
use oxigraph::sparql::SparqlEvaluator;
use oxigraph::store::Store;
use sha2::{Sha256, Digest};
use std::io::Cursor;
use std::path::PathBuf;
use std::process::{Command, exit};
use std::time::Instant;
use std::fs;
use std::collections::{HashMap, HashSet};

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
    /// Initialize .lex/ in the current repo and install global drivers
    Init {
        /// Use case kit (e.g., squad). Defines valid document types and ontology.
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
    ///   git lex query "SELECT ?file WHERE { ?file git:language 'markdown' }"
    Query {
        /// The SPARQL query string
        query: String,
    },
    /// Emit commit history as N-Quads
    Log {
        /// Filter by author
        #[arg(long)]
        author: Option<String>,
        /// Limit number of commits
        #[arg(short, long, default_value = "50")]
        n: usize,
        /// Output format: nq (default) or pretty
        #[arg(long, default_value = "pretty")]
        format: String,
    },
    /// Emit file tree at a ref as N-Quads
    Tree {
        /// Git ref (default: HEAD)
        #[arg(default_value = "HEAD")]
        r#ref: String,
        /// Output format: nq or pretty (default)
        #[arg(long, default_value = "pretty")]
        format: String,
    },
    /// Emit branches and tags as N-Quads
    Refs {
        /// Output format: nq or pretty (default)
        #[arg(long, default_value = "pretty")]
        format: String,
    },
    /// Extract frontmatter from .md files → write .spo sidecars + compile log
    Extract,
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
    /// Semantic diff of knowledge between refs
    Diff {
        /// Show changes since this date or ref
        #[arg(long)]
        since: Option<String>,
    },
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
}

/// Find the git repo root from the current directory.
fn find_git_root() -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Some(PathBuf::from(path))
    } else {
        None
    }
}

/// Get the repo identifier (org/name) from the git remote, or fall back to directory name.
fn get_repo_id() -> String {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output();
    if let Ok(o) = output {
        if o.status.success() {
            let url = String::from_utf8_lossy(&o.stdout).trim().to_string();
            // Extract org/repo from URLs like:
            //   https://github.com/repolex-ai/git-lex-test.git
            //   git@github.com:repolex-ai/git-lex-test.git
            if let Some(stripped) = url.strip_suffix(".git") {
                if let Some(idx) = stripped.rfind('/') {
                    if let Some(idx2) = stripped[..idx].rfind('/') {
                        return stripped[idx2 + 1..].to_string();
                    }
                    // Try colon separator for ssh URLs
                    if let Some(idx2) = stripped[..idx].rfind(':') {
                        return stripped[idx2 + 1..].to_string();
                    }
                }
            }
        }
    }
    // Fallback to directory name
    find_git_root()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "unknown".to_string())
}

fn base_uri() -> String {
    let repo_id = get_repo_id();
    format!("https://repolex.ai/r/{}", repo_id)
}

/// Escape a string for use in N-Quads literals.
/// Escape a string for use in N-Quads literals.
fn nq_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Percent-encode a path for use in URIs (spaces, special chars).
fn uri_encode_path(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '<' => "%3C".to_string(),
            '>' => "%3E".to_string(),
            '{' => "%7B".to_string(),
            '}' => "%7D".to_string(),
            '|' => "%7C".to_string(),
            '^' => "%5E".to_string(),
            '`' => "%60".to_string(),
            '[' => "%5B".to_string(),
            ']' => "%5D".to_string(),
            _ => c.to_string(),
        })
        .collect()
}

// ─── git lex log ───────────────────────────────────────────────

fn cmd_log(author: Option<String>, n: usize, format: String) {
    let mut args = vec![
        "log".to_string(),
        format!("-{}", n),
        "--format=%H%x00%ae%x00%an%x00%aI%x00%s%x00%P%x00%ce%x00%cn%x00%cI".to_string(),
    ];
    if let Some(ref a) = author {
        args.push(format!("--author={}", a));
    }

    let output = Command::new("git").args(&args).output();
    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => {
            eprintln!("fatal: failed to run git log");
            exit(1);
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let base = base_uri();
    let graph = format!("<{}/commits>", base);

    for line in stdout.lines() {
        let fields: Vec<&str> = line.split('\x00').collect();
        if fields.len() < 9 {
            continue;
        }
        let (sha, email, name, date, subject, parents) =
            (fields[0], fields[1], fields[2], fields[3], fields[4], fields[5]);

        let commit_uri = format!("<{}/commit/{}>", base, sha);

        if format == "nq" {
            println!(
                "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/git/Commit> {} .",
                commit_uri, graph
            );
            println!(
                "{} <https://repolex.ai/ontology/git-lex/git/hexsha> \"{}\" {} .",
                commit_uri, sha, graph
            );
            println!(
                "{} <https://repolex.ai/ontology/git-lex/git/authorEmail> \"{}\" {} .",
                commit_uri,
                nq_escape(email),
                graph
            );
            println!(
                "{} <https://repolex.ai/ontology/git-lex/git/authorName> \"{}\" {} .",
                commit_uri,
                nq_escape(name),
                graph
            );
            println!(
                "{} <https://repolex.ai/ontology/git-lex/git/date> \"{}\"^^<http://www.w3.org/2001/XMLSchema#dateTime> {} .",
                commit_uri, date, graph
            );
            println!(
                "{} <https://repolex.ai/ontology/git-lex/git/message> \"{}\" {} .",
                commit_uri,
                nq_escape(subject),
                graph
            );
            for parent in parents.split_whitespace() {
                println!(
                    "{} <https://repolex.ai/ontology/git-lex/git/parent> <{}/commit/{}> {} .",
                    commit_uri, base, parent, graph
                );
            }
        } else {
            // Pretty format
            let short_sha = &sha[..7.min(sha.len())];
            println!("{} {} <{}> {}", short_sha, date, email, subject);
        }
    }
}

// ─── git lex tree ──────────────────────────────────────────────

fn cmd_tree(git_ref: String, format: String) {
    let output = Command::new("git")
        .args(["ls-tree", "-r", "--format=%(objectmode) %(objecttype) %(objectname) %(objectsize)\t%(path)", &git_ref])
        .output();
    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => {
            eprintln!("fatal: failed to run git ls-tree for {}", git_ref);
            exit(1);
        }
    };

    // Resolve ref to full sha
    let ref_sha = Command::new("git")
        .args(["rev-parse", &git_ref])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| git_ref.clone());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let base = base_uri();
    let graph = format!("<{}/filetree/{}>", base, ref_sha);

    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(2, '\t').collect();
        if parts.len() < 2 {
            continue;
        }
        let path = parts[1];
        let meta: Vec<&str> = parts[0].split_whitespace().collect();
        if meta.len() < 4 {
            continue;
        }
        let (_mode, obj_type, blob_hash, size) = (meta[0], meta[1], meta[2], meta[3]);

        let file_uri = format!("<{}/tree/{}/{}>", base, ref_sha, uri_encode_path(path));

        if format == "nq" {
            println!(
                "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/git/Blob> {} .",
                file_uri, graph
            );
            println!(
                "{} <https://repolex.ai/ontology/git-lex/git/path> \"{}\" {} .",
                file_uri,
                nq_escape(path),
                graph
            );
            println!(
                "{} <https://repolex.ai/ontology/git-lex/git/blobHash> \"{}\" {} .",
                file_uri, blob_hash, graph
            );
            println!(
                "{} <https://repolex.ai/ontology/git-lex/git/blob> <{}/blob/{}> {} .",
                file_uri, base_uri(), blob_hash, graph
            );
            println!(
                "{} <https://repolex.ai/ontology/git-lex/git/size> \"{}\"^^<http://www.w3.org/2001/XMLSchema#integer> {} .",
                file_uri, size, graph
            );
            println!(
                "{} <https://repolex.ai/ontology/git-lex/git/type> \"{}\" {} .",
                file_uri, obj_type, graph
            );
        } else {
            println!("{} {} {}  {}", blob_hash, size, obj_type, path);
        }
    }
}

// ─── git lex refs ──────────────────────────────────────────────

fn cmd_refs(format: String) {
    let base = base_uri();
    let graph = format!("<{}/refs>", base);

    // Branches
    let output = Command::new("git")
        .args(["branch", "-a", "--format=%(refname:short) %(objectname:short)"])
        .output();
    if let Ok(o) = output {
        if o.status.success() {
            let stdout = String::from_utf8_lossy(&o.stdout);
            for line in stdout.lines() {
                let parts: Vec<&str> = line.splitn(2, ' ').collect();
                if parts.len() < 2 {
                    continue;
                }
                let (name, sha) = (parts[0], parts[1]);
                let ref_uri = format!("<{}/branch/{}>", base, nq_escape(name));

                if format == "nq" {
                    println!(
                        "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/git/Branch> {} .",
                        ref_uri, graph
                    );
                    println!(
                        "{} <https://repolex.ai/ontology/git-lex/git/shortName> \"{}\" {} .",
                        ref_uri,
                        nq_escape(name),
                        graph
                    );
                    println!(
                        "{} <https://repolex.ai/ontology/git-lex/git/commit> <{}/commit/{}> {} .",
                        ref_uri, base, sha, graph
                    );
                } else {
                    println!("branch  {} -> {}", name, sha);
                }
            }
        }
    }

    // Tags
    let output = Command::new("git")
        .args(["tag", "-l", "--format=%(refname:short) %(objectname:short)"])
        .output();
    if let Ok(o) = output {
        if o.status.success() {
            let stdout = String::from_utf8_lossy(&o.stdout);
            for line in stdout.lines() {
                let parts: Vec<&str> = line.splitn(2, ' ').collect();
                if parts.len() < 2 {
                    continue;
                }
                let (name, sha) = (parts[0], parts[1]);
                let ref_uri = format!("<{}/tag/{}>", base, nq_escape(name));

                if format == "nq" {
                    println!(
                        "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/git/Tag> {} .",
                        ref_uri, graph
                    );
                    println!(
                        "{} <https://repolex.ai/ontology/git-lex/git/shortName> \"{}\" {} .",
                        ref_uri,
                        nq_escape(name),
                        graph
                    );
                    println!(
                        "{} <https://repolex.ai/ontology/git-lex/git/commit> <{}/commit/{}> {} .",
                        ref_uri, base, sha, graph
                    );
                } else {
                    println!("tag     {} -> {}", name, sha);
                }
            }
        }
    }
}

// ─── git lex init ──────────────────────────────────────────────

// Embedded ontologies
const ONT_GIT: &str = include_str!("../ontology/git-lex/git/git.ttl");
const ONT_FM: &str = include_str!("../ontology/git-lex/fm/fm.ttl");
const ONT_LEX: &str = include_str!("../ontology/git-lex/lex/lex.ttl");
const ONT_LEX_O: &str = include_str!("../ontology/git-lex/lex-o/lex-o.ttl");
const ONT_KIT_SQUAD: &str = include_str!("../ontology/git-lex/kit/squad/squad.ttl");

fn cmd_init(kit: Option<String>) {
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

    // Validate kit
    let kit_name = kit.as_deref().unwrap_or("none");
    if let Some(ref k) = kit {
        if k != "squad" {
            eprintln!("Unknown kit: {}. Available kits: squad", k);
            exit(1);
        }
    }

    let lex_dir = root.join(".lex");
    let lex_exists = lex_dir.exists();

    // Create .lex/ structure
    fs::create_dir_all(lex_dir.join("extract")).ok();

    // Install ontologies (full directory structure)
    let ont_dir = lex_dir.join("ontology");
    let ontologies: Vec<(&str, &str)> = vec![
        ("git/git.ttl", ONT_GIT),
        ("fm/fm.ttl", ONT_FM),
        ("lex/lex.ttl", ONT_LEX),
        ("lex-o/lex-o.ttl", ONT_LEX_O),
    ];
    for (path, content) in &ontologies {
        let full_path = ont_dir.join(path);
        fs::create_dir_all(full_path.parent().unwrap()).ok();
        if !full_path.exists() {
            fs::write(&full_path, content).expect(&format!("failed to write {}", path));
        }
    }

    // Install kit ontology if specified
    if let Some(ref k) = kit {
        let kit_content = match k.as_str() {
            "squad" => ONT_KIT_SQUAD,
            _ => unreachable!(),
        };
        let kit_path = ont_dir.join(format!("kit/{}/{}.ttl", k, k));
        fs::create_dir_all(kit_path.parent().unwrap()).ok();
        if !kit_path.exists() {
            fs::write(&kit_path, kit_content).expect("failed to write kit ontology");
        }
    }

    // .gitattributes
    let gitattributes = root.join(".gitattributes");
    let attr_content = "# git-lex: semantic diff/merge for knowledge graph files\n.lex/**/*.nq diff=lex merge=lex\n";
    if gitattributes.exists() {
        let existing = fs::read_to_string(&gitattributes).unwrap_or_default();
        if !existing.contains("diff=lex") {
            fs::write(&gitattributes, format!("{}\n{}", existing.trim_end(), attr_content)).ok();
        }
    } else {
        fs::write(&gitattributes, attr_content).ok();
    }

    // .gitignore
    let gitignore = root.join(".gitignore");
    let ignore_entries = ".lex/oxigraph/\n.lex/raw/\n";
    if gitignore.exists() {
        let existing = fs::read_to_string(&gitignore).unwrap_or_default();
        if !existing.contains(".lex/oxigraph/") {
            fs::write(&gitignore, format!("{}\n{}", existing.trim_end(), ignore_entries)).ok();
        }
    } else {
        fs::write(&gitignore, ignore_entries).ok();
    }

    // repo.yml
    let repo_yml_path = lex_dir.join("repo.yml");
    if !repo_yml_path.exists() {
        let repo_name = root.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let today = Command::new("date").args(["+%Y-%m-%d"]).output().ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        fs::write(&repo_yml_path, format!(
            "name: {}\nkit: {}\ncreated: {}\nversion: \"1.0\"\n",
            repo_name, kit_name, today
        )).ok();
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

    // Create type folders from kit ontology
    if kit.is_some() {
        let kit_types = get_kit_types(kit_name);
        for (type_name, _) in &kit_types {
            let type_dir = root.join(type_name.to_lowercase());
            fs::create_dir_all(&type_dir).ok();
            // Add a .gitkeep so empty dirs are tracked
            let gitkeep = type_dir.join(".gitkeep");
            if !gitkeep.exists() {
                fs::write(&gitkeep, "").ok();
            }
        }
        if !kit_types.is_empty() {
            let type_names: Vec<String> = kit_types.iter().map(|(n, _)| n.to_lowercase()).collect();
            println!("Created type folders: {}", type_names.join(", "));

            // Generate README.lex.md
            let readme_lex = root.join("README.lex.md");
            if !readme_lex.exists() {
                let mut doc = String::new();
                doc.push_str(&format!("# git-lex — {} kit\n\n", kit_name));
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
                doc.push_str(&format!("Documents use YAML frontmatter with a `{}:` block for structured data:\n\n", kit_name));
                doc.push_str("```yaml\n");
                doc.push_str("---\n");
                doc.push_str("title: \"My Document\"\n");
                doc.push_str("tags: [topic1, topic2]\n");
                doc.push_str(&format!("{}:\n", kit_name));
                doc.push_str("  type: <Type>\n");
                doc.push_str("  property: \"value\"\n");
                doc.push_str("---\n\n");
                doc.push_str("Your content here. Use @mentions and [[wikilinks]] for relationships.\n");
                doc.push_str("```\n\n");

                doc.push_str("## @mentions and [[wikilinks]]\n\n");
                doc.push_str("Reference other agents and documents naturally in your text:\n\n");
                doc.push_str("- `@agentname` — creates a `lex:mentions` relationship\n");
                doc.push_str("- `[[document-title]]` — creates a `lex:linksTo` relationship\n\n");
                doc.push_str("These are extracted automatically from document bodies and commit messages.\n\n");

                // Kit-specific section
                doc.push_str(&format!("## {} Kit — Document Types\n\n", kit_name));
                for (type_name, properties) in &kit_types {
                    doc.push_str(&format!("### {}\n\n", type_name));
                    doc.push_str(&format!("Create: `git lex create {}`\n\n", type_name.to_lowercase()));
                    if !properties.is_empty() {
                        doc.push_str("| Property | Type |\n");
                        doc.push_str("|---|---|\n");
                        for (prop_name, prop_type, _) in properties {
                            doc.push_str(&format!("| {} | {} |\n", prop_name, prop_type));
                        }
                        doc.push_str("\n");
                    }
                }

                doc.push_str("## Querying\n\n");
                doc.push_str("Auto-injected prefixes: `git:`, `fm:`, `lex:`, `lex-o:`");
                if kit_name != "none" {
                    doc.push_str(&format!(", `{}:`", kit_name));
                }
                doc.push_str("\n\n");
                doc.push_str("```sparql\n");
                doc.push_str("# List all documents by type\n");
                doc.push_str(&format!("SELECT ?name ?type WHERE {{\n  GRAPH ?g {{ ?doc fm:{}.type ?type ; fm:title ?name }}\n}}\n", kit_name));
                doc.push_str("```\n");

                fs::write(&readme_lex, &doc).ok();
                println!("Created README.lex.md");
            }
        }
    }

    // Print summary
    if lex_exists {
        println!("Reinitialized .lex/ in {}", root.display());
    } else {
        println!("Initialized .lex/ in {}", root.display());
    }
    println!();
    println!("  .lex/repo.yml     — repo config (kit: {})", kit_name);
    println!("  .lex/extract/     — extraction sidecars");
    println!("  .lex/ontology/    — ontology definitions");
    println!("  .gitattributes    — semantic diff/merge drivers");
    println!();

    // Install global drivers
    install_global_drivers();

    // Pre-commit hook: extract changed files → write sidecars → stage
    let hooks_dir = root.join(".git").join("hooks");
    fs::create_dir_all(&hooks_dir).ok();

    let pre_commit_path = hooks_dir.join("pre-commit");
    let pre_commit_content = "#!/bin/sh\ngit-lex extract\ngit add .lex/extract/ 2>/dev/null\n";
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

    let _ = Command::new("git").args(["add", ".lex/", ".gitattributes", ".gitignore"]).status();
    if Command::new("git").args(["commit", "-m", "git lex init"]).output()
        .map(|o| o.status.success()).unwrap_or(false) {
        println!("\nCommitted git-lex setup files.");
    }

    // Offer to commit existing content
    if !has_commits || !lex_exists {
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
}

fn install_global_drivers() {
    let commands = [
        ["config", "--global", "diff.lex.command", "git-lex diff-driver"],
        ["config", "--global", "merge.lex.name", "git-lex semantic merge"],
        [
            "config",
            "--global",
            "merge.lex.driver",
            "git-lex merge-driver %O %A %B %P",
        ],
    ];

    for args in &commands {
        let status = Command::new("git").args(args.as_slice()).status();
        match status {
            Ok(s) if s.success() => {}
            _ => {
                eprintln!("failed to set git config: git {}", args.join(" "));
                exit(1);
            }
        }
    }

    println!("Installed global git-lex drivers:");
    println!("  diff.lex.command = git-lex diff-driver");
    println!("  merge.lex.driver = git-lex merge-driver %O %A %B %P");
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

    let gitattributes = root.join(".gitattributes");
    if gitattributes.exists() {
        let content = fs::read_to_string(&gitattributes).unwrap_or_default();
        if content.contains("diff=lex") {
            println!("  .gitattributes — lex drivers configured");
        } else {
            println!("  .gitattributes — exists but no lex drivers");
        }
    } else {
        println!("  .gitattributes — not found");
    }

    let diff_check = Command::new("git")
        .args(["config", "--global", "diff.lex.command"])
        .output();
    match diff_check {
        Ok(o) if o.status.success() => {
            println!("  global config — lex drivers installed");
        }
        _ => {
            println!("  global config — lex drivers NOT installed (run 'git lex init')");
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

/// Generate all virtual N-Quads from git (commits, tree, refs).
fn generate_git_nquads() -> String {
    let mut nq = String::new();

    // Commits
    let output = Command::new("git")
        .args(["log", "--all", "--format=%H%x00%ae%x00%an%x00%aI%x00%s%x00%P%x00%ce%x00%cn%x00%cI"])
        .output();
    if let Ok(o) = output {
        if o.status.success() {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let base = base_uri();
            let graph = format!("<{}/commits>", base);
            for line in stdout.lines() {
                let f: Vec<&str> = line.split('\x00').collect();
                if f.len() < 9 { continue; }
                let (sha, email, name, date, subject, parents) = (f[0], f[1], f[2], f[3], f[4], f[5]);
                let (committer_email, committer_name, committer_date) = (f[6], f[7], f[8]);
                let cu = format!("<{}/commit/{}>", base, sha);
                nq.push_str(&format!("{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/git/Commit> {} .\n", cu, graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/hexsha> \"{}\" {} .\n", cu, sha, graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/authorEmail> \"{}\" {} .\n", cu, nq_escape(email), graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/authorName> \"{}\" {} .\n", cu, nq_escape(name), graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/authoredDate> \"{}\"^^<http://www.w3.org/2001/XMLSchema#dateTime> {} .\n", cu, date, graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/committerEmail> \"{}\" {} .\n", cu, nq_escape(committer_email), graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/committerName> \"{}\" {} .\n", cu, nq_escape(committer_name), graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/committedDate> \"{}\"^^<http://www.w3.org/2001/XMLSchema#dateTime> {} .\n", cu, committer_date, graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/message> \"{}\" {} .\n", cu, nq_escape(subject), graph));
                for parent in parents.split_whitespace() {
                    nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/parent> <{}/commit/{}> {} .\n", cu, base, parent, graph));
                }
            }
        }
    }

    // Tree at HEAD
    let ref_sha = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    if !ref_sha.is_empty() {
        let output = Command::new("git")
            .args(["ls-tree", "-r", "--format=%(objectmode) %(objecttype) %(objectname) %(objectsize)\t%(path)", "HEAD"])
            .output();
        if let Ok(o) = output {
            if o.status.success() {
                let stdout = String::from_utf8_lossy(&o.stdout);
                let base = base_uri();
                let graph = format!("<{}/filetree/{}>", base, ref_sha);
                for line in stdout.lines() {
                    let parts: Vec<&str> = line.splitn(2, '\t').collect();
                    if parts.len() < 2 { continue; }
                    let path = parts[1];
                    let meta: Vec<&str> = parts[0].split_whitespace().collect();
                    if meta.len() < 4 { continue; }
                    let (obj_type, blob_hash, size) = (meta[1], meta[2], meta[3]);
                    let fu = format!("<{}/tree/{}/{}>", base, ref_sha, uri_encode_path(path));
                    nq.push_str(&format!("{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/git/Blob> {} .\n", fu, graph));
                    nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/path> \"{}\" {} .\n", fu, nq_escape(path), graph));
                    nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/blobHash> \"{}\" {} .\n", fu, blob_hash, graph));
                    nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/blob> <{}/blob/{}> {} .\n", fu, base, blob_hash, graph));
                    nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/size> \"{}\"^^<http://www.w3.org/2001/XMLSchema#integer> {} .\n", fu, size, graph));
                    nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/type> \"{}\" {} .\n", fu, obj_type, graph));
                }
            }
        }
    }

    // Branches
    let output = Command::new("git")
        .args(["branch", "-a", "--format=%(refname:short) %(objectname)"])
        .output();
    if let Ok(o) = output {
        if o.status.success() {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let base = base_uri();
            let graph = format!("<{}/refs>", base);
            for line in stdout.lines() {
                let parts: Vec<&str> = line.splitn(2, ' ').collect();
                if parts.len() < 2 { continue; }
                let (name, sha) = (parts[0], parts[1]);
                let ru = format!("<{}/branch/{}>", base, nq_escape(name));
                nq.push_str(&format!("{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/git/Branch> {} .\n", ru, graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/shortName> \"{}\" {} .\n", ru, nq_escape(name), graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/commit> <{}/commit/{}> {} .\n", ru, base, sha, graph));
            }
        }
    }

    // Tags
    let output = Command::new("git")
        .args(["tag", "-l", "--format=%(refname:short) %(objectname)"])
        .output();
    if let Ok(o) = output {
        if o.status.success() {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let base = base_uri();
            let graph = format!("<{}/refs>", base);
            for line in stdout.lines() {
                let parts: Vec<&str> = line.splitn(2, ' ').collect();
                if parts.len() < 2 { continue; }
                let (name, sha) = (parts[0], parts[1]);
                let ru = format!("<{}/tag/{}>", base, nq_escape(name));
                nq.push_str(&format!("{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/git/Tag> {} .\n", ru, graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/shortName> \"{}\" {} .\n", ru, nq_escape(name), graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/commit> <{}/commit/{}> {} .\n", ru, base, sha, graph));
            }
        }
    }

    // Changesets: which files each commit touched
    let output = Command::new("git")
        .args(["log", "--all", "--format=%H", "--name-status", "--diff-filter=ADMR"])
        .output();
    if let Ok(o) = output {
        if o.status.success() {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let base = base_uri();
            let mut current_sha = String::new();
            for line in stdout.lines() {
                let line = line.trim();
                if line.is_empty() { continue; }
                // SHA lines are 40 hex chars
                if line.len() == 40 && line.chars().all(|c| c.is_ascii_hexdigit()) {
                    current_sha = line.to_string();
                    continue;
                }
                if current_sha.is_empty() { continue; }
                // Status lines: "M\tpath" or "A\tpath" or "D\tpath" or "R100\told\tnew"
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() < 2 { continue; }
                let status = parts[0];
                let path = parts[1];
                let graph = format!("<{}/changeset/{}>", base, current_sha);
                let change_uri = format!("<{}/changeset/{}/{}>", base, current_sha, uri_encode_path(path));
                let commit_uri = format!("<{}/commit/{}>", base, current_sha);

                // Link commit to changeset (in commits graph so joins work)
                let commits_graph = format!("<{}/commits>", base);
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/changed> {} {} .\n", commit_uri, change_uri, commits_graph));

                // Change details
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/path> \"{}\" {} .\n", change_uri, nq_escape(path), graph));

                let status_label = match status.chars().next() {
                    Some('A') => "added",
                    Some('M') => "modified",
                    Some('D') => "deleted",
                    Some('R') => "renamed",
                    _ => "unknown",
                };
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/changeType> \"{}\" {} .\n", change_uri, status_label, graph));

                // For renames, capture the new path
                if status.starts_with('R') && parts.len() >= 3 {
                    nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/renamedTo> \"{}\" {} .\n", change_uri, nq_escape(parts[2]), graph));
                }
            }
        }
    }

    // Blame: per-file authorship using git2 (handles unicode/emoji safely)
    if let Ok(repo) = git2::Repository::discover(".") {
        if let Ok(head) = repo.head() {
            if let Some(head_oid) = head.target() {
                let head_sha = head_oid.to_string();
                let base = base_uri();
                let graph = format!("<{}/blame/{}>", base, head_sha);

                // Get file list from HEAD tree
                if let Ok(commit) = repo.find_commit(head_oid) {
                    if let Ok(tree) = commit.tree() {
                        let mut paths = Vec::new();
                        tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
                            if entry.kind() == Some(git2::ObjectType::Blob) {
                                let path = if dir.is_empty() {
                                    entry.name().unwrap_or("").to_string()
                                } else {
                                    format!("{}{}", dir, entry.name().unwrap_or(""))
                                };
                                if !path.starts_with(".lex/") {
                                    paths.push(path);
                                }
                            }
                            git2::TreeWalkResult::Ok
                        }).ok();

                        // Limit blame to reasonable number of files
                        let max_files = 500;
                        for path in paths.iter().take(max_files) {
                            let blame = repo.blame_file(std::path::Path::new(path), None);
                            if let Ok(blame) = blame {
                                let file_uri = format!("<{}/tree/{}/{}>", base, head_sha, uri_encode_path(path));
                                let mut authors_seen = std::collections::HashSet::new();

                                for i in 0..blame.len() {
                                    if let Some(hunk) = blame.get_index(i) {
                                        if let Some(sig) = hunk.final_signature().name() {
                                            let key = format!("name:{}:{}", sig, path);
                                            if authors_seen.insert(key) {
                                                nq.push_str(&format!(
                                                    "{} <https://repolex.ai/ontology/git-lex/git/blamedAuthor> \"{}\" {} .\n",
                                                    file_uri, nq_escape(sig), graph
                                                ));
                                            }
                                        }
                                        if let Some(email) = hunk.final_signature().email() {
                                            let key = format!("email:{}:{}", email, path);
                                            if authors_seen.insert(key) {
                                                nq.push_str(&format!(
                                                    "{} <https://repolex.ai/ontology/git-lex/git/blamedEmail> \"{}\" {} .\n",
                                                    file_uri, nq_escape(email), graph
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Language detection from file extensions
    {
        let base = base_uri();
        let head_sha = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let graph = format!("<{}/filetree/{}>", base, head_sha);

        let output = Command::new("git")
            .args(["ls-tree", "-r", "--name-only", "HEAD"])
            .output();
        if let Ok(o) = output {
            if o.status.success() {
                let stdout = String::from_utf8_lossy(&o.stdout);
                for path in stdout.lines() {
                    let lang = match path.rsplit('.').next() {
                        Some("md") => Some("markdown"),
                        Some("txt") => Some("text"),
                        Some("rs") => Some("rust"),
                        Some("py") => Some("python"),
                        Some("js") => Some("javascript"),
                        Some("ts") => Some("typescript"),
                        Some("go") => Some("go"),
                        Some("java") => Some("java"),
                        Some("rb") => Some("ruby"),
                        Some("c") | Some("h") => Some("c"),
                        Some("cpp") | Some("cc") | Some("cxx") | Some("hpp") => Some("cpp"),
                        Some("sh") | Some("bash") => Some("shell"),
                        Some("json") => Some("json"),
                        Some("yaml") | Some("yml") => Some("yaml"),
                        Some("toml") => Some("toml"),
                        Some("xml") => Some("xml"),
                        Some("html") | Some("htm") => Some("html"),
                        Some("css") => Some("css"),
                        Some("sql") => Some("sql"),
                        Some("nq") => Some("nquads"),
                        Some("nt") => Some("ntriples"),
                        Some("ttl") => Some("turtle"),
                        Some("jsonld") => Some("jsonld"),
                        _ => None,
                    };
                    if let Some(lang) = lang {
                        let fu = format!("<{}/tree/{}/{}>", base, head_sha, uri_encode_path(path));
                        nq.push_str(&format!(
                            "{} <https://repolex.ai/ontology/git-lex/git/language> \"{}\" {} .\n",
                            fu, lang, graph
                        ));
                    }
                }
            }
        }
    }

    nq
}

/// Load .lex/*.nq files and return their contents.
fn load_lex_nquads() -> String {
    let root = match find_git_root() {
        Some(r) => r,
        None => return String::new(),
    };

    let mut nq = String::new();
    let lex_dir = root.join(".lex");

    // Recursively find all .nq files
    fn walk_nq(dir: &std::path::Path, nq: &mut String) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_dir() {
                    walk_nq(&path, nq);
                } else if path.extension().is_some_and(|e| e == "nq") {
                    if let Ok(content) = fs::read_to_string(&path) {
                        nq.push_str(&content);
                        if !content.ends_with('\n') {
                            nq.push('\n');
                        }
                    }
                }
            }
        }
    }

    if lex_dir.exists() {
        walk_nq(&lex_dir, &mut nq);
    }

    nq
}

/// Get the persistent store path.
fn store_path() -> Option<PathBuf> {
    find_git_root().map(|r| r.join(".lex").join("oxigraph"))
}

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

/// Flatten a YAML value into .spo lines with dot-notation for nested keys.
/// Individual .spo files use simple format: subject | predicate | object
fn flatten_yaml(prefix: &str, value: &serde_yaml::Value, lines: &mut Vec<String>) {
    match value {
        serde_yaml::Value::String(s) => {
            lines.push(format!("{} | hasValue | {}", prefix, s));
        }
        serde_yaml::Value::Sequence(seq) => {
            for item in seq {
                if let Some(s) = item.as_str() {
                    lines.push(format!("{} | hasValue | {}", prefix, s));
                } else if let Some(n) = item.as_f64() {
                    lines.push(format!("{} | hasValue | {}", prefix, n));
                } else if let Some(b) = item.as_bool() {
                    lines.push(format!("{} | hasValue | {}", prefix, b));
                }
            }
        }
        serde_yaml::Value::Bool(b) => {
            lines.push(format!("{} | hasValue | {}", prefix, b));
        }
        serde_yaml::Value::Number(n) => {
            lines.push(format!("{} | hasValue | {}", prefix, n));
        }
        serde_yaml::Value::Mapping(map) => {
            for (k, v) in map {
                if let Some(key_str) = k.as_str() {
                    let nested_prefix = format!("{}.{}", prefix, key_str);
                    flatten_yaml(&nested_prefix, v, lines);
                }
            }
        }
        _ => {}
    }
}

/// Extract frontmatter from all .md files.
/// Returns N-Quads for oxigraph AND writes .spo sidecar files.
fn generate_frontmatter_nquads() -> String {
    let root = match find_git_root() {
        Some(r) => r,
        None => return String::new(),
    };

    let base = base_uri();
    let graph = format!("<{}/frontmatter>", base);
    let mut nq = String::new();

    // Open git repo for blob hash lookups
    let repo = git2::Repository::discover(".").ok();

    // Walk all .md files in the repo (skip .lex/ and .git/)
    fn walk_md(dir: &std::path::Path, files: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                if path.is_dir() {
                    walk_md(&path, files);
                } else if name.ends_with(".md") || name.ends_with(".txt") {
                    files.push(path);
                }
            }
        }
    }

    let mut files = Vec::new();
    walk_md(&root, &mut files);

    // Ensure extract dir exists
    let extract_dir = root.join(".lex").join("extract");
    fs::create_dir_all(&extract_dir).ok();

    // Regex patterns for @mentions and [[wikilinks]]
    let mention_re = regex::Regex::new(r"@([a-zA-Z0-9_-]+)").unwrap();
    let wikilink_re = regex::Regex::new(r"\[\[([^\]]+)\]\]").unwrap();

    for filepath in &files {
        let content = match fs::read_to_string(filepath) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let relpath = filepath.strip_prefix(&root).unwrap_or(filepath);
        let relpath_str = relpath.to_string_lossy().to_string();

        // Get blob hash from git index (staging area)
        let blob_hash = repo.as_ref().and_then(|r| {
            if let Ok(index) = r.index() {
                if let Some(entry) = index.get_path(std::path::Path::new(&relpath_str), 0) {
                    return Some(entry.id.to_string());
                }
            }
            let head = r.head().ok()?;
            let tree = head.peel_to_tree().ok()?;
            let entry = tree.get_path(std::path::Path::new(&relpath_str)).ok()?;
            Some(entry.id().to_string())
        }).unwrap_or_default();

        let short_hash = if blob_hash.len() >= 8 { &blob_hash[..8] } else { &blob_hash };

        // --- Frontmatter extraction ---
        let mut spo_lines = Vec::new();
        let body_text;

        if content.starts_with("---\n") || content.starts_with("---\r\n") {
            let rest = &content[4..];
            if let Some(end) = rest.find("\n---") {
                let yaml_str = &rest[..end];
                if let Ok(yaml) = serde_yaml::from_str::<HashMap<String, serde_yaml::Value>>(yaml_str) {
                    for (key, value) in &yaml {
                        flatten_yaml(key, value, &mut spo_lines);
                    }
                }
                // Body is everything after the closing ---
                let after_fm = &rest[end + 4..]; // skip "\n---"
                body_text = after_fm.to_string();
            } else {
                body_text = content.clone();
            }
        } else {
            body_text = content.clone();
        }

        // --- @mention extraction ---
        let mut mentions_seen = HashSet::new();
        for cap in mention_re.captures_iter(&body_text) {
            let mention = cap[1].to_lowercase();
            if mentions_seen.insert(mention.clone()) {
                spo_lines.push(format!("@{} | mentions | {}", relpath_str, mention));
            }
        }

        // --- [[wikilink]] extraction ---
        let mut links_seen = HashSet::new();
        for cap in wikilink_re.captures_iter(&body_text) {
            let link = cap[1].to_string();
            if links_seen.insert(link.clone()) {
                spo_lines.push(format!("{} | linksTo | {}", relpath_str, link));
            }
        }

        // Sort and dedup
        spo_lines.sort();
        spo_lines.dedup();

        // Write .spo sidecar (only if there's content)
        if !spo_lines.is_empty() {
            let spo_path = extract_dir.join(format!("{}.fm.spo", relpath_str));
            fs::create_dir_all(spo_path.parent().unwrap()).ok();
            let spo_content = spo_lines.join("\n") + "\n";
            fs::write(&spo_path, &spo_content).ok();
        }

        // --- Generate N-Quads for oxigraph (frontmatter graph) ---
        let doc_uri = format!("<{}/file/{}/{}>", base, short_hash, uri_encode_path(&relpath_str));

        nq.push_str(&format!(
            "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/lex-upper/Document> {} .\n",
            doc_uri, graph
        ));
        nq.push_str(&format!(
            "{} <https://repolex.ai/ontology/git-lex/fm/path> \"{}\" {} .\n",
            doc_uri, nq_escape(&relpath_str), graph
        ));
        nq.push_str(&format!(
            "{} <https://repolex.ai/ontology/git-lex/git/blobHash> \"{}\" {} .\n",
            doc_uri, blob_hash, graph
        ));

        for line in &spo_lines {
            let parts: Vec<&str> = line.splitn(3, " | ").collect();
            if parts.len() == 3 {
                let subject = parts[0];
                let predicate = parts[1];
                let object = parts[2];

                if predicate == "mentions" {
                    // @mention → lex:mentions
                    nq.push_str(&format!(
                        "{} <https://repolex.ai/ontology/git-lex/lex/mentions> \"{}\" {} .\n",
                        doc_uri, nq_escape(object), graph
                    ));
                } else if predicate == "linksTo" {
                    // [[wikilink]] → lex:linksTo
                    nq.push_str(&format!(
                        "{} <https://repolex.ai/ontology/git-lex/lex/linksTo> \"{}\" {} .\n",
                        doc_uri, nq_escape(object), graph
                    ));
                } else {
                    // Frontmatter key-value
                    let fm_predicate = format!("<https://repolex.ai/ontology/git-lex/fm/{}>", uri_encode_path(subject));
                    nq.push_str(&format!(
                        "{} {} \"{}\" {} .\n",
                        doc_uri, fm_predicate, nq_escape(object), graph
                    ));
                }
            }
        }
    }

    // --- Scan commit messages for @mentions and [[wikilinks]] ---
    let commit_output = Command::new("git")
        .args(["log", "--all", "--format=%H%x00%s"])
        .output();
    if let Ok(o) = commit_output {
        if o.status.success() {
            let stdout = String::from_utf8_lossy(&o.stdout);
            for line in stdout.lines() {
                let parts: Vec<&str> = line.splitn(2, '\x00').collect();
                if parts.len() < 2 { continue; }
                let (sha, message) = (parts[0], parts[1]);
                let commit_uri = format!("<{}/commit/{}>", base, sha);

                for cap in mention_re.captures_iter(message) {
                    let mention = cap[1].to_lowercase();
                    nq.push_str(&format!(
                        "{} <https://repolex.ai/ontology/git-lex/lex/mentions> \"{}\" {} .\n",
                        commit_uri, nq_escape(&mention), graph
                    ));
                }
                for cap in wikilink_re.captures_iter(message) {
                    let link = &cap[1];
                    nq.push_str(&format!(
                        "{} <https://repolex.ai/ontology/git-lex/lex/linksTo> \"{}\" {} .\n",
                        commit_uri, nq_escape(link), graph
                    ));
                }
            }
        }
    }

    nq
}

/// Compile extraction log from all .spo sidecar files.
/// Prepends blobhash/filepath to each line for grounding.
fn compile_extraction_log() {
    let root = find_git_root().unwrap();
    let extract_dir = root.join(".lex").join("extract");
    let log_path = root.join(".lex").join("extraction.log.spo");

    // Build blob hash lookup from git index
    let repo = git2::Repository::discover(".").ok();
    let blob_map: HashMap<String, String> = repo.as_ref().map(|r| {
        let mut map = HashMap::new();
        if let Ok(index) = r.index() {
            for entry in index.iter() {
                let path = String::from_utf8_lossy(&entry.path).to_string();
                let hash = entry.id.to_string();
                let short = hash[..8.min(hash.len())].to_string();
                map.insert(path, short);
            }
        }
        map
    }).unwrap_or_default();

    let mut all_spo_lines: Vec<String> = Vec::new();

    // Walk .spo files, derive source file path from sidecar path
    fn walk_spo(dir: &std::path::Path, extract_dir: &std::path::Path, blob_map: &HashMap<String, String>, lines: &mut Vec<String>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_dir() {
                    walk_spo(&path, extract_dir, blob_map, lines);
                } else if path.extension().is_some_and(|e| e == "spo") {
                    // Derive source file path: strip extract_dir prefix and .fm.spo/.llm.spo suffix
                    let rel = path.strip_prefix(extract_dir).unwrap_or(&path);
                    let rel_str = rel.to_string_lossy().to_string();
                    // Strip extractor suffix: filename.ext.{extractor}.spo → filename.ext
                    let source_path = if let Some(pos) = rel_str.rfind(".spo") {
                        let without_spo = &rel_str[..pos];
                        // Find the second-to-last dot (the extractor separator)
                        if let Some(ext_pos) = without_spo.rfind('.') {
                            // Check if what's between the dots looks like an extractor name
                            // (not a file extension like .md)
                            let maybe_ext = &without_spo[ext_pos + 1..];
                            if maybe_ext == "fm" || maybe_ext.contains('-') || maybe_ext.len() > 5 {
                                without_spo[..ext_pos].to_string()
                            } else {
                                without_spo.to_string()
                            }
                        } else {
                            without_spo.to_string()
                        }
                    } else {
                        rel_str.clone()
                    };

                    let blob_hash = blob_map.get(&source_path)
                        .cloned()
                        .unwrap_or_else(|| "00000000".to_string());

                    let file_id = format!("{}/{}", blob_hash, source_path);

                    if let Ok(content) = fs::read_to_string(&path) {
                        for line in content.lines() {
                            if !line.is_empty() && !line.starts_with('#') {
                                lines.push(format!("{} | {}", file_id, line));
                            }
                        }
                    }
                }
            }
        }
    }
    if extract_dir.exists() {
        walk_spo(&extract_dir, &extract_dir, &blob_map, &mut all_spo_lines);
    }

    // Sort for deterministic output (canonical ordering)
    all_spo_lines.sort();
    all_spo_lines.dedup();

    let new_log = if all_spo_lines.is_empty() {
        String::new()
    } else {
        all_spo_lines.join("\n") + "\n"
    };
    let old_log = fs::read_to_string(&log_path).unwrap_or_default();

    if new_log != old_log {
        fs::write(&log_path, &new_log).expect("failed to write extraction.log.spo");
        let new_count = all_spo_lines.len();
        let old_count = old_log.lines().filter(|l| !l.is_empty()).count();
        let added = new_count as i64 - old_count as i64;
        if added > 0 {
            eprintln!("Extraction log: {} assertions (+{})", new_count, added);
        } else if added < 0 {
            eprintln!("Extraction log: {} assertions ({})", new_count, added);
        } else {
            eprintln!("Extraction log: {} assertions (content changed)", new_count);
        }
    } else {
        eprintln!("Extraction log: {} assertions (unchanged)", all_spo_lines.len());
    }
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

/// Validate an IRI string. Returns true if valid.
fn is_valid_iri(iri: &str) -> bool {
    oxiri::Iri::parse(iri).is_ok()
}

/// Sanitize a string for use in a URI path segment.
/// Removes/replaces characters that would make an invalid IRI.
fn sanitize_uri_segment(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            ' ' | ':' | '/' | '\\' | '<' | '>' | '{' | '}' | '|' | '^' | '`' | '[' | ']' | '#' | '?' | '@' => '-',
            _ if c.is_alphanumeric() => c,
            _ => '-',
        })
        .collect::<String>()
        .replace("--", "-")
        .trim_matches('-')
        .to_string()
}

/// Generate a short deterministic hash from a string.
fn short_hash(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..8]) // 16 hex chars
}

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

        // Handle object: if predicate is isA or hasValue, object is a literal
        // Otherwise, object is another entity from the same file
        let object_nq = if predicate == "isA" || predicate == "hasValue" {
            format!("\"{}\"", nq_escape(object))
        } else {
            format!("<{}/entity/{}~{}>", base, sanitize_uri_segment(object), blob_hash)
        };

        // Write the assertion triple
        nq.push_str(&format!("{} {} {} {} .\n", subject_uri, predicate_nq, object_nq, graph));

        // Write name triple for subject (if we haven't seen it yet)
        nq.push_str(&format!(
            "{} <https://repolex.ai/ontology/lex-upper/name> \"{}\" {} .\n",
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

/// Read repo.yml and return the kit name.
fn get_kit() -> Option<String> {
    let root = find_git_root()?;
    let repo_yml = root.join(".lex").join("repo.yml");
    let content = fs::read_to_string(&repo_yml).ok()?;
    for line in content.lines() {
        if let Some(kit) = line.strip_prefix("kit: ") {
            let kit = kit.trim();
            if kit != "none" {
                return Some(kit.to_string());
            }
        }
    }
    None
}

/// Parse the kit ontology to find document types and their properties.
fn get_kit_types(kit: &str) -> Vec<(String, Vec<(String, String, bool)>)> {
    let root = match find_git_root() {
        Some(r) => r,
        None => return Vec::new(),
    };

    let ontology_path = root.join(".lex").join("ontology").join("kit").join(kit).join(format!("{}.ttl", kit));
    let content = match fs::read_to_string(&ontology_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    // Extract the kit prefix namespace
    let mut prefix = String::new();
    for line in content.lines() {
        if line.starts_with(&format!("@prefix {}:", kit)) {
            if let Some(start) = line.find('<') {
                if let Some(end) = line.find('>') {
                    prefix = line[start + 1..end].to_string();
                }
            }
        }
    }

    // Find all owl:Class declarations and their properties
    let mut types: HashMap<String, Vec<(String, String, bool)>> = HashMap::new();
    let mut current_class = String::new();

    for line in content.lines() {
        let trimmed = line.trim();

        // Detect class: "squad:Decision a owl:Class ;"
        if trimmed.contains("a owl:Class") {
            if let Some(class_name) = trimmed.split_whitespace().next() {
                let name = class_name
                    .strip_prefix(&format!("{}:", kit))
                    .unwrap_or(class_name)
                    .to_string();
                current_class = name.clone();
                types.entry(name).or_default();
            }
        }

        // Detect property with domain: "rdfs:domain squad:Decision ;"
        if trimmed.contains("rdfs:domain") && trimmed.contains(&format!("{}:", kit)) {
            // Look back to find the property name — this is tricky with TTL
            // Instead, we'll parse properties differently
        }
    }

    // Parse properties: track current property name and type across multi-line TTL blocks
    let mut current_prop = String::new();
    let mut current_prop_type = String::new();

    for line in content.lines() {
        let trimmed = line.trim();

        // New property block starts with "kit:propName a owl:DatatypeProperty/ObjectProperty"
        if trimmed.contains("a owl:DatatypeProperty") || trimmed.contains("a owl:ObjectProperty") {
            if let Some(prop) = trimmed.split_whitespace().next() {
                current_prop = prop
                    .strip_prefix(&format!("{}:", kit))
                    .unwrap_or(prop)
                    .to_string();
                current_prop_type = if trimmed.contains("DatatypeProperty") {
                    "string".to_string()
                } else {
                    "reference".to_string()
                };
            }
        }

        // Domain line within a property block
        if !current_prop.is_empty() && trimmed.starts_with("rdfs:domain") {
            if let Some(domain) = trimmed.split_whitespace().nth(1) {
                let class_name = domain
                    .strip_prefix(&format!("{}:", kit))
                    .unwrap_or(domain)
                    .trim_end_matches(|c: char| c == ' ' || c == ';' || c == '.')
                    .to_string();

                if let Some(props) = types.get_mut(&class_name) {
                    props.push((current_prop.clone(), current_prop_type.clone(), false));
                }
            }
        }

        // A blank line or a new top-level definition ends the current property block
        if trimmed.is_empty() {
            current_prop.clear();
        }
    }

    types.into_iter().collect()
}

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
    let doctype_lower = doctype.to_lowercase();

    let matching_type = kit_types.iter().find(|(name, _)| name.to_lowercase() == doctype_lower);

    let (class_name, properties) = match matching_type {
        Some((name, props)) => (name.clone(), props.clone()),
        None => {
            let valid: Vec<String> = kit_types.iter().map(|(n, _)| n.to_lowercase()).collect();
            eprintln!(
                "Unknown document type '{}'. Valid types for kit '{}': {}",
                doctype, kit, valid.join(", ")
            );
            exit(1);
        }
    };

    // Generate filename in type-specific folder
    let title_str = title.unwrap_or("untitled");
    let slug = title_str
        .to_lowercase()
        .replace(' ', "-")
        .replace(|c: char| !c.is_alphanumeric() && c != '-', "");

    let type_folder = class_name.to_lowercase();
    let type_dir = root.join(&type_folder);
    fs::create_dir_all(&type_dir).ok();

    let filename = format!("{}.md", slug);
    let filepath = type_dir.join(&filename);
    let display_path = format!("{}/{}", type_folder, filename);

    if filepath.exists() {
        eprintln!("File already exists: {}", display_path);
        exit(1);
    }

    // Auto-generate agent email for Agent type
    let agent_email = format!("{}@lex.local", slug);

    // Build frontmatter
    let mut fm = String::new();
    fm.push_str("---\n");
    fm.push_str(&format!("title: \"{}\"\n", title_str));
    fm.push_str("tags: []\n");
    fm.push_str(&format!("{}:\n", kit));
    fm.push_str(&format!("  type: {}\n", class_name));

    for (prop_name, prop_type, _required) in &properties {
        // Auto-fill agentEmail for Agent type
        if prop_name == "agentEmail" && class_name == "Agent" {
            fm.push_str(&format!("  {}: \"{}\"\n", prop_name, agent_email));
        } else {
            match prop_type.as_str() {
                "string" => fm.push_str(&format!("  {}: \"\"\n", prop_name)),
                "reference" => fm.push_str(&format!("  {}: \n", prop_name)),
                _ => fm.push_str(&format!("  {}: \n", prop_name)),
            }
        }
    }

    fm.push_str("---\n\n");
    fm.push_str(&format!("# {}\n\n", title_str));
    fm.push_str("<!-- Write your content here -->\n");

    fs::write(&filepath, &fm).expect("failed to create document");
    println!("Created: {}", display_path);
    println!("Type: {}:{}", kit, class_name);
    if class_name == "Agent" {
        println!("Agent ID: {}", agent_email);
        println!("Use this as your git author: git -c user.email=\"{}\"", agent_email);
    }
    println!("Edit the file, then run 'git lex save' to commit.");
}

// ─── git lex save ──────────────────────────────────────────────

fn cmd_save(message: &str) {
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

fn cmd_extract() {
    let start = Instant::now();

    // Run frontmatter extraction (writes .spo sidecars as a side effect)
    generate_frontmatter_nquads();

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

    // Clear non-sync graphs (virtual graphs get regenerated)
    for graph_uri in &existing_graphs {
        if !graph_uri.contains("/sync/") {
            if let Ok(graph) = oxigraph::model::NamedNode::new(graph_uri) {
                store.clear_graph(&oxigraph::model::GraphName::from(graph)).ok();
            }
        }
    }

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
            let object_nq = if predicate == "isA" || predicate == "hasValue" || predicate == "mentions" || predicate == "linksTo" {
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
    println!("  Virtual: {} git + {} frontmatter", git_count, fm_count);
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
    println!("  Total sync graphs: {}", total_sync + if sync_count > 0 { 1 } else { 0 });
    println!("Store: {}", store_path().unwrap().display());
}

/// Add default prefixes to a query. Injects any standard prefixes not already declared.
fn add_prefixes(query: &str) -> String {
    // Get first commit SHA for content ontology prefix
    let first_commit = Command::new("git")
        .args(["rev-list", "--max-parents=0", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim()[..8].to_string())
        .unwrap_or_default();
    let o_prefix = format!("PREFIX o: <https://repolex.ai/ont/{}/>", first_commit);

    // Also read kit from repo.yml for squad: prefix
    let kit_prefix = find_git_root().and_then(|r| {
        let content = fs::read_to_string(r.join(".lex").join("repo.yml")).ok()?;
        for line in content.lines() {
            if let Some(kit) = line.strip_prefix("kit: ") {
                let kit = kit.trim();
                if kit != "none" {
                    return Some((
                        format!("{}:", kit),
                        format!("PREFIX {}: <https://repolex.ai/ontology/kit/{}/>", kit, kit),
                    ));
                }
            }
        }
        None
    });

    let mut defaults = vec![
        ("git:".to_string(), "PREFIX git: <https://repolex.ai/ontology/git-lex/git/>".to_string()),
        ("lex:".to_string(), "PREFIX lex: <https://repolex.ai/ontology/git-lex/lex/>".to_string()),
        ("fm:".to_string(), "PREFIX fm: <https://repolex.ai/ontology/git-lex/fm/>".to_string()),
        ("lex-o:".to_string(), "PREFIX lex-o: <https://repolex.ai/ontology/lex-upper/>".to_string()),
        ("o:".to_string(), o_prefix),
        ("rdf:".to_string(), "PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>".to_string()),
        ("rdfs:".to_string(), "PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>".to_string()),
        ("owl:".to_string(), "PREFIX owl: <http://www.w3.org/2002/07/owl#>".to_string()),
        ("xsd:".to_string(), "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>".to_string()),
    ];
    if let Some((short, full)) = kit_prefix {
        defaults.push((short, full));
    }
    let defaults = defaults;
    let upper = query.to_uppercase();
    let mut prefix_block = String::new();
    for (short, full) in &defaults {
        // Add if the prefix is used in the query but not declared
        if query.contains(short) && !upper.contains(&format!("PREFIX {}", short.to_uppercase())) {
            prefix_block.push_str(full);
            prefix_block.push('\n');
        }
    }
    // If no user prefixes at all, add everything
    if !upper.contains("PREFIX") {
        for (_, full) in &defaults {
            prefix_block.push_str(full);
            prefix_block.push('\n');
        }
    }
    format!("{}{}", prefix_block, query)
}

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
        Commands::Init { kit } => cmd_init(kit),
        Commands::Status => cmd_status(),
        Commands::Create { doctype, title } => cmd_create(&doctype, title.as_deref()),
        Commands::Save { message } => cmd_save(&message),
        Commands::Log { author, n, format } => cmd_log(author, n, format),
        Commands::Tree { r#ref, format } => cmd_tree(r#ref, format),
        Commands::Refs { format } => cmd_refs(format),
        Commands::Query { query } => cmd_query(query),
        Commands::Dump => {
            let git_nq = generate_git_nquads();
            let fm_nq = generate_frontmatter_nquads();
            let lex_nq = load_lex_nquads();
            print!("{}{}{}", git_nq, fm_nq, lex_nq);
        }
        Commands::Extract => cmd_extract(),
        Commands::Llm { command } => match command {
            LlmCommands::List => cmd_llm_list(),
            LlmCommands::Extract { file, model } => cmd_llm_extract(&file, &model),
            LlmCommands::Recheck { file, model } => cmd_llm_recheck(&file, &model),
        },
        Commands::Resolve { full } => cmd_resolve(full),
        Commands::Sync => cmd_sync(),
        Commands::Diff { since } => {
            println!(
                "git lex diff {} — not yet implemented",
                since.unwrap_or_else(|| "(working tree)".to_string())
            );
        }
    }
}
