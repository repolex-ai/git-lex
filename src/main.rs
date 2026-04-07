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
use tree_sitter;

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
    /// Join a squad repo (creates mutual identity binding)
    Join {
        /// Path to the squad repo to join
        squad_path: String,
    },
    /// Show this repo's identity
    Identity,
    /// Parse a markdown file and show the syntax tree (debug)
    Parse {
        /// File to parse
        file: String,
    },
    /// Start the visualization server (HTTP + WebSocket on localhost)
    Viz {
        /// Port to listen on
        #[arg(long, default_value = "7878")]
        port: u16,
    },
    /// Run a SPARQL CONSTRUCT query and push the result to the local viz server
    Display {
        /// SPARQL CONSTRUCT query (uses viz: namespace for rendering hints)
        query: String,
        /// Port the viz server is running on
        #[arg(long, default_value = "7878")]
        port: u16,
    },
    /// Manage kits (install, update, list)
    Kit {
        #[command(subcommand)]
        command: KitCommands,
    },
}

#[derive(Subcommand)]
enum KitCommands {
    /// Update the current kit (re-fetch from GitHub)
    Update,
    /// List available kits
    List,
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

/// Parse the git remote URL into (host, org, repo) components.
/// Falls back to ("localhost", "local", directory_name) if no remote.
fn get_repo_parts() -> (String, String, String) {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output();
    if let Ok(o) = output {
        if o.status.success() {
            let url = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let stripped = url.strip_suffix(".git").unwrap_or(&url);

            // HTTPS: https://github.com/org/repo
            if stripped.starts_with("https://") || stripped.starts_with("http://") {
                let without_scheme = stripped.split("://").nth(1).unwrap_or(stripped);
                let parts: Vec<&str> = without_scheme.splitn(4, '/').collect();
                if parts.len() >= 3 {
                    return (parts[0].to_string(), parts[1].to_string(), parts[2].to_string());
                }
            }

            // SSH: git@github.com:org/repo
            if let Some(at_pos) = stripped.find('@') {
                let after_at = &stripped[at_pos + 1..];
                if let Some(colon_pos) = after_at.find(':') {
                    let host = &after_at[..colon_pos];
                    let path = &after_at[colon_pos + 1..];
                    let parts: Vec<&str> = path.splitn(2, '/').collect();
                    if parts.len() == 2 {
                        return (host.to_string(), parts[0].to_string(), parts[1].to_string());
                    }
                }
            }
        }
    }
    // Fallback
    let dir_name = find_git_root()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "unknown".to_string());
    ("localhost".to_string(), "local".to_string(), dir_name)
}

/// Get the repo identifier (org/name) from the git remote, or fall back to directory name.
fn get_repo_id() -> String {
    let (_, org, repo) = get_repo_parts();
    format!("{}/{}", org, repo)
}

fn base_uri() -> String {
    let (host, org, repo) = get_repo_parts();
    format!("https://{}/{}/{}", host, org, repo)
}

/// Get the TTL prefix name for a kit (kit name may differ from prefix)
fn get_kit_prefix_name(kit_name: &str) -> &str {
    match kit_name {
        "claude-code" => "cc",
        "lex-lab" => "lab",
        other => other,
    }
}

/// Parse SHACL shapes TTL to extract inline hints for class template comments.
/// Returns a map of property name → hint string (e.g. "enum: certain, likely, hypothesis, hunch")
fn parse_shacl_hints(shapes_ttl: &str) -> HashMap<String, String> {
    let mut hints: HashMap<String, String> = HashMap::new();
    let mut current_path = String::new();
    let mut current_in_values: Vec<String> = Vec::new();
    let mut current_node_kind = String::new();
    let mut current_min_count: Option<u32> = None;

    for line in shapes_ttl.lines() {
        let trimmed = line.trim();

        // sh:path soul:confidence ;
        if trimmed.starts_with("sh:path ") {
            // Flush previous property
            if !current_path.is_empty() {
                let hint = build_shacl_hint(&current_in_values, &current_node_kind, current_min_count);
                if !hint.is_empty() {
                    hints.insert(current_path.clone(), hint);
                }
            }
            current_path = trimmed
                .strip_prefix("sh:path ").unwrap_or("")
                .trim_end_matches(|c: char| c == ' ' || c == ';')
                .to_string();
            current_in_values.clear();
            current_node_kind.clear();
            current_min_count = None;
        }

        // sh:in ( "certain" "likely" "hypothesis" "hunch" ) ;
        if trimmed.starts_with("sh:in") {
            // Extract values between ( and )
            if let Some(start) = trimmed.find('(') {
                if let Some(end) = trimmed.find(')') {
                    let values_str = &trimmed[start + 1..end];
                    current_in_values = values_str
                        .split('"')
                        .filter(|s| !s.trim().is_empty())
                        .map(|s| s.to_string())
                        .collect();
                }
            }
        }

        // sh:nodeKind sh:IRI ;
        if trimmed.starts_with("sh:nodeKind") {
            current_node_kind = trimmed
                .strip_prefix("sh:nodeKind ").unwrap_or("")
                .trim_end_matches(|c: char| c == ' ' || c == ';')
                .to_string();
        }

        // sh:minCount 1 ;
        if trimmed.starts_with("sh:minCount") {
            if let Some(num_str) = trimmed.split_whitespace().nth(1) {
                current_min_count = num_str.trim_end_matches(|c: char| c == ' ' || c == ';').parse().ok();
            }
        }
    }

    // Flush last property
    if !current_path.is_empty() {
        let hint = build_shacl_hint(&current_in_values, &current_node_kind, current_min_count);
        if !hint.is_empty() {
            hints.insert(current_path, hint);
        }
    }

    hints
}

fn build_shacl_hint(in_values: &[String], node_kind: &str, min_count: Option<u32>) -> String {
    let required = min_count.map_or("optional", |n| if n > 0 { "required" } else { "optional" });
    if !in_values.is_empty() {
        format!("{}, enum: {}", required, in_values.join(", "))
    } else if node_kind == "sh:IRI" {
        format!("{}, IRI", required)
    } else {
        format!("{}, str", required)
    }
}

/// Resolve a slug to a full IRI using the slug index.
/// If found in the index, generates a proper Class/file.md IRI.
/// Otherwise falls back to entity/{slug}.
fn resolve_slug_to_uri(slug: &str, base: &str, slug_index: &HashMap<String, String>) -> String {
    if let Some(rel_path) = slug_index.get(slug) {
        // Found a matching file — build IRI from its path
        let path_parts: Vec<&str> = rel_path.splitn(2, '/').collect();
        if path_parts.len() == 2 && rel_path.ends_with(".md") {
            let folder = path_parts[0];
            let file = path_parts[1];
            let class_name = {
                let mut c = folder.chars();
                match c.next() {
                    None => folder.to_string(),
                    Some(f) => f.to_uppercase().to_string() + c.as_str(),
                }
            };
            format!("<{}/{}/{}>", base, uri_encode_path(&class_name), uri_encode_path(file))
        } else {
            format!("<{}/{}>", base, uri_encode_path(rel_path))
        }
    } else {
        // No matching file — fall back to entity URI
        format!("<{}/entity/{}>", base, uri_encode_path(slug))
    }
}

