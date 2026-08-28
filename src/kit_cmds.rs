//! Kit lifecycle commands: `kit-update`, `kit-add`, `kit-remove`.
//!
//! Command-level orchestration over the kit internals in `crate::kit`:
//! fetching kits, converging scaffold files and hooks, regenerating derived
//! artifacts (SHACL shapes, class templates, folder audit), and keeping the
//! engine runtime dirs (`ENGINE_GITIGNORE_DIRS`) gitignored.

use std::fs;
use std::path::Path;
use std::process::{Command, exit};

use git_lex::{find_git_root, resolve_kit_spec};

use crate::harness;
use crate::hooks;
use crate::kit::{append_optional_kit, fetch_and_validate_optional_kit, fetch_kit_from_github,
                 installed_kit_sha, record_kit_sha, remote_kit_sha, short_sha,
                 install_scaffold_files_from_skip_existing, kit_config_str,
                 read_repo_yml_optional_kits, remove_kit_install_dir, remove_optional_kit,
                 KitFetchOutcome};
use crate::ontology::{self, get_kit_prefix_name, get_kit_types};
use crate::shacl::{build_shacl_shapes, parse_shacl_hints};
use crate::{open_or_create_store, require_git_root, BASE_KIT};

// ─── kit-update ────────────────────────────────────────────────


/// Fetch a single kit into its install dir. Caller decides whether to
/// remove-and-replace (cleanest for update) or skip-if-present.
/// Returns true on success.
fn fetch_kit_for_update(kit_spec: &str) -> bool {
    let root = match find_git_root() {
        Some(r) => r,
        None => return false,
    };
    let (org, repo, _) = resolve_kit_spec(kit_spec);
    let kit_dir = root.join(".lex").join("kit").join(&org).join(&repo);
    let _ = fs::remove_dir_all(&kit_dir);
    if fs::create_dir_all(&kit_dir).is_err() {
        return false;
    }
    fetch_kit_from_github(kit_spec, &kit_dir)
}

/// Regenerate one kit's derived artifacts: SHACL shapes, class folders +
/// __ClassName.md templates, and the folder-vs-ontology audit.
///
/// Used by both `cmd_kit_update` (in a loop over all kits) and
/// `cmd_kit_add` (single-kit). Stays silent if the kit has no types.
fn regenerate_kit_artifacts(kit_name: &str, root: &std::path::Path, create_folders: bool) {
    match build_shacl_shapes(kit_name) {
        Ok(Some(shapes_path)) => println!("  SHACL shapes regenerated: {}",
            shapes_path.file_name().unwrap_or_default().to_string_lossy()),
        Ok(None) => {} // kit ships no ontology — nothing to regenerate
        Err(e) => {
            eprintln!("fatal: SHACL shapes generation failed for '{}': {}", kit_name, e);
            eprintln!("       a broken kit ontology must not install silently — fix the kit TTL and re-run");
            exit(1);
        }
    }

    let templates_updated = emit_class_templates(kit_name, root, create_folders);

    // Folder audit — only meaningful when the kit declares a folder_base.
    let kit_types = get_kit_types(kit_name);
    let folder_base = kit_config_str(kit_name, "folder base");
    if let Some(ref base) = folder_base {
        let declared_all: std::collections::HashSet<String> =
            kit_types.iter().map(|(name, _)| name.clone()).collect();
        // The folder contract is `git-lex:foldered AND NOT owl:deprecated`
        // (#74) — the SAME shared predicate emit_class_templates applies
        // (ontology::class_gets_folder; the two gates once computed it
        // separately and could drift). Unfoldered classes are graph-only by
        // design, and a deprecated class keeps resolving (history replays)
        // but must not demand its folder back: creating one would invite
        // new writing into retired vocabulary. Auditing "every known class"
        // instead printed phantom missing-folder lines fleet-wide after the
        // soul 0.9.x deprecation appendix.
        let expected: std::collections::HashSet<String> = declared_all
            .iter()
            .filter(|n| ontology::class_gets_folder(kit_name, n))
            .cloned()
            .collect();
        let base_dir = root.join(base);

        let mut missing = Vec::new();
        for name in &expected {
            if !base_dir.join(name).exists() {
                missing.push(name.clone());
            }
        }
        let mut extra = Vec::new();
        if let Ok(entries) = fs::read_dir(&base_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().to_string();
                // Extra = a folder no declared class explains. A folder for
                // a deprecated or unfoldered class is NOT extra — it's legal
                // residue awaiting owner-paced evacuation, or the owner's
                // choice to keep foldering a graph-only class.
                if entry.path().is_dir() && !declared_all.contains(&name) {
                    extra.push(name);
                }
            }
        }
        if !missing.is_empty() {
            eprintln!("  ⚠ Missing folders (in ontology but not on disk): {}", missing.join(", "));
        }
        // Reap-when-empty (Rob-ruled 2026-07-30): an extra folder holding
        // nothing but kit scaffold debris (`__Name.md` template + `.gitkeep`)
        // is retired-class residue from a kit release that removed or moved
        // the class. Reap it. A folder with ANY real content stays put and
        // keeps its warning — content migration is per-repo, never automatic.
        let extra = if extra.is_empty() { extra } else {
            let declared = folders_declared_by_installed_kits(root);
            let mut kept = Vec::new();
            for name in extra {
                let dir = base_dir.join(&name);
                if !declared.contains(&dir)
                    && folder_is_scaffold_only(&dir, &name)
                    && fs::remove_dir_all(&dir).is_ok()
                {
                    println!("  Reaped retired folder (scaffold only): {}/{}", base, name);
                } else {
                    kept.push(name);
                }
            }
            kept
        };
        if !extra.is_empty() {
            eprintln!("  ⚠ Extra folders (on disk but not in ontology): {}", extra.join(", "));
        }
        if missing.is_empty() && extra.is_empty() && !expected.is_empty() {
            println!("  Folders: {}/{} match ontology ✓", expected.len(), expected.len());
        }

        // Residue receipt (tr1p's ask, Rob-approved 2026-08-08): name the
        // deprecated-class folders still on disk, so retired residue stops
        // being detectable only by a human reading the repo — quiet reads
        // as clean. Informational ONLY: no deletion, no prompt (content
        // evacuation is the owner's, at the owner's pace).
        //
        // Three safety constraints, per the w3blord `skill/` near-miss (a
        // repo-root lowercase dir full of live symlinked skill packages
        // that LOOKED like residue): the candidate set is EXACTLY the
        // `owl:deprecated` class names declared in the loaded ontology,
        // matched exact-case — never "looks like a class folder"; only
        // `<folder_base>/<Name>/` is eligible — the filesystem knows
        // nothing, only the ontology knows which names are dead; and a
        // candidate that is (or contains) a SYMLINK is never DESCRIBED —
        // these repos carry live symlinked infrastructure, and a walker
        // that doesn't check will eventually be confidently wrong about
        // someone's working setup. The skip itself is COUNTED, though
        // (tr1p's refinement): silence about the content, but not silence
        // about the existence of doubt — an unnamed blind spot becomes a
        // surprise later, and the owner's eyes stay the fallback detector
        // exactly where the tool has chosen not to look.
        let deprecated = ontology::get_deprecated_classes(kit_name);
        let mut residue: Vec<String> = Vec::new();
        let mut skipped_symlink = 0usize;
        // BTreeMap iteration → stable alphabetical order in the receipt.
        for (name, replaced_by) in deprecated.iter().collect::<std::collections::BTreeMap<_, _>>() {
            let dir = base_dir.join(name);
            // symlink_metadata first: is_dir() would FOLLOW a symlink.
            let Ok(meta) = fs::symlink_metadata(&dir) else { continue };
            if meta.file_type().is_symlink() || (meta.is_dir() && tree_has_symlink(&dir)) {
                skipped_symlink += 1;
                continue;
            }
            if !meta.is_dir() {
                continue; // a plain file where a folder would be — not a folder
            }
            residue.push(match replaced_by {
                Some(r) => format!("{} (replaced by {})", name, r),
                None => name.clone(),
            });
        }
        if !residue.is_empty() {
            let skip_note = if skipped_symlink > 0 {
                format!(
                    " ({} candidate(s) skipped: symlink inside — the tool asserts \
                     nothing about symlinked trees; check those by eye)",
                    skipped_symlink
                )
            } else {
                String::new()
            };
            println!(
                "  Retired-class folders present: {} — deprecated classes; their \
                 content awaits your evacuation, at your pace (git-lex never \
                 deletes content folders).{}",
                residue.join(", "),
                skip_note
            );
        } else if skipped_symlink > 0 {
            println!(
                "  Retired-class folder check: {} candidate(s) skipped (symlink \
                 inside) — the tool asserts nothing about symlinked trees; check \
                 those by eye if you expect residue.",
                skipped_symlink
            );
        }
    }

    if templates_updated > 0 {
        println!("  {} class template(s) regenerated.", templates_updated);
    }
}

