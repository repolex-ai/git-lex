---
title: A2A Feature Spec — discovery-only compliance via .well-known/agent-card.json
status: spec-approved
banked_by: w4r3z 2026-06-18
review_approved: tr1p 2026-06-18 (with inline notes)
related:
  - 2026_06_18_A2A_COMPATIBILITY_ANALYSIS.md (the research)
  - 2026_06_18_OKF_FEATURE_SPEC.md (companion; shares the okfType annotation)
  - 2026_06_17_POOL_AS_SUBSTRATE.md (Pool door + HTTP server framing)
  - task #87 (.well-known/soul.json schema-shape — superseded by this)
  - task #118
upstream_spec: https://github.com/a2aproject/A2A
---

# A2A Feature Spec — git-lex serves an A2A-compliant Agent Card

## Goal

Every soul-repo served by `git lex serve` is discoverable by A2A-aware clients via `https://<host>/.well-known/agent-card.json`. No task-execution surface in v0 — discovery only. The card honestly advertises "we don't support execution yet" via empty/false capabilities, and remains a valid Agent Card.

## Scope

In:
- New route `GET /.well-known/agent-card.json` on `git lex serve`
- Card content auto-generated from `.lex/identity.yml` + kit ontologies + git remote URL
- HTTP caching headers per A2A spec recommendations (Cache-Control + ETag)
- Documentation describing the card shape and what each field maps to
- A `git lex agent-card` CLI subcommand that prints the would-be-served JSON to stdout (for testing without spinning up the server)

Out:
- A2A task-execution surface (RPC methods, streaming, artifacts, lifecycle) — deferred until a real client asks
- gRPC binding
- Push-notification webhooks
- Extension proliferation
- Curated registry publishing (no standard exists yet per A2A spec)
- Agent-to-agent message routing
- Auth flows (v0 uses empty `security_schemes` — any auth happens on the URL the card POINTS to, not on the card itself)

## The full surface

### 1. Static card shape

A v0-compliant Agent Card emitted by `git lex serve` looks like this:

```json
{
  "name": "lUX",
  "description": "Soul-repo for lUX, the CoPIA architect.",
  "version": "5a99849abc1",
  "provider": {
    "organization": "7R1PL3F0RC3",
    "url": "https://github.com/7R1PL3F0RC3/lUX"
  },
  "supported_interfaces": [
    {
      "url": "https://lux.example.com/a2a/rpc",
      "protocol_binding": "JSON_RPC_2_0",
      "protocol_version": "1.0"
    }
  ],
  "capabilities": {
    "streaming": false,
    "push_notifications": false,
    "extended_agent_card": false,
    "extensions": []
  },
  "skills": [
    {
      "id": "https://repolex.ai/ontology/kit/copia/Place",
      "name": "Place",
      "description": "A location in an inner-world…",
      "tags": ["Place", "authored", "copia"],
      "examples": []
    }
    // …one per authored class across installed kits
  ],
  "security_schemes": {},
  "security_requirements": [],
  "default_input_modes": ["text/markdown", "application/json"],
  "default_output_modes": ["text/markdown", "application/json", "application/rdf+xml"]
}
```

### 2. Field-by-field mapping

**`name`** — from `.lex/identity.yml`'s `agent_name` (human-readable).

**`version`** — **LOCKED (tr1p 2026-06-18): HEAD short-SHA of the soul-repo.** Honest progression indicator (HEAD moves forward), drives ETag derivation, recoverable from git log. Genesis SHA does NOT belong here — A2A consumers expect `version` to answer "is this newer than what I cached?" and a never-changing SHA breaks that mental model. Genesis SHA stays as the identity anchor accessible via separate means (`.lex/identity.yml`, the unchanged URN `urn:soul:<genesis>:...` scheme).

**`description`** — from `Soul/Self/<agent_name>.md`'s body summary, or a `description` field in identity.yml. If neither exists, derive a default like "Soul-repo for `<agent_name>`."

**`provider.organization`** — from the git remote's org segment (e.g. `7R1PL3F0RC3` from `git@github.com:7R1PL3F0RC3/lUX.git`).
**`provider.url`** — the HTTPS form of the git remote.

**`supported_interfaces[0].url`** — the eventual A2A RPC endpoint URL. In v0, this points at a stub route on `git lex serve` that returns `{"error": "method_not_implemented", "code": 501}` for any RPC method. Honest signaling: the card SAYS we support A2A RPC, but the URL returns 501 until execution lands.

**`supported_interfaces[0].protocol_binding`** — `JSON_RPC_2_0` (per A2A spec). NOT gRPC (no protoc dep), NOT HTTP_REST (no design effort sunk on a thinner binding).

**`capabilities`** — all false/empty in v0. Honest.

