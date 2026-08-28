# Moving, Renaming, and Deleting Documents

*Last updated for git-lex v0.1.1 (2026-08-27)*

This document outlines how `git-lex` handles file moves, renames, and deletions in your workspace while maintaining graph integrity.

---

## 1. Identity vs. Filename

> [!IMPORTANT]
> **The filename is not the identity.** 
> A document's semantic identity is defined solely by its ID field in the YAML frontmatter (e.g., `soul.Note.noteId: "field-notes"`). 
> While the filename typically matches this ID by default (since `git lex create <type> <id>` scaffolds both from the same argument), they are independent:
> * Renaming a file does not change the identity of the Thing it expresses.
> * Modifying the ID field in the frontmatter creates a new semantic entity, even if the file itself remains in the same folder.

---

## 2. Two Types of Links, Two Resolution Rules

`git-lex` distinguishes between physical path references and semantic concept references:

1. **Markdown Links (`[text](/Soul/Pursuit/x.md)`):** These links point at physical paths in the File Plane. If the target file is renamed or deleted, the link registers as unresolved but remains visible in the graph.
2. **Frontmatter References (`relatedToId` / `id`):** These point to stable Things in the Thing Plane. They resolve based on identity and are completely unaffected by file relocations.

---

## 3. Best Practices for Document Lifecycle

### Renaming and Moving Files
When you rename or move a document file, `git lex save` automatically detects the operation through git's rename-tracking subsystem:
* **Link Healing:** `git-lex` automatically rewrites all inbound markdown links pointing to the old path, converting them to canonical root-relative links pointing to the new path.
* **Sidecar Migration:** The cached metadata extract (sidecar file) is relocated to match the new file path instead of being regenerated from scratch, keeping history intact.

> [!WARNING]
> * **Inbound Only:** Only links *pointing at* the moved file are rewritten. A relocated file's own relative outbound links (e.g., `../Note/x.md`) are not rewritten (though root-relative links will survive any relocation untouched).
> * **Frontmatter Immunity:** Frontmatter reference properties are never modified during file moves because semantic identifiers do not follow physical filenames.

### Changing a Semantic Identity
To change the actual identifier of a concept, edit its ID field in the frontmatter and run `git lex save`. In a single commit, the old identity's assertions are retracted from the graph's active state, and the new identity's facts are asserted.

### Deletions
When a document is deleted:
* Its corresponding metadata extract under `.lex/extract/` is cleaned up during the next `git lex save` invocation.
* The facts associated with the deleted entity are retracted from the active graph.

> [!CAUTION]
> **Never modify the `.lex/` directory manually.**
> Let `git lex save` handle all file reconciliations. Deleting files inside `.lex/extract/` manually will drop those documents from the graph, bypassing proper git history extraction.

---

## 4. Historical Retention

When an identity changes or a document is deleted, its facts are **retracted**, not permanently deleted. 

Because `git-lex` builds its store directly from git commits, the graph retains a complete audit trail of every assertion and retraction tied to the specific commit and author that introduced it. You can query the historical state of the graph at any commit using `git lex serve sparql`.

