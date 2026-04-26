# git-lex

Git extensions for knowledge graphs. `git-lex` installs as a git subcommand (`git lex ...`) and turns any git repo into a SHACL-validated, SPARQL-queryable knowledge graph.

## Install

### From a release binary

Download the binary for your platform from the [Releases page](https://github.com/repolex-ai/git-lex/releases), put it on your `PATH`, and make it executable:

```bash
# macOS / Linux
chmod +x git-lex
mv git-lex /usr/local/bin/
```

Verify:

```bash
git lex --help
```

### From source (cargo)

You'll need a Rust toolchain. The easiest path is [rustup](https://rustup.rs/):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Then:

```bash
git clone https://github.com/repolex-ai/git-lex
cd git-lex
cargo install --path . --locked
```

This installs both `git-lex` and `git-lex-serve` to `~/.cargo/bin/`.

## Quick start

```bash
mkdir my-kg && cd my-kg && git init
git lex init --kit soul        # initialize with the soul kit
git lex create Memory          # create a typed document
git lex save "first memory"    # add + commit + extract + validate
git lex query "SELECT ..."     # SPARQL the graph
```

## Commands

| Command | What it does |
|---|---|
| `git lex init [--kit <name>]` | Initialize `.lex/` in the current repo |
| `git lex create <type> <id>` | Create a new document from the kit ontology |
| `git lex save "msg"` | Stage + commit + extract frontmatter + validate |
| `git lex sync` | Rebuild the SPARQL store from `.spo` sidecars |
| `git lex query "SPARQL"` | Run a SPARQL query against the graph |
| `git lex list` | List all document classes from installed shapes |
| `git lex status` | Show extraction status |
| `git lex kit-update` | Re-download and reinstall the kit |
| `git lex serve` | Start the local viz server |
| `git lex nuke` | Remove `.lex/` (content + git history preserved) |

Run `git lex help <subcommand>` for full options.

## Architecture

- **Content** lives in the repo as normal markdown. Frontmatter uses dot notation: `kit.class.property: value`.
- **`.lex/`** is a git-tracked index — extraction sidecars (`.spo`), kit definitions, generated SHACL shapes.
- **`.git/lex/`** holds derived data (oxigraph store) — never tracked, rebuildable from sidecars.
- **Kits** are installed from GitHub (`repolex-ai/git-lex-kit-{name}`) and define the ontology, scaffold files, and harness adapters for a use-case.

## License

[Unlicense](LICENSE) — public domain.
