# postgres-mcp

A PostgreSQL MCP server for [Cosmonic Desktop](https://cosmonic.com/docs/desktop),
written in Rust and running as a sandboxed WebAssembly component. It exposes a
curated, **read-only-by-default** tool surface — parameterized `query`,
schema/table/index introspection, `EXPLAIN` as JSON, table statistics, object
search — plus `execute` / `execute_batch` that only work when the deployment
opts into writes. The database is reached through Desktop's **native
`wasmcloud:postgres@0.2.0` host interface**: the daemon owns the connection
pool and dials the database; the component never sees the URL, needs no
outbound `allowedHosts`, no loopback grant and no volumes.

Built from [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28 (stateless streamable HTTP), exports
`wasi:http/handler@0.3.0`, serves a discovery document on `GET /` and
`GET /health`, and publishes its playbook as a skill at
`skill://postgres-mcp/SKILL.md`.

Reachable on Cosmonic Desktop at <http://postgres-mcp.localhost:8200/>.

## Tools

| Tool | Parameters | Output | Gated? |
|---|---|---|---|
| `server_info` | — | `status` ok/invalid/unreachable, server (version, database, user, recovery, statement_timeout, default_transaction_read_only, search_path, …), effective config (`writes_allowed`, limits, timeout), `remediation` on failure | — |
| `query` | `sql` (one statement, ≤ 256 KiB), `params?` (≤ 100, JSON scalars or `{"type", "value"}`), `limit?` (1..1000, default 100) | `{columns, rows, row_count, truncated, truncated_reason?, limit_applied, statement}` | read-only inspector unless `POSTGRES_ALLOW_WRITES=true` |
| `execute` | `sql`, `params?` | `{rows_affected}` | refused unless `POSTGRES_ALLOW_WRITES=true` |
| `execute_batch` | `sql` (script, ≤ 1 MiB, no params) | `{ok, statements_estimated}` — one implicit transaction | refused unless `POSTGRES_ALLOW_WRITES=true` |
| `list_schemas` | `include_system?` | schemas {name, owner, comment, is_system} | — |
| `list_tables` | `schema?` (default `public`), `kinds?` | tables {name, kind, estimated_rows, total_bytes, total_size, comment} | — |
| `describe_table` | `table` (may be `schema.name` / quoted), `schema?` | columns (type, nullable, default, identity, enum values, comment), primary key, foreign keys, unique/check constraints, indexes, `view_definition`, `param_hints` | — |
| `list_indexes` | `schema?`, `table?` | indexes {definition, is_unique, is_primary, method, bytes, scans, tuples_read, …} | — |
| `explain_query` | `sql` (no leading EXPLAIN), `params?`, `analyze?` | `{plan: <EXPLAIN FORMAT JSON>, plan_summary}` | `analyze: true` on a write needs writes enabled |
| `table_stats` | `table`, `schema?`, `exact_count?` | estimated rows, live/dead tuples, scans, inserts/updates/deletes, last (auto)vacuum/analyze, table/index/toast/total bytes, `exact_row_count?` | — |
| `search_objects` | `pattern` (1..200), `kinds?` (table/view/column/function), `include_system?`, `limit?` (≤ 500), `glob?` | matches {kind, schema, name, table, data_type}, `truncated` | — |

Skill: `skill://postgres-mcp/SKILL.md` (catalog at `skill://index.json`;
references `references/TOOLS.md`, `references/TYPES.md`,
`references/ERRORS.md`). The skill carries the sequencing (`server_info` →
discovery → `describe_table` → `explain_query` → `query`), the read-only
semantics, the parameter-typing rules, the list of column types the host
cannot return, the limits and the full error catalogue.

## How it talks to Postgres (no network from the component)

`wit/world.wit` imports the host interface under **labels**:

```wit
world postgres-mcp {
  import wasmcloud:postgres/types@0.2.0;
  import db: wasmcloud:postgres/query@0.2.0;
  import db-prepared: wasmcloud:postgres/prepared@0.2.0;
}
```

A labeled import is a component-model `(implements ..)` import; Cosmonic
Desktop routes each to the workload's `hostInterfaces` entry with the same
`name`. `deploy/workload.yaml` therefore declares two named
`wasmcloud:postgres@0.2.0` interfaces — `db` (`types`, `query`) and
`db-prepared` (`prepared`; WIT allows one interface per label) — and each
gets its connection URL from the secret ref `postgres-mcp-url` through
`secretFrom`. The daemon resolves the ref into the interface's config key
`url` at bind time (so the ref **must** be registered with `env: url`),
builds one pool per URL, and serves both labels from it. The component's
`allowedHosts` is `[]`: nothing dials out.

`wasm-tools component wit` on the built component shows the labeled imports,
and `cosmonic_inspect` on the promoted image lists them by label:

```
$ wasm-tools component wit target/wasm32-wasip2/release/postgres_mcp.wasm | grep -E 'postgres|handler'
  import wasmcloud:postgres/types@0.2.0;
  import db: wasmcloud:postgres/query@0.2.0;
  import db-prepared: wasmcloud:postgres/prepared@0.2.0;
  export wasi:http/handler@0.3.0;

$ cosmonic_inspect image=oci.localhost:8200/apps/postgres-mcp:0.1.0@sha256:…
  "imports": [ …, "wasmcloud:postgres/db", "wasmcloud:postgres/db-prepared", "wasmcloud:postgres/types" ],
  "exports": [ …, "wasi:http/handler", … ],
  "producer": "wit-component 0.257.0"
```

Inside the component, host calls are WASI p3 futures and must never be
awaited from tool (tokio) code. `src/bridge.rs` extends the template's
tokio ↔ component-model bridge with a second job kind, `bridge::host_call`:
tool code hands the driver a closure, the driver runs the returned future
(the query, consuming its `stream<row>`, awaiting its completion future) in
component-model context under a `wasi:clocks` deadline, and the typed result
comes back over a oneshot. The template's outbound-HTTP job is unchanged.

## Configuration

| Env | Kind | Default | Required |
|---|---|---|---|
| `url` | secret ref `postgres-mcp-url` — **not** a component env var: the config key of the `db` / `db-prepared` host interfaces | — | yes (the workload does not bind without it) |
| `POSTGRES_ALLOW_WRITES` | named config | `false` | no — `"true"` enables `execute`/`execute_batch` and lifts the read-only inspector on `query`/`explain_query` |
| `POSTGRES_DEFAULT_ROW_LIMIT` | named config | `100` | no — rows `query` returns when `limit` is omitted |
| `POSTGRES_MAX_ROW_LIMIT` | named config | `1000` | no — ceiling for `limit` (clamped; the result carries `limit_applied`) |
| `POSTGRES_QUERY_TIMEOUT_MS` | named config | `30000` | no — component-side deadline per host call (1000..600000); on expiry the row stream is dropped and `timeout` is returned; pair with `statement_timeout` in the URL |
| `POSTGRES_MAX_RESULT_BYTES` | named config | `1048576` | no — cap on one call's serialized result; rows are cut with `truncated=true` |
| `POSTGRES_DEFAULT_SCHEMA` | named config | `public` | no — schema for the table tools when `schema` is omitted |
| `POSTGRES_HIDE_SYSTEM_SCHEMAS` | named config | `true` | no — hide `pg_*`/`information_schema` in `list_schemas`/`search_objects` |
| `MCP_ALLOWED_HOSTS` | named config | `postgres-mcp.localhost` | DNS-rebinding guard; must match the ingress host |
| `RUST_LOG` | named config | `info` | log filter |

### The secret: a libpq URL, registered with `env: url`

```console
$ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock         # Linux
$ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs \
    -H 'Content-Type: application/json' \
    -d '{"name":"postgres-mcp-url","uri":"keychain://cosmonic/postgres-mcp-url","env":"url","value":"postgres://mcp:mcp@127.0.0.1:5432/mcp?sslmode=disable"}'
```

or `cosmonic_set_secret name=postgres-mcp-url uri=keychain://cosmonic/postgres-mcp-url env=url value=<url>`,
or paste it in Desktop → Secrets. The value above is the placeholder for a
developer machine running the `mcp-pg` podman container
(`podman run -d --name mcp-pg -p 127.0.0.1:5432:5432 -e POSTGRES_USER=mcp -e POSTGRES_PASSWORD=mcp -e POSTGRES_DB=mcp docker.io/library/postgres:16-alpine`)
— replace it with your database's URL. Notes:

- **`127.0.0.1` is correct for a database on this machine**: the daemon
  dials from the host process (unlike outbound-HTTP servers, which need
  `host.wasmcloud.internal`).
- Percent-encode special characters in the password (`@` → `%40`, `:` →
  `%3A`, `/` → `%2F`, space → `%20`).
- `sslmode=require|verify-ca|verify-full` makes the daemon use TLS with
  webpki (public CA) roots; self-signed or private-CA servers fail — use
  `sslmode=disable` on a trusted network.
- Optional: `&pool_size=10` (daemon pool per URL, default 10) and
  `&options=-c%20statement_timeout%3D30000` so Postgres cancels runaway
  statements server-side.
- **Recommended production posture**: a dedicated read-only role, so that
  read-only mode is a database guarantee and not just the component's
  statement inspector:
  ```sql
  CREATE ROLE mcp_ro LOGIN PASSWORD '…';
  GRANT USAGE ON SCHEMA public TO mcp_ro;
  GRANT SELECT ON ALL TABLES IN SCHEMA public TO mcp_ro;
  ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO mcp_ro;
  ALTER ROLE mcp_ro SET default_transaction_read_only = on;
  ```
- The env of the ref **must** be `url`: any other name fails the workload
  bind with `named wasmcloud:postgres interface requires a 'url' config`.
  Changing the value requires re-applying the workload (resolved at bind).

A missing or mis-registered ref fails the **bind** (the workload never becomes
ready — check its status/logs in Desktop), not a tool call. A wrong password
or unreachable host shows up on the first call as a `connection failed` tool
error carrying the same remediation. `GET /` reports the ref in its
`credentials` block (presence only) and names `server_info` as the check.

## Grants

None. `allowedHosts: []` (nothing dials out), no `allowedHostLoopbackPorts`,
no host-level loopback door, no `spec.volumes`. The only host capability is
the two named `wasmcloud:postgres@0.2.0` interfaces carrying the
`postgres-mcp-url` secret; `.wash/config.yaml` mirrors them.

## Build and test

Toolchain: Rust 1.90+ with `wasm32-wasip2`, `wasm-tools`. The labeled imports
need `wasm-component-ld` **0.5.30+** (the copy rustup bundles, 0.5.22, cannot
encode `(implements ..)` imports — "invalid leading byte (0x2) for import
name"), which the build takes from the project-local `.tools/`:

```console
$ cargo install wasm-component-ld --version 0.5.30 --root .tools   # once
$ cargo fmt --check && cargo clippy --all-features -- -D warnings
$ cargo build --release
$ wasm-tools component wit target/wasm32-wasip2/release/postgres_mcp.wasm | grep -E 'import db|export wasi:http/handler@0.3.0'
```

The e2e suite is **Desktop-only** — the upstream is a host capability that
`wasmtime serve` cannot provide, so there is no hermetic HTTP fixture. It
builds, promotes and applies the workload on the local Desktop (idempotently),
seeds a schema in the `mcp-pg` podman container and runs about 250 cases
through the ingress (protocol/spec/discovery/skills framework checks, every
tool's happy path, value rendering for every supported type, parameter typing
and its automatic re-encoding, limits and truncation, the read-only
inspector's refusals, error mapping, the timeout, the missing-secret bind
failure; `E2E_ALLOW_WRITES=1` adds the `execute`/`execute_batch`/`RETURNING`
cases):

```console
$ scripts/e2e.sh                     # read-only deployment (the default)
$ E2E_ALLOW_WRITES=1 scripts/e2e.sh  # also the write cases; restores read-only at the end
$ E2E_KEEP=1 …                       # keep the e2e schema
$ E2E_BIND_CHECK=0 …                 # skip the missing-secret re-apply
```

Without the daemon socket or the container the suite prints `SKIP` and exits
0 (`E2E_START_PG=1` lets it create the container). `cargo test` is not used
(wasm target).

## Deploy on Cosmonic Desktop

Manifest: [`deploy/workload.yaml`](deploy/workload.yaml) (the only one; the
dev-loop/promote draft comes from `.wash/config.yaml`). Register the secret
ref first (above), then:

```console
$ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock
$ cargo build --release
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects \
    -H 'Content-Type: application/json' -d "{\"path\":\"$PWD\"}"
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects/postgres-mcp/promote \
    -H 'Content-Type: application/json' -d '{"ref":"postgres-mcp:0.1.0","rebuild":true}'
# → {"image":"oci.localhost:8200/apps/postgres-mcp:0.1.0@sha256:…", …}
$ python3 -c 'import yaml,json,sys; d=yaml.safe_load(open("deploy/workload.yaml")); d["spec"]["components"][0]["image"]=sys.argv[1]; print(json.dumps(d))' "$IMAGE" \
    | curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/workloads -H 'Content-Type: application/json' --data-binary @-
```

(or `cosmonic_apply_workload` with the pinned image). Then:

```console
$ curl -s http://postgres-mcp.localhost:8200/ | jq .status
"ok"
$ curl -s -X POST http://postgres-mcp.localhost:8200/ -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/list' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
$ curl -s -X POST http://postgres-mcp.localhost:8200/ -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: query' \
    -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"query","arguments":{"sql":"SELECT id, name FROM public.users WHERE id = $1","params":[42]},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
data: {"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","content":[…],"structuredContent":{"columns":["id","name"],"rows":[[42,"…"]],"row_count":1,"truncated":false,"limit_applied":100,"statement":"select"},"isError":false}}
```

To enable writes, set `POSTGRES_ALLOW_WRITES: "true"` in the manifest and
re-apply. Never reuse the `postgres-mcp` name for another workload.

### Connect a client

```console
$ claude mcp add --transport http postgres-mcp http://postgres-mcp.localhost:8200/
```

Claude Desktop (`claude_desktop_config.json`):
`{"mcpServers":{"postgres-mcp":{"type":"http","url":"http://postgres-mcp.localhost:8200/"}}}`.
Cosmonic Desktop also detects the `mcp.ai/*` labels and can register the
server into detected coding agents from its UI.

## Known limitations

- **Result types the host cannot convert fail the whole query** — cast them
  in SQL. In particular `numeric`/`decimal`/`money` columns come back through
  a lossy path in the daemon's value conversion (the binary bytes read as
  text); the server decodes the few values that survive and refuses the rest
  rather than return garbage: use `col::text` (exact) or `col::float8`. Also
  enums, `char(n)`, `oid`/`regclass`, `interval`, `timetz`, ranges,
  `circle`/`line`, `void`, `record`, `tsquery`/`tsvector` (domains are fine).
  `describe_table` shows column types; never `SELECT *` on an unfamiliar
  table.
- **Parameters are bound in binary with no type hints.** Bare JSON values
  are re-encoded automatically when Postgres rejects them (see
  `references/TYPES.md`), but a bare integer against a `float8`/`float4`
  column and a bare float against an `int8`/`int4` column are read silently
  as garbage (same byte length) — pass floats as floats / integers as
  integers, or a typed param (`{"type": "float8", "value": 3}`).
- Read-only mode is a **statement inspector** (keyword allow-list after
  lexing past strings/comments; quoted identifiers, `U&"…"` included, are
  matched against the built-in side-effect function list but never against
  keywords), not a database guarantee: a side-effecting user function called
  from a `SELECT` gets through. Use a read-only role for real safety.
- No sessions: `BEGIN`/`SET`/temp tables/cursors/advisory locks are refused.
  `execute_batch` is the atomic multi-statement path (writes on).
- One query at a time per instance (`poolSize: 1`); a slow query blocks the
  instance until the deadline. Dropping the row stream on timeout stops the
  daemon's fetch but does not cancel the statement server-side — set
  `statement_timeout` via the URL `options`.
- `execute` returns `rows_affected` but no `RETURNING` rows (use `query`
  with writes on); `execute_batch` takes no parameters.
- The missing-secret failure is a workload bind failure (visible in Desktop
  logs/status), not a tool error — by construction of the host interface.
- The e2e suite needs Cosmonic Desktop and the podman database; there is no
  wasmtime path.

## Borrowed from

- [modelcontextprotocol/servers-archived — src/postgres](https://github.com/modelcontextprotocol/servers-archived/tree/main/src/postgres)
  (MIT): the `query` tool name and the read-only-by-default posture (their
  `BEGIN READ ONLY` cannot work over a pooled host interface, so it became
  the statement inspector).
- [crystaldba/postgres-mcp](https://github.com/crystaldba/postgres-mcp)
  (MIT): the tool surface (`list_schemas`, `list_objects` → `list_tables`,
  `get_object_details` → `describe_table`, `execute_sql`, `explain_query`
  with `FORMAT JSON`) and the restricted-mode rules (reject transaction
  control, `EXPLAIN ANALYZE` of writes, `FOR UPDATE`/`SHARE`, per-statement
  validation). No code.
- [bytebase/dbhub](https://github.com/bytebase/dbhub) (MIT): the guardrail
  design (default row cap + query timeout, token-cheap surface) and the
  `search_objects` idea. No code.
- [googleapis/genai-toolbox](https://github.com/googleapis/genai-toolbox)
  (Apache-2.0): naming cross-check (`list_tables`, `execute_sql`).
- [wasmCloud wash-runtime](https://github.com/wasmCloud/wasmCloud/tree/main/crates/wash-runtime)
  (Apache-2.0): `wit/deps/wasmcloud-postgres-0.2.0/package.wit` verbatim, the
  guest-side stream/completion recipe from `tests/fixtures/postgres-stream-p3`,
  and the `hashable-f64`/timestamp encodings from
  `src/plugin/wasmcloud_postgres/conversions.rs`.
- [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs)
  (Apache-2.0): everything else.

This port is Apache-2.0 (see `LICENSE`).
