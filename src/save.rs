//! `git lex save` + the pre-commit gates — identity resolution, validation,
//! extraction. Extracted from main.rs (#39, task #92).

use std::process::Command;
use git_lex::{Failure, Outcome};
use rudof_rdf::rdf_core::RDFFormat;
use rudof_rdf::rdf_impl::{InMemoryGraph, ReaderMode};
use shacl_ir::compiled::schema_ir::SchemaIR as ShaclSchemaIR;
use shacl_rdf::ShaclParser;
use shacl_validation::shacl_processor::{GraphValidation, ShaclProcessor, ShaclValidationMode};
use shacl_validation::store::Graph;
use sparql_service::RdfData;
use std::time::Instant;
use git_lex::get_kit;
use git_lex::nquad;
use git_lex::require_git_root;
use git_lex::nquad::generate_frontmatter_nquads;
use git_lex::extraction::frontmatter_to_turtle;
use git_lex::kit::read_repo_yml_fields;
use crate::harness;
use git_lex::{ontology, soul_md, spo_events};

// ─── git lex save ──────────────────────────────────────────────

/// Resolve the agent's git identity for this commit. THREE sources, in
/// precedence order (C23 fix, Day 40 — the resolver is now 3-of-3, not 2-of-3):
///
/// 1. **Process environment** — `GIT_AUTHOR_NAME` + `GIT_AUTHOR_EMAIL`. The
///    *live-session / squad-repo* case: the agent's Claude Code session injects
///    these from `<soul>/.claude/settings.json`, and they carry through to
///    `git lex save`. Highest authority — it's the running agent's identity now.
///
/// 2. **`<root>/.lex/repo.yml`** (`agent_name` + `agent_email`) — the
///    human-edited source of truth for identity. settings.json is *derived from*
///    repo.yml at init/kit-update time, so when they disagree repo.yml is the
///    authoritative one (settings.json is a stale cache). Reading repo.yml HERE
///    is what fixes the frozen-config trap: edit repo.yml and identity takes
///    effect immediately, no kit-update required.
///
/// 3. **`<root>/.claude/settings.json`** env block (read as data) — the last
///    fallback, for repos that predate the repo.yml identity fields or where
///    repo.yml is absent.
///
/// Returns `(name, email)` from the first source that resolves. Returns `None`
/// only if all three are missing — in which case we hard-fail rather than commit
/// as the user's global gitconfig.
fn resolve_agent_identity(root: &std::path::Path) -> Option<(String, String)> {
    // 1. Process environment (live session).
    if let (Ok(name), Ok(email)) = (
        std::env::var("GIT_AUTHOR_NAME"),
        std::env::var("GIT_AUTHOR_EMAIL"),
    )
        && !name.is_empty() && !email.is_empty() {
            return Some((name, email));
        }

    // 2. repo.yml (the human-edited source of truth — authoritative over the
    //    settings.json cache, so editing it works WITHOUT a kit-update).
    let fields = read_repo_yml_fields(&git_lex::layout::repo_yml(root));
    if let (Some(name), Some(email)) = (fields.get("agent_name"), fields.get("agent_email"))
        && !name.is_empty() && !email.is_empty() {
            return Some((name.clone(), email.clone()));
        }

    // 3. .claude/settings.json env block — last fallback, read through the
    //    module that WRITES that block (review #38: reader and writer of
    //    the env schema live in one file, so the schema can't drift apart
    //    across an unrelated module boundary again — the .env retirement
    //    already proved this block's location migrates).
    if let Some(id) = harness::claude::read_identity_env(root) {
        return Some(id);
    }

    // 4. Fallback for generic / non-soul repos: read user's git config.
    if !soul_md::soul_kit_installed(root)
        && let Ok(repo) = git2::Repository::open(root)
            && let Ok(sig) = repo.signature()
                && let (Some(name), Some(email)) = (sig.name(), sig.email())
                    && !name.is_empty() && !email.is_empty() {
                        return Some((name.to_string(), email.to_string()));
                    }

    None
}

