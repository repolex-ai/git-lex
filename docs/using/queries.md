# Querying

Two querying doors:

- **`git lex query "SPARQL"`** — a live view of your working tree (current
  files plus the Git commit layer). Reflects what is true right now, including
  uncommitted edits. It is rebuilt from the working tree on every run and
  does not read the synced store — so **history is invisible here**; a
  history query returns zero rows, not an error.
- **`git lex serve sparql`** — a standard W3C SPARQL endpoint over the
  synced store, which also holds full repository history. Default
  `http://127.0.0.1:7880/sparql`, Swagger UI at `/swagger-ui`. Run
  `git lex sync` first to populate the store.

Common prefixes are injected automatically on both doors (`git-lex:`,
`git2:`, `md:`, `fm:`, `rdf:`, `rdfs:`, `owl:`, `xsd:`, and your kit's —
e.g. `soul:`).

Two common pitfalls that can produce zero rows without an error:

- **Every repolex namespace ends in a slash, never a hash.** `git-lex/`,
  `soul/`, `copia/` — a hand-typed `PREFIX gl: <…/git-lex#>` is
  syntactically valid, matches nothing, and produces no warning. Prefer the
  injected prefixes over typing your own.
- **Universal properties emit under `git-lex/`, not your kit.** A
  frontmatter key is written `soul.Exploration.relatedToId`, but `id`,
  `relatedToId`, `fileId` and the other universal properties are
  git-lex vocabulary — query them as `git-lex:relatedToId`. Only
  class-specific keys (`soulDay`, `explorationStatus`, …) live under
  the kit's namespace. The key path does not name the emitted IRI.

When a query returns zero rows and you suspect an IRI mismatch rather than
missing data, ask the graph what vocabulary it actually uses:

```sparql
SELECT DISTINCT ?p WHERE { ?s ?p ?o } ORDER BY ?p
```

One semantic difference to know: `git lex query` searches across all its
graphs, so bare `?s ?p ?o` matches everything. The SPARQL endpoint follows the
W3C default strictly, and the synced store keeps its data in **named**
graphs — so a bare pattern there matches almost nothing. Wrap patterns in
`GRAPH <…> { … }`: current state lives in
`<https://repolex.ai/git-lex/NamedGraph/now>`, history in
`<https://repolex.ai/git-lex/LexHistoryGraph>`, commits in
`<https://repolex.ai/git-lex/NamedGraph/commits>`.

## Every Named Graph, and Which Door Has It

All graph names share the base `https://repolex.ai/git-lex/NamedGraph/`
except the history graph, which has its own IRI. "Query door" is
`git lex query`; "serve door" is the SPARQL endpoint over the synced store.

| Graph | Holds | Query door | Serve door |
|---|---|---|---|
| `now` | Current-state facts from your documents | yes | yes |
| `…/LexHistoryGraph` | Every assertion and retraction ever, with provenance | no | yes |
| `commits` | One node per commit: sha, author, time, message | yes | yes |
| `refs` | Branches and tags, and the commit each points at | yes | yes |
| `repo` | The repository itself: genesis sha, and the facts from `.lex/repo.yml` (name, kit, version, agent, created) | yes | yes |
| `filetree/<sha>` | Every file at one commit, as index entries. There is only ever one such graph | the current HEAD | the last synced commit |
| `repo-ontology` | The installed kits' schema, queryable | no | yes |

The `repo-ontology` graph contains the ontology of every installed kit, loaded as data. It answers queries like "what fields can a Journal carry?" without reading TTL files directly:

```sparql
SELECT ?property ?range WHERE {
  GRAPH <https://repolex.ai/git-lex/NamedGraph/repo-ontology> {
    ?property rdfs:domain soul:Journal .
    OPTIONAL { ?property rdfs:range ?range }
  }
}
```

It lives only in the synced store (it is loaded at `init` and `kit-update`),
so this query runs on the serve door — `git lex query` returns zero rows
for it.

## Saved Queries

A query you run frequently does not need to be retyped. `git lex query <name>`
runs the query saved at `.lex/query/<name>.md`:

```bash
git lex query recent      # runs .lex/query/recent.md
git lex query things
```

A saved query is plain Markdown. **The first fenced code block is the query**;
everything else in the file is documentation for the query — what it
answers, parameters to edit, and how it is structured. Frontmatter, if present, is
skipped. A file with no code fence is treated entirely as the query.

````markdown
# What changed lately

Documents by their last change, newest first.

```sparql
SELECT ?doc ?date
WHERE { ?doc <https://repolex.ai/ontology/git-lex/updatedDate> ?date }
ORDER BY DESC(?date)
LIMIT 20
```
````

Two starters — `things` (every typed thing in the repo, counted by class) and
`recent` (documents by last change) — are written into `.lex/query/` the first
time the folder is created. After that the folder is fully user-managed: `init` and
`kit-update` never overwrite what is in it. Save custom queries
alongside them as `.lex/query/<name>.md`.

Anything that is not a saved-query name runs as SPARQL text, so inline queries
work seamlessly. A name-shaped argument that matches no file lists what
is available:

```
No stored query named 'recnt'. Available: recent, things
```

Saved queries run through `git lex query`, so they see the live view of
the working tree — not the synced store or history.

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

## History: "When did this change, and who changed it?"

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

The `<<( ... )>>` blocks are RDF 1.2 triple terms — the syntax in which history events
are stored.

## Lifespan: "When did this fact become true, and when did it stop?"

Run against the synced store. An assertion and its retraction are **separate events**, so joining them requires matching on the reified triple. This query computes each fact's lifespan; a fact still active today returns with `?died` unbound:

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

(The `MIN` and ordinal filter pair each assertion with its earliest following retraction, correctly handling facts that are retracted and re-asserted over time.)
