## Quick Start

```bash
# Install git-lex
cargo install --git https://github.com/repolex-ai/git-lex --locked

# In an existing Git repo, or create a new one:
mkdir my-graph && cd my-graph && git init
git lex init --kit soul          # initialize .lex/ in the repo with a domain kit
git lex create Note "first"      # scaffold a typed document
git lex save "my first note"     # extract + SHACL-validate + commit; nudges gitlexd
git lex sync                     # compile committed history into the persistent store
git lex query "SELECT * WHERE { ?s ?p ?o } LIMIT 10"  # query via gitlexd (auto-starts if needed)
git lex direct "SELECT * WHERE { ?s ?p ?o } LIMIT 10" # query working tree in memory
```

---

## Documentation Index

* [Getting Started](using/getting-started.md) — Install and run your first query in five minutes.
* [Writing Documents](using/writing-documents.md) — Document structures, frontmatter syntax, and markdown links.
* [Files and Things](using/files-and-things.md) — The File Plane and the Thing Plane, and which of your facts live where.
* [Moving, Renaming, and Deleting](using/renames-moves-deletes.md) — What survives a file moving, and how links heal themselves.
* [Commands](using/commands.md) — The complete CLI command reference.
* [Querying with SPARQL](using/queries.md) — Query your repository, inline or saved, with worked examples.
* [History](using/history.md) — Query the graph as it stood at any commit.
* [gitlexd](using/gitlexd.md) — The local service that syncs every repository's graph and answers queries, with its HTTP endpoint.
* [Exporting the graph](using/export-spine.md) — Write the semantic index as one TSV spine for LLM context caches.
* [Kits](using/kits.md) — Installing and updating the vocabulary packs that define document types.

### Kit development

* [Kit Authoring](kit-development/kit-authoring.md) — Layout, file ownership rules, and the local-to-kit development flow.
* [Kit Ontology Design](kit-development/kit-ontology.md) — Classes, enums, and property shapes.
* [Ontology Guidelines](kit-development/ontology-guidelines.md) — Naming conventions, identifier rules, and reference properties.
* [Hook Authoring](kit-development/hook-authoring.md) — Pre-commit gates and post-tool lifecycle hooks.
* [Harness-Specific Features](kit-development/harness-specific-features.md) — Multi-substrate integration, frontmatter dialects, and harness hook translation (Claude Code, Google Antigravity, Hermes).
* [Engine Runtime Dirs](kit-development/engine-runtime-dirs.md) — The `_ignore/` pocket law: committed vs. untracked directories.
