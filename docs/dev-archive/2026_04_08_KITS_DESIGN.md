# Kits Design

**Status:** DRAFT — in progress
**Started:** 2026-04-08
**Author:** lux + claude (working session)

This doc defines what a git-lex kit is, what it contains, what it doesn't, and how `git lex init` / `git lex kit update` should behave. It will be edited as decisions are made during the design walkthrough. Final version will be copied to the squad repo as the canonical reference.

## 1. What is a kit?

A kit is the **minimum viable use-case definition** for a use case.

A kit declares:
- An ontology (what shapes of data are valid for this use case)
- The assets required to produce data of those shapes
- The configurations that make a fresh repo behave like "a thing of this type"

Everything inside a kit exists in service of making the resulting repo **queryable and interoperable with other repos of the same kit**.

### 1.1 The interoperability test

When evaluating whether X belongs in a kit, ask:

> Does X exist to make the data shape of this repo conform to the kit's definition?

If yes → it's kit material. If no → it probably belongs somewhere else (a harness adapter, a personal config, an experiment).

### 1.2 What kits should be

- **Useable by any model** — the soul kit should produce a working soul whether the agent is Claude, Gemini, or future X. *Open question: how. See §5.*
- **Easily understood and well documented** — kit developers (future audience) must be able to grok the format.
- **Mostly permanent** — kit schema is stable; content can evolve but format changes are rare.
- **Easily usable from standard locations** — `github.com/repolex-ai/git-lex-kit-{name}` by default, but pullable from anywhere.
- **Integrated with git-lex** — `git lex init --kit {name}` is the primary entry point.
- **Representative of a base use case behavior** — a kit captures "what it means to be a soul / squad / library / etc."

---

## 2. The four layers

A kit spans four conceptually distinct layers. The scaffold/ directory in the current soul kit muddles layers 2 and 3, which was the source of earlier complexity. Keeping them separated is load-bearing for the design.

### Layer 1 — Ontology (the use-case definition)

- TTL files defining classes and properties
- SHACL shapes (derived from TTL)
- Class templates (`__Memory.md`, etc., derived from SHACL)

**Lives at:** `.lex/ontology/kit/{name}/`
**Update behavior:** Always regenerated from upstream on `kit update`. This is already how it works.
**Who owns it:** The kit, absolutely.

### Layer 2 — Use-case assets

Files the kit *prescribes must exist* for the use case to function. The kit maintains canonical versions; updates propagate.

Examples (soul kit):
- `AGENTS.md` — how the agent uses the kit (rehydration protocol, save protocol, etc.)
- `SOUL.md` — agent identity template (filled in at init time)
- `.env` skeleton — for git identity
- `README.md` or `README.lex.md` — human-facing repo description
- For a hypothetical library kit: baked-in skills for URL fetching, summarization, etc.

**Open question:** Where do these live in the *kit* repo (source)? Options:
- Inside `scaffold/` alongside harness files (current approach, muddled)
- In a sibling `assets/` dir (proposed, cleaner separation)

**Open question:** Where do these land in the *target* repo? Currently all root-level. Probably correct.

### Layer 3 — Harness adapters

Glue that makes the kit work with a specific model runtime.

Examples:
- `.claude/settings.json` — Claude Code session hooks
- `.claude/hooks/SessionStart.sh` — startup script
- Hypothetical `.gemini/config.yaml` — Gemini equivalent
- Hypothetical `.openai/...` — OpenAI equivalent

**Key property:** These exist because *without them the agent can't run the kit on this harness*, but they're **about the harness, not the kit's use-case definition**. A Claude settings.json doesn't say anything about what a soul *is* — it just makes the Claude runtime behave soul-ishly.

**Open design question (see §5):** How should harness adapters be organized? Inside the kit per-harness, or as a separate install concept? *Deferred.*

### Layer 4 — Agent content

What the agent produces by using the kit. Memories, decisions, journal entries, adaptive skills, custom work.

**Lives at:** repo root, in type folders (`memory/`, `decision/`, etc.)
**Update behavior:** Kit never touches.
**Who owns it:** The agent.

---

## 3. Managed vs. content — the simple rule

Dropping the word "owned" in favor of **managed** vs. **content**:

