# Vocabulary Kits

*Last updated for git-lex v0.1.1 (2026-08-27)*

A kit is a vocabulary package. It defines your workspace's document types (classes), metadata properties, and graph validation constraints. 

Under the hood, these are defined using a standard Web Ontology Language (OWL) ontology along with SHACL (Shapes Constraint Language) shapes. 

---

## 1. Kit Hierarchy

`git-lex` supports three tiers of vocabulary scope:
1. **Base Kit (`git-lex-kit-base`):** Declares system-wide schemas, File/Thing duality properties, and baseline graph validation. This is always installed automatically.
2. **Domain Kit (e.g., `soul`):** Defines the primary focus of your workspace (e.g., Journal, Note, Task document templates). Exactly one domain kit is active and is set during repo setup.
3. **Optional Kits:** Layered vocabularies added to extend the workspace for specific workflows.

---

## 2. Command Reference

```bash
git lex init --kit soul      # Install a domain kit during repository setup
git lex kit-add <kit>        # Layer an optional kit onto the repository
git lex kit-update           # Refresh all installed kits to their latest versions
git lex kit-update <kit>     # Fetch updates for a single kit (rebuilds shapes for all)
git lex kit-remove <kit>     # Remove an optional kit from the repository
```

* **`kit-add`** works only for kits declared optional (`scope: optional` in the kit's `kit.yml` configuration). It registers the kit in `.lex/repo.yml`, scaffolds class folders, and deploys document templates.
* **`kit-remove`** prompts you before deleting class content folders to prevent data loss. You can bypass the verification prompt with `--force`.

---

## 3. How Kits Land in Your Workspace

When you install or update a kit, `git-lex` mirrors the vocabulary layout under the hidden `.lex/` folder:

```
.lex/
├── repo.yml                  # Tracks list of active kits
└── ontology/
    ├── git-lex/              # System base kit ontology
    └── <kit-name>/
        ├── <kit-name>.ttl    # Pinned ontology Turtle file
        └── <kit-name>-shapes.ttl # Compiled SHACL validation shapes
```

During `git lex kit-update`, `git-lex` synchronizes this folder. It deletes orphaned ontologies, mirrors the latest fetched Turtle specifications, and recompiles the corresponding SHACL shapes used to validate your commits. This ensures that the local repository always validates against the latest shared standards.

> [!TIP]
> Because kits share standardized ontologies, repositories utilizing the same kit speak the exact same metadata language. This allows you to run distributed SPARQL queries across different repositories out-of-the-box.

---

## 4. Custom Kit Development

If you want to build your own vocabulary kit rather than consuming an existing one, see the [Kit Development Guide](../kit-development/kit-authoring.md) for details on ontology design, naming conventions, and hooks.

