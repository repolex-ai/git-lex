//! git2-layer N-Quads producer — the ONE producer for the git machinery layer.
//!
//! A faithful mirror of the git2 (libgit2) object model, emitting exactly the
//! vocabulary declared in `ontology/git-lex/git2/git2.ttl` (kit-base). Classes
//! are git2 types, properties are git2 accessors; nothing here is invented.
//! Every IRI is DERIVED from repo data under the universal law
//! (instance IRI = t-box IRI minus `ontology/`):
//!
//!   class    https://repolex.ai/ontology/git-lex/git2/Commit
//!   instance https://repolex.ai/git-lex/git2/Commit/<sha>
//!
//! This module replaces the shell-out `generate_git_nquads` (old `git:` vocab,
//! retired): library reads instead of parsing porcelain text — no subprocess,
//! no PATH/locale dependence, no text-format drift.
//!
//! ONE-producer contract (Rob-ruled): this function feeds BOTH `git lex query`
//! (serialized to the caller) and sync (loaded into oxigraph). Byte-same
//! output, two sinks — the surfaces can never drift.
//!
//! Layers emitted, and their named graphs (same graph names as the old layer):
//!   repo             — the managed-repo node (git-lex:Repo ⊑ git2:Repository,
//!                      genesisSha + repo.yml facts per git-lex.ttl v0.6)
//!   commits          — git2:Commit + per-commit git2:Signature records
//!   refs             — git2:Branch / git2:Tag
//!   filetree/<head>  — git2:IndexEntry per file at HEAD + git2:Blob nodes
//!
//! Signature records are PER COMMIT (Rob-ruled: git2's exact structure; no
//! invented person-node dedup — an authors rollup, if ever wanted, is a
//! derived view). A Signature's identity is its owning commit + role, so its
//! IRI derives as git2/Signature/<sha>/author | /committer.
//!
//! NOT emitted here (deliberately):
//!   - Changesets — ruled dead 2026-07-20 (the one graph subsumes them).
//!   - DiffDelta/BlameHunk/IndexEntry-at-every-commit — declared in git2.ttl
//!     (legit library types, kept per Rob's trim rule) but not persisted.
//!   - The old layer's language tags + blame author strings — invented,
//!     write-only, consumer-less; killed at the git2 cutover (Rob-ruled).

use crate::git::graph_uri;
use crate::nquad::{nq_escape, uri_encode_path};
use git_lex::find_git_root;

/// Instance-IRI base for git2 machinery objects (universal law: the git2
/// t-box minus `ontology/`).
pub(crate) fn git2_uri(path: &str) -> String {
    format!("https://repolex.ai/git-lex/git2/{path}")
}

const GIT2_NS: &str = "https://repolex.ai/ontology/git-lex/git2/";
const GITLEX_NS: &str = "https://repolex.ai/ontology/git-lex/";
const RDF_TYPE: &str = "<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>";
const XSD_DATETIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";

/// Convert a git2 `Time` (seconds since epoch + offset minutes) to an
/// `xsd:dateTime` string WITH the original timezone offset preserved,
/// e.g. `2026-07-19T14:08:37-07:00`.
///
/// This computes `git2:xsdDateTimeDerived` — the ONE derived (non-library)
/// value in the git2 vocabulary, and its name carries that status (Rob-ruled,
/// git2.ttl v0.2.0). The raw pair (`git2:seconds`/`git2:offsetMinutes`) is
/// what git itself stores; this conversion exists because every consumer of
/// the graph sorts and filters on time, and xsd:dateTime is what SPARQL
/// understands natively.
///
/// Date math is the standard civil-from-days algorithm (Howard Hinnant,
/// public domain) — no external date dependency.
pub(crate) fn git2_time_to_datetime(seconds: i64, offset_minutes: i32) -> String {
    let local_secs = seconds + (offset_minutes as i64) * 60;
    let days = local_secs.div_euclid(86_400);
    let secs_of_day = local_secs.rem_euclid(86_400);

    // civil_from_days: days since 1970-01-01 → (y, m, d)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };

    let (hh, rem) = (secs_of_day / 3600, secs_of_day % 3600);
    let (mi, ss) = (rem / 60, rem % 60);

    let (sign, off) = if offset_minutes < 0 { ('-', -offset_minutes) } else { ('+', offset_minutes) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}{:02}:{:02}",
        y, m, d, hh, mi, ss, sign, off / 60, off % 60
    )
}

