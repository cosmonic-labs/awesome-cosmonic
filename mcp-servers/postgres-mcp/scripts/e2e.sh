#!/usr/bin/env bash
# End-to-end tests for postgres-mcp.
#
# This suite is Desktop-only. The server's only upstream is the
# `wasmcloud:postgres@0.2.0` host interface, which `wasmtime serve` cannot
# provide, so there is no hermetic HTTP fixture to stand in for it: the script
# builds the component, promotes and applies it on the local Cosmonic Desktop
# (idempotently, workload `postgres-mcp`), points the shared harness at the
# ingress (MCP_BASE=http://postgres-mcp.localhost:8200/) and runs the tool
# cases against a real PostgreSQL — the podman container `mcp-pg`
# (postgres:16-alpine, user mcp / password mcp / db mcp on 127.0.0.1:5432),
# which the script seeds and cleans through `podman exec … psql`.
#
# Framework checks (protocol, spec enforcement, discovery route, skills over
# MCP, robustness) come from ../../scripts/mcp_e2e_lib.sh. The wasmtime-only
# parts of the harness — the guard instance and guard_tests — do not apply:
# the Host guard is exercised by the ingress itself (it routes by Host).
#
# Prerequisites (otherwise the suite prints SKIP and exits 0): the daemon
# socket, podman with the mcp-pg container answering, python3 with PyYAML,
# curl, cargo + wasm-tools. Set E2E_START_PG=1 to let the script create the
# container when it is missing.
#
# Knobs:
#   E2E_ALLOW_WRITES=1   also deploy with POSTGRES_ALLOW_WRITES=true and run the
#                        write cases (execute, execute_batch atomicity, RETURNING);
#                        the read-only manifest is re-applied at the end.
#   E2E_BIND_CHECK=0     skip the missing-secret bind check (it re-applies the
#                        workload without the secret ref, asserts the daemon's
#                        bind error, then restores it).
#   E2E_KEEP=1           keep the e2e schema in the database after the run.
#   E2E_PG_URL           connection URL registered as the secret ref when it does
#                        not exist yet (default: the podman container's).
#
# Usage: scripts/e2e.sh [--no-build]
set -u
cd "$(dirname "$0")/.."

