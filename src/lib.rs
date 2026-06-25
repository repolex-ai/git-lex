//! Shared utilities for the git-lex crate.
//!
//! Used by both `git-lex` (the CLI) and `git-lex-serve` (the server binary).

use oxigraph::store::Store;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

pub const KIT_GITHUB_ORG: &str = "repolex-ai";

/// Find the root of the current git repo.
pub fn find_git_root() -> Option<PathBuf> {
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

/// Path to the oxigraph store directory: `{repo_root}/.git/lex/oxigraph/`.
/// Lives under .git/ because the store is derived data (rebuildable from
/// .spo sidecars) and should never be version-controlled.
pub fn store_path() -> Option<PathBuf> {
    find_git_root().map(|r| r.join(".git").join("lex").join("oxigraph"))
}

/// Open the persistent store in read-only mode. Does not acquire the
/// RocksDB write lock, so writers (`git lex sync`, `git lex save`) can run
/// concurrently. The view is a snapshot from open-time and will not reflect
/// later writes until the store is reopened.
pub fn open_store_read_only() -> Option<Store> {
    let path = store_path()?;
    if path.exists() {
        Store::open_read_only(&path).ok()
    } else {
        None
    }
}

/// Read the kit spec from `.lex/repo.yml`. Returns None if no kit or kit is "none".
// FIXME(w4r3z, Day 38): hand-rolled YAML parse via `strip_prefix("kit: ")` —
// brittle: breaks on `kit:  soul` (two spaces), `kit:\tsoul` (tab), a trailing
// comment (`kit: soul  # ...`), or quoted values. The crate ALREADY depends on
// serde_yaml (used in extraction); parse repo.yml into a struct once and read
// fields off it. NOTE: `add_prefixes` below parses repo.yml's `kit:` AGAIN by
// hand (line ~236) — two independent brittle parsers for the same file. Unify.
pub fn get_kit() -> Option<String> {
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

// ─── Machine-level registry (~/.lex/repos) ───────��─────────────

/// Path to the machine-level registry file: `~/.lex/repos`.
/// One absolute path per line, each pointing to a git-lex repo on this machine.
fn registry_path() -> Option<PathBuf> {
    // HOME on macOS/Linux, USERPROFILE on Windows
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(|h| PathBuf::from(h).join(".lex").join("repos"))
}

/// Register a repo path in `~/.lex/repos`. Idempotent — won't add duplicates.
/// Creates `~/.lex/` if it doesn't exist.
pub fn registry_add(repo_path: &std::path::Path) {
    let reg = match registry_path() {
        Some(p) => p,
        None => return,
    };
    fs::create_dir_all(reg.parent().unwrap()).ok();

    let canonical = match repo_path.canonicalize() {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(_) => repo_path.to_string_lossy().to_string(),
    };

    // Read existing entries, check for duplicates
    let existing = fs::read_to_string(&reg).unwrap_or_default();
    for line in existing.lines() {
        if line.trim() == canonical {
            return; // already registered
        }
    }

    // Append
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&reg)
        .expect("failed to open ~/.lex/repos");
    use std::io::Write;
    writeln!(file, "{}", canonical).ok();
}

/// Remove a repo path from `~/.lex/repos`. No-op if not found.
pub fn registry_remove(repo_path: &std::path::Path) {
    let reg = match registry_path() {
        Some(p) => p,
        None => return,
    };

    let canonical = match repo_path.canonicalize() {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(_) => repo_path.to_string_lossy().to_string(),
    };

    let existing = match fs::read_to_string(&reg) {
        Ok(s) => s,
        Err(_) => return,
    };

    let filtered: Vec<&str> = existing.lines()
        .filter(|l| l.trim() != canonical)
        .collect();
    fs::write(&reg, filtered.join("\n") + "\n").ok();
}

/// Check if a repo path is already in `~/.lex/repos`.
pub fn registry_contains(repo_path: &std::path::Path) -> bool {
    let reg = match registry_path() {
        Some(p) => p,
        None => return false,
    };

    let canonical = match repo_path.canonicalize() {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(_) => repo_path.to_string_lossy().to_string(),
    };

    let existing = fs::read_to_string(&reg).unwrap_or_default();
    existing.lines().any(|l| l.trim() == canonical)
}

/// Prune stale entries from `~/.lex/repos`. Removes any path where the
/// directory no longer exists or no longer contains a `.lex/` subdirectory.
/// Returns the number of entries removed.
pub fn registry_check() -> usize {
    let reg = match registry_path() {
        Some(p) => p,
        None => return 0,
    };

    let existing = match fs::read_to_string(&reg) {
        Ok(s) => s,
        Err(_) => return 0,
    };

    let mut kept = Vec::new();
    let mut pruned = 0usize;
    for line in existing.lines() {
        let path = line.trim();
        if path.is_empty() { continue; }
        let p = std::path::Path::new(path);
        if p.is_dir() && p.join(".lex").is_dir() {
            kept.push(path);
        } else {
            pruned += 1;
        }
    }

    if pruned > 0 {
        fs::write(&reg, kept.join("\n") + "\n").ok();
    }
    pruned
}

/// List all registered repo paths from `~/.lex/repos`.
pub fn registry_list() -> Vec<String> {
    let reg = match registry_path() {
        Some(p) => p,
        None => return vec![],
    };
    let content = fs::read_to_string(&reg).unwrap_or_default();
    content.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Resolve a kit spec into (org, repo, short_name). Accepts either a short
/// form (`soul`) which is sugar for `repolex-ai/git-lex-kit-{name}`, or a
/// full `org/repo` form.
pub fn resolve_kit_spec(spec: &str) -> (String, String, String) {
    if let Some((org, repo)) = spec.split_once('/') {
        let short = repo
            .strip_prefix("git-lex-kit-")
            .unwrap_or(repo)
            .to_string();
        (org.to_string(), repo.to_string(), short)
    } else {
        (
            KIT_GITHUB_ORG.to_string(),
            format!("git-lex-kit-{}", spec),
            spec.to_string(),
        )
    }
}

/// Install dir for a given kit spec, relative to the repo root.
/// `.lex/kit/{org}/{repo}/`.
pub fn kit_install_dir_for_spec(root: &std::path::Path, spec: &str) -> PathBuf {
    let (org, repo, _) = resolve_kit_spec(spec);
    root.join(".lex").join("kit").join(&org).join(&repo)
}

/// THE canonical install path for a static kit's ontology TTL, relative to the
/// repo root: `.lex/ontology/{short}/{short}.ttl`.
///
/// This is a CONTRACT, not a guess. A static kit ships `ontology/{short}/{short}.ttl`
/// and `git lex kit` copies that subtree verbatim into `.lex/ontology/`, so this is
/// exactly where the file lands (see `kit.rs` install dest + `find_kit_ttl` primary
/// tier — both MUST agree with this function by construction). Downstream consumers
/// (e.g. Pool's `locate_kit_copia_ontology`) may rely on this path ALONE; the older
/// `.lex/kit/{org}/{repo}/` and `{Short}/.kit/` layouts are legacy and are swept
/// forward by `git lex kit-update`.
///
/// Pinned by `canonical_kit_ontology_path_is_stable` — if this formula ever changes,
/// that test breaks loudly so the move is deliberate (and consumers get re-notified)
/// rather than silent. (#8 / EDGE-1: git-lex moved this path twice historically,
/// forcing every consumer to carry a fallback chain. One pinned path ends that.)
pub fn canonical_kit_ontology_path(root: &std::path::Path, spec: &str) -> PathBuf {
    let (_, _, short) = resolve_kit_spec(spec);
    root.join(".lex")
        .join("ontology")
        .join(&short)
        .join(format!("{short}.ttl"))
}

/// Auto-inject SPARQL prefixes into a query string. Adds standard prefixes
/// (git:, lex:, fm:, rdf:, rdfs:, owl:, xsd:) plus the content ontology
/// prefix (o:) and the kit prefix if one is configured.
pub fn add_prefixes(query: &str) -> String {
    // Get first commit SHA for content ontology prefix
    let first_commit = Command::new("git")
        .args(["rev-list", "--max-parents=0", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim()[..8].to_string())
        .unwrap_or_default();
    let o_prefix = format!("PREFIX o: <https://repolex.ai/ont/{}/>", first_commit);

    // Read kit from repo.yml, then pull the kit's prefix+namespace from its
    // installed SHACL shapes file (the runtime source of truth). Shapes live
    // at .lex/ontology/{short}/{short}-shapes.ttl.
    let kit_prefix = find_git_root().and_then(|r| {
        let content = fs::read_to_string(r.join(".lex").join("repo.yml")).ok()?;
        for line in content.lines() {
            if let Some(kit) = line.strip_prefix("kit: ") {
                let kit = kit.trim();
                if kit == "none" { return None; }
                let (_, _, short) = resolve_kit_spec(kit);
                let shapes_path = r
                    .join(".lex")
                    .join("ontology")
                    .join(&short)
                    .join(format!("{}-shapes.ttl", short));
                let kit_ns_pattern = format!("/kit/{}/", short);
                if let Ok(ttl) = fs::read_to_string(&shapes_path) {
                    for tline in ttl.lines() {
                        let tline = tline.trim();
                        if tline.starts_with("@prefix ") && tline.contains(&kit_ns_pattern) {
                            if let Some(colon_pos) = tline[8..].find(':') {
                                let pname = tline[8..8 + colon_pos].trim();
                                let ns_start = tline.find('<');
                                let ns_end = tline.find('>');
                                let ns = match (ns_start, ns_end) {
                                    (Some(s), Some(e)) if s < e => tline[s + 1..e].to_string(),
                                    _ => format!("https://repolex.ai/ontology/kit/{}/", short),
                                };
                                return Some((
                                    format!("{}:", pname),
                                    format!("PREFIX {}: <{}>", pname, ns),
                                ));
                            }
                        }
                    }
                }
                // Fallback: use short kit name as prefix
                return Some((
                    format!("{}:", short),
                    format!("PREFIX {}: <https://repolex.ai/ontology/kit/{}/>", short, short),
                ));
            }
        }
        None
    });

    let mut defaults = vec![
        ("git:".to_string(), "PREFIX git: <https://repolex.ai/ontology/git-lex/git/>".to_string()),
        ("lex:".to_string(), "PREFIX lex: <https://repolex.ai/ontology/git-lex/lex/>".to_string()),
        ("fm:".to_string(), "PREFIX fm: <https://repolex.ai/ontology/git-lex/fm/>".to_string()),
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
    // FIXME(w4r3z, Day 38): prefix detection is naive substring match —
    // `query.contains("o:")` matches any token containing "o:" (e.g. another
    // prefix, or "http://..." inside a literal IRI), so `o:` (the content
    // ontology, namespaced DIFFERENTLY at /ont/<8charsha>/ vs everyone else's
    // /ontology/...) gets injected spuriously, and a query using a literal that
    // happens to contain "git:" pulls in unwanted PREFIXes. Match prefix tokens
    // on a word boundary (regex `\b<short>` or tokenize), not raw contains().
    // QUESTION: why does `o:` use /ont/<8-char-sha>/ while identity uses the
    // FULL sha at urn:soul:<sha>? Two SHA lengths + two ontology roots for the
    // "same" repo is a latent mismatch worth reconciling for the soft-release.
    let upper = query.to_uppercase();
    let mut prefix_block = String::new();
    for (short, full) in &defaults {
        if query.contains(short) && !upper.contains(&format!("PREFIX {}", short.to_uppercase())) {
            prefix_block.push_str(full);
            prefix_block.push('\n');
        }
    }
    if !upper.contains("PREFIX") {
        for (_, full) in &defaults {
            prefix_block.push_str(full);
            prefix_block.push('\n');
        }
    }
    format!("{}{}", prefix_block, query)
}
