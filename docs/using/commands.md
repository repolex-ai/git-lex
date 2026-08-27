# Commands

*Last updated for git-lex v0.1.0 (2026-08-12)*

`git lex --help` gives the full reference (the binary maintains its own man
page), and every subcommand answers `--help` too.

| Command | What it does |
|---|---|
| `git lex init [<dir>] [--kit <kit>]` | Set up git-lex in a repo (offers `git init` if needed; base kit always installed) |
| `git lex create <type> [id] [--json]` | Scaffold a new document of a kit type |
| `git lex save ["msg"] [--dry-run]` | Stage, validate, extract, commit; `--dry-run` runs every gate, commits nothing |
| `git lex sync` | Build/update the synced knowledge graph store from commits |
| `git lex query "SPARQL"\|<name> [--json]` | Query a fresh view of the working tree (does NOT read the synced store); a bare name runs the saved query `.lex/query/<name>.md` |
| `git lex list [--json]` | List every document class the installed kits define |
| `git lex kit-add <kit>` | Add an optional kit |
| `git lex kit-update [<kit>]` | Refresh kits (no argument = all installed kits) |
| `git lex kit-remove <kit> [--force]` | Remove an optional kit (asks before deleting content folders) |
| `git lex serve viz [--port]` | Local web UI over the synced store (default port 7878) |
| `git lex serve sparql [--port]` | W3C SPARQL endpoint over the synced store (default port 7880) |
| `git lex verify` | Health-check the synced store (temporary; removed after the v1 rollout) |
| `git lex nuke` | Remove git-lex from a repo (commits + pushes the removal) |

The `--json` flags emit machine-readable output on stdout (SPARQL 1.1 JSON
Results for `query`; JSON summaries for `create` and `list`).

`git lex query` takes either SPARQL text or the name of a saved query kept in
`.lex/query/` — see [Querying](queries.md#saved-queries).

There is also a hidden `git lex hook` subcommand — it is the entrypoint git's
pre-commit hook calls, not for direct use.

<!-- TODO(additive): one short page per command with examples; note which
     commands read the synced store vs the live working tree -->