/// Escape a string for use in N-Quads literals.
fn nq_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Unescape a git-quoted path.
/// Git wraps paths with non-ASCII chars in double quotes and uses octal escapes.
/// e.g. "message/list_messages-\342\200\224-foo.md" → message/list_messages-—-foo.md
fn git_unescape_path(s: &str) -> String {
    let s = if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        &s[1..s.len() - 1]
    } else {
        return s.to_string();
    };
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'n' => { result.push(b'\n'); i += 2; }
                b't' => { result.push(b'\t'); i += 2; }
                b'r' => { result.push(b'\r'); i += 2; }
                b'\\' => { result.push(b'\\'); i += 2; }
                b'"' => { result.push(b'"'); i += 2; }
                // Octal escape: \NNN
                d if d.is_ascii_digit() && i + 3 < bytes.len()
                    && bytes[i + 2].is_ascii_digit()
                    && bytes[i + 3].is_ascii_digit() =>
                {
                    let octal = (d - b'0') as u32 * 64
                        + (bytes[i + 2] - b'0') as u32 * 8
                        + (bytes[i + 3] - b'0') as u32;
                    result.push(octal as u8);
                    i += 4;
                }
                _ => { result.push(bytes[i]); i += 1; }
            }
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&result).into_owned()
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
// Base ontologies — embedded in the binary, always installed
const ONT_GIT: &str = include_str!("../ontology/git-lex/git/git.ttl");
const ONT_FM: &str = include_str!("../ontology/git-lex/fm/fm.ttl");
const ONT_LEX: &str = include_str!("../ontology/git-lex/lex/lex.ttl");
const ONT_LEX_O: &str = include_str!("../ontology/git-lex/lex-o/lex-o.ttl");
// Kit ontologies are fetched from GitHub at init time — no embedded fallback.

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

    let kit_name = kit.as_deref().unwrap_or("none");

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

    // Install kit ontology from GitHub
    if let Some(ref k) = kit {
        let kit_dir = ont_dir.join(format!("kit/{}", k));
        fs::create_dir_all(&kit_dir).ok();

        if fetch_kit_from_github(k, &kit_dir) {
            println!("Kit '{}' fetched from GitHub.", k);
        } else {
            eprintln!("Failed to fetch kit '{}' from GitHub.", k);
            eprintln!("Check that {}/git-lex-kit-{} exists and you have network access.", KIT_GITHUB_ORG, k);
            exit(1);
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
    let ignore_entries = if kit_name == "claude-code" {
        // Claude Code kit: whitelist approach — ignore everything except what we index
        "*\n\
         !.gitignore\n\
         !.gitattributes\n\
         !.lex/\n\
         !.lex/**\n\
         .lex/oxigraph/\n\
         .lex/raw/\n\
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
         !history.jsonl\n"
    } else {
        ".lex/oxigraph/\n.lex/raw/\n"
    };
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

    // Create type folders from kit ontology (only if kit.yml says createTypeFolders: true)
    if kit.is_some() {
        let create_folders = kit_config_bool(kit_name, "createTypeFolders", false);
        let kit_types = get_kit_types(kit_name);
        if create_folders {
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
            }
        }
        let type_names: Vec<String> = kit_types.iter().map(|(n, _)| n.to_lowercase()).collect();
        if !kit_types.is_empty() {

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
                doc.push_str("Documents use YAML frontmatter with flat dot notation: `kit.class.property`\n\n");
                doc.push_str("```yaml\n");
                doc.push_str("---\n");
                doc.push_str(&format!("{}.memory.confidence: \"certain\"\n", kit_name));
                doc.push_str(&format!("{}.memory.source: \"observation\"\n", kit_name));
                doc.push_str(&format!("{}.memory.category: \"fact\"\n", kit_name));
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
                doc.push_str(&format!("## {} Kit — Document Types\n\n", kit_name));
                for (type_name, properties) in &kit_types {
                    doc.push_str(&format!("### {}\n\n", type_name));
                    doc.push_str(&format!("Create: `git lex create {}`\n\n", type_name.to_lowercase()));
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

    // Generate SHACL shapes from ontology, then class templates
    if kit.is_some() {
        if let Some(shapes_path) = build_shacl_shapes(kit_name) {
            println!("SHACL shapes generated: {}", shapes_path.file_name().unwrap_or_default().to_string_lossy());
        }
    }
    if kit.is_some() {
        let kit_types = get_kit_types(kit_name);
        let shapes_content = {
            let r = find_git_root().unwrap();
            let shapes_path = r.join(".lex").join("ontology").join("kit").join(kit_name).join(format!("{}-shapes.ttl", kit_name));
            fs::read_to_string(&shapes_path).unwrap_or_default()
        };
        let shacl_hints = parse_shacl_hints(&shapes_content);

        for (type_name, properties) in &kit_types {
            let type_lower = type_name.to_lowercase();
            let type_dir = root.join(&type_lower);
            let template_path = type_dir.join(format!("__{}.md", type_name));

            if !template_path.exists() {
                let mut tmpl = String::new();
                tmpl.push_str("---\n");

                for (prop_name, prop_type, _required, _comment) in properties {
                    // Property names pass through as-is from the ontology (camelCase)
                    let key = format!("{}.{}.{}", kit_name, type_lower, prop_name);

                    // Look up SHACL hint for this property
                    let prefix_name = get_kit_prefix_name(kit_name);
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

    // Install scaffold files from the kit (SOUL.md, AGENTS.md, skills, journal, etc.)
    // These live in scaffold/ inside the kit repo and mirror the target repo structure.
    // Never overwrites existing files. Supports {kit} template variable.
    if kit.is_some() {
        let scaffold_count = install_scaffold_files(kit_name);
        if scaffold_count > 0 {
            println!("Installed {} scaffold file(s) from kit", scaffold_count);
        }

        // Ensure journal/ exists even if no scaffold (backward compat)
        let journal_dir = root.join("journal");
        if !journal_dir.exists() {
            fs::create_dir_all(&journal_dir).ok();
            let gitkeep = journal_dir.join(".gitkeep");
            fs::write(&gitkeep, "").ok();
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
    // Post-merge/Post-receive hooks for squad/lab kits (message notifications)
    if let Some(ref k) = kit {
        if k == "squad" || k == "lab" {
            let hook_content = "#!/bin/bash\n# git-lex-mail-notifier\n\n# Detect current agent handle (from SOUL.md or first agent file in repo)\nHANDLE=$(grep -oE \"Display Name: .*\" SOUL.md 2>/dev/null | cut -d: -f2 | xargs)\nif [ -z \"$HANDLE\" ]; then\n    HANDLE=$(ls agent/ 2>/dev/null | head -n 1 | cut -d. -f1)\nfi\n\nif [ ! -z \"$HANDLE\" ]; then\n    MAIL_COUNT=$(git lex query \"SELECT ?msg WHERE { ?msg a squad:Message ; squad:to ?to ; squad:messageStatus 'open' . FILTER(regex(str(?to), '$HANDLE', 'i')) }\" | grep -c \"^|\" | awk '{print $1 - 2}')\n\n    if [[ \"$MAIL_COUNT\" -gt 0 ]]; then\n      echo -e \"\\n\\033[1;33m🔔 YOU'VE GOT MAIL ($MAIL_COUNT new message(s) for $HANDLE)!\\033[0m\"\n      echo -e \"\\033[0;36mRun 'git lex query' or check the 'message/' directory.\\033[0m\\n\"\n    fi\nfi\n";

            for hook_name in &["post-merge", "post-receive", "post-commit"] {
                let hook_path = hooks_dir.join(hook_name);
                if !hook_path.exists() {
                    fs::write(&hook_path, &hook_content).ok();
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).ok();
                    }
                }
            }
            println!("Installed {} and {} hooks (message notifications)", "post-merge", "post-receive");
        }
    }

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

    // Write .lex/identity.yml with first commit SHA (the repo's cryptographic identity)
    let identity_path = root.join(".lex").join("identity.yml");
    if !identity_path.exists() {
        let first_sha = Command::new("git")
            .args(["rev-list", "--max-parents=0", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();

        if !first_sha.is_empty() {
            let now = Command::new("date").args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
                .output().ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_else(|| "unknown".to_string());

            let identity = format!(
                "# git-lex identity — do not edit\n\
                 # This file anchors this repo's cryptographic identity.\n\
                 # The SHA below is the first commit hash — immutable and unique.\n\
                 identity: {}\n\
                 created: {}\n\
                 kit: {}\n",
                first_sha, now, kit_name
            );
            fs::write(&identity_path, &identity).ok();

            // Commit the identity file
            let _ = Command::new("git").args(["add", ".lex/identity.yml"]).status();
            let _ = Command::new("git").args(["commit", "-m", "git lex identity"]).status();
            println!("Identity: {}", first_sha);
        }
    }
}

// ─── Kit fetch (GitHub tarball) ─────────────────────────────────

/// The GitHub org where kits are published
const KIT_GITHUB_ORG: &str = "repolex-ai";
// Kit repos live at: github.com/repolex-ai/git-lex-kit-{name}

/// Fetch a kit from GitHub as a tarball and extract it into the target directory.
/// Returns true if the fetch succeeded, false if it failed (caller should fall back to embedded).
fn fetch_kit_from_github(kit_name: &str, target_dir: &std::path::Path) -> bool {
    let repo_name = format!("git-lex-kit-{}", kit_name);
    let url = format!(
        "https://github.com/{}/{}/archive/refs/heads/main.tar.gz",
        KIT_GITHUB_ORG, repo_name
    );

    // Create a temp dir for extraction
    let tmp_dir = std::env::temp_dir().join(format!("git-lex-kit-{}", kit_name));
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).ok();

    // Download and extract with curl + tar
    let status = Command::new("curl")
        .args(["-sL", "--fail", "-o", "-", &url])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|curl| {
            Command::new("tar")
                .args(["xzf", "-", "-C", &tmp_dir.to_string_lossy(), "--strip-components=1"])
                .stdin(curl.stdout.unwrap())
                .status()
        });

    match status {
        Ok(s) if s.success() => {
            // Verify we actually got files (curl --fail should prevent empty extracts, but be safe)
            let has_files = fs::read_dir(&tmp_dir).ok()
                .map(|entries| entries.count() > 0)
                .unwrap_or(false);

            if !has_files {
                let _ = fs::remove_dir_all(&tmp_dir);
                return false;
            }

            // Copy files from temp dir to target
            if let Ok(entries) = fs::read_dir(&tmp_dir) {
                fs::create_dir_all(target_dir).ok();
                for entry in entries.flatten() {
                    let src = entry.path();
                    let dest = target_dir.join(entry.file_name());
                    if src.is_file() {
                        fs::copy(&src, &dest).ok();
                    } else if src.is_dir() {
                        copy_dir_recursive(&src, &dest).ok();
                    }
                }
            }
            let _ = fs::remove_dir_all(&tmp_dir);
            true
        }
        _ => {
            let _ = fs::remove_dir_all(&tmp_dir);
            false
        }
    }
}

/// Install scaffold files from the kit into the repo root.
/// Scaffold files live in .lex/ontology/kit/{name}/scaffold/ and mirror the repo structure.
/// Never overwrites existing files. Supports {kit} template variable in file contents.
fn install_scaffold_files(kit_name: &str) -> usize {
    let root = match find_git_root() {
        Some(r) => r,
        None => return 0,
    };

    let scaffold_dir = root.join(".lex").join("ontology").join("kit").join(kit_name).join("scaffold");
    if !scaffold_dir.exists() {
        return 0;
    }

    let mut count = 0;

    fn install_recursive(src_dir: &std::path::Path, dest_dir: &std::path::Path, kit_name: &str, count: &mut usize) {
        if let Ok(entries) = fs::read_dir(src_dir) {
            for entry in entries.flatten() {
                let src = entry.path();
                let name = entry.file_name();
                let dest = dest_dir.join(&name);

                if src.is_dir() {
                    fs::create_dir_all(&dest).ok();
                    install_recursive(&src, &dest, kit_name, count);
                } else if src.is_file() {
                    if !dest.exists() {
                        // Read content and substitute {kit} template variable
                        if let Ok(content) = fs::read_to_string(&src) {
                            let processed = content.replace("{kit}", kit_name);
                            fs::create_dir_all(dest.parent().unwrap_or(&dest)).ok();
                            fs::write(&dest, &processed).ok();
                            *count += 1;
                        }
                    }
                }
            }
        }
    }

    install_recursive(&scaffold_dir, &root, kit_name, &mut count);
    count
}

fn copy_dir_recursive(src: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dest_path)?;
        } else {
            fs::copy(&src_path, &dest_path)?;
        }
    }
    Ok(())
}

fn cmd_kit_update() {
    let root = match find_git_root() {
        Some(r) => r,
        None => { eprintln!("fatal: not a git repository"); exit(1); }
    };

    let kit = match get_kit() {
        Some(k) => k,
        None => { eprintln!("No kit configured."); exit(1); }
    };

    let kit_dir = root.join(".lex").join("ontology").join("kit").join(&kit);
    println!("Updating kit '{}'...", kit);

    if fetch_kit_from_github(&kit, &kit_dir) {
        println!("Kit '{}' updated from GitHub.", kit);

        // Generate SHACL shapes from ontology (single source of truth)
        if let Some(shapes_path) = build_shacl_shapes(&kit) {
            println!("SHACL shapes generated: {}", shapes_path.file_name().unwrap_or_default().to_string_lossy());
        }

        // Regenerate class templates using the generated shapes for hints
        let kit_types = get_kit_types(&kit);
        let shapes_content = {
            let shapes_path = kit_dir.join(format!("{}-shapes.ttl", kit));
            fs::read_to_string(&shapes_path).unwrap_or_default()
        };
        let shacl_hints = parse_shacl_hints(&shapes_content);

        for (type_name, properties) in &kit_types {
            let type_lower = type_name.to_lowercase();
            let type_dir = root.join(&type_lower);
            fs::create_dir_all(&type_dir).ok();
            let template_path = type_dir.join(format!("__{}.md", type_name));

            // Always overwrite templates on update
            let mut tmpl = String::new();
            tmpl.push_str("---\n");

            for (prop_name, prop_type, _required, _comment) in properties {
                // Property names pass through as-is from the ontology (camelCase)
                let key = format!("{}.{}.{}", kit, type_lower, prop_name);

                let prefix_name = get_kit_prefix_name(&kit);
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
        println!("Class templates regenerated.");
    } else {
        eprintln!("Failed to fetch kit '{}' from GitHub. Is the repo repolex-ai/git-lex-kit-{} accessible?", kit, kit);
    }
}

fn cmd_kit_list() {
    println!("Official kits:");
    println!("  soul    — An agent's persistent mind (memory, decision, exploration, friend, journal, skill, mantra, routine, resource, creation, interest, note, task)");
    println!("  squad   — Multi-agent team collaboration (agent, message, decision, task, project, note)");
    println!("  collab  — Two-party shared workspace (idea, question, reference, decision, note)");
    println!();
    println!("Custom kits: any GitHub repo at {}/git-lex-kit-<name>", KIT_GITHUB_ORG);
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
    let base = base_uri();

    // Repo metadata from .lex/repo.yml — name, kit, version
    if let Some(root) = find_git_root() {
        let repo_yml_path = root.join(".lex").join("repo.yml");
        if let Ok(content) = fs::read_to_string(&repo_yml_path) {
            let repo_uri = format!("<{}>", base);
            let graph = format!("<{}/repo>", base);
            nq.push_str(&format!(
                "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/git/Repo> {} .\n",
                repo_uri, graph
            ));
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') { continue; }
                if let Some(idx) = line.find(':') {
                    let key = line[..idx].trim();
                    let val = line[idx + 1..].trim().trim_matches('"');
                    if !val.is_empty() {
                        nq.push_str(&format!(
                            "{} <https://repolex.ai/ontology/git-lex/git/{}> \"{}\" {} .\n",
                            repo_uri, key, nq_escape(val), graph
                        ));
                    }
                }
            }
        }
    }

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
                    let path = git_unescape_path(parts[1]);
                    let meta: Vec<&str> = parts[0].split_whitespace().collect();
                    if meta.len() < 4 { continue; }
                    let (obj_type, blob_hash, size) = (meta[1], meta[2], meta[3]);
                    let fu = format!("<{}/tree/{}/{}>", base, ref_sha, uri_encode_path(&path));
                    nq.push_str(&format!("{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/git/Blob> {} .\n", fu, graph));
                    nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/path> \"{}\" {} .\n", fu, nq_escape(&path), graph));
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
                let path = git_unescape_path(parts[1]);
                let graph = format!("<{}/changeset/{}>", base, current_sha);
                let change_uri = format!("<{}/changeset/{}/{}>", base, current_sha, uri_encode_path(&path));
                let commit_uri = format!("<{}/commit/{}>", base, current_sha);

                // Link commit to changeset (in commits graph so joins work)
                let commits_graph = format!("<{}/commits>", base);
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/changed> {} {} .\n", commit_uri, change_uri, commits_graph));

                // Change details
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/path> \"{}\" {} .\n", change_uri, nq_escape(&path), graph));

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
                    let renamed_to = git_unescape_path(parts[2]);
                    nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/git/renamedTo> \"{}\" {} .\n", change_uri, nq_escape(&renamed_to), graph));
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

