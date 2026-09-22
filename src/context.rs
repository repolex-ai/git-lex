//! The agent startup context (#8, #14): the git-lex manual compiled into the
//! binary, followed by a compact description of every class and property the
//! INSTALLED kits declare. One generated file, so a waking agent never has to
//! discover the graph's shape by probing it.
//!
//! Which kits are installed is answered by `.lex/repo.yml` alone (base kit,
//! `kit:`, `optional_kits:`). A leftover folder under `.lex/ontology/` that no
//! installed kit owns is never read.
//!
//! Deterministic: identical inputs give identical bytes, and the file is only
//! rewritten when the bytes change.

use oxigraph::model::Term;
use oxigraph::sparql::QueryResults;
use oxigraph::store::Store;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Where the generated context lives, relative to the repo root. The name and
/// location are provisional (goodlux, 2026-09-16; issue #19), so they are
/// spelled here and nowhere else.
pub const CONTEXT_FILE: &str = ".lex/COMPACT-ONTOLOGY.md";

/// TRANSITIONAL: the file's first name, for one day. `refresh` removes it so
/// no repo carries both. Delete once the fleet has regenerated.
const OLD_CONTEXT_FILE: &str = ".lex/CONTEXT.md";

const MANUAL: &str = include_str!("agent_manual.md");

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
const THING: &str = "https://repolex.ai/ontology/git-lex/Thing";

// ─── model ───────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq)]
struct Prop {
    iri: String,
    /// `xsd:date`, a range class, `IRI`, or empty for a plain string.
    kind: String,
    required: bool,
    single: bool,
    /// `sh:in` members, sorted.
    values: Vec<String>,
    comment: String,
}

#[derive(Clone, Debug, Default)]
struct Class {
    iri: String,
    comment: String,
    /// The nearest parent class that is itself described here.
    parent: Option<String>,
    /// Repo-relative folder, when the class is authored as files.
    folder: Option<String>,
    /// Own properties only: the parent's and the universal ones are removed.
    props: Vec<Prop>,
}

#[derive(Clone, Debug, Default)]
struct Model {
    /// (prefix, namespace), longest namespace first.
    prefixes: Vec<(String, String)>,
    /// Properties every Thing carries, listed once.
    universals: Vec<Prop>,
    classes: Vec<Class>,
    /// The repo's own kit (`kit:` in repo.yml), by prefix; the guide's
    /// examples use one of ITS classes.
    main_kit: Option<String>,
}

// ─── installed kits ──────────────────────────────────────────

/// (kit spec, ontology folder name) for every installed kit. Which kits are
/// installed is `crate::installed_kit_specs`' answer (repo.yml), like every
/// other reader (#17).
fn installed_ontologies(root: &Path) -> Vec<(String, String)> {
    crate::installed_kit_specs(root)
        .into_iter()
        .map(|spec| {
            let name = if spec == crate::BASE_KIT {
                crate::BASE_ONTOLOGY_FOLDER.to_string()
            } else {
                crate::resolve_kit_spec(&spec).2
            };
            (spec, name)
        })
        .collect()
}

/// `folder base:` from an installed kit's kit.yml.
fn folder_base(root: &Path, spec: &str) -> Option<String> {
    let yml = fs::read_to_string(crate::kit_install_dir_for_spec(root, spec).join("kit.yml")).ok()?;
    yml.lines()
        .filter_map(|l| l.trim().strip_prefix("folder base:"))
        .map(|v| v.trim().to_string())
        .find(|v| !v.is_empty())
}

// ─── building the model ──────────────────────────────────────

fn rows(store: &Store, q: &str) -> Vec<BTreeMap<String, Term>> {
    let Ok(QueryResults::Solutions(sols)) = crate::eval_query(store, q) else { return Vec::new() };
    sols.flatten()
        .map(|s| s.iter().map(|(v, t)| (v.as_str().to_string(), t.clone())).collect())
        .collect()
}

fn iri(row: &BTreeMap<String, Term>, var: &str) -> Option<String> {
    match row.get(var) {
        Some(Term::NamedNode(n)) => Some(n.as_str().to_string()),
        _ => None,
    }
}

