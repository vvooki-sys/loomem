# Loomem MCP — Instructions for Claude

> **Runtime source of truth:** `loomem-server/mcp_instructions.md` — embedded at compile time via `include_str!()`. This document is the extended version with examples and guidance. See [MCP Tools Reference](mcp-tools.md) for the full list of 14 tools.

---

## System prompt (add to Claude Code / Desktop project instructions)

```
You have access to Loomem — a persistent memory engine that stores structured
knowledge across all conversations. Use it as your long-term memory.

## Core principle: QUALITY over QUANTITY
Do NOT store raw conversation text. Extract specific facts, decisions, and
preferences. Use memory_ingest for full conversations, memory_store for
single confirmed facts.

## Tools available (14 memory tools):

### Loading memory
- memory_context — load relevant background at start of task (token-budgeted)
- memory_profile — get user's synthesized profile (identity, preferences, work)
- memory_search — search for specific facts or past decisions
- memory_associate — surface serendipitous, non-obvious associations
- memory_namespaces — see available memory namespaces (personal, projects, etc.)

### Storing memory
- memory_ingest — PREFERRED: extract structured facts from a conversation
  via LLM. Produces typed facts with subject, date, confidence. Use instead
  of memory_store when processing conversations.
- memory_store — store a single, confirmed fact. Use for explicit user
  statements ("I prefer X", "My name is Y"). NOT for raw text.

### Maintenance
- memory_dream — trigger memory consolidation. Merges related facts,
  resolves contradictions, synthesizes knowledge. Use after long sessions.
- memory_reflect — check memory quality. Reports noise level, missing
  metadata, suggestions for cleanup. Use periodically.
- memory_history — trace how a fact evolved over time (version chain).
- memory_delete — delete a specific memory by ID.
- memory_feedback — rate how useful a specific chunk was for a task.

### Info
- memory_status — engine health (chunk count, index status, uptime)
- memory_graph — explore entity connections (people, projects, tech)
```

---

## Scope model

Data lives in isolated **streams**. Understanding scope prevents confusion:

| Scope | Tools |
|-------|-------|
| **Per-stream** (isolated) | `memory_store`, `memory_search`, `memory_ingest`, `memory_context`, `memory_profile`, `memory_dream`, `memory_reflect`, `memory_associate`, `memory_graph` |
| **Global** (engine-wide) | `memory_status`, `memory_namespaces` |
| **By chunk ID** | `memory_history`, `memory_delete`, `memory_feedback` |

- **Stream** = data isolation boundary. The built-in default stream is `__user_default__`.
- **Namespace** = human-readable label mapped to a stream ID (e.g., `personal` → `100`). Configured in `config.toml`.
- The knowledge graph is per-stream isolated — only entities and edges within the active stream are visible.

---

## When to call each tool

### memory_context
Call **once at the start** of conversations involving:
- Work on a specific project (query: project name)
- Personal advice or planning (query: "user preferences and goals")
- Technical decisions (query: technology stack, tools)

Do NOT call for: simple questions, creative tasks, greetings.

### memory_ingest (NEW — preferred for conversations)
Call when you have a meaningful conversation to preserve:
- After a planning session with decisions made
- After the user shared multiple facts about themselves or projects
- When compacting a session that contains valuable information

```
memory_ingest(
  content="User: I switched from VSCode to Cursor last week, it's much faster. Also decided to use Tailwind v4 for the dashboard project. The deadline is April 15th.",
  conversation_date="2026-04-02"
)
→ Typically extracts ~2-3 typed facts, e.g.: preference (Cursor), project_state (Tailwind v4), project_state (deadline)
  Note: exact extraction depends on LLM judgment — count and phrasing may vary.
```

### memory_store
Call for **single, confirmed facts** only:
- User explicitly states a preference: "I prefer dark mode"
- User confirms a biographical fact: "I'm 44 years old"
- A clear decision was made: "We chose Rust for the backend"

Do NOT use for raw text or multi-fact conversations — use memory_ingest instead.

```
memory_store(
  content="Switched from VSCode to Cursor (reason: AI features, inline completions)",
  subject="Alice",
  source="user-stated"
)
```

### memory_search
Call when:
- User asks about something they mentioned before
- You need context before answering about their work/preferences
- Checking if something was already decided

### memory_dream
Call:
- After a long, productive session (many new facts stored)
- When user says "clean up memory" or "consolidate"
- Periodically (once per day if active use)

### memory_reflect
Call:
- When memory quality seems poor (wrong answers, missing context)
- Periodically (once per week) to check health
- After bulk ingestion to verify quality

### memory_profile
Call at the start of new conversations to load user identity.

### memory_history
Call when:
- User asks "what did I use before X?"
- You need to understand how a preference/decision changed over time
- Debugging contradictory information

### memory_graph
Call when:
- Understanding relationships between people, projects, technologies
- User asks "who works on X?" or "what's connected to Y?"

### memory_delete
Call when:
- User explicitly asks to forget something: "delete that", "forget X"
- Never delete without user confirmation

### memory_status
Call when:
- Tools seem unresponsive or return unexpected results
- User asks about memory health or capacity
- You want to verify the engine is running before a complex operation

### memory_namespaces
Call when:
- You need to know what memory spaces exist
- User asks about their memory organization

---

## What NOT to do

- **NEVER store raw conversation text** — use memory_ingest to extract facts
- Do not store: temporary status, small talk, questions without answers
- Do not call memory_search for every response — only when recall matters
- Do not store sensitive data (passwords, API keys)
- Do not call memory_context with vague queries ("everything") — be specific
- Empty search results = nothing stored yet, not a bug

---

## Garbage-in prevention (CRITICAL)

The #1 quality issue is storing raw text instead of structured facts.

**Bad (garbage-in):**
```
memory_store(content="User: I tried Cursor today and it was really fast, much better than VS Code which I've been using for years. Also I need to finish the dashboard by next Friday.")
```

**Good (structured knowledge):**
```
memory_ingest(
  content="User: I tried Cursor today and it was really fast, much better than VS Code which I've been using for years. Also I need to finish the dashboard by next Friday.",
  conversation_date="2026-04-02"
)
```
→ LLM typically extracts facts like: "User switched from VS Code to Cursor (reason: speed)" + "Dashboard deadline: 2026-04-11"
  (Exact output depends on LLM — treat examples as illustrative, not contractual.)

**Also good (single fact):**
```
memory_store(content="Alice prefers Cursor over VS Code for speed", subject="Alice")
```

---

## Notes for updating

After testing, check:
1. Which tools Claude calls too often / too rarely
2. What content gets stored as raw text (should be going through ingest)
3. memory_reflect output — is quality score improving?
4. Are memory_dream results meaningful?
