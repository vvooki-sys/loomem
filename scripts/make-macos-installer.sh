#!/bin/sh
# Build the self-extracting macOS installer (.command) for Loomem.
#
#   ./scripts/make-macos-installer.sh \
#       --bins   <dir with loomem-server/-cli/-migrate> \
#       --model  <dir with model.onnx + tokenizer.json> \
#       --out    loomem-install-<ver>-aarch64.command
#
# Defaults: --model ~/.loomem/models/multilingual-e5-small,
#           --out   ./loomem-install-aarch64.command
# The model is fetched by scripts/fetch-embedding-model.sh if you don't have one.
#
# Assembles stub + gzipped payload (binaries, config templates, model,
# LaunchAgent template, uninstaller) into one double-clickable file.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"

BINS=""
MODEL="$HOME/.loomem/models/multilingual-e5-small"
OUT="$REPO/loomem-install-aarch64.command"

while [ $# -gt 0 ]; do
  case "$1" in
    --bins)  BINS="$2"; shift 2 ;;
    --model) MODEL="$2"; shift 2 ;;
    --out)   OUT="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

[ -n "$BINS" ] || { echo "error: --bins <dir> is required" >&2; exit 2; }
for b in loomem-server loomem-cli loomem-migrate; do
  [ -f "$BINS/$b" ] || { echo "error: missing binary $BINS/$b" >&2; exit 2; }
done
for f in model.onnx tokenizer.json; do
  [ -f "$MODEL/$f" ] || { echo "error: missing model file $MODEL/$f (run fetch-embedding-model.sh)" >&2; exit 2; }
done

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

mkdir -p "$STAGE/bin" "$STAGE/models/multilingual-e5-small" "$STAGE/_seed"

# binaries
for b in loomem-server loomem-cli loomem-migrate; do
  cp "$BINS/$b" "$STAGE/bin/$b"
  chmod +x "$STAGE/bin/$b"
done
# model
cp "$MODEL/model.onnx" "$MODEL/tokenizer.json" "$STAGE/models/multilingual-e5-small/"
# config templates + plist template (seeded by the stub, non-clobber)
cp "$REPO/config.toml" "$STAGE/_seed/config.toml"
cp "$REPO/entities.toml.example" "$STAGE/_seed/entities.toml.example"
[ -f "$REPO/synonyms.toml.example" ] && cp "$REPO/synonyms.toml.example" "$STAGE/_seed/synonyms.toml.example"
cp "$HERE/com.loomem.server.plist.template" "$STAGE/_seed/com.loomem.server.plist.template"
# uninstaller
cp "$HERE/macos-uninstall.command" "$STAGE/uninstall.command"
chmod +x "$STAGE/uninstall.command"

# assemble: stub + marker (already at end of stub) + gzipped payload
cp "$HERE/macos-installer-stub.sh" "$OUT"
tar czf - -C "$STAGE" . >> "$OUT"
chmod +x "$OUT"

SIZE="$(du -h "$OUT" | awk '{print $1}')"
echo "Built: $OUT ($SIZE)"
echo "Test:  LOOMEM_HOME=/tmp/loomem-test LOOMEM_PORT=3099 LOOMEM_AGENT_LABEL=com.loomem.test sh \"$OUT\""