fn lit(row: &BTreeMap<String, Term>, var: &str) -> Option<String> {
    match row.get(var) {
        Some(Term::Literal(l)) => Some(l.value().to_string()),
        _ => None,
    }
}

const PREFIXES: &str = "PREFIX sh: <http://www.w3.org/ns/shacl#>
PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
PREFIX owl: <http://www.w3.org/2002/07/owl#>
PREFIX gl: <https://repolex.ai/ontology/git-lex/>
";

fn build_model(root: &Path) -> Model {
    let installed = installed_ontologies(root);

    // One in-memory graph: the shapes and vocabularies of the installed kits,
    // nothing else.
    let Ok(store) = Store::new() else { return Model::default() };
    let files: Vec<PathBuf> = crate::installed_ontology_files(root)
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "ttl"))
        .collect();
    for f in &files {
        let Ok(bytes) = fs::read(f) else { continue };
        if let Err(e) = store.load_from_reader(oxigraph::io::RdfFormat::Turtle, bytes.as_slice()) {
            eprintln!("warning: {} not read into the agent context: {e}", f.display());
        }
    }

    // Properties, from the shapes validation actually enforces.
    let mut by_class: BTreeMap<String, BTreeMap<String, Prop>> = BTreeMap::new();
    let q = format!(
        "{PREFIXES} SELECT ?class ?path ?nodeKind ?datatype ?minCount ?comment ?range ?value WHERE {{
            ?shape sh:targetClass ?class .
            OPTIONAL {{
                ?shape sh:property ?prop . ?prop sh:path ?pathNode .
                OPTIONAL {{ ?pathNode sh:alternativePath/rdf:rest*/rdf:first ?alt }}
                BIND(COALESCE(?alt, ?pathNode) AS ?path)
                FILTER(isIRI(?path))
                OPTIONAL {{ ?prop sh:nodeKind ?nodeKind }}
                OPTIONAL {{ ?prop sh:datatype ?datatype }}
                OPTIONAL {{ ?prop sh:minCount ?minCount }}
                OPTIONAL {{ ?prop rdfs:comment ?comment }}
                OPTIONAL {{ ?prop sh:in/rdf:rest*/rdf:first ?value }}
                OPTIONAL {{ ?path rdfs:range ?range . FILTER(isIRI(?range)) }}
            }}
        }}"
    );
    for r in rows(&store, &q) {
        let Some(class) = iri(&r, "class") else { continue };
        let props = by_class.entry(class).or_default();
        let Some(path) = iri(&r, "path") else { continue };
        let p = props.entry(path.clone()).or_insert_with(|| Prop { iri: path, ..Prop::default() });
        if let Some(dt) = iri(&r, "datatype") {
            if p.kind.is_empty() || p.kind == "IRI" || dt < p.kind { p.kind = dt; }
        } else if iri(&r, "nodeKind").is_some_and(|k| k.ends_with("#IRI")) {
            // A reference: name the declared range class when there is one
            // (the first in IRI order when several), else just say IRI.
            match iri(&r, "range") {
                Some(range) if p.kind.is_empty() || p.kind == "IRI" || range < p.kind => p.kind = range,
                None if p.kind.is_empty() => p.kind = "IRI".to_string(),
                _ => {}
            }
        }
        if lit(&r, "minCount").and_then(|n| n.parse::<u32>().ok()).is_some_and(|n| n >= 1) {
            p.required = true;
        }
        if let Some(c) = lit(&r, "comment")
            && (p.comment.is_empty() || c < p.comment) { p.comment = c; }
        if let Some(v) = lit(&r, "value")
            && !p.values.contains(&v) { p.values.push(v); p.values.sort(); }
    }

    // At most one value: a cardinality restriction on the class or any ancestor.
    let q = format!(
        "{PREFIXES} SELECT ?class ?path WHERE {{
            ?class rdfs:subClassOf* ?anc . ?anc rdfs:subClassOf ?r .
            ?r owl:onProperty ?path .
            {{ ?r owl:maxCardinality ?n }} UNION {{ ?r owl:cardinality ?n }}
            FILTER(STR(?n) = \"1\")
        }}"
    );
    for r in rows(&store, &q) {
        if let (Some(c), Some(p)) = (iri(&r, "class"), iri(&r, "path"))
            && let Some(prop) = by_class.get_mut(&c).and_then(|m| m.get_mut(&p)) { prop.single = true; }
    }

    // Universal properties: declared on git-lex:Thing itself.
    let q = format!("{PREFIXES} SELECT ?path WHERE {{ ?path rdfs:domain gl:Thing }}");
    let universal_iris: BTreeSet<String> = rows(&store, &q).iter().filter_map(|r| iri(r, "path")).collect();
    let mut universals: BTreeMap<String, Prop> = BTreeMap::new();
    for props in by_class.values() {
        for (k, p) in props {
            if universal_iris.contains(k) { universals.entry(k.clone()).or_insert_with(|| p.clone()); }
        }
    }

    // Class facts.
    let mut comments: BTreeMap<String, String> = BTreeMap::new();
    let mut foldered: BTreeSet<String> = BTreeSet::new();
    let q = format!(
        "{PREFIXES} SELECT ?class ?comment ?foldered WHERE {{
            ?class a owl:Class .
            OPTIONAL {{ ?class rdfs:comment ?comment }}
            OPTIONAL {{ ?class gl:foldered ?foldered }}
        }}"
    );
    for r in rows(&store, &q) {
        let Some(c) = iri(&r, "class") else { continue };
        if let Some(t) = lit(&r, "comment") {
            let have = comments.entry(c.clone()).or_default();
            if have.is_empty() || t < *have { *have = t; }
        }
        if lit(&r, "foldered").as_deref() == Some("true") { foldered.insert(c); }
    }
    let mut parents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let q = format!("{PREFIXES} SELECT ?class ?parent WHERE {{ ?class rdfs:subClassOf ?parent . FILTER(isIRI(?parent)) }}");
    for r in rows(&store, &q) {
        if let (Some(c), Some(p)) = (iri(&r, "class"), iri(&r, "parent"))
            && c != p { parents.entry(c).or_default().insert(p); }
    }

    let prefixes = {
        // The shared binding list spells names with their colon.
        let mut b: Vec<(String, String)> = crate::prefix_bindings_at(Some(root))
            .into_iter()
            .map(|(n, ns)| (n.trim_end_matches(':').to_string(), ns))
            .collect();
        b.sort_by(|a, z| z.1.len().cmp(&a.1.len()).then(a.0.cmp(&z.0)));
        b
    };
    // Namespace -> (kit spec) so a class finds its kit's folder base.
    let base_of = |class_iri: &str| -> Option<String> {
        let (prefix, _) = prefixes.iter().find(|(_, ns)| class_iri.starts_with(ns.as_str()))?;
        let (spec, _) = installed.iter().find(|(_, name)| name == prefix)?;
        folder_base(root, spec)
    };

    let mut classes = Vec::new();
    for (class_iri, props) in &by_class {
        // Nearest described ancestor, breadth-first, first in IRI order.
        let mut parent = None;
        let mut frontier: Vec<String> = parents.get(class_iri).map(|s| s.iter().cloned().collect()).unwrap_or_default();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        while parent.is_none() && !frontier.is_empty() {
            parent = frontier.iter().find(|p| by_class.contains_key(*p)).cloned();
            let next: Vec<String> = frontier
                .iter()
                .filter(|p| seen.insert((*p).clone()))
                .flat_map(|p| parents.get(p).into_iter().flatten().cloned())
                .collect();
            frontier = next;
        }
        let inherited = parent.as_ref().and_then(|p| by_class.get(p));
        let own: Vec<Prop> = props
            .values()
            .filter(|p| !universal_iris.contains(&p.iri))
            .filter(|p| inherited.and_then(|m| m.get(&p.iri)).is_none_or(|theirs| theirs != *p))
            .cloned()
            .collect();
        let local = class_iri.rsplit(['/', '#']).next().unwrap_or(class_iri);
        let folder = foldered.contains(class_iri).then(|| match base_of(class_iri) {
            Some(b) => format!("{b}/{local}/"),
            None => format!("{local}/"),
        });
        classes.push(Class {
            iri: class_iri.clone(),
            comment: comments.get(class_iri).cloned().unwrap_or_default(),
            parent: parent.or_else(|| (class_iri != THING && !universals.is_empty()
                && props.keys().any(|k| universal_iris.contains(k))).then(|| THING.to_string())),
            folder,
            props: own,
        });
    }

    let main_kit = installed.iter().map(|(spec, name)| (spec.as_str(), name)).find(|(spec, _)| *spec != crate::BASE_KIT).map(|(_, n)| n.clone());
    Model { prefixes, universals: universals.into_values().collect(), classes, main_kit }
}