/// True when `dir` itself, or ANYTHING beneath it, is a symlink — or when
/// any of it can't be read (doubt = true: the residue receipt says nothing
/// rather than risk describing live symlinked infrastructure as residue).
fn tree_has_symlink(dir: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(dir) else { return true };
    if meta.file_type().is_symlink() {
        return true;
    }
    if meta.is_dir() {
        let Ok(entries) = fs::read_dir(dir) else { return true };
        for e in entries.filter_map(|e| e.ok()) {
            if tree_has_symlink(&e.path()) {
                return true;
            }
        }
    }
    false
}

/// True when `dir` contains nothing except its own class template
/// (`__{class_name}.md`) and/or `.gitkeep` — i.e. pure kit scaffold with no
/// agent content. Subdirectories or any other file make it NOT scaffold-only.
fn folder_is_scaffold_only(dir: &Path, class_name: &str) -> bool {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return false,
    };
    let template = format!("__{}.md", class_name);
    for entry in entries.filter_map(|e| e.ok()) {
        if entry.path().is_dir() {
            return false;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name != template && name != ".gitkeep" {
            return false;
        }
    }
    true
}

/// Every folder path any installed kit declares as a foldered class. Guard
/// for the retired-folder reap: a folder that looks "extra" to the kit being
/// audited may legitimately belong to another installed kit sharing the same
/// folder base — those must never be reaped.
fn folders_declared_by_installed_kits(root: &Path) -> std::collections::HashSet<std::path::PathBuf> {
    let mut declared = std::collections::HashSet::new();
    for kit in collect_kits_for_update(root, None) {
        let base = kit_config_str(&kit, "folder base");
        for (type_name, _) in &get_kit_types(&kit) {
            if !ontology::class_gets_folder(&kit, type_name) {
                continue;
            }
            let dir = match &base {
                Some(b) => root.join(b).join(type_name),
                None => root.join(type_name),
            };
            declared.insert(dir);
        }
    }
    declared
}

/// Emit the `__ClassName.md` frontmatter template for every foldered class of
/// a kit. This is the CANONICAL template emitter — `git lex init`, `kit-add`
/// and `kit-update` all converge templates through it, so template output is
/// identical no matter which command wrote it. Returns the number of
/// templates written.
pub(crate) fn emit_class_templates(kit_name: &str, root: &std::path::Path, create_folders: bool) -> usize {
    let (_, _, short) = resolve_kit_spec(kit_name);

    let kit_types = get_kit_types(kit_name);
    let shapes_content = {
        let shapes_p = root.join(".lex").join("ontology").join(&short)
            .join(format!("{}-shapes.ttl", short));
        fs::read_to_string(&shapes_p).unwrap_or_default()
    };
    let shacl_hints = parse_shacl_hints(&shapes_content, &short);
    let prefix_name = get_kit_prefix_name(&short);

    let folder_base = kit_config_str(kit_name, "folder base");
    let mut templates_updated = 0usize;
    for (type_name, properties) in &kit_types {
        // Foldered gate — the shared predicate (foldered AND NOT
        // deprecated, #74). This emitter once tested `foldered` alone and
        // agreed with the audit only by the kit author's habit of never
        // leaving `foldered` on a deprecated class; one slip would have
        // rebuilt the retired folder AND written a fresh template into it
        // on every seat at kit-update (tr1p's §2j filing). The quiet
        // default stays graph-only, so vocabulary classes never litter
        // empty folders.
        if !ontology::class_gets_folder(kit_name, type_name) {
            continue;
        }

        let type_dir = if let Some(ref base) = folder_base {
            root.join(base).join(type_name)
        } else {
            root.join(type_name)
        };
        // Create the folder if (a) caller wants it (kit-add / kit-update) and
        // (b) it's missing. Templates land in here either way.
        if create_folders {
            fs::create_dir_all(&type_dir).ok();
            let gitkeep = type_dir.join(".gitkeep");
            if !gitkeep.exists() {
                fs::write(&gitkeep, "").ok();
            }
        } else if !type_dir.exists() {
            // No folder + no create → skip template emit so we don't litter
            // a __ClassName.md in a parent that doesn't have the kit folder.
            continue;
        }
        let template_path = type_dir.join(format!("__{}.md", type_name));

        let mut tmpl = String::new();
        tmpl.push_str("---\n");
        // How to write more than one value, on the surface people copy from
        // (#101). Same argument the type words made in #100: the template
        // gets read far more often than the ontology does.
        tmpl.push_str(&git_lex::multivalue_teaching_block(&short, type_name));
        // ONE LINE PER EMITTED KEY, not per source property.
        //
        // FLEET-STOPPING, found by @th34 and @w4r3z-pool within minutes of the
        // 2026-08-28 release: __ScenarioTake.md carried FIVE identical
        // `copia.ScenarioTake.relatedToId` lines and __Shotlist.md two, so the
        // duplicate-key gate rejected them — and they are kit-managed files a
        // soul never wrote and is told not to edit. The gate is repo-wide, so a
        // template for a class nobody uses blocked EVERY save in the repo,
        // including the .id backfill the release had just ordered.
        //
        // The file violated the rule printed in its own header eight lines
        // above the offence: "Write the key once."
        //
        // Several source properties now legitimately map onto one key —
        // ScenarioTake's four deleted twins all collapse to relatedToId, and
        // its four owl:onClass restrictions each produce their own sh:property
        // on that same path. Both are correct upstream; emitting one template
        // line each is not.
        let mut emitted_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (prop_name, prop_type, _required, _comment) in properties {
            let key = format!("{}.{}.{}", short, type_name, prop_name);
            if !emitted_keys.insert(key.clone()) {
                continue;
            }
            let hint = shacl_hints.get(&format!("{}:{}", prefix_name, prop_name));
            let comment = match hint {
                Some(h) => format!(" # {}", h),
                None => match prop_type.as_str() {
                    "reference" => " # IRI — repo-relative path or full IRI".to_string(),
                    _ => String::new(),
                },
            };
            tmpl.push_str(&format!("{}: {}\n", key, comment.trim_start()));
        }
        tmpl.push_str("---\n");
        // The class's authoringGuidance, as the template's BODY. This is the
        // surface that matters most: an agent writing a document by hand —
        // no `git lex create` in the loop — has this template and nothing
        // else. Safe to carry markdown: `__` templates are never walked
        // (nquad::is_template), so nothing here can reach the graph.
        if let Some(guidance) = ontology::get_class_authoring(kit_name, type_name).guidance {
            let guidance = guidance.trim();
            if !guidance.is_empty() {
                tmpl.push('\n');
                tmpl.push_str(guidance);
                tmpl.push('\n');
            }
        }
        // Count only what actually landed (review #54): a swallowed write
        // still counted, so "N template(s) regenerated" could be a lie —
        // leaving a stale __Class.md that scaffolds docs with retired keys.
        match fs::write(&template_path, &tmpl) {
            Ok(()) => templates_updated += 1,
            Err(e) => eprintln!(
                "warning: could not write class template {} ({e}) — the on-disk \
                 template is STALE; fix the error and re-run `git lex kit-update`",
                template_path.display()
            ),
        }
    }

    templates_updated
}

/// Build the ordered list of kits a `kit-update` should refresh.
/// Order matters: base first (carries shared scaffold/ontology), then
/// domain, then optionals (alphabetical for determinism in output).
///
/// If `target` is provided, returns only that one kit (still validated
/// against installed-kit list — refuses to update a kit that isn't here).
pub(crate) fn collect_kits_for_update(root: &std::path::Path, target: Option<&str>) -> Vec<String> {
    let mut all = vec![BASE_KIT.to_string()];
    if let Some(domain) = git_lex::RepoYml::load(root).domain_kit() {
        if domain != BASE_KIT { all.push(domain); }
    }
    let mut optionals = read_repo_yml_optional_kits(&root.join(".lex").join("repo.yml"));
    optionals.sort();
    optionals.dedup();
    for o in optionals {
        if !all.contains(&o) { all.push(o); }
    }
    match target {
        None => all,
        Some(t) => {
            // Exact match against installed list. Allow short or long form by
            // resolving both sides to canonical (org, repo) tuples.
            let (t_org, t_repo, _) = resolve_kit_spec(t);
            let matched: Vec<String> = all.into_iter()
                .filter(|k| {
                    let (o, r, _) = resolve_kit_spec(k);
                    o == t_org && r == t_repo
                })
                .collect();
            if matched.is_empty() {
                eprintln!("Kit '{}' is not installed in this repo. Use `git lex kit-add` first.", t);
                exit(1);
            }
            matched
        }
    }
}

