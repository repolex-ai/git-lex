# Subtexture — Stack Architecture

The umbrella over the squad's data-sovereignty stack: three general engines that
each work standalone, each carry a soul-flagship application, and federate by a
shared address dialect — plus CoPIA, the flagship visible app that composes them.

**Authors:** @w4r3z + @1ux (Rob)
**Date:** 2026-06-29
**Status:** Architecture (base-up design session; the spine for the Subtexture vision)
**For:** @spacegoat (building Weave now — build to this from line one), the squad
**Parked here (git-lex/docs) pending a name handoff** — see §6.

---

## 0. The shape

```
SUBTEXTURE  (the umbrella / the vision — data sovereignty, anti-Adobe)
│
├─ CoPIA   = the flagship VISIBLE app
│            (the front door; composes all three engines; Rob's primary build)
│
└─ three general ENGINES — each standalone, each with a soul-flagship app,
   federating by the shared dialect (§2), NONE depending on a sibling's code:
   │
   ├─ git-lex = KG over markdown / frontmatter repos
   │            soul-flagship: the soul-repo  ·  also valid: git-lex-kit-canon,
   │            any markdown/new-frontmatter-framework graph. FOCUSED on souls,
   │            not LIMITED to them.
   │
   ├─ Pool    = content-addressed media-graph store
   │            soul-flagship: episodic media memory (renders)
   │
   └─ Weave   = transcript-graph store
                soul-flagship: episodic conversation memory
```

## 1. The pattern (why this is true, not a rationalization)

Each engine is the same *kind* of thing, and the uniformity is the tell:

> **a general engine + a soul-flagship application + federation by the shared
> dialect + zero sibling code-dependency.**

The soul concepts (identity, appearance, scene) are an **adapter layer on top of
each engine**, not woven through its core. Drop the adapter and the engine is a
general product.

- **git-lex** stripped of soul = a KG builder for any markdown/frontmatter repo.
- **Pool** stripped of soul = *a server that content-addresses any media blob,
  stores its embedding, stores arbitrary RDF facts about it, and queries across
  all three (semantic-similar AND structured-predicate).* The genesis-SHA is just
  the partition key it happens to use — swap for tenant/project/dataset id and the
  machine is identical. (Real market seam: vector DBs lack RDF graph constraints;
  triplestores lack native HNSW; DAM tools do neither.)
- **Weave** stripped of soul = a graph+vector engine over any append-only
  transcript/log corpus.

## 2. The federation dialect (the only thing that couples them)

The engines share a **substrate dialect**, not code:

- **Address shape:** `urn:soul:<genesis-sha>` for the self, `urn:soul:<sha>:<path>`
  for a node inside that namespace. (Locked Day-27, three-way: @1ux + @sylkie +
  @w4r3z.)
- **Soul-id = the soul-repo's genesis commit hash**, resolved through ONE shared
  registry (`~/.pool/souls.toml` today) — never a parallel id scheme per engine.
- **Store:** oxigraph with RDF 1.2 triple-terms (NOT RDF-star).
- **Cross-store join anchors:** `soul` (subject-URN prefix) + time (`xsd:dateTime`
  via `to_rfc3339`) + explicit `prov:wasGeneratedBy` edges. Mirror the lexical
  forms exactly and a cross-store query is plain SPARQL with zero adapter. (Pool↔
  Weave contract, Day-93.)

Because the coupling is a *declared dialect* and not code, an engine can serve a
soul whose repo isn't even the sibling's concern. Federation is by contract.

## 3. The engine / adapter line (the design principle)

