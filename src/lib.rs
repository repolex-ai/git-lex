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

/// Path to the oxigraph store directory: `{repo_root}/.lex/_ignore/oxigraph/`.
/// The store is derived data (rebuildable from .spo sidecars) and never
/// version-controlled; under the pocket law (Rob, 2026-08-05 — in any tool's
/// dotdir, `_ignore/` is machine-local, everything else is committed) it
/// lives in the worktree pocket, same shape as `.ravel/_ignore/` and
/// `.pan/_ignore/`.
pub fn store_path() -> Option<PathBuf> {
    find_git_root().map(|r| store_path_at(&r))
}

/// Open the persistent store in read-only mode. Does not acquire the
/// RocksDB write lock, so the writer (`git lex sync`) can run
/// concurrently. The view is a snapshot from open-time and will not reflect
/// later writes until the store is reopened.
pub fn open_store_read_only() -> Option<Store> {
    open_store_read_only_at(&find_git_root()?)
}

/// Store dir for an EXPLICIT repo root (multi-repo servers) — the single
/// authority for the `.lex/_ignore/oxigraph` layout.
pub fn store_path_at(root: &std::path::Path) -> PathBuf {
    root.join(".lex").join("_ignore").join("oxigraph")
}

/// Pre-pocket store dir (`.git/lex/oxigraph`). TRANSITIONAL: exists only as
/// the migration source and the read-side fallback for repos that haven't
/// run a write command since the pocket law landed — dies in ship-prep once
/// the fleet is on the pocket layout.
pub fn legacy_store_path_at(root: &std::path::Path) -> PathBuf {
    root.join(".git").join("lex").join("oxigraph")
}

/// [`open_store_read_only`] for an explicit repo root.
pub fn open_store_read_only_at(root: &std::path::Path) -> Option<Store> {
    // Exists-but-unopenable is NOT "no store" (review #53): consumers map
    // None to "run `git lex sync` first", which is wrong advice for a
    // corrupt/locked store — so the open error is named before the two
    // cases collapse into one return type.
    let open_loud = |path: &std::path::Path| -> Option<Store> {
        match Store::open_read_only(path) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!(
                    "warning: a store EXISTS at {} but failed to open: {e}\n\
                     This is not a missing store — `git lex sync` will not fix an \
                     open error. If the store is corrupt, delete the directory and \
                     re-run `git lex sync` to rebuild it.",
                    path.display()
                );
                None
            }
        }
    };
    let path = store_path_at(root);
    if path.exists() {
        return open_loud(&path);
    }
    // Both-shapes read window: a repo whose store predates the pocket law
    // still has it at the legacy path until its next sync/kit-update
    // migrates it. Read it where it is. TRANSITIONAL — dies with
    // `legacy_store_path_at`.
    let legacy = legacy_store_path_at(root);
    if legacy.exists() {
        return open_loud(&legacy);
    }
    None
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

/// [`eval_query`] with the default graph set to the UNION of all named
/// graphs — the exploration semantics `git lex query` and the viz use
/// deliberately (those surfaces browse the whole store; the W3C protocol
/// endpoint keeps standard dataset semantics). ONE parse/execute for both
/// union surfaces (review #8); the error keeps its parse-vs-eval identity
/// so callers can report a 400-class and 500-class failure differently.
pub fn eval_query_union<'a>(
    store: &'a Store,
    q: &str,
) -> Result<oxigraph::sparql::QueryResults<'a>, W3cQueryError> {
    let mut parsed = oxigraph::sparql::SparqlEvaluator::new()
        .parse_query(q)
        .map_err(|e| W3cQueryError::Parse(e.to_string()))?;
    parsed.dataset_mut().set_default_graph_as_union();
    parsed
        .on_store(store)
        .execute()
        .map_err(|e| W3cQueryError::Eval(e.to_string()))
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
    /// LEGACY key (pre-2026-08-01): the genesis sha under its old name.
    /// Read for self-migration only — `genesis_sha` is the canonical key
    /// (Rob-ruled 2026-08-01, matching identity.yml's key and the git2
    /// ontology's genesisSha property). Sync rewrites the line in place.
    #[serde(default)]
    pub first_commit: Option<String>,
    /// The repo's genesis (first-commit) sha — its stable identity. Written
    /// by init; self-migrated from `first_commit` at sync. Replaces
    /// `.lex/identity.yml` as the authority once Pool's boot-skip read
    /// cuts over (coordinated 3-step; identity.yml still written until
    /// then).
    #[serde(default)]
    pub genesis_sha: Option<String>,
    /// DEV-ONLY stopgap: a date; history walking starts at the first
    /// commit after it. Exists for the ~10 pre-v1 squad repos whose early
    /// development churn predates the data rules. Normal repos never set
    /// this.
    #[serde(default)]
    pub dev_history_horizon: Option<String>,
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
        put("genesis_sha", &self.genesis_sha);
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

