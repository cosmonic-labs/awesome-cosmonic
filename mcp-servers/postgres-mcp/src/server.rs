//! The MCP server implementation: the tool surface over
//! [`crate::postgres`].
//!
//! Tools are thin: validate and clamp arguments, refuse what the read-only
//! inspector ([`crate::inspect`]) rejects, run one or a few statements
//! through the client, and render rows as JSON ([`crate::pgvalue`]). Every
//! database failure comes back as a tool-level error (`isError: true`) whose
//! text says what happened and what to do; `ErrorData` (JSON-RPC errors) is
//! reserved for requests the server cannot even route.
//!
//! Alongside the tools, this server publishes **skills** — natural-language
//! playbooks served over the MCP resources primitive under `skill://` URIs.
//! See [`crate::skills`]; the handlers at the bottom of this file are the
//! protocol surface for them.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ListResourceTemplatesResult, ListResourcesResult,
    PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult,
    ResourceContents, ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::inspect::{self, Refusal};
use crate::pgvalue::{self, Param};
use crate::postgres::{self, Config, PgError, RawResult};
use crate::{catalog, skills};

/// Longest SQL text accepted by `query` / `execute` / `explain_query`.
const MAX_SQL_BYTES: usize = 256 * 1024;
/// Longest script accepted by `execute_batch`.
const MAX_BATCH_BYTES: usize = 1024 * 1024;
/// Most parameters per statement.
const MAX_PARAMS: usize = 100;
/// `search_objects` limit ceiling / default.
const SEARCH_MAX_LIMIT: usize = 500;
const SEARCH_DEFAULT_LIMIT: usize = 50;
/// Longest `search_objects` pattern.
const MAX_PATTERN_CHARS: usize = 200;

/// The MCP server for this component. One instance is created per request —
/// the transport is stateless (2026-07-28 spec), so do not keep per-session
/// state on this struct. The prepared-statement cache lives in a static (see
/// [`crate::postgres`]) because that is per *instance*, not per session.
#[derive(Clone)]
pub struct PostgresServer {
    tool_router: ToolRouter<Self>,
    config: Config,
}