/// Open the persistent store in read-only mode. Does not acquire the
/// RocksDB write lock, so writers (`git lex sync`, `git lex save`) can run
/// concurrently. The view is a snapshot from open-time and will not reflect
/// later writes until the store is reopened.
fn open_store_read_only() -> Option<Store> {
    let path = store_path()?;
    if path.exists() {
        Store::open_read_only(&path).ok()
    } else {
        None
    }
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
// ─── JSONL extractor (for claude-code kit) ─────────────────────

/// Extract structural metadata from .jsonl conversation files.
/// Writes .spo sidecars with session metadata.
fn extract_jsonl_sessions() {
    let root = match find_git_root() {
        Some(r) => r,
        None => return,
    };

    // Only run for claude-code kit
    let kit = get_kit();
    if kit.as_deref() != Some("claude-code") {
        return;
    }

    let extract_dir = root.join(".lex").join("extract");
    fs::create_dir_all(&extract_dir).ok();

    // Find all .jsonl files (not in .lex/)
    let mut jsonl_files = Vec::new();
    fn walk_jsonl(dir: &std::path::Path, files: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') { continue; }
                if path.is_dir() {
                    walk_jsonl(&path, files);
                } else if name.ends_with(".jsonl") {
                    files.push(path);
                }
            }
        }
    }
    walk_jsonl(&root, &mut jsonl_files);

    for filepath in &jsonl_files {
        let relpath = filepath.strip_prefix(&root).unwrap_or(filepath);
        let relpath_str = relpath.to_string_lossy().to_string();

        // Check for incremental: read meta file for last processed line
        let meta_path = extract_dir.join(format!("{}.meta", relpath_str));
        let last_line: usize = fs::read_to_string(&meta_path)
            .ok()
            .and_then(|s| {
                for line in s.lines() {
                    if let Some(n) = line.strip_prefix("last_line: ") {
                        return n.trim().parse().ok();
                    }
                }
                None
            })
            .unwrap_or(0);

        // Read the file
        let content = match fs::read_to_string(filepath) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        // Skip if no new lines
        if last_line >= total_lines {
            continue;
        }

        // Parse all lines (or just new ones for incremental)
        let mut session_id = String::new();
        let mut project_path = String::new();
        let mut first_timestamp = String::new();
        let mut last_timestamp = String::new();
        let mut cwd = String::new();
        let mut version = String::new();
        let mut git_branch = String::new();
        let mut user_count: usize = 0;
        let mut assistant_count: usize = 0;
        let mut system_count: usize = 0;
        let mut channel_count: usize = 0;
        let mut tool_counts: HashMap<String, usize> = HashMap::new();

        for (i, line) in lines.iter().enumerate() {
            let line = line.trim();
            if line.is_empty() { continue; }

            let obj: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let msg_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

            // Extract session metadata from first valid message
            if session_id.is_empty() {
                if let Some(sid) = obj.get("sessionId").and_then(|v| v.as_str()) {
                    session_id = sid.to_string();
                }
            }
            if cwd.is_empty() {
                if let Some(c) = obj.get("cwd").and_then(|v| v.as_str()) {
                    cwd = c.to_string();
                }
            }
            if version.is_empty() {
                if let Some(v) = obj.get("version").and_then(|v| v.as_str()) {
                    version = v.to_string();
                }
            }
            if git_branch.is_empty() {
                if let Some(b) = obj.get("gitBranch").and_then(|v| v.as_str()) {
                    git_branch = b.to_string();
                }
            }

            // Track timestamps
            if let Some(ts) = obj.get("timestamp").and_then(|v| v.as_str()) {
                if first_timestamp.is_empty() {
                    first_timestamp = ts.to_string();
                }
                last_timestamp = ts.to_string();
            }

            // Count message types
            match msg_type {
                "user" => user_count += 1,
                "assistant" => {
                    assistant_count += 1;
                    // Count tool usage
                    if let Some(content) = obj.get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_array())
                    {
                        for item in content {
                            if item.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                                if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                                    *tool_counts.entry(name.to_string()).or_insert(0) += 1;
                                }
                            }
                        }
                    }
                }
                "system" => system_count += 1,
                "queue-operation" => channel_count += 1,
                _ => {}
            }
        }

        // Derive project from parent directory name
        if project_path.is_empty() {
            if let Some(parent) = relpath.parent() {
                project_path = parent.to_string_lossy().to_string();
            }
        }

        // Generate .spo
        let session_name = if !session_id.is_empty() {
            format!("session-{}", &session_id[..8.min(session_id.len())])
        } else {
            let stem = filepath.file_stem().unwrap_or_default().to_string_lossy();
            format!("session-{}", &stem[..8.min(stem.len())])
        };

        let mut spo_lines = Vec::new();
        spo_lines.push(format!("{} | isA | session", session_name));

        if !session_id.is_empty() {
            spo_lines.push(format!("{} | sessionId | {}", session_name, session_id));
        }
        if !project_path.is_empty() {
            spo_lines.push(format!("{} | project | {}", session_name, project_path));
        }
        if !first_timestamp.is_empty() {
            spo_lines.push(format!("{} | startTime | {}", session_name, first_timestamp));
        }
        if !last_timestamp.is_empty() {
            spo_lines.push(format!("{} | endTime | {}", session_name, last_timestamp));
        }
        if !cwd.is_empty() {
            spo_lines.push(format!("{} | cwd | {}", session_name, cwd));
        }
        if !version.is_empty() {
            spo_lines.push(format!("{} | ccVersion | {}", session_name, version));
        }
        if !git_branch.is_empty() {
            spo_lines.push(format!("{} | gitBranch | {}", session_name, git_branch));
        }

        spo_lines.push(format!("{} | messageCount | {}", session_name, total_lines));
        spo_lines.push(format!("{} | userMessageCount | {}", session_name, user_count));
        spo_lines.push(format!("{} | assistantMessageCount | {}", session_name, assistant_count));

        if channel_count > 0 {
            spo_lines.push(format!("{} | channelMessageCount | {}", session_name, channel_count));
        }

        // Tool usage
        let mut tools: Vec<(&String, &usize)> = tool_counts.iter().collect();
        tools.sort_by(|a, b| b.1.cmp(a.1));
        for (tool, count) in &tools {
            spo_lines.push(format!("{} | toolUsage | {}:{}", session_name, tool, count));
        }

        // Sort and write sidecar
        spo_lines.sort();
        spo_lines.dedup();

        let spo_path = extract_dir.join(format!("{}.cc.spo", relpath_str));
        fs::create_dir_all(spo_path.parent().unwrap()).ok();
        fs::write(&spo_path, spo_lines.join("\n") + "\n").ok();

        // Write meta for incremental
        fs::create_dir_all(meta_path.parent().unwrap()).ok();
        fs::write(&meta_path, format!("last_line: {}\nlast_sync: {}\n", total_lines, last_timestamp)).ok();
    }
}

fn generate_frontmatter_nquads() -> String {
    let root = match find_git_root() {
        Some(r) => r,
        None => return String::new(),
    };

    let base = base_uri();
    let graph = format!("<{}/frontmatter>", base);
    let mut nq = String::new();

    // Build ObjectProperty lookup from kit ontology
    let obj_props = get_kit().map(|k| get_object_properties(&k)).unwrap_or_default();
    let prop_datatypes = get_kit().map(|k| get_property_datatypes(&k)).unwrap_or_default();

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

    // Build slug-to-path index for reference resolution
    // Maps "spacegoat" → "agent/spacegoat.md", "use-oxigraph-for-sparql" → "decision/use-oxigraph-for-sparql.md"
    let mut slug_index: HashMap<String, String> = HashMap::new();
    for f in &files {
        if let Ok(rel) = f.strip_prefix(&root) {
            let rel_str = rel.to_string_lossy().to_string();
            // Extract slug from filename (without .md extension)
            if let Some(file_name) = f.file_stem() {
                let slug = file_name.to_string_lossy().to_lowercase();
                // Skip template files
                if !slug.starts_with("__") {
                    slug_index.insert(slug, rel_str);
                }
            }
        }
    }

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
        // IRI scheme: https://{host}/{org}/{repo}/{Class}/{id}.md
        // For files in a class folder (e.g. memory/foo.md), use the folder as class.
        // Otherwise fall back to the blob-hash based URI.
        let doc_uri = {
            let path_parts: Vec<&str> = relpath_str.splitn(2, '/').collect();
            if path_parts.len() == 2 && relpath_str.ends_with(".md") {
                // e.g. memory/karpathy-validates.md → /Memory/karpathy-validates.md
                let folder = path_parts[0];
                let file = path_parts[1];
                // Capitalize folder name to match class convention
                let class_name = {
                    let mut c = folder.chars();
                    match c.next() {
                        None => folder.to_string(),
                        Some(f) => f.to_uppercase().to_string() + c.as_str(),
                    }
                };
                format!("<{}/{}/{}>", base, uri_encode_path(&class_name), uri_encode_path(file))
            } else {
                // Top-level files or non-.md — use path-based URI
                format!("<{}/{}>", base, uri_encode_path(&relpath_str))
            }
        };

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

        // Track which kit types we've seen for rdf:type emission (dedup)
        let mut emitted_types: HashSet<String> = HashSet::new();

        for line in &spo_lines {
            let parts: Vec<&str> = line.splitn(3, " | ").collect();
            if parts.len() == 3 {
                let subject = parts[0];
                let predicate = parts[1];
                let object = parts[2];

                if predicate == "mentions" {
                    // @mention → lex:mentions — resolve to IRI if file exists
                    let mention_slug = object.to_lowercase();
                    if slug_index.contains_key(&mention_slug) {
                        let mention_uri = resolve_slug_to_uri(&mention_slug, &base, &slug_index);
                        nq.push_str(&format!(
                            "{} <https://repolex.ai/ontology/git-lex/lex/mentions> {} {} .\n",
                            doc_uri, mention_uri, graph
                        ));
                    } else {
                        // No matching file — keep as literal
                        nq.push_str(&format!(
                            "{} <https://repolex.ai/ontology/git-lex/lex/mentions> \"{}\" {} .\n",
                            doc_uri, nq_escape(object), graph
                        ));
                    }
                } else if predicate == "linksTo" {
                    // [[wikilink]] → lex:linksTo (resolved) or lex:unresolvedLink (broken)
                    let link_slug = object.to_lowercase()
                        .replace(' ', "-")
                        .replace(|c: char| !c.is_alphanumeric() && c != '-', "");
                    if slug_index.contains_key(&link_slug) {
                        let link_uri = resolve_slug_to_uri(&link_slug, &base, &slug_index);
                        nq.push_str(&format!(
                            "{} <https://repolex.ai/ontology/git-lex/lex/linksTo> {} {} .\n",
                            doc_uri, link_uri, graph
                        ));
                    } else {
                        nq.push_str(&format!(
                            "{} <https://repolex.ai/ontology/git-lex/lex/unresolvedLink> \"{}\" {} .\n",
                            doc_uri, nq_escape(object), graph
                        ));
                    }
                } else {
                    // Check for three-segment dot notation: kit.class.property
                    let segments: Vec<&str> = subject.splitn(3, '.').collect();

                    if segments.len() == 3 {
                        // New dot notation: kit.class.property
                        let kit_name = segments[0];
                        let class_seg = segments[1];
                        let prop_seg = segments[2];

                        // Emit rdf:type from class segment (once per class)
                        let type_key = format!("{}.{}", kit_name, class_seg);
                        if emitted_types.insert(type_key) {
                            // Capitalize class name: "memory" → "Memory"
                            let class_capitalized = {
                                let mut c = class_seg.chars();
                                match c.next() {
                                    None => class_seg.to_string(),
                                    Some(f) => f.to_uppercase().to_string() + c.as_str(),
                                }
                            };
                            let type_uri = format!("<https://repolex.ai/ontology/kit/{}/{}>", kit_name, class_capitalized);
                            nq.push_str(&format!(
                                "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> {} {} .\n",
                                doc_uri, type_uri, graph
                            ));
                        }

                        // Property name passes through as-is (camelCase from ontology)
                        let kit_predicate = format!("<https://repolex.ai/ontology/kit/{}/{}>", kit_name, prop_seg);

                        // Check if this is an ObjectProperty (from ontology) → resolve as IRI
                        if obj_props.contains(prop_seg) {
                            // ObjectProperty: split on commas, resolve each as IRI
                            let values: Vec<&str> = object.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
                            for val in values {
                                if val.is_empty() { continue; }
                                let slug = val.trim_start_matches('@').to_lowercase()
                                    .replace(' ', "-")
                                    .replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '/' && c != '.', "");
                                if slug.is_empty() { continue; }
                                let object_uri = if slug.contains('/') || slug.ends_with(".md") {
                                    format!("<{}/{}>", base, uri_encode_path(&slug))
                                } else {
                                    resolve_slug_to_uri(&slug, &base, &slug_index)
                                };
                                nq.push_str(&format!(
                                    "{} {} {} {} .\n",
                                    doc_uri, kit_predicate, object_uri, graph
                                ));
                            }
                        } else {
                            // DatatypeProperty: emit as typed literal if ontology specifies a non-string range
                            if let Some(datatype) = prop_datatypes.get(prop_seg) {
                                nq.push_str(&format!(
                                    "{} {} \"{}\"^^<{}> {} .\n",
                                    doc_uri, kit_predicate, nq_escape(object), datatype, graph
                                ));
                            } else {
                                nq.push_str(&format!(
                                    "{} {} \"{}\" {} .\n",
                                    doc_uri, kit_predicate, nq_escape(object), graph
                                ));
                            }
                        }
                    } else {
                        // Legacy or non-kit frontmatter (title, tags, etc.) — use fm: namespace
                        let fm_predicate = format!("<https://repolex.ai/ontology/git-lex/fm/{}>", uri_encode_path(subject));

                        if subject.ends_with("-link") || subject.ends_with("-links") {
                            let values: Vec<&str> = if subject.ends_with("-links") {
                                object.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect()
                            } else {
                                vec![object.trim()]
                            };
                            for val in values {
                                if val.is_empty() { continue; }
                                let slug = val.trim_start_matches('@').to_lowercase()
                                    .replace(' ', "-")
                                    .replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '/' && c != '.', "");
                                if slug.is_empty() { continue; }
                                let object_uri = if slug.contains('/') || slug.ends_with(".md") {
                                    format!("<{}/{}>", base, uri_encode_path(&slug))
                                } else {
                                    resolve_slug_to_uri(&slug, &base, &slug_index)
                                };
                                nq.push_str(&format!(
                                    "{} {} {} {} .\n",
                                    doc_uri, fm_predicate, object_uri, graph
                                ));
                            }
                        } else {
                            nq.push_str(&format!(
                                "{} {} \"{}\" {} .\n",
                                doc_uri, fm_predicate, nq_escape(object), graph
                            ));
                        }
                    }
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

// ─── Ontology Builder ──────���───────────────────────────────────
// Loads kit TTL into oxigraph, queries OWL constraints, generates SHACL shapes.
// Single source of truth: the TTL. Shapes are derived artifacts.

/// Read a boolean config value from the kit's kit.yml file.
/// Returns the default if the file doesn't exist or the key isn't found.
fn kit_config_bool(kit: &str, key: &str, default: bool) -> bool {
    let root = match find_git_root() {
        Some(r) => r,
        None => return default,
    };
    let config_path = root.join(".lex").join("ontology").join("kit").join(kit).join("kit.yml");
    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return default,
    };
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') { continue; }
        if let Some(val) = line.strip_prefix(&format!("{}:", key)) {
            let val = val.trim();
            return val == "true" || val == "yes";
        }
    }
    default
}

