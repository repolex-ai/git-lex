# Exporting the Graph: `git lex export-spine`

`git lex export-spine` writes your repository's semantic index as one plain-text
TSV file — the **spine** — built for loading into an LLM's context cache.
Every `git lex sync` refreshes it automatically; the command exists for
refreshing without a full sync.

## Why This Exists: A Neural KV-Cache

Feed a repository's complete graph into an LLM's context cache. LLM context
caching can hold an entire file resident across many calls at a
steep per-token discount, with effectively zero added latency to recall
anything in it. If that resident file is your repository's semantic graph —
every document, every fact, every link — the model has instant, exact
recall over everything you know. For example, a 150-document soul
repository comes out to ~5,300 facts in ~520 KB — roughly 130,000 tokens,
comfortably inside modern context windows.

## The File

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

- **Identity header**: Identifies which entity this file represents — so a cache holding many
  spines can attribute every fact, and `# repo` + a `fileId` row
  reconstructs a real path on disk.
- **Prefix lines**: Only the prefixes actually used. Instance IRIs are
  relativized against `@base` (`<copia/Being/w4r3z>`), following standard
  Turtle rules without invented prefixes.
- **Rows**: Tab-separated, shaped like W3C SPARQL
  1.1 TSV results, native to standard text processing and database imports. One fact per line.
- **Sorted**: Unchanged content produces a byte-identical file, allowing
  consumers to cache on the file hash.

Scope is the `now` graph (current state of every document's facts) plus
`repo-ontology` (the vocabulary that explains them). Commit history, the
file tree, and raw plumbing are excluded to maximize meaning per
token. Blank-node rows and RDF 1.2 annotation terms are excluded as well.

A `manifest.json` file beside it tracks the current file (`commit`, `spine`,
`spine_bytes`). Other tools may add custom keys (such as an external cache manager);
git-lex rewrites only the keys it owns.

## The Cloud Handoff

Git-lex never talks to external cloud services directly. After each spine write, it can invoke
`pythia cache update` (detached, repo root as working directory) **if** a
`pythia` binary is on `PATH`. If not installed, the step is silently skipped;
the spine remains available on disk for any consumer.

## Pitfalls & Guidelines

- **The store has to be synced first.** This command reads the synced
  store, not the working tree. Run `git lex sync` before exporting.
- **One generation is kept.** Each export prunes older spine files after
  the new one is written; the manifest always names the current file.
