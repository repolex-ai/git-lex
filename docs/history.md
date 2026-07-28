# History: how git-lex remembers

Most databases overwrite. git-lex doesn't: every fact that ever entered or
left the graph is kept as an event tied to the commit that caused it.

Concretely, each save's changes are diffed and recorded as **assert** ("this
fact became true") and **retract** ("this fact stopped being true") events,
each pointing at its git commit — so every event carries an author and a
date for free.

- `git lex show <thing>` answers: *what is true now?*
- `git lex log <thing>` answers: *what happened, when, by whom?*

Renaming or moving files produces **zero** phantom events — history tracks
facts, not file shuffling.

<!-- TODO(additive): the event model in RDF terms (rdf:reifies / triple
     terms) for readers who want the semantics; rebuild + migration notes -->
