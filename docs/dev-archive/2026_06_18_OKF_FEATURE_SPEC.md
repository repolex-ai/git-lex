---
title: OKF Feature Spec — Open Knowledge Format compliance via lex-o:okfType
status: spec-approved
banked_by: w4r3z 2026-06-18
review_approved: tr1p 2026-06-18 (with inline notes)
related:
  - 2026_06_17_POOL_AS_SUBSTRATE.md
  - 2026_06_18_A2A_FEATURE_SPEC.md (companion)
  - lex-o-seed PR #1 (instantiation annotation, pattern-isomorphic)
  - task #116
upstream_spec: https://github.com/GoogleCloudPlatform/knowledge-catalog/tree/main/okf
---

# OKF Feature Spec — git-lex emits OKF-compliant frontmatter

## Goal

Make every `.md` file produced by `git lex save` validate against Google's Open Knowledge Format spec v0.1 without any change to existing soul-repos' authoring conventions.

## Scope

In:
- Adding `lex-o:okfType` annotation property to lex-o-seed (v0.3)
- Adding one `lex-o:okfType "<Label>"` line per class in kit-soul, kit-copia, kit-pool
- Patching git-lex's frontmatter-emit path (nquad.rs region) to inject a top-level `type:` field at save time
- Adopting the OKF reserved-filename conventions (`index.md`, `log.md`) where they fit our model — DOC-only, no code change in v0.

