#!/usr/bin/env bash
# Smoke test for a running md-html-sanitizer-mcp deployment on Cosmonic Desktop.
#
# Deploy the workload first (see the README), then run this. It lists the tools
# and exercises `sanitize_html` and `render_markdown` over the MCP
# streamable-http transport through the Desktop ingress. This tool is PURE
# COMPUTE with ZERO egress — the workload's outbound allowedHosts is empty
# (deny-all), so no network access is involved.
set -euo pipefail

INGRESS="${INGRESS:-http://127.0.0.1:8200/}"
HOST="${HOST:-md-html-sanitizer-mcp.localhost.cosmonic.sh}"
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

echo "== sanitize_html { script + handlers + javascript: url } =="
call tools/call sanitize_html \
  "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"sanitize_html\",\"arguments\":{\"html\":\"<p onclick=alert(1)>Hi</p><script>steal()</script><a href=\\\"javascript:evil()\\\">x</a><img src=x onerror=alert(1)>\"},$META}}"

echo "== render_markdown { heading + bold + raw <script> } =="
call tools/call render_markdown \
  "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"render_markdown\",\"arguments\":{\"markdown\":\"# Hi\\n\\nnormal **bold** and a raw <script>alert(1)</script> tag\"},$META}}"
