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
/// RocksDB write lock, so the writer (`git lex sync`) can run
/// concurrently. The view is a snapshot from open-time and will not reflect
/// later writes until the store is reopened.
pub fn open_store_read_only() -> Option<Store> {
    open_store_read_only_at(&find_git_root()?)
}

/// Store dir for an EXPLICIT repo root (multi-repo servers) — the single
/// authority for the `.git/lex/oxigraph` layout.
pub fn store_path_at(root: &std::path::Path) -> PathBuf {
    root.join(".git").join("lex").join("oxigraph")
}

/// [`open_store_read_only`] for an explicit repo root.
pub fn open_store_read_only_at(root: &std::path::Path) -> Option<Store> {
    let path = store_path_at(root);
    if path.exists() {
        Store::open_read_only(&path).ok()
    } else {
        None
    }
}

/// The domain kit spec from `.lex/repo.yml` (None if unset or "none").
pub fn get_kit() -> Option<String> {
    RepoYml::load(&find_git_root()?).domain_kit()
}

/// Evaluate a SPARQL query on a store via the current oxigraph API
/// (`SparqlEvaluator`) — the deprecated `Query::parse` + `Store::query`
/// pair lived at nine call sites; this is the one replacement.
pub fn eval_query<'a>(
    store: &'a Store,
    q: &str,
) -> Result<oxigraph::sparql::QueryResults<'a>, String> {
    oxigraph::sparql::SparqlEvaluator::new()
        .parse_query(q)
        .map_err(|e| format!("parse: {e}"))?
        .on_store(store)
        .execute()
        .map_err(|e| format!("eval: {e}"))
}


// ─── repo.yml — the ONE reader ─────────────────────────────────

/// The parsed `.lex/repo.yml`. SEVEN hand-rolled line scanners of this one
/// file (each with different whitespace/quoting/list rules — an observed
/// drift source) collapsed into this struct. Read side only: writers still
/// edit the file textually to preserve comments and ordering.
#[derive(Debug, Default, serde::Deserialize)]
pub struct RepoYml {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kit: Option<String>,
    #[serde(default)]
    pub agent_name: Option<String>,
    #[serde(default)]
    pub agent_email: Option<String>,
    #[serde(default)]
    pub first_commit: Option<String>,
    /// DEV-ONLY stopgap: a date; history walking starts at the first
    /// commit after it. Exists for the ~10 pre-v1 squad repos whose early
    /// development churn predates the data rules. Normal repos never set
    /// this.
    #[serde(default)]
    pub dev_history_horizon: Option<String>,
    /// Wikilink resolution semantics — a MIGRATION FENCE, not user config
    /// (same lifecycle as dev_history_horizon: deletable once every repo
    /// has crossed). `git lex init` stamps "obsidian" on NEW repos: bare
    /// targets are repo-root-relative, leading `/` is rejected at save
    /// (Rob-ruled 2026-08-01). Absent = the legacy 2026-07-28 markdown
    /// semantics (bare = source-folder-relative, `/` = repo-rooted) —
    /// pre-existing repos keep it until their Phase-4 migration flips
    /// them. KEY NAME PENDING ROB — nothing writes it until he picks.
    #[serde(default)]
    pub link_semantics: Option<String>,
    #[serde(default)]
    pub optional_kits: Vec<String>,
    #[serde(default)]
    pub substrates: Vec<String>,
    /// Every other key, preserved for init's re-init carryover.
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_yaml::Value>,
}

impl RepoYml {
    /// Load from `<root>/.lex/repo.yml`. Missing file = Default. A file
    /// that exists but is not valid YAML WARNS loudly and returns Default —
    /// never a silent misread.
    pub fn load(root: &std::path::Path) -> RepoYml {
        Self::load_path(&root.join(".lex").join("repo.yml"))
    }

