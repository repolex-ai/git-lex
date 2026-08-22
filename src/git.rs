//! Git operations and repo-identity helpers.
//!
//! Peeled out of `main.rs` during modularization. Everything here either
//! shells out to `git` or derives identity from the remote URL.

use std::fs;
use std::process::Command;

use git_lex::find_git_root;

/// Resolve the soul's genesis SHA (three tiers, ordered by cost) and make
/// sure it is RECORDED. Called ONCE per sync.
///
/// The authority is `repo.yml`'s `genesis_sha:` key (Rob-ruled 2026-08-01
/// — the identity-fragmentation cleanup: one fact, one file, one name).
/// Repos carrying the legacy `first_commit:` key self-migrate here: the
/// line is rewritten in place, value preserved.
///
/// TRANSITION: `.lex/identity.yml` is still written too — it is a
/// cross-system contract (Pool's boot-skip reads it). It stops being
/// written, and repos delete it, ONLY after Pool's read cuts over to
/// repo.yml (coordinated 3-step, step 2 is Pool's).
pub(crate) fn ensure_genesis_recorded() -> Option<String> {
    let sha = genesis_sha()?;
    if let Err(e) = ensure_repo_yml_genesis(&sha) {
        eprintln!("warning: could not record genesis_sha in .lex/repo.yml: {e}");
    }
    // Downstream consumers read identity.yml until the Pool cutover; a
    // swallowed write error is invisible in-process while an external
    // consumer reads a missing/stale file — warn loudly.
    if let Err(e) = write_identity_yml_sha(&sha) {
        eprintln!("warning: could not write .lex/identity.yml: {} — downstream consumers (Pool, federation) read this file", e);
    }
    Some(sha)
}

/// The soul's genesis (first-commit) SHA, if resolvable. repo.yml is the
/// authority; identity.yml is the transition-era fallback; git itself is
/// the recompute-of-last-resort.
pub(crate) fn genesis_sha() -> Option<String> {
    sha_from_repo_yml()
        .or_else(sha_from_identity_yml)
        .or_else(sha_from_git)
}

/// Ensure repo.yml carries `genesis_sha: <sha>` — writing textually to
/// preserve comments/ordering (the RepoYml struct is read-side only).
/// Three cases: canonical key present → no-op; legacy `first_commit:` line
/// present → rewritten in place to the canonical key (self-migration);
/// neither → appended.
pub(crate) fn ensure_repo_yml_genesis(sha: &str) -> std::io::Result<()> {
    let Some(root) = find_git_root() else { return Ok(()) };
    let path = root.join(".lex").join("repo.yml");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|l| l.trim_start().starts_with("genesis_sha:")) {
        return Ok(());
    }
    let mut migrated = false;
    let mut lines: Vec<String> = existing
        .lines()
        .map(|l| {
            if !migrated && l.trim_start().starts_with("first_commit:") {
                migrated = true;
                format!("genesis_sha: {sha}")
            } else {
                l.to_string()
            }
        })
        .collect();
    if !migrated {
        lines.push(format!("genesis_sha: {sha}"));
    }
    let mut content = lines.join("\n");
    content.push('\n');
    fs::write(&path, content)
}

/// The MANAGED-BY header every `.lex/repo.yml` carries (Rob-ruled
/// 2026-08-22). The file mixes machine bookkeeping with the two keys that
/// decide who signs commits, and nothing in it marked which was which or
/// who owned them — so nobody, Rob included, could tell what was safe to
/// touch. The ruling settles it: ALL of repo.yml is git-lex's, none of it
/// is the squaddie's, and the file now says so in its own first lines.
///
/// Text, not a doc-comment, because the audience reads the FILE.
pub(crate) const REPO_YML_HEADER: &str = "\
# these values are set at init, managed by git-lex, DO NOT EDIT
#
# What they do, so the file is not a mystery:
#
#   name, kit, created, optional_kits   repo bookkeeping.
#   genesis_sha                         your soul's permanent identity in
#                                       the graph. Never changes, ever.
#   agent_name, agent_email             who signs your commits. git-lex
#                                       copies these into
#                                       .claude/settings.json on every
#                                       `git lex kit-update`, and Claude
#                                       Code injects them into git.
";

