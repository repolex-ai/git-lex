# Getting Started

*Last updated for git-lex v0.1.1 (2026-08-27)*

This guide will walk you through installing `git-lex`, initializing a new repository, and performing your first workflow.

---

## 1. Installation

You can install `git-lex` directly from the repository source using Cargo:

```bash
cargo install --path . --locked   # Installs from a local clone
```

This installs two primary binaries:
* `git-lex`: The core command-line utility. Since it is prefixed with `git-`, git automatically discovers it, allowing you to invoke it as `git lex`.
* `git-lex-serve`: The server companion for local SPARQL queries and visualization.

> [!NOTE]
> Installing the CLI also registers a man page. You can access help documentation at any time by running `git lex --help` or `man git-lex`.

---

## 2. Initializing Your First Repository

The initialization and lifecycle flow follows four basic commands:

```bash
git lex init --kit soul     # Initialize git-lex with your choice of domain kit
git lex create <type> <id>  # Scaffold a new document (e.g. note, journal)
git lex save "first save"   # Commit changes; extraction + validation run automatically
git lex sync                # Build and update the synced knowledge graph store
```

### Initializing (`init`)
Running `git lex init` configures `git-lex` in the current directory:
1. It downloads the base system kit and your specified domain kit (e.g., `soul`).
2. It generates declarative validation schemas (SHACL shapes).
3. It creates scaffolding directories and class templates for each document type defined in your kits.
4. It installs the pre-commit git hooks that enforce graph validation.
5. If the current directory is not yet a git repository, it offers to run `git init` automatically.

> [!TIP]
> Running `git lex init` is safe to run on existing repositories; it will prompt you before refreshing configuration files, preserving your existing notes and custom settings.

### Scaffolding (`create`)
The `git lex create <type> <id>` command initializes a new markdown document of the specified class. It generates the required YAML frontmatter structure and prints the location of the new file.

### Committing (`save`)
`git lex save "message"` is the unified entrypoint for committing changes. It stages your files, extracts frontmatter attributes into a local triple store, runs validation checks, and commits the result.

### Syncing (`sync`)
`git lex sync` processes your git history, building the offline SPARQL-queryable knowledge graph store.

---

## 3. The Core Concept: Validation at the Gate

The most important concept to understand when using `git-lex` is that **`git lex save` is the only safe write path.** 

Every save action extracts your markdown frontmatter, reconciles it with the graph, and validates it against your kit's rules. If validation fails, the commit is blocked, and no invalid data enters the repository history.

> [!IMPORTANT]
> To preview a commit and check for validation errors without committing changes, run the dry-run flag:
> ```bash
> git lex save --dry-run
> ```

---

## 4. Troubleshooting & Best Practices

### Git Identity Setup
`git-lex` relies on git metadata for authorship and provenance. Ensure your local git configuration is set before running your first save:
```bash
git config --global user.name "Your Name"
git config --global user.email "your.email@domain.com"
```

### Common Validation Failures
* **Missing Identifiers:** If you see a warning about a missing identifier property (such as `soul.Note.noteId`), add the suggested ID key-value pair to your document's frontmatter.
* **Property Collisions:** Repeated keys in a single YAML block are invalid and will cause `save` to reject the commit. Always format multiple values using a standard YAML list block rather than repeating the key.

