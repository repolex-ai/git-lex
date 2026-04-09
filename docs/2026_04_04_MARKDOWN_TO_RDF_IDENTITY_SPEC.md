# Markdown-to-RDF Identity & Addressing Spec

Design specification for how markdown files with YAML frontmatter map to RDF, how instances are identified, and how references resolve across the git-lex ecosystem.

Authors: @lux, @tr1p-l3x
Date: 2026-04-04

---

## 1. Core Model

A markdown file is an **instance** of an RDF class. The file IS the thing — not a file that contains data about a thing.

| Concept | Maps to |
|---|---|
| Markdown file | RDF instance (resource) |
| Kit | Ontology namespace |
| Class | `rdf:type` |
| YAML frontmatter | Properties (predicates + objects) |
| Markdown body | `dcterms:description` or summary of the instance |

---

## 2. Frontmatter Convention

### Dot notation: kit.class.property

Frontmatter keys use flat dot notation with three segments:

```yaml
---
solo.session.session_id: "abc-123"
solo.session.summary: "SHACL shapes work"
solo.session.started_at: 2026-04-03T00:00:00Z
---
```

- **First segment**: kit name (ontology namespace) — `solo`, `squad`, `lab`, etc.
- **Second segment**: class name — `session`, `message`, `agent`, `decision`, etc.
- **Third segment**: property name

This is valid YAML. The dots have no special meaning to YAML parsers — they are string keys. The semantic structure (kit.class.property) is a convention imposed by our extraction layer, which splits on `.` to recover the hierarchy.

### Triple extraction

```yaml
solo.session.session_id: "abc-123"
```

Produces:

```turtle
<instance-iri> rdf:type solo:Session .
<instance-iri> solo:sessionId "abc-123" .
```

---

## 3. Inline Property Constraints (YAML Comments)

When an agent edits an existing file (not using `git lex create`), YAML comments provide inline schema hints derived from the SHACL shapes:

```yaml
---
squad.message.from_agent:  # required, IRI -> Agent
squad.message.sent_at:  # required, dateTime
squad.message.priority:  # optional, enum: low, medium, high, critical
squad.message.related_to:  # optional, IRI -> Decision
squad.message.tags:  # optional, ["str", "str"]
squad.message.summary:  # optional, str
---
```

### Comment grammar

`# required|optional, type-hint`

### Type hints

| Hint | Meaning |
|---|---|
| `str` | Plain string |
| `int` | Integer |
| `dateTime` | ISO 8601 timestamp |
| `IRI -> ClassName` | Reference to an instance of that class |
| `enum: val1, val2, val3` | Pick from list |
| `["str", "str"]` | List of strings |
| `["IRI -> Agent"]` | List of references to instances of a class |

### Source of truth chain

1. **TTL** — the ontology (OWL class + property definitions)
2. **SHACL shapes** — derived from the TTL (constraints, cardinality, allowed values)
3. **Inline comments** — derived from the SHACL shapes (human/agent-readable projection)

The comments are generated, not hand-maintained. The TTL is always authoritative.

---

## 4. IRI Scheme

### Base pattern

```
https://{host}/{org}/{repo}/{Class}/{instance-id}.md
```

### Examples

```
https://github.com/7R1PL3F0RC3/7R1PL3F0RC3/Agent/spacegoat.md
https://github.com/7R1PL3F0RC3/7R1PL3F0RC3/Message/2026-04-03-standup.md
https://github.com/7R1PL3F0RC3/TR1P.L3X/Session/abc-123.md
https://github.com/repolex-ai/ontology-builder/Decision/shacl-layer-design.md
```

### Design decisions

- **No ref (branch/tag) in the IRI.** Refs are mutable pointers — they change on every commit. The IRI is stable identity. Provenance (commit SHA, blob hash) is tracked in the triple store via triple terms, not embedded in the IRI.
- **No forge routing noise.** GitHub uses `/blob/`, GitLab uses `/-/blob/`, Codeberg uses `/src/branch/`. These are platform UI plumbing. Our IRIs omit them — the IRI is an identifier, not necessarily a clickable link.
- **`.md` extension included.** The IRI maps directly to the file in the repo. On GitHub, adding `/blob/main/` makes it dereferenceable — you can view and edit the resource directly in the browser.
- **Host derived from git remote.** The extraction layer reads the remote URL to determine host, org, and repo.

### Forge-aware dereferenceable URLs (optional, generated)

When a clickable link is needed, generate the forge-specific URL:

| Forge | Dereferenceable URL |
|---|---|
| GitHub | `https://github.com/{org}/{repo}/blob/main/{Class}/{id}.md` |
| GitLab | `https://gitlab.com/{org}/{repo}/-/blob/main/{Class}/{id}.md` |
| Codeberg | `https://codeberg.org/{org}/{repo}/src/branch/main/{Class}/{id}.md` |

