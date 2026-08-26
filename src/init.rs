//! `git lex init` — bootstrap a repo into a git-lex knowledge repo.
//!
//! Installs the base kit (plus an optional domain kit), writes the `.lex/`
//! structure and `repo.yml`, generates SHACL shapes, class folders,
//! class templates and README.lex.md, collects kit-declared init variables,
//! wires substrate identity, and installs the pre-commit hook.

use std::collections::HashMap;
use std::fs;
use std::process::{Command, exit};

use git_lex::{find_git_root, registry_add, resolve_kit_spec};

use crate::git::auto_commit_snapshot;
use crate::harness;
use crate::hooks;
use crate::kit::{collect_init_variables, fetch_kit_from_github, install_scaffold_files_from,
                 kit_config_bool, kit_config_str, read_repo_yml_fields};
use crate::kit_cmds;
use crate::ontology::{self, get_kit_types};
use crate::shacl::build_shacl_shapes;
use crate::BASE_KIT;

// Base ontologies (git.ttl, fm.ttl, lex.ttl) are no longer embedded in the
// binary. They ship in the base kit scaffold at scaffold/.lex/ontology/ and
// are installed to .lex/ontology/ by the scaffold installer during init.
// Kit ontologies are fetched from GitHub at init time — no embedded fallback.


