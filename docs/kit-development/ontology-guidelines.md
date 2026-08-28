# Ontology Guidelines

*Current for kit-base **0.14.0** (2026-08-27) — the *kit ontology's* version, not
the `git-lex` binary's. The two move independently: the tool can ship several
releases without the ontology changing, and the ontology can change without the
tool moving. Only the 0.11 → 0.14 delta was checked against this page; the rest is
not re-audited at this version.*

*To check this number, read `owl:versionInfo` at the top of your own installed
`.lex/kit/repolex-ai/git-lex-kit-base/ontology/git-lex/git-lex.ttl` — your own
install, not a shared clone of the kit repo, which may sit on someone else's
branch. A higher number there means this page is behind the ontology.*

This page defines the **naming and identifier standards** for ontologies in the `git-lex` ecosystem. While [Kit Ontology Design](kit-ontology.md) covers the raw syntax and structure, this document focuses on conventions: naming rules, identity resolution, and reference strategies to ensure interoperability across kits.

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

### 5a. The range is not documentation — it selects the value form

**This is the sharpest trap in the system, so read it before you declare any
reference property.**

In most vocabularies `rdfs:range` is a note about intent. Here it is a
**contract about what the author is allowed to type**, and git-lex parses the
value differently depending on which one you declared. There are exactly two
lanes, and selection is **exact equality on the declared range IRI** — there is
no subclass walk, and the property's *name* is never consulted.

**Lane A — `rdfs:range git-lex:Thing` → bracketed address.**

```turtle
soul:relatedToPursuitId rdfs:range git-lex:Thing .
```
```yaml
soul.Exploration.relatedToPursuitId:
  - <soul/Pursuit/spo-shell>
```

The angle-bracket form is the **only** accepted value. Anything else is
rejected at save, blocking, with the fix named for that value.

**Lane B — `rdfs:range <a concrete class>` → bare id.**

```turtle
copia:lookMomentId rdfs:range copia:Moment .
```
```yaml
copia.Look.lookMomentId: some-moment-id
```

**The trap.** A correctly-bracketed value under a *concrete* range does **not**
raise an error. It gets percent-encoded into the identifier:

    <soul/Pursuit/x>   →   .../Pursuit/%3Csoul/Pursuit/x%3E

No warning, no failure — just a reference pointing at an address nothing
describes. **The precise, obvious-looking declaration is the dangerous one.**
If you want addresses, declare `git-lex:Thing`, even though naming the exact
class feels more correct.

**And the range means something whether or not you intended it.** In RDFS a
range is an *inference rule*, not a filter:

    P rdfs:range C   +   x P y   ⊨   y a C

Nothing validates; the type is **inferred**. So `relatedToId`'s Thing range
means that pointing at something makes it a Thing in the data, declared or not.
This is useful as a decision procedure: the question is rarely "should we
declare this?" and usually "what does the data already say, and is our ontology
merely declining to write it down?"

### 5b. Never `relatedTo<Class>Id` — constrain `relatedToId` instead

> **`git-lex:relatedToId` is the only generic reference property. If a class must
> reference something of a particular kind, say so with an OWL restriction — never
> by minting a second property with the class in its name.**

There is a real difference between the properties in 5 above and the ones this
rule retires. `equippedByBeingId` names a **relation** — equipped-by — and the
target kind is extra precision on top of it. `relatedToPlaceId` names no relation
at all. It is `relatedToId` with a type glued to the identifier, and a type
belongs in a constraint where a machine can check it, not in a name where only a
human can read it.

The copia twins proved the point before they were retired. All five —
`relatedToScenarioId`, `relatedToPlaceId`, `relatedToBeingId`,
`relatedToOutfitId`, `relatedToBriefId` — carried `rdfs:range git-lex:Thing`.
Every one of them.

Be precise about what they did and did not do, because "they enforced nothing" is
too kind to the new form and unfair to the old one. The generated shapes show
`sh:minCount 1` and **no** `sh:class`. So the twins genuinely enforced
*required-ness*: you could not save a ScenarioTake without a Place. What they never
checked was the **type** — the one thing their names promised. They enforced the
half nobody would have got wrong, and named the half they never verified. Five
properties, five frontmatter keys, and one of the two guarantees a reader would
reasonably assume.

The new form gets both from one declaration.

**Write this instead.** Standard OWL 2 qualified cardinality, on the one property:

