# Convos Kit Design — Conversation Archive & Memory

**Date:** 2026-03-31
**From:** W4R3Z + 1UX
**Status:** Design spec, ready for implementation

## Purpose

A git-lex kit for archiving and extracting knowledge from Claude Code conversations.
Raw `.jsonl` files committed to git. Extracted knowledge feeds into agent solo repos
as conversation summaries with pointers.

## Architecture

```
--kit convos repo:
  conversation/
    {project-path}/
      {session-id}.jsonl          ← raw conversation (committed to git)
  .lex/
    extract/
      conversation/{session}.spo  ← extracted metadata + entities
    ontology/
      kit/convos/convos.ttl       ← conversation ontology
```

## Relationship to Solo Repo

The convos repo is the ARCHIVE. The solo repo is the INDEX.

```
Convos repo (raw data, big):
  conversation/repolex-ai-git-lex/121c0cc3.jsonl  ← 18MB, 4890 lines

Solo repo (summaries, small):
  memory/2026-03-26-git-lex-birth.md               ← summary + pointers
    → references session 121c0cc3
    → key decisions: [[use-oxigraph]], [[drop-gliner2]]
    → participants: @1ux, @w4r3z
```

Agent queries solo for "what conversations were about X?" then digs into
convos repo for specific messages when needed. Solo is fast, convos is deep.

## Source Data Format

Claude Code stores conversations at `~/.claude/projects/{project-path}/{session-id}.jsonl`.

Each line is a JSON object:
```json
{"type": "user", "message": {"role": "user", "content": "..."}, "uuid": "...", "timestamp": "...", "cwd": "...", "sessionId": "..."}
{"type": "assistant", "message": {"role": "assistant", "content": "..."}, ...}
{"type": "file-history-snapshot", ...}
```

Key fields:
- `type` — "user", "assistant", "file-history-snapshot"
- `message.role` — "user" or "assistant"
- `message.content` — the actual text (can be very large for assistant)
- `timestamp` — ISO 8601
- `sessionId` — conversation ID
- `cwd` — working directory at time of message
- `uuid` — unique message ID
- Subagent conversations nested in `{session-id}/subagents/`

## Parsing Strategy

### JSONL Parsing: jaq (Rust-native jq)

Use `jaq-core` + `jaq-interpret` crates — pure Rust jq implementation.
No external dependency, embedded in the git-lex binary.

```rust
// Parse each line as JSON, extract fields
let filter = "{type, role: .message.role, content: .message.content[:200], timestamp, uuid}";
```

Why jaq over serde_json directly:
- Flexible field extraction without defining full structs
- jq filter syntax is well-known and composable
- Can be user-configurable (custom extraction filters in repo.yml)

### Markdown Parsing: tree-sitter

Use `tree-sitter` + `tree-sitter-markdown` crates for structural markdown parsing.
Replaces current regex for @mentions and [[wikilinks]].

Benefits over regex:
- Proper heading extraction (section-level granularity)
- Structural wikilink/link detection (not just pattern matching)
- Code block awareness (don't extract mentions from code examples)
- Heading hierarchy (h1 > h2 > h3 nesting)
- Future: any grammar we want (YAML, TOML, etc.)

Already have tree-sitter markdown ontology from repolex:
- `ontology/extracts/tree-sitter/v0.25/lang/markdown.ttl` (53 node types)
- `ontology/extracts/tree-sitter/v0.25/lang/markdown_inline.ttl` (28 node types)

Key node types for extraction:
- `atx_heading` → section structure
- `shortcut_link` → [[wikilinks]]
- `inline_link` → [text](url)
- `paragraph` → body text for @mention scanning
- `fenced_code_block` → skip (don't extract from code)
- `task_list_marker_checked/unchecked` → task tracking

## Incremental Extraction

Conversations grow — new messages append to existing `.jsonl` files.
Don't re-extract the entire file on every sync.

```
.lex/extract/conversation/{session}.meta
  last_line: 4890
  last_sync: 2026-03-31T12:00:00Z
```

Each sync:
1. Read meta → get last_line
2. Read `.jsonl` from last_line to EOF
3. Parse only new lines with jaq
4. Generate `.spo` for new messages
5. Update meta with new last_line

## Extraction Output (.spo)

Per-session sidecar:
```
session-121c0cc3 | isA | conversation
session-121c0cc3 | project | repolex-ai/git-lex
session-121c0cc3 | startDate | 2026-03-26T04:03:39Z
session-121c0cc3 | participant | user
session-121c0cc3 | participant | assistant
session-121c0cc3 | messageCount | 4890
session-121c0cc3 | mentions | oxigraph
session-121c0cc3 | mentions | memoria
session-121c0cc3 | topic | knowledge-graphs
session-121c0cc3 | topic | rdf-1.2
```

NOT full message content — just metadata, entities, topics, mentions.
Full content stays in the `.jsonl`.

## Convos Kit Ontology Classes

```
Session      — a conversation session
Message      — a single message within a session (optional, for deep indexing)
Topic        — an extracted topic from conversation content
Participant  — who was in the conversation
```

## Commands

```bash
git lex init --kit convos                    # Initialize convos repo
git lex import ~/.claude/projects/           # Import all conversations
git lex import ~/.claude/projects/{project}  # Import specific project
git lex sync                                 # Extract + build graph
```

The `import` command copies `.jsonl` files into the repo's conversation/ directory,
organizing by project path. Only copies new/changed files (by size/mtime).

## Access Control

The convos repo is PRIVATE by default. Conversations may contain sensitive info.
Access from solo repo via:
1. Agent reads their own convos directly (same machine, file access)
2. `git lex serve` on convos repo + crypto verification from solo repo
3. Or just: agent queries their solo summaries, asks the human if they need raw convo data

## Dependencies (new)

```toml
jaq-core = "2"
jaq-interpret = "2"
tree-sitter = "0.25"
tree-sitter-markdown = "0.4"
```

## Implementation Order

1. Convos kit ontology (convos.ttl)
2. `git lex import` command (copy .jsonl files)
3. jaq-based JSONL extraction (session metadata, message counts)
4. tree-sitter markdown integration (replace regex extractors)
5. Incremental extraction (line tracking)
6. Solo repo conversation summary generation