pub(crate) fn cmd_save(message: &str, dry_run: bool, mechanical: bool) -> Outcome {
    // A commit that only reshapes files says so in a trailer, which is what
    // sync reads when it dates each document (sync::write_document_dates).
    let marked;
    let message = if mechanical {
        marked = format!("{}\n\n{}: true", message.trim_end(), git_lex::MECHANICAL_TRAILER);
        marked.as_str()
    } else {
        message
    };
    let root = require_git_root()?;
    // Only `git lex init` creates `.lex/` (git-lex#47). A save in a
    // repository that was never set up used to create `.lex/`, install the
    // hook, commit, and register the repository on this machine.
    git_lex::require_lex_repo(&root)?;

    // Identity floor: a soul repo without its root SOUL.md must not save
    // (fail-loud, #29 — the file is restorable via kit-update).
    soul_md::require_soul_md(&root)?;

    // ...and the file EXISTING is not the same as its identity being right.
    // soulId is derived from the genesis sha and the code says so twice — the
    // module header calls the IRI "DERIVED, never stored" and the heal message
    // tells the agent never to edit it by hand. But until now the only two
    // places that enforced it were `init` and `kit-update`. save checked
    // existence and nothing else, so a hand-edited soulId committed cleanly,
    // the store derived the Thing IRI straight from it, and the next
    // kit-update silently revoked the identity the soul had been walking
    // around under in between. @w3bl0rd reproduced exactly that: kira set
    // hers to `kira`, saved, and the store minted .../Soul/kira without a
    // word of objection.
    //
    // Heal here rather than refuse: the correct value is derived, so there is
    // nothing for the agent to decide, and heal_soul_id already says loudly
    // what it changed and why. Placed BEFORE the `git add -A` below, so the
    // correction lands in the same commit as the work — never as a surprise
    // diff the next save has to explain.
    //
    // A dry run PREVIEWS it. The probe must not edit the identity file: the
    // one file whose point is that it is not casually rewritten is the last
    // one a --dry-run should touch.
    if dry_run {
        match soul_md::preview_soul_id_heal(&root) {
            soul_md::HealOutcome::Healed => eprintln!(
                "DRY RUN: SOUL.md carries a hand-edited soulId — a real save would \
                 correct it to the genesis sha. soulId is derived; never edit it by hand."
            ),
            soul_md::HealOutcome::Filled => eprintln!(
                "DRY RUN: SOUL.md's soulId or soul.Soul.id line is missing or out of date — a real save would write it."
            ),
            _ => {}
        }
    } else {
        soul_md::heal_soul_id(&root);
    }

    // Hook floor: `git clone` does not copy .git/hooks, so a fresh clone
    // silently loses EVERY save gate — and until 2026-08-14 save committed
    // ungated without a word. Converge the hook before anything else; if it
    // cannot be converged, refuse (a save without the hook is a save
    // without gates, and a gate that can't run must not pretend it passed).
    if git_lex::layout::lex_dir(&root).exists() {
        match crate::hooks::converge_hook() {
            Ok(true) => println!(
                "Repaired: the pre-commit hook was missing or stale (clones don't \
                 carry hooks) — reinstalled, all save gates active."
            ),
            Ok(false) => {}
            Err(e) => {
                return Err(Failure::new([format!("fatal: cannot install the pre-commit save gate: {e}"), String::from("Saving without it would commit with NO validation, cleanup, or identity gate — refusing.")].join("\n")));
            }
        }
    }

    // Resolve the agent's identity — THREE sources in precedence order
    // (see resolve_agent_identity): env (squad-repo case where the soul
    // session injects GIT_AUTHOR_*), then .lex/repo.yml (authoritative,
    // travels with the soul — the C23 Day-40 fix), then settings.json
    // (legacy soul-repo case). Hard-fail otherwise — saving with the wrong
    // identity (e.g. user's global gitconfig leaking in) is worse than not
    // saving.
    let (author_name, author_email) = match resolve_agent_identity(&root) {
        Some(id) => id,
        None => {
            return Err(Failure::new([String::from("fatal: no agent identity configured."), String::new(), String::from("Couldn't resolve an author identity from any of:"), String::from("  - agent_name: / agent_email: in .lex/repo.yml (the simplest fix:"), String::from("    add those two lines there and save again)"), String::from("  - GIT_AUTHOR_NAME / GIT_AUTHOR_EMAIL in the environment"), format!("  - {}/.claude/settings.json", root.display()), String::new(), String::from("Agent repos: `git lex kit-update` refreshes identity; squad repos get"), String::from("env vars injected by your agent session's settings.")].join("\n")));
        }
    };
    // Who is saving (goodlux, 2026-11-15): the agent whose SESSION this is,
    // not the folder being saved in. When a harness places the session in a
    // soul that holds a bound key (src/identity.rs), the commit is that
    // agent's: its name as author and committer, its key as the signature.
    // A save in another agent's folder then says, and proves, who made it.
    // With no harness marker (a person at a terminal) or no bound key,
    // nothing here changes: the identity resolved above stands, unsigned.
    //
    // The signing agent's name and email come from ITS soul: repo.yml when
    // it has both, else the same sources save has always read, looked up
    // in that soul (a soul whose repo.yml lacked agent_email saved
    // unsigned without a word, found on the first Antigravity bind,
    // 2026-11-15). A bound agent whose name cannot be found is refused:
    // an agent with a key never saves unsigned by accident.
    let bound = git_lex::identity::session_signer().map(|s| (s.key, s.soul));
    let signer = match bound {
        None => None,
        Some((key, soul)) => {
            let yml = git_lex::RepoYml::load(&soul);
            let named = match (yml.agent_name.filter(|n| !n.is_empty()), yml.agent_email.filter(|e| !e.is_empty())) {
                (Some(name), Some(email)) => Some((name, email)),
                _ => resolve_agent_identity(&soul),
            };
            let Some((name, email)) = named else {
                return Err(Failure::new(format!(
                    "fatal: this session's agent has a key but no name to sign with.\n\
                     Add these two lines to {} and save again:\n\
                     \x20   agent_name: <the agent's name>\n\
                     \x20   agent_email: <the agent's email>",
                    git_lex::layout::repo_yml(&soul).display()
                )));
            };
            Some((key, name, email))
        }
    };
    let (author_name, author_email) = match &signer {
        Some((_, name, email)) => (name.clone(), email.clone()),
        None => (author_name, author_email),
    };
    let author = format!("{} <{}>", author_name, author_email);

    // The write-health probe: run the exact gates a real save runs —
    // extraction (which refreshes derived sidecars on disk), the sidecar
    // write-gate, the identity gate, SHACL validation — and commit nothing.
    // Exists because `verify` audits the STORE while the gates live on the
    // WRITE path, and a clean-tree save short-circuits before any gate: a
    // repo could be write-dead with NO command able to say so until the
    // moment a real write is needed (W3BL0RD's receipt, 2026-08-06: verify
    // ALL CHECKS PASSED on a repo that could not save). Known fidelity gap:
    // a real save stages deletions before the hook, so its sidecar cleanup
    // sees them; the probe stages nothing and skips that pass.
    if dry_run {
        cmd_extract()?;
        if !cmd_validate()? {
            return Err(Failure::new(format!("DRY RUN: a real `git lex save` would FAIL validation in {}.", root.display())));
        }
        println!(
            "DRY RUN: all save gates pass in {} — a real save would proceed [as {}].",
            root.display(),
            author
        );
        println!("(nothing was committed; derived sidecars under .lex/extract/ may have been refreshed)");
        return Ok(());
    }

    // Sync skills/subagents into every active substrate's harness. The
    // substrate list comes from `.lex/repo.yml`'s `substrates:` field
    // (explicit override) or auto-detection from on-disk markers
    // (.claude/, .hermes/, .gemini/). Falls back to Claude if nothing
    // is detected, preserving pre-multi-substrate behavior.
    harness::sync_all(&root);


    // Orphaned-sidecar convergence (#107, same ethos as the hook floor
    // above): a sidecar whose source document is gone can only come from a
    // raw git delete/rename outside save (historically: hookless clones).
    // The staged-changes cleanup in the hook can't see it — nothing is
    // staged on a clean tree, so save printed "Nothing to save" OVER the
    // damage while verify's check 6a reported it with no way to heal.
    // Removing the orphan here makes it a staged deletion below: this save
    // carries it, and the next sync retracts its facts honestly.
    let orphans = spo_events::remove_orphaned_sidecars(&root);
    if !orphans.is_empty() {
        println!(
            "Repaired: {} orphaned sidecar(s) whose source document is gone (a raw git \
             delete/rename bypassed save) — removed, so this save retracts their facts:",
            orphans.len()
        );
        for o in &orphans {
            println!("  - {o}");
        }
    }

    // Add everything, commit; the pre-commit hook handles extract + validate
    // (NOT sync — the store is updated separately by `git lex sync`)
    let status = Command::new("git")
        .args(["add", "-A"])
        .status();
    if !status.map(|s| s.success()).unwrap_or(false) {
        return Err(Failure::new("fatal: git add failed"));
    }

    // Markdown link healing (Rob-ruled 2026-08-14, lifecycle spec ruling
    // 1): staged .md renames pull every inline link that pointed at the
    // old path onto the new one — same commit, every edited file named.
    // A healer that cannot run fails the save: proceeding would commit a
    // rename while silently breaking the links the ruling promises to
    // carry ("a gate that can't run must not pretend it passed").
    match crate::heal::heal_staged_renames(&root) {
        Ok(report) if !report.is_empty() => {
            let total: usize = report.iter().map(|(_, n)| n).sum();
            println!(
                "Healed: {} markdown link(s) followed the staged rename(s) — {} file(s) edited in this same save:",
                total,
                report.len()
            );
            for (path, n) in &report {
                println!("  - {path} ({n} link(s))");
            }
        }
        Ok(_) => {}
        Err(e) => {
            return Err(Failure::new([format!("fatal: markdown link healing failed: {e}"), String::from("A staged rename may leave dangling links if this save proceeds — refusing."), String::from("fatal: git commit was not attempted — NOTHING WAS COMMITTED.")].join("\n")));
        }
    }

    // Check if there's anything to commit
    let diff = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .status();
    if diff.map(|s| s.success()).unwrap_or(false) {
        // Name the repo: save targets the CWD's repo, and an agent shell's cwd
        // drifts (Day 120: a save fired from another repo's dir reported
        // "nothing to save" while the intended repo sat modified — the bare
        // message was a null signal indistinguishable from a clean save).
        println!("Nothing to save (no changes) in {}", root.display());
        return Ok(());
    }

    let mut commit = Command::new("git");
    if let Some((key, _, _)) = &signer {
        commit.args(git_lex::identity::signing_args(key));
    }
    // Author and committer are the same agent on every save, signed or not
    // (goodlux, 2026-11-15). git takes the committer from its own config
    // when the harness sets no GIT_COMMITTER_* variables, which recorded the
    // machine's human as the committer of an agent's save.
    commit
        .env("GIT_COMMITTER_NAME", &author_name)
        .env("GIT_COMMITTER_EMAIL", &author_email)
        .args(["commit", "--author", &author, "-m", message]);
    if signer.is_some() {
        commit.arg("-S");
    }
    let status = commit.status();
    match status {
        Ok(s) if s.success() => {
            // The reconciler teaching line (selkie's incident, link 4): save
            // RECONCILES the working tree into git + graph — additions,
            // edits, AND deletions — and derived state under .lex/ is not
            // the caller's problem. Printed on every save so the mental
            // model builds on first use.
            println!(
                "Extracts reconciled automatically (adds, edits, and deletions) — \
                 .lex/ is maintained for you; never edit or delete it by hand."
            );
            // State change LAST: agents truncate output and keep the tail,
            // so the one line a caller must see survives `| tail -3`.
            println!("Saved in {}: {} [as {}]", root.display(), message, author);
            // Tell gitlexd, if it runs, that HEAD moved: one request, no
            // wait, and nothing said when no gitlexd is there. The sync
            // that used to run in this process now runs in the daemon.
            git_lex::gitlexd::client::nudge(&root);

            // The commit is real, but it must not be the last word when part
            // of the save did not happen. @w3bl0rd carried four dead skills
            // for four months because the failures printed ABOVE this line
            // every single time.
            // Author-actionable diagnostics get a COUNT below the success
            // line. @w3bl0rd-web, 2026-08-27: they print above it, which is the
            // same channel that hid four skill-sync failures on every save for
            // nineteen days, because nobody reads upward from "Saved". This does
            // not move or reshape those lines — it puts the number where the eye
            // lands, and stays compatible with whatever shape the stream itself
            // ends up with.
            let author_warnings = git_lex::nquad::author_warning_count();
            if author_warnings > 0 {
                eprintln!(
                    "...saved with {} warning(s) — see the warning:/note: lines above. \
                     The commit landed; those values saved as plain ungoverned data.",
                    author_warnings
                );
            }

            let harness_failed = crate::harness::claude::harness_failure_count();
            if harness_failed > 0 {
                eprintln!(
                    "...but {} harness file(s) did NOT sync — see the harness: lines above. \
                     The commit landed; the skills/subagents it should have installed did not.",
                    harness_failed
                );
            }
        }
        _ => {
            // An agent that doesn't know the transaction failed will assume
            // success and move on — say it in the final line, loudly.
            return Err(Failure::new([String::from("fatal: git commit failed — NOTHING WAS COMMITTED."), String::from("The gate output above names each blocking file and its fix; fix and save again.")].join("\n")));
        }
    }
    Ok(())
}


