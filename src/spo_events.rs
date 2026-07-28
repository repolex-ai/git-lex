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
use std::path::PathBuf;
use std::process::Command;


// ════════════════════════════════════════════════════════════════════════════
// Data types
// ════════════════════════════════════════════════════════════════════════════

/// A single add/remove event extracted from a unified diff. `op` is `'+'`
/// for an addition or `'-'` for a removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpikeEvent {
    pub op: char,
    pub path: String,
    pub line: String,
}

/// A file-level rename event detected by git when `-M` is passed to
/// `diff-tree`. Renames are semantically different from line adds/removes —
/// they're file-level facts, not triple-level facts — so they live in a
/// parallel vector on `SpikeCommit`, not in the `events` stream.
///
/// Phase 2 (2026-04-11): added by w4r3z to support orphan cleanup via
/// `git mv` of .spo mirrors when an .md file is renamed, and to support
/// history-graph ingest correctly annotating introducedInFile as a triple
/// migrates from one file URI to another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rename {
    pub old_path: String,
    pub new_path: String,
    /// Git's similarity index (0-100). Set by git based on the `-M<n>%`
    /// threshold we pass — by construction, always >= the threshold.
    pub similarity: u8,
}

/// All the events in a single commit, plus enough metadata to label the
/// output readably.
#[allow(dead_code)] // `sha` kept for future debug use during the spike
pub struct SpikeCommit {
    pub sha: String,
    pub short_sha: String,
    pub parent_sha: String,
    pub author: String,
    pub date: String,
    pub subject: String,
    pub events: Vec<SpikeEvent>,
    /// File-level renames detected by git with `-M50%`. One entry per
    /// renamed file in this commit. These are NOT included in `events` —
    /// when git reports a rename it suppresses the add/remove pair that
    /// would otherwise describe the same content move.
    pub renames: Vec<Rename>,
}

// ════════════════════════════════════════════════════════════════════════════
// Layer 1: git runner (thin wrappers around shelling out)
// ════════════════════════════════════════════════════════════════════════════

/// Find the git repo root by asking git. Duplicated from main.rs to keep the
/// module self-contained; if this spike graduates to a real feature we can
/// promote a shared helper to a `util` module.
fn find_git_root() -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}
/// Collect commit metadata + diff events for a list of SHAs.
pub(crate) fn collect_commits_from_shas(shas: &[String]) -> Vec<SpikeCommit> {
    shas.iter().map(|sha| build_commit(sha)).collect()
}

