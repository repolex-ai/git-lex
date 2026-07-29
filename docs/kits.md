# Kits

*Last updated for git-lex v0.1.0 (2026-07-29)*

A kit is a vocabulary pack: it defines your document types (classes), their
properties, and the validation rules — as a real OWL ontology with SHACL
shapes derived from it. The base kit (git-lex's own vocabulary) is always
installed; a domain kit (like `soul`) defines what YOU write.

```bash
git lex init --kit soul      # install at setup
git lex kit-add <kit>        # add another later
git lex kit-update           # refresh to the kit's latest version
```

Kits are the contract that makes repos queryable *together*: two repos on the
same kit answer the same SPARQL.

<!-- TODO(additive): authoring your own kit; the adaptive `_ontology/`
     mechanism; how kit files land in .lex/ -->
