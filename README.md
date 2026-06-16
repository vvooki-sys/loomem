<div align="center">

<img src="docs/assets/loomem-banner.svg" alt="Loomem — Memory, woven. A persistent memory engine for AI agents." width="100%">

<br>

[![Website](https://img.shields.io/badge/website-vvooki--sys.github.io%2Floomem-1684DC?style=flat-square&labelColor=1F1B16)](https://vvooki-sys.github.io/loomem/)
[![License](https://img.shields.io/badge/license-Apache--2.0-EE9913?style=flat-square&labelColor=1F1B16)](LICENSE)
[![Built in Rust](https://img.shields.io/badge/built%20in-Rust-A2610B?style=flat-square&labelColor=1F1B16)](https://www.rust-lang.org)
[![MCP-native](https://img.shields.io/badge/MCP-native-1684DC?style=flat-square&labelColor=1F1B16)](https://modelcontextprotocol.io)
[![Status: early](https://img.shields.io/badge/status-early-CE7D08?style=flat-square&labelColor=1F1B16)](#)

**[Website](https://vvooki-sys.github.io/loomem/)** · **[Quickstart](#quickstart)** · **[Install](#install)** · **[Architecture](#architecture)** · **[Docs](#documentation)**

</div>

---

A persistent memory engine for AI agents. Single binary, local-first, MCP-native.

Loomem stores structured knowledge extracted from conversations, and serves it back to any MCP-capable client (Claude, Cursor, custom agents) through hybrid retrieval:

- **Hybrid search** — BM25 (Tantivy) + vector embeddings + entity graph signals, fused with reciprocal-rank fusion.
- **Consolidation** — background workers merge related facts, resolve contradictions, and decay stale memories ("dreaming").
- **Bitemporal model** — facts carry both ingestion time and event time (`valid_from` / `valid_until`), so "what did I know in March" and "what happened in March" are different queries.
- **Entity graph** — people, projects, and technologies are extracted into a graph with aliases and relations, used both for retrieval and exploration.
- **MCP-native** — 14 `memory_*` tools over the standard MCP HTTP transport, including OAuth dynamic client registration for remote connectors.
- **Encryption at rest** (optional) — field-level AES-GCM envelope encryption with a master key from the environment.

Built in Rust on RocksDB + Tantivy. Single binary, no external services required.

> **Status: early.** The engine has been in daily personal use for a while, but the public API and storage format may still change. Expect rough edges; issues and PRs are welcome.

## Quickstart

From nothing to Claude remembering things across conversations, on macOS or Linux. Each step is independent — run them in order.

**1. Install the binaries** (no sudo; lands in `~/.loomem`):

```bash
curl -fsSL https://raw.githubusercontent.com/vvooki-sys/loomem/main/install.sh | sh
```

> Repo still private? Install through the [GitHub CLI](https://cli.github.com) instead — same script, authenticated:
> ```bash
> gh api repos/vvooki-sys/loomem/contents/install.sh -H "Accept: application/vnd.github.raw" | sh
> ```

**2. Put `~/.loomem/bin` on your PATH** (the installer prints this too):

```bash
echo 'export PATH="$HOME/.loomem/bin:$PATH"' >> ~/.zshrc   # or ~/.bashrc
exec $SHELL
```

**3. Start the server** (config is already seeded in `~/.loomem`):

```bash
cd ~/.loomem && loomem-server
```

**4. Check it's alive** (in another terminal):

```bash
curl http://localhost:3030/health
# {"status":"ok","version":"0.2.0"}
```

**5. Connect Claude Code:**

```bash
claude mcp add --transport http loomem http://localhost:3030/mcp
```

**6. Try it.** In Claude: *"Remember that I prefer dark mode in all my tools."* Then, in a fresh conversation: *"What do you know about my preferences?"* — the answer comes back from Loomem.

Other clients (claude.ai, ChatGPT, OpenClaw), TLS/remote exposure, and the full options matrix are below and in [docs/installation.md](docs/installation.md).

## Install

### One-liner (prebuilt binaries, macOS + Linux)

```bash
curl -fsSL https://raw.githubusercontent.com/vvooki-sys/loomem/main/install.sh | sh
```

> **While this repository is private**, the raw URL above requires authentication. Use the [GitHub CLI](https://cli.github.com) instead — same script, authenticated transparently:
>
> ```bash
> gh api repos/vvooki-sys/loomem/contents/install.sh -H "Accept: application/vnd.github.raw" | sh
> ```
>
> The installer detects your `gh` login (or a `GH_TOKEN`) automatically and downloads release assets through the GitHub API.

Installs `loomem-server`, `loomem-cli`, and `loomem-migrate` to `~/.loomem/bin` (no sudo) and drops config templates into `~/.loomem`. Pin a version with `LOOMEM_VERSION=v0.2.0`, change the location with `LOOMEM_INSTALL_DIR`. Archives are verified against `SHA256SUMS` from the [releases page](https://github.com/vvooki-sys/loomem/releases).

**Full guide** — requirements, version pinning, manual checksum verification, upgrading, uninstalling, troubleshooting: [docs/installation.md](docs/installation.md).

### From source

```bash
git clone https://github.com/vvooki-sys/loomem.git
cd loomem
cp entities.toml.example entities.toml   # personal entity config (gitignored)
cargo run --release -p loomem-server
# server listens on http://127.0.0.1:3030, data in ./data
```

Requires Rust (stable) and libclang (for the RocksDB build): `apt install libclang-dev` on Debian/Ubuntu, included with Xcode CLT on macOS.

### Docker

```bash
docker build -t loomem .
docker run -p 3030:3030 -v loomem-data:/data loomem
```

Authentication is off by default for local use. To require an API key, set the env var named in `config.toml` (`server.auth_token_env`, default `LOOMEM_AUTH_TOKEN`) and pass it as `Authorization: Bearer <key>`. **If the server is reachable by anyone but you, set the token.**

## Connect an MCP client

Loomem speaks MCP over streamable HTTP at `/mcp`. Any MCP-capable client works; recipes for the common ones:

### Claude Code

```bash
claude mcp add --transport http loomem http://localhost:3030/mcp
```

### Claude (claude.ai / desktop) — remote connector

claude.ai connects to remote MCP servers over HTTPS. Expose your instance behind a reverse proxy with TLS (or a tunnel like Cloudflare Tunnel), set `SERVER_ORIGIN=https://your-domain` (required so OAuth metadata advertises the right URL), then add the connector in Claude settings pointing at `https://your-domain/mcp`. Loomem supports OAuth dynamic client registration out of the box (`/.well-known/oauth-authorization-server`).

### ChatGPT — custom connector

ChatGPT requires HTTPS and OAuth for custom connectors (static API keys are not supported); developer mode must be enabled (Pro/Team/Enterprise plans). Expose the server over HTTPS as above, then: ChatGPT → Settings → Connectors → Add custom connector → paste `https://your-domain/mcp` and complete the OAuth flow.

### OpenClaw

```bash
openclaw mcp add loomem --url http://localhost:3030/mcp --transport http \
  --header "Authorization: Bearer $LOOMEM_AUTH_TOKEN"
```

For a remote OpenClaw gateway, point `--url` at your HTTPS endpoint instead.

### Generic MCP client config

```json
{
  "mcpServers": {
    "loomem": {
      "type": "http",
      "url": "http://localhost:3030/mcp",
      "headers": { "Authorization": "Bearer <your LOOMEM_AUTH_TOKEN>" }
    }
  }
}
```

### Standalone server notes

One Loomem instance can serve several clients at once (Claude Code locally, ChatGPT through the HTTPS endpoint, etc.) — they share the same memory. Keep the bind address `127.0.0.1` unless you front it with TLS + auth; never expose the bare HTTP port to the internet.

## Architecture

```
                    ┌──────────────────────────────────────────────┐
                    │                loomem-server                 │
 MCP client ──────► │  /mcp (JSON-RPC) ── dispatcher ─┐            │
 HTTP client ─────► │  /v1/* + /api/* ─── handlers ───┤            │
                    └─────────────────────────────────┼────────────┘
                                                      ▼
                    ┌──────────────────────────────────────────────┐
                    │                 loomem-core                  │
                    │  hybrid search (BM25 + vector + graph + RRF) │
                    │  consolidation / decay / dream workers       │
                    │  entity extraction + alias graph             │
                    │  encryption at rest (optional)               │
                    └───────┬───────────────┬──────────────────────┘
                            ▼               ▼
                       RocksDB          Tantivy
                  (chunks, graph,    (full-text index)
                   embeddings)
```

Workspace crates: `loomem-core` (engine), `loomem-server` (HTTP/MCP server), `loomem-migrate` (offline DB maintenance), `loomem-cli` (command-line client).

## Documentation

- [Quick start](docs/QUICK_START.md)
- [Configuration](docs/configuration.md)
- [API reference](docs/api-reference.md)
- [MCP tools](docs/mcp-tools.md)
- [Architecture](docs/architecture.md)
- [Deployment](docs/deployment.md)
- [Security model](docs/SECURITY.md)
- [Backup & restore](docs/backup-and-restore.md)

## License

Apache-2.0. See [LICENSE](LICENSE).
