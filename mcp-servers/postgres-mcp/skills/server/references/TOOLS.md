# Tool reference

Supporting file of the `postgres-mcp` skill (`skill://postgres-mcp/references/TOOLS.md`).
Every tool returns `structuredContent` (mirrored as JSON text in `content`);
failures are tool-level errors (`isError: true`) whose text says what happened
and what to do. Table names are matched **as stored** in the catalog: an
unquoted `CREATE TABLE Foo` made `foo`; a quoted `"Foo"` needs `"Foo"`.

| Tool | Arguments | Output | Gated? |
|---|---|---|---|
| `server_info` | — | `{status: ok\|invalid\|unreachable\|denied\|error, server: {version, database, user, version_num, in_recovery, now, statement_timeout, default_transaction_read_only, search_path, timezone, server_encoding, server_addr, server_port, started_at, backends}, config: {writes_allowed, default_row_limit, max_row_limit, query_timeout_ms, max_result_bytes, default_schema, hide_system_schemas, max_cell_chars}, remediation?}` | — |
| `query` | `sql` (≤ 256 KiB, one statement), `params?` (≤ 100, see TYPES.md), `limit?` (1..1000, default 100) | `{columns: [..], rows: [[..]], row_count, truncated, truncated_reason?, limit_applied, statement}` | read-only inspector unless writes enabled |
| `execute` | `sql` (one statement), `params?` | `{rows_affected}` | refused unless `POSTGRES_ALLOW_WRITES=true` |
| `execute_batch` | `sql` (≤ 1 MiB script, no params) | `{ok: true, statements_estimated}` | refused unless `POSTGRES_ALLOW_WRITES=true` |
| `list_schemas` | `include_system?` (default false) | `{count, schemas: [{name, owner, comment, is_system}], include_system}` | — |
| `list_tables` | `schema?` (default `public`), `kinds?` (`table`, `partitioned_table`, `view`, `materialized_view`, `foreign_table`) | `{schema, count, tables: [{name, kind, estimated_rows, total_bytes, total_size, comment}], hint?}` | — |
| `describe_table` | `table` (may be `schema.name` or quoted), `schema?` | `{schema, table, kind, owner, comment, estimated_rows, total_bytes, view_definition, columns: [{position, name, data_type, nullable, default_value, identity, generated, comment, is_enum, is_domain, enum_values}], primary_key: [..], foreign_keys: [{name, columns, referenced_schema, referenced_table, referenced_columns, on_delete, on_update, definition}], unique_constraints, check_constraints, exclusion_constraints, indexes: [{name, definition, is_unique, is_primary, is_valid, method, bytes, size, scans, tuples_read, tuples_fetched}], param_hints: [{column, data_type, bind_as}]}` | — |
| `list_indexes` | `schema?`, `table?` | `{schema, table, count, indexes: [..as above.., schema, table]}` | — |
| `explain_query` | `sql` (no leading EXPLAIN), `params?`, `analyze?` (default false) | `{plan: <EXPLAIN FORMAT JSON>, plan_summary: {node_type, relation, startup_cost, total_cost, plan_rows, plan_width, actual_rows, actual_total_time_ms, planning_time_ms, execution_time_ms, analyzed}}` | `analyze: true` on a non-read-only statement needs writes enabled |
| `table_stats` | `table`, `schema?`, `exact_count?` | `{schema, table, kind, estimated_rows, live_tuples, dead_tuples, seq_scans, index_scans, inserts, updates, deletes, last_vacuum, last_autovacuum, last_analyze, last_autoanalyze, table_bytes, index_bytes, toast_bytes, total_bytes, total_size, exact_row_count?, note?}` | — |
| `search_objects` | `pattern` (1..200 chars), `kinds?` (`table`, `view`, `column`, `function`), `include_system?`, `limit?` (1..500, default 50), `glob?` | `{pattern, kinds, include_system, count, matches: [{kind, schema, name, table, data_type}], truncated}` | — |

## Notes per tool

