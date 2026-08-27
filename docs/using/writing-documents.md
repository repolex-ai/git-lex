# Writing documents

*Last updated for git-lex v0.1.0 (2026-08-12)*

Documents are plain markdown with YAML frontmatter in dot notation:

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

## Fields every document has

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
[files-and-things.md](files-and-things.md); here it's enough to know the
fields exist and what to type.

## More than one value for a key

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
value, and it doesn't — YAML keeps only the last one and throws the rest
away:

```yaml
# WRONG — "abyssal-veil" is lost, only "lumen-strand" survives
copia.Outfit.includesItemId: "abyssal-veil"
copia.Outfit.includesItemId: "lumen-strand"
```

git-lex rejects a repeated key at save and names it, so the mistake can no
longer land quietly. Before that gate existed it did land quietly: 28
documents in one soul repo kept only their last value, and the graph read as
though the rest had never been written.

Whether a given property *may* hold more than one value is a question for
your kit's ontology — the list form is how you write it when it does.

## Empty values

An empty value counts as not written — and whitespace-only counts as empty,
so `" "` doesn't sneak past. Leaving optional fields blank, the way the
templates scaffold them, is fine.

A field your kit marks **required** (`# required` in the template) is
different: leaving it empty fails the save, and the violation names the file
and the property to fill. This holds even when *every* field is empty — a
document with a class key is always validated, never silently skipped.

## Properties declared without a class

Some kits declare a property that belongs to no one class (`soul:relatedTo`
is one). Those are legal on any class: write them with your document's own
class in the key — `soul.Note.relatedTo` — and the value behaves exactly as
the kit declared it (reference or plain text). They don't appear in class
templates, precisely because they belong to no one class.

## The reference rule (one sentence)

**A reference is an identifier in angle brackets, a repo-relative path, or a
full IRI — the graph never guesses.**

- Frontmatter, property with a **declared class range** (the common
  case): the value is the target's bare **id** — `assignedTo: selkie`.
  The ontology names the class, the id names the Thing, and git-lex
  derives the one IRI. A dangling id is rejected at save — nothing
  guesses.
- Frontmatter, reference property **without** a declared range (`id` and
  `relatedToId` are the everyday examples): the value is the identifier
  notation `<namespace/Class/identifier>` — e.g. `<soul/Journal/day-7>`,
  brackets included, and the namespace comes from the value, never from
  your own kit — or a repo-relative path (`source: friend/selkie.md`), or
  a full IRI. A bare name is rejected with the fix spelled out.
- Body text: a standard markdown link —
  `[day 56](Soul/Journal/2026-07-23-day-56.md)` — becomes a generic
  `linksTo` edge. Targets are repo-root-relative; `.md` is added for you
  when the target has no extension. `[[...]]` is not read anywhere — it
  is plain prose. (The one exception in a soul repo is Claude Code's
  private `Harness/Memory/` notation, which git-lex never touches.)
- Linking to a file that doesn't exist *yet* is fine (create the target
  in the same save and nothing even warns); a link whose target never
  appears warns at every save until fixed.

Also: no `[[...]]` or `@...` syntax inside frontmatter values — ids,
identifiers in brackets, paths, or IRIs only.

<!-- TODO(additive): value resolution rules in full; typed properties
     (dates, integers); example error messages -->
