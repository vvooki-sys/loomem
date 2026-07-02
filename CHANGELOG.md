# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.0] - 2026-07-01

Security hardening release — implements items 1–7 of the 2026-07-01 security
& performance audit (PR #32).

### Security

- **Startup fail-safe: an unauthenticated non-loopback bind now refuses to start.** The Docker image binds `0.0.0.0`, so a container without `LOOMEM_AUTH_TOKEN` used to expose the full admin API; it now fails fast with an actionable error. Deliberate opt-out: `LOOMEM_ALLOW_UNAUTH=1` (e.g. behind an authenticating reverse proxy). (#32)
- **No more silent plaintext at rest.** A missing master key logs a prominent PLAINTEXT warning, and the Docker image sets `LOOMEM_AT_REST_EXPECT_ENABLED=1` — networked deployments must provide `LOOMEM_AT_REST_MASTER_KEY` or explicitly opt out. (#32)
- **Per-stream rate limiting on hot paths.** The previously dead `RateLimiter` is wired into `/v1`/`/api` (429 + `Retry-After`) and the MCP dispatcher (tool error): `[rate_limit]` in `config.toml`, off by default, enabled in the Docker image. Applies to every caller — no admin exemption (single-user: everyone is admin). (#32)
- **Constant-time bearer-token comparison** (`subtle::ConstantTimeEq`) closes a theoretical timing side-channel on the API key. (#32)
- **Master key and cached DEKs are zeroized** on provider drop, FIFO eviction, and `flush_cache` (defense-in-depth for after-teardown memory). (#32)

### Added

- Vector search now warns loudly when stored embeddings mismatch the query dimension (silent BM25-only degradation before), naming the `--reembed` remedy. (#32)

### Changed

- **HTTP connection-pool settings for the shared OpenAI client are now config-driven** (`[llm].pool_max_idle_per_host`, `pool_idle_timeout_secs`, `tcp_keepalive_secs`) instead of hardcoded, and a single `LlmConfig::build_http_client` is used by both the server and the CLI re-embed path. The idle pool is bounded and actively recycled so long-running instances don't accumulate stale keep-alive sockets (silent ingest degradation). **Note:** the idle-timeout/keep-alive defaults are 30s/30s — more aggressive recycling than the previous hardcoded 90s/60s (#24); deployments upgrading without the new config keys will pick up the shorter intervals via `#[serde(default)]`. (#29)
- Extraction now separates a successful-but-empty result (2xx, zero facts) from a real transport/parse failure: a windowed `extraction_empty` counter (surfaced in `memory_status` / `llm_failures_recent`, excluded from the failure total) plus a debug log carrying HTTP status and response body length, so silent "Extracted 0 facts" degradation is visible. (#29)

## [0.4.1] - 2026-06-23

### Security

- **MCP callers can no longer self-elevate trust.** `memory_store` derived the trust tier from the caller-supplied `source` with no authority check (introduced in 0.4.0, #9), so any client — including a prompt-injected LLM — could request full-trust `a1`. MCP writes are now clamped to at most `a2` unless the instance opts in via the new `server.honor_caller_trust_source` setting (default `false`). Single-user/dogfood instances can enable it; cloud/multi-client stays safe by default. (#12)

### Fixed

- `memory_search` now emits a `[trust:?]` sentinel when a result's chunk cannot be loaded (read error / index-vs-store desync), instead of silently dropping the trust tag. (#12)
- Filled `provenance_role` in integration-test fixtures missed by #6, so `cargo test -p <crate>` compiles cleanly. (#12)

## [0.4.0] - 2026-06-23

### Added

- **`relations` parameter on `memory_store`** — callers can supply explicit subject/relation/object triples that are injected as deterministic graph edges (graph-only, capped, aliases resolved from `entities.toml`), instead of relying solely on extraction. (#10)
- **MCP trust tier derived from source** — `memory_store` now derives the trust level from the write source rather than hardcoding it, and `memory_search` surfaces each result's trust tier and score multiplier, so full-trust memories visibly outrank derived ones. (#9)

### Fixed

- **Consolidated chunks are now embedded** — both background consolidation (`L1:` chunks) and `memory_dream` (`dream:` chunks) route the merged chunk through the configured embedder / shared embedding queue, so consolidated facts receive a vector instead of being stuck permanently "pending" in `memory_status` (retrievable by BM25 only). The embedder is selected the same way as everywhere else (local-first, OpenAI fallback), keeping the vector dimension consistent with the index. (#7)
- **`/v1/embed-missing` back-fill works without an OpenAI key** — the back-fill now embeds through the configured embedder and requires an API key only when no local embedder is available, so chunks left without a vector (e.g. previously-stuck consolidated chunks) can be recovered on keyless/local instances. (#8, #11)

## [0.3.0] - 2026-06-23

### Added

- **MemIR trust-provenance scoring** — chunks now carry a `ProvenanceRole` (`Claim`/`Evidence`/`Cue`, default `Claim`), and hybrid retrieval applies a trust + provenance multiplier to the fused score before ranking, so full-trust (`a1`) memories outrank derived (`a2`) ones for equally relevant content. Backward-compatible: existing databases deserialize unchanged via `#[serde(default)]`, no migration. (#6)
- **Auto-dream consolidation trigger** — opt-in background consolidation fires after a configurable number of new chunks per stream; the configured shared stream is excluded and a cooldown prevents thrashing. (#3)
- **`chunk_id` exposed in search results** plus per-stream embedding readiness in `memory_status`, so callers can trace, rate, and inspect specific chunks. (#4)
- **Environment overrides** for the embedding provider/dimension and the consolidation interval, easing per-deployment configuration.
- **Branded OAuth authorize page** rendered in the Loomem layout.

### Fixed

- Orphan entity nodes are pruned on `memory_delete`, keeping the entity graph GDPR-correct.
- Auto-dream is opt-in and excludes the configured shared stream.
- Cheaper embedding-existence probe and a standalone id suffix in search results.

### Changed

- Bumped `quinn-proto` and `memmap2` to address RUSTSEC-2026-0185 and -0186.
- Added `CLAUDE.md` + agent-conventions for AI code generators; refreshed landing/Quickstart/ChatGPT-Apps documentation.

## [0.2.2] - 2026-06-17

### Added

- **Custom extraction topics are filterable** — operator-configured `[knowledge_extraction].topics` keys (e.g. `risk_item`, `contact`) are preserved in the new `ExtractionMeta.topic` field instead of silently collapsing to `fact`, and the `/v1/search` `fact_type` filter matches them. (Greptile P1)

### Fixed

- **Consolidation prompt** now advertises the `experience` type, so procedural lessons keep `FactType::Experience` through L1 consolidation instead of being relabeled `event`/`fact`/`preference`. (Greptile P1)
- **Docs** — added Codex to the "Which LLM clients" FAQ (README + landing page, visible and JSON-LD) to match the hero/persona/architecture lists; made the installation guide public-first and demoted the private-fork `gh`/`GH_TOKEN` flow to an optional note. (Greptile P2)

## [0.2.1] - 2026-06-17

### Added

- **Native stdio↔HTTP MCP bridge** — `loomem-cli mcp-stdio --url http://127.0.0.1:<port>` proxies a stdio MCP client (the Claude desktop app, Cowork, Cursor, …) to the server's HTTP `/mcp` endpoint, with **no Node/npx required** (a drop-in replacement for `npx mcp-remote`). It reads newline-delimited JSON-RPC on stdin, POSTs each message to `/mcp`, captures and echoes the `mcp-session-id`, and writes replies to stdout; notifications get no reply. `--url` is now a global flag.
- **Installer port selection** — `install.sh` and the macOS turnkey installer detect a busy port, suggest a free one, and prompt for it (works under `curl | sh` via `/dev/tty`); set `LOOMEM_PORT` to skip the prompt. The chosen port is written into `config.toml`.

### Fixed

- **Robust `/dev/tty` probe** in the installers: the port prompt no longer prints a spurious error when there is no controlling terminal (CI, pipes), and cleanly falls back to auto-selecting a free port.

### Changed

- **Docs** — the Claude desktop app / Cowork connection is now documented as a stdio bridge (native `loomem-cli mcp-stdio` recommended, `mcp-remote` as the universal fallback) across the README, landing page, quick-start, and user guide, clarifying that the desktop app cannot use a bare `http://localhost` connector.

## [0.2.0] - 2026-06-16

First public release.

### Removed

- **Dashboard** (cycle/004): the React web UI, `/api/dashboard/*` REST endpoints, static file serving, and the Node build stage. Loomem is now a headless engine (MCP + REST + CLI). The dashboard will return as a separate project.
- **File registry** (cycle/005): the 8 `file_*` MCP tools, `/v1/files*` REST endpoints, the `loomem-parsers` crate, memory↔file links (`file_refs`), and the `doc_abstract` search signal. MCP now exposes 14 `memory_*` tools. Existing databases remain readable (the removed fields are ignored on deserialization).

### Added

- **Release pipeline** (cycle/006): tagged releases (`v*`) publish prebuilt binaries for Linux (x86_64, aarch64) and macOS (aarch64, x86_64) with SHA256SUMS.
- **`install.sh`** (cycle/006): one-line installer for prebuilt binaries (no sudo, `~/.loomem` by default).
- README: install matrix and MCP client recipes (Claude Code, claude.ai remote connector, ChatGPT custom connector, OpenClaw).
- **Private-repo install support** (cycle/009): `install.sh` picks a download source automatically — `LOOMEM_BASE_URL` mirror → `GH_TOKEN`/`GITHUB_TOKEN` (GitHub API assets) → logged-in `gh` CLI → public URLs. Verified end-to-end against a simulated release.
- **Docs** (cycle/009): full [installation guide](docs/installation.md) (verify, PATH, upgrade, uninstall, troubleshooting) and a maintainer [release runbook](docs/release-runbook.md).
- **Local embeddings, keyless by default** (cycle/010): fresh installs compute embeddings on-device via a bundled ONNX model (`multilingual-e5-small`, 384-dim) using the pure-Rust `tract` runtime — no API key, nothing leaves the machine. New `[llm].embedding_provider` (`"local"`/`"openai"`) separates embeddings from the completions provider; `embedding_model_path` and `scripts/fetch-embedding-model.sh` (sha256-verified) manage the model. The completions LLM stays optional (regex fallback without a key).
- **Embedding-dimension guard** (cycle/010): the server records the dimension a database was built with and refuses to start on a mismatch (e.g. switching provider/model) instead of silently mixing vector sizes.
- **`loomem-server --reembed`** (cycle/010): offline maintenance mode that recomputes all vectors with the configured provider and records the new dimension.
- **macOS turnkey installer** (cycle/011): a self-extracting single-file `.command` for Apple Silicon — no terminal, no API key. Extracts binaries + the int8 local embedding model (~113 MB) to `~/.loomem`, runs the server as a background LaunchAgent (auto-start at login), and wires up Claude Code. Builder `scripts/make-macos-installer.sh`, stub, LaunchAgent template, and `~/.loomem/uninstall.command` (keeps `data/` by default). See [docs/macos-install.md](docs/macos-install.md). Not yet codesigned/notarized — install uses a one-time right-click→Open.

### Changed

- **Default embedding provider is now `local`** (cycle/010), with `embedding_dim = 384`. **Breaking for existing databases:** a database built with OpenAI embeddings (1536-dim) will refuse to start under the new default until re-embedded (`loomem-server --reembed`) or until the config is set back to `embedding_provider = "openai"` / `embedding_dim = 1536`. Configs that predate the `embedding_provider` key are read as `"openai"`, so in-place upgrades keep their current behavior unless the config is changed.

### Fixed

- **Release workflow** (cycle/009): Intel macOS builds moved from the retired `macos-13` runner to `macos-15-intel` (would have failed on first live run); Linux builds pinned to `ubuntu-22.04` so binaries run on glibc ≥ 2.35 (24.04 builds required `GLIBC_2.39`, verified live with v0.2.0-rc1); checksums use `sha256sum` when available; `-rc`/`-beta`/`-alpha` tags are published as prereleases so unpinned installs never receive them.
- **First run** (cycle/009): the installer seeds `entities.toml` from the example — the server requires it at startup, so a fresh install now boots without manual steps.

### Changed

- `scripts/railway-entrypoint.sh` renamed to `scripts/docker-entrypoint.sh`; `railway.toml` removed from the repo (inline example remains in the deployment docs).
- `synonyms.toml` is now gitignored (personal vocabulary); a generic `synonyms.toml.example` ships instead.
