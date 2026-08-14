# Hook Authoring Guide

*Last updated for git-lex v0.1.0 (2026-08-12)*

> **Being broken out.** The core hook material (naming, registration,
> `settings.json` vs `settings.local.json`, the local→kit development flow,
> the reap rules) currently lives in the
> [Kit authoring guide, section 3](kit-authoring.md#3-hooks). It will move
> here during the post-rollout docs pass. What's below is new material that
> starts on this page.

## Helper code: where hook logic beyond the `.sh` should live

Only `.sh` files register and fire (see
[Kit authoring §3.3](kit-authoring.md#33-sh-only)). When a hook needs real
logic — Python, an app's internals — the question is where that logic lives.

**Preferred: the logic lives in the application package, and the `.sh` calls
the app's blessed entrypoint.** Illustrative shape (no shipped kit hook uses
this yet):

```bash
#!/bin/bash
# thin shim; all logic in the app's package
exec uv run --project "$APP_HOME" python -m theapp.hooks.thehook
```

Why this is the preferred shape:

- The logic is **versioned, tested, and released with the app** — not a loose
  script the kit has to ship as a raw file and converge on every update.
- The kit ships only the thin `.sh` shim, which almost never changes. App
  releases update the behavior; kit-updates update the wiring. One authority
  per fact.
- A loose sibling `.py` is unmanaged code: no tests, no imports from the app,
  silently drifting from the app it belongs to.

**If a loose helper file must ship anyway** (no app package to call), the
convention is: same stem as the hook it serves, `.py` extension. This shape
ships today: copia's `PostToolUse-copia-read-seen.sh` calls
`PostToolUse-copia-read-seen.py`, sitting next to it in `.claude/hooks/`.
The shared stem makes the pairing obvious from `ls` and keeps the helper
covered by the same mental namespace as its hook. The `.sh` pipes the hook
payload to the `.py` on stdin and passes the program as a *file path* (not a
heredoc — a heredoc would eat the stdin the payload rides on). Treat this as
the fallback, not the pattern.

**One thing you cannot do: a shared `.sh` library in `.claude/hooks/`.**
kit-update hard-errors on any `.sh` there whose leading filename segment is
not a real Claude Code event (a hook that would never fire is worse than a
crash), so a `hook-common.sh` cannot live in that directory. This is why the
opt-out guard block is duplicated verbatim into every kit hook instead of
being sourced from a shared script.

## Worked end-to-end example

One real hook, followed start to finish.

### The hook: `SessionEnd-soul-save.sh`

Shipped by the soul kit. Purpose: when a Claude Code session ends, commit the
session's final bytes with `git lex save` so they land in the same-session
commit. The real file is ~80 lines; the skeleton:

```bash
#!/bin/bash
set -e

# --- kit-hook opt-out guard (managed; do not edit) ---
# ... checks soul.disabledHooks in settings.local.json; exits 0 if this
#     hook's basename is listed ...
# --- end kit-hook opt-out guard ---

HOOK_INPUT="$(cat 2>/dev/null || true)"   # hook payload arrives on stdin (JSON)

# Parse what we need from the payload (here: the session-end reason).
MATCHER="$(printf '%s' "$HOOK_INPUT" | python3 -c '...')"
case "$MATCHER" in clear|resume) exit 0 ;; esac   # skip non-real ends

cd "$CLAUDE_PROJECT_DIR" || exit 0
git lex save "SessionEnd auto-save (reason: $MATCHER)" >/dev/null 2>&1 || true
exit 0
```

Things every hook script should copy from it:

- **The opt-out guard block first** — it's what makes `soul.disabledHooks`
  work. Kit hooks ship it verbatim.
- **Read stdin once** into a variable; the payload is JSON.
- **Fail soft** (`|| true`, `exit 0`) — a hook that exits non-zero can
  interrupt the user's session. Only fail loudly on purpose.
- **`$CLAUDE_PROJECT_DIR`** is the repo root; never assume cwd.

### Where it lives in the kit

```
git-lex-kit-soul/harness/.claude/hooks/SessionEnd-soul-save.sh
```

### What kit-update writes to `settings.json`

Generated automatically — never hand-written. From a live soul repo:

```json
{
  "hooks": {
    "SessionEnd": [
      { "hooks": [ { "type": "command",
          "command": "bash \"$CLAUDE_PROJECT_DIR/.claude/hooks/SessionEnd-soul-save.sh\"" } ] }
    ]
  }
}
```

### The `settings.local.json` stage (development phase)

Before promotion, the same hook would be named `SessionEnd-local-save.sh` and
hand-registered in `settings.local.json`. That whole flow — the `-local-`
protection, the registration JSON, the promotion steps — is documented in
[Kit authoring §3.5](kit-authoring.md#35-the-hook-development-flow).

### With an app-package helper

No kit hook ships the app-package shim shape yet. The closest real specimen
is the sibling-`.py` fallback: copia's `PostToolUse-copia-read-seen.sh` +
`.py` pair (see the helper-code section above). This walk-through gets
written when the first app-package hook ships.
