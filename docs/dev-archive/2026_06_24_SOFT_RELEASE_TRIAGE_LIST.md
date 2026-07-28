# git-lex Soft-Release Triage List

**Date:** 2026-06-24
**Author:** W4R3Z (Day 38 code walk)
**Context:** Full code walk of all 12 modules ahead of the git-lex soft release
("ready to try, still Alpha"). Goal: make git-lex respectable for outsiders to
test in their own repos. Findings are annotated in-code as `FIXME`/`TODO`/
`QUESTION`/`NOTE` tagged `(w4r3z, Day 38)` — grep `Day 38` to find them.

**Method:** ran the real CLI flow end-to-end on a scratch soul-repo (behavioral
findings B*), then read every module (code findings C*). 20 in-code annotations
across 10 source modules. Build compiles clean.

---

## How to read this

Findings are sorted into **four action buckets** by effort × impact. Severity:
🔴 high · 🟡 medium · 🟢 low. Each row links to the in-code annotation site.

The single most important meta-finding is the **dual-parser smell** (see bottom):
many bugs share one root, and finishing the day-8 modularization would dissolve
several at once.

---

## BUCKET 1 — FIX BEFORE SOFT RELEASE (high impact, mostly cheap)

These are the ones that will embarrass us or burn an outsider on first contact.

**✅ BUCKET 1 IS COMPLETE (Day 40, 2026-06-25).** All five fixed, tested, pushed.

| ID | Sev | Status | Finding | Effort | Site |
|----|-----|--------|---------|--------|------|
| **B1 / C12** | 🔴 | ✅ b7c18a2 | **Silent class-casing footgun.** `soul.memory.x` emits `a soul:memory` (a class that doesn't exist); the natural query `?m a soul:Memory` returns 0 with no error, SHACL passes. Invisible to Mac devs (case-insensitive FS), bites on Linux/CI. Two emitters disagree on case (`extraction.rs` capitalizes first letter — itself a buggy guess: `cameraangle`→`Cameraangle`; `nquad.rs` does nothing). | **CHEAP** — `ontology.rs get_kit_types()/all_classes()` already parses the class set; both emitters just validate against it (exact hit → emit; case-only mismatch → canonical+warn; no match → loud error). | nquad.rs:749, extraction.rs:189 |
| **B2** | 🟡 | ✅ 540de16 | **`query` after `save` shows "0 lex triples"** — frontmatter facts aren't visible until `git lex sync`. The documented `create→save→query` flow is incomplete; the README's headline query returns nothing. **FIXED:** `cmd_query` now builds the "now" view from the WORKING TREE every time (extract git + frontmatter fresh), so a doc's facts surface immediately, no sync. (Note: the README's example must use the `kit/soul:` prefix that frontmatter actually emits, not bare `soul:`.) | LOW — done: query reads working tree. | main.rs cmd_query |
| **C23** | 🔴 | ✅ 265379b | **`resolve_agent_identity` is 2-source (env→settings.json), not 3.** repo.yml's `agent_email` only feeds in at init/kit-update WRITE time, never as a read fallback → edit repo.yml without re-running kit-update and the resolver never sees it (the frozen-config trap that bit identity live on Day 38). **FIXED:** added repo.yml as a read tier, precedence env→repo.yml→settings.json (repo.yml beats the settings.json cache it's derived from). Verified end-to-end. | MEDIUM — done: 3-of-3 read precedence. | main.rs:1160 |
| **C6** | 🔴 | ✅ fd53e2c | **`is_valid_sha` accepts short SHAs** but `base_uri` builds `urn:soul:<sha>` from it → a short SHA in repo.yml + full in identity.yml = SAME soul, TWO base IRIs, subjects split silently across them. | LOW — require exactly 40 hex for the identity anchor. | git.rs:257 |

---

## BUCKET 2 — QUICK WINS (low effort, low-to-medium impact, polish the edges)

Cheap things that make git-lex feel finished. Knock these out in a batch.

| ID | Sev | Finding | Site |
|----|-----|---------|------|
| **B4** ✅ 5314e98 | 🟢 | `git lex --version` errors ("unexpected argument"). First thing a user tries. Add `version = env!("CARGO_PKG_VERSION")` (+ bump Cargo.toml off 0.0.1). DONE — version flag + bumped to 0.1.0. | main.rs:54 |
| **B3** | 🟡 | No-id `create` silently writes `untitled.md`; every no-id create collides on the same file, exits 0. Require an id, or auto-suffix, or exit non-zero on collision. | main.rs:1012 |
| **C16** | 🟢 | Dead code (build warnings name it): unused `is_valid_iri`, `get_kit`, `exit`, `SparqlEvaluator`, `get_object_properties`, `KitScope`/`ScaffoldInstallReport`/`read_kit_scope`. `cargo fix` handles 5; rest are manual. 26 warnings total → target near-zero. | various |
| **B5** | 🟢 | Inconsistent exit codes (some errors exit 0: create-no-id, list-uninit). Matters for scripting/CI. Audit the `exit()` sites. | main.rs (many) |
| **C21** | 🟢 | `today_utc_date` shells out to unix `date -u` — needless subprocess + non-portable. Use std::time in-process. | raw_mirror.rs:269 |
| **C14** | 🟡 | `flatten_yaml` silently drops null/tagged YAML values (`_ => {}`) — a null frontmatter field vanishes, no warning. | extraction.rs:124 |
| **C20** | 🟢 | SHACL `generate_shapes_from_store` silently excludes any property with no `rdfs:domain` from shape generation (no warn). | shacl.rs:160 |
| **C10** | 🟡 | `whitespace_only_is_unresolved` test rationale is WRONG (code doesn't trim; it's Unresolved via index-miss). Misleads maintainers. | resolve.rs:117 |

