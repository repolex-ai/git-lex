//! Git-history walker engine and sidecar cleanup helpers.
//!
//! This module is the foundation for two features:
//!
//! The one-graph walk engine (statement history), the git diff parsing
//! layer it rides on, and the git-aware orphan-sidecar cleanup used by the
//! pre-commit hook.

use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::process::Command;


// ════════════════════════════════════════════════════════════════════════════
// v1 write-gate: strict sidecar validation at save time
// ════════════════════════════════════════════════════════════════════════════

/// The closed operator vocabulary of the v1 sidecar format (spo format
/// spec §4). The WALKER stays vocabulary-tolerant — history legally carries
/// retired vocab, counted in DropAccounting — but the write-gate is strict:
/// nothing outside this set can be WRITTEN anymore. Adding an operator is a
/// spec revision, not a code change.
pub(crate) const SPO_OPERATORS_V1: &[&str] = &["hasValue", "linksTo"];

/// Validate one sidecar's full content against the v1 format spec. This is
/// the round-trip write-gate: it runs at save, AFTER extraction writes the
/// sidecars, using the same `splitn(3, " | ")` shape rule as the history
/// walker — so nothing can be written that history can't later read (the
/// enforcement brick the 3-month wrapped-line bug proved missing).
///
/// Returns (1-based line number, error) pairs; empty = valid.
///
/// Rules (spec §2, §4, §5 — all Rob-ruled 2026-07-30/08-01):
///   - `subject | operator | object`, splitn(3): exactly three fields
///   - no blank lines
///   - operator ∈ SPO_OPERATORS_V1 (closed vocabulary)
///   - object: no control characters (Unicode Cc — the standard property,
///     not a homegrown parser; format chars Cf stay legal)
///   - `hasValue` with empty object is LEGAL (present-but-empty field)
///   - `linksTo`: non-empty target; under Obsidian link semantics
///     (`obsidian_links` — repos stamped by init), a leading `/` is
///     rejected (retired form); legacy repos accept both forms until
///     their Phase-4 migration.
pub(crate) fn validate_sidecar_v1(content: &str, obsidian_links: bool) -> Vec<(usize, String)> {
    let mut errors = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let lineno = idx + 1;
        if line.trim().is_empty() {
            errors.push((lineno, "blank line (format is one triple per physical line)".to_string()));
            continue;
        }
        let fields: Vec<&str> = line.splitn(3, " | ").collect();
        if fields.len() != 3 {
            errors.push((lineno, format!(
                "malformed line {line:?} (expected `subject | operator | object`)"
            )));
            continue;
        }
        let (subject, operator, object) = (fields[0], fields[1], fields[2]);
        if subject.trim().is_empty() {
            errors.push((lineno, "empty subject".to_string()));
        }
        if !SPO_OPERATORS_V1.contains(&operator) {
            errors.push((lineno, format!(
                "operator {operator:?} not in the closed vocabulary {{{}}}",
                SPO_OPERATORS_V1.join(", ")
            )));
        }
        if let Some(c) = object.chars().find(|c| c.is_control()) {
            errors.push((lineno, format!(
                "control character U+{:04X} in object value", c as u32
            )));
        }
        if operator == "linksTo" {
            if object.trim().is_empty() {
                errors.push((lineno, "linksTo with an empty target".to_string()));
            } else if obsidian_links && object.starts_with('/') {
                errors.push((lineno, format!(
                    "linksTo target {object:?} has a leading slash — retired \
                     under Obsidian link semantics (spec §5); write the \
                     repo-root-relative path"
                )));
            }
            // Legacy (unstamped) repos: both forms accepted — under the
            // 2026-07-28 law the slash is SEMANTIC (bare = source-folder-
            // relative, `/` = repo-rooted). A blanket rejection here (tried
            // 2026-08-01) outlawed valid data.
        }
    }
    errors
}

// ════════════════════════════════════════════════════════════════════════════
// Data types
// ════════════════════════════════════════════════════════════════════════════

/// One commit as the history walk consumes it: sha, diff baseline, and the
/// touched sidecar paths (renames as pairs). There is deliberately NO
/// line-level diff detail here — the walk diffs full RESOLVED sidecar
/// content per side, so touched paths are the only diff input it needs.
/// (The old unified-diff parsing layer that used to live here mis-parsed
/// filenames containing spaces and swallowed git failures; NUL-separated
/// --name-status has neither problem.)
pub struct WalkCommit {
    /// Full commit SHA.
    pub sha: String,
    /// First parent, or the empty-tree SHA for a root commit.
    pub parent_sha: String,
    /// Sidecar paths added/modified/deleted/type-changed in this commit.
    pub touched: Vec<String>,
    /// (old_path, new_path) pairs from -M50% rename detection.
    pub renames: Vec<(String, String)>,
}

// ════════════════════════════════════════════════════════════════════════════
// Layer 1: git runner (thin wrappers around shelling out)
// ════════════════════════════════════════════════════════════════════════════

use git_lex::find_git_root;

/// Collect the walk inputs for a list of SHAs. Any git failure is an ERROR
/// for the whole walk: a commit whose diff can't be read must stop the
/// build, not silently contribute nothing (a corrupt object used to shrink
/// history with exit 0 — adversarial finding 1e).
pub(crate) fn collect_commits_from_shas(
    shas: &[String],
    horizon_start: Option<&str>,
) -> Result<Vec<WalkCommit>, String> {
    shas.iter()
        .map(|sha| {
            let mut c = build_commit(sha)?;
            // dev_history_horizon: the first walked commit diffs against
            // the EMPTY tree so the whole tree asserts as of the horizon.
            if horizon_start == Some(sha.as_str()) {
                c = rebuild_against_empty_tree(sha)?;
            }
            Ok(c)
        })
        .collect()
}

/// Build a WalkCommit whose baseline is the empty tree — every sidecar in
/// the commit's tree counts as touched (horizon-start semantics).
fn rebuild_against_empty_tree(sha: &str) -> Result<WalkCommit, String> {
    let diff_out = Command::new("git")
        .args([
            "diff-tree", "--no-commit-id", "--no-color", "--no-ext-diff",
            "--name-status", "-z", "-r", EMPTY_TREE_SHA, sha, "--",
            ".lex/extract/*.spo",
        ])
        .output()
        .map_err(|e| format!("git diff-tree {sha}: spawn failed: {e}"))?;
    if !diff_out.status.success() {
        return Err(format!(
            "git diff-tree (horizon baseline) {sha} failed ({}): {}",
            diff_out.status,
            String::from_utf8_lossy(&diff_out.stderr).trim()
        ));
    }
    let (touched, renames) =
        parse_name_status_z(&String::from_utf8_lossy(&diff_out.stdout));
    Ok(WalkCommit {
        sha: sha.to_string(),
        parent_sha: EMPTY_TREE_SHA.to_string(),
        touched,
        renames,
    })
}

