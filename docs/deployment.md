# Deployment Guide

## Local development

### Prerequisites

- Rust toolchain (stable)
- clang + libclang-dev (for RocksDB)
- OpenAI API key (optional — local embeddings available)

### Build and run

```bash
git clone <repository-url>
cd loomem

# Build
cargo build --release -p loomem-server

# Set environment (optional)
export OPENAI_API_KEY="sk-..."
export LOOMEM_AUTH_TOKEN="your-secret-token"

# Run
./target/release/loomem-server

# Verify
curl http://localhost:3030/health
```

### First memory

```bash
curl -X POST http://localhost:3030/v1/store \
  -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{"content": "Hello, this is my first memory"}'
```

### Local embeddings (no OpenAI dependency)

Set `embedding_provider = "local"` in `config.toml` under `[llm]` — this is the default on a fresh install. Uses the tract ONNX runtime — pure Rust, no native dependencies, and nothing leaves the machine.

Note: consolidation, extraction, and dream use OpenAI completions (`gpt-4.1-mini`). Without an `OPENAI_API_KEY` those steps fall back to a built-in regex extractor; embeddings and search stay fully local either way.

---

## Docker

### Build

```bash
docker build -t loomem .
```

### Run

```bash
docker run -d \
  --name loomem \
  -p 3030:3030 \
  -v loomem-data:/data \
  -e OPENAI_API_KEY="sk-..." \
  -e LOOMEM_AUTH_TOKEN="your-secret-token" \
  loomem
```

The Dockerfile automatically:
- Binds to `0.0.0.0` (accessible from outside container)
- Sets data directory to `/data` (mount a volume here)

### Docker Compose

```yaml
version: '3.8'
services:
  loomem:
    build: .
    ports:
      - "3030:3030"
    volumes:
      - loomem-data:/data
    environment:
      - OPENAI_API_KEY=${OPENAI_API_KEY}
      - LOOMEM_AUTH_TOKEN=${LOOMEM_AUTH_TOKEN}
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:3030/health"]
      interval: 30s
      timeout: 10s
      retries: 3

volumes:
  loomem-data:
```

---

## Railway

### One-click deploy

1. Fork the repository
2. Connect it to Railway
3. Create new project → Deploy from GitHub repo
4. Add environment variables:
   - `OPENAI_API_KEY` — your OpenAI API key
   - `LOOMEM_AUTH_TOKEN` — admin token (generate a random string)
5. Add a volume mounted at `/data` for persistent storage
6. Deploy

### Configuration

If you deploy to Railway, add a `railway.toml` at the repo root (not shipped by default):

```toml
[build]
dockerfilePath = "Dockerfile"

[deploy]
healthcheckPath = "/health"
healthcheckTimeout = 30
restartPolicyType = "ON_FAILURE"
restartPolicyMaxRetries = 3
```

The `PORT` environment variable is automatically set by Railway and overrides `server.port` in config.

### Custom domain

After deployment, you can add a custom domain in Railway settings. Update `SERVER_ORIGIN` env var to match (required for OAuth).

---

## Container entrypoint and startup migrations

The container image starts via `scripts/docker-entrypoint.sh` (works on any container platform). It supports an optional one-time data migration before the server starts:

| Env var | Purpose |
|---|---|
| `LOOMEM_MIGRATE_GRAPH_STREAMS_ON_START` | Set to `1` to run `loomem-migrate --migrate-graph-entity-streams` before server start (re-stamps graph entities/edges whose stream disagrees with the chunks they reference). Unset = skip. |
| `LOOMEM_MIGRATE_GRAPH_STREAMS_COMMIT` | Set to `1` to actually write changes. Default is a dry run. |
| `LOOMEM_MIGRATE_GRAPH_STREAMS_MANIFEST_DIR` | Where migration manifests are written. Defaults to `/data/graph-migration-plans`. |

The migration is idempotent (already-correct entities are skipped) and creates a backup checkpoint before the first write when committing. After a successful migration, unset the `ON_START` flag and redeploy so subsequent restarts take the no-op fast path.

Other `loomem-migrate` subcommands (run manually, not via the entrypoint): `--validate-graph-entity-streams` and `--sample-embeddings`.

---

## Production checklist

### Security