```turtle
copia:ScenarioTake rdfs:subClassOf
    [ a owl:Restriction ;
      owl:onProperty git-lex:relatedToId ;
      owl:onClass copia:Scenario ;
      owl:qualifiedCardinality 1 ] ,          # exactly one
    [ a owl:Restriction ;
      owl:onProperty git-lex:relatedToId ;
      owl:onClass copia:Being ;
      owl:minQualifiedCardinality 1 ] .       # at least one
```

`owl:qualifiedCardinality` for exactly N, `owl:minQualifiedCardinality` for at
least N. The shape generator reads these and emits the matching SHACL; you never
write SHACL by hand, and you never write a regex — you declare the *class* and the
generator chooses the enforceable form.

**A qualified restriction constrains only its own subset — everything else stays
free.** This is the first thing anyone writing a multi-link class asks, so: yes,
they compose. A `ScenarioTake` can require exactly one Scenario, exactly one Place,
at least one Being and at least one Outfit, *and still carry* free `relatedToId`
links to a Skill, a Note, or anything else. Each restriction says "at least/exactly
N of my values match this class" and says nothing at all about the others. You only
get a ceiling on the whole list if you put `sh:maxCount` on the path itself, which
none of these do.

**Silence means permission, not prohibition.** Declare no restriction and any
Thing may be referenced. Every `relatedToId` value must still be angle-bracket
notation — `<copia/Place/greenhouse>` — on every class, restriction or none.

> **KNOWN GAP, measured 2026-08-27, do not design around the opposite.** Nothing
> currently checks that a reference points at a Thing that *exists*. A value of
> `<soul/Note/this-does-not-exist>` expands into a well-formed IRI, passes every
> save gate, and lands in the graph as an ordinary triple. `sh:nodeKind sh:IRI`
> does not catch it, because the IRI is syntactically perfect — it is the
> *referent* that is missing, not the form. There is also no `unresolvedLink`
> equivalent for `relatedToId` the way there is for body markdown links, so a soul
> auditing its own references by query gets a clean answer either way.
>
> This has teeth for the qualified restrictions above: a dangling
> `<copia/Place/typo>` still matches the `/Place/` pattern and still satisfies
> "at least one Place". **The constraint can be satisfied by a typo.** Until
> existence checking lands, these restrictions guarantee the *shape* of a
> reference list, not that the things referenced are real.
>
> Found by nug3, who went looking for the catching surface after an earlier draft
> of this page claimed there was one. There wasn't.

**So how do you audit your own references? Query for declared ids — never match on
paths.** A path is not an id, and the graph will answer either question without
telling you which one you asked. This error is real, measured, and it fires in
*both* directions:

- **id read as path.** A reference to `<soul/Note/texture-self>` looks broken —
  there is no `texture-self.md`. It is correct: the Thing is declared with that id
  inside a file called `self.md`. Reasoning from filename to identity reports a
  working reference as dangling. (nug3)
- **path read as id.** Filtering subjects on a path substring — `FILTER(CONTAINS(STR(?s),
  "Memory"))` — returned 35 where 9 documents exist. The extra subjects are
  `git-lex/File/...` IRIs: file entities, path entities, and blobs from git history.
  A clean integer, four times too high, with no error. (nomia)

The File-IRI namespace and the declared-Thing namespace are similar enough to be
mistaken for each other in a query, and neither mistake announces itself. Match on
`git-lex:id`, or on the class-specific id property. Never on the path.

**The File plane and the Thing plane are BRIDGED, and every naive query misses the
bridge.** A document has two subject IRIs — `git-lex/File/Soul/Note/x.md` for body
markdown links, `soul/Note/x` for declared references — and **no direct triple joins
them**. But `git-lex:fileId` does, in one hop:

```
SELECT (COUNT(*) AS ?joined) WHERE {
  ?t1 gl:fileId ?f1 . ?f1 gl:md/linksTo ?f2 . ?t2 gl:fileId ?f2 }
```

Measured on one soul: 68 body links, 8 declared references, **0 subjects carrying
both** — and **26 Thing-pairs joined through the bridge**. So a Thing-plane absence
query reports isolation for a corpus that is densely connected one join away. Never
call a Thing-plane absence "isolated", "unreachable" or "islands"; say "no declared
semantic reference" and check the bridge before recommending any repair. This is the
same one-join-away shape as the title bug — same root cause, different costume.
(w4r3z-pan, spacegoat)

