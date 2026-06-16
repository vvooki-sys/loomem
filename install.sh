#!/bin/sh
# Loomem installer — downloads a release binary for your platform.
#
# Public repo:
#   curl -fsSL https://raw.githubusercontent.com/vvooki-sys/loomem/main/install.sh | sh
#
# Private repo (requires GitHub CLI, https://cli.github.com):
#   gh api repos/vvooki-sys/loomem/contents/install.sh -H "Accept: application/vnd.github.raw" | sh
#
# Download source is picked automatically, first match wins:
#   1. LOOMEM_BASE_URL          — plain HTTP(S) mirror (also used for testing);
#                                 requires LOOMEM_VERSION
#   2. GH_TOKEN / GITHUB_TOKEN  — authenticated GitHub API (works on private repos)
#   3. gh CLI (logged in)       — `gh release download` (works on private repos)
#   4. public GitHub release URLs
#
# Environment overrides:
#   LOOMEM_INSTALL_DIR  — install location (default: ~/.loomem/bin)
#   LOOMEM_CONFIG_DIR   — config location (default: ~/.loomem)
#   LOOMEM_VERSION      — specific version tag, e.g. v0.2.0 (default: latest)
#   LOOMEM_BASE_URL     — fetch archives from this base URL instead of GitHub
#
# No sudo required for the default install location.

set -eu

REPO="vvooki-sys/loomem"
INSTALL_DIR="${LOOMEM_INSTALL_DIR:-$HOME/.loomem/bin}"
CONFIG_DIR="${LOOMEM_CONFIG_DIR:-$HOME/.loomem}"
GH_API="https://api.github.com"

say()  { printf '%s\n' "$*"; }
fail() { printf 'install.sh: error: %s\n' "$*" >&2; exit 1; }

# --- prerequisites -----------------------------------------------------------
if command -v curl >/dev/null 2>&1; then
  HTTP=curl
elif command -v wget >/dev/null 2>&1; then
  HTTP=wget
else
  fail "need curl or wget"
fi

# fetch <url> [token]           — print body to stdout
# fetch_to <url> <dest> [token] — save body to file (Accept: octet-stream)
fetch() {
  if [ "$HTTP" = curl ]; then
    if [ -n "${2:-}" ]; then
      curl -fsSL -H "Authorization: Bearer $2" "$1"
    else
      curl -fsSL "$1"
    fi
  else
    if [ -n "${2:-}" ]; then
      wget -qO- --header="Authorization: Bearer $2" "$1"
    else
      wget -qO- "$1"
    fi
  fi
}
fetch_to() {
  if [ "$HTTP" = curl ]; then
    if [ -n "${3:-}" ]; then
      curl -fsSL -H "Authorization: Bearer $3" -H "Accept: application/octet-stream" -o "$2" "$1"
    else
      curl -fsSL -o "$2" "$1"
    fi
  else
    if [ -n "${3:-}" ]; then
      wget -qO "$2" --header="Authorization: Bearer $3" --header="Accept: application/octet-stream" "$1"
    else
      wget -qO "$2" "$1"
    fi
  fi
}

command -v tar >/dev/null 2>&1 || fail "need tar"

# --- platform detection ------------------------------------------------------
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)  os_part="unknown-linux-gnu" ;;
  Darwin) os_part="apple-darwin" ;;
  *) fail "unsupported OS: $OS (build from source: cargo build --release)" ;;
esac

case "$ARCH" in
  x86_64|amd64)  arch_part="x86_64" ;;
  arm64|aarch64) arch_part="aarch64" ;;
  *) fail "unsupported architecture: $ARCH (build from source: cargo build --release)" ;;
esac

TARGET="${arch_part}-${os_part}"

# --- pick a download source --------------------------------------------------
TOKEN="${GH_TOKEN:-${GITHUB_TOKEN:-}}"
if [ -n "${LOOMEM_BASE_URL:-}" ]; then
  SOURCE="base_url"
  [ -n "${LOOMEM_VERSION:-}" ] || fail "LOOMEM_BASE_URL requires LOOMEM_VERSION (e.g. LOOMEM_VERSION=v0.2.0)"
elif [ -n "$TOKEN" ]; then
  SOURCE="token"
elif command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
  SOURCE="gh"
else
  SOURCE="public"
fi

# --- resolve version ---------------------------------------------------------
resolve_latest_tag() {
  case "$SOURCE" in
    token)
      fetch "${GH_API}/repos/${REPO}/releases/latest" "$TOKEN" \
        | grep '"tag_name"' | head -n1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/'
      ;;
    gh)
      gh release view --repo "$REPO" --json tagName --jq .tagName 2>/dev/null
      ;;
    public)
      fetch "${GH_API}/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' | head -n1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/'
      ;;
  esac
}

if [ -n "${LOOMEM_VERSION:-}" ]; then
  TAG="$LOOMEM_VERSION"
