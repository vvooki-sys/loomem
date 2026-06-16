# macOS turnkey installer (Apple Silicon)

A single double-clickable file that installs Loomem with **no terminal, no API
key, nothing leaving the Mac**. It extracts the binaries and a local embedding
model to `~/.loomem`, runs the server as a background LaunchAgent (auto-start at
login), and connects Claude Code if present.

> Apple Silicon only. Intel/Linux/Windows are separate distribution targets.

## For the user — installing

1. Double-click `loomem-install-<version>-aarch64.command`.
2. macOS will warn it's from an unidentified developer (the build is not yet
   notarized). **Right-click the file → Open → Open** to approve it once.
3. A Terminal window shows the steps; when it says *"Loomem is running"* you're
   done. The server stays running in the background and restarts at login.

To connect Claude Code manually (if it wasn't auto-detected):

```sh
claude mcp add --transport http loomem http://127.0.0.1:3030/mcp
```

### Uninstalling

```sh
sh ~/.loomem/uninstall.command            # removes Loomem, keeps your memories
sh ~/.loomem/uninstall.command --purge    # also deletes ~/.loomem/data
```

### Troubleshooting

- **Nothing on `/health`** — check `~/.loomem/logs/stderr.log`. The first start
  loads the embedding model and can take a few seconds.
- **Port 3030 in use** — another Loomem (or service) holds the port. The
  installer binds the port from `~/.loomem/config.toml` (`[server].port`); edit
  it and reload: `launchctl bootout gui/$(id -u)/com.loomem.server && launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.loomem.server.plist`.
- **Service status** — `launchctl print gui/$(id -u)/com.loomem.server`.

## For the maintainer — building the installer

The installer is assembled from prebuilt **release** binaries plus the local
model. From a checkout on an Apple Silicon Mac:

```sh
# 1. release binaries (aarch64-apple-darwin)
cargo build --release -p loomem-server -p loomem-cli -p loomem-migrate

# 2. the local embedding model (int8, ~113 MB; sha256-verified)
./scripts/fetch-embedding-model.sh

# 3. bake the single-file installer
./scripts/make-macos-installer.sh \
    --bins  target/release \
    --model ~/.loomem/models/multilingual-e5-small \
    --out   loomem-install-$(git describe --tags --always)-aarch64.command
```

The result is one self-extracting `.command` (stub + gzipped payload: binaries,
config templates, the model, the LaunchAgent template, the uninstaller).

Pieces (in `scripts/`): `make-macos-installer.sh` (builder),
`macos-installer-stub.sh` (the head/logic), `com.loomem.server.plist.template`
(LaunchAgent), `macos-uninstall.command`.

Test against an isolated home/port/label without touching a real install:

```sh
LOOMEM_HOME=/tmp/loomem-test LOOMEM_PORT=3099 LOOMEM_AGENT_LABEL=com.loomem.test \
  sh loomem-install-<version>-aarch64.command
# cleanup:
launchctl bootout gui/$(id -u)/com.loomem.test
rm -rf /tmp/loomem-test ~/Library/LaunchAgents/com.loomem.test.plist
```

> **Not yet signed/notarized** — distribution relies on the one-time
> right-click→Open. Developer-ID codesign + notarization is a later cycle and
> will remove that prompt.