pub(crate) fn cmd_kit_update(kit_arg: Option<String>) {
    let root = require_git_root();
    let lex_dir = root.join(".lex");

    if !lex_dir.exists() {
        eprintln!("Not a git-lex repo. Run 'git lex init' first.");
        exit(1);
    }

    // TWO SCOPES, and they are NOT the same one (#102). Keeping them as two
    // named bindings is the fix: the bug was one list doing both jobs, so a
    // narrowing meant for the network silently narrowed a local rebuild.
    //
    //   FETCH scope — narrowed by the argument. Downloading is per-kit,
    //   remote and expensive, and "just refresh copia" is a reasonable ask.
    //
    //   REBUILD scope — ALWAYS every installed kit, whatever the argument.
    //   Shapes and templates are derived from the installed payloads, and
    //   converge_ontology_mirror below rewrites .lex/ontology/ for EVERY kit
    //   as an exact mirror of its payload — which by design does not contain
    //   the derived -shapes.ttl. So every kit's shapes are deleted on every
    //   run and must be rebuilt on every run. Rebuilding only the fetched kit
    //   left the others with no shapes at all.
    //
    // That was not merely cosmetic. The typed-literal map is parsed out of
    // the generated shape files (ontology.rs get_property_datatypes_all_kits),
    // so a kit with no shapes emits its literals UNTYPED — and an untyped
    // literal does not match the typed one it should replace, so the retract
    // never fires and the property ends up holding two contradictory values.
    // Restoring the shapes does not heal it; only a full store rebuild does.
    //
    // THE RULE: an argument may narrow what you FETCH; it must never narrow
    // what you REBUILD. Same scope leak as #28, which was this command
    // reaping hooks from a partial kit set.
    let kits_to_fetch = collect_kits_for_update(&root, kit_arg.as_deref());
    let all_installed_kits = collect_kits_for_update(&root, None);

    // Fetch every kit fresh. Bail on any fetch failure — partial state is
    // worse than no state, and the only way to fail here is network/auth
    // (since the spec was validated against the installed list).
    // Version receipt. The tarball fetch is an archive of refs/heads/main with
    // no commit id inside it, so "Kit update complete" could report success
    // for months while landing byte-identical content — and no soul could tell
    // the difference from inside their own repo. kira read one such report as
    // evidence her viz fix had arrived; it had not, and there was nothing she
    // could have checked. So: resolve the remote tip BEFORE the fetch, read
    // what was installed BEFORE the wipe, and say plainly which of the three
    // things happened.
    let mut already_current = 0usize;
    for spec in &kits_to_fetch {
        let (org, repo, _) = resolve_kit_spec(spec);
        let kit_dir = root.join(".lex").join("kit").join(&org).join(&repo);
        let before = installed_kit_sha(&kit_dir);
        let remote = remote_kit_sha(spec);

        println!("Updating kit '{}/{}' from GitHub...", org, repo);
        if !fetch_kit_for_update(spec) {
            eprintln!("Failed to fetch kit '{}' from GitHub.", spec);
            eprintln!("Check network access to https://github.com/{}/{}", org, repo);
            exit(1);
        }

        match (&before, &remote) {
            // Already current is a FIRST-CLASS outcome, not a quieter success
            // (@w3bl0rd's call, and he is right): "nothing changed" is the
            // reading people most need and the one a success message is most
            // likely to swallow.
            (Some(b), Some(r)) if b == r => {
                println!("  already current ({})", short_sha(r));
                already_current += 1;
            }
            (Some(b), Some(r)) => println!("  {} -> {}", short_sha(b), short_sha(r)),
            (None, Some(r)) => println!("  now at {} (no previous version recorded)", short_sha(r)),
            // Unknown is reported as unknown. A fetch that worked while
            // ls-remote did not is still a real fetch, but this run cannot
            // say what landed, and pretending otherwise is the whole defect.
            (_, None) => println!("  fetched, but the version could not be determined \
                                   (could not reach the remote to ask)"),
        }
        if let Some(r) = &remote {
            record_kit_sha(&kit_dir, r);
        }
    }

    // Install each kit's files. Missing → installed; identical → no-op;
    // differing → converged to the kit version (no backup — git history is
    // the backup, Rob-ruled 2026-08-08). SOUL.md is never overwritten.
    let mut total_installed = 0usize;
    let mut total_skipped = 0usize;
    let mut all_updated: Vec<String> = Vec::new();
    let mut total_swept_baks = 0usize;
    // #72: which kit(s) converged each path this run. Two kits shipping the
    // SAME path with different content flip the file back and forth every
    // update — the last kit silently wins by install order, not intent, and
    // the loser's copy churns as <file>.bak forever. Receipt: base shipped
    // an empty .claude/CLAUDE.md while soul ships the real one — every
    // fleet kit-update printed the path twice and re-.bak'd it.
    let mut converged_by: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for spec in &kits_to_fetch {
        let (org, repo, _) = resolve_kit_spec(spec);
        let kit_dir = lex_dir.join("kit").join(&org).join(&repo);
        let report = install_scaffold_files_from_skip_existing(&kit_dir);
        total_installed += report.installed;
        total_skipped += report.skipped;
        total_swept_baks += report.swept_baks;
        for path in &report.updated {
            converged_by.entry(path.clone()).or_default().push(repo.clone());
        }
        all_updated.extend(report.updated);
    }
    if total_swept_baks > 0 {
        println!(
            "Swept {} retired .bak convergence backup(s) — git history is the backup",
            total_swept_baks
        );
    }
    for (path, kits) in &converged_by {
        if kits.len() > 1 {
            eprintln!(
                "⚠ kit CONFLICT: {} is shipped by {} — each kit-update rewrites it {} times \
                 and `{}` wins only by install order. This is a kit-lane bug: exactly one \
                 kit should ship this file. Report it to the kit owner.",
                path,
                kits.join(" AND "),
                kits.len(),
                kits.last().map(String::as_str).unwrap_or("?")
            );
        }
    }

    // Stored-query starters: create .lex/query/ with the defaults if this
    // repo has never had one (folder exists = operator's, untouched).
    crate::query::scaffold_default_queries(&root);

    // File-level hook reap (twin of the registration reap) — extracted
    // step, see fn doc.
    reap_stale_hooks(&root);

    // The .kit-latest drift-sidecar mechanism is retired; sweep any leftovers.
    let swept = crate::kit::sweep_kit_latest_files(&root);
    if !swept.is_empty() {
        println!("Swept {} leftover .kit-latest file(s) (retired mechanism)", swept.len());
    }

    if total_installed > 0 || total_skipped > 0 || !all_updated.is_empty() {
        println!("Scaffold: {} file(s) installed, {} unchanged", total_installed, total_skipped);
        if !all_updated.is_empty() {
            println!(
                "Updated {} file(s) to the kit's version:",
                all_updated.len()
            );
            for path in &all_updated {
                println!("  {}", path);
            }
        }
    }

    // Refresh substrate identity for every active substrate. Identity is
    // per-repo, not per-kit, so this runs once after all kit scaffolds are
    // in place. Each substrate gets its own injection pass.
    //
    // This pass is what registers hooks + writes the identity env block into
    // settings.json — through the ONE shared gate (review #11), whose
    // no-agent_name branch is LOUD (a silent skip of the thing that makes
    // hooks FIRE is exactly the R11 ghost; found by w3bl0rd's flinch-audit
    // on the convergence rollout, Day 50 — #67).
    harness::run_substrate_setup(&root, None);

    // Converge the managed-by header onto .lex/repo.yml (Rob-ruled
    // 2026-08-22). Runs here because kit-update runs at every compaction,
    // so the existing fleet grows the header with no one doing anything.
    // Idempotent; a failure is a warning, never a blocked update — a
    // missing comment is a teaching gap, not a broken repo.
    if let Err(e) = crate::git::ensure_repo_yml_header(&root) {
        eprintln!("warning: could not add the managed-by header to .lex/repo.yml: {e}");
    }

    // Retired-mechanism sweeps (legacy .env, ontology/kit/ tier,
    // link_semantics key) — extracted step, see fn doc.
    sweep_legacy_layouts(&root);

    // #71: converge .lex/ontology/ to exactly what the installed kits
    // own — extracted step, see fn doc.
    converge_ontology_mirror(&root);

    // Converge the engine runtime-dir gitignore on every existing soul. Souls
    // that predate the `.pool/`/`.copia/`/`.weave/` standard hand-wrote their
    // .gitignore and never got these lines — so their engine index stores leaked
    // into git (lUX: 155 .pool/ files; W4R3Z: 11 Pool/oxigraph/). Idempotent: adds
    // the sentinel block once, reports already-tracked files that now match so the
    // soul can `git rm --cached` them deliberately (never auto-mutates the index).
    ensure_engine_gitignore(&root);

    // Regenerate derived artifacts (shapes, class templates, folder audit)
    // for EVERY installed kit — never just the fetched one (#102). The
    // mirror above deleted every kit's shapes; whatever is not rebuilt here
    // stays deleted. Base first, same ordering rule as the fetch list.
    for spec in &all_installed_kits {
        let (org, repo, _) = resolve_kit_spec(spec);
        println!("Regenerating artifacts for '{}/{}'...", org, repo);
        regenerate_kit_artifacts(spec, &root, true);
    }

    // Converge the git pre-commit hook to the current managed section.
    // Old repos carry pre-marker-era hooks calling removed subcommands
    // (`git-lex extract`/`validate`) — without this, every save breaks
    // after a binary upgrade until the hook is refreshed by hand.
    match hooks::install_hook() {
        Ok(()) => println!("Pre-commit hook: converged to current version."),
        Err(e) => {
            eprintln!("ERROR: could not refresh the pre-commit hook: {e}");
            eprintln!("`git lex save` may fail until the hook is fixed.");
            exit(1);
        }
    }

    // Identity floor: self-heal soul.Soul.soulId in the root SOUL.md from
    // the genesis sha (#29 — fills a missing/empty value AND corrects a
    // wrong one; the receipt prints inside heal_soul_id). A missing
    // SOUL.md was just restored by the scaffold install above; if it's
    // STILL absent something upstream is broken — say so rather than
    // letting the next save fail-loud without context.
    if let crate::soul_md::HealOutcome::NoSoulMd = crate::soul_md::heal_soul_id(&root) {
        eprintln!("warning: root SOUL.md is missing and the kit scaffold did not restore it —");
        eprintln!("`git lex sync`/`save` will refuse to run until it exists.");
    }

    if already_current > 0 {
        println!("{} of {} kit(s) were already current — nothing changed for those.",
            already_current, kits_to_fetch.len());
    }
    println!("Kit update complete: {} kit(s) fetched, {} kit(s) rebuilt.",
             kits_to_fetch.len(), all_installed_kits.len());

    // t-box refresh: kit vocab may have changed.
    reload_ontology_graph();
}