/// Converge `.lex/repo.yml` onto the current [`REPO_YML_HEADER`]. An
/// older header is REPLACED, not left alone — install-once was the bug in
/// the first cut of this (2026-08-22, same hour): Rob edited the text and
/// every repo that had already taken the previous version would have kept
/// it forever, which is the README.lex.md disease (#75) reproduced in a
/// new file. Convergence means the header is a thing we can still edit.
///
/// Textual write, same reason as `ensure_repo_yml_genesis` — the file
/// carries comments and ordering the read-side struct cannot round-trip.
/// Every non-header byte is preserved, in order. No file → no-op: this
/// heals a repo.yml, it never conjures one. Idempotent once converged.
///
/// Called from kit-update, which runs at every compaction, so the whole
/// existing fleet converges without anyone doing anything.
pub(crate) fn ensure_repo_yml_header(root: &std::path::Path) -> std::io::Result<()> {
    let path = root.join(".lex").join("repo.yml");
    let Ok(existing) = fs::read_to_string(&path) else { return Ok(()) };
    let body = strip_managed_header(&existing);
    let want = format!("{REPO_YML_HEADER}{body}");
    if existing == want {
        return Ok(());
    }
    fs::write(&path, want)
}

/// Drop a git-lex managed header from the front of a repo.yml, returning
/// the rest untouched. Scoped deliberately: only the LEADING run of
/// comment lines is considered, and only when that run is ours (it names
/// itself). A squaddie's own leading comment, or the trailing
/// `dev_history_horizon` note, is never eaten.
fn strip_managed_header(content: &str) -> &str {
    let lead_len = content
        .lines()
        .take_while(|l| l.trim_start().starts_with('#'))
        .map(|l| l.len() + 1)
        .sum::<usize>()
        .min(content.len());
    let (lead, rest) = content.split_at(lead_len);
    if lead.to_lowercase().contains("managed by git-lex") {
        rest
    } else {
        content
    }
}

// ---------------------------------------------------------------------
// Task-2 IRI families (Day-50 decisions): graph names + instance subjects
// carry NO soul identity. The store is the scope; one query works against
// every soul's oxigraph.
// ---------------------------------------------------------------------

/// Graph-container names: soul-independent ABSOLUTE IRIs, identical across
/// every soul's store — `GRAPH <https://repolex.ai/git-lex/NamedGraph/now>` is the same
/// query everywhere. (A literally-bare name like `now` is not a legal IRI:
/// oxigraph rejects it at the model level and SPARQL won't parse `<now>` —
/// probed Day-50. Absolute-and-identical is the standard shape that delivers
/// the portability requirement.)
pub(crate) const GRAPH_BASE: &str = "https://repolex.ai/git-lex/NamedGraph/";

/// The a-box (instance) base for soul-repo subjects. Subtexture-wide shape
/// (Rob, Day-50): `https://repolex.ai/<application>/<Class>/<instanceId>` —
/// no base word; vocabulary stays under `https://repolex.ai/ontology/...`.
/// Process-cached a-box base + scaffold-folder prefix, derived once from
/// repo.yml via `git_lex::resource_base_at` (kit short name, else repo
/// name; NEVER hardcoded — Rob-ruled 2026-07-28). The scaffold strip
/// generalizes the old "Soul/" rule: the folder named after the namespace,
/// capitalized, maps onto the namespace root ("Soul/Journal/x" →
/// <base>/Journal/x — no "/soul/Soul/" stutter).
/// (NOT-CHOSEN alternative, recorded for context: a per-call derivation
/// with an explicit root parameter — correct for multi-repo processes,
/// but every caller here is the single-repo CLI; the multi-repo serve
/// binary derives per-repo via resource_base_at directly.)
fn resource_base() -> &'static (String, String) {
    static BASE: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();
    BASE.get_or_init(|| {
        let base = find_git_root()
            .map(|r| git_lex::resource_base_at(&r))
            .unwrap_or_else(|| "https://repolex.ai/repo".to_string());
        let ns = base.rsplit('/').next().unwrap_or("repo");
        let mut cap = ns.to_string();
        if let Some(first) = cap.get_mut(0..1) {
            let up = first.to_uppercase();
            cap.replace_range(0..1, &up);
        }
        let strip = format!("{cap}/");
        (base, strip)
    })
}