pub(crate) fn cmd_init(directory: Option<String>, kit: Option<String>) {
    // Follow git convention: `git lex init [<directory>]`
    // If a directory is given, cd into it (creating it if necessary).
    if let Some(ref dir) = directory {
        let path = std::path::Path::new(dir);
        if !path.exists() {
            if let Err(e) = fs::create_dir_all(path) {
                eprintln!("fatal: cannot create {}: {e}", path.display());
                exit(1);
            }
        }
        std::env::set_current_dir(path).expect("failed to cd into directory");
    }

    let root = match find_git_root() {
        Some(r) => r,
        None => {
            let cwd = std::env::current_dir().expect("failed to get current directory");
            eprint!("Not a git repository. Initialize one in {}? [Y/n] ", cwd.display());
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).unwrap_or_default();
            let input = input.trim().to_lowercase();
            if input.is_empty() || input == "y" || input == "yes" {
                let status = Command::new("git").args(["init"]).status();
                match status {
                    Ok(s) if s.success() => { println!(); }
                    _ => { eprintln!("fatal: failed to initialize git repository"); exit(1); }
                }
                cwd
            } else {
                eprintln!("Aborted.");
                exit(1);
            }
        }
    };

    // Every repo gets the base kit. If --kit is specified, that kit is
    // installed alongside base (not instead of it). The kit_spec in repo.yml
    // records the domain kit; base is implicit and always present.
    let kit_name = kit.as_deref().unwrap_or(BASE_KIT);
    let (org, repo, kit_short) = resolve_kit_spec(kit_name);
    let kit_spec = format!("{}/{}", org, repo);

    let lex_dir = root.join(".lex");

    // Carry-over on re-init: if repo.yml already exists, stash its fields so
    // we can reuse previously-collected init variables (agent_name, etc.)
    // without re-prompting.
    let mut carryover: HashMap<String, String> = HashMap::new();

    // If .lex/ already exists, this is a re-initialization. We ask the user,
    // then refresh only the kit-derived subdirs (kit/, ontology/). User
    // data (extract/, tickets/) is preserved.
    if lex_dir.exists() {
        carryover = read_repo_yml_fields(&lex_dir.join("repo.yml"));

        eprint!(
            "This repo is already initialized at {}.\n\
             Re-initializing will refresh the kit and ontology files and overwrite scaffold files.\n\
             Extractions and repo.yml fields are preserved.\n\
             Continue? [y/N] ",
            lex_dir.display()
        );
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).unwrap_or_default();
        let input = input.trim().to_lowercase();
        if input != "y" && input != "yes" {
            println!("Aborted.");
            return;
        }

        // Auto-commit any uncommitted work before the destructive step.
        // Git history is the safety net — if anything in the working tree
        // would be lost, we want it in a commit first.
        auto_commit_snapshot("re-initialization");

        // Remove only kit-derived subdirs. Everything else in .lex/ stays.
        let _ = fs::remove_dir_all(lex_dir.join("kit"));
        let _ = fs::remove_dir_all(lex_dir.join("ontology"));
    }

    // Create .lex/ structure (idempotent)
    fs::create_dir_all(lex_dir.join("extract")).ok();

    // Ontologies are installed from the base kit scaffold (scaffold/.lex/ontology/)
    // by the scaffold installer below — no hardcoded ontology block needed.

    // Install kit(s) — extracted step, see fn doc.
    fetch_kits(&lex_dir, kit_name, &kit_spec, &org, &repo);

    // Create the machine-local pocket for derived data (oxigraph store, etc.)
    // — gitignored via the managed engine block below, never committed.
    let pocket_dir = lex_dir.join("_ignore");
    fs::create_dir_all(&pocket_dir).ok();

    // Claude Code kit needs a whitelist-style root .gitignore because it
    // runs against an existing project directory and only wants to track
    // specific patterns. This is the one kit-specific exception until kits
    // ship their own .gitignore assets.
    if kit_name == "claude-code" {
        let gitignore = root.join(".gitignore");
        let cc_ignore = "*\n\
                         !.gitignore\n\
                         !.lex/\n\
                         !.lex/**\n\
                         !README.lex.md\n\
                         !*/\n\
                         !*/*.jsonl\n\
                         !*/memory/\n\
                         !*/memory/**\n\
                         !agents/\n\
                         !agents/**\n\
                         !plans/\n\
                         !plans/**\n\
                         !CLAUDE.md\n\
                         !history.jsonl\n";
        if gitignore.exists() {
            let existing = fs::read_to_string(&gitignore).unwrap_or_default();
            if !existing.contains("!history.jsonl") {
                fs::write(&gitignore, format!("{}\n{}", existing.trim_end(), cc_ignore)).ok();
            }
        } else {
            fs::write(&gitignore, cc_ignore).ok();
        }
    }

    // Every soul gets the engine runtime dirs (.pool/ .copia/ .weave/) ignored.
    // These are the per-soul LOCAL state of the three Subtexture engines (Pool,
    // CoPIA, Weave) — index stores, embeddings, media roots — that must never be
    // committed. Pushed by git-lex so souls don't hand-maintain it and miss a new
    // dir (the leak: lUX committed 155 .pool/ files, W4R3Z 11 Pool/oxigraph/, both
    // because their hand-written .gitignore predated .pool/). Idempotent.
    kit_cmds::ensure_engine_gitignore(&root);

    // repo.yml — create if missing, otherwise update the kit: field to
    // match the spec passed to this init run. This matters for re-init:
    // if the user ran init once without --kit and then runs again with
    // --kit X, the kit: field needs to change from "none" to the new spec.
    let repo_yml_path = lex_dir.join("repo.yml");
    write_repo_yml(&repo_yml_path, &root, &kit_spec);

    // README
    let readme_path = lex_dir.join("README.md");
    if !readme_path.exists() {
        fs::write(&readme_path, format!(
            "# .lex/\n\nKnowledge graph managed by git-lex.\nKit: {}\n\n\
             - `extract/` — extraction sidecars (.spo)\n\
             - `ontology/` — ontology definitions\n\
             - `_ignore/oxigraph/` — local SPARQL store (machine-local, gitignored)\n",
            kit_name
        )).ok();
    }

    // ─── Install scaffold files BEFORE type folders ───
    // Scaffold copy puts kit ontologies into .lex/ontology/ which
    // get_kit_types() needs to read for folder/template creation.
    {
        let (base_org, base_repo, _) = resolve_kit_spec(BASE_KIT);
        let base_kit_dir = lex_dir.join("kit").join(&base_org).join(&base_repo);
        let mut scaffold_count = install_scaffold_files_from(&base_kit_dir);

        let domain_kit_dir = lex_dir.join("kit").join(&org).join(&repo);
        if domain_kit_dir != base_kit_dir {
            scaffold_count += install_scaffold_files_from(&domain_kit_dir);
        }
        if scaffold_count > 0 {
            println!("Installed {} scaffold file(s) from kit(s)", scaffold_count);
        }
    }

    // Generate SHACL shapes from kit ontology now — both the type-folder
    // loop below and the README generator read shapes via get_kit_types,
    // so shapes must exist before either runs.
    match build_shacl_shapes(kit_name) {
        Ok(Some(shapes_path)) => println!("SHACL shapes generated: {}", shapes_path.file_name().unwrap_or_default().to_string_lossy()),
        Ok(None) => {} // kit ships no ontology — nothing to generate
        Err(e) => {
            eprintln!("fatal: SHACL shapes generation failed for '{}': {}", kit_name, e);
            eprintln!("       a broken kit ontology must not install silently — validation would be skipped and object properties would degrade to literals");
            exit(1);
        }
    }

    // Create type folders from kit ontology — extracted step, see fn doc.
    create_class_folders(kit_name, &root);

    // README.lex.md — install-once (convergence is board #75); the body is
    // a pure generator so it is testable and reusable.
    {
        let kit_types = get_kit_types(kit_name);
        if !kit_types.is_empty() {
            let readme_lex = root.join("README.lex.md");
            if !readme_lex.exists() {
                let doc = generate_readme_lex(&kit_short, &kit_types);
                fs::write(&readme_lex, &doc).ok();
                println!("Created README.lex.md");
            }
        }
    }

    // Class templates (shapes were already generated above before the
    // type-folder loop). Emitted by the shared canonical emitter in
    // kit_cmds — the same one kit-add / kit-update run — so init-time
    // templates match kit-update output exactly and stay converged over
    // the repo's life.
    {
        let create_folders = kit_config_bool(kit_name, "install folders", false)
            || kit_config_bool(kit_name, "createTypeFolders", false);
        kit_cmds::emit_class_templates(kit_name, &root, create_folders);
        println!("Created class templates");
    }

    // Stored-query starters — .lex/query/ with the defaults, first init only.
    crate::query::scaffold_default_queries(&root);

    // Collect kit-declared init variables (agent name, etc.) by prompting
    // the user. On re-init, reuse values carried over from the previous
    // repo.yml so the user doesn't re-answer the same prompts.
    //
    // Then install files from the kit in two passes:
    //   1. scaffold/ — raw byte-for-byte copy, always clobber. Kit
    //      infrastructure: .claude/, AGENTS.md, hooks, etc.
    //   2. assets/ — template-processed (substitute `{varname}` placeholders
    //      from the variable map). Skip if destination exists (default);
    //      under --force, prompt to overwrite with clear warning. No backup
    //      files — git history is the safety net.
    {
        let vars = collect_init_variables(kit_name, &carryover);

        // Persist collected variables into repo.yml (one line per var, append
        // if not already present). Skip the `kit` key — already in repo.yml.
        if let Ok(existing) = fs::read_to_string(&repo_yml_path) {
            let mut updated = existing.clone();
            for (k, v) in &vars {
                if k == "kit" { continue; }
                if !existing.lines().any(|l| l.starts_with(&format!("{}:", k))) {
                    if !updated.ends_with('\n') { updated.push('\n'); }
                    updated.push_str(&format!("{}: {}\n", k, v));
                }
            }
            fs::write(&repo_yml_path, &updated).unwrap_or_else(|e| {
                eprintln!("fatal: could not persist init variables to .lex/repo.yml: {}", e);
                exit(1);
            });
        }

        // Scaffold files already installed above (before type folder creation).

        // ── Substrate setup ──────────────────────────────────────────
        //
        // After scaffold files are installed and the agent identifier is
        // known (from the init prompts above, stored in `vars`), configure
        // each supported LLM substrate so the agent's identity is wired
        // into the session.
        //
        // Each substrate gets its own setup function. The agent_name
        // (collected via init prompts from the kit's kit.yml) is injected
        // into the substrate's local config so commits, hooks, and tool
        // calls all use the correct identity.
        //
        // Currently: Claude Code (via settings.json env block).
        // Future: Gemini, OpenAI Codex, etc. — stub those as needed after
        // researching how each model's CLI handles per-project identity.

        // Per-substrate identity injection through the ONE shared gate
        // (review #11). Souls are portable across machines via git, so
        // identity travels with the repo — committed to a substrate-
        // specific config file. This copy of the gate used to skip in
        // TOTAL silence when no agent_name was collected (empty enter,
        // scripted init, kit.yml without the prompt) — the exact
        // well-dressed-dead class #67 fixed in kit-add/kit-update while
        // this third copy kept it. An empty collection now falls back to
        // repo.yml, and the no-name-anywhere case warns loudly inside.
        let agent_name = vars.get("agent_name").cloned().unwrap_or_default();
        harness::run_substrate_setup(
            &root,
            if agent_name.is_empty() { None } else { Some(&agent_name) },
        );
    }

    // Print summary
    println!("Initialized .lex/ in {}", root.display());
    println!();
    println!("  .lex/repo.yml     — repo config (kit: {})", kit_spec);
    println!("  .lex/extract/     — extraction sidecars");
    println!("  .lex/ontology/    — upper ontologies");
    println!("  .lex/kit/         — installed kit");
    println!();

    // Pre-commit hook: extract + validate on every commit. This IS the
    // enforcement gate — if it can't be installed, saying so loudly beats
    // a repo that silently never validates.
    // Respects core.hooksPath (husky, lefthook, etc.)
    match hooks::install_hook() {
        Ok(()) => println!("Installed pre-commit hook (extract + validate on commit)"),
        Err(e) => {
            eprintln!("fatal: could not install the pre-commit hook: {}", e);
            eprintln!("       commits would silently skip extraction + validation — fix and re-run init");
            exit(1);
        }
    }

    // NO post-commit hook — sync is manual/background

    // Commit setup files + offer the initial-content commit for
    // brand-new repos — extracted step, see fn doc.
    commit_setup_and_content();

    // Genesis identity: repo.yml genesis_sha + SOUL.md soulId fill,
    // committed as "git lex identity" — extracted step, see fn doc.
    record_identity(&root, &repo_yml_path);

    // t-box: load installed kit ontologies into the persistent ontology
    // graph (the shared lifecycle helper — review #11).
    crate::kit_cmds::reload_ontology_graph();

    // Register this repo in the machine-level registry (~/.lex/repos)
    registry_add(&root);
}

