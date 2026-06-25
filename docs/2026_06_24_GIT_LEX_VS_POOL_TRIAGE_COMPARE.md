# git-lex × Pool — Cross-Repo Triage Compare

**Date:** 2026-06-24
**Author:** W4R3Z (Day 38 — first real run of the `/triage-compare` skill)
**Inputs:**
- `git-lex/docs/2026_06_24_SOFT_RELEASE_TRIAGE_LIST.md` (34 findings)
- `pool/docs/2026_06_24_SOFT_RELEASE_TRIAGE_LIST.md` (11 findings)

**Method:** loaded both triage lists, matched findings by **root cause not symptom**,
then **confirmed every cross-repo seam empirically** (read the actual shared files /
store paths / ontology consumers — not the triage prose). The seam map below is from
`grep`, not from memory.

> A copy of this doc is filed in `pool/docs/` as well — the comparison belongs to
> both repos.

---

## TL;DR — the one thing to take away

git-lex and Pool **do not call each other**. There is no shared library, no HTTP API
between them, no shelling-out. They are siblings on one RDF spine. So *most* of their
findings are genuinely independent — **don't force links.**

BUT they share **two real seams**, both one-directional (**git-lex → Pool**):

1. **Pool reads git-lex's oxigraph store** (`sync_from_soul_repo` →
   `<soul_repo>/.git/lex/oxigraph`).
2. **Pool reads the kit ontology git-lex installs** (`locate_kit_copia_ontology` →
   `.lex/ontology/copia/copia.ttl`).

Both seams flow **out of** git-lex **into** Pool. That asymmetry is the headline:
**git-lex is upstream. Its instability becomes Pool's defensive code.** Two of Pool's
findings are *scar tissue from git-lex*, not Pool bugs. Fix them on the git-lex side.

And the two repos **converge** (not connect) on the same meta-patterns — proving
these are *worldview* bugs, not integration bugs. Where they converge, **Pool already
solved it the right way** → backport Pool's approach into git-lex.

---

## A. SHARED ROOTS — convergent meta-patterns (fix-once-helps-both *thinking*)

These appear in BOTH repos independently. They don't share code, so there's no
single fix — but the *direction* is clear: **Pool is the more-evolved version of the
same idea** (it started from the worked-out Python spike;
see [[project_git_lex_vs_pool_learning_history]]). Backport Pool → git-lex.

| Meta-pattern | git-lex instance | Pool instance | Who's ahead | Direction |
|---|---|---|---|---|
| **Silent-drop-on-name-mismatch** | **B1** — class-casing footgun (`soul:memory` vs `soul:Memory`); query returns 0, no warning | **PC1** — gate drops un-homed/mis-cased predicates silently | tie (both silent) | Both need the **same fix**: an opt-in debug log (`GIT_LEX_DEBUG_TTL` ↔ `POOL_GATE_DEBUG`). Keep silent default; make it inspectable. **Build them to mirror each other.** |
| **Dual-parser** (hand-roll next to a real engine) | **C19/C3/C13** — `get_kit`/`add_prefixes`/`parse_shacl_hints`/`get_class_instantiation` hand-scan YAML+TTL line-by-line | **PC2** — `xmp.rs` regex-parses RDF/XML | **Pool, decisively** — Pool uses serde for TOML + oxigraph for TTL everywhere *except* xmp.rs; git-lex hand-rolls TTL even in `ontology.rs` | **git-lex ← Pool.** Adopt Pool's "parse via the store" approach. Pool's `ontology.rs` is the model; git-lex's `ontology.rs` is the offender. (Pool's *own* lone offender, xmp.rs, is lower-pri — writer/reader symmetry with sylkie's stamper.) |
| **Portability gate** | **C9** — POSIX-only commit hook (`#[cfg(unix)]`, `#!/bin/sh`, `date -u`) | **PC3** — macOS-only `pool service` (launchd) | tie | Both → **honest README caveat**, no code change now. Same Bucket-4 treatment in both repos. |
| **Silent-fallback-default** | **frozen settings.json** (C23: identity resolver never re-reads repo.yml) | **worker.rs:143** (config-default → dead endpoint) **+** the **copia COPIA_CONFIG** bug | — | **Three-way pattern across the whole stack.** The lesson is squad-wide: a silent fallback to a default is how a deliberately-set value gets ignored. Audit for it everywhere. |

