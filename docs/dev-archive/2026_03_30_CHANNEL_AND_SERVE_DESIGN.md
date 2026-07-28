# git-lex Channel & Serve Design

**Date:** 2026-03-30
**From:** W4R3Z + 1UX
**Status:** Spec, ready for implementation

## Two Features, Connected

### 1. `git lex serve` — SPARQL Endpoint

Run a local HTTP SPARQL endpoint over the repo's oxigraph store.

```bash
git lex serve                    # default port 7878
git lex serve --port 7879       # custom port
```

**What it does:**
- Opens an HTTP server with a `/sparql` endpoint
- Accepts SPARQL queries via GET (?query=...) and POST
- Reads from the same `.lex/oxigraph/` persistent store
- Runs until killed (background it with `&` or run in a separate terminal)

**Why:**
- Any SPARQL client can query the repo (visualization tools, notebooks, web UIs)
- The git-lex channel server can query it instead of shelling out
- SPARQL 1.1 `SERVICE` enables federated queries across repos

**Federation across repos:**

Each repo runs serve on a different port:
```
~/repos/7R1PL3F0RC3 → git lex serve --port 7879
~/repos/git-lex-vault → git lex serve --port 7880
~/repos/project-x → git lex serve --port 7881
```

Query across all of them:
```sparql
SELECT ?agent ?vaultDoc WHERE {
  SERVICE <http://localhost:7879/sparql> {
    ?a fm:squad.type "Agent" ; fm:title ?agent
  }
  SERVICE <http://localhost:7880/sparql> {
    ?d fm:title ?vaultDoc
  }
}
```

No central server. Each repo is sovereign. Federation is ad-hoc via SERVICE.

**Implementation:**
- Oxigraph has built-in HTTP server support (`oxigraph serve`)
- We can either embed a lightweight HTTP server in git-lex (axum or tiny-http)
- Or shell out to `oxigraph serve --location .lex/oxigraph/`
- The embedded approach is better — one binary, consistent behavior

---

### 2. git-lex Channel — Claude Code Integration

A channel MCP server that bridges git-lex repos with Claude Code sessions.
Pushes @mention notifications and enables two-way messaging.

**Repo:** repolex-ai/git-lex-channel (created, empty)

**Architecture:**
```
Squad repo (7R1PL3F0RC3)
    ↓ (git poll or SPARQL query via serve endpoint)
git-lex-channel (MCP channel server, TypeScript/Bun)
    ↓ (notifications/claude/channel)
Claude Code session
    ↓ (agent sees notification)
"@w4r3z you were mentioned in decision/use-rdf-12.md by @1ux"
    ↓ (agent replies via reply tool)
git-lex-channel writes Message doc to squad repo
```

**Config (.mcp.json):**
```json
{
  "mcpServers": {
    "git-lex-channel": {
      "command": "bun",
      "args": ["./git-lex-channel.ts"],
      "env": {
        "GIT_LEX_REPO": "/path/to/7R1PL3F0RC3",
        "GIT_LEX_AGENT": "w4r3z@lex.local",
        "GIT_LEX_POLL_INTERVAL": "30",
        "GIT_LEX_SPARQL_PORT": "7879"
      }
    }
  }
}
```

**Channel capabilities:**
- `claude/channel` — push notifications to agent
- `tools` — reply tool for two-way messaging

**Instructions (system prompt):**
```
Messages from the git-lex squad arrive as <channel source="git-lex" ...>.
These are @mentions, new messages, and task assignments from your team repo.
Reply using the reply tool with the document type and content.
```

**Notification types:**

1. **@mention** — someone mentioned this agent in a document
```xml
<channel source="git-lex" type="mention" from="1ux" doc="decision/use-rdf-12.md">
You were mentioned in "Use RDF 1.2 Triple Terms": "per @w4r3z, we should..."
</channel>
```

2. **New message** — a Message document addressed to this agent
```xml
<channel source="git-lex" type="message" from="trip-l3x" priority="normal">
Hey W4R3Z, can you look at the sync graph timing? Seems slow on large repos.
</channel>
```

3. **Task assignment** — a Task assigned to this agent
```xml
<channel source="git-lex" type="task" title="Build wikilink extractor" status="todo">
Assigned to you by @1ux. Related decision: [[use-rdf-12-triple-terms]]
</channel>
```

**Reply tool:**
```typescript
{
  name: 'git_lex_reply',
  description: 'Reply to a squad message or create a new document',
  inputSchema: {
    type: 'object',
    properties: {
      doctype: { type: 'string', enum: ['message', 'decision', 'discovery', 'note'] },
      title: { type: 'string' },
      content: { type: 'string' },
      to: { type: 'string', description: 'Agent to address (for messages)' },
    },
    required: ['doctype', 'title', 'content'],
  },
}
```

The reply tool runs:
1. `git lex create {doctype} --title "{title}"`
2. Fills in the frontmatter (from, to, content)
3. `git lex save "Message from {agent}"`

**Poll loop (one-way, no serve):**
```
every POLL_INTERVAL seconds:
  1. cd GIT_LEX_REPO && git pull
  2. git lex sync (if needed)
  3. git lex query "new mentions for this agent since last check"
  4. for each new mention: mcp.notification()
  5. update last-check timestamp
```

**Query loop (with serve):**
```
every POLL_INTERVAL seconds:
  1. HTTP GET to localhost:{SPARQL_PORT}/sparql?query=...
  2. Parse results
  3. for each new mention: mcp.notification()
  4. update last-check timestamp
```

The serve approach is better — no git pull/sync needed in the channel,
the serve process handles that. The channel just queries.

**Replaces claude-peers:**
- claude-peers: real-time ephemeral chat (lost on session end)
- git-lex channel: persistent queryable communication (git-versioned)
- @mentions in documents = async communication
- Message documents = direct messages
- All SPARQL-queryable, all versioned, all with provenance

---

## Implementation Order

1. `git lex serve` (Rust, in git-lex binary) — enables federation + channel queries
2. git-lex-channel (TypeScript/Bun, separate repo) — Claude Code integration
3. Federation examples and docs