These are views, not identity. The IRI (without routing noise) is the canonical identifier.

---

## 5. Universal Code Coordinate System

Any piece of code in any public git repo can be identified with:

| Component | Identifies |
|---|---|
| `forge_hash` | Which repo in the world (`hash(host/org/repo)`) |
| `commit_hash` | Which moment in that repo's history |
| `path` | Which file |
| `range` | Which part of that file (line, byte offset, etc.) |

The blob hash (content hash of a single file) and tree hash (content hash of the full file tree) are derivable from commit + path — they don't need separate tracking in the coordinate.

### Git hash anatomy

| Hash Inclusion | Component |
|---|---|
| blob hash | File content (raw bytes) |
| tree hash | Blob hashes + filenames + permissions |
| commit hash | Tree hash + parent commit(s) + author + committer + message |
| — (mutable) | Ref (branch/tag) — named pointer to a commit |
| — (mutable) | Org/Repo — forge-level namespace |

Everything below ref is immutable and content-addressed. Ref is a mutable cursor. Org/repo is a mutable namespace.

---

## 6. Naming Standards

### Agent canonical IDs

Agent names must be valid in: URIs, git repo names, YAML keys, email local parts.

**Rules:**
- Lowercase alphanumeric + hyphens only
- Must start with a letter
- No special characters (`?`, `!`, `.`, `@`, spaces)
- Case-insensitive matching, but canonical form is always lowercase

**Display names** can be expressive. **Canonical IDs** are strict.

| Display Name | Canonical ID |
|---|---|
| ?M4RQ | `m4rq` |
| SpaceG.O.A.T. | `spacegoat` |
| TR1P.L3X | `tr1p-l3x` |
| W4R3Z | `w4r3z` |
| lUX | `lux` |
| W3BL0RD | `w3bl0rd` |
| 4RX | `forx` |
| M3RCUR14L | `m3rcur14l` |

### Instance IDs

For non-agent instances (sessions, messages, decisions, etc.):
- Same character rules as agent IDs: lowercase alphanumeric + hyphens
- Date-prefixed where chronological ordering is useful: `2026-04-03-shacl-work`
- Or UUID/short-id where order doesn't matter: `abc-123`

### IRI-friendliness

All instance IDs must be valid IRI path segments. No percent-encoding should be needed. If you have to encode it, the name is wrong.

---

## 7. Reference Resolution

### Identity format

```
{canonical-id}@{squad}
```

Examples:
```
lux@7R1PL3F0RC3
spacegoat@7R1PL3F0RC3
tr1p-l3x@7R1PL3F0RC3
```

### @mentions in markdown

`@spacegoat` in a squad doc resolves using the current squad context:

1. Current squad is `7R1PL3F0RC3`
2. Look up `Agent/spacegoat.md` in squad repo
3. IRI: `https://github.com/7R1PL3F0RC3/7R1PL3F0RC3/Agent/spacegoat.md`

### Frontmatter IRI references

When a property is typed `IRI -> Agent`, the agent writes a human-readable identifier:

```yaml
squad.message.from_agent: spacegoat  # required, IRI -> Agent
```

The extraction layer resolves `spacegoat` → full IRI against the squad registry (the `Agent/` folder in the squad repo).

### Scope rules

| Scope | Resolution |
|---|---|
| **Solo repo** | References are private. Use whatever names make sense to you. No external resolution. |
| **Squad repo** | References resolve against the squad registry. `@spacegoat` → `Agent/spacegoat.md` in the squad repo. |
| **Cross-squad** | Use fully qualified `{id}@{squad}` format. Requires federation (future work — see lex-id.ttl proposal). |

### Registry

The squad repo IS the registry. If `Agent/spacegoat.md` exists in the squad repo, then `spacegoat` is a valid canonical name. Identity keys are stored in solo repos and validated against the squad.

---

## 8. Class Templates & Folder Structure

### Instance folders with class templates

Each class in a kit gets its own folder. Instances live alongside a **class template** file prefixed with `__`:

```
Session/
├── __Session.md                        # type: lex.Class (template)
├── 2026-04-04-design-session.md       # type: solo.Session (instance)
└── 2026-04-03-shacl-work.md           # type: solo.Session (instance)

Contact/
├── __Contact.md                        # type: lex.Class (template)
└── spacegoat.md                        # type: solo.Contact (instance)

Skill/
├── __Skill.md                          # type: lex.Class (template)
└── journal.md                          # type: solo.Skill (instance)
```

### Class templates are graph objects

Class templates use `type: lex.Class` — a lex-level concept that works across all kits. They ARE included in the knowledge graph, so agents can discover available classes via SPARQL:

