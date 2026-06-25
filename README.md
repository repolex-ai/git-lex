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
git lex query "SELECT ?m WHERE { ?m a soul:Memory }"
```

> [!WARNING]
> **Alpha.** git-lex works and is used daily, but it's early and the surface is
> still moving. Try it on a repo you don't mind re-initializing. Feedback and bug
> reports are very welcome — that's exactly what this stage is for.

---

## Why

Most "knowledge graph" tools ask you to maintain a database *alongside* your
files. git-lex doesn't. The four things a knowledge graph needs, git already has:

| Knowledge-graph need | git-lex uses |
|---|---|
| **Identity** | the repo's first commit — your base URI is `urn:soul:<genesis-sha>` |
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

Download the binary for your platform from the
[Releases page](https://github.com/repolex-ai/git-lex/releases), put it on your
`PATH`, and make it executable:

```bash
chmod +x git-lex && mv git-lex /usr/local/bin/
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
```

`git lex save` is the everyday command: it stages your changes, runs frontmatter
extraction and SHACL validation as a pre-commit gate (a bad document blocks the
commit with a clear error), commits, and updates the graph.

---

## Commands

| Command | What it does |
|---|---|
| `git lex init [--kit <name>]` | Initialize `.lex/` in the current repo. The base kit is always installed; `--kit` adds a domain kit (e.g. `soul`). |
| `git lex create <Type> [<id>]` | Scaffold a new document from a kit class, with frontmatter stubbed from the ontology. |
| `git lex save "msg"` | Stage + extract frontmatter + SHACL-validate + commit, in one step. |
| `git lex query "SPARQL"` | Run a SPARQL query over the whole graph (all commits + files). `--json` for machine output. |
| `git lex list` | List every document class the repo's installed kits define. `--json` for machine output. |
| `git lex sync` | Rebuild the SPARQL store from the `.spo` sidecars (the store is derived; this regenerates it). |
| `git lex kit-update [<kit>]` | Re-download + reinstall kits. Drift-aware: locally-changed files are preserved and the new version lands beside them as `<file>.kit-latest` to diff (`--force` overwrites, stashing your version first). |
| `git lex kit-add <org/repo>` | Add an optional kit (the kit's `scope:` must be `optional`). Creates its folders + templates. |
| `git lex kit-remove <org/repo>` | Remove an optional kit. Asks before deleting any content folders it owns. |
| `git lex join <squad-path>` | Join a squad repo — creates a mutual identity binding (a ticket) between you and the squad. |
| `git lex serve [...]` | Start the local servers (graph visualizer, listener). |
| `git lex display "CONSTRUCT ..."` | Run a SPARQL CONSTRUCT and push the result to the running viz server. |
| `git lex history-verify` | Check the history⇄now invariant — that the temporal graph faithfully reconstructs the current state. |
| `git lex raw backfill` | One-shot: mirror pre-existing harness session files into `Raw/` (the live mirror runs on every save). |
| `git lex nuke` | Remove `.lex/` entirely. Your content files and git history are untouched. |

Run `git lex help <subcommand>` for full options on any command.

---

## How it works

git-lex keeps a clean split between **what you write**, **the index**, and
**derived data**:

- **Content** — normal Markdown in your repo. Structure goes in YAML frontmatter
  using dot notation: `soul.memory.confidence: "certain"`. In the body, link
  documents with `[[wikilinks]]` and `@mentions`.

- **`.lex/`** — the git-*tracked* index. Holds extraction sidecars
  (`.lex/extract/**.spo`), the installed kit(s) (`.lex/kit/`), generated SHACL
  shapes and ontology (`.lex/ontology/`), your identity (`.lex/identity.yml`,
  anchored on the genesis commit), and squad bindings (`.lex/tickets/`). It's
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
edit .md  →  extract frontmatter to .spo  →  SHACL validate  →  commit  →  update graph
                                                  │
                                          (blocks the commit
                                           if a shape fails)
```

Because each fact is tagged (via RDF 1.2 triple terms) with the commit that
asserted it, the history graph lets you ask not just *what is true* but *what was
true, and when you learned it*.

---

## Pairs with Pool

git-lex indexes **text** — Markdown documents and their git history. Its sibling
[Pool](https://github.com/repolex-ai/pool) indexes **media** — images and other
blobs, content-addressed, with the same RDF/SPARQL spine plus a vector index for
similarity search. Together they cover a soul's text and episodic/visual memory.
You don't need Pool to use git-lex.

---

## License

[Unlicense](LICENSE) — public domain.