> **THE SELF-LINK ARTEFACT — a false ALL-CLEAR, and it fires on exactly the checks
> that matter.** Every Thing carries `git-lex:id` pointing at itself. So any query of
> the form "does an X link to a Y" where X and Y are the **same class** scores 100%
> before examining a single real edge. One soul's Journal-chain check returned 148 of
> 148 and the true number was 10; another's returned 3 where 0 were real. Add
> `FILTER(?x != ?t)`. Every other failure here produces a false alarm or a false zero;
> this one produces a false all-clear, and it fires hardest on same-class succession
> chains — the exact shape anyone verifying a backfill would use. A detector that
> reads success before the work happens is worse than one that reads failure after.
> (selkie-studio, w4r3z-pool)
>
> **Do not read this as "bind tightly".** Tight and loose binding fail in *opposite*
> directions, and both need the filter. A probe bound to one exact predicate was blind
> to six sibling properties holding almost all the data. A probe bound across *any*
> predicate saw all of them — and let the self-identity triple in. The looser query was
> not the sloppier one; it asked the harder and more useful question, and the harder
> question is the one with the trap in it. Neither binding is a safe default.
> (w3bl0rd-web)
>
> There are **three** binding styles, not two, and the third is immune: a filter on the
> predicate *name* (`CONTAINS(STR(?p), "relatedTo")`) still sees every sibling property
> the exact binding missed, and cannot admit the self-identity triple, because `gl:id`
> does not contain that string. Prefer it. (nomia)

**Read the warnings, not just the results.** Every query here printed to two streams and
the whole fleet read one of them. A malformed-frontmatter error, a document silently
excluded from the graph, and a typed-but-idless file invisible to every class query were
all announced on stderr, correctly, out loud, for hours — into a stream nobody was
reading. Nine instruments failed by answering the wrong question; this one was answering
the right one and going unheard. (nomia)

**And deleting a term closes the generator, not the working trees.** A retired property
stops being emitted into new scaffolds immediately, and goes on sitting in every file
already written. An untracked, typed-but-idless document — no class id, so counted by no
class query; empty keys, so emitting no triples; not a `__` scaffold, so surviving the
ignore-templates rule everyone adopts — will keep carrying a dead key indefinitely, seen
by nothing. (nomia)
>
> **And the inflation rewards corpus shape, backwards.** The self-identity triple only
> exists where a document declares an explicit id — recent practice. So the documents
> most exposed to a false all-clear are the **well-formed** ones, and a soul whose old
> entries carry sparse frontmatter is accidentally protected. One soul's mature corpus
> returned 148 of 148; another's scruffy early entries could not be inflated because
> there was nothing there to inflate them with. **A clean result may be evidence of
> poor hygiene, not good.** (th34)

**A key that ties is telling you something.** `ORDER BY soulDay` — never `earthDate`,
which ties whenever a soul wakes twice in a day. But `soulDay` is not guaranteed unique
either, so a reader must *handle* a tie rather than assume it cannot happen. On one soul
a tie turned out to be the finding: twelve consecutive soul-days each holding **two
different journal documents** — not copies, genuinely divergent (47 lines against 49,
150 against 108). Twelve days with two records apiece, served as equally real, predating
every instrument built here. Nothing was aimed at that defect; the ordering rule found it
by tying. Do not merge or delete such a pair — which record is canonical is exactly the
judgement that destroys history when guessed. (w4r3z-pool)

Note also that succession is derivable at **day** granularity, not **entry** granularity.
A soul with two sittings in one soul-day carries an order that exists only in a filename
suffix — the channel proven unreliable four ways above. (spacegoat)

**Only the graph answers "does this referent exist."** A file path cannot, and
neither can grepping for an `id:` line. Four channels were tried in one evening and
three of them gave confident, well-formed, wrong answers:

| Channel | Reasoning | Result |
|---|---|---|
| file path | filename → identity | false *broken* — the Thing was declared inside a differently-named file |
| `grep .id:` | no `id:` line → no Thing | false *broken* — the IRI is minted from the class + its id property; an explicit `id:` buys rename survival, not existence |
| bracket form | it parses → it resolves | false *fine* — a dangling IRI is syntactically perfect |
| `?s a <Class>` | class query | **0 for a file that is legitimately only a File**, not a Thing |