/// Returns true if all files pass, false if any violations found.
pub(crate) fn cmd_validate() -> Outcome<bool> {
    let start = Instant::now();

    let root = require_git_root()?;

    let kit = match get_kit() {
        Some(k) => k,
        None => {
            println!("No kit configured — nothing to validate.");
            return Ok(true);
        }
    };

    // Shapes come from ontology.rs's canonical resolver (review #14) — the
    // ONE owner of the shapes-path rule. This fn used to hand-build the
    // path, re-creating the exact divergence the resolver's own doc records
    // (task #29: a stale kit/-tier copy shadowing canonical shapes) and
    // skipping its shadow-fossil audit warning.
    //
    // SCOPE: validation runs against the DOMAIN kit only, deliberately for
    // now — frontmatter_to_turtle extracts only domain-kit keys, so
    // optional-kit facts are neither emitted nor judged by this gate.
    // Widening to all_shape_files() must land TOGETHER with multi-kit
    // extraction (board #82's domain-less-property rework), not alone.
    let shapes_ttl = ontology::read_kit_shapes(&kit);

    if shapes_ttl.is_empty() {
        return Ok(verdict_without_shapes(&kit));
    }

    // One walker for the whole codebase; `.txt` files ride along for the
    // slug index (sync's resolver indexes them as link targets, so validate
    // must too). Only .md files are validated (filter in validate_document).
    let files = git_lex::nquad::walk_repo_docs(&root);

    let Some(compiled_shapes) = compile_shapes(&kit, &shapes_ttl) else {
        return Ok(false);
    };

    let mut tally = ValidationTally::default();
    for filepath in &files {
        validate_document(filepath, &root, &kit, &compiled_shapes, &mut tally);
    }
    Ok(report_validation(&tally, start))
}

