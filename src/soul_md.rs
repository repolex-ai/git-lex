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

/// Is this a soul repo? True when the repo's domain kit resolves to the
/// short name `soul`. Optional kits can't make a repo a soul repo.
pub(crate) fn soul_kit_installed(root: &Path) -> bool {
    git_lex::RepoYml::load(root)
        .domain_kit()
        .map(|k| git_lex::resolve_kit_spec(&k).2 == "soul")
        .unwrap_or(false)
}

/// What `heal` did to the root SOUL.md.
pub(crate) enum HealOutcome {
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
pub(crate) fn heal_soul_id(root: &Path) -> HealOutcome {
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
    match healed_content(&content, &sha) {
        None => HealOutcome::Unchanged,
        Some((updated, previous)) => {
            if let Err(e) = fs::write(&path, updated) {
                eprintln!("warning: could not write SOUL.md soulId: {}", e);
                return HealOutcome::Unchanged;
            }
            match previous {
                None => {
                    println!("SOUL.md: soulId filled from genesis sha ({}).", &sha[..8.min(sha.len())]);
                    HealOutcome::Filled
                }
                Some(prev) => {
                    println!(
                        "SOUL.md: soulId healed to the genesis sha ({}) — was `{}`. \
                         soulId is derived; never edit it by hand.",
                        &sha[..8.min(sha.len())],
                        prev
                    );
                    HealOutcome::Healed
                }
            }
        }
    }
}

/// Fail-loud gate for wake (`sync`) and `save`: a soul repo without its root
/// SOUL.md has no identity floor. Exits the process with restore
/// instructions; a no-op for non-soul repos or when the file exists.
pub(crate) fn require_soul_md(root: &Path) {
    if !soul_kit_installed(root) {
        return;
    }
    if root.join("SOUL.md").is_file() {
        return;
    }
    eprintln!("fatal: root SOUL.md is missing — this is a soul repo and SOUL.md is its identity floor.");
    eprintln!("Restore it: `git lex kit-update` reinstalls the template when the file is missing");
    eprintln!("(an existing SOUL.md is never overwritten), then fills soulId from the genesis sha.");
    std::process::exit(1);
}

/// Pure transform. Returns `None` when the content already carries the
/// correct soulId; otherwise `Some((new_content, previous_value))` where
/// `previous_value` is `Some(old)` for a corrected wrong value and `None`
/// for a fill (empty or absent line).
///
/// Rules, matching the extractor's frontmatter framing
/// (extraction.rs::frontmatter_to_turtle — `---\n` opener, closer found via
/// `\n---`):
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

    // Frontmatter framing identical to the extractor: body starts after the
    // opener line; the closer is the next line beginning `---`.
    let opener_len = if content.starts_with("---\r\n") { 5 } else { 4 };
    let rest = &content[opener_len..];
    let Some(close) = rest.find("\n---") else {
        // Unterminated frontmatter — malformed; extraction would fail loud.
        // Don't compound it by editing.
        return None;
    };
    let block = &rest[..close];

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
            out.push_str(&content[..opener_len]);
            out.push_str(&new_block.join("\n"));
            out.push_str(&rest[close..]);
            Some((out, previous))
        }
        None => {
            let mut out = String::with_capacity(content.len() + sha.len() + 32);
            out.push_str(&content[..opener_len]);
            out.push_str(&format!("{}: {}\n", SOUL_ID_KEY, sha));
            out.push_str(rest);
            Some((out, None))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "495d8c70114a9918ecba0bc4d49ba3653b5930fb";

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
        let content = format!("---\nsoul.Soul.soulId: deadbeef\n---\n\nbody\n");
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
}
