# Sync Graph & Squad Kit Design

**Date:** 2026-03-30
**From:** OntologistClaude + Rob
**Status:** Design complete, ready for implementation

## The Big Insight

Triple stores aren't meant to be edited. They're append-only representations of things that change elsewhere. Git is where the changes happen (files created, edited, deleted). Oxigraph is where the accumulated knowledge lives — piled on, never modified, always growing.

## Architecture Overview

```
Git (source of truth)          Oxigraph (accumulated knowledge)
├── .md files with lex: yaml   ├── Git virtual triples (regenerated at HEAD)
├── .spo extraction sidecars   ├── Sync graphs (append-only, one per sync)
├── .lex/ontology.nq           │   ├── /sync/abc123/ (delta)
└── git history                │   ├── /sync/def456/ (delta)
                               │   └── /sync/ghi789/ (delta)
                               └── Triple terms (provenance per assertion)
```

## Two Views of the Same Repo

**Git virtual triples = current truth at HEAD.** Regenerated every sync from git commands. Ephemeral. "What does git say right now?"

**Sync graphs = knowledge through time.** Persistent, append-only. Each sync produces a named graph containing only the delta (new/changed/retracted assertions). "What have we known, and when?"

Day-to-day queries ("what do we know about X?") use both — virtual triples for git data, sync graphs for extracted knowledge.

Evolution queries ("how did our understanding change?") use sync graphs exclusively — time travel without git checkout.

## Sync Mechanics

A sync compares the current state to the last sync point:

```
1. git diff since last sync → which files changed, added, deleted
2. Run extractors on changed files → new .spo sidecars
3. Diff sidecars → new assertions, changed assertions, removed assertions
4. Write delta to new sync named graph in oxigraph
5. Enrich each assertion with triple terms (from git metadata)
6. Write retraction markers for deleted file assertions
```

**Lazy sync:** Syncs don't have to happen every commit. A sync can span multiple commits. The delta is everything since the last sync.

**No checkout needed:** Everything comes from `git diff` and current file state. Never touches git history.

**Append-only:** No sync graph is ever modified after creation. Retractions are new annotations in newer sync graphs.

## Named Graphs

```
/sync/{commit_sha}/    ← assertions extracted during this sync
                         named by the commit synced TO
                         contains ONLY the delta since last sync
```

For current state: query across ALL sync graphs, exclude retracted.
For state at time T: query sync graphs with commit dates <= T, exclude retracted before T.

## Triple Terms (RDF 1.2)

Each assertion in a sync graph gets provenance via triple terms:

```nq
# The assertion
<entity/oauth2~006ba2ee> lex:linksTo <entity/auth-module~006ba2ee> <sync/ghi789> .

# Provenance
<ann/x> rdf:reifies <<( <entity/oauth2~006ba2ee> lex:linksTo <entity/auth-module~006ba2ee> )>> <sync/ghi789> .
<ann/x> lex:filePath "decisions/oauth.md" <sync/ghi789> .
<ann/x> lex:blobHash "006ba2ee" <sync/ghi789> .
<ann/x> git:commitId "def456" <sync/ghi789> .
<ann/x> lex:extractor "wikilink" <sync/ghi789> .
```

**Required triple terms:**
- `lex:filePath` — which file (needed for retractions when file is deleted)
- `lex:blobHash` — which content version (needed for stale detection)
- `git:commitId` — which specific commit (needed for multi-commit syncs and time queries)

**Optional:**
- `lex:extractor` — which extractor produced this (fm, wikilink, mention, llm)

**Not needed:**
- Timestamp — derived from git commit via join, not stored separately
- Assertion log — the .spo sidecars committed to git serve this purpose

## Retractions

When a file is deleted:

```
1. git diff shows: decisions/oauth.md DELETED in commit ghi789
2. Query oxigraph: which assertions have lex:filePath "decisions/oauth.md"?
3. Write retraction markers in /sync/ghi789/:
```

```nq
<ann/original> lex:retracted true <sync/ghi789> .
```

Original assertions stay in their original sync graph (immutable history). Retraction marker goes in the new sync graph. Current state query filters them out.

## Extraction Sidecars (.spo)

Per-file extraction output, committed to git:

```
.lex/extract/
├── fm/decisions/oauth.md.spo        ← frontmatter extraction
├── wikilink/decisions/oauth.md.spo  ← wikilink extraction
└── llm/decisions/oauth.md.spo       ← LLM extraction (optional)
```

Format: `{blobsha}/{filepath} | subject | predicate | object`

```
006ba2ee/decisions/oauth.md | oauth2 | replaces | api-keys
006ba2ee/decisions/oauth.md | oauth2 | linksTo | auth-module
```