PORT=${PORT:-9845}          # only keys the harness scratch dir; nothing listens
GUARD_PORT=${GUARD_PORT:-9846}
FIXTURE_PORT=${FIXTURE_PORT:-9847}
WASM=${WASM:-target/wasm32-wasip2/release/postgres_mcp.wasm}
SKILL_NAME=postgres-mcp
WORKLOAD=postgres-mcp
SECRET_REF=postgres-mcp-url
SOCK=${SOCK:-/run/user/$(id -u)/cosmonic/cosmonicd.sock}
PG_CONTAINER=${E2E_PG_CONTAINER:-mcp-pg}
PG_URL=${E2E_PG_URL:-postgres://mcp:mcp@127.0.0.1:5432/mcp?sslmode=disable}
E2E_ALLOW_WRITES=${E2E_ALLOW_WRITES:-0}
E2E_BIND_CHECK=${E2E_BIND_CHECK:-1}
E2E_KEEP=${E2E_KEEP:-0}
export MCP_BASE=${MCP_BASE:-http://postgres-mcp.localhost:8200/}

# shellcheck source=../../scripts/mcp_e2e_lib.sh
source "$(dirname "$0")/../../scripts/mcp_e2e_lib.sh"

skip() { echo "SKIP: $*"; exit 0; }
api() { curl -sS --max-time 600 --unix-socket "$SOCK" "$@"; }
psql_run() { podman exec -i "$PG_CONTAINER" psql -U mcp -d mcp -v ON_ERROR_STOP=1 -X -q -At "$@"; }
psql_val() { podman exec "$PG_CONTAINER" psql -U mcp -d mcp -X -At -c "$1" 2>/dev/null; }

# ── preflight ────────────────────────────────────────────────────────────────
[ -S "$SOCK" ] || skip "Cosmonic Desktop daemon socket not found at $SOCK"
command -v podman >/dev/null || skip "podman not found (the suite needs the $PG_CONTAINER PostgreSQL container)"
command -v python3 >/dev/null || skip "python3 not found"
python3 -c 'import yaml' 2>/dev/null || skip "python3 PyYAML not found"
if ! podman exec "$PG_CONTAINER" psql -U mcp -d mcp -X -Atc 'select 1' >/dev/null 2>&1; then
  if [ "${E2E_START_PG:-0}" = "1" ]; then
    echo "starting $PG_CONTAINER (postgres:16-alpine on 127.0.0.1:5432)..."
    podman run -d --name "$PG_CONTAINER" -p 127.0.0.1:5432:5432 -e POSTGRES_USER=mcp \
      -e POSTGRES_PASSWORD=mcp -e POSTGRES_DB=mcp docker.io/library/postgres:16-alpine >/dev/null \
      || skip "could not start $PG_CONTAINER"
    for _ in $(seq 1 60); do
      podman exec "$PG_CONTAINER" psql -U mcp -d mcp -X -Atc 'select 1' >/dev/null 2>&1 && break
      sleep 2
    done
  fi
  podman exec "$PG_CONTAINER" psql -U mcp -d mcp -X -Atc 'select 1' >/dev/null 2>&1 \
    || skip "container $PG_CONTAINER is not answering; start it with: podman run -d --name mcp-pg -p 127.0.0.1:5432:5432 -e POSTGRES_USER=mcp -e POSTGRES_PASSWORD=mcp -e POSTGRES_DB=mcp docker.io/library/postgres:16-alpine (or E2E_START_PG=1)"
fi

# ── build gates ──────────────────────────────────────────────────────────────
if [ "${1:-}" != "--no-build" ]; then
  if [ ! -x .tools/bin/wasm-component-ld ]; then
    echo "installing wasm-component-ld 0.5.30 into .tools (labeled imports need >= 0.5.30)..."
    cargo install wasm-component-ld --version 0.5.30 --root .tools --locked || exit 1
  fi
  echo "cargo fmt --check..."
  cargo fmt --check || exit 1
  echo "cargo clippy..."
  cargo clippy --all-features -- -D warnings || exit 1
fi
mcp_build_if_needed "${1:-}"
if wasm-tools component wit "$WASM" 2>/dev/null | grep -q 'import db: wasmcloud:postgres/query@0.2.0'; then
  pass "component imports db: wasmcloud:postgres/query@0.2.0 (labeled)"
else
  fail "component imports db: wasmcloud:postgres/query@0.2.0 (labeled)" "labeled import missing — is .tools/bin/wasm-component-ld the linker?"
fi
if wasm-tools component wit "$WASM" 2>/dev/null | grep -q 'import db-prepared: wasmcloud:postgres/prepared@0.2.0'; then
  pass "component imports db-prepared: wasmcloud:postgres/prepared@0.2.0 (labeled)"
else
  fail "component imports db-prepared: wasmcloud:postgres/prepared@0.2.0 (labeled)" "labeled import missing"
fi

# ── deploy on Desktop (idempotent) ───────────────────────────────────────────
echo "== deploy on Cosmonic Desktop =="
api -X POST http://localhost/v1/projects -H 'Content-Type: application/json' \
  -d "{\"path\":\"$PWD\"}" >/dev/null 2>&1 || true
PROMOTE=$(api -X POST "http://localhost/v1/projects/$WORKLOAD/promote" -H 'Content-Type: application/json' \
  -d "{\"ref\":\"$WORKLOAD:0.1.0\",\"rebuild\":false}")
IMAGE=$(printf '%s' "$PROMOTE" | python3 -c 'import json,sys
try:
    print(json.load(sys.stdin).get("image", ""))
except Exception:
    print("")')
if [ -n "$IMAGE" ]; then
  pass "promote returns a digest-pinned image ($IMAGE)"
else
  fail "promote returns a digest-pinned image" "$PROMOTE"
  mcp_harness_report
fi

if api http://localhost/v1/secrets/refs | python3 -c 'import json,sys; sys.exit(0 if any(r.get("name")==sys.argv[1] for r in json.load(sys.stdin)) else 1)' "$SECRET_REF"; then
  pass "secret ref $SECRET_REF is registered"
else
  echo "registering secret ref $SECRET_REF (env url) with the local database URL..."
  OUT=$(api -X POST http://localhost/v1/secrets/refs -H 'Content-Type: application/json' \
    -d "{\"name\":\"$SECRET_REF\",\"uri\":\"keychain://cosmonic/$SECRET_REF\",\"env\":\"url\",\"value\":\"$PG_URL\"}")
  assert_contains "secret ref $SECRET_REF registered with env url" '"env":"url"' "$OUT"
fi

# render_workload <writes true|false> [--no-secret] → JSON on stdout
render_workload() {
  python3 - "$IMAGE" "$1" "${2:-}" <<'PY'
import json, sys, yaml
d = yaml.safe_load(open("deploy/workload.yaml"))
c = d["spec"]["components"][0]
c["image"] = sys.argv[1]
c["localResources"]["environment"]["config"]["POSTGRES_ALLOW_WRITES"] = sys.argv[2]
if sys.argv[3] == "--no-secret":
    for hi in d["spec"]["hostInterfaces"]:
        hi.pop("secretFrom", None)
print(json.dumps(d))
PY
}

apply_workload() {  # apply_workload <writes> [--no-secret]
  render_workload "$1" "${2:-}" | api -X POST http://localhost/v1/workloads \
    -H 'Content-Type: application/json' --data-binary @-
}

workload_state() {
  api "http://localhost/v1/workloads/default/$WORKLOAD" | python3 -c 'import json,sys
d = json.load(sys.stdin); s = d.get("status", {})
print(s.get("state", "?"), (s.get("message") or ""))'
}

# The serving revision and the spec hash. The daemon rolls out a new spec as
# a new revision and keeps the previous one serving until the new one is
# ready ("cut over"), so "state: running" alone does not mean the new spec
# is live — the revision number is what moves.
workload_revision() {
  api "http://localhost/v1/workloads/default/$WORKLOAD" 2>/dev/null | python3 -c 'import json,sys
try:
    d = json.load(sys.stdin); s = d.get("status", {})
    print(s.get("observedRevision", 0), s.get("specHash", ""))
except Exception:
    print("0 none")'
}

# deploy <writes> [--no-secret] — apply and wait for the cutover (unless the
# spec is unchanged). Sets DEPLOY_OUT.
deploy() {
  local before after prev_rev prev_hash new_hash i
  before=$(workload_revision); prev_rev=${before%% *}; prev_hash=${before#* }
  DEPLOY_OUT=$(apply_workload "$@")
  after=$(workload_revision); new_hash=${after#* }
  if [ "$new_hash" = "$prev_hash" ] && [ -n "$prev_hash" ]; then
    : # identical spec: nothing rolls out
  else
    for i in $(seq 1 120); do
      after=$(workload_revision)
      [ "${after%% *}" -gt "$prev_rev" ] 2>/dev/null && break
      sleep 1
    done
  fi
  for i in $(seq 1 60); do
    case "$(workload_state)" in running*) curl -sf --max-time 5 -o /dev/null "$MCP_BASE" && return 0 ;; esac
    sleep 1
  done
  return 1
}

WRITES_DEPLOYED=false
if deploy false; then
  assert_contains "apply deploy/workload.yaml (read-only) accepted" '"state"' "$DEPLOY_OUT"
  pass "workload $WORKLOAD is running and GET / answers"
else
  fail "workload $WORKLOAD is running and GET / answers" "$(workload_state) $DEPLOY_OUT"
  mcp_harness_report
fi

# ── framework, discovery, skills ────────────────────────────────────────────
FIRST_TOOL_NAME=server_info
FIRST_TOOL_ARGS='{}'
FIRST_TOOL_EXPECT='"PostgreSQL'

framework_tests describe_table execute execute_batch explain_query list_indexes list_schemas list_tables query search_objects server_info table_stats
discovery_tests server_info
ROOT=$(curl -sS --max-time 20 "$MCP_BASE")
assert_contains "GET / carries the credentials block (ref postgres-mcp-url)" '"ref": "postgres-mcp-url"' "$ROOT"
assert_contains "GET / credentials name the url config key" '"env": "url"' "$ROOT"
skills_tests "$SKILL_NAME" references/TOOLS.md references/TYPES.md references/ERRORS.md
OUT=$(mcp_read_resource "skill://$SKILL_NAME/SKILL.md")
assert_contains "SKILL.md teaches the numeric cast rule" 'numeric' "$OUT"
assert_contains "SKILL.md teaches the no-sessions rule" 'No sessions' "$OUT"

# ── seed ─────────────────────────────────────────────────────────────────────
echo "== seed =="
if psql_run <<'SQL' >/dev/null 2>"$E2E_TMP/seed.err"
DROP SCHEMA IF EXISTS e2e CASCADE;
CREATE SCHEMA e2e;
CREATE TYPE e2e.kind AS ENUM ('a', 'b');
CREATE TABLE e2e.items (
  id serial PRIMARY KEY, name text NOT NULL, note varchar(40), fixed char(3), price numeric(12,2), ratio float8,
  big bigint, ok bool, tags text[], meta jsonb, u uuid DEFAULT gen_random_uuid(), d date, ts timestamp,
  tstz timestamptz, blob bytea, k e2e.kind, dur interval, ip inet,
  CONSTRAINT items_price_nonneg CHECK (price >= 0), CONSTRAINT items_name_key UNIQUE (name));
COMMENT ON TABLE e2e.items IS 'e2e fixture table';
COMMENT ON COLUMN e2e.items.name IS 'display name';
CREATE TABLE e2e.orders (id serial PRIMARY KEY,
  item_id int NOT NULL REFERENCES e2e.items(id) ON DELETE CASCADE ON UPDATE RESTRICT, qty int DEFAULT 1);
CREATE INDEX items_name_idx ON e2e.items (lower(name));
CREATE VIEW e2e.item_names AS SELECT id, name FROM e2e.items;
INSERT INTO e2e.items (name, note, fixed, price, ratio, big, ok, tags, meta, d, ts, tstz, blob, k, dur, ip)
SELECT 'item ' || g, 'note ' || g, 'abc', g * 1.25, g / 3.0, 9007199254740993 + g, g % 2 = 0,
       ARRAY['t' || g, 'x'], jsonb_build_object('g', g, 'nested', jsonb_build_object('ok', true)),
       date '2024-01-01' + g, timestamp '2024-01-01 10:00' + (g || ' hours')::interval,
       timestamptz '2024-01-01 10:00+02' + (g || ' hours')::interval, decode('deadbeef', 'hex'),
       (CASE WHEN g % 2 = 0 THEN 'a' ELSE 'b' END)::e2e.kind, (g || ' days')::interval, ('10.0.0.' || g)::inet
FROM generate_series(1, 10) g;
INSERT INTO e2e.items (name, note, price, ratio, big, ok, tags, meta, d, ts, tstz) VALUES
  ('héllo — 日本', NULL, 1234567890.12, -0.0, -1, NULL, ARRAY[]::text[], '{"unicode":"日本"}', NULL, NULL, NULL),
  (repeat('x', 20000), 'long', 0, 5e-324, 0, false, NULL, NULL, '2000-02-29', 'infinity', '-infinity');
INSERT INTO e2e.orders (item_id, qty) SELECT id, id * 2 FROM e2e.items WHERE id <= 5;
ANALYZE e2e.items; ANALYZE e2e.orders;
SQL
then
  pass "seeded schema e2e (12 items, 5 orders, enum, view, index, comments)"
else
  fail "seeded schema e2e" "$(cat "$E2E_TMP/seed.err")"
  mcp_harness_report
fi

cleanup_seed() {
  if [ "$E2E_KEEP" != "1" ]; then
    psql_val 'DROP SCHEMA IF EXISTS e2e CASCADE' >/dev/null
  fi
  if [ "$WRITES_DEPLOYED" = "true" ] && [ "$E2E_KEEP" != "1" ]; then
    echo "restoring the read-only workload..."
    deploy false >/dev/null
  fi
  mcp_harness_cleanup
}
trap cleanup_seed EXIT

# ── server_info ──────────────────────────────────────────────────────────────
echo "== server_info =="
OUT=$(mcp_call server_info '{}')
assert_json "server_info status ok" 'r["result"]["structuredContent"]["status"] == "ok"' "$OUT"
assert_json "server_info reports PostgreSQL 16" 'r["result"]["structuredContent"]["server"]["version"].startswith("PostgreSQL 16")' "$OUT"
assert_json "server_info reports database mcp / user mcp" 'r["result"]["structuredContent"]["server"]["database"] == "mcp" and r["result"]["structuredContent"]["server"]["user"] == "mcp"' "$OUT"
assert_json "server_info reports writes_allowed=false" 'r["result"]["structuredContent"]["config"]["writes_allowed"] is False' "$OUT"
assert_json "server_info reports the row limits" 'r["result"]["structuredContent"]["config"]["default_row_limit"] == 100 and r["result"]["structuredContent"]["config"]["max_row_limit"] == 1000' "$OUT"
assert_json "server_info now is an ISO-8601 UTC timestamp" 'r["result"]["structuredContent"]["server"]["now"].endswith("Z")' "$OUT"

# ── schema tools ─────────────────────────────────────────────────────────────
echo "== list_schemas / list_tables / search_objects =="
OUT=$(mcp_call list_schemas '{}')
assert_json "list_schemas contains e2e" 'any(s["name"] == "e2e" for s in r["result"]["structuredContent"]["schemas"])' "$OUT"
assert_not_contains "list_schemas hides pg_catalog by default" '"pg_catalog"' "$OUT"
OUT=$(mcp_call list_schemas '{"include_system":true}')
assert_contains "list_schemas include_system shows pg_catalog" '"pg_catalog"' "$OUT"
assert_json "list_schemas flags pg_catalog as system" 'any(s["name"] == "pg_catalog" and s["is_system"] for s in r["result"]["structuredContent"]["schemas"])' "$OUT"

OUT=$(mcp_call list_tables '{"schema":"e2e"}')
assert_json "list_tables e2e lists items, orders and the view" 'sorted(t["name"] for t in r["result"]["structuredContent"]["tables"]) == ["item_names", "items", "orders"]' "$OUT"
assert_json "list_tables kinds are table/view" '{t["kind"] for t in r["result"]["structuredContent"]["tables"]} == {"table", "view"}' "$OUT"
assert_json "list_tables carries the table comment" 'any(t.get("comment") == "e2e fixture table" for t in r["result"]["structuredContent"]["tables"])' "$OUT"
OUT=$(mcp_call list_tables '{"schema":"e2e","kinds":["view"]}')
assert_json "list_tables kinds filter keeps only views" '[t["name"] for t in r["result"]["structuredContent"]["tables"]] == ["item_names"]' "$OUT"
OUT=$(mcp_call list_tables '{"schema":"no_such_schema_zz"}')
assert_json "list_tables unknown schema is empty with a hint" 'r["result"]["structuredContent"]["count"] == 0 and "list_schemas" in r["result"]["structuredContent"]["hint"]' "$OUT"
OUT=$(mcp_call list_tables '{"schema":"e2e","kinds":["sequence"]}')
assert_contains "list_tables rejects an unknown kind" 'unknown variant' "$OUT"

OUT=$(mcp_call search_objects '{"pattern":"item"}')
assert_json "search_objects finds the table, the view and the column" 'sorted((m["kind"], m["name"]) for m in r["result"]["structuredContent"]["matches"]) == [("column", "item_id"), ("table", "items"), ("view", "item_names")]' "$OUT"
OUT=$(mcp_call search_objects '{"pattern":"%"}')
assert_json "search_objects treats % literally" 'r["result"]["structuredContent"]["count"] == 0' "$OUT"
OUT=$(mcp_call search_objects '{"pattern":"item%","glob":true,"kinds":["table"]}')
assert_json "search_objects glob mode uses ILIKE wildcards" '[m["name"] for m in r["result"]["structuredContent"]["matches"]] == ["items"]' "$OUT"
OUT=$(mcp_call search_objects '{"pattern":"'"'"'; DROP TABLE e2e.items; --"}')
assert_json "search_objects injection-shaped pattern is literal" 'r["result"]["structuredContent"]["count"] == 0' "$OUT"
assert_contains "e2e.items still exists after the injection-shaped search" '12' "$(psql_val 'select count(*) from e2e.items')"
OUT=$(mcp_call search_objects '{"pattern":"日本"}')
assert_json "search_objects accepts unicode" 'r["result"]["structuredContent"]["count"] == 0' "$OUT"
OUT=$(mcp_call search_objects '{"pattern":"%","glob":true,"include_system":true,"limit":99999}')
assert_json "search_objects clamps limit to 500 and flags truncation (pg_catalog holds thousands of objects)" 'r["result"]["structuredContent"]["count"] == 500 and len(r["result"]["structuredContent"]["matches"]) == 500 and r["result"]["structuredContent"]["truncated"] is True' "$OUT"
OUT=$(mcp_call search_objects '{"pattern":"%","glob":true,"include_system":true}')
assert_json "search_objects default limit is 50 (truncated)" 'r["result"]["structuredContent"]["count"] == 50 and r["result"]["structuredContent"]["truncated"] is True' "$OUT"
OUT=$(mcp_call search_objects '{"pattern":"item","limit":2}')
assert_json "search_objects limit 2 of 3 matches is truncated" 'r["result"]["structuredContent"]["count"] == 2 and r["result"]["structuredContent"]["truncated"] is True' "$OUT"
OUT=$(mcp_call search_objects '{"pattern":""}')
assert_contains "search_objects rejects an empty pattern" 'pattern must be 1..200' "$OUT"
OUT=$(mcp_call search_objects "{\"pattern\":\"$(printf 'a%.0s' $(seq 1 201))\"}")
assert_contains "search_objects rejects a 201-char pattern" 'pattern must be 1..200' "$OUT"
OUT=$(mcp_call search_objects '{"pattern":"e","limit":0}')
assert_contains "search_objects rejects limit 0" 'limit must be between 1 and 500' "$OUT"
OUT=$(mcp_call search_objects '{"pattern":"pg_sleep","include_system":true,"kinds":["function"]}')
assert_json "search_objects include_system reaches pg_catalog functions (pg_sleep)" 'any(m["name"] == "pg_sleep" and m["schema"] == "pg_catalog" and m["kind"] == "function" for m in r["result"]["structuredContent"]["matches"])' "$OUT"
OUT=$(mcp_call search_objects '{"pattern":"pg_sleep","kinds":["function"]}')
assert_json "search_objects hides pg_catalog without include_system (pg_sleep absent)" 'r["result"]["structuredContent"]["count"] == 0' "$OUT"

# ── describe_table / list_indexes / table_stats ──────────────────────────────
echo "== describe_table / list_indexes / table_stats =="
OUT=$(mcp_call describe_table '{"table":"e2e.items"}')
assert_json "describe_table lists all 18 columns" 'len(r["result"]["structuredContent"]["columns"]) == 18' "$OUT"
assert_json "describe_table primary key is id" 'r["result"]["structuredContent"]["primary_key"] == ["id"]' "$OUT"
assert_json "describe_table shows the enum type and its values" 'any(c["name"] == "k" and c["data_type"] == "e2e.kind" and c["enum_values"] == ["a", "b"] for c in r["result"]["structuredContent"]["columns"])' "$OUT"
assert_json "describe_table shows numeric(12,2) and nullable/default" 'any(c["name"] == "price" and c["data_type"] == "numeric(12,2)" and c["nullable"] for c in r["result"]["structuredContent"]["columns"]) and any(c["name"] == "id" and not c["nullable"] and "nextval" in (c["default_value"] or "") for c in r["result"]["structuredContent"]["columns"])' "$OUT"
assert_json "describe_table carries column and table comments" 'r["result"]["structuredContent"]["comment"] == "e2e fixture table" and any(c["comment"] == "display name" for c in r["result"]["structuredContent"]["columns"])' "$OUT"
assert_json "describe_table lists the unique and check constraints" '[c["name"] for c in r["result"]["structuredContent"]["unique_constraints"]] == ["items_name_key"] and [c["name"] for c in r["result"]["structuredContent"]["check_constraints"]] == ["items_price_nonneg"]' "$OUT"
assert_json "describe_table lists 3 indexes incl. the expression index" 'sorted(i["name"] for i in r["result"]["structuredContent"]["indexes"]) == ["items_name_idx", "items_name_key", "items_pkey"] and any("lower(name)" in i["definition"] for i in r["result"]["structuredContent"]["indexes"])' "$OUT"
assert_json "describe_table param_hints cover id, price, uuid, timestamptz" '{h["column"] for h in r["result"]["structuredContent"]["param_hints"]} >= {"id", "price", "u", "tstz", "d", "k", "dur", "fixed"}' "$OUT"
OUT=$(mcp_call describe_table '{"table":"orders","schema":"e2e"}')
assert_json "describe_table foreign key with referenced table and ON DELETE/UPDATE" 'r["result"]["structuredContent"]["foreign_keys"][0]["referenced_table"] == "items" and r["result"]["structuredContent"]["foreign_keys"][0]["referenced_columns"] == ["id"] and r["result"]["structuredContent"]["foreign_keys"][0]["on_delete"] == "CASCADE" and r["result"]["structuredContent"]["foreign_keys"][0]["on_update"] == "RESTRICT"' "$OUT"
OUT=$(mcp_call describe_table '{"table":"\"e2e\".\"item_names\""}')
assert_json "describe_table on a quoted view carries view_definition" 'r["result"]["structuredContent"]["kind"] == "view" and "SELECT" in r["result"]["structuredContent"]["view_definition"]' "$OUT"
OUT=$(mcp_call describe_table '{"table":"nope","schema":"e2e"}')
assert_contains "describe_table missing table is a clean error with hints" 'no table or view named e2e.nope' "$OUT"
OUT=$(mcp_call describe_table '{"table":"../../etc/passwd"}')
assert_contains "describe_table rejects a malformed reference" 'invalid table reference' "$OUT"
OUT=$(mcp_call describe_table '{"table":""}')
assert_contains "describe_table rejects an empty name" 'invalid table reference' "$OUT"
OUT=$(mcp_call describe_table '{}')
assert_contains "describe_table without table is a parameter error" 'missing field `table`' "$OUT"

OUT=$(mcp_call list_indexes '{"schema":"e2e"}')
assert_json "list_indexes for a schema lists 4 indexes with usage counters" 'r["result"]["structuredContent"]["count"] == 4 and all("scans" in i for i in r["result"]["structuredContent"]["indexes"])' "$OUT"
OUT=$(mcp_call list_indexes '{"table":"e2e.orders"}')
assert_json "list_indexes for one table" '[i["name"] for i in r["result"]["structuredContent"]["indexes"]] == ["orders_pkey"] and r["result"]["structuredContent"]["indexes"][0]["is_primary"]' "$OUT"
OUT=$(mcp_call list_indexes '{"schema":"no_such_schema_zz"}')
assert_json "list_indexes unknown schema is empty" 'r["result"]["structuredContent"]["count"] == 0' "$OUT"

OUT=$(mcp_call table_stats '{"table":"e2e.items","exact_count":true}')
assert_json "table_stats exact_row_count is 12" 'r["result"]["structuredContent"]["exact_row_count"] == 12' "$OUT"
assert_json "table_stats carries estimated rows, inserts, sizes and last_analyze" 'r["result"]["structuredContent"]["estimated_rows"] == 12 and r["result"]["structuredContent"]["inserts"] == 12 and r["result"]["structuredContent"]["total_bytes"] > 0 and r["result"]["structuredContent"]["last_analyze"] is not None' "$OUT"
OUT=$(mcp_call table_stats '{"table":"item_names","schema":"e2e"}')
assert_contains "table_stats on a view is a clean error" 'views have no statistics' "$OUT"
OUT=$(mcp_call table_stats '{"table":"e2e.items\"; DROP TABLE e2e.items; --","exact_count":true}')
assert_contains "table_stats injection-shaped name is a clean error" 'invalid table reference' "$OUT"
assert_contains "e2e.items still has 12 rows" '12' "$(psql_val 'select count(*) from e2e.items')"

# ── query: values ───────────────────────────────────────────────────────────
echo "== query: value rendering =="
OUT=$(mcp_call query '{"sql":"SELECT id, name, note, price::text AS price, price::float8 AS pricef, ratio, big, ok, tags, meta, u::text AS u, d, ts, tstz, blob, k::text AS k, dur::text AS dur, EXTRACT(EPOCH FROM dur)::float8 AS secs, ip FROM e2e.items WHERE id = 2"}')
assert_json "query renders int/text/varchar/float/bigint/bool" 'r["result"]["structuredContent"]["rows"][0][0] == 2 and r["result"]["structuredContent"]["rows"][0][1] == "item 2" and r["result"]["structuredContent"]["rows"][0][2] == "note 2" and abs(r["result"]["structuredContent"]["rows"][0][5] - 2/3.0) < 1e-12 and r["result"]["structuredContent"]["rows"][0][6] == 9007199254740995 and r["result"]["structuredContent"]["rows"][0][7] is True' "$OUT"
assert_json "query renders numeric via ::text exactly and ::float8 as a number" 'r["result"]["structuredContent"]["rows"][0][3] == "2.50" and r["result"]["structuredContent"]["rows"][0][4] == 2.5' "$OUT"
assert_json "query renders text[] and jsonb structurally" 'r["result"]["structuredContent"]["rows"][0][8] == ["t2", "x"] and r["result"]["structuredContent"]["rows"][0][9]["nested"]["ok"] is True' "$OUT"
assert_json "query renders uuid, date, timestamp, timestamptz (UTC)" 'len(r["result"]["structuredContent"]["rows"][0][10]) == 36 and r["result"]["structuredContent"]["rows"][0][11] == "2024-01-03" and r["result"]["structuredContent"]["rows"][0][12] == "2024-01-01T12:00:00" and r["result"]["structuredContent"]["rows"][0][13] == "2024-01-01T10:00:00Z"' "$OUT"
assert_json "query renders bytea as base64, enum/interval via casts, inet" 'r["result"]["structuredContent"]["rows"][0][14] == "3q2+7w==" and r["result"]["structuredContent"]["rows"][0][15] == "a" and r["result"]["structuredContent"]["rows"][0][16] == "2 days" and r["result"]["structuredContent"]["rows"][0][17] == 172800.0 and r["result"]["structuredContent"]["rows"][0][18] == "10.0.0.2"' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT name, meta, tags, note FROM e2e.items WHERE id = 11"}')
assert_json "query round-trips unicode, empty arrays and NULL" 'r["result"]["structuredContent"]["rows"][0] == ["héllo — 日本", {"unicode": "日本"}, [], None]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT ratio, ts::text AS ts, tstz::text AS tstz, length(name) AS len FROM e2e.items WHERE id = 12"}')
assert_json "query renders a subnormal float and infinite timestamps" 'r["result"]["structuredContent"]["rows"][0][0] == 5e-324 and r["result"]["structuredContent"]["rows"][0][1] == "infinity" and r["result"]["structuredContent"]["rows"][0][2] == "-infinity" and r["result"]["structuredContent"]["rows"][0][3] == 20000' "$OUT"
OUT=$(mcp_call query "{\"sql\":\"SELECT 'NaN'::float8 AS nan, 'Infinity'::float8 AS inf, -0.0::float8 AS negz, 1.5::float4 AS f4, B'1011'::bit(4) AS b, '<a/>'::xml AS x, point(1,2) AS p\"}")
assert_json "query renders NaN/Infinity as strings, float4, bit, xml, point" 'r["result"]["structuredContent"]["rows"][0] == ["NaN", "Infinity", -0.0, 1.5, {"bits": 4, "hex": "b0"}, "<a/>", [1.0, 2.0]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT name FROM e2e.items WHERE id = 12"}')
assert_json "query returns a 20 KB cell intact (under the 32 K cell cap)" 'len(r["result"]["structuredContent"]["rows"][0][0]) == 20000' "$OUT"
OUT=$(mcp_call query "{\"sql\":\"SELECT repeat('y', 40000) AS y\"}")
assert_contains "query cuts a cell over 32 K chars with a marker" '…[truncated: 32768 of 40000 chars shown]' "$OUT"

echo "== query: unsupported column types =="
OUT=$(mcp_call query '{"sql":"SELECT id, k FROM e2e.items LIMIT 1"}')
assert_contains "query on an enum column is a conversion error naming the column" 'cannot convert a result column (`k`)' "$OUT"
assert_contains "the enum error carries the cast hint" 'k::text' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT dur FROM e2e.items LIMIT 1"}')
assert_contains "query on an interval column is a conversion error" 'cannot convert a result column (`dur`)' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT price FROM e2e.items ORDER BY id LIMIT 3"}')
assert_contains "query on a numeric column explains the host limitation" 'numeric/decimal/money' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT * FROM information_schema.tables WHERE table_schema = '"'"'e2e'"'"' ORDER BY table_name LIMIT 1"}')
assert_json "information_schema (domain-typed columns) works: the wire reports base types" 'r["result"]["structuredContent"]["row_count"] == 1 and "table_name" in r["result"]["structuredContent"]["columns"]' "$OUT"
OUT=$(mcp_call query "{\"sql\":\"SELECT 'x'::char(3) AS c\"}")
assert_contains "query on a char(n) column is a conversion error naming the column" 'cannot convert a result column (`c`)' "$OUT"
OUT=$(mcp_call query "{\"sql\":\"SELECT int4range(1,5) AS r\"}")
assert_contains "query on a range column is a conversion error" 'cannot convert a result column (`r`)' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT column_name::text, data_type::text FROM information_schema.columns WHERE table_schema = '"'"'e2e'"'"' AND table_name = '"'"'orders'"'"' ORDER BY ordinal_position"}')
assert_json "information_schema works with ::text casts" 'r["result"]["structuredContent"]["rows"][0] == ["id", "integer"]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT pg_sleep(0)"}')
assert_contains "query on a void column is a conversion error" 'cannot convert a result column' "$OUT"

# ── query: limits and truncation ─────────────────────────────────────────────
echo "== query: limits =="
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items ORDER BY id","limit":5}')
assert_json "query limit 5 returns 5 rows and truncated=true" 'r["result"]["structuredContent"]["row_count"] == 5 and r["result"]["structuredContent"]["truncated"] is True and r["result"]["structuredContent"]["limit_applied"] == 5' "$OUT"
assert_contains "query truncation explains how to paginate" 'paginate with ORDER BY' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items ORDER BY id","limit":5000}')
assert_json "query limit 5000 is clamped to 1000" 'r["result"]["structuredContent"]["limit_applied"] == 1000 and r["result"]["structuredContent"]["row_count"] == 12 and r["result"]["structuredContent"]["truncated"] is False' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items","limit":0}')
assert_contains "query limit 0 is refused" 'limit must be between 1 and 1000' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items","limit":-1}')
assert_contains "query limit -1 is refused" 'limit must be between 1 and 1000' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items","limit":"x"}')
assert_contains "query limit 'x' is a parameter error, not a crash" 'expected i64' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items","limit":9223372036854775807}')
assert_json "query limit i64::MAX is clamped" 'r["result"]["structuredContent"]["limit_applied"] == 1000' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items ORDER BY id"}')
assert_json "query default limit is 100" 'r["result"]["structuredContent"]["limit_applied"] == 100' "$OUT"
OUT=$(mcp_call query "{\"sql\":\"SELECT repeat('y', 20000) AS y FROM generate_series(1, 200)\",\"limit\":200}")
assert_json "query result over POSTGRES_MAX_RESULT_BYTES is cut with truncated_reason" 'r["result"]["structuredContent"]["truncated"] is True and "POSTGRES_MAX_RESULT_BYTES" in r["result"]["structuredContent"]["truncated_reason"] and r["result"]["structuredContent"]["row_count"] < 200' "$OUT"
OUT=$(mcp_call query "$(python3 -c 'import json; print(json.dumps({"sql": "SELECT 1 -- " + "x" * 300000}))')")
assert_contains "query refuses SQL over 256 KiB" 'the limit is 262144 bytes' "$OUT"
OUT=$(mcp_call query "$(python3 -c 'import json; print(json.dumps({"sql": "SELECT 1", "params": list(range(101))}))')")
assert_contains "query refuses 101 params" 'too many parameters: 101' "$OUT"
OUT=$(mcp_call query '{"sql":""}')
assert_contains "query refuses an empty statement" 'the statement is empty' "$OUT"
OUT=$(mcp_call query '{"sql":"-- nothing\n/* insert */"}')
assert_contains "query refuses a comment-only statement" 'the statement is empty' "$OUT"
OUT=$(mcp_call query '{}')
assert_contains "query without sql is a parameter error" 'missing field `sql`' "$OUT"

# ── query: parameters ────────────────────────────────────────────────────────
echo "== query: parameters =="
OUT=$(mcp_call query '{"sql":"SELECT id, name FROM e2e.items WHERE id = $1","params":[3]}')
assert_json "bare integer binds an int4 (serial) column" 'r["result"]["structuredContent"]["rows"] == [[3, "item 3"]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items ORDER BY id LIMIT $1","params":[2]}')
assert_json "bare integer binds LIMIT (int8)" 'r["result"]["structuredContent"]["rows"] == [[1], [2]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items WHERE big = $1","params":[9007199254740996]}')
assert_json "bare integer binds a bigint column" 'r["result"]["structuredContent"]["rows"] == [[3]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items WHERE price > $1 ORDER BY id","params":[10]}')
assert_json "bare integer binds a numeric column (no silent zero)" 'r["result"]["structuredContent"]["rows"] == [[9], [10], [11]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items WHERE price > $1 ORDER BY id","params":[10.5]}')
assert_json "bare float binds a numeric column" 'r["result"]["structuredContent"]["rows"] == [[9], [10], [11]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items WHERE ratio > $1 ORDER BY id","params":[3.0]}')
assert_json "bare float binds a float8 column" 'r["result"]["structuredContent"]["rows"] == [[10]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items WHERE id = $1","params":[3.0]}')
assert_contains "bare float against an int4 column fails with the encodings tried" 'Parameter encodings tried: $1 sent as numeric, then retried as float8' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT count(*)::int AS n FROM e2e.items WHERE name = $1","params":[3]}')
assert_contains "bare integer against a text column fails cleanly (NUL byte)" 'Parameter encodings tried' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id, name FROM e2e.items WHERE id = $1","params":[{"type":"int4","value":3}]}')
assert_json "typed int4 param" 'r["result"]["structuredContent"]["rows"] == [[3, "item 3"]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items WHERE id = $1::text::int","params":["3"]}')
assert_json "string param with a ::text::int cast" 'r["result"]["structuredContent"]["rows"] == [[3]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items WHERE id = $1","params":["3"]}')
assert_json "string param against an int4 column is re-encoded automatically" 'r["result"]["structuredContent"]["rows"] == [[3]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items WHERE d = $1","params":["2024-01-03"]}')
assert_json "string param against a date column" 'r["result"]["structuredContent"]["rows"] == [[2]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items WHERE tstz = $1","params":["2024-01-01T11:00:00+02:00"]}')
assert_json "string param with zone against a timestamptz column" 'r["result"]["structuredContent"]["rows"] == [[1]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items WHERE tstz > $1 ORDER BY id","params":[{"type":"timestamptz","value":"2024-01-01T12:00:00+02:00"}]}')
assert_json "typed timestamptz param converts the offset to UTC" 'r["result"]["structuredContent"]["rows"][0] == [3] and len(r["result"]["structuredContent"]["rows"]) == 8' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items WHERE d = $1","params":[{"type":"date","value":"2024-01-03"}]}')
assert_json "typed date param" 'r["result"]["structuredContent"]["rows"] == [[2]]' "$OUT"
# Multibyte text in a zone / fraction: every parser must fail cleanly (a
# byte-length slice here used to trap the instance → HTTP 500, empty body).
OUT=$(mcp_call query '{"sql":"SELECT $1::text AS t","params":[{"type":"timestamptz","value":"2024-01-01T10:00+a€"}]}')
assert_json "typed timestamptz with a multibyte zone is a clean tool error (no trap)" 'r["result"]["isError"] is True and "expected an ISO-8601 timestamp with zone" in r["result"]["content"][0]["text"]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT $1::text AS t","params":[{"type":"timestamp","value":"2024-01-01T10:00+a€"}]}')
assert_json "typed timestamp with a multibyte zone is a clean tool error" 'r["result"]["isError"] is True and "expected an ISO-8601 timestamp" in r["result"]["content"][0]["text"]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT $1::text AS t","params":[{"type":"time","value":"10:00+a€"}]}')
assert_json "typed time with a multibyte zone is a clean tool error" 'r["result"]["isError"] is True and "expected HH:MM" in r["result"]["content"][0]["text"]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT $1::text AS t","params":[{"type":"time","value":"10:00:00.€€€"}]}')
assert_json "typed time with multibyte fraction digits is a clean tool error" 'r["result"]["isError"] is True and "expected HH:MM" in r["result"]["content"][0]["text"]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT $1::text AS t","params":[{"type":"timestamptz","value":"2024-01-01T10:00:00+1a3b"}]}')
assert_json "typed timestamptz with a non-digit 4-char zone is a clean tool error" 'r["result"]["isError"] is True and "expected an ISO-8601 timestamp with zone" in r["result"]["content"][0]["text"]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items WHERE u = $1","params":["2024-01-01T10:00+a€"]}')
assert_json "bare string with a multibyte zone against a uuid column: the retry chain ends in a clean error" 'r["result"]["isError"] is True and "pass a typed param" in r["result"]["content"][0]["text"]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT ($1::timestamptz AT TIME ZONE $$UTC$$)::text AS t","params":[{"type":"timestamptz","value":"2024-01-01T10:00:00+0530"}]}')
assert_json "typed timestamptz with a 4-digit +HHMM zone still converts" 'r["result"]["structuredContent"]["rows"] == [["2024-01-01 04:30:00"]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT ($1::timestamptz AT TIME ZONE $$UTC$$)::text AS t","params":[{"type":"timestamptz","value":"2024-01-01T10:00:00.25-08"}]}')
assert_json "typed timestamptz with a fraction and a -HH zone" 'r["result"]["structuredContent"]["rows"] == [["2024-01-01 18:00:00.25"]]' "$OUT"
OUT=$(mcp_call server_info '{}')
assert_json "the instance is still alive after the malformed zones (no restart needed)" 'r["result"]["structuredContent"]["status"] == "ok"' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT count(*)::int AS n FROM e2e.items WHERE u = $1","params":["123e4567-e89b-12d3-a456-426614174000"]}')
assert_json "uuid string param against a uuid column" 'r["result"]["structuredContent"]["rows"] == [[0]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items WHERE u = $1","params":["not-a-uuid"]}')
assert_contains "non-uuid string against a uuid column fails with the typed-param hint" 'pass a typed param' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT count(*)::int AS n FROM e2e.items WHERE ok = $1","params":["true"]}')
assert_json "string param against a bool column" 'r["result"]["structuredContent"]["rows"] == [[5]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items WHERE meta @> $1","params":[{"g":2}]}')
assert_json "bare object param binds jsonb" 'r["result"]["structuredContent"]["rows"] == [[2]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items WHERE tags && $1 ORDER BY id","params":[["t2","t3"]]}')
assert_json "bare string array param binds text[]" 'r["result"]["structuredContent"]["rows"] == [[2], [3]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items WHERE id = ANY($1) ORDER BY id","params":[{"type":"int4[]","value":[1,2]}]}')
assert_json "typed int4[] param" 'r["result"]["structuredContent"]["rows"] == [[1], [2]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items WHERE d > $1 AND id = $2","params":["2024-01-01","3"]}')
assert_json "two string params (date + int) are re-encoded independently" 'r["result"]["structuredContent"]["rows"] == [[3]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items WHERE blob = $1","params":[{"type":"bytea","value":"3q2+7w=="}]}')
assert_json "typed bytea param (base64)" 'len(r["result"]["structuredContent"]["rows"]) == 10' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.items WHERE blob = $1","params":[{"type":"bytea","value":"not base64!!"}]}')
assert_contains "typed bytea with invalid base64 is a clean error" 'expected base64' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT 1","params":[{"type":"wat","value":1}]}')
assert_contains "unknown typed param type is a clean error listing known types" 'unknown parameter type' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT $1::text AS a, $2::text AS b","params":["a"]}')
assert_contains "param count mismatch names the rule" 'highest $N placeholder' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT $1","params":["a"]}')
assert_json 'untyped SELECT $1 with a string still works (text)' 'r["result"]["structuredContent"]["rows"] == [["a"]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT count(*)::int AS n FROM e2e.items WHERE name = $1","params":["x'"'"'; DROP TABLE e2e.items; --"]}')
assert_json "injection-shaped string param stays a literal" 'r["result"]["structuredContent"]["rows"] == [[0]]' "$OUT"
assert_contains "e2e.items survives the injection-shaped param" '12' "$(psql_val 'select count(*) from e2e.items')"

