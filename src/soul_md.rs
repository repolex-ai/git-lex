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

/// The soul's one-word name, which the soul's own human or the soul writes
/// once. git-lex reads it and never writes it.
const USERNAME_KEY: &str = "soul.Soul.username";

fn soul_thing_id(id: &str) -> String {
    format!("<soul/Soul/{id}>")
}

/// The value of a one-line frontmatter key: quotes and a trailing comment
/// removed. None when the key is absent or its value is empty.
fn line_value(block: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    let line = block.lines().find(|l| l.trim_start().starts_with(&prefix))?;
    let after = &line[line.find(':')? + 1..];
    let value = after.split(" #").next().unwrap_or("").trim().trim_matches('"').trim_matches('\'').trim();
    (!value.is_empty() && !value.starts_with('#')).then(|| value.to_string())
}

/// The username a SOUL.md declares, exactly as written. Whether it is a
/// usable one is `identity::valid_username`'s to say.
pub fn declared_username(content: &str) -> Option<String> {
    line_value(crate::split_frontmatter(content).0?, USERNAME_KEY)
}

/// [`declared_username`] for the SOUL.md at a repo's root.
pub fn username_at(root: &Path) -> Option<String> {
    declared_username(&fs::read_to_string(root.join("SOUL.md")).ok()?)
}

