//! History-graph spike — read-only walker over .spo changes across commits.
//!
//! This module is **exploratory** and intentionally decoupled from the main
//! pipeline. It does not write to the oxigraph store, does not emit RDF, and
//! does not build annotated triple terms. The point is to answer a single
//! question: *can we walk git history commit-by-commit and see meaningful
//! per-commit changes to the `.spo` files that frontmatter extraction
//! produces, and what shape does that data actually take?*
//!
//! Design context: squad-repo `situation/2026-04-09-history-graph-temporal-
//! ledger.md`. The real implementation will supersede this module; the spike
//! exists to inform that design, not to ship.
//!
//! ## Architecture
//!
//! The walker is split into layers so each piece can be tested in isolation:
//!
//! 1. **git runner** — shells out to `git rev-list` and `git diff-tree`,
//!    returns raw strings. The only layer that touches the filesystem.
//! 2. **unified-diff parser** — pure function over strings, turns diff output
//!    into `SpikeEvent` records tagged with file path and op.
//! 3. **dedup normalizer** — pure function that canonicalizes event lines
//!    by dropping extraction-id hash prefixes (Finding 1 from the first
//!    spike run: `extraction.log.spo` lines carry a content-hash first
//!    field that churns on every content edit).
//! 4. **sanity sweeper** — pure function that walks a slice of events and
//!    flags inconsistencies without throwing. Designed to LOG what's weird,
//!    not to crash. The inconsistency stream is how we build a picture of
//!    the real-world mess before committing to a data model.
//! 5. **reporter** — drives the pipeline and prints the human-readable log.
//!
//! Layers 2, 3, and 4 are pure and have unit tests in `#[cfg(test)] mod tests`
//! at the bottom of this file. Layer 1 is thin enough that integration-style
//! testing against a real git repo is more useful than mocking; that can
//! come later if this spike graduates into a real feature.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, exit};

// spike: history-graph walker imports
use oxigraph::sparql::SparqlEvaluator;

// ════════════════════════════════════════════════════════════════════════════
// Public surface
// ════════════════════════════════════════════════════════════════════════════

/// Caller-provided options for `run`. Kept as a struct so adding new knobs
/// doesn't churn the main.rs match arm every time.
pub struct Options {
    pub limit: usize,
    pub only_changes: bool,
    pub dedup: bool,
    pub inconsistency_log: Option<String>,
    /// Print canonical URIs alongside event lines. Requires the walker to be
    /// scoped to `.lex/extract/**/*.spo` only (which it now is by default —
    /// `extraction.log.spo` is excluded because it lives outside `extract/`).
    pub canonical: bool,
}

/// Main entry point called from `main.rs`. Performs the walk and prints both
/// the event log (stdout) and the inconsistency report (stderr or file).
pub fn run(opts: Options) {
    let root = find_git_root().expect("not in a git repo");
    std::env::set_current_dir(&root).expect("failed to cd to repo root");

    // Repo name drives the canonical-URI base prefix. Defaults to "unknown" if
    // the repo.yml is missing or unreadable — the walker still runs, URIs just
    // look a little generic.
    let repo_name = read_repo_name(&root).unwrap_or_else(|| "unknown".to_string());

    let commits = collect_commits(opts.limit);
    let total = commits.len();
    eprintln!("spike: walking {} commit(s) (oldest → newest of that slice)", total);
    eprintln!("spike: repo = {} (name: {})", root.display(), repo_name);
    eprintln!(
        "spike: dedup={}, only_changes={}, canonical={}",
        opts.dedup, opts.only_changes, opts.canonical
    );
    eprintln!("spike: ────────────────────────────────────────────");

    // Sanity sweeper state — accumulates findings as we walk. Reported at
    // the end so the main event log stays linear and readable.
    let mut sweeper = InconsistencySweeper::new();

    // Stats rolled up across the whole walk.
    let mut commits_with_changes = 0usize;
    let mut total_added_raw = 0usize;
    let mut total_removed_raw = 0usize;
    let mut total_added_dedup = 0usize;
    let mut total_removed_dedup = 0usize;

    for c in &commits {
        // Sweep the raw events BEFORE dedup. Dedup changes the event count
        // and could mask anomalies we want to see.
        sweeper.sweep_commit(c);

        let displayed: Vec<&SpikeEvent> = if opts.dedup {
            dedup_events(&c.events)
        } else {
            c.events.iter().collect()
        };

        let has_renames = !c.renames.is_empty();
        if opts.only_changes && displayed.is_empty() && !has_renames {
            continue;
        }
        if displayed.is_empty() && !has_renames {
            println!("{}  {}  {}  (no .spo changes)", c.short_sha, c.date, c.subject);
            continue;
        }
        commits_with_changes += 1;

        let raw_added = c.events.iter().filter(|e| e.op == '+').count();
        let raw_removed = c.events.iter().filter(|e| e.op == '-').count();
        total_added_raw += raw_added;
        total_removed_raw += raw_removed;

        let dd_added = displayed.iter().filter(|e| e.op == '+').count();
        let dd_removed = displayed.iter().filter(|e| e.op == '-').count();
        total_added_dedup += dd_added;
        total_removed_dedup += dd_removed;

        println!(
            "\n{}  {}  {}  <{}>",
            c.short_sha, c.date, c.subject, c.author
        );
        if opts.dedup {
            println!(
                "  {} raw event(s) → {} after dedup (+{} -{}), {} rename(s)",
                c.events.len(),
                displayed.len(),
                dd_added,
                dd_removed,
                c.renames.len(),
            );
        } else {
            println!(
                "  {} event(s): +{} -{}, {} rename(s)",
                c.events.len(),
                raw_added,
                raw_removed,
                c.renames.len(),
            );
        }
        for r in &c.renames {
            println!(
                "  R{}%  {} → {}",
                r.similarity, r.old_path, r.new_path
            );
        }
        for ev in displayed {
            if opts.canonical {
                // Print both the canonical URI and the reconstructed
                // (subject, predicate, object) triple so the human reader
                // can see what the hash means. If the line is unparseable,
                // print the raw content with a marker — the sweeper has
                // already flagged it separately.
                match (
                    canonical_uri(&repo_name, &ev.path, &ev.line),
                    reconstructed_triple(&ev.path, &ev.line),
                ) {
                    (Some(uri), Some((s, p, o))) => {
                        println!("  {}  {}", ev.op, uri);
                        println!("       {}  {}  {}", s, p, o);
                    }
                    _ => println!(
                        "  {}  {}  {} (UNPARSEABLE)",
                        ev.op, ev.path, ev.line
                    ),
                }
            } else {
                println!("  {}  {}  {}", ev.op, ev.path, ev.line);
            }
        }
    }

    eprintln!("spike: ────────────────────────────────────────────");
    eprintln!(
        "spike: {} commits walked, {} with .spo changes",
        total, commits_with_changes
    );
    eprintln!(
        "spike: raw    +{} -{}  ({} net)",
        total_added_raw,
        total_removed_raw,
        total_added_raw as i64 - total_removed_raw as i64,
    );
    if opts.dedup {
        eprintln!(
            "spike: dedup  +{} -{}  ({} net)",
            total_added_dedup,
            total_removed_dedup,
            total_added_dedup as i64 - total_removed_dedup as i64,
        );
        let hash_churn_add = total_added_raw.saturating_sub(total_added_dedup);
        let hash_churn_rem = total_removed_raw.saturating_sub(total_removed_dedup);
        eprintln!(
            "spike: churn  {} adds and {} removes were extraction-id noise",
            hash_churn_add, hash_churn_rem,
        );
    }

    // Emit the sweeper report last so the stats immediately above it are
    // easy to find without scrolling.
    sweeper.report(opts.inconsistency_log.as_deref());
}

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

