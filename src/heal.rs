//! Markdown link healing (Rob-ruled 2026-08-14, lifecycle spec ruling 1):
//! "if a file moves, markdown links move with it. This is work the agent
//! would have to do anyway — git-lex does it, at save time, in the same
//! commit, every edited file named."
//!
//! Runs inside `git lex save` after staging: staged RENAMES of .md files
//! (git's own -M detection — no parallel manifest, the rename map IS git's)
//! become a rewrite pass over every tracked .md file. Each inline link
//! destination is resolved EXACTLY the way the extractor resolves it
//! (fragment-strip, percent-decode, normalize against the linking file's
//! directory — one resolution policy, the A5 law); a destination that
//! resolves to a renamed file's OLD path is rewritten to the new path in
//! CANONICAL root-relative form (`/Soul/Note/x.md` — the full-path law),
//! fragment preserved. Edited files are re-staged so the heal lands in the
//! SAME commit as the rename.
//!
//! Scope, stated honestly:
//! - Inline links only (`[text](path)`) — the same set the extractor reads;
//!   reference-style definitions are invisible to both, by symmetry.
//! - INBOUND healing only: links pointing AT a moved file. A moved file's
//!   own `../`-relative outbound links are not rewritten (root-relative
//!   outbound links — the canonical form — survive any move untouched,
//!   and same-directory renames leave sibling-relative links resolvable).
//! - Structural references (frontmatter `<namespace/Class/id>`) are NEVER
//!   healed — Rob-ruled: ids don't follow filenames; the filename↔id gate
//!   owns that boundary.

use std::path::Path;
use std::process::Command;

/// A staged rename: (old repo-relative path, new repo-relative path).
pub(crate) type RenameMap = Vec<(String, String)>;