// ─── formatting ──────────────────────────────────────────────

fn shorten(prefixes: &[(String, String)], iri: &str) -> String {
    if let Some(local) = iri.strip_prefix(XSD) {
        return format!("xsd:{local}");
    }
    for (name, ns) in prefixes {
        if let Some(local) = iri.strip_prefix(ns.as_str())
            && !local.is_empty() && !local.contains(['/', '#']) { return format!("{name}:{local}"); }
    }
    format!("<{iri}>")
}

/// First sentence, on one line. "e.g." and "i.e." do not end a sentence. A
/// sentence over the cap is cut at a word boundary, never inside a word.
fn brief(text: &str, cap: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut end = flat.len();
    for (i, _) in flat.match_indices(". ") {
        let before = &flat[..i];
        if before.ends_with("e.g") || before.ends_with("i.e") { continue; }
        end = i;
        break;
    }
    let first = flat[..end].trim().trim_end_matches('.');
    if first.chars().count() <= cap {
        return first.to_string();
    }
    let cut: String = first.chars().take(cap).collect();
    let cut = cut.rsplit_once(' ').map(|(head, _)| head).unwrap_or(&cut);
    format!("{} …", cut.trim_end_matches([',', ';', ':', ' ']))
}

fn prop_line(m: &Model, p: &Prop) -> String {
    let mut line = format!("  {}", shorten(&m.prefixes, &p.iri));
    match p.kind.as_str() {
        "" => {}
        "IRI" => line.push_str(" IRI"),
        k => { line.push(' '); line.push_str(&shorten(&m.prefixes, k)); }
    }
    if !p.values.is_empty() {
        let quoted: Vec<String> = p.values.iter().map(|v| format!("\"{v}\"")).collect();
        line.push_str(&format!(" in=[{}]", quoted.join(" ")));
    }
    line.push_str(match (p.required, p.single) {
        (true, true) => " [1..1]",
        (true, false) => " [1..*]",
        (false, true) => " [0..1]",
        (false, false) => "",
    });
    line.push_str(" .");
    let c = brief(&p.comment, 120);
    if !c.is_empty() { line.push_str(&format!(" # {c}")); }
    line
}

