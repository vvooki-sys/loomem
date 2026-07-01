# Installation Guide

Loomem ships as three static binaries — `loomem-server`, `loomem-cli`, `loomem-migrate` — with no runtime dependencies (RocksDB and Tantivy are compiled in). This guide covers every supported install path, verification, upgrades, and troubleshooting.

## Supported platforms

| Platform | Target | Prebuilt binary |
|---|---|---|
| macOS Apple Silicon | `aarch64-apple-darwin` | ✅ |
| macOS Intel | `x86_64-apple-darwin` | ✅ |
| Linux x86_64 (glibc ≥ 2.35: Ubuntu 22.04+, Debian 12+, Fedora 36+) | `x86_64-unknown-linux-gnu` | ✅ |
| Linux ARM64 (glibc ≥ 2.35) | `aarch64-unknown-linux-gnu` | ✅ |
| Anything else (musl, BSD, Windows/WSL2…) | — | build [from source](#from-source) |

Requirements for the installer: `sh`, `tar`, and `curl` or `wget`. No sudo — everything lands in your home directory.

## Quick install

The repository is public, so the one-liner needs no authentication:

```bash
curl -fsSL https://raw.githubusercontent.com/vvooki-sys/loomem/main/install.sh | sh
```

The installer defaults to port **3030** and asks for a free one if it's taken; set `LOOMEM_PORT=NNNN` to pick a port non-interactively.

<details>
<summary>Installing from a <strong>private fork</strong> (authenticated)</summary>

If your copy of the repo is private, authenticate with the [GitHub CLI](https://cli.github.com):

```bash
gh auth login            # once
gh api repos/<owner>/loomem/contents/install.sh -H "Accept: application/vnd.github.raw" | sh
```

No `gh`? A personal access token with `repo` scope works too:

```bash
export GH_TOKEN=ghp_xxx
curl -fsSL -H "Authorization: Bearer $GH_TOKEN" -H "Accept: application/vnd.github.raw" \
  https://api.github.com/repos/<owner>/loomem/contents/install.sh | sh
```

</details>

### What the installer does

1. Detects OS and architecture (`uname`).
2. Picks a download source, first match wins: `LOOMEM_BASE_URL` mirror → `GH_TOKEN`/`GITHUB_TOKEN` (GitHub API) → logged-in `gh` CLI → public release URLs.
3. Resolves the latest release (or the tag pinned with `LOOMEM_VERSION`).
4. Downloads the platform archive and `SHA256SUMS`, **verifies the checksum** (hard failure on mismatch).
5. Installs the three binaries to `~/.loomem/bin`.
6. Copies config templates (`config.toml`, `entities.toml.example`, `synonyms.toml.example`) to `~/.loomem` and seeds `entities.toml` from the example (the server requires it at startup) — **never overwrites existing files**, so re-running is safe.
7. Prints PATH instructions if `~/.loomem/bin` isn't on your PATH yet.

### Installer environment variables

| Variable | Default | Purpose |
|---|---|---|
| `LOOMEM_VERSION` | latest release | Pin a tag, e.g. `v0.2.0`. Required for prereleases (`v0.2.0-rc1`), which are excluded from "latest". |
| `LOOMEM_INSTALL_DIR` | `~/.loomem/bin` | Where binaries go. |
| `LOOMEM_CONFIG_DIR` | `~/.loomem` | Where config templates go. |
| `LOOMEM_PORT` | `3030` | Bind port written into `config.toml`; skips the interactive prompt and auto-bumps to the next free port if taken. |
| `GH_TOKEN` / `GITHUB_TOKEN` | — | GitHub token for downloads from a private fork. |
| `LOOMEM_BASE_URL` | — | Fetch archives from a plain HTTP(S) mirror instead of GitHub (requires `LOOMEM_VERSION`). |

Example — pin a version into a custom prefix:

```bash
LOOMEM_VERSION=v0.2.0 LOOMEM_INSTALL_DIR=/opt/loomem/bin sh install.sh
```

## PATH setup

The installer doesn't modify your shell config. Add once:

```bash
echo 'export PATH="$HOME/.loomem/bin:$PATH"' >> ~/.zshrc   # or ~/.bashrc
exec $SHELL
```

## First run

```bash
cd ~/.loomem
loomem-server
```

The server **requires** `config.toml` and `entities.toml` in its working directory — the installer seeds both, so running from `~/.loomem` just works. Edit `entities.toml` to teach Loomem your people/projects/aliases (see the comments inside).

The server listens on `http://127.0.0.1:3030` and stores data in `./data` (relative to the working directory — running from `~/.loomem` keeps data in `~/.loomem/data`). Verify:

```bash
curl http://localhost:3030/health
```

Then connect an MCP client — recipes for Claude Code, claude.ai, ChatGPT, and OpenClaw are in the [README](../README.md#connect-an-mcp-client).

**Security:** authentication is off by default, fine for localhost. If the server is reachable by anyone but you, set `LOOMEM_AUTH_TOKEN` (see [SECURITY.md](SECURITY.md) and [deployment.md](deployment.md)).

## Verifying checksums manually

The installer verifies automatically, but to audit by hand:

```bash
gh release download v0.2.0 --repo vvooki-sys/loomem \
  --pattern 'loomem-0.2.0-aarch64-apple-darwin.tar.gz' --pattern 'SHA256SUMS'
shasum -a 256 -c --ignore-missing SHA256SUMS    # macOS
sha256sum -c --ignore-missing SHA256SUMS        # Linux
```

## Upgrading

Re-run the installer — binaries are replaced, your config files are left untouched:

```bash
gh api repos/vvooki-sys/loomem/contents/install.sh -H "Accept: application/vnd.github.raw" | sh
```

Check the [CHANGELOG](../CHANGELOG.md) for breaking changes first. If a release notes a storage-format migration, stop the server and run `loomem-migrate` before starting the new version.

## Uninstalling

Loomem touches nothing outside its two directories:

```bash
rm -rf ~/.loomem          # binaries + config + data (if run from ~/.loomem)
```

Remove the PATH line from your shell config if you added one. If you ran the server from another working directory, its `./data` lives there.

## From source

```bash
git clone https://github.com/vvooki-sys/loomem.git
cd loomem
cp entities.toml.example entities.toml
cargo build --release -p loomem-server -p loomem-cli -p loomem-migrate
# binaries in target/release/
```

Requires Rust (stable) and libclang for the RocksDB build: `apt install libclang-dev` on Debian/Ubuntu; on macOS it ships with the Xcode Command Line Tools.

## Docker

```bash
docker build -t loomem .
export LOOMEM_AUTH_TOKEN=$(openssl rand -hex 32)
export LOOMEM_AT_REST_MASTER_KEY=$(openssl rand -base64 32)
docker run -p 3030:3030 -v loomem-data:/data \
  -e LOOMEM_AUTH_TOKEN -e LOOMEM_AT_REST_MASTER_KEY loomem
```

The image binds `0.0.0.0` and therefore requires an auth token and (by default) an at-rest master key — the server refuses to start without them. Deliberate opt-outs: `LOOMEM_ALLOW_UNAUTH=1`, `LOOMEM_AT_REST_EXPECT_ENABLED=0`. See [deployment.md](deployment.md) for reverse-proxy, TLS, and cloud options, and [SECURITY.md](SECURITY.md) for the security model.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `could not determine the latest release` | The only release is a prerelease (excluded from "latest") → pin it with `LOOMEM_VERSION=v0.2.0-rc1`. Installing from a **private fork**? Authenticate first: `gh auth login` or set `GH_TOKEN`. |
| `download failed: … If this repo is private…` | Network/transient failure on the public repo — re-run. From a **private fork**, the installer can't see release assets without auth (`gh auth login` / `GH_TOKEN`). |
| `checksum mismatch` | Corrupted or tampered download. Re-run; if it persists, **don't install** — compare against `SHA256SUMS` on the releases page and open an issue. |
| `WARNING: SHA256SUMS not available` | The release has no checksum file (shouldn't happen for official releases). Install proceeds unverified — treat with suspicion. |
| `unsupported OS/architecture` | No prebuilt binary for your platform — [build from source](#from-source). |
| `version 'GLIBC_2.35' not found` (or similar) | Your distro's glibc is older than the build baseline — [build from source](#from-source) or use [Docker](#docker). |
| `loomem-server: command not found` after install | `~/.loomem/bin` not on PATH — see [PATH setup](#path-setup). |
| `gh: Not Found (HTTP 404)` on the `gh api` one-liner | Your GitHub account has no access to the private repo, or you're logged into the wrong account (`gh auth status`). |
| `Error: Failed to load entities file (entities.toml)` | The server needs `entities.toml` in its working directory — `cp ~/.loomem/entities.toml.example ~/.loomem/entities.toml` and run from `~/.loomem`. |
| Server starts but `curl /health` fails | First start builds indexes (can take a few seconds). Port 3030 taken? Check the server log; the bind address/port live in `config.toml`. |
