# git-lex: agent manual

git-lex turns this git repo into a knowledge graph. You write markdown files
with a YAML header; git-lex reads the headers and links, checks them against
the installed kits' ontology, and makes everything queryable with SPARQL.
This file is generated. `git lex --skill` prints it fresh.

## The loop

```bash
git lex list                       # every document class you can create
git lex create <class> <id>        # new document; the id becomes the filename
git lex save "what changed — you"  # validate, extract, commit. Use this, not git commit
git lex query "<sparql>"           # ask your soul's graph, history included
```

Add `--json` to `list`, `create` or `query` for structured output.

## Writing a document

- Start from `git lex create`. It writes the header with the right keys.
- A header key is `<kit>.<Class>.<property>`, for example `soul.Journal.title`.
  Inherited properties use the same form: `soul.Journal.title` is `git-lex:title`.
- A reference to another document is written `<kit/Class/id>` in angle
  brackets, for example `<soul/Journal/day-7>`.
- Several values: one key, a YAML list. Never repeat a key.
- In the body, link with root-relative markdown links: `[text](/Soul/Note/x.md)`.
  Links become `md:linksTo` edges in the graph.
- Only use keys listed in the ontology section below. An undeclared key saves
  with a warning and no query will find it.
- `id`, `createdDate`, `updatedDate` and `substrate` are maintained for you.

## When save refuses

Save prints each blocking file and what is wrong (a missing required field, a
wrong value type, a duplicate id). Fix the file and save again. Nothing is
committed until the whole save passes.

## Querying

- Prefixes for every installed kit are added for you. Do not declare them.
- A document is `?d a <kit>:<Class>`. Its properties are the IRIs in the
  ontology section, for example `git-lex:title`.
- `git lex query <name>` runs a stored query from `.lex/query/<name>.md`.
- `git lex query` answers from your soul's store through gitlexd: current
  documents, files, commits, and the history of every statement ("when did
  this change?"). It is tied to the soul your session started in, and
  starts gitlexd itself if none is running.
- `git lex direct "<sparql>"` builds a view of the working tree in memory
  instead — unsaved edits included, no history, no gitlexd needed.
- What else is in the graph, what addresses look like, and queries that run:
  the "Querying" section after this manual, written for this repo.

## Where things live

- Your documents: the class folders named in the ontology section, plus any
  markdown file in the repo.
- `.lex/` belongs to git-lex. Never edit or delete anything in it by hand.
  `.lex/repo.yml` lists the installed kits; `.lex/ontology/` holds their
  vocabularies; `.lex/query/` is the one place for your own stored queries.
- `git lex kit-update` refreshes kits and repairs generated files.
  `git lex verify` health-checks the graph.
