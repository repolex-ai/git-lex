# Exporting the graph: `git lex export-spine`

*Last updated for git-lex v0.1.0 (2026-08-29)*

`git lex export-spine` writes your repo's semantic index as one plain-text
TSV file — the **spine** — built for loading into an LLM's context cache.
Every `git lex sync` refreshes it automatically; the command exists for
refreshing without a sync.

## Why this exists: a neural KV-cache

Feed a repo's whole graph into an LLM's context cache. Gemini's explicit
context caching can hold an entire file resident across many calls at a
steep per-token discount, with effectively zero added latency to recall
anything in it. If that resident file is your repo's semantic graph —
every document, every fact, every link — the model has instant, exact
recall over everything you know. One real number: a 152-document soul
repo comes out to ~5,300 facts in ~520 KB — roughly 130,000 tokens,
comfortably inside a 1M-token window.

## The file

`.lex/_ignore/spine/<synced-commit>.spine.tsv` — named by the commit the
**store** is synced to (deliberately not `HEAD`: committing without
syncing must not put a fresh name on stale content). Layout:

```
# genesis_sha: 495d8c70
# soul: W4R3Z
# repo: 7R1PL3F0RC3/W4R3Z
@base <https://repolex.ai/>
@prefix soul: <https://repolex.ai/ontology/soul/>

?s	?p	?o
<soul/Note/kira>	git-lex:title	"Kira"
```

- **Identity header**: which soul this file IS — so a cache holding many
  souls' spines can attribute every fact, and `# repo` + a `fileId` row
  reconstructs a real path on disk.
- **Prefix lines**: only the prefixes actually used. Instance IRIs are
  relativized against `@base` (`<copia/Being/w4r3z>`), the standard
  Turtle rule, no invented prefixes.
- **Rows**: tab-separated, no pipes, no padding — shaped like W3C SPARQL
  1.1 TSV results, native to `cut`/`awk`/SQLite import, and ~8% smaller
  than a pipe table. One fact per line; a literal containing a raw tab
  gets it escaped.
- **Sorted**: unchanged content produces a byte-identical file, so a
  consumer can cache-key on the file hash.

Scope is the `now` graph (current state of every document's facts) plus
`repo-ontology` (the vocabulary that explains them). Commit history, the
file tree, and other plumbing are excluded on purpose — low meaning per
token, and the history graph alone outgrows any context window at fleet
scale. Blank-node rows (OWL structural shells with unstable labels) and
RDF 1.2 annotation terms are excluded too.

`manifest.json` beside it names the current file (`commit`, `spine`,
`spine_bytes`). Other tools may add their own keys (a cache manager
records its cache id here); git-lex rewrites only the keys it owns.

## The cloud handoff

git-lex never talks to any cloud. After each spine write it spawns
`pythia cache update` (detached, repo root as working directory) **if** a
`pythia` binary is on PATH — pythia owns the context-cache upload and
records its cache id in `manifest.json`. No pythia installed means the
step is silently skipped; the spine is still on disk for any consumer.

## Pitfalls

- **The store has to be synced first.** This command reads the synced
  store, not the working tree. No store, or no synced commits → it fails
  loudly and tells you to run `git lex sync`.
- **One generation is kept.** Each export prunes older spine files after
  the new one is in place; the manifest always names the current one.
- **`.lex/_ignore/cottas/` is gone on purpose.** An earlier format (a
  Parquet triple table) lived there; it was retired 2026-08-29 and the
  directory is cleaned up automatically on the next sync.