// ─── init steps (extracted from cmd_init — review #12: the 582-line
// single function was the longest in the crate, and these seams were
// already delimited as anonymous blocks) ─────────────────────────────

/// init step: fetch and install the base kit (always) plus the domain
/// kit (when different). Any fetch failure aborts init — partial kit
/// state is worse than none, and the only failure mode here is
/// network/auth.
fn fetch_kits(lex_dir: &std::path::Path, kit_name: &str, kit_spec: &str, org: &str, repo: &str) {
    let lex_kit_root = lex_dir.join("kit");
    let _ = fs::remove_dir_all(&lex_kit_root);

    // Always install base kit.
    let (base_org, base_repo, _) = resolve_kit_spec(BASE_KIT);
    let base_dir = lex_kit_root.join(&base_org).join(&base_repo);
    fs::create_dir_all(&base_dir).ok();
    println!("Downloading base kit {}/{}...", base_org, base_repo);
    if fetch_kit_from_github(BASE_KIT, &base_dir) {
        println!("Base kit installed.");
    } else {
        eprintln!("Failed to fetch base kit from GitHub.");
        eprintln!("Check network access to https://github.com/{}/{}", base_org, base_repo);
        exit(1);
    }

    // Install the domain kit (if different from base).
    let kit_dir = lex_kit_root.join(org).join(repo);
    if kit_spec != format!("{}/{}", base_org, base_repo) {
        fs::create_dir_all(&kit_dir).ok();
        println!("Downloading additional kit {}/{}...", org, repo);
        if fetch_kit_from_github(kit_name, &kit_dir) {
            println!("Additional kit installed.");
        } else {
            eprintln!("Failed to fetch kit '{}' from GitHub.", kit_name);
            eprintln!("Check that https://github.com/{}/{} exists and you have network access.", org, repo);
            exit(1);
        }
    }
}

