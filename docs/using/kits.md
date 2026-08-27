# Kits

*Last updated for git-lex v0.1.0 (2026-08-12)*

A kit is a vocabulary pack: it defines your document types (classes), their
properties, and the validation rules — as a real OWL ontology with SHACL
shapes derived from it. The base kit (git-lex's own vocabulary) is always
installed; a domain kit (like `soul`) defines what YOU write; optional kits
can be layered on top later.

```bash
git lex init --kit soul      # install your domain kit at setup
git lex kit-add <kit>        # add an optional kit later
git lex kit-update           # refresh ALL installed kits to their latest
git lex kit-update <kit>     # fetch just one kit (artifacts still rebuild for all)
git lex kit-remove <kit>     # remove an optional kit
```

`kit-add` only accepts kits marked optional (`scope: optional` in the kit's
`kit.yml`) — a domain kit is installed with `init --kit` instead. It creates
the kit's folders and class templates so the new types show up in `ls`, and
records the kit in `.lex/repo.yml`.

`kit-remove` asks before deleting the kit's content folders — those hold your
documents. `--force` skips the prompt.

Kits are the contract that makes repos queryable *together*: two repos on the
same kit answer the same SPARQL.

Building a kit of your own is a different job from using one — it has its own
section: [Kit development](../kit-development/kit-authoring.md).

<!-- TODO(additive): the adaptive `_ontology/` mechanism; how kit files land
     in .lex/ -->
