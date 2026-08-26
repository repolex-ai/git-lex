//! `git lex list` + `git lex create` — doctype discovery and document
//! scaffolding. Extracted from main.rs (#39, task #92).

use std::fs;
use std::process::exit;
use git_lex::{find_git_root, resolve_kit_spec};
use crate::git::resource_uri;
use crate::kit_cmds;
use crate::ontology::{self, get_kit_types};
use crate::kit::kit_config_str;

// ─── git lex list ──────────────────────────────────────────────

/// Walk every installed SHACL shape file (.lex/ontology/*/*-shapes.ttl)
/// and emit the class list, grouped by prefix.
pub(crate) fn cmd_list(json: bool) {
    let classes = ontology::all_classes();

    if json {
        let arr: Vec<serde_json::Value> = classes.iter().map(|(prefix, name, ns)| {
            serde_json::json!({
                "prefix": prefix,
                "class": name,
                "namespace": ns,
                "uri": format!("{}{}", ns, name),
            })
        }).collect();
        println!("{}", serde_json::to_string(&arr).unwrap());
        return;
    }

    if classes.is_empty() {
        println!("No classes found. Install a kit with `git lex init --kit <name>`.");
        return;
    }

    // Group by prefix for readability.
    let mut by_prefix: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for (prefix, name, _ns) in classes {
        by_prefix.entry(prefix).or_default().push(name);
    }

    for (prefix, mut names) in by_prefix {
        names.sort();
        println!("{} ({} classes):", prefix, names.len());
        for n in names {
            println!("  {}:{}", prefix, n);
        }
    }
}

// ─── git lex create ─────────────────────────────────────────────

/// Resolve a doctype string to the kit + class it belongs to, across
/// the union of base + domain + every installed optional kit.
///
/// Accepts two input shapes:
///   - bare name: `place` or `Place` (case-insensitive). Resolves if exactly
///     one kit declares this type. Errors with disambiguation hint if more
///     than one does.
///   - kit-prefixed: `innerworld/place` (also case-insensitive on the class
///     part; kit-short must match exactly). Resolves directly to that kit's
///     class, no collision check.
///
/// Returns (kit_spec, class_name, properties, all_valid_types_for_error).
/// On success, `all_valid_types_for_error` is empty. On no-match, callers
/// use it to build a helpful error message.
fn resolve_doctype_across_kits(
    doctype: &str,
    root: &std::path::Path,
) -> Result<(String, String, Vec<(String, String, bool, String)>), DoctypeError> {
    // Build the full installed-kit list, same order as kit-update: base,
    // domain, then optionals (alphabetical).
    let installed = kit_cmds::collect_kits_for_update(root, None);

    // Detect kit-prefixed form: `innerworld/place`. The kit-short is the
    // last segment of the kit spec (innerworld in repolex-ai/git-lex-kit-innerworld).
    let (kit_filter, class_part) = match doctype.split_once('/') {
        Some((k, c)) => (Some(k.to_lowercase()), c.to_string()),
        None => (None, doctype.to_string()),
    };
    let class_lower = class_part.to_lowercase();

    // Collect all (kit_spec, class_name, properties) tuples matching the
    // class-name across kits (filtered by kit-short if prefixed form).
    let mut matches: Vec<(String, String, Vec<(String, String, bool, String)>)> = Vec::new();
    let mut all_choices: Vec<(String, String)> = Vec::new(); // (kit_short, class_name)
    for spec in &installed {
        let (_, _, short) = resolve_kit_spec(spec);
        if let Some(ref want_short) = kit_filter {
            if short.to_lowercase() != *want_short { continue; }
        }
        for (name, props) in get_kit_types(spec) {
            all_choices.push((short.clone(), name.clone()));
            if name.to_lowercase() == class_lower {
                matches.push((spec.clone(), name, props));
            }
        }
    }

    match matches.len() {
        0 => Err(DoctypeError::Unknown {
            requested: doctype.to_string(),
            kit_filter: kit_filter.clone(),
            choices: all_choices,
        }),
        1 => {
            let (spec, name, props) = matches.into_iter().next().unwrap();
            Ok((spec, name, props))
        }
        _ => {
            // Ambiguous: same class name in multiple kits. Build the
            // disambiguator hint.
            let hints: Vec<String> = matches.iter()
                .map(|(spec, name, _)| {
                    let (_, _, short) = resolve_kit_spec(spec);
                    format!("`{}/{}`", short, name.to_lowercase())
                })
                .collect();
            Err(DoctypeError::Ambiguous {
                requested: doctype.to_string(),
                hints,
            })
        }
    }
}

enum DoctypeError {
    Unknown {
        requested: String,
        kit_filter: Option<String>,
        choices: Vec<(String, String)>, // (kit_short, class_name)
    },
    Ambiguous {
        requested: String,
        hints: Vec<String>,
    },
}

