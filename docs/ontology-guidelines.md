# Ontology Guidelines

*Last updated for git-lex v0.1.0 (2026-08-03)*

This page is the **naming and identifier standard** for every ontology in the
git-lex ecosystem. [Kit ontology design](kit-ontology.md) shows you the
mechanics — where the file goes, how to declare a class. This page is about
the decisions that page can't make for you: what to *call* things, and how
identity and references work so that every kit behaves the same way.

These aren't style preferences. The tooling **derives behavior from names**
(which file a `git lex create` makes, which node a reference resolves to,
which property anchors a document's identity), so a name that breaks the
pattern doesn't just read badly — it breaks resolution. It's a starter guide;
it will grow. But everything on it is load-bearing today.

---

## 1. Names are the ontology

A predicate name is a permanent, public claim about what a thing *is*. Data
gets migrated; names get inherited by every query, every document, and every
person who reads the graph after you. So:

- **Optimize for the reader who has never seen your kit.** A great name is
  legible with zero context (`wearsOutfitId`); a bad one is a commitment to
  confusion (`ref2`).
- **The name carries the contract; the comment carries the meaning.** If a
  property's value must be a Being's id, the *name* says so
  (`equippedByBeingId`) — an `rdfs:comment` saying "joined by beingId" is
  documentation, not a contract. Nothing machine-readable enforces prose.
- **Don't abbreviate.** `choseNocturneActivityId` is long and instantly
  clear; `choseActId` is short and a lie waiting to happen.

## 2. The shapes of names

- **Classes**: `UpperCamelCase`, singular — `Being`, `FamiliarLookNote`.
- **Properties**: `lowerCamelCase` — `beingDescription`, `inPlaceId`.
- **Namespace**: your app's short name, one word, and the prefix matches the
  IRI's last segment — `@prefix copia: <https://repolex.ai/ontology/copia/>`.
  Hyphens are legal (`git-lex:` is valid Turtle).
- **Instance IRIs** are class-in-path, no base word:
  `https://repolex.ai/<app>/<Class>/<instanceId>` — e.g.
  `https://repolex.ai/copia/Being/w4r3z`. The a-box is the t-box minus
  `ontology/`.

### 2a. Banned words

These are not discouraged, they are **banned**. Each was banned by ruling after
it caused a real problem, and each has a replacement that says more.

| Banned | Why | Instead |
|---|---|---|
| `kind` | Not descriptive, and it gets thrown onto everything — a `kind` field tells you a discrimination happened without saying along what axis. | Name the axis (`substrate`, `severity`, `encoding`). But first check §2b: usually the class already carries it. |
| `mint`, `minted` | Implies a value is conjured rather than computed, which hides whether a rebuild reproduces it. Only a commit is ever minted. | `derived` when it's computed from source, `assigned` when it's allocated once and recorded (Pan's `panId`). |
| `ledger` | Says "list of entries" and nothing about what the entries *are* or when they're true. | Name the event (`SpoEvent`) or the graph (`LexHistoryGraph`). |
| `type`, `data`, `status`, `info`, `meta`, `value` — **alone** | Generic words say nothing on their own. | Scope them (`health`, not `status`) or don't ship them. Same rule as [Kit ontology design §10](kit-ontology.md#10-naming-rules-short-absolute). |

### 2b. Before you add a discriminator, check what already holds it

A field that answers "what sort of thing is this?" is usually the third copy of
a fact you already store twice. Before declaring one, ask:

1. **Does the class already say it?** If instances are typed
   `rdf:type app:Image`, then "which images" is already a one-line query and
   the class IS the answer. A parallel field can now disagree with it — and
   the moment two places can disagree, one of them will.
2. **Does an existing standard field already contain it?** Coarse values are
   often a *prefix* of a precise one you already keep. Splitting the precise
   value costs nothing and can never drift; storing the prefix separately can.
3. **If it survives both questions**, it is a real, independent fact — so name
   it for what it actually holds, per §2a.

The general form of this rule: **declare once, derive the rest.** A derived
value can be recomputed and can't rot. A stored duplicate is a drift source
with a maintenance schedule.

## 3. The identity law

> **Every foldered class declares its own identity property, named
> `<className>Id`, and identity is never inherited.**

A foldered class (one tagged `git-lex:foldered true`, so `git lex create`
scaffolds files for it) gets exactly one identity property:

```turtle
copia:setId a owl:DatatypeProperty ;
    rdfs:label "setId" ;
    rdfs:comment "Unique identifier for this Set (== filename)." ;
    rdfs:domain copia:Set ;
    rdfs:range xsd:string .
```

- The name is the lowerCamelCase class name + `Id`: `Being` → `beingId`,
  `FamiliarLookNote` → `familiarLookNoteId`.
- It's a **DatatypeProperty**, `xsd:string`, value `==` the filename (no
  extension). The id property *carries* the id — it never joins on one.
- **Never inherit identity from an abstract parent.** Shared facts
  (`groupTitle`) may live on an abstract class; identity may not. The
  tooling derives the id property from the concrete class's own name
  (convention-as-law — there is no annotation to point elsewhere, on
  purpose), so an inherited id is invisible to it. This is a law learned the
  hard way: copia's `Set`/`Sequence` once inherited `groupId` from abstract
  `Group`, and every Set anchored to nothing until v0.27 gave each class its
  own id.

### 3a. Which class owns the id: follow the identity event

The rule above says identity is never inherited. The question it doesn't
answer is *which* class declares it when a hierarchy is involved — and the
answer is not "the most specific one," it's:

> **Identity belongs to the class that owns the identity event.**

Ask: *when is identity assigned, and by what event?* Put the id property
there.

- **Authored documents** — the identity event is "a file was written." The
  foldered class owns it; the file **is** the thing and the filename **is**
  the id. That's where §3 comes from.
- **Stores and engines** — the identity event is "an object entered the
  store." The base class owns it. Pan assigns a `panId` at put — to *bytes*,
  before and independent of what those bytes turn out to be — so `panId` lives
  on `pan:Media`, while `pan:Image` / `pan:Audio` / `pan:Video` carry only
  their divergent metadata and declare no id of their own.

The decisive argument is what happens to a **reclassification**. Split
identity per subclass and a mislabeled `.heic` that turns out to be video must
retract `imageId` and assert `videoId` — identity churn caused by a
*classification correction*, for an object whose identity never changed. In an
append-only store that is exactly backwards. **Subclassing describes what a
thing turned out to be; it never creates a new identity.**

Subclass freely for the metadata, though — genuinely different property sets
(`duration` on video and audio, dimensions on image and video, `pageCount` on
documents) are what subclassing is *for*. One flat class carrying a union of
mostly-inapplicable optional fields is the open-domain smell, and it costs you
SHACL: you cannot say "duration is required for video" if everything is one
class.

Two things follow, one settled and one deliberately not:

- **The id's NAME is a ruling, not a derivation.** `<class>Id` is the default,
  but stored-data naming is the project owner's call, and where the two
  diverge the ruling wins. `panId` on `pan:Media` is the precedent: `mediaId`
  was the mechanical answer, was argued for on consistency grounds, and was
  rejected — a thing in Pan has a `panId`. Follow the stamp, then write it
  down here.
- **Whether a hierarchy ALSO carries a coarse type field is not a question
  this page answers.** Once the class carries the kind, a parallel
  `type`/`kind` field may be redundant, may be wanted at an API boundary, or
  may be the right home for a different fact entirely (an encoding, say —
  `image/png` is information no class split carries). Those are different
  answers with different names, and the choice is the project owner's, per
  the bullet above. Don't default it, don't infer it from the class split, and
  don't let a proposal become a convention by being written down. Ask.

## 4. The four kinds of id-valued properties

Almost every property whose value looks like an identifier is one of exactly
four kinds. Decide which one you have *before* you declare it:

| Kind | Declared as | Value | Example |
|---|---|---|---|
| **Identity** | DatatypeProperty, `xsd:string` | this document's own id (== filename) | `beingId` on `Being` |
| **Reference** (join) | ObjectProperty, `rdfs:range <TargetClass>` | the *target's* bare id, resolved to its IRI at emission | `equippedByBeingId` → `Being` |
| **External designator** | DatatypeProperty, `xsd:string` | an id owned by a system outside the graph; a grouping key, never resolved | `sessionId`, `lookerModelId` |
| **Vocabulary token** | ObjectProperty to a vocab class of named individuals | one of a closed set of terms | `cameraAngle` → `CameraAngle` |

The one that bites people is the second: **a reference is an ObjectProperty
with a declared range**, and its authored value is the target's **bare id**
(`storm-glass-lantern`), never a path (`Copia/Item/storm-glass-lantern.md`).
The resolver builds the target IRI from the range class + the value; a path
value produces a nonsense IRI and a dangling reference the save gate will
reject. If you're tempted to accept both forms, don't — one convention,
validated loudly, beats two conventions resolved silently (an alias is just
drift with a permit).

## 5. Naming reference properties

> **A reference property's name states the id it joins on:**
> relation + `<Target>Id`, deduplicating when the relation already names the
> target.

| Relation you mean | Name it | Not |
|---|---|---|
| Item is equipped by a Being | `equippedByBeingId` | `equippedBy` |
| Place connects to a Place | `connectsToPlaceId` | `connectsTo` |
| Item is in a Place | `inPlaceId` | `inPlace` (dedup: target already named) |
| Nocturne produced a Moment | `producedMomentId` | `producedMoment` |
| Moment's lineage source (Moment→Moment) | `sourceMomentId` | `source` |

Don't prefix with your own domain class (`includesItemId` on `Outfit`, not
`outfitIncludesItemId` — the domain already says whose property it is). When
the range is an abstract union (`depictsId` → `Depictable`), plain
relation + `Id` is right: the target kind comes from the target's `rdf:type`,
and the name shouldn't pretend otherwise.

