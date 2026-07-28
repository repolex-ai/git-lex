# Querying

Two doors:

- **`git lex query "SPARQL"`** — a live view of your working tree (current
  files + the git commit layer). What's true right now, including
  uncommitted edits.
- **`git lex serve sparql`** — a standard SPARQL endpoint over the synced
  store, which also holds the full history.

Common prefixes are injected automatically (`git-lex:`, `git2:`, `md:`,
`fm:`, and your kit's).

## Starters

```sparql
# Everything, raw
SELECT * WHERE { ?s ?p ?o } LIMIT 20

# All documents by type
SELECT ?doc ?type WHERE { ?doc a ?type } LIMIT 20

# Commits
SELECT ?c WHERE { ?c a git2:Commit } LIMIT 5
```

## History: "when did this change, and who changed it?"

Run against the synced store (`git lex serve sparql`, or any SPARQL client).
Replace `day-56` with any fragment of the document's IRI:

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

<!-- TODO(additive): more temporal recipes (facts true at a date, most-edited
     documents, per-author changes); cross-repo queries via shared kits -->
