# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