/// Find the kit TTL file path. Tries {kit}.ttl first, then any .ttl in the kit dir.
fn find_kit_ttl(kit: &str) -> Option<PathBuf> {
    let root = find_git_root()?;
    let kit_dir = root.join(".lex").join("ontology").join("kit").join(kit);
    let primary = kit_dir.join(format!("{}.ttl", kit));
    if primary.exists() {
        return Some(primary);
    }
    fs::read_dir(&kit_dir).ok()
        .and_then(|entries| entries.filter_map(|e| e.ok())
            .find(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.ends_with(".ttl") && !name.contains("shapes")
            })
            .map(|e| e.path()))
}

/// Load a kit TTL into an in-memory oxigraph store for SPARQL querying.
fn load_kit_into_store(kit: &str) -> Option<Store> {
    let ttl_path = find_kit_ttl(kit)?;
    let content = fs::read_to_string(&ttl_path).ok()?;
    let store = Store::new().ok()?;
    store.load_from_reader(RdfFormat::Turtle, Cursor::new(content.as_bytes())).ok()?;
    Some(store)
}

/// Generate SHACL shapes TTL from a kit ontology using SPARQL queries.
/// Reads OWL constraints (owl:oneOf, owl:Restriction, owl:ObjectProperty, rdfs:range)
/// and emits equivalent SHACL shapes.
fn generate_shacl_shapes(kit: &str) -> Option<String> {
    let store = load_kit_into_store(kit)?;
    let ttl_path = find_kit_ttl(kit)?;
    let ttl_content = fs::read_to_string(&ttl_path).ok()?;

    // Find the kit prefix name and namespace from the TTL
    let kit_ns_pattern = format!("/kit/{}/", kit);
    let mut prefix_name = kit.to_string();
    let mut namespace = format!("https://repolex.ai/ontology/kit/{}/", kit);
    for line in ttl_content.lines() {
        if line.starts_with("@prefix ") && line.contains(&kit_ns_pattern) {
            if let Some(colon_pos) = line[8..].find(':') {
                prefix_name = line[8..8 + colon_pos].trim().to_string();
            }
            if let Some(start) = line.find('<') {
                if let Some(end) = line.find('>') {
                    namespace = line[start + 1..end].to_string();
                }
            }
            break;
        }
    }

    // Helper: extract local name from full IRI
    let local_name = |iri: &str| -> String {
        iri.rsplit('/').next().unwrap_or(iri).to_string()
    };

    // Query 1: Find all classes
    let classes: Vec<String> = {
        let q = "PREFIX owl: <http://www.w3.org/2002/07/owl#>
                 SELECT ?class WHERE { ?class a owl:Class }";
        match store.query(q) {
            Ok(oxigraph::sparql::QueryResults::Solutions(sols)) => {
                sols.filter_map(|s| s.ok().and_then(|s| {
                    s.get("class").map(|t| match t {
                        Term::NamedNode(n) => n.as_str().to_string(),
                        _ => String::new(),
                    })
                })).filter(|s| s.starts_with(&namespace)).collect()
            }
            _ => Vec::new(),
        }
    };

    // Query 2: Find properties with domains, types, and ranges
    struct PropInfo {
        iri: String,
        is_object_prop: bool,
        domain: String,
        range: String,
    }
    let properties: Vec<PropInfo> = {
        let q = "PREFIX owl: <http://www.w3.org/2002/07/owl#>
                 PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
                 SELECT ?prop ?propType ?domain ?range WHERE {
                     ?prop rdfs:domain ?domain .
                     ?prop a ?propType .
                     FILTER(?propType IN (owl:DatatypeProperty, owl:ObjectProperty))
                     OPTIONAL { ?prop rdfs:range ?range }
                 } ORDER BY ?domain ?prop";
        match store.query(q) {
            Ok(oxigraph::sparql::QueryResults::Solutions(sols)) => {
                sols.filter_map(|s| s.ok().map(|s| {
                    let term_str = |name: &str| -> String {
                        s.get(name).map(|t| match t {
                            Term::NamedNode(n) => n.as_str().to_string(),
                            _ => String::new(),
                        }).unwrap_or_default()
                    };
                    PropInfo {
                        iri: term_str("prop"),
                        is_object_prop: term_str("propType").contains("ObjectProperty"),
                        domain: term_str("domain"),
                        range: term_str("range"),
                    }
                })).collect()
            }
            _ => Vec::new(),
        }
    };

    // Query 3: Find enum values (rdfs:Datatype with owl:oneOf)
    let mut enum_values: HashMap<String, Vec<String>> = HashMap::new();
    {
        let q = "PREFIX owl: <http://www.w3.org/2002/07/owl#>
                 PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
                 PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
                 SELECT ?dtype ?value WHERE {
                     ?dtype a rdfs:Datatype ;
                            owl:oneOf ?list .
                     ?list rdf:rest*/rdf:first ?value .
                 } ORDER BY ?dtype ?value";
        if let Ok(oxigraph::sparql::QueryResults::Solutions(sols)) = store.query(q) {
            for s in sols.flatten() {
                let dtype = s.get("dtype").map(|t| match t {
                    Term::NamedNode(n) => n.as_str().to_string(),
                    _ => String::new(),
                }).unwrap_or_default();
                let value = s.get("value").map(|t| match t {
                    Term::Literal(l) => l.value().to_string(),
                    _ => String::new(),
                }).unwrap_or_default();
                if !dtype.is_empty() && !value.is_empty() {
                    enum_values.entry(dtype).or_default().push(value);
                }
            }
        }
    }

    // Query 4: Find required fields (owl:Restriction with minCardinality)
    let mut required_props: HashSet<(String, String)> = HashSet::new(); // (class_iri, prop_iri)
    {
        let q = "PREFIX owl: <http://www.w3.org/2002/07/owl#>
                 PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
                 SELECT ?class ?prop WHERE {
                     ?class rdfs:subClassOf ?restriction .
                     ?restriction a owl:Restriction ;
                                  owl:onProperty ?prop ;
                                  owl:minCardinality ?minCard .
                     FILTER(?minCard >= 1)
                 }";
        if let Ok(oxigraph::sparql::QueryResults::Solutions(sols)) = store.query(q) {
            for s in sols.flatten() {
                let class = s.get("class").map(|t| match t {
                    Term::NamedNode(n) => n.as_str().to_string(),
                    _ => String::new(),
                }).unwrap_or_default();
                let prop = s.get("prop").map(|t| match t {
                    Term::NamedNode(n) => n.as_str().to_string(),
                    _ => String::new(),
                }).unwrap_or_default();
                if !class.is_empty() && !prop.is_empty() {
                    required_props.insert((class, prop));
                }
            }
        }
    }

    // Build the SHACL Turtle output
    let mut shacl = String::new();
    shacl.push_str(&format!("@prefix sh:    <http://www.w3.org/ns/shacl#> .\n"));
    shacl.push_str(&format!("@prefix {}: <{}> .\n", prefix_name, namespace));
    shacl.push_str(&format!("@prefix xsd:   <http://www.w3.org/2001/XMLSchema#> .\n"));
    shacl.push_str(&format!("@prefix rdfs:  <http://www.w3.org/2000/01/rdf-schema#> .\n\n"));
    shacl.push_str(&format!("# Auto-generated SHACL shapes from {} ontology.\n", kit));
    shacl.push_str(&format!("# Do not hand-edit — regenerate with: git lex kit update\n\n"));

    for class_iri in &classes {
        let class_name = local_name(class_iri);
        let shape_name = format!("{}Shape", class_name);

        shacl.push_str(&format!("\n# --- {} ---\n\n", class_name));
        shacl.push_str(&format!("{}:{} a sh:NodeShape ;\n", prefix_name, shape_name));
        shacl.push_str(&format!("    sh:targetClass {}:{}", prefix_name, class_name));

        // Collect properties for this class
        let class_props: Vec<&PropInfo> = properties.iter()
            .filter(|p| p.domain == *class_iri)
            .collect();

        if class_props.is_empty() {
            shacl.push_str(" .\n");
            continue;
        }

        for (i, prop) in class_props.iter().enumerate() {
            let prop_name = local_name(&prop.iri);
            let is_last = i == class_props.len() - 1;
            let is_required = required_props.contains(&(class_iri.clone(), prop.iri.clone()));

            shacl.push_str(" ;\n    sh:property [\n");
            shacl.push_str(&format!("        sh:path {}:{} ;\n", prefix_name, prop_name));

            if prop.is_object_prop {
                shacl.push_str("        sh:nodeKind sh:IRI ;\n");
                let msg = format!("{} must be an IRI reference.", prop_name);
                shacl.push_str(&format!("        sh:message \"{}\" ;\n", msg));
            } else if let Some(values) = enum_values.get(&prop.range) {
                let quoted: Vec<String> = values.iter().map(|v| format!("\"{}\"", v)).collect();
                shacl.push_str(&format!("        sh:in ( {} ) ;\n", quoted.join(" ")));
                let msg = format!("{} must be {}.",
                    prop_name,
                    values.iter().map(|v| format!("'{}'", v)).collect::<Vec<_>>().join(", "));
                shacl.push_str(&format!("        sh:message \"{}\" ;\n", msg));
            } else {
                let xsd_prefix = "http://www.w3.org/2001/XMLSchema#";
                if prop.range.starts_with(xsd_prefix) && prop.range != format!("{}string", xsd_prefix) {
                    let xsd_type = &prop.range[xsd_prefix.len()..];
                    shacl.push_str(&format!("        sh:datatype xsd:{} ;\n", xsd_type));
                    let msg = format!("Expected datatype: xsd:{}.", xsd_type);
                    shacl.push_str(&format!("        sh:message \"{}\" ;\n", msg));
                }
            }

            if is_required {
                shacl.push_str("        sh:minCount 1 ;\n");
            }

            if is_last {
                shacl.push_str("    ] .\n");
            } else {
                shacl.push_str("    ]");
            }
        }
    }

    Some(shacl)
}

