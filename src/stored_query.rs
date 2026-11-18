//! Saved queries as documents (goodlux, 2026-11-16).
//!
//! A saved query is a document of a `Query` class (`git-lex:Query`, and each
//! kit's own subclass such as `soul:Query`), in that kit's `Query/` folder.
//! The query a person reads and edits is the FIRST FENCED CODE BLOCK in the
//! body. That block is the one source: `git lex query <name>` runs it, and at
//! save git-lex derives the `sparql` property from it, so a service can fetch
//! a query by its address and run it.
//!
//! `.lex/query/<name>.md` is the earlier home and is still read, after the
//! kits' folders.

use std::path::{Path, PathBuf};

/// The class folder name every kit uses for its saved queries.
pub const QUERY_FOLDER: &str = "Query";

/// The first fenced code block of a markdown body, or None when there is
/// none. The fence's label (```sparql or bare) is ignored. An unclosed fence
/// runs to the end.
pub fn first_fenced_block(body: &str) -> Option<String> {
    let mut in_fence = false;
    let mut lines: Vec<&str> = Vec::new();
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            if in_fence {
                return Some(lines.join("\n"));
            }
            in_fence = true;
            continue;
        }
        if in_fence {
            lines.push(line);
        }
    }
    in_fence.then(|| lines.join("\n"))
}

/// The query as ONE line of standard SPARQL with every prefix written out.
///
/// A stored fact is one physical line, and SPARQL cannot be put on one line
/// by joining its lines: a `#` comment would swallow everything after it. So
/// the text is parsed and printed back by the SPARQL parser. The result means
/// the same, carries no comments, and needs no prefix list to run, which is
/// what a caller fetching it from the graph wants. `Err` is the parser's
/// message: a query that does not parse has no `sparql` value.
pub fn canonical_sparql(root: Option<&Path>, text: &str) -> Result<String, String> {
    let with_prefixes = crate::add_prefixes_at(root, text);
    let parsed = spargebra::SparqlParser::new().parse_query(&with_prefixes).map_err(|e| e.to_string())?;
    let one_line = parsed.to_string().split_whitespace().collect::<Vec<_>>().join(" ");
    // The printed form is what gets stored and later run, so it must itself
    // parse. If the printer ever writes something the parser refuses, store
    // nothing and say so.
    spargebra::SparqlParser::new()
        .parse_query(&one_line)
        .map_err(|e| format!("its one-line form does not parse back ({e})"))?;
    Ok(one_line)
}

/// Every `Query/` folder of the installed kits, as (kit short name, folder),
/// base kit first. A kit with a `folder base:` keeps its classes under it
/// (`Soul/Query/`); one without keeps them at the repo root (`Query/`).
pub fn kit_query_dirs(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for spec in crate::installed_kit_specs(root) {
        let short = if spec == crate::BASE_KIT {
            crate::BASE_ONTOLOGY_FOLDER.to_string()
        } else {
            crate::resolve_kit_spec(&spec).2
        };
        let base = std::fs::read_to_string(crate::kit_install_dir_for_spec(root, &spec).join("kit.yml"))
            .ok()
            .and_then(|yml| {
                yml.lines()
                    .filter_map(|l| l.trim().strip_prefix("folder base:"))
                    .map(|v| v.trim().to_string())
                    .find(|v| !v.is_empty())
            });
        let dir = match base {
            Some(b) => root.join(b).join(QUERY_FOLDER),
            None => root.join(QUERY_FOLDER),
        };
        if dir.is_dir() && !out.iter().any(|(_, d)| *d == dir) {
            out.push((short, dir));
        }
    }
    out
}

/// What a name resolved to.
#[derive(Debug, PartialEq)]
pub enum Found {
    /// The query text, the file it came from (relative to the repo root) and
    /// the parameters its frontmatter declares (`Err` when the declaration
    /// itself is wrong).
    Query { text: String, file: String, params: Result<Vec<crate::query_params::Param>, String> },
    /// More than one kit has a query of this name: the `kit/name` forms.
    Ambiguous(Vec<String>),
    /// The file is there and holds no fenced code block.
    NoBlock(String),
    Missing,
}

