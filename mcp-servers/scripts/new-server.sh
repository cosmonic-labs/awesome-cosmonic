#!/usr/bin/env bash
# Scaffold a new MCP server directory from mcp-server-template-rs.
#
#   scripts/new-server.sh <name> [--template <path>] [--domain <mcp.ai/domain>] [--description "<text>"]
#
# <name> is the directory and crate name (kebab-case, e.g. notion-mcp). The
# server is reachable on Cosmonic Desktop at http://<name>.localhost:8200/.
#
# What it does: copies the template (never `git clone`s into the new dir),
# renames every `mcp-server-template` / `mcp-server` occurrence, points the
# e2e at the shared harness in mcp-servers/scripts/mcp_e2e_lib.sh, and leaves
# the example tools in place so the result builds and passes its suite
# before you touch a line of Rust.
set -euo pipefail

TEMPLATE="${MCP_TEMPLATE:-$HOME/source/mcp-server-template-rs}"
DOMAIN=""
DESCRIPTION=""
NAME=""
while [ $# -gt 0 ]; do
  case "$1" in
    --template) TEMPLATE="$2"; shift 2 ;;
    --domain) DOMAIN="$2"; shift 2 ;;
    --description) DESCRIPTION="$2"; shift 2 ;;
    -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
    *) NAME="$1"; shift ;;
  esac
done
[ -n "$NAME" ] || { echo "usage: $0 <name> [--template <path>] [--domain <d>] [--description <text>]" >&2; exit 2; }
case "$NAME" in
  *[!a-z0-9-]*|-*|*-) echo "name must be lowercase kebab-case: $NAME" >&2; exit 2 ;;
esac
[ -d "$TEMPLATE/src" ] || { echo "template not found at $TEMPLATE (set MCP_TEMPLATE)" >&2; exit 2; }

HERE="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$HERE/$NAME"
[ -e "$DEST" ] && { echo "$DEST already exists" >&2; exit 1; }
CRATE="${NAME//-/_}"
DOMAIN="${DOMAIN:-${NAME%-mcp}}"
DESCRIPTION="${DESCRIPTION:-$NAME MCP server (WebAssembly component, wasi:http@0.3.0)}"

mkdir -p "$DEST"
# No root workload.yaml here (repo convention): deploy/workload.yaml is THE
# manifest, and the dev-loop/promote draft comes from .wash/config.yaml's
# `workload:` block.
cp -R "$TEMPLATE/.cargo" "$TEMPLATE/.wash" "$TEMPLATE/src" "$TEMPLATE/scripts" \
      "$TEMPLATE/deploy" "$TEMPLATE/Cargo.toml" "$TEMPLATE/Cargo.lock" \
      "$TEMPLATE/.gitignore" "$TEMPLATE/LICENSE" "$DEST/"
mkdir -p "$DEST/skills" && cp -R "$TEMPLATE/skills/server" "$DEST/skills/server"
[ -d "$TEMPLATE/docs" ] && cp -R "$TEMPLATE/docs" "$DEST/docs"

# --- rename -----------------------------------------------------------------
# Order matters: the longer `mcp-server-template` first, then the bare
# workload/host name `mcp-server`.
sed_i() { sed -i.bak "$@" && rm -f "${@: -1}.bak"; }

sed_i "s/^name = \"mcp-server-template\"/name = \"$NAME\"/" "$DEST/Cargo.toml"
sed_i "s|^description = .*|description = \"$DESCRIPTION\"|" "$DEST/Cargo.toml"
sed_i "s|^repository = .*|repository = \"https://github.com/cosmonic-labs/awesome-cosmonic\"|" "$DEST/Cargo.toml"
# Cargo.lock names the root package too.
sed_i "s/^name = \"mcp-server-template\"/name = \"$NAME\"/" "$DEST/Cargo.lock"

sed_i "s|mcp_server_template.wasm|$CRATE.wasm|" "$DEST/.wash/config.yaml"

