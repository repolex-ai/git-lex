//! `git lex save` + the pre-commit gates — identity resolution, validation,
//! extraction. Extracted from main.rs (#39, task #92).

use std::process::{Command, exit};
use std::time::Instant;
use git_lex::get_kit;
use crate::nquad;
use crate::require_git_root;
use crate::nquad::generate_frontmatter_nquads;
use crate::extraction::frontmatter_to_turtle;
use crate::kit::read_repo_yml_fields;
use crate::{harness, ontology, soul_md, spo_events};

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

pub(crate) fn cmd_save(message: &str, dry_run: bool, no_restamp: bool) {
    if no_restamp {
        unsafe {
            std::env::set_var("GIT_LEX_NO_RESTAMP", "1");
        }
    }
    let root = require_git_root();

    // Identity floor: a soul repo without its root SOUL.md must not save
    // (fail-loud, #29 — the file is restorable via kit-update).
    soul_md::require_soul_md(&root);

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
                "DRY RUN: SOUL.md has no soulId — a real save would fill it from the genesis sha."
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
                eprintln!("fatal: cannot install the pre-commit save gate: {e}");
                eprintln!("Saving without it would commit with NO validation, cleanup, or identity gate — refusing.");
                exit(1);
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
            eprintln!("fatal: no agent identity configured.");
            eprintln!();
            eprintln!("Couldn't resolve an author identity from any of:");
            eprintln!("  - agent_name: / agent_email: in .lex/repo.yml (the simplest fix:");
            eprintln!("    add those two lines there and save again)");
            eprintln!("  - GIT_AUTHOR_NAME / GIT_AUTHOR_EMAIL in the environment");
            eprintln!("  - {}/.claude/settings.json", root.display());
            eprintln!();
            eprintln!("Agent repos: `git lex kit-update` refreshes identity; squad repos get");
            eprintln!("env vars injected by your agent session's settings.");
            exit(1);
        }
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
        cmd_extract();
        if !cmd_validate() {
            eprintln!("DRY RUN: a real `git lex save` would FAIL validation in {}.", root.display());
            exit(1);
        }
        println!(
            "DRY RUN: all save gates pass in {} — a real save would proceed [as {}].",
            root.display(),
            author
        );
        println!("(nothing was committed; derived sidecars under .lex/extract/ may have been refreshed)");
        return;
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
        eprintln!("fatal: git add failed");
        exit(1);
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
            eprintln!("fatal: markdown link healing failed: {e}");
            eprintln!("A staged rename may leave dangling links if this save proceeds — refusing.");
            eprintln!("fatal: git commit was not attempted — NOTHING WAS COMMITTED.");
            exit(1);
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
        return;
    }

    let status = Command::new("git")
        .args(["commit", "--author", &author, "-m", message])
        .status();
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
            let author_warnings = crate::nquad::author_warning_count();
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
            eprintln!("fatal: git commit failed — NOTHING WAS COMMITTED.");
            eprintln!("The gate output above names each blocking file and its fix; fix and save again.");
            exit(1);
        }
    }
}