/// The verdict when the kit has no installed shapes: pass when its ontology
/// declares no document classes, block when shapes should exist.
fn verdict_without_shapes(kit: &str) -> bool {
    // Two very different "no shapes" cases (found live by the fresh
    // base-kit-only init receipt, review #12 sweep):
    // - the kit's ontology yields NO shapes (base ships engine vocab
    //   only, no document classes) → there is genuinely nothing to
    //   validate; the gate passes (blocking every commit of a
    //   base-kit repo forever is not a gate, it's a wall);
    // - the kit's ontology WOULD yield shapes but they're not
    //   installed → broken/partial install; a gate that can't run
    //   must not pretend it passed (Rob-ruled 2026-07-29).
    // Deciding which by re-deriving from the source TTL — the same
    // generator init/kit-update run.
    match crate::shacl::generate_shacl_shapes(kit) {
        Ok(None) => {
            println!("Kit '{}' declares no document classes — nothing to validate.", kit);
            true
        }
        Ok(Some(_)) => {
            eprintln!("fatal: kit '{}' is configured but its SHACL shapes are not installed — validation cannot run.", kit);
            eprintln!("Fix: `git lex kit-update` (reinstalls the kit's ontology and shapes), then retry.");
            false
        }
        Err(e) => {
            eprintln!("fatal: kit '{}' ontology is broken ({e}) — validation cannot run.", kit);
            eprintln!("Fix the kit TTL (or `git lex kit-update` for a fresh copy), then retry.");
            false
        }
    }
}

/// The kit's shapes, parsed and compiled once for the whole run. None, after
/// saying why, when they are installed but unusable.
fn compile_shapes(kit: &str, shapes_ttl: &str) -> Option<ShaclSchemaIR> {
    // CORRUPT shapes = same law as MISSING shapes (verdict_without_shapes):
    // a gate that can't run must not pretend it passed (Rob-ruled
    // 2026-07-29). These four arms used to `return true` — a broken shapes
    // file waved every save through while printing an error nobody was
    // required to read. All four are the identical cure: kit-update
    // regenerates shapes.
    let shapes_broken = |stage: &str, e: &dyn std::fmt::Display| -> Option<ShaclSchemaIR> {
        eprintln!("fatal: kit '{}' shapes are installed but unusable — {stage}: {e}", kit);
        eprintln!("Validation cannot run, so the save is blocked (a gate that can't run must not pretend it passed).");
        eprintln!("Fix: `git lex kit-update` (regenerates the kit's shapes), then retry.");
        None
    };
    // Strict: in lax mode the reader skips what it cannot parse, so a
    // garbled shapes file read as an EMPTY set of shapes, and empty shapes
    // pass every document — the exact wave-through the law above forbids.
    let shapes_graph = match InMemoryGraph::from_reader(
        &mut shapes_ttl.as_bytes(), "shapes", &RDFFormat::Turtle, None, &ReaderMode::Strict,
    ) {
        Ok(g) => g,
        Err(e) => return shapes_broken("Turtle parse failed", &e),
    };
    let shapes_rdf = match RdfData::from_graph(shapes_graph) {
        Ok(d) => d,
        Err(e) => return shapes_broken("graph load failed", &e),
    };
    let shapes_schema = match ShaclParser::new(shapes_rdf).parse() {
        Ok(s) => s,
        Err(e) => return shapes_broken("SHACL parse failed", &e),
    };
    match ShaclSchemaIR::compile(&shapes_schema) {
        Ok(c) => Some(c),
        Err(e) => shapes_broken("schema compile failed", &e),
    }
}

/// What validation counted over the run.
#[derive(Default)]
struct ValidationTally {
    files: usize,
    violations: usize,
    failed: Vec<String>,
}

