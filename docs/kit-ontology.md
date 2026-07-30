# Kit Ontology Design

*Last updated for git-lex v0.1.0 (2026-07-30)*

This guide is for kit builders defining **document types** — the vocabulary a
kit gives its users. It covers where the ontology file lives, and the four
building blocks that cover almost every kit: a class, its id, an enum, and a
reference to another Thing. That's the whole basic toolkit. (An advanced guide
— restrictions, subclassing, multi-kit layering — comes later.)

The best worked example is the soul kit's ontology:
[`git-lex-kit-soul/ontology/soul/soul.ttl`](https://repolex.ai/ontology/soul/soul.ttl).
Everything below is the pattern that file follows.

---

## 1. Where the ontology file goes

```
<kit-repo>/ontology/<name>/<name>.ttl
```

`<name>` is your kit's short name — the same word as `name:` in `kit.yml`. It
becomes three things at once: the folder name, the namespace prefix, and the
last segment of the namespace IRI:

```turtle
@prefix garden: <https://repolex.ai/ontology/garden/> .
```

If your kit belongs to an application that has its own repo (like copia), the
**application repo's** `ontology/<name>/` directory is the source of truth,
and publishing copies it into the kit. If the kit is the whole product (like
soul), the kit repo is the edit point. Either way there is exactly one place
you edit.

On `git lex kit-update`, the ontology installs to `.lex/ontology/<name>/` in
every repo that has the kit.

## 2. The skeleton

Every ontology file starts the same way — prefixes, then a header naming the
ontology itself:

```turtle
@prefix git-lex: <https://repolex.ai/ontology/git-lex/> .
@prefix garden:  <https://repolex.ai/ontology/garden/> .
@prefix owl:     <http://www.w3.org/2002/07/owl#> .
@prefix rdfs:    <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd:     <http://www.w3.org/2001/XMLSchema#> .

<https://repolex.ai/ontology/garden> a owl:Ontology ;
    rdfs:label "Garden Kit Ontology" ;
    rdfs:comment "What this kit is for, in one sentence." ;
    owl:versionInfo "0.1.0" .
```

Bump `owl:versionInfo` when you change the file. SHACL validation shapes are
generated from this file — you never write shapes by hand.

## 3. Making a class

A class is a document type. Declaring one:

```turtle
garden:Plant a owl:Class ;
    git-lex:foldered true ;
    rdfs:label "Plant" ;
    rdfs:comment "A plant under care: what it is, where it lives, how it's doing." .
```

Two rules:

- **`git-lex:foldered true`** means this class is *authored* — agents write
  these documents by hand. It gets a `/Garden/Plant/` folder, a template, and
  a `git lex create plant` command. A class without the flag is
  vocabulary-only: it exists in the graph but no folder is scaffolded.
- **The `rdfs:comment` is mandatory in spirit**: one line saying what this is
  and who writes it. A class nobody can explain in one line is a class that
  shouldn't ship.

## 4. The id property (required on every class)

Every class declares an id property, named `<class>Id`:

```turtle
garden:plantId a owl:DatatypeProperty ;
    rdfs:label "plantId" ;
    rdfs:comment "Unique identifier for this plant." ;
    rdfs:domain garden:Plant ;
    rdfs:range xsd:string .
```

- ids are human-readable, lowercase, hyphenated strings the author picks
  (`"back-porch-fern"`), unique **within the class** — enforced at save.
- The id is what gives the Thing its address in the graph:
  `https://repolex.ai/garden/Plant/back-porch-fern`. Files can move; the id
  (and therefore the address) doesn't.

## 5. Simple properties

A property is declared once, with the class it belongs to (`rdfs:domain`) and
the kind of value it holds (`rdfs:range`):

```turtle
garden:wateredOn a owl:DatatypeProperty ;
    rdfs:label "watered on" ;
    rdfs:comment "The date this plant was last watered." ;
    rdfs:domain garden:Plant ;
    rdfs:range xsd:date .
```

Common ranges: `xsd:string`, `xsd:integer`, `xsd:date`, `xsd:dateTime`,
`xsd:boolean`, `xsd:anyURI`.

## 6. Enums (a fixed set of allowed values)

When a property should only accept certain values, declare the value set as a
datatype, then point the property's range at it:

```turtle
garden:HealthValue a rdfs:Datatype ;
    owl:oneOf ("thriving" "okay" "struggling") .

garden:health a owl:DatatypeProperty ;
    rdfs:label "health" ;
    rdfs:comment "How the plant is doing: 'thriving', 'okay', 'struggling'." ;
    rdfs:domain garden:Plant ;
    rdfs:range garden:HealthValue .
```

Validation rejects any other value at save time, with a clear message — that's
the enum doing its job.

## 7. Pointing at another Thing

A reference property holds the **target's id**, and its name says so — end the
name with `Id`:

```turtle
garden:livesInBedId a owl:ObjectProperty ;
    rdfs:label "lives in bed" ;
    rdfs:comment "The Bed this plant is planted in. The value is the bed's bedId." ;
    rdfs:domain garden:Plant ;
    rdfs:range garden:Bed .
```

The naming rule, stack-wide:

> **A reference property's name ends with what its value is** — `...Id` for a
> Thing's id, `...Path` for a repo-relative file path, `...Url` for an
> external address. `<class>Id` = who I am; any other `*Id` field = who I
> point at.

The point is that someone typing frontmatter into a markdown file can tell
from the field name alone what kind of value belongs there, without opening
the ontology. A bare string field that secretly holds a reference is the one
pattern that's outlawed — if it points at something, declare it so the graph
and the validator can follow it.

## 8. What it looks like from the user's side

The ontology above gives an agent this authoring experience:

```markdown
---
garden.Plant.plantId: "back-porch-fern"
garden.Plant.health: "thriving"
garden.Plant.wateredOn: 2026-07-30
garden.Plant.livesInBedId: "shade-bed"
---

# The back-porch fern

Prose body — anything you want. The frontmatter is the structured part;
the body is yours.
```

Frontmatter keys are `<kit>.<Class>.<property>`. Everything declared validates
at save; a key the ontology doesn't declare still works as a plain
frontmatter key, but joins nothing.

## 9. Naming rules (short, absolute)

1. **The name is the definition.** If the name needs a paragraph to explain,
   it's the wrong name.
2. **No generic words** — `kind`, `type`, `data`, `status` alone say nothing.
   Scope them (`health`, not `status`) or don't ship them.
3. **Value-type suffixes on references**: `...Id` / `...Path` / `...Url`.
   Plain literal fields get no suffix.
4. **Every class and property earns its place.** Add a field when a real
   document needs it — never because it "feels right." Unused fields don't
   stay neutral; they rot into confusion.

## 10. Publishing

When the ontology changes: commit in the source repo, then run the publish
flow (`subtexture/tools/ontology-publish` from the source repo root). It
copies to the kit repo, pushes both, and updates the live copy at
`https://repolex.ai/ontology/<name>/`. Repos pick the change up on their next
`git lex kit-update`.