/// init step: create repo.yml if missing (three keys: name/kit/created —
/// the wikilink-era `link_semantics` fence is fully retired, one link
/// law, Rob-ruled 2026-08-08), or rewrite just the `kit:` line of an
/// existing one so a re-init with --kit X rebinds without touching any
/// other field.
fn write_repo_yml(repo_yml_path: &std::path::Path, root: &std::path::Path, kit_spec: &str) {
    if !repo_yml_path.exists() {
        let repo_name = root.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let today = Command::new("date").args(["+%Y-%m-%d"]).output().ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        fs::write(repo_yml_path, format!(
            "{}name: {}\nkit: {}\ncreated: {}\n",
            crate::git::REPO_YML_HEADER, repo_name, kit_spec, today
        )).unwrap_or_else(|e| {
            eprintln!("fatal: could not write .lex/repo.yml: {}", e);
            exit(1);
        });
    } else if let Ok(existing) = fs::read_to_string(repo_yml_path) {
        let mut updated_lines: Vec<String> = Vec::new();
        let mut saw_kit_line = false;
        for line in existing.lines() {
            if line.starts_with("kit: ") || line.starts_with("kit:") {
                updated_lines.push(format!("kit: {}", kit_spec));
                saw_kit_line = true;
            } else {
                updated_lines.push(line.to_string());
            }
        }
        if !saw_kit_line {
            updated_lines.push(format!("kit: {}", kit_spec));
        }
        let mut content = updated_lines.join("\n");
        if !content.ends_with('\n') { content.push('\n'); }
        fs::write(repo_yml_path, content).unwrap_or_else(|e| {
            eprintln!("fatal: could not update .lex/repo.yml kit binding: {}", e);
            exit(1);
        });
    }
}

