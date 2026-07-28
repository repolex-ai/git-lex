# Session Notes & Roadmap — 2026-03-26

## What We Built Today

### git-lex v0.0.1 — SPARQL Over Git
- Rust CLI installs as `git lex` subcommand
- Translates git objects (commits, filetree, refs, changesets, blame, language) into RDF triples on the fly
- Persistent oxigraph store at `.lex/oxigraph/` (gitignored)
- Full SPARQL 1.1 queries, sub-millisecond on persistent store
- Cross-graph joins between git data and agent-written `.lex/*.nq` knowledge
- RDF 1.2 triple terms confirmed working: `<<( s p o )>>` syntax with `rdf:reifies`

### Commands
- `git lex init` — set up `.lex/`, `.gitattributes`, `.gitignore`, global drivers
- `git lex sync` — generate virtual triples from git + load `.lex/*.nq` into oxigraph
- `git lex query 'SPARQL...'` — query with auto-injected prefixes
- `git lex log` — commit history (pretty or nq)
- `git lex tree` — file tree at any ref
- `git lex refs` — branches and tags
- `git lex status` — lexification progress (lexified / stale / unlexified)
- `git lex dump` — dump all generated NQ to stdout (debug)

### Ontology
- **git-lex.ttl** — base ontology for git objects (7 classes, 25 properties)
  - Namespace: `lex: = https://repolex.ai/ontology/git-lex/`
- **lex-o.ttl** — upper ontology by OntologistClaude
  - Namespace: `lex-o: = https://repolex.ai/ontology/lex-upper/`
  - 7 top-level classes, 6 core relationships, SKOS broader/narrower
  - RDF 1.2 provenance properties (observedBy, confidence, observedAt)
- **Content ontology** — per-repo, namespaced by first commit SHA
  - e.g., `o: = https://repolex.ai/ont/9b179c0a/`
  - Content classes subclass lex-o: terms

### Grounding Model
- `lex-o:mentionedInPath` — file path string (joins with `lex:path`)
- `lex-o:mentionedInBlob` — blob URI (content-addressed, stale detection)
- `git lex status` compares current blob hashes against lexified blobs
- RDF 1.2 `rdf:reifies` for relationship-level provenance

### Key Decisions
- Namespace: `https://repolex.ai/ontology/git-lex/` (separate from repolex)
- Instance base: `https://repolex.ai/r/{org}/{repo}/`
- Storage: `.nq` files in `.lex/graph/` (committed to git, line-per-triple, diffable)
- Oxigraph cache: `.lex/oxigraph/` (gitignored, rebuilt by sync)
- Ontology: `.ttl` for readability, parsed for prefix injection, not loaded into oxigraph
- Agents write normal files to repo root, ontology agent writes triples to `.lex/graph/`
- Per-repo content namespace via first commit SHA (forks share namespace)

## Landscape Analysis

Evaluated existing tools — none use actual git:
- **Memoria** — SQL on MatrixOne, "git" is marketing (14 methods, 234 lines)
- **SAGE** — blockchain consensus + SQLite, no git, no KG
- **DiffMem** — markdown + grep, barely uses git (9 git calls)
- **ASIMOV** — Rust ETL pipeline for RDF, early stage
- **Graphiti** — temporal KG on Neo4j, not git-based

## Architecture

```
Content (markdown, docs)     →  committed to repo root
                                    ↓
Agent or NLP extracts        →  raw entities + relationships
                                    ↓
Ontology agent classifies    →  .lex/graph/*.nq (committed)
                                    ↓
git lex sync                 →  .lex/oxigraph/ (local cache)
                                    ↓
git lex query                →  SPARQL across git + knowledge
```

Two-pass extraction:
1. **Extract** — dump entities and relationships (no types, no ontology decisions)
2. **Reconcile** — ontology agent classifies, subclasses, evolves schema

## Roadmap / TODO

### High Priority
- [ ] `git lex add rels --doc <path>` — stdin-based batch relationship creation with auto entity stubs, reports unclassified entities/predicates
- [ ] Semantic diff driver — `git-lex diff-driver` implementation for `.nq` files
- [ ] Semantic merge driver — `git-lex merge-driver` for triple-level conflict resolution
- [ ] Resolve data model to accommodate enrichment pieces (NLP, JSON, etc.)

### Enrichment Modules (Dynamic Loading)
Per-filetype triple extractors that load additional tooling only when needed:
- [ ] `-nlp` module — GLiNER2 → ONNX → ORT for markdown/text entity+relationship extraction
- [ ] `-json` module — JSON file parsing (Claude Code conversations, config files, API responses)
- [ ] `-obsidian` module — Obsidian-specific features (wikilinks, frontmatter, backlinks)
- [ ] These should be optional features / separate crates, not part of the core package unless requested
- [ ] Rust approach: feature flags in Cargo.toml, or separate binaries that git-lex shells out to

### Templates
`git lex init --template <name>` — starter KG templates for common use cases:
- [ ] `agentmemory` — single agent memory repo (inbox, preferences, learned facts)
- [ ] `multiagentmemory` — multi-agent with per-agent identity, shared knowledge
- [ ] `decisions` — decision log with temporal traces, context, outcomes
- [ ] `conversations` — Claude Code / LLM conversation analysis
- [ ] `docrepo` — document repository (papers, notes, research)
- [ ] `obsidian` — Obsidian vault with wikilink-aware extraction
- [ ] Templates would include: starter ontology extending lex-o:, example `.nq` structure, recommended schema, `.gitattributes`, agent steering rules

### Reasoner
- [ ] `git lex reason` — OWL 2 RL reasoning via `reasonable` crate
- [ ] Materializes inferred triples (subclass, inverse, transitive) into store
- [ ] Runs after sync, before query

### Other
- [ ] `git lex entities` — list all entities with types and source docs
- [ ] `git lex describe <entity>` — show everything about an entity
- [ ] `git lex grep` — SPARQL + grep hybrid (find entity mentions with line numbers)
- [ ] Incremental sync (don't rebuild everything, diff the changes)
- [ ] Claude hooks injection system (push-based knowledge delivery)
- [ ] `git lex extract` — NLP extraction using ONNX runtime
- [ ] Research KGGen and similar KG construction tools
- [ ] Prefix injection from .ttl file (like lexq) instead of hardcoded

## Research TODO
- [ ] KGGen — investigate for NLP extraction approaches
- [ ] Similar KG construction tools — survey the landscape
- [ ] Rust ONNX Runtime (`ort` crate) — evaluate for GLiNER2 integration
- [ ] Dynamic loading patterns in Rust — feature flags vs plugin architecture vs subprocess