    /// [`RepoYml::load`] for an explicit file path.
    pub fn load_path(path: &std::path::Path) -> RepoYml {
        let Ok(content) = fs::read_to_string(path) else {
            return RepoYml::default();
        };
        match serde_yaml::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "warning: {} is not valid YAML ({e}) — treating as empty",
                    path.display()
                );
                RepoYml::default()
            }
        }
    }

    /// True when this repo uses Obsidian wikilink semantics (bare targets
    /// are repo-root-relative; leading `/` rejected). False = legacy
    /// 2026-07-28 markdown semantics.
    pub fn obsidian_links(&self) -> bool {
        self.link_semantics.as_deref().map(str::trim) == Some("obsidian")
    }

    /// The domain kit, if configured and not "none".
    pub fn domain_kit(&self) -> Option<String> {
        match self.kit.as_deref().map(str::trim) {
            None | Some("") | Some("none") => None,
            Some(k) => Some(k.to_string()),
        }
    }

    /// All scalar fields as strings (known + extra) — what init's re-init
    /// carryover preserves.
    pub fn scalar_fields(&self) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        let mut put = |k: &str, v: &Option<String>| {
            if let Some(v) = v {
                if !v.is_empty() { out.insert(k.to_string(), v.clone()); }
            }
        };
        put("name", &self.name);
        put("kit", &self.kit);
        put("agent_name", &self.agent_name);
        put("agent_email", &self.agent_email);
        put("first_commit", &self.first_commit);
        for (k, v) in &self.extra {
            let s = match v {
                serde_yaml::Value::String(s) => s.clone(),
                serde_yaml::Value::Number(n) => n.to_string(),
                serde_yaml::Value::Bool(b) => b.to_string(),
                _ => continue,
            };
            if !s.is_empty() { out.insert(k.clone(), s); }
        }
        out
    }
}

// ─── Kit namespace derivation (ONE authority) ──────────────────

/// The conventional kit namespace, used ONLY as a fallback when no installed
/// TTL declares one (e.g. frontmatter referencing a kit that isn't
/// installed). Everywhere else the kit's own `@prefix` declaration is the
/// authority — this function is the single place the convention is written
/// down. The convention is the app-tier pattern (`ontology/<short>/`); the
/// old `ontology/kit/` tier is ruled dead (2026-07-24 flip).
pub fn conventional_kit_namespace(short: &str) -> String {
    format!("https://repolex.ai/ontology/{}/", short)
}

/// The repo's a-box namespace base — DERIVED, never configured and never
/// hardcoded (Rob-ruled 2026-07-28: "nothing should be hardcoded to soul").
///
/// Derivation: `https://repolex.ai/<ns>` where `<ns>` is
///   1. the domain kit's short name (`kit: soul` → `soul`) — so every
///      soul-kit repo keeps its existing IRIs verbatim;
///   2. else the repo.yml `name:`, slugified — kit-less repos get their
///      own namespace instead of being wrongly stamped `soul`;
///   3. else `repo` — a last-resort constant so IRIs stay valid.
pub fn resource_base_at(root: &std::path::Path) -> String {
    let y = RepoYml::load(root);
    let ns = y
        .domain_kit()
        .map(|k| resolve_kit_spec(&k).2)
        .or_else(|| y.name.as_deref().map(slugify_ns).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "repo".to_string());
    format!("https://repolex.ai/{ns}")
}

/// Slugify a repo name into a namespace segment: lowercase, spaces → `-`,
/// anything not alphanumeric/dash/dot dropped.
fn slugify_ns(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .replace(' ', "-")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
        .collect()
}

