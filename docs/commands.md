# Commands

| Command | What it does |
|---|---|
| `git lex init [--kit <kit>]` | Set up git-lex in a repo |
| `git lex create <type> [id]` | Scaffold a new document of a kit type |
| `git lex save ["msg"]` | Stage, validate, extract, commit |
| `git lex sync` | Build/update the knowledge graph from commits |
| `git lex log [<thing>]` | Fact history: every add/remove with commit + author + date |
| `git lex query "SPARQL"` | Query a live view of the working tree |
| `git lex list` | List installed kits and their document types |
| `git lex kit-add / kit-update / kit-remove` | Manage kits |
| `git lex serve viz` | Local web UI |
| `git lex serve sparql` | Standard SPARQL endpoint |
| `git lex nuke` | Remove git-lex from a repo (commits + pushes the removal) |

<!-- TODO(additive): one short page per command with examples; note which
     commands read the synced store vs the live working tree -->