/// The File-plane instance family (identity model Law 4, Rob-ruled
/// 2026-07-30): a File's id IS its repo-relative path, and File nodes are
/// git-lex application instances under the universal instance law — the
/// same shape the NamedGraph family already uses. NO scaffold-folder
/// stripping here (that is a Thing-plane derivation nicety): the path is
/// the id, verbatim, so `git-lex/File/Soul/Journal/day-1.md` and a
/// no-kit repo's `git-lex/File/README.md` follow one rule.
pub(crate) const FILE_BASE: &str = "https://repolex.ai/git-lex/File/";

/// THE one base every instance address resolves against.
///
/// Not a new hardcode — it is the part `GRAPH_BASE`, `FILE_BASE` and the
/// per-repo derived base (`https://repolex.ai/<ns>`) already share, named once
/// instead of spelled four times.
///
/// This is what the authored identifier form `<soul/Journal/day-7>` resolves
/// against (Rob's notation ruling). The whole point of that form is that the
/// namespace comes from the VALUE — so it must NOT resolve against the
/// document's own kit base, or a copia Texture referenced from a soul Note
/// lands under soul and joins to nothing.
pub(crate) const RESOURCE_ROOT: &str = "https://repolex.ai/";

/// A File node's IRI from its (already uri-encoded) repo-relative path.
pub(crate) fn file_iri(encoded_path: &str) -> String {
    format!("{FILE_BASE}{encoded_path}")
}

/// Mint a graph name: `https://repolex.ai/git-lex/NamedGraph/<name>`.
/// Graphs are instances of `git-lex:NamedGraph` (⊑ sd:NamedGraph, kit-base
/// 72be113) under the universal instance law — the git-lex application's
/// NamedGraph objects. The ontology graph's instance name is
/// `repo-ontology` (renamed from `ontology`, Rob Day-50).
pub(crate) fn graph_uri(name: &str) -> String {
    format!("{GRAPH_BASE}{name}")
}

/// Mint an instance-subject IRI under the soul a-box base.
///
/// - Empty path = the Self node: the namespace root itself
///   (`https://repolex.ai/soul`). Soul identity (genesis SHA) is a
///   FACT about the Self node, never part of any IRI.
/// - A tracked path under the `Soul/` scaffold root maps onto the namespace
///   root (`Soul/Memory/foo.md` → `…/soul/Memory/foo.md`) — the
///   `Soul/` folder IS the soul namespace, so it doesn't repeat.
/// - Everything else joins verbatim (`journal/day-1.md`, `commit/<sha>`,
///   `entity/<x>~<hash>`, …).
///
/// NOTE: file-derived subjects keep their extension (`.md`) — joins are by
/// filename (nquad.rs wikilink resolution + downstream resolvers exact-match
/// on it); see the Task-2 spec's ON HOLD ruling before ever changing that.
pub(crate) fn resource_uri(path: &str) -> String {
    let (base, strip) = resource_base();
    if path.is_empty() {
        return base.clone();
    }
    let tail = path.strip_prefix(strip.as_str()).unwrap_or(path);
    format!("{base}/{tail}")
}
// ---------------------------------------------------------------------
// SHA resolution (three tiers, ordered by cost)
// ---------------------------------------------------------------------

