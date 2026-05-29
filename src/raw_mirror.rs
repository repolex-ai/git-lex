//! Raw harness-session mirror — the soul-as-canonical-record adapter.
//!
//! Soul holds the receipts. Harness storage (Claude Code's
//! `~/.claude/projects/.../` jsonl files, future Hermes thread logs, etc.) is
//! fungible — Anthropic can change its expiry, location, or format anytime.
//! At `git lex save` time this module copies harness session files
//! byte-faithfully into `<soul>/Raw/<HarnessName>/` so they survive any
//! upstream change.
//!
//! Three load-bearing invariants:
//!
//! - **Byte-faithful** — Raw files are bit-identical to the harness source.
//!   No frontmatter, no normalization. Raw is evidence; its value depends on
//!   being untouched.
//! - **Additive-only** — never delete from `Raw/`. Cross-machine workflow
//!   depends on this; Mac B must not nuke Mac A's sessions just because they
//!   aren't local.
//! - **State per-machine** — the session-id → first-seen-date map lives in
//!   `~/.local/share/git-lex/raw-mirror-state.json`, NOT in the soul repo.
//!   Different machines have different first-seen dates for the same session.
//!
//! See `Squad/Task/git-lex-raw-mirror-harness-sessions.md` for the full spec.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// One entry per session-id seen by the adapter on this machine. The schema
/// reserves space for the v0.2 cross-machine restore feature (restored_at)
/// so future additions don't require a state-file migration.
#[derive(Debug, Default, Serialize, Deserialize, Clone)]
struct SessionState {
    first_seen_date: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    restored_at: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct MirrorState {
    sessions: HashMap<String, SessionState>,
}

/// One configured harness watch from `.lex/repo.yml`. Shape mirrors the
/// declarative YAML; the `watch-path` may contain the literal substring
/// `<derived-from-cwd>` which the adapter expands per harness convention.
#[derive(Debug, Clone)]
pub struct HarnessPath {
    pub harness: String,
    pub watch_path: String,
    pub file_glob: String,
}

/// Mirror outcome for one save invocation. Lets `cmd_save` print a single
/// quiet summary line matching the extract-pass register.
#[derive(Debug, Default)]
pub struct MirrorReport {
    pub new: usize,
    pub updated: usize,
    pub harnesses_checked: usize,
}

impl MirrorReport {
    pub fn is_noop(&self) -> bool {
        self.new == 0 && self.updated == 0
    }
}

// ─── Config ────────────────────────────────────────────────────

/// Read the `raw-mirror:` block from `.lex/repo.yml`. Returns `(enabled,
/// harness_paths)`.
///
/// If the block is missing and the repo uses the soul kit, fall back to a
/// built-in Claude Code default. This lets existing souls get the mirror
/// behavior on a binary upgrade without requiring `kit-update` or any
/// hand-edits — additive code change, zero migration ceremony.
pub fn read_config(root: &Path) -> (bool, Vec<HarnessPath>) {
    let repo_yml = root.join(".lex").join("repo.yml");
    let content = match fs::read_to_string(&repo_yml) {
        Ok(s) => s,
        Err(_) => return (true, default_harness_paths(root)),
    };

    let parsed: serde_yaml::Value = match serde_yaml::from_str(&content) {
        Ok(v) => v,
        Err(_) => return (true, default_harness_paths(root)),
    };

    let block = match parsed.get("raw-mirror") {
        Some(b) => b,
        None => return (true, default_harness_paths(root)),
    };

    let enabled = block
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    // Three states for harness-paths:
    //   key absent          → fall back to defaults (block exists but unconfigured)
    //   key present, empty  → explicit suppression, no harness watched
    //   key present, items  → use what user wrote
    match block.get("harness-paths").and_then(|v| v.as_sequence()) {
        None => (enabled, default_harness_paths(root)),
        Some(arr) => {
            let mut paths = Vec::new();
            for entry in arr {
                let harness = entry.get("harness").and_then(|v| v.as_str()).unwrap_or("");
                let watch_path = entry.get("watch-path").and_then(|v| v.as_str()).unwrap_or("");
                let file_glob = entry.get("file-glob").and_then(|v| v.as_str()).unwrap_or("*");
                if harness.is_empty() || watch_path.is_empty() {
                    continue;
                }
                paths.push(HarnessPath {
                    harness: harness.to_string(),
                    watch_path: watch_path.to_string(),
                    file_glob: file_glob.to_string(),
                });
            }
            (enabled, paths)
        }
    }
}

/// Built-in default harness paths. Active when no explicit `raw-mirror:`
/// config is present and the repo uses a kit that needs mirroring (soul kit
/// today; extensible). Returns empty for non-soul kits so the adapter is a
/// pure no-op there.
fn default_harness_paths(root: &Path) -> Vec<HarnessPath> {
    let kit = git_lex::get_kit().unwrap_or_default();
    let is_soul = kit.ends_with("/git-lex-kit-soul")
        || kit == "soul"
        || kit.ends_with("/soul");
    if !is_soul {
        let _ = root; // silence unused warning when not soul
        return Vec::new();
    }
    vec![HarnessPath {
        harness: "ClaudeCodeSessionLog".to_string(),
        watch_path: "~/.claude/projects/<derived-from-cwd>".to_string(),
        file_glob: "*.jsonl".to_string(),
    }]
}

// ─── Path expansion ────────────────────────────────────────────

/// Expand a watch-path template into a concrete filesystem path.
///
/// Two substitutions:
/// - `~` at the start expands to `$HOME`.
/// - `<derived-from-cwd>` expands to the soul-repo's absolute path with `/`
///   replaced by `-` (Claude Code's project-dir mangling convention).
///
/// The mangling is intentionally adapter-local rather than config-loader
/// concern: future harness adapters (Hermes, etc.) won't share this rule,
/// and keeping it here lets the config stay declarative across vendors.
pub fn expand_watch_path(template: &str, soul_root: &Path) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let mut s = template.to_string();

