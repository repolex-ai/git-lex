# Writing Documents

Documents are plain Markdown with YAML frontmatter in dot notation:

```yaml
---
soul.Journal.soulDay: 56
soul.Journal.earthDate: 2026-07-23
---
Body text. Link other documents with [markdown links](Soul/Note/example.md).
```

The pattern is `kit.Class.property`. Class names are case-sensitive and come
from your kit — check the `__ClassName.md` template files in each folder for
every valid property.

## Fields Every Document Has

Five fields are shared by every class in every kit, and every class template
lists them first. You write them with your document's **own** class in the
key — `soul.Note.title`, `copia.Being.cue` — never any other class name:

```yaml
soul.Note.id:           # which Thing this document IS — <namespace/Class/identifier>
soul.Note.title:        # one short name, single value
soul.Note.abstract:     # a short summary, single value
soul.Note.cue:          # when to reach for this document — list for more than one
soul.Note.relatedToId:  # another document's identifier — list for more than one
```

All five are optional. `title`, `abstract`, and `cue` take plain text.
`id` and `relatedToId` take the identifier notation, angle brackets
included — `<soul/Journal/day-7>` — or a full IRI. What a Thing is, and why
an `id` survives a file rename, is explained in
[Files and Things](files-and-things.md); here it's enough to know the
fields exist and what to type.

## More Than One Value for a Key

Use a YAML list:

```yaml
---
copia.Outfit.outfitId: "abyssal-drift"
copia.Outfit.includesItemId:
  - "abyssal-veil"
  - "lumen-strand"
---
```

**Do not repeat the key.** Repeating it looks like it should add a second
value, but standard YAML parsers keep only the last one and drop the rest:

```yaml
# WRONG — "abyssal-veil" is lost, only "lumen-strand" survives
copia.Outfit.includesItemId: "abyssal-veil"
copia.Outfit.includesItemId: "lumen-strand"
```

Git-lex rejects repeated keys during `git lex save` to prevent silent data loss.
Whether a given property may hold more than one value is defined by your kit's
ontology — the list form is how to write it.

## Empty Values

An empty value counts as not written — and whitespace-only counts as empty,
so `" "` does not count as a value. Leaving optional fields blank, the way the
templates scaffold them, is completely valid.

A field your kit marks **required** (`# required` in the template) is
different: leaving it empty fails validation on save, and the error identifies the
file and property to fill. A document with a class key is always validated.

## Properties Declared Without a Class

Some kits declare a property that belongs to no single class (such as `soul:relatedTo`).
Those are legal on any class: write them with your document's own
class in the key — `soul.Note.relatedTo` — and the value behaves exactly as
the kit declared it (reference or plain text). They do not appear in class
templates precisely because they belong to no single class.

## The Reference Rule

**A reference is an identifier in angle brackets, a repo-relative path, or a
full IRI — the graph never guesses.**

- Frontmatter, property with a **declared class range** (the common
  case): the value is the target's bare **id** — `assignedTo: selkie`.
  The ontology names the class, the id names the Thing, and git-lex
  derives the canonical IRI. Dangling IDs fail validation at save.
- Frontmatter, reference property **without** a declared range (`id` and
  `relatedToId` are the everyday examples): the value is the identifier
  notation `<namespace/Class/identifier>` — e.g. `<soul/Journal/day-7>`,
  brackets included, where the namespace comes from the value — or a
  repo-relative path (`source: friend/selkie.md`), or a full IRI.
- Body text: a standard Markdown link —
  `[day 56](Soul/Journal/2026-07-23-day-56.md)` — becomes a generic
  `linksTo` edge. Targets are repo-root-relative; `.md` is added automatically
  when the target has no extension. `[[...]]` is not parsed into edges — it
  remains plain prose.
- Linking to a file that does not exist yet will not error if created in the
  same save; links to missing files produce warnings on save.

Note: Avoid `[[...]]` or `@...` syntax inside frontmatter values — use IDs,
identifiers in angle brackets, paths, or full IRIs.
