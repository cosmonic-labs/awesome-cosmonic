//! Introspection SQL for the schema tools.
//!
//! Every statement reads `pg_catalog` directly and casts every output column
//! to a type the host can convert (`::text`, `::bigint`, `::int`, `bool`,
//! `timestamptz`, `text[]`) — `name`, `"char"`, `oid`, `regclass` and
//! `float4` columns are cast explicitly because the host's value conversion
//! rejects some of them and the rest deserve a stable JSON shape.
//!
//! Shapes follow crystaldba/postgres-mcp (MIT) and the archived official
//! server (MIT): `list_schemas`, `list_tables`/`list_objects`,
//! `describe_table`/`get_object_details`, `explain_query`; the queries
//! themselves are written for this host's type constraints.

/// `server_info`: identity, version, settings that change tool behaviour.
pub const SERVER_INFO: &str = "\
SELECT version()::text AS version,
       current_database()::text AS database,
       current_user::text AS \"user\",
       current_setting('server_version_num')::int AS version_num,
       pg_is_in_recovery() AS in_recovery,
       now() AS now,
       current_setting('statement_timeout')::text AS statement_timeout,
       current_setting('default_transaction_read_only')::text AS default_transaction_read_only,
       current_setting('search_path')::text AS search_path,
       current_setting('TimeZone')::text AS timezone,
       current_setting('server_encoding')::text AS server_encoding,
       inet_server_addr()::text AS server_addr,
       inet_server_port()::int AS server_port,
       pg_postmaster_start_time() AS started_at,
       (SELECT count(*)::int FROM pg_stat_activity) AS backends";

/// `list_schemas` — `$1::bool` = include system schemas.
pub const LIST_SCHEMAS: &str = "\
SELECT n.nspname::text AS name,
       pg_get_userbyid(n.nspowner)::text AS owner,
       obj_description(n.oid, 'pg_namespace')::text AS comment,
       (n.nspname LIKE 'pg\\_%' OR n.nspname = 'information_schema') AS is_system
FROM pg_namespace n
WHERE $1::bool OR NOT (n.nspname LIKE 'pg\\_%' OR n.nspname = 'information_schema')
ORDER BY 1";

/// `list_tables` — `$1::text` = schema.
pub const LIST_TABLES: &str = "\
SELECT c.relname::text AS name,
       (CASE c.relkind WHEN 'r' THEN 'table' WHEN 'p' THEN 'partitioned_table' WHEN 'v' THEN 'view'
                       WHEN 'm' THEN 'materialized_view' WHEN 'f' THEN 'foreign_table' END)::text AS kind,
       c.reltuples::bigint AS estimated_rows,
       pg_total_relation_size(c.oid)::bigint AS total_bytes,
       pg_size_pretty(pg_total_relation_size(c.oid))::text AS total_size,
       obj_description(c.oid, 'pg_class')::text AS comment
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1::text AND c.relkind IN ('r', 'p', 'v', 'm', 'f')
ORDER BY 1";

/// `describe_table` — relation header. `$1::text` schema, `$2::text` table.
pub const TABLE_HEADER: &str = "\
SELECT (CASE c.relkind WHEN 'r' THEN 'table' WHEN 'p' THEN 'partitioned_table' WHEN 'v' THEN 'view'
                       WHEN 'm' THEN 'materialized_view' WHEN 'f' THEN 'foreign_table'
                       ELSE c.relkind::text END)::text AS kind,
       obj_description(c.oid, 'pg_class')::text AS comment,
       pg_get_userbyid(c.relowner)::text AS owner,
       c.reltuples::bigint AS estimated_rows,
       pg_total_relation_size(c.oid)::bigint AS total_bytes,
       (CASE WHEN c.relkind IN ('v', 'm') THEN pg_get_viewdef(c.oid, true) END)::text AS view_definition
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1::text AND c.relname = $2::text AND c.relkind IN ('r', 'p', 'v', 'm', 'f')";

