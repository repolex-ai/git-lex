//! Saved queries as documents (goodlux, 2026-11-16).
//!
//! A saved query is a document of a `Query` class (`git-lex:Query`, and each
//! kit's own subclass such as `soul:Query`), in that kit's `Query/` folder.
//! The query a person reads and edits is the FIRST FENCED CODE BLOCK in the
//! body. That block is the one source: `git lex query '<soul/Query/soul-kit-recent>'` runs it, and at
//! save git-lex derives the `sparql` property from it, so a service can fetch
//! a query by its address and run it.
//!
//! A stored query is named by its id, exactly as stored:
//! `<soul/Query/soul-kit-recent>` (goodlux, 2026-11-27). The earlier home,
//! `.lex/query/<name>.md`, has no id and is no longer read.

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
    // A query writes this store's addresses short (`<soul/Note/x>`,
    // `GRAPH <git-lex/NamedGraph/now>`), read against the store's root. The
    // stored line keeps them short, so it is the same text in every store
    // and is read the same way when it is run.
    let base = root.map_or_else(|| crate::PLATFORM_ROOT.to_string(), crate::store_root_at);
    let parser = || spargebra::SparqlParser::new().with_base_iri(&base).map_err(|e| format!("the store's address {base} is not usable: {e}"));
    let parsed = parser()?.parse_query(&with_prefixes).map_err(|e| e.to_string())?;
    let one_line = parsed.to_string().split_whitespace().collect::<Vec<_>>().join(" ");
    // The printer writes the base it was given as a BASE line. The stored
    // line carries none: whoever runs it supplies the store's root.
    let one_line = one_line.strip_prefix(&format!("BASE <{base}> ")).map(str::to_string).unwrap_or(one_line);
    let one_line = if base == crate::PLATFORM_ROOT { one_line } else { one_line.replace(&format!("<{base}"), "<") };
    // The printed form is what gets stored and later run, so it must itself
    // parse. If the printer ever writes something the parser refuses, store
    // nothing and say so.
    parser()?.parse_query(&one_line).map_err(|e| format!("its one-line form does not parse back ({e})"))?;
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

/// What an id resolved to.
#[derive(Debug, PartialEq)]
pub enum Found {
    /// The query text, the file it came from (relative to the repo root) and
    /// the parameters its frontmatter declares (`Err` when the declaration
    /// itself is wrong).
    Query { text: String, file: String, params: Result<Vec<crate::query_params::Param>, String> },
    /// The file is there and holds no fenced code block.
    NoBlock(String),
    Missing,
}

fn is_template(name: &str) -> bool {
    name.starts_with("__")
}

/// A stored query is named by its id, exactly as stored, angle brackets and
/// all: `<soul/Query/soul-kit-recent>` (goodlux, 2026-11-27). The store's full address
/// (`<https://repolex.ai/7r1pl3f0rc3/w4r3z/soul/Query/recent>`) is the same
/// id. Returns the kit's name and the document's name, or a sentence saying
/// how to write it.
pub fn parse_id(root: &Path, given: &str) -> Result<(String, String), String> {
    let example = "<soul/Query/soul-kit-recent>";
    let Some(inner) = given.trim().strip_prefix('<').and_then(|s| s.strip_suffix('>')) else {
        return Err(format!(
            "'{given}' is not a query id. A stored query is named by its id in angle brackets, in quotes on the command line: '{example}'."
        ));
    };
    let full_root = crate::store_root_at(root);
    let short = inner.strip_prefix(full_root.as_str()).unwrap_or(inner);
    match short.split('/').collect::<Vec<_>>().as_slice() {
        [kit, QUERY_FOLDER, name] if !kit.is_empty() && !name.is_empty() && !is_template(name) => Ok((kit.to_string(), name.to_string())),
        _ => Err(format!("'{given}' is not a query id. A query id reads <kit/Query/name>, for example '{example}'.")),
    }
}

/// The id of the query document `name` in kit `kit`.
pub fn id_of(kit: &str, name: &str) -> String {
    format!("<{kit}/{QUERY_FOLDER}/{name}>")
}

fn file_for(root: &Path, kit: &str, name: &str) -> Option<PathBuf> {
    kit_query_dirs(root)
        .into_iter()
        .find(|(k, _)| k == kit)
        .map(|(_, dir)| dir.join(format!("{name}.md")))
        .filter(|f| f.is_file())
}