    if s.starts_with("~/") {
        s = format!("{}/{}", home, &s[2..]);
    } else if s == "~" {
        s = home;
    }

    if s.contains("<derived-from-cwd>") {
        let derived = derive_from_cwd(soul_root);
        s = s.replace("<derived-from-cwd>", &derived);
    }

    Some(PathBuf::from(s))
}

/// Claude Code's path-mangling: absolute path with `/` → `-`. The leading
/// slash becomes a leading dash. So `/Users/rob/repos/7R1PL3F0RC3/W4R3Z`
/// becomes `-Users-rob-repos-7R1PL3F0RC3-W4R3Z`.
fn derive_from_cwd(soul_root: &Path) -> String {
    soul_root.to_string_lossy().replace('/', "-")
}

// ─── State file ────────────────────────────────────────────────

/// Path to the per-machine mirror state file. XDG state dir convention with
/// fallback to `~/.local/share/git-lex/raw-mirror-state.json`.
fn state_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        return Some(PathBuf::from(xdg).join("git-lex").join("raw-mirror-state.json"));
    }
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("git-lex")
            .join("raw-mirror-state.json"),
    )
}

fn read_state() -> MirrorState {
    let path = match state_path() {
        Some(p) => p,
        None => return MirrorState::default(),
    };
    let content = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return MirrorState::default(),
    };
    serde_json::from_str(&content).unwrap_or_default()
}