/// kit-update step: file-level hook reap (twin of the registration reap).
/// The keep-set MUST come from ALL installed kits — not just the kits
/// being updated — or a single-kit `kit-update soul` would reap every
/// other kit's hooks (the live incident of 2026-07-30: pool's hooks
/// .bak'd and deregistered). Non-updated kits weren't re-fetched, but
/// their install dirs are already on disk from their own install. If any
/// installed kit's dir is missing we can't know its hook set, so the reap
/// is SKIPPED (loudly) rather than guessed. Removed files stay beside as
/// `<file>.bak` — a reaped personal hook may be uncommitted user work,
/// the one case git history cannot cover — and their now-dangling
/// settings.json registrations get pruned by the substrate-setup pass
/// (the file is gone → its registration reaps).
fn reap_stale_hooks(root: &Path) {
    let lex_dir = root.join(".lex");
    let mut kit_hook_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut reap_safe = true;
    for spec in collect_kits_for_update(root, None) {
        let (org, repo, _) = resolve_kit_spec(&spec);
        let kit_dir = lex_dir.join("kit").join(&org).join(&repo);
        if !kit_dir.exists() {
            eprintln!(
                "  ⚠ Kit '{}' has no install dir ({}); skipping the hook reap this run \
                 (can't know which hooks it ships). Run a full `git lex kit-update` to repair.",
                spec,
                kit_dir.display()
            );
            reap_safe = false;
            continue;
        }
        for name in crate::kit::kit_shipped_hook_names(&kit_dir) {
            kit_hook_names.insert(name);
        }
    }
    // Remove any .claude/hooks/*.sh that is neither kit-shipped nor a
    // `<Event>-local-*.sh` personal hook — this kills old-named hooks left
    // behind by a rename (the exact tangle a migrating soul hits: old + new
    // both present + firing).
    if reap_safe {
        let reaped_hooks = crate::kit::reap_non_kit_non_local_hooks(root, &kit_hook_names);
        if !reaped_hooks.is_empty() {
            println!(
                "Removed {} hook file(s) no installed kit ships (old copy kept as <file>.bak):",
                reaped_hooks.len()
            );
            for path in &reaped_hooks {
                println!("  {}", path);
            }
        }
    }
}

/// kit-update step: sweep retired-mechanism residue. Three lanes, all
/// self-healing no-ops once clean:
/// - legacy `.env` (identity now lives in .claude/settings.json; the .env
///   path silently WON over settings.json when both existed);
/// - legacy `.lex/ontology/kit/` tier (pre-multi-kit layout whose stale
///   shapes used to shadow canonical ones via the old glob-walk — task
///   #29; the resolver is canonical-path-based now, fossils just confuse);
/// - the retired `link_semantics:` repo.yml key (ONE link law, Rob-ruled
///   2026-08-08 — inert config left lying around reads as meaning
///   something).
fn sweep_legacy_layouts(root: &Path) {
    let legacy_env = root.join(".env");
    if legacy_env.exists() {
        if fs::remove_file(&legacy_env).is_ok() {
            println!("Removed legacy .env — identity now lives in .claude/settings.json");
        }
    }

    let legacy_ontology = root.join(".lex").join("ontology").join("kit");
    if legacy_ontology.exists() {
        if fs::remove_dir_all(&legacy_ontology).is_ok() {
            println!("Removed legacy .lex/ontology/kit/ — shapes now resolve via canonical .lex/ontology/<short>/ path");
        }
    }

    let ryml = root.join(".lex").join("repo.yml");
    if let Ok(content) = fs::read_to_string(&ryml) {
        if content.lines().any(|l| l.trim_start().starts_with("link_semantics:")) {
            let cleaned: String = content
                .lines()
                .filter(|l| !l.trim_start().starts_with("link_semantics:"))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            if fs::write(&ryml, cleaned).is_ok() {
                println!("Removed retired `link_semantics:` key from .lex/repo.yml (one link law)");
            }
        }
    }
}

