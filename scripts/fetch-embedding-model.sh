#!/bin/sh
# Fetch the default local embedding model (multilingual-e5-small, 384-dim)
# used when `embedding_provider = "local"`. Downloads model.onnx +
# tokenizer.json and verifies them against known SHA256 sums.
#
# Idempotent: re-running with a complete, verified model is a no-op.
#
#   ./scripts/fetch-embedding-model.sh                 # → ~/.loomem/models/multilingual-e5-small
#   ./scripts/fetch-embedding-model.sh /custom/dir     # → /custom/dir
#
# The installer (cycle /011) calls this to seed the model offline; a fresh
# server start expects the model at the path above (see config.toml).

set -eu

MODEL_NAME="multilingual-e5-small"
DEST="${1:-$HOME/.loomem/models/$MODEL_NAME}"
BASE="https://huggingface.co/Xenova/multilingual-e5-small/resolve/main"

# int8-quantized export (~113 MB; tract loads it, multilingual/Polish gate
# passes — see cycles/011). fp32 model.onnx (448 MB) was the /010 default;
# quantized keeps the single-file installer (cycle /011) airdrop-friendly.
MODEL_SHA="f80102d3f2a1229f387d3c81909990d8945513e347b0eab049f7de3c6f98c193"
TOKENIZER_SHA="0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39"

say()  { printf '%s\n' "$*"; }
fail() { printf 'fetch-embedding-model: error: %s\n' "$*" >&2; exit 1; }

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
else
  fail "need sha256sum or shasum"
fi

command -v curl >/dev/null 2>&1 || fail "need curl"

# fetch <url> <dest> <expected-sha>
fetch() {
  url="$1"; dest="$2"; want="$3"
  if [ -f "$dest" ] && [ "$(sha256 "$dest")" = "$want" ]; then
    say "ok (cached): $dest"
    return 0
  fi
  say "downloading $(basename "$dest") ..."
  curl -fsSL -o "$dest" "$url" || fail "download failed: $url"
  got="$(sha256 "$dest")"
  [ "$got" = "$want" ] || fail "checksum mismatch for $dest (got $got, want $want)"
  say "verified: $dest"
}

mkdir -p "$DEST"
fetch "$BASE/onnx/model_quantized.onnx" "$DEST/model.onnx"     "$MODEL_SHA"
fetch "$BASE/tokenizer.json"            "$DEST/tokenizer.json" "$TOKENIZER_SHA"

say ""
say "Local embedding model ready at: $DEST"
say "Set embedding_model_path there, or leave it unset to use the default."