**`server_info`** — the credential check. `status` other than `ok` carries
`error` (`{error, sqlstate, message}`) and `remediation`. `server_addr` is
null over a Unix socket. `backends` is the number of sessions on the server,
including the daemon's pool.

**`query`** — `statement` echoes the leading keyword. Fetches `limit + 1`
rows to set `truncated` exactly, then drops the stream. Cells are rendered per
[TYPES.md](TYPES.md); a column of a type the host cannot convert fails the
whole call (cast it). With writes enabled any single statement runs and
`RETURNING` rows come back; without `RETURNING` a write via `query` returns
zero rows and no count — use `execute` for `rows_affected`.

**`execute`** — prepares once per instance (the token is cached per SQL text)
and executes; the host evicting the token is handled by re-preparing. Same
parameter encoding and retries as `query`. Constraint violations arrive as
`23505`/`23503`/`23502`/`23514` with the constraint name and detail.

**`execute_batch`** — no parameters (embed literals; the script is trusted
input, not user data). Runs through the simple-query protocol on one
connection: one implicit transaction unless the script contains its own
`BEGIN`/`COMMIT`. `statements_estimated` is a `;` count, not a parse.

**`list_schemas`** — hides `pg_*` and `information_schema` unless
`include_system` (or the deployment sets `POSTGRES_HIDE_SYSTEM_SCHEMAS=false`).

**`list_tables`** — `estimated_rows` is `pg_class.reltuples` (`-1` = never
analyzed). An empty list carries a `hint` (the schema may not exist or may be
invisible to the role).

**`describe_table`** — four catalog queries (`pg_attribute`, `pg_constraint`,
`pg_index`, `pg_class`). `data_type` is `format_type()` output (e.g.
`character varying(40)`, `numeric(12,2)`, `e2e.kind` for an enum, `text[]`).
`param_hints` lists only the columns whose type a bare JSON value does not
bind cleanly, with the exact `{"type": …}` form or cast to use.

**`list_indexes`** — `scans`/`tuples_read`/`tuples_fetched` come from
`pg_stat_user_indexes` (cumulative since the last stats reset); an index with
`scans: 0` on a busy table is a candidate for removal.

**`explain_query`** — `plan` is the parsed `EXPLAIN (FORMAT JSON)` array
(`[{Plan: {…}, Planning Time?, Execution Time?}]`). With `analyze: true` the
statement **runs** (`ANALYZE, BUFFERS`) and `actual_*`/`execution_time_ms`
are filled. Parameters are bound, so plans reflect the given values.

**`table_stats`** — from `pg_class` + `pg_stat_user_tables`; views and foreign
tables have none (error). `toast_bytes` is total − heap − indexes.

**`search_objects`** — one `UNION` over `pg_class`, `pg_attribute`, `pg_proc`
with `ILIKE '%pattern%'`; `%`, `_`, `\` in the pattern are escaped unless
`glob: true`. Functions include procedures and aggregates; `data_type` holds
the return type for functions and the column type for columns. Fetches
`limit + 1` to set `truncated`.

## Configuration knobs the tools reflect

| Env (named config) | Default | Effect |
|---|---|---|
| `POSTGRES_ALLOW_WRITES` | `false` | `"true"` enables `execute`/`execute_batch` and lifts the read-only inspector |
| `POSTGRES_DEFAULT_ROW_LIMIT` | `100` | `query` rows when `limit` is omitted |
| `POSTGRES_MAX_ROW_LIMIT` | `1000` | ceiling for `limit` (clamped) |
| `POSTGRES_QUERY_TIMEOUT_MS` | `30000` | component-side deadline per host call (1 s..10 min) |
| `POSTGRES_MAX_RESULT_BYTES` | `1048576` | serialized-result cap per call |
| `POSTGRES_DEFAULT_SCHEMA` | `public` | default `schema` for the table tools |
| `POSTGRES_HIDE_SYSTEM_SCHEMAS` | `true` | hide `pg_*`/`information_schema` in `list_schemas`/`search_objects` |
