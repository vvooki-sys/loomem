#!/usr/bin/env bash
# Install the repo pre-commit hook. Works from the main checkout and from any
# linked worktree: hooks live in the shared git directory, so the installed
# hook is a small wrapper that runs the versioned hook of whichever checkout
# is committing. Idempotent: re-running overwrites the wrapper.
# Added 2026-04-16 w ramach /06d-governance.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
HOOK_SRC="$REPO_ROOT/scripts/hooks/pre-commit"
# Resolves to the shared hooks dir (or core.hooksPath), also in a worktree
# where .git is a file rather than a directory.
HOOKS_DIR="$(git rev-parse --path-format=absolute --git-path hooks)"
HOOK_DST="$HOOKS_DIR/pre-commit"

if [ ! -f "$HOOK_SRC" ]; then
  echo "error: $HOOK_SRC not found" >&2
  exit 1
fi

mkdir -p "$HOOKS_DIR"
rm -f "$HOOK_DST"
cat > "$HOOK_DST" <<'HOOK'
#!/usr/bin/env bash
# Installed by scripts/install-hooks.sh. Runs the versioned hook of the
# checkout or worktree being committed from.
set -euo pipefail
exec "$(git rev-parse --show-toplevel)/scripts/hooks/pre-commit" "$@"
HOOK
chmod +x "$HOOK_DST" "$HOOK_SRC"

echo "pre-commit hook installed: $HOOK_DST (runs scripts/hooks/pre-commit of the committing checkout)"