/// init step: create the kit's class folders (gated per class on the
/// shared foldered-AND-not-deprecated predicate, #74). Reads kit.yml
/// "install folders" / "folder base" ("createTypeFolders" is the legacy
/// pre-migration key).
fn create_class_folders(kit_name: &str, root: &std::path::Path) {
    let create_folders = kit_config_bool(kit_name, "install folders", false)
        || kit_config_bool(kit_name, "createTypeFolders", false);
    if !create_folders {
        return;
    }
    let folder_base = kit_config_str(kit_name, "folder base");
    let kit_types = get_kit_types(kit_name);
    let mut created: Vec<String> = Vec::new();
    for (type_name, _) in &kit_types {
        if !ontology::class_gets_folder(kit_name, type_name) {
            continue;
        }
        let type_dir = if let Some(ref base) = folder_base {
            root.join(base).join(type_name)
        } else {
            root.join(type_name)
        };
        fs::create_dir_all(&type_dir).ok();
        // Add a .gitkeep so empty dirs are tracked
        let gitkeep = type_dir.join(".gitkeep");
        if !gitkeep.exists() {
            fs::write(&gitkeep, "").ok();
        }
        created.push(type_name.clone());
    }
    if !created.is_empty() {
        let prefix = folder_base.as_deref().unwrap_or("");
        if prefix.is_empty() {
            println!("Created type folders: {}", created.join(", "));
        } else {
            println!("Created type folders: {}/{{{}}}", prefix, created.join(", "));
        }
    }
}

