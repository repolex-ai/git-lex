# History: How Git-lex Remembers

Most databases overwrite data in place. Git-lex does not: every fact that ever entered or
left the graph is preserved as an event tied to the commit that caused it.

Concretely, `git lex sync` walks your commits and records each fact change
as an **assert** event ("this fact became true") or a **retract** event
("this fact stopped being true"), each pointing at its Git commit — so
every event carries an author and timestamp automatically. (`git lex save` writes and
commits; `git lex sync` updates the persistent graph store.)

Renaming or moving a file does not churn its facts — a document's identity
is its `id`, not its path, so a pure move records only the document-to-file
link changing.

## Where History Answers: `git lex query`

History lives in the **synced store**, which [gitlexd](gitlexd.md) holds:

- **`git lex query`** — Yes. It asks gitlexd, which answers from the
  synced store.
- **`git lex direct`** — **No.** It rebuilds a fresh view of your working
  tree and does not open the synced store, so a history pattern there returns
  zero rows.

History sits in one named graph,
`<https://repolex.ai/git-lex/LexHistoryGraph>`; a pattern with no GRAPH
clause sees it along with everything else, and `GRAPH <…> { ... }` keeps a
pattern to it. The ready-made history queries in [Querying](queries.md)
provide templates for this.

## The Event Model and Joins

Each event is one node carrying three facts: its class, the statement it
chronicles (an RDF 1.2 triple term), and its commit:

```
<event> a gl:SpoEvent .
<event> rdf:reifies <<( ?s ?p ?o )>> .
<event> gl:assertedIn <.../git2/Commit/sha> .   # or gl:retractedIn
```

(`gl:` is `https://repolex.ai/ontology/git-lex/` — declare it in your
query; it is not one of the auto-injected prefixes.)

**Assertion and retraction are separate reified nodes** — one event carries
`gl:assertedIn`, a different event carries `gl:retractedIn`. To query lifespans, join on the *reified triple*, not on the event:

```sparql
# Matching assertions and retractions
?e1 rdf:reifies <<( ?s ?p ?o )>> ; gl:assertedIn ?a .
?e2 rdf:reifies <<( ?s ?p ?o )>> ; gl:retractedIn ?r .
```

Current state remains fast and lightweight: what is true right now is stored as plain
triples, so `?s ?p ?o` answers current questions without evaluating reification.

## History Tracks the Main Branch

Git-lex records the semantic history of the project as a whole: the
default branch (`main`). Running `sync` on feature branches is refused. Branch experiments
belong to Git; they enter the knowledge graph's history when they merge into `main`,
providing a single, unified timeline of repository state.

## Rebuilding

The store is derived, never the source of truth. `sync` normally appends
incrementally from where it left off; to rebuild from scratch, delete
`.lex/_ignore/oxigraph` and run `git lex sync` — the whole graph is
re-derived from your Git commit history.
