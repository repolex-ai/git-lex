# Harness-Specific Kit Features & Multi-Substrate Integration

A central design principle of `git-lex` is that **soul content is the single source of truth**, while **agent harness directories are ephemeral, derived targets**. 

When you author a skill (`Soul/Skill/journal.md`), an identity note, or a lifecycle hook in a kit, you author it once. `git-lex` then projects that content into the exact directory structures, frontmatter dialects, and hook formats required by each active agent harness.

---

## Substrate Selection & Auto-Detection

A soul repository can run against one or multiple agent substrates simultaneously. Substrates are resolved on every `git lex save`, `git lex sync`, and `git lex kit-update` in order of precedence:

1. **Explicit Declarative Override (`.lex/repo.yml`):**
   ```yaml
   substrates:
     - gemini
     - claude
     - hermes
   ```
2. **On-Disk Auto-Detection:**
   * If `.claude/` is present $\rightarrow$ `Substrate::Claude`
   * If `.agents/`, `.gemini/`, or `.antigravity/` is present $\rightarrow$ `Substrate::Gemini`
   * If `.hermes/` or `hermes-config.yaml` is present $\rightarrow$ `Substrate::Hermes`
3. **Back-Compat Default:** Falls back to `[Claude]` if nothing is detected.

---

## Claude Integration (`.claude/`)

Claude Code organizes extensions around tool configurations, command shims, and shell hooks:

### Skills Layout
* **Source:** `Soul/Skill/{name}.md`
* **Target:** `.claude/skills/{name}/SKILL.md`
* **Frontmatter Translation:**
   ```yaml
   ---
   name: journal
   description: Write or read your daily journal entry.
   user-invocable: true
   allowed-tools: Bash, FileRead
   argument-hint: "<command>"
   ---
   ```

### Lifecycle Hooks
* **Location:** `.claude/hooks/<Event>-<name>.sh`
* **Manifest:** Registered into `.claude/settings.json` under `"hooks": { ... }`.
* **Standard Events:**
  * `UserPromptSubmit`: Fires before Claude processes the turn. Used for memory recall (`UserPromptSubmit-soul-recall.py`).
  * `SessionEnd`: Fires when the session terminates. Used for `git lex save` auto-commit.
  * `PreCompact`: Fires before context compaction. Used for memory consolidation.

---

## Antigravity Integration (`.agents/`)

Google Antigravity uses a declarative, progressive-disclosure architecture designed to minimize context window bloat:

### Skills Layout & Progressive Disclosure
* **Source:** `Soul/Skill/{name}.md`
* **Target:** `.agents/skills/{name}/SKILL.md`
* **Frontmatter Translation:**
   ```yaml
   ---
   name: journal
   description: Write or read your daily journal entry.
   ---
   ```
* **How Antigravity Runs Skills:** Skills are indexed by name and description into the system prompt's `<skills>` block. The agent progressively loads the full skill body via `view_file` only when invoked.

### Lifecycle Hooks (`.agents/hooks.json`)
* **Location:** `.agents/hooks.json`
* **Format:** Structured JSON mapping event names to handler arrays:
   ```json
   {
     "soul-recall": {
       "PreInvocation": [
         {
           "type": "command",
           "command": "python3 .agents/hooks/PreInvocation-soul-recall.py",
           "timeout": 5
         }
       ]
     },
     "soul-save": {
       "Stop": [
         {
           "type": "command",
           "command": "git lex save 'Session auto-save' && git lex sync",
           "timeout": 15
         }
       ]
     }
   }
   ```
* **Supported Events:**
  * `PreInvocation`: Equivalent to `UserPromptSubmit`.
  * `Stop`: Equivalent to `SessionEnd`.
  * `PreToolUse` / `PostToolUse`: Run commands before/after specific tool execution.

### Targeted Rules (`.agents/rules/*.md`)
In addition to the global `AGENTS.md` loaded at startup, Antigravity supports scoped rules triggered by file paths:
```yaml
---
trigger:
  glob: "Soul/Journal/*.md"
---
# Rules for authoring journal entries...
```

---

## Hermes and Codex Integration (`.hermes/`)

For Hermes and OpenAI Codex-based harnesses:
* **Configuration:** `hermes-config.yaml` or `.hermes/config.json`.
* **Skills:** Exported to `.hermes/tools/` or injected into system prompts.
* **Subagents:** Mapped to role prompt specifications.

---

## Structuring a Kit for Multi-Harness Deployment

When building a kit that ships harness integrations (like `git-lex-kit-soul`), structure your kit's `harness/` directory like this:

```text
my-kit/
├── ontology/
│   └── mykit/mykit.ttl
├── harness/
│   ├── .claude/
│   │   ├── hooks/
│   │   │   └── SessionEnd-save.sh
│   │   └── settings.json
│   ├── .agents/
│   │   ├── hooks.json
│   │   └── rules/
│   └── .hermes/
│       └── hermes-config.yaml
└── templates/
    └── __Skill.md
```

### Convergence Behavior
1. When a user runs `git lex kit-update`, `git-lex` inspects the repo's active substrates.
2. It copies only the relevant harness trees (`harness/.claude/` $\rightarrow$ `.claude/`, `harness/.agents/` $\rightarrow$ `.agents/`).
3. Every `git lex save` automatically transforms `Soul/Skill/*.md` into the active harness formats and prunes stale deployed files.
