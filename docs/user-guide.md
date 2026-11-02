# User Guide

This guide is for end users who want to connect Claude to a Loomem instance and start using persistent memory.

---

## What is Loomem?

Loomem gives Claude access to memory across conversations. Claude can store and recall information about you, what you discussed, and the decisions you made, from devices that can connect to your instance.

---

## Setup

Ask whoever runs your Loomem server for its URL and, if authentication is enabled, an API key. You can also run the server yourself — see the [Deployment Guide](deployment.md).
- **Server URL** — looks like `https://your-server.app/mcp`
- **API key** — the server's configured Bearer key (e.g. `loom_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx`), if authentication is enabled

### Claude.ai (web)

1. Go to **Settings** → **Customize** → **Connectors**
2. Click **Add connector**
3. Paste the server URL
4. When prompted for authorization, provide the credentials required by your server configuration
5. Done

### Claude Desktop (macOS / Windows)

Open Claude Desktop → **Settings** → **Developer** → **Edit Config**, edit `claude_desktop_config.json`, then **fully restart** Claude Desktop. Which block you add depends on where Loomem runs.

**Loomem running locally on this machine** (the usual case). The desktop app connects to local servers over **stdio**, not HTTP, and its custom-connector box only accepts an `https://` URL — so you cannot point it at `http://localhost`. Bridge the local HTTP endpoint to stdio. Two ways:

*Recommended — native bridge, no Node* (Loomem ≥ v0.2.1; built into `loomem-cli`):

```json
{
  "mcpServers": {
    "loomem": {
      "command": "/Users/you/.loomem/bin/loomem-cli",
      "args": ["mcp-stdio", "--url", "http://127.0.0.1:3030"]
    }
  }
}
```

Use the **absolute** path to `loomem-cli` — the desktop app doesn't inherit your shell `PATH`. For auth, add `"--token", "loom_your_api_key_here"` to `args` (or set `LOOMEM_AUTH_TOKEN` in an `"env"` block).

*Alternative — `mcp-remote`* (works with any version, needs Node 18+):

```json
{
  "mcpServers": {
    "loomem": {
      "command": "npx",
      "args": ["-y", "mcp-remote", "http://127.0.0.1:3030/mcp", "--allow-http"]
    }
  }
}
```

`--allow-http` is required for the plain-HTTP localhost endpoint. For auth: `"args": ["-y", "mcp-remote", "http://127.0.0.1:3030/mcp", "--allow-http", "--header", "Authorization: Bearer ${LOOMEM_AUTH_TOKEN}"]` plus `"env": { "LOOMEM_AUTH_TOKEN": "loom_your_api_key_here" }`.

**Loomem hosted elsewhere over HTTPS.** Point the connector straight at the remote URL — no bridge needed:

```json
{
  "mcpServers": {
    "loomem": {
      "url": "https://your-server.app/mcp",
      "headers": {
        "Authorization": "Bearer loom_your_api_key_here"
      }
    }
  }
}
```

### Claude Code (CLI / VS Code)

Add to your project's `.mcp.json` or global MCP config:

```json
{
  "mcpServers": {
    "loomem": {
      "url": "https://your-server.app/mcp",
      "headers": {
        "Authorization": "Bearer loom_your_api_key_here"
      }
    }
  }
}
```

---

## How to use

Once connected, Claude can use memory during a conversation. You don't need to learn commands — just talk naturally. Whether and when it calls the tools depends on the client and its configuration. Extraction also depends on the server's LLM configuration.

### Claude can remember information

```
You:    "I'm a data scientist working at Acme Corp"
Claude: [stores: user is a data scientist at Acme Corp]

        ... next conversation ...

You:    "Can you help me with this dataset?"
Claude: [recalls: user is a data scientist]
        "Sure! Since you're working in data science, I'll focus on..."
```

### Ask Claude to remember

```
You:    "Remember that I prefer dark mode in all tools"
Claude: [stores preference]
```

### Ask Claude to recall

```
You:    "What do you know about my preferences?"
Claude: [searches memory, returns relevant facts]
```

### Ask Claude to forget

```
You:    "Forget that I work at Acme Corp"
Claude: [deletes the relevant memory]
```

Deleting a derived fact does not automatically remove its source or other related facts. Delete the source ID to remove that conversation and its linked memories from retrieval.

---

## What gets remembered?

Claude can store:
- **Preferences** — tools, languages, coding style, work habits
- **Decisions** — technology choices, project directions
- **Facts about you** — role, expertise, projects
- **Project state** — deadlines, goals, current status

The client instructions recommend storing concrete, useful facts and skipping small talk, greetings, speculation, and transient status updates as standalone facts. `memory_ingest` can also preserve a sanitized conversation source alongside extracted facts, so do not assume that no conversation text is stored. Do not share secrets in conversations intended for ingestion.

---

## How memory works (simplified)

### Layers

Your memories live in three layers:

1. **Fresh memories** — recent, detailed information
2. **Compressed memories** — multiple related facts merged into one clear statement
3. **Core knowledge** — stable, long-term understanding (your identity, key preferences)

Fresh memories can fade over time. Important information can be consolidated, and frequently accessed memories can persist longer.

### Dream consolidation

Loomem can run a "dream" process that:
- Groups related memories together
- Merges similar information
- Resolves contradictions (e.g., if you changed your preferred IDE)
- Compresses verbose memories into concise facts

Dreaming can be requested through `memory_dream` or triggered automatically when configured; it requires the relevant LLM-backed functions to be available.

### Contradiction handling

If you tell Claude something that contradicts an earlier memory:

```
March:  "I use VSCode"
April:  "I switched to Cursor"
```

Loomem can create a version chain, marking the old fact as superseded and the new one as current. Claude can then use the newer version.

---

## Privacy

- **Single-user by design** — Loomem is intended for one user. A Bearer token can protect the instance; authentication is off by default for local use, so secure the server according to your deployment.
- **PII protection** — the default configuration enables redaction of phone numbers, email addresses, and IDs. Protection depends on configuration and detection; do not assume every sensitive item will be recognized.
- **Your server, your data** — memory is stored on your self-hosted server. LLM-backed functions may send data to the configured provider.

---

## Tips

1. **Be specific** — "I prefer Tailwind CSS over styled-components because of utility-first approach" is better than "I like Tailwind"
2. **Share context** — tell Claude about your role, projects, and goals early on
3. **Correct mistakes** — if Claude remembers something wrong, tell it to update
4. **Use across sessions** — the more you use it, the better Claude understands you
5. **Don't worry about duplicates** — Loomem can deduplicate similar facts

---

## Troubleshooting

### "Claude doesn't seem to remember anything"

- Check that the connector is active in Settings → Customize → Connectors
- Try asking: "What do you know about me?" and check whether Claude calls the memory tools
- If authentication is enabled, verify your API key is correct
- Check the LLM configuration: in keyless mode `memory_store` and search work, but LLM-backed extraction is unavailable

### "Claude is remembering wrong things"

- Ask: "What memories do you have about [topic]?"
- Tell Claude: "That's outdated, please update: [new fact]"
- Or: "Delete the memory about [topic]"

### "Memory seems slow"

- An initial search can take longer before the cache is populated
- Later searches may use the cache if enabled in the configuration
- Response time depends on the environment and configuration

---

## Questions?

For connection or API key problems, check the server logs and the [Deployment Guide](deployment.md), or ask whoever operates your Loomem server.
