# Commands

`git lex --help` gives the full reference (the binary maintains its own man
page), and every subcommand answers `--help` too.

| Command | What it does |
|---|---|
| `git lex init [<dir>] [--kit <kit>]` | Set up git-lex in a repository (offers Git initialization if needed; base kit always installed). Only `init` creates `.lex/` — `sync`, `verify`, and `query` refuse in a repository without `.lex/repo.yml`. |
| `git lex create <type> [id] [--list] [--json]` | Scaffold a new document of a kit type; `--list` lists creatable classes. |
| `git lex save ["msg"] [--dry-run] [--no-restamp]` | Stage, validate, extract, commit; `--dry-run` runs every gate, commits nothing; `--no-restamp` preserves `updatedDate` on existing documents during mechanical sweeps. Nudges `gitlexd` after committing. |
| `git lex sync` | Compile committed history into the persistent store (Oxigraph). Handed to `gitlexd` when running (waits for result); prints a per-phase elapsed timing breakdown. Refuses without `.lex/repo.yml`. |
| `git lex export-spine` | Write the semantic index as one TSV spine for LLM context caches (`.lex/_ignore/spine/<synced-commit>.spine.tsv`). |
| `git lex --skill` | Print the agent manual plus this repo's compact ontology (every class and property of the installed kits). The same text is kept current in `.lex/COMPACT-ONTOLOGY.md`. |
| `git lex query "SPARQL"\|<name> [--json]` | Ask your soul's graph through `gitlexd` — current state and full history; a bare name runs the saved query `.lex/query/<name>.md`. Auto-starts `gitlexd` if not running; refuses without `.lex/repo.yml`. |
| `git lex direct "SPARQL"\|<name> [--json]` | Query a fresh in-memory view of the working tree (unsaved edits included, no history, no `gitlexd` needed). Follows the shell's current directory. |
| `git lex list [--json]` | List every document class the installed kits define. |
| `git lex kit-add <kit>` | Add an optional kit (`scope: optional` in `kit.yml`). |
| `git lex kit-update [<kit>]` | Refresh kits (no argument = all installed kits). Re-fetches, converges files, reconciles hooks, mirrors ontologies, and regenerates SHACL shapes and templates. |
| `git lex kit-remove <kit> [--force]` | Remove an optional kit (prompts before deleting content folders unless `--force`). |
| `git lex verify` | Health-check the synced store (vocabulary declared, history well-formed, current state matches history). Refuses without `.lex/repo.yml`. |
| `git lex nuke` | Remove git-lex from a repository: tells `gitlexd` to drop the soul, removes `.lex/`, cleans every git-lex line from `.gitignore`, sweeps leftovers, commits and pushes the removal. Preserves content files and Git history. |
| `git lex soul session [--json]` | Inspect active session attestation, genesis SHA, verified substrate, and session hash. |
| `git lex soul voice [<msg>] [--list]` | Attach or read sovereign voice reflections on the commit tree via `git notes` (`refs/notes/soul/voice`). |

The `--json` flags emit machine-readable output on stdout (SPARQL 1.1 JSON
Results for `query` and `direct`; JSON summaries for `create`, `list`, and `soul session`).

`git lex query` and `git lex direct` take either SPARQL text or the name of a
saved query kept in `.lex/query/` — see [Querying](queries.md#saved-queries).

## gitlexd

`gitlexd` is the local service that holds each repository's store, syncs on commits, and answers SPARQL over HTTP on port 7880.

| Command | What it does |
|---|---|
| `gitlexd` | Run in the foreground; exits immediately if one is already running. |
| `gitlexd start` | Stop any other gitlexd on this machine, then run in this terminal. |
| `gitlexd restart` | Stop any other gitlexd on this machine, then run in this terminal. |
| `gitlexd stop` | Stop every gitlexd on this machine and exit. |
| `gitlexd status` | Report whether one is running, and list each soul it holds with its synced-to commit. |

`git lex query` starts gitlexd itself when none is running; there is never
more than one (the port is the lock). Repositories initialized after `gitlexd` starts join dynamically on their first request without requiring a restart. See [gitlexd](gitlexd.md).

There is also an internal `git lex hook` subcommand — it is the entrypoint Git's
pre-commit hook calls, not intended for direct use.
