# Files and Things

**DRAFT — for review by Rob and tr1p**

*Written for git-lex v0.1.0 (2026-08-12)*

git-lex tracks two different kinds of identity, and knowing which is which
tells you where your facts live and whether they survive a file being moved.

## A File is an address

Every file in the repo gets a **File** node. Its id *is* its repo-relative
path:

    Soul/Journal/day-7.md  →  git-lex/File/Soul/Journal/day-7.md

Because the path is the id, a File node is an address, not a lifetime. Rename
the file and that address now points at nothing; a later, unrelated file at
the same path is a new state of the *same* address. File-plane facts live
here: the links in your prose (`linksTo` edges), free-form frontmatter, git
facts. You never author a File identity — every file has one automatically,
even in a repo with no kit installed.

## A Thing is a lifetime

A **Thing** is the entity a document *expresses*, as opposed to the file
expressing it. Its id is authored, and its IRI is built from namespace, class,
and id — no path anywhere in it:

    soul/Journal/day-7

Every foldered class in every kit — a Note, a Journal, a Being — is a Thing
(`rdfs:subClassOf git-lex:Thing` in the ontology). When a document has a Thing
identity, its kit-declared facts (`soul.Journal.earthDate: ...`) land on the
Thing node, not on the File node.

## Why facts about a Thing survive a move

The Thing's IRI contains no path, so moving the file cannot change it. The
only connection between the two planes is one derived edge, **fileId** —
"the File currently expressing this Thing" — which git-lex stamps at save and
re-derives when the file moves:

    soul/Journal/day-7  --fileId-->  git-lex/File/Soul/Journal/day-7.md

Rename `day-7.md` and the old fileId edge is retracted, a new one asserted.
Every fact on the Thing stays put. (File-plane facts, like prose links,
re-anchor to the new address — they belong to the file, not the Thing.) The
history of fileId answers "where has this Thing lived?"

## How a document gets a Thing identity

Author the class's id line — the property is always the class name with `Id`
on the end (`noteId` for a Note, `journalId` for a Journal), and the value is
a bare identifier:

```yaml
---
soul.Note.noteId: "graph-thoughts"
---
```

`git lex create note graph-thoughts` writes this line for you — the id you
pass becomes the filename *and* the id value.

There is also a universal `id` field (see below), written fully qualified:
`soul.Note.id: <soul/Note/graph-thoughts>`. It states which Thing this
document IS, and it resolves to exactly the document's own Thing IRI. Today
the `<class>Id` line is what the tool reads to anchor the document; how the
two relate long-term is a question for Rob and tr1p.

## What happens when you don't

Nothing is lost — but the facts bind to the path. A classed document with no
id anchors no Thing: its facts save attached to the File node, and they die on
rename the way any File-plane fact does. Save tells you, with the exact line
to add:

    warning: Soul/Note/graph-thoughts.md: this soul.Note document has no id.
    Fix: add this line to the YAML block at the top of the file:
    soul.Note.noteId: "graph-thoughts"

(If the warning instead says the *class* has no id key in its ontology,
that's not yours to fix — report it to the kit owner. Your facts still save,
attached to the file.)

## The five universal properties

Because every foldered class is a Thing, five properties are declared once —
on `git-lex:Thing` — and inherited by every class in every kit. You write
them with your document's own class (`soul.Note.title`, never
`git-lex.Thing.title`):

| key          | value                                             |
|--------------|---------------------------------------------------|
| `id`         | which Thing this document IS — `<soul/Note/graph-thoughts>` |
| `title`      | one short name, what a listing shows (single-valued) |
| `abstract`   | a short summary (single-valued)                   |
| `cue`        | WHEN to reach for this — a situation, not a topic (list) |
| `relatedToId`| another Thing, any class in any kit — `<copia/Texture/deep-water>` (list) |

The `<namespace/Class/identifier>` form in `id` and `relatedToId` is what
makes cross-kit references work without coordination: the namespace comes
from the value, never from the writing document's kit.