/// Validate one document against the compiled shapes, printing its
/// violations and counting them into `tally`.
fn validate_document(
    filepath: &std::path::Path,
    root: &std::path::Path,
    kit: &str,
    compiled_shapes: &ShaclSchemaIR,
    tally: &mut ValidationTally,
) {
    if !filepath.to_string_lossy().ends_with(".md") { return; }
    // __ClassName.md templates are kit-owned scaffolds, never documents.
    // They were previously invisible here by accident (all-null values →
    // no triples → Ok(None)); now that a classed document emits its type
    // even with no values, the skip must be explicit — the same filter
    // extraction's own walker applies.
    if git_lex::nquad::is_template(filepath) { return; }
    let ttl = match frontmatter_to_turtle(filepath, root, kit) {
        Ok(Some(t)) => t,
        Ok(None) => return,
        Err(e) => {
            eprintln!("  {}: {}", filepath.display(), e);
            tally.files += 1;
            tally.violations += 1;
            tally.failed.push(filepath.display().to_string());
            return;
        }
    };
    tally.files += 1;

    // Parse this file's Turtle into RdfData
    // Every failure arm below COUNTS as a violation (review #24): a
    // file whose extracted Turtle can't parse, load, or validate is a
    // file the gate could not judge — and a gate that can't run must
    // not pretend it passed (same law as the missing-shapes arm above).
    let data_graph = match InMemoryGraph::from_reader(
        &mut ttl.as_bytes(), &filepath.to_string_lossy(), &RDFFormat::Turtle, None, &ReaderMode::Strict,
    ) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("  Parse error in {}: {}", filepath.display(), e);
            tally.violations += 1;
            tally.failed.push(filepath.display().to_string());
            return;
        }
    };
    let data_rdf = match RdfData::from_graph(data_graph) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("  Data load error in {}: {}", filepath.display(), e);
            tally.violations += 1;
            tally.failed.push(filepath.display().to_string());
            return;
        }
    };

    // Validate
    let mut validator = GraphValidation::from_graph(
        Graph::from_data(data_rdf), ShaclValidationMode::Native,
    );
    match ShaclProcessor::validate(&mut validator, compiled_shapes) {
        Ok(report) => {
            if !report.conforms() {
                let relpath = filepath.strip_prefix(root).unwrap_or(filepath);
                let violations = report.count_violations();
                tally.violations += violations;
                tally.failed.push(relpath.to_string_lossy().to_string());
                eprintln!("  {} — {} violation(s):", relpath.display(), violations);
                for result in report.results() {
                    let msg = result.message().unwrap_or("(no message)");
                    // Name the PROPERTY: "MinCount(1) not satisfied" alone
                    // tells the author nothing about which field to fix
                    // (selkie's incident — three empty identity fields,
                    // zero named). Local name is enough; the file line
                    // above scopes the kit.
                    match result.path().and_then(|p| p.pred()) {
                        Some(pred) => {
                            let local = pred.as_str().rsplit('/').next().unwrap_or(pred.as_str());
                            eprintln!("    → {}: {}", local, msg);
                        }
                        None => eprintln!("    → {}", msg),
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("  Validation error for {}: {}", filepath.display(), e);
            tally.violations += 1;
            tally.failed.push(filepath.display().to_string());
        }
    }
}

/// Print the run's totals; true when nothing failed.
fn report_validation(tally: &ValidationTally, start: Instant) -> bool {
    let elapsed = start.elapsed();
    if tally.violations == 0 {
        eprintln!("Validated {} files in {:.1}ms — all pass ✓",
            tally.files, elapsed.as_secs_f64() * 1000.0);
        true
    } else {
        eprintln!("Validated {} files in {:.1}ms — {} violation(s) in {} file(s)",
            tally.files, elapsed.as_secs_f64() * 1000.0,
            tally.violations, tally.failed.len());
        false
    }
}



// `cleanup_orphaned_sidecars` was deleted in Phase 3 of the history-graph
// work (2026-04-11). Its replacement is `spo_events::cleanup_sidecars_for_
// staged_changes()` which asks git for the staged change set instead of
// walking the filesystem — fixes the macOS APFS case-insensitivity bug
// and adds rename-as-move support so expensive-to-regenerate sidecars
// (future `.haiku.spo` subagent output) survive folder renames without
// re-running extractors.

/// Paths under `.lex/extract/` whose working-tree bytes differ from what the
/// index holds — i.e. extraction artifacts that were rewritten but are not
/// staged. Empty is the healthy answer. `git diff --name-only` compares the
/// working tree against the index, which is exactly the question.
///
/// A git that cannot answer returns empty rather than failing the commit:
/// this is a check on top of a staging step that already reported success,
/// and a broken `git diff` is not evidence of skew.
fn unstaged_extracts(root: &std::path::Path) -> Vec<String> {
    let Ok(out) = Command::new("git")
        .args(["diff", "--name-only", "--", ".lex/extract/"])
        .current_dir(root)
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .filter(|l| !l.is_empty())
        .collect()
}

/// Combined extraction + validation, called by the pre-commit hook.
/// Stamps machine-maintained dates, runs sidecar cleanup, frontmatter
/// extraction, markdown link extraction, stages artifacts, then SHACL
/// validates. Exits non-zero if anything fails.
pub(crate) fn hook_pre_commit() -> Outcome {
    // A hook left behind in a repository without git-lex (nuked by hand,
    // or copied) does nothing: it must neither create `.lex/` (git-lex#47)
    // nor block the commit.
    if let Some(root) = git_lex::find_git_root()
        && git_lex::lex_repo_check(&root).is_err() {
            return Ok(());
        }
    // Phase 1: extraction
    cmd_extract()?;

    // Stage extraction artifacts. A failed add would let the commit land
    // with sidecars that no longer match the .md content — the history
    // history build diffs COMMITTED sidecars, so that divergence would be
    // permanent and silent. Fail the commit instead.
    //
    // Exception: a repo that gitignores .lex/ has declared its artifacts
    // machine-local (the git-lex code repo dogfoods this way) — nothing is
    // committed, so no committed-sidecar divergence is possible. Skip
    // staging rather than fatal on `git add` refusing an ignored path,
    // which broke every commit in such repos (2026-08-04).
    let root = git_lex::require_git_root()?;
    let lex_ignored = Command::new("git").args(["check-ignore", "-q", ".lex"])
        .current_dir(&root)
        .status()
        .map(|s| s.success()).unwrap_or(false);
    if lex_ignored {
        println!(".lex/ is gitignored here — extraction artifacts stay local, not staged.");
    } else {
        let staged = Command::new("git").args(["add", "--", ".lex/extract/"])
            .current_dir(&root)
            .status()
            .map(|s| s.success()).unwrap_or(false);
        if !staged {
            return Err(Failure::new("fatal: failed to stage extraction artifacts (.lex/extract/)"));
        }
        // ...and check that it took. `git add` reporting success is not the
        // same as the index holding what is on disk: on lUX, 12,102 extract
        // files were written correctly during the hook, the add reported
        // success, and the commit carried the PREVIOUS run's bytes anyway.
        // They arrived one commit late, swept in by the next save's
        // `git add -A`. For that whole window the graph answered with
        // predicates the documents no longer used, and every gate passed.
        //
        // The invariant is one line of git: nothing under .lex/extract/ may
        // differ between the working tree and the index once staging is
        // done. Breaking the commit is the only honest outcome — a silent
        // one-commit skew between a document and the facts derived from it
        // is what history is built on.
        let out_of_sync = unstaged_extracts(&root);
        if !out_of_sync.is_empty() {
            eprintln!(
                "fatal: {} extraction artifact(s) under .lex/extract/ were rewritten \
                 but did not reach the index, so this commit would carry documents \
                 and extracts that disagree:",
                out_of_sync.len()
            );
            for p in out_of_sync.iter().take(10) {
                eprintln!("  {p}");
            }
            if out_of_sync.len() > 10 {
                eprintln!("  ...and {} more", out_of_sync.len() - 10);
            }
            return Err(Failure::new("Run `git add .lex/extract/` and save again; if it happens twice, report it."));
        }
    }

    // Phase 2: SHACL validation
    if !cmd_validate()? {
        return Err(Failure::new(""));
    }
    Ok(())
}

/// The model running this session, read and never guessed: a name the
/// environment states, else the last model named in the harness's own
/// session log. None when nothing says (goodlux, 2026-11-22: no
/// written-in model names).
pub(crate) fn runtime_model() -> Option<String> {
    for var in ["SUBSTRATE", "ANTIGRAVITY_MODEL", "GEMINI_MODEL", "CLAUDE_MODEL"] {
        if let Ok(val) = std::env::var(var)
            && !val.trim().is_empty()
        {
            return Some(val.trim().to_string());
        }
    }
    claude_session_model().or_else(antigravity_session_model)
}

/// Who is writing, for a new document's `author` line: `w4r3z-on-<model>`
/// for a squaddie whose model can be read, the bare username when it
/// cannot, and the bare username of the person this machine belongs to
/// when no harness is running. None when the session belongs to nobody
/// git-lex knows.
pub(crate) fn author_name() -> Option<String> {
    let session = git_lex::gitlexd::session::session_soul().ok()?;
    if !session.from_harness() {
        let host = git_lex::identity::host_dir()?;
        let owner = git_lex::identity::load_owner(&host).ok().flatten()?;
        let path = git_lex::registry_entries()
            .into_iter()
            .find(|e| e.genesis.as_deref() == Some(owner.genesis.as_str()))
            .map(|e| std::path::PathBuf::from(e.path))?;
        return git_lex::RepoYml::load(&path).username;
    }
    let username = git_lex::RepoYml::load(&session.root).username?;
    Some(match runtime_model() {
        Some(model) => format!("{username}-on-{model}"),
        None => username,
    })
}

/// The last `"model"` named in the tail of a session log, on a line that
/// `wanted` accepts. Session logs run to hundreds of megabytes and the last
/// turn is always near the end, so only the tail is read, in a window that
/// grows when large tool results pushed that turn further back. A tail cut
/// mid-line leaves one unparsable fragment at the top, which is skipped.
fn last_model_in_log(log: &std::path::Path, wanted: impl Fn(&serde_json::Value) -> Option<String>) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(log).ok()?;
    let len = f.metadata().ok()?.len();
    for window in [256u64 * 1024, 4 * 1024 * 1024, 32 * 1024 * 1024] {
        f.seek(SeekFrom::Start(len.saturating_sub(window))).ok()?;
        let mut buf = Vec::with_capacity(window.min(len) as usize);
        f.read_to_end(&mut buf).ok()?;
        let text = String::from_utf8_lossy(&buf);
        let last = text
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter_map(|v| wanted(&v))
            .filter(|m| !m.is_empty() && m != "<synthetic>")
            .last();
        if last.is_some() || window >= len {
            return last;
        }
    }
    None
}

/// The model of the running Antigravity session. Antigravity puts
/// `ANTIGRAVITY_APP_DATA_DIR` and `ANTIGRAVITY_CONVERSATION_ID` into every
/// tool shell and logs each planner step, with its model, to
/// `<dir>/brain/<id>/.system_generated/logs/transcript.jsonl`.
fn antigravity_session_model() -> Option<String> {
    let dir = std::env::var("ANTIGRAVITY_APP_DATA_DIR").ok()?;
    let id = std::env::var("ANTIGRAVITY_CONVERSATION_ID").ok()?;
    if id.is_empty() || id.contains('/') || id.contains("..") {
        return None;
    }
    let log = std::path::Path::new(&dir)
        .join("brain")
        .join(id)
        .join(".system_generated/logs/transcript.jsonl");
    last_model_in_log(&log, |v| {
        (v.get("type").and_then(|t| t.as_str()) == Some("PLANNER_RESPONSE"))
            .then(|| v.get("model").and_then(|m| m.as_str()).map(String::from))
            .flatten()
    })
}

/// Read the exact model id of the running Claude Code session.
///
/// Claude Code exports no model env var, but it logs every assistant turn to
/// `~/.claude/projects/<project-slug>/<CLAUDE_CODE_SESSION_ID>.jsonl` with a
/// `"model":"<id>"` field. The last one seen is the model that is saving now.
/// Returns `None` when the id or the log is missing.
fn claude_session_model() -> Option<String> {
    let session_id = std::env::var("CLAUDE_CODE_SESSION_ID").ok()?;
    if session_id.is_empty() || session_id.contains('/') || session_id.contains("..") {
        return None;
    }
    let home = std::env::var("HOME").ok()?;
    let projects = std::path::Path::new(&home).join(".claude").join("projects");
    let log = std::fs::read_dir(&projects)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path().join(format!("{session_id}.jsonl")))
        .find(|p| p.is_file())?;
    last_model_in_log(&log, |v| {
        v.get("message").and_then(|m| m.get("model")).and_then(|m| m.as_str()).map(String::from)
    })
}