/// #71 (kit-update step): converge .lex/ontology/ to exactly what the
/// Kit installs copy additively and never delete, so every layout era
/// left fossils behind (top-level fm/ git/ lex/ lex-o/, retired optional
/// kits, nested git-lex/git2/) — and the ontology loader walks the WHOLE
/// tree, so stale vocabulary from retired eras kept loading into the
/// ontology graph fleet-wide (lupov's find). Ownership derives from the
/// installed kit payloads (.lex/kit/<org>/<repo>/ontology/<short>/):
/// unowned top-level dirs are reaped, owned dirs converge to an exact
/// MIRROR of their payload so nested fossils die too. The -shapes.ttl
/// derived artifacts are regenerated just below, so the mirror losing
/// them is part of the design, not a casualty.
fn converge_ontology_mirror(root: &Path) {
    // Every error propagates (review #22): this copier rebuilds a dir
    // the sweep deletes, so a swallowed failure guts the live ontology
    // and the loader then reads "kit ships no ontology" — validation
    // silently skipped, object properties emitted as string literals.
    fn copy_dir_recursive(
        src: &std::path::Path,
        dest: &std::path::Path,
    ) -> std::io::Result<()> {
        fs::create_dir_all(dest)?;
        for e in fs::read_dir(src)? {
            let e = e?;
            let d = dest.join(e.file_name());
            if e.path().is_dir() {
                copy_dir_recursive(&e.path(), &d)?;
            } else {
                fs::copy(e.path(), &d)?;
            }
        }
        Ok(())
    }
    let kit_root = root.join(".lex").join("kit");
    let mut payload_dirs: Vec<(String, std::path::PathBuf)> = Vec::new();
    if let Ok(orgs) = fs::read_dir(&kit_root) {
        for org in orgs.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
            if let Ok(repos) = fs::read_dir(org.path()) {
                for repo in repos.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
                    let ont = repo.path().join("ontology");
                    if let Ok(shorts) = fs::read_dir(&ont) {
                        for s in shorts.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
                            payload_dirs.push((
                                s.file_name().to_string_lossy().to_string(),
                                s.path(),
                            ));
                        }
                    }
                }
            }
        }
    }
    // Fail-safe: an empty payload set means the kit tree is in a state
    // this sweep can't reason about — touch nothing.
    if !payload_dirs.is_empty() {
        let owned: std::collections::HashSet<&String> =
            payload_dirs.iter().map(|(n, _)| n).collect();
        let ont_root = root.join(".lex").join("ontology");
        if let Ok(entries) = fs::read_dir(&ont_root) {
            for e in entries.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
                let name = e.file_name().to_string_lossy().to_string();
                if !owned.contains(&name) && fs::remove_dir_all(e.path()).is_ok() {
                    println!(
                        "Reaped orphaned ontology dir (no installed kit owns it): .lex/ontology/{}",
                        name
                    );
                }
            }
        }
        for (name, src) in &payload_dirs {
            let dest = ont_root.join(name);
            // Build the mirror BESIDE the live dir, then swap (review
            // #22). The old order — delete live, then copy with
            // swallowed errors — meant one failed copy left
            // .lex/ontology/<short>/ empty while kit-update reported
            // success, and that path is the documented contract external
            // consumers (Pool) read alone. Now a failure leaves the live
            // dir untouched and says so.
            let tmp = ont_root.join(format!(".{}.kit-tmp", name));
            let _ = fs::remove_dir_all(&tmp);
            if let Err(e) = copy_dir_recursive(src, &tmp) {
                eprintln!(
                    "ERROR: building the ontology mirror for `{name}` failed ({e}) — \
                     the live .lex/ontology/{name}/ is UNTOUCHED. Fix the error \
                     (disk/permissions) and re-run `git lex kit-update`."
                );
                let _ = fs::remove_dir_all(&tmp);
                continue;
            }
            if dest.exists() {
                if let Err(e) = fs::remove_dir_all(&dest) {
                    eprintln!(
                        "ERROR: could not clear .lex/ontology/{name}/ for convergence \
                         ({e}) — left as-is; re-run `git lex kit-update` after fixing."
                    );
                    let _ = fs::remove_dir_all(&tmp);
                    continue;
                }
            }
            if let Err(e) = fs::rename(&tmp, &dest) {
                eprintln!(
                    "ERROR: ontology mirror swap for `{name}` failed ({e}) — \
                     .lex/ontology/{name}/ is MISSING until a re-run of \
                     `git lex kit-update` succeeds. The kit payload copy is intact."
                );
                let _ = fs::remove_dir_all(&tmp);
            }
        }
    }
}

/// t-box reload: installed kit ontologies → the persistent ontology graph
/// (it stays put across syncs). ONE helper for the three lifecycle moments
/// that change kit vocab — init, kit-add, kit-update; the block used to be
/// tripled verbatim (review #11).
pub(crate) fn reload_ontology_graph() {
    let store = open_or_create_store();
    let n = crate::nquad::load_ontology_graph(&store);
    println!("Ontology graph: {} kit ttl file(s) loaded", n);
}

/// The engine runtime dirs every soul must gitignore: the per-soul LOCAL state
/// of the Subtexture engines. These hold index stores, embeddings, HNSW
/// indexes, and media roots — heavy, high-churn, machine-local, never
/// committed. `.weave/` retired 2026-08-04 after a fleet sweep found zero
/// on-disk and zero tracked dirs. Removing a live entry commits someone's
/// store at their next save (th34's day-one repo vacuumed 1.4MB of .ravel/
/// RocksDB before ".ravel/" was here) — sweep before you drop.
///
/// Pocket law (Rob, 2026-08-05; doc:
/// subtexture/docs/stack/2026_08_05_DOTDIR_IGNORE_POCKET.md): in any tool's
/// dotdir, `_ignore/` is machine-local and everything else is committed. An
/// engine's entry converges whole-dir → `<dir>_ignore/` per repo, gated on
/// that engine's KNOWN legacy machine-local paths being gone from outside
/// the pocket — the engine's own data migration is the trigger, so the flip
/// can never make a pre-move store committable (spaceGOAT's inverted-82fe1d7
/// hazard: an 81M transcript tree, one file over GitHub's 100MB cap, one
/// save away from a rejected push). The gate + legacy lists are
/// TRANSITIONAL — they die in ship-prep once every engine has moved.
struct EngineIgnore {
    dir: &'static str,
    /// Known legacy machine-local paths (relative to `dir`) from before the
    /// pocket law. `Some(paths)`: flip to the narrow pocket entry once ALL
    /// are absent. `None`: never flips (whole-dir entry retained).
    legacy: Option<&'static [&'static str]>,
}

const ENGINE_IGNORE: &[EngineIgnore] = &[
    // Rob 2026-08-05: .pool/ absolutely untouched until the Pool conversion
    // ("it's hanging on by a thread").
    EngineIgnore { dir: ".pool/", legacy: None },
    // Legacy path list not yet confirmed by the copia owner — whole-dir
    // until it is.
    EngineIgnore { dir: ".copia/", legacy: None },
    // Horae (nomia's ask, Rob-approved 2026-08-26): live render queue is
    // ~170MB of machine-local sqlite at .horae/sqlite/ — already swept into
    // a lUX commit once and caught. Whole-dir until nomia confirms the
    // legacy path list (queue home + PNG spool), then flips to the pocket
    // like every post-law engine.
    EngineIgnore { dir: ".horae/", legacy: None },
    // spaceGOAT-confirmed complete (disk survey ×3 installs + sync.rs writes
    // only these two): store + transcript mirror.
    EngineIgnore { dir: ".ravel/", legacy: Some(&["oxigraph", "transcripts"]) },
    // Pan adopts the pocket young; defensive single entry.
    EngineIgnore { dir: ".pan/", legacy: Some(&["oxigraph"]) },
];

/// git-lex's own pocket entry. UNCONDITIONAL: the legacy store lived under
/// `.git/lex/`, never loose in `.lex/`, so there is nothing to gate on.
const LEX_POCKET_IGNORE: &str = ".lex/_ignore/";

/// The ignore entries to emit for this repo's ACTUAL layout: git-lex's own
/// pocket first, then each engine at whole-dir or narrow pocket form per the
/// gate above. Stray files outside a pocket after a flip are committable by
/// law and surface LOUD in git status — gating on dir-emptiness instead of
/// the known list would hide them forever (dedup-hides-errors disease).
fn engine_ignore_entries(root: &Path) -> Vec<String> {
    let mut entries = vec![LEX_POCKET_IGNORE.to_string()];
    for e in ENGINE_IGNORE {
        let flipped = match e.legacy {
            None => false,
            Some(paths) => paths.iter().all(|p| !root.join(e.dir).join(p).exists()),
        };
        if flipped {
            entries.push(format!("{}_ignore/", e.dir));
        } else {
            entries.push(e.dir.to_string());
        }
    }
    entries
}

const ENGINE_GITIGNORE_BEGIN: &str = "# >>> git-lex engine runtime (managed) >>>";
const ENGINE_GITIGNORE_END: &str = "# <<< git-lex engine runtime (managed) <<<";