/// init step: the README.lex.md body — a PURE generator (no I/O), so the
/// onboarding page is testable and ready for the #75 convergence path.
fn generate_readme_lex(
    kit_short: &str,
    kit_types: &[(String, Vec<(String, String, bool, String)>)],
) -> String {
    let type_names: Vec<String> = kit_types.iter().map(|(n, _)| n.clone()).collect();
    let mut doc = String::new();
    doc.push_str(&format!("# git-lex — {} kit\n\n", kit_short));
    doc.push_str("This repo is managed by [git-lex](https://github.com/repolex-ai/git-lex) — git extensions for knowledge graphs.\n\n");

    doc.push_str("## Quick Start\n\n");
    doc.push_str("```bash\n");
    doc.push_str(&format!("git lex create <type>    # Create a new document (types: {})\n", type_names.join(", ")));
    doc.push_str("git lex save \"message\"   # Add + commit (extracts automatically)\n");
    doc.push_str("git lex sync              # Build/update the knowledge graph\n");
    doc.push_str("git lex query \"SPARQL...\" # Query the knowledge graph\n");
    doc.push_str("```\n\n");

    doc.push_str("## Commands\n\n");
    doc.push_str("| Command | What it does |\n");
    doc.push_str("|---|---|\n");
    doc.push_str(&format!("| `git lex create <type>` | Scaffold a new document. Valid types: {} |\n", type_names.join(", ")));
    doc.push_str("| `git lex save \"msg\"` | Stage all changes, commit, extract frontmatter |\n");
    doc.push_str("| `git lex sync` | Build/update the knowledge graph from the commit history |\n");
    doc.push_str("| `git lex query \"...\"` | SPARQL over the working tree (current files + git layer) |\n");
    doc.push_str("| `git lex serve viz` | Local web view: activity, graph, history replay |\n");
    doc.push_str("| `git lex serve sparql` | SPARQL endpoint over the synced store (history queries) |\n\n");

    doc.push_str("## Writing Documents\n\n");
    doc.push_str("Documents use YAML frontmatter with flat dot notation: `kit.class.property`\n\n");
    doc.push_str("```yaml\n");
    doc.push_str("---\n");
    let example_type = type_names.first().map(|s| s.as_str()).unwrap_or("Class");
    doc.push_str(&format!("{}.{}.<property>: \"value\"\n", kit_short, example_type));
    doc.push_str(&format!("# class names are case-sensitive: {}.{}. — see the __{}.md template\n", kit_short, example_type, example_type));
    doc.push_str("---\n\n");
    doc.push_str("Your content here. Link other documents with standard markdown links.\n");
    doc.push_str("```\n\n");
    doc.push_str("See `__ClassName.md` files in each folder for available properties and SHACL-derived constraints.\n\n");

    doc.push_str("## Linking documents\n\n");
    doc.push_str("Use standard markdown links with root-relative paths:\n\n");
    doc.push_str("- `[display text](/Soul/Note/some-doc.md)` — creates a `linksTo` relationship to that document\n");
    doc.push_str("- links resolve from the repository root; they survive the linking file moving\n\n");
    doc.push_str("git-lex does not read `[[wikilinks]]` — the only place that notation appears is\n");
    doc.push_str("Claude Code's own memory files under `Harness/Memory/`, where it is the\n");
    doc.push_str("harness's private note-taking shorthand and is never resolved by any tool.\n\n");

    // Kit-specific section
    doc.push_str(&format!("## {} Kit — Document Types\n\n", kit_short));
    for (type_name, properties) in kit_types {
        doc.push_str(&format!("### {}\n\n", type_name));
        doc.push_str(&format!("Create: `git lex create {}`\n\n", type_name));
        if !properties.is_empty() {
            let has_comments = properties.iter().any(|(_, _, _, c)| !c.is_empty());
            if has_comments {
                doc.push_str("| Property | Type | Description |\n");
                doc.push_str("|---|---|---|\n");
            } else {
                doc.push_str("| Property | Type |\n");
                doc.push_str("|---|---|\n");
            }
            for (prop_name, prop_type, _, comment) in properties {
                if comment.is_empty() {
                    doc.push_str(&format!("| {} | {} |\n", prop_name, prop_type));
                } else {
                    doc.push_str(&format!("| {} | {} | {} |\n", prop_name, prop_type, comment));
                }
            }
            doc.push_str("\n");
        }
    }

    doc.push_str("## Querying\n\n");
    doc.push_str("Auto-injected prefixes: `git-lex:`, `git2:`, `md:`, `fm:`");
    if kit_short != "none" {
        doc.push_str(&format!(", `{}:`", kit_short));
    }
    doc.push_str("\n\n");
    doc.push_str("```sparql\n");
    doc.push_str("# List all documents by type\n");
    doc.push_str("SELECT ?doc ?type WHERE { ?doc a ?type } LIMIT 20\n");
    doc.push_str("```\n");
    doc
}

