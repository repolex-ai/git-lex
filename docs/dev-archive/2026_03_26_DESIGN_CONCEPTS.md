# git-lex Design Concepts

Working notes from initial brainstorming. Everything here is formative — directions, not decisions.

## Core Thesis

Git is already a DAG with content-addressed immutable nodes, branching, merging, diffing, blame, and full temporal history. That's a knowledge graph runtime. We just need a query layer and semantic tooling on top.

## What Lives Where

**In the repo (normal git):**
- Content files — markdown, docs, papers, notes, whatever agents produce
- The content IS the knowledge. Agents read and write it naturally.

**In `.lex/` (the index layer):**
- Derived knowledge that can't be obtained directly from git
- Triples/relationships extracted from content
- Schema/ontology definitions
- Potentially a lightweight graph index (must be git-diff-friendly, NOT binary stores like oxigraph/rocksdb)

**Derivable from git directly (don't duplicate in .lex/):**
- File tree structure → `git ls-tree`
- Commit history, authors, dates → `git log`
- Branch/tag state → `git refs`
- Diffs between versions → `git diff`
- Blame/provenance → `git blame`
- File content at any point → `git show sha:path`
- Temporal queries → `git log --since/--until`

## Git Extension Points (no git modding required)

| Extension | Mechanism | Purpose |
|---|---|---|
| `git lex query` | Custom subcommand (git-lex on PATH) | Query the knowledge graph |
| `git lex log` | Custom subcommand | Temporal knowledge queries |
| `git lex diff` | Custom diff driver (.gitattributes) | Semantic diff on KG files |
| `git lex merge` | Custom merge driver (.gitattributes) | Semantic merge resolution |
| `git lex init` | Custom subcommand | Set up .lex/ and .gitattributes |
| textconv | Diff driver option | Transform files before diffing |
| git notes | `refs/notes/lex` | Annotate commits with semantic metadata |
| pre-commit hook | .git/hooks/ | Validate KG consistency |
| post-commit hook | .git/hooks/ | Auto-update .lex/ index |

## Installation Model

```bash
# One-time global install
cargo install git-lex    # puts git-lex on PATH → git lex subcommand
git lex install          # writes diff/merge drivers to ~/.gitconfig

# Per-repo setup
cd my-knowledge-repo
git lex init             # creates .lex/, .gitattributes, hooks
```

## Temporal Model

HEAD = what we know NOW. Git history = the past. Free temporal indexing:

```bash
git lex query --at="2026-02-26" "OAuth"     # what did we know then?
git lex log --entity="auth-module"           # knowledge evolution
git lex diff --since="2026-02-26"            # what changed?
```

Under the hood: `git show` and `git diff` with semantic layer on top.

## Multi-Agent Model

**Author-based identity** (preferred over worktrees or branches):
- Each agent gets its own git author email: `agent-a@lex.local`
- Everyone works on the same branch, same checkout
- `git log --author="agent-a"` = everything that agent contributed
- `git blame` = which agent wrote each piece of knowledge
- `git shortlog -sn` = contribution summary per agent

**Repomaster Agent:**
- Dedicated agent whose job is managing the knowledge graph
- Processes raw content from other agents (classifies, extracts, links)
- Manages and evolves the ontology/schema
- Resolves contradictions between agents
- Only entity that writes to `.lex/graph/` (other agents write content to repo root)

**Injection System (push-based knowledge delivery):**
- Claude hooks monitor agent conversations
- Reactive: user asks "remember that paper by X?" → hook queries git-lex → injects context
- Proactive: hook monitors conversation topics → pushes relevant knowledge without being asked

## RDF Question

Still open: what format for triples in `.lex/`?

| Format | Git-diff friendly? | Agent-friendly? | Reasoning support? |
|---|---|---|---|
| N-Triples (.nt) | Excellent (one triple per line) | Needs tooling | With OWL reasoner |
| JSON-LD (.jsonld) | Good | Native (it's JSON) | With context/OWL |
| Turtle (.ttl) | Okay (multi-line blocks) | Readable | With OWL reasoner |
| Markdown + frontmatter | Excellent | Agents love it | Only if we parse to RDF |

JSON-LD is interesting: valid JSON (agents read/write natively) AND valid RDF (supports rdfs:subClassOf, owl:inverseOf). OntologistClaude suggested this — worth exploring.

The evolutionary ontology concept (ontology evolves with the corpus, managed by the Repomaster) is described in the repolex ontology-builder docs.

## Open Questions

1. Can we build a useful query layer without a binary store? (grep over .nt files + in-memory graph traversal?)
2. What's the minimal .lex/ structure that's useful?
3. How does the injection system work mechanically? (Claude hooks → git-lex query → context injection)
4. Should .lex/ triples be RDF or something simpler?
5. How does the Repomaster agent run? (daemon, hook-triggered, manual?)
6. What can we reuse from ASIMOV's Rust crates?
