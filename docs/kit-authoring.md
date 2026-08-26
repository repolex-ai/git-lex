# Kit Authoring Guide

*Last updated for git-lex v0.1.0 (2026-08-12)*

This guide is for people **building or maintaining kits** — not for agents using
them. It covers the kit layout, the file-ownership rules, and (in the most
detail) hooks: how they're named, how they're registered, how to develop one,
and how to ship it.

---

## 1. What a kit is

A kit is a GitHub repo (e.g. `repolex-ai/git-lex-kit-soul`) that ships four
kinds of things:

| Layer      | Folder in the kit repo      | Installs to (agent repo)         | What it is |
|------------|-----------------------------|----------------------------------|------------|
| Ontology   | `ontology/<kit>/`           | `.lex/ontology/<kit>/`           | The kit's vocabulary: classes, properties (`.ttl`). |
| Content    | `content/`                  | repo root                        | Class folders, document templates, starter docs. |
| Harness    | `harness/`                  | repo root (`.claude/…`, `AGENTS.md`, …) | Substrate wiring: hooks, skills, agent instructions. |
| Www        | `www/`                      | `.lex/www/`                      | Static site assets (GitHub Pages). |
| Reference  | `reference/`                | *ships, does not install*        | Material that must travel with the kit and stay current, but must never enter a repo's graph or root. |

**The `reference/` layer is the odd one.** Only `ontology/<kit>/` is mirrored
into `.lex/ontology/`, so anything outside it rides along in
`.lex/kit/<org>/<repo>/` without being loaded, scaffolded, or validated. That
buys automatic freshness — every `kit-update` refetches it — with no risk of
polluting the graph.

kit-base uses it for the ontology-authoring standard:

- `reference/EXAMPLE-KIT.ttl` — how to write a kit ontology, as a **working
  Turtle file** rather than a snippet, so it parses and can be tested and cannot
  rot into something invalid unnoticed. Versioned separately, with its own
  changelog.
- `reference/check-kit-ontology.py` — runs the mechanical half of that
  standard's checklist against any kit TTL.

If you are writing or changing a kit ontology, start there.

The ontology path is a pinned contract: a kit ships
`ontology/<kit>/<kit>.ttl` and it lands at `.lex/ontology/<kit>/<kit>.ttl`,
where downstream consumers may rely on it by that path alone. (An older
`scaffold/` layer installing to the repo root is still accepted for
pre-migration kits.)

The kit's **short name** comes from the repo name: `git-lex-kit-soul` → `soul`.
Its SPARQL prefix and namespace come from the prefix declaration in its own
TTL.

`kit.yml` at the kit root declares the fields git-lex reads:

- `scope:` — `base`, `domain`, or `optional`. Missing means `domain`.
  `kit-add` only accepts `optional`; base and domain kits install via
  `git lex init`.
- `folder base:` — the repo-root folder that holds the kit's class folders
  (e.g. `Soul`). Without it, class folders land at the repo root and the
  folder audit is skipped.
- `init_prompts:` — variable names `git lex init` prompts the user for.

## 2. File ownership: what kit-update does to your files