/// Read first-commit SHA from `.lex/identity.yml` if present.
///
/// `.lex/identity.yml` is a write-once cache of the soul's genesis-SHA.
/// System-owned (NOT squaddie-editable like `repo.yml`). Path-level
/// separation makes the invariant visible.
fn sha_from_identity_yml() -> Option<String> {
    let root = find_git_root()?;
    let content = fs::read_to_string(root.join(".lex").join("identity.yml")).ok()?;
    for line in content.lines() {
        if let Some(rest) = line.trim().strip_prefix("genesis_sha:") {
            let sha = rest.trim();
            if is_valid_sha(sha) {
                return Some(sha.to_string());
            }
        }
    }
    None
}

/// Genesis SHA from `.lex/repo.yml`, via the ONE reader. Canonical
/// `genesis_sha` first; legacy `first_commit` accepted until the repo's
/// next sync rewrites it (self-migration in ensure_repo_yml_genesis).
fn sha_from_repo_yml() -> Option<String> {
    let yml = git_lex::RepoYml::load(&find_git_root()?);
    let sha = yml.genesis_sha.or(yml.first_commit)?;
    is_valid_sha(&sha).then_some(sha)
}

/// Query git directly for the first commit SHA.
fn sha_from_git() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-list", "--max-parents=0", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    // Multiple root commits (rare) → take the LAST printed line, matching
    // git rev-list's default reverse-chronological order — earliest commit.
    let sha = raw.lines().last()?.trim();
    if is_valid_sha(sha) {
        Some(sha.to_string())
    } else {
        None
    }
}

/// Write the genesis SHA to `.lex/identity.yml`.
///
/// Three cases:
/// 1. File doesn't exist → write it fresh.
/// 2. File exists, has `genesis_sha:` line → no-op (already correct).
/// 3. File exists but has the legacy `identity:` key (from git-lex's
///    pre-7df99ce git-as-PKI era) → rewrite in place under the canonical
///    `genesis_sha:` key. Same SHA bytes, new label. The legacy `identity:`
///    line is dropped; everything else is preserved.
///
/// The "write once never modify" header in the file body refers to the
/// SHA *value*, not the file's existence — the genesis SHA is immutable,
/// but rewriting the key name when we discover the file's stuck on the
/// legacy schema is a fix, not a violation. Souls initialized pre-7df99ce
/// would otherwise be permanently stuck under sync-from / federation
/// readers that only know the new key.
fn write_identity_yml_sha(sha: &str) -> std::io::Result<()> {
    let root = match find_git_root() {
        Some(r) => r,
        None => return Ok(()),
    };
    let path = root.join(".lex").join("identity.yml");
    if path.exists() {
        let existing = fs::read_to_string(&path).unwrap_or_default();
        match identity_yml_rewrite_decision(&existing, sha) {
            None => Ok(()), // case 2: already correct
            Some(new_body) => fs::write(path, new_body), // case 3: legacy rewrite
        }
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, canonical_identity_yml(sha))
    }
}

/// Pure helper: given the existing identity.yml body and the canonical SHA,
/// return `Some(new_body)` if the file needs to be rewritten (legacy key
/// present, canonical missing), or `None` if it's already correct.
///
/// Preserves non-`identity:` lines from the original file so any comments
/// or future fields a soul added survive the migration.
fn identity_yml_rewrite_decision(existing: &str, sha: &str) -> Option<String> {
    let has_canonical = existing
        .lines()
        .any(|l| l.trim().starts_with("genesis_sha:"));
    if has_canonical {
        return None;
    }
    let mut body = canonical_identity_yml(sha);
    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("identity:") {
            continue; // drop legacy key
        }
        if body.contains(line) {
            continue; // don't double-write our own header lines
        }
        body.push_str(line);
        body.push('\n');
    }
    Some(body)
}