/// Idempotently ensure the soul repo's root `.gitignore` carries the managed
/// engine-runtime entries for this repo's layout (`engine_ignore_entries`).
/// Wrapped in a sentinel block so re-runs replace-in-place (never duplicate);
/// the next `git lex kit-update` re-emits the block, which is also how an
/// engine's whole-dir entry converges to its `_ignore/` pocket form after
/// that engine migrates its data. Reports (does NOT auto-remove) files
/// already tracked that now match, so the soul can `git rm --cached` them
/// deliberately — git-lex never mutates the index on the soul's behalf
/// (Rob's call, Day 51).
pub(crate) fn ensure_engine_gitignore(root: &Path) {
    let gitignore = root.join(".gitignore");
    let existing = fs::read_to_string(&gitignore).unwrap_or_default();

    // Build the managed block from the repo's actual layout.
    let entries = engine_ignore_entries(root);
    let mut block = String::from(ENGINE_GITIGNORE_BEGIN);
    block.push('\n');
    for entry in &entries {
        block.push_str(entry);
        block.push('\n');
    }
    block.push_str(ENGINE_GITIGNORE_END);

    // Replace an existing managed block in place, or append a fresh one.
    let new_contents = if let (Some(start), Some(end_idx)) = (
        existing.find(ENGINE_GITIGNORE_BEGIN),
        existing.find(ENGINE_GITIGNORE_END),
    ) {
        let end = end_idx + ENGINE_GITIGNORE_END.len();
        let mut s = String::with_capacity(existing.len());
        s.push_str(&existing[..start]);
        s.push_str(&block);
        s.push_str(&existing[end..]);
        s
    } else if existing.trim().is_empty() {
        format!("{block}\n")
    } else {
        format!("{}\n\n{}\n", existing.trim_end(), block)
    };

    if new_contents != existing {
        if fs::write(&gitignore, &new_contents).is_ok() {
            println!(
                "Ensured engine runtime dirs are gitignored ({}).",
                entries.join(" ")
            );
        }
    }

    // Report — but never auto-remove — files already tracked that now match. A
    // soul that committed its engine state before this ran needs a deliberate
    // `git rm --cached` (history retained, files stay on disk).
    report_tracked_engine_paths(root);
}

/// Print a warning for any git-tracked paths that fall under the engine runtime
/// dirs, with the exact `git rm --cached` line to untrack them. Read-only: this
/// never touches the index.
fn report_tracked_engine_paths(root: &Path) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output();
    let stdout = match out {
        Ok(o) if o.status.success() => o.stdout,
        _ => return,
    };
    // Prefixes to match against tracked paths: the entries actually emitted
    // for this repo's layout (post-flip, e.g. `.ravel/config/` is committable
    // by law — only the pocket must stay untracked) plus legacy trees the
    // report should still catch — retired `.weave/` (anyone resurrecting a
    // pre-rename store deserves the warning) and the capitalized `Pool/` tree
    // from the pre-`.pool` layout.
    let mut prefixes: Vec<String> = engine_ignore_entries(root);
    prefixes.push(".weave/".to_string());
    prefixes.push("Pool/".to_string());
    let mut hits: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for path in stdout.split(|b| *b == 0) {
        if path.is_empty() {
            continue;
        }
        let p = String::from_utf8_lossy(path);
        for pre in &prefixes {
            if p.starts_with(pre.as_str()) {
                *hits.entry(pre.as_str()).or_insert(0) += 1;
                break;
            }
        }
    }
    if hits.is_empty() {
        return;
    }
    let total: usize = hits.values().sum();
    eprintln!(
        "\nwarning: {total} tracked file(s) match engine runtime dirs and should NOT be committed:"
    );
    for (pre, n) in &hits {
        eprintln!("    {pre} ({n} file(s))");
    }
    eprintln!("  To untrack (history retained, files stay on disk):");
    for pre in hits.keys() {
        eprintln!("    git rm -r --cached {}", pre.trim_end_matches('/'));
    }
    eprintln!("  Then commit the removal. (`Pool/` is legacy — migrate it to `.pool/` first.)\n");
}

// ─── kit-add ─────────────────────────────────────────────────────

/// Add an optional kit to the repo. Validates `scope: optional`, installs
/// scaffold via the drift-handler, creates class folders + templates, and
/// records the kit in `repo.yml`'s `optional_kits:` list.
pub(crate) fn cmd_kit_add(kit_spec: String) {
    let root = require_git_root();
    let lex_dir = root.join(".lex");
    if !lex_dir.exists() {
        eprintln!("Not a git-lex repo. Run 'git lex init' first.");
        exit(1);
    }
    let (org, repo, _) = resolve_kit_spec(&kit_spec);
    let canonical_spec = format!("{}/{}", org, repo);

    // Refuse to re-add an already-installed kit; the right move is kit-update.
    let already: Vec<String> = read_repo_yml_optional_kits(&lex_dir.join("repo.yml"));
    let already_present = already.iter()
        .any(|s| {
            let (o, r, _) = resolve_kit_spec(s);
            o == org && r == repo
        });
    if already_present {
        eprintln!("Kit '{}' is already installed. Use `git lex kit-update {}` to refresh it.", canonical_spec, canonical_spec);
        exit(1);
    }

    // Also refuse if it's the domain or base kit — those install via init,
    // not kit-add.
    if canonical_spec == BASE_KIT {
        eprintln!("Kit '{}' is the base kit — installed implicitly by `git lex init`. Cannot kit-add.", canonical_spec);
        exit(1);
    }
    if let Some(domain) = git_lex::RepoYml::load(&root).domain_kit() {
        let (d_org, d_repo, _) = resolve_kit_spec(&domain);
        if d_org == org && d_repo == repo {
            eprintln!("Kit '{}' is this repo's domain kit. Cannot kit-add a domain kit.", canonical_spec);
            exit(1);
        }
    }

    println!("Fetching '{}' from GitHub...", canonical_spec);
    let kit_dir = match fetch_and_validate_optional_kit(&canonical_spec) {
        KitFetchOutcome::Ready(p) => p,
        KitFetchOutcome::FetchFailed => {
            eprintln!("Failed to fetch kit '{}' from GitHub.", canonical_spec);
            eprintln!("Check that https://github.com/{}/{} exists and is reachable.", org, repo);
            exit(1);
        }
        KitFetchOutcome::ScopeMismatch(found_scope) => {
            eprintln!(
                "Kit '{}' has scope `{:?}`, not `Optional`. Use `git lex init --kit {}` for a domain kit.",
                canonical_spec, found_scope, canonical_spec
            );
            // Leave the fetched dir for inspection but back out of the install.
            exit(1);
        }
    };
    println!("Kit fetched at {}.", kit_dir.strip_prefix(&root).unwrap_or(&kit_dir).display());

    // Install scaffold. For a new optional kit nothing should exist locally
    // yet, so this is almost entirely fresh-install — but if the agent has
    // already hand-authored files matching the kit's paths, those converge to
    // the kit version (no backup — git history is the backup, Rob-ruled 2026-08-08).
    let report = install_scaffold_files_from_skip_existing(&kit_dir);
    if report.installed > 0 || report.skipped > 0 || !report.updated.is_empty() {
        println!(
            "Scaffold: {} file(s) installed, {} unchanged",
            report.installed, report.skipped
        );
        if !report.updated.is_empty() {
            println!(
                "Updated {} file(s) to the kit's version:",
                report.updated.len()
            );
            for path in &report.updated {
                println!("  {}", path);
            }
        }
    }

    // Regenerate derived artifacts for this kit. create_folders=true so the
    // class folders show up on disk immediately — lux's call: discoverability.
    println!("Regenerating artifacts for '{}/{}'...", org, repo);
    regenerate_kit_artifacts(&canonical_spec, &root, true);

    // Record in repo.yml.
    let repo_yml = lex_dir.join("repo.yml");
    if let Err(e) = append_optional_kit(&repo_yml, &canonical_spec) {
        eprintln!("Warning: failed to update .lex/repo.yml: {}", e);
        eprintln!("The kit is installed but won't be tracked by `git lex kit-update`.");
        eprintln!("Add this line manually under `optional_kits:`:");
        eprintln!("  - {}", canonical_spec);
    } else {
        println!("Recorded '{}' under optional_kits in .lex/repo.yml.", canonical_spec);
    }

    // Register the kit's hooks (and reap any orphans) in the substrate
    // config. install_scaffold_files_from_skip_existing above copies the
    // hook *files* to .claude/hooks/, but a hook does nothing until it's
    // registered under its event in settings.json — run_substrate_setup is
    // that pass, the ONE shared gate with the loud no-agent_name branch
    // (the pool-kit gap, Day 50; #67; review #11). Identity is per-repo,
    // not per-kit, so it re-derives the whole hook set from all installed
    // kits — exactly the convergent behavior we want.
    harness::run_substrate_setup(&root, None);

    println!("Kit '{}' added.", canonical_spec);

    // t-box: the new kit's ontology joins the persistent ontology graph.
    reload_ontology_graph();
}

// ─── kit-remove ──────────────────────────────────────────────────