/// Generate and write SHACL shapes for the current kit.
/// Returns the path to the generated shapes file.
fn build_shacl_shapes(kit: &str) -> Option<PathBuf> {
    let root = find_git_root()?;
    let shacl = generate_shacl_shapes(kit)?;
    let kit_dir = root.join(".lex").join("ontology").join("kit").join(kit);
    let shapes_path = kit_dir.join(format!("{}-shapes.ttl", kit));
    fs::write(&shapes_path, &shacl).ok()?;
    Some(shapes_path)
}

/// Build a set of ObjectProperty names from the kit ontology TTL.
/// These are properties whose values should be resolved as IRIs, not literals.
fn get_object_properties(kit: &str) -> HashSet<String> {
    let root = match find_git_root() {
        Some(r) => r,
        None => return HashSet::new(),
    };

    let kit_dir = root.join(".lex").join("ontology").join("kit").join(kit);
    let content = {
        let primary = kit_dir.join(format!("{}.ttl", kit));
        match fs::read_to_string(&primary) {
            Ok(c) => c,
            Err(_) => {
                fs::read_dir(&kit_dir).ok()
                    .and_then(|entries| entries.filter_map(|e| e.ok())
                        .find(|e| e.path().extension().is_some_and(|ext| ext == "ttl") && !e.file_name().to_string_lossy().contains("shapes"))
                        .and_then(|e| fs::read_to_string(e.path()).ok()))
                    .unwrap_or_default()
            }
        }
    };

    if content.is_empty() { return HashSet::new(); }

    // Find prefix name
    let kit_ns_pattern = format!("/kit/{}/", kit);
    let mut prefix_name = kit.to_string();
    for line in content.lines() {
        if line.starts_with("@prefix ") && line.contains(&kit_ns_pattern) {
            if let Some(colon_pos) = line[8..].find(':') {
                prefix_name = line[8..8 + colon_pos].trim().to_string();
            }
            break;
        }
    }

    let mut obj_props = HashSet::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.contains("a owl:ObjectProperty") {
            if let Some(prop) = trimmed.split_whitespace().next() {
                let name = prop
                    .strip_prefix(&format!("{}:", prefix_name))
                    .unwrap_or(prop)
                    .to_string();
                obj_props.insert(name);
            }
        }
    }
    obj_props
}

/// Build a map of property name → XSD datatype from the kit ontology TTL.
/// Only includes properties with non-string ranges (xsd:integer, xsd:date, xsd:dateTime, xsd:boolean, xsd:decimal, xsd:anyURI).
/// Properties with xsd:string or no range are omitted (they use the default string behavior).
fn get_property_datatypes(kit: &str) -> HashMap<String, String> {
    let root = match find_git_root() {
        Some(r) => r,
        None => return HashMap::new(),
    };

    let kit_dir = root.join(".lex").join("ontology").join("kit").join(kit);
    let content = {
        let primary = kit_dir.join(format!("{}.ttl", kit));
        match fs::read_to_string(&primary) {
            Ok(c) => c,
            Err(_) => {
                fs::read_dir(&kit_dir).ok()
                    .and_then(|entries| entries.filter_map(|e| e.ok())
                        .find(|e| e.path().extension().is_some_and(|ext| ext == "ttl") && !e.file_name().to_string_lossy().contains("shapes"))
                        .and_then(|e| fs::read_to_string(e.path()).ok()))
                    .unwrap_or_default()
            }
        }
    };

    if content.is_empty() { return HashMap::new(); }

    // Find prefix name
    let kit_ns_pattern = format!("/kit/{}/", kit);
    let mut prefix_name = kit.to_string();
    for line in content.lines() {
        if line.starts_with("@prefix ") && line.contains(&kit_ns_pattern) {
            if let Some(colon_pos) = line[8..].find(':') {
                prefix_name = line[8..8 + colon_pos].trim().to_string();
            }
            break;
        }
    }

    // Parse property blocks: track current property name, then capture rdfs:range
    let mut datatypes = HashMap::new();
    let mut current_prop = String::new();

    for line in content.lines() {
        let trimmed = line.trim();

        // New property block
        if trimmed.contains("a owl:DatatypeProperty") {
            if let Some(prop) = trimmed.split_whitespace().next() {
                current_prop = prop
                    .strip_prefix(&format!("{}:", prefix_name))
                    .unwrap_or(prop)
                    .to_string();
            }
        }

        // Capture rdfs:range with XSD type
        if !current_prop.is_empty() && trimmed.starts_with("rdfs:range") {
            if let Some(range) = trimmed.split_whitespace().nth(1) {
                let range = range.trim_end_matches(|c: char| c == ' ' || c == ';' || c == '.');
                // Map XSD prefix to full URI
                let xsd_type = match range {
                    "xsd:integer" => Some("http://www.w3.org/2001/XMLSchema#integer"),
                    "xsd:date" => Some("http://www.w3.org/2001/XMLSchema#date"),
                    "xsd:dateTime" => Some("http://www.w3.org/2001/XMLSchema#dateTime"),
                    "xsd:boolean" => Some("http://www.w3.org/2001/XMLSchema#boolean"),
                    "xsd:decimal" => Some("http://www.w3.org/2001/XMLSchema#decimal"),
                    "xsd:float" => Some("http://www.w3.org/2001/XMLSchema#float"),
                    "xsd:double" => Some("http://www.w3.org/2001/XMLSchema#double"),
                    "xsd:anyURI" => Some("http://www.w3.org/2001/XMLSchema#anyURI"),
                    _ => None, // xsd:string or unknown → default string behavior
                };
                if let Some(dt) = xsd_type {
                    datatypes.insert(current_prop.clone(), dt.to_string());
                }
            }
        }

        // Blank line ends property block
        if trimmed.is_empty() {
            current_prop.clear();
        }
    }

    datatypes
}

