#!/bin/sh
# Loomem turnkey installer for macOS (Apple Silicon) — self-extracting.
#
# This stub is the head of the generated `loomem-install-<ver>-aarch64.command`.
# Everything after the __LOOMEM_PAYLOAD_BELOW__ marker is a gzipped tar holding
# the binaries, config templates, the local embedding model, and the LaunchAgent
# template. Double-clicking the .command in Finder runs this in Terminal.
#
# What it does, no terminal knowledge required:
#   1. checks this is an Apple Silicon Mac
#   2. extracts everything to ~/.loomem (override: LOOMEM_HOME)
#   3. clears the Gatekeeper quarantine flag on the unsigned binaries
#   4. installs a LaunchAgent so the server runs in the background at login
#   5. wires up the Claude Code client if present, else prints the snippet
#   6. confirms the server answers on http://127.0.0.1:<port>/health
#
# Re-runnable: never overwrites an existing config; reloads the agent.

set -eu

LOOMEM_HOME="${LOOMEM_HOME:-$HOME/.loomem}"
BIN_DIR="$LOOMEM_HOME/bin"
MODELS_DIR="$LOOMEM_HOME/models"
LOG_DIR="$LOOMEM_HOME/logs"
if [ -n "${LOOMEM_PORT:-}" ]; then PORT="$LOOMEM_PORT"; LOOMEM_PORT_SET=1; else PORT=3030; fi
LABEL="${LOOMEM_AGENT_LABEL:-com.loomem.server}"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"

say()  { printf '%s\n' "$*"; }
ok()   { printf '  \033[32m✓\033[0m %s\n' "$*"; }
warn() { printf '  \033[33m!\033[0m %s\n' "$*"; }
fail() { printf '\nLoomem installer: %s\n' "$*" >&2; exit 1; }

port_in_use() {  # $1 = port — return 0 if something is already listening
  lsof -nP -iTCP:"$1" -sTCP:LISTEN >/dev/null 2>&1
}
first_free_port() {  # $1 = starting port
  p="$1"
  while port_in_use "$p"; do p=$((p + 1)); [ "$p" -gt 65000 ] && break; done
  printf '%s' "$p"
}
# Resolve $PORT: honor $LOOMEM_PORT non-interactively, else suggest a free port
# (avoiding conflicts with anything already listening) and ask in the Terminal.
choose_port() {
  if [ -n "${LOOMEM_PORT_SET:-}" ]; then
    port_in_use "$PORT" && warn "port $PORT is in use — the server may fail to start."
    return
  fi
  # Respect an existing install's port as the default (re-run safe).
  if [ -f "$LOOMEM_HOME/config.toml" ]; then
    cur="$(sed -n 's/^[[:space:]]*port[[:space:]]*=[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$LOOMEM_HOME/config.toml" 2>/dev/null | head -n1)"
    [ -n "${cur:-}" ] && PORT="$cur"
  fi
  suggested="$PORT"
  port_in_use "$PORT" && suggested="$(first_free_port "$PORT")"
  if [ -r /dev/tty ] && [ -w /dev/tty ]; then
    if [ "$suggested" != "$PORT" ]; then
      printf 'Port %s is in use. Port for Loomem [%s]: ' "$PORT" "$suggested" > /dev/tty
    else
      printf 'Port for Loomem [%s]: ' "$suggested" > /dev/tty
    fi
    read ans < /dev/tty || ans=""
    PORT="${ans:-$suggested}"
    if port_in_use "$PORT"; then
      alt="$(first_free_port "$PORT")"
      printf 'Port %s is in use; using %s instead.\n' "$PORT" "$alt" > /dev/tty
      PORT="$alt"
    fi
  else
    PORT="$suggested"
  fi
  case "$PORT" in ''|*[!0-9]*) fail "invalid port: '$PORT'" ;; esac
}

say ""
say "Loomem installer"
say "================"

# --- 1. platform guard -------------------------------------------------------
[ "$(uname -s)" = "Darwin" ] || fail "this installer is for macOS only."
[ "$(uname -m)" = "arm64" ] || fail "this build is for Apple Silicon (arm64) Macs only."

# --- 2. extract payload ------------------------------------------------------
SELF="$0"
marker_line="$(awk '/^__LOOMEM_PAYLOAD_BELOW__$/ { print NR + 1; exit }' "$SELF")"
[ -n "${marker_line:-}" ] || fail "corrupt installer (payload marker not found)."

mkdir -p "$LOOMEM_HOME" "$BIN_DIR" "$MODELS_DIR" "$LOG_DIR"
say ""
say "Installing to $LOOMEM_HOME ..."
tail -n +"$marker_line" "$SELF" | tar xzf - -C "$LOOMEM_HOME" || fail "extraction failed."
ok "files extracted"

