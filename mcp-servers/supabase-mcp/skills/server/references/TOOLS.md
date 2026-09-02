# Tool reference

Supporting file of the `supabase-mcp` skill, served at
`skill://supabase-mcp/references/TOOLS.md`. Every project-scoped tool takes
`project_id` (the 20-letter lowercase ref from `list_projects`); it is ignored
when the server is pinned with `SUPABASE_PROJECT_REF`. Upstream failures are
tool errors (`isError: true`) carrying the mapped message from the SKILL.md
error catalogue; JSON-RPC `-32602` only appears for unroutable requests.

| Tool | Arguments | Output (`structuredContent`) | Upstream | Gated by |
|---|---|---|---|---|
| `check_auth` | none | `status` ok/missing/invalid/insufficient/error, `identity` (organizations or the pinned project), `mode` {read_only, pinned_project, base_url}, `remediation` on failure | `GET /v1/organizations` (or `GET /v1/projects/{ref}` when pinned) | — |
| `list_organizations` | none | `count`, `organizations[]` {id, slug, name} | `GET /v1/organizations` | hidden + refused when pinned |
| `list_projects` | none | `count`, `projects[]` {id, name, organization_id, organization_slug, region, status, created_at, database_version} | `GET /v1/projects` (no pagination) | hidden + refused when pinned |
| `get_project` | `project_id` | `project` (raw), `healthy` (status == ACTIVE_HEALTHY), `hint` when not healthy | `GET /v1/projects/{ref}` | — |
| `list_tables` | `project_id`, `schemas?` string[] (default `["public"]`, `[]` = all non-system, ≤ 20), `verbose?` bool, `max_rows?` 1..1000 | `tables[]` {name "schema.table", rls_enabled, rows, size, primary_keys, comment?; verbose adds columns[] {name, data_type, format, options[], default_value?, identity_generation?, enums?, check?, comment?} and foreign_key_constraints[]}, `count`, `total`, `truncated`, `advisory?` (rls_disabled) | `POST /v1/projects/{ref}/database/query` (pg-meta SQL, schemas bound as `$1…`, `read_only: true`) | — |
| `list_extensions` | `project_id` | `extensions[]` {name, schema, default_version, installed_version, comment}, `count`, `installed` | `POST …/database/query` (extensions SQL, read-only) | — |
| `list_migrations` | `project_id` | `migrations[]` {version, name}, `count` | `GET /v1/projects/{ref}/database/migrations` | — |
| `execute_sql` | `project_id`, `query` (1..100,000 chars), `max_rows?` 1..1000 | `rows[]`, `row_count`, `returned`, `truncated`, `read_only`; text block = rows inside an untrusted-data boundary | `POST …/database/query` {query, read_only} | writes fail upstream (25006) when `SUPABASE_READ_ONLY=true` |
| `apply_migration` | `project_id`, `name` `^[a-z0-9_]{1,100}$`, `query` (1..200,000 chars) | `success: true`, `name`, `hint` | `POST …/database/migrations` {name, query} | refused when `SUPABASE_READ_ONLY=true` |
| `get_logs` | `project_id`, `service` api\|postgres\|auth\|storage\|realtime\|edge-function\|edge-function-runtime\|branch-action, `iso_timestamp_start?`, `iso_timestamp_end?` (RFC 3339 with offset), `limit?` 1..100 | `result[]` (newest first), `count`, `service`, normalised timestamps, `window_clamped`, `limit`; text block inside an untrusted-data boundary | `GET …/analytics/endpoints/logs?sql=&iso_timestamp_start=&iso_timestamp_end=` | 30 req/min upstream |
| `get_advisors` | `project_id`, `type` security\|performance, `level_min?` INFO\|WARN\|ERROR | `lints[]` {name, title, level, facing, categories, description, detail, remediation, metadata}, `count`, `total`, `filtered_out`, `by_level` (or `raw` when the shape is unexpected) | `GET …/advisors/{type}` | — |
| `list_edge_functions` | `project_id` | `functions[]` {id, slug, name, status, version, verify_jwt, entrypoint_path, import_map_path, created_at, updated_at}, `count` | `GET …/functions` | — |
| `get_edge_function` | `project_id`, `function_slug` `^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$` | function metadata + `files[]` {name, content, truncated}, `file_count`, `truncated` | `GET …/functions/{slug}` then `GET …/functions/{slug}/body` with `Accept: multipart/form-data` | 512 KiB/file, 2 MiB total |
| `get_project_url` | `project_id` | `url` `https://{ref}.supabase.co` | none (computed) | — |
| `get_publishable_keys` | `project_id` | `keys[]` {api_key, name, type legacy\|publishable, id?, description?, disabled?}, `legacy_keys_enabled`, `count` | `GET …/api-keys?reveal=false` + `GET …/api-keys/legacy` | secret / service_role keys never returned |
| `generate_typescript_types` | `project_id`, `included_schemas?` string[] (default `["public"]`, ≤ 20) | `types` (string), `bytes`, `truncated`, `included_schemas` | `GET …/types/typescript?included_schemas=` | 1 MiB output cap |

## Validation performed before any network call

- `project_id`: exactly 20 lowercase ASCII letters. Anything else (uppercase,
  unicode, path segments, org ids) is refused locally.
- Schema names: `^[A-Za-z_][A-Za-z0-9_$]*$`, ≤ 63 bytes, ≤ 20 per call,
  de-duplicated; passed to Postgres as bound parameters.
- Migration names: `^[a-z0-9_]{1,100}$`.
- Function slugs: `^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$`.
- Timestamps: `YYYY-MM-DDTHH:MM:SS[.fff](Z|±HH:MM)`, calendar-checked.
- Numeric limits are clamped, not rejected: `max_rows` → 1..1000, `limit` →
  1..100.

## Headers sent upstream

`Authorization: Bearer <SUPABASE_ACCESS_TOKEN>`, `User-Agent:
supabase-mcp-cosmonic/<version>`, `Accept: application/json` (or
`multipart/form-data` for the function body), `Content-Type:
application/json` on POSTs. Query values are percent-encoded (RFC 3986
unreserved set).

## Deployment knobs (env)

| Env | Kind | Default | Effect |
|---|---|---|---|
| `SUPABASE_ACCESS_TOKEN` | secret ref `supabase-mcp-access-token` | — (required) | the PAT |
| `SUPABASE_READ_ONLY` | named config | `true` | read-only role for `execute_sql`; `apply_migration` refused |
| `SUPABASE_PROJECT_REF` | named config | unset | pin to one project; hides account tools |
| `SUPABASE_MAX_ROWS` | named config | `200` | default row cap (1..1000) |
| `SUPABASE_BASE_URL` | test override | `https://api.supabase.com` | Management API origin (`/v1` appended); drives the project-URL domain |
