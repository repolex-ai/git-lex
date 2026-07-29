# Kit Authoring Guide

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
| Ontology   | `ontology/`                 | `.lex/ontology/<kit>/`           | The kit's vocabulary: classes, properties, SHACL shapes (`.ttl`). |
| Content    | `content/`                  | repo root                        | Class folders, document templates, starter docs. |
| Harness    | `harness/`                  | repo root (`.claude/…`, `AGENTS.md`, …) | Substrate wiring: hooks, skills, agent instructions. |
| Www        | `www/`                      | `.lex/www/`                      | Static site assets (GitHub Pages). |

`kit.yml` at the kit root declares `name:` (the short name, which becomes the
namespace prefix) and `scope:` (`base`, `domain`, or `optional` — `kit-add`
only accepts `optional`).

## 2. File ownership: what kit-update does to your files

**Every file a kit ships converges to the kit's version on `git lex
kit-update`.** The rules, in full:

- File missing locally → installed.
- File identical to the kit's → nothing happens.
- File differs from the kit's → **your copy is renamed `<file>.bak` and the
  kit's version is put in place.** You are told exactly which files this
  happened to. No diffing, no deciding — if a kit ships it, the kit owns it.
- `SOUL.md` → **never overwritten.** Identity belongs to the agent, not the kit.

There is no `--force`, no drift sidecar, no stash folder. If you need the old
version back, it's sitting next to the file as `.bak` (and in git history).

**Consequence for kit authors:** do not ship a file you expect agents to
customize. If agents need a customization point, give them a separate file
that the kit does *not* ship (like `settings.local.json`, or `SOUL.md`).

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
git-lex splits the filename on the first `-` to find the event, and registers
each file under it. `UserPromptSubmit-soul-recall.sh` and
`UserPromptSubmit-pool-share.sh` both fire on `UserPromptSubmit`; Claude Code
runs all registered entries for an event.

### 3.3 `.sh` only

**A hook must be a `.sh` file. Nothing else registers.** A `.py` (or anything
else) placed in `hooks/` is copied but silently never fires — git-lex's
registrar only parses `*.sh`.

If your hook logic is Python: ship the `.py` alongside and call it from a
thin `.sh` wrapper. The `.sh` is the hook; the `.py` is a helper.

### 3.4 settings.json vs settings.local.json

| | `.claude/settings.json` | `.claude/settings.local.json` |
|---|---|---|
| Git | **committed** (travels with the repo) | **gitignored** (this machine only) |
| Owner | **git-lex** — rewritten/converged on every kit-update | **you** — git-lex never touches it |
| Contains | kit hook registrations + git identity env | personal hook registrations, local overrides |

Claude Code merges both at load (hooks from both files all fire; for
conflicting scalar settings, local wins). Hand-edits to the managed blocks of
`settings.json` are reverted on the next kit-update — that's the convergence
working as designed.

### 3.5 The hook development flow

**Step 1 — develop as a local hook.** In your own repo:

1. Write `.claude/hooks/<Event>-local-<purpose>.sh` (the literal word `local`
   as the middle segment).
2. Register it yourself in `.claude/settings.local.json`:
   ```json
   { "hooks": { "UserPromptSubmit": [ { "hooks": [ { "type": "command",
     "command": "bash \"$CLAUDE_PROJECT_DIR/.claude/hooks/UserPromptSubmit-local-mything.sh\"" } ] } ] } }
   ```
3. Iterate freely. `-local-` hooks are protected: kit-update never removes,
   overwrites, or unregisters them.

**Step 2 — promote to the kit** when it's proven:

1. Rename: `-local-` → `-<kit>-` (e.g. `UserPromptSubmit-local-mything.sh` →
   `UserPromptSubmit-pool-mything.sh`).
2. Add the file to the kit repo at `harness/.claude/hooks/`.
3. Delete your local copy and its `settings.local.json` registration.
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
`<file>.bak`, its registration pruned). This is what cleans up renamed and
retired hooks automatically. The rule to remember:

> A hook file is either **kit-shipped** or **`-local-`**. Anything else gets
> removed on the next kit-update.

## 4. Skills and agent instructions

Same convergence rules as everything else. Kits ship skills under
`harness/.claude/skills/` (and `Skill/`), agent instructions as `AGENTS.md` /
`.claude/CLAUDE.md`. All of it overwrites on kit-update (old copy → `.bak`).
An agent wanting a custom skill creates a **new** skill file the kit doesn't
ship, rather than editing a kit-shipped one (the edit would be reverted).

## 5. kit-update, end to end

For the record, one `git lex kit-update` run does, in order:

1. **Fetch** every installed kit fresh from GitHub — bails hard if any fetch
   fails (never operates on a partial set).
2. **Install/converge** each kit's files per the §2 rules.
3. **Reap** hook files no kit ships (§3.6).
4. **Sweep** debris from retired mechanisms (`*.kit-latest` files).
5. **Reconcile registrations**: prune settings.json entries pointing at
   deleted hook files, register every current kit hook under its event, and
   re-assert the git identity env block.