fn write_state(state: &MirrorState) {
    let path = match state_path() {
        Some(p) => p,
        None => return,
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    if let Ok(json) = serde_json::to_string_pretty(state) {
        fs::write(&path, json + "\n").ok();
    }
}

// ─── Glob matching ─────────────────────────────────────────────

/// Tiny glob: supports `*` (any-chars) and literal characters. The configs
/// we ship only need `*.jsonl`, `*.json`, `*.pdf` — full glob semantics
/// would add a dependency for no current benefit.
fn matches_glob(pattern: &str, name: &str) -> bool {
    let mut p_idx = 0;
    let mut n_idx = 0;
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let mut star_p: Option<usize> = None;
    let mut star_n: usize = 0;

    while n_idx < n.len() {
        if p_idx < p.len() && p[p_idx] == '*' {
            star_p = Some(p_idx);
            star_n = n_idx;
            p_idx += 1;
        } else if p_idx < p.len() && p[p_idx] == n[n_idx] {
            p_idx += 1;
            n_idx += 1;
        } else if let Some(sp) = star_p {
            p_idx = sp + 1;
            star_n += 1;
            n_idx = star_n;
        } else {
            return false;
        }
    }
    while p_idx < p.len() && p[p_idx] == '*' {
        p_idx += 1;
    }
    p_idx == p.len()
}

// ─── Today ─────────────────────────────────────────────────────

/// ISO `YYYY-MM-DD` for today in UTC. Used as the first-seen-date prefix
/// in Raw target filenames. The state file holds the canonical record so
/// resumes don't re-date; this is only called for sessions never seen before
/// on this machine.
fn today_utc_date() -> String {
    use std::process::Command;
    if let Ok(out) = Command::new("date").args(["-u", "+%Y-%m-%d"]).output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    // Fallback: derive from SystemTime — gives epoch-relative date in UTC.
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    // Civil-from-days (Howard Hinnant's algorithm), UTC.
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", y, m, d)
}

// ─── Mirror pass ───────────────────────────────────────────────

/// Run one mirror pass for all configured harness paths. Called by
/// `cmd_save` before `git add -A` so newly-mirrored files land in the same
/// commit. Returns a report for the caller to print.
///
/// Failures on individual files do not abort the pass — Raw is best-effort
/// at save time. A harness path that doesn't exist (e.g. user has never
/// started Claude Code in this repo) is silently skipped.
pub fn run(root: &Path) -> MirrorReport {
    let mut report = MirrorReport::default();

    let (enabled, paths) = read_config(root);
    if !enabled || paths.is_empty() {
        return report;
    }

    let mut state = read_state();
    let today = today_utc_date();
    let mut state_dirty = false;

    for hp in &paths {
        let watch_dir = match expand_watch_path(&hp.watch_path, root) {
            Some(p) => p,
            None => continue,
        };
        if !watch_dir.exists() {
            continue;
        }
        report.harnesses_checked += 1;

        let target_dir = root.join("Raw").join(&hp.harness);
        if let Err(_) = fs::create_dir_all(&target_dir) {
            continue;
        }

        let entries = match fs::read_dir(&watch_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.filter_map(|e| e.ok()) {
            let src = entry.path();
            if !src.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !matches_glob(&hp.file_glob, &name) {
                continue;
            }

            // session-id = filename without extension. For Claude Code jsonls
            // this is the UUID. Generalizes to any harness whose filename is
            // already a stable per-session identifier.
            let session_id = match src.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let ext = src.extension().and_then(|s| s.to_str()).unwrap_or("");

            let first_seen = match state.sessions.get(&session_id) {
                Some(s) => s.first_seen_date.clone(),
                None => {
                    state.sessions.insert(
                        session_id.clone(),
                        SessionState {
                            first_seen_date: today.clone(),
                            restored_at: None,
                        },
                    );
                    state_dirty = true;
                    today.clone()
                }
            };

            let target_name = if ext.is_empty() {
                format!("{}-{}", first_seen, session_id)
            } else {
                format!("{}-{}.{}", first_seen, session_id, ext)
            };
            let target = target_dir.join(&target_name);

            let src_mtime = fs::metadata(&src).and_then(|m| m.modified()).ok();
            let target_mtime = fs::metadata(&target).and_then(|m| m.modified()).ok();

            match (target.exists(), src_mtime, target_mtime) {
                (false, _, _) => {
                    if fs::copy(&src, &target).is_ok() {
                        report.new += 1;
                    }
                }
                (true, Some(s), Some(t)) if s > t => {
                    if fs::copy(&src, &target).is_ok() {
                        report.updated += 1;
                    }
                }
                _ => {
                    // Target exists, source not newer — additive-only no-op.
                }
            }
        }
    }

    if state_dirty {
        write_state(&state);
    }

    report
}

// ─── Backfill ──────────────────────────────────────────────────

/// One-shot backfill: walk configured harness paths and copy every existing
/// session file into Raw/, using each file's mtime as the date prefix (we
/// don't have a first-seen record for pre-adapter sessions). Rescues
/// historical sessions before they expire from the harness side. Raw-only —
/// no Soul-side manifests, no extraction.
pub fn backfill(root: &Path) -> MirrorReport {
    let mut report = MirrorReport::default();

    let (enabled, paths) = read_config(root);
    if !enabled || paths.is_empty() {
        return report;
    }

    let mut state = read_state();
    let mut state_dirty = false;

    for hp in &paths {
        let watch_dir = match expand_watch_path(&hp.watch_path, root) {
            Some(p) => p,
            None => continue,
        };
        if !watch_dir.exists() {
            continue;
        }
        report.harnesses_checked += 1;

        let target_dir = root.join("Raw").join(&hp.harness);
        if let Err(_) = fs::create_dir_all(&target_dir) {
            continue;
        }

        let entries = match fs::read_dir(&watch_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.filter_map(|e| e.ok()) {
            let src = entry.path();
            if !src.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !matches_glob(&hp.file_glob, &name) {
                continue;
            }

            let session_id = match src.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let ext = src.extension().and_then(|s| s.to_str()).unwrap_or("");

            // For backfill we use mtime-as-date if no prior state. If state
            // already has a first-seen-date (because the live mirror has
            // seen this session), prefer that — keeps idempotency with run().
            let first_seen = match state.sessions.get(&session_id) {
                Some(s) => s.first_seen_date.clone(),
                None => {
                    let date = src
                        .metadata()
                        .and_then(|m| m.modified())
                        .ok()
                        .map(date_from_systime)
                        .unwrap_or_else(today_utc_date);
                    state.sessions.insert(
                        session_id.clone(),
                        SessionState {
                            first_seen_date: date.clone(),
                            restored_at: None,
                        },
                    );
                    state_dirty = true;
                    date
                }
            };

            let target_name = if ext.is_empty() {
                format!("{}-{}", first_seen, session_id)
            } else {
                format!("{}-{}.{}", first_seen, session_id, ext)
            };
            let target = target_dir.join(&target_name);

            if !target.exists() {
                if fs::copy(&src, &target).is_ok() {
                    report.new += 1;
                }
            }
        }
    }

    if state_dirty {
        write_state(&state);
    }

    report
}

fn date_from_systime(t: SystemTime) -> String {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_star_jsonl() {
        assert!(matches_glob("*.jsonl", "abc.jsonl"));
        assert!(matches_glob("*.jsonl", "x.jsonl"));
        assert!(!matches_glob("*.jsonl", "x.json"));
        assert!(!matches_glob("*.jsonl", "abc"));
    }

    #[test]
    fn glob_star_any() {
        assert!(matches_glob("*", "anything"));
        assert!(matches_glob("*", ""));
    }

    #[test]
    fn glob_exact() {
        assert!(matches_glob("foo.txt", "foo.txt"));
        assert!(!matches_glob("foo.txt", "bar.txt"));
    }

    #[test]
    fn derive_cwd_mangling() {
        let p = PathBuf::from("/Users/rob/repos/7R1PL3F0RC3/W4R3Z");
        assert_eq!(
            derive_from_cwd(&p),
            "-Users-rob-repos-7R1PL3F0RC3-W4R3Z"
        );
    }
}