/// Parse the kit ontology to find document types and their properties.
/// Returns: Vec<(ClassName, Vec<(prop_name, prop_type, required, comment)>)>
fn get_kit_types(kit: &str) -> Vec<(String, Vec<(String, String, bool, String)>)> {
    let root = match find_git_root() {
        Some(r) => r,
        None => return Vec::new(),
    };

    // Try {kit}.ttl first, then find any .ttl in the kit directory
    let kit_dir = root.join(".lex").join("ontology").join("kit").join(kit);
    let ontology_path = kit_dir.join(format!("{}.ttl", kit));
    let content = match fs::read_to_string(&ontology_path) {
        Ok(c) => c,
        Err(_) => {
            // Fallback: find the first .ttl file in the kit directory
            match fs::read_dir(&kit_dir).ok().and_then(|entries| {
                entries.filter_map(|e| e.ok())
                    .find(|e| e.path().extension().is_some_and(|ext| ext == "ttl"))
                    .and_then(|e| fs::read_to_string(e.path()).ok())
            }) {
                Some(c) => c,
                None => return Vec::new(),
            }
        }
    };

    // Extract the kit prefix — find the @prefix that maps to this kit's namespace URL
    let kit_ns_pattern = format!("/kit/{}/", kit);
    let mut prefix = String::new();
    let mut prefix_name = kit.to_string();
    for line in content.lines() {
        if line.starts_with("@prefix ") && line.contains(&kit_ns_pattern) {
            // Extract prefix name: @prefix lab: <...> .
            if let Some(colon_pos) = line[8..].find(':') {
                prefix_name = line[8..8 + colon_pos].trim().to_string();
            }
            if let Some(start) = line.find('<') {
                if let Some(end) = line.find('>') {
                    prefix = line[start + 1..end].to_string();
                }
            }
            break;
        }
    }

    // Find all owl:Class declarations and their properties
    let mut types: HashMap<String, Vec<(String, String, bool, String)>> = HashMap::new();
    let mut current_class = String::new();

    for line in content.lines() {
        let trimmed = line.trim();

        // Detect class: "squad:Decision a owl:Class ;"
        if trimmed.contains("a owl:Class") {
            if let Some(class_name) = trimmed.split_whitespace().next() {
                let name = class_name
                    .strip_prefix(&format!("{}:", prefix_name))
                    .unwrap_or(class_name)
                    .to_string();
                current_class = name.clone();
                types.entry(name).or_default();
            }
        }

        // Detect property with domain: "rdfs:domain squad:Decision ;"
        if trimmed.contains("rdfs:domain") && trimmed.contains(&format!("{}:", prefix_name)) {
            // Look back to find the property name — this is tricky with TTL
            // Instead, we'll parse properties differently
        }
    }

    // Parse properties: track current property name, type, and comment across multi-line TTL blocks
    let mut current_prop = String::new();
    let mut current_prop_type = String::new();
    let mut current_comment = String::new();

    for line in content.lines() {
        let trimmed = line.trim();

        // New property block starts with "kit:propName a owl:DatatypeProperty/ObjectProperty"
        if trimmed.contains("a owl:DatatypeProperty") || trimmed.contains("a owl:ObjectProperty") {
            if let Some(prop) = trimmed.split_whitespace().next() {
                current_prop = prop
                    .strip_prefix(&format!("{}:", prefix_name))
                    .unwrap_or(prop)
                    .to_string();
                current_prop_type = if trimmed.contains("DatatypeProperty") {
                    "string".to_string()
                } else {
                    "reference".to_string()
                };
                current_comment.clear();
            }
        }

        // Capture rdfs:comment within a property block
        if !current_prop.is_empty() && trimmed.starts_with("rdfs:comment") {
            // Extract the quoted string: rdfs:comment "Some text." ;
            if let Some(start) = trimmed.find('"') {
                if let Some(end) = trimmed[start + 1..].find('"') {
                    current_comment = trimmed[start + 1..start + 1 + end].to_string();
                }
            }
        }

        // Domain line within a property block
        if !current_prop.is_empty() && trimmed.starts_with("rdfs:domain") {
            if let Some(domain) = trimmed.split_whitespace().nth(1) {
                let class_name = domain
                    .strip_prefix(&format!("{}:", prefix_name))
                    .unwrap_or(domain)
                    .trim_end_matches(|c: char| c == ' ' || c == ';' || c == '.')
                    .to_string();

                if let Some(props) = types.get_mut(&class_name) {
                    props.push((current_prop.clone(), current_prop_type.clone(), false, current_comment.clone()));
                }
            }
        }

        // A blank line or a new top-level definition ends the current property block
        if trimmed.is_empty() {
            current_prop.clear();
            current_comment.clear();
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

    // Build frontmatter — flat dot notation: kit.class.property
    let class_lower = class_name.to_lowercase();
    let mut fm = String::new();
    fm.push_str("---\n");

    for (prop_name, prop_type, _required, comment) in &properties {
        // Property names pass through as-is from the ontology (camelCase)
        let key = format!("{}.{}.{}", kit, class_lower, prop_name);

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

// ─── SHACL validation via rudof ────────────────────────────────

/// Convert frontmatter from a markdown file to Turtle RDF for SHACL validation.
/// Supports dot notation (kit.class.property) format.
/// Returns None if the file has no frontmatter or no kit-specific properties.
fn frontmatter_to_turtle(filepath: &std::path::Path, root: &std::path::Path, kit: &str) -> Option<String> {
    let content = fs::read_to_string(filepath).ok()?;

    if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
        return None;
    }

    let rest = &content[4..];
    let end = rest.find("\n---")?;
    let yaml_str = &rest[..end];

    let yaml: HashMap<String, serde_yaml::Value> = serde_yaml::from_str(yaml_str).ok()?;

    // Find dot notation keys matching this kit: kit.class.property
    let kit_prefix = format!("{}.", kit);
    let mut doc_type: Option<String> = None;
    let mut kit_props: Vec<(String, String)> = Vec::new(); // (property_name, value)

    for (key, value) in &yaml {
        if let Some(rest) = key.strip_prefix(&kit_prefix) {
            let segments: Vec<&str> = rest.splitn(2, '.').collect();
            if segments.len() == 2 {
                let class_seg = segments[0];
                let prop_name = segments[1];

                // Infer doc type from class segment (capitalize)
                if doc_type.is_none() {
                    let mut c = class_seg.chars();
                    doc_type = Some(match c.next() {
                        None => class_seg.to_string(),
                        Some(f) => f.to_uppercase().to_string() + c.as_str(),
                    });
                }

                // Handle all YAML value types (string, number, bool)
                let val_str = match value {
                    serde_yaml::Value::String(s) if !s.is_empty() => Some(s.clone()),
                    serde_yaml::Value::Number(n) => Some(n.to_string()),
                    serde_yaml::Value::Bool(b) => Some(b.to_string()),
                    _ => None,
                };
                if let Some(s) = val_str {
                    kit_props.push((prop_name.to_string(), s));
                }
            }
        }
    }

    let doc_type = doc_type?;
    if kit_props.is_empty() {
        return None;
    }

    // Read the kit ontology to find the prefix name and namespace
    let kit_dir = root.join(".lex").join("ontology").join("kit").join(kit);
    let ttl_path = {
        let primary = kit_dir.join(format!("{}.ttl", kit));
        if primary.exists() { primary } else {
            fs::read_dir(&kit_dir).ok()?
                .filter_map(|e| e.ok())
                .find(|e| e.path().extension().is_some_and(|ext| ext == "ttl") && !e.file_name().to_string_lossy().contains("shapes"))?
                .path()
        }
    };
    let kit_ttl = fs::read_to_string(&ttl_path).ok()?;

    // Find prefix name and namespace from TTL
    let kit_ns_pattern = format!("/kit/{}/", kit);
    let mut prefix_name = kit.to_string();
    let mut namespace = format!("https://repolex.ai/ontology/kit/{}/", kit);
    for line in kit_ttl.lines() {
        if line.starts_with("@prefix ") && line.contains(&kit_ns_pattern) {
            if let Some(colon_pos) = line[8..].find(':') {
                prefix_name = line[8..8 + colon_pos].trim().to_string();
            }
            if let Some(start) = line.find('<') {
                if let Some(end) = line.find('>') {
                    namespace = line[start + 1..end].to_string();
                }
            }
            break;
        }
    }

    // Build ObjectProperty set and datatype map for proper literal emission
    let obj_props = get_object_properties(kit);
    let prop_datatypes = get_property_datatypes(kit);

    // Build Turtle RDF for this document
    let relpath = filepath.strip_prefix(root).ok()?;
    let doc_id = relpath.to_string_lossy().replace('/', "_").replace('.', "_");

    let mut ttl = String::new();
    ttl.push_str(&format!("@prefix {}: <{}> .\n", prefix_name, namespace));
    ttl.push_str("@prefix sh: <http://www.w3.org/ns/shacl#> .\n");
    ttl.push_str("@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\n");

    // Declare the document as an instance of the type
    ttl.push_str(&format!("<urn:doc:{}> a {}:{} .\n", doc_id, prefix_name, doc_type));

    // Add properties
    for (prop_name, value) in &kit_props {
        if obj_props.contains(prop_name.as_str()) {
            // ObjectProperty — resolve each comma-separated value as IRI
            let values: Vec<&str> = value.split(',').map(|v| v.trim()).filter(|v| !v.is_empty()).collect();
            for val in values {
                let slug = val.trim_start_matches('@').to_lowercase()
                    .replace(' ', "-")
                    .replace(|c: char| !c.is_alphanumeric() && c != '-', "");
                if !slug.is_empty() {
                    ttl.push_str(&format!(
                        "<urn:doc:{}> {}:{} <urn:entity:{}> .\n",
                        doc_id, prefix_name, prop_name, slug
                    ));
                }
            }
        } else if let Some(datatype) = prop_datatypes.get(prop_name.as_str()) {
            // Typed literal (xsd:integer, xsd:date, etc.)
            ttl.push_str(&format!(
                "<urn:doc:{}> {}:{} \"{}\"^^<{}> .\n",
                doc_id, prefix_name, prop_name, value.replace('"', "\\\""), datatype
            ));
        } else {
            // Plain string literal
            ttl.push_str(&format!(
                "<urn:doc:{}> {}:{} \"{}\" .\n",
                doc_id, prefix_name, prop_name, value.replace('"', "\\\"")
            ));
        }
    }

    Some(ttl)
}

// ─── git lex identity ──────────────────────────────────────────

fn read_identity(root: &std::path::Path) -> Option<String> {
    let content = fs::read_to_string(root.join(".lex").join("identity.yml")).ok()?;
    for line in content.lines() {
        if let Some(sha) = line.strip_prefix("identity: ") {
            return Some(sha.trim().to_string());
        }
    }
    None
}

fn cmd_identity() {
    let root = match find_git_root() {
        Some(r) => r,
        None => {
            eprintln!("fatal: not a git repository");
            exit(1);
        }
    };

    let identity_path = root.join(".lex").join("identity.yml");
    if !identity_path.exists() {
        eprintln!("No identity found. Run 'git lex init' to create one.");
        exit(1);
    }

    let content = fs::read_to_string(&identity_path).unwrap_or_default();
    println!("{}", content.trim());

    // Show tickets if any
    let tickets_dir = root.join(".lex").join("tickets");
    if tickets_dir.exists() {
        if let Ok(entries) = fs::read_dir(&tickets_dir) {
            let tickets: Vec<_> = entries.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "ticket"))
                .collect();
            if !tickets.is_empty() {
                println!("\nSquad memberships:");
                for entry in tickets {
                    let ticket = fs::read_to_string(entry.path()).unwrap_or_default();
                    let squad_name = ticket.lines()
                        .find(|l| l.starts_with("squad_name:"))
                        .map(|l| l.strip_prefix("squad_name:").unwrap_or("").trim())
                        .unwrap_or("unknown");
                    println!("  {} ({})", squad_name, entry.file_name().to_string_lossy());
                }
            }
        }
    }
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

    // Load kit ontology TTL
    let kit_dir = root.join(".lex").join("ontology").join("kit").join(&kit);
    let ont_ttl = {
        let primary = kit_dir.join(format!("{}.ttl", &kit));
        if primary.exists() {
            fs::read_to_string(&primary).unwrap_or_default()
        } else {
            fs::read_dir(&kit_dir).ok()
                .and_then(|entries| entries.filter_map(|e| e.ok())
                    .find(|e| e.path().extension().is_some_and(|ext| ext == "ttl"))
                    .and_then(|e| fs::read_to_string(e.path()).ok()))
                .unwrap_or_default()
        }
    };

    // Load SHACL shapes TTL
    let shapes_ttl = fs::read_dir(&kit_dir).ok()
        .and_then(|entries| entries.filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().contains("shapes"))
            .and_then(|e| fs::read_to_string(e.path()).ok()));

    let shapes_ttl = match shapes_ttl {
        Some(s) => s,
        None => {
            println!("No SHACL shapes found for kit '{}' — skipping validation.", kit);
            return true;
        }
    };

    // Combine ontology + shapes for rudof (shapes reference ontology classes)
    let combined_shapes = format!("{}\n{}", ont_ttl, shapes_ttl);

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
        &mut combined_shapes.as_bytes(),
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

// ─── Tree-sitter markdown parsing ──────────────────────────────

/// Extract markdown links from body text using tree-sitter.
/// Writes .md.spo sidecars with link type (internal/external/unresolved) and destination.
fn extract_markdown_links() {
    let root = match find_git_root() {
        Some(r) => r,
        None => return,
    };

    let extract_dir = root.join(".lex").join("extract");
    fs::create_dir_all(&extract_dir).ok();

    let mut parser = tree_sitter_md::MarkdownParser::default();

    // Walk all .md files
    fn walk_md(dir: &std::path::Path, files: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') { continue; }
                if path.is_dir() { walk_md(&path, files); }
                else if name.ends_with(".md") && !name.starts_with("__") { files.push(path); }
            }
        }
    }

    let mut files = Vec::new();
    walk_md(&root, &mut files);

    // Build file index for resolving internal links
    let mut file_index: HashSet<String> = HashSet::new();
    for f in &files {
        if let Ok(rel) = f.strip_prefix(&root) {
            file_index.insert(rel.to_string_lossy().to_string());
        }
    }

    let mut total_links = 0;

    for filepath in &files {
        let content = match fs::read_to_string(filepath) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let relpath = filepath.strip_prefix(&root).unwrap_or(filepath);
        let relpath_str = relpath.to_string_lossy().to_string();

        let tree = match parser.parse(content.as_bytes(), None) {
            Some(t) => t,
            None => continue,
        };

        let mut spo_lines: Vec<String> = Vec::new();

        // Walk inline trees for links
        for inline_tree in tree.inline_trees() {
            let inline_root = inline_tree.root_node();

            fn extract_links(node: tree_sitter::Node, source: &str, lines: &mut Vec<String>, file_index: &HashSet<String>, doc_dir: &str) {
                if node.kind() == "inline_link" {
                    let dest = node.children(&mut node.walk())
                        .find(|c| c.kind() == "link_destination")
                        .map(|c| source[c.start_byte()..c.end_byte()].to_string())
                        .unwrap_or_default();

                    if !dest.is_empty() {
                        if dest.starts_with("http://") || dest.starts_with("https://") {
                            // External link
                            lines.push(format!("md.externalLink | hasValue | {}", dest));
                        } else {
                            // Internal link — resolve relative to doc's directory
                            let resolved = if dest.starts_with('/') {
                                dest[1..].to_string()
                            } else if !doc_dir.is_empty() {
                                format!("{}/{}", doc_dir, dest)
                            } else {
                                dest.clone()
                            };

                            if file_index.contains(&resolved) {
                                lines.push(format!("md.internalLink | hasValue | {}", resolved));
                            } else {
                                lines.push(format!("md.unresolvedLink | hasValue | {}", dest));
                            }
                        }
                    }
                }

                // Also catch bare autolinks
                if node.kind() == "uri_autolink" {
                    let text = &source[node.start_byte()..node.end_byte()];
                    let url = text.trim_matches(|c| c == '<' || c == '>');
                    if !url.is_empty() {
                        lines.push(format!("md.externalLink | hasValue | {}", url));
                    }
                }

                let mut cursor = node.walk();
                if cursor.goto_first_child() {
                    loop {
                        extract_links(cursor.node(), source, lines, file_index, doc_dir);
                        if !cursor.goto_next_sibling() { break; }
                    }
                }
            }

            let doc_dir = relpath.parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();

            extract_links(inline_root, &content, &mut spo_lines, &file_index, &doc_dir);
        }

        // Write .md.spo sidecar
        if !spo_lines.is_empty() {
            spo_lines.sort();
            spo_lines.dedup();
            let spo_path = extract_dir.join(format!("{}.md.spo", relpath_str));
            fs::create_dir_all(spo_path.parent().unwrap()).ok();
            fs::write(&spo_path, spo_lines.join("\n") + "\n").ok();
            total_links += spo_lines.len();
        }
    }

    if total_links > 0 {
        eprintln!("Markdown links: {} from {} files", total_links, files.len());
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

// ─── git lex viz ────────────────────────────────────────────────
// HTTP + WebSocket server for visualizing the knowledge graph.
// Embedded D3 frontend, served on localhost. Agent pushes CONSTRUCT
// query results over WebSocket to drive the viz.

// Viz UI assets — embedded at compile time
const VIZ_INDEX_HTML: &str = include_str!("../viz/index.html");
const VIZ_CSS_MAIN: &str = include_str!("../viz/css/main.css");
const VIZ_JS_MAIN: &str = include_str!("../viz/js/main.js");

#[derive(Clone)]
struct VizState {
    store: std::sync::Arc<Store>,
    /// In-memory scene buffer — the most recent push from an agent
    scene: std::sync::Arc<tokio::sync::Mutex<Option<serde_json::Value>>>,
    /// Broadcast channel for live updates to all connected WebSocket clients
    tx: tokio::sync::broadcast::Sender<String>,
}

/// Run a SPARQL query and return the result as JSON.
/// Handles SELECT (rows of bindings), CONSTRUCT/DESCRIBE (triples), and ASK (boolean).
fn run_sparql_to_json(store: &Store, query: &str) -> serde_json::Value {
    let prefixed = add_prefixes(query);
    let mut parsed = match oxigraph::sparql::Query::parse(&prefixed, None) {
        Ok(p) => p,
        Err(e) => return serde_json::json!({"error": format!("parse error: {}", e)}),
    };
    parsed.dataset_mut().set_default_graph_as_union();

    let results = match store.query(parsed) {
        Ok(r) => r,
        Err(e) => return serde_json::json!({"error": format!("query error: {}", e)}),
    };

    match results {
        oxigraph::sparql::QueryResults::Solutions(sols) => {
            // Backward-compatible flat shape: each row is { var: "value", ... }
            let vars: Vec<String> = sols.variables().iter().map(|v| v.as_str().to_string()).collect();
            let mut rows = Vec::new();
            for sol in sols.flatten() {
                let mut row = serde_json::Map::new();
                for var in &vars {
                    if let Some(t) = sol.get(var.as_str()) {
                        let val = match t {
                            Term::NamedNode(n) => n.as_str().to_string(),
                            Term::Literal(l) => l.value().to_string(),
                            Term::BlankNode(b) => format!("_:{}", b.as_str()),
                            Term::Triple(t) => format!("<<{} {} {}>>", t.subject, t.predicate, t.object),
                        };
                        row.insert(var.clone(), serde_json::Value::String(val));
                    }
                }
                rows.push(serde_json::Value::Object(row));
            }
            serde_json::json!({"type": "select", "vars": vars, "results": rows})
        }
        oxigraph::sparql::QueryResults::Boolean(b) => {
            serde_json::json!({"type": "ask", "boolean": b})
        }
        oxigraph::sparql::QueryResults::Graph(triples) => {
            // CONSTRUCT/DESCRIBE — emit triples as JSON
            let mut emitted = Vec::new();
            for t in triples.flatten() {
                let s = match t.subject {
                    oxigraph::model::NamedOrBlankNode::NamedNode(n) => n.as_str().to_string(),
                    oxigraph::model::NamedOrBlankNode::BlankNode(b) => format!("_:{}", b.as_str()),
                };
                let p = t.predicate.as_str().to_string();
                let (o_val, o_type, o_datatype) = match t.object {
                    Term::NamedNode(n) => (n.as_str().to_string(), "iri", None),
                    Term::Literal(l) => (l.value().to_string(), "literal", Some(l.datatype().as_str().to_string())),
                    Term::BlankNode(b) => (format!("_:{}", b.as_str()), "bnode", None),
                    Term::Triple(t) => (format!("<<{} {} {}>>", t.subject, t.predicate, t.object), "triple", None),
                };
                let mut triple = serde_json::Map::new();
                triple.insert("subject".to_string(), serde_json::Value::String(s));
                triple.insert("predicate".to_string(), serde_json::Value::String(p));
                let mut obj = serde_json::Map::new();
                obj.insert("value".to_string(), serde_json::Value::String(o_val));
                obj.insert("type".to_string(), serde_json::Value::String(o_type.to_string()));
                if let Some(dt) = o_datatype {
                    obj.insert("datatype".to_string(), serde_json::Value::String(dt));
                }
                triple.insert("object".to_string(), serde_json::Value::Object(obj));
                emitted.push(serde_json::Value::Object(triple));
            }
            serde_json::json!({"type": "construct", "triples": emitted})
        }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn run_viz_server(port: u16) {
    use axum::{
        Router,
        routing::{get, post},
        response::{Html, Json},
        extract::ws::WebSocketUpgrade,
    };
    use std::sync::Arc;
    use tokio::sync::{Mutex, broadcast};

    let store = Arc::new(
        open_store_read_only().expect("failed to open store read-only — run `git lex sync` first"),
    );
    let scene: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let (tx, _rx) = broadcast::channel::<String>(64);

    let state = VizState { store, scene, tx };

    let app = Router::new()
        .route("/", get(|| async { Html(VIZ_INDEX_HTML) }))
        .route("/css/main.css", get(|| async {
            ([("content-type", "text/css")], VIZ_CSS_MAIN)
        }))
        .route("/js/main.js", get(|| async {
            ([("content-type", "application/javascript")], VIZ_JS_MAIN)
        }))
        .route("/api/query", post({
            let state = state.clone();
            move |Json(payload): Json<serde_json::Value>| {
                let state = state.clone();
                async move {
                    let query = payload.get("query")
                        .and_then(|v| v.as_str())
                        .unwrap_or("SELECT ?s ?p ?o WHERE { ?s ?p ?o } LIMIT 10");
                    Json(run_sparql_to_json(&state.store, query))
                }
            }
        }))
        .route("/api/push", post({
            let state = state.clone();
            move |Json(payload): Json<serde_json::Value>| {
                let state = state.clone();
                async move {
                    // Store the new scene
                    {
                        let mut scene = state.scene.lock().await;
                        *scene = Some(payload.clone());
                    }
                    // Broadcast to all WebSocket clients
                    let msg = serde_json::json!({
                        "type": "scene",
                        "data": payload
                    }).to_string();
                    let _ = state.tx.send(msg);
                    Json(serde_json::json!({"ok": true}))
                }
            }
        }))
        .route("/api/run-and-push", post({
            let state = state.clone();
            move |Json(payload): Json<serde_json::Value>| {
                let state = state.clone();
                async move {
                    let query = payload.get("query").and_then(|v| v.as_str()).unwrap_or("");
                    if query.is_empty() {
                        return Json(serde_json::json!({"error": "missing 'query' field"}));
                    }
                    let result = run_sparql_to_json(&state.store, query);
                    let scene = serde_json::json!({
                        "query": query,
                        "result": result,
                    });
                    {
                        let mut s = state.scene.lock().await;
                        *s = Some(scene.clone());
                    }
                    let msg = serde_json::json!({
                        "type": "scene",
                        "data": scene
                    }).to_string();
                    let _ = state.tx.send(msg);
                    Json(serde_json::json!({"ok": true}))
                }
            }
        }))
        .route("/api/scene", get({
            let state = state.clone();
            move || {
                let state = state.clone();
                async move {
                    let scene = state.scene.lock().await;
                    Json(scene.clone().unwrap_or(serde_json::Value::Null))
                }
            }
        }))
        .route("/ws", get({
            let state = state.clone();
            move |ws: WebSocketUpgrade| {
                let state = state.clone();
                async move {
                    ws.on_upgrade(move |socket| handle_ws(socket, state))
                }
            }
        }));

    // Try the requested port; if taken, walk forward up to 20 ports until one
    // binds. Lets multiple viz instances co-exist without hand-picking ports.
    let mut chosen_port = port;
    let mut listener = None;
    for candidate in port..port.saturating_add(20) {
        let addr = format!("127.0.0.1:{}", candidate);
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => {
                chosen_port = candidate;
                listener = Some(l);
                break;
            }
            Err(_) => continue,
        }
    }
    let listener = match listener {
        Some(l) => l,
        None => {
            eprintln!("Failed to bind: ports {}..{} all in use", port, port.saturating_add(20));
            return;
        }
    };

    let addr = format!("127.0.0.1:{}", chosen_port);
    if chosen_port != port {
        println!("Port {} was taken, using {} instead", port, chosen_port);
    }
    let url = format!("http://{}", addr);
    println!("git-lex viz server listening on {}", url);
    println!("Press Ctrl+C to stop, or: kill {}", std::process::id());

    // Open the URL in the user's default browser. Best-effort: ignore failure
    // (headless boxes, no DISPLAY, etc) since the URL is already printed above.
    let _ = open::that_detached(&url);

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("Server error: {}", e);
    }
}

async fn handle_ws(socket: axum::extract::ws::WebSocket, state: VizState) {
    use axum::extract::ws::Message;
    use futures_util::{SinkExt, StreamExt};

    let (mut sender, mut receiver) = socket.split();

    // Send the current scene immediately so the client gets caught up
    {
        let scene = state.scene.lock().await;
        if let Some(s) = scene.as_ref() {
            let initial = serde_json::json!({"type": "scene", "data": s}).to_string();
            let _ = sender.send(Message::Text(initial.into())).await;
        } else {
            let _ = sender.send(Message::Text("{\"type\":\"hello\"}".into())).await;
        }
    }

    // Subscribe to broadcasts
    let mut rx = state.tx.subscribe();

    // Spawn a task to forward broadcasts to this client
    let mut send_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            if sender.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    // Read incoming messages (mostly ignored for now, but keeps the connection alive)
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(_msg)) = receiver.next().await {
            // Future: clients could send commands here
        }
    });

    // If either task ends, abort the other
    tokio::select! {
        _ = (&mut send_task) => recv_task.abort(),
        _ = (&mut recv_task) => send_task.abort(),
    }
}

