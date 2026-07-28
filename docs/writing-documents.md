# Writing documents

Documents are plain markdown with YAML frontmatter in dot notation:

```yaml
---
soul.Journal.soulDay: 56
soul.Journal.earthDate: 2026-07-23
---
Body text. Link other documents with [[wikilinks]].
```

The pattern is `kit.Class.property`. Class names are case-sensitive and come
from your kit — check the `__ClassName.md` template files in each folder for
every valid property.

## The reference rule (one sentence)

**A reference is the document's repo-relative path (or a full IRI) — the
graph never guesses.**

- Frontmatter: `assignedTo: friend/selkie.md` — the relationship comes
  from the ontology (whatever the property means in your kit).
- Body text: `[[Journal/2026-07-23-day-56]]` — becomes a generic
  `linksTo` edge. Targets are paths too: relative to the file's own
  folder, or from the repo root with a leading `/`. `.md` is added for
  you when the target has no extension.
- Bare names (`assignedTo: selkie`, `[[selkie]]`) are **errors** — save
  lists every offender with its file and the fix. Guessing which file a
  name means is how links silently rebind; git-lex refuses to.
- Linking to a file that doesn't exist *yet* is fine (create the target
  in the same save and nothing even warns); a link whose target never
  appears warns at every save until fixed.

Also: no `[[...]]` or `@...` syntax inside frontmatter values — paths
only.

<!-- TODO(additive): value resolution rules in full; typed properties
     (dates, integers); what validation checks and example error messages -->
