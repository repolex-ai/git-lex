# git-lex Data Architecture Spec

**Date:** 2026-03-28
**From:** OntologistClaude + Rob
**Status:** Design complete, ready for implementation

## Three Layers

### 1. Extraction Log — raw string tuples, NOT RDF

Format per line: `{blobsha}/{filepath} | subject | predicate | object`

```
006ba2ee/notes/friends.md | sarah | isA | friend
006ba2ee/notes/friends.md | sarah | hasA | cat
006ba2ee/notes/friends.md | sarah | likes | rob
f3a1b7c9/notes/enemies.md | sarah | isA | enemy
```

Written by extractors (fm extractor built-in, llm extractor optional). Each extraction is grounded to its source file via blob+path identity. This is the "source code" of the knowledge — sacred, replayable.

Frontmatter extraction uses the same format:

```
006ba2ee/notes/oauth-decision.md | title | hasValue | OAuth Migration Decision
006ba2ee/notes/oauth-decision.md | tags | hasValue | security
006ba2ee/notes/oauth-decision.md | tags | hasValue | auth
```

Stored as sidecars per-file, combined into an assertion log per-commit.

### 2. Knowledge Graph — resolved RDF triples in oxigraph

Entities + their types + their relationships, all in ONE graph. No separate entity graph. Entity URIs encode their source: `<entity/sarah~006ba2ee>`. Entities from different files are different individuals by default.

```nq
<entity/sarah~006ba2ee> rdf:type <o:Friend> .
<entity/sarah~006ba2ee> lex:name "Sarah" .
<entity/sarah~006ba2ee> <o:owns> <entity/cat~006ba2ee> .
```

With RDF 1.2 triple terms for provenance:

```nq
<<( <entity/sarah~006ba2ee> rdf:type <o:Friend> )>> lex:extractor "llm" .
<<( <entity/sarah~006ba2ee> rdf:type <o:Friend> )>> lex:filePath "notes/friends.md" .
<<( <entity/sarah~006ba2ee> rdf:type <o:Friend> )>> lex:blobHash "006ba2ee" .
```

When a file is deleted, assertions grounded to it get `lex:retracted true` annotations.

Named graphs per commit (delta only) for history:

```
<lex/commit/abc123>  ← new triples from this commit + triple term annotations
<lex/commit/def456>  ← delta triples from this commit
```

### 3. Ontology — OWL classes, properties, constraints

Defines what types exist and what relationships are valid between them.

Three sub-layers:
- **lex-o:** upper ontology (shared, fixed) — Physical, Abstract, Information, Concept, Event, Decision, etc.
- **{hash}:** per-repo content ontology (evolved by Repomaster) — Friend, Enemy, Faction, owns, hates, etc.
- **Domain/range constraints** for validation — Person owns Animal, not the reverse

The ontology is what makes SPARQL powerful — `rdfs:subClassOf` enables hierarchical queries. Disambiguation happens through classification, not a separate entity resolution step.

## Four-Step Pipeline

```
Step 1: Extract (per-file, mechanical)
  - fm extractor: parse YAML frontmatter → string tuples
  - llm extractor: LLM reads doc → string tuples (KGGen-style)
  - Output: .spo sidecar files with blob/path grounding
  - Frontmatter is built-in. LLM extraction is optional.

Step 2: Assert (per-commit, mechanical)
  - Diff the extraction sidecars (new/changed/removed)
  - Produce the assertion delta for this commit
  - Commit the assertion log to git (replayable history)

Step 3: Resolve (aggregate, LLM-assisted)
  - Turn string tuples into RDF triples
  - Create entity URIs (or match to existing in knowledge graph)
  - Map predicates to ontology terms (or flag as unknown)
  - Disambiguate: same name from different files = different entities
  - Merge: LLM decides if entities across files are the same → owl:sameAs
  - Validate: domain/range constraints from ontology
  - Output: new/updated triples for the knowledge graph

Step 4: Reason + Curate (aggregate, LLM + reasoner)
  - Run reasoner mechanically (inverses, transitives, subclass materialization)
  - LLM reviews reasoning output
  - LLM proposes ontology mutations for assertions that don't fit
  - LLM verifies consistency before committing
  - Only commits after validation passes
```

## Key Design Decisions

1. **Entity identity = name + source file (blob+path).** Different files = different individuals by default. Merging is explicit via owl:sameAs.

2. **Extraction log is NOT RDF.** Just text tuples. RDF happens at the resolve step (step 3). This keeps extraction fast and format-agnostic.

3. **Knowledge graph is ONE graph.** Entities + relationships together. No separate entity graph. An entity is just a URI that has triples about it.

4. **Disambiguation via ontology/subclassing.** Not a separate entity resolution step. AutoSchemaKG-inspired — the schema handles it.

5. **Extraction log is replayable.** Can rebuild the entire knowledge graph from scratch by replaying the assertion log through steps 3-4.

6. **Named graphs per commit (delta only)** with RDF 1.2 triple terms for provenance. Efficient storage, full history.

7. **File deletion = retraction.** All assertions grounded to a deleted file get `lex:retracted true` annotations.

8. **Git virtual triples = current truth at HEAD.** Oxigraph = accumulated history. Two complementary views.

## Oxigraph Structure

```
Git virtual triples (ephemeral, regenerated):
  commits graph     ← commit metadata
  filetree graph    ← current file listing
  refs graph        ← branches and tags
  changeset graphs  ← per-commit file changes
  blame graphs      ← per-file author attribution
  frontmatter graph ← parsed YAML key-values

Knowledge graph (persistent, accumulated):
  <lex/commit/abc123>  ← delta: new triples + triple term annotations
  <lex/commit/def456>  ← delta: new triples from this commit
  ...
```

## File Structure

```
.lex/
├── extract/
│   ├── fm/
│   │   ├── notes/friends.md.spo        ← frontmatter extraction
│   │   └── notes/oauth-decision.md.spo
│   └── llm/
│       ├── notes/friends.md.spo        ← llm extraction
│       └── notes/oauth-decision.md.spo
├── assertions.log                       ← combined extraction log
├── ontology.nq                          ← evolved content ontology
└── oxigraph/                            ← persistent store (.gitignored)
```

## Entity URI Pattern

```
<https://repolex.ai/r/{org}/{repo}/entity/{name}~{blobsha}>
```

Short form in queries with prefix:
```
<entity/sarah~006ba2ee>
```

When entities are merged across files:
```
<entity/sarah~006ba2ee> owl:sameAs <entity/sarah~f3a1b7c9> .
```

## Frontmatter Namespace

`fm:` is a dynamic namespace. No ontology needed. Every YAML key becomes a predicate:

```nq
<file> fm:title "OAuth Migration Decision" .
<file> fm:tags "security" .
<file> fm:deploy.target "production" .    ← nested keys use dot notation
```
