//! Root SOUL.md identity floor — soulId fill + self-heal.
//!
//! soulId = the BARE genesis (first-commit) sha (Rob + tr1p ruled
//! 2026-08-01); the IRI `<ns>/Soul/<soulId>` is DERIVED, never stored.
//! The soul kit ships the template with an empty `soul.Soul.soulId:` line
//! and a comment declaring this contract; git-lex owns the VALUE: fill it
//! at init, self-heal it at kit-update — including correcting a WRONG
//! value back to the true genesis sha (the identity-floor contract), not
//! just filling a missing one. Agents never type it by hand.
//!
//! Only the soulId line is ever touched. The rest of SOUL.md is the
//! squaddie's own and is never modified here (matching the never-overwrite
//! rule in kit.rs::is_never_overwrite).

use std::fs;
use std::path::Path;

const SOUL_ID_KEY: &str = "soul.Soul.soulId";
/// Every Thing carries its `.id` line (goodlux, 2026-08-28), and the save
/// gate requires it. A soul's is derived from the same first commit as its
/// soulId, so git-lex writes it; the kit's template cannot.
const SOUL_THING_ID_KEY: &str = "soul.Soul.id";

fn soul_thing_id(sha: &str) -> String {
    format!("<soul/Soul/{sha}>")
}

/// Pure transform: the content with its `soul.Soul.id` line set to the id
/// derived from the first commit. None when it is already right, and when
/// there is no well-formed frontmatter to put it in (the soulId fill runs
/// first and makes one).
fn with_thing_id(content: &str, sha: &str) -> Option<String> {
    let (fm, body) = crate::split_frontmatter(content);
    let block = fm?;
    let want = soul_thing_id(sha);
    let prefix = format!("{SOUL_THING_ID_KEY}:");
    let mut lines: Vec<String> = block.lines().map(String::from).collect();
    match lines.iter().position(|l| l.trim_start().starts_with(&prefix)) {
        Some(i) => {
            let value = lines[i].trim_start()[prefix.len()..].trim();
            if value == want {
                return None;
            }
            lines[i] = format!("{SOUL_THING_ID_KEY}: {want}");
        }
        None => {
            // Beside the soulId line it is derived with, else first.
            let soul_id = format!("{SOUL_ID_KEY}:");
            let at = lines.iter().position(|l| l.trim_start().starts_with(&soul_id)).map_or(0, |i| i + 1);
            lines.insert(at, format!("{SOUL_THING_ID_KEY}: {want}"));
        }
    }
    Some(format!("---\n{}\n---\n{}", lines.join("\n"), body))
}

/// Is this a soul repo? True when the repo's domain kit resolves to the
/// short name `soul`. Optional kits can't make a repo a soul repo.
pub fn soul_kit_installed(root: &Path) -> bool {
    crate::RepoYml::load(root)
        .domain_kit()
        .map(|k| crate::resolve_kit_spec(&k).2 == "soul")
        .unwrap_or(false)
}

/// What `heal` did to the root SOUL.md.
pub enum HealOutcome {
    /// Not a soul repo — nothing to do.
    NotSoulRepo,
    /// Soul repo, but no root SOUL.md on disk. Callers decide severity:
    /// kit-update prints a pointer (scaffold install should have restored
    /// it); sync/save fail loud via `require_soul_md`.
    NoSoulMd,
    /// Genesis sha not resolvable yet (no commits) — value left alone.
    NoGenesis,
    /// soulId already correct.
    Unchanged,
    /// soulId was empty/missing and has been filled.
    Filled,
    /// soulId held a wrong value and has been corrected (the receipt with
    /// the previous value prints inside `heal_soul_id`).
    Healed,
}

/// Fill or correct `soul.Soul.soulId:` in the root SOUL.md from the genesis
/// sha. Returns what happened; writes the file only when the value changes.
pub fn heal_soul_id(root: &Path) -> HealOutcome {
    heal_soul_id_inner(root, true)
}

