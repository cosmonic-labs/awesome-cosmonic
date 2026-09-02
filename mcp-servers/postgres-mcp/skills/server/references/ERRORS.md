# Error catalogue

Supporting file of the `postgres-mcp` skill
(`skill://postgres-mcp/references/ERRORS.md`). Every entry: what you see →
what it means → what to do. Database errors are rendered as
`SEVERITY SQLSTATE (condition_name): message — detail. Hint: …`.

## Policy results (do not retry)

| You see | Meaning | Do |
|---|---|---|
| `read-only mode: \`X\` is not allowed …` | `POSTGRES_ALLOW_WRITES` is not `"true"` and the inspector found a write/DDL keyword, a locking clause, `SELECT … INTO`, or a side-effecting function | If the write is intended, ask for a redeploy with writes enabled and use `execute`; if `X` is only an identifier in your SQL, quote it (keywords) or alias it (a column spelled like a side-effect function, e.g. `"nextval"`, is refused even quoted) |
| `\`execute\` is disabled … read-only` / `\`execute_batch\` is disabled` | same gate, before anything ran | same |
| `\`BEGIN\` is refused: there are no sessions …` (also `SET`, `PREPARE`, `DECLARE`, `LISTEN`, …) | statement needs a session; every call may use a different connection | drop it; use `execute_batch` for atomic scripts, `SET LOCAL` inside one |
| `only one statement per call is accepted` | `;`-separated statements in `query`/`execute` | one call per statement, or `execute_batch` (writes on) |
| `\`foo\` is not a SQL statement keyword — a syntax error?` | the first word is not a statement keyword | fix the typo / wrap a parenthesised query in `SELECT * FROM (…) q` |
| `the statement is empty` | only whitespace/comments | send SQL |
| `limit must be between 1 and 1000` / `pattern must be 1..200 characters` / `too many parameters` / `the SQL text is N bytes; the limit is …` | argument out of range | adjust the argument |
| `pass the statement to plan without a leading EXPLAIN` | `explain_query` adds EXPLAIN itself | strip it |
| `invalid table reference …` | not `name`, `schema.name` or `"quoted"."name"` | fix the reference |
| `failed to deserialize parameters: …` | a JSON argument has the wrong type (`limit: "x"`, unknown `kinds` value) | fix the argument |

## Deployment / connectivity