/// The canonical body for a fresh `.lex/identity.yml`.
fn canonical_identity_yml(sha: &str) -> String {
    format!(
        "# WRITE ONCE. NEVER MODIFY. Soul identity anchor.\n\
         # If you accidentally delete this file, the next `git lex sync`\n\
         # will recompute and rewrite it from the genesis commit.\n\
         genesis_sha: {}\n",
        sha
    )
}

/// Validate a SHA-1 as a SOUL IDENTITY ANCHOR: exactly 40 hex chars.
///
/// C6 fix (Day 38): this is the gate inside all three `sha_from_*` readers.
/// The genesis SHA is identity data — `.lex/identity.yml` + the
/// `git:genesisSha` fact (it no longer appears in any IRI, Day-50). A short
/// SHA here is still an IDENTITY-SPLIT risk across the Pool seam:
/// `pool sync-from` builds the Moments named-graph IRI from identity.yml's
/// genesis_sha, so a length disagreement between tiers would orphan a
/// soul's Moments in a differently-named graph (see the git-lex×Pool
/// compare, EDGE-2 / Pool #110). The anchor MUST be the full 40-hex SHA-1;
/// short forms are fine for display only.
fn is_valid_sha(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}
/// Stage and commit the working tree as a snapshot before a destructive
/// operation (e.g. `nuke`). The commit message includes `reason`.
///
/// No-op if there's nothing to commit (clean working tree) or if git is
/// unavailable. Errors are reported but not fatal — the destructive
/// operation can still proceed; we just won't have a snapshot.
pub(crate) fn auto_commit_snapshot(reason: &str) {
    // Check if we're in a git repo with commits
    let has_head = Command::new("git").args(["rev-parse", "HEAD"]).output()
        .map(|o| o.status.success()).unwrap_or(false);
    if !has_head {
        return; // No commits yet, nothing to snapshot
    }

    // Check if there's anything to commit
    let status = Command::new("git").args(["status", "--porcelain"]).output();
    let has_changes = match status {
        Ok(o) if o.status.success() => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        _ => return,
    };
    if !has_changes {
        return; // Clean working tree, nothing to snapshot
    }

    // Stage everything and commit. The add's exit status is CHECKED
    // (review #52): with `--allow-empty`, a failed add still let the
    // commit exit 0 with nothing staged — and the user was told their
    // working tree was snapshotted right before the destructive operation
    // proceeded. A false safety receipt is worse than none.
    let add_ok = Command::new("git")
        .args(["add", "-A"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !add_ok {
        eprintln!(
            "Warning: `git add -A` failed — the working tree was NOT snapshotted \
             before {}. Commit your changes yourself if you want them kept. Continuing.",
            reason
        );
        return;
    }
    let msg = format!("git lex auto-snapshot: {}", reason);
    let commit = Command::new("git").args(["commit", "-m", &msg, "--allow-empty"]).output();
    match commit {
        Ok(o) if o.status.success() => {
            println!("Auto-committed working tree before {}.", reason);
        }
        _ => {
            // Not fatal — maybe pre-commit hook failed, maybe nothing
            // actually staged (hidden files?). Just report and continue.
            eprintln!("Warning: auto-commit before {} did not succeed. Continuing.", reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SHA: &str = "700c5bd401abcd02abcd03abcd04abcd05abcd06";

    // C6 regression (Day 38): the identity anchor must be EXACTLY 40 hex —
    // a short SHA must NOT validate, or the same soul can get two base IRIs
    // (and Pool would orphan its Moments in a differently-named graph).
    #[test]
    fn full_40_hex_sha_is_valid() {
        assert!(is_valid_sha(TEST_SHA));
        assert_eq!(TEST_SHA.len(), 40);
    }

    #[test]
    fn short_sha_is_rejected_as_identity_anchor() {
        assert!(!is_valid_sha("700c5bd"));        // 7-char short form
        assert!(!is_valid_sha("700c5bd401abcd")); // 14-char
    }

    #[test]
    fn non_hex_and_overlong_are_rejected() {
        assert!(!is_valid_sha(""));                                          // empty
        assert!(!is_valid_sha("700c5bd401abcd02abcd03abcd04abcd05abcd0g"));  // 40 with 'g'
        assert!(!is_valid_sha("700c5bd401abcd02abcd03abcd04abcd05abcd06a")); // 41 hex
    }

    #[test]
    fn canonical_file_is_a_noop() {
        // Case 2: file already has the canonical key. No rewrite needed.
        let existing = canonical_identity_yml(TEST_SHA);
        let decision = identity_yml_rewrite_decision(&existing, TEST_SHA);
        assert!(decision.is_none(), "canonical file should not be rewritten");
    }

    #[test]
    fn legacy_identity_key_gets_migrated() {
        // Case 3: pre-7df99ce souls have `identity: <sha>` instead of
        // `genesis_sha: <sha>`. Must rewrite to canonical form.
        let legacy = format!("identity: {}\n", TEST_SHA);
        let decision = identity_yml_rewrite_decision(&legacy, TEST_SHA);
        let rewritten = decision.expect("legacy file must be rewritten");
        assert!(
            rewritten.contains(&format!("genesis_sha: {}", TEST_SHA)),
            "rewrite must contain canonical key: {}",
            rewritten
        );
        assert!(
            !rewritten.contains("identity:"),
            "rewrite must NOT contain legacy `identity:` line: {}",
            rewritten
        );
    }

    #[test]
    fn migration_preserves_extra_lines() {
        // If a soul or future git-lex added other fields (comments, metadata),
        // they survive the migration. Only `identity:` is dropped.
        let mixed = format!(
            "# my custom comment\n\
             identity: {}\n\
             custom_field: hello\n",
            TEST_SHA
        );
        let rewritten = identity_yml_rewrite_decision(&mixed, TEST_SHA)
            .expect("should rewrite");
        assert!(rewritten.contains("custom_field: hello"), "{}", rewritten);
        assert!(rewritten.contains("# my custom comment"), "{}", rewritten);
        assert!(rewritten.contains(&format!("genesis_sha: {}", TEST_SHA)));
        assert!(!rewritten.contains("identity:"));
    }

    #[test]
    fn empty_or_garbage_file_gets_canonical_body() {
        // If someone deleted the file body but the file exists, treat as
        // legacy and rewrite — better to write the canonical body than to
        // leave a soul stuck.
        let empty = "";
        let rewritten = identity_yml_rewrite_decision(empty, TEST_SHA)
            .expect("empty file must be rewritten");
        assert!(rewritten.contains(&format!("genesis_sha: {}", TEST_SHA)));
    }

    #[test]
    fn both_keys_present_is_a_noop() {
        // Defensive: if a file somehow has BOTH keys (manual edit by a
        // confused squaddie), trust the canonical one and leave it alone.
        // We don't want to thrash if both already exist.
        let both = format!(
            "identity: {sha}\n\
             genesis_sha: {sha}\n",
            sha = TEST_SHA
        );
        let decision = identity_yml_rewrite_decision(&both, TEST_SHA);
        assert!(decision.is_none(), "both-keys present should be no-op");
    }
}

#[cfg(test)]
mod repo_yml_header_tests {
    use super::*;

    fn tmp_repo(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
        let root = std::env::temp_dir().join(format!("gitlex-hdr-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(root.join(".lex")).unwrap();
        root
    }

    /// The heal prepends and preserves: every original byte survives below
    /// the header, in order, and a second run is a no-op (idempotent — this
    /// runs at every compaction, fleet-wide, forever).
    #[test]
    fn header_prepends_once_and_preserves_everything() {
        let root = tmp_repo("prepend");
        let body = "name: lUX\nagent_name: selkie\nagent_email: selkie@repolex.ai\n\noptional_kits:\n  - repolex-ai/git-lex-kit-copia\n";
        let path = root.join(".lex").join("repo.yml");
        fs::write(&path, body).unwrap();

        ensure_repo_yml_header(&root).unwrap();
        let once = fs::read_to_string(&path).unwrap();
        assert!(once.starts_with("# these values are set at init"), "header is first");
        assert!(once.contains("DO NOT EDIT"));
        assert!(once.ends_with(body), "every original byte preserved, in order");

        ensure_repo_yml_header(&root).unwrap();
        assert_eq!(once, fs::read_to_string(&path).unwrap(), "second run is a no-op");
        fs::remove_dir_all(&root).ok();
    }

    /// The header must not disturb the readers: the YAML struct still parses
    /// every field, and the graph emitter skips comment lines (verified
    /// against the real parser, not assumed).
    #[test]
    fn header_is_invisible_to_the_readers() {
        let root = tmp_repo("readers");
        fs::write(
            root.join(".lex").join("repo.yml"),
            "name: lUX\nkit: repolex-ai/git-lex-kit-soul\nagent_name: selkie\nagent_email: selkie@repolex.ai\n",
        ).unwrap();
        ensure_repo_yml_header(&root).unwrap();

        let yml = git_lex::RepoYml::load(&root);
        assert_eq!(yml.agent_name.as_deref(), Some("selkie"));
        assert_eq!(yml.agent_email.as_deref(), Some("selkie@repolex.ai"));
        assert_eq!(yml.name.as_deref(), Some("lUX"));
        // No comment line survives as a key in the scalar view.
        assert!(yml.scalar_fields().keys().all(|k| !k.starts_with('#')));
        fs::remove_dir_all(&root).ok();
    }

    /// An OLD header converges to the current text. This is the case the
    /// first cut got wrong: skip-if-present meant a repo that had taken
    /// version A of the header would keep it forever, and the very next
    /// edit (Rob cut a paragraph within the hour) would have reached only
    /// fresh repos.
    #[test]
    fn an_old_header_is_replaced_not_kept() {
        let root = tmp_repo("converge");
        let path = root.join(".lex").join("repo.yml");
        let stale = "# ─────\n# MANAGED BY git-lex — DO NOT EDIT.\n# some wording we since cut\n# ─────\n";
        let body = "name: lUX\nagent_name: selkie\n";
        fs::write(&path, format!("{stale}{body}")).unwrap();

        ensure_repo_yml_header(&root).unwrap();
        let out = fs::read_to_string(&path).unwrap();
        assert!(!out.contains("some wording we since cut"), "stale header gone");
        assert_eq!(out, format!("{REPO_YML_HEADER}{body}"), "current text, body intact");
        assert_eq!(out.to_lowercase().matches("managed by git-lex").count(), 1, "exactly one header");
        fs::remove_dir_all(&root).ok();
    }

    /// A leading comment that is NOT ours is never eaten — the strip is
    /// scoped to a header that names itself.
    #[test]
    fn a_squaddies_own_leading_comment_survives() {
        let root = tmp_repo("theirs");
        let path = root.join(".lex").join("repo.yml");
        fs::write(&path, "# my own note about this repo\nname: lUX\n").unwrap();

        ensure_repo_yml_header(&root).unwrap();
        let out = fs::read_to_string(&path).unwrap();
        assert!(out.contains("# my own note about this repo"), "their comment kept");
        assert!(out.contains("DO NOT EDIT"), "ours added above it");
        fs::remove_dir_all(&root).ok();
    }

    /// No repo.yml → no-op. The heal repairs a file, it never conjures one.
    #[test]
    fn missing_repo_yml_is_left_alone() {
        let root = tmp_repo("missing");
        ensure_repo_yml_header(&root).unwrap();
        assert!(!root.join(".lex").join("repo.yml").exists());
        fs::remove_dir_all(&root).ok();
    }
}
