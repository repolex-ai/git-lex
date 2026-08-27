# Moving, renaming, and deleting documents

> **Partly settled.** Link healing and derived-state cleanup landed
> 2026-08-14 and are described below as shipped behavior. The remaining
> lifecycle rules land with the lifecycle spec.

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
- **Renames and moves of files:** handled at save. `git lex save` reads
  git's own rename detection and does two things in the same commit — it
  rewrites every markdown link that pointed at the old path, in canonical
  root-relative form, and it moves the file's derived sidecar rather than
  regenerating it. You do not run anything extra.

  Two limits worth knowing. Only **inbound** links are rewritten — links
  *pointing at* the moved file. A moved file's own `../`-relative outbound
  links are left alone (root-relative links, the canonical form, survive any
  move untouched). And frontmatter references are never rewritten: ids do not
  follow filenames, deliberately.
- **Deletes:** the derived sidecar for a deleted document is cleaned up at
  save, and a sidecar whose source document has gone missing some other way
  is swept the next time you save.
- **Never touch `.lex/` by hand** — not to fix a rename, not for anything.
  Save reconciles it for you; deleting files under `.lex/extract/` silently
  drops documents from the graph.

## History is never lost

When an identity changes or a document is deleted, its facts are *retracted*,
not erased: the history graph keeps every assertion and retraction with the
commit that caused it. You can always ask what was true, and when it stopped
being true, through `git lex serve sparql`.
