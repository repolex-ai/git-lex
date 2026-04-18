//! Ontology reading: parse kit TTL files to extract classes, properties,
//! ObjectProperty / DatatypeProperty classification, ranges, and type info.
//!
//! Peeled out of `main.rs` during modularization. All functions here are
//! pure reads against TTL files on disk; no store writes, no side effects
//! beyond reading `.lex/ontology/*.ttl` and `.lex/kit/**/*.ttl`.

use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::store::Store;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Cursor;
use std::process::exit;

use git_lex::{find_git_root, kit_install_dir_for_spec, resolve_kit_spec};

/// Get the TTL prefix name for a kit (kit name may differ from prefix).
pub(crate) fn get_kit_prefix_name(kit_name: &str) -> &str {
    match kit_name {
        "claude-code" => "cc",
        "lex-lab" => "lab",
        other => other,
    }
}

/// Extract the `owl:Ontology` IRI declaration from a Turtle file's body.
/// Returns the subject IRI of the first `<IRI> a owl:Ontology` triple found,
/// or None if the file doesn't declare itself as an ontology. Deterministic,
/// regex-based — we don't need a full Turtle parser to pull this one fact out.
pub(crate) fn extract_ontology_iri(ttl: &str) -> Option<String> {
    // Matches patterns like:
    //   <https://example.org/ont> a owl:Ontology
    //   <https://example.org/ont> rdf:type owl:Ontology
    // Newlines allowed between subject and `a`/`rdf:type` and `owl:Ontology`.
    let re = regex::Regex::new(
        r"<([^>]+)>\s*(?:a|rdf:type)\s+owl:Ontology\b",
    ).ok()?;
    re.captures(ttl)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// Walk `.lex/ontology/**/*.ttl`, extract each ontology's self-declared IRI,
/// and load each file into the store under that IRI as a named graph.
/// Drop-and-replace on every call — drift-proof.
///
/// Shape files (`*-shapes.ttl`) are skipped here (SHACL lives elsewhere in
/// the pipeline). Files without a `owl:Ontology` declaration are skipped
/// with a warning.
///
/// On parse error we fail loudly: print the file path + parser error and
/// exit non-zero. A broken TTL on disk is a user-visible state we want
/// them to fix immediately, not silently paper over.
pub(crate) fn load_ontology_tboxes(store: &Store, root: &std::path::Path) -> usize {
    let mut ttl_files: Vec<std::path::PathBuf> = Vec::new();
    fn collect_ttls(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_dir() {
                    collect_ttls(&path, out);
                } else if path.extension().is_some_and(|e| e == "ttl") {
                    let name = path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("");
                    // Skip SHACL shape files — they're loaded by the validator,
                    // not the TBox loader.
                    if name.contains("shapes") {
                        continue;
                    }
                    out.push(path);
                }
            }
        }
    }
    // Load ontology TTL files from both .lex/ontology/ (built-in: git, fm,
    // lex) and .lex/kit/ (kit-provided: squad.ttl, soul.ttl, etc.). Both
    // directories are scanned recursively so kit TTLs in nested org/repo/
    // paths are found automatically.
    let ontology_dir = root.join(".lex").join("ontology");
    if ontology_dir.exists() {
        collect_ttls(&ontology_dir, &mut ttl_files);
    }
    let kit_dir = root.join(".lex").join("kit");
    if kit_dir.exists() {
        collect_ttls(&kit_dir, &mut ttl_files);
    }
    ttl_files.sort();

    let mut loaded = 0;
    for path in &ttl_files {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error reading ontology {}: {}", path.display(), e);
                exit(1);
            }
        };

        let iri = match extract_ontology_iri(&content) {
            Some(i) => i,
            None => {
                eprintln!(
                    "warning: {} has no `owl:Ontology` declaration, skipping TBox load",
                    path.display()
                );
                continue;
            }
        };

        let graph = match oxigraph::model::NamedNode::new(&iri) {
            Ok(n) => n,
            Err(e) => {
                eprintln!(
                    "error: {} has invalid ontology IRI <{}>: {}",
                    path.display(), iri, e
                );
                exit(1);
            }
        };

        // Drop-and-replace: clear the graph first.
        store
            .clear_graph(&oxigraph::model::GraphName::from(graph.clone()))
            .ok();

        // Load the turtle into the named graph. Use with_default_graph so
        // any triples in the TTL (which is graphless) land inside our IRI.
        let parser = RdfParser::from_format(RdfFormat::Turtle)
            .with_default_graph(graph.clone());
        if let Err(e) = store.load_from_reader(parser, Cursor::new(content.as_bytes())) {
            eprintln!("error: failed to parse/load {}: {}", path.display(), e);
            exit(1);
        }
        loaded += 1;
    }
    loaded
}