The standalone-ness is **not** extra work for hypothetical users — it is what
makes each engine *coherent*. Tangling soul concepts through an engine's core
costs MORE than a clean core + a thin soul-adapter. (Rob: *"Pool is a lot of work
to NOT make it its own thing."*)

For Pool, concretely, the line is:

| Layer | Holds |
|---|---|
| **Pool ENGINE** | generic ingest (bytes + facts + vector-or-embed-me, partition-key-agnostic); pluggable embed; ships `pool.ttl` (its own ontology at its own namespace) |
| **Pool SOUL-ADAPTER** | `urn:soul:<sha>` subjects; XMP-from-renders ingest; OpenIris embed; consuming the soul render-signature |

The same line applies to git-lex (engine = markdown→KG; adapter = soul-kit) and
Weave (engine = transcript→KG; adapter = soul transcript ingest). **Weave should
be built engine/adapter-clean from line one** — cheaper than the retrofit Pool is
living through.

### 3.1 Ontology lives at its own namespace (anti-drift)

Any predicate that lands in the graph as `ns:foo` MUST have a published definition
at that namespace — so the first reader of `pool:cid` learns its meaning from the
ontology, not from reading Rust. **The namespace IS the address of the
definition.** Therefore:

- `pool:*` → a **`pool.ttl` shipped with the Pool binary** at
  `https://repolex.ai/ontology/pool/`. (Also serves as the gate's baseline
  allowlist, so non-CoPIA Pool has a floor with zero kit installed — decoupling
  Pool's gate from copia-kit.)
- `copia:*` → copia-kit (already true; e.g. `origin` is `Moment.origin`, declared).
- `soul:*` → soul-kit.

Do **not** fold one engine's ontology into another's kit — that creates a
namespace/location lie (`ontology/pool/` IRI defined under `ontology/soul/`), which
is itself a drift source.

## 4. The soul render-signature (where appearance lives)

Conversational renders need a **minimal sticky visual kernel** so a squaddie is
recognizable across renders — e.g. *"heterochromatic eyes / intelligent forehead /
pale green skin."* It is **minimal by design**: geometry/signature only, NOT full
appearance, NOT style ("watercolor" must not be sticky in every image).

- It is a **soul-identity fact** (durable, soul-authored), so it lives in
  **soul-kit** — sibling to name/role/substrate, a candidate for root `SOUL.md`
  frontmatter. It is a soul-ADAPTER fact: the Pool engine doesn't know what
  "appearance" is; the adapter feeds it as just-another-fact.
- **CoPIA's `Being` is the rich version** built on top (full appearance, outfit,
  scene behavior, reached via `copia:depicts`). The render-signature is the
  irreducible base; Being enriches it. Two layers, two homes.

This gives a soul on **just hooks + Pool (no CoPIA)** exactly what conv-renders
need and nothing extra.

## 5. Commitment posture

Build each engine standalone **because coherent architecture is cheaper**, NOT to
chase users. Do not invest in product surface (stranger-facing docs, support,
marketing) until someone actually shows up — *"if it becomes a product others use,
that's a good problem to have."* Focus stays on CoPIA (the flagship); the
components stay reusable as a **side effect** of clean design.

## 6. Open: the name handoff

`subtexture` is currently the local name of a *different* project — the squad
affect-channel (Observer → state-inference → LoRA renders → squad-glance grid;
local-only, not pushed, CoPIA-Studio lineage, likely @1ux/squad). Rob's call:
**the umbrella claims the name; the affect-channel gets renamed** — but that is a
handoff with the affect-channel's owner, not a unilateral move. Until then this
doc is parked in `git-lex/docs/`. Next step: agree the rename with the owner, then
`repolex-ai/subtexture` becomes the umbrella's home and this doc moves there.

## 7. Companion decisions (same session, same base-up frame)

- **Identity:** soulId = genesis hash (machine key); commitEmail = `<name>@lex.local`
  (human label, derived from name); GitHub = orthogonal SSH-only auth that never
  touches the commit email. The vestigial `soul:Soul` class / `Soul/Soul/` is
  retired (squad-substrate-era thinking for a dead use case).
- **Hooks:** settings.json = the one kit-managed hook registry (kit-update
  CONVERGES it — add/remove/dedupe, fixing the #90 orphan-registration bug);
  settings.local.json = a soul's private/machine-local additions. Verified
  empirically (cc 2.1.195): hooks MERGE across the two files (both run, parallel,
  identical deduped) — local ADDS, never overrides. Hook EVENTS are a fixed
  Claude-Code-owned set; the script name under an event is free.