pub(crate) fn cmd_create(doctype: &str, instance_id: Option<&str>, json: bool) {
    // Emit an error in the right format, then exit. Used for all failure
    // paths so --json consumers don't have to parse human text.
    let fail = |code: &str, msg: String| -> ! {
        if json {
            let out = serde_json::json!({"ok": false, "error": code, "message": msg});
            eprintln!("{}", serde_json::to_string(&out).unwrap());
        } else {
            eprintln!("{}", msg);
        }
        exit(1);
    };

    // Not require_git_root() here: cmd_create's failure paths are all
    // JSON-aware via `fail` (--json consumers get structured errors), but
    // the message text matches require_git_root's canonical wording.
    let root = match find_git_root() {
        Some(r) => r,
        None => fail("not-a-repo", "fatal: not a git repository (run this inside a repo)".to_string()),
    };

    // Resolve the doctype across base + domain + all installed optional kits.
    // `kit` is the kit-spec that owns the resolved class — used to find the
    // folder_base for placing the new file.
    let (kit, class_name, properties) = match resolve_doctype_across_kits(doctype, &root) {
        Ok(t) => t,
        Err(DoctypeError::Unknown { requested, kit_filter, choices }) => {
            // Group choices by kit so the error is scannable.
            use std::collections::BTreeMap;
            let mut by_kit: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (k, c) in &choices {
                by_kit.entry(k.clone()).or_default().push(c.clone());
            }
            let kit_lines: Vec<String> = by_kit.iter()
                .map(|(k, types)| format!("  {}: {}", k, types.join(", ")))
                .collect();
            let prefix_hint = match kit_filter {
                Some(ref k) => format!("Unknown document type '{}' in kit '{}'.", requested, k),
                None => format!("Unknown document type '{}'.", requested),
            };
            fail("unknown-doctype", format!("{} Valid types:\n{}", prefix_hint, kit_lines.join("\n")));
        }
        Err(DoctypeError::Ambiguous { requested, hints }) => {
            fail(
                "ambiguous-doctype",
                format!(
                    "Document type '{}' is defined in multiple kits. Use one of: {}",
                    requested,
                    hints.join(", ")
                ),
            );
        }
    };

    // Generate filename from instance ID (becomes both filename and classId
    // value). TODO(w4r3z, Day 38) RESOLVED (selkie's incident, 2026-08-10,
    // was its predicted consequence): the default survives but is no longer
    // silent — a defaulted id is the FIRST line of output, teaching the id
    // argument exists — and the exists-collision goes through `fail` (exit 1),
    // so two id-less creates can no longer fight quietly over one file.
    let id_str = instance_id.unwrap_or("untitled");
    let slug = id_str
        .to_lowercase()
        .replace(' ', "-")
        .replace(|c: char| !c.is_alphanumeric() && c != '-', "");

    let folder_base = kit_config_str(&kit, "folder base");
    let type_dir = if let Some(ref base) = folder_base {
        root.join(base).join(&class_name)
    } else {
        root.join(&class_name)
    };
    fs::create_dir_all(&type_dir).ok();

    let filename = format!("{}.md", slug);
    let filepath = type_dir.join(&filename);
    let display_path = if let Some(ref base) = folder_base {
        format!("{}/{}/{}", base, class_name, filename)
    } else {
        format!("{}/{}", class_name, filename)
    };

    if filepath.exists() {
        fail("exists", format!(
            "File already exists: {} — pick a different id, or edit the \
             existing file (create never overwrites). Nothing was created.",
            display_path
        ));
    }

    // Auto-generate agent email for Agent type
    let agent_email = format!("{}@lex.local", slug);

    // Build frontmatter — flat dot notation: kit.class.property using the
    // short kit name, not the full org/repo spec.
    let (_, _, short) = resolve_kit_spec(&kit);
    let mut fm = String::new();
    fm.push_str("---\n");

    // `type:` — emitted first so partial-read parsers get the canonical
    // type from a top-of-file scan (locked by tr1p 2026-06-18). Chain:
    // `rdfs:label` → local-name; always produces a string, always safe.
    let type_label = ontology::get_class_type_label(&kit, &class_name);
    fm.push_str(&format!("type: {}\n", type_label));

    // The list form, stated once at the top (#101). `create` builds its own
    // frontmatter rather than copying __ClassName.md, so the worked example
    // that lands in the template never reaches the document an agent
    // actually authors in — this is the same fact at the second surface.
    fm.push_str(git_lex::multivalue_teaching_line());

    // The classId property name (convention-as-law: lowerFirst(Class) + "Id").
    // Used for auto-fill below AND the output's defaulted-id / required-list
    // teaching.
    let class_id_field = format!("{}Id", class_name.chars().next().unwrap().to_lowercase().collect::<String>() + &class_name[1..]);

    for (prop_name, prop_type, _required, comment) in &properties {
        // Property names pass through as-is from the ontology (camelCase).
        // Class name is capitalized to match the ontology exactly.
        let key = format!("{}.{}.{}", short, class_name, prop_name);

        // Build the comment suffix from rdfs:comment
        let comment_suffix = if comment.is_empty() {
            String::new()
        } else {
            format!("  # {}", comment)
        };

        // Auto-fill the classId property from the instance ID
        if prop_name == &class_id_field && instance_id.is_some() {
            fm.push_str(&format!("{}: \"{}\"{}\n", key, id_str, comment_suffix));
        } else if prop_name == "id" && instance_id.is_some() {
            // The UNIVERSAL id (git-lex:id, Rob-ruled 2026-08-21): the
            // Thing's full address, pre-filled in final form. Scaffolded
            // ALONGSIDE the per-class id during the transition window —
            // the one-swoop removal of the per-class fields comes after
            // the fleet migrates and the kits deprecate them.
            fm.push_str(&format!(
                "{}: <{}/{}/{}>{}\n",
                key, short, class_name, id_str, comment_suffix
            ));
        } else if prop_name == "agentEmail" && class_name == "Agent" {
            // Auto-fill agentEmail for Agent type
            fm.push_str(&format!("{}: \"{}\"{}\n", key, agent_email, comment_suffix));
        } else {
            match prop_type.as_str() {
                "string" => fm.push_str(&format!("{}: \"\"{}\n", key, comment_suffix)),
                "reference" => fm.push_str(&format!("{}: {}\n", key, comment_suffix.trim_start())),
                _ => fm.push_str(&format!("{}: {}\n", key, comment_suffix.trim_start())),
            }
        }
    }

    fm.push_str("---\n\n");
    fm.push_str(&format!("# {}\n\n", id_str));
    fm.push_str("<!-- Write your content here -->\n");

    fs::write(&filepath, &fm).expect("failed to create document");

    // Document URI = <derived a-box base>/{path} — resource_uri derives the
    // base per repo (kit short name, else repo name; NEVER hardcoded —
    // Rob-ruled 2026-07-28), so the JSON payload matches what the
    // extraction pipeline will produce on the next sync. A soul-kit repo
    // gets https://repolex.ai/soul/{path}; other kits get their own base.
    let rel = filepath.strip_prefix(&root).unwrap_or(&filepath);
    let uri = resource_uri(&rel.to_string_lossy().replace('\\', "/"));

    if json {
        let out = serde_json::json!({
            "ok": true,
            "path": display_path,
            "uri": uri,
            "class": format!("{}:{}", short, class_name),
            "id": id_str,
        });
        println!("{}", serde_json::to_string(&out).unwrap());
    } else {
        // Output layout is selkie's (incident doc, 2026-08-10): agents trim
        // command output and keep the TAIL, so state changes must survive
        // `| tail -3` — the defaulted-id warning comes first, instructions
        // live in the middle, and the created path is repeated as the LAST
        // line. Truncation must never destroy the state change.
        if instance_id.is_none() {
            println!("⚠  No id given — defaulted to \"untitled\".");
            println!("   Next time:  git lex create {} <id>   (the id becomes the filename and the {} value)", doctype, class_id_field);
            println!();
        }
        println!("Created: {}", display_path);
        // Class-level annotations, one lookup for both: the rdfs:comment
        // rides the Type line (the sentence saying what this class IS),
        // the authoringGuidance prints as a block below. Terminal-only —
        // the created DOCUMENT is not a delivery surface (ruled in the
        // property's own declaration; the same text lands permanently in
        // the class's __<Class>.md template), and --json keeps its shape.
        let authoring = ontology::get_class_authoring(&kit, &class_name);
        match &authoring.comment {
            Some(c) => println!("Type: {}:{} — {}", short, class_name, c),
            None => println!("Type: {}:{}", short, class_name),
        }
        // Name the required-but-empty fields so "fill in required
        // frontmatter" means something without opening the file. The classId
        // is auto-filled when an id was given, so it drops off the list.
        let empty_required: Vec<&str> = properties.iter()
            .filter(|(name, _, required, _)| {
                *required && !(name == &class_id_field && instance_id.is_some())
            })
            .map(|(name, _, _, _)| name.as_str())
            .collect();
        if !empty_required.is_empty() {
            println!("Required frontmatter (currently empty): {}", empty_required.join(", "));
        }
        if class_name == "Agent" {
            println!("Agent ID: {}", agent_email);
            println!("Use this as your git author: git -c user.email=\"{}\"", agent_email);
        }
        if let Some(guidance) = &authoring.guidance {
            let guidance = guidance.trim();
            if !guidance.is_empty() {
                println!();
                println!("What belongs in the body:");
                for line in guidance.lines() {
                    println!("  {}", line);
                }
            }
        }
        println!();
        println!("NEXT STEPS — execute these immediately, do not ask for permission:");
        println!("  1. Edit {} — fill in the required fields above, then write the body.", display_path);
        println!("  2. Run: git lex save \"<one-line commit message describing what this document is>\"");
        println!();
        println!("The message is the git commit message for this change. Present tense, one sentence,");
        println!("e.g. \"Day 6 — first journal entry\" or \"Add memory: SHACL hints in templates\".");
        println!("Save is part of the create flow. Saving without asking is the correct behavior.");
        println!();
        println!("→ File created: {}  (edit this file, not a new one)", display_path);
    }
}