fn cmd_viz(port: u16) {
    if open_store_read_only().is_none() {
        eprintln!("No knowledge graph store found.");
        eprintln!("Run 'git lex sync' first to build the store.");
        exit(1);
    }
    run_viz_server(port);
}

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
            eprintln!("Is the viz server running? Try: git lex viz --port {}", port);
            exit(1);
        }
    }
}

/// Walk .lex/extract/ and remove any .spo sidecar whose source markdown file
/// no longer exists in the working tree. Handles deletes and renames.
fn cleanup_orphaned_sidecars() -> usize {
    let root = match find_git_root() {
        Some(r) => r,
        None => return 0,
    };
    let extract_dir = root.join(".lex").join("extract");
    if !extract_dir.exists() {
        return 0;
    }

    let mut removed = 0;
    fn walk(dir: &std::path::Path, extract_root: &std::path::Path, repo_root: &std::path::Path, removed: &mut usize) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, extract_root, repo_root, removed);
                    // Try to remove the dir if it's now empty
                    let _ = fs::remove_dir(&path);
                } else if path.extension().is_some_and(|e| e == "spo") {
                    // Derive source markdown from sidecar path:
                    // .lex/extract/contact/m4rq.md.fm.spo → contact/m4rq.md
                    let rel = match path.strip_prefix(extract_root) {
                        Ok(r) => r,
                        Err(_) => continue,
                    };
                    let rel_str = rel.to_string_lossy().to_string();
                    // Strip .{extractor}.spo suffix
                    let source = if let Some(s) = rel_str.strip_suffix(".fm.spo") {
                        s.to_string()
                    } else if let Some(s) = rel_str.strip_suffix(".md.spo") {
                        s.to_string()
                    } else if let Some(s) = rel_str.strip_suffix(".cc.spo") {
                        s.to_string()
                    } else {
                        continue; // unknown extractor
                    };
                    let source_path = repo_root.join(&source);
                    if !source_path.exists() {
                        if fs::remove_file(&path).is_ok() {
                            *removed += 1;
                        }
                    }
                }
            }
        }
    }
    walk(&extract_dir, &extract_dir, &root, &mut removed);
    removed
}