- [ ] Set strong `LOOMEM_AUTH_TOKEN` (64+ random hex chars) — without it the server accepts every request (local passthrough mode)
- [ ] Move API keys to environment variables (not in config.toml)
- [ ] Review PII settings — ensure `pii.enabled = true`
- [ ] Consider enabling at-rest encryption (`LOOMEM_AT_REST_MASTER_KEY`) — see [SECURITY.md](SECURITY.md)
- [ ] Set up HTTPS (managed platforms provide this; Docker needs a reverse proxy)

If a secret leaks, rotate it: replace the env var value, restart the service, and verify with a request using the old key (must be rejected).

### Performance

- [ ] Mount `/data` on fast storage (SSD)
- [ ] Set `storage.rocksdb.write_buffer_size` based on available RAM
- [ ] Review `storage.tantivy.heap_size_mb` — more RAM = faster indexing
- [ ] Consider enabling `search.cache` for high-traffic deployments

### Reliability

- [ ] Enable `storage.intent_log.enabled = true` for crash recovery
- [ ] Set `storage.intent_log.sync_on_write = true` for durability
- [ ] Configure backup: `worker.backup.enabled = true`
- [ ] Set appropriate `resource_guards` for your environment
- [ ] Monitor with `GET /v1/status` endpoint

### Cost control

- [ ] Set `cost.daily_cap_usd` to prevent runaway LLM costs
- [ ] Set `cost.alert_threshold_usd` for early warnings
- [ ] Review `dream.cost_cap_usd_per_run` per dream session
- [ ] Monitor daily costs via RocksDB cost column family

---

## Connecting MCP clients

### Claude.ai (web)

1. Settings → Customize → Connectors
2. Add new connector
3. Enter URL: `https://your-loomem.app/mcp`
4. Complete OAuth — enter API key when prompted

### Claude Desktop

Edit `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "loomem": {
      "url": "https://your-loomem.app/mcp",
      "headers": {
        "Authorization": "Bearer loom_your_api_key"
      }
    }
  }
}
```

Restart Claude Desktop.

> The `url` form above is for a **remote instance reachable over HTTPS**. If Loomem
> runs **locally** over plain HTTP (`http://127.0.0.1:<port>`), the desktop app can't
> use a bare `http://localhost` URL — bridge it to stdio with
> `"command": "npx", "args": ["-y", "mcp-remote", "http://127.0.0.1:3030/mcp", "--allow-http"]`.
> See the [User Guide](user-guide.md#claude-desktop-macos--windows).

### Claude Code (CLI / VS Code)

Add to `.mcp.json` in your project or global config:

```json
{
  "mcpServers": {
    "loomem": {
      "url": "https://your-loomem.app/mcp",
      "headers": {
        "Authorization": "Bearer loom_your_api_key"
      }
    }
  }
}
```

---

## Logging

### Text logs (default)

```bash
RUST_LOG=debug ./target/release/loomem-server
```

### JSON logs (for Grafana / ELK)

```bash
LOOMEM_LOG_FORMAT=json ./target/release/loomem-server
```

---

## Backups

### Automatic

Enable in config:

```toml
[worker.backup]
enabled = true
interval_secs = 43200    # every 12 hours
max_copies = 2            # keep last 2 backups
```

Checkpoints are written to `{data_dir}/backups/checkpoint-<timestamp>/`. For an out-of-band backup, copy that directory (or snapshot the volume) while the server is running — checkpoints are consistent point-in-time copies. See [Backup and Restore](backup-and-restore.md) for details, including the extra requirements for encrypted instances.

### Restore

1. Stop Loomem
2. Replace `data/` directory with backup
3. Start Loomem — Tantivy index rebuilds automatically if schema mismatches

---

## Upgrading

1. Pull latest code: `git pull`
2. Build: `cargo build --release -p loomem-server`
3. Stop old instance
4. Start new binary

Loomem handles schema migrations automatically:
- RocksDB: backward-compatible (new fields get defaults)
- Tantivy: auto-rebuild on schema version mismatch
- Config: new keys are required — check `config.toml` after upgrade

---

## CI

GitHub Actions runs on every push/PR to `main`:
- `cargo check --workspace`
- `cargo test --workspace --lib`
- `cargo clippy -- -D warnings`
- `cargo fmt --check`

---

## Security notes

- **Never hardcode API keys in `config.toml`** — use `OPENAI_API_KEY` env var instead. The `api_key_env` field specifies which env var to read.
- `config_snapshot.toml` files (created by eval runs) are gitignored to prevent accidental secret leaks.
- All dependencies use permissive licenses (MIT, Apache-2.0, BSD, ISC).