/// Run `git rev-list --topo-order --reverse [--max-count=N] HEAD` and return
/// the resulting SHAs as a vector. Note the quirk (called out in the spike
/// report): with `--max-count=N`, git takes the most recent N commits from
/// HEAD backwards and only *then* applies `--reverse`, so you get the slice
/// of the N most-recent commits presented oldest-first-within-slice. This
/// is usually NOT what "first N commits from repo root" would mean. The real
/// walker implementation will need to decide which semantics it wants; for
/// the spike we document the quirk and move on.
fn rev_list_head(limit: usize) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "rev-list".into(),
        "--topo-order".into(),
        "--reverse".into(),
        "HEAD".into(),
    ];
    if limit > 0 {
        args.push(format!("--max-count={}", limit));
    }
    let out = Command::new("git").args(&args).output().expect("git rev-list failed");
    if !out.status.success() {
        eprintln!("git rev-list failed: {}", String::from_utf8_lossy(&out.stderr));
        exit(1);
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Collect per-commit metadata and diff events. One git process per commit;
/// wasteful but the spike's only goal is correctness, not speed.
fn collect_commits(limit: usize) -> Vec<SpikeCommit> {
    let shas = rev_list_head(limit);
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
    // the old `.lex/extraction.log.spo` file was a leftover from an earlier
    // attempt and is not part of the real knowledge ledger. Everything that
    // matters lives under `.lex/extract/` as per-document sidecars with names
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

/// Ask git for the staged-but-not-yet-committed change set on `.md` files,
/// filtered to the tracked content tree (no `.lex/**`). Returns the raw
/// diff status output, which `parse_staged_md_changes` then parses.
///
/// Uses `diff --cached` (index vs HEAD) because this function is called
/// from the pre-commit hook, where changes have been staged by the hook
/// caller (via `git add` or `git lex save`'s explicit `git add -A`) but
/// not yet committed.
///
/// `-M50%` turns on rename detection at 50% similarity — same threshold
/// the diff-tree walker uses, for consistency.
fn git_staged_md_changes() -> Option<String> {
    let out = Command::new("git")
        .args([
            "diff",
            "--cached",
            "--name-status",
            "-M50%",
            "-z",
            "--",
            "*.md",
            ":!.lex/",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // `-z` gives NUL-separated records; we want lossy UTF-8 because paths
    // might not be strict UTF-8 but we'll still see them correctly.
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
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
        Some(r) => r,
        None => {
            // No staged .md changes, or git command failed. Either way
            // nothing to clean up — this is the common case when commits
            // don't touch the content tree.
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
// Layer 3: dedup normalizer (pure)
// ════════════════════════════════════════════════════════════════════════════

/// Given a slice of `SpikeEvent`s, return a new `Vec<&SpikeEvent>` containing
/// just the semantically meaningful ones — with `extraction.log.spo`
/// hash-prefix churn collapsed.
///
/// The story: `.lex/extraction.log.spo` lines have the shape
/// `<content-hash>/<path> | <subject> | <predicate> | <object>`. The content
/// hash is a first-8-hex-digits fingerprint of the source document's
/// content. Any edit to the source document — even a typo fix that doesn't
/// change a single triple — rotates the hash and makes every line in that
/// document appear to have been removed and re-added.
///
/// Dedup strategy:
/// 1. For each `.lex/extraction.log.spo` event, split the line on " | " and
///    drop the first field (the `<hash>/<path>` prefix), keeping only
///    `<subject> | <predicate> | <object>` as a canonical key.
/// 2. Pair up `+` and `-` events with the same canonical key inside the
///    same commit — those are the hash-churn artifacts.
/// 3. Return everything except the paired-off events.
///
/// Events for non-`extraction.log.spo` files pass through unchanged — those
/// are the per-document `.fm.spo` sidecars, which don't carry hash prefixes
/// and don't need dedup.
///
/// This is deliberately conservative: we only collapse when we have both a
/// `+` and a `-` with identical canonical keys. A standalone `+` or `-`
/// survives, so real additions and real removals are never hidden.
pub fn dedup_events(events: &[SpikeEvent]) -> Vec<&SpikeEvent> {
    // Map from canonical-key → (pending additions, pending removals) as
    // vectors of indices into the input. We walk the events once to build
    // the index, then walk again to decide who survives.
    let mut log_adds: HashMap<String, Vec<usize>> = HashMap::new();
    let mut log_rems: HashMap<String, Vec<usize>> = HashMap::new();

    for (i, ev) in events.iter().enumerate() {
        if !is_extraction_log(&ev.path) {
            continue;
        }
        if let Some(key) = canonical_log_key(&ev.line) {
            match ev.op {
                '+' => log_adds.entry(key).or_default().push(i),
                '-' => log_rems.entry(key).or_default().push(i),
                _ => {}
            }
        }
    }

    // Figure out which indices are "churn" (paired in both directions).
    let mut churn = std::collections::HashSet::new();
    for (key, add_indices) in &log_adds {
        if let Some(rem_indices) = log_rems.get(key) {
            // Pair up as many as possible — if there are 3 adds and 2 removes
            // for the same key, 2 of each are churn and 1 addition survives.
            let n_pair = add_indices.len().min(rem_indices.len());
            for idx in add_indices.iter().take(n_pair) {
                churn.insert(*idx);
            }
            for idx in rem_indices.iter().take(n_pair) {
                churn.insert(*idx);
            }
        }
    }

    events
        .iter()
        .enumerate()
        .filter(|(i, _)| !churn.contains(i))
        .map(|(_, ev)| ev)
        .collect()
}

/// Is this path the extraction log (single aggregated file)? We check by
/// suffix-matching because the log lives at `.lex/extraction.log.spo`
/// relative to the repo root but diff paths come through unprefixed.
fn is_extraction_log(path: &str) -> bool {
    path.ends_with(".lex/extraction.log.spo") || path == ".lex/extraction.log.spo"
}

/// Normalize an `extraction.log.spo` line into a canonical dedup key by
/// dropping the first pipe-delimited field (which contains the content-hash
/// prefix). Returns `None` if the line doesn't have at least 2 pipe fields,
/// which would be a malformed log entry the sweeper also cares about.
fn canonical_log_key(line: &str) -> Option<String> {
    // Lines look like: `<hash>/<path> | <subject> | <predicate> | <object>`
    // We drop everything up to and including the FIRST ` | ` delimiter.
    let idx = line.find(" | ")?;
    Some(line[(idx + 3)..].to_string())
}

// ════════════════════════════════════════════════════════════════════════════
// Layer 4: sanity sweeper (inconsistency logger)
// ════════════════════════════════════════════════════════════════════════════

/// Accumulates inconsistencies noticed during the walk. The point is to
/// surface weirdness early so design decisions can account for it, not to
/// fail loudly on first sight. Each finding is a one-line message tagged
/// with the commit it came from.
struct InconsistencySweeper {
    findings: Vec<Finding>,
    counts: HashMap<FindingKind, usize>,
}

#[derive(Debug, Clone)]
struct Finding {
    commit: String,
    kind: FindingKind,
    detail: String,
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
enum FindingKind {
    /// A `.fm.spo` line did not match the expected `a | b | c` three-pipe-field
    /// format. Flagged because the real walker will want to parse these
    /// lines into structured triples.
    MalformedFmSpoLine,
    /// A `.lex/extraction.log.spo` line did not have at least four pipe
    /// fields (`<hash>/<path>` + subject + predicate + object). Similar
    /// reason as above but the format is different.
    MalformedLogSpoLine,
    /// Blank-node identifier (`_:xxx`) encountered anywhere. The spike
    /// report flagged this as worth confirming across the whole corpus —
    /// if blank nodes are real and common, the dedup/diff strategy needs
    /// to be aware of them.
    BlankNode,
    /// A diff-path with embedded quoting (" or \") suggests the file path
    /// has spaces or special characters. The spike's path parser doesn't
    /// handle quoting yet.
    QuotedDiffPath,
    /// An `extraction.log.spo` event with a canonical key that had both +
    /// and - in the same commit — i.e. pure hash-prefix churn, no semantic
    /// change. Reported as a CHURN count rather than as individual findings
    /// so the report doesn't drown in them.
    ExtractionIdChurn,
}

impl InconsistencySweeper {
    fn new() -> Self {
        Self {
            findings: Vec::new(),
            counts: HashMap::new(),
        }
    }

    fn add(&mut self, commit: &str, kind: FindingKind, detail: impl Into<String>) {
        *self.counts.entry(kind).or_insert(0) += 1;
        // We only KEEP individual findings for the unique-ish categories.
        // Churn is counted but not stored per-instance (it would flood the
        // report otherwise).
        if !matches!(kind, FindingKind::ExtractionIdChurn) {
            self.findings.push(Finding {
                commit: commit.to_string(),
                kind,
                detail: detail.into(),
            });
        }
    }

    /// Walk a commit's events and flag anything that looks weird.
    fn sweep_commit(&mut self, c: &SpikeCommit) {
        // Count up extraction-id churn for the summary. We do this by
        // re-running the dedup logic and measuring what got dropped.
        let kept = dedup_events(&c.events);
        let dropped = c.events.len() - kept.len();
        for _ in 0..dropped {
            *self.counts.entry(FindingKind::ExtractionIdChurn).or_insert(0) += 1;
        }

        for ev in &c.events {
            // Check for quoted-path hints in the event's path field.
            if ev.path.contains('"') || ev.path.contains('\\') {
                self.add(
                    &c.short_sha,
                    FindingKind::QuotedDiffPath,
                    format!("path={}", ev.path),
                );
            }

            // Check for blank-node identifiers anywhere in the line.
            if ev.line.contains("_:") {
                self.add(
                    &c.short_sha,
                    FindingKind::BlankNode,
                    format!("{}: {}", ev.path, ev.line),
                );
            }

            // Format checks depend on which kind of .spo file this is.
            if is_extraction_log(&ev.path) {
                // Log format: 4 pipe-delimited fields.
                let n_fields = ev.line.split(" | ").count();
                if n_fields < 4 && !ev.line.is_empty() {
                    self.add(
                        &c.short_sha,
                        FindingKind::MalformedLogSpoLine,
                        format!("{} fields: {}", n_fields, ev.line),
                    );
                }
            } else if ev.path.ends_with(".fm.spo") {
                // Sidecar format: 3 pipe-delimited fields.
                let n_fields = ev.line.split(" | ").count();
                if n_fields != 3 && !ev.line.is_empty() {
                    self.add(
                        &c.short_sha,
                        FindingKind::MalformedFmSpoLine,
                        format!("{}: {} fields: {}", ev.path, n_fields, ev.line),
                    );
                }
            }
        }
    }

    /// Emit the accumulated findings. Targets either stderr (default) or a
    /// file path provided via `--inconsistency-log`.
    fn report(&self, log_path: Option<&str>) {
        let mut out: Box<dyn Write> = match log_path {
            Some(p) => match fs::File::create(p) {
                Ok(f) => Box::new(f),
                Err(e) => {
                    eprintln!("spike: could not open inconsistency log {}: {}", p, e);
                    Box::new(std::io::stderr())
                }
            },
            None => Box::new(std::io::stderr()),
        };

        let _ = writeln!(out, "\nspike: ══ INCONSISTENCY REPORT ══");
        if self.counts.is_empty() {
            let _ = writeln!(out, "spike: no inconsistencies detected");
            return;
        }

        // Sorted counts for stable output.
        let mut count_vec: Vec<(&FindingKind, &usize)> = self.counts.iter().collect();
        count_vec.sort_by_key(|(k, _)| format!("{:?}", k));
        for (k, n) in &count_vec {
            let _ = writeln!(out, "spike:   {:?}: {}", k, n);
        }

        // Only print individual findings for non-churn kinds. Churn is too
        // noisy to enumerate.
        let detailed: Vec<&Finding> = self
            .findings
            .iter()
            .filter(|f| !matches!(f.kind, FindingKind::ExtractionIdChurn))
            .collect();
        if detailed.is_empty() {
            return;
        }
        let _ = writeln!(out, "spike: ── details (non-churn) ──");
        // Cap output at a reasonable size so one malformed commit doesn't
        // drown the report.
        let cap = 50usize;
        for f in detailed.iter().take(cap) {
            let _ = writeln!(out, "spike:   {}  {:?}  {}", f.commit, f.kind, f.detail);
        }
        if detailed.len() > cap {
            let _ = writeln!(
                out,
                "spike:   ... {} more findings elided (raise cap if investigating)",
                detailed.len() - cap
            );
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Repo metadata helpers
// ════════════════════════════════════════════════════════════════════════════

/// Read the repo name from `.lex/repo.yml`. Returns `None` if the file is
/// missing, unreadable, or has no `name:` line. The format is intentionally
/// loose parsing because this is a spike — the real implementation will use
/// serde_yaml or the existing reader in main.rs.
fn read_repo_name(root: &PathBuf) -> Option<String> {
    let yml = root.join(".lex").join("repo.yml");
    let content = fs::read_to_string(&yml).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("name:") {
            return Some(rest.trim().trim_matches('"').to_string());
        }
    }
    None
}

// ════════════════════════════════════════════════════════════════════════════
// Sidecar → canonical URI (Option A + C preview)
// ════════════════════════════════════════════════════════════════════════════
//
// The `.spo` sidecar format is pipe-delimited and has THREE shapes in
// practice. The first spike pass missed the third, and the sanity sweeper
// caught it by reporting all `linksTo` events as `(canonical: UNPARSEABLE)`
// — a nice demonstration of the sweeper earning its keep.
//
//   1. Frontmatter form (most common):
//        squad.message.priority | hasValue | normal
//      The LEFT field is a dot-path through the source document's
//      frontmatter, the MIDDLE is always `hasValue`, and the RIGHT is the
//      asserted literal value. The IMPLICIT subject is the source document
//      URI — encoded by the sidecar filename via the extractor convention
//      `<rel-path>.<extractor>.spo`.
//
//   2. Mention-edge form (cross-document mentions, @-decorated subject):
//        @brief/foo.md | mentions | kira
//      The LEFT field is the explicit subject path prefixed with `@` (the
//      `@` appears decorative — all real-world examples have it on mentions
//      only). The MIDDLE is the predicate, the RIGHT is the object.
//
//   3. Wikilink-edge form (body wikilinks → linksTo edges, bare subject):
//        brief/foo.md | linksTo | target-doc
//      Same shape as form 2 but without the `@` prefix. Emitted by the
//      tree-sitter markdown link extractor for wikilinks in document body.
//
// For the canonical URI scheme, all three shapes are treated uniformly: we
// reconstruct (subject, predicate, object), hash the canonical pipe-joined
// form, and build the URI as:
//
//   <base>/history/<sidecar-rel-path>#<hash[..8]>
//
// where <base> = "repolex://<repo-name>/" — a placeholder non-IETF scheme
// that's safe to use as an IRI in turtle without needing a real HTTP server.

/// Parsed representation of one line from a `.spo` sidecar. One of three
/// forms: frontmatter (implicit subject), mention-edge (`@`-prefixed
/// subject), or wikilink-edge (bare explicit subject).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SidecarLine {
    /// `<dot-path> | hasValue | <value>` — subject is the document path
    /// (implicit; reconstructed from the sidecar filename).
    Frontmatter {
        dot_path: String,
        value: String,
    },
    /// Generic `<subject> | <predicate> | <object>` triple with an explicit
    /// subject. Covers both `@subject | mentions | object` (mention-edge
    /// form) and `subject | linksTo | object` (wikilink-edge form). The `@`
    /// prefix, when present, is stripped during parsing because it appears
    /// decorative rather than semantic in the real data.
    GenericEdge {
        subject: String,
        predicate: String,
        object: String,
    },
}

/// Parse a raw sidecar line into one of the three forms. Returns `None` if
/// the line doesn't have exactly three pipe-delimited fields — the sweeper
/// is the layer that logs malformed lines for human investigation.
///
/// Disambiguation rule: if the MIDDLE field is `hasValue`, it's frontmatter
/// form. Otherwise it's a generic edge with an explicit subject, regardless
/// of whether the subject carries an `@` decoration.
fn parse_sidecar_line(line: &str) -> Option<SidecarLine> {
    let fields: Vec<&str> = line.split(" | ").collect();
    if fields.len() != 3 {
        return None;
    }
    let (left, middle, right) = (fields[0], fields[1], fields[2]);

    if middle == "hasValue" {
        // Frontmatter form — the LEFT field is a dot-path into the source
        // document's frontmatter, and the subject is the document itself
        // (derived from the sidecar filename later).
        Some(SidecarLine::Frontmatter {
            dot_path: left.to_string(),
            value: right.to_string(),
        })
    } else {
        // Generic edge form. Strip any leading `@` on the subject because it
        // looks decorative in the real data.
        let subject = left.strip_prefix('@').unwrap_or(left).to_string();
        Some(SidecarLine::GenericEdge {
            subject,
            predicate: middle.to_string(),
            object: right.to_string(),
        })
    }
}

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
fn derive_source_document(sidecar_rel_path: &str) -> Option<String> {
    // Strip the `.lex/extract/` prefix first so the returned path is relative
    // to the repo root.
    let after_extract = sidecar_rel_path
        .strip_prefix(".lex/extract/")
        .unwrap_or(sidecar_rel_path);

    // Try known extractor suffixes in longest-first order.
    for suffix in &[".fm.spo", ".md.spo", ".cc.spo"] {
        if let Some(base) = after_extract.strip_suffix(suffix) {
            return Some(base.to_string());
        }
    }
    None
}

/// Reconstruct the (subject, predicate, object) triple from a sidecar
/// event's raw line. Uses the same parsing logic as `canonical_uri` but
/// returns the triple tuple directly for display purposes. Returns `None`
/// for unparseable lines or unknown sidecar suffixes.
pub fn reconstructed_triple(
    sidecar_path: &str,
    line: &str,
) -> Option<(String, String, String)> {
    let parsed = parse_sidecar_line(line)?;
    match parsed {
        SidecarLine::Frontmatter { dot_path, value } => {
            let source = derive_source_document(sidecar_path)?;
            Some((source, dot_path, value))
        }
        SidecarLine::GenericEdge {
            subject,
            predicate,
            object,
        } => Some((subject, predicate, object)),
    }
}

/// Compute the canonical URI for a single sidecar event. The URI encodes:
///   - the repo name (from repo.yml) as a base scope
///   - the sidecar relative path (for provenance)
///   - a content hash fragment that is STABLE across source-document edits
///     that don't touch this specific triple
///
/// Returns `None` if the event's line is unparseable as a sidecar line.
pub fn canonical_uri(repo_name: &str, sidecar_path: &str, line: &str) -> Option<String> {
    let parsed = parse_sidecar_line(line)?;

    // Reconstruct the canonical triple tuple. For frontmatter form, the
    // subject is implicit from the sidecar filename; for generic-edge form,
    // it's explicit in the line.
    let (subject, predicate, object) = match &parsed {
        SidecarLine::Frontmatter { dot_path, value } => {
            // Derive source document path from sidecar path.
            let source = derive_source_document(sidecar_path)?;
            (source, dot_path.clone(), value.clone())
        }
        SidecarLine::GenericEdge {
            subject,
            predicate,
            object,
        } => (subject.clone(), predicate.clone(), object.clone()),
    };

    // Canonical form for hashing: pipe-joined triple tuple. Using a simple
    // delimiter rather than turtle or n-triples serialization because (a)
    // the input data is already pipe-delimited so there's no escaping work,
    // and (b) the hash only needs to be stable across invocations, not
    // interoperable with any external tool.
    let canonical_form = format!("{}|{}|{}", subject, predicate, object);
    let hash = sha256_prefix(&canonical_form, 8);

    // Strip the `.lex/extract/` prefix for the URI fragment so the resulting
    // URI is shorter and more readable. The extractor suffix (`.fm.spo` etc.)
    // is preserved as provenance.
    let sidecar_for_uri = sidecar_path
        .strip_prefix(".lex/extract/")
        .unwrap_or(sidecar_path);

    Some(format!(
        "repolex://{}/history/{}#{}",
        repo_name, sidecar_for_uri, hash
    ))
}

/// Compute a hex-encoded SHA256 prefix of the given length. 8 hex chars
/// gives us 4 billion buckets — more than enough for within-file uniqueness
/// given typical per-file triple counts in the tens.
fn sha256_prefix(input: &str, hex_chars: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let full = hex::encode(hasher.finalize());
    full[..hex_chars.min(full.len())].to_string()
}

// ════════════════════════════════════════════════════════════════════════════
// SPIKE: history graph triple maker + walker (Phase 4, 2026-04-11)
// ════════════════════════════════════════════════════════════════════════════
//
// Everything in this section is intentionally a SPIKE — focused, deliberately
// scoped down, error-tolerant, and meant to prove out the data shape before
// committing the binary to a final design. Code in here is tagged `spike:`
// in comments. Errors are logged to stderr (non-fatal) so a single
// problematic .spo line doesn't kill a 200-commit walk.
//
// The goal: walk git history, for each commit diff `.lex/extract/*.spo`
// against the parent, run each added/removed line through a focused
// "spike triple maker", wrap the resulting (s, p, o) in an RDF 1.2 triple
// term via `rdf:reifies`, attach `spo:addedIn` / `spo:removedIn`
// annotations pointing at the commit URI, and write into a scratch named
// graph `<base/historytest>` so we can SPARQL-query it without disturbing
// the production `<base/now>` graph.
//
// What this spike does NOT do:
//   - Reuse the production spo→nquad logic in `generate_frontmatter_nquads`
//     (main.rs:2429). That code is tangled with frontmatter extraction; the
//     spike rewrites a smaller version focused only on what the walker needs.
//   - Handle every legacy frontmatter shape. If a line doesn't match the
//     spike's known patterns, it goes to the error log and the walker
//     keeps going.
//   - Migrate historical triples to current ontology. Old commits may
//     reference predicates that no longer exist. The spike emits them
//     anyway with whatever predicate URI the dot-notation expands to,
//     and downstream queries will see the dead predicate. Acceptable for
//     v1; lux's "triple sidecar" idea is the long-term fix if needed.
//
// Path forward: once the spike validates the shape and queries work, the
// real implementation will either (a) extract the production logic into a
// shared helper that both `generate_frontmatter_nquads` and the history
// walker call, or (b) ship the spike code as-is and accept the duplication
// as a known stale-pair. lux's call after seeing the demo.

/// SPIKE: produce RDF N-Quad strings for one .spo line, in one named graph.
/// Returns Vec because some lines (the dot-notation kit.Class.property form)
/// emit two quads — the property assertion plus an `rdf:type` for the class.
///
/// `doc_uri` should be the IRI of the source markdown file (e.g.
/// `<https://github.com/7R1PL3F0RC3/W4R3Z/friend/1ux.md>`), built by the
/// caller using the same `base_uri()` + path-encoding rules as the
/// production extractor.
///
/// `graph` is the named-graph URI to embed in each quad (e.g.
/// `<https://repolex.ai/.../historytest>`).
///
/// On unparseable input, returns `Err(reason)` so the walker can log and
/// continue. Never panics.
pub fn spike_triple_maker(
    spo_line: &str,
    doc_uri: &str,
    graph: &str,
    obj_props: &std::collections::HashSet<String>,
    prop_datatypes: &std::collections::HashMap<String, String>,
) -> Result<Vec<String>, String> {
    let parts: Vec<&str> = spo_line.splitn(3, " | ").collect();
    if parts.len() != 3 {
        return Err(format!("expected 3 fields, got {}: {}", parts.len(), spo_line));
    }
    let subject = parts[0];
    let predicate = parts[1];
    let object = parts[2];

    let mut out: Vec<String> = Vec::new();

    // spike: handle the three-segment dot notation (kit.Class.property),
    // which is the modern shape we care about most. Older legacy shapes
    // (bare title, tags, mentions, linksTo) get a simpler fallback.
    let segments: Vec<&str> = subject.splitn(3, '.').collect();
    if segments.len() == 3 && predicate == "hasValue" {
        let kit_name = segments[0];
        let class_seg = segments[1];
        let prop_seg = segments[2];

        // Always emit rdf:type for the class. The walker dedups idempotent
        // re-inserts via RDF set semantics, so we don't bother with the
        // emitted_types HashSet the production extractor uses.
        let type_uri = format!(
            "<https://repolex.ai/ontology/kit/{}/{}>",
            kit_name, class_seg
        );
        out.push(format!(
            "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> {} {} .",
            doc_uri, type_uri, graph
        ));

        let kit_predicate = format!(
            "<https://repolex.ai/ontology/kit/{}/{}>",
            kit_name, prop_seg
        );

        // spike: for ObjectProperties, the value SHOULD be an IRI, but the
        // spike doesn't run the full resolver chain. We do a minimal best-
        // effort: if the value looks like a path or has slashes/dots that
        // suggest a doc reference, emit it as an IRI under base; otherwise
        // emit a plain literal. Production code uses src/resolve.rs for
        // this, which the spike intentionally does not depend on.
        if obj_props.contains(prop_seg) {
            // Multi-value: ObjectProperty values may be comma-separated.
            for raw_val in object.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                if raw_val.contains('[') || raw_val.contains(']') {
                    eprintln!("  [skip] IRI-illegal chars in ObjectProperty value: {:?} (line: {})", raw_val, spo_line);
                    continue;
                }
                let base = extract_base_from_doc_uri(doc_uri);
                let iri = format!("<{}/{}>", base, sanitize_for_iri(raw_val));
                out.push(format!(
                    "{} {} {} {} .",
                    doc_uri, kit_predicate, iri, graph
                ));
            }
        } else if let Some(datatype) = prop_datatypes.get(prop_seg) {
            // DatatypeProperty with non-string range → typed literal.
            out.push(format!(
                "{} {} \"{}\"^^<{}> {} .",
                doc_uri, kit_predicate, crate::nq_escape(object), datatype, graph
            ));
        } else {
            // DatatypeProperty defaulting to string.
            out.push(format!(
                "{} {} \"{}\" {} .",
                doc_uri, kit_predicate, crate::nq_escape(object), graph
            ));
        }
        return Ok(out);
    }

    // spike: legacy / non-kit forms — title, tags, plain key | hasValue | x
    if predicate == "hasValue" {
        let fm_predicate = format!(
            "<https://repolex.ai/ontology/git-lex/fm/{}>",
            crate::uri_encode_path(subject)
        );
        out.push(format!(
            "{} {} \"{}\" {} .",
            doc_uri, fm_predicate, crate::nq_escape(object), graph
        ));
        return Ok(out);
    }

    // spike: mentions edge. The subject usually starts with `@` to mark a
    // doc reference. We strip the @ and emit a lex:mentions triple with a
    // string object — the spike doesn't try to resolve mention objects to
    // IRIs (production does, via slug_index, which is too heavy here).
    if predicate == "mentions" {
        out.push(format!(
            "{} <https://repolex.ai/ontology/git-lex/lex/mentions> \"{}\" {} .",
            doc_uri, crate::nq_escape(object), graph
        ));
        return Ok(out);
    }

    // spike: linksTo edge.
    if predicate == "linksTo" {
        out.push(format!(
            "{} <https://repolex.ai/ontology/git-lex/lex/linksTo> \"{}\" {} .",
            doc_uri, crate::nq_escape(object), graph
        ));
        return Ok(out);
    }

    Err(format!("unrecognized predicate {:?} in line: {}", predicate, spo_line))
}

/// spike: extract `https://host/org/repo` from a doc URI like
/// `<https://host/org/repo/path/to/file.md>`. Drops the angle brackets and
/// the trailing path. Returns the base without trailing slash.
fn extract_base_from_doc_uri(doc_uri: &str) -> String {
    let trimmed = doc_uri.trim_start_matches('<').trim_end_matches('>');
    // The base is the first 5 path segments: scheme://host/org/repo
    // (scheme://, host, org, repo). We approximate by finding the 4th `/`
    // after the scheme.
    if let Some(scheme_end) = trimmed.find("://") {
        let after_scheme = &trimmed[scheme_end + 3..];
        // Find the third `/` in `host/org/repo/...`
        let mut count = 0;
        for (i, c) in after_scheme.char_indices() {
            if c == '/' {
                count += 1;
                if count == 3 {
                    return trimmed[..scheme_end + 3 + i].to_string();
                }
            }
        }
        // Fewer than 3 path segments — return everything we have.
        return trimmed.to_string();
    }
    trimmed.to_string()
}

/// spike: very crude IRI sanitization. Real production uses
/// `uri_encode_path` for path-style IRIs and a separate sanitizer for
/// entity-style IRIs. The spike doesn't need the precision; we just need
/// the result to parse as an IRI when oxigraph loads it.
fn sanitize_for_iri(s: &str) -> String {
    s.trim()
        .trim_start_matches('@')
        .replace(' ', "-")
}

/// spike: derive a doc URI for a sidecar path. The sidecar path is the
/// `.spo` file path relative to the repo root, e.g.
/// `.lex/extract/friend/1ux.md.fm.spo`. The corresponding doc is
/// `friend/1ux.md`. We strip the `.lex/extract/` prefix and the
/// `.{extractor}.spo` suffix.
///
/// Returns the doc URI in `<...>` form, ready to embed in N-Quads.
pub fn doc_uri_from_sidecar(sidecar_path: &str, base: &str) -> Option<String> {
    let after_extract = sidecar_path.strip_prefix(".lex/extract/")?;
    let doc_path = if let Some(s) = after_extract.strip_suffix(".fm.spo") {
        s
    } else if let Some(s) = after_extract.strip_suffix(".md.spo") {
        s
    } else if let Some(s) = after_extract.strip_suffix(".cc.spo") {
        s
    } else {
        return None;
    };
    Some(format!("<{}/{}>", base, crate::uri_encode_path(doc_path)))
}

/// spike: build the N-Quad lines for the history-graph annotation of a
/// single (s, p, o, op) event. The annotation pattern is:
///
///     <ann-uri> rdf:reifies <<( s p o )>> .
///     <ann-uri> spo:addedIn   <commit/sha> .   (or spo:removedIn)
///     <ann-uri> spo:inFile    "path/to/file.md.fm.spo" .
///
/// The annotation URI is content-addressed: SHA256 of the canonical
/// `commit|op|s|p|o|file` tuple, truncated to 8 hex chars, in a `<base/spo-ann/HASH>`
/// scheme. Idempotent on re-insert (same content → same URI → set semantics
/// dedupe).
///
/// `triple_nq` is one assertion line as already produced by
/// `spike_triple_maker`, in N-Quad form: `S P O G .`. We parse out S/P/O
/// to build the triple term.
pub fn spike_history_annotation(
    triple_nq: &str,
    op: char,
    commit_sha: &str,
    sidecar_path: &str,
    base: &str,
    history_graph: &str,
) -> Option<Vec<String>> {
    // The triple_nq looks like: `<S> <P> O <G> .`
    // where O may be `<iri>` or `"literal"^^<datatype>` or `"literal"`.
    // We strip the trailing graph + period to isolate the (S, P, O) trio.
    let trimmed = triple_nq.trim_end_matches('.').trim();
    let trimmed = trimmed.rsplit_once(' ').map(|(rest, _)| rest)?.trim();
    // Now `trimmed` is `<S> <P> O`. Parse three terms by walking.
    let (s, rest) = take_term(trimmed)?;
    let (p, rest) = take_term(rest.trim())?;
    let o = rest.trim().to_string();
    if s.is_empty() || p.is_empty() || o.is_empty() {
        return None;
    }

    // spike: hash the canonical key for the annotation URI.
    let key = format!("{}|{}|{}|{}|{}|{}", commit_sha, op, s, p, o, sidecar_path);
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    let ann_uri = format!("<{}/spo-ann/{}>", base, &hash[..16]);

    let added_or_removed = if op == '+' {
        "<https://repolex.ai/ontology/spo/addedIn>"
    } else {
        "<https://repolex.ai/ontology/spo/removedIn>"
    };
    let commit_uri = format!("<{}/commit/{}>", base, commit_sha);

    Some(vec![
        // The triple term annotation: <ann-uri> rdf:reifies <<( s p o )>>
        format!(
            "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> <<( {} {} {} )>> {} .",
            ann_uri, s, p, o, history_graph
        ),
        // Which commit added/removed it
        format!(
            "{} {} {} {} .",
            ann_uri, added_or_removed, commit_uri, history_graph
        ),
        // Which file the assertion was in
        format!(
            "{} <https://repolex.ai/ontology/spo/inFile> \"{}\" {} .",
            ann_uri, crate::nq_escape(sidecar_path), history_graph
        ),
    ])
}

/// spike: take one whitespace-separated term from the start of `s`. A term
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

/// SPIKE: walk git history, build the history graph in `<base/historytest>`,
/// load it into oxigraph, run a verification SPARQL query.
///
/// This is the entry point a new CLI subcommand will eventually call. For
/// the spike, we wire it up as a function and let main.rs add a subcommand.
/// Errors during walk are logged to stderr but never fatal.
pub fn spike_history_walk(commit_limit: usize) {
    let root = match find_git_root() {
        Some(r) => r,
        None => {
            eprintln!("spike: not in a git repo");
            return;
        }
    };
    if std::env::set_current_dir(&root).is_err() {
        eprintln!("spike: failed to cd to {}", root.display());
        return;
    }

    // Read repo base URI the same way the production code does.
    let base = crate::git::base_uri();
    let history_graph = format!("<{}/history>", base);
    let meta_graph = format!("<{}/meta>", base);

    // Capture HEAD SHA at the start of the walk so the marker triple
    // records exactly which commit was the tip when this rebuild ran.
    let head_sha = match Command::new("git").args(["rev-parse", "HEAD"]).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => {
            eprintln!("spike: failed to resolve HEAD — aborting");
            return;
        }
    };

    // Load ontology helpers from the kit, same as production extract.
    let kit = match git_lex::get_kit() {
        Some(k) => k,
        None => {
            eprintln!("spike: no kit configured in .lex/repo.yml — aborting");
            return;
        }
    };
    let obj_props = crate::get_object_properties(&kit);
    let prop_datatypes = crate::get_property_datatypes(&kit);

    eprintln!("spike: walking history → <historytest>");
    eprintln!("spike: kit = {}", kit);
    eprintln!("spike: base = {}", base);
    eprintln!("spike: history_graph = {}", history_graph);

    let commits = collect_commits(commit_limit);
    let total = commits.len();
    eprintln!("spike: {} commit(s) to walk", total);

    let mut nq_buffer = String::new();
    let mut events_seen = 0usize;
    let mut events_emitted = 0usize;
    let mut events_skipped = 0usize;
    let mut errors: Vec<String> = Vec::new();

    for c in &commits {
        for ev in &c.events {
            events_seen += 1;

            let doc_uri = match doc_uri_from_sidecar(&ev.path, &base) {
                Some(u) => u,
                None => {
                    events_skipped += 1;
                    errors.push(format!(
                        "{}: could not derive doc URI from sidecar path {}",
                        c.short_sha, ev.path
                    ));
                    continue;
                }
            };

            // Use a SCRATCH graph for the per-event triple — it's not the
            // real assertion graph, it's the annotation target.
            let scratch_graph = history_graph.clone();
            let triple_nqs = match spike_triple_maker(
                &ev.line,
                &doc_uri,
                &scratch_graph,
                &obj_props,
                &prop_datatypes,
            ) {
                Ok(v) => v,
                Err(e) => {
                    events_skipped += 1;
                    errors.push(format!("{}: {}", c.short_sha, e));
                    continue;
                }
            };

            // Each triple_nq is "S P O G ." — we want to wrap the
            // (S, P, O) of each in a triple term and emit annotations.
            // The rdf:type quad emitted by spike_triple_maker is also
            // wrapped, so the history graph records "this commit added a
            // type assertion" alongside "this commit added a property".
            for triple_nq in &triple_nqs {
                if let Some(ann_quads) = spike_history_annotation(
                    triple_nq,
                    ev.op,
                    &c.sha,
                    &ev.path,
                    &base,
                    &history_graph,
                ) {
                    for q in ann_quads {
                        nq_buffer.push_str(&q);
                        nq_buffer.push('\n');
                    }
                    events_emitted += 1;
                } else {
                    errors.push(format!(
                        "{}: failed to build annotation for {}",
                        c.short_sha, triple_nq
                    ));
                    events_skipped += 1;
                }
            }
        }
    }

    eprintln!("spike: events seen={}, emitted={}, skipped={}", events_seen, events_emitted, events_skipped);
    eprintln!("spike: nquad buffer size = {} bytes", nq_buffer.len());
    eprintln!("spike: error count = {}", errors.len());
    if !errors.is_empty() {
        let err_log = root.join(".lex").join("history-spike.errors.log");
        if let Ok(mut f) = fs::File::create(&err_log) {
            for e in &errors {
                let _ = writeln!(f, "{}", e);
            }
            eprintln!("spike: errors written to {}", err_log.display());
        } else {
            for e in errors.iter().take(20) {
                eprintln!("spike error: {}", e);
            }
            if errors.len() > 20 {
                eprintln!("spike: ... {} more errors", errors.len() - 20);
            }
        }
    }

    // Load into oxigraph
    eprintln!("spike: loading {} bytes into oxigraph store", nq_buffer.len());
    let store_path = root.join(".lex").join("oxigraph");
    let store = match oxigraph::store::Store::open(&store_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("spike: failed to open store at {}: {}", store_path.display(), e);
            return;
        }
    };

    // Clear the historytest graph first so re-runs are idempotent.
    if let Ok(graph_node) = oxigraph::model::NamedNode::new(
        history_graph.trim_start_matches('<').trim_end_matches('>'),
    ) {
        let _ = store.clear_graph(&graph_node);
    }

    let parser = oxigraph::io::RdfParser::from_format(oxigraph::io::RdfFormat::NQuads);
    match store.load_from_reader(parser, std::io::Cursor::new(nq_buffer.as_bytes())) {
        Ok(_) => eprintln!("spike: load OK"),
        Err(e) => {
            eprintln!("spike: load FAILED: {}", e);
            // Dump first few lines of buffer to help debug
            for (i, line) in nq_buffer.lines().take(5).enumerate() {
                eprintln!("spike:   nq[{}]: {}", i, line);
            }
            return;
        }
    }

    // Write marker triple: <base/meta> spo:lastHistorySync <commit/HEAD>.
    // Phase 6 (incremental sync) reads this to know where to pick up.
    // Lives in its own <base/meta> graph so it doesn't leak into history
    // graph queries.
    let marker_nq = format!(
        "<{}/meta> <https://repolex.ai/ontology/spo/lastHistorySync> <{}/commit/{}> {} .\n",
        base, base, head_sha, meta_graph
    );
    if let Ok(meta_node) = oxigraph::model::NamedNode::new(
        meta_graph.trim_start_matches('<').trim_end_matches('>'),
    ) {
        let _ = store.clear_graph(&meta_node);
    }
    let parser = oxigraph::io::RdfParser::from_format(oxigraph::io::RdfFormat::NQuads);
    match store.load_from_reader(parser, std::io::Cursor::new(marker_nq.as_bytes())) {
        Ok(_) => eprintln!("spike: marker written — lastHistorySync = {}", head_sha),
        Err(e) => eprintln!("spike: marker write FAILED: {}", e),
    }

    // Verification queries
    eprintln!("spike: ────────────────────────────────────────────");
    eprintln!("spike: verification queries");
    eprintln!("spike: ────────────────────────────────────────────");

    let queries = [
        ("triple count in history",
         format!("SELECT (COUNT(*) AS ?c) WHERE {{ GRAPH {} {{ ?s ?p ?o }} }}", history_graph)),
        ("annotation subject count",
         format!("SELECT (COUNT(DISTINCT ?ann) AS ?c) WHERE {{ GRAPH {} {{ ?ann <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> ?tt }} }}", history_graph)),
        ("addedIn vs removedIn count",
         format!("SELECT ?op (COUNT(?ann) AS ?c) WHERE {{ GRAPH {} {{ ?ann ?op ?commit . FILTER(?op IN (<https://repolex.ai/ontology/spo/addedIn>, <https://repolex.ai/ontology/spo/removedIn>)) }} }} GROUP BY ?op", history_graph)),
    ];

    for (label, q) in &queries {
        eprintln!("\nspike Q: {}", label);
        eprintln!("spike:   {}", q.replace('\n', " "));
        let result = SparqlEvaluator::new()
            .parse_query(q.as_str())
            .ok()
            .and_then(|prepared| prepared.on_store(&store).execute().ok());
        match result {
            Some(oxigraph::sparql::QueryResults::Solutions(sols)) => {
                let vars: Vec<String> = sols.variables().iter().map(|v| v.as_str().to_string()).collect();
                let mut row_count = 0;
                for sol in sols.flatten() {
                    let mut parts: Vec<String> = Vec::new();
                    for v in &vars {
                        if let Some(t) = sol.get(v.as_str()) {
                            parts.push(format!("{}={}", v, term_short(t)));
                        }
                    }
                    eprintln!("spike:     {}", parts.join("  "));
                    row_count += 1;
                }
                eprintln!("spike:   ({} row(s))", row_count);
            }
            Some(_) => eprintln!("spike:   non-SELECT result"),
            None => eprintln!("spike:   query error or no results"),
        }
    }
}

/// spike: short string repr of an oxigraph Term for debug output.
fn term_short(t: &oxigraph::model::Term) -> String {
    match t {
        oxigraph::model::Term::NamedNode(n) => format!("<{}>", n.as_str()),
        oxigraph::model::Term::Literal(l) => format!("\"{}\"", l.value()),
        oxigraph::model::Term::BlankNode(b) => format!("_:{}", b.as_str()),
        oxigraph::model::Term::Triple(t) => {
            format!("<<{} {} {}>>", t.subject, t.predicate, t.object)
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Tests
// ════════════════════════════════════════════════════════════════════════════

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

    // ─── canonical_log_key ─────────────────────────────────────────────────

    #[test]
    fn canonical_key_drops_hash_prefix() {
        let line = "28155d69/foo/bar.md | @foo/bar.md | mentions | kira";
        assert_eq!(
            canonical_log_key(line),
            Some("@foo/bar.md | mentions | kira".to_string())
        );
    }

    #[test]
    fn canonical_key_rejects_malformed_line() {
        assert_eq!(canonical_log_key("just-one-field"), None);
    }

    #[test]
    fn canonical_keys_match_for_same_triple_different_hash() {
        let a = "aaaaaaaa/foo/bar.md | squad.task.taskStatus | hasValue | done";
        let b = "bbbbbbbb/foo/bar.md | squad.task.taskStatus | hasValue | done";
        assert_eq!(canonical_log_key(a), canonical_log_key(b));
    }

    // ─── dedup_events ──────────────────────────────────────────────────────

    fn mk_event(op: char, path: &str, line: &str) -> SpikeEvent {
        SpikeEvent {
            op,
            path: path.to_string(),
            line: line.to_string(),
        }
    }

    #[test]
    fn dedup_collapses_paired_log_churn() {
        // A commit where the extraction hash changed but the triple didn't.
        let events = vec![
            mk_event(
                '-',
                ".lex/extraction.log.spo",
                "aaaaaaaa/foo.md | @foo.md | mentions | kira",
            ),
            mk_event(
                '+',
                ".lex/extraction.log.spo",
                "bbbbbbbb/foo.md | @foo.md | mentions | kira",
            ),
        ];
        let kept = dedup_events(&events);
        assert!(kept.is_empty(), "paired churn should be dropped");
    }

    #[test]
    fn dedup_preserves_real_add_or_remove() {
        // A true addition with no matching removal should survive.
        let events = vec![mk_event(
            '+',
            ".lex/extraction.log.spo",
            "aaaaaaaa/foo.md | @foo.md | mentions | kira",
        )];
        let kept = dedup_events(&events);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn dedup_preserves_status_transition_as_two_events() {
        // A task status transition should NOT be collapsed — the subject is
        // the same but the object is different, so canonical keys differ.
        let events = vec![
            mk_event(
                '-',
                ".lex/extraction.log.spo",
                "aaaaaaaa/task.md | squad.task.taskStatus | hasValue | todo",
            ),
            mk_event(
                '+',
                ".lex/extraction.log.spo",
                "bbbbbbbb/task.md | squad.task.taskStatus | hasValue | done",
            ),
        ];
        let kept = dedup_events(&events);
        assert_eq!(kept.len(), 2, "status change should survive dedup");
    }

    #[test]
    fn dedup_ignores_non_log_events() {
        // Sidecar events should pass through dedup untouched even if they
        // happen to look like paired add/remove.
        let events = vec![
            mk_event('-', "foo.fm.spo", "squad.x | hasValue | y"),
            mk_event('+', "foo.fm.spo", "squad.x | hasValue | y"),
        ];
        let kept = dedup_events(&events);
        // Both survive because this file is a sidecar, not the log.
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn dedup_handles_asymmetric_pairs() {
        // 2 removes + 1 add for the same key → 1 pair collapsed, 1 remove
        // survives.
        let events = vec![
            mk_event(
                '-',
                ".lex/extraction.log.spo",
                "aaaa/foo.md | @foo.md | mentions | kira",
            ),
            mk_event(
                '-',
                ".lex/extraction.log.spo",
                "bbbb/foo.md | @foo.md | mentions | kira",
            ),
            mk_event(
                '+',
                ".lex/extraction.log.spo",
                "cccc/foo.md | @foo.md | mentions | kira",
            ),
        ];
        let kept = dedup_events(&events);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].op, '-');
    }

    // ─── sweeper ───────────────────────────────────────────────────────────

    fn mk_commit(events: Vec<SpikeEvent>) -> SpikeCommit {
        SpikeCommit {
            sha: "deadbeef".into(),
            short_sha: "deadbee".into(),
            author: "test".into(),
            date: "2026-04-09".into(),
            subject: "test commit".into(),
            events,
            renames: Vec::new(),
        }
    }

    #[test]
    fn sweeper_flags_malformed_fm_line() {
        let mut sw = InconsistencySweeper::new();
        sw.sweep_commit(&mk_commit(vec![mk_event(
            '+',
            "foo.fm.spo",
            "not-enough-fields",
        )]));
        assert_eq!(
            *sw.counts.get(&FindingKind::MalformedFmSpoLine).unwrap_or(&0),
            1
        );
    }

    #[test]
    fn sweeper_flags_blank_node() {
        let mut sw = InconsistencySweeper::new();
        sw.sweep_commit(&mk_commit(vec![mk_event(
            '+',
            "foo.fm.spo",
            "squad.x | hasValue | _:anon_1",
        )]));
        assert_eq!(
            *sw.counts.get(&FindingKind::BlankNode).unwrap_or(&0),
            1
        );
    }

    #[test]
    fn sweeper_counts_churn() {
        let mut sw = InconsistencySweeper::new();
        sw.sweep_commit(&mk_commit(vec![
            mk_event(
                '-',
                ".lex/extraction.log.spo",
                "aaaa/foo.md | @foo.md | mentions | kira",
            ),
            mk_event(
                '+',
                ".lex/extraction.log.spo",
                "bbbb/foo.md | @foo.md | mentions | kira",
            ),
        ]));
        assert_eq!(
            *sw.counts.get(&FindingKind::ExtractionIdChurn).unwrap_or(&0),
            2
        );
    }

    // ─── parse_sidecar_line ───────────────────────────────────────────────

    #[test]
    fn parses_frontmatter_line() {
        let line = "squad.message.priority | hasValue | normal";
        let parsed = parse_sidecar_line(line).expect("should parse");
        assert_eq!(
            parsed,
            SidecarLine::Frontmatter {
                dot_path: "squad.message.priority".to_string(),
                value: "normal".to_string(),
            }
        );
    }

    #[test]
    fn parses_mention_edge_with_at_prefix() {
        let line = "@message/foo.md | mentions | kira";
        let parsed = parse_sidecar_line(line).expect("should parse");
        assert_eq!(
            parsed,
            SidecarLine::GenericEdge {
                subject: "message/foo.md".to_string(),
                predicate: "mentions".to_string(),
                object: "kira".to_string(),
            }
        );
    }

    #[test]
    fn parses_wikilink_edge_without_at_prefix() {
        // This is the third form the sanity sweeper caught in production —
        // body-wikilink edges emitted by the tree-sitter extractor with
        // bare (unprefixed) subjects.
        let line = "brief/foo.md | linksTo | target-doc";
        let parsed = parse_sidecar_line(line).expect("should parse");
        assert_eq!(
            parsed,
            SidecarLine::GenericEdge {
                subject: "brief/foo.md".to_string(),
                predicate: "linksTo".to_string(),
                object: "target-doc".to_string(),
            }
        );
    }

    #[test]
    fn rejects_malformed_line() {
        assert_eq!(parse_sidecar_line("not-enough-pipes"), None);
        assert_eq!(parse_sidecar_line("a | b"), None);
        assert_eq!(
            parse_sidecar_line("a | b | c | d"),
            None,
            "four-field lines should be rejected (not sidecar form)"
        );
    }

    #[test]
    fn three_field_non_hasvalue_is_generic_edge_not_rejected() {
        // The old behavior rejected `foo.bar | otherPredicate | baz` but
        // that was wrong — it's a valid generic edge. Only hard-malformed
        // (wrong field count) lines should be rejected.
        let parsed = parse_sidecar_line("foo.bar | otherPredicate | baz");
        assert_eq!(
            parsed,
            Some(SidecarLine::GenericEdge {
                subject: "foo.bar".to_string(),
                predicate: "otherPredicate".to_string(),
                object: "baz".to_string(),
            })
        );
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
    fn handles_sidecar_path_without_extract_prefix() {
        // If the path isn't under .lex/extract/, strip_prefix returns the
        // original. `.fm.spo` still strips correctly, leaving `foo.md`.
        assert_eq!(
            derive_source_document("foo.md.fm.spo"),
            Some("foo.md".to_string())
        );
    }

    // ─── canonical_uri ─────────────────────────────────────────────────────

    #[test]
    fn canonical_uri_for_frontmatter_is_stable() {
        // Same triple from same file produces identical URI regardless of
        // how many times we call it.
        let a = canonical_uri(
            "my-repo",
            ".lex/extract/message/foo.md.fm.spo",
            "squad.message.priority | hasValue | normal",
        );
        let b = canonical_uri(
            "my-repo",
            ".lex/extract/message/foo.md.fm.spo",
            "squad.message.priority | hasValue | normal",
        );
        assert_eq!(a, b);
        assert!(a.is_some(), "should produce a URI");
    }

    #[test]
    fn canonical_uri_for_different_values_differs() {
        // Same subject + predicate + DIFFERENT object → different URIs.
        // This is the property that makes status transitions (todo → done)
        // survive dedup.
        let a = canonical_uri(
            "my-repo",
            ".lex/extract/task/foo.md.fm.spo",
            "squad.task.taskStatus | hasValue | todo",
        );
        let b = canonical_uri(
            "my-repo",
            ".lex/extract/task/foo.md.fm.spo",
            "squad.task.taskStatus | hasValue | done",
        );
        assert_ne!(a, b);
    }

    #[test]
    fn canonical_uri_for_different_subjects_differs() {
        // Same predicate + object, different source document → different URIs.
        let a = canonical_uri(
            "my-repo",
            ".lex/extract/task/foo.md.fm.spo",
            "squad.task.taskStatus | hasValue | done",
        );
        let b = canonical_uri(
            "my-repo",
            ".lex/extract/task/bar.md.fm.spo",
            "squad.task.taskStatus | hasValue | done",
        );
        assert_ne!(a, b);
    }

    #[test]
    fn canonical_uri_for_mention_edge() {
        // `@`-prefixed mention edge. The URI should build successfully,
        // and the path-scoping should give us a URI under the sidecar path.
        let uri = canonical_uri(
            "my-repo",
            ".lex/extract/message/foo.md.fm.spo",
            "@message/foo.md | mentions | kira",
        );
        assert!(uri.is_some());
        let uri = uri.unwrap();
        assert!(uri.starts_with("repolex://my-repo/history/message/foo.md.fm.spo#"));
    }

    #[test]
    fn canonical_uri_for_wikilink_edge() {
        // Bare-subject wikilink edge — the third form the sweeper caught.
        // Should ALSO produce a clean canonical URI. This test exists to
        // prevent regression on the "parser rejects non-hasValue lines" bug
        // that we shipped in the first pass.
        let uri = canonical_uri(
            "my-repo",
            ".lex/extract/brief/foo.md.fm.spo",
            "brief/foo.md | linksTo | target-doc",
        );
        assert!(uri.is_some(), "wikilink edges must produce canonical URIs");
        let uri = uri.unwrap();
        assert!(uri.starts_with("repolex://my-repo/history/brief/foo.md.fm.spo#"));
    }

    #[test]
    fn canonical_uri_at_prefix_does_not_affect_hash() {
        // A mention edge with `@subject` and an identical-without-@ edge
        // should produce the SAME canonical URI fragment, because the `@` is
        // decorative and we strip it during parse. If this ever breaks it
        // means the hash is sensitive to the decoration, which would be a
        // real inconsistency.
        let with_at = canonical_uri(
            "my-repo",
            ".lex/extract/foo.md.fm.spo",
            "@foo.md | mentions | kira",
        )
        .unwrap();
        let without_at = canonical_uri(
            "my-repo",
            ".lex/extract/foo.md.fm.spo",
            "foo.md | mentions | kira",
        )
        .unwrap();
        assert_eq!(with_at, without_at);
    }

    #[test]
    fn canonical_uri_fragment_has_expected_shape() {
        let uri = canonical_uri(
            "my-repo",
            ".lex/extract/task/foo.md.fm.spo",
            "squad.task.taskStatus | hasValue | done",
        )
        .unwrap();
        // Should look like: repolex://my-repo/history/task/foo.md.fm.spo#<8-hex>
        assert!(uri.starts_with("repolex://my-repo/history/task/foo.md.fm.spo#"));
        let fragment = uri.rsplit('#').next().unwrap();
        assert_eq!(fragment.len(), 8, "hash fragment should be 8 hex chars");
        assert!(
            fragment.chars().all(|c| c.is_ascii_hexdigit()),
            "fragment should be hex"
        );
    }

    #[test]
    fn canonical_uri_returns_none_for_unparseable_line() {
        assert_eq!(
            canonical_uri("my-repo", ".lex/extract/foo.md.fm.spo", "garbage line"),
            None
        );
    }

    #[test]
    fn canonical_uri_returns_none_for_unknown_sidecar_suffix() {
        // Frontmatter line is parseable BUT the sidecar suffix is unknown →
        // we can't derive the source document, so we can't build the URI.
        assert_eq!(
            canonical_uri(
                "my-repo",
                ".lex/extract/foo.md.weird.spo",
                "squad.foo | hasValue | bar"
            ),
            None
        );
    }

    #[test]
    fn canonical_uri_deterministic_across_repo_names() {
        // Same triple, different repo name → different URI (scoped) but
        // the FRAGMENT should be identical because the hash is over the
        // triple tuple, not the repo.
        let a = canonical_uri(
            "repo-a",
            ".lex/extract/task/foo.md.fm.spo",
            "squad.task.taskStatus | hasValue | done",
        )
        .unwrap();
        let b = canonical_uri(
            "repo-b",
            ".lex/extract/task/foo.md.fm.spo",
            "squad.task.taskStatus | hasValue | done",
        )
        .unwrap();
        assert_ne!(a, b, "URIs should differ because repo name differs");
        let frag_a = a.rsplit('#').next().unwrap();
        let frag_b = b.rsplit('#').next().unwrap();
        assert_eq!(frag_a, frag_b, "fragments should be identical");
    }

    #[test]
    fn sweeper_does_not_flag_wellformed_lines() {
        let mut sw = InconsistencySweeper::new();
        sw.sweep_commit(&mk_commit(vec![
            mk_event('+', "foo.fm.spo", "squad.task.taskStatus | hasValue | done"),
            mk_event(
                '+',
                ".lex/extraction.log.spo",
                "aaaa/foo.md | @foo.md | mentions | kira",
            ),
        ]));
        assert!(sw
            .counts
            .get(&FindingKind::MalformedFmSpoLine)
            .copied()
            .unwrap_or(0)
            == 0);
        assert!(sw
            .counts
            .get(&FindingKind::MalformedLogSpoLine)
            .copied()
            .unwrap_or(0)
            == 0);
    }

    // ─── spike_triple_maker ────────────────────────────────────────────────

    use std::collections::{HashMap, HashSet};

    fn obj_props_with(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn dt_map_with(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    const DOC_URI: &str = "<https://github.com/acme/repo/Task/fix-bug.md>";
    const GRAPH: &str = "<https://github.com/acme/repo/history>";

    #[test]
    fn tm_rejects_line_with_wrong_field_count() {
        let r = spike_triple_maker("only-one-field", DOC_URI, GRAPH,
            &HashSet::new(), &HashMap::new());
        assert!(r.is_err());
    }

    #[test]
    fn tm_datatype_property_default_string_literal() {
        let r = spike_triple_maker(
            "squad.Task.title | hasValue | Fix bug",
            DOC_URI, GRAPH,
            &HashSet::new(), &HashMap::new(),
        ).expect("ok");
        // Two quads: rdf:type + title literal
        assert_eq!(r.len(), 2);
        assert!(r[0].contains("rdf-syntax-ns#type"));
        assert!(r[0].contains("kit/squad/Task"));
        assert!(r[1].contains("kit/squad/title"));
        assert!(r[1].contains("\"Fix bug\""));
    }

    #[test]
    fn tm_datatype_property_with_typed_range() {
        let r = spike_triple_maker(
            "squad.Task.dueDate | hasValue | 2026-04-14",
            DOC_URI, GRAPH,
            &HashSet::new(),
            &dt_map_with(&[("dueDate", "http://www.w3.org/2001/XMLSchema#date")]),
        ).expect("ok");
        assert_eq!(r.len(), 2);
        assert!(r[1].contains("\"2026-04-14\"^^<http://www.w3.org/2001/XMLSchema#date>"));
    }

    #[test]
    fn tm_object_property_emits_iri() {
        let r = spike_triple_maker(
            "squad.Task.assignee | hasValue | w4r3z",
            DOC_URI, GRAPH,
            &obj_props_with(&["assignee"]),
            &HashMap::new(),
        ).expect("ok");
        assert_eq!(r.len(), 2);
        assert!(r[1].contains("kit/squad/assignee"));
        // w4r3z should be rendered as an IRI, not a literal
        assert!(r[1].contains("<https://github.com/acme/repo/w4r3z>"));
        assert!(!r[1].contains("\"w4r3z\""));
    }

    #[test]
    fn tm_object_property_comma_splits_multivalue() {
        let r = spike_triple_maker(
            "squad.Task.assignee | hasValue | w4r3z, m4rq, kira",
            DOC_URI, GRAPH,
            &obj_props_with(&["assignee"]),
            &HashMap::new(),
        ).expect("ok");
        // rdf:type + 3 assignee triples
        assert_eq!(r.len(), 4);
        assert!(r[1].contains("/w4r3z"));
        assert!(r[2].contains("/m4rq"));
        assert!(r[3].contains("/kira"));
    }

    #[test]
    fn tm_object_property_skips_wikilink_brackets() {
        // The log-and-skip path for bad historical data — [[m4rq]] should
        // not produce any triple (bracket values are IRI-illegal and we
        // refuse to paper over them).
        let r = spike_triple_maker(
            "squad.Task.assignee | hasValue | [[m4rq]]",
            DOC_URI, GRAPH,
            &obj_props_with(&["assignee"]),
            &HashMap::new(),
        ).expect("ok");
        // Only the rdf:type should remain; the skipped value yields no quad.
        assert_eq!(r.len(), 1);
        assert!(r[0].contains("rdf-syntax-ns#type"));
    }

    #[test]
    fn tm_object_property_mixed_valid_and_bracketed_values() {
        let r = spike_triple_maker(
            "squad.Task.assignee | hasValue | w4r3z, [[m4rq]], kira",
            DOC_URI, GRAPH,
            &obj_props_with(&["assignee"]),
            &HashMap::new(),
        ).expect("ok");
        // rdf:type + 2 valid assignees (bracketed one skipped)
        assert_eq!(r.len(), 3);
        assert!(r[1].contains("/w4r3z"));
        assert!(r[2].contains("/kira"));
    }

    #[test]
    fn tm_legacy_hasvalue_without_dot_notation() {
        let r = spike_triple_maker(
            "title | hasValue | Fix bug",
            DOC_URI, GRAPH,
            &HashSet::new(), &HashMap::new(),
        ).expect("ok");
        // Legacy path: fm: predicate, string literal. No rdf:type emitted.
        assert_eq!(r.len(), 1);
        assert!(r[0].contains("ontology/git-lex/fm/title"));
        assert!(r[0].contains("\"Fix bug\""));
    }

    #[test]
    fn tm_mentions_edge_emits_lex_mentions() {
        let r = spike_triple_maker(
            "@w4r3z | mentions | w4r3z",
            DOC_URI, GRAPH,
            &HashSet::new(), &HashMap::new(),
        ).expect("ok");
        assert_eq!(r.len(), 1);
        assert!(r[0].contains("ontology/git-lex/lex/mentions"));
        assert!(r[0].contains("\"w4r3z\""));
    }

    #[test]
    fn tm_linksto_edge_emits_lex_linksto() {
        let r = spike_triple_maker(
            "[[foo.md]] | linksTo | foo.md",
            DOC_URI, GRAPH,
            &HashSet::new(), &HashMap::new(),
        ).expect("ok");
        assert_eq!(r.len(), 1);
        assert!(r[0].contains("ontology/git-lex/lex/linksTo"));
    }

    #[test]
    fn tm_rejects_unknown_predicate() {
        let r = spike_triple_maker(
            "subject | nonsense | object",
            DOC_URI, GRAPH,
            &HashSet::new(), &HashMap::new(),
        );
        assert!(r.is_err());
    }

    #[test]
    fn tm_escapes_quote_in_literal() {
        let r = spike_triple_maker(
            r#"squad.Note.body | hasValue | He said "hi""#,
            DOC_URI, GRAPH,
            &HashSet::new(), &HashMap::new(),
        ).expect("ok");
        assert_eq!(r.len(), 2);
        assert!(r[1].contains(r#"\"hi\""#));
    }

    #[test]
    fn tm_emoji_literal_roundtrips() {
        let r = spike_triple_maker(
            "squad.Journal.emojimood | hasValue | 🔥🧠⚡",
            DOC_URI, GRAPH,
            &HashSet::new(), &HashMap::new(),
        ).expect("ok");
        assert_eq!(r.len(), 2);
        assert!(r[1].contains("🔥🧠⚡"));
    }
}
