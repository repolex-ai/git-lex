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
pub const SPO_OPERATORS_V1: &[&str] = &["hasValue", "linksTo"];

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
///   - `linksTo`: non-empty target. ONE law (Rob-ruled 2026-08-08): the
///     target is a repo-root-relative path; a historical leading `/`
///     (retired repo-rooted form) names the same path and is normalized at
///     emit, so it is tolerated here — the gate polices shape, the emitter
///     owns resolution.
pub fn validate_sidecar_v1(content: &str) -> Vec<(usize, String)> {
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
        if operator == "linksTo" && object.trim().is_empty() {
            errors.push((lineno, "linksTo with an empty target".to_string()));
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

use crate::find_git_root;

/// Collect the walk inputs for a list of SHAs. Any git failure is an ERROR
/// for the whole walk: a commit whose diff can't be read must stop the
/// build, not silently contribute nothing (a corrupt object used to shrink
/// history with exit 0 — adversarial finding 1e).
pub fn collect_commits_from_shas(
    shas: &[String],
    horizon_start: Option<&str>,
) -> Result<Vec<WalkCommit>, String> {
    let root = find_git_root().ok_or("not inside a git repository")?;
    let repo = git2::Repository::open(&root)
        .map_err(|e| format!("open git repository {}: {e}", root.display()))?;
    // First parents, read in process.
    let mut bases: Vec<String> = Vec::with_capacity(shas.len());
    for sha in shas {
        let commit = git2::Oid::from_str(sha)
            .and_then(|oid| repo.find_commit(oid))
            .map_err(|e| format!("read commit {sha}: {e}"))?;
        bases.push(match commit.parent_ids().next() {
            Some(parent) => parent.to_string(),
            None => EMPTY_TREE_SHA.to_string(),
        });
    }
    // Every diff through ONE `git diff-tree --stdin` (a process per commit
    // was an hour of a 88k-commit rebuild, #15). Same options as before:
    // NUL-separated name-status, `-M50%` rename detection (folder recases
    // must pair old→new, not read as delete+create), sidecars only.
    let mut input = String::new();
    for (sha, base) in shas.iter().zip(&bases) {
        if base == EMPTY_TREE_SHA {
            input.push_str(sha); // a root commit: --root diffs it against nothing
        } else {
            input.push_str(&format!("{sha} {base}"));
        }
        input.push('\n');
    }
    let raw = diff_tree_stdin(&root, &input)?;
    let per_commit = split_diff_tree_stdin(&raw, shas)?;

    shas.iter()
        .zip(bases)
        .zip(per_commit)
        .map(|((sha, base), records)| {
            // dev_history_horizon: the first walked commit diffs against
            // the EMPTY tree so the whole tree asserts as of the horizon.
            if horizon_start == Some(sha.as_str()) {
                return rebuild_against_empty_tree(sha);
            }
            let (touched, renames) = parse_name_status_z(&records);
            Ok(WalkCommit { sha: sha.clone(), parent_sha: base, touched, renames })
        })
        .collect()
}

/// Run one `git diff-tree --stdin` over `input` (one "<commit> [<parent>]"
/// line per commit) and return its raw NUL-separated output.
fn diff_tree_stdin(root: &std::path::Path, input: &str) -> Result<String, String> {
    use std::process::Stdio;
    let mut child = Command::new("git")
        .current_dir(root)
        .args([
            "diff-tree", "--stdin", "--always", "--root", "--no-color", "--no-ext-diff",
            "--name-status", "-z", "-M50%", "-r", "--", ".lex/extract/*.spo",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("git diff-tree --stdin: spawn failed: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("git diff-tree --stdin: no stdin")?;
    let input = input.to_string();
    // Feed from a thread: git writes as it reads, and a full pipe on either
    // side would otherwise deadlock.
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let out = child
        .wait_with_output()
        .map_err(|e| format!("git diff-tree --stdin: {e}"))?;
    writer
        .join()
        .map_err(|_| "git diff-tree --stdin: input writer panicked".to_string())?
        .map_err(|e| format!("git diff-tree --stdin: writing input failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git diff-tree --stdin failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Split `git diff-tree --stdin -z --always` output into each commit's
/// name-status records, in input order. Every commit prints its own sha as
/// a header field first (`--always`), and no record field can be a bare
/// sha — paths all start with `.lex/extract/`, statuses are letters — so a
/// field equal to the next expected sha starts that commit's block. A
/// missing or out-of-order header is an error: a commit whose diff cannot
/// be attributed must stop the build.
fn split_diff_tree_stdin(raw: &str, shas: &[String]) -> Result<Vec<String>, String> {
    let mut blocks: Vec<String> = Vec::with_capacity(shas.len());
    let mut current: Option<String> = None;
    let mut next = 0usize;
    for field in raw.split('\0') {
        if next < shas.len() && field.trim() == shas[next] {
            if let Some(done) = current.take() {
                blocks.push(done);
            }
            current = Some(String::new());
            next += 1;
            continue;
        }
        if field.is_empty() {
            continue;
        }
        let Some(block) = current.as_mut() else {
            return Err(format!("git diff-tree --stdin: output before any commit header: {field:?}"));
        };
        block.push_str(field);
        block.push('\0');
    }
    if let Some(done) = current.take() {
        blocks.push(done);
    }
    if blocks.len() != shas.len() {
        return Err(format!(
            "git diff-tree --stdin: {} commit(s) asked, {} answered",
            shas.len(),
            blocks.len()
        ));
    }
    Ok(blocks)
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
///   - `md`   : markdown links (tree-sitter walker → `linksTo`; the
///              wikilink/mention lanes are retired)
///
/// Historical suffixes (kept so cleanup still globs their legacy sidecars):
///   - `cc`   : claude-code JSONL sessions — extractor deleted
///              (Rob-ruled 2026-08-01; transcript analytics is ravel's
///              domain), old sidecars may survive in repos and history
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
fn sidecar_paths_for_md(index: &IndexProbe, md_path: &str) -> Vec<String> {
    let mut out = Vec::new();
    for suffix in SPO_EXTRACTOR_SUFFIXES {
        let rel = format!(".lex/extract/{}.{}.spo", md_path, suffix);
        if index.tracked(&rel) {
            out.push(rel);
        }
    }
    out
}

/// Whether a path is currently tracked in git's index, with exact case —
/// what `git ls-files --error-unmatch -- <path>` answers, for one cleanup
/// pass and without a process per path. The index is re-read before each
/// answer when it changed on disk, so a `git rm`/`git mv` made between two
/// checks is seen; every merge stage counts as tracked, as it does for
/// ls-files. Any libgit2 failure answers "not tracked", as a failed spawn
/// did.
///
/// Why not `Path::exists()`? Because on macOS APFS (case-insensitive by
/// default), the filesystem answer is wrong for case-only rename cases.
/// Git's index is always case-exact, so asking git gives us the truth.
struct IndexProbe {
    repo: Option<git2::Repository>,
}

impl IndexProbe {
    fn open(root: &std::path::Path) -> Self {
        IndexProbe { repo: git2::Repository::open(root).ok() }
    }

    fn tracked(&self, path: &str) -> bool {
        let Some(repo) = &self.repo else { return false };
        let Ok(mut index) = repo.index() else { return false };
        if index.read(false).is_err() {
            return false;
        }
        (0..=3).any(|stage| index.get_path(std::path::Path::new(path), stage).is_some())
    }
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
    if let Some(parent) = root.join(new).parent()
        && !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).ok();
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
    let index = IndexProbe::open(&root);

    for md_path in &deleted_mds {
        for sidecar in sidecar_paths_for_md(&index, md_path) {
            match git_rm(&root, &sidecar) {
                Ok(()) => report.deleted.push(sidecar),
                Err(e) => report.errors.push(e),
            }
        }
        // The jsonl extractor also keeps a `.meta` bookkeeping file next to
        // its sidecar; a deleted source must take it along.
        let meta = format!(".lex/extract/{}.meta", md_path);
        if index.tracked(&meta) {
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
            if !index.tracked(&old_sidecar) {
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
            if index.tracked(&new_sidecar) && new_sidecar != old_sidecar {
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
        if index.tracked(&old_meta) {
            if index.tracked(&new_meta) && new_meta != old_meta {
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
pub fn derive_source_document(sidecar_rel_path: &str) -> Option<String> {
    let after_extract = sidecar_rel_path.strip_prefix(".lex/extract/")?;
    for suffix in SPO_EXTRACTOR_SUFFIXES {
        let full = format!(".{}.spo", suffix);
        if let Some(base) = after_extract.strip_suffix(full.as_str()) {
            return Some(base.to_string());
        }
    }
    None
}

/// Orphaned-sidecar convergence (#107): remove sidecars whose SOURCE
/// document no longer exists in the working tree. Returns the removed
/// sidecars' repo-relative paths, sorted.
///
/// [`cleanup_sidecars_for_staged_changes`] can only see damage save itself
/// is about to commit — it reads the STAGED md changes. A sidecar orphaned
/// by a raw git delete/rename OUTSIDE save (historically: hookless-clone
/// commits) leaves the tree clean, so save short-circuited at "Nothing to
/// save" and the orphan kept its facts alive in every future sync. Same
/// ethos as save converging its own pre-commit hook: derived state under
/// .lex/extract/ is save's product, so save converges it. The removal
/// becomes a staged deletion in THIS save, and the next sync retracts the
/// orphan's facts honestly. Unknown .spo suffixes are left untouched
/// (derive_source_document returns None — never guess an attribution).
pub fn remove_orphaned_sidecars(root: &std::path::Path) -> Vec<String> {
    let extract_root = crate::layout::extract_dir(root);
    let mut removed = Vec::new();
    let mut stack = vec![extract_root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(rel) = path.strip_prefix(root) else { continue };
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            let Some(source) = derive_source_document(&rel_str) else { continue };
            if !root.join(&source).exists() && std::fs::remove_file(&path).is_ok() {
                removed.push(rel_str);
            }
        }
    }
    removed.sort();
    removed
}

/// Committed-sidecar reads for the one-graph walk, in process (libgit2).
///
/// One repository handle per walk. It replaces a `git show` / `git ls-tree` /
/// `git cat-file` process per sidecar per commit, which was most of a
/// sync's wall time once a repo reached tens of thousands of documents
/// (#15). The read semantics are unchanged:
///
/// "Path absent at this commit" is a NORMAL outcome — the added/deleted side
/// of a diff resolves against a commit where the file doesn't exist — and
/// reads as empty. Every OTHER failure is an ERROR: treating it as absence
/// would let a broken read fabricate history (an empty old side reads as
/// "everything was added", an empty new side as "everything was removed" —
/// assert/retract events manufactured into the one graph). Absence is
/// libgit2's NotFound on the path lookup inside a tree that DID resolve; an
/// unresolvable commit is an error, never an empty read.
pub struct SidecarReader {
    repo: git2::Repository,
    /// commit sha → its root tree (None = the empty tree).
    trees: HashMap<String, Option<git2::Oid>>,
    /// tree oid → its entries (name → (oid, is a blob)). libgit2 re-reads
    /// and re-parses a large tree on every path lookup, so a folder of 180k
    /// sidecars cost a full parse per sidecar read. Trees are content-
    /// addressed: an unchanged folder is one entry across every commit.
    dirs: HashMap<git2::Oid, HashMap<String, (git2::Oid, bool)>>,
}

/// Parsed trees kept at once; the walk moves forward through history, so
/// old folder versions stop being asked for.
const SIDECAR_DIR_CACHE: usize = 64;

impl SidecarReader {
    pub fn open() -> Result<Self, String> {
        let root = find_git_root().ok_or("not inside a git repository")?;
        Self::open_at(&root)
    }

    /// The reader for the repository at `root` (the walk's own entry point
    /// is [`SidecarReader::open`]; tests hand in a scratch repository).
    pub fn open_at(root: &std::path::Path) -> Result<Self, String> {
        let repo = git2::Repository::open(root)
            .map_err(|e| format!("open git repository {}: {e}", root.display()))?;
        Ok(SidecarReader { repo, trees: HashMap::new(), dirs: HashMap::new() })
    }

    /// The root tree oid of a commit (or tree) sha; None for the empty tree.
    fn root_tree(&mut self, sha: &str) -> Result<Option<git2::Oid>, String> {
        if let Some(oid) = self.trees.get(sha) {
            return Ok(*oid);
        }
        let oid = if sha == EMPTY_TREE_SHA {
            None
        } else {
            let obj = self
                .repo
                .revparse_single(sha)
                .map_err(|e| format!("resolve {sha}: {e}"))?;
            let tree = obj
                .peel_to_tree()
                .map_err(|e| format!("tree of {sha}: {e}"))?;
            Some(tree.id())
        };
        self.trees.insert(sha.to_string(), oid);
        Ok(oid)
    }

    /// One tree's entries, parsed once.
    fn entries(&mut self, tree: git2::Oid) -> Result<&HashMap<String, (git2::Oid, bool)>, String> {
        if !self.dirs.contains_key(&tree) {
            if self.dirs.len() >= SIDECAR_DIR_CACHE {
                self.dirs.clear();
            }
            let parsed = self
                .repo
                .find_tree(tree)
                .map_err(|e| format!("read tree {tree}: {e}"))?;
            let mut map = HashMap::with_capacity(parsed.len());
            for entry in parsed.iter() {
                // Names are raw bytes in git; sidecar paths are UTF-8 (they
                // come from the diff as strings), so a non-UTF-8 name can
                // never be the one asked for.
                if let Ok(name) = std::str::from_utf8(entry.name_bytes()) {
                    let is_blob = entry.kind() == Some(git2::ObjectType::Blob);
                    map.insert(name.to_string(), (entry.id(), is_blob));
                }
            }
            self.dirs.insert(tree, map);
        }
        Ok(&self.dirs[&tree])
    }

    /// A sidecar's SPO lines at a commit; empty when the path is absent there.
    pub fn lines_at(&mut self, sha: &str, sidecar_path: &str) -> Result<Vec<String>, String> {
        match self.blob_at(sha, sidecar_path)? {
            Some(oid) => self.blob_lines(oid),
            None => Ok(Vec::new()),
        }
    }

    /// The blob a path holds at a commit; None when the path is absent there
    /// (any missing component). A path that names a folder is an error.
    fn blob_at(&mut self, sha: &str, path: &str) -> Result<Option<git2::Oid>, String> {
        let Some(mut tree) = self.root_tree(sha)? else { return Ok(None) };
        let mut components = path.split('/').peekable();
        while let Some(name) = components.next() {
            let Some(&(oid, is_blob)) = self.entries(tree)?.get(name) else {
                return Ok(None);
            };
            match (components.peek().is_some(), is_blob) {
                (false, true) => return Ok(Some(oid)),
                (false, false) => return Err(format!("{sha}:{path} is not a file")),
                (true, false) => tree = oid,
                (true, true) => return Ok(None), // a file where a folder was asked
            }
        }
        Ok(None)
    }

    /// A blob's SPO lines (non-empty, non-comment).
    fn blob_lines(&self, oid: git2::Oid) -> Result<Vec<String>, String> {
        let blob = self
            .repo
            .find_blob(oid)
            .map_err(|e| format!("read blob {oid}: {e}"))?;
        Ok(String::from_utf8_lossy(blob.content())
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| l.to_string())
            .collect())
    }
}

/// A sidecar's SPO lines at a commit (see [`SidecarReader`] for the
/// absent-vs-failure contract). One-off reads; the walk keeps one reader.
#[cfg(test)]
fn read_sidecar_at_commit(sha: &str, sidecar_path: &str) -> Result<Vec<String>, String> {
    SidecarReader::open()?.lines_at(sha, sidecar_path)
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
pub const LEXHISTORY_GRAPH_IRI: &str = "https://repolex.ai/git-lex/LexHistoryGraph";

/// The statement-lifecycle predicates, declared in git-lex.ttl v0.5+
/// (kit-base 9e6f4bf): domain git-lex:SpoEvent, range git2:Commit.
pub const ONEGRAPH_ASSERTED_IN: &str = "https://repolex.ai/ontology/git-lex/assertedIn";
pub const ONEGRAPH_RETRACTED_IN: &str = "https://repolex.ai/ontology/git-lex/retractedIn";

/// Build the one-graph N-Quads for a single resolved triple event.
///
/// Reuses the real emitter's N-Quad output verbatim (parse-then-rewrap), and
/// emits the "Option B" one-graph shape — the base fact asserted STANDALONE plus
/// a reified triple-term carrying the commit event. This matches the agreed
/// Turtle form:
///
/// ```text
/// s p o .                                        # base fact, asserted standalone
/// <reifier> rdf:reifies         <<( s p o )>> .
/// <reifier> git-lex:assertedIn  <Commit/SHA> .   (op == '+')
/// <reifier> git-lex:retractedIn <Commit/SHA> .   (op == '-')
/// ```
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
/// Returns the summary counts and the subjects whose base-layer facts
/// changed (so the now view can be refreshed for exactly those), or an
/// error if git or the store failed anywhere — a partial walk must never
/// report success, because the one graph is the system of record.
///
/// ── Memory (#15) ─────────────────────────────────────────────────────────
/// A full rebuild writes the graph in BATCHES: the old graph is cleared
/// first, and every `REBUILD_BATCH_QUADS` pending quads the events and the
/// base-layer changes so far are written and dropped from memory. Each
/// batch is exactly an incremental append onto what the earlier batches
/// wrote, so the finished graph is the same graph. Held in one piece, a
/// rebuild's single store transaction cost about 3 KB of memory per quad —
/// 36 GB at 88,000 commits — and grew with the whole of history.
///
/// An append (`clear_first = false`) still writes once, at the end: its
/// size follows the change, not the repo.
///
/// What a batched rebuild gives up is all-or-nothing: killed part-way, the
/// graph holds only the batches written so far. The CALLER must therefore
/// write the sync marker (git2 commit ordinals — see sync.rs) only after
/// this returns Ok, so that a partial graph is never read as a finished one.
pub fn onegraph_walk_engine(
    commits: &[WalkCommit],
    store: &oxigraph::store::Store,
    one_graph: &str,
    ctx: &crate::nquad::ResolverContext,
    show_progress: bool,
    clear_first: bool,
) -> Result<WalkOutcome, String> {
    let mut reader = SidecarReader::open()?;
    let batch_quads = if clear_first { REBUILD_BATCH_QUADS } else { usize::MAX };
    onegraph_walk_engine_with(&mut reader, commits, store, one_graph, ctx, show_progress, clear_first, batch_quads)
}

/// Pending quads that trigger a write during a full rebuild. One store
/// transaction holds its whole write set in memory at roughly 3 KB a quad
/// (measured: 1.26M quads, +3.7 GB), so 200,000 quads is about 600 MB per
/// batch — small next to the store's own caches, large enough that a
/// 17M-quad rebuild is under a hundred commits to the store.
pub const REBUILD_BATCH_QUADS: usize = 200_000;

/// Remove every quad of `graph`, a bounded number per store transaction.
/// `Store::clear_graph` does the same removal in ONE transaction, whose
/// memory grows with the graph. Like it, this leaves the graph registered.
pub fn clear_graph_in_batches(
    store: &oxigraph::store::Store,
    graph: &oxigraph::model::NamedNode,
    batch_quads: usize,
) -> Result<(), String> {
    let batch_quads = batch_quads.max(1);
    loop {
        let quads: Vec<oxigraph::model::Quad> = store
            .quads_for_pattern(None, None, None, Some(graph.as_ref().into()))
            .take(batch_quads)
            .collect::<Result<_, _>>()
            .map_err(|e| format!("reading {graph} to clear it failed: {e}"))?;
        if quads.is_empty() {
            return Ok(());
        }
        let mut transaction = store
            .start_transaction()
            .map_err(|e| format!("clearing {graph} failed: {e}"))?;
        for quad in &quads {
            transaction.remove(quad);
        }
        transaction
            .commit()
            .map_err(|e| format!("clearing {graph} failed: {e}"))?;
    }
}

/// One batch of a walk, written: the pending events, plus the base-layer
/// (current-state) effect of those events — net-assert → the plain triple
/// is inserted, net-retract → it is REMOVED. Both buffers come back empty.
///
/// Both failure modes of a retract are hard errors: a malformed line here
/// is OUR OWN emitter's output gone wrong, and a failed remove leaves a
/// retracted fact live in the base layer — either way the materialized now
/// would silently lie.
fn write_walk_batch(
    store: &oxigraph::store::Store,
    nq_buffer: &mut String,
    base_final: &mut HashMap<String, char>,
) -> Result<(), String> {
    for (line, op) in base_final.drain() {
        match op {
            '+' => {
                nq_buffer.push_str(&line);
                nq_buffer.push('\n');
            }
            '-' => {
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
    if !nq_buffer.is_empty() {
        let parser = oxigraph::io::RdfParser::from_format(oxigraph::io::RdfFormat::NQuads);
        store
            .load_from_reader(parser, std::io::Cursor::new(nq_buffer.as_bytes()))
            .map_err(|e| format!("one-graph event load failed: {e}"))?;
    }
    nq_buffer.clear();
    Ok(())
}

/// The walk, with its reader and batch size handed in (tests drive a
/// scratch repository and a tiny batch through here).
#[allow(clippy::too_many_arguments)]
pub fn onegraph_walk_engine_with(
    reader: &mut SidecarReader,
    commits: &[WalkCommit],
    store: &oxigraph::store::Store,
    one_graph: &str,
    ctx: &crate::nquad::ResolverContext,
    show_progress: bool,
    clear_first: bool,
    batch_quads: usize,
) -> Result<WalkOutcome, String> {
    let total = commits.len();
    let mut nq_buffer = String::new();
    // Quad lines waiting in `nq_buffer`; with `base_final.len()` it is the
    // size of the next store write.
    let mut pending_quads = 0usize;
    let mut changed_subjects: HashSet<String> = HashSet::new();
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
        // #28: retracts suppressed because an UNTOUCHED sidecar still
        // asserts the same triple (duplicate ids from merge commits or
        // pre-gate history). Suppression is correct — the fact never left
        // the world — but it must be visible, not silent.
        dup_retracts_suppressed: usize,
    }
    let mut acct = DropAccounting::default();
    // Unknown-suffix sidecars warn once per path (the walk visits the same
    // path once per touching commit — repeating the warning is noise).
    let mut warned_unknown: HashSet<String> = HashSet::new();

    // Net base-layer effect per triple across this walk (last op wins).
    let mut base_final: HashMap<String, char> = HashMap::new();

    // Resolve one sidecar's LINES (already read from git) into the set of
    // resolved triple-quad lines. Split from the by-commit reader so the
    // duplicate-id retract guard below resolves blobs by oid through the
    // SAME path — one resolver, no drift between the diff sides and the
    // guard's view of the untouched world.
    let resolve_lines = |lines: &[String],
                         sidecar_path: &str,
                         relpath_str: &str,
                         acct: &mut DropAccounting|
     -> Result<HashSet<String>, String> {
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
            lines,
            relpath_str,
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
        for line in lines {
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
            // counts the same errors, so the walk must too. warn=false: the
            // walk revisits every commit — replaying the save path's live
            // to-dos per visit is the #73 spam (the counts still land).
            acct.resolver_errors += crate::nquad::emit_spo_line_nquads(
                line, &subjects, one_graph, relpath_str, ctx,
                false,
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

    let resolve_sidecar_at = |reader: &mut SidecarReader,
                              commit: &str,
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
        let lines = reader.lines_at(commit, sidecar_path)?;
        // ABSENT (or empty) sidecar = NO anchors (review-critical fix): the
        // File rdf:type used to emit unconditionally even for the verified-
        // empty set of a path absent at this commit — identical on both
        // diff sides, so a file's anchor facts never diffed: a new file
        // under-reported its events and a deletion never retracted its
        // anchors. No sidecar lines, no facts of any kind.
        if lines.is_empty() {
            return Ok(HashSet::new());
        }
        resolve_lines(&lines, sidecar_path, &relpath_str, acct)
    };

    // ── Duplicate-id retract guard state (#28) ──
    // (blob oid, sidecar path) → resolved triples, content-addressed:
    // identical sidecar bytes resolve identically within one walk (the
    // resolution context is constant), so repo-wide guard scans amortize to
    // one resolve per unique sidecar version. Guard scans count into a
    // SCRATCH accounting — those sidecars' lines are counted when their own
    // commits walk; the guard must not inflate the receipt.
    // Dropped at every batch write: a cache, so emptying it changes no
    // answer, and an old sidecar version stops being asked for as the walk
    // moves forward.
    let mut blob_memo: HashMap<(git2::Oid, String), HashSet<String>> = HashMap::new();
    let mut guard_acct = DropAccounting::default();
    // Which documents anchor which Thing: the store's base layer plus every
    // fileId change this walk has made SINCE ITS LAST WRITE. Keyed subject →
    // (file → last op). On a full rebuild the base layer is this walk's own
    // earlier batches (the old graph is cleared below, before the first
    // commit), so the store is consulted there too.
    let file_id_pred = oxigraph::model::NamedNodeRef::new_unchecked(ONEGRAPH_FILE_ID);
    let graph_node = oxigraph::model::NamedNode::new(
        one_graph.trim_start_matches('<').trim_end_matches('>'),
    )
    .map_err(|e| format!("one-graph IRI is not a valid named node: {e}"))?;
    let mut walk_file_ids: HashMap<String, HashMap<String, char>> = HashMap::new();

    // clear_first = full rebuild (store deleted/rebuilt; also the fallback when an
    // incremental resume point turns out invalid, e.g. after history rewrite).
    // clear_first = false is the sync path: the one graph is PERSISTENT and
    // append-only; sync walks only commits newer than the store's newest and
    // appends their events.
    //
    // Every store operation in this walk is a hard error: this graph is the
    // system of record, and "printed a warning but reported success" was the
    // defect class that let a build fail invisibly (review finding A2).
    if clear_first {
        clear_graph_in_batches(store, &graph_node, batch_quads)
            .map_err(|e| format!("one-graph clear (full rebuild) failed: {e}"))?;
    }

    // Write what is pending and forget it. `walk_file_ids` and `blob_memo`
    // go too: after the write the store's base layer answers for the first,
    // and the second is a cache. A full rebuild re-materializes the whole
    // now view, so it keeps no list of changed subjects.
    macro_rules! write_batch {
        () => {{
            if !clear_first {
                changed_subjects.extend(
                    base_final.keys().filter_map(|line| take_term(line).map(|(subject, _)| subject)),
                );
            }
            write_walk_batch(store, &mut nq_buffer, &mut base_final)?;
            walk_file_ids.clear();
            blob_memo.clear();
        }};
    }

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
                resolve_sidecar_at(reader, &c.parent_sha, path, &mut acct, &mut warned_unknown)
                    .map_err(|e| format!("commit {} (old side): {e}", c.sha))?,
            );
        }
        for path in &new_side {
            new_triples.extend(
                resolve_sidecar_at(reader, &c.sha, path, &mut acct, &mut warned_unknown)
                    .map_err(|e| format!("commit {} (new side): {e}", c.sha))?,
            );
        }

        // The diff of resolved worlds IS the event stream for this commit.
        // base_final tracks each touched triple's NET state across this walk
        // (commits are processed oldest→newest, so the last op wins) — it
        // becomes the base-layer mutation set after the walk.
        //
        // Retract guard (#28): a retract candidate may still be asserted by
        // an UNTOUCHED sidecar — duplicate ids enter history through merge
        // commits (which bypass the pre-commit identity gate), pre-gate
        // history, and stale .lex/extract/ subtrees; and a document's other
        // sidecars carry its File node's anchor too. The per-commit diff
        // sees only touched paths, so deleting one duplicate would emit a
        // false death event AND drop the survivor's facts from the base
        // layer (state parity can't catch it: base and derived go wrong
        // together).
        //
        // Every emitted fact's subject is its document's File node or the
        // Thing that document anchors (emit_spo_line_nquads,
        // emit_file_anchor_nquads). So the only untouched sidecars that can
        // still assert a candidate belong to the candidate subject's own
        // document (a File node) or to a document whose fileId edge points
        // from the candidate subject (a Thing). Those few are read and
        // resolved; the rest of the repo is not (#15 — scanning the whole
        // extract tree made every edit cost the size of the repo).
        let retracts: Vec<&String> = old_triples.difference(&new_triples).collect();
        let mut still_live: HashSet<&String> = HashSet::new();
        if !retracts.is_empty() {
            let touched_any: HashSet<&str> = old_side.union(&new_side).copied().collect();
            let mut docs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            let mut subjects_seen: HashSet<String> = HashSet::new();
            for cand in &retracts {
                let Some((subject, _)) = take_term(cand) else { continue };
                if !subjects_seen.insert(subject.clone()) {
                    continue;
                }
                let iri = subject.trim_start_matches('<').trim_end_matches('>');
                if let Some(doc) = file_iri_document(iri) {
                    docs.insert(doc);
                }
                let mut files: HashMap<String, char> = HashMap::new();
                if let Ok(s_node) = oxigraph::model::NamedNodeRef::new(iri) {
                    for q in store.quads_for_pattern(
                        Some(s_node.into()),
                        Some(file_id_pred),
                        None,
                        Some(graph_node.as_ref().into()),
                    ) {
                        let q = q.map_err(|e| format!("fileId lookup failed: {e}"))?;
                        files.insert(q.object.to_string(), '+');
                    }
                }
                if let Some(changed) = walk_file_ids.get(&subject) {
                    for (f, op) in changed {
                        files.insert(f.clone(), *op);
                    }
                }
                for (f, op) in files {
                    if op == '+'
                        && let Some(doc) = file_iri_document(f.trim_start_matches('<').trim_end_matches('>'))
                    {
                        docs.insert(doc);
                    }
                }
            }
            'scan: for doc in &docs {
                for suffix in SPO_EXTRACTOR_SUFFIXES {
                    let path = format!(".lex/extract/{doc}.{suffix}.spo");
                    if touched_any.contains(path.as_str()) {
                        continue;
                    }
                    let Some(oid) = reader.blob_at(&c.sha, &path)? else { continue };
                    let key = (oid, path.clone());
                    if !blob_memo.contains_key(&key) {
                        let lines = reader.blob_lines(oid)?;
                        let triples = if lines.is_empty() {
                            HashSet::new()
                        } else {
                            resolve_lines(&lines, &path, doc, &mut guard_acct)?
                        };
                        blob_memo.insert(key.clone(), triples);
                    }
                    let set = &blob_memo[&key];
                    for cand in &retracts {
                        if set.contains(*cand) {
                            still_live.insert(*cand);
                            if still_live.len() == retracts.len() {
                                break 'scan; // every candidate accounted for
                            }
                        }
                    }
                }
            }
        }
        for line in retracts {
            if still_live.contains(line) {
                // Still asserted by an untouched file: the fact never left
                // the world, so there is no event and no base change. It
                // retracts when its LAST asserting file drops it.
                acct.dup_retracts_suppressed += 1;
                continue;
            }
            events_seen += 1;
            if let Some(quads) = onegraph_event(line, '-', &c.sha, one_graph) {
                for q in quads { nq_buffer.push_str(&q); nq_buffer.push('\n'); pending_quads += 1; }
                events_emitted += 1;
                note_file_id(&mut walk_file_ids, line, '-');
                base_final.insert(line.clone(), '-');
            }
            // The guard above has already judged this whole commit, so a
            // write part-way through its events changes nothing it reads —
            // and one commit that asserts a whole tree stays bounded too.
            if pending_quads + base_final.len() >= batch_quads {
                write_batch!();
                pending_quads = 0;
            }
        }
        for line in new_triples.difference(&old_triples) {
            events_seen += 1;
            if let Some(quads) = onegraph_event(line, '+', &c.sha, one_graph) {
                for q in quads { nq_buffer.push_str(&q); nq_buffer.push('\n'); pending_quads += 1; }
                events_emitted += 1;
                note_file_id(&mut walk_file_ids, line, '+');
                base_final.insert(line.clone(), '+');
            }
            if pending_quads + base_final.len() >= batch_quads {
                write_batch!();
                pending_quads = 0;
            }
        }
    }

    // ─── Base layer = the MATERIALIZED NOW (ruled contract) ───
    // net-assert → the plain triple is (re)asserted alongside its events;
    // net-retract → the plain triple is REMOVED from the graph. Commits are
    // walked oldest→newest and the last op on a triple wins, so applying
    // each batch's net effect in order ends where applying the whole walk's
    // net effect would. The last (for an append, the only) write:
    write_batch!();

    // Completeness accounting (BUG 4). Malformed lines hard-fail above;
    // what remains countable is emitter-side drops and unknown suffixes.
    if acct.empty_object > 0 || acct.resolver_other > 0 || acct.unknown_suffix > 0 || acct.resolver_errors > 0 {
        eprintln!(
            "  one-graph accounting: {} line(s) read — empty-value (no fact): {}, resolver-other: {}, unknown-suffix sidecar(s): {}, resolver error(s): {}",
            acct.lines_in, acct.empty_object, acct.resolver_other,
            acct.unknown_suffix, acct.resolver_errors
        );
    }
    // #28 receipt: suppressed duplicate retracts are correct behavior but
    // never silent — they mean duplicate Thing ids exist(ed) in history.
    if acct.dup_retracts_suppressed > 0 {
        eprintln!(
            "  one-graph: {} retract(s) suppressed — the same fact is still asserted \
             by an untouched file (duplicate ids: merge commits bypass the identity \
             gate, and pre-gate history carries them). A fact retracts when its \
             LAST asserting file drops it. This is history accounting, not a to-do.",
            acct.dup_retracts_suppressed
        );
    }
    if show_progress && total > 0 {
        eprintln!(" done");
    }

    Ok(WalkOutcome { events_seen, events_emitted, changed_subjects })
}

/// What one walk did: the summary counts, and every subject whose
/// base-layer (current-state) facts it changed.
pub struct WalkOutcome {
    pub events_seen: usize,
    pub events_emitted: usize,
    /// Bracketed subject terms (`<iri>`), deduplicated. EMPTY after a full
    /// rebuild: everything changed, the caller re-materializes the whole
    /// now view, and the list would grow with the whole of history.
    pub changed_subjects: HashSet<String>,
}

/// The Thing → File edge the anchor facts carry (git-lex:fileId).
const ONEGRAPH_FILE_ID: &str = "https://repolex.ai/ontology/git-lex/fileId";

/// Record a fileId event from the walk: `<Thing> fileId <File>` lines only.
fn note_file_id(walk_file_ids: &mut HashMap<String, HashMap<String, char>>, line: &str, op: char) {
    let Some((subject, rest)) = take_term(line) else { return };
    let Some((predicate, rest)) = take_term(rest) else { return };
    if predicate.trim_start_matches('<').trim_end_matches('>') != ONEGRAPH_FILE_ID {
        return;
    }
    let Some((file, _)) = take_term(rest) else { return };
    walk_file_ids.entry(subject).or_default().insert(file, op);
}

/// The repo-relative document path a File node names, or None when the IRI
/// is not a File node. Inverse of `file_iri(uri_encode_path(path))`: the
/// encoder escapes `%` itself, so percent-decoding is exact.
fn file_iri_document(iri: &str) -> Option<String> {
    let encoded = iri.strip_prefix(crate::git::FILE_BASE)?;
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
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
                if s[end..].starts_with("^^<")
                    && let Some(dt_end) = s[end + 2..].find('>') {
                        end = end + 2 + dt_end + 1;
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

    /// The orphan sweep removes exactly the sidecars whose source is gone:
    /// live-source sidecars stay, unknown .spo suffixes stay (never guess
    /// an attribution), and the orphan's file is actually deleted.
    #[test]
    fn orphan_sweep_removes_only_sourceless_sidecars() {
        let root = std::env::temp_dir().join(format!(
            "gitlex-orphan-sweep-test-{}",
            std::process::id()
        ));
        let extract = root.join(".lex/extract/Soul/Note");
        std::fs::create_dir_all(&extract).unwrap();
        std::fs::create_dir_all(root.join("Soul/Note")).unwrap();
        // Live source + its sidecar → kept.
        std::fs::write(root.join("Soul/Note/alive.md"), "x").unwrap();
        std::fs::write(extract.join("alive.md.fm.spo"), "x").unwrap();
        // No source → orphan, removed (both extractor suffixes).
        std::fs::write(extract.join("gone.md.fm.spo"), "x").unwrap();
        std::fs::write(extract.join("gone.md.md.spo"), "x").unwrap();
        // Unknown suffix → untouched even with no source.
        std::fs::write(extract.join("gone.md.mystery.spo"), "x").unwrap();

        let removed = remove_orphaned_sidecars(&root);
        assert_eq!(
            removed,
            vec![
                ".lex/extract/Soul/Note/gone.md.fm.spo".to_string(),
                ".lex/extract/Soul/Note/gone.md.md.spo".to_string(),
            ]
        );
        assert!(extract.join("alive.md.fm.spo").exists());
        assert!(!extract.join("gone.md.fm.spo").exists());
        assert!(extract.join("gone.md.mystery.spo").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    // ─── v1 write-gate (validate_sidecar_v1) ───────────────────────────────

    #[test]
    fn gate_accepts_all_three_blessed_shapes() {
        let content = "\
copia.Texture.textureId | hasValue | self
Copia/Texture/self.md | linksTo | Soul/Journal/day-15.md
md.externalLink | hasValue | https://github.com/repolex-ai/git-lex
soul.Memory.category | hasValue | \n";
        // Includes the legal empty-object hasValue on the last line.
        assert!(validate_sidecar_v1(content).is_empty());
    }

    #[test]
    fn gate_accepts_pipe_inside_object() {
        // splitn(3): the object may contain " | " verbatim.
        let content = "soul.Note.title | hasValue | a | b | c\n";
        assert!(validate_sidecar_v1(content).is_empty());
    }

    #[test]
    fn gate_rejects_wrong_field_count() {
        let errs = validate_sidecar_v1("         uad/Squaddie/lspy\n");
        assert_eq!(errs.len(), 1);
        assert!(errs[0].1.contains("malformed line"));
    }

    #[test]
    fn gate_rejects_unknown_operator() {
        // The dormant jsonl session extractor's shape must not pass.
        let errs = validate_sidecar_v1("session-abc123 | isA | session\n");
        assert_eq!(errs.len(), 1);
        assert!(errs[0].1.contains("closed vocabulary"));
    }

    #[test]
    fn gate_rejects_control_characters_in_object() {
        let errs = validate_sidecar_v1("soul.Note.title | hasValue | tab\there\n");
        assert_eq!(errs.len(), 1);
        assert!(errs[0].1.contains("U+0009"));
    }

    #[test]
    fn gate_accepts_both_link_forms_rejects_empty() {
        // ONE link law (Rob-ruled 2026-08-08): targets are repo-root-
        // relative; a historical leading `/` names the same path and is
        // normalized at emit — the gate tolerates both, rejects only the
        // truly-broken (empty target).
        let errs = validate_sidecar_v1(
            "A.md | linksTo | /Soul/Note/x.md\nA.md | linksTo | Soul/Note/y.md\nB.md | linksTo |  \n");
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].0, 3);
        assert!(errs[0].1.contains("empty target"));
    }

    #[test]
    fn gate_reports_line_numbers_and_blank_lines() {
        let errs = validate_sidecar_v1("a | hasValue | ok\n\nbroken line\n");
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

    /// The retract guard maps a File node back to its document; the map
    /// must invert the encoder exactly, or the guard reads the wrong file.
    #[test]
    fn file_iri_document_inverts_the_encoder() {
        for path in ["Soul/Note/plain.md", "a b/100%.md", "Café/naïve \"q\".md", "x%41.md"] {
            let iri = crate::git::file_iri(&crate::nquad::uri_encode_path(path));
            assert_eq!(file_iri_document(&iri).as_deref(), Some(path), "{iri}");
        }
        assert_eq!(file_iri_document("https://repolex.ai/soul/Note/x"), None);
    }

    /// Only `<Thing> fileId <File>` lines feed the guard's anchor map.
    #[test]
    fn note_file_id_tracks_only_file_id_lines() {
        let mut m: HashMap<String, HashMap<String, char>> = HashMap::new();
        note_file_id(
            &mut m,
            "<https://repolex.ai/soul/Note/x> <https://repolex.ai/ontology/git-lex/fileId> <https://repolex.ai/git-lex/File/Soul/Note/x.md> <g> .",
            '+',
        );
        note_file_id(
            &mut m,
            "<https://repolex.ai/soul/Note/x> <https://repolex.ai/ontology/git-lex/title> \"t\" <g> .",
            '-',
        );
        assert_eq!(m.len(), 1);
        assert_eq!(
            m["<https://repolex.ai/soul/Note/x>"]["<https://repolex.ai/git-lex/File/Soul/Note/x.md>"],
            '+'
        );
    }

    /// Each commit's records land in its own block, in input order —
    /// including a commit with no sidecar changes (an empty block).
    #[test]
    fn diff_tree_stdin_output_splits_per_commit() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let c = "c".repeat(40);
        let raw = format!(
            "{a}\0M\0.lex/extract/x.md.fm.spo\0{b}\0{c}\0R090\0.lex/extract/o.md.fm.spo\0.lex/extract/n.md.fm.spo\0"
        );
        let shas = vec![a.clone(), b.clone(), c.clone()];
        let blocks = split_diff_tree_stdin(&raw, &shas).unwrap();
        assert_eq!(blocks[0], "M\0.lex/extract/x.md.fm.spo\0");
        assert_eq!(blocks[1], "");
        assert_eq!(parse_name_status_z(&blocks[2]).1.len(), 1);
        // A commit git never answered for is an error, not an empty diff.
        let short = format!("{a}\0M\0.lex/extract/x.md.fm.spo\0");
        assert!(split_diff_tree_stdin(&short, &shas).is_err());
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

/// A full rebuild writes in batches (#15). Wherever the batches fall, the
/// store must end as the store one write would have made — and as the
/// store a run of appends makes, which is the claim the batches rest on.
#[cfg(test)]
mod batched_walk_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn git(root: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn note(id: &str, title: &str) -> String {
        format!(
            "soul.Note.id | hasValue | <soul/Note/{id}>\n\
             soul.Note.noteId | hasValue | {id}\n\
             soul.Note.title | hasValue | {title}\n"
        )
    }

    /// One commit: write (`Some`) or delete (`None`) each sidecar, and hand
    /// back the WalkCommit the collector would have built for it.
    fn commit(root: &Path, parent: &str, changes: &[(&str, Option<String>)]) -> WalkCommit {
        let mut touched = Vec::new();
        for (doc, content) in changes {
            let path = format!(".lex/extract/{doc}.fm.spo");
            let full = root.join(&path);
            match content {
                Some(text) => {
                    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
                    std::fs::write(&full, text).unwrap();
                }
                None => std::fs::remove_file(&full).unwrap(),
            }
            touched.push(path);
        }
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", "c"]);
        WalkCommit {
            sha: git(root, &["rev-parse", "HEAD"]),
            parent_sha: parent.to_string(),
            touched,
            renames: Vec::new(),
        }
    }

    /// A short history with every kind of change the walk tells apart: new
    /// facts, a changed value, a deleted document, a moved one (its fileId
    /// retracts and re-asserts), a fact that comes back after it left, and
    /// two documents claiming one id, one of which is then deleted (the
    /// retract guard must find the survivor).
    fn history(tag: &str) -> (PathBuf, Vec<WalkCommit>) {
        let root = std::env::temp_dir().join(format!("gitlex-batched-walk-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        let mut commits: Vec<WalkCommit> = Vec::new();
        let mut add = |changes: &[(&str, Option<String>)]| {
            let parent = commits.last().map(|c| c.sha.clone()).unwrap_or_else(|| EMPTY_TREE_SHA.to_string());
            commits.push(commit(&root, &parent, changes));
        };
        add(&[("Soul/Note/a.md", Some(note("a", "first"))), ("Soul/Note/b.md", Some(note("b", "bee")))]);
        add(&[("Soul/Note/a.md", Some(note("a", "second")))]);
        add(&[("Soul/Note/c.md", Some(note("c", "sea"))), ("Soul/Note/b.md", None)]);
        add(&[("Soul/Note/a.md", None), ("Soul/Note/moved/a.md", Some(note("a", "second")))]);
        add(&[("Soul/Note/b.md", Some(note("b", "bee"))), ("Soul/Note/c.md", Some(note("c", "ocean")))]);
        add(&[("Soul/Note/twin.md", Some(note("c", "ocean")))]);
        add(&[("Soul/Note/c.md", None)]);
        add(&[("Soul/Note/a.md", Some(note("a", "third"))), ("Soul/Note/moved/a.md", None)]);
        (root, commits)
    }

    fn ctx() -> crate::nquad::ResolverContext {
        let mut kit_namespaces = HashMap::new();
        kit_namespaces.insert("soul".to_string(), "https://repolex.ai/ontology/soul/".to_string());
        crate::nquad::ResolverContext {
            files: Vec::new(),
            path_index: HashSet::new(),
            obj_props: HashSet::new(),
            prop_datatypes: HashMap::new(),
            declared_props: HashSet::new(),
            kit_namespaces,
            ref_ranges: HashMap::new(),
            prop_iris: HashMap::new(),
            deprecated_props: HashMap::new(),
            domain_open_props: HashMap::new(),
        }
    }

    fn one_graph() -> String {
        format!("<{LEXHISTORY_GRAPH_IRI}>")
    }

    fn walk(root: &Path, store: &oxigraph::store::Store, commits: &[WalkCommit], clear_first: bool, batch_quads: usize) -> WalkOutcome {
        let mut reader = SidecarReader::open_at(root).unwrap();
        onegraph_walk_engine_with(&mut reader, commits, store, &one_graph(), &ctx(), false, clear_first, batch_quads)
            .expect("walk succeeds")
    }

    fn quads(store: &oxigraph::store::Store) -> Vec<String> {
        let mut all: Vec<String> = store.iter().map(|q| q.unwrap().to_string()).collect();
        all.sort();
        all
    }

    #[test]
    fn a_rebuild_in_batches_is_the_rebuild_in_one_piece() {
        let (root, commits) = history("batches");
        let whole = oxigraph::store::Store::new().unwrap();
        let outcome = walk(&root, &whole, &commits, true, usize::MAX);
        let expected = quads(&whole);
        // The history really does exercise retracts and the guard.
        assert!(outcome.events_emitted > 20, "only {} events", outcome.events_emitted);
        assert!(expected.iter().any(|q| q.contains("retractedIn")), "no retract in the fixture");
        assert!(
            expected.iter().any(|q| q.contains("/title> \"ocean\"") && !q.contains("<<(")),
            "the twin's facts must survive its double's deletion: {expected:#?}"
        );

        // Batch sizes from one quad (a write after every event, so every
        // commit is split) up past the whole walk.
        for batch_quads in [1, 2, 3, 5, 7, 16, 40, 1000] {
            let batched = oxigraph::store::Store::new().unwrap();
            let got = walk(&root, &batched, &commits, true, batch_quads);
            assert_eq!(got.events_seen, outcome.events_seen, "batch of {batch_quads}");
            assert_eq!(got.events_emitted, outcome.events_emitted, "batch of {batch_quads}");
            assert_eq!(quads(&batched), expected, "batch of {batch_quads} quads built a different store");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rebuild_clears_what_the_graph_held_before() {
        let (root, commits) = history("clears");
        let fresh = oxigraph::store::Store::new().unwrap();
        walk(&root, &fresh, &commits, true, 5);

        let stale = oxigraph::store::Store::new().unwrap();
        let junk: String = (0..12)
            .map(|i| format!("<https://e/s{i}> <https://e/p> \"old\" {} .\n", one_graph()))
            .chain(["<https://e/s> <https://e/p> \"kept\" <https://e/other-graph> .\n".to_string()])
            .collect();
        stale.load_from_reader(oxigraph::io::RdfFormat::NQuads, junk.as_bytes()).unwrap();
        walk(&root, &stale, &commits, true, 5);

        let mut expected = quads(&fresh);
        expected.push("<https://e/s> <https://e/p> \"kept\" <https://e/other-graph>".to_string());
        expected.sort();
        assert_eq!(quads(&stale), expected);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The claim the batches rest on: a batch is an append onto what came
    /// before. Appending the history in two syncs ends where a rebuild ends.
    #[test]
    fn appends_end_where_a_rebuild_ends() {
        let (root, commits) = history("appends");
        let rebuilt = oxigraph::store::Store::new().unwrap();
        walk(&root, &rebuilt, &commits, true, usize::MAX);
        for split in 1..commits.len() {
            let appended = oxigraph::store::Store::new().unwrap();
            walk(&root, &appended, &commits[..split], true, usize::MAX);
            let outcome = walk(&root, &appended, &commits[split..], false, usize::MAX);
            assert_eq!(quads(&appended), quads(&rebuilt), "split before commit {split}");
            assert!(!outcome.changed_subjects.is_empty(), "an append reports what it changed");
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
