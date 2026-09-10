#!/usr/bin/env bash
# Smoke test for a running pii-redactor-mcp deployment on Cosmonic Desktop.
#
# Deploy the workload first (see the README), then run this. It lists the tool
# and exercises `redact` over the MCP streamable-http transport through the
# Desktop ingress. This tool is PURE COMPUTE with ZERO egress — the workload's
# outbound allowedHosts is empty (deny-all), so no network access is involved.
set -euo pipefail

INGRESS="${INGRESS:-http://127.0.0.1:8200/}"
HOST="${HOST:-pii-redactor-mcp.localhost.cosmonic.sh}"
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

echo "== redact { the sample text — all six categories } =="
call tools/call redact \
  "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"redact\",\"arguments\":{\"text\":\"Email me at jane.doe@example.com or call (555) 123-4567. SSN 123-45-6789, card 4111 1111 1111 1111, from 10.0.0.5, key AKIAIOSFODNN7EXAMPLE.\"},$META}}"

echo "== redact { Luhn-INVALID card — must NOT redact } =="
call tools/call redact \
  "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"redact\",\"arguments\":{\"text\":\"card 1234 5678 9012 3456 should stay\"},$META}}"

echo "== redact { types filter: email only } =="
call tools/call redact \
  "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"redact\",\"arguments\":{\"text\":\"a@b.com and 10.0.0.5\",\"types\":[\"email\"]},$META}}"