/// What `heal_soul_id` WOULD do, without doing it.
///
/// `save --dry-run` is a probe: it must report what a real save would do and
/// change nothing. Healing from inside the probe would mean a dry run edits
/// the identity file — the one file whose whole point is that it is not
/// casually rewritten.
pub fn preview_soul_id_heal(root: &Path) -> HealOutcome {
    heal_soul_id_inner(root, false)
}

/// Shared body. `write` is the only difference between doing and previewing,
/// so the two can never drift into disagreeing about what needs fixing.
fn heal_soul_id_inner(root: &Path, write: bool) -> HealOutcome {
    if !soul_kit_installed(root) {
        return HealOutcome::NotSoulRepo;
    }
    let path = root.join("SOUL.md");
    let Ok(content) = fs::read_to_string(&path) else {
        return HealOutcome::NoSoulMd;
    };
    let Some(sha) = crate::git::genesis_sha() else {
        return HealOutcome::NoGenesis;
    };
    // soulId first, then the `.id` line beside it. A file whose soulId was
    // already right but which has no `.id` line counts as a fill.
    let soul_id_fix = healed_content(&content, &sha);
    let after_soul_id = soul_id_fix.as_ref().map_or(content.as_str(), |(updated, _)| updated.as_str());
    let thing_id_fix = with_thing_id(after_soul_id, &sha);
    let fix = match (soul_id_fix.clone(), thing_id_fix) {
        (None, None) => None,
        (Some((_, previous)), Some(both)) => Some((both, previous)),
        (Some(only_soul_id), None) => Some(only_soul_id),
        (None, Some(only_thing_id)) => Some((only_thing_id, None)),
    };
    match fix {
        None => HealOutcome::Unchanged,
        Some((updated, previous)) => {
            if write
                && let Err(e) = fs::write(&path, updated) {
                    eprintln!("warning: could not write SOUL.md soulId: {}", e);
                    return HealOutcome::Unchanged;
                }
            match previous {
                None => {
                    if write {
                        println!("SOUL.md: soulId filled from genesis sha ({}).", &sha[..8.min(sha.len())]);
                    }
                    HealOutcome::Filled
                }
                Some(prev) => {
                    if write {
                        println!(
                            "SOUL.md: soulId healed to the genesis sha ({}) — was `{}`. \
                             soulId is derived; never edit it by hand.",
                            &sha[..8.min(sha.len())],
                            prev
                        );
                    }
                    HealOutcome::Healed
                }
            }
        }
    }
}

/// Fail-loud gate for wake (`sync`) and `save`: a soul repo without its root
/// SOUL.md has no identity floor. Fails with restore instructions; a no-op
/// for non-soul repos or when the file exists.
pub fn require_soul_md(root: &Path) -> crate::Outcome {
    if !soul_kit_installed(root) || root.join("SOUL.md").is_file() {
        return Ok(());
    }
    Err(crate::Failure::new(
        "fatal: root SOUL.md is missing — this is a soul repo and SOUL.md is its identity floor.\n\
         Restore it: `git lex kit-update` reinstalls the template when the file is missing\n\
         (an existing SOUL.md is never overwritten), then fills soulId from the genesis sha.",
    ))
}

