#!/usr/bin/env bash
# Install the MCP Bridge Auto panel into Adobe After Effects.
#
# Copies bridge/mcp-bridge-auto.jsx into After Effects' ScriptUI Panels folder
# so it appears under the Window menu. Run again after editing the panel or
# upgrading After Effects.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SOURCE="$SCRIPT_DIR/bridge/mcp-bridge-auto.jsx"

if [[ ! -f "$SOURCE" ]]; then
    echo "error: $SOURCE not found" >&2
    exit 1
fi

# Newest installed After Effects wins.
AE_DIR="$(ls -d "/Applications/Adobe After Effects"* 2>/dev/null | sort -V | tail -1 || true)"
if [[ -z "$AE_DIR" ]]; then
    echo "error: no Adobe After Effects installation found in /Applications" >&2
    echo "Copy the panel manually into:" >&2
    echo "  /Applications/Adobe After Effects <version>/Scripts/ScriptUI Panels/" >&2
    exit 1
fi

DEST_DIR="$AE_DIR/Scripts/ScriptUI Panels"
DEST="$DEST_DIR/mcp-bridge-auto.jsx"

echo "Installing bridge panel:"
echo "  from: $SOURCE"
echo "  to:   $DEST"

if mkdir -p "$DEST_DIR" 2>/dev/null && cp "$SOURCE" "$DEST" 2>/dev/null; then
    :
else
    echo "Direct copy failed (permissions); retrying with sudo..."
    sudo mkdir -p "$DEST_DIR"
    sudo cp "$SOURCE" "$DEST"
fi

echo
echo "Installed. Next steps:"
echo "  1. Make sure the ae-mcp workload is running on Cosmonic Desktop:"
echo "       curl http://ae-mcp.localhost.cosmonic.sh:8200/healthz"
echo "  2. In After Effects: Settings > Scripting & Expressions >"
echo "     enable 'Allow Scripts to Write Files and Access Network',"
echo "     then restart After Effects."
echo "  3. Open the panel: Window > mcp-bridge-auto.jsx (leave it open)."
echo "  4. Register the MCP server with your client, e.g.:"
echo "       claude mcp add --transport http after-effects \\"
echo "         http://ae-mcp.localhost.cosmonic.sh:8200/mcp"