# ── query: read-only inspector and refusals ──────────────────────────────────
echo "== query: read-only mode =="
OUT=$(mcp_call query '{"sql":"INSERT INTO e2e.items (name) VALUES ($1)","params":["z"]}')
assert_contains "INSERT via query is refused in read-only mode" 'read-only mode: `INSERT` is not allowed' "$OUT"
assert_contains "the refusal names POSTGRES_ALLOW_WRITES" 'POSTGRES_ALLOW_WRITES' "$OUT"
OUT=$(mcp_call query '{"sql":"WITH d AS (DELETE FROM e2e.items RETURNING 1) SELECT 1"}')
assert_contains "data-modifying CTE is refused" 'read-only mode: `DELETE`' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT * FROM e2e.orders FOR UPDATE"}')
assert_contains "SELECT … FOR UPDATE is refused" 'FOR UPDATE/SHARE' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT id INTO e2e.copy FROM e2e.items"}')
assert_contains "SELECT INTO is refused" 'read-only mode: `INTO`' "$OUT"
OUT=$(mcp_call query "{\"sql\":\"SELECT nextval('e2e.items_id_seq')\"}")
assert_contains "nextval() is refused" 'read-only mode: `NEXTVAL`' "$OUT"
# Quoted identifiers are still function calls: "nextval"(…) must be refused
# like nextval(…), including the schema-qualified and U&"…" spellings.
SEQ_BEFORE=$(psql_val 'select last_value from e2e.items_id_seq')
OUT=$(mcp_call query "{\"sql\":\"SELECT \\\"nextval\\\"('e2e.items_id_seq')\"}")
assert_contains "quoted \"nextval\"() is refused" 'read-only mode: `NEXTVAL`' "$OUT"
OUT=$(mcp_call query "{\"sql\":\"SELECT pg_catalog.\\\"set_config\\\"('e2e.x', '1', false) AS v\"}")
assert_contains "schema-qualified quoted \"set_config\"() is refused" 'read-only mode: `SET_CONFIG`' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT \"pg_advisory_unlock_all\"()::text"}')
assert_contains "quoted \"pg_advisory_unlock_all\"() is refused" 'read-only mode: `PG_ADVISORY_UNLOCK_ALL`' "$OUT"
OUT=$(mcp_call query "{\"sql\":\"SELECT U&\\\"\\\\006eextval\\\"('e2e.items_id_seq')\"}")
assert_contains "U&\"…\" unicode-escaped nextval is decoded and refused" 'read-only mode: `NEXTVAL`' "$OUT"
OUT=$(mcp_call query "{\"sql\":\"SELECT U&\\\"!0073et_config\\\" UESCAPE '!'('e2e.x', '1', false)\"}")
assert_contains "U&\"…\" UESCAPE spelling of set_config is decoded and refused" 'read-only mode: `SET_CONFIG`' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT pg_stat_reset()"}')
assert_contains "pg_stat_reset() is refused" 'read-only mode: `PG_STAT_RESET`' "$OUT"
assert_contains "the sequence was not advanced by the refused calls" "$SEQ_BEFORE" "$(psql_val 'select last_value from e2e.items_id_seq')"
OUT=$(mcp_call query '{"sql":"SELECT \"NEXTVAL\" FROM (SELECT 1 AS \"NEXTVAL\") q"}')
assert_json "a quoted identifier in another case is not the function (case-sensitive match)" 'r["result"]["structuredContent"]["rows"] == [[1]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT 1 AS \"insert\", 2 AS \"update\", 3 AS \"select\", 4 AS \"into\""}')
assert_json "quoted identifiers never act as keywords" 'r["result"]["structuredContent"]["rows"] == [[1, 2, 3, 4]]' "$OUT"
OUT=$(mcp_call query '{"sql":"\"select\" 1"}')
assert_contains "a quoted first word is not a statement" 'is not a SQL statement keyword' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT 1; SELECT 2"}')
assert_contains "two statements are refused" 'only one statement per call' "$OUT"
OUT=$(mcp_call query '{"sql":"BEGIN"}')
assert_contains "BEGIN is refused as session control" 'there are no sessions' "$OUT"
OUT=$(mcp_call query '{"sql":"SET search_path TO e2e"}')
assert_contains "SET is refused as session control" 'there are no sessions' "$OUT"
OUT=$(mcp_call query '{"sql":"SELEC 1"}')
assert_contains "a non-statement leading word is reported as a syntax problem" 'is not a SQL statement keyword' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT 1 AS x;"}')
assert_json "a trailing semicolon is fine" 'r["result"]["structuredContent"]["rows"] == [[1]]' "$OUT"
OUT=$(mcp_call query "{\"sql\":\"SELECT 'DROP TABLE x; DELETE' AS s, \\\"name\\\" FROM e2e.items WHERE name = 'update'\"}")
assert_json "keywords inside string literals and quoted identifiers do not trip the inspector" 'r["result"]["structuredContent"]["row_count"] == 0' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT $$delete from$$ AS s, $tag$insert$tag$ AS t"}')
assert_json "keywords inside dollar-quoted strings do not trip the inspector" 'r["result"]["structuredContent"]["rows"] == [["delete from", "insert"]]' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT name AS comment, note AS load, ok AS refresh, id AS security FROM e2e.items WHERE id = 1"}')
assert_json "non-reserved DDL words used as identifiers are not false positives" 'r["result"]["structuredContent"]["row_count"] == 1' "$OUT"
OUT=$(mcp_call query '{"sql":"EXPLAIN (FORMAT JSON) CREATE TABLE e2e.zz AS SELECT 1"}')
assert_contains "EXPLAIN of CREATE TABLE AS is refused (DDL at the inner head)" 'read-only mode: `CREATE`' "$OUT"
OUT=$(mcp_call query '{"sql":"EXPLAIN ANALYZE INSERT INTO e2e.items (name) VALUES ($$zz$$)"}')
assert_contains "EXPLAIN ANALYZE of an INSERT is refused" 'read-only mode: `INSERT`' "$OUT"
OUT=$(mcp_call explain_query '{"sql":"CREATE TABLE e2e.zz AS SELECT 1","analyze":true}')
assert_contains "explain_query analyze of CREATE TABLE AS is refused" 'read-only mode: `CREATE`' "$OUT"
assert_contains "no e2e.zz table was created by the refused EXPLAINs" '0' "$(psql_val "select count(*) from pg_tables where schemaname = 'e2e' and tablename = 'zz'")"
OUT=$(mcp_call query '{"sql":"SHOW search_path"}')
assert_json "SHOW is allowed" 'r["result"]["structuredContent"]["columns"] == ["search_path"]' "$OUT"
OUT=$(mcp_call query '{"sql":"VALUES (1,2),(3,4)"}')
assert_json "VALUES is allowed" 'r["result"]["structuredContent"]["rows"] == [[1, 2], [3, 4]]' "$OUT"
OUT=$(mcp_call query '{"sql":"TABLE e2e.orders"}')
assert_json "TABLE is allowed" 'r["result"]["structuredContent"]["row_count"] == 5' "$OUT"
OUT=$(mcp_call query '{"sql":"EXPLAIN SELECT 1"}')
assert_contains "EXPLAIN via query is allowed" 'QUERY PLAN' "$OUT"
OUT=$(mcp_call execute '{"sql":"INSERT INTO e2e.items (name) VALUES ($1)","params":["z"]}')
assert_contains "execute is refused while writes are off" '`execute` is disabled' "$OUT"
assert_contains "the execute refusal says nothing was executed" 'Nothing was executed' "$OUT"
OUT=$(mcp_call execute_batch '{"sql":"CREATE TABLE e2e.t (x int); DROP TABLE e2e.t"}')
assert_contains "execute_batch is refused while writes are off" '`execute_batch` is disabled' "$OUT"
assert_contains "row count unchanged after the refused writes" '12' "$(psql_val 'select count(*) from e2e.items')"

