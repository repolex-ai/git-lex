# Landscape Analysis: Agent Memory & Knowledge Graph Systems

Evaluated three existing systems to understand the state of the art and identify gaps.

## Memoria (matrixorigin/Memoria)

**What it claims:** "Git for AI Agent Memory" — persistent memory with branching, snapshots, merge.

**What it actually is:** A SQL-based memory store on MatrixOne (MySQL-compatible HTAP database). The "git" branding is marketing — zero git internals, zero libgit2, zero git commands. The `GitForDataService` is 14 methods / 234 lines wrapping MatrixOne's proprietary DDL:

- `CREATE SNAPSHOT` → MatrixOne DDL, not git
- `data branch create table` → creates a new SQL table via CoW
- `data branch merge` → SQL-level row reconciliation
- `data branch diff` → compares two tables

MatrixOne itself doesn't use git either — it's LSM tree + MVCC snapshot isolation, marketed as "Copy-on-Write."

**KG quality:** Weak. NER is 32 hardcoded tech terms + capitalized word detection + hyphenated suffix matching. Graph nodes: Episodic/Semantic/Scene/Entity. Edges: temporal/causal/association. No ontology, no RDF, no SPARQL.

**Good ideas worth stealing:**
- Trust tiers (T1 verified → T4 unverified) with confidence decay
- Governance: contradiction detection, quarantine low-confidence, scheduled cleanup
- Branching for experimental memory (even if their implementation is SQL, the concept is sound)

**Key metrics (via lexq):**
- `SqlMemoryStore`: 74 methods, 2,723 lines — the actual product
- `GitForDataService`: 14 methods, 234 lines — the "git" feature
- Top dependency: sqlx (1,764 calls) — it's a database app

## SAGE (l33tdawg/sage)

**What it claims:** "Sovereign Agent Governed Experience" — consensus-validated memory.

**What it actually is:** CometBFT (blockchain consensus) + SQLite + Ed25519 signatures. Every memory write goes through 4 BFT validators (Sentinel, Dedup, Quality, Consistency) requiring 3/4 quorum.

**No git, no RDF, no KG.** The "brain graph" in their dashboard is a force-directed visualization, not a queryable knowledge graph. Storage is SQLite + BadgerDB.

**Good ideas worth stealing:**
- Validation before commit — memories should be validated (our Repomaster agent serves this role)
- Multi-agent identity with per-agent permissions and clearance levels
- Agent pipeline for inter-agent messaging

## DiffMem (Growth-Kinetics/DiffMem)

**What it claims:** "Git-based differential memory for AI agents."

**Closest to our idea** but much simpler. Markdown files in a git repo, retrieval agent uses shell commands (grep, git log, git diff, git blame) to find context.

**Key findings (via lexq):**
- Total codebase: 3,387 lines Python across 10 source files
- Uses git worktrees for multi-user isolation
- WriterAgent calls OpenAI (via OpenRouter) to process sessions into entities
- Retrieval agent is basically an LLM that runs shell commands
- No ontology, no semantic diff/merge, no query layer beyond grep
- `jedi` (Python code analysis): 291 calls — it's more code-context than general memory
- `git`: 9 calls — barely touching git despite the name

**Architecture traced via lexq call graph:**
```
server.py (entry point, 3-line main)
├── RepoManager — git worktrees per user
├── DiffMemory (api.py) — the core API
│   ├── process_session → WriterAgent → _call_llm (OpenAI)
│   ├── get_context → retrieval_agent (LLM + shell commands)
│   └── get_recent_timeline / get_repo_status
└── periodic_sync / sync_user_to_github
```

## The Gap

Nobody is combining all three pieces:

| Capability | Who has it? |
|---|---|
| Git as storage/versioning | DiffMem (but no KG) |
| Knowledge graph with semantics | Graphiti, Semantica (but not git-based) |
| Custom git diff/merge for semantic ops | Nobody for KGs |
| SPARQL/RDF queryability | Nobody in the memory space |
| Multi-agent coordination | SAGE (but no KG, no git) |

## ASIMOV (asimov-platform)

Arto Bendiken's Rust-based flow/dataflow platform for ETL into RDF triples. Modular system with blocks, systems, and flows. Relevant crates: `asimov-graph`, `asimov-kb`, `asimov-ontology`. Still heavily under construction. Could potentially provide infrastructure for the query/triple layer, but doesn't have a SPARQL Anything-style virtual graph projection over arbitrary sources.

## Conclusion

Everyone wants git semantics for data, but nobody is actually using git. They rebuild worse versions on top of databases. The opportunity is to use actual git, extended with semantic tooling.
