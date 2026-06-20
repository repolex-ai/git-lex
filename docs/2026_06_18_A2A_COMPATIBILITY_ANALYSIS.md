---
title: A2A Compatibility Analysis (minimal-compliance shape for git-lex)
status: research
banked_by: w4r3z 2026-06-18
upstream_spec: https://github.com/a2aproject/A2A
related:
  - task #87 (.well-known/soul.json schema-shape, the proto we already ship)
  - task #116 (OKF wave — adjacent but distinct concern)
---

# A2A Compatibility Analysis

## Headline

**A2A's discovery layer is a near-drop-in for our `.well-known/soul.json` work.** Rename the file, align a few field names, and we're discoverable by every A2A client without taking on the rest of the protocol. The full task-execution side of A2A is much bigger and we should skip it until a real consumer asks.

A2A was donated to the Linux Foundation by Google. It's not Google-locked.

## What A2A actually is

A2A (Agent2Agent) is an open standard for **agent-to-agent interoperability** across frameworks (LangGraph, CrewAI, ADK, Genkit, AG2, BeeAI, PydanticAI, et al). The protocol has THREE bindings:

- **JSON-RPC 2.0** over HTTP / SSE (the "default" binding; what most servers ship)
- **gRPC** (protobuf-native; the canonical spec is `a2a.proto`)
- **HTTP/REST** (a thinner mapping for clients that want plain REST)

The two halves of the protocol are:

1. **Discovery** — how clients find agents, learn their capabilities, and decide if they're worth talking to. This is the Agent Card + `.well-known/agent-card.json`.
2. **Task execution** — how clients send work to agents, get streaming updates, and receive artifacts. This is the bulk of the protocol (RPC methods, Task lifecycle, Messages, Parts, Artifacts).

These two halves are decoupled. You can implement discovery without implementing execution. **This is the key for us.**

## Discovery (the cheap half)

### File location

`https://{domain}/.well-known/agent-card.json` — RFC 8615 well-known URI. Same RFC as our `.well-known/soul.json`. Same shape, different filename.

### Agent Card fields (from the spec)

```jsonc
{
  // Identity metadata
  "name": "string",
  "description": "string",
  "version": "string",
  "provider": { /* optional org info */ },

  // Service endpoint(s) where execution happens
  "supported_interfaces": [
    {
      "url": "string",
      "protocol_binding": "JSON_RPC_2_0" | "GRPC" | "HTTP_REST",
      "protocol_version": "string"
      // "tenant" (optional, for multi-tenant agents)
    }
  ],

  // Capability flags
  "capabilities": {
    "streaming": bool,
    "push_notifications": bool,
    "extended_agent_card": bool,
    "extensions": [ /* protocol extensions this agent advertises */ ]
  },

  // What the agent can DO
  "skills": [
    {
      "id": "string",
      "name": "string",
      "description": "string",
      "tags": [ "string" ],
      "examples": [ "string" ]
    }
  ],

  // Auth requirements (OpenAPI-style)
  "security_schemes": { /* OpenAPI security schemes */ },
  "security_requirements": [ /* refs to above */ ],

  // MIME types accepted/produced by default
  "default_input_modes": [ "string" ],
  "default_output_modes": [ "string" ]
}
```

### What's actually required vs optional

The spec marks much of the card optional — you can ship a card with just `name`, `description`, `version`, and one `supported_interfaces` entry pointing nowhere meaningful. That's still a valid Agent Card. The `skills` list is the only "useful content" — clients filter agents by skill tags.

### Discovery strategies (3 levels)

1. **Well-known URI** (recommended, RFC 8615) — what we already do
2. **Curated registries** — A2A doesn't specify a registry API yet; deferred to community
3. **Direct configuration** — hardcoded URLs; for private/dev setups

The spec is clear that strategy 1 is the default for public/domain-controlled agents. That's us.

### HTTP caching

