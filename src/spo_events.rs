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

        if opts.only_changes && displayed.is_empty() {
            continue;
        }
        if displayed.is_empty() {
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
                "  {} raw event(s) → {} after dedup (+{} -{})",
                c.events.len(),
                displayed.len(),
                dd_added,
                dd_removed
            );
        } else {
            println!("  {} event(s): +{} -{}", c.events.len(), raw_added, raw_removed);
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

    // Zero-context unified diff over extraction sidecar files only.
    //
    // Scope narrowed from `*.spo` to `.lex/extract/*.spo` (lux: 2026-04-09) —
    // the old `.lex/extraction.log.spo` file was a leftover from an earlier
    // attempt and is not part of the real knowledge ledger. Everything that
    // matters lives under `.lex/extract/` as per-document sidecars with names
    // like `foo.md.fm.spo`, `foo.md.md.spo`, `foo.md.cc.spo`, and future
    // extractors (`gliner.spo`, `haiku.spo`) will follow the same shape.
    //
    // `--no-renames` is the default for `diff-tree` (renames require `-M`),
    // which is what we want for the spike — see §9.2 of the history-graph
    // Situation doc for the rename handling discussion.
    let diff_out = Command::new("git")
        .args([
            "diff-tree",
            "--no-commit-id",
            "--no-color",
            "--no-ext-diff",
            "--unified=0",
            "-r",
            &base,
            sha,
            "--",
            ".lex/extract/*.spo",
        ])
        .output()
        .expect("git diff-tree failed");

    let diff_text = String::from_utf8_lossy(&diff_out.stdout).to_string();
    let events = parse_unified_diff(&diff_text);

    SpikeCommit {
        sha: sha.to_string(),
        short_sha,
        author,
        date,
        subject,
        events,
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Layer 2: unified-diff parser (pure)
// ════════════════════════════════════════════════════════════════════════════

/// Parse a unified diff as produced by `git diff-tree --unified=0` into a
/// flat list of add/remove events, each tagged with its file path.
///
/// Only these line kinds are relevant:
/// - `diff --git a/<path> b/<path>`  → switches the current file
/// - `+<content>`                    → addition
/// - `-<content>`                    → removal
///
/// Everything else (`@@` hunk headers, `---`/`+++` file-marker lines, `index`
/// lines, empty lines) is skipped. With `--unified=0` there are no context
/// lines so we don't have to filter space-prefixed content.
///
/// Pure function, unit-tested below.
pub fn parse_unified_diff(diff: &str) -> Vec<SpikeEvent> {
    let mut events = Vec::new();
    let mut current_path = String::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // "a/path b/path" — we want the b-path (post-change). For paths
            // with spaces this format uses quoting, which the squad repo
            // doesn't need today. Sanity checker flags quoted paths if they
            // ever appear.
            if let Some((_a, b)) = rest.split_once(' ') {
                current_path = b.trim_start_matches("b/").to_string();
            }
            continue;
        }
        if line.starts_with("+++")
            || line.starts_with("---")
            || line.starts_with("@@")
            || line.starts_with("index ")
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
}