/// `describe_table` — columns.
pub const TABLE_COLUMNS: &str = "\
SELECT a.attnum::int AS position,
       a.attname::text AS name,
       format_type(a.atttypid, a.atttypmod)::text AS data_type,
       (NOT a.attnotnull) AS nullable,
       pg_get_expr(d.adbin, d.adrelid)::text AS default_value,
       (CASE a.attidentity WHEN 'a' THEN 'always' WHEN 'd' THEN 'by default' END)::text AS identity,
       (CASE a.attgenerated WHEN 's' THEN 'stored' END)::text AS generated,
       col_description(a.attrelid, a.attnum)::text AS comment,
       (t.typtype = 'e') AS is_enum,
       (t.typtype = 'd') AS is_domain,
       (CASE WHEN t.typtype = 'e' THEN (SELECT array_agg(e.enumlabel::text ORDER BY e.enumsortorder)
                                         FROM pg_enum e WHERE e.enumtypid = t.oid) END)::text[] AS enum_values
FROM pg_attribute a
JOIN pg_class c ON c.oid = a.attrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
JOIN pg_type t ON t.oid = a.atttypid
LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
WHERE n.nspname = $1::text AND c.relname = $2::text AND a.attnum > 0 AND NOT a.attisdropped
ORDER BY a.attnum";

/// `describe_table` — constraints (primary key, unique, check, foreign keys).
pub const TABLE_CONSTRAINTS: &str = "\
SELECT con.conname::text AS name,
       (CASE con.contype WHEN 'p' THEN 'primary_key' WHEN 'u' THEN 'unique' WHEN 'c' THEN 'check'
                         WHEN 'f' THEN 'foreign_key' WHEN 'x' THEN 'exclusion' ELSE con.contype::text END)::text AS type,
       pg_get_constraintdef(con.oid, true)::text AS definition,
       (SELECT array_agg(a.attname::text ORDER BY k.ord)
          FROM unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord)
          JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.attnum)::text[] AS columns,
       (CASE WHEN con.contype = 'f' THEN (SELECT n2.nspname::text FROM pg_class c2 JOIN pg_namespace n2 ON n2.oid = c2.relnamespace WHERE c2.oid = con.confrelid) END)::text AS referenced_schema,
       (CASE WHEN con.contype = 'f' THEN (SELECT c2.relname::text FROM pg_class c2 WHERE c2.oid = con.confrelid) END)::text AS referenced_table,
       (CASE WHEN con.contype = 'f' THEN (SELECT array_agg(a.attname::text ORDER BY k.ord)
          FROM unnest(con.confkey) WITH ORDINALITY AS k(attnum, ord)
          JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.attnum) END)::text[] AS referenced_columns,
       (CASE WHEN con.contype = 'f' THEN (CASE con.confdeltype WHEN 'a' THEN 'NO ACTION' WHEN 'r' THEN 'RESTRICT' WHEN 'c' THEN 'CASCADE' WHEN 'n' THEN 'SET NULL' WHEN 'd' THEN 'SET DEFAULT' END) END)::text AS on_delete,
       (CASE WHEN con.contype = 'f' THEN (CASE con.confupdtype WHEN 'a' THEN 'NO ACTION' WHEN 'r' THEN 'RESTRICT' WHEN 'c' THEN 'CASCADE' WHEN 'n' THEN 'SET NULL' WHEN 'd' THEN 'SET DEFAULT' END) END)::text AS on_update,
       con.condeferrable AS deferrable
FROM pg_constraint con
JOIN pg_class c ON c.oid = con.conrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1::text AND c.relname = $2::text
ORDER BY con.contype, con.conname";

/// `describe_table` / `list_indexes` — indexes. `$1::text` schema, `$2::text`
/// table or NULL for the whole schema.
pub const INDEXES: &str = "\
SELECT n.nspname::text AS schema,
       c.relname::text AS table,
       ic.relname::text AS name,
       pg_get_indexdef(i.indexrelid)::text AS definition,
       i.indisunique AS is_unique,
       i.indisprimary AS is_primary,
       i.indisvalid AS is_valid,
       am.amname::text AS method,
       pg_relation_size(i.indexrelid)::bigint AS bytes,
       pg_size_pretty(pg_relation_size(i.indexrelid))::text AS size,
       COALESCE(s.idx_scan, 0)::bigint AS scans,
       COALESCE(s.idx_tup_read, 0)::bigint AS tuples_read,
       COALESCE(s.idx_tup_fetch, 0)::bigint AS tuples_fetched
FROM pg_index i
JOIN pg_class ic ON ic.oid = i.indexrelid
JOIN pg_class c ON c.oid = i.indrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
LEFT JOIN pg_am am ON am.oid = ic.relam
LEFT JOIN pg_stat_user_indexes s ON s.indexrelid = i.indexrelid
WHERE n.nspname = $1::text AND ($2::text IS NULL OR c.relname = $2::text)
ORDER BY c.relname, ic.relname";