# Seed config templates without clobbering existing ones (re-run safe).
for f in config.toml entities.toml.example synonyms.toml.example; do
  [ -f "$LOOMEM_HOME/_seed/$f" ] || continue
  if [ ! -e "$LOOMEM_HOME/$f" ]; then
    cp "$LOOMEM_HOME/_seed/$f" "$LOOMEM_HOME/$f"
  fi
done
# entities.toml is required at server start.
if [ ! -e "$LOOMEM_HOME/entities.toml" ] && [ -f "$LOOMEM_HOME/_seed/entities.toml.example" ]; then
  cp "$LOOMEM_HOME/_seed/entities.toml.example" "$LOOMEM_HOME/entities.toml"
fi
# Ask which port to use (suggests a free one if the default is taken), then keep
# the server's bind port in sync with what we health-check and advertise
# (the server reads [server].port from config.toml, not the environment).
choose_port
if [ -f "$LOOMEM_HOME/config.toml" ]; then
  sed -i '' "s/^[[:space:]]*port[[:space:]]*=.*/port = $PORT/" "$LOOMEM_HOME/config.toml" 2>/dev/null || true
fi
ok "config ready on port $PORT (embeddings run locally — no API key needed)"

# --- 3. clear Gatekeeper quarantine on the unsigned binaries -----------------
# The installer itself was approved by the user (right-click → Open); we can
# now clear the flag on what we extracted so the agent can launch them.
xattr -dr com.apple.quarantine "$BIN_DIR" 2>/dev/null || true
chmod +x "$BIN_DIR"/loomem-* 2>/dev/null || true
ok "binaries unblocked"

# --- 4. install + load the LaunchAgent ---------------------------------------
mkdir -p "$HOME/Library/LaunchAgents"
sed -e "s|@LABEL@|$LABEL|g" \
    -e "s|@BIN@|$BIN_DIR/loomem-server|g" \
    -e "s|@WORKDIR@|$LOOMEM_HOME|g" \
    -e "s|@LOG@|$LOG_DIR|g" \
    "$LOOMEM_HOME/_seed/com.loomem.server.plist.template" > "$PLIST"

launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
if launchctl bootstrap "gui/$(id -u)" "$PLIST" 2>/dev/null; then
  ok "background service installed (starts at login)"
else
  # Fallback for older macOS
  launchctl load "$PLIST" 2>/dev/null && ok "background service installed" \
    || warn "could not load the LaunchAgent automatically (plist at $PLIST)"
fi

# --- 5. wait for health ------------------------------------------------------
say ""
printf "Starting Loomem"
i=0
until curl -fsS -m 2 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1; do
  i=$((i + 1)); printf "."
  [ "$i" -ge 45 ] && { say ""; warn "still starting — give it a few more seconds; if it stays down see $LOG_DIR/stderr.log"; break; }
  sleep 1
done
if curl -fsS -m 2 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1; then
  say ""; ok "Loomem is running on http://127.0.0.1:$PORT"
fi

# --- 6. wire up the AI client ------------------------------------------------
say ""
if command -v claude >/dev/null 2>&1; then
  if claude mcp add --transport http loomem "http://127.0.0.1:$PORT/mcp" >/dev/null 2>&1; then
    ok "connected to Claude Code"
  else
    warn "Claude Code found but auto-connect failed — run this yourself:"
    say  "    claude mcp add --transport http loomem http://127.0.0.1:$PORT/mcp"
  fi
else
  say "To connect your AI client, add this MCP server:"
  say "    URL:  http://127.0.0.1:$PORT/mcp"
  say "    (Claude Code:  claude mcp add --transport http loomem http://127.0.0.1:$PORT/mcp)"
fi

# The Claude *desktop app* (and Cowork) speak stdio, not HTTP, and reject a bare
# http://localhost connector — so they need an mcp-remote bridge regardless of
# whether Claude Code was wired up above.
say ""
say "Using the Claude desktop app or Cowork? It needs a stdio bridge (requires"
say "Node/npx). Add this to ~/Library/Application Support/Claude/claude_desktop_config.json"
say "and restart Claude:"
say "    {\"mcpServers\":{\"loomem\":{\"command\":\"npx\",\"args\":[\"-y\",\"mcp-remote\",\"http://127.0.0.1:$PORT/mcp\",\"--allow-http\"]}}}"

say ""
say "Done. Loomem remembers across conversations, fully on this Mac."
say "Uninstall any time:  sh $LOOMEM_HOME/uninstall.command"
say ""
exit 0

__LOOMEM_PAYLOAD_BELOW__
