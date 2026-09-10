#!/usr/bin/env bash
# Install the Illustrator MCP Bridge into Adobe Illustrator (macOS).
#
# Default: installs the CEP panel (Window > Extensions > Illustrator MCP
# Bridge) into the user CEP extensions folder and enables Adobe's
# PlayerDebugMode so the unsigned extension loads. Restart Illustrator after
# installing.
#
#   ./install-bridge.sh            install the CEP panel from ./bridge
#   ./install-bridge.sh --remote   fetch the files from the running workload
#                                  (the copy guaranteed to match the server)
#   ./install-bridge.sh --shuttle  save the zero-install AppleScript shuttle
#                                  (run `bash ~/Documents/illustrator-mcp-shuttle.sh`
#                                  in a terminal; no install, no restart)
#   ./install-bridge.sh --pump     save the in-app pump script (Illustrator
#                                  2022 and older only — newer versions
#                                  removed ExtendScript's Socket class)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOST_HEADER="${MCP_HOST:-illustrator-mcp.localhost}"
INGRESS="${MCP_INGRESS:-http://127.0.0.1:8200}"
EXT_ID="com.cosmonic.illustrator-mcp"
EXT_DIR="$HOME/Library/Application Support/Adobe/CEP/extensions/$EXT_ID"

fetch() { curl -fsS -H "Host: $HOST_HEADER" "$INGRESS$1" -o "$2"; }

if [[ "${1:-}" == "--shuttle" ]]; then
    DEST="$HOME/Documents/illustrator-mcp-shuttle.sh"
    echo "Fetching the shuttle from the running workload ($HOST_HEADER)..."
    fetch /bridge/shuttle.sh "$DEST"
    chmod +x "$DEST"
    echo
    echo "Saved: $DEST"
    echo "Run it in a terminal while Illustrator is open:  bash $DEST"
    echo "It relays MCP commands into Illustrator until you press Ctrl-C."
    exit 0
fi

if [[ "${1:-}" == "--pump" ]]; then
    DEST="$HOME/Documents/illustrator-mcp-pump.jsx"
    echo "Fetching the pump script from the running workload ($HOST_HEADER)..."
    fetch /bridge/pump.jsx "$DEST"
    echo
    echo "Saved: $DEST"
    echo "In Illustrator: File > Scripts > Other Script… and pick that file."
    echo "It drains queued MCP commands for ~2 minutes per run, then exits."
    exit 0
fi

echo "Installing the Illustrator MCP Bridge CEP panel:"
echo "  to: $EXT_DIR"
mkdir -p "$EXT_DIR/CSXS"

if [[ "${1:-}" == "--remote" ]]; then
    echo "Fetching panel files from the running workload ($HOST_HEADER)..."
    fetch /bridge/cep/manifest.xml "$EXT_DIR/CSXS/manifest.xml"
    fetch /bridge/cep/index.html "$EXT_DIR/index.html"
    fetch /bridge/cep/main.js "$EXT_DIR/main.js"
    fetch /bridge/commands.jsx "$EXT_DIR/commands.jsx"
else
    cp "$SCRIPT_DIR/bridge/cep/CSXS/manifest.xml" "$EXT_DIR/CSXS/manifest.xml"
    cp "$SCRIPT_DIR/bridge/cep/index.html" "$EXT_DIR/index.html"
    cp "$SCRIPT_DIR/bridge/cep/main.js" "$EXT_DIR/main.js"
    cp "$SCRIPT_DIR/bridge/commands.jsx" "$EXT_DIR/commands.jsx"
fi

# Unsigned extensions load only with PlayerDebugMode on. Cover the CEP
# versions Illustrator 2020-2025 use.
for csxs in 10 11 12; do
    defaults write "com.adobe.CSXS.$csxs" PlayerDebugMode 1
done
killall cfprefsd 2>/dev/null || true

echo
echo "Installed. Next steps:"
echo "  1. Make sure the workload is running on Cosmonic Desktop:"
echo "       curl -H 'Host: $HOST_HEADER' $INGRESS/healthz"
echo "  2. Restart Adobe Illustrator."
echo "  3. Open Window > Extensions > Illustrator MCP Bridge and leave it open."
echo "     If your deployment uses a different ingress name, set it in the"
echo "     panel's Server field."
echo "  4. Register the MCP server with your client, e.g.:"
echo "       claude mcp add --transport http illustrator http://$HOST_HEADER:8200/"