/// Well-known magic SHA for the empty git tree. Used as the diff baseline
/// for root commits (commits with no parents) so the walker sees every
/// initial `.spo` line as an addition.
const EMPTY_TREE_SHA: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Build a `WalkCommit`: find the first parent, then ONE NUL-separated
/// `--name-status` diff for the touched sidecar set. `-M50%` keeps rename
/// detection (folder recases must pair old→new, not read as delete+create).
fn build_commit(sha: &str) -> Result<WalkCommit, String> {
    let parent_out = Command::new("git")
        .args(["rev-list", "--parents", "-n", "1", sha])
        .output()
        .map_err(|e| format!("git rev-list --parents {sha}: spawn failed: {e}"))?;
    if !parent_out.status.success() {
        return Err(format!(
            "git rev-list --parents {sha} failed ({}): {}",
            parent_out.status,
            String::from_utf8_lossy(&parent_out.stderr).trim()
        ));
    }
    let parent_line = String::from_utf8_lossy(&parent_out.stdout);
    let parent_fields: Vec<&str> = parent_line.trim().split_whitespace().collect();
    let base = if parent_fields.len() >= 2 {
        parent_fields[1].to_string()
    } else {
        EMPTY_TREE_SHA.to_string()
    };

    let diff_out = Command::new("git")
        .args([
            "diff-tree",
            "--no-commit-id",
            "--no-color",
            "--no-ext-diff",
            "--name-status",
            "-z",
            "-M50%",
            "-r",
            &base,
            sha,
            "--",
            ".lex/extract/*.spo",
        ])
        .output()
        .map_err(|e| format!("git diff-tree {sha}: spawn failed: {e}"))?;
    if !diff_out.status.success() {
        return Err(format!(
            "git diff-tree {base}..{sha} failed ({}): {}",
            diff_out.status,
            String::from_utf8_lossy(&diff_out.stderr).trim()
        ));
    }

    let (touched, renames) =
        parse_name_status_z(&String::from_utf8_lossy(&diff_out.stdout));
    Ok(WalkCommit { sha: sha.to_string(), parent_sha: base, touched, renames })
}

/// Parse `--name-status -z` output into (touched paths, rename pairs).
///
/// The `-z` record format: `<status>\0<path>\0` for single-path statuses
/// (A/M/D/T), `<status>\0<old>\0<new>\0` for two-path statuses (R<score>,
/// C<score>). Paths are RAW — no C-style quoting, so filenames with spaces,
/// quotes, or unicode arrive intact (the old human-format header parsing
/// mangled space-bearing names and silently dropped their sidecars from
/// history — adversarial finding 1c).
///
/// Status handling: A/M/D/T → touched (the resolved-set diff decides what
/// actually changed); R → rename pair; C → the NEW path is touched (a copy
/// leaves the old side unchanged).
pub fn parse_name_status_z(raw: &str) -> (Vec<String>, Vec<(String, String)>) {
    let mut touched = Vec::new();
    let mut renames = Vec::new();
    let fields: Vec<&str> = raw.split('\0').filter(|s| !s.is_empty()).collect();
    let mut i = 0;
    while i < fields.len() {
        match fields[i].chars().next() {
            Some('R') => {
                if let (Some(old), Some(new)) = (fields.get(i + 1), fields.get(i + 2)) {
                    renames.push((old.to_string(), new.to_string()));
                }
                i += 3;
            }
            Some('C') => {
                if let Some(new) = fields.get(i + 2) {
                    touched.push(new.to_string());
                }
                i += 3;
            }
            _ => {
                // A/M/D/T (and any future single-path status): one path.
                if let Some(p) = fields.get(i + 1) {
                    touched.push(p.to_string());
                }
                i += 2;
            }
        }
    }
    (touched, renames)
}

// ════════════════════════════════════════════════════════════════════════════
// Orphan cleanup — git-aware, used by the pre-commit hook
// ════════════════════════════════════════════════════════════════════════════
//
// Phase 3 (2026-04-11): replaces the old `cleanup_orphaned_sidecars()` that
// lived in main.rs. The old version walked .lex/extract/ and called
// `Path::exists()` on the reconstructed source path, which broke on macOS
// APFS because it's case-insensitive by default: after a rename like
// `friend/ → Friend/`, `Path::new("friend/1ux.md").exists()` returns TRUE
// even when the actual file is at `Friend/1ux.md`. Orphans silently
// survived. On the lowercase → capital class proclamation, every agent
// would have generated ghost triples fleet-wide.
//
// The new approach asks git, not the filesystem, via `git diff --cached
// --name-status -M50%`. Git gives us exact casing and a structured change
// set that distinguishes deletes from renames — so we can:
//
//   - delete stale .spo mirrors when an .md is deleted
//   - `git mv` stale .spo mirrors to the new path when an .md is renamed,
//     preserving their content (important for future `haiku.spo` subagent
//     output that is expensive to regenerate)
//
// Cleanup runs from the pre-commit hook (via `cmd_extract`) so that the
// .spo mirror moves/deletes land in the same commit as the .md change
// itself. The commit is atomic from git's perspective: source file and
// sidecar stay in lockstep across the whole history.

/// A record of what cleanup did in one invocation. Kept as counts + a
/// details field so the reporter in `cmd_extract` can print a one-line
/// summary and verbose logs when needed.
#[derive(Debug, Default)]
pub struct CleanupReport {
    /// .spo mirror files deleted because their source .md was deleted.
    pub deleted: Vec<String>,
    /// .spo mirror files moved because their source .md was renamed.
    /// Each entry is (old_spo_path, new_spo_path).
    pub renamed: Vec<(String, String)>,
    /// Non-fatal errors encountered — things that didn't stop the walk but
    /// should be visible to the agent. E.g. a stale .spo that couldn't be
    /// removed because git rm returned an error.
    pub errors: Vec<String>,
}

impl CleanupReport {
    pub fn is_empty(&self) -> bool {
        self.deleted.is_empty() && self.renamed.is_empty() && self.errors.is_empty()
    }

    pub fn summary(&self) -> String {
        format!(
            "{} deleted, {} renamed, {} errors",
            self.deleted.len(),
            self.renamed.len(),
            self.errors.len()
        )
    }
}

/// Known extractor suffixes on `.spo` sidecar files. Each source `.md`
/// document may have multiple sidecars — one per extractor — all living
/// under `.lex/extract/<relpath>.<extractor>.spo`. When cleanup handles
/// a deleted or renamed .md, it must handle every sidecar for that file,
/// regardless of which extractor wrote it.
///
/// Current extractors:
///   - `fm`   : frontmatter (YAML header → triples, mainline)
///   - `md`   : markdown links (tree-sitter walker, mentions + wikilinks)
///   - `cc`   : claude-code JSONL sessions (claude-export kit)
///
/// Future extractors (not yet implemented but named in the spec):
///   - `gliner` : entity mentions via the gliner2 Rust crate
///   - `haiku`  : LLM-generated haiku annotations, subagent-driven
///
/// Add new suffixes here when new extractors ship. Cleanup will glob them
/// automatically.
const SPO_EXTRACTOR_SUFFIXES: &[&str] = &["fm", "md", "cc", "gliner", "haiku"];