fn is_template(name: &str) -> bool {
    name.starts_with("__")
}

/// Resolve a saved query's name. `name` is a bare id (`recent`) or `kit/id`
/// (`soul/recent`) to pick one kit. The kits' `Query/` folders come first;
/// `.lex/query/` is read only when no kit has the name.
pub fn find(root: &Path, name: &str) -> Found {
    let (want_kit, id) = match name.split_once('/') {
        Some((k, i)) => (Some(k), i),
        None => (None, name),
    };
    if id.is_empty() || is_template(id) {
        return Found::Missing;
    }
    let rel = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();
    let hits: Vec<(String, PathBuf)> = kit_query_dirs(root)
        .into_iter()
        .filter(|(kit, _)| want_kit.is_none_or(|w| w == kit))
        .map(|(kit, dir)| (kit, dir.join(format!("{id}.md"))))
        .filter(|(_, file)| file.is_file())
        .collect();
    match hits.as_slice() {
        [(_, file)] => {
            let Ok(md) = std::fs::read_to_string(file) else { return Found::Missing };
            let (frontmatter, body) = crate::split_frontmatter(&md);
            match first_fenced_block(body) {
                Some(text) => Found::Query { text, file: rel(file), params: crate::query_params::declared(frontmatter) },
                None => Found::NoBlock(rel(file)),
            }
        }
        [] => {
            if want_kit.is_some() {
                return Found::Missing;
            }
            let file = crate::layout::query_dir(root).join(format!("{id}.md"));
            let Ok(md) = std::fs::read_to_string(&file) else { return Found::Missing };
            // The earlier format: no fence means the whole body is the query.
            let (frontmatter, body) = crate::split_frontmatter(&md);
            let text = first_fenced_block(body).unwrap_or_else(|| body.trim().to_string());
            Found::Query { text, file: rel(&file), params: crate::query_params::declared(frontmatter) }
        }
        many => Found::Ambiguous(many.iter().map(|(kit, _)| format!("{kit}/{id}")).collect()),
    }
}