/// Read the raw content of a kit's primary TTL file. Tries `{short}.ttl`
/// first, then falls back to the first non-`-shapes.ttl` file in the kit
/// directory. Returns an empty string if nothing is found.
fn read_kit_ttl(kit: &str) -> String {
    let root = match find_git_root() {
        Some(r) => r,
        None => return String::new(),
    };
    let (_, _, short) = resolve_kit_spec(kit);

    // Primary: read from .lex/ontology/{short}/{short}.ttl
    // This is where scaffold copy installs kit ontologies at init/update time.
    let ontology_dir = root.join(".lex").join("ontology").join(&short);
    let primary = ontology_dir.join(format!("{}.ttl", short));
    if let Ok(c) = fs::read_to_string(&primary) {
        return c;
    }

    // Fallback: any .ttl in .lex/ontology/{short}/ (excluding shapes)
    if ontology_dir.exists() {
        if let Some(c) = fs::read_dir(&ontology_dir).ok()
            .and_then(|entries| entries.filter_map(|e| e.ok())
                .find(|e| e.path().extension().is_some_and(|ext| ext == "ttl")
                    && !e.file_name().to_string_lossy().contains("shapes"))
                .and_then(|e| fs::read_to_string(e.path()).ok()))
        {
            return c;
        }
    }

    // Legacy fallback: read from .lex/kit/ (pre-scaffold-migration repos)
    let kit_dir = kit_install_dir_for_spec(&root, kit);
    let legacy = kit_dir.join(format!("{}.ttl", short));
    if let Ok(c) = fs::read_to_string(&legacy) {
        return c;
    }

    String::new()
}

/// Find the prefix name declared for the kit's namespace in TTL content.
/// Falls back to the short kit name if no declaration is found.
fn find_prefix_name(content: &str, short: &str) -> String {
    let kit_ns_pattern = format!("/kit/{}/", short);
    for line in content.lines() {
        if line.starts_with("@prefix ") && line.contains(&kit_ns_pattern) {
            if let Some(colon_pos) = line[8..].find(':') {
                return line[8..8 + colon_pos].trim().to_string();
            }
        }
    }
    short.to_string()
}

pub(crate) fn get_object_properties(kit: &str) -> HashSet<String> {
    let content = read_kit_ttl(kit);
    if content.is_empty() { return HashSet::new(); }

    let (_, _, short) = resolve_kit_spec(kit);
    let prefix_name = find_prefix_name(&content, &short);

    let mut obj_props = HashSet::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.contains("a owl:ObjectProperty") {
            if let Some(prop) = trimmed.split_whitespace().next() {
                let name = prop
                    .strip_prefix(&format!("{}:", prefix_name))
                    .unwrap_or(prop)
                    .to_string();
                obj_props.insert(name);
            }
        }
    }
    obj_props
}