/// Well-known magic SHA for the empty git tree. Used as the diff baseline
/// for root commits (commits with no parents) so the walker sees every
/// initial `.spo` line as an addition.
const EMPTY_TREE_SHA: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Build a `SpikeCommit` by asking git for metadata and for the .spo diff
/// against the first parent.
fn build_commit(sha: &str) -> SpikeCommit {
    // Metadata via a single pretty-format line, NUL-delimited. NUL is safe
    // because commit subjects have newlines stripped by `-s` and git never
    // embeds NUL in any of these fields.
    let out = Command::new("git")
        .args(["show", "-s", "--format=%h%x00%an%x00%aI%x00%s", sha])
        .output()
        .expect("git show failed");
    let meta = String::from_utf8_lossy(&out.stdout);
    let parts: Vec<&str> = meta.trim_end().split('\x00').collect();
    let (short_sha, author, date, subject) = if parts.len() == 4 {
        (parts[0].to_string(), parts[1].to_string(), parts[2].to_string(), parts[3].to_string())
    } else {
        (sha[..7.min(sha.len())].to_string(), "?".into(), "?".into(), "?".into())
    };

    // Find the first parent so we can diff against it. `git rev-list
    // --parents -n 1 <sha>` returns a line like `<sha> <parent1> <parent2>
    // ...` where the parents are in commit order.
    let parent_out = Command::new("git")
        .args(["rev-list", "--parents", "-n", "1", sha])
        .output()
        .expect("git rev-list --parents failed");
    let parent_line = String::from_utf8_lossy(&parent_out.stdout);
    let parent_fields: Vec<&str> = parent_line.trim().split_whitespace().collect();
    let base = if parent_fields.len() >= 2 {
        parent_fields[1].to_string()
    } else {
        EMPTY_TREE_SHA.to_string()
    };

    // Zero-context unified diff over extraction sidecar files only, with
    // rename detection at 50% similarity (Phase 2, 2026-04-11).
    //
    // Scope narrowed from `*.spo` to `.lex/extract/*.spo` (lux: 2026-04-09) —
    // the old top-level `extraction.log.spo` was a leftover from an earlier
    // attempt, never part of the real knowledge graph (removed Day 48).
    // Everything that matters lives under `.lex/extract/` as per-document sidecars with names
    // like `foo.md.fm.spo`, `foo.md.md.spo`, `foo.md.cc.spo`, and future
    // extractors (`gliner.spo`, `haiku.spo`) will follow the same shape.
    //
    // `-M50%` turns on rename detection at 50% similarity. This is needed
    // for the orphan cleanup case (folder renames during the lowercase →
    // capital class proclamation will rename every .md under friend/ to
    // Friend/ etc., content unchanged — similarity is 100%, easily above
    // threshold). Without this flag, git reports those as delete+create
    // pairs and the walker can't distinguish rename from real deletion.
    //
    // Renames come through the diff output as `rename from <old>` and
    // `rename to <new>` header lines, which `parse_diff_output` below
    // collects into a separate `renames` vector — NOT into the events
    // stream, because renames are file-level facts, not triple-level.
    let diff_out = Command::new("git")
        .args([
            "diff-tree",
            "--no-commit-id",
            "--no-color",
            "--no-ext-diff",
            "--unified=0",
            "-M50%",
            "-r",
            &base,
            sha,
            "--",
            ".lex/extract/*.spo",
        ])
        .output()
        .expect("git diff-tree failed");

    let diff_text = String::from_utf8_lossy(&diff_out.stdout).to_string();
    let (events, renames) = parse_diff_output(&diff_text);

    SpikeCommit {
        sha: sha.to_string(),
        short_sha,
        parent_sha: base,
        author,
        date,
        subject,
        events,
        renames,
    }
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
fn git_staged_md_changes() -> Result<String, String> {
    let out = Command::new("git")
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
fn sidecar_paths_for_md(md_path: &str) -> Vec<String> {
    let mut out = Vec::new();
    for suffix in SPO_EXTRACTOR_SUFFIXES {
        let rel = format!(".lex/extract/{}.{}.spo", md_path, suffix);
        if git_path_is_tracked(&rel) {
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
fn git_path_is_tracked(path: &str) -> bool {
    Command::new("git")
        .args(["ls-files", "--error-unmatch", "--", path])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run `git rm -f <path>` — used to stage the deletion of a stale .spo
/// mirror. We use `-f` because the file may already be deleted from the
/// working tree (if the agent manually cleaned it up) but still tracked
/// in the index; `git rm -f` handles both cases.
fn git_rm(path: &str) -> Result<(), String> {
    let out = Command::new("git")
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
fn git_mv(old: &str, new: &str) -> Result<(), String> {
    // Ensure the destination parent directory exists — git mv doesn't
    // auto-create intermediate dirs.
    if let Some(parent) = std::path::Path::new(new).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).ok();
        }
    }
    let out = Command::new("git")
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

    // Must run `git` commands from the repo root so relative paths resolve
    // correctly. Save and restore cwd so we don't surprise the caller.
    let prev_cwd = std::env::current_dir().ok();
    if std::env::set_current_dir(&root).is_err() {
        report.errors.push(format!(
            "failed to cd to repo root at {}",
            root.display()
        ));
        return report;
    }

    let raw = match git_staged_md_changes() {
        Ok(r) => r,
        Err(e) => {
            // A failed query is NOT "nothing staged" — skipping cleanup on
            // it would leave orphan sidecars whose facts live forever. The
            // caller fails the commit on any report error.
            report.errors.push(format!(
                "staged-change query failed — cleanup skipped, orphan sidecars may remain: {e}"
            ));
            if let Some(p) = prev_cwd {
                let _ = std::env::set_current_dir(p);
            }
            return report;
        }
    };

    let (deleted_mds, renamed_mds) = parse_staged_md_changes(&raw);

    for md_path in &deleted_mds {
        for sidecar in sidecar_paths_for_md(md_path) {
            match git_rm(&sidecar) {
                Ok(()) => report.deleted.push(sidecar),
                Err(e) => report.errors.push(e),
            }
        }
        // The jsonl extractor also keeps a `.meta` bookkeeping file next to
        // its sidecar; a deleted source must take it along.
        let meta = format!(".lex/extract/{}.meta", md_path);
        if git_path_is_tracked(&meta) {
            match git_rm(&meta) {
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
            if !git_path_is_tracked(&old_sidecar) {
                continue;
            }
            // Skip only if the destination is ALREADY TRACKED IN THE
            // INDEX (separately from old_sidecar). A case-only rename
            // where old and new paths resolve to the same inode on APFS
            // is still a legitimate rename we want to do.
            if git_path_is_tracked(&new_sidecar) && new_sidecar != old_sidecar {
                report.errors.push(format!(
                    "skipping rename of {}: destination {} is already tracked",
                    old_sidecar, new_sidecar
                ));
                continue;
            }
            match git_mv(&old_sidecar, &new_sidecar) {
                Ok(()) => report.renamed.push((old_sidecar, new_sidecar)),
                Err(e) => report.errors.push(e),
            }
        }
        // Move the jsonl extractor's `.meta` bookkeeping file along with a
        // renamed source (same tracked-in-index rules as the sidecars).
        let old_meta = format!(".lex/extract/{}.meta", old_md);
        let new_meta = format!(".lex/extract/{}.meta", new_md);
        if git_path_is_tracked(&old_meta)
            && !(git_path_is_tracked(&new_meta) && new_meta != old_meta)
        {
            match git_mv(&old_meta, &new_meta) {
                Ok(()) => report.renamed.push((old_meta, new_meta)),
                Err(e) => report.errors.push(e),
            }
        }
    }

    if let Some(p) = prev_cwd {
        let _ = std::env::set_current_dir(p);
    }

    report
}

// ════════════════════════════════════════════════════════════════════════════
// Layer 2: unified-diff parser (pure)
// ════════════════════════════════════════════════════════════════════════════

/// Parse a unified diff as produced by `git diff-tree --unified=0 -M50%`
/// into both the flat event stream AND a separate list of file-level
/// renames. Renames are semantically distinct — they describe a file
/// moving to a new path, not a triple being added or removed — so they
/// don't go into the event list.
///
/// When git reports a rename, it suppresses the add/remove pair that
/// would otherwise describe the content move (since by definition a
/// rename at >=50% similarity has mostly-identical content on both
/// sides). If the content changed slightly during the rename, git emits
/// the rename headers followed by a small normal diff body; this parser
/// handles that case by letting `parse_unified_diff` process the body
/// in its entirety, so changed lines still show up as events at the
/// new path.
///
/// Returns `(events, renames)`. Pure function, unit-tested below.
///
/// Phase 2 (2026-04-11): introduced by w4r3z for orphan cleanup + history
/// ingest. Also fixes the quoted-path blind spot the spike sweeper found —
/// git quotes non-ASCII paths in `diff --git` header lines and the old
/// parser was recording the escaped bytes. `decode_git_quoted_path` below
/// handles the C-style escape syntax git uses.
pub fn parse_diff_output(diff: &str) -> (Vec<SpikeEvent>, Vec<Rename>) {
    let mut renames: Vec<Rename> = Vec::new();

    // First pass: scan for rename blocks. A rename block looks like:
    //     diff --git a/old/path b/new/path
    //     similarity index 100%
    //     rename from old/path
    //     rename to new/path
    // It may or may not have a diff body after that (if the content
    // changed slightly during the rename). We collect the rename metadata
    // and let `parse_unified_diff` consume the whole thing for events.
    let mut pending_similarity: Option<u8> = None;
    let mut pending_from: Option<String> = None;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            // New file — reset per-file pending state.
            pending_similarity = None;
            pending_from = None;
            continue;
        }
        if let Some(rest) = line.strip_prefix("similarity index ") {
            let pct = rest.trim_end_matches('%').trim();
            if let Ok(n) = pct.parse::<u8>() {
                pending_similarity = Some(n);
            }
            continue;
        }
        if let Some(from) = line.strip_prefix("rename from ") {
            pending_from = Some(decode_git_quoted_path(from.trim()));
            continue;
        }
        if let Some(to) = line.strip_prefix("rename to ") {
            let new_path = decode_git_quoted_path(to.trim());
            if let Some(old_path) = pending_from.take() {
                renames.push(Rename {
                    old_path,
                    new_path,
                    similarity: pending_similarity.unwrap_or(0),
                });
            }
            // Don't clear pending_similarity — some git outputs put the
            // similarity line after the rename pair. Defensive.
            continue;
        }
    }

    let events = parse_unified_diff(diff);
    (events, renames)
}

/// Parse a unified diff as produced by `git diff-tree --unified=0` into a
/// flat list of add/remove events, each tagged with its file path.
///
/// Only these line kinds are relevant:
/// - `diff --git a/<path> b/<path>`  → switches the current file
/// - `+<content>`                    → addition
/// - `-<content>`                    → removal
///
/// Everything else (`@@` hunk headers, `---`/`+++` file-marker lines, `index`
/// lines, rename headers, similarity headers, empty lines) is skipped.
/// With `--unified=0` there are no context lines so we don't have to filter
/// space-prefixed content.
///
/// Handles git's C-style path quoting (for non-ASCII filenames) in the
/// `diff --git` header — see `decode_git_quoted_path` for the decode rules.
///
/// Pure function, unit-tested below.
pub fn parse_unified_diff(diff: &str) -> Vec<SpikeEvent> {
    let mut events = Vec::new();
    let mut current_path = String::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // "a/path b/path" — we want the b-path (post-change). For paths
            // with spaces or non-ASCII chars, git wraps the path in double
            // quotes and octal-escapes the unsafe bytes. Handle both cases
            // via `split_git_diff_header_paths` + `decode_git_quoted_path`.
            if let Some((_a, b)) = split_git_diff_header_paths(rest) {
                let decoded = decode_git_quoted_path(&b);
                current_path = decoded.trim_start_matches("b/").to_string();
            }
            continue;
        }
        if line.starts_with("+++")
            || line.starts_with("---")
            || line.starts_with("@@")
            || line.starts_with("index ")
            || line.starts_with("similarity index ")
            || line.starts_with("rename from ")
            || line.starts_with("rename to ")
            || line.starts_with("new file mode ")
            || line.starts_with("deleted file mode ")
            || line.starts_with("old mode ")
            || line.starts_with("new mode ")
        {
            continue;
        }
        if let Some(content) = line.strip_prefix('+') {
            events.push(SpikeEvent {
                op: '+',
                path: current_path.clone(),
                line: content.to_string(),
            });
        } else if let Some(content) = line.strip_prefix('-') {
            events.push(SpikeEvent {
                op: '-',
                path: current_path.clone(),
                line: content.to_string(),
            });
        }
    }
    events
}

/// Split the `rest` of a `diff --git ` line into (a-path, b-path), handling
/// both unquoted and quoted forms. Examples:
///   `a/foo.md b/foo.md`                       → ("a/foo.md", "b/foo.md")
///   `"a/foo\342\200\224bar.md" "b/foo\342\200\224bar.md"`
///                                              → (quoted a, quoted b)
/// Returns None if the line doesn't have both halves.
fn split_git_diff_header_paths(rest: &str) -> Option<(String, String)> {
    // If the first character is `"`, both paths are quoted. Walk until
    // the closing quote of the first path (unescaped), then take the rest
    // as the second path.
    if rest.starts_with('"') {
        // Find the closing quote of the first quoted path. Git escapes `"`
        // inside paths as `\"`, so we need to honor backslash escaping.
        let bytes = rest.as_bytes();
        let mut i = 1;
        while i < bytes.len() {
            if bytes[i] == b'\\' {
                i += 2; // skip the escaped char
                continue;
            }
            if bytes[i] == b'"' {
                // Found the end of the first path. Anything after the
                // following space is the second path.
                let a = &rest[..=i];
                let after = rest[i + 1..].trim_start();
                return Some((a.to_string(), after.to_string()));
            }
            i += 1;
        }
        return None;
    }
    // Unquoted: paths are separated by a single space, and path components
    // can't contain spaces in the unquoted form (git would have quoted).
    rest.split_once(' ').map(|(a, b)| (a.to_string(), b.to_string()))
}

/// Decode a path from git's C-style quoted form into UTF-8 bytes. Git
/// quotes non-ASCII and special characters when they appear in paths in
/// diff headers. The quoted form looks like `"foo\342\200\224bar.md"`
/// where `\342\200\224` is the octal byte sequence for U+2014 (em dash).
///
/// Rules (per git's `quote_c_style`):
/// - Wrapped in double quotes.
/// - `\a`, `\b`, `\t`, `\n`, `\v`, `\f`, `\r`      → single ASCII char
/// - `\"`, `\\`                                      → literal
/// - `\<three octal digits>`                         → byte value
/// - Any other character                             → literal
///
/// If the input isn't quoted (no surrounding double quotes), it's returned
/// as-is — this makes the function safe to call unconditionally.
///
/// Pure function, unit-tested below. Fixes the QuotedDiffPath blind spot
/// the spike sweeper flagged: 179 findings in the squad repo history from
/// message files with em-dashes in filenames.
fn decode_git_quoted_path(raw: &str) -> String {
    if !(raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2) {
        return raw.to_string();
    }
    let inner = &raw[1..raw.len() - 1];
    let bytes = inner.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        // Escape sequence starting at `bytes[i]`.
        i += 1;
        if i >= bytes.len() {
            // Trailing backslash — shouldn't happen on well-formed git output.
            out.push(b'\\');
            break;
        }
        match bytes[i] {
            b'a' => { out.push(0x07); i += 1; }
            b'b' => { out.push(0x08); i += 1; }
            b't' => { out.push(b'\t'); i += 1; }
            b'n' => { out.push(b'\n'); i += 1; }
            b'v' => { out.push(0x0b); i += 1; }
            b'f' => { out.push(0x0c); i += 1; }
            b'r' => { out.push(b'\r'); i += 1; }
            b'"' => { out.push(b'"'); i += 1; }
            b'\\' => { out.push(b'\\'); i += 1; }
            c if (b'0'..=b'7').contains(&c) => {
                // Octal escape: up to 3 digits.
                let mut val: u32 = 0;
                let mut n = 0;
                while n < 3 && i + n < bytes.len() {
                    let d = bytes[i + n];
                    if !(b'0'..=b'7').contains(&d) {
                        break;
                    }
                    val = val * 8 + (d - b'0') as u32;
                    n += 1;
                }
                if val > 0xff {
                    // Not a valid single-byte octal; fall back to literal.
                    out.push(b'\\');
                    out.push(bytes[i]);
                    i += 1;
                } else {
                    out.push(val as u8);
                    i += n;
                }
            }
            other => {
                // Unknown escape — preserve as literal.
                out.push(b'\\');
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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
        .args(["ls-tree", sha, "--", sidecar_path])
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
// SPIKE: the "one graph" temporal model (Day 52/53, w4r3z + Rob)
// ════════════════════════════════════════════════════════════════════════════
//
// STATUS: SPIKE. Reachable only via the `git lex spike-onegraph` command, which
// is documented and clearly labelled as experimental. Nothing here runs during
// normal `git lex save` / `git lex sync`. This exists to "try on for size" a
// replacement for the current history subsystem — evaluate the output, then
// decide (Rob decides) whether it becomes the real model.
//
// THE MODEL (formerly contrasted with the retired history walker, whose
//   undeclared `spo:` vocabulary + pre-RDF-1.2 design died in Part 5):
//
//     <reifier> rdf:reifies <<( s p o )>> .
//     <reifier> git-lex:assertedIn  <Commit/SHA> .   (line added to a .spo)
//     <reifier> git-lex:retractedIn <Commit/SHA> .   (line removed from a .spo)
//
//   Differences from the old model, point by point:
//     1. ONE graph, not a history graph + N sync graphs + a now graph. The
//        "now" view is DERIVED by query (a triple whose most-recent event is an
//        assert, with no later retract, is live).
//     2. `assertedIn`/`retractedIn` instead of `addedIn`/`removedIn`. These are
//        PLACEHOLDER predicate names — the final names are Rob's call and must
//        be DECLARED in the ontology before this model ships. The spike emits
//        them only so we can look at real output.
//     3. NO `inFile` annotation. The old model recorded which sidecar the line
//        came from; the one-graph model treats the Thing (the doc IRI) as the
//        stable subject and doesn't leak the sidecar path into the graph.
//     4. The commit object is the EXISTING `git:Commit` IRI that `git lex query`
//        already emits into the commits graph — so a fact JOINS straight to its
//        commit's author/date. No new commit/actor emission; we ride the
//        command-faithful `git:` layer that's already there.
//
//   What it SHARES with the real pipeline (deliberately — this is the whole
//   point of the spike vs. the earlier throwaway prototype): it resolves each
//   `.spo` line through the SAME `crate::nquad::emit_spo_line_nquads` the
//   now-graph uses. No naive re-implementation of sidecar-line → triple
//   resolution. The old prototype's garbage predicates came from skipping this.

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

/// SPIKE. Build the one-graph N-Quads for a single resolved triple event.
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
/// NOTE (spike simplification): a `-` (retract) event emits the base fact too.
/// A single named graph can't hold "the fact once existed" AND "the fact is not
/// live now" as the same plain triple; resolving that (retract removes the base
/// triple, or the now-view is always derived from the latest event) is an open
/// modeling question for Rob. For the spike we keep the base triple present on
/// both events so the reification audit trail is symmetric, and the DERIVED
/// now-view (asserted-with-no-later-retract) remains the authoritative "now".
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
    commits: &[SpikeCommit],
    store: &oxigraph::store::Store,
    one_graph: &str,
    slug_index: &HashMap<String, String>,
    path_index: &HashSet<String>,
    obj_props: &HashSet<String>,
    prop_datatypes: &HashMap<String, String>,
    kit_namespaces: &HashMap<String, String>,
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
    // Per-reason drop accounting (BUG 4): every sidecar line either yields
    // triples or is counted under exactly one drop reason. Nothing vanishes
    // silently. Classification happens WALKER-SIDE — the shared emitter
    // (`emit_spo_line_nquads`, also serving the now view + `git lex query`)
    // is deliberately untouched; lines it drops for its own reasons land in
    // `resolver_other` until a conscious cross-surface change is ruled.
    #[derive(Default)]
    struct DropAccounting {
        lines_in: usize,
        retired_body_extract: usize, // BUG 3 — quarantined legacy shim (legacy_spo)
        malformed_shape: usize,      // not exactly 3 ` | `-separated fields
        empty_object: usize,         // third field empty/whitespace
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
        let doc_uri = format!(
            "<{}>",
            crate::git::resource_uri(&crate::nquad::uri_encode_path(&relpath_str))
        );
        let lines = read_sidecar_at_commit(commit, sidecar_path)?;
        acct.lines_in += lines.len();
        let mut triples: HashSet<String> = HashSet::new();
        let mut emitted_types: HashSet<String> = HashSet::new();
        for line in &lines {
            // Quarantined legacy formats first (never reach the emitter).
            if crate::legacy_spo::is_retired_body_extract_line(line) {
                acct.retired_body_extract += 1;
                continue;
            }
            // Shape pre-checks, mirroring the emitter's own silent drops so
            // they're COUNTED here (the emitter still guards for its other
            // callers).
            let fields: Vec<&str> = line.split(" | ").collect();
            if fields.len() != 3 {
                acct.malformed_shape += 1;
                continue;
            }
            if fields[2].trim().is_empty() {
                acct.empty_object += 1;
                continue;
            }
            let mut emit_buf = String::new();
            // Emitter errors are COUNTED (a line can yield some triples AND
            // errors — e.g. one rejected value among several); the now path
            // counts the same errors, so the walk must too.
            acct.resolver_errors += crate::nquad::emit_spo_line_nquads(
                line, &doc_uri, one_graph, &relpath_str,
                slug_index, path_index, obj_props, prop_datatypes,
                kit_namespaces, &mut emitted_types, &mut emit_buf,
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
        for ev in &c.events {
            old_side.insert(ev.path.as_str());
            new_side.insert(ev.path.as_str());
        }
        for r in &c.renames {
            old_side.insert(r.old_path.as_str());
            new_side.insert(r.new_path.as_str());
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

    // Completeness accounting, itemized by reason (BUG 4). Loud whenever any
    // line was dropped; which reasons should hard-fail is Rob's pending call.
    let dropped_total = acct.retired_body_extract + acct.malformed_shape
        + acct.empty_object + acct.resolver_other;
    if dropped_total > 0 || acct.unknown_suffix > 0 || acct.resolver_errors > 0 {
        eprintln!(
            "  one-graph accounting: {} line(s) read, {} dropped — retired-body-extract(@): {}, malformed-shape: {}, empty-object: {}, resolver-other: {}, unknown-suffix sidecar(s): {}, resolver error(s): {}",
            acct.lines_in, dropped_total, acct.retired_body_extract,
            acct.malformed_shape, acct.empty_object, acct.resolver_other,
            acct.unknown_suffix, acct.resolver_errors
        );
    }

    if show_progress && total > 0 {
        eprintln!(" done");
    }

    // clear_first = full rebuild (the spike command; also the fallback when an
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

    // ─── parse_unified_diff ────────────────────────────────────────────────

    #[test]
    fn parser_handles_empty_diff() {
        assert!(parse_unified_diff("").is_empty());
    }

    #[test]
    fn parser_extracts_add_and_remove_with_path() {
        let diff = concat!(
            "diff --git a/foo/bar.fm.spo b/foo/bar.fm.spo\n",
            "index aaaaaaa..bbbbbbb 100644\n",
            "--- a/foo/bar.fm.spo\n",
            "+++ b/foo/bar.fm.spo\n",
            "@@ -1,0 +1,1 @@\n",
            "+squad.task.taskStatus | hasValue | done\n",
            "@@ -2,1 +2,0 @@\n",
            "-squad.task.taskStatus | hasValue | todo\n",
        );
        let events = parse_unified_diff(diff);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].op, '+');
        assert_eq!(events[0].path, "foo/bar.fm.spo");
        assert_eq!(events[0].line, "squad.task.taskStatus | hasValue | done");
        assert_eq!(events[1].op, '-');
        assert_eq!(events[1].path, "foo/bar.fm.spo");
        assert_eq!(events[1].line, "squad.task.taskStatus | hasValue | todo");
    }

    #[test]
    fn parser_skips_hunk_headers_and_index_lines() {
        let diff = concat!(
            "diff --git a/x.spo b/x.spo\n",
            "index 000..111 100644\n",
            "--- a/x.spo\n",
            "+++ b/x.spo\n",
            "@@ -1 +1 @@\n",
            "-old\n",
            "+new\n",
        );
        let events = parse_unified_diff(diff);
        // Exactly 2 events — not 6, not 4. The `---`, `+++`, `@@`, and
        // `index` lines should all be skipped.
        assert_eq!(events.len(), 2);
        assert_eq!(events.iter().filter(|e| e.op == '+').count(), 1);
        assert_eq!(events.iter().filter(|e| e.op == '-').count(), 1);
    }

    #[test]
    fn parser_handles_multi_file_diff() {
        let diff = concat!(
            "diff --git a/a.fm.spo b/a.fm.spo\n",
            "+line-a\n",
            "diff --git a/b.fm.spo b/b.fm.spo\n",
            "+line-b\n",
        );
        let events = parse_unified_diff(diff);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].path, "a.fm.spo");
        assert_eq!(events[1].path, "b.fm.spo");
    }

    // ─── rename detection (Phase 2, 2026-04-11) ────────────────────────────

    #[test]
    fn parse_diff_output_detects_simple_rename() {
        // The canonical rename block git emits for a pure rename at 100%
        // similarity (folder rename, content unchanged).
        let diff = concat!(
            "diff --git a/friend/1ux.md.fm.spo b/Friend/1ux.md.fm.spo\n",
            "similarity index 100%\n",
            "rename from friend/1ux.md.fm.spo\n",
            "rename to Friend/1ux.md.fm.spo\n",
        );
        let (events, renames) = parse_diff_output(diff);
        assert_eq!(events.len(), 0, "pure rename should have no line events");
        assert_eq!(renames.len(), 1);
        assert_eq!(renames[0].old_path, "friend/1ux.md.fm.spo");
        assert_eq!(renames[0].new_path, "Friend/1ux.md.fm.spo");
        assert_eq!(renames[0].similarity, 100);
    }

    #[test]
    fn parse_diff_output_detects_rename_with_modification() {
        // A rename at less than 100% similarity still emits a body diff
        // showing the changed lines at the NEW path. Both the rename and
        // the events should land.
        let diff = concat!(
            "diff --git a/friend/1ux.md.fm.spo b/Friend/1ux.md.fm.spo\n",
            "similarity index 85%\n",
            "rename from friend/1ux.md.fm.spo\n",
            "rename to Friend/1ux.md.fm.spo\n",
            "index aaaa..bbbb 100644\n",
            "--- a/friend/1ux.md.fm.spo\n",
            "+++ b/Friend/1ux.md.fm.spo\n",
            "@@ -5 +5 @@\n",
            "-soul.friend.relationship | hasValue | boss\n",
            "+soul.friend.relationship | hasValue | captain\n",
        );
        let (events, renames) = parse_diff_output(diff);
        assert_eq!(renames.len(), 1);
        assert_eq!(renames[0].similarity, 85);
        assert_eq!(events.len(), 2, "body diff should produce 2 line events");
        // Both events land at the b-path (new path).
        assert!(events.iter().all(|e| e.path == "Friend/1ux.md.fm.spo"));
    }

    #[test]
    fn parse_diff_output_mixes_rename_and_unrelated_file_changes() {
        // One rename + one regular add in the same diff. Both should land.
        let diff = concat!(
            "diff --git a/friend/x.md.fm.spo b/Friend/x.md.fm.spo\n",
            "similarity index 100%\n",
            "rename from friend/x.md.fm.spo\n",
            "rename to Friend/x.md.fm.spo\n",
            "diff --git a/new-memory.md.fm.spo b/new-memory.md.fm.spo\n",
            "new file mode 100644\n",
            "index 0000000..aaa\n",
            "--- /dev/null\n",
            "+++ b/new-memory.md.fm.spo\n",
            "@@ -0,0 +1,1 @@\n",
            "+tags | hasValue | new\n",
        );
        let (events, renames) = parse_diff_output(diff);
        assert_eq!(renames.len(), 1);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, "new-memory.md.fm.spo");
        assert_eq!(events[0].line, "tags | hasValue | new");
    }

    #[test]
    fn parse_diff_output_skips_rename_headers_in_events() {
        // The old parser would have picked up `rename from` and `rename to`
        // as no-op lines, but with the Phase 2 parser they're in the
        // stop-list. Confirm no stray events get emitted from a rename
        // block.
        let diff = concat!(
            "diff --git a/a.fm.spo b/b.fm.spo\n",
            "similarity index 100%\n",
            "rename from a.fm.spo\n",
            "rename to b.fm.spo\n",
        );
        let events = parse_unified_diff(diff);
        assert_eq!(events.len(), 0);
    }

    // ─── git quoted path decoding (fixes QuotedDiffPath blind spot) ────────

    #[test]
    fn decode_unquoted_path_is_identity() {
        assert_eq!(decode_git_quoted_path("a/foo.md"), "a/foo.md");
        assert_eq!(decode_git_quoted_path(""), "");
    }

    #[test]
    fn decode_em_dash_path() {
        // U+2014 (em dash) encoded as UTF-8 bytes 0xE2 0x80 0x94, which
        // git renders as octal \342\200\224.
        let raw = r#""Message/channel\342\200\224test.md.fm.spo""#;
        assert_eq!(
            decode_git_quoted_path(raw),
            "Message/channel—test.md.fm.spo"
        );
    }

    #[test]
    fn decode_escaped_quote_and_backslash() {
        assert_eq!(decode_git_quoted_path(r#""a\"b""#), "a\"b");
        assert_eq!(decode_git_quoted_path(r#""a\\b""#), "a\\b");
    }

    #[test]
    fn decode_standard_c_escapes() {
        assert_eq!(decode_git_quoted_path(r#""tab\there""#), "tab\there");
        assert_eq!(decode_git_quoted_path(r#""line\nbreak""#), "line\nbreak");
    }

    #[test]
    fn split_header_paths_unquoted() {
        let (a, b) = split_git_diff_header_paths("a/foo.md b/foo.md").unwrap();
        assert_eq!(a, "a/foo.md");
        assert_eq!(b, "b/foo.md");
    }

    #[test]
    fn split_header_paths_quoted() {
        let raw = r#""a/f\342\200\224o.md" "b/f\342\200\224o.md""#;
        let (a, b) = split_git_diff_header_paths(raw).unwrap();
        assert_eq!(a, r#""a/f\342\200\224o.md""#);
        assert_eq!(b, r#""b/f\342\200\224o.md""#);
    }

    #[test]
    fn parser_resolves_quoted_path_in_diff_header() {
        let diff = concat!(
            r#"diff --git "a/Message/channel\342\200\224test.md.fm.spo" "b/Message/channel\342\200\224test.md.fm.spo""#,
            "\n+tags | hasValue | channel\n",
        );
        let events = parse_unified_diff(diff);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, "Message/channel—test.md.fm.spo");
    }

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
