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

<!-- TODO(additive): the history cookbook — asking temporal questions of the
     event graph (this is the deep power; needs worked examples), joining
     facts to authors/dates, cross-repo queries via shared kits -->
