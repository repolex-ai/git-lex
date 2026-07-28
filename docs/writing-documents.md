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

## Rules that will save you time

- Values that reference other documents are written as bare names
  (`assignedTo: w4r3z`) — not `[[...]]`, not `@...`. Save tells you if you
  get this wrong.
- Wikilinks belong in body text and become graph edges automatically.

<!-- TODO(additive): value resolution rules in full; typed properties
     (dates, integers); what validation checks and example error messages -->