/// Every saved query's name, sorted, for the "no such query" message. A name
/// held by more than one kit is listed in its `kit/name` forms.
pub fn names(root: &Path) -> Vec<String> {
    let stems = |dir: &Path| -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|es| {
                es.filter_map(|e| e.ok())
                    .filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".md").map(str::to_string))
                    .filter(|n| !is_template(n))
                    .collect()
            })
            .unwrap_or_default()
    };
    let per_kit: Vec<(String, Vec<String>)> =
        kit_query_dirs(root).into_iter().map(|(kit, dir)| (kit, stems(&dir))).collect();
    let mut out: Vec<String> = Vec::new();
    for (kit, ids) in &per_kit {
        for id in ids {
            let shared = per_kit.iter().filter(|(_, other)| other.contains(id)).count() > 1;
            out.push(if shared { format!("{kit}/{id}") } else { id.clone() });
        }
    }
    for id in stems(&crate::layout::query_dir(root)) {
        if !out.contains(&id) && !per_kit.iter().any(|(_, ids)| ids.contains(&id)) {
            out.push(id);
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("glx-storedq-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".lex")).unwrap();
        std::fs::write(
            root.join(".lex/repo.yml"),
            "kit: repolex-ai/git-lex-kit-soul\noptional_kits:\n  - repolex-ai/git-lex-kit-copia\n",
        )
        .unwrap();
        for (kit, base) in [("soul", "Soul"), ("copia", "Copia")] {
            let dir = crate::kit_install_dir_for_spec(&root, &format!("repolex-ai/git-lex-kit-{kit}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("kit.yml"), format!("name: {kit}\nfolder base: {base}\n")).unwrap();
            std::fs::create_dir_all(root.join(base).join("Query")).unwrap();
        }
        root
    }

    fn doc(sparql: &str) -> String {
        format!("---\ntype: Query\nsoul.Query.id: <soul/Query/x>\n---\n\n# A query\n\nProse.\n\n```sparql\n{sparql}\n```\n\n```\nnot this one\n```\n")
    }

    #[test]
    fn the_first_fenced_block_is_the_query() {
        assert_eq!(first_fenced_block("a\n```sparql\nASK { ?s ?p ?o }\n```\n```\nx\n```\n").as_deref(), Some("ASK { ?s ?p ?o }"));
        assert_eq!(first_fenced_block("no fence here"), None);
        assert_eq!(first_fenced_block("```\nSELECT *\nWHERE {}").as_deref(), Some("SELECT *\nWHERE {}"));
    }

    /// A comment must not swallow the rest of the query when it goes on one line.
    #[test]
    fn canonical_text_is_one_line_runs_alone_and_survives_comments() {
        let text = "# newest first\nSELECT ?d ?date  # two columns\nWHERE { ?d git-lex:updatedDate ?date }\nORDER BY DESC(?date)\nLIMIT 5";
        let one = canonical_sparql(None, text).unwrap();
        assert!(!one.contains('\n') && !one.contains("newest") && !one.contains("two columns"), "{one}");
        assert!(one.contains("<https://repolex.ai/ontology/git-lex/updatedDate>"), "{one}");
        assert!(one.contains("LIMIT 5"), "{one}");
        // It parses again with no prefix list at all.
        spargebra::SparqlParser::new().parse_query(&one).unwrap();
        assert!(canonical_sparql(None, "SELECT WHERE oops").is_err());
    }

    #[test]
    fn a_name_is_found_in_the_kits_folders_first_then_the_old_folder() {
        let root = repo("find");
        std::fs::create_dir_all(root.join(".lex/query")).unwrap();
        std::fs::write(root.join(".lex/query/recent.md"), "old prose\n\nSELECT ?old WHERE { ?old ?p ?o }\n").unwrap();
        std::fs::write(root.join(".lex/query/legacy.md"), "ASK { ?s ?p ?o }\n").unwrap();
        std::fs::write(root.join("Soul/Query/recent.md"), doc("SELECT ?new WHERE { ?new ?p ?o }")).unwrap();
        std::fs::write(root.join("Soul/Query/__Query.md"), doc("ASK {}")).unwrap();

        assert_eq!(
            find(&root, "recent"),
            Found::Query { text: "SELECT ?new WHERE { ?new ?p ?o }".into(), file: "Soul/Query/recent.md".into(), params: Ok(vec![]) }
        );
        assert_eq!(find(&root, "legacy"), Found::Query { text: "ASK { ?s ?p ?o }".into(), file: ".lex/query/legacy.md".into(), params: Ok(vec![]) });
        assert_eq!(find(&root, "__Query"), Found::Missing);
        assert_eq!(find(&root, "nope"), Found::Missing);
        assert_eq!(names(&root), vec!["legacy", "recent"]);

        // The same name in two kits: say so, and let `kit/name` pick one.
        std::fs::write(root.join("Copia/Query/recent.md"), doc("ASK { ?c ?p ?o }")).unwrap();
        assert_eq!(find(&root, "recent"), Found::Ambiguous(vec!["soul/recent".into(), "copia/recent".into()]));
        assert_eq!(
            find(&root, "copia/recent"),
            Found::Query { text: "ASK { ?c ?p ?o }".into(), file: "Copia/Query/recent.md".into(), params: Ok(vec![]) }
        );
        assert_eq!(names(&root), vec!["copia/recent", "legacy", "soul/recent"]);

        // A Query document with no code block is reported, not run as prose.
        std::fs::write(root.join("Soul/Query/empty.md"), "---\ntype: Query\n---\n\njust words\n").unwrap();
        assert_eq!(find(&root, "empty"), Found::NoBlock("Soul/Query/empty.md".into()));
        let _ = std::fs::remove_dir_all(&root);
    }
}
