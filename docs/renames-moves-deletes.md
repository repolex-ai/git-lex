# Moving, renaming, and deleting documents

> **DRAFT — behavior is being finalized for the release.** This page states
> what is true today and what is safe practice; the full rules land with the
> lifecycle spec.

## The one fact that explains everything else

**The filename is not the identity.** A document's identity is its id field
in the frontmatter (`soul.Note.noteId: "field-notes"`), which mints the
persistent Thing the graph knows it by. The filename usually *matches* the
id because `git lex create <type> <id>` fills both from the same argument —
but they are independent: renaming the file does not change the object, and
changing the id field creates a new object even if the file never moves.

## Two kinds of links, two different rules

- **Markdown links** in a document's body (`[text](/Soul/Pursuit/x.md)`)
  point at *files by path*. If the target path stops existing, the link is
  recorded as unresolved — visible, not lost.
- **Frontmatter references** (`relatedToId` and friends) point at *Things by
  identity*. They don't care where the target's file lives; they care that
  the identity exists.

## What to do today

- **Changing a document's identity:** edit the id field in place and save.
  This is the clean path — the old identity's facts are properly retracted
  into history and the new identity is asserted, in one commit.
- **Renames, moves, and deletes of files:** currently leave stale derived
  state behind (the graph can keep facts about the old path). Prefer
  id-field edits over file renames until the release finalizes these rules.
  If you must rename or delete, run `git lex save` and `git lex sync`
  afterward and expect the graph to lag.
- **Never touch `.lex/` by hand** — not to fix a rename, not for anything.
  Save reconciles it for you; deleting files under `.lex/extract/` silently
  drops documents from the graph.

## History is never lost

When an identity changes or a document is deleted, its facts are *retracted*,
not erased: the history graph keeps every assertion and retraction with the
commit that caused it. You can always ask what was true, and when it stopped
being true, through `git lex serve sparql`.