f="$DEST/deploy/workload.yaml"
sed_i "s|mcp-server-template|$NAME|g" "$f"
# One DNS name per server, everywhere: <name>.localhost (no .cosmonic.sh variant).
sed_i "s|mcp-server\.localhost\.cosmonic\.sh,mcp-server\.localhost|$NAME.localhost|g" "$f"
sed_i "s|mcp-server\.localhost\.cosmonic\.sh|$NAME.localhost|g" "$f"
sed_i "s|mcp-server\.localhost|$NAME.localhost|g" "$f"
sed_i "s|name: \"mcp-server\"|name: \"$NAME\"|; s|name: mcp-server$|name: $NAME|" "$f"
sed_i "s|mcp.ai/domain: \"template\"|mcp.ai/domain: \"$DOMAIN\"|" "$f"
sed_i "s|ghcr.io/<org>/$NAME|ghcr.io/cosmonic-labs/awesome-cosmonic/$NAME|" "$f"
# The template's header describes a two-manifest layout; this repo ships one.
python3 - "$f" <<'PY'
import sys, pathlib
p = pathlib.Path(sys.argv[1]); t = p.read_text()
old_head = t.split("apiVersion:", 1)[0]
new_head = """# Cosmonic Desktop workload manifest for this MCP server (the only manifest
# in this project; the dev-loop/promote draft comes from .wash/config.yaml).
#
# Apply it as-is for the published image, or replace `image` with the
# digest-pinned reference `cosmonic_promote` returns for a local build.
# Reach the server at http://NAME.localhost:8200/ (Desktop routes by Host).
# Deploy docs: https://cosmonic.com/docs/desktop
""".replace("NAME", p.parent.parent.name)
p.write_text(new_head + "apiVersion:" + t.split("apiVersion:", 1)[1])
PY

# Skill: the URI name is the package name, so only the frontmatter/title need
# the rename; the body is rewritten by the author.
sed_i "s|mcp-server-template|$NAME|g" "$DEST/skills/server/SKILL.md"
sed_i "s|mcp-server-template|$NAME|g" "$DEST/skills/server/references/TOOLS.md"

# Source: server struct name + instructions mention the template.
sed_i "s|mcp-server-template|$NAME|g" "$DEST/src/server.rs" "$DEST/src/lib.rs" "$DEST/src/skills.rs" "$DEST/src/discovery.rs"

# e2e: source the shared harness, point at the right wasm, pick free ports.
# Ports are derived from the name so suites can run concurrently.
HASH=$(printf '%s' "$NAME" | cksum | cut -d' ' -f1)
PORT=$((9000 + HASH % 900 * 1))
cat > "$DEST/scripts/e2e.sh" <<EOS
#!/usr/bin/env bash
# End-to-end tests for $NAME. Framework checks (protocol, spec enforcement,
# discovery route, skills over MCP, robustness, Host guard) come from the shared
# harness in ../../scripts/mcp_e2e_lib.sh; tool cases live below.
#
# Usage: scripts/e2e.sh [--no-build]
set -u
cd "\$(dirname "\$0")/.."

PORT=\${PORT:-$PORT}
GUARD_PORT=\${GUARD_PORT:-$((PORT + 1))}
FIXTURE_PORT=\${FIXTURE_PORT:-$((PORT + 2))}
WASM=\${WASM:-target/wasm32-wasip2/release/$CRATE.wasm}
SKILL_NAME=$NAME

# shellcheck source=../../scripts/mcp_e2e_lib.sh
source "\$(dirname "\$0")/../../scripts/mcp_e2e_lib.sh"

# The concurrency test in framework_tests fires this tool 8x in parallel.
FIRST_TOOL_NAME=add
FIRST_TOOL_ARGS='{"a":1,"b":2}'
FIRST_TOOL_EXPECT='"sum"'

mcp_build_if_needed "\${1:-}"
mcp_harness_start
mcp_harness_start_guard

framework_tests add current_time echo http_get
discovery_tests
skills_tests "\$SKILL_NAME" "references/TOOLS.md"

echo "== tools =="
OUT=\$(mcp_call echo '{"message":"e2e says hi"}')
assert_contains "echo round-trips" 'e2e says hi' "\$OUT"

guard_tests
mcp_harness_report
EOS
chmod +x "$DEST/scripts/e2e.sh"

cat > "$DEST/README.md" <<EOS
# $NAME

$DESCRIPTION

Built from [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28, exports \`wasi:http/handler@0.3.0\`, serves a
discovery document on \`GET /\` and \`GET /health\`, and publishes its playbook
as a skill at \`skill://$NAME/SKILL.md\`.

Reachable on Cosmonic Desktop at <http://$NAME.localhost:8200/>.

_Scaffolded by \`mcp-servers/scripts/new-server.sh\`; replace this README._
EOS

echo "scaffolded $DEST (crate $CRATE, e2e port $PORT, host $NAME.localhost)"
