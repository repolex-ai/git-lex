# git-lex

Git extensions for knowledge graphs. How much KG can we get out of git?

## Concept

What if git IS the knowledge graph? Git already has content-addressed storage, branching, merging, diffing, blame, and a full temporal DAG. Instead of building a database and calling it git (like everyone else), we use actual git — extended with custom subcommands, diff/merge drivers, and a lightweight index layer.

## Architecture

- **Content lives in the repo** — markdown, docs, whatever. Normal git.
- **`.lex/` is the KG index** — derived knowledge, triples, schema. Tracked by git.
- **`git-lex` binary** — Rust CLI that installs as a git subcommand (`git lex ...`)
- **Custom diff/merge drivers** — semantic operations on knowledge files
- **No binary stores** — everything must be git-diff-friendly (no oxigraph/rocksdb in the repo)

## Status

Early exploration. See `docs/` for research notes and design thinking.
