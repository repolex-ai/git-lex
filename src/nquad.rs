//! N-Quad / N-Triple encoding and generation.
//!
//! Low-level escapers (`nq_escape`, `uri_encode_path`) plus the N-Quad
//! generators that produce git-lex's "now" view of the world:
//!
//! - `generate_frontmatter_nquads` — the "now" graph: current-state frontmatter
//!   extraction, body wikilinks.
//! - `load_lex_nquads` — slurp any `.lex/**/*.nq` files the user wrote by hand.
//!
//! The git machinery layer lives in `git2_nquads` (library reads, `git2:`
//! vocab); the old shell-out `generate_git_nquads` (`git:` vocab, plus the
//! write-only changeset/blame/language layers nothing consumed) was removed
//! at the git2 cutover.
//!
//! Peeled out of `main.rs` during modularization.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use git_lex::find_git_root;

use crate::git::graph_uri;

/// The one wikilink pattern, shared by every extraction site. `[^\]\n]+`:
/// a link target NEVER spans lines. A `[[...]]` token broken across two
/// physical lines (e.g. a wrapped terminal paste inside a transcript doc)
/// is prose, not a link — capturing it used to put a raw newline inside a
/// sidecar value, splitting one triple across two physical lines and
/// aborting sync (Selkie's day-10 transcript, 2026-07-30).
pub(crate) const WIKILINK_PATTERN: &str = r"\[\[([^\]\n]+)\]\]";
use crate::extraction::{flatten_yaml, normalize_wikilink_path};
use crate::ontology::{get_kit_namespaces_all_kits, get_object_properties_all_kits,
                       get_property_datatypes_all_kits};
use crate::resolve;

/// Write a sidecar (or its `.meta`) file, failing the process on error.
/// A silently unwritten sidecar is a permanent history gap: the committed
/// sidecar diff is the one graph's ONLY event source, so "couldn't write,
/// carried on" means facts that never happened as far as history is
/// concerned (review finding A8).
pub(crate) fn write_sidecar_loud(path: &std::path::Path, content: &str) {
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!("fatal: failed to create sidecar dir {}: {e}", parent.display());
            std::process::exit(1);
        }
    }
    if let Err(e) = fs::write(path, content) {
        eprintln!("fatal: failed to write sidecar {}: {e}", path.display());
        std::process::exit(1);
    }
}

/// Remove a stale sidecar, failing the process on error (already-gone is
/// fine — that's the desired end state). A stale sidecar that survives
/// removal keeps its facts alive forever: the sync diff never sees the
/// lines vanish, so the retraction events never exist (review finding A3).
pub(crate) fn remove_sidecar_loud(path: &std::path::Path) {
    if let Err(e) = fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            eprintln!("fatal: failed to remove stale sidecar {}: {e}", path.display());
            std::process::exit(1);
        }
    }
}

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
            '%' => out.push_str("%25"),
            '"' => out.push_str("%22"),
            '\\' => out.push_str("%5C"),
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

/// THE repo document walker. Every consumer walks the same file set —
/// `.md` and `.txt`, dot-entries skipped — through this one function.
/// Call sites that need a narrower set filter EXPLICITLY (see
/// `is_template`); the four hand-rolled walkers this replaces had drifted
/// into three different file policies.
pub(crate) fn walk_repo_docs(root: &std::path::Path) -> Vec<PathBuf> {
    fn walk(dir: &std::path::Path, files: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') { continue; }
                if path.is_dir() {
                    walk(&path, files);
                } else if name.ends_with(".md") || name.ends_with(".txt") {
                    files.push(path);
                }
            }
        }
    }
    let mut files = Vec::new();
    walk(root, &mut files);
    files
}

/// Is this a kit-derived `__Class.md` scaffold template?
pub(crate) fn is_template(path: &std::path::Path) -> bool {
    path.file_name()
        .map(|n| n.to_string_lossy().starts_with("__"))
        .unwrap_or(false)
}

/// Everything the shared emitter resolves against, built ONCE per run:
/// the slug/path indexes over the repo's documents and the all-kits
/// ontology tables. Sync used to build the indexes twice per run (once
/// inside frontmatter generation, again for the history walk).
pub(crate) struct ResolverContext {
    pub files: Vec<PathBuf>,
    pub path_index: HashSet<String>,
    pub obj_props: HashSet<String>,
    pub prop_datatypes: HashMap<String, String>,
    /// Every "{kit}/{Class}/{prop}" any installed kit declares — datatype-
    /// unconditional. The undeclared-key warning consults THIS set;
    /// `prop_datatypes` (typed-literal emission only) misses every
    /// xsd:string property by design and must never gate the warning.
    pub declared_props: HashSet<String>,
    pub kit_namespaces: HashMap<String, String>,
    /// Repo-level wikilink semantics (repo.yml `link_semantics`): true =
    /// Obsidian (bare targets root-relative, `/` rejected), false = legacy
    /// 2026-07-28 markdown semantics. Applies uniformly to the now view AND
    /// the whole history walk — correct because only repos BORN under
    /// Obsidian semantics (or fully migrated in Phase 4) carry the stamp.
    pub obsidian_links: bool,
    /// "{kit}/{Class}" → declared id-property name (Law 3 declarations
    /// from installed kit TTLs). Empty = pre-declaration state; the
    /// transitional `<class>Id` convention applies (see
    /// `derive_file_subjects`).
    pub id_props: HashMap<String, String>,
}