pub(crate) fn cmd_extract() -> Outcome {
    let start = Instant::now();

    // Clean up .spo sidecars for .md files that are being deleted or
    // renamed in the currently-staged commit. Uses git to detect the
    // change set — exact-case, handles rename-as-move so future subagent-
    // driven `.haiku.spo` content survives folder renames without
    // regeneration. Replaces the old cleanup_orphaned_sidecars walker that
    // was buggy on macOS APFS (case-insensitive `Path::exists()`).
    //
    // See src/spo_events.rs (module header) and docs/history.md for the
    // design.
    let cleanup = spo_events::cleanup_sidecars_for_staged_changes();
    if !cleanup.is_empty() {
        eprintln!("Cleanup: {}", cleanup.summary());
        for p in &cleanup.deleted {
            eprintln!("  removed  {}", p);
        }
        for (old, new) in &cleanup.renamed {
            eprintln!("  moved    {} → {}", old, new);
        }
        for err in &cleanup.errors {
            eprintln!("  error    {}", err);
        }
        if !cleanup.errors.is_empty() {
            // An orphan sidecar left behind here keeps its facts alive in
            // the graph forever (the sync diff never sees the lines vanish).
            // Fail the commit; fix the state and retry.
            return Err(Failure::new("fatal: sidecar cleanup failed — see errors above"));
        }
    }

    // Run the ONE working-tree walk: frontmatter + markdown links, writing
    // both sidecar families (.fm.spo/.md.spo) in a single read + parse per
    // document. build_nquads is off — save needs the gates and the sidecars,
    // and used to build the full now-graph text only to discard it here.
    // Extraction errors join the save gate (review #23): an unextractable
    // doc keeps a stale sidecar. The context is built here and shared with
    // the identity gate below.
    let walk_opts = nquad::NowWalkOpts { write_sidecars: true, build_nquads: false };
    let ctx_root = git_lex::find_git_root();
    let (extraction_errors, extract_ctx) = match &ctx_root {
        Some(root) => {
            let ctx = nquad::ResolverContext::build(root);
            let errs = nquad::generate_frontmatter_nquads_with(root, &ctx, walk_opts)?.errors;
            (errs, Some(ctx))
        }
        None => {
            let errs = generate_frontmatter_nquads(walk_opts)?.errors;
            (errs, None)
        }
    };

    // (The .jsonl session extractor ran here 2026-04→08: claude-code-kit
    // only, 13 ad-hoc operators no ontology declared, zero sidecars ever
    // produced in any live repo. Deleted Rob-ruled 2026-08-01 — transcript
    // analytics is ravel's domain.)

    // ONE walk of .lex/extract/ (review #37): every sidecar's path +
    // content is collected once and feeds BOTH gates below — the v1
    // write-gate reads all .spo, the identity gate filters the .fm.spo
    // subset. The two copy-pasted walkers this replaces re-read the same
    // files and had to be kept in sync by hand.
    let all_spo: Vec<(std::path::PathBuf, String)> = {
        let mut out = Vec::new();
        if let Some(root) = &ctx_root {
            let mut stack = vec![git_lex::layout::extract_dir(root)];
            while let Some(dir) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&dir) else { continue };
                for entry in entries.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if path.extension().and_then(|e| e.to_str()) == Some("spo") {
                        let content = std::fs::read_to_string(&path).unwrap_or_default();
                        out.push((path, content));
                    }
                }
            }
        }
        out
    };

    // The v1 write-gate: validate EVERY sidecar (extraction rewrites the
    // full tree each save) against the format spec using the walker's own
    // line rules. Nothing gets committed that history can't later read —
    // the enforcement brick whose absence let one wrapped line ride 549
    // commits of lUX history.
    let mut gate_files = 0usize;
    let mut gate_errors = 0usize;
    if let Some(root) = &ctx_root {
        for (path, content) in &all_spo {
            gate_files += 1;
            for (lineno, err) in spo_events::validate_sidecar_v1(content) {
                let rel = path.strip_prefix(root).unwrap_or(path);
                eprintln!("sidecar gate: {}:{}: {}", rel.display(), lineno, err);
                gate_errors += 1;
            }
        }
    }

    let elapsed = start.elapsed();
    eprintln!("Extracted in {:.1}ms", elapsed.as_secs_f64() * 1000.0);

    if gate_errors > 0 {
        return Err(Failure::new(format!("fatal: sidecar write-gate: {} error(s) across {} sidecar file(s). \
             An out-of-spec sidecar means the extractor produced output the \
             format spec forbids — a git-lex bug unless the message names \
             damage in the sidecar file itself. Report it.",
            gate_errors, gate_files)));
    }
    eprintln!("Sidecar gate: {} file(s) conform to the v1 format ✓", gate_files);

    // Identity gate (identity model Law 3): per-class id uniqueness across
    // the repo, enforced at save. Two files claiming the same
    // <kit>/<Class>/<id> would collapse into ONE Thing IRI — a collision,
    // rejected loudly (Rob: "you can't have two things and reliably tell
    // them apart without an id — enforced, must-have"). Only files whose
    // Thing anchor actually derives participate; unanchored classed files
    // already warned in extraction (the Phase-4 work list).
    if let (Some(root), Some(ctx)) = (&ctx_root, &extract_ctx) {
        let mut id_errors = 0usize;
        let mut owners: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut all_sidecars: Vec<(String, Vec<String>)> = Vec::new();
        // Consumes the shared walk's collection (review #37): the identity
        // gate is the .fm.spo view of the same file set the v1 gate read.
        {
            for (path, content) in &all_spo {
                let Some(rel) = path.strip_prefix(root).ok().map(|p| p.to_string_lossy().to_string()) else { continue };
                let Some(src) = rel
                    .strip_prefix(".lex/extract/")
                    .and_then(|s| s.strip_suffix(".fm.spo"))
                else { continue };
                let lines: Vec<String> = content.lines().map(String::from).collect();
                let subjects = nquad::derive_file_subjects(
                    &lines, src, &ctx.declared_props,
                    &ctx.obj_props, &ctx.kit_namespaces, false,
                );
                if let Some(thing) = subjects.thing_uri {
                    if let Some(prior) = owners.get(&thing) {
                        eprintln!(
                            "identity gate: {} and {} both claim the Thing {} — \
                             per-class ids must be unique; change one file's id",
                            prior, src, thing
                        );
                        id_errors += 1;
                    } else {
                        owners.insert(thing, src.to_string());
                    }
                }
                all_sidecars.push((src.to_string(), lines));
            }
        }
        if id_errors > 0 {
            return Err(Failure::new(format!("fatal: identity gate: {} id collision(s)", id_errors)));
        }
        eprintln!("Identity gate: {} Thing id(s) unique ✓", owners.len());

        // Law 6, save-side: a declared reference whose range class is
        // FILE-EXPRESSED IN THIS REPO (foldered) must point at a Thing
        // that exists here — dangling rejects at save, same posture as
        // the path law. Graph-only ranges (Moment, …) skip the existence
        // check: their id-spaces live in engine stores, which own their
        // own integrity; the IRI still derives deterministically.
        let mut ref_errors = 0usize;
        let mut foldered_cache: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
        for (src, lines) in &all_sidecars {
            for line in lines {
                let parts: Vec<&str> = line.splitn(3, " | ").collect();
                if parts.len() != 3 || parts[1] != "hasValue" || parts[2].trim().is_empty() {
                    continue;
                }
                let segs: Vec<&str> = parts[0].splitn(3, '.').collect();
                if segs.len() != 3 {
                    continue;
                }
                // Range table is keyed by the DECLARED property IRI
                // (2026-08-20): join through the shapes' prop_iris — the
                // sidecar subject names the authoring kit+class, which for
                // an inherited property is NOT where the range lives. The
                // conventional-glue fallback covers same-kit properties in
                // repos with no generated shapes.
                let prop_iri = ctx
                    .prop_iris
                    .get(&format!("{}/{}/{}", segs[0], segs[1], segs[2]))
                    .cloned()
                    .unwrap_or_else(|| {
                        let ns = ctx
                            .kit_namespaces
                            .get(segs[0])
                            .cloned()
                            .unwrap_or_else(|| git_lex::conventional_kit_namespace(segs[0]));
                        format!("{}{}", ns, segs[2])
                    });
                let Some(range_iri) = ctx.ref_ranges.get(&prop_iri) else { continue };
                // The Thing range never enters this gate: its values are
                // full addresses (any class, any repo), so the foldered
                // in-repo existence law below has no single id-space to
                // check them against. Resolution/rejection already
                // happened in the emit lane.
                if git_lex::nquad::is_thing_root(range_iri) {
                    continue;
                }
                let enforce = *foldered_cache.entry(range_iri.clone()).or_insert_with(|| {
                    let Some(cut) = range_iri.rfind('/') else { return false };
                    let (ns, class) = range_iri.split_at(cut + 1);
                    let kit_short = ns.trim_end_matches('/').rsplit('/').next().unwrap_or("");
                    !kit_short.is_empty() && !class.is_empty()
                        && ontology::get_class_foldered(kit_short, class)
                });
                if !enforce {
                    continue;
                }
                // URL-aware split (review #26): same splitter as the emitter,
                // so the gate checks the exact values sync will resolve.
                for val in nquad::split_object_values(parts[2]) {
                    if let Some(target) = nquad::thing_iri_from_range(range_iri, &val)
                        && !owners.contains_key(&target) {
                            eprintln!(
                                "identity gate: {}: `{}` references `{}` but no Thing {} exists \
                                 in this repo — dangling references reject at save (Law 6)",
                                src, parts[0], val, target
                            );
                            ref_errors += 1;
                        }
                }
            }
        }
        if ref_errors > 0 {
            return Err(Failure::new(format!("fatal: identity gate: {} dangling reference(s)", ref_errors)));
        }
    }

    if extraction_errors > 0 {
        return Err(Failure::new(format!("fatal: {} frontmatter error(s) — fix before committing", extraction_errors)));
    }
    Ok(())
}

