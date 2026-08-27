# Kit Ontology Design

*Current for kit-base 0.14.0 (2026-08-27). Only the 0.11 → 0.14 delta was checked
against this page; the rest is not re-audited at this version.*

*This number is hand-typed, and it read 0.11.0 for three minor versions with
nothing anywhere objecting. To check it, read `owl:versionInfo` at the top of your
own installed `.lex/kit/repolex-ai/git-lex-kit-base/ontology/git-lex/git-lex.ttl`
— your own install, not a shared clone of the kit repo, which may sit on someone
else's branch. A higher number there means this page is behind the ontology.*

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

### Telling authors what goes in the body

`rdfs:comment` describes every *field*. To say what belongs in the document's
**body**, add `git-lex:authoringGuidance`:

```turtle
garden:Plant a owl:Class ;
    git-lex:foldered true ;
    rdfs:label "Plant" ;
    rdfs:comment "A plant under care: what it is, where it lives, how it's doing." ;
    git-lex:authoringGuidance """
## Condition
What it looks like right now. Observations, not plans.

## History
What you've done to it and when.
""" .
```

This is delivered two ways: `git lex create` shows it, and it is written into
the class's `__Plant.md` template for anyone writing files without calling
`create`. **It does not land in the document**, so nobody has to delete it.

**It is never enforced** — no gate, no warning, nothing in `verify`. A document
that ignores its guidance is a perfectly valid document. The moment guidance can
fail something, it stops being help and becomes one more gate to satisfy.

On length: let the class decide. A freeform class gets a sentence; a class with
real required structure gets the headings with one line under each. If you're
writing paragraphs you're writing a manual, and a manual belongs somewhere this
can point at.

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

When the constraint is a bound rather than a list, declare the base type plus
its bounds the same way:

```turtle
garden:RatingValue a rdfs:Datatype ;
    owl:onDatatype xsd:integer ;
    owl:withRestrictions ( [ xsd:minInclusive 1 ] [ xsd:maxInclusive 5 ] ) .
```

The generated shape carries both the base type and the bounds. The facets
git-lex translates: `xsd:minInclusive`, `xsd:maxInclusive`, `xsd:minExclusive`,
`xsd:maxExclusive`, `xsd:minLength`, `xsd:maxLength`, `xsd:pattern`. Any other
facet prints a warning that the bound is **not** enforced — declared bounds are
never silently dropped.

## 7. Pointing at another Thing

A reference property holds the **target's id**, and its name says so — end the
name with `Id`.

**Before you write one, pick your lane. The `rdfs:range` you declare decides
what the author is allowed to type**, and the two forms are not
interchangeable.

### Lane A — pointing anywhere: `rdfs:range git-lex:Thing`

```turtle
garden:relatedToId a owl:ObjectProperty ;
    rdfs:comment "Anything related to this plant, written <namespace/Class/id>." ;
    rdfs:domain garden:Plant ;
    rdfs:range git-lex:Thing .
```
```yaml
garden.Plant.relatedToId:
  - <garden/Bed/south-wall>
  - <soul/Note/why-mulch>
```

Values are full addresses in angle brackets. This is the lane that works
**across kits** with no coordination, and it is the one to use when in doubt.
Anything that isn't a bracketed address is rejected at save, with the fix named.

### Lane B — pointing at one known class: `rdfs:range <that class>`

```turtle
garden:livesInBedId a owl:ObjectProperty ;
    rdfs:label "lives in bed" ;
    rdfs:comment "The Bed this plant is planted in. The value is the bed's bedId." ;
    rdfs:domain garden:Plant ;
    rdfs:range garden:Bed .
```
```yaml
garden.Plant.livesInBedId: south-wall
```

The value is a **bare id**, and the class comes from the declaration.

### The mistake this causes, and what it looks like

The two lanes are chosen by **exact match on the range IRI** — no subclass
walk, and your property's name is never consulted. So if you declare the
precise class but the author writes the address form, **nothing errors.** The
brackets are treated as part of the id:

```yaml
# declared: rdfs:range garden:Bed   (Lane B — expects a bare id)
garden.Plant.livesInBedId: <garden/Bed/south-wall>
```

    you meant   →   https://repolex.ai/garden/Bed/south-wall
    you got     →   https://repolex.ai/garden/Bed/%3Cgarden/Bed/south-wall%3E

No warning. The reference simply points at an address nothing describes, and
every query that joins on it silently returns nothing.

**So: naming the exact class feels more correct and is the riskier choice** — not
because Lane B is wrong, but because choosing it commits your authors to the bare
form, and a bracketed value typed into it fails silently instead of loudly.

Which lane to pick:

- **Use Lane A (`git-lex:Thing`) when the target could be anything** — any class,
  any kit. `relatedToId` itself is the type case. Values are bracketed addresses.
- **Use Lane B (a concrete class) when the target is always one known class**, and
  you want a dangling reference to fail **at save**. The declared range is what
  makes that check possible: git-lex derives the full IRI from the range plus the
  bare id, so a target that doesn't exist is caught rather than stored.

**Anything named `relatedTo…Id` uses Lane A** — including the typed forms like
`relatedToPursuitId` or `relatedToPlaceId`. The class named in the property is
the author's signal about what to point at; it is not carried by the range. So a
typed reference property still declares `rdfs:range git-lex:Thing` and still
takes a bracketed address.

Lane B is for a property whose value genuinely is a bare id and which is not part
of that family — `copia:lookMomentId` is the shape.

One consequence to know: under Lane A, a well-formed address pointing at
something that does not exist saves cleanly. The concrete range was what allowed
a dangling target to be rejected at save, and typed references give that up.

What is *not* in dispute is the mechanism: the range you declare picks the lane,
by exact match, and the author has to write the matching form.

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
at save. A kit-qualified key the ontology doesn't declare warns at save (with
the closest declared keys suggested) and its value saves as plain ungoverned
data; a bare key (`title:`) stays free. Multiple values for one key are a YAML
list (`- value` per line) — a repeated key is rejected at save.

## 9. Graph-only kits (no authored documents)

Some kits have no markdown at all — their instances are written by an engine
into the store (ravel's conversation Turns are the canonical example). Three
things change, and only three:

- **Omit `git-lex:foldered true` on every class.** No flag, no folder: kit-add
  and kit-update scaffold nothing into the repo. That's the whole mechanism.
- **Machine-derived ids are fine.** The `<class>Id` law still holds (every
  class declares its id property), but "human-readable, hyphenated" applies to
  *authored* ids. An engine-written class uses whatever stable id the source
  provides (a UUID, a sha, a content id) — stability and per-class uniqueness
  are the contract, prettiness is not.
- **Store-native references don't need the value-type suffix.** The
  `...Id`/`...Path`/`...Url` suffix exists so a human typing frontmatter knows
  what string belongs in the field. An engine-written `owl:ObjectProperty`
  whose value is the actual target node (an IRI, not an id string) is named
  plainly for the relationship (`parentTurn`, not `parentTurnId`) — the name
  still tells the truth about the value, which is the law underneath the
  suffix rule.

Everything else — the skeleton, enums, comments, publishing — is identical.

## 10. Naming rules (short, absolute)

1. **The name is the definition.** If the name needs a paragraph to explain,
   it's the wrong name.
2. **No generic words** — `kind`, `type`, `data`, `status` alone say nothing.
   Scope them (`health`, not `status`) or don't ship them.
3. **Value-type suffixes on references**: `...Id` / `...Path` / `...Url`.
   Plain literal fields get no suffix.
4. **Every class and property earns its place.** Add a field when a real
   document needs it — never because it "feels right." Unused fields don't
   stay neutral; they rot into confusion.

## 11. Publishing

When the ontology changes: commit in the source repo, then run the publish
flow (`subtexture/tools/ontology-publish` from the source repo root). It
copies to the kit repo, pushes both, and updates the live copy at
`https://repolex.ai/ontology/<name>/`. Repos pick the change up on their next
`git lex kit-update`.