/// Pure transform. Returns `None` when the content already carries the
/// correct soulId; otherwise `Some((new_content, previous_value))` where
/// `previous_value` is `Some(old)` for a corrected wrong value and `None`
/// for a fill (empty or absent line).
///
/// Rules, matching the extractor's frontmatter framing (the ONE shared
/// fence parser, crate::split_frontmatter — review #9):
/// - soulId line present: replace the value, PRESERVING any trailing
///   `# comment` (the kit template's "never type it by hand" warning
///   survives the fill). serde_yaml strips the comment at read time.
/// - line absent but frontmatter exists: insert the key as the block's
///   first line.
/// - no frontmatter at all: prepend a minimal block (the one system-owned
///   key only — the body is the squaddie's and is not touched).
fn healed_content(content: &str, sha: &str) -> Option<(String, Option<String>)> {
    let has_frontmatter = content.starts_with("---\n") || content.starts_with("---\r\n");

    if !has_frontmatter {
        return Some((
            format!("---\n{}: {}\n---\n\n{}", SOUL_ID_KEY, sha, content),
            None,
        ));
    }

    // Frontmatter framing = the shared fence parser, so heal and extraction
    // can never frame the same file differently. A heal rewrite normalizes
    // the fences themselves to `---\n`; block content and body are spliced
    // byte-exact.
    let (fm, body) = crate::split_frontmatter(content);
    let Some(block) = fm else {
        // Opener with no closer — malformed; extraction would fail loud.
        // Don't compound it by editing.
        return None;
    };

    let key_prefix = format!("{}:", SOUL_ID_KEY);
    let mut found: Option<(usize, &str)> = None;
    for (i, line) in block.lines().enumerate() {
        if line.trim_start().starts_with(&key_prefix) {
            found = Some((i, line));
            break;
        }
    }

    match found {
        Some((idx, line)) => {
            let after_key = &line[line.find(':').unwrap() + 1..];
            // Split value from trailing comment; shas are hex so a `#` can
            // only start a comment.
            let (value_part, comment) = match after_key.find('#') {
                Some(h) => (&after_key[..h], Some(after_key[h..].trim_end())),
                None => (after_key, None),
            };
            let value = value_part.trim().trim_matches('"').trim_matches('\'');
            if value == sha {
                return None;
            }
            let new_line = match comment {
                Some(c) => format!("{}: {} {}", SOUL_ID_KEY, sha, c),
                None => format!("{}: {}", SOUL_ID_KEY, sha),
            };
            let previous = if value.is_empty() { None } else { Some(value.to_string()) };
            let new_block: Vec<String> = block
                .lines()
                .enumerate()
                .map(|(i, l)| if i == idx { new_line.clone() } else { l.to_string() })
                .collect();
            let mut out = String::with_capacity(content.len() + sha.len());
            out.push_str("---\n");
            out.push_str(&new_block.join("\n"));
            out.push_str("\n---\n");
            out.push_str(body);
            Some((out, previous))
        }
        None => {
            let mut out = String::with_capacity(content.len() + sha.len() + 32);
            out.push_str("---\n");
            out.push_str(&format!("{}: {}\n", SOUL_ID_KEY, sha));
            out.push_str(block);
            out.push_str("---\n");
            out.push_str(body);
            Some((out, None))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "495d8c70114a9918ecba0bc4d49ba3653b5930fb";

    #[test]
    fn the_thing_id_line_is_written_beside_the_soul_id_and_then_left_alone() {
        let template = format!("---\n{SOUL_ID_KEY}: {SHA} # derived\nsoul.Soul.role: # optional\n---\n\n# name\n");
        let filled = with_thing_id(&template, SHA).expect("a template has no .id line");
        assert_eq!(
            filled,
            format!("---\n{SOUL_ID_KEY}: {SHA} # derived\nsoul.Soul.id: <soul/Soul/{SHA}>\nsoul.Soul.role: # optional\n---\n\n# name\n")
        );
        assert_eq!(with_thing_id(&filled, SHA), None, "right already: untouched");
        let wrong = filled.replace(&format!("<soul/Soul/{SHA}>"), "<soul/Soul/kira>");
        assert_eq!(with_thing_id(&wrong, SHA).as_deref(), Some(filled.as_str()), "a wrong one is corrected");
        assert_eq!(with_thing_id("no frontmatter\n", SHA), None);
    }

    #[test]
    fn fills_empty_template_line_preserving_comment() {
        let content = "---\nsoul.Soul.soulId: # derived — never type it by hand\nsoul.Soul.role: builder\n---\n\n# W4R3Z\n";
        let (out, prev) = healed_content(content, SHA).expect("should fill");
        assert!(prev.is_none(), "empty value is a fill, not a heal");
        assert!(out.contains(&format!(
            "soul.Soul.soulId: {} # derived — never type it by hand",
            SHA
        )));
        assert!(out.contains("soul.Soul.role: builder"));
        assert!(out.ends_with("# W4R3Z\n"), "body untouched");
    }

    #[test]
    fn heals_wrong_value_and_reports_previous() {
        let content = "---\nsoul.Soul.soulId: deadbeef\n---\n\nbody\n".to_string();
        let (out, prev) = healed_content(&content, SHA).expect("should heal");
        assert_eq!(prev.as_deref(), Some("deadbeef"));
        assert!(out.contains(&format!("soul.Soul.soulId: {}\n", SHA)));
        assert!(!out.contains("deadbeef"));
    }

    #[test]
    fn correct_value_is_untouched() {
        let content = format!("---\nsoul.Soul.soulId: {}\n---\n\nbody\n", SHA);
        assert!(healed_content(&content, SHA).is_none());
    }

    #[test]
    fn correct_quoted_value_is_untouched() {
        let content = format!("---\nsoul.Soul.soulId: \"{}\"\n---\n\nbody\n", SHA);
        assert!(
            healed_content(&content, SHA).is_none(),
            "quoted-but-correct must not churn the file"
        );
    }

    #[test]
    fn missing_key_inserted_as_first_frontmatter_line() {
        let content = "---\nsoul.Soul.role: builder\n---\n\n# Me\n";
        let (out, prev) = healed_content(content, SHA).expect("should insert");
        assert!(prev.is_none());
        assert!(out.starts_with(&format!("---\nsoul.Soul.soulId: {}\nsoul.Soul.role: builder\n---", SHA)));
    }

    #[test]
    fn no_frontmatter_gets_minimal_block_prepended() {
        let content = "# W4R3Z\n\nJust prose, no frontmatter.\n";
        let (out, prev) = healed_content(content, SHA).expect("should prepend");
        assert!(prev.is_none());
        assert!(out.starts_with(&format!("---\nsoul.Soul.soulId: {}\n---\n\n# W4R3Z", SHA)));
    }

    #[test]
    fn unterminated_frontmatter_left_alone() {
        let content = "---\nsoul.Soul.soulId: deadbeef\nno closer here\n";
        assert!(
            healed_content(content, SHA).is_none(),
            "malformed file: extraction fails loud; healing must not compound it"
        );
    }

    /// The bug this split exists to prevent: gating save's identity heal
    /// behind `dry_run` was added and the FIRST version healed inside the
    /// probe too — a `--dry-run` that rewrites the identity file. Preview and
    /// heal must agree on WHAT needs fixing and disagree only on writing.
    #[test]
    fn preview_reports_the_same_drift_it_refuses_to_write() {
        let tampered = format!("---\n{}: kira\nsoul.Soul.role: builder\n---\n\n# W4R3Z\n", SOUL_ID_KEY);

        // The shared body decides; both entry points read the same answer.
        let (updated, previous) = healed_content(&tampered, SHA)
            .expect("a hand-edited soulId is drift and must be detected");
        assert_eq!(previous.as_deref(), Some("kira"),
            "the preview has to be able to NAME the wrong value, not just flag it");
        assert!(updated.contains(&format!("{}: {}", SOUL_ID_KEY, SHA)),
            "the correction is the genesis sha, derived — never invented");

        // And a correct file is drift-free, so neither path fires on it.
        let clean = format!("---\n{}: {}\n---\n\n# W4R3Z\n", SOUL_ID_KEY, SHA);
        assert!(healed_content(&clean, SHA).is_none(),
            "a correct soulId must not be rewritten — a heal that always fires is churn");
    }
}