/// Remove an optional kit. Scrubs from repo.yml's optional_kits list and
/// deletes `.lex/kit/{org}/{repo}/`. Asks before deleting content folders
/// (e.g. `Innerworld/`) unless --force.
pub(crate) fn cmd_kit_remove(kit_spec: String, force: bool) {
    let root = require_git_root();
    let lex_dir = root.join(".lex");
    if !lex_dir.exists() {
        eprintln!("Not a git-lex repo. Run 'git lex init' first.");
        exit(1);
    }
    let (org, repo, _) = resolve_kit_spec(&kit_spec);
    let canonical_spec = format!("{}/{}", org, repo);

    // Refuse to remove the base or domain kit.
    if canonical_spec == BASE_KIT {
        eprintln!("Cannot remove the base kit.");
        exit(1);
    }
    if let Some(domain) = git_lex::RepoYml::load(&root).domain_kit() {
        let (d_org, d_repo, _) = resolve_kit_spec(&domain);
        if d_org == org && d_repo == repo {
            eprintln!("Cannot remove the domain kit ('{}'). To switch domain kits, re-init.", canonical_spec);
            exit(1);
        }
    }

    // Verify it's in the optional_kits list. If not, nothing to do — but
    // still try to remove the on-disk dir in case of a half-removed state.
    let in_optionals = read_repo_yml_optional_kits(&lex_dir.join("repo.yml"))
        .iter()
        .any(|s| {
            let (o, r, _) = resolve_kit_spec(s);
            o == org && r == repo
        });
    if !in_optionals {
        eprintln!("Kit '{}' is not in optional_kits. Nothing to remove.", canonical_spec);
        exit(0);
    }

    // Identify the kit's content folder for the prompt. read folder_base
    // from the kit's kit.yml before we delete the install dir.
    let folder_base = kit_config_str(&canonical_spec, "folder base");
    let kit_types = get_kit_types(&canonical_spec);

    // Prompt before deleting content folders.
    let content_dir = folder_base.as_ref().map(|b| root.join(b));
    let content_exists = content_dir.as_ref().map(|p| p.exists()).unwrap_or(false);
    let mut delete_content = false;
    if content_exists {
        if force {
            delete_content = true;
        } else {
            eprint!(
                "Kit '{}' has a content folder at `{}/` with {} class folder(s). \
                 Delete it (with all your authored content inside)? [y/N] ",
                canonical_spec,
                folder_base.as_deref().unwrap_or("?"),
                kit_types.len()
            );
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).unwrap_or_default();
            let input = input.trim().to_lowercase();
            delete_content = input == "y" || input == "yes";
        }
    }

    // Scrub repo.yml.
    let repo_yml = lex_dir.join("repo.yml");
    if let Err(e) = remove_optional_kit(&repo_yml, &canonical_spec) {
        eprintln!("Failed to update repo.yml: {}", e);
        exit(1);
    }

    // Delete the kit install dir.
    if let Err(e) = remove_kit_install_dir(&canonical_spec) {
        eprintln!("Warning: failed to delete .lex/kit/{}/{}/: {}", org, repo, e);
    }

    // Delete content folder if confirmed.
    if delete_content {
        if let Some(cd) = content_dir {
            if let Err(e) = fs::remove_dir_all(&cd) {
                eprintln!("Warning: failed to delete content folder '{}': {}",
                    cd.strip_prefix(&root).unwrap_or(&cd).display(), e);
            } else {
                println!("Deleted content folder '{}/'.", folder_base.as_deref().unwrap_or("?"));
            }
        }
    } else if content_exists {
        println!("Content folder '{}/' kept on disk (you said no).", folder_base.as_deref().unwrap_or("?"));
    }

    println!("Kit '{}' removed.", canonical_spec);
}

#[cfg(test)]
mod residue_receipt_tests {
    use super::tree_has_symlink;
    use std::fs;