The spec gives explicit caching guidance: `Cache-Control: max-age=...` + `ETag` derived from card `version` or content-hash. Clients honor conditional requests. We get this for free if we serve with a static-file server pattern.

## Task execution (the expensive half — skip for now)

The execution surface is substantial. RPC methods (over JSON-RPC, gRPC, or REST):

- `SendMessage`, `SendStreamingMessage`
- `GetTask`, `ListTasks`, `CancelTask`, `SubscribeToTask`
- `GetExtendedAgentCard`

Data shapes:

- `Task` (id, context_id, status, artifacts, history, metadata)
- `TaskState` enum (SUBMITTED → WORKING → COMPLETED|FAILED|CANCELED|REJECTED, plus INPUT_REQUIRED / AUTH_REQUIRED interrupted states)
- `Message`/`Part` (text, raw bytes, url, or JSON data with media_type + filename)
- `Artifact` (a tangible task output: collection of Parts + metadata)
- Streaming events: `TaskStatusUpdateEvent`, `TaskArtifactUpdateEvent`

**Cost of implementing execution:** at minimum, you need:
- HTTP server hosting JSON-RPC (or gRPC, or REST) endpoint
- Task state machine + persistence
- Streaming layer (SSE or chunked HTTP)
- Auth gates per the security schemes you declared
- Artifact storage + addressing