/// Percent-encode a path for use as an IRI tail segment (same policy as the
/// old layer's `uri_encode_path`: keep `/`, encode spaces + reserved chars).
fn encode_path(path: &str) -> String {
    uri_encode_path(path)
}

/// Emit one Signature record: the per-commit value object, exactly as git2
/// models it. `role` is "author" or "committer" (the two accessors on Commit).
/// IRI shape (Rob-ruled 2026-07-21, final): Signature/<full-40-sha>-<role> —
/// the FULL commit sha, no truncation, so the segment matches the Commit IRI
/// exactly and the join is obvious. Two nodes per commit always, even when
/// author and committer are byte-identical values.
fn emit_signature(
    nq: &mut String,
    graph: &str,
    commit_sha: &str,
    role: &str,
    sig: &git2::Signature<'_>,
) -> String {
    let su = format!("<{}>", git2_uri(&format!("Signature/{}-{}", commit_sha, role)));
    nq.push_str(&format!("{su} {RDF_TYPE} <{GIT2_NS}Signature> {graph} .\n"));
    if let Some(name) = sig.name() {
        if !name.is_empty() {
            nq.push_str(&format!("{su} <{GIT2_NS}signatureName> \"{}\" {graph} .\n", nq_escape(name)));
        }
    }
    if let Some(email) = sig.email() {
        if !email.is_empty() {
            nq.push_str(&format!("{su} <{GIT2_NS}email> \"{}\" {graph} .\n", nq_escape(email)));
        }
    }
    // Time (git2.ttl v0.2.0, Rob-ruled): the raw pair is library-native (what
    // git stores in the commit bytes); the dateTime is git-lex's DERIVATION
    // and its property name says so. git2:when is retired — never emit it.
    let when = sig.when();
    nq.push_str(&format!(
        "{su} <{GIT2_NS}seconds> \"{}\"^^<{XSD_INTEGER}> {graph} .\n",
        when.seconds()
    ));
    nq.push_str(&format!(
        "{su} <{GIT2_NS}offsetMinutes> \"{}\"^^<{XSD_INTEGER}> {graph} .\n",
        when.offset_minutes()
    ));
    nq.push_str(&format!(
        "{su} <{GIT2_NS}xsdDateTimeDerived> \"{}\"^^<{XSD_DATETIME}> {graph} .\n",
        git2_time_to_datetime(when.seconds(), when.offset_minutes())
    ));
    su
}