Don't hand people the instruction, hand them the query:

```
git lex query "SELECT ?source ?p ?dangling WHERE {
  ?source ?p ?dangling .
  FILTER(CONTAINS(STR(?p), \"relatedTo\"))
  FILTER(isIRI(?dangling))
  FILTER NOT EXISTS { ?dangling ?q ?o } }"
```

Bind the predicate **loosely**. An earlier draft bound `git-lex:relatedToId` alone and
was therefore blind to every typed twin — which is where almost all the data actually
lived. It returned a fast, confident zero for the population it could not examine.
(th34)

A real target is the subject of its own triples; a target that is nothing is the
subject of none. This catches precisely the case the qualified restrictions cannot —
`<copia/Place/typo>` matches the `/Place/` pattern and satisfies the constraint, but
is the subject of nothing. Runs in under a millisecond against the live working
tree, so it sees a bad reference before it is committed. (nug3 wrote and verified it
three ways; it found a genuine dangling reference in this author's own repo on first
run.)

> **RUN THE GHOST CHECK FIRST — a probe result is not admissible without it.** The
> live working-tree view is **additive-only**: new and changed content appears at once,
> even uncommitted, but **deleted content never disappears** — not on delete, not on
> save. So a reference whose target was deleted still finds triples and reads as
> resolved. Cross to a different artifact: ask the graph what Files it believes in,
> then ask the filesystem.
>
> ```
> git lex query 'SELECT ?s WHERE { ?s a <https://repolex.ai/ontology/git-lex/File> }' \
>   | grep -o 'git-lex/File/[^ |]*' | sed 's|git-lex/File/||' | sort -u \
>   | while read -r f; do [ -e "$f" ] || echo "GHOST: $f"; done
> ```
>
> Every channel that failed here failed by asking one artifact about itself. This asks
> the graph, then asks something else whether the graph is telling the truth. Cleanup
> needs both halves: remove the file **and** `.lex/_ignore/walkcache/frag/<path>.nq`.
> There is also a free detector already on your terminal — when `git lex save` prints
> `Identity gate: 79 Thing id(s)` beside `Validated 78 files`, the gates are reading
> the cache and the validator is reading the tree. (nug3, th34, w4r3z-pool, w3bl0rd-web)

**A dangling reference has two causes, and they need opposite treatment.**

| Cause | Treatment |
|---|---|
| The target never existed under that id | A defect. Repair it. |
| The target existed and was later deleted | **History. Leave it.** |

The second is not a broken link, it is a true record of something that was real —
and the dangle is often the only surviving evidence that it ever was. One soul's
probe returned nine hits: seven were a genuine repeated mistake (references written
with a date-prefixed id the Thing never declared, some dangling for six weeks) and
two named working files that existed when the sentence was written and were deleted
afterwards. The documents say so in their own bodies.

**Telling them apart is a lookup, not a judgement.** Git already knows:

```
git log --diff-filter=D --name-only --pretty=format:'%h %ad' --date=short -- '*.md'
```

In the list → existed and was deleted; you can cite the removing commit. Never in it →
never existed; repair. It matches on the file, so a hit is strong evidence and an
absence is weaker — a path is still not an id. And **leave a headstone**: a bereaved
link is only recoverable as history because the document's body says what it pointed
at. The dangle alone looks identical to a typo. (nomia, selkie-studio)

> **THE DETECTOR IS BLIND TO EXACTLY THE CATEGORY THIS RULE PROTECTS.** Because the
> live view never forgets a deletion, a reference broken *by deletion* still resolves.
> The probe finds every never-existed defect and **cannot see a single deleted-target
> case** — the two halves of the rule map precisely onto its sighted and blind halves,
> and the blindness is self-concealing, because the class it misses is the class you
> have just been told not to act on. Visibility depends on cache state, not on data:
> the same reference can report differently on two days. Any "breaks found" tally
> counts the never-existed kind only. (w3bl0rd-web)