// ─── Frontmatter framing (ONE authority) ───────────────────────

/// Split a document into `(frontmatter, body)` — THE frontmatter-delimiter
/// parser. Five hand-rolled scanners with divergent CRLF and closing-fence
/// rules used to live across the crate (review #9 — the same disease the
/// repo.yml SEVEN-scanner consolidation cured): a CRLF file's frontmatter
/// parsed in extraction but read as body in the viz `/api/file` endpoint
/// and in harness skill/subagent sync, silently dropping its keys.
///
/// Rules, written down once:
/// - The opener is `---` as the FIRST line (`---\n` or `---\r\n`).
/// - The closer is the first subsequent line that is exactly `---`
///   (trailing whitespace/CR tolerated) — the Jekyll/Obsidian fence rule.
/// - Returns `(Some(yaml), body)` with `yaml` keeping its trailing
///   newline and `body` starting after the closer line.
/// - No opener, or an opener with no closer (a doc that TRIED to fence and
///   failed), returns `(None, content)` — callers that must be loud about
///   the malformed case check the opener themselves first.
pub fn split_frontmatter(content: &str) -> (Option<&str>, &str) {
    let after = if let Some(r) = content.strip_prefix("---\n") {
        r
    } else if let Some(r) = content.strip_prefix("---\r\n") {
        r
    } else {
        return (None, content);
    };
    let mut offset = 0usize;
    for line in after.split_inclusive('\n') {
        if line.trim_end() == "---" {
            let fm = &after[..offset];
            let body = &after[offset + line.len()..];
            return (Some(fm), body);
        }
        offset += line.len();
    }
    (None, content)
}

/// Parse a frontmatter YAML block into an ordered mapping — THE frontmatter
/// YAML parser, and the gate that stops a repeated key from eating data.
///
/// `serde_yaml` behaves differently depending on the target type, and the
/// difference is the whole bug (#101). Deserializing into `HashMap` — what
/// every call site used to do — accepts a repeated key and keeps only the
/// LAST value, silently. `lUX/Copia/Outfit/the-cold-errand.md` wrote eight
/// items that way and kept one; 28 documents and 60 facts went the same way
/// across the fleet, with exit 0 and a clean save every time. Deserializing
/// into `Mapping` makes serde_yaml itself reject the duplicate, at any
/// nesting depth. So the fix is the target type, and this function is where
/// it is written down once.
///
/// The error text is the other half of the job (Rob, 2026-08-11: the
/// teaching matters more than the rejection). Repeating a key is a
/// reasonable-looking thing to write and nothing anywhere shows the author
/// that YAML already has a way to say "more than one" — so the message shows
/// the list form spelled out with the author's OWN key, not a generic
/// complaint.
pub fn parse_frontmatter_map(yaml_str: &str) -> Result<serde_yaml::Mapping, String> {
    match serde_yaml::from_str::<serde_yaml::Mapping>(yaml_str) {
        Ok(map) => Ok(map),
        Err(e) => {
            let repeated = repeated_top_level_keys(yaml_str);
            if repeated.is_empty() {
                // Either genuinely malformed YAML, or a duplicate nested
                // deeper than the scan below looks. serde's own message
                // carries the line and column either way.
                return Err(format!("malformed YAML frontmatter: {}", e));
            }
            let example = &repeated[0];
            let what = if repeated.len() == 1 {
                format!("the key `{}`", example)
            } else {
                format!("{} keys: {}", repeated.len(), repeated.join(", "))
            };
            // Assembled line by line rather than as one continued literal:
            // a `\` line-continuation eats the following line's leading
            // whitespace, which is exactly the indentation the worked
            // example needs to be copy-pasteable.
            let lines = [
                format!("frontmatter repeats {} — a repeated key does NOT add a second value.", what),
                "YAML keeps only the last one, so every earlier value is thrown away.".to_string(),
                String::new(),
                "To give a key more than one value, write a list:".to_string(),
                String::new(),
                format!("    {}:", example),
                "      - \"first value\"".to_string(),
                "      - \"second value\"".to_string(),
                String::new(),
                "Rewrite the repeated key(s) as lists and save again.".to_string(),
                format!("(YAML parser: {})", e),
            ];
            Err(lines.join("\n"))
        }
    }
}