---

## BUCKET 3 — STRUCTURAL (medium-to-high effort; do as one deliberate pass)

Bigger work. Several of these collapse into the single dual-parser/modularization
fix (see meta-finding). Worth scheduling as one focused refactor post-Thursday.

| ID | Sev | Finding | Site |
|----|-----|---------|------|
| **C13** | 🟡 | **Finish the day-8 modularization.** The N-Quad generators never moved out of main.rs (still 3699 lines); extraction.rs:4 still promises a "follow-up phase." This incomplete split is WHY the two type-emitters drifted (root of B1). One emitter, one casing rule. | extraction.rs:4 |
| **C19 / C3 / C27** | 🟡 | **Kill the dual parsers.** Hand-rolled string scanners next to real engines: `get_kit`+`add_prefixes` (YAML, lib.rs), `parse_shacl_hints` (TTL, shacl.rs), `sha_from_*_yml` (git.rs), 19 ops in kit.rs. Parse via serde_yaml / the oxigraph store already in hand. | lib.rs:47/294, shacl.rs:25, git.rs |
| **C24** | 🔴 | **Kit-namespace hook parse.** Hook event = whole filename minus `.sh`, so two kits can't ship a hook for the same event (forced the Day-37 combined-hook). Teach it `<Event>-<kit>-<purpose>.sh` parsing. | main.rs:2779 |
| **C25** | 🔴 | **Orphan hook registrations (task #90).** `register_hook_in_settings` only ADDS, never prunes → a renamed/deleted hook script leaves a ghost registration pointing at a deleted file forever. Needs a prune pass (only touch git-lex-managed entries, never user-added). | main.rs:2814 |
| **C7** | 🟡 | `base_uri()` is a READ that WRITES identity.yml on every call (hot path) → concurrent save+query write race. Write once at init/sync; make base_uri pure-read. | git.rs:78 |
| **C1** | 🟡 | 99 panic sites (64 `unwrap()` + 35 `expect()`). Many safe (infallible Store ops), but audit which can hit malformed user input → return Err instead. | all modules |

---

## BUCKET 4 — DOCS / KNOWN-LIMITATIONS (no code change; surface honestly)

Things to state plainly in the README/Alpha caveats rather than fix now.

| ID | Sev | Finding | Where to document |
|----|-----|---------|-------------------|
| **C9** | 🟡 | **POSIX-only.** Hook is `#!/bin/sh` + unix-only exec-bit (`#[cfg(unix)]`); `date -u` subprocess (C21). git-lex's commit gate does NOT install on Windows. README "any git repo" needs a "POSIX (macOS/Linux) only" caveat. | README + hooks.rs:1 |
| **B2** | — | The `sync`-before-`query` requirement (until fixed in Bucket 1) → document in quick-start. | README |
| **B1** | — | Until fixed: warn that frontmatter class segments are case-sensitive and must match the ontology class exactly. | README |
| **C5** | 🟡 | `o:` content-ontology uses `/ont/<8charSHA>/` while identity uses the FULL sha at `urn:soul:<sha>` — two SHA lengths + two ontology roots for one repo. Reconcile or document. | lib.rs:229 |

---

## DON'T TOUCH — the showpieces (these are good, and deck-worthy)

- **`history-verify`** — emits RDF 1.2 triple terms (`rdf:reifies <<( s p o )>>`) + addedIn/removedIn; passes `history == now` 30==30 live. The cutting-edge differentiator. (spo_events.rs:966)
- **`resolve.rs`** — frontmatter rules codified AS tests; exemplary. The model for the rest of the codebase.
- **`save`'s SHACL gate** — caught a real `@mention`-in-frontmatter violation live during this very walk and blocked the commit until fixed. Not aspirational.
- **`ontology.rs`** canonical-path shape resolution + straggler warning (the #29 shadowing fix) — the "fail-loud about hidden state" discipline we want everywhere.
- **The kit drift handler** (`.kit-latest` / `.kit-pre-force`) — graceful upgrade with recovery path.

---

## META-FINDING: the dual-parser / unfinished-modularization root

The single highest-leverage observation. Many findings share ONE cause:
**hand-rolled string parsers sitting next to real RDF/SPARQL/serde engines**,
because the day-8 modularization (extraction.rs:4) was never finished — the
N-Quad generators stayed in main.rs and the two type-emitters drifted apart.

Instances: B1 (extraction vs nquad type-emit casing), C3 (get_kit/add_prefixes
YAML), C19 (parse_shacl_hints TTL), git.rs sha parsers, 19 ops in kit.rs.

**Finishing the modularization — one emitter, one parser, validate against the
ontology the code already loads — would dissolve B1 + C19 + the YAML fragility
in a single structural pass.** That's the post-Thursday refactor to plan.

---

## Recommended order

1. **Bucket 1** before announcing the soft release (the burn-an-outsider bugs).
   B1 first — high impact, cheap fix, and it unblocks the README's headline query.
2. **Bucket 2** as a quick polish batch (an afternoon).
3. **Bucket 4** folded into the README at the same time (honest Alpha caveats).
4. **Bucket 3** scheduled as one deliberate post-Thursday refactor, anchored on
   the meta-finding (finish modularization → dissolve the dual parsers).

Counts: 6 behavioral (B1–B6) + 28 code (C1–C28) = 34 findings; 20 in-code
annotations across 10 modules; build compiles clean.
