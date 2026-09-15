# git-lex

**Turn any git repo of Markdown into a queryable knowledge graph.**

git-lex turns your Markdown files and git history into a W3C RDF/SPARQL knowledge graph without running a separate database. Write plain notes and documents with lightweight frontmatter, and git-lex extracts, validates, and indexes everything straight into git.

## Installation

```bash
cargo install --git https://github.com/repolex-ai/git-lex --locked
```

*(Prebuilt binaries for macOS and Linux are also available on the [Releases](https://github.com/repolex-ai/git-lex/releases) page).*

---

If you came here for soul repos, you'll want to install the soul kit:

```bash
git lex kit-add soul
```

- More about soul kits: https://github.com/repolex-ai/git-lex-kit-soul
- Full git-lex documentation is here: https://github.com/repolex-ai/git-lex/blob/main/docs/index.md

---

## Why git-lex?

Most knowledge management tools, wikis, and agent memory architectures force you into an awkward split: either you keep notes in simple text files and lose structured, relational querying, or you maintain a complex external database (like Neo4j, vector stores, or triplestores) that drifts out of sync with your files and version control.

git-lex bridges this divide by making Git itself your knowledge graph. Git already provides cryptographic identity, distributed branching, human/agent authorship, and an immutable commit timeline. By layering a W3C RDF 1.2 semantic index directly over your repository, git-lex gives you the power of SPARQL queries, schema validation, and historical time-travel—while your files remain clean, portable Markdown you can edit with any editor or AI coding agent.

Whether you are creating a persistent cognitive memory store for an autonomous agent (a "soul repo"), organizing a technical codebase, or curating personal research notes, git-lex ensures your knowledge is verifiable, durable, and completely under your control.

---

## Features

- **Git is the Database**: No background daemons, servers, or external databases to keep running. Your repository is the database, and the graph is derived directly from your committed files and working tree.
- **Markdown-First Authoring**: Write natural Markdown notes and link them using standard markdown syntax. Lightweight YAML frontmatter defines typed properties that extract into graph statements automatically.
- **Standard SPARQL 1.2 Querying**: Query your entire graph with SPARQL over working-tree files (`git lex query`) or explore past revisions with the embedded Oxigraph store.
- **SHACL Pre-Commit Validation**: Prevent broken links, missing fields, and typos before they reach history. `git lex save` validates your documents against declarative SHACL shapes at commit time, catching errors early.
- **Files and Things Duality**: Move, rename, or reorganize files without breaking graph relations. git-lex distinguishes between the physical file path (File Plane) and the persistent semantic concept (Thing Plane) it expresses.
- **Temporal History & Provenance**: Powered by RDF 1.2 triple terms, every statement in the graph knows exactly which commit and file asserted or retracted it. Query what was true at any point in your repo's history.
- **Modular Kit Ecosystem**: Customize your graph's ontology and document scaffolding for specific domains. Install official kits like `soul` for personal and agent memory, or author custom kits with your own shapes and templates.
- **Local Visualization & SPARQL Endpoint**: Explore your knowledge graph visually in your browser with `git lex serve viz`, or expose a standards-compliant SPARQL endpoint with `git lex serve sparql`.
- **Built for Humans and AI Agents**: Seamless integration for human note-takers and AI agents alike, including `export-spine` to write compact semantic indexes tailored for neural context caches.

---

## Quick Start

```bash
mkdir my-graph && cd my-graph && git init
git lex init                     # initialize .lex/ in the repo
git lex create Memory "first"    # scaffold a typed document
git lex save "my first memory"   # extract + SHACL-validate + commit
git lex query "SELECT * WHERE { ?s ?p ?o } LIMIT 10"
```

---

## License

[Unlicense](LICENSE) — public domain.