// ── parameters ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct QueryParams {
    /// One SQL statement. Placeholders are `$1..$n`. Read-only unless the
    /// workload runs with POSTGRES_ALLOW_WRITES=true: only SELECT / WITH … SELECT /
    /// EXPLAIN / SHOW / VALUES / TABLE, no data-modifying CTEs, no FOR
    /// UPDATE/SHARE, no transaction control, exactly one statement.
    pub sql: String,
    /// Positional parameters for `$1..$n` (at most 100). Bare JSON values map to
    /// text / int8 / float8 / bool / null; use `{"type": "int4"|"uuid"|"timestamptz"|
    /// "numeric"|"date"|"jsonb"|"bytea"|"text[]"|…, "value": …}` to bind other
    /// column types (see skill://postgres-mcp/references/TYPES.md).
    #[serde(default)]
    pub params: Option<Vec<Value>>,
    /// Maximum rows to return (default POSTGRES_DEFAULT_ROW_LIMIT = 100, clamped
    /// to POSTGRES_MAX_ROW_LIMIT = 1000). The result's `truncated` flag says
    /// whether more rows existed.
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExecuteParams {
    /// One writing or DDL statement (INSERT/UPDATE/DELETE/CREATE/ALTER/…) with
    /// `$1..$n` placeholders. Refused unless POSTGRES_ALLOW_WRITES=true. For
    /// RETURNING rows use `query` (writes on) instead.
    pub sql: String,
    /// Positional parameters, same encoding as `query`.
    #[serde(default)]
    pub params: Option<Vec<Value>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExecuteBatchParams {
    /// A multi-statement SQL script (migrations, seed data), no parameters. Runs
    /// on one connection as a single implicit transaction unless it contains its
    /// own BEGIN/COMMIT — a failing statement rolls back the earlier ones. Refused
    /// unless POSTGRES_ALLOW_WRITES=true.
    pub sql: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListSchemasParams {
    /// Include pg_catalog, pg_toast, pg_temp_*, information_schema (default false).
    #[serde(default)]
    pub include_system: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TableKind {
    Table,
    PartitionedTable,
    View,
    MaterializedView,
    ForeignTable,
}

impl TableKind {
    fn as_str(self) -> &'static str {
        match self {
            TableKind::Table => "table",
            TableKind::PartitionedTable => "partitioned_table",
            TableKind::View => "view",
            TableKind::MaterializedView => "materialized_view",
            TableKind::ForeignTable => "foreign_table",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListTablesParams {
    /// Schema to list (default POSTGRES_DEFAULT_SCHEMA = public).
    #[serde(default)]
    pub schema: Option<String>,
    /// Only these kinds: table, partitioned_table, view, materialized_view,
    /// foreign_table (default all).
    #[serde(default)]
    pub kinds: Option<Vec<TableKind>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DescribeTableParams {
    /// Table or view name; may be schema-qualified (`sales.orders`, `"My"."T"`).
    pub table: String,
    /// Schema when `table` is not qualified (default POSTGRES_DEFAULT_SCHEMA).
    #[serde(default)]
    pub schema: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListIndexesParams {
    /// Schema (default POSTGRES_DEFAULT_SCHEMA).
    #[serde(default)]
    pub schema: Option<String>,
    /// Restrict to one table (may be schema-qualified); omit for the whole schema.
    #[serde(default)]
    pub table: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExplainParams {
    /// The statement to plan (without a leading EXPLAIN). Same rules as `query`.
    pub sql: String,
    /// Positional parameters, same encoding as `query`.
    #[serde(default)]
    pub params: Option<Vec<Value>>,
    /// EXPLAIN (ANALYZE, BUFFERS): actually executes the statement. Allowed for
    /// read-only statements, or for anything when POSTGRES_ALLOW_WRITES=true.
    #[serde(default)]
    pub analyze: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TableStatsParams {
    /// Table (may be schema-qualified).
    pub table: String,
    /// Schema when `table` is not qualified (default POSTGRES_DEFAULT_SCHEMA).
    #[serde(default)]
    pub schema: Option<String>,
    /// Also run SELECT count(*) for an exact row count (can be slow on big tables).
    #[serde(default)]
    pub exact_count: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    Table,
    View,
    Column,
    Function,
}

impl ObjectKind {
    fn as_str(self) -> &'static str {
        match self {
            ObjectKind::Table => "table",
            ObjectKind::View => "view",
            ObjectKind::Column => "column",
            ObjectKind::Function => "function",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchObjectsParams {
    /// Case-insensitive substring to look for in table/view/column/function
    /// names (1..200 chars). Matched literally: `%`, `_` and `\` are escaped
    /// unless `glob` is true.
    pub pattern: String,
    /// Kinds to search: table (incl. partitioned/foreign), view (incl.
    /// materialized), column, function (default all).
    #[serde(default)]
    pub kinds: Option<Vec<ObjectKind>>,
    /// Include system schemas (default false).
    #[serde(default)]
    pub include_system: Option<bool>,
    /// Maximum matches (default 50, max 500).
    #[serde(default)]
    pub limit: Option<i64>,
    /// Treat `pattern` as a raw ILIKE pattern (`%`/`_` wildcards) instead of a
    /// literal substring.
    #[serde(default)]
    pub glob: Option<bool>,
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

fn db_error(err: &PgError) -> CallToolResult {
    tool_error(err.render())
}

/// Converts the JSON `params` argument into host parameters.
fn bind_params(params: Option<Vec<Value>>) -> Result<Vec<Param>, String> {
    let params = params.unwrap_or_default();
    if params.len() > MAX_PARAMS {
        return Err(format!(
            "too many parameters: {} (max {MAX_PARAMS})",
            params.len()
        ));
    }
    params
        .iter()
        .enumerate()
        .map(|(i, v)| pgvalue::param_from_json(i + 1, v))
        .collect()
}

fn check_sql_size(sql: &str, max: usize) -> Result<(), String> {
    if sql.len() > max {
        return Err(format!(
            "the SQL text is {} bytes; the limit is {max} bytes",
            sql.len()
        ));
    }
    Ok(())
}

/// Renders rows as JSON arrays, stopping when the serialized size passes
/// `max_bytes`. Returns `(rows, cut_by_bytes)`, or the error for a cell the
/// host mis-decoded (the whole result is refused rather than corrupted).
fn render_rows(raw: &RawResult, max_bytes: usize) -> Result<(Vec<Value>, bool), PgError> {
    let mut out = Vec::with_capacity(raw.rows.len());
    let mut bytes = 0usize;
    for row in &raw.rows {
        let mut rendered = Vec::with_capacity(row.len());
        for (i, cell) in row.iter().enumerate() {
            rendered.push(
                pgvalue::to_json(cell).map_err(|e| {
                    PgError::cell(raw.columns.get(i).map(String::as_str), &e.reason)
                })?,
            );
        }
        // Cheap size estimate: the serialized row.
        bytes = bytes.saturating_add(
            serde_json::to_string(&rendered)
                .map(|s| s.len())
                .unwrap_or(0),
        );
        if bytes > max_bytes && !out.is_empty() {
            return Ok((out, true));
        }
        out.push(Value::Array(rendered));
        if bytes > max_bytes {
            return Ok((out, true));
        }
    }
    Ok((out, false))
}

/// Renders rows as objects keyed by column name (introspection tools).
fn rows_to_objects(raw: &RawResult) -> Result<Vec<Value>, PgError> {
    raw.rows
        .iter()
        .map(|row| {
            let mut map = Map::with_capacity(raw.columns.len());
            for (i, value) in row.iter().enumerate() {
                let key = raw
                    .columns
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| format!("column_{i}"));
                let cell =
                    pgvalue::to_json(value).map_err(|e| PgError::cell(Some(&key), &e.reason))?;
                map.insert(key, cell);
            }
            Ok(Value::Object(map))
        })
        .collect()
}

/// The first cell of the first row, rendered.
fn first_cell(raw: &RawResult) -> Result<Value, PgError> {
    match raw.rows.first().and_then(|r| r.first()) {
        Some(cell) => pgvalue::to_json(cell)
            .map_err(|e| PgError::cell(raw.columns.first().map(String::as_str), &e.reason)),
        None => Ok(Value::Null),
    }
}

fn text_param(s: &str) -> Param {
    Param::fixed(
        postgres::PgValue::Text(s.to_owned()),
        Value::String(s.to_owned()),
    )
}

fn bool_param(b: bool) -> Param {
    Param::fixed(postgres::PgValue::Bool(b), Value::Bool(b))
}

fn null_param() -> Param {
    Param::fixed(postgres::PgValue::Null, Value::Null)
}

fn int4_param(n: i32) -> Param {
    Param::fixed(postgres::PgValue::Int4(n), json!(n))
}

fn text_array_param(items: Vec<String>) -> Param {
    Param::fixed(postgres::PgValue::TextArray(items.clone()), json!(items))
}

/// Resolves `(schema, table)` from a possibly qualified table reference plus
/// the optional schema argument and the configured default.
fn resolve_table(
    config: &Config,
    table: &str,
    schema: Option<&str>,
) -> Result<(String, String), String> {
    let (qualified_schema, name) = inspect::split_table_ref(table).ok_or_else(|| {
        format!("invalid table reference {table:?}: use name, schema.name or \"quoted\".\"name\"")
    })?;
    let schema = qualified_schema
        .or_else(|| {
            schema
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| config.default_schema.clone());
    Ok((schema, name))
}

fn refusal_message(refusal: &Refusal, config: &Config) -> String {
    refusal.message(config.allow_writes)
}

fn write_gate(config: &Config, tool: &str) -> Option<CallToolResult> {
    if config.allow_writes {
        return None;
    }
    Some(tool_error(format!(
        "`{tool}` is disabled: this deployment runs read-only (POSTGRES_ALLOW_WRITES is not \
         \"true\"). Only read statements are executed. To enable writes, set \
         POSTGRES_ALLOW_WRITES: \"true\" in deploy/workload.yaml (components[0].localResources.\
         environment.config) and re-apply the workload — and make sure the postgres-mcp-url role \
         actually has write privileges. Nothing was executed."
    )))
}

#[tool_router]
impl PostgresServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            config: Config::from_env(),
        }
    }

    /// Names of the tools this server exposes, read off the generated router
    /// so the discovery document (see [`crate::discovery`]) cannot drift from
    /// what `tools/list` actually returns.
    pub fn tool_names() -> Vec<String> {
        Self::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect()
    }

    /// Connectivity probe + capability summary. Call first.
    #[tool(
        description = "Check database connectivity and report the server (PostgreSQL version, \
                       current database/user, recovery state, statement_timeout, \
                       default_transaction_read_only, search_path) plus this MCP server's \
                       effective settings (writes_allowed, row limits, timeout). Call this first; \
                       it is also the credential check (status ok|invalid|unreachable with a \
                       remediation)."
    )]
    #[tracing::instrument(name = "tool.server_info", skip(self))]
    async fn server_info(&self) -> Result<CallToolResult, ErrorData> {
        match postgres::query(&self.config, catalog::SERVER_INFO.to_owned(), Vec::new(), 1).await {
            Ok(raw) => {
                let server = match rows_to_objects(&raw) {
                    Ok(rows) => rows.into_iter().next().unwrap_or(Value::Null),
                    Err(err) => return Ok(db_error(&err)),
                };
                Ok(CallToolResult::structured(json!({
                    "status": "ok",
                    "server": server,
                    "config": self.config.summary(),
                    "secret_ref": postgres::SECRET_REF,
                })))
            }
            Err(err) => {
                let status = match &err {
                    PgError::ConnectionFailed(m)
                        if m.to_ascii_lowercase().contains("authentication")
                            || m.contains("28P01")
                            || m.to_ascii_lowercase().contains("does not exist") =>
                    {
                        "invalid"
                    }
                    PgError::ConnectionFailed(_) | PgError::Deadline(_) => "unreachable",
                    PgError::AccessDenied => "denied",
                    _ => "error",
                };
                Ok(CallToolResult::structured(json!({
                    "status": status,
                    "error": err.to_json(),
                    "remediation": postgres::credential_hint(),
                    "config": self.config.summary(),
                    "secret_ref": postgres::SECRET_REF,
                })))
            }
        }
    }

    /// Parameterized read (or, with writes enabled, any single statement).
    #[tool(
        description = "Run one parameterized SQL statement and return rows as JSON \
                       ({columns, rows, row_count, truncated, limit_applied}). Read-only by \
                       default: SELECT / WITH … SELECT / EXPLAIN / SHOW / VALUES / TABLE only, one \
                       statement, no FOR UPDATE, no data-modifying CTEs, no BEGIN/SET. Placeholders \
                       are $1..$n; bare JSON params bind as text/int8/float8/bool — use \
                       {\"type\": \"int4\"|\"uuid\"|\"timestamptz\"|…, \"value\": …} for other \
                       column types (describe_table shows them). Rows are capped by `limit` \
                       (default 100, max 1000). Cast enum/interval/domain/oid columns to text."
    )]
    #[tracing::instrument(name = "tool.query", skip(self, params))]
    async fn query(
        &self,
        Parameters(params): Parameters<QueryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Err(m) = check_sql_size(&params.sql, MAX_SQL_BYTES) {
            return Ok(tool_error(m));
        }
        let limit = match params.limit {
            None => self.config.default_row_limit,
            Some(n) if n < 1 => {
                return Ok(tool_error(format!(
                    "limit must be between 1 and {} (got {n}); omit it for the default of {}",
                    self.config.max_row_limit, self.config.default_row_limit
                )))
            }
            Some(n) => usize::try_from(n)
                .unwrap_or(usize::MAX)
                .min(self.config.max_row_limit),
        };
        let inspection = match inspect::inspect(&params.sql) {
            Ok(i) => i,
            Err(refusal) => return Ok(tool_error(refusal_message(&refusal, &self.config))),
        };
        if !self.config.allow_writes && !inspection.read_only {
            let kw = inspection.write_keyword.clone().unwrap_or_default();
            return Ok(tool_error(refusal_message(
                &Refusal::WriteInReadOnly(kw),
                &self.config,
            )));
        }
        let bound = match bind_params(params.params) {
            Ok(b) => b,
            Err(m) => return Ok(tool_error(m)),
        };
        // Fetch one extra row so `truncated` is exact.
        match postgres::query(&self.config, params.sql, bound, limit + 1).await {
            Ok(mut raw) => {
                let more = raw.rows.len() > limit;
                raw.rows.truncate(limit);
                let (rows, cut_by_bytes) = match render_rows(&raw, self.config.max_result_bytes) {
                    Ok(r) => r,
                    Err(err) => return Ok(db_error(&err)),
                };
                let row_count = rows.len();
                let mut out = json!({
                    "columns": raw.columns,
                    "rows": rows,
                    "row_count": row_count,
                    "truncated": more || cut_by_bytes,
                    "limit_applied": limit,
                    "statement": inspection.leading,
                });
                if cut_by_bytes {
                    out["truncated_reason"] = json!(format!(
                        "result exceeded POSTGRES_MAX_RESULT_BYTES ({} bytes): select fewer/narrower \
                         columns, lower the limit, or aggregate in SQL",
                        self.config.max_result_bytes
                    ));
                } else if more {
                    out["truncated_reason"] = json!(format!(
                        "more than {limit} rows matched: paginate with ORDER BY + OFFSET/keyset or \
                         aggregate in SQL; do not infer counts from a truncated page"
                    ));
                }
                Ok(CallToolResult::structured(out))
            }
            Err(err) => Ok(db_error(&err)),
        }
    }

    /// Writing/DDL statement, gated.
    #[tool(
        description = "Run one writing or DDL statement (INSERT/UPDATE/DELETE/MERGE/CREATE/ALTER/\
                       DROP/…) with $1..$n parameters and return {rows_affected}. Only available \
                       when the workload runs with POSTGRES_ALLOW_WRITES=true; otherwise it refuses \
                       without executing anything. No RETURNING output — use `query` for that."
    )]
    #[tracing::instrument(name = "tool.execute", skip(self, params))]
    async fn execute(
        &self,
        Parameters(params): Parameters<ExecuteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(refused) = write_gate(&self.config, "execute") {
            return Ok(refused);
        }
        if let Err(m) = check_sql_size(&params.sql, MAX_SQL_BYTES) {
            return Ok(tool_error(m));
        }
        if let Err(refusal) = inspect::inspect(&params.sql) {
            return Ok(tool_error(refusal_message(&refusal, &self.config)));
        }
        let bound = match bind_params(params.params) {
            Ok(b) => b,
            Err(m) => return Ok(tool_error(m)),
        };
        match postgres::execute(&self.config, params.sql, bound).await {
            Ok(n) => Ok(CallToolResult::structured(json!({ "rows_affected": n }))),
            Err(err) => Ok(db_error(&err)),
        }
    }

    /// Multi-statement script, gated.
    #[tool(
        description = "Run a multi-statement SQL script (migrations, seed data; no parameters) on \
                       one connection as a single implicit transaction — a failing statement rolls \
                       back the earlier ones unless the script has its own BEGIN/COMMIT. Only \
                       available when POSTGRES_ALLOW_WRITES=true."
    )]
    #[tracing::instrument(name = "tool.execute_batch", skip(self, params))]
    async fn execute_batch(
        &self,
        Parameters(params): Parameters<ExecuteBatchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(refused) = write_gate(&self.config, "execute_batch") {
            return Ok(refused);
        }
        if let Err(m) = check_sql_size(&params.sql, MAX_BATCH_BYTES) {
            return Ok(tool_error(m));
        }
        if params.sql.trim().is_empty() {
            return Ok(tool_error("the script is empty"));
        }
        let statements_estimated = params
            .sql
            .split(';')
            .filter(|s| !s.trim().is_empty())
            .count();
        match postgres::execute_batch(&self.config, params.sql).await {
            Ok(()) => Ok(CallToolResult::structured(json!({
                "ok": true,
                "statements_estimated": statements_estimated,
            }))),
            Err(err) => Ok(db_error(&err)),
        }
    }

    /// Schemas.
    #[tool(
        description = "List schemas with owner and comment. System schemas (pg_catalog, pg_toast, \
                       pg_temp_*, information_schema) are hidden unless include_system=true."
    )]
    #[tracing::instrument(name = "tool.list_schemas", skip(self))]
    async fn list_schemas(
        &self,
        Parameters(params): Parameters<ListSchemasParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let include_system =
            params.include_system.unwrap_or(false) || !self.config.hide_system_schemas;
        match postgres::query(
            &self.config,
            catalog::LIST_SCHEMAS.to_owned(),
            vec![bool_param(include_system)],
            10_000,
        )
        .await
        {
            Ok(raw) => match rows_to_objects(&raw) {
                Ok(schemas) => Ok(CallToolResult::structured(json!({
                    "count": schemas.len(),
                    "schemas": schemas,
                    "include_system": include_system,
                }))),
                Err(err) => Ok(db_error(&err)),
            },
            Err(err) => Ok(db_error(&err)),
        }
    }

    /// Tables in a schema.
    #[tool(
        description = "List tables, views, materialized views, foreign and partitioned tables in a \
                       schema (default public) with kind, estimated rows, size and comment. \
                       Optional `kinds` filter."
    )]
    #[tracing::instrument(name = "tool.list_tables", skip(self))]
    async fn list_tables(
        &self,
        Parameters(params): Parameters<ListTablesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let schema = params
            .schema
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.config.default_schema.clone());
        match postgres::query(
            &self.config,
            catalog::LIST_TABLES.to_owned(),
            vec![text_param(&schema)],
            100_000,
        )
        .await
        {
            Ok(raw) => {
                let mut tables = match rows_to_objects(&raw) {
                    Ok(t) => t,
                    Err(err) => return Ok(db_error(&err)),
                };
                if let Some(kinds) = params.kinds.filter(|k| !k.is_empty()) {
                    let wanted: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
                    tables.retain(|t| {
                        t.get("kind")
                            .and_then(Value::as_str)
                            .is_some_and(|k| wanted.contains(&k))
                    });
                }
                let hint = if tables.is_empty() {
                    Some(format!(
                        "no relations in schema {schema:?} (or it does not exist / is not visible \
                         to this role): list_schemas shows the schemas, search_objects finds a \
                         table by name"
                    ))
                } else {
                    None
                };
                Ok(CallToolResult::structured(json!({
                    "schema": schema,
                    "count": tables.len(),
                    "tables": tables,
                    "hint": hint,
                })))
            }
            Err(err) => Ok(db_error(&err)),
        }
    }

    /// Full definition of one relation.
    #[tool(
        description = "Describe one table or view: columns (name, type, nullable, default, \
                       identity/generated, enum values, comment), primary key, unique and check \
                       constraints, foreign keys (referenced table/columns, ON DELETE/UPDATE), \
                       indexes (definition, unique, size, scans), comment and kind. Call this before \
                       writing parameterized queries — it tells you which typed params or casts a \
                       column needs."
    )]
    #[tracing::instrument(name = "tool.describe_table", skip(self))]
    async fn describe_table(
        &self,
        Parameters(params): Parameters<DescribeTableParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (schema, table) =
            match resolve_table(&self.config, &params.table, params.schema.as_deref()) {
                Ok(t) => t,
                Err(m) => return Ok(tool_error(m)),
            };
        let args = || vec![text_param(&schema), text_param(&table)];

        let header = match postgres::query(
            &self.config,
            catalog::TABLE_HEADER.to_owned(),
            args(),
            2,
        )
        .await
        {
            Ok(raw) => match rows_to_objects(&raw) {
                Ok(rows) => rows.into_iter().next(),
                Err(err) => return Ok(db_error(&err)),
            },
            Err(err) => return Ok(db_error(&err)),
        };
        let Some(header) = header else {
            return Ok(tool_error(format!(
                "no table or view named {schema}.{table} — identifiers are case-sensitive as \
                 stored in the catalog (unquoted names were folded to lower case at creation). \
                 Use list_tables (schema {schema:?}) or search_objects to find it, or pass \
                 schema.table."
            )));
        };
        let columns = match postgres::query(
            &self.config,
            catalog::TABLE_COLUMNS.to_owned(),
            args(),
            5_000,
        )
        .await
        {
            Ok(raw) => match rows_to_objects(&raw) {
                Ok(rows) => rows,
                Err(err) => return Ok(db_error(&err)),
            },
            Err(err) => return Ok(db_error(&err)),
        };
        let constraints = match postgres::query(
            &self.config,
            catalog::TABLE_CONSTRAINTS.to_owned(),
            args(),
            5_000,
        )
        .await
        .and_then(|raw| rows_to_objects(&raw))
        {
            Ok(rows) => rows,
            Err(err) => return Ok(db_error(&err)),
        };
        let indexes =
            match postgres::query(&self.config, catalog::INDEXES.to_owned(), args(), 5_000)
                .await
                .and_then(|raw| rows_to_objects(&raw))
            {
                Ok(rows) => rows,
                Err(err) => return Ok(db_error(&err)),
            };

        let of_type = |t: &str| -> Vec<Value> {
            constraints
                .iter()
                .filter(|c| c.get("type").and_then(Value::as_str) == Some(t))
                .cloned()
                .collect()
        };
        let primary_key: Vec<Value> = of_type("primary_key")
            .into_iter()
            .flat_map(|c| {
                c.get("columns")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            })
            .collect();
        let typed_param_hints: Vec<Value> = columns
            .iter()
            .filter_map(|c| {
                let name = c.get("name")?.as_str()?;
                let ty = c.get("data_type")?.as_str()?;
                let hint = param_hint_for_type(ty)?;
                Some(json!({ "column": name, "data_type": ty, "bind_as": hint }))
            })
            .collect();

        Ok(CallToolResult::structured(json!({
            "schema": schema,
            "table": table,
            "kind": header.get("kind"),
            "owner": header.get("owner"),
            "comment": header.get("comment"),
            "estimated_rows": header.get("estimated_rows"),
            "total_bytes": header.get("total_bytes"),
            "view_definition": header.get("view_definition"),
            "columns": columns,
            "primary_key": primary_key,
            "foreign_keys": of_type("foreign_key"),
            "unique_constraints": of_type("unique"),
            "check_constraints": of_type("check"),
            "exclusion_constraints": of_type("exclusion"),
            "indexes": indexes,
            "param_hints": typed_param_hints,
        })))
    }

    /// Indexes for a table or schema.
    #[tool(
        description = "List indexes for one table or a whole schema: definition, uniqueness, \
                       validity, access method, size and usage counters (scans, tuples read) — \
                       for spotting unused or missing indexes."
    )]
    #[tracing::instrument(name = "tool.list_indexes", skip(self))]
    async fn list_indexes(
        &self,
        Parameters(params): Parameters<ListIndexesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (schema, table) = match params
            .table
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            Some(t) => match resolve_table(&self.config, t, params.schema.as_deref()) {
                Ok((s, t)) => (s, Some(t)),
                Err(m) => return Ok(tool_error(m)),
            },
            None => (
                params
                    .schema
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| self.config.default_schema.clone()),
                None,
            ),
        };
        let args = vec![
            text_param(&schema),
            table.as_deref().map(text_param).unwrap_or_else(null_param),
        ];
        match postgres::query(&self.config, catalog::INDEXES.to_owned(), args, 100_000)
            .await
            .and_then(|raw| rows_to_objects(&raw))
        {
            Ok(indexes) => Ok(CallToolResult::structured(json!({
                "schema": schema,
                "table": table,
                "count": indexes.len(),
                "indexes": indexes,
            }))),
            Err(err) => Ok(db_error(&err)),
        }
    }

    /// EXPLAIN (FORMAT JSON).
    #[tool(
        description = "Return the query plan as parsed JSON — EXPLAIN (FORMAT JSON) of the \
                       statement, plus a plan_summary (node type, total cost, estimated rows, and \
                       actual timings when analyze=true). analyze=true executes the statement \
                       (ANALYZE, BUFFERS); it is refused for non-read-only statements unless \
                       POSTGRES_ALLOW_WRITES=true. Use it before running unfamiliar heavy queries."
    )]
    #[tracing::instrument(name = "tool.explain_query", skip(self, params))]
    async fn explain_query(
        &self,
        Parameters(params): Parameters<ExplainParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Err(m) = check_sql_size(&params.sql, MAX_SQL_BYTES) {
            return Ok(tool_error(m));
        }
        if inspect::starts_with_explain(&params.sql) {
            return Ok(tool_error(
                "pass the statement to plan without a leading EXPLAIN; this tool adds \
                 EXPLAIN (FORMAT JSON …) itself",
            ));
        }
        let inspection = match inspect::inspect(&params.sql) {
            Ok(i) => i,
            Err(refusal) => return Ok(tool_error(refusal_message(&refusal, &self.config))),
        };
        let analyze = params.analyze.unwrap_or(false);
        if !self.config.allow_writes && !inspection.read_only {
            let kw = inspection.write_keyword.clone().unwrap_or_default();
            let base = refusal_message(&Refusal::WriteInReadOnly(kw), &self.config);
            let msg = if analyze {
                format!("{base} (EXPLAIN ANALYZE would execute it)")
            } else {
                format!(
                    "{base} (even a plain EXPLAIN of a write is refused in read-only mode: enable \
                     writes to plan writes)"
                )
            };
            return Ok(tool_error(msg));
        }
        let bound = match bind_params(params.params) {
            Ok(b) => b,
            Err(m) => return Ok(tool_error(m)),
        };
        let options = if analyze {
            "FORMAT JSON, ANALYZE, BUFFERS"
        } else {
            "FORMAT JSON"
        };
        let sql = format!("EXPLAIN ({options}) {}", params.sql);
        match postgres::query(&self.config, sql, bound, 10).await {
            Ok(raw) => {
                let plan = match first_cell(&raw) {
                    Ok(v) => v,
                    Err(err) => return Ok(db_error(&err)),
                };
                let top = plan.get(0).cloned().unwrap_or(Value::Null);
                let node = top.get("Plan").cloned().unwrap_or(Value::Null);
                let summary = json!({
                    "node_type": node.get("Node Type"),
                    "relation": node.get("Relation Name"),
                    "startup_cost": node.get("Startup Cost"),
                    "total_cost": node.get("Total Cost"),
                    "plan_rows": node.get("Plan Rows"),
                    "plan_width": node.get("Plan Width"),
                    "actual_rows": node.get("Actual Rows"),
                    "actual_total_time_ms": node.get("Actual Total Time"),
                    "planning_time_ms": top.get("Planning Time"),
                    "execution_time_ms": top.get("Execution Time"),
                    "analyzed": analyze,
                });
                Ok(CallToolResult::structured(json!({
                    "plan": plan,
                    "plan_summary": summary,
                })))
            }
            Err(err) => Ok(db_error(&err)),
        }
    }

    /// Size and health numbers.
    #[tool(
        description = "Size and health numbers for one table: estimated rows, live/dead tuples, \
                       seq/index scans, inserts/updates/deletes, last (auto)vacuum/analyze, \
                       table/index/toast/total bytes; exact_count=true adds SELECT count(*) \
                       (can be slow). Use this for counts instead of paging through `query`."
    )]
    #[tracing::instrument(name = "tool.table_stats", skip(self))]
    async fn table_stats(
        &self,
        Parameters(params): Parameters<TableStatsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (schema, table) =
            match resolve_table(&self.config, &params.table, params.schema.as_deref()) {
                Ok(t) => t,
                Err(m) => return Ok(tool_error(m)),
            };
        let stats = match postgres::query(
            &self.config,
            catalog::TABLE_STATS.to_owned(),
            vec![text_param(&schema), text_param(&table)],
            2,
        )
        .await
        .and_then(|raw| rows_to_objects(&raw))
        {
            Ok(rows) => rows.into_iter().next(),
            Err(err) => return Ok(db_error(&err)),
        };
        let Some(mut stats) = stats else {
            return Ok(tool_error(format!(
                "no table named {schema}.{table} (views have no statistics; identifiers are \
                 case-sensitive as stored). Use list_tables or search_objects to find it."
            )));
        };
        if params.exact_count.unwrap_or(false) {
            let sql = format!(
                "SELECT count(*)::bigint AS n FROM {}.{}",
                inspect::quote_ident(&schema),
                inspect::quote_ident(&table)
            );
            match postgres::query(&self.config, sql, Vec::new(), 1)
                .await
                .and_then(|raw| first_cell(&raw))
            {
                Ok(n) => {
                    if let Value::Object(map) = &mut stats {
                        map.insert("exact_row_count".to_owned(), n);
                    }
                }
                Err(err) => return Ok(db_error(&err)),
            }
        }
        if let Value::Object(map) = &mut stats {
            map.insert("schema".to_owned(), json!(schema));
            map.insert("table".to_owned(), json!(table));
            if map
                .get("estimated_rows")
                .and_then(Value::as_i64)
                .is_some_and(|n| n < 0)
            {
                map.insert(
                    "note".to_owned(),
                    json!("estimated_rows is -1: the table has never been analyzed; use exact_count=true or ANALYZE it"),
                );
            }
        }
        Ok(CallToolResult::structured(stats))
    }

    /// Name search across schemas.
    #[tool(
        description = "Find tables, views, columns and functions whose name contains a \
                       case-insensitive substring, across all non-system schemas — the discovery \
                       step when you do not know where data lives. Returns {matches: [{kind, \
                       schema, name, table?, data_type?}], truncated}."
    )]
    #[tracing::instrument(name = "tool.search_objects", skip(self))]
    async fn search_objects(
        &self,
        Parameters(params): Parameters<SearchObjectsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let pattern = params.pattern.trim();
        let chars = pattern.chars().count();
        if chars == 0 || chars > MAX_PATTERN_CHARS {
            return Ok(tool_error(format!(
                "pattern must be 1..{MAX_PATTERN_CHARS} characters (got {chars})"
            )));
        }
        let limit = match params.limit {
            None => SEARCH_DEFAULT_LIMIT,
            Some(n) if n < 1 => {
                return Ok(tool_error(format!(
                    "limit must be between 1 and {SEARCH_MAX_LIMIT} (got {n})"
                )))
            }
            Some(n) => usize::try_from(n)
                .unwrap_or(usize::MAX)
                .min(SEARCH_MAX_LIMIT),
        };
        let like = if params.glob.unwrap_or(false) {
            pattern.to_owned()
        } else {
            format!("%{}%", inspect::escape_like(pattern))
        };
        let kinds: Vec<String> = params
            .kinds
            .filter(|k| !k.is_empty())
            .map(|k| k.iter().map(|k| k.as_str().to_owned()).collect())
            .unwrap_or_else(|| {
                ["table", "view", "column", "function"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            });
        let include_system =
            params.include_system.unwrap_or(false) || !self.config.hide_system_schemas;
        let args = vec![
            text_param(&like),
            bool_param(include_system),
            text_array_param(catalog::search_kinds(&kinds)),
            int4_param(i32::try_from(limit + 1).unwrap_or(i32::MAX)),
        ];
        match postgres::query(
            &self.config,
            catalog::SEARCH_OBJECTS.to_owned(),
            args,
            limit + 1,
        )
        .await
        {
            Ok(mut raw) => {
                let truncated = raw.rows.len() > limit;
                raw.rows.truncate(limit);
                let matches = match rows_to_objects(&raw) {
                    Ok(m) => m,
                    Err(err) => return Ok(db_error(&err)),
                };
                Ok(CallToolResult::structured(json!({
                    "pattern": pattern,
                    "kinds": kinds,
                    "include_system": include_system,
                    "count": matches.len(),
                    "matches": matches,
                    "truncated": truncated,
                })))
            }
            Err(err) => Ok(db_error(&err)),
        }
    }
}