/// Collect top-level keys that appear more than once in a frontmatter block,
/// in first-seen order.
///
/// PRESENTATION ONLY. serde_yaml is the authority on whether a duplicate
/// exists — this scan runs afterwards, purely so the error can name every
/// repeated key at once instead of making the author fix one, re-save, and
/// meet the next. git-lex frontmatter is flat `kit.Class.property` keys, so a
/// zero-indent scan sees the real cases exactly; if it ever misses one the
/// caller still reports serde's message, so a miss costs detail, never the
/// rejection itself.
fn repeated_top_level_keys(yaml_str: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut repeated: Vec<String> = Vec::new();
    for line in yaml_str.lines() {
        // Zero-indent, non-comment, non-list lines only: anything indented
        // belongs to a nested mapping or a block scalar, where a colon is
        // just as likely to be prose as a key.
        if line.starts_with([' ', '\t', '#', '-']) || line.trim().is_empty() {
            continue;
        }
        let Some((key, _)) = line.split_once(':') else { continue };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        if seen.iter().any(|k| k == key) {
            if !repeated.iter().any(|k| k == key) {
                repeated.push(key.to_string());
            }
        } else {
            seen.push(key.to_string());
        }
    }
    repeated
}

/// The list-form teaching, worked out against a real class, for the
/// `__ClassName.md` reference template.
///
/// The rejection in [`parse_frontmatter_map`] catches the mistake; this is
/// the half that stops it being made (Rob, 2026-08-11: "more important than
/// the rejection is the clear instructions about the right way to do
/// multivalues"). Nothing in git-lex showed an author that YAML already has
/// a way to say "more than one", and repeating a key looks entirely
/// reasonable — so the syntax has to be visible on the surface people copy
/// from, the same argument #100 made about type words.
///
/// The property name is deliberately a placeholder. Which fields accept more
/// than one value is a cardinality question the kit ontologies mostly do not
/// declare, and naming a real field here would teach a claim git-lex cannot
/// stand behind — writing `someProperty` teaches the SYNTAX, which is what
/// the author is missing.
pub fn multivalue_teaching_block(short: &str, class_name: &str) -> String {
    format!(
        "# More than one value for a key? Write a YAML list. Repeating a key does NOT\n\
         # add a second value — YAML keeps only the last one and the rest are lost:\n\
         #\n\
         #     {}.{}.someProperty:\n\
         #       - \"first value\"\n\
         #       - \"second value\"\n",
        short, class_name,
    )
}

/// The same teaching, one line, for the frontmatter `git lex create` writes.
///
/// Short by design: the template is a reference artifact that can afford a
/// worked example, but this text lands in a real document and stays there,
/// so it carries the rule and the shape and nothing else.
pub fn multivalue_teaching_line() -> &'static str {
    "# Multiple values for one key? Use a YAML list (`- value` per line) — a repeated key keeps only the last.\n"
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
    // A failed registry write warns (review #55): a missing entry means
    // multi-repo serve silently doesn't see this repo.
    if let Err(e) = writeln!(file, "{}", canonical) {
        eprintln!(
            "warning: could not register {} in ~/.lex/repos ({e}) — \
             multi-repo serve will not see this repo until it is added",
            canonical
        );
    }
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
    // A failed rewrite warns (review #55): a stale entry means multi-repo
    // serve keeps trying a repo that no longer wants to be served.
    if let Err(e) = fs::write(&reg, filtered.join("\n") + "\n") {
        eprintln!(
            "warning: could not update ~/.lex/repos ({e}) — the stale entry for \
             {} remains; edit the file by hand to drop it",
            canonical
        );
    }
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
// Shared machinery for every SPARQL surface: term serialization
// (term_to_json), the W3C SELECT envelope (solutions_to_w3c_json), and
// parse/execute (eval_query / eval_query_union). The SURFACES deliberately
// differ in dataset semantics — `git lex query` and the viz set the default
// graph to the union of all graphs (exploration), while the protocol
// endpoint (`git lex serve sparql`) keeps standard W3C semantics; the viz
// also emits its own simplified payload for the UI. But they all assemble
// through these one-per-job functions, so the shapes can't drift (two
// SPARQL paths drifting was the bug class this section kills — review #8
// caught the old header claiming more sharing than existed).

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
        oxigraph::sparql::QueryResults::Solutions(solutions) => Ok(W3cQueryOutcome::Solutions(
            solutions_to_w3c_json(solutions).map_err(W3cQueryError::Eval)?,
        )),
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