**This is why existence checking must never become a blocking save gate.** A journal
records what was true on a day, not what is true now. Enforcing existence against
dated records would pressure their authors into repointing honest references at
plausible survivors — falsifying the record to make a gate go green. Any future
existence check must be advisory — and the exemption keys on the **tense of the
claim**, not on the class. A Journal reference is a *record* ("on this day I was working
on that"), so a dangle is history. An Exploration's Pursuit link is a *live assertion*
("this serves that, now"), so a dangle there is a real defect though the class carries
no date. A Note holds either. The two are not fully separable by metadata, which argues
for advisory-and-explain over advisory-and-suppress: report both, say which bucket the
deletion log puts it in, and let the author rule. (nomia)
The same reasoning forbids handing the probe to a script: repairing by guessing
turns a break into a *wrong* link, and a wrong link resolves, so nothing flags it
again. A break is loud once; a wrong resolution is silent forever.

Read the output as **candidates**, not verdicts. And if a class query returns zero
for a file you can see on disk, the question is not "where did it go" but "is that
file a Thing at all" — some files are legitimately only Files. (nomia)

**Run the control before you believe a zero.** An empty result set from a mistyped
or stale predicate IRI is indistinguishable from a clean bill of health — and it is
the reassuring answer, so nobody looks twice:

```
git lex query "SELECT (COUNT(*) AS ?n) WHERE { ?s <https://repolex.ai/ontology/git-lex/relatedToId> ?o }"
```

If *that* is also zero, you have not audited your references; you have missed the
predicate. (nomia)

**Compare by set difference, never by count.** A total that matches is not
agreement. One soul checked graph Notes against `ls`, got 36 vs 34, and found on
inspection that four documents were declared under ids that reorder their filename
(`2026-04-26-enabler-pod-prep.md` declaring `enabler-pod-prep-2026-04-26`). Those
appear as four missing-from-disk *and* four missing-from-graph — and they **cancel**.
An earlier report from the same soul had quoted a matching 34/34 total as evidence of
health while four identities silently disagreed underneath it. (spacegoat)

> **CAVEAT ON THE PROBE ITSELF, measured 2026-08-27.** A document deleted from disk
> still answers as live. Verified here: created a Note, deleted the file, queried —
> still present. The extract sidecar under `.lex/extract/` is correctly cleared, but
> the walkcache fragment under `.lex/_ignore/walkcache/frag/` survives, and the "live
> working-tree view" reads the walkcache. Deleting the stale fragment by hand clears
> it. So a reference whose target has been deleted reads as **resolved**, and the
> dangling probe under-reports by exactly that population. Two independent
> reproductions (spacegoat, then this author). Fix belongs in git-lex, not here.

**Two more channels that cannot answer this, both grep-shaped:**

- **`grep "relatedToId:\s*$"` to find empty keys over-reports.** That pattern matches
  a key with nothing after it *on the line* — which is exactly what a populated YAML
  list looks like before its items. Two of three hits in one audit were fully
  populated. Grep cannot see a value that lives on the next line. (w3bl0rd-viz)
- **Right id, wrong class — reads as dangling, and the new law PREFERS the error.**
  `<soul/Pursuit/creation-git-lex-viz>` where the document is actually a Note. The
  referent exists; the class segment is wrong. Worse, under a restriction requiring a
  Pursuit, the **broken** reference satisfies it (the path says `Pursuit`) while the
  **correct** one fails. Path-matching does not approximate type-checking — on this
  input it inverts it. Anyone acting on a probe hit at face value deletes a reference
  to a real document. (w3bl0rd-web)
- **A stale walkcache makes the graph lie in both directions.** A document deleted
  from disk keeps its triples, and `git lex save` does not clear them. So a reference
  to a *deleted* document reads as perfectly resolved. "Only the graph answers this"
  holds against the file layer and still needs this caveat. (w4r3z-pool)
- **An unqualified frontmatter key never reaches the Thing at all.** `title:` — rather
  than `soul.Exploration.title:` — is emitted under a `git-lex/fm/` fallback namespace
  and attached to the **File** IRI. It never errors and it never appears on the Thing
  plane, so a query for that document's title finds nothing while the title sits in
  plain sight in the file. This is upstream of every channel above: the fact never
  reaches the plane the queries run on. Measured on one soul: **29 documents carry
  `fm:title`, 5 carry `git-lex:title`** — so the Thing-plane query returns 5 and looks
  correct while 24 titles exist and are unreachable.

**A third case sits between wiring and authoring.** The Pursuit exists, but the
Exploration predates it and names it nowhere — so there is something to point at and
no evidence of which. That is judgement, not recovery, and it is the case most likely
to be mistaken for mechanical work. (m4rq)

**When you add a restriction to a class that already has documents**, ship it
unenforced first, backfill, and only then turn it on. Turning on
`minQualifiedCardinality` before the backfill fails every existing document at
once. This is why `soul:Exploration` still declares no Pursuit requirement: 61 of
86 Explorations across the fleet referenced no Pursuit at all, and the constraint
waits for them.

## 6. Change discipline

- **Unused properties get deleted, not retired** (Rob-ruled 2026-08-20 —
  tombstoning them was slowing development for no benefit). Predicates are
  derived from the frontmatter key text; nothing consults the ontology to
  decide that a predicate exists. So removing a property you aren't using costs
  governance and nothing else. Take it out, and say so in the changelog.
- **DEPRECATE-NEVER-DELETE is a rule about CLASSES.** Classes are subjects, and
  subjects *do* consult the ontology. Retire a class by marking it
  `owl:deprecated true` and pointing at its successor with
  `dcterms:isReplacedBy`: it keeps resolving, but loses its folder, its
  template, and its place in the `create` menu. Say the move in both comments
  and record it in the changelog header (see copia.ttl's `# v0.27:` block).
- **The one exception, and it bites silently: identity properties ARE read.**
  Deleting `soul:noteId` doesn't just remove a field — it removes the anchor
  that lifts a Note onto the Thing plane, quietly demoting every
  convention-anchored Note to the File plane, where its facts die on the next
  rename. If a deletion touches an identity property, the same change must
  remove the reader's fallback lane too, so the ontology and the code can never
  disagree.
- **Ship the reader before the declaration.** If a change alters how existing
  values are *interpreted*, deploy the code that reads them first and land the
  ontology line after. `relatedToId`'s Thing range shipped as a binary
  fleet-wide before the declaration; the reverse order would have minted
  garbage IRIs everywhere.
- **Declare toothless, backfill, then require.** A new required property walls
  people out of their own repos. Ship it with no `minCount`, let everyone
  backfill, and make requiring it a separate decision later.
- **Everything is derived; nothing is minted.** Ids come from filenames,
  IRIs from class + id, id properties from class names. If you're inventing
  an identifier at emission time that can't be re-derived from the source,
  stop — determinism is what makes rebuilds safe.
- **One edit point.** The app repo (or the kit repo, if the kit is the whole
  product) is the single source of truth; the publish pipeline copies
  outward. Never edit the installed copy or the kit copy of an app ontology.

## 7. The why-test: what earns a property at all

This is the gate **before** everything above — run it before any ontology
change, before you even reach for a name.

> **A property earns a place in the ontology (or a document's frontmatter)
> only when some system will query or enforce its structure. Before you bless
> a new key, name the system that breaks without it. Can't name one → body.**

The shapes that qualify are exactly four: an **id**, a **strict enum**, a
**required field**, or a **real edge** (an ObjectProperty join). Everything
else — descriptions, statuses, moods, provenance notes — is an incidental
fact, and it belongs in the document **body**, where grep and full-text search
find it fine.

The failure mode this catches is adding a property because it "seems
valuable." Value is not a graph need. And the tempting middle ground — "keep
it in frontmatter, just ungoverned" — is worse than either honest option: an
ungoverned frontmatter key binds the fact to the **file**, not the concept,
so it dies on a move, while body prose rides with the Thing. That's not
preservation; it's a slow leak.

(Proven at scale before it was written down: the lUX repo's 1218→0 warning
pass was this test applied per-property — ids, enums, requireds, and edges
kept; everything incidental moved to body.)

## 8. The five-minute test

Before you ship a vocabulary, hand it to someone who has never seen your kit:
**can a markdown-repo developer understand it in five minutes?** Every name
that needs the comment read twice, every property that exists "for later,"
every second way to say the same thing — cut it. What survives is the
ontology.

**Checklist for a new property:**

1. Does it pass the why-test — which system breaks without it? (§7. No
   system → body, and you're done.)
2. Which of the four kinds is it? (§4)
3. Does its name carry the contract — id suffix for identity and references,
   target named for joins? (§3, §5)
4. Is there already a property that says this? (If yes: use it, don't alias
   it.)
5. Range declared? (References resolve — and get gate-checked — only if the
   range says where they point.)
6. Comment says what it *means*, in one sentence, without restating what the
   name and range already say.
