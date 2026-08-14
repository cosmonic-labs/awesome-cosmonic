#!/usr/bin/env bash
# Smoke test for a running threat-intel-mcp deployment on Cosmonic Desktop.
#
# Deploy the workload first (see the README), then run this. It lists the tools
# and exercises both against real OSV data over the MCP streamable-http
# transport through the Desktop ingress. Requires the outbound host api.osv.dev
# in the workload's allowedHosts. The OSV API is public — no API key needed.
set -euo pipefail

INGRESS="${INGRESS:-http://127.0.0.1:8200/}"
HOST="${HOST:-threat-intel-mcp.localhost.cosmonic.sh}"
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

echo "== lookup_package_vulnerabilities { PyPI, jinja2, 2.4.1 } =="
call tools/call lookup_package_vulnerabilities \
  "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"lookup_package_vulnerabilities\",\"arguments\":{\"ecosystem\":\"PyPI\",\"package\":\"jinja2\",\"version\":\"2.4.1\"},$META}}"

echo "== get_vulnerability { GHSA-9v9h-cgj8-h64p } =="
call tools/call get_vulnerability \
  "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"get_vulnerability\",\"arguments\":{\"id\":\"GHSA-9v9h-cgj8-h64p\"},$META}}"