/// SELECT solutions → the W3C SPARQL 1.1 Query Results JSON envelope
/// (`{"head":{"vars":[…]},"results":{"bindings":[…]}}`). ONE assembler for
/// the protocol endpoint and the CLI's `--json` output, so the envelope
/// shape can't drift between them (review #8; per-term serialization is
/// already shared via `term_to_json`). A per-solution error aborts with
/// `Err` — a partial result must never present as a complete one.
pub fn solutions_to_w3c_json(
    solutions: oxigraph::sparql::QuerySolutionIter<'_>,
) -> Result<serde_json::Value, String> {
    let vars: Vec<String> = solutions
        .variables()
        .iter()
        .map(|v| v.as_str().to_string())
        .collect();
    let mut bindings = Vec::new();
    for sol in solutions {
        let sol = sol.map_err(|e| e.to_string())?;
        let mut row = serde_json::Map::new();
        for var in &vars {
            if let Some(term) = sol.get(var.as_str()) {
                row.insert(var.clone(), term_to_json(term));
            }
        }
        bindings.push(serde_json::Value::Object(row));
    }
    Ok(serde_json::json!({
        "head": { "vars": vars },
        "results": { "bindings": bindings },
    }))
}

/// The `optional_kits:` list from a repo.yml (serve + kit commands).
pub fn read_repo_yml_optional_kits(path: &std::path::Path) -> Vec<String> {
    RepoYml::load_path(path).optional_kits
}


#[cfg(test)]
mod split_frontmatter_tests {
    use super::split_frontmatter;

    /// PIN (review #9): ONE fence rule for the whole crate. LF and CRLF
    /// openers both parse; the closer is a line that is exactly `---`;
    /// no opener or an unterminated fence means no frontmatter.
    #[test]
    fn one_fence_rule_for_every_consumer() {
        // LF
        let (fm, body) = split_frontmatter("---\nkey: v\n---\nbody\n");
        assert_eq!(fm, Some("key: v\n"));
        assert_eq!(body, "body\n");
        // CRLF opener — the case three of the five old scanners rejected.
        let (fm, body) = split_frontmatter("---\r\nkey: v\r\n---\r\nbody");
        assert_eq!(fm, Some("key: v\r\n"));
        assert_eq!(body, "body");
        // Closer at EOF without trailing newline.
        let (fm, body) = split_frontmatter("---\nkey: v\n---");
        assert_eq!(fm, Some("key: v\n"));
        assert_eq!(body, "");
        // No opener.
        assert_eq!(split_frontmatter("plain text"), (None, "plain text"));
        // Opener but no closer: a doc that tried to fence and failed has
        // NO frontmatter — callers that must be loud check the opener.
        assert_eq!(
            split_frontmatter("---\nkey: v\nbody"),
            (None, "---\nkey: v\nbody")
        );
        // A `----` divider is NOT a closer (exact-line fence rule).
        assert_eq!(
            split_frontmatter("---\nkey: v\n----\nx").0,
            None
        );
    }
}

#[cfg(test)]
mod frontmatter_duplicate_key_tests {
    use super::*;

