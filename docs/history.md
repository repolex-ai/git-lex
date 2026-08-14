# History: how git-lex remembers

*Last updated for git-lex v0.1.0 (2026-08-12)*

Most databases overwrite. git-lex doesn't: every fact that ever entered or
left the graph is kept as an event tied to the commit that caused it.

Concretely, `git lex sync` walks your commits and records each fact change
as an **assert** event ("this fact became true") or a **retract** event
("this fact stopped being true"), each pointing at its git commit — so
every event carries an author and a date for free. (`save` writes and
commits; `sync` is the separate step that updates the store.)

Renaming or moving a file doesn't churn its facts — a document's identity
is its `id`, not its path, so a pure move records only the document→file
link changing, nothing else.

## Where history answers: `git lex serve sparql`

History lives in the **synced store**, and only one door reads it:

- **`git lex serve sparql`** — yes. A standard SPARQL endpoint over the
  synced store.
- **`git lex query`** — **no.** It rebuilds a fresh view of your working
  tree and never opens the synced store, so a history pattern there returns
  zero rows — no error, just silence.

At the endpoint, history sits in one named graph and standard SPARQL
semantics apply: wrap the pattern in
`GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> { ... }` or you get
zero rows. The ready-made history query in [Querying](queries.md) does
this for you.

## The event model — and the join everyone gets wrong

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
`gl:assertedIn`, a different event carries `gl:retractedIn`. So the obvious
lifespan query is wrong:

```sparql
# WRONG — returns empty, no error
?e gl:assertedIn ?a ; gl:retractedIn ?r .
```

You join on the *reified triple*, not on the event:

```sparql
# RIGHT
?e1 rdf:reifies <<( ?s ?p ?o )>> ; gl:assertedIn ?a .
?e2 rdf:reifies <<( ?s ?p ?o )>> ; gl:retractedIn ?r .
```

Current state stays cheap: what's true *now* is also stored as plain
triples, so `?s ?p ?o` answers "what is true" without touching the
reification at all.

## History is the main branch

git-lex records the semantic history of **the project as a whole**: the
default branch (`main`). Running `sync` anywhere else is refused with a
message. This is a deliberate break from raw git — branch experiments
belong to git; they enter the *knowledge graph's* history when they merge,
because the merged line IS the meaning of the repo. (One graph, one
timeline, no "current state" ambiguity.)

## Rebuilding

The store is derived, never the source. `sync` normally appends
incrementally from where it left off; to rebuild from scratch, delete
`.lex/_ignore/oxigraph` and run `git lex sync` — the whole graph is
re-derived from commit history.