impl ResolverContext {
    pub(crate) fn build(root: &std::path::Path) -> ResolverContext {
        let files = walk_repo_docs(root);
        let path_index = build_path_index(root, &files);
        ResolverContext {
            files,
            path_index,
            obj_props: get_object_properties_all_kits(),
            prop_datatypes: get_property_datatypes_all_kits(),
            declared_props: crate::ontology::get_declared_properties_all_kits(),
            kit_namespaces: get_kit_namespaces_all_kits(),
            obsidian_links: git_lex::RepoYml::load(root).obsidian_links(),
            id_props: crate::ontology::get_id_properties_all_kits(),
        }
    }
}

/// The per-file subject anchors of the two identity planes (identity model
/// Laws 1–5, Rob-ruled 2026-07-30). Derived ONCE per file from its full
/// sidecar line set, then every line emits against the right plane:
///
/// - `file_uri` — the File node `git-lex/File/<path>`: linksTo edges,
///   markdown/fm facts, git facts. Always present; a no-kit repo is a
///   File-only graph and that is the bare-markdown tier working.
/// - `thing_uri` — the Thing node `<kit-app>/<Class>/<id>`: kit-declared
///   facts + rdf:type. Present only when the file's class has a known id
///   property AND the sidecar carries its value. The Thing→File
///   connection is the derived `fileId` edge (Law 5).
///
/// All IRIs carry angle brackets (ready to print into N-Quads).
pub(crate) struct FileSubjects {
    pub file_uri: String,
    pub thing_uri: Option<String>,
    /// (kit short name, canonical class) of the anchoring Thing.
    pub thing_key: Option<(String, String)>,
}

/// A kit's a-box (application) base from its ontology namespace:
/// `https://repolex.ai/ontology/soul/` → `https://repolex.ai/soul/` — the
/// universal instance law (`<application>/<Class>/<id>`, Rob Day-50),
/// derived from the installed TTL's own declaration, never hardcoded.
fn app_base_from_kit_ns(kit_ns: &str) -> String {
    match kit_ns.find("/ontology/") {
        Some(i) => format!("{}/{}", &kit_ns[..i], &kit_ns[i + "/ontology/".len()..]),
        None => kit_ns.to_string(),
    }
}

/// Derive both plane anchors for one file from its full sidecar line set.
///
/// Thing-anchor derivation: the file's class is the FIRST kit-classed line's
/// class (resolved against the ontology, same rule as type emission); its id
/// property comes from the Law-3 declarations (`ctx.id_props`), or — ONLY
/// while no installed kit declares any id property — the transitional
/// `<class>Id` name convention, validated against the class's declared
/// properties so a phantom name can never match. A classed file whose id
/// value is absent anchors NOTHING (facts stay on the File node) and warns:
/// pre-migration corpora hit this constantly, and the warning is the
/// Phase-4 work list. (Flipping that warning to a save-time reject is the
/// post-migration step — see the identity model doc §2.3.)
pub(crate) fn derive_file_subjects(
    spo_lines: &[String],
    relpath_str: &str,
    ctx_id_props: &HashMap<String, String>,
    declared_props: &HashSet<String>,
    obj_props: &HashSet<String>,
    kit_namespaces: &HashMap<String, String>,
    warn: bool,
) -> FileSubjects {
    let file_uri = format!("<{}>", crate::git::file_iri(&uri_encode_path(relpath_str)));

    // First kit-classed line decides the file's class (existing single-class
    // rule; other classes' lines warn at emission as before).
    let mut anchor: Option<(String, String)> = None; // (kit, canonical class)
    for line in spo_lines {
        let parts: Vec<&str> = line.splitn(3, " | ").collect();
        if parts.len() != 3 || parts[1] != "hasValue" {
            continue;
        }
        let segments: Vec<&str> = parts[0].splitn(3, '.').collect();
        if segments.len() != 3 {
            continue;
        }
        if let Ok(canonical) = crate::ontology::resolve_class_segment(segments[0], segments[1]) {
            anchor = Some((segments[0].to_string(), canonical));
            break;
        }
    }
    let Some((kit, class)) = anchor else {
        return FileSubjects { file_uri, thing_uri: None, thing_key: None };
    };

    // Law-3 declaration, else the fenced transitional convention.
    let key = format!("{}/{}", kit, class);
    let id_prop: Option<String> = match ctx_id_props.get(&key) {
        Some(p) => Some(p.clone()),
        None if ctx_id_props.is_empty() => {
            let mut conv = class.clone();
            if let Some(first) = conv.get_mut(0..1) {
                let low = first.to_lowercase();
                conv.replace_range(0..1, &low);
            }
            conv.push_str("Id");
            let prop_key = format!("{}/{}/{}", kit, class, conv);
            (declared_props.contains(&prop_key) || obj_props.contains(&prop_key))
                .then_some(conv)
        }
        None => None, // declarations exist, this class has none — no anchor
    };
    let Some(id_prop) = id_prop else {
        return FileSubjects { file_uri, thing_uri: None, thing_key: None };
    };

    // Find the id value in this file's own lines.
    let id_line_key = format!("{}.{}.{}", kit, class, id_prop);
    let id_value = spo_lines.iter().find_map(|line| {
        let parts: Vec<&str> = line.splitn(3, " | ").collect();
        if parts.len() == 3
            && parts[1] == "hasValue"
            && parts[0] == id_line_key
            && !parts[2].trim().is_empty()
        {
            Some(parts[2].trim().to_string())
        } else {
            None
        }
    });
    let Some(id_value) = id_value else {
        if warn {
            eprintln!(
                "warning: {relpath_str}: classed as {kit}.{class} but carries no \
                 `{id_line_key}` — facts anchor to the File node until the id is authored \
                 (Phase-4 migration work list)"
            );
        }
        return FileSubjects { file_uri, thing_uri: None, thing_key: None };
    };

    let kit_ns = kit_namespaces
        .get(&kit)
        .cloned()
        .unwrap_or_else(|| git_lex::conventional_kit_namespace(&kit));
    let thing_uri = format!(
        "<{}{}/{}>",
        app_base_from_kit_ns(&kit_ns),
        class,
        uri_encode_path(&id_value)
    );
    FileSubjects {
        file_uri,
        thing_uri: Some(thing_uri),
        thing_key: Some((kit, class)),
    }
}

