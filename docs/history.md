# History: how git-lex remembers

Most databases overwrite. git-lex doesn't: every fact that ever entered or
left the graph is kept as an event tied to the commit that caused it.

Concretely, each save's changes are diffed and recorded as **assert** ("this
fact became true") and **retract** ("this fact stopped being true") events,
each pointing at its git commit — so every event carries an author and a
date for free.

Current state is a query away (or just the document itself); history
questions are asked with SPARQL — see [Querying](queries.md) for the
ready-made history query.

Renaming or moving files produces **zero** phantom events — history tracks
facts, not file shuffling.

## History is the main branch

git-lex records the semantic history of **the project as a whole**: the
default branch (`main`). Running `sync` anywhere else is refused with a
message. This is a deliberate break from raw git — branch experiments
belong to git; they enter the *knowledge graph's* history when they merge,
because the merged line IS the meaning of the repo. (One graph, one
timeline, no "current state" ambiguity.)

<!-- TODO(additive): the event model in RDF terms (rdf:reifies / triple
     terms) for readers who want the semantics; rebuild + migration notes -->
