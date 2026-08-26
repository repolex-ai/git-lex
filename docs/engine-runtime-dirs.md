# Engine runtime directories and the `_ignore/` pocket

Every tool in the stack keeps its per-repo state in one visible dotdir at
the repo root — `.lex/`, `.pool/`, `.copia/`, `.horae/`, `.ravel/`,
`.pan/`. This page is the layout law for those directories and the
`.gitignore` block git-lex manages for them.

## The law, in one sentence

**In any tool's dotdir, `_ignore/` is machine-local; everything else is
committed.**

- **`<dotdir>/_ignore/`** is the machine-local pocket: graph stores,
  indexes, caches, queues, transcript mirrors — anything rebuildable or
  relocatable that would bloat or break the shared record. It is
  gitignored and never committed.
- **Everything else in the dotdir is committable**: config, extraction
  sidecars, anything that belongs in the shared record. If it sits outside
  `_ignore/`, git sees it — deliberately.

The name states its git status, and the underscore sorts it to the top of
the dotdir so it reads as "special" in any file browser.

## The managed `.gitignore` block

`git lex init` and `git lex kit-update` maintain a sentinel-wrapped block
in the repo's root `.gitignore`:

```
# >>> git-lex engine runtime (managed) >>>
.lex/_ignore/
.pool/
.copia/
.horae/
.ravel/_ignore/
.pan/_ignore/
# <<< git-lex engine runtime (managed) <<<
```

Do not hand-edit inside the sentinels — the next kit-update rewrites the
block in place. Your own entries outside the block are never touched.

Two entry shapes appear, and the difference is transitional:

- **Pocket form** (`.ravel/_ignore/`) — the law's shape. The engine keeps
  its committable files in the dotdir and its machine-local state in the
  pocket.
- **Whole-dir form** (`.pool/`) — a holdover for engines that predate the
  law or haven't yet moved their machine-local state into the pocket.
  Ignoring the whole dotdir protects today's layout; nothing in it can be
  committed until the engine migrates.

An engine's entry converges from whole-dir to pocket form automatically,
gated on that engine's known pre-law paths being gone from outside the
pocket — the engine's own data migration is the trigger, so the flip can
never expose a not-yet-moved store to git.

## If you are building an engine

1. Put machine-local state under `<yourdotdir>/_ignore/` from day one.
2. Put committable files (config, shared-record artifacts) directly in the
   dotdir.
3. Ask the git-lex owner to add your engine to the managed block. A
   new engine that follows rule 1 gets the pocket entry immediately; one
   with machine-local state outside the pocket gets a whole-dir entry
   until its owner confirms the migration.

## Already committed something you shouldn't have?

git-lex reports tracked files that fall under the managed entries but
**never** untracks them for you — removing things from the index is your
deliberate act:

```
git rm --cached -r .horae/
```

The files stay on disk; only git stops tracking them.

*The stack-wide ruling behind this page: Rob, 2026-08-05 (dotdir `_ignore/`
pocket law).*
