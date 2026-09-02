---
name: postgres-mcp
description: Query and inspect one PostgreSQL database — run read-only SQL with parameters, discover schemas/tables/columns, describe a table's columns/keys/indexes, EXPLAIN a query, get table sizes and row counts, and (only when the deployment enables writes) run INSERT/UPDATE/DDL or multi-statement scripts. Use when connected to this server to answer questions from a Postgres database, to write SQL against it, or to explain why one of its calls failed.
---

# Using the postgres-mcp MCP server

This server runs as a sandboxed WebAssembly component on Cosmonic Desktop and
talks to **one PostgreSQL database** through the host's `wasmcloud:postgres`
interface. The connection URL lives in the Desktop secret ref
`postgres-mcp-url`; the component never sees it and never dials anything.
It is **stateless**: every call may run on a different pooled connection, so
nothing set in one call exists in the next. Tool schemas are on the wire via
`tools/list`; this playbook is the operating knowledge a schema cannot give
you. References: [tools](references/TOOLS.md) · [value and parameter
types](references/TYPES.md) · [error catalogue](references/ERRORS.md).

## Start here (every session)

1. **`server_info`** — proves connectivity and tells you `writes_allowed`,
   the row limits and the timeout. `status: invalid` / `unreachable` is a
   *deployment* problem: report the `remediation` text (it names the secret
   ref, the config key `url` and the `cosmonic_set_secret` call) and stop —
   no other tool will work, and retrying changes nothing.
2. **Find the data**: `list_schemas` → `list_tables` (one schema, default
   `public`), or `search_objects` when you only know a word from a table or
   column name.
3. **`describe_table` before writing SQL with parameters.** Its `columns`
   carry the exact types and its `param_hints` say which columns need a typed
   parameter or a cast (see below). It also lists keys, foreign keys and
   indexes, which you need for joins and for `ORDER BY` choices.
4. **`explain_query`** before any unfamiliar heavy query (default `analyze:
   false` never executes it), then **`query`**.

## Which tool for what

| Task | Tool | Notes |
|---|---|---|
| Read data, aggregate, join | `query` | one statement; `$1..$n` params; `limit` ≤ 1000 |
| Count rows / table size | `table_stats` | `estimated_rows` is free; `exact_count: true` runs `count(*)` |
| Where is column X / table Y? | `search_objects` | literal substring match across schemas; `glob: true` for `%` patterns |
| Columns, keys, FKs, indexes of a table | `describe_table` | also `enum_values` and `view_definition` |
| Index usage / unused indexes | `list_indexes` | `scans` = index scans since stats reset |
| Query plan | `explain_query` | `analyze: true` executes the statement |
| INSERT/UPDATE/DELETE/DDL | `execute` | **only** when `writes_allowed` — otherwise refused before execution |
| Migration / seed script | `execute_batch` | one transaction unless the script has BEGIN/COMMIT; writes only |

## Read-only mode (the default)

`POSTGRES_ALLOW_WRITES` is `"false"` in the shipped manifest (anything but
`"true"` means read-only), so:

- `query` accepts only `SELECT`, `WITH … SELECT`, `EXPLAIN`, `SHOW`,
  `VALUES`, `TABLE` — one statement, no data-modifying CTE, no
  `SELECT … INTO`, no `FOR UPDATE`/`FOR SHARE`, no `nextval()`/`setval()`/
  `pg_terminate_backend()`-style side effects. The check is a **statement
  inspector in the component** (it lexes past strings, `$$` bodies and
  comments; quoted identifiers are never keywords, but `"nextval"(…)`,
  `pg_catalog."set_config"(…)` and the `U&"…"` escaped spelling are still
  matched against the side-effect function list, exactly as spelled), not a
  database guarantee: a side-effecting user function called from a SELECT
  gets through. A rejection names the keyword. DDL/maintenance words (`CREATE`, `COMMENT`, `LOAD`, `CALL`, …)
  are matched only at a statement head (including the statement `EXPLAIN`
  wraps), so `SELECT note AS comment …` is fine; the data-modifying words
  `INSERT`/`UPDATE`/`DELETE`/`MERGE`/`INTO` are matched anywhere — used as
  unquoted identifiers they are a false positive, fixed by quoting or
  aliasing the identifier. A column spelled exactly like a listed function
  (`"nextval"`) is refused even when quoted — alias it.
- `execute` and `execute_batch` refuse without executing.
- `explain_query` plans read-only statements only; `analyze: true` on a
  write is refused.

Treat a `read-only mode:` error as a **policy** result: do not retry, do not
try to smuggle the write through a CTE. If the user wants writes, the fix is a
redeploy with `POSTGRES_ALLOW_WRITES: "true"` — and, for real safety, a URL
role that is *not* read-only. When writes are on, `query` runs any single
statement (use it for `… RETURNING`), `execute` returns `rows_affected`, and
`execute_batch` runs scripts atomically.

## No sessions

`BEGIN`/`COMMIT`/`ROLLBACK`, `SET`, `SET ROLE`, `PREPARE`, `DECLARE`/`FETCH`,
temp tables, advisory locks and `LISTEN` are refused in every mode because
they cannot outlive the call. Need several statements atomically? Use
`execute_batch` (writes on): it runs on one connection as one implicit
transaction; a failing statement rolls back the earlier ones. `SET LOCAL`
inside such a script is fine.