/// Read staged .md renames from git's own rename detection. Same command
/// shape as the sidecar cleanup's staged-change query (-M50%, -z), R
/// records only. An error is an error — the caller decides posture; it
/// must never read as "no renames".
pub(crate) fn staged_md_renames(root: &Path) -> Result<RenameMap, String> {
    let out = Command::new("git")
        .current_dir(root)
        .args([
            "diff", "--cached", "--name-status", "-M50%", "-z", "--", "*.md", ":!.lex/",
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
    Ok(parse_rename_records(&String::from_utf8_lossy(&out.stdout)))
}

/// Pure parser for `-z` name-status records: fields are NUL-separated,
/// a rename is `R<score>\0old\0new`, everything else is `X\0path`.
pub(crate) fn parse_rename_records(raw: &str) -> RenameMap {
    let mut fields = raw.split('\0').filter(|f| !f.is_empty());
    let mut renames = Vec::new();
    while let Some(status) = fields.next() {
        if status.starts_with('R') {
            if let (Some(old), Some(new)) = (fields.next(), fields.next()) {
                renames.push((old.to_string(), new.to_string()));
            }
        } else {
            // A/M/D/C…: rename records aside, C (copy) also carries two
            // paths; consume accordingly so the frame never desyncs.
            let extra = if status.starts_with('C') { 2 } else { 1 };
            for _ in 0..extra {
                fields.next();
            }
        }
    }
    renames
}

/// One healed file: (repo-relative path, number of links rewritten).
pub(crate) type HealReport = Vec<(String, usize)>;

/// Rewrite every inline-link destination in `content` that resolves to a
/// renamed file's old path. Returns the healed content and the number of
/// links rewritten (0 = no change; the caller skips the write).
///
/// Pure — all I/O stays in [`heal_staged_renames`] — so the acceptance
/// rows run as unit tests.
pub(crate) fn heal_content(content: &str, doc_relpath: &str, renames: &RenameMap) -> (String, usize) {
    let mut parser = tree_sitter_md::MarkdownParser::default();
    let Some(tree) = parser.parse(content.as_bytes(), None) else {
        return (content.to_string(), 0);
    };
    let doc_dir = match doc_relpath.rfind('/') {
        Some(pos) => &doc_relpath[..pos],
        None => "",
    };

    // Collect (byte range, replacement destination), applied back-to-front.
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    fn walk(
        node: tree_sitter::Node,
        source: &str,
        doc_dir: &str,
        renames: &RenameMap,
        edits: &mut Vec<(std::ops::Range<usize>, String)>,
    ) {
        if node.kind() == "inline_link" {
            if let Some(dest_node) = node
                .children(&mut node.walk())
                .find(|c| c.kind() == "link_destination")
            {
                let dest = &source[dest_node.start_byte()..dest_node.end_byte()];
                if !dest.starts_with("http://") && !dest.starts_with("https://") {
                    // The extractor's resolution, step for step.
                    let (path_part, fragment) = match dest.find('#') {
                        Some(pos) => (&dest[..pos], &dest[pos..]),
                        None => (dest, ""),
                    };
                    let target = crate::extraction::percent_decode(path_part);
                    if !target.is_empty() {
                        if let Some(resolved) =
                            crate::extraction::normalize_wikilink_path(&target, doc_dir)
                        {
                            if let Some((_, new_path)) =
                                renames.iter().find(|(old, _)| *old == resolved)
                            {
                                // Canonical root-relative form (full-path
                                // law), fragment preserved, spaces re-encoded
                                // (the only decode-sensitive byte a repo path
                                // realistically carries in a destination).
                                let healed = format!(
                                    "/{}{}",
                                    new_path.replace(' ', "%20"),
                                    fragment
                                );
                                edits.push((
                                    dest_node.start_byte()..dest_node.end_byte(),
                                    healed,
                                ));
                            }
                        }
                    }
                }
            }
        }
        for child in node.children(&mut node.walk()) {
            walk(child, source, doc_dir, renames, edits);
        }
    }
    // tree_sitter_md wraps block and inline trees; walk both the way the
    // extractor does — inline links live in the inline trees.
    for inline_tree in tree.inline_trees() {
        walk(inline_tree.root_node(), content, doc_dir, renames, &mut edits);
    }

    if edits.is_empty() {
        return (content.to_string(), 0);
    }
    let count = edits.len();
    let mut healed = content.to_string();
    edits.sort_by_key(|(range, _)| range.start);
    for (range, replacement) in edits.into_iter().rev() {
        healed.replace_range(range, &replacement);
    }
    (healed, count)
}

/// The save-time pass: detect staged renames, heal every tracked .md file,
/// write and re-stage the edits. Returns the per-file report (empty = no
/// renames or nothing referenced them).
pub(crate) fn heal_staged_renames(root: &Path) -> Result<HealReport, String> {
    let renames = staged_md_renames(root)?;
    if renames.is_empty() {
        return Ok(Vec::new());
    }

    // Tracked .md files, current tree — the healing surface. `git ls-files`
    // sees the post-rename names (the rename is staged).
    let out = Command::new("git")
        .current_dir(root)
        .args(["ls-files", "-z", "--", "*.md", ":!.lex/"])
        .output()
        .map_err(|e| format!("git ls-files spawn failed: {e}"))?;
    if !out.status.success() {
        return Err(format!("git ls-files failed ({})", out.status));
    }
    let files: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|f| !f.is_empty())
        .map(|f| f.to_string())
        .collect();

    let mut report = Vec::new();
    for relpath in files {
        let abs = root.join(&relpath);
        let Ok(content) = std::fs::read_to_string(&abs) else { continue };
        let (healed, count) = heal_content(&content, &relpath, &renames);
        if count == 0 {
            continue;
        }
        std::fs::write(&abs, &healed)
            .map_err(|e| format!("healing {relpath}: write failed: {e}"))?;
        let staged = Command::new("git")
            .current_dir(root)
            .args(["add", "--", &relpath])
            .status()
            .map_err(|e| format!("healing {relpath}: git add spawn failed: {e}"))?;
        if !staged.success() {
            return Err(format!("healing {relpath}: git add failed"));
        }
        report.push((relpath, count));
    }
    report.sort();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The -z record frame: renames parse as pairs, other statuses (incl.
    /// two-path copies) never desync the frame.
    #[test]
    fn rename_records_parse_and_frame_survives_mixed_statuses() {
        let raw = "M\0a.md\0R100\0Soul/Note/old.md\0Soul/Note/20260821-new.md\0A\0b.md\0C75\0src.md\0dup.md\0R080\0x.md\0y.md\0";
        assert_eq!(
            parse_rename_records(raw),
            vec![
                ("Soul/Note/old.md".to_string(), "Soul/Note/20260821-new.md".to_string()),
                ("x.md".to_string(), "y.md".to_string()),
            ]
        );
    }

    /// The core contract: root-relative, sibling-relative, and `../`
    /// inbound links to a renamed file all heal to the canonical
    /// root-relative form; fragments survive; unrelated links, external
    /// links, and links inside code fences are untouched.
    #[test]
    fn inbound_links_heal_to_canonical_form() {
        let renames = vec![(
            "Soul/Note/old-name.md".to_string(),
            "Soul/Note/20260821-old-name.md".to_string(),
        )];

        // Root-relative from anywhere.
        let (healed, n) = heal_content(
            "See [the note](/Soul/Note/old-name.md) for details.",
            "Soul/Journal/day-1.md",
            &renames,
        );
        assert_eq!(n, 1);
        assert!(healed.contains("(/Soul/Note/20260821-old-name.md)"), "{healed}");

        // Sibling-relative (no leading slash) from the same directory.
        let (healed, n) = heal_content(
            "See [the note](old-name.md).",
            "Soul/Note/other.md",
            &renames,
        );
        assert_eq!(n, 1);
        assert!(healed.contains("(/Soul/Note/20260821-old-name.md)"), "{healed}");

        // ../-relative from a sibling folder, fragment preserved.
        let (healed, n) = heal_content(
            "See [section](../Note/old-name.md#findings).",
            "Soul/Journal/day-2.md",
            &renames,
        );
        assert_eq!(n, 1);
        assert!(
            healed.contains("(/Soul/Note/20260821-old-name.md#findings)"),
            "{healed}"
        );

        // Untouched: unrelated link, external link, fenced code.
        let content = "A [live](/Soul/Note/other.md) link, an [ext](https://example.com/old-name.md) link.\n\n```\n[dead](/Soul/Note/old-name.md)\n```\n";
        let (healed, n) = heal_content(content, "README.md", &renames);
        assert_eq!(n, 0, "nothing outside real inline links may change");
        assert_eq!(healed, content);
    }

    /// Two renames in one save (the bulk-batch case): both heal in one
    /// pass, multiple links per file all rewritten.
    #[test]
    fn bulk_renames_heal_in_one_pass() {
        let renames = vec![
            ("a/one.md".to_string(), "a/20260821-one.md".to_string()),
            ("b/two.md".to_string(), "b/20260821-two.md".to_string()),
        ];
        let (healed, n) = heal_content(
            "[one](/a/one.md) and [two](/b/two.md) and [one again](/a/one.md).",
            "README.md",
            &renames,
        );
        assert_eq!(n, 3);
        assert!(healed.contains("(/a/20260821-one.md)"));
        assert!(healed.contains("(/b/20260821-two.md)"));
    }
}
