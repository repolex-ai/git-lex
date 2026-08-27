# Querying

*Last updated 2026-08-12 (git-lex v0.1.0)*

Two doors:

- **`git lex query "SPARQL"`** — a live view of your working tree (current
  files + the git commit layer). What's true right now, including
  uncommitted edits. It is rebuilt from the working tree on every run and
  never opens the synced store — so **history is invisible here**; a
  history query returns zero rows, not an error.
- **`git lex serve sparql`** — a standard W3C SPARQL endpoint over the
  synced store, which also holds the full history. Default
  `http://127.0.0.1:7880/sparql`, Swagger UI at `/swagger-ui`. Run
  `git lex sync` first to populate the store.

Common prefixes are injected automatically on both doors (`git-lex:`,
`git2:`, `md:`, `fm:`, `rdf:`, `rdfs:`, `owl:`, `xsd:`, and your kit's —
e.g. `soul:`).

One semantic difference to know: `git lex query` searches across all its
graphs, so bare `?s ?p ?o` matches everything. The endpoint follows the
W3C default strictly, and the synced store keeps its data in **named**
graphs — so a bare pattern there matches almost nothing. Wrap patterns in
`GRAPH <…> { … }`: current state lives in
`<https://repolex.ai/git-lex/NamedGraph/now>`, history in
`<https://repolex.ai/git-lex/LexHistoryGraph>`, commits in
`<https://repolex.ai/git-lex/NamedGraph/commits>`.

## Saved queries

A query you will run again does not need to be retyped. `git lex query <name>`
runs the query saved at `.lex/query/<name>.md`:

```bash
git lex query recent      # runs .lex/query/recent.md
git lex query things
```

A saved query is plain markdown. **The first fenced code block is the query**;
everything else in the file is notes for whoever reads it next — what the query
answers, what to edit, why it is shaped that way. Frontmatter, if present, is
skipped. A file with no code fence is all query.

````markdown
# What changed lately

Documents by their last change, newest first.

```sparql
SELECT ?doc ?date
WHERE { ?doc <https://repolex.ai/ontology/git-lex/dateUpdated> ?date }
ORDER BY DESC(?date)
LIMIT 20
```
````

Two starters — `things` (every typed thing in the repo, counted by class) and
`recent` (documents by last change) — are written into `.lex/query/` the first
time the folder is created. After that the folder is yours: `init` and
`kit-update` never add to it or overwrite what is in it. Save your own
alongside them as `.lex/query/<name>.md`.

Anything that is not a saved-query name runs as SPARQL text, so inline queries
work exactly as before. A name-shaped argument that matches no file lists what
*is* available rather than handing the name to the SPARQL parser:

```
No stored query named 'recnt'. Available: recent, things
```

Saved queries run through `git lex query`, so they see the same live view of
the working tree — not the synced store, and not history.

## Starters

These run with `git lex query`:

```sparql
# Everything, raw
SELECT * WHERE { ?s ?p ?o } LIMIT 20

# All documents by type
SELECT ?doc ?type WHERE { ?doc a ?type } LIMIT 20

# Documents of one class (any kit class works the same way)
SELECT ?s WHERE { ?s a soul:Note }

# Which documents link to which — markdown links become md:linksTo edges
SELECT ?from ?to WHERE { ?from md:linksTo ?to } LIMIT 20

# Commits
SELECT ?c WHERE { ?c a git2:Commit } LIMIT 5
```

## History: "when did this change, and who changed it?"

Run against the synced store (`git lex serve sparql`, or any SPARQL client)
— not `git lex query`. Replace `day-56` with any fragment of the
document's IRI:

```sparql
PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
PREFIX gl:  <https://repolex.ai/ontology/git-lex/>
PREFIX g2:  <https://repolex.ai/ontology/git-lex/git2/>

SELECT ?when ?event ?doc ?property ?value ?author WHERE {
  GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> {
    { ?e rdf:reifies <<( ?doc ?property ?value )>> ; gl:assertedIn ?c .
      BIND("ASSERT" AS ?event) }
    UNION
    { ?e rdf:reifies <<( ?doc ?property ?value )>> ; gl:retractedIn ?c .
      BIND("RETRACT" AS ?event) }
    FILTER(CONTAINS(STR(?doc), "day-56"))
  }
  GRAPH <https://repolex.ai/git-lex/NamedGraph/commits> {
    ?c g2:ordinalDerived ?ordinal ; g2:author ?sig .
    ?sig g2:xsdDateTimeDerived ?when .
    OPTIONAL { ?sig g2:signatureName ?author }
  }
} ORDER BY ASC(?ordinal)
```

The `<<( ... )>>` blocks are RDF 1.2 triple terms — the syntax history events
are stored in. Copy this query and edit the FILTER; that beats writing it
from scratch.

## Lifespan: "when did this fact become true, and when did it stop?"

Same door: the synced store only. One trap to know first: an assertion and
its retraction are **separate events**, so the obvious join

```sparql
# WRONG — returns empty, no error
?e gl:assertedIn ?a ; gl:retractedIn ?r .
```

matches nothing (no event carries both). Join the two events on the
reified triple instead. This gives each fact's lifespan; a fact still true
today comes back with `?died` unbound:

```sparql
PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
PREFIX gl:  <https://repolex.ai/ontology/git-lex/>
PREFIX g2:  <https://repolex.ai/ontology/git-lex/git2/>

SELECT ?doc ?property ?value ?born (MIN(?retracted) AS ?died) WHERE {
  GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> {
    ?e1 rdf:reifies <<( ?doc ?property ?value )>> ; gl:assertedIn ?ca .
    FILTER(CONTAINS(STR(?doc), "SOUL.md"))
  }
  GRAPH <https://repolex.ai/git-lex/NamedGraph/commits> {
    ?ca g2:ordinalDerived ?oa ; g2:author ?sigA .
    ?sigA g2:xsdDateTimeDerived ?born .
  }
  OPTIONAL {
    GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> {
      ?e2 rdf:reifies <<( ?doc ?property ?value )>> ; gl:retractedIn ?cr .
    }
    GRAPH <https://repolex.ai/git-lex/NamedGraph/commits> {
      ?cr g2:ordinalDerived ?or ; g2:author ?sigR .
      ?sigR g2:xsdDateTimeDerived ?retracted .
    }
    FILTER(?or >= ?oa)
  }
}
GROUP BY ?doc ?property ?value ?born
ORDER BY ?born
```

(The `MIN` + ordinal filter pairs each assertion with its *earliest
following* retraction, so a fact that was retracted and later re-asserted
gets one row per life.)

<!-- TODO(additive): more temporal recipes (facts true at a date, most-edited
     documents, per-author changes); cross-repo queries via shared kits -->
