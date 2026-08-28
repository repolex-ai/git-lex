# Files and Things

*Last updated for git-lex v0.1.1 (2026-08-27)*

`git-lex` tracks two distinct categories of identity. Understanding the distinction between them clarifies where metadata facts reside and how they behave when files are moved or renamed.

---

## 1. The File Plane: Physical Addresses

Every physical file in the repository corresponds to a **File** node in the knowledge graph. Its identifier is its repo-relative workspace path:

```
Soul/Journal/day-7.md  →  git-lex/File/Soul/Journal/day-7.md
```

Because the path serves as the identifier, a File node represents a physical location rather than a persistent concept. If you rename the file, the old address is deleted from the current state, and a new address node is created.

File-plane metadata includes:
* Raw links extracted from your markdown prose (`linksTo` edges)
* Incidental, untyped frontmatter properties
* Git commit metadata (author, timestamps)

You do not need to manually configure File identities; every file in the repository gets one automatically.

---

## 2. The Thing Plane: Semantic Lifetimes

A **Thing** represents the conceptual entity that a document expresses, independent of the file itself. A Thing's identifier (IRI) is compiled from its namespace, class, and a stable identifier, completely decoupled from any file system path:

```
soul/Journal/day-7
```

A Thing is any entity with its own independent identity and lifecycle. Classes designed to hold structured knowledge (such as `Note`, `Journal`, `Being`) are declared as subclasses of `git-lex:Thing` in their respective ontologies. 

When a document has a Thing identity, its structured properties (such as `soul.Journal.earthDate`) bind directly to the Thing node rather than the transient File node.

---

## 3. Why Thing-Plane Metadata Survives File Operations

Because a Thing's IRI contains no path information, renaming or moving its source markdown file cannot alter its identity. 

The link between the two planes is a single derived edge: **`fileId`** ("the File currently expressing this Thing"). `git-lex` asserts this link during the save process and automatically updates it whenever a file is moved:

```
soul/Journal/day-7  --fileId-->  git-lex/File/Soul/Journal/day-7.md
```

If you rename `day-7.md` to `archive-day-7.md`, the old `fileId` edge is retracted, and a new one is asserted. All custom properties bound to `soul/Journal/day-7` remain unchanged. Tracking the history of the `fileId` edge allows you to reconstruct where a conceptual entity has lived over time.

---

## 4. How a Document Resolves Its Thing Identity

To establish a Thing identity for a document, declare its class-specific ID in the YAML frontmatter. This property is always the camel-cased class name appended with `Id` (e.g., `noteId` for a `Note`, `journalId` for a `Journal`):

```yaml
---
soul.Note.noteId: "graph-thoughts"
---
```

When you scaffold a file using `git lex create note graph-thoughts`, this line is generated automatically.

Additionally, documents can carry a fully-qualified, universal `id` field:
```yaml
soul.Note.id: <soul/Note/graph-thoughts>
```
This field explicitly states the Thing URI that the document represents. The class-specific ID property is used as the primary lookup key to anchor and index the document, while the fully-qualified `id` property acts as the formal URI descriptor.

---

## 5. What Happens Without a Thing ID?

If a classed document does not define an ID, no Thing node is minted. Its properties are bound to its File node instead. This means they are tied to its physical path and will not survive file renames. 

When this occurs, the `save` command will emit a warning with the exact line to add:

```
warning: Soul/Note/graph-thoughts.md: this soul.Note document has no id.
Fix: add this line to the YAML block at the top of the file:
soul.Note.noteId: "graph-thoughts"
```

> [!NOTE]
> If the warning indicates that the *class* lacks an identifier property in its ontology, this is a kit-level schema issue. Report it to the kit author. In this case, your facts will still be saved and linked to the File node.

---

## 6. The Nine Universal Properties

Every document class in the `git-lex` ecosystem inherits a set of universal properties from the base `git-lex:Thing` class. When writing these in your frontmatter, always use your document's specific class namespace (e.g., `soul.Note.title`, not `git-lex.Thing.title`):

| Property Key | Type | Description |
|:---|:---|:---|
| `id` | URI / Reference | Which Thing this document represents (e.g., `<soul/Note/graph-thoughts>`). |
| `title` | String | A short, human-readable display name for listings. (Single-valued) |
| `description` | String | A deliberate, human-authored summary designed for software/agent consumption. (Single-valued) |
| `abstract` | String | An automatically generated summary. May be overwritten by background agents. (Single-valued) |
| `cue` | String | Structural triggers specifying *when* an agent should consult this document. (Multi-valued list) |
| `relatedToId` | URI / Reference | A link pointing to another Thing in any namespace (e.g., `<copia/Texture/deep-water>`). (Multi-valued list) |
| `dateCreated` | DateTime | An immutable ISO 8601 timestamp generated on first save. |
| `dateUpdated` | DateTime | An ISO 8601 timestamp rewritten on every save to log the last update. |
| `substrate` | String | The model name or substrate that performed the last save (e.g., `gemini-3.5-flash`). |

### Key Property Behaviors

#### `description` vs. `abstract`
These properties serve separate roles:
* `description` is authored by humans or primary agents as a static, authoritative summary.
* `abstract` is designated as scratch space for machine-derived summaries. Automation tools are permitted to overwrite the `abstract` field, so other systems should treat it as transient and avoid relying on its persistence.

#### Timestamps (`dateCreated` and `dateUpdated`)
Both properties are parsed as XML schema `xsd:dateTime` values representing a precise timestamp rather than a plain calendar date (`YYYY-MM-DD`). 
* You should not edit these values manually.
* `git lex save` automatically stamps `dateUpdated` on every commit, and initializes `dateCreated` if it is the document's first save.
* If a document is moved or migrated from an external location, the `dateCreated` property preserves its original creation timestamp, which would otherwise be lost in git history.

#### Cross-Namespace References
The `<namespace/Class/identifier>` form in `id` and `relatedToId` allows documents to reference objects defined in entirely different kits without requiring compile-time coordination. The namespace resolution is driven dynamically by the referenced value.

