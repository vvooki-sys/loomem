#!/usr/bin/env bash
# Install the repo pre-commit hook. Works from the main checkout and from any
# linked worktree: hooks live in the shared git directory, so the installed
# hook is a small wrapper that runs the versioned hook of whichever checkout
# is committing. Idempotent: re-running overwrites the wrapper.
# Added 2026-04-16 w ramach /06d-governance.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
HOOK_SRC="$REPO_ROOT/scripts/hooks/pre-commit"
# This clone's own hooks dir, shared by all of its worktrees (in a worktree
# .git is a file, so .git/hooks cannot be used). Deliberately not
# `--git-path hooks`: that follows core.hooksPath, which may be a global dir
# shared with unrelated repositories.
HOOKS_DIR="$(git rev-parse --path-format=absolute --git-common-dir)/hooks"
HOOK_DST="$HOOKS_DIR/pre-commit"

if [ ! -f "$HOOK_SRC" ]; then
  echo "error: $HOOK_SRC not found" >&2
  exit 1
fi

# With core.hooksPath set, git never runs hooks from $HOOKS_DIR. Do not touch
# that (possibly global, shared) directory; tell the user instead.
if HOOKS_PATH="$(git config --get core.hooksPath)"; then
  echo "error: core.hooksPath is set to '$HOOKS_PATH', so git ignores this clone's hooks dir." >&2
  echo "Nothing was installed. Either unset it for this repo:" >&2
  echo "  git config --local --unset core.hooksPath   (if set locally)" >&2
  echo "or call scripts/hooks/pre-commit from the hook in '$HOOKS_PATH'." >&2
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