/// Returns true if all files pass, false if any violations found.
pub(crate) fn cmd_validate() -> bool {
    let start = Instant::now();

    let root = require_git_root();

    let kit = match get_kit() {
        Some(k) => k,
        None => {
            println!("No kit configured — nothing to validate.");
            return true;
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
        match crate::shacl::generate_shacl_shapes(&kit) {
            Ok(None) => {
                println!("Kit '{}' declares no document classes — nothing to validate.", kit);
                return true;
            }
            Ok(Some(_)) => {
                eprintln!("fatal: kit '{}' is configured but its SHACL shapes are not installed — validation cannot run.", kit);
                eprintln!("Fix: `git lex kit-update` (reinstalls the kit's ontology and shapes), then retry.");
                return false;
            }
            Err(e) => {
                eprintln!("fatal: kit '{}' ontology is broken ({e}) — validation cannot run.", kit);
                eprintln!("Fix the kit TTL (or `git lex kit-update` for a fresh copy), then retry.");
                return false;
            }
        }
    }

    // One walker for the whole codebase; `.txt` files ride along for the
    // slug index (sync's resolver indexes them as link targets, so validate
    // must too). Only .md files are validated (filter in the loop below).
    let files = crate::nquad::walk_repo_docs(&root);

    // Parse SHACL shapes into compiled schema (once)
    use rudof_rdf::rdf_core::RDFFormat;
    use rudof_rdf::rdf_impl::{InMemoryGraph, ReaderMode};
    use sparql_service::RdfData;
    use shacl_rdf::ShaclParser;
    use shacl_ir::compiled::schema_ir::SchemaIR as ShaclSchemaIR;
    use shacl_validation::shacl_processor::{GraphValidation, ShaclProcessor, ShaclValidationMode};
    use shacl_validation::store::Graph;

    // CORRUPT shapes = same law as MISSING shapes (twenty lines up): a gate
    // that can't run must not pretend it passed (Rob-ruled 2026-07-29).
    // These four arms used to `return true` — a broken shapes file waved
    // every save through while printing an error nobody was required to
    // read. All four are the identical cure: kit-update regenerates shapes.
    let shapes_broken = |stage: &str, e: &dyn std::fmt::Display| -> bool {
        eprintln!("fatal: kit '{}' shapes are installed but unusable — {stage}: {e}", kit);
        eprintln!("Validation cannot run, so the save is blocked (a gate that can't run must not pretend it passed).");
        eprintln!("Fix: `git lex kit-update` (regenerates the kit's shapes), then retry.");
        false
    };
    let shapes_graph = match InMemoryGraph::from_reader(
        &mut shapes_ttl.as_bytes(), "shapes", &RDFFormat::Turtle, None, &ReaderMode::Lax,
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
    let compiled_shapes = match ShaclSchemaIR::compile(&shapes_schema) {
        Ok(c) => c,
        Err(e) => return shapes_broken("schema compile failed", &e),
    };

    let mut total_files = 0;
    let mut total_violations = 0;
    let mut failed_files = Vec::new();

    for filepath in &files {
        if !filepath.to_string_lossy().ends_with(".md") { continue; }
        // __ClassName.md templates are kit-owned scaffolds, never documents.
        // They were previously invisible here by accident (all-null values →
        // no triples → Ok(None)); now that a classed document emits its type
        // even with no values, the skip must be explicit — the same filter
        // extraction's own walker applies.
        if crate::nquad::is_template(filepath) { continue; }
        let ttl = match frontmatter_to_turtle(filepath, &root, &kit) {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(e) => {
                eprintln!("  {}: {}", filepath.display(), e);
                total_files += 1;
                total_violations += 1;
                failed_files.push(filepath.display().to_string());
                continue;
            }
        };
        total_files += 1;

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
                total_violations += 1;
                failed_files.push(filepath.display().to_string());
                continue;
            }
        };
        let data_rdf = match RdfData::from_graph(data_graph) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("  Data load error in {}: {}", filepath.display(), e);
                total_violations += 1;
                failed_files.push(filepath.display().to_string());
                continue;
            }
        };

        // Validate
        let mut validator = GraphValidation::from_graph(
            Graph::from_data(data_rdf), ShaclValidationMode::Native,
        );
        match ShaclProcessor::validate(&mut validator, &compiled_shapes) {
            Ok(report) => {
                if !report.conforms() {
                    let relpath = filepath.strip_prefix(&root).unwrap_or(filepath);
                    let violations = report.count_violations();
                    total_violations += violations;
                    failed_files.push(relpath.to_string_lossy().to_string());
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
                total_violations += 1;
                failed_files.push(filepath.display().to_string());
            }
        }
    }

    let elapsed = start.elapsed();
    if total_violations == 0 {
        eprintln!("Validated {} files in {:.1}ms — all pass ✓",
            total_files, elapsed.as_secs_f64() * 1000.0);
        true
    } else {
        eprintln!("Validated {} files in {:.1}ms — {} violation(s) in {} file(s)",
            total_files, elapsed.as_secs_f64() * 1000.0,
            total_violations, failed_files.len());
        false
    }
}



// ─── viz/serve (moved to git-lex-serve binary) ─────────────────

// Viz server and SPARQL endpoint live in src/bin/git-lex-serve.rs


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
pub(crate) fn hook_pre_commit() {
    // Phase 0: machine-maintained dates (git-lex:updatedDate, Rob-ruled
    // 2026-08-26). BEFORE extraction, so the stamped value reaches the
    // sidecar and both land in the same commit. Lives in the hook, not in
    // cmd_save, so `git lex save` and a plain `git commit` behave
    // identically — one door's documents must not date-drift from the
    // other's.
    stamp_dates_for_staged_changes();

    // Phase 1: extraction
    cmd_extract();

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
    let root = crate::require_git_root();
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
            eprintln!("fatal: failed to stage extraction artifacts (.lex/extract/)");
            exit(1);
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
            eprintln!("Run `git add .lex/extract/` and save again; if it happens twice, report it.");
            exit(1);
        }
    }

    // Phase 2: SHACL validation
    if !cmd_validate() {
        exit(1);
    }
}