/// Pure transform: the content with its `soul.Soul.id` line added or moved
/// to the soul's username.
///
/// A soul is addressed by its username (goodlux, 2026-11-19); `soulId`
/// keeps the first commit. So the line is `<soul/Soul/{username}>` when the
/// file declares a usable username, and `<soul/Soul/{first commit}>` for a
/// soul that has not declared one yet. An existing line is rewritten in one
/// case only: it holds the first-commit form this function wrote earlier
/// and a username is now declared. Any other line is left exactly as it is.
/// None when nothing changes, and when there is no well-formed frontmatter.
fn with_thing_id(content: &str, sha: &str) -> Option<String> {
    let (fm, body) = crate::split_frontmatter(content);
    let block = fm?;
    let username = line_value(block, USERNAME_KEY).filter(|n| crate::identity::valid_username(n));
    let want = soul_thing_id(username.as_deref().unwrap_or(sha));
    let prefix = format!("{SOUL_THING_ID_KEY}:");
    let mut lines: Vec<String> = block.lines().map(String::from).collect();
    match lines.iter().position(|l| l.trim_start().starts_with(&prefix)) {
        Some(i) => {
            let old = soul_thing_id(sha);
            if username.is_none() || line_value(block, SOUL_THING_ID_KEY).as_deref() != Some(old.as_str()) {
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

/// Pure transform: `.lex/repo.yml` with the three lines that say which
/// store this is (goodlux, 2026-11-19). `username` follows SOUL.md, so a
/// line that differs is corrected. `squad` and `visibility` are written
/// only when absent: a line that is there is someone's answer. None when
/// nothing changes.
fn with_store_lines(content: &str, squad: Option<&str>, username: &str) -> Option<String> {
    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    let find = |lines: &[String], key: &str| lines.iter().position(|l| l.starts_with(&format!("{key}:")));
    let mut add = Vec::new();
    if let Some(squad) = squad
        && find(&lines, "squad").is_none()
    {
        add.push(format!("squad: {squad}"));
    }
    let want = format!("username: {username}");
    match find(&lines, "username") {
        Some(i) if lines[i] != want => lines[i] = want.clone(),
        Some(_) => {}
        None => add.push(want.clone()),
    }
    if find(&lines, "visibility").is_none() {
        add.push("visibility: personal".to_string());
    }
    // Beside the first commit they are read with, else at the end.
    let at = find(&lines, "genesis_sha").map_or(lines.len(), |i| i + 1);
    lines.splice(at..at, add);
    let out = lines.join("\n") + "\n";
    (out != content).then_some(out)
}

/// Keep a soul repo's `.lex/repo.yml` saying which store it is: `squad`,
/// `username`, `visibility`. The username is the one SOUL.md declares; the
/// squad is the name of the folder the soul's folder sits in, when that is
/// one lowercase word. A soul with no usable username is left alone, and so
/// is a repo that is not a soul: nothing in it says what its name is.
pub fn heal_store_lines(root: &Path) {
    if !soul_kit_installed(root) {
        return;
    }
    let Some(username) = username_at(root).filter(|n| crate::identity::valid_username(n)) else {
        return;
    };
    let squad = root
        .canonicalize()
        .ok()
        .and_then(|p| p.parent()?.file_name()?.to_str().map(String::from))
        .filter(|s| crate::identity::valid_username(s));
    let path = crate::layout::repo_yml(root);
    let Ok(content) = fs::read_to_string(&path) else {
        return;
    };
    if let Some(updated) = with_store_lines(&content, squad.as_deref(), &username) {
        match fs::write(&path, updated) {
            Ok(()) => println!(".lex/repo.yml: this store is {}{username}.", squad.map(|s| s + "/").unwrap_or_default()),
            Err(e) => eprintln!("warning: could not write {}: {e}", path.display()),
        }
    }
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
        (None, Some(only_thing_id)) => {
            // soulId was right; only the `.id` line moves. Say that, not
            // "soulId filled".
            if write {
                match fs::write(&path, &only_thing_id) {
                    Ok(()) => println!(
                        "SOUL.md: {SOUL_THING_ID_KEY} is now {}.",
                        line_value(crate::split_frontmatter(&only_thing_id).0.unwrap_or(""), SOUL_THING_ID_KEY).unwrap_or_default()
                    ),
                    Err(e) => {
                        eprintln!("warning: could not write SOUL.md {SOUL_THING_ID_KEY}: {e}");
                        return HealOutcome::Unchanged;
                    }
                }
            }
            return HealOutcome::Filled;
        }
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
        assert_eq!(with_thing_id(&filled, SHA), None, "there already: untouched");
        let other = filled.replace(&format!("<soul/Soul/{SHA}>"), "<soul/Soul/kira>");
        assert_eq!(with_thing_id(&other, SHA), None, "a line git-lex did not write is never rewritten");
        assert_eq!(with_thing_id("no frontmatter\n", SHA), None);
    }

    #[test]
    fn a_soul_that_declares_a_username_is_addressed_by_it() {
        let old = format!("---\n{SOUL_ID_KEY}: {SHA}\nsoul.Soul.id: <soul/Soul/{SHA}>\nsoul.Soul.username: \"w4r3z\" # one word\n---\n\nbody\n");
        let moved = with_thing_id(&old, SHA).expect("the first-commit form moves to the username");
        assert_eq!(
            moved,
            format!("---\n{SOUL_ID_KEY}: {SHA}\nsoul.Soul.id: <soul/Soul/w4r3z>\nsoul.Soul.username: \"w4r3z\" # one word\n---\n\nbody\n")
        );
        assert!(moved.contains(&format!("{SOUL_ID_KEY}: {SHA}\n")), "soulId keeps the first commit");
        assert_eq!(with_thing_id(&moved, SHA), None, "and then it is left alone");

        // A new soul with a username gets the username form straight away.
        let new = format!("---\n{SOUL_ID_KEY}: {SHA}\nsoul.Soul.username: kira\n---\n");
        assert!(with_thing_id(&new, SHA).unwrap().contains("soul.Soul.id: <soul/Soul/kira>\n"));

        // A name that is not usable changes nothing: all hex digits, two
        // words, a kit's name, an empty line.
        for bad in ["c0ffee", "tr1p-l3x", "TR1P", "soul", ""] {
            let file = old.replace("\"w4r3z\" # one word", bad);
            assert_eq!(with_thing_id(&file, SHA), None, "{bad:?}");
        }
        assert_eq!(declared_username(&old).as_deref(), Some("w4r3z"));
    }

    #[test]
    fn repo_yml_says_which_store_this_is() {
        let before = "# managed\nname: W4R3Z\ngenesis_sha: abc\n\noptional_kits:\n  - a/b\n";
        let after = with_store_lines(before, Some("7r1pl3f0rc3"), "w4r3z").unwrap();
        assert_eq!(
            after,
            "# managed\nname: W4R3Z\ngenesis_sha: abc\nsquad: 7r1pl3f0rc3\nusername: w4r3z\nvisibility: personal\n\noptional_kits:\n  - a/b\n"
        );
        assert_eq!(with_store_lines(&after, Some("7r1pl3f0rc3"), "w4r3z"), None, "a second run changes nothing");
        // The username follows SOUL.md; squad and visibility, once there, are kept.
        let set = "squad: elsewhere\nusername: old\nvisibility: shared\n";
        assert_eq!(with_store_lines(set, Some("7r1pl3f0rc3"), "w4r3z").unwrap(), "squad: elsewhere\nusername: w4r3z\nvisibility: shared\n");
        // No usable folder name: no squad line is invented.
        assert_eq!(with_store_lines("name: x\n", None, "w4r3z").unwrap(), "name: x\nusername: w4r3z\nvisibility: personal\n");
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
