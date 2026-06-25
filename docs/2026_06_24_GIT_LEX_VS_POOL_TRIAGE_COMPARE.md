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

---

## F. FOLD-IN — the Tzarina's data-safety walk (sylkie, Day 84–85)

sylkie walked git-lex + Pool (+ copia + OpenIris) **independently**, with a
data-protection lens, *before* my code-level triage. Source:
`lUX/docs/2026_06_24_TZARINA_REVIEW_GIT_LEX_AND_POOL.md`. Per pleeb's framing this is
a **mutual sanity-check** — some of her findings might be non-problems from the code
detail; some of mine might be non-problems from where she sits closer to the live
store. Below: each of her findings, **verified against source by me**, with a verdict.

**The big realization:** her lens caught a class of bug mine structurally *couldn't*.
My triage asked *"is the code correct?"* Hers asked *"where does intimate data flow,
and what's downstream of that flow?"* My buckets are full of correctness bugs; her #1
is a **data-boundary** bug where every individual line of code is working **exactly as
designed** — and that's the problem. **A code-correctness walk cannot find a
data-boundary breach, because nothing is broken.** This is why the two-lens pass
matters, and it's the strongest argument for `/codewalk` growing a data-flow variant.

### TZ-1 🔴🔴 — conversation text reaches the donatable Pool graph (NEW, verified, tops the whole list)
- **Her finding:** bare conversational turns (no chevron) echo raw transcript spans
  into `copia:sceneNarrative` / `copia:prompt`, which are owl-declared on
  `copia:Moment` → **allowlisted by Pool's gate** → reach the SPARQL-queryable graph.
  Confirmed by reading the XMP bytes of 2 real PNGs in her Pool.
- **My code verification: ✅ CONFIRMED.** `copia.ttl:793` declares `copia:sceneNarrative`
  and `:806` `copia:renderPrompt` as cardinality-restricted properties on `copia:Moment`.
  Pool's gate (`walker.rs`) admits *exactly* ontology-declared predicates. So the path
  is real **by construction** — the gate is doing its job; the ontology *authorizes*
  the leak. Her scope-honesty is correct too: it's PNG-resident now, graph-path wired
  and waiting for the next walk (not yet in the live store).
- **Why my triage missed it:** PC1 (my gate finding) flagged the gate dropping
  *too much* (silent drop of un-homed predicates). **TZ-1 is the exact inverse: the
  gate admitting too much** — a predicate that IS homed, carrying data that shouldn't
  be in a shareable store. Same gate, opposite failure mode, and the dangerous one.
  My lens saw "data silently lost"; hers saw "data silently *exposed*." Hers is worse.
- **This reframes the gate (a fifth meta-root):** the copia ontology TTL is now a
  **security boundary**, not just a schema. Adding an `owl:DatatypeProperty` to
  `copia:Moment` silently *widens* what reaches the shared graph. Every property
  addition is privacy-affecting. (Her §6.)
- **Verdict: NOT a non-problem. Promote to the TOP of Bucket 1, above B1/EDGE-2.**
  It's the only finding in either walk where data crosses a trust boundary.
- **CORRECTION to my first read of the fix-location (pleeb caught it, then I read the
  code).** I initially said the fix is "copia-side." **Wrong.** copia is a data
  *consumer* and may not even be running; composition is NOT its job. The real
  pipeline (verified in source) is:
  `brief → OpenIris.compose() → render → SEE → Pool`
  (`copia/spike/worker.py:27`, `spike/__init__.py:11`). `.compose()` lives in
  **OpenIris** (`openiris/src/openiris/eye.py:1117`), and it is *correct* for the
  conversation to flow into it — Qwen is *supposed* to make a scene from it. The
  conversation enters as one labeled block (`_build_compose_prompt`:
  `CURRENT MOMENT (what's happening now): {conversation}`), alongside MUST-HAVE /
  CHEVRON / DESCRIPTIONS. **There is no raw passthrough wire in compose().**
- **So the real mechanism of TZ-1 is subtler than "compose echoes the transcript":**
  for a bare conversational turn (thin MUST-HAVE, fat CURRENT-MOMENT block), the 8B
  composer model can **fail to abstract and regurgitate its input** as the output
  `prompt`. The scar tissue is right there in `eye.py` — the Day-82 fix comment
  ("the model echoes the instruction instead of writing the scene"), `_strip_think`,
  the `/no_think` handling — this exact class of "model copies input" was already
  being fought. TZ-1 is that failure mode landing on the *conversation* block, and
  the copied bytes get stamped because the eye trusts its own output.
