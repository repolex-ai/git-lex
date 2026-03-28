use clap::{Parser, Subcommand};
use oxigraph::io::RdfFormat;
use oxigraph::model::*;
use oxigraph::sparql::SparqlEvaluator;
use oxigraph::store::Store;
use std::io::Cursor;
use std::path::PathBuf;
use std::process::{Command, exit};
use std::time::Instant;
use std::fs;
use std::collections::HashMap;

#[derive(Parser)]
#[command(name = "git-lex", about = "Git extensions for knowledge graphs")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize .lex/ in the current repo and install global drivers
    Init,
    /// Query the knowledge graph
    Query {
        /// The query string
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
    /// Dump all generated N-Quads to stdout (debug)
    Dump,
    /// Sync git data + .lex/*.nq into the persistent store
    Sync,
    /// Semantic diff of knowledge between refs
    Diff {
        /// Show changes since this date or ref
        #[arg(long)]
        since: Option<String>,
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
                "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/Commit> {} .",
                commit_uri, graph
            );
            println!(
                "{} <https://repolex.ai/ontology/git-lex/hexsha> \"{}\" {} .",
                commit_uri, sha, graph
            );
            println!(
                "{} <https://repolex.ai/ontology/git-lex/authorEmail> \"{}\" {} .",
                commit_uri,
                nq_escape(email),
                graph
            );
            println!(
                "{} <https://repolex.ai/ontology/git-lex/authorName> \"{}\" {} .",
                commit_uri,
                nq_escape(name),
                graph
            );
            println!(
                "{} <https://repolex.ai/ontology/git-lex/date> \"{}\"^^<http://www.w3.org/2001/XMLSchema#dateTime> {} .",
                commit_uri, date, graph
            );
            println!(
                "{} <https://repolex.ai/ontology/git-lex/message> \"{}\" {} .",
                commit_uri,
                nq_escape(subject),
                graph
            );
            for parent in parents.split_whitespace() {
                println!(
                    "{} <https://repolex.ai/ontology/git-lex/parent> <{}/commit/{}> {} .",
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
                "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/Blob> {} .",
                file_uri, graph
            );
            println!(
                "{} <https://repolex.ai/ontology/git-lex/path> \"{}\" {} .",
                file_uri,
                nq_escape(path),
                graph
            );
            println!(
                "{} <https://repolex.ai/ontology/git-lex/blobHash> \"{}\" {} .",
                file_uri, blob_hash, graph
            );
            println!(
                "{} <https://repolex.ai/ontology/git-lex/blob> <{}/blob/{}> {} .",
                file_uri, base_uri(), blob_hash, graph
            );
            println!(
                "{} <https://repolex.ai/ontology/git-lex/size> \"{}\"^^<http://www.w3.org/2001/XMLSchema#integer> {} .",
                file_uri, size, graph
            );
            println!(
                "{} <https://repolex.ai/ontology/git-lex/type> \"{}\" {} .",
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
                        "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/Branch> {} .",
                        ref_uri, graph
                    );
                    println!(
                        "{} <https://repolex.ai/ontology/git-lex/shortName> \"{}\" {} .",
                        ref_uri,
                        nq_escape(name),
                        graph
                    );
                    println!(
                        "{} <https://repolex.ai/ontology/git-lex/commit> <{}/commit/{}> {} .",
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
                        "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/Tag> {} .",
                        ref_uri, graph
                    );
                    println!(
                        "{} <https://repolex.ai/ontology/git-lex/shortName> \"{}\" {} .",
                        ref_uri,
                        nq_escape(name),
                        graph
                    );
                    println!(
                        "{} <https://repolex.ai/ontology/git-lex/commit> <{}/commit/{}> {} .",
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

fn cmd_init() {
    let root = match find_git_root() {
        Some(r) => r,
        None => {
            // Not a git repo — offer to create one
            let cwd = std::env::current_dir().expect("failed to get current directory");
            eprint!("Not a git repository. Initialize one in {}? [Y/n] ", cwd.display());
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).unwrap_or_default();
            let input = input.trim().to_lowercase();
            if input.is_empty() || input == "y" || input == "yes" {
                let status = Command::new("git")
                    .args(["init"])
                    .status();
                match status {
                    Ok(s) if s.success() => {
                        println!();
                    }
                    _ => {
                        eprintln!("fatal: failed to initialize git repository");
                        exit(1);
                    }
                }
                cwd
            } else {
                eprintln!("Aborted.");
                exit(1);
            }
        }
    };

    let lex_dir = root.join(".lex");
    let lex_exists = lex_dir.exists();

    fs::create_dir_all(lex_dir.join("graph")).expect("failed to create .lex/graph/");
    fs::create_dir_all(lex_dir.join("ontology")).expect("failed to create .lex/ontology/");

    let gitattributes = root.join(".gitattributes");
    let attr_content = "# git-lex: semantic diff/merge for knowledge graph files\n\
                        .lex/**/*.nq diff=lex merge=lex\n";

    if gitattributes.exists() {
        let existing = fs::read_to_string(&gitattributes).unwrap_or_default();
        if !existing.contains("diff=lex") {
            fs::write(&gitattributes, format!("{}\n{}", existing.trim_end(), attr_content))
                .expect("failed to update .gitattributes");
            println!("Updated .gitattributes with lex diff/merge drivers");
        }
    } else {
        fs::write(&gitattributes, attr_content).expect("failed to create .gitattributes");
        println!("Created .gitattributes");
    }

    // Ensure .store/ is in .gitignore (it's a local cache, not committed)
    let gitignore = root.join(".gitignore");
    let ignore_entry = ".lex/oxigraph/";
    if gitignore.exists() {
        let existing = fs::read_to_string(&gitignore).unwrap_or_default();
        if !existing.contains(ignore_entry) {
            fs::write(&gitignore, format!("{}\n{}\n", existing.trim_end(), ignore_entry))
                .expect("failed to update .gitignore");
        }
    } else {
        fs::write(&gitignore, format!("{}\n", ignore_entry))
            .expect("failed to create .gitignore");
    }

    let readme_path = lex_dir.join("README.md");
    if !readme_path.exists() {
        fs::write(
            &readme_path,
            "# .lex/\n\n\
             Knowledge graph index managed by git-lex.\n\n\
             - `graph/` — derived triples and relationships\n\
             - `ontology/` — ontology definitions and controlled vocabulary\n\n\
             Content lives in the repo root. This directory is the index layer.\n",
        )
        .expect("failed to create .lex/README.md");
    }

    if lex_exists {
        println!("Reinitialized .lex/ in {}", root.display());
    } else {
        println!("Initialized .lex/ in {}", root.display());
    }
    println!();
    println!("  .lex/graph/    — knowledge graph triples");
    println!("  .lex/ontology/ — ontology definitions");
    println!("  .gitattributes — semantic diff/merge drivers");
    println!();
    install_global_drivers();

    // Install post-commit hook for auto-sync
    let hooks_dir = root.join(".git").join("hooks");
    let hook_path = hooks_dir.join("post-commit");
    let hook_content = "#!/bin/sh\ngit-lex sync\n";
    if hook_path.exists() {
        let existing = fs::read_to_string(&hook_path).unwrap_or_default();
        if !existing.contains("git-lex sync") {
            fs::write(&hook_path, format!("{}\n{}", existing.trim_end(), hook_content))
                .expect("failed to update post-commit hook");
            println!("Updated post-commit hook with git-lex sync");
        }
    } else {
        fs::create_dir_all(&hooks_dir).ok();
        fs::write(&hook_path, hook_content).expect("failed to create post-commit hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755))
                .expect("failed to set hook permissions");
        }
        println!("Installed post-commit hook (auto-sync)");
    }

    // Check if repo has any commits
    let has_commits = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    // Commit 1: lex setup files
    let lex_files = [".lex/", ".gitattributes", ".gitignore"];
    for f in &lex_files {
        let _ = Command::new("git").args(["add", f]).status();
    }
    let status = Command::new("git")
        .args(["commit", "-m", "git lex init"])
        .output();
    match status {
        Ok(o) if o.status.success() => {
            println!("\nCommitted git-lex setup files.");
        }
        _ => {
            // May fail if nothing to commit (reinit case)
        }
    }

    // Commit 2: if this is a fresh repo with uncommitted content, commit it all
    if !has_commits || !lex_exists {
        let untracked = Command::new("git")
            .args(["status", "--porcelain"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();

        if !untracked.is_empty() {
            eprint!("Commit existing files to the repository? [Y/n] ");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).unwrap_or_default();
            let input = input.trim().to_lowercase();
            if input.is_empty() || input == "y" || input == "yes" {
                let _ = Command::new("git").args(["add", "."]).status();
                let _ = Command::new("git")
                    .args(["commit", "-m", "Initial content"])
                    .status();
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
                nq.push_str(&format!("{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/Commit> {} .\n", cu, graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/hexsha> \"{}\" {} .\n", cu, sha, graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/authorEmail> \"{}\" {} .\n", cu, nq_escape(email), graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/authorName> \"{}\" {} .\n", cu, nq_escape(name), graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/authoredDate> \"{}\"^^<http://www.w3.org/2001/XMLSchema#dateTime> {} .\n", cu, date, graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/committerEmail> \"{}\" {} .\n", cu, nq_escape(committer_email), graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/committerName> \"{}\" {} .\n", cu, nq_escape(committer_name), graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/committedDate> \"{}\"^^<http://www.w3.org/2001/XMLSchema#dateTime> {} .\n", cu, committer_date, graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/message> \"{}\" {} .\n", cu, nq_escape(subject), graph));
                for parent in parents.split_whitespace() {
                    nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/parent> <{}/commit/{}> {} .\n", cu, base, parent, graph));
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
                    nq.push_str(&format!("{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/Blob> {} .\n", fu, graph));
                    nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/path> \"{}\" {} .\n", fu, nq_escape(path), graph));
                    nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/blobHash> \"{}\" {} .\n", fu, blob_hash, graph));
                    nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/blob> <{}/blob/{}> {} .\n", fu, base, blob_hash, graph));
                    nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/size> \"{}\"^^<http://www.w3.org/2001/XMLSchema#integer> {} .\n", fu, size, graph));
                    nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/type> \"{}\" {} .\n", fu, obj_type, graph));
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
                nq.push_str(&format!("{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/Branch> {} .\n", ru, graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/shortName> \"{}\" {} .\n", ru, nq_escape(name), graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/commit> <{}/commit/{}> {} .\n", ru, base, sha, graph));
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
                nq.push_str(&format!("{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/Tag> {} .\n", ru, graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/shortName> \"{}\" {} .\n", ru, nq_escape(name), graph));
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/commit> <{}/commit/{}> {} .\n", ru, base, sha, graph));
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
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/changed> {} {} .\n", commit_uri, change_uri, commits_graph));

                // Change details
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/path> \"{}\" {} .\n", change_uri, nq_escape(path), graph));

                let status_label = match status.chars().next() {
                    Some('A') => "added",
                    Some('M') => "modified",
                    Some('D') => "deleted",
                    Some('R') => "renamed",
                    _ => "unknown",
                };
                nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/changeType> \"{}\" {} .\n", change_uri, status_label, graph));

                // For renames, capture the new path
                if status.starts_with('R') && parts.len() >= 3 {
                    nq.push_str(&format!("{} <https://repolex.ai/ontology/git-lex/renamedTo> \"{}\" {} .\n", change_uri, nq_escape(parts[2]), graph));
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
                                                    "{} <https://repolex.ai/ontology/git-lex/blamedAuthor> \"{}\" {} .\n",
                                                    file_uri, nq_escape(sig), graph
                                                ));
                                            }
                                        }
                                        if let Some(email) = hunk.final_signature().email() {
                                            let key = format!("email:{}:{}", email, path);
                                            if authors_seen.insert(key) {
                                                nq.push_str(&format!(
                                                    "{} <https://repolex.ai/ontology/git-lex/blamedEmail> \"{}\" {} .\n",
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
                            "{} <https://repolex.ai/ontology/git-lex/language> \"{}\" {} .\n",
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

/// Extract frontmatter from all .md files and generate N-Quads.
fn generate_frontmatter_nquads() -> String {
    let root = match find_git_root() {
        Some(r) => r,
        None => return String::new(),
    };

    let base = base_uri();
    let graph = format!("<{}/frontmatter>", base);
    let mut nq = String::new();

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

    for filepath in &files {
        let content = match fs::read_to_string(filepath) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Check for frontmatter (starts with ---)
        if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
            continue;
        }

        // Find the closing ---
        let rest = &content[4..]; // skip first "---\n"
        let end = match rest.find("\n---") {
            Some(pos) => pos,
            None => continue,
        };

        let yaml_str = &rest[..end];

        // Parse YAML
        let yaml: HashMap<String, serde_yaml::Value> = match serde_yaml::from_str(yaml_str) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let relpath = filepath.strip_prefix(&root).unwrap_or(filepath);
        let relpath_str = relpath.to_string_lossy();
        let doc_uri = format!("<{}/doc/{}>", base, uri_encode_path(&relpath_str));

        // Type the document
        nq.push_str(&format!(
            "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/lex-upper/Document> {} .\n",
            doc_uri, graph
        ));
        nq.push_str(&format!(
            "{} <https://repolex.ai/ontology/git-lex/path> \"{}\" {} .\n",
            doc_uri, nq_escape(&relpath_str), graph
        ));

        for (key, value) in &yaml {
            let predicate = format!("<https://repolex.ai/ontology/git-lex/fm/{}>", uri_encode_path(key));

            match value {
                serde_yaml::Value::String(s) => {
                    nq.push_str(&format!(
                        "{} {} \"{}\" {} .\n",
                        doc_uri, predicate, nq_escape(s), graph
                    ));
                }
                serde_yaml::Value::Sequence(seq) => {
                    for item in seq {
                        if let Some(s) = item.as_str() {
                            nq.push_str(&format!(
                                "{} {} \"{}\" {} .\n",
                                doc_uri, predicate, nq_escape(s), graph
                            ));
                        }
                    }
                }
                serde_yaml::Value::Bool(b) => {
                    nq.push_str(&format!(
                        "{} {} \"{}\"^^<http://www.w3.org/2001/XMLSchema#boolean> {} .\n",
                        doc_uri, predicate, b, graph
                    ));
                }
                serde_yaml::Value::Number(n) => {
                    nq.push_str(&format!(
                        "{} {} \"{}\" {} .\n",
                        doc_uri, predicate, n, graph
                    ));
                }
                _ => {}
            }
        }
    }

    nq
}

fn cmd_sync() {
    let start = Instant::now();

    let store = open_or_create_store();

    // Clear and reload — simple for now, incremental later
    store.clear().expect("failed to clear store");

    // Git virtual triples
    let git_nq = generate_git_nquads();
    let git_count = git_nq.lines().count();
    store
        .load_from_reader(RdfFormat::NQuads, Cursor::new(git_nq.as_bytes()))
        .expect("failed to load git triples");

    // Frontmatter triples
    let fm_nq = generate_frontmatter_nquads();
    let fm_count = fm_nq.lines().filter(|l| !l.is_empty()).count();
    if !fm_nq.is_empty() {
        store
            .load_from_reader(RdfFormat::NQuads, Cursor::new(fm_nq.as_bytes()))
            .expect("failed to load frontmatter triples");
    }

    // .lex/*.nq files
    let lex_nq = load_lex_nquads();
    let lex_count = lex_nq.lines().filter(|l| !l.is_empty()).count();
    if !lex_nq.is_empty() {
        store
            .load_from_reader(RdfFormat::NQuads, Cursor::new(lex_nq.as_bytes()))
            .expect("failed to load .lex/ triples");
    }

    store.flush().expect("failed to flush store");

    let elapsed = start.elapsed();
    println!(
        "Synced {} git + {} frontmatter + {} lex = {} total triples in {:.1}ms",
        git_count,
        fm_count,
        lex_count,
        git_count + fm_count + lex_count,
        elapsed.as_secs_f64() * 1000.0
    );
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

    let defaults = [
        ("lex:", "PREFIX lex: <https://repolex.ai/ontology/git-lex/>".to_string()),
        ("lex-o:", "PREFIX lex-o: <https://repolex.ai/ontology/lex-upper/>".to_string()),
        ("o:", o_prefix),
        ("rdf:", "PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>".to_string()),
        ("rdfs:", "PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>".to_string()),
        ("owl:", "PREFIX owl: <http://www.w3.org/2002/07/owl#>".to_string()),
        ("xsd:", "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>".to_string()),
    ];
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

/// Execute a SPARQL query on a store and print results.
fn run_query(store: &Store, query: &str, store_type: &str) {
    let start = Instant::now();
    let prefixed = add_prefixes(query);

    let evaluator = match SparqlEvaluator::new().parse_query(&prefixed) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("SPARQL parse error: {}", e);
            exit(1);
        }
    };
    let results = match evaluator.on_store(store).execute() {
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

            for solution in solutions {
                let solution = solution.expect("failed to read solution");
                count += 1;
                let mut parts = Vec::new();
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
                    parts.push(format!("{}={}", var, val));
                }
                println!("{}", parts.join(" | "));
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
        Commands::Init => cmd_init(),
        Commands::Status => cmd_status(),
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
        Commands::Sync => cmd_sync(),
        Commands::Diff { since } => {
            println!(
                "git lex diff {} — not yet implemented",
                since.unwrap_or_else(|| "(working tree)".to_string())
            );
        }
    }
}