**The meta-meta-finding:** the patterns that show up in BOTH repos are the ones worth
a squad-level rule, because they're clearly not local accidents — they're how *we*
(the builders) think when we're moving fast. Silent-drop and silent-fallback are the
two to internalize: **a name/case mismatch or an ignored override should never be
invisible.**

---

## B. CROSS-REPO EDGES — Pool findings that are actually git-lex's fault

These are the high-value ones the skill exists to find: a finding filed against Pool
whose **true cause is git-lex's contract**. Fixing the Pool side would be patching a
symptom. **Confirmed by reading the actual seam code**, not inferred.

### EDGE-1 🔴 — Pool's 3-path ontology fallback is git-lex install-churn scar tissue
- **Where:** `pool/src/bin/pool.rs:480` `locate_kit_copia_ontology` falls through
  **three** candidate paths:
  `.lex/ontology/copia/copia.ttl` (canonical), `.lex/kit/repolex-ai/git-lex-kit-copia/ontology/copia/copia.ttl` (old),
  `Copia/.kit/ontology/copia.ttl` (older). The comment literally says *"the
  kit-install layout has evolved; tolerate the spread."*
- **True cause:** **git-lex** moved its kit-install location twice. Pool grew defensive
  code to chase it. This is not a Pool bug — it's Pool absorbing git-lex's instability.
- **Fix side: git-lex.** git-lex should commit to **one** canonical kit-install path
  and a stable contract ("kit ontology always lives at `.lex/ontology/<kit>/<kit>.ttl`").
  Once git-lex guarantees that, Pool can **delete two of the three** fallback paths.
- **Sequencing:** git-lex first (stabilize the path) → then Pool simplifies. Doing
  Pool first (e.g. "just hardcode the canonical path") would re-break the moment
  git-lex moves again. Relates to git-lex **C5** (two SHA-length ontology roots — same
  family of "git-lex hasn't committed to one layout").

### EDGE-2 🟡 — Pool's `sync_from_soul_repo` inherits git-lex's identity ambiguity
- **Where:** `pool/src/lib.rs:1051` builds Pool's named-graph IRI from the soul-repo's
  `.lex/identity.yml` `genesis_sha`, reading `<soul_repo>/.git/lex/oxigraph`.
- **True cause:** git-lex **C6** — `is_valid_sha` accepts short SHAs, so a soul can
  have two base IRIs (short in repo.yml, full in identity.yml). If Pool reads the
  *other* length than git-lex wrote, Pool snapshots into a **differently-named graph**
  → silent split, Pool's data orphaned from the soul's.
- **Fix side: git-lex** (C6 — require exactly 40 hex for the identity anchor). Pool is
  correctly trusting `identity.yml`; the ambiguity is upstream.
- **Severity shift:** C6 was 🔴 in git-lex's own list for in-repo reasons; across the
  seam it's *also* the root of a Pool data-integrity risk. The cross-repo view
  **confirms** C6's priority — two independent reasons to fix it.

### EDGE-3 🟡 — git-lex hand-parses the SAME kit-TTL Pool consumes
- **Where:** git-lex `ontology.rs:499` `parse_class_okf_type` / `:440`
  `get_class_instantiation` hand-scan kit TTL line-by-line (C19/C3). Pool's
  `ontology.rs` parses the *same files* via SPARQL over a real store.
- **The edge:** both repos read `git-lex-kit-copia`/`-pool` ontology TTLs. git-lex
  reads them with a brittle line-scanner; Pool reads them correctly. They can
  **disagree** on what the ontology says (e.g. a multi-line class stanza, an unusual
  prefix) → git-lex and Pool would have **different views of the same kit**.
