# Writing documents

*Last updated for git-lex v0.1.0 (2026-08-08)*

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
longer land quietly. Before v0.1.1 it did land quietly: 28 documents in one
soul repo kept only their last value, and the graph read as though the rest
had never been written.

Whether a given property *may* hold more than one value is a question for
your kit's ontology — the list form is how you write it when it does.

## The reference rule (one sentence)

**A reference is the document's repo-relative path (or a full IRI) — the
graph never guesses.**

- Frontmatter, property with a **declared class range** (the common
  case): the value is the target's bare **id** — `assignedTo: selkie`.
  The ontology names the class, the id names the Thing, and git-lex
  derives the one IRI. A dangling id is rejected at save — nothing
  guesses.
- Frontmatter, reference property **without** a declared range: the value
  is the document's repo-relative path (`source: friend/selkie.md`) or a
  full IRI.
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
paths, or IRIs only.

<!-- TODO(additive): value resolution rules in full; typed properties
     (dates, integers); what validation checks and example error messages -->
