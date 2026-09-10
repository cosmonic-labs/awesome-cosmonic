#!/usr/bin/env bash
# Smoke test for a running github-mcp deployment on Cosmonic Desktop.
#
# Deploy the workload first (see the README), then run this. It lists the tools
# and exercises all four against public GitHub data over the MCP streamable-http
# transport through the Desktop ingress. Requires the outbound host
# api.github.com in the workload's allowedHosts. Runs unauthenticated (public
# data, 60 requests/hour); set a github-token secret to raise the limit.
set -euo pipefail

INGRESS="${INGRESS:-http://127.0.0.1:8200/}"
HOST="${HOST:-github-mcp.localhost.cosmonic.sh}"
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

echo "== search_repositories { query: cosmonic } =="
call tools/call search_repositories \
  "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"search_repositories\",\"arguments\":{\"query\":\"cosmonic\"},$META}}"

echo "== get_repository { cosmonic-labs/awesome-cosmonic } =="
call tools/call get_repository \
  "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"get_repository\",\"arguments\":{\"owner\":\"cosmonic-labs\",\"repo\":\"awesome-cosmonic\"},$META}}"

echo "== list_issues { cosmonic-labs/awesome-cosmonic, state: all } =="
call tools/call list_issues \
  "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"list_issues\",\"arguments\":{\"owner\":\"cosmonic-labs\",\"repo\":\"awesome-cosmonic\",\"state\":\"all\"},$META}}"

echo "== get_file_contents { cosmonic-labs/awesome-cosmonic README.md } =="
call tools/call get_file_contents \
  "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\",\"params\":{\"name\":\"get_file_contents\",\"arguments\":{\"owner\":\"cosmonic-labs\",\"repo\":\"awesome-cosmonic\",\"path\":\"README.md\"},$META}}"