- **Fix side: git-lex ← Pool** (this is the A-table dual-parser direction, but it has
  a concrete cross-repo consequence here: a *shared contract* — the kit ontology —
  parsed two ways). Adopt Pool's store-based parse so both repos agree by construction.

---

## C. INDEPENDENT — genuinely local, do not link

Most findings. Listing them so the next reader doesn't go looking for a seam that
isn't there.

- **git-lex only:** B2 (sync-before-query), B3/B4/B5 (create-untitled, --version, exit
  codes), C24/C25 (kit-namespace hooks, orphan registrations), C7 (base_uri
  read-that-writes), C13 (finish modularization — internal), C1 (panic audit), C10
  (wrong test rationale), C14/C20 (YAML/SHACL silent excludes), C21 (date subprocess).
- **Pool only:** PB1 (no README — now stabbed), PC4 (serve index-root ignores
  pool.yml), PC5 (RW-Door + watcher), PC6 (m10 dead-weight deletion), PB2–PB5
  (layout/vector-pair/named-graph docs).

None of these touch the other repo. Fix them in their home repo on their home
schedule. **Resisting the urge to connect these is the point** — a forced edge sends a
fix to the wrong repo.

---

## D. SEQUENCING — the order the seam dictates

1. **git-lex C6** (40-hex identity anchor) — **before** any further Pool `sync_from`
   work. Unblocks EDGE-2; cheap (LOW effort in git-lex's list); removes a silent
   data-split risk that spans both repos.
2. **git-lex: commit to one kit-install path** (the EDGE-1 / C5 stabilization) —
   **before** simplifying Pool's `locate_kit_copia_ontology`. Then Pool deletes 2 of 3
   fallback paths as a follow-up.
3. **The twin debug diagnostics** (`GIT_LEX_DEBUG_TTL` + `POOL_GATE_DEBUG`) — build
   them **together, mirrored**, so the squad has one mental model for "why did my fact
   vanish." B1 + PC1 in one sitting. sylkie already filed Pool #108 for the Pool half.
4. **git-lex dual-parser refactor** (C13/C19/C3) — adopt Pool's store-based ontology
   parse. Post-Thursday, as git-lex's own list scheduled it. This *also* closes EDGE-3
   (the shared-kit-contract disagreement) for free.
5. Everything in **section C** — independent, no cross-repo ordering constraint.

---

## E. THE BIG PICTURE (what two lists showed that one couldn't)

- **git-lex is upstream of Pool, and both seams prove it.** Pool reads git-lex's store
  and git-lex's installed ontology; git-lex reads neither of Pool's. So **git-lex's
  stability is load-bearing for Pool.** Every "tolerate the spread" / "has sync been
  run?" defensive line in Pool is Pool flinching at git-lex's churn. The cleanest way
  to make Pool simpler is to make **git-lex's contract stable** — not to patch Pool.
- **Pool is the more-evolved twin** ([[project_git_lex_vs_pool_learning_history]]):
  it inherited the worked-out shape from the Python spike. So on every *shared* pattern
  (parsers, ontology reads), the fix-direction is **git-lex ← Pool**. Pool is the
  reference implementation of ideas git-lex discovered the hard way.
- **Their independent lists tell different stories, and the compare confirms why:**
  git-lex's list = "consolidate the lessons now that we know them" (finish
  modularization, unify parsers, pick one casing rule, commit to one layout). Pool's
  list = "wire the last connections + write docs." git-lex has the structural debt
  *because* it's where the learning happened — and that same debt is what leaks across
  the seam into Pool.

**One-line summary:** *Three of Pool's findings (the 3-path fallback, the sync
identity risk, the kit-parse disagreement) are git-lex bills come due. Pay them on the
git-lex side and Pool gets simpler for free.*

---

Counts: 45 source findings (34 git-lex + 11 Pool) → **4 convergent meta-roots**
(A) + **3 confirmed cross-repo edges** (B, all git-lex→Pool) + the rest independent.
All seams empirically confirmed against source on 2026-06-24.