/// Build a map of property name → XSD datatype from the kit ontology TTL.
/// Only includes properties with non-string ranges (xsd:integer, xsd:date,
/// xsd:dateTime, xsd:boolean, xsd:decimal, xsd:anyURI).
/// Properties with xsd:string or no range are omitted (they use the
/// default string behavior).
pub(crate) fn get_property_datatypes(kit: &str) -> HashMap<String, String> {
    let content = read_kit_ttl(kit);
    if content.is_empty() { return HashMap::new(); }

    let (_, _, short) = resolve_kit_spec(kit);
    let prefix_name = find_prefix_name(&content, &short);

    // Parse property blocks: track current property name, then capture rdfs:range
    let mut datatypes = HashMap::new();
    let mut current_prop = String::new();

    for line in content.lines() {
        let trimmed = line.trim();

        // New property block
        if trimmed.contains("a owl:DatatypeProperty") {
            if let Some(prop) = trimmed.split_whitespace().next() {
                current_prop = prop
                    .strip_prefix(&format!("{}:", prefix_name))
                    .unwrap_or(prop)
                    .to_string();
            }
        }

        // Capture rdfs:range with XSD type
        if !current_prop.is_empty() && trimmed.starts_with("rdfs:range") {
            if let Some(range) = trimmed.split_whitespace().nth(1) {
                let range = range.trim_end_matches(|c: char| c == ' ' || c == ';' || c == '.');
                // Map XSD prefix to full URI
                let xsd_type = match range {
                    "xsd:integer" => Some("http://www.w3.org/2001/XMLSchema#integer"),
                    "xsd:date" => Some("http://www.w3.org/2001/XMLSchema#date"),
                    "xsd:dateTime" => Some("http://www.w3.org/2001/XMLSchema#dateTime"),
                    "xsd:boolean" => Some("http://www.w3.org/2001/XMLSchema#boolean"),
                    "xsd:decimal" => Some("http://www.w3.org/2001/XMLSchema#decimal"),
                    "xsd:float" => Some("http://www.w3.org/2001/XMLSchema#float"),
                    "xsd:double" => Some("http://www.w3.org/2001/XMLSchema#double"),
                    "xsd:anyURI" => Some("http://www.w3.org/2001/XMLSchema#anyURI"),
                    _ => None, // xsd:string or unknown → default string behavior
                };
                if let Some(dt) = xsd_type {
                    datatypes.insert(current_prop.clone(), dt.to_string());
                }
            }
        }

        // Blank line ends property block
        if trimmed.is_empty() {
            current_prop.clear();
        }
    }

    datatypes
}

/// Build a map of ObjectProperty name → range class IRI from the kit ontology TTL.
/// Only includes properties whose range is a kit class (not xsd:*, not rdfs:Resource).
/// Used by the range-aware wikilink/mention resolver at extraction time: if
/// `squad:from rdfs:range squad:Agent`, then `[[4rx]]` on a `from` field gets
/// resolved against the agent slug space, not the whole entity space.
pub(crate) fn get_property_ranges(kit: &str) -> HashMap<String, String> {
    let content = read_kit_ttl(kit);
    if content.is_empty() { return HashMap::new(); }

    let (_, _, short) = resolve_kit_spec(kit);
    let prefix_name = find_prefix_name(&content, &short);

    // Parse property blocks: capture rdfs:range for ObjectProperties only.
    let mut ranges = HashMap::new();
    let mut current_prop = String::new();
    let mut current_is_object_prop = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.contains("a owl:ObjectProperty") {
            if let Some(prop) = trimmed.split_whitespace().next() {
                current_prop = prop
                    .strip_prefix(&format!("{}:", prefix_name))
                    .unwrap_or(prop)
                    .to_string();
                current_is_object_prop = true;
            }
        } else if trimmed.contains("a owl:DatatypeProperty") {
            current_is_object_prop = false;
        }

        if current_is_object_prop && !current_prop.is_empty() && trimmed.starts_with("rdfs:range") {
            if let Some(range) = trimmed.split_whitespace().nth(1) {
                let range = range.trim_end_matches(|c: char| c == ' ' || c == ';' || c == '.');
                // Skip xsd:* and rdfs:* — only care about kit class ranges.
                if range.starts_with("xsd:") || range.starts_with("rdfs:") {
                    continue;
                }
                // Resolve prefix:ClassName to full IRI (uses short kit name).
                let class_iri = if let Some(local) = range.strip_prefix(&format!("{}:", prefix_name)) {
                    format!("https://repolex.ai/ontology/kit/{}/{}", short, local)
                } else if range.starts_with('<') && range.ends_with('>') {
                    range[1..range.len() - 1].to_string()
                } else {
                    continue; // unknown prefix, skip
                };
                ranges.insert(current_prop.clone(), class_iri);
            }
        }

        if trimmed.is_empty() {
            current_prop.clear();
            current_is_object_prop = false;
        }
    }

    ranges
}