/// Emit the per-file anchor facts shared by BOTH graph paths:
/// the File node's rdf:type, and — when the file expresses a Thing — the
/// Thing's rdf:type plus the derived `fileId` edge (Law 5,
/// Thing → File). In the one-graph walk these participate in resolved-set
/// diffing, so a file move produces exactly the honest fileId
/// retract+assert pair and nothing else.
pub(crate) fn emit_file_anchor_nquads(
    subjects: &FileSubjects,
    kit_namespaces: &HashMap<String, String>,
    graph: &str,
    emitted_types: &mut HashSet<String>,
    out: &mut String,
) {
    out.push_str(&format!(
        "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/File> {} .\n",
        subjects.file_uri, graph
    ));
    let (Some(thing_uri), Some((kit, class))) = (&subjects.thing_uri, &subjects.thing_key) else {
        return;
    };
    let kit_ns = kit_namespaces
        .get(kit)
        .cloned()
        .unwrap_or_else(|| git_lex::conventional_kit_namespace(kit));
    if emitted_types.insert(format!("{}.{}", kit, class)) {
        out.push_str(&format!(
            "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{}{}> {} .\n",
            thing_uri, kit_ns, class, graph
        ));
    }
    out.push_str(&format!(
        "{} <https://repolex.ai/ontology/git-lex/fileId> {} {} .\n",
        thing_uri, subjects.file_uri, graph
    ));
}

/// Extract frontmatter and body wikilinks from all .md/.txt files in the
/// repo into the "now" graph. Also writes `.fm.spo` sidecars and scans
/// commit messages for wikilinks.
pub(crate) fn generate_frontmatter_nquads() -> (String, u32) {
    let root = match find_git_root() {
        Some(r) => r,
        None => return (String::new(), 0),
    };
    let ctx = ResolverContext::build(&root);
    generate_frontmatter_nquads_with(&root, &ctx)
}