| You see | Meaning | Do |
|---|---|---|
| `server_info` → `status: invalid`, `connection failed: … password authentication failed for user` (`28P01`) or `database "x" does not exist` | the URL in secret ref `postgres-mcp-url` is wrong | follow `remediation`: fix the ref (`cosmonic_set_secret … env=url value=postgres://…`), re-apply the workload, `server_info` again |
| `status: unreachable`, `connection failed: … Connection refused / timed out` | the database is not reachable **from the Desktop daemon** (it dials the URL from the host process — `127.0.0.1` is the developer's machine); container stopped, wrong host/port, firewall | start the database (`podman start mcp-pg` locally), check host:port, `sslmode=disable` for local containers |
| `connection failed: … certificate / UnknownIssuer` | `sslmode=require|verify-*` against a self-signed or private-CA server; the daemon trusts webpki roots only | public certificate, or `sslmode=disable` on a trusted network |
| `connection failed: failed to get connection: Timed out` or `53300 too_many_connections` | the daemon's pool (default 10 per URL) or the server's `max_connections` is exhausted | lower concurrency, `&pool_size=N` in the URL, raise `max_connections` |
| `access denied: the host refused this component's access` | Desktop policy refused the postgres interface | check the workload's `hostInterfaces` (`db`, `db-prepared`) and policy; re-apply |
| `database backend error: no database configured for this component` | the component reached the daemon through an *unnamed* postgres import (legacy path) | rebuild with the labeled imports; `cosmonic_inspect` must list `db` and `db-prepared`; re-promote, re-apply |
| Workload never becomes ready; status says `named wasmcloud:postgres interface requires a 'url' config` | the secret ref is missing, not listed under both postgres `hostInterfaces`, or registered with an `env` other than `url` | register `postgres-mcp-url` with `env: url`, re-apply |
| Status says `failed to parse postgres URL` / `postgres config` | the secret value is not a valid `postgres://` URL (unencoded `@`, `:`, `/`, space in the password) | percent-encode the password, re-register, re-apply |
| `the request ended before the database call completed` | the client disconnected mid-call or the instance was torn down | retry |

## Statement errors (SQLSTATE)

| SQLSTATE | Meaning | Do |
|---|---|---|
| `42P01 undefined_table` | wrong schema (search_path is the role's default, usually `public`), case-sensitive identifier, or typo | `list_tables` / `search_objects`; qualify `schema.table`; quote mixed-case |
| `42703 undefined_column` | column name/case | `describe_table` |
| `42601 syntax_error` (position N) | SQL syntax; `?`/`:name` placeholders are not Postgres | fix; use `$1..$n` |
| `42601 cannot insert multiple commands into a prepared statement` | more than one statement reached the host | one statement per call / `execute_batch` |
| `42501 insufficient_privilege` | the URL role lacks a GRANT | `GRANT USAGE ON SCHEMA …`, `GRANT SELECT ON …`, or a different role |
| `25006 read_only_sql_transaction` | the role/server is read-only at the database level (`default_transaction_read_only=on`, standby) — the recommended production posture | use another role/URL for writes |
| `42P18 indeterminate_datatype` (`could not determine data type of parameter $N`) | `$N` where the type cannot be inferred (`SELECT $1`, `COALESCE($1, …)`) | cast: `$1::text`, `$1::bigint` |
| `42P02 undefined_parameter` | `$N` with no matching `params` entry | send exactly N params |
| `08P01 … expected N parameters but got M` (rendered as `connection failed: expected …`) | params count ≠ placeholders | fix the count |
| `22P03 invalid_binary_representation` (`incorrect binary data format in bind parameter N`) | parameter N's encoding is longer than its slot expects and no automatic re-encoding fit; the message lists the encodings tried | typed param `{"type": …}` or `$N::text::<type>` |
| `08P01 insufficient data left in message` | a parameter's encoding is shorter than its slot (a string where a number/uuid/timestamp is needed) | typed param or cast |
| `22021 invalid byte sequence … 0x00` | a bare number bound where text is expected | pass a string |
| `22023 unsupported jsonb version` | a string bound to a jsonb slot | pass an object, or `{"type": "jsonb", "value": …}` |
| `22P02 invalid_text_representation`, `42804 datatype_mismatch`, `42846 cannot_coerce`, `42883 undefined_function` | a value or cast does not fit; an operator has no such signature (often `text = integer` after a text cast) | fix value/cast; `describe_table` |
| `22003 numeric_value_out_of_range`, `22012 division_by_zero`, `22007`/`22008` datetime | data exception at runtime | fix the expression |
| `23505 unique_violation`, `23503 foreign_key_violation`, `23502 not_null_violation`, `23514 check_violation` | a constraint rejected a write (writes on) | `describe_table` → constraints; `ON CONFLICT`; fix referenced rows |
| `57014 query_canceled` | server-side `statement_timeout` fired | narrow the query or raise the timeout deliberately |
| `40001 serialization_failure`, `40P01 deadlock_detected`, `55P03 lock_not_available` | transient concurrency failure | retry once |
| `3F000 invalid_schema_name` | schema does not exist | `list_schemas` |
| `0A000 feature_not_supported` | the server cannot do that | rethink |

## Result conversion and limits

| You see | Meaning | Do |
|---|---|---|
| `the host cannot convert a result column (\`col\`): column N: error deserializing column N …` | one column's type is unsupported by the host's value conversion (numeric/decimal/money, enum, char(n), oid/regclass, interval, timetz, ranges, circle/line, void, record, tsquery/tsvector); the whole result was discarded | cast the named column (`::text`, `::float8`, `EXTRACT(EPOCH …)`, `::int`); avoid `SELECT *` |
| `result column \`col\` cannot be rendered: a numeric/decimal value could not be decoded …` | a numeric cell arrived in the host's lossy form and could not be recovered | `col::text` or `col::float8` |
| `truncated: true`, `truncated_reason: more than N rows matched` | row cap (`limit`, ≤ 1000) | paginate (`ORDER BY` + `OFFSET`/keyset), aggregate, `table_stats` for counts |
| `truncated: true`, `truncated_reason: result exceeded POSTGRES_MAX_RESULT_BYTES` | byte cap (1 MiB) | fewer/narrower columns, lower limit |
| a cell ending in `…[truncated: 32768 of M chars shown]` | per-cell cap | `left(col, n)` / `length(col)` |
| `timeout: no result within N ms (POSTGRES_QUERY_TIMEOUT_MS)` | the component's deadline; the statement may still run on the server | `explain_query`, add `WHERE`/`LIMIT`, `options=-c statement_timeout=…` in the URL, or raise the setting |
| `the host's time limit expired` | daemon-side timeout | same |
| `the prepared statement was evicted by the host and could not be re-prepared` | `execute`'s token vanished twice in a row | retry once |
| `execute_batch` fails on statement k | the whole script rolled back (single implicit transaction) unless it has its own BEGIN/COMMIT | fix and re-run the full script; verify with `query` |

## Transport

| You see | Meaning | Do |
|---|---|---|
| JSON-RPC `error` `-32602` | malformed request (missing/ill-typed `params`, missing `_meta`, missing `Mcp-Name`) | fix the call; not an outage |
| HTTP `403 Forbidden` before any JSON-RPC body | the DNS-rebinding guard (`MCP_ALLOWED_HOSTS`) rejected the `Host` header | connect via `postgres-mcp.localhost:8200` |
| HTTP `413` | request body over the transport limit (4 MiB) | smaller SQL/params |
