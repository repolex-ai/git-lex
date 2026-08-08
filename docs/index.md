# git-lex

*Last updated for git-lex v0.1.0 (2026-07-29)*

**A knowledge graph that lives in your git repo.**

You write markdown files with a little structured frontmatter. git-lex turns
them into a queryable knowledge graph — and because everything rides on git
commits, the graph has full history: not just *what is true*, but *when it
became true and when it stopped*.

- [Getting started](getting-started.md) — install to first query in five minutes
- [Commands](commands.md) — the full command reference
- [Writing documents](writing-documents.md) — frontmatter, markdown links, document types
- [Kits](kits.md) — the vocabulary packs that define your document types
- [Kit authoring](kit-authoring.md) — building kits: layout, file ownership, the full hook development flow
- [Kit ontology design](kit-ontology.md) — defining your document types: classes, ids, enums, references
- [Ontology guidelines](ontology-guidelines.md) — the naming and identifier standard: the identity law, the four kinds of id-valued properties, reference naming
- [Hook authoring](hook-authoring.md) — placeholder; hook material lives in Kit authoring §3 until the docs pass
- [History](history.md) — how git-lex remembers everything that ever changed
- [Querying](queries.md) — SPARQL over your repo, with worked examples
- [Serve & visualize](serve-and-viz.md) — the local web view and SPARQL endpoint

<!-- TODO(additive): a screenshot of the viz + a 30-second example walkthrough -->
