# git-lex

*Last updated for git-lex v0.1.1 (2026-08-27)*

> **A versioned, queryable knowledge graph that lives inside your git repository.**

git-lex is a tool for building decentralized knowledge graphs out of plain text files. By writing standard Markdown documents with a small amount of structured frontmatter, you establish a semantic graph of Things and Files. Because it rides on git commits, the graph inherits a complete historical record: not just what is currently true, but when it became true, and how it evolved over time.

---

## 1. Quick Start

### Easiest Install (via Cargo)
```bash
cargo install --git https://github.com/repolex-ai/git-lex
git lex init
```

---

## 2. Key Features

* **Dual-Plane Duality**: It separates the physical File Plane (repo-relative file paths) from the semantic Thing Plane (stable, persistent concepts). You can rename, move, or reorganize files in your workspace without breaking links or severing graph relations.
* **SPARQL Over Commits**: Run standard SPARQL queries directly against your local git history. Ask questions like *"Find all notes related to the Swarm Intelligence pursuit that were active last week,"* and query the exact state of the graph at any commit.
* **Continuous Substrate Validation**: Use declarative SHACL shapes to validate your knowledge graph on every commit. If an agent or human writes a document with an undeclared property or a broken link, `git-lex` flags it immediately at the pre-commit gate.

---

## 3. The Duality of Text and Graph

Traditional databases separate your project's prose (documentation, journal entries, specs) from its structured logic. Files move, paths break, and the history of *why* a connection was made is lost in database transaction logs. 

git-lex bridges this division. It establishes a duality between the **File Plane** (the physical files you edit) and the **Thing Plane** (the conceptual entities they represent). When you commit a file, git-lex extracts its properties, links, and history into a local triple store. 

The graph is not a separate application; it is a native property of your repository. It version-controls your thinking with the same precision, branching, and attribution you bring to your code. If two agents collaborate, their conceptual graphs merge cleanly via git merges, providing a robust, decentralized substrate for collective intelligence.

---

## 4. Documentation Index

The docs split by what you are doing, because the two halves share almost
nothing. Most people only ever need the first.

### Using git-lex

You have a repo and you want a graph out of it.

* [Getting Started](using/getting-started.md) — Install and run your first query in five minutes.
* [Writing Documents](using/writing-documents.md) — Document structures, frontmatter syntax, and markdown links.
* [Files and Things](using/files-and-things.md) — The File Plane and the Thing Plane, and which of your facts live where.
* [Moving, Renaming, and Deleting](using/renames-moves-deletes.md) — What survives a file moving, and how links heal themselves.
* [Commands](using/commands.md) — The complete CLI command reference.
* [Querying with SPARQL](using/queries.md) — Query your repository, inline or saved, with worked examples.
* [History](using/history.md) — Query the graph as it stood at any commit.
* [Serve & Visualize](using/serve-and-viz.md) — A local SPARQL endpoint and an interactive graph explorer.
* [Exporting the graph](using/export-index.md) — Snapshot the synced store as a COTTAS file and an LLM-context-cache spine.
* [Kits](using/kits.md) — Installing and updating the vocabulary packs that define document types.

### Kit development

You are building a kit for other people to install. Everything here is
optional unless you are shipping vocabulary to someone else.

* [Kit Authoring](kit-development/kit-authoring.md) — Layout, file ownership rules, and the local-to-kit development flow.
* [Kit Ontology Design](kit-development/kit-ontology.md) — Classes, enums, and property shapes.
* [Ontology Guidelines](kit-development/ontology-guidelines.md) — Naming conventions, identifier rules, and reference properties.
* [Hook Authoring](kit-development/hook-authoring.md) — Pre-commit gates and post-tool lifecycle hooks.
* [Engine Runtime Dirs](kit-development/engine-runtime-dirs.md) — The `_ignore/` pocket law: committed vs. untracked directories.