- **Kit-managed files**: Layers 1, 2, 3. The kit maintains a canonical version. `git lex kit update` restores/updates them from the latest kit. The agent can always edit them, but doing so means the next `kit update` will overwrite — which is fine, because:
  - The agent's prior version is in git history
  - `git diff HEAD~1` shows what the update changed
  - `git revert` or manual merge is the escape hatch

- **Agent content files**: Layer 4. Kit never touches. Write-once if scaffolded at init, otherwise agent-created.

**No manifest. No hash tracking. No --force flag.** Git is the history mechanism, git diff is the inspection mechanism, git revert is the rollback mechanism. git-lex doesn't reinvent any of this.

---

## 4. Proposed kit repo layout

Current (muddled):
```
git-lex-kit-soul/
  kit.yml
  soul.ttl
  scaffold/
    .claude/            ← harness adapter (layer 3)
    AGENTS.md           ← use-case asset (layer 2)
    SOUL.md             ← use-case asset (layer 2)
    .env                ← use-case asset (layer 2)
    journal/            ← vestigial?
```

Proposed (separated):
```
git-lex-kit-soul/
  kit.yml
  soul.ttl              ← layer 1 (ontology)
  assets/               ← layer 2 (use-case assets)
    AGENTS.md
    SOUL.md
    .env
    README.md
  harness/              ← layer 3 (harness adapters)
    claude/
      .claude/
        settings.json
        hooks/SessionStart.sh
  scripts/              ← optional side-channel (e.g. Kira listener probe)
    soul-listener.py
```

**Open question:** Is `harness/` per-kit (each kit has its own `harness/claude/`) or is there a separate `git-lex-harness-claude` repo that every kit references? *Deferred — see §5.*

---

## 5. Open design questions (to resolve during this session or later)

### 5.1 Harness adapter model

*See §2 layer 3.* Two candidate shapes:

- **A. Per-kit harness subdirs.** Each kit ships its own `harness/{claude,gemini,...}/` tree. Burden on kit authors to maintain N adapters. Cleaner today, possibly painful at scale.
- **B. Separate harness concept.** `git lex harness install claude` pulls from `repolex-ai/git-lex-harness-claude`. Kit authors declare abstract capabilities ("needs a session-start hook") and harness adapters translate. More up-front work, cleaner separation.

**Status:** Deferred. Do not hardcode `.claude/` throughout git-lex source — keep it contained so we can migrate later.

### 5.2 Skills — baked-in vs adaptive

Two kinds of skills:
- **Baked-in** (library kit style): kit ships required skills, updates propagate, agent doesn't author them
- **Adaptive** (soul kit style): agent creates/evolves skills in the content area

Possible unification: `.claude/skills/` (or harness equivalent) always symlinks to content-area `skill/` folder. Skills marked `soul.skill.managed: true` in frontmatter are kit-managed (update on `kit update`); others are agent content.

**Status:** Candidate pattern. Revisit after walking through init.

### 5.3 Naming flow for new agents

`git lex init --kit soul` currently doesn't name the agent. That's a problem because:
- SOUL.md, .env, AGENTS.md all need the name substituted in
- Agents end up with names like `?M4RQ` that don't work as repo names / email local-parts / identifiers

Proposed: init asks for a name, validates it (lowercase, no special chars, matches `[a-z][a-z0-9-]*`), fails fast if invalid. This happens before any templated file is written.

**Status:** Agreed in principle. Needs to land during init walkthrough (§6).

### 5.4 README conflict

`git lex init` in an existing repo may find a pre-existing README.md. Current workaround: write README.lex.md. Problem: GitHub doesn't show it by default.

Options:
- Ask at init time ("take over README.md? existing will be moved to README.original.md")
- Only scaffold README if none exists; otherwise skip entirely
- Always write README.lex.md and let users manually rename

**Status:** Minor compared to other questions. Defer.

### 5.5 journal/ folder

Currently in soul kit scaffold. lux doesn't remember why.

**Status:** Candidate for deletion. Confirm and remove during cleanup.

---

## 6. Walkthrough: `git lex init --kit soul`

*To be filled in during the design walkthrough session.*

### 6.1 Current behavior (as of 2026-04-08)

*TODO: document step-by-step what cmd_init does today*