else
  TAG="$(resolve_latest_tag || true)"
  if [ -z "$TAG" ]; then
    if [ "$SOURCE" = "public" ]; then
      fail "could not determine the latest release. If this repo is private, log in with 'gh auth login' or set GH_TOKEN; otherwise pin a version with LOOMEM_VERSION=vX.Y.Z"
    fi
    fail "could not determine the latest release via ${SOURCE} (set LOOMEM_VERSION=vX.Y.Z to pin one)"
  fi
fi

VERSION="${TAG#v}"
PKG="loomem-${VERSION}-${TARGET}"

say "Installing loomem ${TAG} (${TARGET}) to ${INSTALL_DIR} [source: ${SOURCE}]"

# --- download ----------------------------------------------------------------
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# download_asset <filename> <dest> — non-zero exit if unavailable
download_asset() {
  case "$SOURCE" in
    base_url)
      fetch_to "${LOOMEM_BASE_URL%/}/$1" "$2"
      ;;
    token)
      # Private-repo downloads must go through the release-assets API endpoint.
      if [ ! -s "$TMP/.release.json" ]; then
        fetch "${GH_API}/repos/${REPO}/releases/tags/${TAG}" "$TOKEN" > "$TMP/.release.json" || return 1
      fi
      asset_id="$(awk -v want="$1" '
        /"id":/   { id = $2; gsub(/[^0-9]/, "", id); last_id = id }
        /"name":/ { name = $2; gsub(/[",]/, "", name); if (name == want) { print last_id; exit } }
      ' "$TMP/.release.json")"
      [ -n "$asset_id" ] || return 1
      fetch_to "${GH_API}/repos/${REPO}/releases/assets/${asset_id}" "$2" "$TOKEN"
      ;;
    gh)
      gh release download "$TAG" --repo "$REPO" --pattern "$1" --output "$2"
      ;;
    public)
      fetch_to "https://github.com/${REPO}/releases/download/${TAG}/$1" "$2"
      ;;
  esac
}

if ! download_asset "${PKG}.tar.gz" "$TMP/${PKG}.tar.gz"; then
  if [ "$SOURCE" = "public" ]; then
    fail "download failed: ${PKG}.tar.gz from release ${TAG}. If this repo is private, log in with 'gh auth login' or set GH_TOKEN"
  fi
  fail "download failed: ${PKG}.tar.gz from release ${TAG} via ${SOURCE}"
fi

# --- verify ------------------------------------------------------------------
if download_asset "SHA256SUMS" "$TMP/SHA256SUMS" 2>/dev/null; then
  expected="$(grep "${PKG}.tar.gz" "$TMP/SHA256SUMS" | awk '{print $1}')"
  if [ -n "$expected" ]; then
    if command -v sha256sum >/dev/null 2>&1; then
      actual="$(sha256sum "$TMP/${PKG}.tar.gz" | awk '{print $1}')"
    else
      actual="$(shasum -a 256 "$TMP/${PKG}.tar.gz" | awk '{print $1}')"
    fi
    [ "$expected" = "$actual" ] || fail "checksum mismatch for ${PKG}.tar.gz"
    say "Checksum OK"
  else
    say "WARNING: ${PKG}.tar.gz not found in SHA256SUMS, skipping verification"
  fi
else
  say "WARNING: SHA256SUMS not available, skipping verification"
fi

tar -xzf "$TMP/${PKG}.tar.gz" -C "$TMP"

# --- install -----------------------------------------------------------------
mkdir -p "$INSTALL_DIR"
for bin in loomem-server loomem-cli loomem-migrate; do
  install -m 0755 "$TMP/${PKG}/${bin}" "$INSTALL_DIR/${bin}"
done

# First-install config templates (never overwrite existing ones)
mkdir -p "$CONFIG_DIR"
for f in config.toml entities.toml.example synonyms.toml.example; do
  if [ ! -e "$CONFIG_DIR/$f" ]; then
    cp "$TMP/${PKG}/$f" "$CONFIG_DIR/$f"
  fi
done
# entities.toml is required at server start — seed it from the example on first install
if [ ! -e "$CONFIG_DIR/entities.toml" ]; then
  cp "$TMP/${PKG}/entities.toml.example" "$CONFIG_DIR/entities.toml"
fi

say ""
say "Installed:"
say "  $INSTALL_DIR/loomem-server"
say "  $INSTALL_DIR/loomem-cli"
say "  $INSTALL_DIR/loomem-migrate"
say ""
say "Config in $CONFIG_DIR (entities.toml seeded from the example — edit it to personalize)."
say ""
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    say "Add to your PATH (e.g. in ~/.zshrc or ~/.bashrc):"
    say "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    say ""
    ;;
esac
say "Start the server:"
say "  cd $CONFIG_DIR && loomem-server"
say ""
say "Connect an MCP client (Claude Code example):"
say "  claude mcp add --transport http loomem http://localhost:3030/mcp"
say ""
say "Docs: https://github.com/${REPO}#readme  (docs/installation.md for the full guide)"
