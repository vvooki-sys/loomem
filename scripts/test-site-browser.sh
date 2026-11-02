#!/usr/bin/env bash
set -euo pipefail
server_log=$(mktemp)
browser_output=$(mktemp)
profile=$(mktemp -d)
python3 -m http.server 18765 --bind 127.0.0.1 --directory docs >"$server_log" 2>&1 &
server_pid=$!
trap 'kill "$server_pid" 2>/dev/null || true; rm -f "$server_log" "$browser_output"; rm -rf "$profile"' EXIT
for attempt in {1..50}; do
  if curl -fsS http://127.0.0.1:18765/ >/dev/null 2>&1; then break; fi
  sleep 0.1
done
browser=${CHROME_BIN:-google-chrome}
for language in en pl; do
  "$browser" --headless --no-sandbox --disable-gpu --disable-dev-shm-usage \
    --user-data-dir="$profile/$language" --virtual-time-budget=3000 --dump-dom \
    "http://127.0.0.1:18765/$([[ $language == pl ]] && printf 'pl/' || true)?loomem_lang=$language" > "$browser_output"
  grep -q "<html lang=\"$language\"" "$browser_output"
  grep -q 'data-language="pl"' "$browser_output"
  grep -q 'data-language="en"' "$browser_output"
done