/// Does `text` write a name under this prefix (`md:linksTo`)? A longer prefix
/// ending the same way (`git-lex:` for `lex:`), a filename (`x.md:`) and a URL
/// scheme do not count.
fn uses_prefix(text: &str, name: &str) -> bool {
    let needle = format!("{name}:");
    text.match_indices(&needle).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + needle.len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && after.is_some_and(|c| c.is_alphanumeric())
    })
}

/// THE formatter: the model as SHACL Compact Syntax, comments carrying the
/// prose SHACLC has no slot for. Swap this one function to change the format.
/// `around` is the rest of the generated file: the prefix list names what the
/// file as a whole writes, so an agent can expand every short name it meets.
fn format_ontology(m: &Model, around: &str) -> String {
    let mut out = String::new();
    if !m.universals.is_empty() {
        out.push_str("\n# Every Thing carries these. They are not repeated on the classes below.\n");
        out.push_str(&format!("shapeClass {} {{\n", shorten(&m.prefixes, THING)));
        for p in &m.universals { out.push_str(&prop_line(m, p)); out.push('\n'); }
        out.push_str("}\n");
    }
    for c in &m.classes {
        out.push('\n');
        let mut notes: Vec<String> = Vec::new();
        if let Some(p) = &c.parent { notes.push(format!("extends {}", shorten(&m.prefixes, p))); }
        if let Some(f) = &c.folder { notes.push(format!("files in {f}")); }
        let about = brief(&c.comment, 110);
        if !about.is_empty() { notes.push(about); }
        if !notes.is_empty() { out.push_str(&format!("# {}\n", notes.join("; "))); }
        if c.props.is_empty() {
            out.push_str(&format!("shapeClass {} {{}}\n", shorten(&m.prefixes, &c.iri)));
        } else {
            out.push_str(&format!("shapeClass {} {{\n", shorten(&m.prefixes, &c.iri)));
            for p in &c.props { out.push_str(&prop_line(m, p)); out.push('\n'); }
            out.push_str("}\n");
        }
    }
    // `git lex query` binds more prefixes than these (the same table); the
    // ones nothing here writes are left out, by name order.
    let mut bound: Vec<&(String, String)> = m.prefixes.iter()
        .filter(|(name, _)| uses_prefix(&out, name) || uses_prefix(around, name))
        .collect();
    bound.sort();
    let prefixes: String = bound.iter().map(|(name, ns)| format!("PREFIX {name}: <{ns}>\n")).collect();
    prefixes + &out
}

