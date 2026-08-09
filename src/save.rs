//! `git lex save` + the pre-commit gates — identity resolution, validation,
//! extraction. Extracted from main.rs (#39, task #92).

use std::process::{Command, exit};
use std::time::Instant;
use git_lex::get_kit;
use crate::nquad;
use crate::require_git_root;
use crate::nquad::generate_frontmatter_nquads;
use crate::extraction::{extract_markdown_links, frontmatter_to_turtle};
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
    ) {
        if !name.is_empty() && !email.is_empty() {
            return Some((name, email));
        }
    }

    // 2. repo.yml (the human-edited source of truth — authoritative over the
    //    settings.json cache, so editing it works WITHOUT a kit-update).
    let fields = read_repo_yml_fields(&root.join(".lex").join("repo.yml"));
    if let (Some(name), Some(email)) = (fields.get("agent_name"), fields.get("agent_email")) {
        if !name.is_empty() && !email.is_empty() {
            return Some((name.clone(), email.clone()));
        }
    }

    // 3. .claude/settings.json env block — last fallback, read through the
    //    module that WRITES that block (review #38: reader and writer of
    //    the env schema live in one file, so the schema can't drift apart
    //    across an unrelated module boundary again — the .env retirement
    //    already proved this block's location migrates).
    harness::claude::read_identity_env(&root)
}

pub(crate) fn cmd_save(message: &str, dry_run: bool) {
    let root = require_git_root();

    // Identity floor: a soul repo without its root SOUL.md must not save
    // (fail-loud, #29 — the file is restorable via kit-update).
    soul_md::require_soul_md(&root);

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


    // Add everything, commit; the pre-commit hook handles extract + validate
    // (NOT sync — the store is updated separately by `git lex sync`)
    let status = Command::new("git")
        .args(["add", "-A"])
        .status();
    if !status.map(|s| s.success()).unwrap_or(false) {
        eprintln!("fatal: git add failed");
        exit(1);
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
            println!("Saved in {}: {} [as {}]", root.display(), message, author);
        }
        _ => {
            eprintln!("fatal: git commit failed");
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
                        eprintln!("    → {}", msg);
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

/// Combined extraction + validation, called by the pre-commit hook.
/// Runs sidecar cleanup, frontmatter extraction, markdown link extraction,
/// stages artifacts, then SHACL validates. Exits non-zero if anything fails.
pub(crate) fn hook_pre_commit() {
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
    let lex_ignored = Command::new("git").args(["check-ignore", "-q", ".lex"]).status()
        .map(|s| s.success()).unwrap_or(false);
    if lex_ignored {
        println!(".lex/ is gitignored here — extraction artifacts stay local, not staged.");
    } else {
        let staged = Command::new("git").args(["add", ".lex/extract/"]).status()
            .map(|s| s.success()).unwrap_or(false);
        if !staged {
            eprintln!("fatal: failed to stage extraction artifacts (.lex/extract/)");
            exit(1);
        }
    }

    // Phase 2: SHACL validation
    if !cmd_validate() {
        exit(1);
    }
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

    // Run frontmatter extraction (writes .spo sidecars as a side effect).
    // The context is built here and shared with the identity gate below.
    let ctx_root = git_lex::find_git_root();
    let (_nq, mut extraction_errors, extract_ctx) = match &ctx_root {
        Some(root) => {
            let ctx = nquad::ResolverContext::build(root);
            let (nq, errs) = nquad::generate_frontmatter_nquads_with(root, &ctx);
            (nq, errs, Some(ctx))
        }
        None => {
            let (nq, errs) = generate_frontmatter_nquads();
            (nq, errs, None)
        }
    };

    // Run markdown link extraction via tree-sitter. Its errors join the
    // save gate (review #23): an unextractable doc keeps a stale sidecar.
    extraction_errors += extract_markdown_links();

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
            let mut stack = vec![root.join(".lex").join("extract")];
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
                let Some(range_iri) = ctx.ref_ranges.get(&format!("{}/{}", segs[0], segs[2])) else { continue };
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
                    if let Some(target) = nquad::thing_iri_from_range(range_iri, &val) {
                        if !owners.contains_key(&target) {
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