# ── query: database error mapping ────────────────────────────────────────────
echo "== query: error mapping =="
OUT=$(mcp_call query '{"sql":"SELECT * FROM e2e.nothere"}')
assert_contains "42P01 undefined_table with the list_tables hint" 'ERROR 42P01 (undefined_table)' "$OUT"
assert_contains "42P01 hint mentions list_tables" 'list_tables' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT nope FROM e2e.items"}')
assert_contains "42703 undefined_column with the describe_table hint" 'ERROR 42703 (undefined_column)' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT FROM WHERE"}')
assert_contains "42601 syntax_error with position" 'ERROR 42601 (syntax_error): syntax error at or near' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT 1/0"}')
assert_contains "22012 division_by_zero is mapped" '22012 (division_by_zero)' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT '"'"'abc'"'"'::int"}')
assert_contains "22P02 invalid_text_representation is mapped" '22P02 (invalid_text_representation)' "$OUT"

# ── explain_query ─────────────────────────────────────────────────────────────
echo "== explain_query =="
OUT=$(mcp_call explain_query '{"sql":"SELECT id FROM e2e.items WHERE id = $1","params":[{"type":"int4","value":3}]}')
assert_json "explain_query returns the JSON plan with a Node Type" '"Node Type" in r["result"]["structuredContent"]["plan"][0]["Plan"]' "$OUT"
assert_json "explain_query plan_summary carries node type and cost" 'r["result"]["structuredContent"]["plan_summary"]["node_type"] and r["result"]["structuredContent"]["plan_summary"]["total_cost"] > 0 and r["result"]["structuredContent"]["plan_summary"]["analyzed"] is False' "$OUT"
OUT=$(mcp_call explain_query '{"sql":"SELECT count(*) FROM e2e.items","analyze":true}')
assert_json "explain_query analyze=true on a SELECT fills actual timings" 'r["result"]["structuredContent"]["plan_summary"]["analyzed"] is True and r["result"]["structuredContent"]["plan_summary"]["execution_time_ms"] is not None and r["result"]["structuredContent"]["plan_summary"]["actual_rows"] == 1' "$OUT"
OUT=$(mcp_call explain_query '{"sql":"UPDATE e2e.items SET name = name","analyze":true}')
assert_contains "explain_query analyze=true on an UPDATE is refused" 'read-only mode: `UPDATE`' "$OUT"
assert_contains "row count unchanged after the refused EXPLAIN ANALYZE" '12' "$(psql_val 'select count(*) from e2e.items')"
OUT=$(mcp_call explain_query '{"sql":"EXPLAIN SELECT 1"}')
assert_contains "explain_query refuses a leading EXPLAIN" 'without a leading EXPLAIN' "$OUT"
OUT=$(mcp_call explain_query '{"sql":"SELECT 1; SELECT 2"}')
assert_contains "explain_query refuses two statements" 'only one statement per call' "$OUT"
OUT=$(mcp_call explain_query '{"sql":"SELECT * FROM e2e.nothere"}')
assert_contains "explain_query maps 42P01" '42P01' "$OUT"