    /// PIN (residue receipt, Rob-approved 2026-08-08): the symlink guard.
    /// A plain tree is mentionable; a tree that IS or CONTAINS a symlink
    /// is never mentioned — live symlinked infrastructure must not be
    /// described as residue (the w3blord `skill/` near-miss).
    #[test]
    fn symlink_anywhere_in_tree_silences_the_receipt() {
        let base = std::env::temp_dir().join(format!(
            "git-lex-residue-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let plain = base.join("Plain");
        fs::create_dir_all(plain.join("sub")).unwrap();
        fs::write(plain.join("__Plain.md"), "x").unwrap();
        fs::write(plain.join("sub").join("note.md"), "x").unwrap();
        assert!(!tree_has_symlink(&plain), "plain tree must be mentionable");

        #[cfg(unix)]
        {
            let linked = base.join("Linked");
            fs::create_dir_all(&linked).unwrap();
            std::os::unix::fs::symlink(&plain, linked.join("pkg")).unwrap();
            assert!(
                tree_has_symlink(&linked),
                "a symlink ANYWHERE beneath silences the receipt"
            );
            // The dir itself being a symlink silences it too.
            let alias = base.join("Alias");
            std::os::unix::fs::symlink(&plain, &alias).unwrap();
            assert!(tree_has_symlink(&alias));
        }

        // Nonexistent/unreadable = doubt = silence.
        assert!(tree_has_symlink(&base.join("NoSuch")));

        let _ = fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod engine_gitignore_tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;


    // ---- ensure_engine_gitignore: the .pool/.copia/.weave runtime-dir push ----

    fn tmp_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gitlex-engine-ignore-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The exact entry lines of the managed block (contains() can't tell
    /// `.ravel/` from `.ravel/_ignore/` — line identity can).
    fn block_lines(got: &str) -> Vec<&str> {
        let start = got.find(ENGINE_GITIGNORE_BEGIN).expect("block begin");
        let end = got.find(ENGINE_GITIGNORE_END).expect("block end");
        got[start..end].lines().skip(1).collect()
    }

    #[test]
    fn engine_gitignore_appends_to_existing_blocklist() {
        let dir = tmp_repo("append");
        fs::write(dir.join(".gitignore"), ".lex/oxigraph/\ncustom/\n").unwrap();
        ensure_engine_gitignore(&dir);
        let got = fs::read_to_string(dir.join(".gitignore")).unwrap();
        // Original lines preserved.
        assert!(got.contains(".lex/oxigraph/"), "must keep existing entries");
        assert!(got.contains("custom/"));
        // Entries added under the sentinel. A layout with no engine dirs on
        // disk has no legacy paths anywhere, so flip-gated engines emit in
        // pocket form; .pool/ and .copia/ stay whole-dir (never flip / owner
        // unconfirmed); .lex/_ignore/ is unconditional.
        let lines = block_lines(&got);
        assert_eq!(
            lines,
            vec![".lex/_ignore/", ".pool/", ".copia/", ".horae/", ".ravel/_ignore/", ".pan/_ignore/"]
        );
        // .weave/ retired 2026-08-04 (Rob; fleet swept clean first) — a
        // re-emitted block must NOT reintroduce it.
        assert!(!got.contains(".weave/"));
        assert!(got.contains(ENGINE_GITIGNORE_END));
        fs::remove_dir_all(&dir).ok();
    }

    // ---- pocket-law layout gate (Rob 2026-08-05): both shapes pinned ----

    #[test]
    fn legacy_engine_layout_keeps_whole_dir_entry() {
        let dir = tmp_repo("legacy-holds");
        // One known legacy path still outside the pocket → the flip must NOT
        // happen, or spaceGOAT's transcripts (69M jsonl > GitHub's cap)
        // become committable one save before the migration runs.
        fs::create_dir_all(dir.join(".ravel").join("transcripts")).unwrap();
        ensure_engine_gitignore(&dir);
        let got = fs::read_to_string(dir.join(".gitignore")).unwrap();
        let lines = block_lines(&got);
        assert!(lines.contains(&".ravel/"), "whole-dir entry retained: {lines:?}");
        assert!(!lines.contains(&".ravel/_ignore/"), "must not flip early: {lines:?}");
        // The other flip-gated engine is unaffected by ravel's layout.
        assert!(lines.contains(&".pan/_ignore/"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn migrated_engine_layout_flips_to_pocket_entry() {
        let dir = tmp_repo("pocket-flips");
        // Data moved into the pocket, nothing legacy outside it → narrow
        // entry, so .ravel/config/ etc. become committable per the law.
        fs::create_dir_all(dir.join(".ravel").join("_ignore").join("oxigraph")).unwrap();
        fs::create_dir_all(dir.join(".ravel").join("config")).unwrap();
        ensure_engine_gitignore(&dir);
        let got = fs::read_to_string(dir.join(".gitignore")).unwrap();
        let lines = block_lines(&got);
        assert!(lines.contains(&".ravel/_ignore/"), "narrow entry expected: {lines:?}");
        assert!(!lines.contains(&".ravel/"), "whole-dir entry must be gone: {lines:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pool_never_flips_even_with_pocket_layout() {
        let dir = tmp_repo("pool-never");
        // Even a pocket-shaped .pool/ stays whole-dir ignored — Rob 2026-08-05:
        // untouched until the Pool conversion.
        fs::create_dir_all(dir.join(".pool").join("_ignore")).unwrap();
        ensure_engine_gitignore(&dir);
        let got = fs::read_to_string(dir.join(".gitignore")).unwrap();
        let lines = block_lines(&got);
        assert!(lines.contains(&".pool/"), "{lines:?}");
        assert!(!lines.contains(&".pool/_ignore/"), "{lines:?}");
        // Same for .copia/ until its owner confirms a legacy list.
        assert!(lines.contains(&".copia/"), "{lines:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn engine_migration_converges_block_on_rerun() {
        let dir = tmp_repo("converge");
        // Before: legacy layout → whole-dir. After the engine moves its data
        // into the pocket, the SAME call converges the entry — the engine's
        // migration is the trigger, no flag day.
        let legacy = dir.join(".ravel").join("oxigraph");
        fs::create_dir_all(&legacy).unwrap();
        ensure_engine_gitignore(&dir);
        let before = block_lines(&fs::read_to_string(dir.join(".gitignore")).unwrap())
            .contains(&".ravel/");
        assert!(before);
        fs::remove_dir_all(&legacy).unwrap();
        fs::create_dir_all(dir.join(".ravel").join("_ignore").join("oxigraph")).unwrap();
        ensure_engine_gitignore(&dir);
        let got = fs::read_to_string(dir.join(".gitignore")).unwrap();
        let lines = block_lines(&got);
        assert!(lines.contains(&".ravel/_ignore/"), "{lines:?}");
        assert!(!lines.contains(&".ravel/"), "{lines:?}");
        assert_eq!(got.matches(ENGINE_GITIGNORE_BEGIN).count(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn engine_gitignore_is_idempotent() {
        let dir = tmp_repo("idempotent");
        fs::write(dir.join(".gitignore"), ".lex/oxigraph/\n").unwrap();
        ensure_engine_gitignore(&dir);
        let once = fs::read_to_string(dir.join(".gitignore")).unwrap();
        ensure_engine_gitignore(&dir);
        ensure_engine_gitignore(&dir);
        let thrice = fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(once, thrice, "re-running must not duplicate the block");
        // Exactly one sentinel pair.
        assert_eq!(thrice.matches(ENGINE_GITIGNORE_BEGIN).count(), 1);
        assert_eq!(thrice.matches(ENGINE_GITIGNORE_END).count(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn engine_gitignore_replaces_block_in_place_on_dir_change() {
        let dir = tmp_repo("replace");
        // Simulate an OLD managed block missing a future dir (e.g. only .pool/).
        let old = format!(
            "keepme/\n\n{}\n.pool/\n{}\ntail/\n",
            ENGINE_GITIGNORE_BEGIN, ENGINE_GITIGNORE_END
        );
        fs::write(dir.join(".gitignore"), &old).unwrap();
        ensure_engine_gitignore(&dir);
        let got = fs::read_to_string(dir.join(".gitignore")).unwrap();
        // The block is rewritten in place (still one pair), now with all
        // entries, and the surrounding non-managed lines are untouched.
        assert_eq!(got.matches(ENGINE_GITIGNORE_BEGIN).count(), 1);
        let lines = block_lines(&got);
        assert_eq!(
            lines,
            vec![".lex/_ignore/", ".pool/", ".copia/", ".horae/", ".ravel/_ignore/", ".pan/_ignore/"]
        );
        assert!(!got.contains(".weave/"), "retired entry must be dropped on rewrite");
        assert!(got.contains("keepme/"), "content before the block is preserved");
        assert!(got.contains("tail/"), "content after the block is preserved");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn engine_gitignore_creates_file_when_absent() {
        let dir = tmp_repo("create");
        // No .gitignore at all.
        ensure_engine_gitignore(&dir);
        let got = fs::read_to_string(dir.join(".gitignore")).unwrap();
        let lines = block_lines(&got);
        assert_eq!(
            lines,
            vec![".lex/_ignore/", ".pool/", ".copia/", ".horae/", ".ravel/_ignore/", ".pan/_ignore/"]
        );
        assert!(!got.contains(".weave/"));
        assert_eq!(got.matches(ENGINE_GITIGNORE_BEGIN).count(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    // ---- folder_is_scaffold_only: the retired-folder reap gate ----

    #[test]
    fn scaffold_only_folder_is_reapable() {
        let dir = tmp_repo("reap-scaffold");
        let task = dir.join("Task");
        fs::create_dir_all(&task).unwrap();
        fs::write(task.join("__Task.md"), "---\n---\n").unwrap();
        fs::write(task.join(".gitkeep"), "").unwrap();
        assert!(folder_is_scaffold_only(&task, "Task"));
        // Empty folder (no template, no .gitkeep) is also reapable.
        let empty = dir.join("Mantra");
        fs::create_dir_all(&empty).unwrap();
        assert!(folder_is_scaffold_only(&empty, "Mantra"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn folder_with_content_is_never_reapable() {
        let dir = tmp_repo("reap-content");
        let tex = dir.join("Texture");
        fs::create_dir_all(&tex).unwrap();
        fs::write(tex.join("__Texture.md"), "---\n---\n").unwrap();
        fs::write(tex.join("self.md"), "agent content").unwrap();
        assert!(!folder_is_scaffold_only(&tex, "Texture"));
        // A foreign template (another class's __X.md) also blocks the reap.
        let odd = dir.join("Habit");
        fs::create_dir_all(&odd).unwrap();
        fs::write(odd.join("__Task.md"), "---\n---\n").unwrap();
        assert!(!folder_is_scaffold_only(&odd, "Habit"));
        // A subdirectory blocks the reap.
        let sub = dir.join("Dream");
        fs::create_dir_all(sub.join("archive")).unwrap();
        assert!(!folder_is_scaffold_only(&sub, "Dream"));
        // A missing folder is not reapable (nothing to do).
        assert!(!folder_is_scaffold_only(&dir.join("Nope"), "Nope"));
        fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod update_scope_tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tmp_repo(tag: &str, repo_yml: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gitlex-update-scope-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".lex")).unwrap();
        fs::write(dir.join(".lex").join("repo.yml"), repo_yml).unwrap();
        dir
    }

    const THREE_KITS: &str = "kit: repolex-ai/git-lex-kit-soul\n\
                              optional_kits:\n  - repolex-ai/git-lex-kit-copia\n";

    /// #102, stated as a test: an argument narrows the FETCH scope and must
    /// leave the REBUILD scope alone.
    ///
    /// The rebuild scope is what regenerates SHACL shapes, and
    /// converge_ontology_mirror deletes every kit's shapes on every run —
    /// so a kit missing from this list ends up with NO shapes, which silently
    /// untypes its literals. `kit-update copia` used to leave the soul kit in
    /// exactly that state.
    #[test]
    fn an_argument_narrows_the_fetch_but_never_the_rebuild() {
        let root = tmp_repo("narrow", THREE_KITS);

        let rebuild = collect_kits_for_update(&root, None);
        assert!(rebuild.len() >= 3, "expected base+soul+copia, got {:?}", rebuild);

        // Narrowing to one kit narrows fetching...
        let fetch = collect_kits_for_update(&root, Some("repolex-ai/git-lex-kit-copia"));
        assert_eq!(fetch.len(), 1, "fetch scope should narrow: {:?}", fetch);

        // ...and the rebuild scope, taken the way cmd_kit_update takes it,
        // is unchanged and still covers every installed kit.
        let rebuild_again = collect_kits_for_update(&root, None);
        assert_eq!(rebuild, rebuild_again);
        for kit in &rebuild {
            assert!(
                rebuild_again.contains(kit),
                "{} dropped out of the rebuild scope — its shapes would be deleted \
                 by the ontology mirror and never regenerated",
                kit
            );
        }

        let _ = fs::remove_dir_all(&root);
    }

    /// Base is always in the rebuild scope, even when the argument names a
    /// different kit — base ships git-lex's own vocabulary, so losing its
    /// shapes untypes File/fm facts in every repo.
    #[test]
    fn base_kit_is_always_rebuilt() {
        let root = tmp_repo("base", THREE_KITS);
        let rebuild = collect_kits_for_update(&root, None);
        assert!(
            rebuild.iter().any(|k| k == BASE_KIT),
            "base kit missing from rebuild scope: {:?}",
            rebuild
        );
        let _ = fs::remove_dir_all(&root);
    }
}

