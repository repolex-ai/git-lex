# Getting started

*Last updated for git-lex v0.1.0 (2026-08-12)*

## Install

```bash
cargo install --path . --locked   # from a clone
```

Installs two binaries: `git-lex` (the CLI — git discovers it, so you call it
as `git lex`) and `git-lex-serve` (the local web servers). The CLI also keeps
a man page next to itself, so `git lex --help` and `man git-lex` both work.

<!-- TODO(additive): release-binary install once binaries ship -->

## Your first repo

```bash
git lex init --kit soul     # or your kit of choice
git lex create <type>       # scaffold a document
git lex save "first save"   # commit; extraction + validation run automatically
git lex sync                # build the knowledge graph store
```

`init` sets up everything in one pass: it downloads the base kit plus your
domain kit, generates validation shapes, creates a folder and template for
each document type, installs the pre-commit hook that enforces validation,
asks you the kit's setup questions (your agent name, etc.), and commits the
setup files. If the directory isn't a git repo yet, it offers to run
`git init` for you. Re-running `init` is safe — it asks first, then refreshes
kit files while preserving your content and previous answers.

`create` prints the new file's path (and warns if you skipped the id — the
file defaults to `untitled`). `save` ends with a state line telling you what
was committed; a failed save says so and commits nothing.

## The one thing to understand

`git lex save` is the only write path. Every save extracts your frontmatter
into the graph's source-of-truth files and validates them against your kit's
rules — a bad save is blocked, never silently absorbed. `save` reconciles the
whole repo (the derived `.lex/` files are its job, not yours), and
`save --dry-run` runs every gate without committing anything.

<!-- TODO(additive): troubleshooting section (identity setup, common errors) -->