/// Find a kit's own prefix declaration in TTL content. Returns
/// `(prefix_name, namespace)`.
///
/// Primary rule: the `@prefix` whose NAME equals the short kit name
/// (`@prefix soul: <...>` for kit `soul`). Matching by NAME — not by
/// namespace pattern — is what makes a namespace migration a one-line TTL
/// edit: every consumer (emitters, shapes generation, query prefix
/// injection) derives whatever the TTL declares, so the declaration can
/// move without touching code. (The old scanners matched the literal
/// `/kit/{short}/` substring and would have silently fallen back to the
/// retired pattern the moment a TTL migrated.)
///
/// Fallback rule: the first `@prefix` that isn't W3C boilerplate or the
/// base `git-lex:`/`git2:` namespaces — for kits whose prefix name differs
/// from their short name.
pub fn extract_kit_prefix(content: &str, short: &str) -> Option<(String, String)> {
    let mut fallback: Option<(String, String)> = None;
    for line in content.lines() {
        let t = line.trim();
        // Accept every legal spelling: `@prefix`, Turtle-1.1 caseless
        // `PREFIX`/`prefix`, and tab or multi-space separators. A valid
        // declaration the scanner can't read silently becomes the
        // conventional-namespace fallback — wrong IRIs everywhere with
        // no warning (adversarial finding, attack 3).
        let lower = t.to_ascii_lowercase();
        let keyword_len = if lower.starts_with("@prefix") {
            "@prefix".len()
        } else if lower.starts_with("prefix") {
            "prefix".len()
        } else {
            continue;
        };
        // The keyword must be followed by whitespace ("prefixfoo:" is not
        // a declaration).
        match t.as_bytes().get(keyword_len) {
            Some(b' ') | Some(b'\t') => {}
            _ => continue,
        }
        let rest = t[keyword_len..].trim_start();
        let Some(colon) = rest.find(':') else { continue };
        let name = rest[..colon].trim();
        let after = &rest[colon + 1..];
        let (Some(s), Some(e)) = (after.find('<'), after.find('>')) else { continue };
        if s >= e { continue; }
        let ns = after[s + 1..e].to_string();
        if name == short {
            return Some((name.to_string(), ns));
        }
        let is_boilerplate = ns.contains("/shacl#")
            || ns.contains("XMLSchema")
            || ns.contains("rdf-schema")
            || ns.contains("22-rdf-syntax-ns")
            || ns.contains("/owl#")
            || name == "git-lex"
            || name == "git2";
        if !is_boilerplate && fallback.is_none() {
            fallback = Some((name.to_string(), ns));
        }
    }
    fallback
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
/// (git-lex:, git2:, git:, md:, fm:, rdf:, rdfs:, owl:, xsd:) plus the
/// kit prefix if one is configured.
pub fn add_prefixes(query: &str) -> String {
    add_prefixes_at(find_git_root().as_deref(), query)
}

/// [`add_prefixes`] anchored to an EXPLICIT repo root instead of the process
/// cwd — the form a multi-repo server (Syrinx) must use so each request gets
/// the prefixes of the soul it landed on, never the server's own cwd.
pub fn add_prefixes_at(root: Option<&std::path::Path>, query: &str) -> String {
    // Read kit from repo.yml, then pull the kit's prefix+namespace from its
    // installed SHACL shapes file (the runtime source of truth). Shapes live
    // at .lex/ontology/{short}/{short}-shapes.ttl.
    let kit_prefix = root.and_then(|r| {
        let kit = RepoYml::load(r).domain_kit()?;
        {
            {
                let (_, _, short) = resolve_kit_spec(&kit);
                let r = r.to_path_buf();
                let shapes_path = r
                    .join(".lex")
                    .join("ontology")
                    .join(&short)
                    .join(format!("{}-shapes.ttl", short));
                if let Ok(ttl) = fs::read_to_string(&shapes_path) {
                    if let Some((pname, ns)) = extract_kit_prefix(&ttl, &short) {
                        return Some((
                            format!("{}:", pname),
                            format!("PREFIX {}: <{}>", pname, ns),
                        ));
                    }
                }
                // Fallback: no installed declaration — conventional pattern.
                return Some((
                    format!("{}:", short),
                    format!("PREFIX {}: <{}>", short, conventional_kit_namespace(&short)),
                ));
            }
        }
    });

    let mut defaults = vec![
        ("git:".to_string(), "PREFIX git: <https://repolex.ai/ontology/git-lex/git/>".to_string()),
        ("git-lex:".to_string(), "PREFIX git-lex: <https://repolex.ai/ontology/git-lex/>".to_string()),
        ("git2:".to_string(), "PREFIX git2: <https://repolex.ai/ontology/git-lex/git2/>".to_string()),
        ("md:".to_string(), "PREFIX md: <https://repolex.ai/ontology/git-lex/md/>".to_string()),
        ("fm:".to_string(), "PREFIX fm: <https://repolex.ai/ontology/git-lex/fm/>".to_string()),
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
    // a query using a literal that happens to contain "git:" pulls in
    // unwanted PREFIXes. Harmless (an unused PREFIX changes nothing) but
    // sloppy; match on a word boundary if it ever bites. (The worst
    // offender, the retired `o:` prefix, was removed 2026-07-28 — it
    // pointed at a namespace nothing ever wrote to and cost a git
    // subprocess per query.)
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


// ─── W3C SPARQL query surface (Task 2 Part B) ────────────────────────────────
// ONE implementation shared by the CLI (`git lex query`) and the protocol
// endpoint (`git lex serve sparql`) — two SPARQL paths drifting was the bug
// class this kills.

/// One RDF term → W3C SPARQL 1.1 Query Results JSON object.
pub fn term_to_json(term: &oxigraph::model::Term) -> serde_json::Value {
    use oxigraph::model::Term;
    match term {
        Term::NamedNode(n) => serde_json::json!({
            "type": "uri",
            "value": n.as_str(),
        }),
        Term::BlankNode(b) => serde_json::json!({
            "type": "bnode",
            "value": b.as_str(),
        }),
        Term::Literal(l) => {
            let mut obj = serde_json::Map::new();
            obj.insert("type".to_string(), serde_json::Value::String("literal".to_string()));
            obj.insert("value".to_string(), serde_json::Value::String(l.value().to_string()));
            if let Some(lang) = l.language() {
                obj.insert("xml:lang".to_string(), serde_json::Value::String(lang.to_string()));
            } else {
                let dt = l.datatype().as_str();
                // W3C convention: only emit datatype if it's not the implicit xsd:string.
                if dt != "http://www.w3.org/2001/XMLSchema#string" {
                    obj.insert("datatype".to_string(), serde_json::Value::String(dt.to_string()));
                }
            }
            serde_json::Value::Object(obj)
        }
        // Not standard SPARQL JSON — RDF 1.2 triple terms. Emit as a nested
        // object with a "triple" type so consumers can detect and parse.
        Term::Triple(t) => serde_json::json!({
            "type": "triple",
            "value": {
                "subject": term_to_json_subject(&t.subject),
                "predicate": term_to_json(&oxigraph::model::Term::NamedNode(t.predicate.clone())),
                "object": term_to_json(&t.object),
            },
        }),
    }
}

/// Subject terms in this oxigraph version are `NamedOrBlankNode` — no
/// quoted-triple subjects yet. (RDF 1.2 triple terms are supported as
/// objects only.)
pub fn term_to_json_subject(subj: &oxigraph::model::NamedOrBlankNode) -> serde_json::Value {
    use oxigraph::model::{NamedOrBlankNode, Term};
    match subj {
        NamedOrBlankNode::NamedNode(n) => term_to_json(&Term::NamedNode(n.clone())),
        NamedOrBlankNode::BlankNode(b) => term_to_json(&Term::BlankNode(b.clone())),
    }
}

/// Outcome of a W3C protocol query, tagged with the correct media type.
pub enum W3cQueryOutcome {
    /// SELECT — `application/sparql-results+json`.
    Solutions(serde_json::Value),
    /// ASK — `application/sparql-results+json`.
    Boolean(serde_json::Value),
    /// CONSTRUCT/DESCRIBE — `application/n-triples`.
    Graph(String),
}

#[derive(Debug)]
pub enum W3cQueryError {
    /// Malformed query — the CALLER's error (HTTP 400).
    Parse(String),
    /// Evaluation failure — the STORE's error (HTTP 500).
    Eval(String),
}

/// Run `query` against `store` with the standard prefix prologue, producing
/// W3C-shaped results under STANDARD SPARQL dataset semantics — a bare
/// `?s ?p ?o` reads exactly the default graph. (The union-default-graph
/// switch was removed here per the Day-50 respec: it was a hackathon-era
/// patch over an empty default graph, and against the persistent store it
/// merged stale sync vintages into every answer. `git lex query`'s live
/// in-memory view keeps its own union by design — that door is the
/// git-to-RDF passthrough over its own freshly-built graphs.)
pub fn w3c_query(store: &Store, query: &str) -> Result<W3cQueryOutcome, W3cQueryError> {
    w3c_query_at(find_git_root().as_deref(), store, query)
}

/// [`w3c_query`] anchored to an explicit repo root (multi-repo servers).
pub fn w3c_query_at(
    root: Option<&std::path::Path>,
    store: &Store,
    query: &str,
) -> Result<W3cQueryOutcome, W3cQueryError> {
    let prefixed = add_prefixes_at(root, query);
    let results = oxigraph::sparql::SparqlEvaluator::new()
        .parse_query(&prefixed)
        .map_err(|e| W3cQueryError::Parse(e.to_string()))?
        .on_store(store)
        .execute()
        .map_err(|e| W3cQueryError::Eval(e.to_string()))?;
    match results {
        oxigraph::sparql::QueryResults::Solutions(solutions) => {
            let vars: Vec<String> = solutions
                .variables()
                .iter()
                .map(|v| v.as_str().to_string())
                .collect();
            let mut bindings = Vec::new();
            for sol in solutions {
                let sol = sol.map_err(|e| W3cQueryError::Eval(e.to_string()))?;
                let mut row = serde_json::Map::new();
                for var in &vars {
                    if let Some(term) = sol.get(var.as_str()) {
                        row.insert(var.clone(), term_to_json(term));
                    }
                }
                bindings.push(serde_json::Value::Object(row));
            }
            Ok(W3cQueryOutcome::Solutions(serde_json::json!({
                "head": { "vars": vars },
                "results": { "bindings": bindings },
            })))
        }
        oxigraph::sparql::QueryResults::Boolean(b) => Ok(W3cQueryOutcome::Boolean(
            serde_json::json!({ "head": {}, "boolean": b }),
        )),
        oxigraph::sparql::QueryResults::Graph(triples) => {
            let mut out = String::new();
            for t in triples {
                let t = t.map_err(|e| W3cQueryError::Eval(e.to_string()))?;
                out.push_str(&t.to_string());
                out.push_str(" .\n");
            }
            Ok(W3cQueryOutcome::Graph(out))
        }
    }
}

/// The `optional_kits:` list from a repo.yml (serve + kit commands).
pub fn read_repo_yml_optional_kits(path: &std::path::Path) -> Vec<String> {
    RepoYml::load_path(path).optional_kits
}


#[cfg(test)]
mod w3c_query_tests {
    use super::*;
    use oxigraph::model::{GraphName, Literal, NamedNode, Quad};

    fn store_with_one_fact() -> Store {
        let store = Store::new().unwrap();
        store.insert(Quad::new(
            NamedNode::new("https://repolex.ai/soul/Memory/x.md").unwrap(),
            NamedNode::new("https://repolex.ai/ontology/git-lex/fm/title").unwrap(),
            Literal::new_simple_literal("hello"),
            GraphName::NamedNode(NamedNode::new("https://repolex.ai/git-lex/NamedGraph/now").unwrap()),
        ).as_ref()).unwrap();
        store
    }

    #[test]
    fn select_produces_w3c_bindings() {
        let store = store_with_one_fact();
        match w3c_query(&store, "SELECT ?s ?o WHERE { GRAPH <https://repolex.ai/git-lex/NamedGraph/now> { ?s ?p ?o } }").unwrap() {
            W3cQueryOutcome::Solutions(v) => {
                assert_eq!(v["head"]["vars"], serde_json::json!(["s", "o"]));
                let b = &v["results"]["bindings"][0];
                assert_eq!(b["s"]["type"], "uri");
                assert_eq!(b["o"]["type"], "literal");
                assert_eq!(b["o"]["value"], "hello");
            }
            _ => panic!("expected solutions"),
        }
    }

    #[test]
    fn bare_pattern_reads_only_the_default_graph() {
        // Standard SPARQL dataset semantics (union switch removed, Day-50
        // respec): a bare pattern does NOT see named-graph facts — the
        // purpose-built default graph is what bare queries read.
        let store = store_with_one_fact();
        match w3c_query(&store, "ASK { ?s ?p \"hello\" }").unwrap() {
            W3cQueryOutcome::Boolean(v) => assert_eq!(v["boolean"], false),
            _ => panic!("expected boolean"),
        }
        // The same fact IS visible when the graph is named.
        match w3c_query(&store, "ASK { GRAPH <https://repolex.ai/git-lex/NamedGraph/now> { ?s ?p \"hello\" } }").unwrap() {
            W3cQueryOutcome::Boolean(v) => assert_eq!(v["boolean"], true),
            _ => panic!("expected boolean"),
        }
    }

    #[test]
    fn malformed_query_is_parse_error() {
        let store = Store::new().unwrap();
        match w3c_query(&store, "NOT SPARQL AT ALL") {
            Err(W3cQueryError::Parse(_)) => {}
            _ => panic!("expected Parse error (the 400 class)"),
        }
    }
}

#[cfg(test)]
mod kit_prefix_tests {
    use super::*;

    #[test]
    fn name_match_wins_over_declaration_order() {
        // copia.ttl declares git-lex: and soul: BEFORE its own copia: —
        // the kit's own prefix must win by NAME, not by position.
        let ttl = "@prefix git-lex: <https://repolex.ai/ontology/git-lex/> .\n\
                   @prefix soul:  <https://repolex.ai/ontology/kit/soul/> .\n\
                   @prefix copia: <https://repolex.ai/ontology/kit/copia/> .\n";
        assert_eq!(
            extract_kit_prefix(ttl, "copia"),
            Some(("copia".into(), "https://repolex.ai/ontology/kit/copia/".into()))
        );
    }

    #[test]
    fn migrated_namespace_is_followed() {
        // THE flip test: when a kit's TTL moves off the kit/ tier, every
        // consumer derives the new namespace with zero code changes.
        let ttl = "@prefix soul: <https://repolex.ai/ontology/soul/> .\n";
        assert_eq!(
            extract_kit_prefix(ttl, "soul"),
            Some(("soul".into(), "https://repolex.ai/ontology/soul/".into()))
        );
    }

    #[test]
    fn no_declaration_yields_none_boilerplate_ignored() {
        let ttl = "@prefix sh:  <http://www.w3.org/ns/shacl#> .\n\
                   @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
                   @prefix git-lex: <https://repolex.ai/ontology/git-lex/> .\n";
        assert_eq!(extract_kit_prefix(ttl, "soul"), None);
    }

    #[test]
    fn differing_prefix_name_falls_back_to_first_non_boilerplate() {
        let ttl = "@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
                   @prefix myk: <https://example.org/vocab/> .\n";
        assert_eq!(
            extract_kit_prefix(ttl, "mykit"),
            Some(("myk".into(), "https://example.org/vocab/".into()))
        );
    }
}

#[cfg(test)]
mod kit_prefix_syntax_tests {
    use super::*;

    #[test]
    fn sparql_style_and_tab_separators_parse() {
        // All legal spellings of the same declaration (adversarial attack 3:
        // these used to silently fall through to the conventional fallback).
        for ttl in [
            "PREFIX soul: <https://repolex.ai/ontology/soul/> .\n",
            "prefix soul: <https://repolex.ai/ontology/soul/> .\n",
            "@prefix\tsoul: <https://repolex.ai/ontology/soul/> .\n",
            "@prefix  soul:  <https://repolex.ai/ontology/soul/> .\n",
        ] {
            assert_eq!(
                extract_kit_prefix(ttl, "soul"),
                Some(("soul".into(), "https://repolex.ai/ontology/soul/".into())),
                "failed on: {ttl:?}"
            );
        }
    }

    #[test]
    fn keyword_needs_a_separator() {
        assert_eq!(extract_kit_prefix("prefixsoul: <https://x.example/> .\n", "soul"), None);
    }
}
