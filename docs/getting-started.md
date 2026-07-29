# Getting started

*Last updated for git-lex v0.1.0 (2026-07-29)*

## Install

```bash
cargo install --path . --locked   # from a clone
```

Installs two binaries: `git-lex` (the CLI — git discovers it, so you call it
as `git lex`) and `git-lex-serve` (the local web server).

<!-- TODO(additive): release-binary install once binaries ship -->

## Your first repo

```bash
git lex init --kit soul     # or your kit of choice
git lex create <type>       # scaffold a document
git lex save "first save"   # commit; extraction + validation run automatically
git lex sync                # build the knowledge graph
```

## The one thing to understand

`git lex save` is the only write path. Every save extracts your frontmatter
into the graph's source-of-truth files and validates them against your kit's
rules — a bad save is blocked, never silently absorbed.

<!-- TODO(additive): troubleshooting section (identity setup, common errors) -->