#[cfg(test)]
mod substrate_detect_tests {
    use super::*;

    /// One test, one env mutation sequence, under a lock: cargo runs tests on
    /// parallel threads and process env is shared, so separate tests that
    /// set/remove CLAUDE_CODE_SESSION_ID would race each other.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn claude_session_model_guards_env_and_path_shaped_ids() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var("CLAUDE_CODE_SESSION_ID").ok();

        // No session id at all → None, so the caller falls back.
        unsafe { std::env::remove_var("CLAUDE_CODE_SESSION_ID") };
        assert_eq!(claude_session_model(), None);

        // A path-shaped id must never reach the filesystem.
        unsafe { std::env::set_var("CLAUDE_CODE_SESSION_ID", "../etc/passwd") };
        assert_eq!(claude_session_model(), None);
        unsafe { std::env::set_var("CLAUDE_CODE_SESSION_ID", "a/b") };
        assert_eq!(claude_session_model(), None);

        match saved {
            Some(v) => unsafe { std::env::set_var("CLAUDE_CODE_SESSION_ID", v) },
            None => unsafe { std::env::remove_var("CLAUDE_CODE_SESSION_ID") },
        }
    }
}

#[cfg(test)]
mod extract_staging_gate_tests {
    use super::*;

    struct Tmp(std::path::PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    /// A repo with one committed extraction artifact.
    fn repo() -> Tmp {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        // Two tests starting in the same clock tick got the same folder
        // and fought over its git config (seen 2026-11-15); the counter
        // makes the name unique within the process.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = Tmp(std::env::temp_dir().join(format!("glx-gate-{}-{nanos}-{n}", std::process::id())));
        let root = &dir.0;
        std::fs::create_dir_all(root.join(".lex/extract/Soul/Note")).unwrap();
        let repo = git2::Repository::init(root).unwrap();
        std::fs::write(root.join(".lex/extract/Soul/Note/a.md.fm.spo"), "soul.Note.title | hasValue | one\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new(".lex/extract/Soul/Note/a.md.fm.spo")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("t", "t@example.invalid").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "one", &tree, &[]).unwrap();
        dir
    }

    #[test]
    fn a_clean_tree_has_no_unstaged_extracts() {
        let dir = repo();
        assert!(unstaged_extracts(&dir.0).is_empty());
    }

    #[test]
    fn an_extract_rewritten_but_not_staged_is_named() {
        let dir = repo();
        let p = dir.0.join(".lex/extract/Soul/Note/a.md.fm.spo");
        // Same length as the committed bytes: a size-only check would miss it.
        std::fs::write(&p, "soul.Note.title | hasValue | two\n").unwrap();
        assert_eq!(unstaged_extracts(&dir.0), vec![".lex/extract/Soul/Note/a.md.fm.spo".to_string()]);
        // Staging it clears the gate.
        assert!(Command::new("git").args(["add", "--", ".lex/extract/"])
            .current_dir(&dir.0).status().unwrap().success());
        assert!(unstaged_extracts(&dir.0).is_empty());
    }

    #[test]
    fn changes_outside_the_extract_folder_are_not_the_gates_business() {
        let dir = repo();
        std::fs::write(dir.0.join("README.md"), "hello\n").unwrap();
        assert!(unstaged_extracts(&dir.0).is_empty());
    }
}

#[cfg(test)]
mod validate_piece_tests {
    use super::*;

    /// A garbled shapes file blocks; it used to read as no shapes at all,
    /// which passed every document.
    #[test]
    fn garbled_shapes_block_instead_of_passing() {
        assert!(compile_shapes("t", "this is not turtle {").is_none());
        let truncated = "@prefix sh: <http://www.w3.org/ns/shacl#> .\n<https://ex/S> a sh:NodeShape ; sh:targetClass";
        assert!(compile_shapes("t", truncated).is_none());
    }

    #[test]
    fn usable_shapes_compile() {
        let ttl = "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
                   @prefix t: <https://ex/t/> .\n\
                   t:NoteShape a sh:NodeShape ; sh:targetClass t:Note ;\n\
                     sh:property [ sh:path t:title ; sh:minCount 1 ] .\n";
        assert!(compile_shapes("t", ttl).is_some());
    }

    #[test]
    fn any_violation_fails_the_run() {
        let start = Instant::now();
        assert!(report_validation(&ValidationTally { files: 3, ..Default::default() }, start));
        let failed = ValidationTally { files: 3, violations: 1, failed: vec!["a.md".into()] };
        assert!(!report_validation(&failed, start));
    }
}
