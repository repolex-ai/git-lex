# Getting Started

This guide will walk you through installing `git-lex`, initializing a new repository, and performing your first workflow.

---

## Installation

You can install `git-lex` using Cargo:

```bash
cargo install --git https://github.com/repolex-ai/git-lex --locked
```

Or from a local clone:

```bash
cargo install --path . --locked
```

This installs two binaries:
* `git-lex`: The core command-line utility. Since it is prefixed with `git-`, Git automatically discovers it, allowing you to invoke it as `git lex`.
* `gitlexd`: The local service that keeps every git-lex repository's graph synced and answers `git lex query`. See [gitlexd](gitlexd.md).

> [!NOTE]
> Installing the CLI also registers a man page. You can access help documentation at any time by running `git lex --help` or `man git-lex`.

---

## Initializing Your First Repository

The initialization and lifecycle flow follows basic commands:

```bash
git lex init --kit soul          # Initialize git-lex with your choice of domain kit
git lex create note "my-note"    # Scaffold a new document (e.g. note, journal)
git lex save "first save"        # Commit changes; validation + extraction run automatically
git lex sync                     # Build and update the synced knowledge graph store
git lex query "SELECT * WHERE { ?s ?p ?o } LIMIT 10"   # Query via gitlexd (auto-starts if needed)
git lex direct "SELECT * WHERE { ?s ?p ?o } LIMIT 10"  # Query working tree directly in memory
```

### Initializing (`init`)
Running `git lex init` configures `git-lex` in the current directory:
1. It downloads the base system kit and your specified domain kit (e.g., `soul`).
2. It generates declarative validation schemas (SHACL shapes).
3. It creates scaffolding directories and class templates for each document type defined in your kits.
4. It installs the pre-commit Git hooks that enforce graph validation.
5. If the current directory is not yet a Git repository, it offers to run `git init` automatically.

> [!NOTE]
> **Only `git lex init` creates `.lex/`.** Subcommands such as `sync`, `verify`, and `query` will refuse to run in a repository without `.lex/repo.yml`. Running `git lex init` is safe on existing repositories; it prompts before refreshing configuration files, preserving notes and custom settings.

### Scaffolding (`create`)
The `git lex create <type> <id>` command initializes a new Markdown document of the specified class. It generates the required YAML frontmatter structure and prints the location of the new file. Use `--list` to see all creatable classes.

### Committing (`save`)
`git lex save "message"` is the unified entrypoint for committing changes. It stages your files, updates timestamps, extracts frontmatter attributes into `.lex/extract/` sidecars, runs SHACL validation checks, commits the result, and nudges `gitlexd` when it is running. Use `--no-restamp` to preserve `updatedDate` during bulk sweeps or mechanical migrations.

### Syncing (`sync`)
`git lex sync` processes your Git history, compiling committed facts and events into the persistent Oxigraph store. When `gitlexd` is running, `sync` hands the work to the daemon and waits for completion. Every sync reports a per-phase elapsed timing breakdown.

### Querying (`query` and `direct`)
* **`git lex query "<sparql>"`**: Queries your repository's persistent store through `gitlexd`. Includes statement history, full commits, and document facts. Starts `gitlexd` automatically if none is running.
* **`git lex direct "<sparql>"`**: Queries an ephemeral, in-memory graph of your current working tree (uncommitted edits included). Requires no running daemon, but has no statement history.

---

## Validation at the Gate

The most important concept to understand when using `git-lex` is that **`git lex save` is the only safe write path.** 

Every save action extracts your Markdown frontmatter, reconciles it with the graph, and validates it against your kit's rules. If validation fails, the commit is blocked, and no invalid data enters the repository history.

> [!IMPORTANT]
> To preview a commit and check for validation errors without committing changes, run the dry-run flag:
> ```bash
> git lex save --dry-run
> ```

---

## Troubleshooting & Best Practices

### Git Identity Setup
`git-lex` relies on Git metadata for authorship and provenance. Ensure your local Git configuration is set before running your first save:
```bash
git config --global user.name "Your Name"
git config --global user.email "your.email@domain.com"
```

### Common Validation Failures
* **Missing Identifiers:** If you see a warning about a missing identifier property (such as `soul.Note.noteId`), add the suggested ID key-value pair to your document's frontmatter.
* **Property Collisions:** Repeated keys in a single YAML block are invalid and will cause `save` to reject the commit. Always format multiple values using a standard YAML list block rather than repeating the key.
