#!/usr/bin/env bash
# Token-gated HTTP transport smoke test.
#
# Starts the built server on a loopback port with a temporary workspace and a
# static bearer token, then verifies:
#   - a request without the token is rejected (401)
#   - a request with the token completes initialize (200)
set -euo pipefail

cd "$(dirname "$0")/.."

PORT="${MCP_ORCA_HTTP_PORT:-18765}"
TOKEN="${MCP_ORCA_HTTP_TOKEN:-smoke-token}"
BIN="${MCP_ORCA_BIN:-target/debug/mcp-orca}"
TMPDIR="$(mktemp -d)"
PID=""

cleanup() {
    if [[ -n "$PID" ]]; then
        kill "$PID" 2>/dev/null || true
        wait "$PID" 2>/dev/null || true
    fi
    rm -rf "$TMPDIR"
}
trap cleanup EXIT

if [[ ! -x "$BIN" ]]; then
    echo "missing $BIN; run 'cargo build' first" >&2
    exit 1
fi

cat >"$TMPDIR/config.toml" <<EOF
[server]
transport = "http"

[server.http]
host = "127.0.0.1"
port = $PORT
path = "/mcp"
allow_unauthenticated = false

[workspace]
root = "$TMPDIR/ws"

[profiles]
use_os_defaults = false
EOF

MCP_ORCA_HTTP_TOKEN="$TOKEN" "$BIN" --config "$TMPDIR/config.toml" \
    >"$TMPDIR/out.log" 2>"$TMPDIR/err.log" &
PID=$!

payload='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"smoke","version":"0.1"}}}'

status=""
for _ in $(seq 1 50); do
    status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$PORT/mcp" \
        -H 'Content-Type: application/json' \
        -H 'Accept: application/json, text/event-stream' \
        -d "$payload" || true)
    if [[ "$status" != "000" ]]; then
        break
    fi
    sleep 0.1
done

if [[ "$status" != "401" ]]; then
    echo "FAILED: expected 401 without token, got $status" >&2
    sed -n '1,40p' "$TMPDIR/err.log" >&2
    exit 1
fi

auth_status=$(curl -s -o "$TMPDIR/resp.txt" -w '%{http_code}' -X POST "http://127.0.0.1:$PORT/mcp" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H "Authorization: Bearer $TOKEN" \
    -d "$payload")

if [[ "$auth_status" != "200" ]]; then
    echo "FAILED: expected 200 with token, got $auth_status" >&2
    sed -n '1,40p' "$TMPDIR/resp.txt" >&2
    exit 1
fi

if ! grep -q '"serverInfo"' "$TMPDIR/resp.txt"; then
    echo "FAILED: initialize response missing serverInfo" >&2
    sed -n '1,40p' "$TMPDIR/resp.txt" >&2
    exit 1
fi

echo "http auth smoke: 401 without token, 200 with token"
