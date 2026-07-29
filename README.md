# git-lex

**Turn a git repo into a knowledge graph you can query.**

`git-lex` installs as a git subcommand (`git lex ...`). You write plain Markdown
with a little structured frontmatter; git-lex extracts that structure into RDF,
validates it against SHACL shapes, and lets you query the whole repo — across its
entire history — with SPARQL. Your identity, your data, and your history all live
in git. git-lex is the index on top.

```bash
git lex create Memory "first-day"     # make a typed document
git lex save "wrote my first memory"  # commit + extract + validate, one step
git lex query "PREFIX ks: <https://repolex.ai/ontology/kit/soul/>
               SELECT ?m WHERE { ?m a ks:Memory }"
```

> [!WARNING]
> **Alpha.** git-lex works and is used daily, but it's early and the surface is
> still moving. Try it on a repo you don't mind re-initializing. Feedback and bug
> reports are very welcome — that's exactly what this stage is for. See
> [Known limitations](#known-limitations-alpha) before you start.

Full documentation lives in [`docs/`](docs/index.md).

---

## Why

Most "knowledge graph" tools ask you to maintain a database *alongside* your
files. git-lex doesn't. The four things a knowledge graph needs, git already has:

| Knowledge-graph need | git-lex uses |
|---|---|
| **Identity** | the repo's genesis (first-commit) SHA — recorded as a fact, not baked into IRIs |
| **Provenance** | RDF 1.2 triple terms record *which commit* asserted each fact |
| **History** | git history *is* the temporal graph — query what was true at any point |
| **Validation** | SHACL shapes (generated from your kit's ontology) run at commit time |

You don't maintain a parallel store. You write Markdown and commit. The graph
falls out of git.

The primary use case it's designed for is a **soul repo** — a personal or agent
memory store where notes, journals, and tasks become a queryable graph of what
you know and when you learned it. But nothing about git-lex is soul-specific: any
repo with structured Markdown can become a graph. The shape of *your* graph comes
from a **kit** (see below).

---

## Install

### From source (recommended during Alpha)

You'll need a Rust toolchain ([rustup](https://rustup.rs/) is the easy path):

```bash
git clone https://github.com/repolex-ai/git-lex
cd git-lex
cargo install --path . --locked
```

This installs both `git-lex` and `git-lex-serve` to `~/.cargo/bin/`. Make sure
that's on your `PATH`, then verify:

```bash
git lex --help
```

> [!IMPORTANT]
> **`--locked` is required.** The `rudof` crate family (SHACL/RDF) has
> sibling-crate API coupling that needs the exact versions pinned in
> `Cargo.lock`. A plain `cargo install --path .` re-resolves transitive deps and
> can fail to compile on `shacl_ast` / `rudof_rdf`. Always pass `--locked` —
> including when reinstalling after local changes
> (`cargo install --path . --force --locked`).

### From a release binary

Download **both** binaries for your platform from the
[Releases page](https://github.com/repolex-ai/git-lex/releases) — `git-lex`
(the CLI) and `git-lex-serve` (needed by `git lex serve`) — put them on your
`PATH`, and make them executable:

```bash
chmod +x git-lex git-lex-serve && mv git-lex git-lex-serve /usr/local/bin/
git lex --help
```

---

## Quick start

```bash
mkdir my-graph && cd my-graph && git init
git lex init --kit soul          # install the base kit + the 'soul' kit
git lex list                     # see the document types this kit gives you
git lex create Memory "day-1"    # scaffold a typed Memory document
# ...edit the new .md file: fill in its frontmatter + body...
git lex save "first memory"      # stage + extract + validate + commit
git lex query "SELECT * WHERE { ?s ?p ?o } LIMIT 10"
git lex sync                     # build the history store
```

`git lex save` is the everyday command: it stages your changes, runs frontmatter
extraction and SHACL validation as a pre-commit gate (a bad document blocks the
commit with a clear error), and commits.

`git lex query` reflects your working tree directly — a `create → save → query`
flow surfaces a new document's frontmatter immediately, no extra step. (`git lex
sync` exists too, but it's for building the persistent history store, not a
prerequisite for querying current state — see [How it works](#how-it-works).)

### Querying by class

The frontmatter key `soul.Memory.category: "..."` types the document as a
**`Memory`** in the `soul` kit. Kit classes live under
`https://repolex.ai/ontology/kit/<kit>/` — so the class IRI is
`https://repolex.ai/ontology/kit/soul/Memory`, and to find every Memory:

```bash
git lex query "PREFIX ks: <https://repolex.ai/ontology/kit/soul/>
               SELECT ?m WHERE { ?m a ks:Memory }"
```

> [!NOTE]
> **Get the exact class IRI from `git lex list --json`** (the `uri` field). The
> plain `git lex list` prints a short label like `soul:Memory`, but that `soul:`
> is display shorthand for the **`…/ontology/kit/soul/`** namespace — *not*
> `…/ontology/soul/`. If a class query returns nothing, this prefix mismatch is
> the first thing to check. (Tightening this so the displayed prefix and the
> query prefix are the same string is on the list.)

---

## Commands

| Command | What it does |
|---|---|
| `git lex init [--kit <name>]` | Initialize `.lex/` in the current repo. The base kit is always installed; `--kit` adds a domain kit (e.g. `soul`). |
| `git lex create <Type> [<id>]` | Scaffold a new document from a kit class, with frontmatter stubbed from the ontology. |
| `git lex save "msg"` | Stage + extract frontmatter + SHACL-validate + commit, in one step. |
| `git lex query "SPARQL"` | Run a SPARQL query over a live view built from your working tree, so it always reflects current frontmatter (no `sync` needed). History lives in the synced store — query it via the SPARQL endpoint (`git lex serve sparql`; ready-made history query in [docs/queries.md](docs/queries.md)). `--json` for machine output. |
| `git lex list` | List every document class the repo's installed kits define, each with its full namespace IRI (the prefix to query against). `--json` for machine output. |
| `git lex sync` | Build/update the persistent store: walks new commits and appends each fact change as an assert/retract event tied to its commit (RDF 1.2 provenance), then refreshes the current-state view. Not required for `query`. |
| `git lex kit-update [<kit>]` | Re-download + reinstall kits. Kit files always converge to the kit's version: a local file that differs is renamed `<file>.bak` and replaced. `SOUL.md` is never overwritten. |
| `git lex kit-add <org/repo>` | Add an optional kit (the kit's `scope:` must be `optional`). Creates its folders + templates. |
| `git lex kit-remove <org/repo>` | Remove an optional kit. Asks before deleting any content folders it owns. |
| `git lex serve <viz\|sparql>` | Start one local server: `viz` (graph visualizer, 7878) or `sparql` (W3C SPARQL endpoint over the synced store, 7880). |
| `git lex verify` | Health-check the synced store (read-only): vocabulary declared, history well-formed, current state matches history. Temporary command — will be removed after the v1 rollout. |
| `git lex nuke` | Remove git-lex from the repo. Your content files and git history are untouched — but note it **commits and pushes** the removal (after snapshotting any uncommitted work). |

Run `git lex help <subcommand>` for full options on any command.

---

## How it works

git-lex keeps a clean split between **what you write**, **the index**, and
**derived data**:

- **Content** — normal Markdown in your repo. Structure goes in YAML frontmatter
  using dot notation: `soul.Memory.confidence: "certain"` (class names are
  case-sensitive). In the body, link documents with `[[wikilinks]]`.

- **`.lex/`** — the git-*tracked* index. Holds extraction sidecars
  (`.lex/extract/**.spo`), the installed kit(s) (`.lex/kit/`), generated SHACL
  shapes and ontology (`.lex/ontology/`), repo configuration + identity
  (`.lex/repo.yml`, anchored on the genesis commit). It's
  checked in, so your graph travels with your repo.

- **`.git/lex/`** — derived data (the oxigraph SPARQL store). Never tracked,
  fully rebuildable from the sidecars with `git lex sync`.

- **Kits** — a kit defines the *shape* of your graph: its ontology (classes +
  properties), the document templates `create` scaffolds, the SHACL shapes
  `save` validates against, and any harness adapters (e.g. editor/agent hooks).
  Kits are fetched from GitHub (`github.com/repolex-ai/git-lex-kit-<name>`). The
  **base** kit is always installed; **domain** kits (like `soul`) layer on top;
  **optional** kits can be added per-repo with `kit-add`.

The pipeline on every `git lex save`:

```
edit .md  →  extract frontmatter to .spo  →  SHACL validate  →  commit
                                                  │
                                          (blocks the commit
                                           if a shape fails)
```

`save` writes to git only. The persistent SPARQL store is updated by
`git lex sync` (run where the system needs it, e.g. a hook or on demand) —
`git lex query` always builds a fresh view from the working tree, so
querying current state never requires a sync.

Because every fact change is recorded as an event (via RDF 1.2 triple terms)
tied to the commit that caused it, the synced store answers not just *what is
true* but *what was true, when it changed, and who changed it* — see the
[query cookbook](docs/queries.md).

---

## Pairs with Pool

git-lex indexes **text** — Markdown documents and their git history. Its sibling
[Pool](https://github.com/repolex-ai/pool) indexes **media** — images and other
blobs, content-addressed, with the same RDF/SPARQL spine plus a vector index for
similarity search. Together they cover a soul's text and episodic/visual memory.
You don't need Pool to use git-lex.

---

## Known limitations (Alpha)

git-lex is early. These are the sharp edges we know about and are working on —
documented here so they don't surprise you on first contact:

- **POSIX only (macOS / Linux).** The commit-time extract+validate gate installs
  as a `#!/bin/sh` git hook with a unix executable bit. There is no Windows
  install path yet, so the validation gate won't run on Windows.

- **Frontmatter class names are case-sensitive.** The class segment of a
  dot-notation key matches the ontology class *exactly*: `soul.Memory.category`
  (capital **M**) types the document as a `Memory`. If you write a casing that
  doesn't match a real class, `git lex save` now **warns** and emits the
  canonical form rather than silently mistyping the document — but the warning is
  your cue to fix the source. Check the exact names with `git lex list`.

Found something else? That's exactly what this stage is for — please file it.

---

## License

[Unlicense](LICENSE) — public domain.
