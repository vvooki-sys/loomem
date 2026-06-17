# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