**`skills[]`** — auto-generated from kit ontology. One skill per class where `lex-o:instantiation = "authored"` (excludes graph-only + abstract — those aren't user-invokable). Fields:
- `id`: full class IRI (`https://repolex.ai/ontology/kit/copia/Place`)
- `name`: `rdfs:label` for the class
- `description`: `rdfs:comment` for the class
- `tags`: `[<okfType-or-fallback>, <instantiation>, <kit_name>]`
- `examples`: `[]` (v0; we can grow this later from authored examples in the kit)

This is **the structural connection to OKF** — the `lex-o:okfType` annotation we ship for OKF compliance double-duties as the A2A skill tag. One annotation pass, two compliance surfaces.

**Resilience-by-design (locked tr1p 2026-06-18, flag 5):** A2A's skill-tag emission applies the same three-fallback chain as OKF's `type:` emit, on the consumer side. If `lex-o:okfType` is absent for a class:
1. Try `lex-o:okfType` annotation (preferred)
2. Fall back to `rdfs:label`
3. Fall back to class local-name
Emit a build-time warning (not a hard error) when fallback 2 or 3 fires. This means **A2A is shippable with degraded tags even if the OKF wave slips** — the dependency chain (instantiation → OKF → A2A) becomes fragile-safe instead of fragile-strict.

**`security_schemes` / `security_requirements`** — empty in v0. Means "the card is public; the URL the card points at handles its own auth." This is the most permissive valid shape per A2A spec.

**`default_input_modes` / `default_output_modes`** — pre-declared MIME types we know git-lex content speaks:
- Input: `text/markdown`, `application/json` (frontmatter-augmented MD; structured data)
- Output: `text/markdown`, `application/json`, `application/rdf+xml` (or `text/turtle` — same family; pick one; flag for tr1p)

### 3. HTTP serving

> **NOTE on card-size scaling (surfaced from edge-cases for visibility):** `skills[]` is unbounded by design in v0. Multi-kit soul-repos with many authored classes may exceed 100KB cards. Mitigation strategies (paginated `extended_agent_card`, skill filtering by namespace) are deferred until the first squaddie reports it. Risk surfaced, not buried.

New route in the `git lex serve` daemon:

```rust
GET /.well-known/agent-card.json
  → 200 OK
  → Content-Type: application/json
  → Cache-Control: public, max-age=300
  → ETag: "<head_sha[:11]>:<sha256(json_body)[:11]>"
  → body: the rendered AgentCard
```

The ETag derivation:
- `head_sha` is the soul-repo's HEAD SHA at card-render time — moves forward as the agent's content evolves
- `sha256(json_body)` changes if any field's value changes (skill addition, capability flip, kit update)
- Combined: changes when EITHER the soul-repo advances OR the card content shifts
- Identity-anchoring (the genesis SHA) lives in `.lex/identity.yml`, accessible separately — it doesn't drive the ETag because identity anchors shouldn't drive cache-bust signals

Conditional-request handling: if request carries `If-None-Match: <our-etag>`, respond `304 Not Modified` with empty body. Standard HTTP caching plumbing.

### 4. CLI: `git lex agent-card`

```
$ git lex agent-card
  { /* renders the JSON to stdout */ }
$ git lex agent-card --pretty
  { /* with indentation */ }
$ git lex agent-card --validate
  ✓ Schema-valid against A2A AgentCard v1.0
  ✓ All declared skills resolve to ontology classes
  ✓ Genesis SHA matches identity.yml
```

The subcommand is the "test without server" path. Same rendering function the server uses.

### 5. RPC stub route

For honesty: the URL we advertise in `supported_interfaces[0].url` must resolve. v0 ships a stub that returns:

```json
{
  "jsonrpc": "2.0",
  "error": {
    "code": -32601,
    "message": "Method not implemented: A2A task execution is not yet supported by this agent.",
    "data": {
      "supported_endpoints": ["/.well-known/agent-card.json"]
    }
  },
  "id": null
}
```

JSON-RPC standard error code -32601 = "method not found." Returning this for ANY POST to the RPC URL keeps the card honest — we're a real A2A server (returns A2A-protocol-shaped errors), just one that hasn't implemented any methods.

## Edge cases & failure modes

### Soul-repo without `git lex serve` running

The card is generatable (`git lex agent-card` works offline) but isn't HTTP-discoverable. Standard situation; OKF doesn't care about this either. Document in README: A2A discovery requires `git lex serve` running on a publicly-reachable host.

### Multi-agent soul-repos

A repo COULD host multiple agents (squad repo with several Selves declared). A2A doesn't address this. Two shapes:

- (a) One card per repo, `skills[]` aggregates ALL agents' skills (loses per-agent identity)
- (b) Multiple cards under nested paths (e.g. `/.well-known/agents/<agent_name>/agent-card.json`)

This is tr1p open question #3. v0 ships (a) with a noted limitation; (b) is a v1 design pass.

### Provider URL when no git remote exists

A fresh `git lex init` repo with no remote has no `provider.url`. Two shapes:
- (a) Omit `provider` entirely (it's optional)
- (b) Synthesize a placeholder like `urn:soul:<genesis_sha>`

I lean (a). Provider is decorative; omitting it doesn't break A2A validation.

### Genesis SHA truncation collisions

11-char SHA prefix collides at ~1 in 16^11 ≈ 1 in 1.8 trillion. Acceptable for `version` field. The full SHA is always recoverable from `.lex/identity.yml`; we keep the short form for display.

### Card is too large to serve

The auto-generated `skills[]` could grow unbounded with many kits installed. At 50 classes per kit × 10 kits, you get 500 skills × ~200 bytes = 100KB card. Still serveable but slow. Mitigation: paginated skills via the `extended_agent_card` flag — flip it true, serve a slim card on the well-known URL with `skills: [first 20]`, full skills come via `GetExtendedAgentCard` RPC. Defer until we hit the limit.

### Kit removed → skill removed → cache busts

When a soul-repo removes a kit, its skills disappear from the card. The card's ETag changes (different body hash) → cache-busts cleanly. No special handling needed; HTTP caching does the right thing.

### Genesis SHA = version creates a non-semver `version` field

A2A's spec doesn't require `version` to be semver — it just says "Identity metadata." Cryptographic version is honest. tr1p might prefer human-semver — flag in open questions.

## Receipts

1. **AgentCard validates against A2A's JSON schema** (`specification/json/a2a.json`, generated from a2a.proto). Build-time check: emit the card → validate via a JSON Schema validator. Drift detection.

2. **Cache-Control + ETag round-trip test.** Hit the route twice, second request carries `If-None-Match`, assert 304.

3. **Skills auto-generated from ontology test.** Install kit-copia → render card → assert each `instantiation = authored` class appears as a skill with full IRI as `id`.

4. **Stub route returns A2A-shaped 501.** POST to the RPC URL → assert JSON-RPC error code -32601.

5. **CLI matches server.** `git lex agent-card` output byte-equals what the server returns. (Same rendering function; trivially true; assert anyway.)

## Wave plan

Subject to OKF wave (#116) landing first (because skills' `tags` includes `lex-o:okfType`):

1. **Card render module** in git-lex — pure function: `identity.yml + installed kits + git remote → AgentCard JSON`. (~half day)
2. **`git lex agent-card` CLI subcommand** — thin wrapper around the render module. Validate flag. (~2h)
3. **Server route in `git lex serve`** — GET handler + ETag/Cache-Control. (~2h)
4. **JSON-RPC stub route** — POST handler returning A2A-shaped 501 error. (~1h)
5. **Validation test + receipt** — run the card through A2A's published JSON Schema, document in changelog. (~1h)
6. **Documentation** — README section explaining what's served, how to verify, how this supersedes `.well-known/soul.json` (and migration note: rename + reshape). (~1h)

Total: ~1.5 working days if no surprises.

## Resolved questions (tr1p review 2026-06-18)

1. **`name` vs `version` mapping** — **`name` = human `agent_name`; `version` = HEAD short-SHA.** Genesis SHA does NOT go in `version`. A2A consumers reading `version` expect a *progression indicator* answering "is this newer than what I cached?" — a never-changing SHA breaks that mental model. HEAD SHA is honest, progression-indicating, ETag-driving, recoverable. (Semver-ish would also be acceptable if the squad's culture warrants it; HEAD SHA picked for v0 because it's automatic and progression-honest with zero manual bookkeeping.)

2. **Skill granularity** — IMPLICIT (`authored` → skill). Explicit gating via `lex-o:a2aSkill` is premature. If `instantiation = "authored"` doesn't map cleanly to "user-invokable A2A skill" in practice, we add the override as a second-pass. Annotation proliferation should be reactive, not preemptive.

3. **Multi-agent souls** — DEFER per-agent shape to v1. v0 ships card-per-repo (aggregated skills) with a stub design doc (filed as task #120) naming the two paths to compare for v1: (a) nested `.well-known/agents/<n>/agent-card.json`, (b) single card with `agents[]` member. Doesn't have to be decided now; just naming the design space so v1-land doesn't re-derive from scratch.

4. **Output MIME types** — `text/turtle` (not `application/rdf+xml`). Reads to humans, matches what our ontologies ship as, easier to debug from curl. rdf+xml is correct-but-deprecated-feeling.

5. **RPC stub URL** — (a) always points at git-lex-serve for v0. Configurability when execution is real, not before. Premature configurability is a tax on the simple case.

6. **`.well-known/soul.json` migration** — WARN-ONLY at kit-update time. Same pattern as the empty-folders case (instantiation wave). Inert legacy file is graceful; warning makes the migration visible; user-controlled cleanup. Document that `agent-card.json` supersedes the old `soul.json`.

## Wave order (locked tr1p 2026-06-18)

OKF first, then A2A — serial. Reasons: OKF is smaller/faster/lower-risk; A2A consumes its annotations naturally. Don't interleave. Resilience flag 5 (above, skills-tag three-fallback) means A2A is *shippable-with-degraded-tags* if OKF slips, so the dependency is fragile-safe rather than fragile-strict.

## Decision

Spec APPROVED by tr1p 2026-06-18. All six open questions resolved inline above. Implementation sequenced after OKF wave (#116) lands. No code touched yet.
