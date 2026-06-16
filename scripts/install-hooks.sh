#!/usr/bin/env bash
# Install repo git hooks into .git/hooks as symlinks.
# Idempotent: re-running replaces existing symlinks with fresh ones.
# Added 2026-04-16 w ramach /06d-governance.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
HOOK_SRC="$REPO_ROOT/scripts/hooks/pre-commit"
HOOK_DST="$REPO_ROOT/.git/hooks/pre-commit"

if [ ! -f "$HOOK_SRC" ]; then
  echo "error: $HOOK_SRC not found" >&2
  exit 1
fi

mkdir -p "$(dirname "$HOOK_DST")"
ln -sf "$HOOK_SRC" "$HOOK_DST"
chmod +x "$HOOK_SRC"

echo "pre-commit hook installed: $HOOK_DST -> $HOOK_SRC"