**Every file a kit ships converges to the kit's version on `git lex
kit-update`.** The rules, in full:

- File missing locally → installed.
- File identical to the kit's → nothing happens.
- File differs from the kit's → **the kit's version is put in place.** No
  backup file — these are tracked files in a git repo, and git history *is*
  the backup. You are told exactly which files this happened to.
- `SOUL.md` → **never overwritten.** Identity belongs to the agent, not the
  kit. (If it's *missing*, the kit's scaffold restores it, and kit-update
  self-heals its `soulId` from the repo's genesis commit.)

There is no `--force`, no drift sidecar, no `.bak` stash. Any `<file>.bak`
left beside a kit-owned path by the retired backup mechanism is swept on the
next kit-update. If a *directory* sits where the kit ships a file, git-lex
refuses loudly and leaves it untouched. And if two installed kits ship the
same path, kit-update warns — that's a kit-lane bug; exactly one kit should
own a file.

**Consequence for kit authors:** do not ship a file you expect agents to
customize. If agents need a customization point, give them a separate file
that the kit does *not* ship (like `settings.local.json`, or `SOUL.md`).

### 2.1 A shared working tree is a shared commit

The rules above are about the kit overwriting *you*. This one is about your
co-tenant, and it surprises people because nothing goes wrong.

**`git lex save` commits the whole working tree.** So when two agents work in the
same repo, **whoever saves first sweeps up the other's uncommitted work** — under
their own name, inside a commit message about something else entirely.

Nothing fails. No conflict, no warning, no lost code, clean tree afterwards. What
you lose is attribution and traceability: the history says someone authored a
change they have never seen, and `git log -S` on that file leads to a commit
about an unrelated subject. That combination — correct outcome, wrong record, no
error — is why it can run for weeks unnoticed.

Observed 2026-08-24 in `repolex-ai/copia`: a two-day-old uncommitted change was
already in the history, swept into another agent's save under a message about a
different pass.

**If you share a repo:** commit your own work before stepping away from it, and
read what `save` reports it staged rather than assuming it staged yours. The
per-file collision people worry about is the easy case — this is the one that
doesn't announce itself.

## 3. Hooks

### 3.1 Where hooks live in the kit

```
<kit-repo>/harness/.claude/hooks/<Event>-<kit>-<purpose>.sh
```

kit-update copies them to `.claude/hooks/` in the agent repo and registers
them in `.claude/settings.json` automatically. You never hand-edit
registrations for kit hooks.

### 3.2 Naming: `<Event>-<kit>-<purpose>.sh`

- `<Event>` is a real Claude Code hook event, exact CamelCase:
  `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`,
  `PreCompact`, `SessionEnd`, … (the registrar carries the full documented
  list; a misspelled event is a **hard error** at kit-update — git-lex refuses
  to register a hook that would never fire).
- `<kit>` is your kit's short name. Its job is to make the filename unique so
  several kits can ship hooks for the same event.
- `<purpose>` is a short word for what it does.

Examples that ship today: `SessionEnd-soul-save.sh`,
`UserPromptSubmit-soul-recall.sh`, `Stop-pool-moment.sh`.

**Multiple kits, same event — yes, this works and is the normal case.**
git-lex splits the filename on the first `-` to find the event (Claude Code
events never contain a hyphen, so this is unambiguous), and registers each
file under it. `UserPromptSubmit-soul-recall.sh` and
`UserPromptSubmit-pool-share.sh` both fire on `UserPromptSubmit`; Claude Code
runs all registered entries for an event.

### 3.3 `.sh` only

**A hook must be a `.sh` file. Nothing else registers.** A `.py` (or anything
else) placed in `hooks/` is copied but silently never fires — git-lex's
registrar only parses `*.sh`.

If your hook logic is Python: ship the `.py` alongside and call it from a
thin `.sh` wrapper. The `.sh` is the hook; the `.py` is a helper.

One consequence of the naming rule: you cannot put a shared library like
`hook-common.sh` in `.claude/hooks/` — its leading segment isn't an event, so
kit-update hard-errors. Inline any shared logic into each hook script.

### 3.4 settings.json vs settings.local.json

| | `.claude/settings.json` | `.claude/settings.local.json` |
|---|---|---|
| Git | **committed** (travels with the repo) | **gitignored** (this machine only) |
| Owner | **git-lex** — rewritten/converged on every kit-update | **you** — git-lex never writes it |
| Contains | hook registrations + git identity env (+ auto-memory dir in soul repos) | personal overrides, `soul.disabledHooks` |

Claude Code merges both at load (hooks from both files all fire; for
conflicting scalar settings, local wins). Hand-edits to the managed blocks of
`settings.json` are reverted on the next kit-update — that's the convergence
working as designed.

### 3.5 The hook development flow

**Step 1 — develop as a local hook.** In your own repo:

1. Write `.claude/hooks/<Event>-local-<purpose>.sh` (the literal word `local`
   as the second segment).
2. Run `git lex kit-update` — it registers **every** valid hook file in
   `.claude/hooks/` under its event in `settings.json`, `-local-` ones
   included. No hand-registration needed (hand-registering the same script in
   `settings.local.json` too would make it fire twice).
3. Iterate freely. `-local-` hooks are protected: kit-update never removes,
   overwrites, or reaps them.

**Step 2 — promote to the kit** when it's proven:

1. Rename: `-local-` → `-<kit>-` (e.g. `UserPromptSubmit-local-mything.sh` →
   `UserPromptSubmit-pool-mything.sh`).
2. Add the file to the kit repo at `harness/.claude/hooks/`.
3. Delete your local copy — its now-dangling registration is pruned
   automatically on the next kit-update.
4. Every agent's next `git lex kit-update` installs and registers it. Done —
   no per-agent setup, ever.

**To disable a kit hook on one machine** (without forking the kit): add its
basename to `soul.disabledHooks` in `settings.local.json`:
```json
{ "soul": { "disabledHooks": ["UserPromptSubmit-soul-recall"] } }
```
The hook stays registered but no-ops locally. This survives every kit-update.

### 3.6 What kit-update enforces (the reap)

After installing all kits, kit-update removes any `.claude/hooks/*.sh` that is
**neither shipped by an installed kit nor named `-local-`** (old copy kept as
`<file>.bak` — a reaped personal hook may be uncommitted work, the one case
git history can't cover — and its registration pruned). This is what cleans up
renamed and retired hooks automatically. If any installed kit's install dir is
missing, the reap is skipped loudly rather than guessed from a partial set.
The rule to remember:

> A hook file is either **kit-shipped** or **`-local-`**. Anything else gets
> removed on the next kit-update.

## 4. Skills and agent instructions

Same convergence rules as everything else. Kits ship skills under
`harness/.claude/skills/` (and `Skill/`), agent instructions as `AGENTS.md` /
`.claude/CLAUDE.md`. All of it converges to the kit's version on kit-update
(the old bytes live in git history). An agent wanting a custom skill creates
a **new** skill file the kit doesn't ship, rather than editing a kit-shipped
one (the edit would be reverted).

## 5. kit-update, end to end

`git lex kit-update` refreshes every installed kit; `git lex kit-update <kit>`
narrows only the *fetch*. **An argument may narrow what is fetched, never what
is rebuilt** — derived artifacts are always regenerated for every installed
kit, because the ontology mirror rewrite deletes every kit's generated shapes
on every run.

One run does, in order:

1. **Fetch** each kit in the fetch scope fresh from GitHub — bails hard if any
   fetch fails (never operates on a partial set).
2. **Install/converge** each fetched kit's files per the §2 rules (and sweep
   retired `.bak` files).
3. **Reap** hook files no kit ships (§3.6), and sweep debris from retired
   mechanisms (`*.kit-latest`, legacy `.env`, retired repo.yml keys).
4. **Reconcile the substrate**: prune registrations pointing at deleted hook
   files, register every current hook file under its event, re-assert the git
   identity env block.
5. **Mirror the ontology**: converge `.lex/ontology/` to exactly what the
   installed kits ship — orphaned kit dirs are reaped, generated
   `*-shapes.ttl` are deleted with the rest.
6. **Regenerate derived artifacts for every installed kit**: SHACL shapes,
   class folders + `__ClassName.md` templates, and the folder-vs-ontology
   audit (missing/extra folder warnings, a reap of retired class folders that
   hold nothing but scaffold, and a receipt naming deprecated-class folders
   still on disk — content is never deleted).
7. **Converge the git pre-commit hook**, self-heal `SOUL.md`'s `soulId`, and
   reload the ontology graph in the store.