/// Detect active runtime substrate for stamping documents on save.
///
/// Order of precedence:
/// 1. Explicit `SUBSTRATE` environment variable (e.g. `gemini-3.7-flash`, `claude-opus-5`)
/// 2. Specific model env vars (`ANTIGRAVITY_MODEL`, `GEMINI_MODEL`, `CLAUDE_MODEL`)
/// 3. Active runtime process/environment markers (`ANTIGRAVITY_AGENT`, `CLAUDE_CODE_SESSION_ID`)
/// 4. Declared / detected active substrates from repo config / disk markers
pub fn detect_runtime_substrate(root: &std::path::Path) -> Option<String> {
    if let Ok(val) = std::env::var("SUBSTRATE") {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if let Ok(val) = std::env::var("ANTIGRAVITY_MODEL").or_else(|_| std::env::var("GEMINI_MODEL")) {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if let Ok(val) = std::env::var("CLAUDE_MODEL") {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if std::env::var("ANTIGRAVITY_AGENT").is_ok()
        || std::env::var("ANTIGRAVITY_CONVERSATION_ID").is_ok()
    {
        return Some("gemini-3.7-flash".to_string());
    }
    if std::env::var("CLAUDE_CODE_SESSION_ID").is_ok()
        || std::env::var("CLAUDE_PROJECT_DIR").is_ok()
    {
        return Some(claude_session_model().unwrap_or_else(|| {
            eprintln!("warning: could not read the Claude session log; writing substrate: claude-opus-5 into the documents this save touches");
            "claude-opus-5".to_string()
        }));
    }
    let subs = crate::harness::active_substrates(root);
    if !subs.is_empty() {
        match subs[0] {
            crate::harness::Substrate::Gemini => Some("gemini-3.7-flash".to_string()),
            crate::harness::Substrate::Claude => Some("claude-opus-5".to_string()),
            crate::harness::Substrate::Hermes => Some("hermes".to_string()),
        }
    } else {
        None
    }
}

/// Read the exact model id of the running Claude Code session.
///
/// Claude Code exports no model env var, but it logs every assistant turn to
/// `~/.claude/projects/<project-slug>/<CLAUDE_CODE_SESSION_ID>.jsonl` with a
/// `"model":"<id>"` field. The last one seen is the model that is saving now.
/// Returns `None` when the id or the log is missing, so the caller can fall
/// back to the historical hardcoded name and nothing changes shape.
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
    // Session logs run to 500+ MB; the last assistant turn is always near
    // the end, so read only the tail. Escalate the window if a burst of
    // large tool results pushed the last turn further back. A tail cut
    // mid-line leaves one unparsable fragment at the top, which the
    // per-line parse skips.
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(&log).ok()?;
    let len = f.metadata().ok()?.len();
    for window in [256u64 * 1024, 4 * 1024 * 1024, 32 * 1024 * 1024] {
        f.seek(SeekFrom::Start(len.saturating_sub(window))).ok()?;
        let mut buf = Vec::with_capacity(window.min(len) as usize);
        f.read_to_end(&mut buf).ok()?;
        let text = String::from_utf8_lossy(&buf);
        let mut last: Option<String> = None;
        for line in text.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
                && let Some(m) = v.get("message").and_then(|m| m.get("model")).and_then(|m| m.as_str())
                    && !m.is_empty() && m != "<synthetic>" {
                        last = Some(m.to_string());
                    }
        }
        if last.is_some() || window >= len {
            return last;
        }
    }
    None
}

/// Stamp the machine-maintained universals into every staged document.
/// `createdDate` is the date it was created; `updatedDate` is the date it
/// was updated; `substrate` is the model saving it. Nothing else decides
/// (goodlux, 2026-09-18). kit-base 0.18 declares all three on
/// git-lex:Thing, so every class carries them.
///
/// A new document (staged as added) gets both dates set to now. A modified
/// or renamed document gets `updatedDate` set to now and its `createdDate`
/// left exactly as it is. The only lines skipped are templates
/// (`__Class.md` is kit scaffold, not a document) and files with no
/// git-lex frontmatter key (README and friends).
///
/// The keys are spelled `createdDate` and `updatedDate`. A line under any
/// other spelling is another key and is not touched: the undeclared-key
/// warning names it at every save, and `kit-update` is what converges a
/// soul's vocabulary.
///
/// Stamped files are re-staged so the commit carries the stamped bytes.
fn read_head_blob(
    repo: &git2::Repository,
    head_tree: Option<&git2::Tree>,
    path: &std::path::Path,
) -> Option<String> {
    let tree = head_tree?;
    let entry = tree.get_path(path).ok()?;
    let object = entry.to_object(repo).ok()?;
    let blob = object.into_blob().ok()?;
    std::str::from_utf8(blob.content()).ok().map(|s| s.to_string())
}

fn normalize_fm_key(key: &str) -> String {
    if let Some(prefix) = key.strip_suffix(".dateCreated") {
        format!("{prefix}.createdDate")
    } else if key == "dateCreated" {
        "createdDate".to_string()
    } else if let Some(prefix) = key.strip_suffix(".dateUpdated") {
        format!("{prefix}.updatedDate")
    } else if key == "dateUpdated" {
        "updatedDate".to_string()
    } else {
        key.to_string()
    }
}

fn normalize_fm_mapping(map: &serde_yaml::Mapping) -> std::collections::BTreeMap<String, serde_yaml::Value> {
    let mut normalized = std::collections::BTreeMap::new();
    for (k, v) in map {
        let key_str = match k {
            serde_yaml::Value::String(s) => normalize_fm_key(s),
            _ => continue,
        };
        normalized.insert(key_str, v.clone());
    }
    normalized
}

fn is_substantive_doc_change(old_content: &str, new_content: &str) -> bool {
    if old_content == new_content {
        return false;
    }
    let (old_fm, old_body) = git_lex::split_frontmatter(old_content);
    let (new_fm, new_body) = git_lex::split_frontmatter(new_content);

    if old_body != new_body {
        return true;
    }

    let (Some(old_yaml), Some(new_yaml)) = (old_fm, new_fm) else {
        return true;
    };

    let Ok(old_map) = git_lex::parse_frontmatter_map(old_yaml) else {
        return true;
    };
    let Ok(new_map) = git_lex::parse_frontmatter_map(new_yaml) else {
        return true;
    };

    let old_norm = normalize_fm_mapping(&old_map);
    let new_norm = normalize_fm_mapping(&new_map);

    old_norm != new_norm
}

/// Frontmatter date stamping for staged changes.
///
/// Documents staged with `git add` are inspected. If the document carries a
/// git-lex frontmatter key, `createdDate`, `updatedDate`, and `substrate` are
/// maintained according to git-lex invariants (#38):
/// - `substrate` is immutable once set: never overwrite an existing non-empty value.
/// - Never add `substrate` to an existing document (!is_new) that lacks one.
/// - `updatedDate` is bumped unless `skip_date_bump` is active (`--no-restamp`
///   flag or non-substantive change detection where body and normalized frontmatter
///   values are unchanged from HEAD, e.g. key migrations or pure file moves).
///
/// Stamped files are re-staged so the commit carries the stamped bytes.
fn stamp_dates_for_staged_changes() {
    let root = crate::require_git_root();
    let runtime_sub = detect_runtime_substrate(&root);
    // Never guess a date into a permanent record: no clock, no stamp.
    let Some(now) = local_datetime_now() else { return };

    let no_restamp = std::env::var("GIT_LEX_NO_RESTAMP")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let out = Command::new("git")
        .args(["diff", "--cached", "--name-status", "-M", "--", "*.md"])
        .current_dir(&root)
        .output();
    let Ok(out) = out else { return };
    let listing = String::from_utf8_lossy(&out.stdout).to_string();

    let repo = git2::Repository::open(&root).ok();
    let head_tree = repo
        .as_ref()
        .and_then(|r| r.head().ok())
        .and_then(|h| h.peel_to_commit().ok())
        .and_then(|c| c.tree().ok());

    // Stamped documents are re-staged in ONE call after the loop: one
    // process per stamped document was most of a nine-minute save.
    let mut to_stage: Vec<std::path::PathBuf> = Vec::new();
    let mut stamped = 0usize;
    let mut born = 0usize;
    for line in listing.lines() {
        let mut cols = line.split('\t');
        let Some(status) = cols.next() else { continue };
        let status_char = status.chars().next();
        let (old_path, path) = match status_char {
            Some('A') => (None, cols.next()),
            Some('M') => {
                let p = cols.next();
                (p, p)
            }
            Some('R') => {
                let old_p = cols.next();
                let new_p = cols.next();
                (old_p, new_p)
            }
            _ => continue,
        };
        let Some(path) = path else { continue };
        let path = std::path::Path::new(path);
        if crate::nquad::is_template(path) {
            continue;
        }
        let full_path = root.join(path);
        let Ok(content) = std::fs::read_to_string(&full_path) else { continue };
        let Some(prefix) = frontmatter_kit_class(&content) else { continue };
        let is_new = status_char == Some('A');

        let mut skip_date_bump = no_restamp;
        if !skip_date_bump
            && !is_new
            && let (Some(repo), Some(old_p)) = (repo.as_ref(), old_path)
            && let Some(old_content) = read_head_blob(repo, head_tree.as_ref(), std::path::Path::new(old_p))
            && !is_substantive_doc_change(&old_content, &content)
        {
            skip_date_bump = true;
        }

        let Some(new_content) =
            stamp_frontmatter_dates(&content, &prefix, &now, runtime_sub.as_deref(), is_new, skip_date_bump)
        else {
            continue;
        };
        if std::fs::write(&full_path, &new_content).is_err() {
            eprintln!("warning: could not write createdDate/updatedDate/substrate into {} — \
                       the file commits with the dates it already had", path.display());
            continue;
        }
        to_stage.push(path.to_path_buf());
        stamped += 1;
        if is_new {
            born += 1;
        }
    }
    // A stamped file that is not re-staged commits unstamped while its
    // sidecar (extracted from disk, next phase) carries the stamp — the
    // committed sidecar and document disagree forever. Fail the commit
    // instead, same posture as staging .lex/extract/ below.
    if let Err(e) = stage_paths(&root, &to_stage) {
        eprintln!("fatal: could not stage the {} document(s) whose dates were just written: {e}", to_stage.len());
        exit(1);
    }
    if stamped > 0 {
        if born > 0 {
            println!("Wrote updatedDate: {} into {} document(s); {} of them are new, so createdDate too",
                now, stamped, born);
        } else {
            println!("Wrote updatedDate: {} into {} document(s)", now, stamped);
        }
    }
}

/// The two date keys. There is no second spelling of either.
const CREATED: &str = "createdDate";
const UPDATED: &str = "updatedDate";

/// Stage `paths` (repo-relative) with ONE `git update-index --add`, the
/// list fed NUL-separated on stdin, so a save that dated 13,000 documents
/// costs one process, not 13,000. `update-index` takes exact paths;
/// `git add` treats each as a pattern and matches it against every index
/// entry, which was 13,000 × 29,000 comparisons — 20 seconds — at that
/// size. Nothing to stage is Ok. Err carries git's own words.
fn stage_paths(root: &std::path::Path, paths: &[std::path::PathBuf]) -> Result<(), String> {
    use std::io::Write;
    if paths.is_empty() {
        return Ok(());
    }
    let mut list: Vec<u8> = Vec::new();
    for p in paths {
        list.extend_from_slice(p.as_os_str().as_encoded_bytes());
        list.push(0);
    }
    let mut child = Command::new("git")
        .args(["update-index", "--add", "-z", "--stdin"])
        .current_dir(root)
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start git: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("git's stdin was not open")?
        .write_all(&list)
        .map_err(|e| format!("could not send the file list to git: {e}"))?;
    let out = child.wait_with_output().map_err(|e| format!("git add did not finish: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Now, ISO-8601 with the machine's UTC offset (`2026-08-26T14:32:05-07:00`)
/// — a valid xsd:dateTime. None when no clock can be read: never guess a
/// date into a permanent record.
fn local_datetime_now() -> Option<String> {
    git_lex::clock::now_rfc3339()
}

/// The document's `<kit>.<Class>` key prefix, read from the first flat
/// dot-notation key in its frontmatter. None when the file has no
/// frontmatter or no such key — that file is not a git-lex document and
/// is never stamped.
fn frontmatter_kit_class(content: &str) -> Option<String> {
    let mut lines = content.lines();
    if lines.next()? != "---" {
        return None;
    }
    for line in lines {
        if line == "---" {
            return None;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        let key = trimmed.split(':').next()?.trim();
        let mut parts = key.split('.');
        if let (Some(kit), Some(class), Some(_prop), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
            && !kit.is_empty()
                && class.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            {
                return Some(format!("{}.{}", kit, class));
            }
    }
    None
}

/// Pure stamping: returns the new content, or None when nothing changes.
///
/// `updatedDate` is set to `now` when modified (unless `skip_date_bump` is true).
/// `createdDate` is set to `now` when `is_new`, and otherwise left exactly as found
/// — a modified document's birth date is never touched, whatever it holds.
/// `substrate` is set on new documents (`is_new`), or filled if an existing key
/// holds an empty placeholder. Existing authored substrate is never overwritten (#38).
/// Absent substrate is never added to an existing document (!is_new).
/// A present key line is rewritten whole (`key: value`; a scaffold's teaching comment
/// retires once the machine owns the value); an absent key is inserted just above the
/// closing `---`, createdDate before updatedDate. No other key is read or written.
fn stamp_frontmatter_dates(
    content: &str,
    kit_class: &str,
    now: &str,
    substrate: Option<&str>,
    is_new: bool,
    skip_date_bump: bool,
) -> Option<String> {
    let updated_key = format!("{kit_class}.{UPDATED}");
    let created_key = format!("{kit_class}.{CREATED}");
    let substrate_key = format!("{kit_class}.substrate");

    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    if lines.first().map(String::as_str) != Some("---") {
        return None;
    }
    let close = lines.iter().skip(1).position(|l| l == "---")? + 1;

    let mut changed = false;
    let mut found_updated = false;
    let mut found_created = false;
    let mut found_substrate = false;
    for line in &mut lines[1..close] {
        let key = line.trim_start().split(':').next().unwrap_or("").trim();
        let wanted = if key == updated_key {
            found_updated = true;
            if skip_date_bump && !is_new {
                continue;
            } else {
                format!("{updated_key}: {now}")
            }
        } else if key == created_key {
            found_created = true;
            if is_new {
                format!("{created_key}: {now}")
            } else {
                // A modified document's birth date is never touched.
                continue;
            }
        } else if key == substrate_key {
            found_substrate = true;
            // Substrate is immutable once set: never overwrite an existing non-empty value (#38).
            let val = line.split_once(':').map(|(_, v)| v.trim()).unwrap_or("");
            let is_empty = val.is_empty() || val == "\"\"" || val == "''" || val.starts_with('#');
            if is_empty {
                match substrate {
                    Some(sub) => format!("{substrate_key}: \"{sub}\""),
                    None => continue,
                }
            } else {
                // Keep existing authored substrate untouched.
                continue;
            }
        } else {
            continue;
        };
        if *line != wanted {
            *line = wanted;
            changed = true;
        }
    }

    // Insert what's missing above the closing `---`, in the order
    // createdDate, updatedDate, substrate (each insert at `close` lands
    // above the ones already inserted, so they go in reverse).
    //
    // Substrate: only inserted on new documents when absent (#38).
    // Never add substrate to an existing document (!is_new) that lacks one.
    if is_new
        && let Some(sub) = substrate
        && !found_substrate
    {
        lines.insert(close, format!("{substrate_key}: \"{sub}\""));
        changed = true;
    }
    if !found_updated && (is_new || !skip_date_bump) {
        lines.insert(close, format!("{updated_key}: {now}"));
        changed = true;
    }
    if is_new && !found_created {
        lines.insert(close, format!("{created_key}: {now}"));
        changed = true;
    }
    if !changed {
        return None;
    }
    // lines() drops the trailing newline; every document ends with one.
    let mut out = lines.join("\n");
    if content.ends_with('\n') {
        out.push('\n');
    }
    Some(out)
}

pub(crate) fn cmd_extract() {
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
            eprintln!("fatal: sidecar cleanup failed — see errors above");
            exit(1);
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
            let errs = nquad::generate_frontmatter_nquads_with(root, &ctx, walk_opts).errors;
            (errs, Some(ctx))
        }
        None => {
            let errs = generate_frontmatter_nquads(walk_opts).errors;
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
        eprintln!(
            "fatal: sidecar write-gate: {} error(s) across {} sidecar file(s). \
             An out-of-spec sidecar means the extractor produced output the \
             format spec forbids — a git-lex bug unless the message names \
             damage in the sidecar file itself. Report it.",
            gate_errors, gate_files
        );
        std::process::exit(1);
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
            eprintln!("fatal: identity gate: {} id collision(s)", id_errors);
            std::process::exit(1);
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
                if range_iri == crate::nquad::THING_CLASS_IRI {
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
            eprintln!("fatal: identity gate: {} dangling reference(s)", ref_errors);
            std::process::exit(1);
        }
    }

    if extraction_errors > 0 {
        eprintln!("fatal: {} frontmatter error(s) — fix before committing", extraction_errors);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod date_stamp_tests {
    use super::{frontmatter_kit_class, is_substantive_doc_change, stamp_frontmatter_dates};

    const NOW: &str = "2026-09-18T01:02:03-07:00";

    const DOC: &str = "---\n\
type: Journal\n\
# a teaching comment line\n\
soul.Journal.journalId: \"day-9\"\n\
soul.Journal.createdDate: 2026-08-01T09:00:00-07:00\n\
soul.Journal.updatedDate: 2026-08-01T09:00:00-07:00\n\
---\n\
\n\
# day-9\n\
\n\
body text stays byte-identical\n";

    #[test]
    fn kit_class_prefix_reads_first_dot_key() {
        assert_eq!(frontmatter_kit_class(DOC).as_deref(), Some("soul.Journal"));
        assert_eq!(frontmatter_kit_class("---\ntitle: x\n---\n"), None);
        assert_eq!(frontmatter_kit_class("# no frontmatter\n"), None);
    }

    #[test]
    fn modified_doc_gets_updated_now_and_keeps_its_birth_date() {
        let out = stamp_frontmatter_dates(DOC, "soul.Journal", NOW, None, false, false).unwrap();
        assert!(out.contains("soul.Journal.createdDate: 2026-08-01T09:00:00-07:00\n"));
        assert!(out.contains(&format!("soul.Journal.updatedDate: {NOW}\n")));
        assert!(out.ends_with("body text stays byte-identical\n"));
    }

    #[test]
    fn new_doc_gets_both_dates_set_to_now_whatever_it_held() {
        // Authored values on a new document are overwritten: created is
        // the date it was created, and that is this save.
        let out = stamp_frontmatter_dates(DOC, "soul.Journal", NOW, None, true, false).unwrap();
        assert!(out.contains(&format!("soul.Journal.createdDate: {NOW}\n")));
        assert!(out.contains(&format!("soul.Journal.updatedDate: {NOW}\n")));
        // Scaffolded empty values with teaching comments are rewritten whole.
        let scaffold = "---\nsoul.Journal.journalId: \"d\"\nsoul.Journal.createdDate: \"\"  # set by git-lex\nsoul.Journal.updatedDate: \"\"  # set by git-lex\n---\nbody\n";
        let out = stamp_frontmatter_dates(scaffold, "soul.Journal", NOW, None, true, false).unwrap();
        assert!(out.contains(&format!("soul.Journal.createdDate: {NOW}\nsoul.Journal.updatedDate: {NOW}\n---\nbody\n")));
        assert!(!out.contains("set by git-lex"));
    }

    #[test]
    fn absent_keys_are_inserted_above_the_close_created_first() {
        let doc = "---\nsoul.Note.noteId: \"n\"\n---\nbody\n";
        let out = stamp_frontmatter_dates(doc, "soul.Note", NOW, None, true, false).unwrap();
        assert_eq!(out, format!("---\nsoul.Note.noteId: \"n\"\nsoul.Note.createdDate: {NOW}\nsoul.Note.updatedDate: {NOW}\n---\nbody\n"));
        // A modified document with no createdDate does not get one invented.
        let out = stamp_frontmatter_dates(doc, "soul.Note", NOW, None, false, false).unwrap();
        assert_eq!(out, format!("---\nsoul.Note.noteId: \"n\"\nsoul.Note.updatedDate: {NOW}\n---\nbody\n"));
    }

    #[test]
    fn already_stamped_now_is_a_no_op() {
        let doc = format!("---\nsoul.Note.noteId: \"n\"\nsoul.Note.updatedDate: {NOW}\n---\nbody\n");
        assert_eq!(stamp_frontmatter_dates(&doc, "soul.Note", NOW, None, false, false), None);
    }

    #[test]
    fn substrate_stamped_into_empty_or_existing_and_left_alone_when_unknown() {
        // Empty substrate line is filled when substrate is known
        let doc = "---\nsoul.Note.noteId: \"n\"\nsoul.Note.updatedDate: x\nsoul.Note.substrate: \"\"\n---\nbody\n";
        let out = stamp_frontmatter_dates(doc, "soul.Note", NOW, Some("gemini"), false, false).unwrap();
        assert!(out.contains("soul.Note.substrate: \"gemini\"\n"));

        // Existing non-empty substrate is NEVER overwritten (#38)
        let doc_existing = "---\nsoul.Note.noteId: \"n\"\nsoul.Note.substrate: \"gemini-3.7-flash\"\nsoul.Note.updatedDate: x\n---\nbody\n";
        let out_preserve = stamp_frontmatter_dates(doc_existing, "soul.Note", NOW, Some("claude-opus-5"), false, false).unwrap();
        assert!(out_preserve.contains("soul.Note.substrate: \"gemini-3.7-flash\"\n"));
        assert!(!out_preserve.contains("claude-opus-5"));

        // Missing substrate on existing document (!is_new) is NEVER added (#38)
        let doc_missing = "---\nsoul.Note.noteId: \"n\"\n---\nbody\n";
        let out2 = stamp_frontmatter_dates(doc_missing, "soul.Note", NOW, Some("claude"), false, false).unwrap();
        assert!(out2.contains(&format!("soul.Note.updatedDate: {NOW}\n")));
        assert!(!out2.contains("substrate"));

        // Missing substrate on new document (is_new) IS added
        let out_new = stamp_frontmatter_dates(doc_missing, "soul.Note", NOW, Some("claude"), true, false).unwrap();
        assert!(out_new.contains(&format!("soul.Note.updatedDate: {NOW}\nsoul.Note.substrate: \"claude\"\n---\n")));

        // No substrate known: an existing line stays as it is, none is added.
        let out3 = stamp_frontmatter_dates(doc, "soul.Note", NOW, None, false, false).unwrap();
        assert!(out3.contains("soul.Note.substrate: \"\"\n"));
        assert_eq!(out3.matches("substrate").count(), 1);
    }

    #[test]
    fn skip_date_bump_preserves_updated_date_on_modified_doc() {
        // Document with existing updatedDate
        let doc = "---\nsoul.Note.noteId: \"n\"\nsoul.Note.updatedDate: 2026-08-22T05:02:21-07:00\n---\nbody\n";
        // When skip_date_bump is true, updatedDate is NOT bumped to NOW
        assert_eq!(stamp_frontmatter_dates(doc, "soul.Note", NOW, None, false, true), None);

        // Even with substrate present, substrate is not added to !is_new, and updatedDate is not bumped
        let doc_no_sub = "---\nsoul.Note.noteId: \"n\"\nsoul.Note.updatedDate: 2026-08-22T05:02:21-07:00\n---\nbody\n";
        assert_eq!(stamp_frontmatter_dates(doc_no_sub, "soul.Note", NOW, Some("gemini"), false, true), None);

        // But on a new doc (is_new == true), both dates are stamped even if skip_date_bump was passed
        let out_new = stamp_frontmatter_dates(doc, "soul.Note", NOW, None, true, true).unwrap();
        assert!(out_new.contains(&format!("soul.Note.updatedDate: {NOW}\n")));
    }

    #[test]
    fn non_substantive_doc_change_detection() {
        // Pure key rename dateCreated -> createdDate, dateUpdated -> updatedDate
        let old_doc = "---\n\
copia.Texture.dateCreated: 2026-08-22T05:02:21-07:00\n\
copia.Texture.id: <copia/Texture/a-guest-in-an-ordinary-morning>\n\
copia.Texture.textureId: \"a-guest-in-an-ordinary-morning\"\n\
origin: nocturne\n\
copia.Texture.dateUpdated: 2026-08-22T05:02:21-07:00\n\
---\n\
body content\n";

        let new_doc = "---\n\
copia.Texture.createdDate: 2026-08-22T05:02:21-07:00\n\
copia.Texture.id: <copia/Texture/a-guest-in-an-ordinary-morning>\n\
copia.Texture.textureId: \"a-guest-in-an-ordinary-morning\"\n\
origin: nocturne\n\
copia.Texture.updatedDate: 2026-08-22T05:02:21-07:00\n\
---\n\
body content\n";

        assert!(!is_substantive_doc_change(old_doc, new_doc));

        // Unprefixed date key rename
        let old_unprefixed = "---\ndateCreated: 2026-08-01\ndateUpdated: 2026-08-02\n---\ntext\n";
        let new_unprefixed = "---\ncreatedDate: 2026-08-01\nupdatedDate: 2026-08-02\n---\ntext\n";
        assert!(!is_substantive_doc_change(old_unprefixed, new_unprefixed));

        // Substantive change: body changed
        let body_changed = "---\ncreatedDate: 2026-08-01\nupdatedDate: 2026-08-02\n---\ndifferent text\n";
        assert!(is_substantive_doc_change(old_unprefixed, body_changed));

        // Substantive change: frontmatter value changed
        let value_changed = "---\ncreatedDate: 2026-08-01\nupdatedDate: 2026-08-03\n---\ntext\n";
        assert!(is_substantive_doc_change(old_unprefixed, value_changed));

        // Substantive change: frontmatter field added
        let field_added = "---\ncreatedDate: 2026-08-01\nupdatedDate: 2026-08-02\nextra: 1\n---\ntext\n";
        assert!(is_substantive_doc_change(old_unprefixed, field_added));
    }

    #[test]
    fn any_other_key_is_left_exactly_as_it_is() {
        // The retired spellings are just other keys now. The stamp does
        // not read them, rename them or remove them; it writes its own two
        // and leaves the document's lines alone.
        let doc = "---\nsoul.Note.noteId: \"n\"\nsoul.Note.dateCreated: 2026-07-01T08:00:00-07:00\nsoul.Note.dateUpdated: 2026-07-02T08:00:00-07:00\n---\nbody\n";
        let out = stamp_frontmatter_dates(doc, "soul.Note", NOW, None, false, false).unwrap();
        assert_eq!(out, format!("---\nsoul.Note.noteId: \"n\"\nsoul.Note.dateCreated: 2026-07-01T08:00:00-07:00\nsoul.Note.dateUpdated: 2026-07-02T08:00:00-07:00\nsoul.Note.updatedDate: {NOW}\n---\nbody\n"));
        // Same document as a new file: createdDate is written too, and the
        // old lines still stand untouched.
        let out = stamp_frontmatter_dates(doc, "soul.Note", NOW, None, true, false).unwrap();
        assert!(out.contains("soul.Note.dateCreated: 2026-07-01T08:00:00-07:00\n"));
        assert!(out.contains(&format!("soul.Note.createdDate: {NOW}\nsoul.Note.updatedDate: {NOW}\n---\n")));
    }

    #[test]
    fn no_frontmatter_is_never_stamped() {
        assert_eq!(stamp_frontmatter_dates("# plain md\n", "soul.Note", NOW, Some("gemini"), false, false), None);
        assert_eq!(stamp_frontmatter_dates("---\nsoul.Note.noteId: \"n\"\nno close\n", "soul.Note", NOW, None, false, false), None);
    }
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
mod staging_tests {
    use super::*;

    /// A throwaway repo with one committed document, built with git2 so the
    /// test never depends on the process working directory.
    struct Tmp(std::path::PathBuf);
    impl Tmp {
        fn path(&self) -> &std::path::Path { &self.0 }
    }
    impl Drop for Tmp {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    fn repo_with_one_commit(tag: &str) -> (Tmp, git2::Repository) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = Tmp(std::env::temp_dir().join(format!("glx-stage-{tag}-{}-{nanos}", std::process::id())));
        std::fs::create_dir_all(dir.path().join("Soul/Note")).unwrap();
        let repo = git2::Repository::init(dir.path()).expect("init");
        std::fs::write(
            dir.path().join("Soul/Note/a.md"),
            "---\nsoul.Note.id: <soul/Note/a>\nsoul.Note.updatedDate: 2026-09-01T00:00:00-07:00\n---\nbody\n",
        )
        .unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("Soul/Note/a.md")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("t", "t@example.invalid").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "one", &tree, &[]).unwrap();
        drop(tree);
        (dir, repo)
    }

    #[test]
    fn stage_paths_stages_every_listed_file_in_one_call() {
        let (dir, repo) = repo_with_one_commit("all");
        let root = dir.path();
        // One edit, one new file, and a name with a space and a quote in it.
        std::fs::write(root.join("Soul/Note/a.md"), "changed\n").unwrap();
        std::fs::write(root.join("Soul/Note/b it's.md"), "new\n").unwrap();
        let paths = vec![
            std::path::PathBuf::from("Soul/Note/a.md"),
            std::path::PathBuf::from("Soul/Note/b it's.md"),
        ];
        stage_paths(root, &paths).expect("stage");
        // git2 caches the index it handed out during setup; re-read the
        // file the git process just wrote.
        let mut index = repo.index().unwrap();
        index.read(true).unwrap();
        let a = index.get_path(std::path::Path::new("Soul/Note/a.md"), 0).expect("a staged");
        let staged_a = repo.find_blob(a.id).unwrap();
        assert_eq!(staged_a.content(), b"changed\n", "the edit reached the index");
        assert!(index.get_path(std::path::Path::new("Soul/Note/b it's.md"), 0).is_some(), "the new file is staged");
        // Nothing to stage is not an error.
        stage_paths(root, &[]).expect("empty list");
    }

    #[test]
    fn stage_paths_reports_a_missing_file() {
        let (dir, _repo) = repo_with_one_commit("missing");
        let err = stage_paths(dir.path(), &[std::path::PathBuf::from("Soul/Note/nowhere.md")]).unwrap_err();
        assert!(err.contains("nowhere.md"), "git's own words come back: {err}");
    }

    #[test]
    fn read_head_blob_reads_committed_file() {
        let (_dir, repo) = repo_with_one_commit("read_head");
        let head = repo.head().unwrap();
        let commit = head.peel_to_commit().unwrap();
        let tree = commit.tree().unwrap();
        let content = read_head_blob(&repo, Some(&tree), std::path::Path::new("Soul/Note/a.md")).unwrap();
        assert!(content.contains("soul.Note.updatedDate: 2026-09-01T00:00:00-07:00"));
        assert_eq!(read_head_blob(&repo, Some(&tree), std::path::Path::new("nonexistent.md")), None);
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
        let dir = Tmp(std::env::temp_dir().join(format!("glx-gate-{}-{nanos}", std::process::id())));
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