/// Find a stored query by its id (see `parse_id`).
pub fn find(root: &Path, id: &str) -> Found {
    let Ok((kit, name)) = parse_id(root, id) else { return Found::Missing };
    let Some(file) = file_for(root, &kit, &name) else { return Found::Missing };
    let rel = file.strip_prefix(root).unwrap_or(&file).display().to_string();
    let Ok(md) = std::fs::read_to_string(&file) else { return Found::Missing };
    let (frontmatter, body) = crate::split_frontmatter(&md);
    match first_fenced_block(body) {
        Some(text) => Found::Query { text, file: rel, params: crate::query_params::declared(frontmatter) },
        None => Found::NoBlock(rel),
    }
}

/// Every stored query's id, sorted.
pub fn names(root: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (kit, dir) in kit_query_dirs(root) {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if let Some(stem) = e.file_name().to_string_lossy().strip_suffix(".md")
                && !is_template(stem)
            {
                out.push(id_of(&kit, stem));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The last segment of the frontmatter key that names the service a query
/// runs against (`soul.Query.service`): `git-lex`, `pan`, `ravel`, `horae`,
/// `punctum` or `syrinx` (goodlux, 2026-11-27).
pub const SERVICE_KEY: &str = "service";

/// The service git-lex itself is. The only one `git lex query` runs.
pub const GIT_LEX_SERVICE: &str = "git-lex";

/// One text value of a frontmatter key whose last dotted segment is `last`.
fn frontmatter_text(frontmatter: Option<&str>, last: &str) -> Option<String> {
    let map = crate::parse_frontmatter_map(frontmatter?).ok()?;
    map.iter().find_map(|(k, v)| {
        (k.as_str()?.rsplit('.').next() == Some(last)).then(|| v.as_str().map(|s| s.trim().to_string())).flatten()
    }).filter(|s| !s.is_empty())
}

/// A stored query, found and with its parameters put in.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Resolved {
    pub id: String,
    pub file: String,
    pub service: String,
    pub title: Option<String>,
    /// The parameters as the document declares them (`name[:kind][=default]`).
    pub parameters: Vec<String>,
    /// The query text with every parameter put in as a value.
    pub sparql: String,
}

/// One line of the list of stored queries.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Listed {
    pub id: String,
    pub service: Option<String>,
    pub title: Option<String>,
    pub parameters: Vec<String>,
}

fn declared_words(frontmatter: Option<&str>) -> Vec<String> {
    let Some(map) = frontmatter.and_then(|y| crate::parse_frontmatter_map(y).ok()) else { return Vec::new() };
    map.iter()
        .filter(|(k, _)| k.as_str().and_then(|k| k.rsplit('.').next()) == Some(crate::query_params::PARAMETER_KEY))
        .flat_map(|(_, v)| match v {
            serde_yaml::Value::String(s) => vec![s.clone()],
            serde_yaml::Value::Sequence(items) => items.iter().filter_map(|i| i.as_str().map(str::to_string)).collect(),
            _ => Vec::new(),
        })
        .collect()
}

/// Find a stored query by name and put in the caller's parameters. Every
/// failure is one plain sentence saying what to do. A query that names no
/// service is refused: the service is required (goodlux, 2026-11-27).
pub fn resolve(root: &Path, id: &str, given: &[(String, String)]) -> Result<Resolved, String> {
    let (kit, name) = parse_id(root, id)?;
    let id = id_of(&kit, &name);
    match find(root, &id) {
        Found::Query { text, file, params } => {
            let md = std::fs::read_to_string(root.join(&file)).unwrap_or_default();
            let (frontmatter, _) = crate::split_frontmatter(&md);
            let service = frontmatter_text(frontmatter, SERVICE_KEY).ok_or_else(|| {
                format!("{file} names no service. Add `{kit}.Query.service: \"git-lex\"` to its frontmatter (or the service it is for: pan, ravel, horae, punctum, syrinx).")
            })?;
            let declared = params.map_err(|e| format!("{file}: {e}"))?;
            let sparql = crate::query_params::bind(Some(root), &text, &declared, given)?;
            Ok(Resolved {
                id: id.clone(),
                file,
                service,
                title: frontmatter_text(frontmatter, "title"),
                parameters: declared_words(frontmatter),
                sparql,
            })
        }
        Found::NoBlock(file) => Err(format!("{file} holds no query. The query is the first fenced code block in the document's body; add one.")),
        Found::Missing => {
            let have = names(root);
            if have.is_empty() {
                Err(format!("No stored query {id}, and this soul has none yet. Make one with `git lex create query {name}` and put the query in a code block in its body."))
            } else {
                Err(format!("No stored query {id}. Stored queries: {}", have.join(", ")))
            }
        }
    }
}

/// Every stored query, with its service, title and parameters.
pub fn list(root: &Path) -> Vec<Listed> {
    names(root)
        .into_iter()
        .map(|id| {
            let md = parse_id(root, &id).ok().and_then(|(k, n)| file_for(root, &k, &n)).and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or_default();
            let (frontmatter, _) = crate::split_frontmatter(&md);
            Listed {
                service: frontmatter_text(frontmatter, SERVICE_KEY),
                title: frontmatter_text(frontmatter, "title"),
                parameters: declared_words(frontmatter),
                id,
            }
        })
        .collect()
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
    fn a_query_is_found_by_its_id_and_only_by_its_id() {
        let root = repo("find");
        std::fs::write(root.join("Soul/Query/recent.md"), doc("SELECT ?new WHERE { ?new ?p ?o }")).unwrap();
        std::fs::write(root.join("Copia/Query/recent.md"), doc("ASK { ?c ?p ?o }")).unwrap();
        std::fs::write(root.join("Soul/Query/__Query.md"), doc("ASK {}")).unwrap();

        let soul = Found::Query { text: "SELECT ?new WHERE { ?new ?p ?o }".into(), file: "Soul/Query/recent.md".into(), params: Ok(vec![]) };
        assert_eq!(find(&root, "<soul/Query/recent>"), soul);
        assert_eq!(find(&root, &format!("<{}soul/Query/recent>", crate::store_root_at(&root))), soul);
        assert_eq!(
            find(&root, "<copia/Query/recent>"),
            Found::Query { text: "ASK { ?c ?p ?o }".into(), file: "Copia/Query/recent.md".into(), params: Ok(vec![]) }
        );
        for wrong in ["recent", "soul/Query/recent", "<soul/recent>", "<soul/Query/__Query>", "<soul/Query/nope>"] {
            assert_eq!(find(&root, wrong), Found::Missing, "{wrong}");
        }
        assert!(parse_id(&root, "soul/Query/recent").unwrap_err().contains("'<soul/Query/soul-kit-recent>'"));
        assert_eq!(names(&root), vec!["<copia/Query/recent>", "<soul/Query/recent>"]);

        std::fs::write(root.join("Soul/Query/empty.md"), "---\ntype: Query\n---\n\njust words\n").unwrap();
        assert_eq!(find(&root, "<soul/Query/empty>"), Found::NoBlock("Soul/Query/empty.md".into()));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The service is required, is handed out with the query, and parameters
    /// are put in as values.
    #[test]
    fn a_stored_query_carries_its_service_and_has_to_have_one() {
        let root = repo("service");
        let with = |extra: &str, sparql: &str| format!("---\nsoul.Query.title: \"Recent\"\n{extra}---\n\n```sparql\n{sparql}\n```\n");
        std::fs::write(root.join("Soul/Query/mine.md"), with("soul.Query.service: \"git-lex\"\nsoul.Query.parameter: \"n:integer=3\"\n", "SELECT ?s WHERE { ?s ?p ?n }")).unwrap();
        std::fs::write(root.join("Soul/Query/pics.md"), with("soul.Query.service: \"pan\"\n", "ASK { ?s ?p ?o }")).unwrap();
        std::fs::write(root.join("Soul/Query/bare.md"), with("", "ASK { ?s ?p ?o }")).unwrap();

        let r = resolve(&root, "<soul/Query/mine>", &[("n".into(), "7".into())]).unwrap();
        assert_eq!((r.service.as_str(), r.title.as_deref()), ("git-lex", Some("Recent")));
        assert_eq!(r.parameters, vec!["n:integer=3".to_string()]);
        assert!(r.sparql.contains('7') && !r.sparql.contains("?n "), "{}", r.sparql);
        assert_eq!(resolve(&root, "<soul/Query/pics>", &[]).unwrap().service, "pan");
        let why = resolve(&root, "<soul/Query/bare>", &[]).unwrap_err();
        assert!(why.contains("names no service"), "{why}");
        assert!(resolve(&root, "<soul/Query/nope>", &[]).unwrap_err().contains("Stored queries: <soul/Query/bare>, <soul/Query/mine>, <soul/Query/pics>"));

        let listed: Vec<(String, Option<String>)> = list(&root).into_iter().map(|q| (q.id, q.service)).collect();
        assert_eq!(listed, vec![("<soul/Query/bare>".into(), None), ("<soul/Query/mine>".into(), Some("git-lex".into())), ("<soul/Query/pics>".into(), Some("pan".into()))]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
