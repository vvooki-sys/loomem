# Loomem — Quick start

Loomem gives your LLM agents memory that persists across conversations.
Every fact, decision, or preference you share gets remembered.

## Setup (about 2 minutes, local)

### 1. Install and start the server

Install the prebuilt binaries (no sudo; they land in `~/.loomem`) and put `~/.loomem/bin` on your `PATH` — full steps in the [Installation guide](installation.md). Then start it:

```bash
cd ~/.loomem && loomem-server
# listens on http://localhost:3030 — auth is off by default for local use
```

### 2. Connect Claude Code

```bash
claude mcp add --transport http loomem http://localhost:3030/mcp
```

### 2b. Or connect the Claude desktop app (and Cowork)

The Claude **desktop app** connects to local servers over **stdio**, not HTTP, and its custom-connector box only accepts an `https://` URL — so `http://localhost` won't paste in. Bridge it in `claude_desktop_config.json` (macOS: `~/Library/Application Support/Claude/claude_desktop_config.json`), then fully restart Claude.

Native bridge, no Node (Loomem ≥ v0.2.1 — use the **absolute** path to `loomem-cli`):

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

Or, with Node, the universal `mcp-remote` fallback:

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

### 3. Try it

In Claude, say:
> "Remember that I prefer dark mode in all my tools."

Then start a new conversation and ask:
> "What do you know about my preferences?"

The answer comes back from Loomem.

## Connecting claude.ai or ChatGPT (remote)

These connect to **remote** MCP servers over HTTPS, so this is a separate, longer step: expose your instance behind TLS, set `SERVER_ORIGIN`, then add the connector pointing at `https://your-domain/mcp` (OAuth dynamic client registration works out of the box). Full walkthrough — TLS, reverse proxy, auth — in the [Deployment guide](deployment.md).

## How it works
- Claude automatically stores facts you share
- Claude searches memory before answering questions about you
- Memory persists across conversations, devices, and sessions

## Privacy
Your memory is stored on your own server. Loomem is self-hosted —
no third party holds your data. See the [Security overview](SECURITY.md).

## More
- [User Guide](user-guide.md) — day-to-day usage
- [MCP Tools Reference](mcp-tools.md) — what Claude can do with memory