/// init step: commit the .lex/ setup files, then (brand-new repos only)
/// offer to commit pre-existing content.
fn commit_setup_and_content() {
    let has_commits = Command::new("git").args(["rev-parse", "HEAD"]).output()
        .map(|o| o.status.success()).unwrap_or(false);

    let _ = Command::new("git").args(["add", ".lex/"]).status();
    if Command::new("git").args(["commit", "-m", "git lex init"]).output()
        .map(|o| o.status.success()).unwrap_or(false) {
        println!("\nCommitted git-lex setup files.");
    }

    // Offer to commit existing content (only for brand-new repos)
    if !has_commits {
        let untracked = Command::new("git").args(["status", "--porcelain"]).output().ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        if !untracked.is_empty() {
            eprint!("Commit existing files to the repository? [Y/n] ");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).unwrap_or_default();
            let input = input.trim().to_lowercase();
            if input.is_empty() || input == "y" || input == "yes" {
                let _ = Command::new("git").args(["add", "."]).status();
                let committed = Command::new("git").args(["commit", "-m", "Initial content"]).status()
                    .map(|s| s.success()).unwrap_or(false);
                if committed {
                    println!("Committed existing content.");
                } else {
                    eprintln!("warning: initial content commit failed — see git output above");
                }
            }
        }
    }
}

/// init step: genesis identity. Records genesis_sha in repo.yml through
/// git.rs, the ONE authority (review #10 — the inline copy this replaces
/// broke multi-root repos into invalid YAML that warn-discarded the whole
/// repo.yml), fills soul.Soul.soulId in SOUL.md (#29), and commits both
/// as "git lex identity" — loudly on failure (#50): identity is the
/// anchor engines join on.
fn record_identity(root: &std::path::Path, repo_yml_path: &std::path::Path) {
    let first_sha = crate::git::genesis_sha().unwrap_or_default();

    if !first_sha.is_empty() {
        let existing = fs::read_to_string(repo_yml_path).unwrap_or_default();
        let mut identity_paths: Vec<&str> = Vec::new();
        if !existing.contains("genesis_sha:") && !existing.contains("first_commit:") {
            if let Err(e) = crate::git::ensure_repo_yml_genesis(&first_sha) {
                eprintln!("fatal: could not record genesis_sha identity in .lex/repo.yml: {}", e);
                exit(1);
            }
            identity_paths.push(".lex/repo.yml");
            println!("Identity: {}", first_sha);
        }
        // Fill soul.Soul.soulId in the freshly installed root SOUL.md from
        // the same genesis sha (#29 — the kit template ships the key empty
        // and declares this fill as git-lex's job).
        match crate::soul_md::heal_soul_id(root) {
            crate::soul_md::HealOutcome::Filled
            | crate::soul_md::HealOutcome::Healed { .. } => identity_paths.push("SOUL.md"),
            _ => {}
        }
        if !identity_paths.is_empty() {
            for p in &identity_paths {
                let _ = Command::new("git").args(["add", p]).status();
            }
            // Identity is the anchor engines (Pool) join on — a failed
            // commit here must not exit 0 silently (review #50). The data
            // survives on disk and sync/kit-update self-heal the content,
            // but the user has to know it's riding uncommitted.
            let committed = Command::new("git")
                .args(["commit", "-m", "git lex identity"])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !committed {
                eprintln!(
                    "warning: identity commit failed — genesis_sha/soulId are on disk \
                     but UNCOMMITTED (see git output above). Commit .lex/repo.yml and \
                     SOUL.md yourself, or the identity rides into your next commit."
                );
            }
        }
    }
}
