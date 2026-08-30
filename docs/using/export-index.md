# Exporting the graph: `git lex export-index cottas`

*Last updated for git-lex v0.1.0 (2026-08-29)*

`git lex export-index cottas` takes a snapshot of your synced store and
writes it out as plain files, so something other than git-lex can read your
whole graph — no server, no SPARQL endpoint, nothing running.

## Why this exists: a semantic cache for an LLM

The headline reason to run this command is to feed a repo's whole graph into
an LLM's context cache. Gemini's explicit context caching can hold an entire
file resident across many calls at a steep per-token discount on the cached
portion, with effectively zero added latency to recall anything in it. If
that resident file is your repo's semantic graph — every document, every
fact, every link — the model has instant, exact recall over everything you
know, instead of guessing from a handful of retrieved snippets.

That resident file is the second artifact this command writes: the **spine**
(below). One real number, from a 152-document soul repo: 5,942 facts came out
to about 542 KB — roughly 130,000 tokens. Comfortably inside Gemini's ~1M
token context window, with room to spare for the conversation itself.

## What it writes

Run it after a sync:

```bash
git lex sync
git lex export-index cottas
```

It writes three files into `.lex/_ignore/cottas/`, all named after the
commit the **store** is synced to — deliberately not `HEAD`. If you commit
without syncing, the store is still describing an older commit, and naming
the snapshot after HEAD would put a fresh-looking name on stale content.

- **`<synced-commit>.spine.md`** — the Tabular Prefix spine: a plain-text
  triple table meant to be pasted or loaded straight into an LLM's context
  cache. It starts with a `@base` line and the `@prefix` lines actually used,
  then one fact per line as `| SUBJECT | PREDICATE | OBJECT |`. It covers two
  graphs only — `now` (current state of every document's facts) and
  `repo-ontology` (the vocabulary that explains them). Commit history, the
  file tree, and other git plumbing are left out on purpose: they cost
  tokens and carry very little meaning per token. Rows are sorted and
  deduplicated, so an unchanged repo produces a byte-identical file — a
  consumer can cache-key on the file's hash and skip re-uploading when
  nothing changed.

- **`<synced-commit>.cottas`** — the same graph as a COTTAS file (Columnar
  Triple Table Storage, from the ISWC 2026 paper): one Apache Parquet file
  with `s` / `p` / `o` / `g` columns, sorted and ZSTD-compressed. This one is
  for machine tools, not model context — see below.

- **`manifest.json`** — names the current snapshot: `commit`, `file`,
  `spine`, `quads`, `bytes`, `spine_bytes`. A consumer that wants to know
  whether its cached copy is stale polls this one small file instead of
  re-reading the big ones.

Run it again with nothing new synced and it's a no-op:

```
Already current: .lex/_ignore/cottas/a1b2c3d4....cottas + a1b2c3d4....spine.md (store synced to a1b2c3d4)
```

Old snapshots are pruned automatically — the command keeps exactly one
generation, so the pocket doesn't accumulate a file per sync forever.

## The machine path: DuckDB and pycottas

The `.cottas` file needs no git-lex and no server. Query it directly:

```sql
-- DuckDB
SELECT * FROM parquet_scan('.lex/_ignore/cottas/a1b2c3d4....cottas') LIMIT 20;
```

```python
# pycottas
import pycottas
table = pycottas.read('.lex/_ignore/cottas/a1b2c3d4....cottas')
```

Because it's just Parquet, DuckDB can scan **several repos'** `.cottas`
files in a single query — a zero-server way to federate more than one
graph at once:

```sql
SELECT * FROM parquet_scan([
  'repo-a/.lex/_ignore/cottas/....cottas',
  'repo-b/.lex/_ignore/cottas/....cottas'
]);
```

## Pitfalls

- **The store has to be synced first.** This command reads the synced
  store, not the working tree — if you've never run `git lex sync`, or the
  repo has no synced commits yet, it fails loudly and tells you to sync.
  `git lex query` and this command read different things; don't assume one
  implies the other.

- **`cottas-rs` has to be installed, once.** Producing the `.cottas` file
  is delegated to an external binary rather than bundled in, because it
  pulls in DuckDB's C++ build — vendoring that would make every fleet
  `cargo install --force` of git-lex itself much slower. If it's missing,
  the command fails immediately with the install line:

  ```bash
  cargo install cottas-rs --locked
  ```

  It's a slow build (it's compiling DuckDB), but it's one time. Today a
  missing binary blocks the whole command, spine included — the check runs
  before anything is written.

- **RDF 1.2 triple terms are excluded from the `.cottas` file, on purpose.**
  git-lex's history graph stores its provenance (which commit asserted or
  retracted each fact) as RDF 1.2 triple terms — an annotation wrapped
  around a triple. A COTTAS file, like any plain triple table, has nowhere
  to put one; the shape doesn't exist in RDF 1.1. So those quads are
  dropped from the dump, and the command tells you exactly how many:

  ```
  Excluded: 214 history annotation quad(s) — RDF 1.2 triple terms, which a COTTAS triple table cannot hold
  ```

  This is documented, expected behavior, not data loss you need to chase
  down. The spine never had them in the first place — it only ever covers
  `now` and `repo-ontology`.

- **Staleness is your job to check, by polling `manifest.json`.** Nothing
  re-runs this command for you when you sync again. A long-lived consumer
  (an agent holding a spine in its context cache, a script scanning the
  `.cottas` file on a timer) should read `manifest.json`'s `commit` field
  and compare it against what it last loaded, rather than assuming the
  snapshot on disk is current.