const ONTOLOGY_INTRO: &str = "## The ontology of this repo

Every class and property of the installed kits, in SHACL Compact Syntax.
How to read it:

- `shapeClass soul:Journal { ... }` is one document class. `# extends X` means
  it also takes every property of X. `# files in F/` is where its documents live.
- A property line is `property type [min..max] .` No type means a plain
  string. A class name as the type means a reference to that kind of Thing.
  No brackets means optional and repeatable. `in=[...]` lists the only
  allowed values.
- Header key for `git-lex:title` on `soul:Journal`: `soul.Journal.title`. The
  key always uses the CLASS's prefix and name, then the property's local name.
- In a query, use the property exactly as written here.
";

/// What an agent needs to write a first query without guessing: what else is
/// in the graph, what addresses look like, and examples that run. The example
/// class is the first foldered class of the repo's own kit, so a repo never
/// gets an example naming a kit it does not have.
fn query_guide(m: &Model) -> String {
    let example = m.main_kit.as_ref().and_then(|kit| {
        let ns = &m.prefixes.iter().find(|(name, _)| name == kit)?.1;
        m.classes.iter().find(|c| c.folder.is_some() && c.iri.starts_with(ns.as_str()))
    }).or_else(|| m.classes.iter().find(|c| c.folder.is_some()));

    let mut out = String::from("## Querying: what is in the graph\n\n");
    out.push_str(
"- A document carries ONLY its own class. Nothing is inferred: `?d a git-lex:Thing`
  matches nothing, and neither does a parent class. `?d git-lex:id ?id` matches
  every document of every class.
- Optional properties are often absent on real documents (many have no title).
  Put them in `OPTIONAL { }`, or the documents without them silently drop out.
- Named graphs, all under `https://repolex.ai/git-lex/NamedGraph/`: `now` holds
  your documents and one `git-lex:File` per file; `commits`, `refs`, `repo` and
  `filetree/<sha>` hold git itself (`git2:Commit`, `git2:Signature`, `git2:Blob`,
  `git2:IndexEntry`, `git2:Branch`, `git2:Repository`, `git-lex:Repo`). A query
  sees all of them at once, and the git layer is most of the graph, so an open
  pattern like `?s a ?class` is mostly git. Require `git-lex:id` to keep to
  documents.
- Body links run FILE to FILE: `?fromFile md:linksTo ?toFile`. A document
  points at its file with `git-lex:fileId`; hop through it to get documents.
- Not every file is a document. A markdown file whose header has no `id` (or
  that has no header) is a `git-lex:File` and nothing more, so a `?doc` joined
  through `git-lex:fileId` comes back blank for it.
- `__<Class>.md` in a class folder is the kit's template for that class, not a
  document: a File with no `git-lex:id`.
");
    let Some(c) = example else { return out };
    let class = shorten(&m.prefixes, &c.iri);
    let local = c.iri.rsplit(['/', '#']).next().unwrap_or_default();
    let kit = class.split(':').next().unwrap_or_default();
    let doc = crate::nquad::thing_iri_from_range(&c.iri, "my-id").unwrap_or_default();
    let doc = doc.trim_matches(['<', '>']);
    let file = format!("{}{}my-id.md", crate::git::FILE_BASE, c.folder.as_deref().unwrap_or_default());
    out.push_str(&format!(
"
Addresses, for the class `{class}`:

- the class: `{iri}`
- a document: `{doc}` (the class address without `ontology/`, then the id).
  In a header it is written `<{kit}/{local}/my-id>`.
- its file: `{file}` (the path from the repo root; a root file is
  `{file_base}README.md`)
- A document's address comes from the `id` in its header, never from where its
  file sits. A document at the repo root is addressed like any other.
",
        iri = c.iri, file_base = crate::git::FILE_BASE));
    let own_id = format!("{}{}Id", local[..1].to_lowercase(), &local[1..]);
    if c.props.iter().any(|p| p.iri.rsplit(['/', '#']).next() == Some(own_id.as_str())) {
        out.push_str(&format!(
"- `{kit}:{own_id}` holds the bare id as plain text (`\"my-id\"`); `git-lex:id` holds
  the document's own address, as a reference.
"));
    }
    out.push_str(&format!(
"
```sparql
# newest documents of one class; the title may be missing
SELECT ?d ?title ?updated WHERE {{ ?d a {class} . OPTIONAL {{ ?d git-lex:title ?title }} OPTIONAL {{ ?d git-lex:updatedDate ?updated }} }} ORDER BY DESC(?updated) LIMIT 5
# everything that relates to one document, with its class
SELECT ?d ?class WHERE {{ ?d git-lex:relatedToId <{doc}> ; a ?class }}
# which documents link to a file
SELECT ?doc ?fromFile WHERE {{ ?fromFile md:linksTo <{file}> . OPTIONAL {{ ?doc git-lex:fileId ?fromFile }} }}
# how many documents of each class
SELECT ?class (COUNT(?d) AS ?n) WHERE {{ ?d git-lex:id ?id ; a ?class }} GROUP BY ?class ORDER BY DESC(?n)
```
"));
    out
}

/// The whole context for a repo: manual, then ontology.
pub fn render(root: &Path) -> String {
    let model = build_model(root);
    let kits: Vec<String> = installed_ontologies(root).into_iter().map(|(_, n)| n).collect();
    let head = format!("{}\n\n{}\n{}", MANUAL.trim_end(), query_guide(&model), ONTOLOGY_INTRO);
    format!(
        "{head}\nInstalled: {}.\n\n```shaclc\n{}```\n",
        kits.join(", "),
        format_ontology(&model, &head)
    )
}

/// Regenerate the context file. Writes only when the bytes changed; a failure
/// is a warning, never a reason for the calling command to fail.
pub fn refresh(root: &Path) {
    if !crate::layout::lex_dir(root).is_dir() { return; }
    let path = root.join(CONTEXT_FILE);
    let text = render(root);
    // TRANSITIONAL: drop the generated file under its old name. The next save
    // stages the deletion.
    let old = root.join(OLD_CONTEXT_FILE);
    if fs::read_to_string(&old).is_ok_and(|have| have.starts_with(MANUAL.lines().next().unwrap_or_default())) {
        let _ = fs::remove_file(&old);
    }
    if fs::read_to_string(&path).is_ok_and(|have| have == text) { return; }
    match fs::write(&path, &text) {
        Ok(()) => println!("Agent context: {CONTEXT_FILE} updated ({} classes).", text.matches("shapeClass ").count()),
        Err(e) => eprintln!("warning: {CONTEXT_FILE} not written: {e}"),
    }
}

/// `git lex --skill`: print the context for the current repo, or the manual
/// alone outside one.
pub fn print_skill() {
    match crate::find_git_root().filter(|r| crate::layout::lex_dir(r).is_dir()) {
        Some(root) => print!("{}", render(&root)),
        None => print!("{MANUAL}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE_TTL: &str = r#"@prefix git-lex: <https://repolex.ai/ontology/git-lex/> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
git-lex:Thing a owl:Class ; rdfs:comment "An entity with its own identity." ;
    rdfs:subClassOf [ a owl:Restriction ; owl:onProperty git-lex:title ; owl:maxCardinality 1 ] .
git-lex:title a owl:DatatypeProperty ; rdfs:domain git-lex:Thing ; rdfs:range xsd:string .
git-lex:foldered a owl:AnnotationProperty .
"#;

    fn kit_ttl(short: &str) -> String {
        format!(r#"@prefix {short}: <https://repolex.ai/ontology/{short}/> .
@prefix git-lex: <https://repolex.ai/ontology/git-lex/> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
{short}:Entry a owl:Class ; rdfs:subClassOf git-lex:Thing ; git-lex:foldered true ;
    rdfs:comment "One dated entry. More words here." .
{short}:Special a owl:Class ; rdfs:subClassOf {short}:Entry .
{short}:day a owl:DatatypeProperty ; rdfs:domain {short}:Entry ; rdfs:range xsd:integer .
{short}:about a owl:ObjectProperty ; rdfs:domain {short}:Entry ; rdfs:range {short}:Entry .
{short}:extra a owl:DatatypeProperty ; rdfs:domain {short}:Special .
"#)
    }

    fn kit_shapes(short: &str) -> String {
        let entry_props = format!(r#"    sh:property [ sh:path {short}:about ; sh:nodeKind sh:IRI ; rdfs:comment "What it is about." ] ;
    sh:property [ sh:path {short}:day ; sh:datatype xsd:integer ; sh:minCount 1 ; rdfs:comment "The day number. Starts at 1." ] ;
    sh:property [ sh:path git-lex:title ; rdfs:comment "One short name." ] "#);
        format!(r#"@prefix {short}: <https://repolex.ai/ontology/{short}/> .
@prefix git-lex: <https://repolex.ai/ontology/git-lex/> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
{short}:EntryShape a sh:NodeShape ; sh:targetClass {short}:Entry ;
{entry_props}.
{short}:SpecialShape a sh:NodeShape ; sh:targetClass {short}:Special ;
{entry_props};
    sh:property [ sh:path {short}:extra ; sh:in ( "a" "b" ) ] .
"#)
    }

    /// A repo root with `installed` kits in repo.yml and on disk, plus
    /// `leftover` ontology folders that no installed kit owns.
    fn fake_root(tag: &str, installed: &[&str], leftover: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("gl-context-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let lex = crate::layout::lex_dir(&root);
        let put = |dir: PathBuf, name: &str, body: &str| {
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(name), body).unwrap();
        };
        let mut yml = String::from("name: t\n");
        for (i, k) in installed.iter().enumerate() {
            if i == 0 { yml.push_str(&format!("kit: repolex-ai/git-lex-kit-{k}\noptional_kits:\n")); }
            else { yml.push_str(&format!("  - repolex-ai/git-lex-kit-{k}\n")); }
            let kit = lex.join("kit").join("repolex-ai").join(format!("git-lex-kit-{k}"));
            fs::create_dir_all(kit.join("ontology").join(k)).unwrap();
            fs::write(kit.join("kit.yml"), format!("name: {k}\nfolder base: Base{k}\n")).unwrap();
        }
        put(lex.clone(), "repo.yml", &yml);
        fs::create_dir_all(lex.join("kit/repolex-ai/git-lex-kit-base/ontology/git-lex")).unwrap();
        put(lex.join("ontology/git-lex"), "git-lex.ttl", BASE_TTL);
        for k in installed.iter().chain(leftover) {
            put(lex.join("ontology").join(k), &format!("{k}.ttl"), &kit_ttl(k));
            put(lex.join("ontology").join(k), &format!("{k}-shapes.ttl"), &kit_shapes(k));
        }
        root
    }

    #[test]
    fn two_runs_give_identical_bytes() {
        let root = fake_root("same", &["alpha", "beta"], &[]);
        assert_eq!(render(&root), render(&root));
        refresh(&root);
        let first = fs::metadata(root.join(CONTEXT_FILE)).unwrap().modified().unwrap();
        refresh(&root);
        assert_eq!(first, fs::metadata(root.join(CONTEXT_FILE)).unwrap().modified().unwrap(), "unchanged bytes must not be rewritten");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn every_installed_kit_appears_and_a_leftover_folder_does_not() {
        let root = fake_root("owned", &["alpha", "beta"], &["ghost"]);
        let text = render(&root);
        assert!(text.contains("shapeClass alpha:Entry"), "{text}");
        assert!(text.contains("shapeClass beta:Entry"), "{text}");
        assert!(!text.contains("ghost"), "a kit absent from repo.yml reached the context:\n{text}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_class_lists_only_what_its_parent_and_thing_do_not() {
        let root = fake_root("own", &["alpha"], &[]);
        let full = render(&root);
        let text = full.split("```shaclc").nth(1).unwrap();
        // Universal: once, on Thing, single-valued from the base restriction.
        assert_eq!(text.matches("git-lex:title").count(), 1, "{text}");
        assert!(text.contains("  git-lex:title [0..1] . # One short name"), "{text}");
        // Own properties: type, range class, cardinality, first sentence.
        assert!(text.contains("  alpha:day xsd:integer [1..*] . # The day number"), "{text}");
        assert!(text.contains("  alpha:about alpha:Entry . # What it is about"), "{text}");
        assert!(text.contains("# extends git-lex:Thing; files in Basealpha/Entry/; One dated entry"), "{text}");
        // The subclass repeats nothing it inherits.
        let special = text.split("shapeClass alpha:Special").nth(1).unwrap();
        assert!(special.contains("alpha:extra in=[\"a\" \"b\"]") && !special.contains("alpha:day"), "{text}");
        assert!(text.contains("# extends alpha:Entry"), "{text}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_guide_speaks_this_repos_kit_and_never_the_query_that_returns_nothing() {
        let root = fake_root("guide", &["alpha", "beta"], &[]);
        let text = render(&root);
        let guide = text.split("## Querying: what is in the graph").nth(1).unwrap().split("## The ontology").next().unwrap();
        // Examples use the repo's OWN kit's first foldered class, by derived addresses.
        assert!(guide.contains("?d a alpha:Entry ."), "{guide}");
        assert!(guide.contains("<https://repolex.ai/alpha/Entry/my-id>"), "{guide}");
        assert!(guide.contains("<https://repolex.ai/git-lex/File/Basealpha/Entry/my-id.md>"), "{guide}");
        assert!(!guide.contains("beta:") && !guide.contains("soul:"), "{guide}");
        // No example may type a document as git-lex:Thing: nothing is inferred.
        assert!(!text.contains("?doc a git-lex:Thing") && !text.contains("?d a git-lex:Thing ;"), "{text}");
        // A prefix the file writes is listed, wherever it is written: md: and
        // git2: only in the guide, beta: only in the ontology.
        assert!(text.contains("PREFIX md: <https://repolex.ai/ontology/git-lex/md/>"), "{text}");
        assert!(text.contains("PREFIX git2: ") && text.contains("PREFIX beta: ") && text.contains("PREFIX xsd: "), "{text}");
        // One nothing writes is not.
        for unused in ["owl", "rdf", "rdfs", "fm", "git"] {
            assert!(!text.contains(&format!("PREFIX {unused}: ")), "unused prefix {unused} listed:\n{text}");
        }
        // Root files, files that are not documents, templates.
        assert!(guide.contains("`https://repolex.ai/git-lex/File/README.md`"), "{guide}");
        assert!(guide.contains("comes back blank") && guide.contains("`__<Class>.md`"), "{guide}");
        assert!(text.contains("\n\n## The ontology of this repo"), "{text}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_prefix_counts_as_used_only_where_a_name_is_written_under_it() {
        assert!(uses_prefix("?a md:linksTo ?b", "md"));
        assert!(uses_prefix("(git2:Commit)", "git2"));
        assert!(!uses_prefix("git-lex:title", "lex"), "the tail of a longer prefix");
        assert!(!uses_prefix("git2:Commit", "git"));
        assert!(!uses_prefix("see SOUL.md: it says", "md"), "a filename");
        assert!(!uses_prefix("the rdf: namespace", "rdf"), "no name follows");
    }

    #[test]
    fn brief_keeps_the_whole_first_sentence_and_cuts_only_between_words() {
        assert_eq!(brief("Which Thing this IS, written <namespace/Class/id>. Without it, more.", 120),
                   "Which Thing this IS, written <namespace/Class/id>");
        assert_eq!(brief("The model, e.g. claude. Second.", 120), "The model, e.g. claude");
        assert_eq!(brief("alpha beta gamma delta", 12), "alpha beta …");
    }
}
