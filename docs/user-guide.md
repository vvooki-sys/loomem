# User Guide

This guide is for end users who want to connect Claude to a Loomem instance and start using persistent memory.

---

## What is Loomem?

Loomem gives Claude long-term memory. Without it, every conversation starts from scratch. With Loomem, Claude remembers who you are, what you've discussed, and what decisions you've made — across all conversations and devices.

---

## Setup

You need two things from whoever runs your Loomem server (possibly you — see the [Deployment Guide](deployment.md)):
- **Server URL** — looks like `https://your-server.app/mcp`
- **API key** — the server's configured Bearer key (e.g. `loom_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx`)

### Claude.ai (web)

1. Go to **Settings** → **Customize** → **Connectors**
2. Click **Add connector**
3. Paste the server URL
4. When prompted for authorization, enter your API key
5. Done

### Claude Desktop (macOS / Windows)

Open Claude Desktop → **Settings** → **Developer** → **Edit Config**, edit `claude_desktop_config.json`, then **fully restart** Claude Desktop. Which block you add depends on where Loomem runs.

**Loomem running locally on this machine** (the usual case). The desktop app connects to local servers over **stdio**, not HTTP, and its custom-connector box only accepts an `https://` URL — so you cannot point it at `http://localhost`. Bridge the local HTTP endpoint to stdio with [`mcp-remote`](https://www.npmjs.com/package/mcp-remote) (needs Node 18+):

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

`--allow-http` is required for the plain-HTTP localhost endpoint. If you enabled an auth token, add it as a header: `"args": ["-y", "mcp-remote", "http://127.0.0.1:3030/mcp", "--allow-http", "--header", "Authorization: Bearer ${LOOMEM_AUTH_TOKEN}"]` plus `"env": { "LOOMEM_AUTH_TOKEN": "loom_your_api_key_here" }`.

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

Once connected, Claude automatically uses memory. You don't need to learn any commands — just talk naturally.

### Claude remembers automatically

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

---

## What gets remembered?

Claude stores:
- **Preferences** — tools, languages, coding style, work habits
- **Decisions** — technology choices, project directions
- **Facts about you** — role, expertise, projects
- **Project state** — deadlines, goals, current status

Claude does **not** store:
- Raw conversation text
- Small talk or greetings
- Passwords, API keys, or secrets
- Temporary status updates

---

## How memory works (simplified)

### Layers

Your memories live in three layers:

1. **Fresh memories** — exactly what you said, recent and detailed
2. **Compressed memories** — multiple related facts merged into one clear statement
3. **Core knowledge** — stable, long-term understanding (your identity, key preferences)

Fresh memories naturally fade over time. Important ones get compressed and promoted. Frequently accessed memories persist longer.

### Dream consolidation

Periodically, Loomem runs a "dream" process that:
- Groups related memories together
- Removes duplicates
- Resolves contradictions (e.g., if you changed your preferred IDE)
- Compresses verbose memories into concise facts

This happens automatically in the background.

### Contradiction handling

If you tell Claude something that contradicts an earlier memory:

```
March:  "I use VSCode"
April:  "I switched to Cursor"
```

Loomem creates a version chain — the old fact is marked as superseded, and the new one becomes current. Claude will answer based on the latest version.

---

## Privacy

- **Single-user by design** — one API key controls the whole instance; nobody else has access
- **PII protection** — phone numbers, emails, and IDs are redacted before any AI processing
- **Your server, your data** — Loomem is self-hosted, not a cloud service

---

## Tips

1. **Be specific** — "I prefer Tailwind CSS over styled-components because of utility-first approach" is better than "I like Tailwind"
2. **Share context** — tell Claude about your role, projects, and goals early on
3. **Correct mistakes** — if Claude remembers something wrong, tell it to update
4. **Use across sessions** — the more you use it, the better Claude understands you
5. **Don't worry about duplicates** — Loomem automatically deduplicates similar facts

---

## Troubleshooting

### "Claude doesn't seem to remember anything"

- Check that the connector is active in Settings → Customize → Connectors
- Try asking: "What do you know about me?" — if Loomem is connected, Claude will search memory
- Verify your API key is correct

### "Claude is remembering wrong things"

- Ask: "What memories do you have about [topic]?"
- Tell Claude: "That's outdated, please update: [new fact]"
- Or: "Delete the memory about [topic]"

### "Memory seems slow"

- First search in a conversation may take 1-2 seconds (cold cache)
- Subsequent searches are faster (cached)
- This is normal — memory quality is more important than speed

---

## Questions?

For connection or API key problems, check the server logs and the [Deployment Guide](deployment.md), or ask whoever operates your Loomem server.
