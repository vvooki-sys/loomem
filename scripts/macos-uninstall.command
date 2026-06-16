#!/bin/sh
# Uninstall Loomem (macOS). Stops the background service and removes the
# binaries, model, and config. Your memories in ~/.loomem/data are KEPT by
# default — pass --purge to delete those too.
set -eu

LOOMEM_HOME="${LOOMEM_HOME:-$HOME/.loomem}"
LABEL="${LOOMEM_AGENT_LABEL:-com.loomem.server}"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
PURGE=0
[ "${1:-}" = "--purge" ] && PURGE=1

echo "Stopping Loomem service ..."
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || launchctl unload "$PLIST" 2>/dev/null || true
rm -f "$PLIST"

if [ "$PURGE" -eq 1 ]; then
  rm -rf "$LOOMEM_HOME"
  echo "Removed $LOOMEM_HOME (including memories)."
else
  # Keep data/; remove everything else we installed.
  for item in bin models _seed config.toml entities.toml entities.toml.example synonyms.toml.example logs uninstall.command; do
    rm -rf "${LOOMEM_HOME:?}/$item"
  done
  echo "Loomem removed. Your memories are kept at $LOOMEM_HOME/data"
  echo "(run with --purge to delete those too)."
fi