/// Ask git for the staged-but-not-yet-committed change set on extractable
/// source files (`.md` AND `.jsonl` — every extension an extractor consumes),
/// filtered to the tracked content tree (no `.lex/**`). Returns the raw
/// diff status output, which `parse_staged_md_changes` then parses.
///
/// A git FAILURE is an error, never "nothing staged": conflating the two
/// silently skips orphan cleanup, which is exactly the ghost-triple scenario
/// this machinery exists to prevent (review finding A6).
///
/// Uses `diff --cached` (index vs HEAD) because this function is called
/// from the pre-commit hook, where changes have been staged by the hook
/// caller (via `git add` or `git lex save`'s explicit `git add -A`) but
/// not yet committed.
///
/// `-M50%` turns on rename detection at 50% similarity — same threshold
/// the diff-tree walker uses, for consistency.
fn git_staged_md_changes(root: &std::path::Path) -> Result<String, String> {
    let out = Command::new("git")
        .current_dir(root)
        .args([
            "diff",
            "--cached",
            "--name-status",
            "-M50%",
            "-z",
            "--",
            "*.md",
            "*.jsonl",
            ":!.lex/",
        ])
        .output()
        .map_err(|e| format!("git diff --cached spawn failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git diff --cached failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // `-z` gives NUL-separated records; we want lossy UTF-8 because paths
    // might not be strict UTF-8 but we'll still see them correctly.
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse the `git diff --cached --name-status -M -z` output into a pair
/// of lists: deleted .md paths and (old, new) rename pairs.
///
/// The `-z` NUL-separated format for `--name-status` is irregular:
///   - For `A`, `M`, `D`, `T` (single-path statuses): `<status>\0<path>\0`
///   - For `R<score>`, `C<score>` (two-path statuses): `<status>\0<old>\0<new>\0`
///
/// So we can't just split on NUL — we need to read the status and decide
/// how many subsequent fields to consume. Pure function, unit-tested below.
pub fn parse_staged_md_changes(raw: &str) -> (Vec<String>, Vec<(String, String)>) {
    let mut deleted = Vec::new();
    let mut renamed = Vec::new();

    // Split on NUL. `-z` separates every field with NUL; the trailing
    // NUL on the last record produces an empty string we filter out.
    let fields: Vec<&str> = raw.split('\0').filter(|s| !s.is_empty()).collect();
    let mut i = 0;
    while i < fields.len() {
        let status = fields[i];
        let first_char = status.chars().next().unwrap_or(' ');
        match first_char {
            'R' | 'C' => {
                // Two-path status: status, old, new
                if i + 2 < fields.len() {
                    let old = fields[i + 1].to_string();
                    let new = fields[i + 2].to_string();
                    if first_char == 'R' {
                        renamed.push((old, new));
                    }
                    // Copies (C) are not treated as renames — the source
                    // file still exists, so its .spo doesn't need moving.
                    i += 3;
                } else {
                    break;
                }
            }
            'D' => {
                // Deletion: status, path
                if i + 1 < fields.len() {
                    deleted.push(fields[i + 1].to_string());
                    i += 2;
                } else {
                    break;
                }
            }
            _ => {
                // A, M, T, U: single-path statuses we don't care about for
                // cleanup. Advance past the path.
                if i + 1 < fields.len() {
                    i += 2;
                } else {
                    break;
                }
            }
        }
    }

    (deleted, renamed)
}

/// For a source .md path like `friend/1ux.md`, return all the sidecar
/// paths under `.lex/extract/` that correspond to it. Checks every known
/// extractor suffix and returns the ones that are currently TRACKED BY
/// GIT (in the index).
///
/// Uses the git index rather than `Path::exists()` to handle macOS APFS
/// case-insensitivity correctly — on APFS, `Path::new("foo/bar")` and
/// `Path::new("Foo/bar")` can resolve to the same inode, but git's index
/// tracks each path with exact casing.
///
/// Returns paths relative to the repo root, suitable for passing to
/// `git rm` / `git mv` (both of which accept repo-relative paths when
/// run from the repo root).
fn sidecar_paths_for_md(root: &std::path::Path, md_path: &str) -> Vec<String> {
    let mut out = Vec::new();
    for suffix in SPO_EXTRACTOR_SUFFIXES {
        let rel = format!(".lex/extract/{}.{}.spo", md_path, suffix);
        if git_path_is_tracked(root, &rel) {
            out.push(rel);
        }
    }
    out
}

/// Ask git whether a given path is currently tracked in the index,
/// with exact case sensitivity. Runs `git ls-files --error-unmatch -- <path>`
/// and treats a successful exit as "tracked".
///
/// Why not `Path::exists()`? Because on macOS APFS (case-insensitive by
/// default), the filesystem answer is wrong for case-only rename cases.
/// Git's index is always case-exact, so asking git gives us the truth.
fn git_path_is_tracked(root: &std::path::Path, path: &str) -> bool {
    Command::new("git")
        .current_dir(root)
        .args(["ls-files", "--error-unmatch", "--", path])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run `git rm -f <path>` — used to stage the deletion of a stale .spo
/// mirror. We use `-f` because the file may already be deleted from the
/// working tree (if the agent manually cleaned it up) but still tracked
/// in the index; `git rm -f` handles both cases.
fn git_rm(root: &std::path::Path, path: &str) -> Result<(), String> {
    let out = Command::new("git")
        .current_dir(root)
        .args(["rm", "-f", "--", path])
        .output()
        .map_err(|e| format!("git rm failed to spawn: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "git rm {} failed: {}",
            path,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Run `git mv <old> <new>`, creating the destination directory if needed.
/// Used to move a .spo mirror from its old path to the new one when the
/// source .md is renamed.
fn git_mv(root: &std::path::Path, old: &str, new: &str) -> Result<(), String> {
    // Ensure the destination parent directory exists — git mv doesn't
    // auto-create intermediate dirs.
    if let Some(parent) = root.join(new).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).ok();
        }
    }
    let out = Command::new("git")
        .current_dir(root)
        .args(["mv", "--", old, new])
        .output()
        .map_err(|e| format!("git mv failed to spawn: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "git mv {} -> {} failed: {}",
            old,
            new,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Clean up .spo sidecars for .md files that are being deleted or renamed
/// in the currently-staged commit. This is the Phase 3 replacement for
/// `cleanup_orphaned_sidecars()` — it asks git for the change set instead
/// of walking the filesystem, which fixes the APFS case-insensitivity
/// bug and adds rename-as-move support.
///
/// Called from the pre-commit hook (via `cmd_extract`) so the .spo moves
/// and deletes are staged into the same commit as the .md change itself.
/// The commit is atomic: source and sidecars stay in lockstep.
///
/// Returns a `CleanupReport` the caller can use for user-facing logging.
/// Non-fatal errors (e.g. a `git mv` that fails because the destination
/// already exists) are recorded in `report.errors` but don't abort the
/// walk — cleanup is best-effort and the sanity check for "did this do
/// anything weird?" happens at the CleanupReport level.
pub fn cleanup_sidecars_for_staged_changes() -> CleanupReport {
    let mut report = CleanupReport::default();

    let root = match find_git_root() {
        Some(r) => r,
        None => {
            report.errors.push("not in a git repo".to_string());
            return report;
        }
    };

    // Every git call runs with current_dir(root) — no process-global cwd
    // mutation (the old save/cd/restore dance was fragile and leaked repo-
    // relative behavior into every other subprocess while active).
    let raw = match git_staged_md_changes(&root) {
        Ok(r) => r,
        Err(e) => {
            // A failed query is NOT "nothing staged" — skipping cleanup on
            // it would leave orphan sidecars whose facts live forever. The
            // caller fails the commit on any report error.
            report.errors.push(format!(
                "staged-change query failed — cleanup skipped, orphan sidecars may remain: {e}"
            ));
            return report;
        }
    };

    let (deleted_mds, renamed_mds) = parse_staged_md_changes(&raw);

    for md_path in &deleted_mds {
        for sidecar in sidecar_paths_for_md(&root, md_path) {
            match git_rm(&root, &sidecar) {
                Ok(()) => report.deleted.push(sidecar),
                Err(e) => report.errors.push(e),
            }
        }
        // The jsonl extractor also keeps a `.meta` bookkeeping file next to
        // its sidecar; a deleted source must take it along.
        let meta = format!(".lex/extract/{}.meta", md_path);
        if git_path_is_tracked(&root, &meta) {
            match git_rm(&root, &meta) {
                Ok(()) => report.deleted.push(meta),
                Err(e) => report.errors.push(e),
            }
        }
    }

    for (old_md, new_md) in &renamed_mds {
        // For each extractor, compute the old sidecar path (derived from
        // the OLD md path) and the new sidecar path (derived from the
        // NEW md path). If the old sidecar is tracked in the index, git
        // mv it. Otherwise skip — the next extract pass will regenerate
        // sidecars under the new path naturally.
        //
        // We check "is this path in the index?" via `git ls-files`
        // instead of `Path::exists()` because on macOS APFS (case-
        // insensitive default), a case-only rename like friend/ → Friend/
        // produces a situation where the old sidecar at
        // `.lex/extract/friend/1ux.md.fm.spo` and the proposed new path
        // `.lex/extract/Friend/1ux.md.fm.spo` resolve to the same inode.
        // `Path::exists()` returns true for both. Git's index always
        // tracks paths with exact casing, so asking git gives us the
        // correct answer.
        for suffix in SPO_EXTRACTOR_SUFFIXES {
            let old_sidecar = format!(".lex/extract/{}.{}.spo", old_md, suffix);
            let new_sidecar = format!(".lex/extract/{}.{}.spo", new_md, suffix);
            if !git_path_is_tracked(&root, &old_sidecar) {
                continue;
            }
            // Destination ALREADY TRACKED IN THE INDEX (separately from
            // old_sidecar): a prior extract pass — typically a dry-run
            // before this save — already wrote fresh sidecars at the new
            // path. The rename's intent ("content lives at the new path")
            // is satisfied; the source is simply stale — delete it.
            // Erroring here instead hard-failed every class-move save
            // that followed a dry-run (tr1p's 0.9.0 convergence find).
            // A case-only rename resolving to the same inode on APFS is
            // excluded by the path-inequality guard: git's index tracks
            // exact casing, so same-inode ≠ same tracked path.
            if git_path_is_tracked(&root, &new_sidecar) && new_sidecar != old_sidecar {
                match git_rm(&root, &old_sidecar) {
                    Ok(()) => report.deleted.push(old_sidecar),
                    Err(e) => report.errors.push(e),
                }
                continue;
            }
            match git_mv(&root, &old_sidecar, &new_sidecar) {
                Ok(()) => report.renamed.push((old_sidecar, new_sidecar)),
                Err(e) => report.errors.push(e),
            }
        }
        // Move the jsonl extractor's `.meta` bookkeeping file along with a
        // renamed source (same tracked-in-index rules as the sidecars).
        let old_meta = format!(".lex/extract/{}.meta", old_md);
        let new_meta = format!(".lex/extract/{}.meta", new_md);
        if git_path_is_tracked(&root, &old_meta) {
            if git_path_is_tracked(&root, &new_meta) && new_meta != old_meta {
                // Same rule as the sidecars above: tracked destination
                // means the move already happened — the source is stale,
                // and silently skipping it left it tracked forever.
                match git_rm(&root, &old_meta) {
                    Ok(()) => report.deleted.push(old_meta),
                    Err(e) => report.errors.push(e),
                }
            } else {
                match git_mv(&root, &old_meta, &new_meta) {
                    Ok(()) => report.renamed.push((old_meta, new_meta)),
                    Err(e) => report.errors.push(e),
                }
            }
        }
    }

    report
}

// ════════════════════════════════════════════════════════════════════════════
// Source document derivation
// ════════════════════════════════════════════════════════════════════════════

/// Strip the extractor suffix from a sidecar relative path, returning the
/// source document path. Mirrors the extractor's cleanup logic in
/// src/main.rs:5442 so the canonical URI derivation stays consistent with
/// the extractor's conventions.
///
/// Known suffixes (ordered longest-first so we don't eat `.spo` when the
/// real suffix is `.fm.spo`):
///   .fm.spo    — frontmatter extractor
///   .md.spo    — markdown link extractor
///   .cc.spo    — claude-code JSONL extractor
///   (future)   — .gliner.spo, .haiku.spo, ...
///
/// Unknown `.spo` suffixes return `None` rather than producing a garbage
/// source path.
///
/// Suffix knowledge lives in `SPO_EXTRACTOR_SUFFIXES` alone — a new extractor
/// added there is automatically recognized here. The `.lex/extract/` prefix
/// is REQUIRED: only paths under it are sidecars (the diff-tree pathspec
/// guarantees it), and a prefix-less path is not a sidecar we know how to
/// attribute.
pub(crate) fn derive_source_document(sidecar_rel_path: &str) -> Option<String> {
    let after_extract = sidecar_rel_path.strip_prefix(".lex/extract/")?;
    for suffix in SPO_EXTRACTOR_SUFFIXES {
        let full = format!(".{}.spo", suffix);
        if let Some(base) = after_extract.strip_suffix(full.as_str()) {
            return Some(base.to_string());
        }
    }
    None
}

/// Read a sidecar file's content at a specific git commit.
/// Returns the non-empty, non-comment lines (the SPO lines).
///
/// "Path absent at this commit" is a NORMAL outcome — the added/deleted side
/// of a diff resolves against a commit where the file doesn't exist — and
/// returns `Ok(empty)`. Every OTHER git failure is an ERROR: treating it as
/// absence would let a transient failure fabricate history (an empty old side
/// reads as "everything was added", an empty new side as "everything was
/// removed" — assert/retract events manufactured into the one graph).
fn read_sidecar_at_commit(sha: &str, sidecar_path: &str) -> Result<Vec<String>, String> {
    let spec = format!("{}:{}", sha, sidecar_path);
    let out = Command::new("git")
        .args(["show", &spec])
        .output()
        .map_err(|e| format!("git show {spec}: spawn failed: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| l.to_string())
            .collect());
    }
    // `git show` failed. Absence is the only failure we accept as empty;
    // disambiguate with ls-tree: exit 0 + empty output = path not in that
    // tree, anything else = a real git failure that must not read as empty.
    let probe = Command::new("git")
        // --full-tree: pathspecs are otherwise cwd-relative, and this probe
        // must mean the same thing no matter where sync was invoked from
        // (a cwd-sensitive probe reclassifies real git failures as
        // "verified absent" — the exact fabrication this fn prevents).
        .args(["ls-tree", "--full-tree", sha, "--", sidecar_path])
        .output()
        .map_err(|e| format!("git ls-tree {sha} -- {sidecar_path}: spawn failed: {e}"))?;
    if probe.status.success() && probe.stdout.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(Vec::new());
    }
    Err(format!(
        "git show {spec} failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

// ════════════════════════════════════════════════════════════════════════════
// The one graph — the PRODUCTION history model
// ════════════════════════════════════════════════════════════════════════════
//
// Every save's fact changes are recorded as events in a single persistent
// graph (`LexHistoryGraph`), one event per statement per direction:
//
//     <event> rdf:reifies          <<( s p o )>> .
//     <event> git-lex:assertedIn   <git2:Commit/SHA> .   (fact became true)
//     <event> git-lex:retractedIn  <git2:Commit/SHA> .   (fact stopped being true)
//
// `git lex sync` runs the walk engine below incrementally; a full rebuild
// (delete .lex/_ignore/oxigraph, sync) re-derives the whole graph from
// commit history.
// The graph's BASE LAYER is current state (net-asserted facts as plain
// triples), maintained by the walk engine and copied out as NamedGraph/now
// each sync. Events join to their commit's author/date via the git2: layer.
//
// Every `.spo` line resolves through the SAME `emit_spo_line_nquads` the
// query surface uses — one resolver, no drift between history and query.
// Predicates (assertedIn/retractedIn, SpoEvent) are DECLARED in git-lex.ttl.

/// The one graph's IRI — Rob-ruled 2026-07-21, class authored in git-lex.ttl
/// v0.7 (`git-lex:LexHistoryGraph ⊑ git-lex:NamedGraph`). A bare per-store
/// singleton: the SAME IRI in every git-lex repo, so documented/kit-shipped
/// queries work verbatim everywhere. (A genesisSha-tailed variant was
/// considered and backed out — which-repo provenance is a FACT on the Repo
/// node, not something IRIs carry.) Class-in-path per the universal law; NOT
/// under NamedGraph/ like the machinery graphs.
pub(crate) const LEXHISTORY_GRAPH_IRI: &str = "https://repolex.ai/git-lex/LexHistoryGraph";

/// The statement-lifecycle predicates, declared in git-lex.ttl v0.5+
/// (kit-base 9e6f4bf): domain git-lex:SpoEvent, range git2:Commit.
const ONEGRAPH_ASSERTED_IN: &str = "https://repolex.ai/ontology/git-lex/assertedIn";
const ONEGRAPH_RETRACTED_IN: &str = "https://repolex.ai/ontology/git-lex/retractedIn";

/// Build the one-graph N-Quads for a single resolved triple event.
///
/// Reuses the real emitter's N-Quad output verbatim (parse-then-rewrap), and
/// emits the "Option B" one-graph shape — the base fact asserted STANDALONE plus
/// a reified triple-term carrying the commit event. This matches the agreed
/// Turtle form:
///
///     s p o .                                        # base fact, asserted standalone
///     <reifier> rdf:reifies         <<( s p o )>> .
///     <reifier> git-lex:assertedIn  <Commit/SHA> .   (op == '+')
///     <reifier> git-lex:retractedIn <Commit/SHA> .   (op == '-')
///
/// The standalone base fact is what makes "what is true now" a PLAIN triple
/// query (`?s ?p ?o`) instead of forcing every reader through the reification.
/// Because the store is set-semantic, a fact re-added after removal collapses to
/// one base triple — but each add/remove still gets its own reified event, so
/// the temporal history is complete and derivable.
///
/// Events carry NO base fact — the base (plain-triple) layer is maintained
/// separately by the walk engine as true final state (insert on net-assert,
/// remove on net-retract).
///
/// The reifier IRI is content-addressed over `(op, commit, s, p, o)` — a
/// deterministic UID, NOT a dedup safety net (a re-emit of the same event is a
/// walk bug we'd want to surface, not silently swallow). The commit object is
/// the existing `git:Commit` IRI so facts join to their commit's author/date.
///
/// `triple_nq` is one assertion line from `emit_spo_line_nquads`, in N-Quad
/// form `<S> <P> O <G> .`. Returns None if it can't parse a complete S/P/O.
pub fn onegraph_event(
    triple_nq: &str,
    op: char,
    commit_sha: &str,
    one_graph: &str,
) -> Option<Vec<String>> {
    // Isolate `<S> <P> O` by stripping the trailing graph + period, same as
    // history_annotation.
    let trimmed = triple_nq.trim_end_matches('.').trim();
    let trimmed = trimmed.rsplit_once(' ').map(|(rest, _)| rest)?.trim();
    let (s, rest) = take_term(trimmed)?;
    let (p, rest) = take_term(rest.trim())?;
    let o = rest.trim().to_string();
    if s.is_empty() || p.is_empty() || o.is_empty() {
        return None;
    }

    // Content-addressed reifier IRI. Op is part of the key so an assert and a
    // later retract of the same (s,p,o) get DISTINCT reifiers (they must, or the
    // retract would overwrite the assert under set semantics).
    let key = format!("{}|{}|{}|{}|{}", op, commit_sha, s, p, o);
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    // The SpoEvent node (git-lex.ttl, Rob-ruled 2026-07-21): a first-class
    // Thing — one temporal event in a statement's lifecycle. Its IRI derives
    // under the universal law (t-box minus `ontology/`), and its id is the
    // event's composite identity (triple + commit + direction), encoded.
    let event = format!("<https://repolex.ai/git-lex/SpoEvent/{}>", &hash[..16]);

    let event_pred = if op == '+' {
        ONEGRAPH_ASSERTED_IN
    } else {
        ONEGRAPH_RETRACTED_IN
    };
    let commit_uri = format!("<{}>", crate::git2_nquads::git2_uri(&format!("Commit/{}", commit_sha)));

    Some(vec![
        // The event carries NO base fact: events are pure history. The base
        // (plain-triple) layer is the MATERIALIZED NOW — maintained by the
        // walk engine as true final state (insert on net-assert, REMOVE on
        // net-retract), per the ruled contract: "'now' is a view … of the
        // latest assertions that have not been retracted." An unconditional
        // base fact here was the defect that let retracted values linger as
        // plain triples.
        // 1) the event's class (git-lex:SpoEvent — machine-derived, validated
        //    by the emitter's integrity checks, not the save-time gate)
        format!(
            "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://repolex.ai/ontology/git-lex/SpoEvent> {} .",
            event, one_graph
        ),
        // 2) which statement this event chronicles: <event> rdf:reifies <<( s p o )>>
        format!(
            "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> <<( {} {} {} )>> {} .",
            event, s, p, o, one_graph
        ),
        // 3) the commit event: assertedIn XOR retractedIn (never both)
        format!(
            "{} <{}> {} {} .",
            event, event_pred, commit_uri, one_graph
        ),
    ])
}

/// Walk pre-collected commits and build the one graph into `store`. This is
/// the PRODUCTION history engine — `git lex sync` runs it incrementally
/// (`clear_first = false`, appending events for commits newer than the
/// store's resume point) and the full-rebuild command runs it with
/// `clear_first = true`. It resolves every `.spo` line through the same
/// `emit_spo_line_nquads` the query surface uses.
///
/// Returns `(events_seen, events_emitted)` for the summary line, or an error
/// if git or the store failed anywhere — a partial walk must never report
/// success, because the one graph is the system of record.
#[allow(clippy::too_many_arguments)]
pub(crate) fn onegraph_walk_engine(
    commits: &[WalkCommit],
    store: &oxigraph::store::Store,
    one_graph: &str,
    ctx: &crate::nquad::ResolverContext,
    show_progress: bool,
    clear_first: bool,
) -> Result<(usize, usize), String> {
    let total = commits.len();
    let mut nq_buffer = String::new();
    let mut events_seen = 0usize;
    let mut events_emitted = 0usize;

    // ─── Resolved-set diffing (BUG 1 fix, Rob-ruled; the contract is in the
    // SpoEvent class comment: "diffed as RESOLVED sets per commit") ───
    //
    // Per commit we resolve the FULL old and new content of every touched
    // sidecar through the real emitter, and diff the RESOLVED TRIPLE SETS —
    // never raw .spo lines. Events exist only for triples that genuinely
    // entered or left the resolved world in this commit. This kills, by
    // construction, every raw-line artifact the triage documented:
    //   - pure file moves (stable Thing IRI): identical sets → ZERO events;
    //   - prefix recases (soul.friend. → soul.Friend.) resolving to the same
    //     triples: identical sets → ZERO events (the m4rq no-op churn);
    //   - value reorders / duplicate values: set semantics → ZERO events;
    //   - IRI-changing moves (type/case changes): honest retract-at-old +
    //     assert-at-new (different Things by design — the m4rq type ruling).
    // No rename special-casing: renames only pair old→new paths for content
    // fetching; the sets carry all the semantics.

    // Resolve one sidecar's full content at a commit into the set of its
    // resolved triple-quad lines (graph term constant, so line-set semantics
    // == triple-set semantics). Also counts lines in / lines dropped by the
    // resolver (a line yielding zero triples) — the completeness accounting
    // foundation (BUG 4).
    // Accounting (BUG 4): every sidecar line either yields triples, is
    // counted (`resolver_other`, `unknown_suffix`), or HARD-FAILS the walk
    // (malformed shape / empty object). The walker knows ONE sidecar format;
    // a line violating it is either a real bug (fix it) or pre-standard
    // dev-era data that `dev_history_horizon` in .lex/repo.yml should be
    // fencing. Nothing vanishes silently, and nothing is tolerated quietly.
    // The shared emitter (`emit_spo_line_nquads`, also serving the now view
    // + `git lex query`) is deliberately untouched; lines it drops for its
    // own reasons land in `resolver_other`.
    #[derive(Default)]
    struct DropAccounting {
        lines_in: usize,
        empty_object: usize,         // `key | hasValue | ` — empty value, no fact
        resolver_other: usize,       // dropped inside the shared emitter
        unknown_suffix: usize,       // sidecar with an undeclared extractor suffix
        resolver_errors: u32,        // errors reported by the shared emitter
    }
    let mut acct = DropAccounting::default();
    // Unknown-suffix sidecars warn once per path (the walk visits the same
    // path once per touching commit — repeating the warning is noise).
    let mut warned_unknown: HashSet<String> = HashSet::new();

    // Net base-layer effect per triple across this walk (last op wins).
    let mut base_final: HashMap<String, char> = HashMap::new();

    let resolve_sidecar_at = |commit: &str,
                                  sidecar_path: &str,
                                  acct: &mut DropAccounting,
                                  warned_unknown: &mut HashSet<String>|
     -> Result<HashSet<String>, String> {
        // Unknown extractor suffix: counted and warned, never silent (the
        // BUG-4 contract). The diff-tree pathspec matches ALL
        // `.lex/extract/**.spo`, so a sidecar from an extractor this binary
        // doesn't know contributes nothing — that must be visible.
        let Some(relpath_str) = derive_source_document(sidecar_path) else {
            acct.unknown_suffix += 1;
            if warned_unknown.insert(sidecar_path.to_string()) {
                eprintln!(
                    "  one-graph: sidecar with unknown extractor suffix NOT walked: {sidecar_path} (known: {})",
                    SPO_EXTRACTOR_SUFFIXES.join(", ")
                );
            }
            return Ok(HashSet::new());
        };
        let lines = read_sidecar_at_commit(commit, sidecar_path)?;
        acct.lines_in += lines.len();
        let mut triples: HashSet<String> = HashSet::new();
        let mut emitted_types: HashSet<String> = HashSet::new();
        // Both plane anchors, derived from the FULL sidecar at this commit
        // (identity model re-anchor). The anchor facts (File type, Thing
        // type, fileId edge) join the resolved set so they diff temporally
        // like every other fact — a file move is exactly one fileId
        // retract+assert pair, nothing else. Warnings stay quiet here: the
        // walk revisits every commit and the save path already warned.
        let subjects = crate::nquad::derive_file_subjects(
            &lines,
            &relpath_str,
            &ctx.declared_props,
            &ctx.obj_props,
            &ctx.kit_namespaces,
            false,
        );
        {
            let mut anchor_buf = String::new();
            crate::nquad::emit_file_anchor_nquads(
                &subjects, &ctx.kit_namespaces, one_graph, &mut emitted_types, &mut anchor_buf,
            );
            for t in anchor_buf.lines().filter(|l| !l.trim().is_empty()) {
                triples.insert(t.to_string());
            }
        }
        for line in &lines {
            // Shape check — HARD error. The walker knows one format:
            // `subject | predicate | object`.
            // splitn(3): MUST match the emitter's split (nquad.rs) — a
            // value containing " | " is one value, not extra fields.
            let fields: Vec<&str> = line.splitn(3, " | ").collect();
            if fields.len() != 3 {
                return Err(format!(
                    "malformed sidecar line in {sidecar_path}: {line:?} \
                     (expected `subject | predicate | object`). \
                     If this file exists in your CURRENT working tree, the damage \
                     is live and must be repaired there: edit the source document \
                     trivially, run `git lex save` (regenerates its sidecar), then \
                     `rm -rf .lex/_ignore/oxigraph` and re-run `git lex sync`. \
                     If the line is only in HISTORY (dev-era data), fence it with \
                     `dev_history_horizon:` in .lex/repo.yml set to the day after \
                     this commit. Otherwise this is a bug — report it. \
                     (Known dev-era damage signature: a value hard-wrapped across \
                     two physical lines — the fragment above may be the tail of \
                     the previous line.)"
                ));
            }
            // Empty object is DEFINED format semantics, not damage: the
            // extractor writes `key | hasValue | ` for a frontmatter field
            // that is present but empty, and an empty value asserts no fact.
            // Same behavior as the now-view emitter. Skipped, counted.
            if fields[2].trim().is_empty() {
                acct.empty_object += 1;
                continue;
            }
            let mut emit_buf = String::new();
            // Emitter errors are COUNTED (a line can yield some triples AND
            // errors — e.g. one rejected value among several); the now path
            // counts the same errors, so the walk must too.
            acct.resolver_errors += crate::nquad::emit_spo_line_nquads(
                line, &subjects, one_graph, &relpath_str,
                &ctx.path_index, &ctx.obj_props,
                &ctx.prop_datatypes, &ctx.declared_props,
                &ctx.kit_namespaces, &ctx.ref_ranges, &ctx.deprecated_props,
                ctx.obsidian_links,
                &mut emitted_types, &mut emit_buf,
            );
            let mut any = false;
            for triple_nq in emit_buf.lines().filter(|l| !l.trim().is_empty()) {
                triples.insert(triple_nq.to_string());
                any = true;
            }
            if !any {
                acct.resolver_other += 1;
            }
        }
        Ok(triples)
    };

    for (ci, c) in commits.iter().enumerate() {
        if show_progress && total > 0 {
            if ci == 0 { eprint!("  one-graph: walking {} commit(s) ", total); }
            if (ci + 1) % 10 == 0 || ci == total - 1 {
                eprint!(".");
                let _ = std::io::stderr().flush();
            }
        }

        // Touched sidecars, old side vs new side. Renames pair old→new;
        // everything else appears under the same path on both sides (a path
        // absent at a commit resolves to a verified-empty set — see
        // read_sidecar_at_commit: absence is checked, never assumed from a
        // failed `git show`).
        let mut old_side: HashSet<&str> = HashSet::new();
        let mut new_side: HashSet<&str> = HashSet::new();
        for p in &c.touched {
            old_side.insert(p.as_str());
            new_side.insert(p.as_str());
        }
        for (old_p, new_p) in &c.renames {
            old_side.insert(old_p.as_str());
            new_side.insert(new_p.as_str());
        }

        let mut old_triples: HashSet<String> = HashSet::new();
        let mut new_triples: HashSet<String> = HashSet::new();
        for path in &old_side {
            old_triples.extend(
                resolve_sidecar_at(&c.parent_sha, path, &mut acct, &mut warned_unknown)
                    .map_err(|e| format!("commit {} (old side): {e}", c.sha))?,
            );
        }
        for path in &new_side {
            new_triples.extend(
                resolve_sidecar_at(&c.sha, path, &mut acct, &mut warned_unknown)
                    .map_err(|e| format!("commit {} (new side): {e}", c.sha))?,
            );
        }

        // The diff of resolved worlds IS the event stream for this commit.
        // base_final tracks each touched triple's NET state across this walk
        // (commits are processed oldest→newest, so the last op wins) — it
        // becomes the base-layer mutation set after the walk.
        for line in old_triples.difference(&new_triples) {
            events_seen += 1;
            if let Some(quads) = onegraph_event(line, '-', &c.sha, one_graph) {
                for q in quads { nq_buffer.push_str(&q); nq_buffer.push('\n'); }
                events_emitted += 1;
                base_final.insert(line.clone(), '-');
            }
        }
        for line in new_triples.difference(&old_triples) {
            events_seen += 1;
            if let Some(quads) = onegraph_event(line, '+', &c.sha, one_graph) {
                for q in quads { nq_buffer.push_str(&q); nq_buffer.push('\n'); }
                events_emitted += 1;
                base_final.insert(line.clone(), '+');
            }
        }
    }

    // ─── Base layer = the MATERIALIZED NOW (ruled contract) ───
    // net-assert → the plain triple is (re)asserted alongside its events;
    // net-retract → the plain triple is REMOVED from the graph (on a full
    // rebuild the graph was just cleared, so there is nothing to remove).
    for (line, op) in &base_final {
        match op {
            '+' => {
                nq_buffer.push_str(line);
                nq_buffer.push('\n');
            }
            '-' if !clear_first => {
                // Parse the single N-Quads line into a Quad and remove it.
                // Both failure modes are hard errors: a malformed line here
                // is OUR OWN emitter's output gone wrong, and a failed
                // remove leaves a retracted fact live in the base layer —
                // either way the materialized now would silently lie.
                let parser = oxigraph::io::RdfParser::from_format(oxigraph::io::RdfFormat::NQuads);
                let mut line_owned = line.clone();
                line_owned.push('\n');
                for quad in parser.for_reader(std::io::Cursor::new(line_owned.into_bytes())) {
                    let quad = quad.map_err(|e| {
                        format!("base-layer retract: emitter produced an unparseable line ({e}): {line}")
                    })?;
                    store
                        .remove(&quad)
                        .map_err(|e| format!("base-layer retract removal failed: {e}"))?;
                }
            }
            _ => {}
        }
    }

    // Completeness accounting (BUG 4). Malformed lines hard-fail above;
    // what remains countable is emitter-side drops and unknown suffixes.
    if acct.empty_object > 0 || acct.resolver_other > 0 || acct.unknown_suffix > 0 || acct.resolver_errors > 0 {
        eprintln!(
            "  one-graph accounting: {} line(s) read — empty-value (no fact): {}, resolver-other: {}, unknown-suffix sidecar(s): {}, resolver error(s): {}",
            acct.lines_in, acct.empty_object, acct.resolver_other,
            acct.unknown_suffix, acct.resolver_errors
        );
    }

    if show_progress && total > 0 {
        eprintln!(" done");
    }

    // clear_first = full rebuild (store deleted/rebuilt; also the fallback when an
    // incremental resume point turns out invalid, e.g. after history rewrite).
    // clear_first = false is the sync path: the one graph is PERSISTENT and
    // append-only; sync walks only commits newer than the store's newest and
    // appends their events.
    //
    // Every store operation from here down is a hard error: this graph is the
    // system of record, and "printed a warning but reported success" was the
    // defect class that let a build fail invisibly (review finding A2).
    if clear_first {
        let graph_node = oxigraph::model::NamedNode::new(
            one_graph.trim_start_matches('<').trim_end_matches('>'),
        )
        .map_err(|e| format!("one-graph IRI is not a valid named node: {e}"))?;
        store
            .clear_graph(&graph_node)
            .map_err(|e| format!("one-graph clear (full rebuild) failed: {e}"))?;
    }
    if !nq_buffer.is_empty() {
        let parser = oxigraph::io::RdfParser::from_format(oxigraph::io::RdfFormat::NQuads);
        store
            .load_from_reader(parser, std::io::Cursor::new(nq_buffer.as_bytes()))
            .map_err(|e| format!("one-graph event load failed: {e}"))?;
    }

    Ok((events_seen, events_emitted))
}

/// take one whitespace-separated term from the start of `s`. A term
/// is either `<...>` (an IRI), or `"..."` possibly with `^^<...>` datatype
/// suffix, or a bare token. Returns (term, rest).
fn take_term(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    if s.starts_with('<') {
        let end = s.find('>')?;
        Some((s[..=end].to_string(), &s[end + 1..]))
    } else if s.starts_with('"') {
        // Find the closing quote, honoring backslash escapes.
        let bytes = s.as_bytes();
        let mut i = 1;
        while i < bytes.len() {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] == b'"' {
                let mut end = i + 1;
                // Check for `^^<datatype>` suffix.
                if s[end..].starts_with("^^<") {
                    if let Some(dt_end) = s[end + 2..].find('>') {
                        end = end + 2 + dt_end + 1;
                    }
                }
                return Some((s[..end].to_string(), &s[end..]));
            }
            i += 1;
        }
        None
    } else {
        let end = s.find(char::is_whitespace).unwrap_or(s.len());
        Some((s[..end].to_string(), &s[end..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── v1 write-gate (validate_sidecar_v1) ───────────────────────────────

    #[test]
    fn gate_accepts_all_three_blessed_shapes() {
        let content = "\
copia.Texture.textureId | hasValue | self
Copia/Texture/self.md | linksTo | Soul/Journal/day-15.md
md.externalLink | hasValue | https://github.com/repolex-ai/git-lex
soul.Memory.category | hasValue | \n";
        // Includes the legal empty-object hasValue on the last line.
        assert!(validate_sidecar_v1(content, false).is_empty());
    }

    #[test]
    fn gate_accepts_pipe_inside_object() {
        // splitn(3): the object may contain " | " verbatim.
        let content = "soul.Note.title | hasValue | a | b | c\n";
        assert!(validate_sidecar_v1(content, false).is_empty());
    }

    #[test]
    fn gate_rejects_wrong_field_count() {
        let errs = validate_sidecar_v1("         uad/Squaddie/lspy\n", false);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].1.contains("malformed line"));
    }

    #[test]
    fn gate_rejects_unknown_operator() {
        // The dormant jsonl session extractor's shape must not pass.
        let errs = validate_sidecar_v1("session-abc123 | isA | session\n", false);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].1.contains("closed vocabulary"));
    }

    #[test]
    fn gate_rejects_control_characters_in_object() {
        let errs = validate_sidecar_v1("soul.Note.title | hasValue | tab\there\n", false);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].1.contains("U+0009"));
    }

    #[test]
    fn gate_accepts_rooted_linksto_rejects_empty() {
        // LEGACY repos: leading slash is SEMANTIC under the 2026-07-28 path
        // law (bare = source-folder-relative, `/` = repo-rooted) — the gate
        // must accept both forms until the repo's Phase-4 migration.
        let errs = validate_sidecar_v1(
            "A.md | linksTo | /Soul/Note/x.md\nB.md | linksTo |  \n", false);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].1.contains("empty target"));
    }

    #[test]
    fn gate_obsidian_mode_rejects_leading_slash() {
        // OBSIDIAN repos (stamped by init, Rob-ruled 2026-08-01): bare is
        // repo-root-relative and the `/` form is retired.
        let errs = validate_sidecar_v1(
            "A.md | linksTo | /Soul/Note/x.md\nA.md | linksTo | Soul/Note/y.md\n",
            true);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].0, 1);
        assert!(errs[0].1.contains("Obsidian"));
    }

    #[test]
    fn gate_reports_line_numbers_and_blank_lines() {
        let errs = validate_sidecar_v1("a | hasValue | ok\n\nbroken line\n", false);
        assert_eq!(errs.len(), 2);
        assert_eq!(errs[0].0, 2); // blank line
        assert_eq!(errs[1].0, 3); // malformed
    }

    // ─── parse_unified_diff ────────────────────────────────────────────────





    // ─── rename detection (Phase 2, 2026-04-11) ────────────────────────────





    // ─── git quoted path decoding (fixes QuotedDiffPath blind spot) ────────








    // ─── staged .md change parser (Phase 3 orphan cleanup) ────────────────

    #[test]
    fn parse_staged_empty_is_empty() {
        let (del, ren) = parse_staged_md_changes("");
        assert_eq!(del.len(), 0);
        assert_eq!(ren.len(), 0);
    }

    #[test]
    fn parse_staged_single_delete() {
        // `git diff --cached --name-status -z` output for one deleted file:
        //   D\0friend/old.md\0
        let raw = "D\0friend/old.md\0";
        let (del, ren) = parse_staged_md_changes(raw);
        assert_eq!(del, vec!["friend/old.md".to_string()]);
        assert_eq!(ren.len(), 0);
    }

    #[test]
    fn parse_staged_single_rename() {
        // Rename format: R<score>\0<old>\0<new>\0
        let raw = "R100\0friend/1ux.md\0Friend/1ux.md\0";
        let (del, ren) = parse_staged_md_changes(raw);
        assert_eq!(del.len(), 0);
        assert_eq!(
            ren,
            vec![("friend/1ux.md".to_string(), "Friend/1ux.md".to_string())]
        );
    }

    #[test]
    fn parse_staged_mixed_operations() {
        // A common pre-commit snapshot: one add, one modify, one delete,
        // one rename. Only delete + rename should end up in the cleanup
        // change set — A and M don't require sidecar cleanup.
        let raw = concat!(
            "A\0memory/new-thing.md\0",
            "M\0memory/updated.md\0",
            "D\0memory/obsolete.md\0",
            "R95\0friend/1ux.md\0Friend/1ux.md\0",
        );
        let (del, ren) = parse_staged_md_changes(raw);
        assert_eq!(del, vec!["memory/obsolete.md".to_string()]);
        assert_eq!(
            ren,
            vec![("friend/1ux.md".to_string(), "Friend/1ux.md".to_string())]
        );
    }

    #[test]
    fn parse_staged_bulk_folder_rename() {
        // Simulate the lowercase → capital proclamation rename wave:
        // every friend/*.md becomes Friend/*.md. The parser must emit
        // one Renamed entry per file, never collapse them.
        let raw = concat!(
            "R100\0friend/1ux.md\0Friend/1ux.md\0",
            "R100\0friend/kira.md\0Friend/kira.md\0",
            "R100\0friend/m4rq.md\0Friend/m4rq.md\0",
            "R100\0friend/tr1pl3x.md\0Friend/tr1pl3x.md\0",
        );
        let (del, ren) = parse_staged_md_changes(raw);
        assert_eq!(del.len(), 0);
        assert_eq!(ren.len(), 4);
        assert_eq!(ren[0].0, "friend/1ux.md");
        assert_eq!(ren[0].1, "Friend/1ux.md");
        assert_eq!(ren[3].0, "friend/tr1pl3x.md");
    }

    #[test]
    fn parse_staged_copy_is_not_treated_as_rename() {
        // C (copy) is semantically different from R (rename): the source
        // file still exists, so its .spo doesn't need moving. Cleanup
        // must skip copies.
        let raw = "C85\0original.md\0duplicate.md\0";
        let (del, ren) = parse_staged_md_changes(raw);
        assert_eq!(del.len(), 0);
        assert_eq!(ren.len(), 0, "copies should NOT be treated as renames");
    }

    #[test]
    fn parse_staged_ignores_modifications_and_additions() {
        let raw = concat!(
            "M\0a.md\0",
            "A\0b.md\0",
            "T\0c.md\0",
        );
        let (del, ren) = parse_staged_md_changes(raw);
        assert_eq!(del.len(), 0);
        assert_eq!(ren.len(), 0);
    }

    #[test]
    fn parse_staged_handles_truncated_input() {
        // Defensive: if the input is malformed and ends mid-record,
        // the parser should stop cleanly rather than panicking.
        let raw = "R100\0only-one-field";
        let (del, ren) = parse_staged_md_changes(raw);
        assert_eq!(del.len(), 0);
        assert_eq!(ren.len(), 0);
    }

    // ─── derive_source_document ───────────────────────────────────────────

    #[test]
    fn derives_source_from_fm_sidecar() {
        assert_eq!(
            derive_source_document(".lex/extract/message/foo.md.fm.spo"),
            Some("message/foo.md".to_string())
        );
    }

    #[test]
    fn derives_source_from_md_sidecar() {
        assert_eq!(
            derive_source_document(".lex/extract/brief/bar.md.md.spo"),
            Some("brief/bar.md".to_string())
        );
    }

    #[test]
    fn derives_source_from_cc_sidecar() {
        assert_eq!(
            derive_source_document(".lex/extract/session/baz.md.cc.spo"),
            Some("session/baz.md".to_string())
        );
    }

    #[test]
    fn rejects_unknown_sidecar_suffix() {
        assert_eq!(
            derive_source_document(".lex/extract/weird/qux.md.unknown.spo"),
            None
        );
    }

    #[test]
    fn rejects_sidecar_path_without_extract_prefix() {
        // Only paths under .lex/extract/ are sidecars (the diff-tree
        // pathspec guarantees the prefix on every real input); a prefix-less
        // path can't be attributed to a source document.
        assert_eq!(derive_source_document("foo.md.fm.spo"), None);
    }

    #[test]
    fn future_extractor_suffixes_already_derive() {
        // gliner/haiku are declared in SPO_EXTRACTOR_SUFFIXES; the moment an
        // extractor ships, its history walks without touching this code.
        assert_eq!(
            derive_source_document(".lex/extract/notes/a.md.gliner.spo"),
            Some("notes/a.md".to_string())
        );
    }

    // ─── read_sidecar_at_commit: absence vs failure ─────────────────────
    // These run against the checkout's own repo (same self-skip pattern as
    // the git2_nquads parity tests) — they need a real git to disambiguate.

    fn in_git_checkout() -> bool {
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn absent_path_at_commit_is_verified_empty() {
        if !in_git_checkout() { return; }
        let got = read_sidecar_at_commit("HEAD", "no/such/file.md.fm.spo");
        assert_eq!(got, Ok(Vec::new()));
    }

    #[test]
    fn bad_commit_is_an_error_not_empty() {
        if !in_git_checkout() { return; }
        // A garbage sha must NOT read as "file absent" — that ambiguity is
        // how transient git failures fabricate history events.
        let got = read_sidecar_at_commit(
            "0000000000000000000000000000000000000000",
            "no/such/file.md.fm.spo",
        );
        assert!(got.is_err(), "expected Err, got {got:?}");
    }

}

#[cfg(test)]
mod pipe_value_shape_tests {
    #[test]
    fn value_containing_pipe_separator_is_still_three_fields() {
        // Walker pre-check must agree with the emitter's splitn(3): a value
        // containing " | " is ONE value (adversarial finding 1d — these
        // lines were silently dropped from history while query showed them).
        let line = "soul.Journal.title | hasValue | pipe | trick";
        let fields: Vec<&str> = line.splitn(3, " | ").collect();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[2], "pipe | trick");
    }
}

#[cfg(test)]
mod name_status_parse_tests {
    use super::*;

    #[test]
    fn basic_statuses_collect_touched_paths() {
        let raw = "A\0.lex/extract/a.md.fm.spo\0M\0.lex/extract/b.md.fm.spo\0D\0.lex/extract/c.md.fm.spo\0";
        let (touched, renames) = parse_name_status_z(raw);
        assert_eq!(touched, vec![
            ".lex/extract/a.md.fm.spo",
            ".lex/extract/b.md.fm.spo",
            ".lex/extract/c.md.fm.spo",
        ]);
        assert!(renames.is_empty());
    }

    #[test]
    fn rename_records_pair_old_and_new() {
        let raw = "R100\0.lex/extract/old.md.fm.spo\0.lex/extract/new.md.fm.spo\0M\0.lex/extract/x.md.fm.spo\0";
        let (touched, renames) = parse_name_status_z(raw);
        assert_eq!(renames, vec![(".lex/extract/old.md.fm.spo".to_string(), ".lex/extract/new.md.fm.spo".to_string())]);
        assert_eq!(touched, vec![".lex/extract/x.md.fm.spo"]);
    }

    #[test]
    fn spaces_and_unicode_in_paths_arrive_intact() {
        // THE fix for adversarial 1c: -z paths are raw, never quote-mangled.
        let raw = "A\0.lex/extract/my note.md.fm.spo\0M\0.lex/extract/idea — draft.md.fm.spo\0";
        let (touched, _)= parse_name_status_z(raw);
        assert_eq!(touched, vec![
            ".lex/extract/my note.md.fm.spo",
            ".lex/extract/idea — draft.md.fm.spo",
        ]);
    }

    #[test]
    fn typechange_counts_as_touched_and_copy_touches_new_path() {
        let raw = "T\0.lex/extract/t.md.fm.spo\0C75\0.lex/extract/src.md.fm.spo\0.lex/extract/dst.md.fm.spo\0";
        let (touched, renames) = parse_name_status_z(raw);
        assert_eq!(touched, vec![".lex/extract/t.md.fm.spo", ".lex/extract/dst.md.fm.spo"]);
        assert!(renames.is_empty());
    }

    #[test]
    fn empty_output_is_empty() {
        let (touched, renames) = parse_name_status_z("");
        assert!(touched.is_empty() && renames.is_empty());
    }
}