# ── timeout ──────────────────────────────────────────────────────────────────
echo "== timeout (POSTGRES_QUERY_TIMEOUT_MS) =="
# The harness's mcp_call gives curl 30 s — exactly the server deadline — so
# this one call uses a longer client timeout.
OUT=$(printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":99,\"method\":\"tools/call\",\"params\":{\"name\":\"query\",\"arguments\":{\"sql\":\"SELECT pg_sleep(40)::text\"},$META}}" \
  | curl -sS --max-time 90 -X POST "$MCP_BASE" -H "$CT" -H "$ACCEPT" -H "$PV" -H 'Mcp-Method: tools/call' -H 'Mcp-Name: query' --data-binary @-)
assert_contains "a 40 s statement times out at the 30 s deadline" 'timeout: no result within 30000 ms' "$OUT"
assert_contains "the timeout hint mentions statement_timeout" 'statement_timeout' "$OUT"
OUT=$(mcp_call query '{"sql":"SELECT 1 AS alive"}')
assert_json "server alive after the timeout" 'r["result"]["structuredContent"]["rows"] == [[1]]' "$OUT"

# ── writes (opt-in) ──────────────────────────────────────────────────────────
if [ "$E2E_ALLOW_WRITES" = "1" ]; then
  echo "== writes (POSTGRES_ALLOW_WRITES=true) =="
  WRITES_DEPLOYED=true
  if deploy true; then pass "write-enabled workload is running"; else fail "write-enabled workload is running" "$(workload_state)"; fi
  OUT=$(mcp_call server_info '{}')
  assert_json "server_info reports writes_allowed=true" 'r["result"]["structuredContent"]["config"]["writes_allowed"] is True' "$OUT"
  OUT=$(mcp_call execute '{"sql":"INSERT INTO e2e.items (name, price, big) VALUES ($1, $2, $3)","params":["w1", {"type":"numeric","value":"9.99"}, 5]}')
  assert_json "execute INSERT with typed params → rows_affected 1" 'r["result"]["structuredContent"]["rows_affected"] == 1' "$OUT"
  OUT=$(mcp_call execute '{"sql":"INSERT INTO e2e.items (name, price, big) VALUES ($1, $2, $3)","params":["w2", 8.5, 6]}')
  assert_json "execute INSERT with bare params (same SQL → cached token)" 'r["result"]["structuredContent"]["rows_affected"] == 1' "$OUT"
  assert_contains "psql confirms the inserted rows and exact numeric values" 'w1|9.99|5