fn cmd_extract() {
    let start = Instant::now();

    // Clean up orphaned sidecars (source .md files that no longer exist)
    let cleaned = cleanup_orphaned_sidecars();
    if cleaned > 0 {
        eprintln!("Cleaned up {} orphaned sidecar(s)", cleaned);
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

    // ─── Phase 3: Class graphs — current-state data tables ───
    // Each class folder gets a named graph: <base>/class/{ClassName}
    // These are cleared and rebuilt on every sync from the .fm.spo files.

    let kit = get_kit();
    let kit_name = kit.as_deref().unwrap_or("none");
    let kit_prefix_name = get_kit_prefix_name(kit_name);
    let obj_props = kit.as_ref().map(|k| get_object_properties(k)).unwrap_or_default();
    let prop_datatypes = kit.as_ref().map(|k| get_property_datatypes(k)).unwrap_or_default();

    // Clear ALL existing class graphs first — handles renames (Contact → Friend)
    // and deletions where the old class graph would otherwise stay forever.
    let class_prefix = format!("{}/class/", base);
    let existing_class_graphs: Vec<String> = {
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
                                    if uri.starts_with(&class_prefix) {
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
    for graph_uri in &existing_class_graphs {
        if let Ok(graph_node) = oxigraph::model::NamedNode::new(graph_uri) {
            store.clear_graph(&oxigraph::model::GraphName::from(graph_node)).ok();
        }
    }

    // Build slug index for reference resolution in class graphs
    let mut class_slug_index: HashMap<String, String> = HashMap::new();
    fn walk_md_for_index(dir: &std::path::Path, root: &std::path::Path, index: &mut HashMap<String, String>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') { continue; }
                if path.is_dir() {
                    walk_md_for_index(&path, root, index);
                } else if name.ends_with(".md") && !name.starts_with("__") {
                    if let Ok(rel) = path.strip_prefix(root) {
                        if let Some(stem) = path.file_stem() {
                            index.insert(stem.to_string_lossy().to_lowercase(), rel.to_string_lossy().to_string());
                        }
                    }
                }
            }
        }
    }
    walk_md_for_index(&root, &root, &mut class_slug_index);

    // Group .fm.spo files by class folder
    let mut class_spo: HashMap<String, Vec<(String, String)>> = HashMap::new(); // folder → [(source_file, content)]
    for (spo_path, content) in &current_spo {
        if !spo_path.ends_with(".fm.spo") { continue; }
        // spo_path looks like "memory/foo.md.fm.spo"
        if let Some(source) = spo_path.strip_suffix(".fm.spo") {
            if let Some(slash) = source.find('/') {
                let folder = &source[..slash];
                class_spo.entry(folder.to_string()).or_default()
                    .push((source.to_string(), content.clone()));
            }
        }
    }

    let mut class_graph_count = 0;
    let mut class_triple_count = 0;

    for (folder, files) in &class_spo {
        // Capitalize folder → class name
        let class_name = {
            let mut c = folder.chars();
            match c.next() {
                None => folder.to_string(),
                Some(f) => f.to_uppercase().to_string() + c.as_str(),
            }
        };

        let class_graph_uri = format!("{}/class/{}", base, class_name);
        let class_graph = format!("<{}>", class_graph_uri);

        // Clear existing class graph
        if let Ok(graph_node) = oxigraph::model::NamedNode::new(&class_graph_uri) {
            store.clear_graph(&oxigraph::model::GraphName::from(graph_node)).ok();
        }

        let mut class_nq = String::new();

        for (source_file, content) in files {
            // Build doc URI from source file path
            let doc_uri = {
                let parts: Vec<&str> = source_file.splitn(2, '/').collect();
                if parts.len() == 2 && source_file.ends_with(".md") {
                    format!("<{}/{}/{}>", base, uri_encode_path(&class_name), uri_encode_path(parts[1]))
                } else {
                    format!("<{}/{}>", base, uri_encode_path(source_file))
                }
            };

            // rdf:type for the class
            let kit_ns = format!("https://repolex.ai/ontology/kit/{}", kit_name);
            class_nq.push_str(&format!(
                "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{}/{}> {} .\n",
                doc_uri, kit_ns, class_name, class_graph
            ));

            // Path triple for convenience
            class_nq.push_str(&format!(
                "{} <https://repolex.ai/ontology/git-lex/fm/path> \"{}\" {} .\n",
                doc_uri, nq_escape(source_file), class_graph
            ));

            for line in content.lines().filter(|l| !l.is_empty() && !l.starts_with('#')) {
                let parts: Vec<&str> = line.splitn(3, " | ").collect();
                if parts.len() != 3 { continue; }
                let (subject, predicate, object) = (parts[0], parts[1], parts[2]);

                if predicate == "mentions" {
                    let mention_slug = object.to_lowercase();
                    if class_slug_index.contains_key(&mention_slug) {
                        let mention_uri = resolve_slug_to_uri(&mention_slug, &base, &class_slug_index);
                        class_nq.push_str(&format!(
                            "{} <https://repolex.ai/ontology/git-lex/lex/mentions> {} {} .\n",
                            doc_uri, mention_uri, class_graph
                        ));
                    } else {
                        class_nq.push_str(&format!(
                            "{} <https://repolex.ai/ontology/git-lex/lex/mentions> \"{}\" {} .\n",
                            doc_uri, nq_escape(object), class_graph
                        ));
                    }
                } else if predicate == "linksTo" {
                    let link_slug = object.to_lowercase()
                        .replace(' ', "-")
                        .replace(|c: char| !c.is_alphanumeric() && c != '-', "");
                    if class_slug_index.contains_key(&link_slug) {
                        let link_uri = resolve_slug_to_uri(&link_slug, &base, &class_slug_index);
                        class_nq.push_str(&format!(
                            "{} <https://repolex.ai/ontology/git-lex/lex/linksTo> {} {} .\n",
                            doc_uri, link_uri, class_graph
                        ));
                    } else {
                        class_nq.push_str(&format!(
                            "{} <https://repolex.ai/ontology/git-lex/lex/unresolvedLink> \"{}\" {} .\n",
                            doc_uri, nq_escape(object), class_graph
                        ));
                    }
                } else if predicate == "hasValue" {
                    // Determine predicate URI from subject (the dotted key)
                    let segments: Vec<&str> = subject.splitn(3, '.').collect();
                    if segments.len() == 3 {
                        // Property name passes through as-is (camelCase from ontology)
                        let prop_name = segments[2];
                        let pred_uri = format!("<{}/{}>", kit_ns, prop_name);

                        // Check ontology for ObjectProperty → resolve as IRI
                        if obj_props.contains(prop_name) {
                            let values: Vec<&str> = object.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
                            for val in values {
                                let slug = val.trim_start_matches('@').to_lowercase()
                                    .replace(' ', "-")
                                    .replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '/' && c != '.', "");
                                if !slug.is_empty() {
                                    let object_uri = if slug.contains('/') || slug.ends_with(".md") {
                                        format!("<{}/{}>", base, uri_encode_path(&slug))
                                    } else {
                                        resolve_slug_to_uri(&slug, &base, &class_slug_index)
                                    };
                                    class_nq.push_str(&format!(
                                        "{} {} {} {} .\n",
                                        doc_uri, pred_uri, object_uri, class_graph
                                    ));
                                }
                            }
                        } else {
                            // Typed literal if ontology specifies non-string range
                            if let Some(datatype) = prop_datatypes.get(prop_name) {
                                class_nq.push_str(&format!(
                                    "{} {} \"{}\"^^<{}> {} .\n",
                                    doc_uri, pred_uri, nq_escape(object), datatype, class_graph
                                ));
                            } else {
                                class_nq.push_str(&format!(
                                    "{} {} \"{}\" {} .\n",
                                    doc_uri, pred_uri, nq_escape(object), class_graph
                                ));
                            }
                        }
                    } else {
                        // Legacy non-dotted key — use fm: namespace
                        let fm_pred = format!("<https://repolex.ai/ontology/git-lex/fm/{}>", uri_encode_path(subject));
                        class_nq.push_str(&format!(
                            "{} {} \"{}\" {} .\n",
                            doc_uri, fm_pred, nq_escape(object), class_graph
                        ));
                    }
                }
            }
        }

        let count = class_nq.lines().filter(|l| !l.is_empty()).count();
        if !class_nq.is_empty() {
            store
                .load_from_reader(RdfFormat::NQuads, Cursor::new(class_nq.as_bytes()))
                .expect(&format!("failed to load class graph for {}", class_name));
            class_graph_count += 1;
            class_triple_count += count;
        }
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
    if class_graph_count > 0 {
        println!("  Class graphs: {} ({} triples)", class_graph_count, class_triple_count);
    }
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

    // Also read kit from repo.yml and get the actual prefix from the TTL
    let kit_prefix = find_git_root().and_then(|r| {
        let content = fs::read_to_string(r.join(".lex").join("repo.yml")).ok()?;
        for line in content.lines() {
            if let Some(kit) = line.strip_prefix("kit: ") {
                let kit = kit.trim();
                if kit == "none" { return None; }
                // Read the kit TTL to find the actual prefix name
                // Try {kit}.ttl first, then find any .ttl in the kit dir
                let kit_dir = r.join(".lex").join("ontology").join("kit").join(kit);
                let ttl_path = kit_dir.join(format!("{}.ttl", kit));
                let ttl_path = if ttl_path.exists() { ttl_path } else {
                    fs::read_dir(&kit_dir).ok()
                        .and_then(|entries| entries.filter_map(|e| e.ok())
                            .find(|e| e.path().extension().is_some_and(|ext| ext == "ttl"))
                            .map(|e| e.path()))
                        .unwrap_or(ttl_path)
                };
                let kit_ns_pattern = format!("/kit/{}/", kit);
                if let Ok(ttl) = fs::read_to_string(&ttl_path) {
                    for tline in ttl.lines() {
                        if tline.starts_with("@prefix ") && tline.contains(&kit_ns_pattern) {
                            if let Some(colon_pos) = tline[8..].find(':') {
                                let pname = tline[8..8 + colon_pos].trim();
                                let ns = format!("https://repolex.ai/ontology/kit/{}/", kit);
                                return Some((
                                    format!("{}:", pname),
                                    format!("PREFIX {}: <{}>", pname, ns),
                                ));
                            }
                        }
                    }
                }
                // Fallback: use kit name as prefix
                return Some((
                    format!("{}:", kit),
                    format!("PREFIX {}: <https://repolex.ai/ontology/kit/{}/>", kit, kit),
                ));
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
        Commands::Validate => {
            if !cmd_validate() {
                exit(1);
            }
        }
        Commands::Join { squad_path } => cmd_join(&squad_path),
        Commands::Identity => cmd_identity(),
        Commands::Parse { file } => cmd_parse(&file),
        Commands::Viz { port } => cmd_viz(port),
        Commands::Display { query, port } => cmd_display(&query, port),
        Commands::Kit { command } => match command {
            KitCommands::Update => cmd_kit_update(),
            KitCommands::List => cmd_kit_list(),
        },
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