/// The git2-layer producer. Reads the repository via the git2 library and
/// returns N-Quads text (the same text `git lex query` serializes and sync
/// loads into oxigraph — one producer, two sinks).
pub(crate) fn generate_git2_nquads() -> String {
    let mut nq = String::new();
    let Some(git_root) = find_git_root() else {
        return nq; // not a git repo — nothing to emit
    };
    let repo = match git2::Repository::open(&git_root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("warning: git2 could not open the repository — git layer will be EMPTY: {e}");
            return nq;
        }
    };

    // ---- repo graph: the ONE managed-repository node --------------------
    // git-lex.ttl v0.6 (kit-base 0bf10d7, Rob-ruled): git-lex:Repo ⊑
    // git2:Repository — the node carries BOTH types; its IRI derives from
    // the genesis sha (the repo's stable identity). One property per
    // repo.yml key (snake_case key → camelCase property). `first_commit` is
    // NOT a property — it IS genesisSha, computed from git, so the computed
    // value is emitted and the yml duplicate ignored. List-valued keys
    // (optional_kits, substrates) emit one triple per value. Unknown keys
    // are reported loudly, never silently dropped, never emitted undeclared.
    if let Some(genesis) = crate::git::genesis_sha() {
        let graph = format!("<{}>", graph_uri("repo"));
        let ru = format!("<{}>", git2_uri(&format!("Repository/{genesis}")));
        nq.push_str(&format!("{ru} {RDF_TYPE} <{GITLEX_NS}Repo> {graph} .\n"));
        nq.push_str(&format!("{ru} {RDF_TYPE} <{GIT2_NS}Repository> {graph} .\n"));
        nq.push_str(&format!("{ru} <{GITLEX_NS}genesisSha> \"{genesis}\" {graph} .\n"));
        if let Ok(content) = std::fs::read_to_string(git_root.join(".lex").join("repo.yml")) {
            let mut current_list: Option<&str> = None;
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                if let Some(item) = trimmed.strip_prefix("- ") {
                    if let Some(prop) = current_list {
                        let item = item.trim().trim_matches('"');
                        if !item.is_empty() {
                            nq.push_str(&format!(
                                "{ru} <{GITLEX_NS}{prop}> \"{}\" {graph} .\n",
                                nq_escape(item)
                            ));
                        }
                    }
                    continue;
                }
                let Some(idx) = trimmed.find(':') else { continue };
                let key = trimmed[..idx].trim();
                let val = trimmed[idx + 1..].trim().trim_matches('"');
                current_list = None;
                match key {
                    "name" | "kit" | "version" if !val.is_empty() => {
                        nq.push_str(&format!(
                            "{ru} <{GITLEX_NS}{key}> \"{}\" {graph} .\n",
                            nq_escape(val)
                        ));
                    }
                    "agent_name" if !val.is_empty() => {
                        nq.push_str(&format!("{ru} <{GITLEX_NS}agentName> \"{}\" {graph} .\n", nq_escape(val)));
                    }
                    "agent_email" if !val.is_empty() => {
                        nq.push_str(&format!("{ru} <{GITLEX_NS}agentEmail> \"{}\" {graph} .\n", nq_escape(val)));
                    }
                    "created" if !val.is_empty() => {
                        nq.push_str(&format!(
                            "{ru} <{GITLEX_NS}created> \"{}\"^^<http://www.w3.org/2001/XMLSchema#date> {graph} .\n",
                            nq_escape(val)
                        ));
                    }
                    "first_commit" => {} // legacy duplicate of genesisSha (computed) — ignored by ruling
                    "genesis_sha" => {} // duplicate of genesisSha (computed) — ignored by ruling
                    "dev_history_horizon" => {} // CONFIG, not a fact (dev-only walk stopgap) — never emitted
                    "link_semantics" => {} // CONFIG (migration fence, spec §5) — never emitted
                    "optional_kits" => current_list = Some("optionalKit"),
                    "substrates" => current_list = Some("substrate"),
                    "name" | "kit" | "version" | "agent_name" | "agent_email" | "created" => {} // known key, empty value
                    other => eprintln!(
                        "warning: repo.yml key `{other}` has no declared git-lex: property — NOT emitted (declare it or remove it)"
                    ),
                }
            }
        }
    }

    // ---- commits graph: every commit reachable from any ref (old layer's
    // `git log --all`), plus HEAD for the detached case. ------------------
    {
        let graph = format!("<{}>", graph_uri("commits"));
        let mut walk = match repo.revwalk() {
            Ok(w) => w,
            Err(e) => {
                eprintln!("warning: git2 revwalk failed — commits layer will be EMPTY: {e}");
                return nq;
            }
        };
        let _ = walk.push_glob("*"); // all refs (branches, tags, remotes)
        let _ = walk.push_head(); // detached HEAD safety
        // Topological, oldest-first: parents always precede children, so the
        // enumeration position IS the commit's ordinal (1 = genesis).
        let _ = walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE);
        for (idx, oid) in walk.flatten().enumerate() {
            let Ok(commit) = repo.find_commit(oid) else { continue };
            let sha = oid.to_string();
            let cu = format!("<{}>", git2_uri(&format!("Commit/{sha}")));
            nq.push_str(&format!("{cu} {RDF_TYPE} <{GIT2_NS}Commit> {graph} .\n"));
            nq.push_str(&format!("{cu} <{GIT2_NS}id> \"{sha}\" {graph} .\n"));
            // git2:ordinalDerived (git2.ttl v0.3.0, Rob-ruled): DERIVED — git
            // does not number commits. The position in the topological walk,
            // stamped at generation; the ordering authority for
            // latest-event-wins, where author dates can tie or lie.
            nq.push_str(&format!(
                "{cu} <{GIT2_NS}ordinalDerived> \"{}\"^^<{XSD_INTEGER}> {graph} .\n",
                idx + 1
            ));
            if let Some(summary) = commit.summary() {
                nq.push_str(&format!("{cu} <{GIT2_NS}summary> \"{}\" {graph} .\n", nq_escape(summary)));
            }
            if let Some(message) = commit.message() {
                nq.push_str(&format!("{cu} <{GIT2_NS}message> \"{}\" {graph} .\n", nq_escape(message)));
            }
            if let Some(body) = commit.body() {
                nq.push_str(&format!("{cu} <{GIT2_NS}body> \"{}\" {graph} .\n", nq_escape(body)));
            }
            let au = emit_signature(&mut nq, &graph, &sha, "author", &commit.author());
            nq.push_str(&format!("{cu} <{GIT2_NS}author> {au} {graph} .\n"));
            let com = emit_signature(&mut nq, &graph, &sha, "committer", &commit.committer());
            nq.push_str(&format!("{cu} <{GIT2_NS}committer> {com} {graph} .\n"));
            for parent in commit.parent_ids() {
                nq.push_str(&format!(
                    "{cu} <{GIT2_NS}parent> <{}> {graph} .\n",
                    git2_uri(&format!("Commit/{parent}"))
                ));
            }
        }
    }

    // ---- refs graph: branches + tags ------------------------------------
    {
        let graph = format!("<{}>", graph_uri("refs"));
        if let Ok(branches) = repo.branches(None) {
            for (branch, _kind) in branches.flatten() {
                let r = branch.get();
                let (Some(refname), Some(short)) = (r.name(), r.shorthand()) else { continue };
                let Some(target) = r.target() else { continue };
                let bu = format!("<{}>", git2_uri(&format!("Branch/{}", encode_path(short))));
                nq.push_str(&format!("{bu} {RDF_TYPE} <{GIT2_NS}Branch> {graph} .\n"));
                nq.push_str(&format!("{bu} <{GIT2_NS}refName> \"{}\" {graph} .\n", nq_escape(refname)));
                nq.push_str(&format!("{bu} <{GIT2_NS}shorthand> \"{}\" {graph} .\n", nq_escape(short)));
                nq.push_str(&format!(
                    "{bu} <{GIT2_NS}target> <{}> {graph} .\n",
                    git2_uri(&format!("Commit/{target}"))
                ));
            }
        }
        if let Ok(refs) = repo.references_glob("refs/tags/*") {
            for r in refs.flatten() {
                let (Some(refname), Some(short)) = (r.name(), r.shorthand()) else { continue };
                // Peel annotated tags to the commit they ultimately point at
                // (matches the old layer, which listed tags by target commit).
                let Ok(target) = r.peel_to_commit() else { continue };
                let tu = format!("<{}>", git2_uri(&format!("Tag/{}", encode_path(short))));
                nq.push_str(&format!("{tu} {RDF_TYPE} <{GIT2_NS}Tag> {graph} .\n"));
                nq.push_str(&format!("{tu} <{GIT2_NS}refName> \"{}\" {graph} .\n", nq_escape(refname)));
                nq.push_str(&format!("{tu} <{GIT2_NS}shorthand> \"{}\" {graph} .\n", nq_escape(short)));
                nq.push_str(&format!(
                    "{tu} <{GIT2_NS}target> <{}> {graph} .\n",
                    git2_uri(&format!("Commit/{}", target.id()))
                ));
            }
        }
    }

    // ---- filetree/<head> graph: every file at HEAD as a git2:IndexEntry,
    // each joined to its content git2:Blob. (git2.ttl: entries materialized
    // from the commit's tree — the flat committed-files view, never the
    // mutable staging index.) --------------------------------------------
    if let Ok(head) = repo.head() {
        if let Some(head_oid) = head.target() {
            let head_sha = head_oid.to_string();
            let graph = format!("<{}>", graph_uri(&format!("filetree/{head_sha}")));
            let commits_graph = format!("<{}>", graph_uri("commits"));
            let cu = format!("<{}>", git2_uri(&format!("Commit/{head_sha}")));
            if let Ok(tree) = repo.find_commit(head_oid).and_then(|c| c.tree()) {
                let mut seen_blobs = std::collections::HashSet::new();
                let _ = tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
                    if entry.kind() != Some(git2::ObjectType::Blob) {
                        return git2::TreeWalkResult::Ok;
                    }
                    let name = entry.name().unwrap_or("");
                    let path = if dir.is_empty() { name.to_string() } else { format!("{dir}{name}") };
                    let blob_oid = entry.id();
                    let eu = format!(
                        "<{}>",
                        git2_uri(&format!("IndexEntry/{head_sha}/{}", encode_path(&path)))
                    );
                    nq.push_str(&format!("{eu} {RDF_TYPE} <{GIT2_NS}IndexEntry> {graph} .\n"));
                    nq.push_str(&format!("{eu} <{GIT2_NS}path> \"{}\" {graph} .\n", nq_escape(&path)));
                    nq.push_str(&format!("{eu} <{GIT2_NS}id> \"{blob_oid}\" {graph} .\n"));
                    nq.push_str(&format!(
                        "{eu} <{GIT2_NS}mode> \"{}\"^^<{XSD_INTEGER}> {graph} .\n",
                        entry.filemode()
                    ));
                    let bu = format!("<{}>", git2_uri(&format!("Blob/{blob_oid}")));
                    nq.push_str(&format!("{eu} <{GIT2_NS}blob> {bu} {graph} .\n"));
                    // The commit → file link (git2:file, declared domain Commit).
                    nq.push_str(&format!("{cu} <{GIT2_NS}file> {eu} {commits_graph} .\n"));
                    if let Ok(blob) = repo.find_blob(blob_oid) {
                        // IndexEntry file_size == the committed content's size.
                        nq.push_str(&format!(
                            "{eu} <{GIT2_NS}fileSize> \"{}\"^^<{XSD_INTEGER}> {graph} .\n",
                            blob.size()
                        ));
                        if seen_blobs.insert(blob_oid) {
                            nq.push_str(&format!("{bu} {RDF_TYPE} <{GIT2_NS}Blob> {graph} .\n"));
                            nq.push_str(&format!("{bu} <{GIT2_NS}id> \"{blob_oid}\" {graph} .\n"));
                            nq.push_str(&format!(
                                "{bu} <{GIT2_NS}size> \"{}\"^^<{XSD_INTEGER}> {graph} .\n",
                                blob.size()
                            ));
                            nq.push_str(&format!(
                                "{bu} <{GIT2_NS}isBinary> \"{}\"^^<{XSD_BOOLEAN}> {graph} .\n",
                                blob.is_binary()
                            ));
                        }
                    }
                    git2::TreeWalkResult::Ok
                });
            }
        }
    }

    nq
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetime_epoch() {
        assert_eq!(git2_time_to_datetime(0, 0), "1970-01-01T00:00:00+00:00");
    }

    #[test]
    fn datetime_negative_offset() {
        // 2026-07-19T14:08:37-07:00 == 2026-07-19T21:08:37Z == 1784495317
        assert_eq!(git2_time_to_datetime(1_784_495_317, -420), "2026-07-19T14:08:37-07:00");
    }

    #[test]
    fn datetime_positive_half_hour_offset() {
        // 1970-01-01T05:30:00+05:30 == epoch 0
        assert_eq!(git2_time_to_datetime(0, 330), "1970-01-01T05:30:00+05:30");
    }

    #[test]
    fn datetime_leap_year_day() {
        // 2024-02-29T00:00:00+00:00 == 1709164800
        assert_eq!(git2_time_to_datetime(1_709_164_800, 0), "2024-02-29T00:00:00+00:00");
    }

    #[test]
    fn datetime_pre_epoch() {
        // 1969-12-31T23:59:59+00:00 == -1
        assert_eq!(git2_time_to_datetime(-1, 0), "1969-12-31T23:59:59+00:00");
    }

    /// Parity: the library walk must see exactly the commits `git rev-list
    /// --all` sees. Shell git is GROUND TRUTH in tests only — production code
    /// never shells out.
    #[test]
    fn commit_count_matches_git_cli() {
        let out = std::process::Command::new("git")
            .args(["rev-list", "--all", "--count"])
            .output()
            .expect("git CLI available for test ground truth");
        if !out.status.success() {
            return; // not in a git repo (e.g. bare CI checkout) — nothing to compare
        }
        let expected: usize = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
        let nq = generate_git2_nquads();
        let typed_commits = nq
            .lines()
            .filter(|l| l.contains("git2/Commit>") && l.contains("22-rdf-syntax-ns#type"))
            .count();
        assert_eq!(
            typed_commits, expected,
            "git2 revwalk commit count must match `git rev-list --all --count`"
        );
    }

    /// Ordinals: every commit gets exactly one, they're unique, and they run
    /// 1..=N with parents before children (genesis = 1).
    #[test]
    fn ordinals_are_unique_and_dense() {
        let nq = generate_git2_nquads();
        if nq.is_empty() {
            return; // not in a git repo
        }
        let ordinals: Vec<i64> = nq
            .lines()
            .filter(|l| l.contains("ordinalDerived>"))
            .filter_map(|l| l.split('"').nth(1))
            .filter_map(|v| v.parse().ok())
            .collect();
        let commits = nq
            .lines()
            .filter(|l| l.contains("git2/Commit>") && l.contains("22-rdf-syntax-ns#type"))
            .count();
        assert_eq!(ordinals.len(), commits, "exactly one ordinal per commit");
        let mut sorted = ordinals.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ordinals.len(), "ordinals must be unique");
        assert_eq!(sorted.first(), Some(&1), "genesis commit is ordinal 1");
        assert_eq!(sorted.last(), Some(&(commits as i64)), "ordinals are dense 1..=N");
    }

    /// Structural: every commit carries exactly one author + one committer
    /// link, and every signature IRI derives from its commit + role.
    #[test]
    fn every_commit_has_author_and_committer() {
        let nq = generate_git2_nquads();
        if nq.is_empty() {
            return; // not in a git repo
        }
        let commits: std::collections::HashSet<&str> = nq
            .lines()
            .filter(|l| l.contains("git2/Commit>") && l.contains("22-rdf-syntax-ns#type"))
            .filter_map(|l| l.split_whitespace().next())
            .collect();
        for cu in &commits {
            let authors = nq.lines().filter(|l| l.starts_with(cu) && l.contains("git2/author>")).count();
            let committers = nq.lines().filter(|l| l.starts_with(cu) && l.contains("git2/committer>")).count();
            assert_eq!(authors, 1, "commit {cu} must have exactly one author link");
            assert_eq!(committers, 1, "commit {cu} must have exactly one committer link");
        }
    }

    /// Vocabulary discipline: every emitted git2: predicate/class must be one
    /// declared in git2.ttl (mirrored here as a constant list — the full
    /// store-side check is the Part-4.5 data-quality suite).
    #[test]
    fn emits_only_declared_git2_vocab() {
        const DECLARED: &[&str] = &[
            "Repository", "Commit", "Signature", "IndexEntry", "Blob", "Branch", "Tag",
            "id", "signatureName", "refName", "summary", "message", "body",
            "author", "committer", "parent", "file", "email", "path",
            "seconds", "offsetMinutes", "xsdDateTimeDerived", "ordinalDerived",
            "fileSize", "mode", "blob", "size", "isBinary", "shorthand", "target",
        ];
        // git-lex.ttl terms THIS module may emit (the repo node, v0.6).
        const GITLEX_DECLARED: &[&str] = &[
            "Repo", "genesisSha", "name", "kit", "created", "agentName",
            "agentEmail", "version", "optionalKit", "substrate",
        ];
        let nq = generate_git2_nquads();
        for line in nq.lines() {
            for term in line.split_whitespace() {
                if let Some(rest) = term.strip_prefix(&format!("<{GIT2_NS}")) {
                    let local = rest.trim_end_matches('>');
                    assert!(
                        DECLARED.contains(&local),
                        "emitted git2 term not declared in git2.ttl: {local}"
                    );
                } else if let Some(rest) = term.strip_prefix(&format!("<{GITLEX_NS}")) {
                    let local = rest.trim_end_matches('>');
                    if local.contains('/') {
                        continue; // a nested namespace (git2/, fm/, md/), not a git-lex: core term
                    }
                    assert!(
                        GITLEX_DECLARED.contains(&local),
                        "emitted git-lex term not declared in git-lex.ttl: {local}"
                    );
                }
            }
        }
    }
}