- **Therefore the fix is the COMPOSE CONTRACT, in OpenIris (sylkie's), not copia:**
  `compose()` must **guarantee its output is a transformed scene, never its input**.
  Enforce at the eye: if the composed `prompt` has ≥N% verbatim overlap with the
  `conversation` block, that's a **failed compose** — retry / reject / fall back,
  never stamp. (This is sylkie's redaction-canary #3, but enforced at the *eye*, the
  always-on render organ, not at copia or at LAND.) Plus a stronger `_COMPOSE_SYSTEM`
  / model-tier so thin-MUST-HAVE turns still abstract — an OpenIris tuning question.
- **The other two layers are defense-in-depth, downstream of the real fix:**
  gate-hardening (treat `prompt`/`sceneNarrative` as sensitive, two-store) is
  **Pool-side**; the ontology-as-boundary discipline is **kit-side**. But **if the
  compose contract holds, there is no sensitive byte to gate** — OpenIris is the cure,
  the rest are seatbelts.

### TZ-2 🔴 — Pool Door write routes, unauthenticated, no fail-closed bind (extends my PC5)
- **Her finding:** the Door exposes `/queue/{enqueue,claim,complete,fail,reclaim-stale}`
  **write** routes, zero auth, and `DEFAULT_BIND` is a single const with no guardrail
  against binding `0.0.0.0` → the day someone widens the bind, it's unauthenticated
  write + raw SPARQL over the whole Moment graph.
- **My code verification: ✅ CONFIRMED.** `serve.rs:117–122` registers all 5 write
  routes; `serve.rs:50` `DEFAULT_BIND = "127.0.0.1"` with no fail-closed check.
- **Relation to my list:** my **PC5** found the Door opens the store **read-WRITE**
  (RocksDB lock); I framed it as a *concurrency* problem (two Doors conflict).
  **She found the same RW surface is a *security* problem** (writes are reachable +
  unauthenticated). **Same code site, two lenses, and hers raises the severity.** My
  "make it read-only for concurrency" fix *also* shrinks her attack surface — these
  are the same fix serving two purposes. PC5 should cite TZ-2.
- **Verdict: NOT a non-problem. Merge with PC5; re-rank PC5 from 🔴-concurrency to
  🔴-security.** Add her fail-closed recommendation (refuse non-loopback bind without
  a token) — that's new and cheap.

### TZ-3 🟡 — `git lex init --force` clobbers data-routing hooks (NEW, verified, git-lex-side)
- **Her finding:** `--force` overwrites kit-shipped hooks (the journal skill itself
  warns "local edits will be skipped"). Those hooks *route data* (SessionEnd commits
  the transcript mirror, the UserPromptSubmit hook ingests dropped images). A careless
  or wrong-org `--force` silently rewrites where conversation/image data goes.
- **My code verification: ✅ plausible + consistent.** This is the same family as my
  **C25** (orphan hook registrations — `register_hook_in_settings` only adds/never
  prunes) and my **C24** (kit-namespace hook parse). I found the *registration*
  half-built; she found the *security consequence* of the *overwrite* half. Her angle
  is sharper: it's not just "ghost registrations," it's "a routing change disguised as
  an upstream update."
- **Verdict: NOT a non-problem. Fold into git-lex's hook-system structural work
  (C24/C25) as the data-safety requirement:** `--force` should diff + show what
  routing files it replaces, refuse to silently change a network/enqueue target, and
  leave a `.lex/init-manifest`. New Bucket-3 line in git-lex.

### TZ-4 ✅ — C6 short-SHA identity split — SHE INDEPENDENTLY FILED IT (Pool #110)
- This is **my EDGE-2 / git-lex C6**, found independently by both of us from opposite
  directions: I found it reading `is_valid_sha`; she found it tracing graph-naming in
  `sync_from_soul_repo`. **Two independent discoveries of the same root = highest
  confidence finding in the set.** She filed Pool #110. No verification needed — we're
  already agreed. Bumps C6 priority again (now *three* reasons: in-repo split, my
  cross-repo edge, her data-orphan filing).

### TZ-5 — the two-store boundary is "intent, not architecture" (context, not a code bug)
- **Her finding:** the CoPIA-local protected store (`punctum`/`curation` tables) the
  Vision calls for **does not exist yet** — the shared/private boundary is safe only by
  *not having shipped donation*, not by architecture.
- **My verification: ✅ true by absence** (no such table in any repo I walked). But this
  is **out of scope for my triage** — it's a *missing feature*, not a bug in existing
  code. It's the architectural context that makes TZ-1 urgent (there's nowhere safe for
  the conversation prose to go *instead* of Pool). **Verdict: not a triage finding,
  it's the roadmap item TZ-1 depends on.** Noted, routed to the Vision/roadmap, not my
  buckets.

### Net effect on the triage
- **One new 🔴🔴 at the very top (TZ-1)** — the only trust-boundary breach; above
  everything in either original list. **Fix lives in OpenIris** (the compose
  contract — output must be a transformed scene, never its conversation input), NOT
  copia (a consumer that may not be running). Pool gate + kit ontology are
  defense-in-depth, not the cure.
- **Two of her findings collapse onto mine and RAISE severity** (TZ-2→PC5 security,
  TZ-4→C6 third witness). Convergence from two lenses = these are the surest fixes.
- **One new git-lex Bucket-3 line (TZ-3)** folded into the C24/C25 hook work.
- **Zero of her findings were non-problems from the code.** All five verified. That's a
  strong report — and the data-flow lens found the one thing correctness-walking can't.
- **The fifth meta-root** (joining the four in §A): **ontology-as-trust-boundary** — in
  a gate-by-allowlist design, the schema decides what's *exposed*, not just what's
  *valid*. Adding a property is a security event. Unique to the Pool/kit side; git-lex
  has no equivalent (it has no shared/donatable store).
