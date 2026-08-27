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

### Getting Started & Reference
* [Getting Started](getting-started.md) — Install and run your first query in five minutes.
* [Commands](commands.md) — The complete CLI command reference.

### Authoring Documents & Identity
* [Writing Documents](writing-documents.md) — Document structures, frontmatter syntax, and markdown links.
* [Files and Things](files-and-things.md) — Deep dive into the File Plane and the Thing Plane.
* [Moving, Renaming, and Deleting](renames-moves-deletes.md) — How git-lex tracks identity and heals links across file mutations.
* [Ontology Guidelines](ontology-guidelines.md) — Standard naming conventions, identifier rules, and reference properties.

### Kits (Vocabulary Packs)
* [Kits](kits.md) — Understand vocabulary packs that define document schemas.
* [Kit Ontology Design](kit-ontology.md) — Creating custom schemas: classes, enums, and property shapes.
* [Kit Authoring](kit-authoring.md) — Layout design, file ownership rules, and local-to-kit development.

### Graph Execution & Tooling
* [History](history.md) — How git-lex tracks and queries graph states across commits.
* [Querying with SPARQL](queries.md) — Query your repository with worked examples.
* [Serve & Visualize](serve-and-viz.md) — Spin up a local SPARQL endpoint and interactive graph explorer.
* [Hook Authoring](hook-authoring.md) — Writing pre-commit gates and post-tool lifecycle hooks.
* [Engine Runtime Dirs](engine-runtime-dirs.md) — The `_ignore/` pocket law: committed vs. untracked directories.