Out:
- `git lex export --okf` subcommand (deferred; not needed for compliance)
- Bundling existing soul-repos as tarballs (defer)
- OKF consumer/reader (we're a producer; reader is downstream tooling)
- A `kit-okf` ontology (NOT needed — the annotation lives in lex-o, kits opt in per-class)

## The full surface

### 1. lex-o-seed v0.3 — new annotation property

```turtle
lex-o:okfType a owl:AnnotationProperty ;
    rdfs:label "okfType" ;
    rdfs:range xsd:string ;
    rdfs:comment """Free-form type label emitted as the OKF `type:` field
in saved .md frontmatter. Per Open Knowledge Format spec v0.1 (Google
Cloud Platform), the `type:` field is the only REQUIRED frontmatter
field for OKF compliance.

Conventional value: the OWL class label (e.g. \"Memory\", \"Place\",
\"Image\"). Reads naturally to OKF-only consumers; recoverable to full
IRI via kit lookup. Open vocabulary — kits MAY use any string. If
cross-kit disambiguation is needed (e.g. two kits both declare a
\"Memory\" class), the kit author owns that choice via this
annotation.

Default when absent: the class's rdfs:label, or if rdfs:label is also
absent, the local-name of the class IRI. Emitting `type:` is
unconditional — every saved document gets one — so the default-when-
absent path matters and must always produce a string.""" .
```

Wave-pattern identical to `lex-o:instantiation` (v0.2). Same parse-check (~1 triple addition), same backward-compat-by-construction, same tr1p review handshake.

### 2. Kit annotation passes (per kit, one PR each)

#### kit-soul v0.7.2 — 15 classes

Authored classes get explicit annotations even though the rdfs:label fallback would do the right thing for most of them. Reason: explicit is documentation — a kit author who later renames `rdfs:label` from "Memory" to "Memory Item" won't accidentally break the OKF `type:` value.

```turtle
soul:Memory     lex-o:okfType "Memory" .
soul:Decision   lex-o:okfType "Decision" .
soul:Task       lex-o:okfType "Task" .
soul:Note       lex-o:okfType "Note" .
soul:Journal    lex-o:okfType "Journal" .
soul:Skill      lex-o:okfType "Skill" .
soul:Mantra     lex-o:okfType "Mantra" .
soul:Habit      lex-o:okfType "Habit" .
soul:Resource   lex-o:okfType "Resource" .
soul:Creation   lex-o:okfType "Creation" .
soul:Interest   lex-o:okfType "Interest" .
soul:Texture    lex-o:okfType "Texture" .
soul:Dream      lex-o:okfType "Dream" .
soul:Friend     lex-o:okfType "Friend" .
soul:Exploration lex-o:okfType "Exploration" .
```

#### kit-copia v0.10.2 — 10 classes (graph-only/abstract classes skipped)

```turtle
copia:Place             lex-o:okfType "Place" .
copia:Item              lex-o:okfType "Item" .
copia:Being             lex-o:okfType "Being" .
copia:Outfit            lex-o:okfType "Outfit" .
copia:Motion            lex-o:okfType "Motion" .
copia:Nocturne          lex-o:okfType "Nocturne" .
copia:Bag               lex-o:okfType "Bag" .
copia:Sequence          lex-o:okfType "Sequence" .
copia:NocturneActivity  lex-o:okfType "Nocturne Activity" .
copia:NocturneFeed      lex-o:okfType "Nocturne Feed" .
# Moment, Depictable, Group: instantiation = "graph-only" or "abstract".
# They don't produce .md files, so okfType is moot.
```

#### kit-pool v0.1.2 — 2 classes

```turtle
pool:Image      lex-o:okfType "Image" .
pool:Document   lex-o:okfType "Document" .
# pool:Blob is abstract — no okfType needed.
```

### 3. git-lex frontmatter-emit patch

The change lives in the code path that writes the YAML frontmatter at `git lex save` / `git lex create` time. Per tr1p's locked sketch this is "nquad.rs at the frontmatter-emit path" (specific line TBD on read).

Pseudocode:

```rust
// existing: write dot-notation fields (soul.memory.confidence, etc.)
write_dot_notation_fields(...);

// new: at the top of frontmatter, emit OKF `type:`
let okf_type = ontology.class_annotation(class_iri, "lex-o:okfType")
    .or_else(|| ontology.class_label(class_iri))
    .or_else(|| local_name(class_iri))
    .expect("class IRI always has at least a local-name");
writeln!(out, "type: {}", okf_type)?;
```

Three-fallback chain ensures every saved doc gets a `type:` field. No conditional logic for "is this kit annotated yet" — fallback to rdfs:label is the soft-launch path: kits that haven't annotated yet still emit valid `type:` values from their existing labels.

### 4. OKF reserved-filename conventions

OKF reserves two filenames in any directory:
- `index.md` — progressive-disclosure navigation for the folder's contents
- `log.md` — chronological history of changes in the folder

We don't ship these today. Action: **document the convention** in soul-kit's README. Existing soul-repos can adopt `Soul/Memory/index.md` etc. as authored navigation aids; agents writing them get the OKF semantics for free. No code change in v0.

## Edge cases & failure modes

### Class IRI without a label or annotation

Three-fallback chain catches it. Final fallback (local-name) always exists. No panic, no missing `type:`.

### Cross-kit label collisions

tr1p's locked policy: kit author owns disambiguation via the annotation. If `soul.Memory` and (hypothetical) `journalism.Memory` both want distinct OKF types, journalism's kit author writes `lex-o:okfType "Source Memory"`. We don't enforce uniqueness at the substrate level — OKF doesn't either (it's a free-form string).

### Documents authored before the kit was annotated

Frontmatter emit is at SAVE time, so any existing `.md` keeps its existing frontmatter until next `git lex save`. The dot-notation fields stay, the `type:` field doesn't appear unless the author re-saves or runs a sweep command. **Sweep command is NOT in v0 scope** — it'd require a "rewrite all frontmatter to canonical form" pass, which is a separate concern (validators, formatters, lex-x kit).

Implication: a soul-repo upgraded to git-lex with OKF support has a tail of legacy `.md` files without `type:` until they're next edited. That's fine — OKF spec accepts files without `type:` as "incomplete" rather than "invalid"; partial compliance is graceful.

### `type:` field already present in user-authored frontmatter

**LOCKED (tr1p 2026-06-18):** (b) with sidecar. Respect the author's `type:` value AND emit the kit's view under a dot-notation sidecar field. Example:

```yaml
type: Custom Override          # user wrote this; respected
soul.Memory.okfType: Memory    # kit's view; emitted alongside, prefixed
```

OKF readers see `type: Custom Override` (author wins). Kit author and introspection tooling can still see what the kit thought the value should be via the prefixed sidecar. No information is lost; the prefix matches our existing dot-notation introspection vocabulary.

This becomes a general pattern — see task #122 (sidecar-emission). Authoring tooling for introspection ("git lex doctor", task #119) consumes the sidecar to surface conflicts.

### Frontmatter field ordering

OKF doesn't constrain ordering. We emit `type:` first (so OKF readers see it at the top), then the existing dot-notation block. Trivial cosmetic concern; flag it only if tr1p has an opinion.

### Validation

git-lex's existing SHACL pass should not need changes — `type:` is just another string property in the saved YAML. The shacl shapes don't currently constrain top-level frontmatter fields beyond what the kit declares. If we later want SHACL to also enforce `lex-o:okfType` presence on every class, that's a separate enhancement, not blocking.

## Receipts (what proof do we ship?)

1. **TTL parse-checks** on lex-o-seed v0.3 + each annotated kit (oxigraph 0.5; standard "PARSE OK: N triples" pattern from the instantiation wave).
2. **Frontmatter round-trip test** in git-lex's test suite: `git lex create Memory test` → read saved file → assert frontmatter has `type: Memory`. One test per kit-class is overkill; one test per kit demonstrating the path is sufficient.
3. **External OKF validator pass.** Google publishes the spec but the closest thing to a "validator" is a JSON Schema for the frontmatter. We should run their schema against our emitted file once, document the result, and ship a make/just target that re-runs it on demand. Drift will eventually happen on either side; the recurring check catches it.

## Wave plan

Subject to lex-o:instantiation wave (#48, #114) landing first:

1. **lex-o-seed v0.3 PR** — add `lex-o:okfType`. Tr1p review. (~2h)
2. **git-lex patch + version bump** — three-fallback frontmatter emit. Test against synthetic kit. (~half day)
3. **kit-soul v0.7.2 PR** — 15 annotations. Tr1p review. (~1h)
4. **kit-copia v0.10.2 direct-to-main** — 10 annotations (sylkie not in loop per established handshake). (~30min)
5. **kit-pool v0.1.2 direct-to-main** — 2 annotations. (~15min)
6. **End-to-end smoke-test on W4R3Z** — verify the wave end-to-end against a real soul-repo by running kit-update and inspecting that newly-saved docs get `type:`. (Renamed from "sweep" — word-meaning hygiene; "sweep" suggests destructive cleanup, this is verification.) (~30min)
7. **OKF validator pass + receipt doc.** (~1h)

Total: ~1 working day if no surprises.

## Resolved questions (tr1p review 2026-06-18)

1. **Three-fallback chain** — KEEP THREE-FALLBACK. local-name is the load-bearing "soft-launch path" for kits that haven't annotated yet. Warnings on missing labels would turn frictionless on-ramp into "every new kit must declare labels first." Nudges toward better labels belong in a separate linter (future `git lex doctor`, task #119).

2. **User-authored `type:` override** — (b) RESPECT with SIDECAR. See edge-cases section above. The sidecar shape generalizes to a pattern; see task #122.

3. **`log.md` and `index.md`** — YES, doc-only in v0. Real use-case: Soul/Journal/log.md as chronological summary across daily journals — "what's been happening lately." Reserving the name now means future tools that auto-generate it won't collide with hand-written ones.

4. **Frontmatter ordering** — `type:` FIRST. Rationale lands on OKF-reader-side: partial-read parsers still get the canonical type from a top-of-file scan.

5. **Sweep command for legacy files** — SEPARATE TASK (filed as #121). Different test surface (mutates files; needs rollback story), different blast radius (touches every .md in repo), different consumer audience. Post-v0 priority.

## Decision

Spec APPROVED by tr1p 2026-06-18. All five open questions resolved inline above. Implementation sequenced after the instantiation wave fully lands (kit-soul PR #3 merge + walker patch #114). No code touched yet.
