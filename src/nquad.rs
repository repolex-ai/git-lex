//! N-Quad / N-Triple encoding and generation.
//!
//! Low-level escapers (`nq_escape`, `uri_encode_path`) plus the N-Quad
//! generators that produce git-lex's "now" view of the world:
//!
//! - `generate_git_nquads` — git-layer triples (commits, tree, refs, blame,
//!   changesets, language detection) across multiple named graphs.
//! - `generate_frontmatter_nquads` — the "now" graph: current-state frontmatter
//!   extraction, body wikilinks.
//! - `load_lex_nquads` — slurp any `.lex/**/*.nq` files the user wrote by hand.
//!
//! Peeled out of `main.rs` during modularization.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use git_lex::{find_git_root, get_kit};

use crate::git::{base_uri, git_unescape_path};
use crate::extraction::{flatten_yaml, normalize_wikilink_path,
                        resolve_slug_to_uri};
use crate::ontology::{get_object_properties, get_property_datatypes};
use crate::resolve;

/// Escape a string for use in N-Quads literals.
pub(crate) fn nq_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Percent-encode a path for use in URIs (spaces, special chars, non-ASCII).
pub(crate) fn uri_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '{' => out.push_str("%7B"),
            '}' => out.push_str("%7D"),
            '|' => out.push_str("%7C"),
            '^' => out.push_str("%5E"),
            '`' => out.push_str("%60"),
            '[' => out.push_str("%5B"),
            ']' => out.push_str("%5D"),
            c if !c.is_ascii() => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("%{:02X}", b));
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Generate all virtual N-Quads from git (commits, tree, refs).
pub(crate) fn generate_git_nquads() -> String {
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
pub(crate) fn load_lex_nquads() -> String {
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

/// Extract frontmatter, body wikilinks, and @mentions from all .md/.txt files
/// in the repo into the "now" graph. Also writes `.fm.spo` sidecars and scans
/// commit messages for mentions/wikilinks.
pub(crate) fn generate_frontmatter_nquads() -> String {
    let root = match find_git_root() {
        Some(r) => r,
        None => return String::new(),
    };

    let base = base_uri();
    // The "now" graph is the canonical view of current state — extracted
    // frontmatter, body wikilinks/mentions, and any triples derived from the
    // working tree as it exists right now. Contrasts with sync/<sha>/ graphs,
    // which hold historical snapshots at past commits. Previously named
    // "frontmatter" but that was misleading: this graph holds more than the
    // fm: namespace (wikilinks, mentions, etc are also here).
    let graph = format!("<{}/now>", base);
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

    let (slug_index, path_index) = build_slug_path_indexes(&root, &files);

    // entity_classes was used by the old range-aware resolver, which has been
    // replaced by src/resolve.rs. The range-check approach (matching class IRIs
    // across kits) had a cross-kit identity bug (squad:Agent ≠ soul:Agent) and
    // is deferred until cross-kit class equivalence is designed. For now the
    // resolver trusts bare-slug + full-IRI resolution without range filtering.

    // Ensure extract dir exists
    let extract_dir = root.join(".lex").join("extract");
    fs::create_dir_all(&extract_dir).ok();

    // Regex pattern for [[wikilinks]].
    // @mentions removed — they were blog inheritance with no job in a system
    // where everything is a document. Canonical direction: [[Class/id]].
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

        let _short_hash = if blob_hash.len() >= 8 { &blob_hash[..8] } else { &blob_hash };

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

        // --- Generate N-Quads for oxigraph (now graph) ---
        // IRI scheme: https://{host}/{org}/{repo}/{path-as-on-disk}
        // The IRI mirrors the file path verbatim — no folder capitalization,
        // no folder→class derivation. Classes come from the ontology and from
        // explicit dot-notation in frontmatter (kit.class.property), never
        // from folder name guessing. Honors "ontology is the single source
        // of truth" — sync stops inventing types the schema does not declare.
        //
        // No-kit repos get `lex:Document` only (plus the git layer).
        // Kit repos get classes their ontology declares, via frontmatter.
        let doc_uri = format!("<{}/{}>", base, uri_encode_path(&relpath_str));

        nq.push_str(&format!(
            "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/lex/Document> {} .\n",
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
            emit_spo_line_nquads(
                line,
                &doc_uri,
                &graph,
                &base,
                &relpath_str,
                &slug_index,
                &path_index,
                &obj_props,
                &prop_datatypes,
                &mut emitted_types,
                &mut nq,
            );
        }
    }

    // --- Scan commit messages for [[wikilinks]] ---
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

/// Emit N-Quads for a single `.spo` line (`subject | predicate | object`).
///
/// This is the shared triple-emitter used by both `generate_frontmatter_nquads`
/// (the "now" graph builder) and the history-graph walker — so byte-identical
/// triples come out of both paths. Extracted from `generate_frontmatter_nquads`
/// as a behavior-preserving refactor; no logic changes.
///
/// Arguments:
/// - `line`: raw `.spo` line in `subject | predicate | object` form
/// - `doc_uri`: IRI of the containing document (with angle brackets)
/// - `graph`: target graph IRI (with angle brackets)
/// - `base`: base URI for the repo (no trailing slash)
/// - `relpath_str`: source document path relative to repo root (for warnings)
/// - `slug_index` / `path_index`: doc lookup tables
/// - `obj_props` / `prop_datatypes`: ontology-derived property metadata
/// - `emitted_types`: in/out dedup set — the caller must zero this per doc
///   so each document emits its `rdf:type` assertions at most once
/// - `out`: the N-Quad buffer being appended to
pub(crate) fn emit_spo_line_nquads(
    line: &str,
    doc_uri: &str,
    graph: &str,
    base: &str,
    relpath_str: &str,
    slug_index: &HashMap<String, String>,
    path_index: &HashSet<String>,
    obj_props: &HashSet<String>,
    prop_datatypes: &HashMap<String, String>,
    emitted_types: &mut HashSet<String>,
    out: &mut String,
) {
    let parts: Vec<&str> = line.splitn(3, " | ").collect();
    if parts.len() != 3 {
        return;
    }
    let subject = parts[0];
    let predicate = parts[1];
    let object = parts[2];

    if predicate == "linksTo" {
        // [[wikilink]] → lex:linksTo (resolved) or literal fallback (broken).
        //
        // Three resolution strategies, tried in order:
        //   1. Path-style — if the target contains `/`, treat it as a path
        //      relative to the source file's directory. Normalize `..`, look
        //      up against the path index.
        //   2. Trailing-segment fallback — if path resolution fails, take the
        //      last segment of the target and try the bare-wikilink path.
        //   3. Bare wikilink — slugify (lowercase, hyphens, alnum-only) and
        //      look up in the slug index keyed by file stem.
        //
        // Falls through to a literal linksTo only when all three miss.
        let source_dir = std::path::Path::new(relpath_str)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        let resolved_path: Option<String> = if object.contains('/') {
            normalize_wikilink_path(object, &source_dir)
                .filter(|p| path_index.contains(p))
        } else {
            None
        };

        let link_uri: Option<String> = if let Some(p) = resolved_path {
            Some(format!("<{}/{}>", base, uri_encode_path(&p)))
        } else {
            // Strategy 2: trailing-segment fallback if the target had a `/`.
            let candidate = if let Some(idx) = object.rfind('/') {
                &object[idx + 1..]
            } else {
                object
            };
            // Strip trailing .md if present so the stem matches the index.
            let stem = candidate.strip_suffix(".md").unwrap_or(candidate);
            // Strategy 3: slugify and look up in slug_index.
            let link_slug = stem.to_lowercase()
                .replace(' ', "-")
                .replace(|c: char| !c.is_alphanumeric() && c != '-', "");
            if !link_slug.is_empty() && slug_index.contains_key(&link_slug) {
                Some(resolve_slug_to_uri(&link_slug, base, slug_index))
            } else {
                None
            }
        };

        if let Some(uri) = link_uri {
            out.push_str(&format!(
                "{} <https://repolex.ai/ontology/git-lex/lex/linksTo> {} {} .\n",
                doc_uri, uri, graph
            ));
        } else {
            // Unresolved wikilink → flat literal on lex:linksTo.
            out.push_str(&format!(
                "{} <https://repolex.ai/ontology/git-lex/lex/linksTo> \"{}\" {} .\n",
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

            // Emit rdf:type from class segment (once per class).
            // The class segment in dot-notation is already capitalized
            // (e.g. squad.Task.assignedTo) and matches the ontology class
            // name exactly. No case transformation needed.
            let type_key = format!("{}.{}", kit_name, class_seg);
            if emitted_types.insert(type_key) {
                let type_uri = format!("<https://repolex.ai/ontology/kit/{}/{}>", kit_name, class_seg);
                out.push_str(&format!(
                    "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> {} {} .\n",
                    doc_uri, type_uri, graph
                ));
            }

            // Property name passes through as-is (camelCase from ontology)
            let kit_predicate = format!("<https://repolex.ai/ontology/kit/{}/{}>", kit_name, prop_seg);

            // Check if this is an ObjectProperty (from ontology) → resolve as IRI
            if obj_props.contains(prop_seg) {
                // ObjectProperty: split on commas, resolve each value
                // via the canonical resolver (see src/resolve.rs).
                let values: Vec<&str> = object.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
                for val in values {
                    if val.is_empty() { continue; }
                    match resolve::resolve_frontmatter_value(val, slug_index, base) {
                        resolve::ResolveResult::Iri(uri) => {
                            out.push_str(&format!(
                                "{} {} {} {} .\n",
                                doc_uri, kit_predicate, uri, graph
                            ));
                        }
                        resolve::ResolveResult::Unresolved(literal) => {
                            out.push_str(&format!(
                                "{} {} \"{}\" {} .\n",
                                doc_uri, kit_predicate, nq_escape(&literal), graph
                            ));
                        }
                        resolve::ResolveResult::Rejected(msg) => {
                            eprintln!(
                                "warning: {}: {} — {}",
                                relpath_str, prop_seg, msg
                            );
                        }
                    }
                }
            } else {
                // DatatypeProperty: typed literal if ontology specifies a non-string range.
                if let Some(datatype) = prop_datatypes.get(prop_seg) {
                    out.push_str(&format!(
                        "{} {} \"{}\"^^<{}> {} .\n",
                        doc_uri, kit_predicate, nq_escape(object), datatype, graph
                    ));
                } else {
                    out.push_str(&format!(
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
                        resolve_slug_to_uri(&slug, base, slug_index)
                    };
                    out.push_str(&format!(
                        "{} {} {} {} .\n",
                        doc_uri, fm_predicate, object_uri, graph
                    ));
                }
            } else {
                out.push_str(&format!(
                    "{} {} \"{}\" {} .\n",
                    doc_uri, fm_predicate, nq_escape(object), graph
                ));
            }
        }
    }
}

/// Build the slug→path and path indexes used for @mention / [[wikilink]]
/// resolution.
///
/// Takes the repo root and a list of `.md` / `.txt` file paths, returns:
/// - `slug_index`: lowercase filename stem → relative path (with a dot-stripped
///   alias key for handles like `@spaceg.o.a.t.` → `spacegoat`). Template files
///   (prefix `__`) are excluded.
/// - `path_index`: set of relative paths, for path-style wikilink resolution.
///
/// Extracted from `generate_frontmatter_nquads` so the history-graph walker
/// can build the same indexes and produce byte-identical triples when replaying
/// historical `.spo` line events through `emit_spo_line_nquads`.
///
/// Known fragility: slug_index is inherently collision-prone (two files with
/// the same stem in different folders). The canonical direction is to prefer
/// full-path wikilinks `[[Class/id]]` going forward and retain slug_index only
/// as a shim for legacy `@mention`-style content.
pub(crate) fn build_slug_path_indexes(
    root: &std::path::Path,
    files: &[PathBuf],
) -> (HashMap<String, String>, HashSet<String>) {
    let mut slug_index: HashMap<String, String> = HashMap::new();
    let mut path_index: HashSet<String> = HashSet::new();
    for f in files {
        if let Ok(rel) = f.strip_prefix(root) {
            let rel_str = rel.to_string_lossy().to_string();
            path_index.insert(rel_str.clone());
            if let Some(file_name) = f.file_stem() {
                let slug = file_name.to_string_lossy().to_lowercase();
                if slug.starts_with("__") { continue; }
                slug_index.insert(slug.clone(), rel_str.clone());
                let dotless = slug.replace('.', "");
                if dotless != slug {
                    slug_index.entry(dotless).or_insert(rel_str);
                }
            }
        }
    }
    (slug_index, path_index)
}