/// Parse the kit ontology to find document types and their properties.
/// Returns: Vec<(ClassName, Vec<(prop_name, prop_type, required, comment)>)>
pub(crate) fn get_kit_types(kit: &str) -> Vec<(String, Vec<(String, String, bool, String)>)> {
    let content = read_kit_ttl(kit);
    if content.is_empty() { return Vec::new(); }

    let (_, _, short) = resolve_kit_spec(kit);

    // Extract the kit prefix — find the @prefix that maps to this kit's
    // namespace URL. The namespace uses the short kit name, not the full
    // org/repo spec.
    let kit_ns_pattern = format!("/kit/{}/", short);
    let mut prefix_name = short.clone();
    for line in content.lines() {
        if line.starts_with("@prefix ") && line.contains(&kit_ns_pattern) {
            // Extract prefix name: @prefix lab: <...> .
            if let Some(colon_pos) = line[8..].find(':') {
                prefix_name = line[8..8 + colon_pos].trim().to_string();
            }
            break;
        }
    }

    // Find all owl:Class declarations and their properties
    let mut types: HashMap<String, Vec<(String, String, bool, String)>> = HashMap::new();
    let mut current_class = String::new();

    for line in content.lines() {
        let trimmed = line.trim();

        // Detect class: "squad:Decision a owl:Class ;"
        if trimmed.contains("a owl:Class") {
            if let Some(class_name) = trimmed.split_whitespace().next() {
                let name = class_name
                    .strip_prefix(&format!("{}:", prefix_name))
                    .unwrap_or(class_name)
                    .to_string();
                current_class = name.clone();
                types.entry(name).or_default();
            }
        }

        // Detect property with domain: "rdfs:domain squad:Decision ;"
        if trimmed.contains("rdfs:domain") && trimmed.contains(&format!("{}:", prefix_name)) {
            // Look back to find the property name — this is tricky with TTL
            // Instead, we'll parse properties differently
        }
    }
    let _ = current_class; // retained for potential future use

    // Parse properties: track current property name, type, and comment across multi-line TTL blocks
    let mut current_prop = String::new();
    let mut current_prop_type = String::new();
    let mut current_comment = String::new();

    for line in content.lines() {
        let trimmed = line.trim();

        // New property block starts with "kit:propName a owl:DatatypeProperty/ObjectProperty"
        if trimmed.contains("a owl:DatatypeProperty") || trimmed.contains("a owl:ObjectProperty") {
            if let Some(prop) = trimmed.split_whitespace().next() {
                current_prop = prop
                    .strip_prefix(&format!("{}:", prefix_name))
                    .unwrap_or(prop)
                    .to_string();
                current_prop_type = if trimmed.contains("DatatypeProperty") {
                    "string".to_string()
                } else {
                    "reference".to_string()
                };
                current_comment.clear();
            }
        }

        // Capture rdfs:comment within a property block
        if !current_prop.is_empty() && trimmed.starts_with("rdfs:comment") {
            // Extract the quoted string: rdfs:comment "Some text." ;
            if let Some(start) = trimmed.find('"') {
                if let Some(end) = trimmed[start + 1..].find('"') {
                    current_comment = trimmed[start + 1..start + 1 + end].to_string();
                }
            }
        }

        // Domain line within a property block
        if !current_prop.is_empty() && trimmed.starts_with("rdfs:domain") {
            if let Some(domain) = trimmed.split_whitespace().nth(1) {
                let class_name = domain
                    .strip_prefix(&format!("{}:", prefix_name))
                    .unwrap_or(domain)
                    .trim_end_matches(|c: char| c == ' ' || c == ';' || c == '.')
                    .to_string();

                if let Some(props) = types.get_mut(&class_name) {
                    props.push((current_prop.clone(), current_prop_type.clone(), false, current_comment.clone()));
                }
            }
        }

        // A blank line or a new top-level definition ends the current property block
        if trimmed.is_empty() {
            current_prop.clear();
            current_comment.clear();
        }
    }

    types.into_iter().collect()
}