### 6.2 Desired behavior

*TODO: document the target behavior post-redesign*

### 6.3 Delta

*TODO: what needs to change*

---

## 7. Walkthrough: `git lex kit update`

*To be filled in after §6. The update flow is downstream of the init flow.*

---

## 8. Command audit

*TODO: walk through every `git lex` subcommand and classify as:*
- *Load-bearing (keep)*
- *Dev-mode scratch (keep but mark)*
- *Duplicative (merge or delete)*
- *Unknown purpose (investigate)*

*lux flagged this: "I noticed that there are a lot in there, but I don't even know what they do or what they are for."*

---

## 9. Decisions made

*Running log of decisions reached during the design session. Timestamped.*

- **2026-04-08** — Four-layer model adopted as the conceptual frame.
- **2026-04-08** — No manifest, no hash tracking, no --force flag. Git's primitives are the source of truth for file state and history.
- **2026-04-08** — "Managed" vs "content" naming, dropping "owned."
- **2026-04-08** — Harness adapter model deferred; don't foreclose separate-concept option.
- **2026-04-08** — Previous manifest+force code in `src/main.rs` to be reverted.
- **2026-04-08** — **Kit landing path**: kits land at `.lex/kit/` (not `.lex/ontology/kit/{name}/`). One kit per repo, single-tenant by construction. The `.lex/kit/` directory is wholly owned by the kit installer — no other files live there. On `kit update` or any kit change, `rm -rf .lex/kit/` and re-fetch fresh. No partial merges, no leftover files.
- **2026-04-08** — **Kit identity**: `repo.yml`'s `kit:` field stores the full `org/repo` form (e.g. `repolex-ai/git-lex-kit-soul`). Short forms (`soul`) resolve to `repolex-ai/git-lex-kit-{name}` as a convenience. `--kit org/repo` syntax allows installing third-party kits from any GitHub repo.
- **2026-04-08** — **`.gitattributes` scoping**: don't write to repo-root `.gitattributes`. Write a nested `.gitattributes` at `.lex/extract/.gitattributes` containing just `*.nq diff=lex merge=lex`. This keeps git-lex's diff/merge rules invisible from the repo root and ensures they only apply where extraction sidecars actually live.
- **2026-04-08** — **`.gitignore` scoping**: don't write to repo-root `.gitignore` for the universal case. Write a nested `.lex/.gitignore` containing just `oxigraph/`. Kit-specific root-level gitignore needs (e.g. claude-code's whitelist) are declared by the kit shipping a literal `.gitignore` file in `assets/` which git-lex copies to the repo root during init. The hardcoded `if kit_name == "claude-code"` branch in `cmd_init` gets deleted.
- **2026-04-08** — **Dead code**: `.lex/raw/` is referenced nowhere in git-lex source except the two gitignore strings. It's a vestige with no implementation behind it. Remove all references during the cleanup pass.
- **2026-04-08** — **`.lex/repo.yml` shape**: drop the hardcoded `version: "1.0"` field (never used). Drop any agent-related fields from repo.yml — agent identity belongs in SOUL.md, not here. Add `kit_sha:` recording the kit's HEAD SHA at install time, fetched via `git ls-remote {url} HEAD` (cheap, no clone, no working tree). The `kit:` field stores the full `org/repo` form. The `created:` timestamp uses UTC ISO-8601 to match `identity.yml`.
- **2026-04-08** — **No kit cloning**: `.lex/kit/` is a flat directory of files, never a git checkout. Fetch via curl+tar (unchanged). No `.git/` inside `.lex/kit/`. Agents have no clone-pull-switch surface area to fiddle with. Re-fetching is the only update mechanism.
- **2026-04-08** — **`.lex/README.md` purpose**: anti-deletion signage only. Pure prose explaining what `.lex/` is and why deleting it is bad. No directory map, no kit name, no settings — nothing that would go stale and need maintenance. One short paragraph about git-lex + a "don't delete this" warning + a `git lex` pointer. That's the whole file.
- **2026-04-08** — **Type folders from ontology, not scaffold**: kits MUST NOT ship empty type folders in `scaffold/`. The ontology is the single source of truth — step 8 of init creates folders from the ontology classes. Soul kit's `scaffold/journal/` is redundant and should be deleted. Same rule applies to any other ontology-class folders that may have been mistakenly added to scaffolds.
- **2026-04-08** — **README handling**: two-tier. (1) git-lex has a default base-case README content for a bare init. If `README.md` doesn't exist in the repo, write `README.md`; if it does exist, write `README.lex.md` and print a note that it was created as `.lex.md` because README.md already existed. (2) If the installed kit ships a `README.md` as an asset (in the kit's `assets/` dir, not the kit repo root), that kit README replaces the default base-case content — same exists/doesn't-exist logic. The current dynamically-generated `README.lex.md` content is too much (TMI). The base-case content should be a small note about git-lex; rich docs belong in the kit's asset README.
- **2026-04-08** — **SHACL shape generation (step 10) is fine as-is**. Mostly works. Agent-facing docs need better Journal date guidance — agents struggle with the date field format. Belongs in the AGENTS.md rewrite, not in shape generation code.
- **2026-04-08** — **Class templates (step 11) — TODO, defer**. Current `__ClassName.md` files put structure in front of the agent but feel ugly. Functional but unloved. Leave for now, revisit later. Tagged TODO.
- **2026-04-08** — **`init` is single-shot and destructive**. Init initializes a fresh repo. If `.lex/` already exists, init prompts the user with a yes/no question (default no): "This repo is already initialized. Re-initializing will delete .lex/ and overwrite scaffold files. Continue?" If yes, init nukes `.lex/` and proceeds as if fresh. If no, exits cleanly. No `--force` flag — it's a yes/no question, not a command-line option.
- **2026-04-08** — **Init overwrites scaffold files**. When init runs (always against a fresh repo per the rule above), it overwrites any pre-existing files in the repo with the kit's scaffold versions. No "skip if exists" logic. This fixes the current "init isn't overwriting" bug. The kit's version wins because there are no customizations to preserve in a fresh repo by definition.
- **2026-04-08** — **Init prompts for kit-declared variables**. Kits declare variables they need (agent name, squad name, etc.) in `kit.yml`. Init walks this list and prompts the user for each, validates against a kit-supplied regex, substitutes into templated files using the existing `{varname}` substitution mechanism (generalized from `{kit}`). Variables can also be passed via `--var name=value` flags for non-interactive use. Variables are not persisted anywhere; they're substituted at init time and forgotten — read SOUL.md / repo.yml / etc. for the values later.
- **2026-04-08** — **Init scaffold flow rewrite**: replace step 12's `install_scaffold_files` (the never-overwrite walker) with a destructive overwrite walker that runs after variable collection. This is a much simpler function than what was previously built — no manifest, no hash check, no skip logic. Just walk the scaffold tree and copy.
- **2026-04-08** — **Update is separate**: scaffold updating is `git lex kit update`'s problem, not init's. We'll design that separately after the init walkthrough is done. Init no longer needs to worry about "what if files already exist" because init only runs on fresh repos.
- **2026-04-08** — **Init summary print (step 13) needs refresh**: drop the "Reinitialized" branch (init is single-shot now). Update the path list to reflect the new layout — `.lex/repo.yml`, `.lex/extract/`, `.lex/ontology/`, `.lex/kit/`, content folders from step 8. Drop the root `.gitattributes` line (now nested at `.lex/extract/.gitattributes`).
- **2026-04-08** — **Git identity stays in `.env`**: agents are portable. `--local` git config doesn't travel with checkouts; only tracked files do. Keep the existing model: kit ships a `.env` template with `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL` placeholders, init substitutes the collected agent name variable, the SessionStart hook sources `.env` to set identity per-session. No new init step for git config.
- **2026-04-08** — **`.env` handling differs across harnesses**: Claude Code doesn't auto-load `.env`, which is why the soul kit's harness adapter has a SessionStart hook that explicitly sources it. Gemini auto-loads `.env` natively. Other harnesses (OpenAI, Grok, etc.) unknown. Relevant for the harness-adapter design (§5.1) — the `.env` sourcing code is Claude-specific and shouldn't be assumed to apply elsewhere.
- **2026-04-08** — **Git hooks ship in the kit, not hardcoded**: step 16's `if kit == "squad" || kit == "lab"` branch is wrong. Kits that need git hooks (post-commit, post-merge, post-receive, etc.) ship them as part of the kit. Init copies them into `.git/hooks/` and overwrites any existing hook file with a printed warning. The hardcoded branch goes away. Where they live in the kit layout deferred — probably `scaffold/` with a special destination convention for `.git/hooks/*`, decided when kit layout is restructured.
- **2026-04-08** — **Merge `identity.yml` into `repo.yml`**: delete `.lex/identity.yml` entirely. Add a `first_commit:` field to `repo.yml` storing the first-commit SHA (the cryptographic anchor used by `cmd_identity` and `cmd_join` for squad bindings). Removes duplication of `kit:` and `created:` fields between the two files. `read_identity()` reads from `repo.yml`. `cmd_identity` still works.
- **2026-04-08** — **`kit.yml` `init_prompts:` section**: kits declare an `init_prompts:` list of variable names (just names, no validation or prompt text). Init asks the user for each, stores results in `.lex/repo.yml`. No regex, no kit-supplied prompts — keep it minimal. Convention-over-configuration for everything else: presence of `scaffold/`, `assets/README.md`, etc. in the kit *is* the declaration; kit.yml doesn't enumerate them.
- **2026-04-08** — **Delete `git lex kit update`**: `init` and `update` were doing nearly the same operation. Make `init` idempotent instead — runs fresh on a fresh repo, runs as an update on an existing one. The only differences are the three "one-shot" things: skip `init_prompts` collection if `repo.yml` already has the values, skip `first_commit:` capture if already set, skip the "commit existing content" prompt if not applicable. Everything else (fetch kit, regenerate shapes/templates, copy scaffold) runs identically. The `KitCommands::Update` variant and `cmd_kit_update` get deleted entirely.
- **2026-04-08** — **Delete `git lex kit list`**: hardcoded printout of three kit names with descriptions. No dynamic discovery, no value. Delete `cmd_kit_list`. The entire `Kit` subcommand and `KitCommands` enum can go.
- **2026-04-08** — **Delete `git lex identity`**: not a real command anyone would use. Just printed `.lex/identity.yml`. Once that file is merged into `repo.yml`, the SHA is visible via `cat .lex/repo.yml`. The underlying `read_identity()` helper stays (used by `cmd_join` for squad bindings), but it reads from `repo.yml` instead. Delete `cmd_identity` and the `Identity` enum variant.
- **2026-04-08** — **Delete `cmd_log`, `cmd_tree`, `cmd_refs`**: standalone n-quad emitters with no internal callers. Once `sync` populates oxigraph, the same data is queryable via `git lex query`. The shared `generate_git_nquads()` helper stays (used by `sync`).
- **2026-04-08** — **Delete `git lex diff` command and all dead semantic-diff scaffolding**: never implemented. The user-facing `Diff { since }` command just prints "not yet implemented." The `.gitattributes` install (step 4) and the global `git config` install (step 14) both register `git-lex diff-driver` and `git-lex merge-driver` as drivers — but those subcommands have never existed. Calling them would error. Delete all of it: the `Diff` enum variant, the `.gitattributes` install, the `install_global_drivers()` call, the `cmd_status` lines that print the (nonexistent) driver config.
- **2026-04-08** — **Plain `git diff` is sufficient for `.spo` files**: extracted `.spo` sidecars are already sorted, line-oriented, human-readable text (`subject | predicate | object` format). Git's built-in line diff shows added/removed triples cleanly. No semantic diff driver needed. The intended job of "notice changes in extracted knowledge" is solved by .spo files + git diff. Revisit later only if a real need surfaces.
- **2026-04-08** — **Drop "contract" / "data contract" language**: not lux's framing. Use "use-case definition" or just describe what kits do directly. Kits exist to make repos of a given type behave consistently and produce queryable data — that's the value, no need for contract metaphors.

---

## 10. Next actions

1. Walk through `cmd_init` together (see §6.1)
2. Decide on asset/harness/script layout for the soul kit (see §4)
3. Revert the manifest/force code in `src/main.rs`
4. Rewrite scaffold sync as the simple version
5. Command audit (see §8)
6. Copy final doc to squad repo