/// [`generate_frontmatter_nquads`] against a caller-built context — sync
/// builds ONE `ResolverContext` and shares it with the history walk.
pub(crate) fn generate_frontmatter_nquads_with(
    root: &std::path::Path,
    ctx: &ResolverContext,
) -> (String, u32) {
    let root = root.to_path_buf();

    // The "now" graph is the canonical view of current state — extracted
    // frontmatter, body wikilinks/mentions, and any triples derived from the
    // working tree as it exists right now. Contrasts with sync/<sha>/ graphs,
    // which hold historical snapshots at past commits. Previously named
    // "frontmatter" but that was misleading: this graph holds more than the
    // fm: namespace (wikilinks, mentions, etc are also here).
    let graph = format!("<{}>", graph_uri("now"));
    let mut nq = String::new();
    let mut total_errors: u32 = 0;

    // Build ObjectProperty + datatype lookup across ALL installed kits (base
    // + domain + optionals). Frontmatter triples from any kit's properties
    // need correct IRI-vs-literal classification and typed-literal tags
    // (xsd:integer, xsd:date, etc.) regardless of which kit declared the
    // property. The previous single-kit lookup hid optional-kit datatypes —
    // e.g. copia:firstVisited (xsd:date) emitted as untyped string.

    // Open git repo for blob hash lookups
    let repo = git2::Repository::discover(".").ok();

    let files = ctx.files.as_slice();
    let path_index = &ctx.path_index;
    let (obj_props, prop_datatypes, kit_namespaces) =
        (&ctx.obj_props, &ctx.prop_datatypes, &ctx.kit_namespaces);

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
    let wikilink_re = regex::Regex::new(WIKILINK_PATTERN).unwrap();

    for filepath in files {
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

        // --- Frontmatter extraction ---
        let mut spo_lines = Vec::new();
        let body_text;

        if content.starts_with("---\n") || content.starts_with("---\r\n") {
            let rest = &content[4..];
            if let Some(end) = rest.find("\n---") {
                let yaml_str = &rest[..end];
                match serde_yaml::from_str::<HashMap<String, serde_yaml::Value>>(yaml_str) {
                    Ok(yaml) => {
                        for (key, value) in &yaml {
                            flatten_yaml(key, value, &mut spo_lines);
                        }
                    }
                    Err(e) => {
                        eprintln!("error: {}: malformed YAML frontmatter: {}", relpath_str, e);
                        total_errors += 1;
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
        // The target passes through VERBATIM: under the 2026-07-28 path law
        // a bare target is relative to the source file's folder and a
        // leading `/` anchors at the repo root, so the slash is SEMANTIC —
        // stripping it here (tried 2026-08-01) silently re-bound every
        // root-anchored link to doc-relative. Canonical-form unification is
        // a path-law decision (Rob's), not an extraction-time rewrite.
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

        // Write .spo sidecar; when a doc's extractable content goes away its
        // existing sidecar must go away too, so the sync diff sees the lines
        // vanish and records retractions (the one graph's only
        // signal — the now graph rebuilds from files and never notices).
        let spo_path = extract_dir.join(format!("{}.fm.spo", relpath_str));
        if !spo_lines.is_empty() {
            let spo_content = spo_lines.join("\n") + "\n";
            write_sidecar_loud(&spo_path, &spo_content);
        } else if spo_path.exists() {
            remove_sidecar_loud(&spo_path);
        }

        // --- Generate N-Quads for oxigraph (now graph) ---
        // Identity model (Rob-ruled 2026-07-30, re-anchor 2026-08-02): two
        // plane anchors per file. The File node `git-lex/File/<path>` (the
        // path IS the id — Law 4) carries the git layer, links, and fm
        // facts; when the file expresses a Thing with an authored id, the
        // Thing node `<kit-app>/<Class>/<id>` (Law 2) carries the
        // kit-declared facts, connected by the derived fileId edge (Law 5).
        // Classes come from the ontology and explicit dot-notation, never
        // folder-name guessing.
        //
        // File location is git:path — a git-lex-authored synthetic fact from
        // the on-disk path, NOT a user frontmatter key; fm: carries ONLY what
        // the user wrote (the fm firewall).
        let subjects = derive_file_subjects(
            &spo_lines,
            &relpath_str,
            &ctx.id_props,
            &ctx.declared_props,
            &obj_props,
            &kit_namespaces,
            true, // the now path is the save/sync moment — warn here
        );

        nq.push_str(&format!(
            "{} <https://repolex.ai/ontology/git-lex/git/path> \"{}\" {} .\n",
            subjects.file_uri, nq_escape(&relpath_str), graph
        ));
        nq.push_str(&format!(
            "{} <https://repolex.ai/ontology/git-lex/git/blobHash> \"{}\" {} .\n",
            subjects.file_uri, blob_hash, graph
        ));

        // Track which kit types we've seen for rdf:type emission (dedup)
        let mut emitted_types: HashSet<String> = HashSet::new();

        // File rdf:type + (when anchored) Thing rdf:type + fileId edge.
        emit_file_anchor_nquads(&subjects, &kit_namespaces, &graph, &mut emitted_types, &mut nq);

        for line in &spo_lines {
            total_errors += emit_spo_line_nquads(
                line,
                &subjects,
                &graph,
                &relpath_str,
                &path_index,
                &obj_props,
                &prop_datatypes,
                &ctx.declared_props,
                &kit_namespaces,
                ctx.obsidian_links,
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
                let commit_uri = format!("<{}>", crate::git2_nquads::git2_uri(&format!("Commit/{}", sha)));

                for cap in wikilink_re.captures_iter(message) {
                    let link = &cap[1];
                    nq.push_str(&format!(
                        "{} <https://repolex.ai/ontology/git-lex/md/linksTo> \"{}\" {} .\n",
                        commit_uri, nq_escape(link), graph
                    ));
                }
            }
        }
    }

    (nq, total_errors)
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
/// - `subjects`: the file's two plane anchors (see `derive_file_subjects`);
///   each line lands on the plane its nature dictates — kit-declared facts
///   on the Thing node, links/markdown/fm facts on the File node. Kit lines
///   with no Thing anchor (missing id, non-anchor class) fall back to the
///   File node so nothing is dropped; the drift is surfaced elsewhere.
/// - `graph`: target graph IRI (with angle brackets)
/// - `relpath_str`: source document path relative to repo root (for warnings)
/// - `path_index`: repo-relative paths of every walked doc (dangling-link
///   warnings only — resolution itself is pure path arithmetic)
/// - `obj_props` / `prop_datatypes`: ontology-derived property metadata
/// - `emitted_types`: in/out dedup set — the caller must zero this per doc
///   so each document emits its `rdf:type` assertions at most once
/// - `out`: the N-Quad buffer being appended to
pub(crate) fn emit_spo_line_nquads(
    line: &str,
    subjects: &FileSubjects,
    graph: &str,
    relpath_str: &str,
    path_index: &HashSet<String>,
    obj_props: &HashSet<String>,
    prop_datatypes: &HashMap<String, String>,
    declared_props: &HashSet<String>,
    kit_namespaces: &HashMap<String, String>,
    obsidian_links: bool,
    emitted_types: &mut HashSet<String>,
    out: &mut String,
) -> u32 {
    let mut errors: u32 = 0;
    let parts: Vec<&str> = line.splitn(3, " | ").collect();
    if parts.len() != 3 {
        return 0;
    }
    let subject = parts[0];
    let predicate = parts[1];
    let object = parts[2];

    // Hard-fail: empty values produce empty literal triples — skip entirely
    if object.trim().is_empty() {
        return 0;
    }

    // Hard-fail: [[wikilinks]] in frontmatter values corrupt the graph
    if predicate != "linksTo" && (object.contains("[[") || object.contains("]]")) {
        eprintln!(
            "error: {}: {} — wikilink syntax [[...]] is not allowed in frontmatter values. Write the repo-relative path (e.g. friend/selkie.md).",
            relpath_str, subject
        );
        return 1;
    }

    if predicate == "linksTo" {
        // [[wikilink]] → md:linksTo. Two semantics, dispatched on the repo's
        // link_semantics stamp (migration fence, see ResolverContext):
        //
        // LEGACY (2026-07-28 ruling, unstamped repos): a link target is
        // a PATH — relative to the source file's folder, or repo-rooted
        // with a leading `/` — resolved by pure path arithmetic.
        //
        // OBSIDIAN (2026-08-01 ruling, stamped repos): bare targets are
        // repo-ROOT-relative; a leading `/` is RETIRED and errors at save.
        //
        // Either way the IRI derives the same way at every commit, whether
        // or not the target file exists yet (forward links are legal;
        // dangling ones warn at save until the target appears). `.md` is
        // appended when the target has no extension.
        //
        // NOT-CHOSEN alternative, recorded for context: the old
        // three-strategy search (path-with-existence-check, then trailing-
        // segment fallback, then stem lookup across the whole
        // repo) — deleted because search-based resolution rebinds silently
        // as files come and go and makes history non-deterministic.
        if obsidian_links && object.starts_with('/') {
            eprintln!(
                "error: {relpath_str}: [[{object}]] — leading `/` is retired under \
                 Obsidian link semantics; write the repo-root-relative path \
                 (e.g. [[{}]])",
                object.trim_start_matches('/')
            );
            return errors + 1;
        }
        let source_dir = if obsidian_links {
            String::new() // bare = repo-root-relative
        } else {
            std::path::Path::new(relpath_str)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
        };

        match normalize_wikilink_path(object, &source_dir) {
            Some(p) => {
                if graph == format!("<{}>", crate::git::graph_uri("now"))
                    && !path_index.contains(&p)
                {
                    eprintln!(
                        "warning: {relpath_str}: [[{object}]] → {p} does not exist (yet) — forward link, or fix the path"
                    );
                }
                // Prose links follow documents (Law 6): File → File, both
                // ends in the File-plane family. A dangling target still
                // derives its IRI — the dangle is true data about the text.
                out.push_str(&format!(
                    "{} <https://repolex.ai/ontology/git-lex/md/linksTo> <{}> {} .\n",
                    subjects.file_uri, crate::git::file_iri(&uri_encode_path(&p)), graph
                ));
            }
            None => {
                eprintln!(
                    "error: {relpath_str}: [[{object}]] escapes the repo root — links stay inside the repo"
                );
                errors += 1;
            }
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
            //
            // B1 fix (Day 38): validate the class segment against the kit's
            // declared classes (the graph path is the one users query, so a
            // phantom type here is what makes `?m a soul:Memory` return 0).
            // `resolve_class_segment` returns the canonical class name on an
            // exact or case-only-mismatch hit (warning on the latter), and an
            // Err on a real typo. On the graph path we DON'T panic mid-sync —
            // we warn loudly and SKIP the type emission, so we never write the
            // phantom `a soul:memory`. The doc's properties still emit; only
            // the bad type is withheld until the frontmatter is fixed.
            let canonical_class: Option<String> =
                match crate::ontology::resolve_class_segment(kit_name, class_seg) {
                    Ok(canonical) => Some(canonical),
                    Err(msg) => {
                        eprintln!("warning: {msg} (skipping type emission for this doc)");
                        None
                    }
                };
            // The kit's namespace comes from its installed TTL declaration
            // (get_kit_namespaces_all_kits); the conventional pattern is only
            // the no-declaration fallback. This is what lets a kit's
            // namespace migrate with a TTL edit and no emitter change.
            let kit_ns = kit_namespaces
                .get(kit_name)
                .cloned()
                .unwrap_or_else(|| git_lex::conventional_kit_namespace(kit_name));

            // The line's subject: the Thing anchor when this line's class IS
            // the file's anchoring class; otherwise the File node (kit lines
            // with no Thing anchor — missing id, second class — fall back so
            // no fact is dropped; the type lands on the same subject, which
            // preserves today's queryability for the unmigrated corpus and
            // relocates Thing-ward per file as ids get authored).
            let line_subject: &str = match (&subjects.thing_uri, &subjects.thing_key, &canonical_class) {
                (Some(t), Some((ak, ac)), Some(c)) if ak == kit_name && ac == c => t,
                _ => &subjects.file_uri,
            };

            if let Some(canonical) = &canonical_class {
                let type_key = format!("{}.{}", kit_name, canonical);
                if emitted_types.insert(type_key) {
                    let type_uri = format!("<{}{}>", kit_ns, canonical);
                    out.push_str(&format!(
                        "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> {} {} .\n",
                        line_subject, type_uri, graph
                    ));
                }
            }

            // Property name passes through as-is (camelCase from ontology)
            let kit_predicate = format!("<{}{}>", kit_ns, prop_seg);

            // Kit+class-qualified lookup (Rob-ruled 2026-07-21): the tables
            // key "{kit}/{Class}/{prop}", so THIS kit's and class's own
            // declaration governs how the value is processed. The old
            // bare-name lookup let any installed kit's same-named property
            // rewrite the behavior (copia:source, a lineage ObjectProperty,
            // was comma-splitting soul:source prose citations).
            let lookup_key = canonical_class
                .as_ref()
                .map(|c| format!("{}/{}/{}", kit_name, c, prop_seg));

            // Check if this is an ObjectProperty (from ontology) → resolve as IRI
            if lookup_key.as_ref().is_some_and(|k| obj_props.contains(k)) {
                // ObjectProperty: split on commas, resolve each value
                // via the canonical resolver (see src/resolve.rs).
                let values: Vec<&str> = object.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
                for val in values {
                    if val.is_empty() { continue; }
                    match resolve::resolve_frontmatter_value(val) {
                        resolve::ResolveResult::Iri(uri) => {
                            out.push_str(&format!(
                                "{} {} {} {} .\n",
                                line_subject, kit_predicate, uri, graph
                            ));
                        }
                        resolve::ResolveResult::Unresolved(literal) => {
                            out.push_str(&format!(
                                "{} {} \"{}\" {} .\n",
                                line_subject, kit_predicate, nq_escape(&literal), graph
                            ));
                        }
                        resolve::ResolveResult::Rejected(msg) => {
                            eprintln!(
                                "error: {}: {} — {}",
                                relpath_str, prop_seg, msg
                            );
                            errors += 1;
                        }
                    }
                }
            } else {
                // Used-on-undeclared-class is LOUD, never silent: the property
                // is declared somewhere in THIS kit but not on this class's
                // shape — it still emits (as a plain literal), and the drift
                // is surfaced so the shape or the frontmatter gets fixed.
                if let Some(key) = &lookup_key {
                    // Membership test against the DECLARED set — datatype-
                    // unconditional. Testing prop_datatypes here false-warned
                    // every xsd:string property in every kit (the shapes
                    // generator omits sh:datatype for strings): 412 bogus
                    // "not declared" warnings per save in W4R3Z alone
                    // (found 2026-08-01).
                    if !declared_props.contains(key) && !obj_props.contains(key) {
                        let kit_scope = format!("{}/", kit_name);
                        let prop_tail = format!("/{}", prop_seg);
                        let declared_elsewhere_in_kit = obj_props
                            .iter()
                            .chain(declared_props.iter())
                            .any(|k| k.starts_with(&kit_scope) && k.ends_with(&prop_tail));
                        if declared_elsewhere_in_kit {
                            eprintln!(
                                "warning: {}: `{}.{}.{}` — property `{}` is not declared on class `{}` in kit `{}` (declared on another class); emitted as a plain literal until the shape or frontmatter is aligned",
                                relpath_str, kit_name, class_seg, prop_seg, prop_seg,
                                canonical_class.as_deref().unwrap_or(class_seg), kit_name
                            );
                        } else if !kit_namespaces.contains_key(kit_name)
                            || obj_props.iter().chain(prop_datatypes.keys())
                                .any(|k| k.starts_with(&kit_scope))
                        {
                            // The kit-qualified prefix CLAIMS ontology
                            // vocabulary; a property the ontology has never
                            // heard of used to sail through silently — how
                            // months of junk keys (writtenFrom, soul.Note.
                            // title, …) accumulated invisibly (Rob-ruled
                            // 2026-07-29: warn at save). Bare keys (title:)
                            // stay free — the open fm: lane is one line up.
                            eprintln!(
                                "warning: {}: `{}.{}.{}` is not declared in the `{}` ontology — emitted as plain data; fix the key (or drop the `{}.` prefix to use plain metadata)",
                                relpath_str, kit_name, class_seg, prop_seg, kit_name, kit_name
                            );
                        }
                    }
                }
                // DatatypeProperty: typed literal if ontology specifies a non-string range.
                if let Some(datatype) = lookup_key.as_ref().and_then(|k| prop_datatypes.get(k)) {
                    out.push_str(&format!(
                        "{} {} \"{}\"^^<{}> {} .\n",
                        line_subject, kit_predicate, nq_escape(object), datatype, graph
                    ));
                } else {
                    out.push_str(&format!(
                        "{} {} \"{}\" {} .\n",
                        line_subject, kit_predicate, nq_escape(object), graph
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
                // Pre-dot-notation-era keys (no current kit emits them;
                // they exist only in old souls' history). Same law as
                // everywhere else: paths derive IRIs, anything else stays
                // a literal — no index lookup, no guessing.
                for val in values {
                    if val.is_empty() { continue; }
                    if val.contains('/') || val.ends_with(".md") {
                        out.push_str(&format!(
                            "{} {} <{}> {} .\n",
                            subjects.file_uri, fm_predicate, crate::git::file_iri(&uri_encode_path(val)), graph
                        ));
                    } else {
                        out.push_str(&format!(
                            "{} {} \"{}\" {} .\n",
                            subjects.file_uri, fm_predicate, nq_escape(val), graph
                        ));
                    }
                }
            } else {
                out.push_str(&format!(
                    "{} {} \"{}\" {} .\n",
                    subjects.file_uri, fm_predicate, nq_escape(object), graph
                ));
            }
        }
    }
    errors
}

/// Build the slug→path and path indexes used for `[[wikilink]]` resolution.
///
/// Repo-relative paths of every walked document — used to warn on
/// dangling links. (The slug index that used to live here — lowercase
/// stem → path, collision-prone by construction — died with the
/// bare-name resolution rule, Rob-ruled 2026-07-28.)
pub(crate) fn build_path_index(
    root: &std::path::Path,
    files: &[PathBuf],
) -> HashSet<String> {
    let mut path_index: HashSet<String> = HashSet::new();
    for f in files {
        if let Ok(rel) = f.strip_prefix(root) {
            path_index.insert(rel.to_string_lossy().to_string());
        }
    }
    path_index
}



/// Load every installed kit ontology TTL (`.lex/ontology/**/*.ttl`, EXCLUDING
/// `-shapes.ttl` SHACL files — Rob: shapes are validation, not vocabulary)
/// into the self-describing ontology graph
/// `<https://repolex.ai/git-lex/NamedGraph/repo-ontology>` of `store`.
///
/// Runs at INIT and KIT-UPDATE only (Rob Day-50): the graph persists in the
/// store ("stays put") — sync does not touch it, query does not rebuild it.
/// The graph is cleared first so a kit-update fully refreshes the vocabulary.
/// LOUD but not fatal on a broken TTL. Returns the number of files loaded.
pub(crate) fn load_ontology_graph(store: &oxigraph::store::Store) -> usize {
    let Some(root) = find_git_root() else { return 0 };
    let ontology_graph = match oxigraph::model::NamedNode::new(graph_uri("repo-ontology")) {
        Ok(g) => g,
        Err(_) => return 0,
    };
    if let Err(e) = store.remove_named_graph(&ontology_graph) {
        eprintln!("warning: failed to clear the repo-ontology graph before reload: {} — retired vocabulary may linger", e);
    }
    fn walk_ttl(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let p = entry.path();
                if p.is_dir() {
                    walk_ttl(&p, out);
                } else if p.extension().is_some_and(|e| e == "ttl")
                    && !p.file_name().is_some_and(|n| n.to_string_lossy().ends_with("-shapes.ttl"))
                {
                    out.push(p);
                }
            }
        }
    }
    let mut ttls: Vec<PathBuf> = Vec::new();
    let ont_root = root.join(".lex").join("ontology");
    if ont_root.exists() {
        walk_ttl(&ont_root, &mut ttls);
    }
    ttls.sort();
    let mut loaded = 0usize;
    for ttl in &ttls {
        match fs::read(ttl) {
            Ok(bytes) => {
                let parser = oxigraph::io::RdfParser::from_format(oxigraph::io::RdfFormat::Turtle)
                    .with_default_graph(ontology_graph.clone());
                match store.load_from_reader(parser, std::io::Cursor::new(bytes)) {
                    Ok(_) => loaded += 1,
                    Err(e) => eprintln!("  warning: ontology load failed for {}: {}", ttl.display(), e),
                }
            }
            Err(e) => eprintln!("  warning: ontology read failed for {}: {}", ttl.display(), e),
        }
    }
    loaded
}

// ════════════════════════════════════════════════════════════════════════════
// Tests
// ════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    /// The emitter's predicate/class namespace comes from the kit's own TTL
    /// declaration (via the kit_namespaces map), NOT a hardcoded pattern —
    /// so a kit namespace migration is a TTL edit, no emitter change. A kit
    /// with no installed declaration falls back to the conventional pattern.
    #[test]
    fn emitter_follows_declared_kit_namespace() {
        let empty_paths: HashSet<String> = HashSet::new();
        let obj_props: HashSet<String> = HashSet::new();
        let datatypes: HashMap<String, String> = HashMap::new();
        let mut namespaces: HashMap<String, String> = HashMap::new();
        // The flip case: soul declares the migrated (kit-less) namespace.
        namespaces.insert("soul".into(), "https://repolex.ai/ontology/soul/".into());

        let mut types = HashSet::new();
        let mut out = String::new();
        let declared = HashSet::new();
        let subjects = FileSubjects {
            file_uri: "<https://repolex.ai/git-lex/File/Journal/day-1.md>".into(),
            thing_uri: None,
            thing_key: None,
        };
        emit_spo_line_nquads(
            "soul.Journal.soulDay | hasValue | 55",
            &subjects,
            "<https://repolex.ai/git-lex/NamedGraph/now>",
            "Journal/day-1.md",
            &empty_paths, &obj_props, &datatypes, &declared, &namespaces, false,
            &mut types, &mut out,
        );
        assert!(
            out.contains("<https://repolex.ai/ontology/soul/soulDay>"),
            "predicate must follow the declared namespace, got: {out}"
        );
        assert!(!out.contains("/ontology/kit/soul/"), "retired pattern leaked: {out}");

        // No declaration for this kit → conventional fallback.
        let mut out2 = String::new();
        let subjects2 = FileSubjects {
            file_uri: "<https://repolex.ai/git-lex/File/friend/selkie.md>".into(),
            thing_uri: None,
            thing_key: None,
        };
        emit_spo_line_nquads(
            "copia.Being.beingName | hasValue | selkie",
            &subjects2,
            "<https://repolex.ai/git-lex/NamedGraph/now>",
            "friend/selkie.md",
            &empty_paths, &obj_props, &datatypes, &declared, &namespaces, false,
            &mut types, &mut out2,
        );
        assert!(
            out2.contains("<https://repolex.ai/ontology/copia/beingName>"),
            "undeclared kit must use the conventional (app-tier) fallback, got: {out2}"
        );
    }

    /// The a-box base derives from the kit's ontology namespace by dropping
    /// the /ontology/ tier — the universal instance law
    /// (`<application>/<Class>/<id>`), never a second hardcoded pattern.
    #[test]
    fn app_base_drops_the_ontology_tier() {
        assert_eq!(
            app_base_from_kit_ns("https://repolex.ai/ontology/soul/"),
            "https://repolex.ai/soul/"
        );
        assert_eq!(
            app_base_from_kit_ns("https://repolex.ai/ontology/git-lex/"),
            "https://repolex.ai/git-lex/"
        );
        // A namespace without the tier passes through untouched.
        assert_eq!(
            app_base_from_kit_ns("https://example.org/vocab/"),
            "https://example.org/vocab/"
        );
    }

    /// A file with no kit-classed lines anchors NO Thing: File node only —
    /// the bare-markdown tier. The File IRI is the path verbatim under the
    /// git-lex File family (no scaffold stripping).
    #[test]
    fn derive_subjects_no_kit_lines_is_file_only() {
        let lines = vec![
            "README.md | linksTo | docs/intro.md".to_string(),
            "title | hasValue | hello".to_string(),
        ];
        let id_props = HashMap::new();
        let declared = HashSet::new();
        let obj_props = HashSet::new();
        let namespaces = HashMap::new();
        let s = derive_file_subjects(
            &lines, "README.md", &id_props, &declared, &obj_props, &namespaces, false,
        );
        assert_eq!(s.file_uri, "<https://repolex.ai/git-lex/File/README.md>");
        assert!(s.thing_uri.is_none());
        assert!(s.thing_key.is_none());
    }

    /// '%', '"', and '\' are legal in filenames but illegal (or
    /// escape-significant) in IRIs — unencoded they made a file like
    /// `100%.md` panic every sync (deep-review HIGH #3). '%' must be
    /// encoded FIRST so already-encoded output is never double-mangled.
    #[test]
    fn uri_encode_path_covers_iri_breaking_chars() {
        assert_eq!(uri_encode_path("100%.md"), "100%25.md");
        assert_eq!(uri_encode_path("he\"llo.md"), "he%22llo.md");
        assert_eq!(uri_encode_path("back\\slash.md"), "back%5Cslash.md");
        assert_eq!(uri_encode_path("a b.md"), "a%20b.md");
        // untouched safe chars
        assert_eq!(uri_encode_path("Soul/Memory/plain-file.md"), "Soul/Memory/plain-file.md");
    }

    /// Every encoded output must parse as an IRI path segment: round-trip
    /// through oxigraph's NamedNode to prove the class of panic is closed.
    #[test]
    fn uri_encode_path_output_is_valid_iri() {
        for name in ["100%.md", "he\"llo.md", "back\\slash.md", "sp ace.md", "a{b}|c^d`e[f].md"] {
            let iri = format!("https://repolex.ai/soul/{}", uri_encode_path(name));
            oxigraph::model::NamedNode::new(&iri)
                .unwrap_or_else(|e| panic!("{} → {} not a valid IRI: {}", name, iri, e));
        }
    }
}

#[cfg(test)]
mod wikilink_pattern_tests {
    use super::WIKILINK_PATTERN;

    /// PIN: a wikilink target never spans lines. A `[[...]]` token broken
    /// across two physical lines (wrapped terminal paste in a transcript
    /// doc) is prose, not a link — matching it used to write a raw newline
    /// into a sidecar value and abort sync (Selkie's day-10 transcript).
    #[test]
    fn wikilink_never_matches_across_newlines() {
        let re = regex::Regex::new(WIKILINK_PATTERN).unwrap();
        assert!(re.captures("see [[Squad/Squaddie/lspy]] there").is_some());
        // The day-10 failure shape: link broken by source-level wrapping.
        let wrapped = "prefix [[Sq\n         uad/Squaddie/lspy]] suffix";
        assert!(re.captures(wrapped).is_none(), "split token must NOT be a link");
    }

    /// PIN: the wikilink target passes into the sidecar VERBATIM, leading
    /// slash included. Under the 2026-07-28 path law the slash is semantic
    /// (bare = source-folder-relative, `/` = repo-rooted); an extraction-
    /// time strip (tried and reverted 2026-08-01) silently re-bound every
    /// root-anchored link to doc-relative.
    #[test]
    fn wikilink_target_passes_through_verbatim() {
        let re = regex::Regex::new(WIKILINK_PATTERN).unwrap();
        let cap = re.captures("see [[/Soul/Note/x.md]]").unwrap();
        assert_eq!(&cap[1], "/Soul/Note/x.md");
    }
}