## Parameters: the thing that bites

Placeholders are `$1..$n` (never `?` or `:name`); `params` is a positional
JSON array with exactly as many entries as the highest `$n`. The host binds
values in Postgres' **binary** format with no type hints, so the JSON type
must match the type Postgres infers for the slot. This server tries several
encodings automatically (bare integer → numeric, int8, int4, int2; bare float
→ numeric, float8; bare string → text, then uuid / timestamp / date / number /
bool if the string looks like one), which covers `WHERE id = $1` with `42`,
`LIMIT $1`, `WHERE created_at > $1` with an ISO string, and `WHERE u = $1`
with a uuid string. Two cases it cannot fix:

- **A bare integer against a `float8`/`float4` column is read silently as a
  garbage float** (same byte length). Pass `3.0`, not `3`, for float columns
  — or `{"type": "float8", "value": 3}`.
- A string that does not parse as the slot's type (e.g. `"nope"` for a uuid)
  fails with `08P01`/`22P03` and the message lists the encodings tried.

The explicit form always works: `{"type": "int4"|"int8"|"numeric"|"float8"|
"text"|"bool"|"uuid"|"date"|"timestamp"|"timestamptz"|"jsonb"|"bytea"
(base64)|"text[]"|…, "value": …}`. A cast in SQL works too (`$1::text::int`,
`$1::text::timestamptz`); note `$1::int` alone does **not** — it tells
Postgres the slot is int4 but the bytes are still whatever the JSON was.
`describe_table` → `param_hints` tells you which form each column needs.

## Result values

Rows are JSON arrays aligned with `columns`. Integers and floats are JSON
numbers (`NaN`/`Infinity` become strings), `bytea` is base64, dates/times are
ISO-8601 strings (`timestamptz` in UTC with `Z`), `json`/`jsonb` are parsed,
arrays are arrays, `hstore` is an object. Cells longer than 32 k characters
are cut with a marker.

**Types the host cannot return — cast them or the whole query fails** with a
`value-conversion` error naming the column: `numeric`/`decimal`/`money`
(`col::text` keeps the exact value, `col::float8` gives a number), enums
(`col::text`), `char(n)` (`col::text`), `oid`/`regclass`/`regproc` (`::int`
or `::text`), `interval` (`col::text` or `EXTRACT(EPOCH FROM col)`),
`timetz`, range/multirange types, `circle`/`line`, `void`
(`pg_sleep(...)::text`), `record` (`ROW(...)::text` or `to_jsonb(row)`),
`tsquery`/`tsvector`. Domains are fine (`information_schema` views work).
Never `SELECT *` on an unfamiliar table; run `describe_table` and cast. This
server's own introspection uses `pg_catalog` with explicit casts for exactly
this reason.

## Limits and truncation

- `query` returns at most `limit` rows (default 100, ceiling 1000; larger
  values are clamped and `limit_applied` says so). `truncated: true` means
  more rows exist or the result hit the byte cap (`truncated_reason` says
  which). Paginate with `ORDER BY` + `OFFSET`/keyset or aggregate in SQL —
  never infer a count from a truncated page; use `table_stats`.
- Results are capped at `POSTGRES_MAX_RESULT_BYTES` (1 MiB); SQL text at
  256 KiB (1 MiB for `execute_batch`); 100 parameters.
- Each host call has a deadline (`POSTGRES_QUERY_TIMEOUT_MS`, 30 s). On
  `timeout` the row stream is dropped, but the statement may keep running on
  the server: put `options=-c statement_timeout=30000` (URL-encoded) in the
  connection URL so Postgres cancels it (then you see `57014`).
- One query at a time per instance: a slow query blocks other calls on that
  instance until it returns or times out.

## Reading errors (short form — full catalogue in [ERRORS.md](references/ERRORS.md))

- `"isError": true` with `read-only mode: …` / `… is refused: there are no
  sessions` / `is disabled` — policy; do not retry.
- `ERROR 42P01 … undefined_table` — wrong schema or case; `list_tables`,
  qualify `schema.table`, quote mixed-case names.
- `ERROR 42703 … undefined_column` — `describe_table` for the real names.
- `ERROR 22P03` / `08P01 insufficient data` — parameter encoding; the
  message lists what was tried; use a typed param or a cast.
- `the host cannot convert a result column …` — cast the named column.
- `connection failed: … password authentication failed` / `Connection
  refused` — the secret URL is wrong or the database is unreachable **from
  the Desktop daemon** (it dials from the host process, so `127.0.0.1` in
  the URL means the developer's machine); follow the remediation, then
  `server_info`.
- `timeout: no result within …` — narrow the query, `explain_query` first.
- JSON-RPC `-32602` — the request itself was malformed; HTTP `403` — the
  DNS-rebinding guard rejected the `Host` header.

## Failure modes that are not tool errors

A missing or mis-registered secret ref (its `env` must be exactly `url`) or an
unparseable URL fails the **workload bind**: the server never starts and
`GET /` is unreachable. That is diagnosed in Cosmonic Desktop (the workload
status says `named wasmcloud:postgres interface requires a 'url' config`),
fixed by registering `postgres-mcp-url` correctly and re-applying the
workload. A wrong password or unreachable host, on the other hand, shows up on
the first call as `connection failed` — which is why `server_info` goes first.
