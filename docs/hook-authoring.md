# Hook Authoring Guide

*Last updated for git-lex v0.1.0 (2026-07-29)*

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
the app's blessed entrypoint.** Example shape for a copia hook:

```bash
#!/bin/bash
# UserPromptSubmit-copia-cosee.sh — thin shim; all logic in the copia package
exec uv run --project "$COPIA_HOME" python -m copia.hooks.coseehook
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
convention is: same stem as the hook it serves, `.py` extension —
`UserPromptSubmit-copia-cosee.sh` calls `UserPromptSubmit-copia-cosee.py`,
sitting next to it in `.claude/hooks/`. The shared stem makes the pairing
obvious from `ls` and keeps the helper covered by the same mental namespace
as its hook. But treat this as the fallback, not the pattern.

<!-- TODO(docs pass): pin down the `uv run` invocation exactly once copia's
     cosee hook ships as .sh — project path resolution ($COPIA_HOME vs
     hardcoded), and what happens when the app env is missing. -->

## Worked end-to-end example

One real hook, followed start to finish. Sections marked *TODO* get filled
during the docs pass.

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

*TODO(docs pass):* the same hook as it would look **before** promotion —
named `SessionEnd-local-save.sh`, hand-registered in `settings.local.json` —
plus the promotion diff.

### With an app-package helper

*TODO(docs pass):* the same walk-through for a hook whose logic lives in an
application package (the `uv run` shim pattern above). Candidate: copia's
cosee hook once it ships as `.sh`.
