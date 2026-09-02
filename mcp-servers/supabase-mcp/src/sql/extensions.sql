-- Borrowed verbatim from supabase-community/supabase-mcp
-- (packages/mcp-server-supabase/src/pg-meta/extensions.sql, Apache-2.0).

SELECT
  e.name,
  n.nspname AS schema,
  e.default_version,
  x.extversion AS installed_version,
  e.comment
FROM
  pg_available_extensions() e(name, default_version, comment)
  LEFT JOIN pg_extension x ON e.name = x.extname
  LEFT JOIN pg_namespace n ON x.extnamespace = n.oid
