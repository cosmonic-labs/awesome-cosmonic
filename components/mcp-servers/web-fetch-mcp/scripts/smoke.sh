#!/usr/bin/env bash
# Smoke test for a running web-fetch-mcp deployment on Cosmonic Desktop.
#
# Deploy the workload first (see the README), then run this. It lists the
# tools, fetches an allowed URL (success), and fetches a URL on a host that is
# NOT in allowedHosts (the friendly egress-boundary error). The allowed fetch
# needs httpbin.org in the workload's allowedHosts; the blocked fetch
# deliberately targets a host that is not.
set -euo pipefail

INGRESS="${INGRESS:-http://127.0.0.1:8200/}"
HOST="${HOST:-web-fetch-mcp.localhost}"
ALLOWED_URL="${ALLOWED_URL:-https://httpbin.org/get}"
BLOCKED_URL="${BLOCKED_URL:-https://example.org/}"
META='"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}'

call() {
  local method="$1" name="${2:-}" body="$3"
  local -a hdr=(
    -H "Host: $HOST"
    -H 'Content-Type: application/json'
    -H 'Accept: application/json, text/event-stream'
    -H 'MCP-Protocol-Version: 2026-07-28'
    -H "Mcp-Method: $method"
  )
  [ -n "$name" ] && hdr+=(-H "Mcp-Name: $name")
  curl -sS -X POST "$INGRESS" "${hdr[@]}" --max-time 45 -d "$body" | sed 's/^data: //'
  echo
}

echo "== tools/list =="
call tools/list "" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{$META}}"

echo "== fetch_url (allowed: $ALLOWED_URL) =="
call tools/call fetch_url \
  "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_url\",\"arguments\":{\"url\":\"$ALLOWED_URL\"},$META}}"

echo "== fetch_url (blocked by allowedHosts: $BLOCKED_URL) =="
call tools/call fetch_url \
  "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_url\",\"arguments\":{\"url\":\"$BLOCKED_URL\"},$META}}"