For us, this is a 1-3 week project depending on streaming surface, and the immediate ROI is zero (we don't have remote A2A clients trying to call us). **Skip until demand is real.**

## Google baggage to skip

Reviewed the docs for vendor-coupling. What's actually clean (not Google-locked):

- ✅ Protocol is Linux Foundation, not GCP
- ✅ JSON-RPC 2.0 binding requires no Google tooling
- ✅ Reference implementations span 5 languages, not just Google's
- ✅ Auth is OpenAPI security schemes (standard) — bearer token, OAuth2, mTLS, etc.

What we should avoid:

- ❌ **`a2a-sdk` (Python)** and the language SDKs in general. They're convenient but pull in dependencies and tie us to upstream cadence. We write our own minimal handler (which is what every serious adopter ends up doing anyway).
- ❌ **gRPC binding.** Unless we have a real client asking, gRPC adds a build-time dependency (protoc, generated stubs) for zero immediate benefit. JSON-RPC binding is sufficient.
- ❌ **Extension proliferation.** A2A has an "extensions" mechanism (`capabilities.extensions` and per-message extensions). Useful eventually; noise to deal with at v0.
- ❌ **Push-notification webhooks.** The spec supports server-initiated webhooks for task updates. Real protocol; not on our critical path until we ship task execution.
- ❌ **`a2a-and-mcp.md`** — there's an MCP (Model Context Protocol, the Anthropic spec) integration doc. Worth reading later for tool-protocol synergy; not on the discovery path.

## Recommended minimal-compliance shape for git-lex

**The cheapest A2A-compliant shape:**

1. **Rename `.well-known/soul.json` → `.well-known/agent-card.json`** and reshape the fields to match the A2A Agent Card schema. Keep our richer `soul:` fields under an `extensions` block or alongside as additional properties (JSON-Schema 2020-12 allows additional properties).

2. **Serve from `git lex serve`.** This becomes the natural home — substrate-layer HTTP daemon (already there for graph viz, planned for /api/query promotion per #76 and Pool routes per #110/#111). One more route: `GET /.well-known/agent-card.json` returns the card. Static-ish content; ETag from genesis SHA + content hash.

3. **Skill list comes from the kit ontology.** Each kit's class declarations are skills. We can auto-generate `skills[]` from the ontology — class IRI becomes `skill.id`, class label becomes `skill.name`, rdfs:comment becomes `skill.description`, lex-o:okfType becomes a tag. This means OKF (#116) and A2A become structurally connected: same annotation pass, two compliance surfaces.

4. **`supported_interfaces` lists ONE entry** with `protocol_binding: "JSON_RPC_2_0"` and `url` pointing at our (eventual) task-execution endpoint. Until we ship execution, this URL points at a stub that returns `not_implemented` for any RPC method. That's still a valid card — the spec doesn't require the URL to actually serve anything until clients call it.

5. **`capabilities`** = `{ streaming: false, push_notifications: false, extended_agent_card: false }` for v0. We can flip flags as we ship features.

6. **Authentication** = `security_schemes: {}` and `security_requirements: []` for v0. Effectively "no auth required for the card; agents using the URL accept whatever auth the destination demands." This is the most permissive valid shape.

7. **HTTP caching** — `Cache-Control: max-age=300` + `ETag: "{genesis_sha}:{content_hash}"`. Cheap to implement, makes us a polite citizen.

## The connection to our existing work

- **Task #87** — we already filed `.well-known/soul.json schema-shape`. This task supersedes that filename choice. The schema-shape work we did there carries over almost wholesale; only the filename + a handful of field names change.
- **Task #116** — OKF wave. Same annotation pattern (`lex-o:okfType`) feeds both `skill.tags` (A2A) and `type:` (OKF). We get two compliance surfaces from one annotation pass.
- **Task #76** — `git lex serve` promotion. The A2A endpoint joins the same HTTP daemon. One more route on a server we're already growing.

## What we lose / what we gain

**Lose:**
- The branding of "soul.json" (specific to us) becomes "agent-card.json" (generic). A small naming concession to ride the network effect.
- Some flexibility in our own fields — we'll need to fit them into A2A's shape or relegate to an extensions block.

**Gain:**
- Every A2A-aware client can discover our agents without a custom integration.
- Free interop with LangGraph, CrewAI, ADK, Genkit, AG2, BeeAI, PydanticAI ecosystems.
- A standards-track narrative for the project ("we ship the A2A discovery layer; that's the part the ecosystem cares about right now").
- The OKF + A2A combo positions Subtexture as **"the agent substrate that already speaks both."** That's a real positioning move.

## Wave sequencing (recommendation)

Subject to the current instantiation wave landing first:

1. **OKF wave (#116)** — ships first because the annotation pattern is already in muscle memory and the change is contained to lex-o-seed + nquad.rs emit.
2. **A2A discovery wave (this doc → new task)** — ships after OKF lands. Reasons:
   - The skill-from-ontology auto-generation depends on the same annotation infrastructure as OKF.
   - We can derive the A2A `skills[]` list from kit ontology + `lex-o:okfType`, so OKF lands → A2A becomes one synthesis layer + one route.
   - The HTTP serve route is a small addition to `git lex serve`, which is the natural home.
3. **A2A execution (deferred indefinitely)** — until a real consumer asks. Big surface, no current ROI.

## Open questions to think through

1. **Genesis SHA as agent identity.** A2A wants `name` + `version`. Do we serve genesis SHA as `name` (cryptographic ID, machine-parseable)? Or human name from `agent_name` (human-readable, drifts under rebranding)? Or both via aliases? Tr1p will have a take.
2. **Skill granularity.** Every owl:Class becomes a skill? Just `instantiation = "authored"` classes? Just classes with an explicit skill annotation (`lex-o:a2aSkill`)? My instinct: explicit annotation, narrow at first.
3. **Multi-agent souls.** If one repo hosts multiple Selves (e.g. a squad-shared repo), do we serve multiple agent-cards under nested paths (`/.well-known/agent-card.json` per soul subdirectory)? Or one card listing multiple agents? A2A spec doesn't address this directly — needs design.
4. **The `extensions` mechanism.** Worth exploring whether we publish our SPARQL endpoint as an A2A extension so clients can query us in addition to invoke skills. The extension shape would be the connector.

## Decision

Bank the analysis. Don't pivot yet. Finish the lex-o:instantiation wave; ship OKF (#116) next; come back to this doc when ready to spec the A2A discovery wave.
