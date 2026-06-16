# Loomem — Persistent Memory for Claude

Loomem gives Claude memory that persists across conversations.
Every fact, decision, or preference you share gets remembered.

## Setup (2 minutes)

### 1. Run a Loomem server

See the [Deployment Guide](deployment.md) for local, Docker, and cloud options. Set an API key:

```bash
export LOOMEM_AUTH_TOKEN="your-secret-key"
./target/release/loomem-server
```

### 2. Add to Claude Desktop

Open Claude Desktop → Settings → Developer → Edit Config

Add to `claude_desktop_config.json`:
```json
{
  "mcpServers": {
    "loomem": {
      "url": "https://your-server.example.com/mcp",
      "headers": {
        "Authorization": "Bearer YOUR_API_KEY_HERE"
      }
    }
  }
}
```

### 3. Restart Claude Desktop

That's it. Claude now has persistent memory. Try:
> "Remember that I prefer dark mode in all tools"

Then start a new conversation and ask:
> "What do you know about my preferences?"

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
