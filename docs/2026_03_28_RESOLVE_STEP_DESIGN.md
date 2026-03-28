# Resolve Step Design

**Date:** 2026-03-28
**From:** WarezClaude + OntologistClaude + Rob

## Overview

The resolve step transforms the extraction log (string tuples) into RDF N-Quads
with commit-scoped named graphs and RDF 1.2 triple term provenance.

Two decoupled steps:
- **Step A: Mechanical RDF transformation** — string tuples → N-Quads with URIs
- **Step B: Ontology evolution** — promote string types/predicates to OWL classes/properties

This doc covers Step A. Step B is OntologistClaude's 5-phase algorithm.

## Input

Extraction log diff (new/removed lines since last resolve):

```
a5e001e0/repolex-ai/octobody/2025_09_18_ANATOMY_GUIDE.md | mantle | isA | core-system
a5e001e0/repolex-ai/octobody/2025_09_18_ANATOMY_GUIDE.md | mantle | contains | system1
```

## Output

N-Quads with commit-scoped named graph and triple term annotations:

```nq
<https://repolex.ai/r/{org}/{repo}/entity/mantle~a5e001e0> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> "core-system" <https://repolex.ai/r/{org}/{repo}/commit/{hash}> .
<https://repolex.ai/r/{org}/{repo}/entity/mantle~a5e001e0> "contains" <https://repolex.ai/r/{org}/{repo}/entity/system1~a5e001e0> <https://repolex.ai/r/{org}/{repo}/commit/{hash}> .
<https://repolex.ai/r/{org}/{repo}/ann/{spo-hash-1}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> <<( <.../entity/mantle~a5e001e0> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> "core-system" )>> <.../commit/{hash}> .
<https://repolex.ai/r/{org}/{repo}/ann/{spo-hash-1}> <https://repolex.ai/ontology/git-lex/git/extractor> "claude-haiku-4-5" <.../commit/{hash}> .
<https://repolex.ai/r/{org}/{repo}/ann/{spo-hash-1}> <https://repolex.ai/ontology/git-lex/git/filePath> "repolex-ai/octobody/2025_09_18_ANATOMY_GUIDE.md" <.../commit/{hash}> .
<https://repolex.ai/r/{org}/{repo}/ann/{spo-hash-1}> <https://repolex.ai/ontology/git-lex/git/blobHash> "a5e001e0" <.../commit/{hash}> .
```

## Named Graph Pattern

Each commit gets its own named graph:

```
<https://repolex.ai/r/{org}/{repo}/commit/{commit_hash}>
```

Matches the git: virtual triple commit URI pattern. The commit entity in the
git graph and the assertion named graph share the same URI root.

Commit named graphs are **immutable** — never modified after creation.
Oxigraph is append-only.

## Entity URI Pattern

```
<https://repolex.ai/r/{org}/{repo}/entity/{name}~{blobhash}>
```

- `name` is the lowercase-with-dashes entity name from the extraction
- `blobhash` is the short (8 char) blob hash of the source file
- Entities from different files are different individuals by default
- Cross-file merging is explicit via owl:sameAs (ontology step)

## Annotation URI Pattern

```
<https://repolex.ai/r/{org}/{repo}/ann/{hash}>
```

Where `hash` is a short SHA of the S+P+O concatenation. Deterministic —
same triple always produces the same annotation URI. Stable for git diff.

## Predicate Handling

- `isA` → `rdf:type` with object as string literal (not OWL class yet)
- `hasValue` (frontmatter) → `fm:{key}` predicate (already handled by fm extractor)
- All other predicates → string literal predicates (not URIs yet)

The ontology step (Step B) promotes string predicates to proper URIs:
```
"contains" → o:contains rdfs:subPropertyOf lex-o:partOf
```

## Retractions

When an extraction log line is REMOVED (file changed or deleted):

```nq
<.../ann/{hash-of-removed-spo}> <.../git/retracted> "true"^^xsd:boolean <.../commit/{new-hash}> .
```

The original assertion stays in its original commit graph. The retraction
is in the NEW commit's graph. Current truth = query all, filter out retracted.

## Diff Logic

1. Read current `extraction.log.spo`
2. Read previous committed version: `git show HEAD~1:.lex/extraction.log.spo`
3. New lines (in current, not in previous) → new assertions
4. Removed lines (in previous, not in current) → retractions
5. Generate N-Quads for both
6. Write to `.lex/graph/knowledge.nq` (append for new, add retraction annotations)
7. Load into oxigraph

## Extractor Identification

The model/extractor name comes from the `.spo` sidecar filename:
- `file.fm.spo` → extractor is "fm"
- `file.claude-haiku-4-5.spo` → extractor is "claude-haiku-4-5"

The extraction log doesn't carry the extractor name, but the compile step
could be enhanced to include it (as a 5th field or a comment).

## Command

```bash
git lex resolve          # resolve new assertions from extraction log diff
git lex resolve --full   # resolve everything (rebuild from scratch)
```