w2|8.50|6' "$(psql_val "select name, price::text, big from e2e.items where name in ('w1','w2') order by name")"
  OUT=$(mcp_call execute '{"sql":"UPDATE e2e.items SET note = $1 WHERE name LIKE $2","params":["updated", "w%"]}')
  assert_json "execute UPDATE → rows_affected 2" 'r["result"]["structuredContent"]["rows_affected"] == 2' "$OUT"
  OUT=$(mcp_call query '{"sql":"UPDATE e2e.items SET note = $1 WHERE name = $2 RETURNING id, note","params":["ret", "w1"]}')
  assert_json "query runs UPDATE … RETURNING when writes are on" 'r["result"]["structuredContent"]["rows"][0][1] == "ret" and r["result"]["structuredContent"]["statement"] == "update"' "$OUT"
  OUT=$(mcp_call execute '{"sql":"INSERT INTO e2e.items (name) VALUES ($1)","params":["w1"]}')
  assert_contains "execute maps 23505 unique_violation with the constraint" '23505 (unique_violation)' "$OUT"
  OUT=$(mcp_call execute '{"sql":"BEGIN"}')
  assert_contains "execute refuses BEGIN even with writes on" 'there are no sessions' "$OUT"
  OUT=$(mcp_call execute '{"sql":"DELETE FROM e2e.orders; DELETE FROM e2e.items"}')
  assert_contains "execute refuses two statements" 'only one statement per call' "$OUT"
  OUT=$(mcp_call execute_batch "{\"sql\":\"INSERT INTO e2e.items (name) VALUES ('b1'); INSERT INTO e2e.items (name) VALUES ('w1');\"}")
  assert_contains "execute_batch with a failing 2nd statement reports the error" '23505' "$OUT"
  assert_contains "execute_batch rolled back the 1st statement (atomic)" '0' "$(psql_val "select count(*) from e2e.items where name = 'b1'")"
  OUT=$(mcp_call execute_batch '{"sql":"CREATE TABLE e2e.batch_t (x int); INSERT INTO e2e.batch_t VALUES (1),(2); DROP TABLE e2e.batch_t;"}')
  assert_json "execute_batch runs a DDL+DML script" 'r["result"]["structuredContent"]["ok"] is True and r["result"]["structuredContent"]["statements_estimated"] == 3' "$OUT"
  OUT=$(mcp_call execute '{"sql":"CREATE TABLE e2e.ddl_t (x int)"}')
  assert_json "execute DDL (CREATE TABLE)" 'r["result"]["structuredContent"]["rows_affected"] == 0' "$OUT"
  OUT=$(mcp_call execute '{"sql":"DROP TABLE e2e.ddl_t"}')
  assert_json "execute DDL (DROP TABLE)" 'r["result"]["structuredContent"]["rows_affected"] == 0' "$OUT"
  OUT=$(mcp_call explain_query '{"sql":"UPDATE e2e.items SET note = note WHERE id = 1","analyze":true}')
  assert_json "explain_query analyze on an UPDATE works with writes on" 'r["result"]["structuredContent"]["plan_summary"]["node_type"] == "ModifyTable"' "$OUT"
  OUT=$(mcp_call query '{"sql":"SELECT id FROM e2e.orders WHERE id = 1 FOR UPDATE"}')
  assert_json "FOR UPDATE is allowed with writes on" 'r["result"]["structuredContent"]["rows"] == [[1]]' "$OUT"
  OUT=$(mcp_call execute '{"sql":"DELETE FROM e2e.items WHERE name LIKE $1","params":["w%"]}')
  assert_json "execute DELETE cleans up → rows_affected 2" 'r["result"]["structuredContent"]["rows_affected"] == 2' "$OUT"
  OUT=$(mcp_call execute_batch '{"sql":""}')
  assert_contains "execute_batch refuses an empty script" 'the script is empty' "$OUT"
  OUT=$(mcp_call execute_batch "$(python3 -c 'import json; print(json.dumps({"sql": "SELECT 1 -- " + "x" * 1100000}))')")
  assert_contains "execute_batch refuses a script over 1 MiB" 'the limit is 1048576 bytes' "$OUT"