/// `table_stats` — `$1::text` schema, `$2::text` table.
pub const TABLE_STATS: &str = "\
SELECT (CASE c.relkind WHEN 'r' THEN 'table' WHEN 'p' THEN 'partitioned_table'
                       WHEN 'm' THEN 'materialized_view' ELSE c.relkind::text END)::text AS kind,
       c.reltuples::bigint AS estimated_rows,
       COALESCE(s.n_live_tup, 0)::bigint AS live_tuples,
       COALESCE(s.n_dead_tup, 0)::bigint AS dead_tuples,
       COALESCE(s.seq_scan, 0)::bigint AS seq_scans,
       COALESCE(s.idx_scan, 0)::bigint AS index_scans,
       COALESCE(s.n_tup_ins, 0)::bigint AS inserts,
       COALESCE(s.n_tup_upd, 0)::bigint AS updates,
       COALESCE(s.n_tup_del, 0)::bigint AS deletes,
       s.last_vacuum AS last_vacuum,
       s.last_autovacuum AS last_autovacuum,
       s.last_analyze AS last_analyze,
       s.last_autoanalyze AS last_autoanalyze,
       pg_relation_size(c.oid)::bigint AS table_bytes,
       pg_indexes_size(c.oid)::bigint AS index_bytes,
       (pg_total_relation_size(c.oid) - pg_relation_size(c.oid) - pg_indexes_size(c.oid))::bigint AS toast_bytes,
       pg_total_relation_size(c.oid)::bigint AS total_bytes,
       pg_size_pretty(pg_total_relation_size(c.oid))::text AS total_size
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
LEFT JOIN pg_stat_user_tables s ON s.relid = c.oid
WHERE n.nspname = $1::text AND c.relname = $2::text AND c.relkind IN ('r', 'p', 'm')";

/// `search_objects` — `$1::text` ILIKE pattern, `$2::bool` include system
/// schemas, `$3::text[]` kinds, `$4::int` limit.
pub const SEARCH_OBJECTS: &str = "\
SELECT o.kind, o.schema_name AS schema, o.name, o.table_name AS \"table\", o.data_type
FROM (
  SELECT (CASE c.relkind WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized_view' WHEN 'f' THEN 'foreign_table'
                         WHEN 'p' THEN 'partitioned_table' ELSE 'table' END)::text AS kind,
         n.nspname::text AS schema_name, c.relname::text AS name, NULL::text AS table_name, NULL::text AS data_type
  FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
  WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND c.relname ILIKE $1::text ESCAPE '\\'
  UNION ALL
  SELECT 'column'::text, n.nspname::text, a.attname::text, c.relname::text, format_type(a.atttypid, a.atttypmod)::text
  FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace
  WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND a.attnum > 0 AND NOT a.attisdropped AND a.attname ILIKE $1::text ESCAPE '\\'
  UNION ALL
  SELECT (CASE p.prokind WHEN 'p' THEN 'procedure' WHEN 'a' THEN 'aggregate' ELSE 'function' END)::text,
         n.nspname::text, p.proname::text, NULL::text, pg_get_function_result(p.oid)::text
  FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
  WHERE p.proname ILIKE $1::text ESCAPE '\\'
) o
WHERE ($2::bool OR NOT (o.schema_name LIKE 'pg\\_%' OR o.schema_name = 'information_schema'))
  AND o.kind = ANY($3::text[])
ORDER BY o.schema_name, o.name, o.kind
LIMIT $4::int";

/// Kinds the `search_objects` `kinds` filter expands to.
pub fn search_kinds(kinds: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for kind in kinds {
        match kind.as_str() {
            "table" => out.extend(
                ["table", "partitioned_table", "foreign_table"]
                    .iter()
                    .map(|s| s.to_string()),
            ),
            "view" => out.extend(["view", "materialized_view"].iter().map(|s| s.to_string())),
            "column" => out.push("column".to_owned()),
            "function" => out.extend(
                ["function", "procedure", "aggregate"]
                    .iter()
                    .map(|s| s.to_string()),
            ),
            other => out.push(other.to_owned()),
        }
    }
    out.sort();
    out.dedup();
    out
}