Why so strict? Because the pre-v0.27 copia graph said `equippedBy:
"illuminator-robe"` — and nothing in that name tells you whether the value is
an id, a path, a label, or a Being at all. The rename wave that fixed it
touched 22 properties and queued a fleet-wide data migration. Names are much
cheaper to get right on day one.

## 6. Change discipline

- **The graph is append-only; so is the vocabulary's story.** You don't edit
  history — you supersede it. Retire a term by declaring the replacement,
  saying so in both comments, and recording the move in the ontology's
  changelog header (see copia.ttl's `# v0.27:` block for the shape).
- **Everything is derived; nothing is minted.** Ids come from filenames,
  IRIs from class + id, id properties from class names. If you're inventing
  an identifier at emission time that can't be re-derived from the source,
  stop — determinism is what makes rebuilds safe.
- **One edit point.** The app repo (or the kit repo, if the kit is the whole
  product) is the single source of truth; the publish pipeline copies
  outward. Never edit the installed copy or the kit copy of an app ontology.

## 7. The five-minute test

Before you ship a vocabulary, hand it to someone who has never seen your kit:
**can a markdown-repo developer understand it in five minutes?** Every name
that needs the comment read twice, every property that exists "for later,"
every second way to say the same thing — cut it. What survives is the
ontology.

**Checklist for a new property:**

1. Which of the four kinds is it? (§4)
2. Does its name carry the contract — id suffix for identity and references,
   target named for joins? (§3, §5)
3. Is there already a property that says this? (If yes: use it, don't alias
   it.)
4. Range declared? (References resolve — and get gate-checked — only if the
   range says where they point.)
5. Comment says what it *means*, in one sentence, without restating what the
   name and range already say.