```sparql
SELECT ?class ?kit WHERE {
    ?c lex:type lex:Class .
    ?c lex:className ?class .
    ?c lex:kit ?kit .
}
```

Filter them out when querying for instances: `FILTER(?type != lex:Class)`

### Template format

The `__ClassName.md` file contains the full frontmatter with inline SHACL comment hints — the same scaffold that `git lex create` would produce:

```yaml
---
solo.session.session_id:  # required, str
solo.session.summary:  # optional, str
solo.session.started_at:  # optional, dateTime
---
```

An agent who doesn't know `git lex create` can look in the class folder, copy the template, fill it in, and have a valid instance.

### Extra frontmatter for hybrid files

Some classes produce files that are read by external tools (e.g., Claude Code reads SKILL.md frontmatter). These files need "foreign" frontmatter fields that are not part of our ontology but must be present in the template.

The class definition declares these via `lex.class.extra_frontmatter` — a map of foreign field names to comment hints:

```yaml
---
lex.class.name: Skill
lex.class.kit: solo
lex.class.extra_frontmatter:
  name: "Name of the skill"
  description: "What this skill does and when to use it"
  user-invocable: "true|false"
  allowed-tools: "space-separated tool names: Read Write Glob Bash Edit"
  argument-hint: "hint for autocomplete"
---
```

The template generator writes foreign fields first (with their comment hints), then our `kit.class.property` fields (with SHACL-derived hints):

```yaml
---
name:  # Name of the skill
description:  # What this skill does and when to use it
user-invocable:  # true|false
allowed-tools:  # space-separated tool names: Read Write Glob Bash Edit
argument-hint:  # hint for autocomplete
solo.skill.created_by:  # required, IRI -> Agent
solo.skill.created_at:  # required, dateTime
solo.skill.version:  # optional, int
---
```

Foreign fields are passthrough — not validated by SHACL, not extracted to triples. They exist for the external tool. Our fields are validated and extracted as normal.

### Generated at init

All class folders, templates, and startup files are generated by `git lex init`:

- `__ClassName.md` templates per class (derived from SHACL shapes + extra_frontmatter)
- `IDENTITY.md` — blank identity template at repo root
- `.claude/CLAUDE.md` — rehydration protocol
- `.claude/skills/journal/SKILL.md` — journal skill
- `journal/` — empty, ready for day-1.md

---

## 9. Skills as Graph Objects

Skills (`.claude/skills/*/SKILL.md`) are simultaneously Claude Code skill definitions and git-lex graph objects. The two systems coexist in the same YAML frontmatter without conflict:

- **Claude Code** reads its known fields (`name`, `description`, `allowed-tools`, etc.)
- **git-lex extraction** reads the `kit.class.property` fields (`solo.skill.*`)
- Unknown fields are ignored by both systems

This means every skill is trackable in the knowledge graph — when it was created, who created it, what version it's on, and (via `created_with` references on output documents) what artifacts it produced. This enables self-improvement: query for "all documents created by the journal skill" to evaluate quality, or "which skills haven't been used in 5 Claude Days" to identify dead skills.

---

## 10. Enforcement

### Current enforcement points

| Point | Mechanism | Bypass risk |
|---|---|---|
| `git lex create` | Scaffolds correct structure, shows SHACL shape | Agent doesn't use it |
| `git lex save` | Validates frontmatter against SHACL | Agent uses raw `git commit` |
| Pre-commit hook (rudof) | Validates on any `git commit` | `--no-verify` |
| Inline comments | Shows constraints at edit time | Agent ignores them |
| Skills (planned) | Instructions for agents on how to use the system | Agent doesn't read them |

### Planned improvements

- **Skills**: Agent-facing instructions that describe the workflow (`git lex create` → edit → `git lex save`)
- **Pre-commit hook**: W4R3Z wiring rudof SHACL validation into git hooks — catches `git commit` directly regardless of whether agent used `git lex save`

---

## 11. Open Questions

- **DID integration**: Current `{id}@{squad}` format is a bridge. Full decentralized identity (DIDs) is future work. The `lex-id.ttl` swarm identity proposal addresses some of this.
- **Forge hash computation**: Exact algorithm for `forge_hash` from `host/org/repo` TBD. Likely SHA-256.
- **Range encoding in IRIs**: GitHub only supports line ranges (`#L10-L25`). Sub-line precision (byte offset, column) needs a convention. Tree-sitter and LSP use different schemes.
- **Dangling references**: In an append-only triple store, references to deleted/retracted things are historical facts, not errors. But we may need a convention for marking resources as inactive vs. truly broken links.
- **`4rx` canonical ID**: Starts with a digit, which some systems reject. Using `forx` as canonical ID — needs confirmation.