fi

# ── missing secret: the bind fails, not a tool call ──────────────────────────
if [ "$E2E_BIND_CHECK" = "1" ]; then
  echo "== missing secret ref (bind failure) =="
  OUT=$(apply_workload false --no-secret)
  assert_contains "apply without the url secret is accepted by the API" '"state"' "$OUT"
  # (no cutover is expected: the new revision cannot bind; the previous one keeps serving)
  FOUND=""
  for _ in $(seq 1 45); do
    LOGS=$(api "http://localhost/v1/logs?workload=default/$WORKLOAD&limit=40")
    case "$LOGS" in
      *"requires a 'url' config"*) FOUND=yes; break ;;
    esac
    sleep 2
  done
  if [ -n "$FOUND" ]; then
    pass "daemon logs report: named wasmcloud:postgres interface requires a 'url' config"
  else
    fail "daemon logs report the missing url config" "$(printf '%s' "$LOGS" | head -c 600)"
  fi
  assert_contains "daemon logs report the plugin bind failure" 'failed to bind workload item to plugin' "$LOGS"
  if deploy false; then pass "workload back to running with the secret ref"; else fail "workload back to running with the secret ref" "$(workload_state)"; fi
  OUT=$(mcp_call server_info '{}')
  assert_json "server_info ok again" 'r["result"]["structuredContent"]["status"] == "ok"' "$OUT"
fi

# The Host-header guard (guard_tests) runs on a wasmtime guard instance in
# other suites; here the Desktop ingress routes by Host, so a foreign Host
# never reaches this component. Skipped by design.
mcp_harness_report