**Sidecars are lean** — no triple terms, no RDF. Just raw tuples. The sync step enriches them with git metadata when writing to oxigraph.

**Sidecars are replayable** — if oxigraph is lost, re-run `git lex sync --rebuild` to replay all sidecars from git history.

## Tier 1 Extractors (Built-in, No LLM)

```
1. fm:        YAML frontmatter key-values → fm:{key} predicates
2. wikilink:  [[reference]] → lex:linksTo relationships
3. @mention:  @agentname → lex:mentions relationships
4. md link:   [text](url) → lex:linksTo or lex:externalLink
```

Run on all text surfaces: document bodies, commit messages, git notes.

## Frontmatter Conventions

Regular frontmatter → `fm:` namespace (dynamic, no ontology):
```yaml
---
title: OAuth Migration Decision
tags: [security, auth]
date: 2026-03-28
---
```

RDF-aware frontmatter → `lex:` block signals structured knowledge:
```yaml
---
title: OAuth Migration Decision
tags: [security, auth]
lex:
  type: Decision
  decidedBy: ontologistclaude
  alternatives: [API keys, JWT, OAuth2]
  outcome: implemented
---
```

Nested YAML uses dot notation: `fm:deploy.target`

## Namespace Hierarchy

All under `https://repolex.ai/ontology/git-lex/`:

```
git:     .../git-lex/git/       Git objects (commits, blobs, refs, files, changesets)
fm:      (dynamic)              Frontmatter key-values (no ontology needed)
lex:     .../git-lex/lex/       Tool features (mentions, linksTo, provenance)
lex-o:   .../git-lex/lex-o/     Upper ontology (Thing, Concept, Physical, Process...)
squad:   .../git-lex/kit/squad/ Squad kit (Agent, Message, Decision, Task...)
```

Ontology files mirror namespace paths:

```
ontology/git-lex/
├── git-lex.ttl              ← unified (all merged)
├── git/git.ttl              ← git objects
├── fm/fm.ttl                ← frontmatter (minimal)
├── lex/lex.ttl              ← tool features
├── lex-o/lex-o.ttl          ← upper ontology
└── kit/squad/squad.ttl      ← squad kit
```

## Squad Kit Document Types

Each becomes a `git lex create` command:

| Type | Subclass Of | Key Properties |
|---|---|---|
| Agent | lex-o:Physical | substrate (carbon/silicon), role, expertise, agentEmail |
| Message | lex-o:Information | from, to, messageStatus, priority, inReplyTo |
| Decision | lex-o:Decision | decidedBy, alternatives, rationale, outcome, supersededBy |
| Discovery | lex-o:Information | foundBy, implications, confidence |
| Task | lex-o:Process | assignedTo, taskStatus, blocks, blockedBy, relatedDecision |
| Project | lex-o:Concept | repo, projectStatus, squadMembers |
| Note | lex-o:Information | topic, relatedTo |

Tags use `fm:tags` from frontmatter — no duplication.

## Repo Configuration

`.lex/repo.yml` created at init:

```yaml
name: tripleforce-memory
kit: squad
created: 2026-03-30
version: 1.0
```

The kit field locks valid document types. `git lex create memory` fails if kit is `squad` and `memory` isn't a squad document type.

## Key Commands

```bash
git lex init --kit squad          # Set up repo with squad ontology
git lex create decision           # Scaffold a decision document
git lex save "message"            # Add + commit + sync in one command
git lex sync                      # Extract + write to oxigraph
git lex sync --rebuild            # Rebuild oxigraph from full git history
git lex query "SPARQL..."         # Query the knowledge graph
git lex status                    # Show lexification status
```

## Example Queries Enabled by Sync Graph

**"When did we first decide to use OAuth2?"**
Query sync graphs for earliest Decision mentioning OAuth2.

**"What has CoderClaude learned this week?"**
Query Discoveries where foundBy is CoderClaude, join against commit dates.

**"What knowledge have we lost?"**
Query for retracted assertions — things once believed, no longer valid.

**"How has our understanding of auth-module evolved?"**
All assertions about auth-module across all sync graphs, ordered by commit date.

**"What's the blast radius if we delete this document?"**
Query which assertions are grounded to that file via lex:filePath.

**"Was Sarah always a friend?"**
Full history: Enemy (January) → Friend (March). The transition is visible, including the commit that changed it and why.

## Portability

The sync graph structure is use-case agnostic. Tested against: squad, solo agent memory, decision log, conversation analysis, Obsidian vault, document repository, multi-squad swarm, fork-and-diverge. No breaking cases found. The sync layer tracks assertions through time regardless of document type. The kit ontology gives the assertions meaning.
