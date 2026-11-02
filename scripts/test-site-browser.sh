#!/usr/bin/env bash
set -euo pipefail
server_log=$(mktemp)
python3 -u -c '
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from functools import partial
server = ThreadingHTTPServer(("127.0.0.1", 0), partial(SimpleHTTPRequestHandler, directory="docs"))
print(server.server_port, flush=True)
server.serve_forever()
' >"$server_log" 2>&1 &
server_pid=$!
trap 'kill "$server_pid" 2>/dev/null || true; wait "$server_pid" 2>/dev/null || true; rm -f "$server_log"' EXIT
port=''
for attempt in {1..50}; do
  port=$(head -n 1 "$server_log")
  if [[ "$port" =~ ^[0-9]+$ ]] && curl -fsS "http://127.0.0.1:$port/" >/dev/null 2>&1; then break; fi
  sleep 0.1
done
[[ "$port" =~ ^[0-9]+$ ]] || { cat "$server_log"; exit 1; }
SITE_TEST_ORIGIN="http://127.0.0.1:$port" node scripts/test-site-browser.cjs
