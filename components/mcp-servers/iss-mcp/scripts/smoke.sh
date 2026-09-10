#!/usr/bin/env bash
# Smoke test for a running iss-mcp deployment on Cosmonic Desktop.
#
# Deploy the workload first (see the README), then run this. It calls the MCP
# streamable-http transport through the Desktop ingress and prints the result
# of each of the two tools. Requires the outbound host api.open-notify.org in
# the workload's allowedHosts, and a reachable Open Notify (it is a small
# community service that is sometimes briefly down).
set -euo pipefail

INGRESS="${INGRESS:-http://127.0.0.1:8200/}"
HOST="${HOST:-iss-mcp.localhost}"
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
  curl -sS -X POST "$INGRESS" "${hdr[@]}" --max-time 30 -d "$body" | sed 's/^data: //'
  echo
}

echo "== tools/list =="
call tools/list "" "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{$META}}"

echo "== who_is_in_space =="
call tools/call who_is_in_space \
  "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"who_is_in_space\",\"arguments\":{},$META}}"

echo "== iss_position =="
call tools/call iss_position \
  "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"iss_position\",\"arguments\":{},$META}}"