    #[test]
    fn repeated_key_is_rejected_and_names_the_key() {
        let yaml = "copia.Outfit.outfitId: \"abyssal-drift\"\n\
                    copia.Outfit.includesItemId: \"abyssal-veil\"\n\
                    copia.Outfit.includesItemId: \"lumen-strand\"\n";
        let err = parse_frontmatter_map(yaml).unwrap_err();
        assert!(err.contains("copia.Outfit.includesItemId"), "{}", err);
        // The whole point of the message: it shows the fix, spelled out
        // with the author's own key.
        assert!(err.contains("copia.Outfit.includesItemId:\n      - \"first value\""), "{}", err);
    }

    #[test]
    fn every_repeated_key_is_listed_not_just_the_first() {
        // serde_yaml stops at the first duplicate; the reporting scan exists
        // so an author fixes all of them in one pass instead of meeting the
        // next one on every re-save.
        let yaml = "a: 1\nb: 2\na: 3\nb: 4\nc: 5\n";
        let err = parse_frontmatter_map(yaml).unwrap_err();
        assert!(err.contains("2 keys: a, b"), "{}", err);
        assert!(!err.contains("a, b, c"), "unrepeated key should not be named: {}", err);
    }

    #[test]
    fn the_list_form_is_accepted() {
        // The form the error message teaches has to actually work.
        let yaml = "copia.Outfit.includesItemId:\n  - \"abyssal-veil\"\n  - \"lumen-strand\"\n";
        let map = parse_frontmatter_map(yaml).unwrap();
        let v = map.get(serde_yaml::Value::String("copia.Outfit.includesItemId".into())).unwrap();
        assert_eq!(v.as_sequence().unwrap().len(), 2);
    }

    #[test]
    fn nested_duplicate_is_rejected_too() {
        // serde_yaml rejects at every depth; the top-level scan finds
        // nothing here, so the message falls back to serde's own — which
        // still carries the key, line and column.
        let yaml = "outer:\n  inner: 1\n  inner: 2\n";
        let err = parse_frontmatter_map(yaml).unwrap_err();
        assert!(err.contains("duplicate entry"), "{}", err);
    }

    #[test]
    fn clean_frontmatter_parses_in_authored_order() {
        // Mapping (unlike the HashMap this replaced) keeps declaration
        // order, so anything downstream that iterates is deterministic.
        let yaml = "soul.Journal.journalId: \"day-58\"\nsoul.Journal.soulDay: 58\n";
        let map = parse_frontmatter_map(yaml).unwrap();
        let keys: Vec<&str> = map.keys().filter_map(|k| k.as_str()).collect();
        assert_eq!(keys, vec!["soul.Journal.journalId", "soul.Journal.soulDay"]);
    }

    #[test]
    fn malformed_yaml_still_reads_as_malformed() {
        let err = parse_frontmatter_map("key: [unclosed\n").unwrap_err();
        assert!(err.contains("malformed YAML frontmatter"), "{}", err);
    }

    #[test]
    fn the_reporting_scan_ignores_nested_comment_and_list_lines() {
        // A colon inside a block scalar or a list item is prose, not a key —
        // counting those would name keys that were never repeated.
        let yaml = "note: |\n  first: thing\n  first: thing\n# first: thing\nitems:\n  - first: thing\n  - first: thing\n";
        assert!(repeated_top_level_keys(yaml).is_empty());
    }
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

#[cfg(test)]
mod store_layout_tests {
    use super::*;

    // ---- open_store_read_only_at: the both-shapes read window ----

    fn tmp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gitlex-store-layout-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_only_open_falls_back_to_legacy_layout() {
        let root = tmp_root("fallback");
        // A repo that hasn't run a write command since the pocket law: store
        // still at .git/lex/oxigraph. Read-only paths (serve) must find it.
        let legacy = legacy_store_path_at(&root);
        fs::create_dir_all(&legacy).unwrap();
        drop(Store::open(&legacy).unwrap()); // real store, then release the lock
        assert!(
            open_store_read_only_at(&root).is_some(),
            "read-only open must fall back to the legacy layout"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn read_only_open_prefers_the_pocket() {
        let root = tmp_root("prefers-pocket");
        // Pocket present → it wins, legacy never consulted.
        let pocket = store_path_at(&root);
        fs::create_dir_all(&pocket).unwrap();
        drop(Store::open(&pocket).unwrap());
        assert!(open_store_read_only_at(&root).is_some());
        assert!(!legacy_store_path_at(&root).exists());
        fs::remove_dir_all(&root).ok();
    }
}