/// The typed-parameter form a column of this type needs (None when a bare
/// JSON value already binds correctly).
fn param_hint_for_type(data_type: &str) -> Option<&'static str> {
    let t = data_type.to_ascii_lowercase();
    let base = t.split('(').next().unwrap_or(&t).trim();
    Some(match base {
        "integer" | "int" | "int4" | "serial" => {
            "{\"type\":\"int4\",\"value\":42} or $N::text::int"
        }
        "smallint" | "int2" | "smallserial" => "{\"type\":\"int2\",\"value\":42}",
        "numeric" | "decimal" | "money" => "{\"type\":\"numeric\",\"value\":\"12.50\"} (a bare number also works) — read it back as col::text (exact) or col::float8",
        "real" | "float4" => "{\"type\":\"float4\",\"value\":1.5}",
        "uuid" => "{\"type\":\"uuid\",\"value\":\"…\"} or $N::text::uuid",
        "date" => "{\"type\":\"date\",\"value\":\"2024-01-31\"} or $N::text::date",
        "timestamp without time zone" => {
            "{\"type\":\"timestamp\",\"value\":\"2024-01-31T12:00:00\"}"
        }
        "timestamp with time zone" => {
            "{\"type\":\"timestamptz\",\"value\":\"2024-01-31T12:00:00Z\"} or $N::text::timestamptz"
        }
        "time without time zone" => "{\"type\":\"time\",\"value\":\"12:00:00\"}",
        "bytea" => "{\"type\":\"bytea\",\"value\":\"<base64>\"}",
        "json" => "{\"type\":\"json\",\"value\":{…}}",
        "jsonb" => "{\"type\":\"jsonb\",\"value\":{…}} (a bare JSON object also binds as jsonb)",
        "inet" => "{\"type\":\"inet\",\"value\":\"10.0.0.1\"}",
        "cidr" => "{\"type\":\"cidr\",\"value\":\"10.0.0.0/8\"}",
        "character" | "char" | "bpchar" => {
            "$N::text::char(n) — char(n) columns cannot be read back unquoted: SELECT col::text"
        }
        "interval" => "$N::text::interval — read back with EXTRACT(EPOCH FROM col) or col::text",
        "text[]" | "character varying[]" => "{\"type\":\"text[]\",\"value\":[\"a\",\"b\"]}",
        "integer[]" => "{\"type\":\"int4[]\",\"value\":[1,2]}",
        "bigint[]" => "{\"type\":\"int8[]\",\"value\":[1,2]}",
        _ if t.ends_with("[]") => "a typed array param or $N::text::<type>[]",
        _ if t.contains('.') => "$N::text::<type> (enum/domain: read back with col::text)",
        _ => return None,
    })
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PostgresServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                // Skills over MCP rides on the resources primitive: declaring
                // it is what makes `skill://` URIs discoverable at all.
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new(
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(
            "PostgreSQL MCP server running as a WebAssembly component on Cosmonic Desktop, \
             talking to one database through the host's wasmcloud:postgres interface (the \
             connection URL is the `postgres-mcp-url` secret; the component never sees it). \
             Read-only by default: `query` accepts SELECT/WITH/EXPLAIN/SHOW/VALUES/TABLE only; \
             `execute`/`execute_batch` work only when the workload sets POSTGRES_ALLOW_WRITES=true. \
             Start with `server_info` (connectivity, version, whether writes are enabled), then \
             `list_schemas`/`list_tables`/`search_objects` to find data, `describe_table` before \
             writing parameterized SQL (it shows which columns need typed params or casts), \
             `explain_query` before heavy queries, `table_stats` for counts and sizes. There are \
             no sessions: BEGIN/SET/temp tables do not carry across calls. Results are capped \
             (limit ≤ 1000 rows) and `truncated` tells you when more exist. Columns of enum, \
             domain, char(n), oid, interval, range types must be cast (`::text`) or the whole \
             query fails.\n\n\
             This server publishes skills — playbooks describing when and how to use its tools. \
             Read `skill://index.json` for the catalog, then `skill://postgres-mcp/SKILL.md` for \
             the operating manual (parameter typing, unsupported types, the error catalogue).",
        )
    }

    /// Skills over MCP: every skill file, plus the catalog, as resources.
    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(skills::resources()))
    }

    /// Parameterized `skill://` URIs, so a client can construct a skill
    /// request without having enumerated every resource first.
    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(
            skills::resource_templates(),
        ))
    }

    #[tracing::instrument(name = "resources.read", skip(self, _context), fields(uri = %request.uri))]
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let (mime_type, text) = skills::read(&request.uri).ok_or_else(|| {
            ErrorData::resource_not_found(
                format!(
                    "no resource at {}; read {} for the skills this server serves",
                    request.uri,
                    skills::INDEX_URI
                ),
                None,
            )
        })?;
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(text, request.uri).with_mime_type(mime_type)
        ])
        .into())
    }
}
