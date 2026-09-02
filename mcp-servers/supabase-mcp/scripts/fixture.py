#!/usr/bin/env python3
"""Hermetic stand-in for the Supabase Management API used by scripts/e2e.sh.

Modelled on the official server's test/mocks.ts (supabase-community/supabase-mcp,
Apache-2.0): an auth gate (401 unless `Authorization: Bearer sbp_e2e_token`,
403 for `sbp_forbidden`), ref-based fault injection (see REF_FAULTS), and the
routes the tools dial. The query endpoint echoes what it received as a single
row so tests can assert encoding, parameters and the read_only flag; the
Edge Function body answers 406 without `Accept: multipart/form-data`, like
the real platform.

Every request is appended to a JSON-lines log (argv[2]) with method, path,
raw target, query, lower-cased headers and the parsed body, so the e2e can
assert on headers and query-string encoding.

Usage: fixture.py <port> <request-log-path>
Optional: E2E_PG=1 makes requests for the ref gggggggggggggggggggg run the
received pg-meta SQL against the local Postgres container `mcp-pg` (podman
exec) instead of returning canned rows, to prove the borrowed SQL still parses.
"""
import json
import os
import re
import subprocess
import time
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

TOKEN = "sbp_e2e_token"
BOUNDARY = "e2e-boundary-7f3a9c"
PORT = int(sys.argv[1])
LOG_PATH = sys.argv[2]
LOCK = threading.Lock()

# 20-letter refs that trigger a fault on every project-scoped route.
REF_FAULTS = {
    "zzzzzzzzzzzzzzzzzzzz": (404, {"message": "Project not found"}, {}),
    "rrrrrrrrrrrrrrrrrrrr": (
        429,
        {"message": "Too many requests"},
        {"X-RateLimit-Limit": "120", "X-RateLimit-Remaining": "0", "X-RateLimit-Reset": "42"},
    ),
    "ffffffffffffffffffff": (403, {"message": "Forbidden"}, {}),
    "eeeeeeeeeeeeeeeeeeee": (500, {"message": "Internal server error"}, {}),
    "qqqqqqqqqqqqqqqqqqqq": (402, {"message": "Payment required"}, {}),
}

PROJECTS = [
    {
        "id": "abcdefghijklmnopqrst",
        "ref": "abcdefghijklmnopqrst",
        "name": "Prod ✓",
        "organization_id": "org-1",
        "organization_slug": "acme",
        "region": "us-east-1",
        "status": "ACTIVE_HEALTHY",
        "created_at": "2024-01-01T00:00:00.000Z",
        "database": {"host": "db.abcdefghijklmnopqrst.supabase.co", "version": "15.1.0.117", "postgres_engine": "15"},
    },
    {
        "id": "pppppppppppppppppppp",
        "ref": "pppppppppppppppppppp",
        "name": "Paused",
        "organization_id": "org-2",
        "organization_slug": "beta",
        "region": "eu-west-1",
        "status": "INACTIVE",
        "created_at": "2024-02-02T00:00:00.000Z",
        "database": {"host": "db.pppppppppppppppppppp.supabase.co", "version": "15.1.0.117", "postgres_engine": "15"},
    },
]

ORGS = [
    {"id": "org-1", "slug": "acme", "name": "Acme"},
    {"id": "org-2", "slug": "beta", "name": "Beta ✓ Ünïcode"},
]

TABLE_ROWS = [
    {
        "id": 16385, "schema": "public", "name": "orders", "rls_enabled": False, "rls_forced": False,
        "replica_identity": "DEFAULT", "bytes": 8192, "size": "8192 bytes", "live_rows_estimate": 42,
        "dead_rows_estimate": 0, "comment": "customer orders ✓",
        "primary_keys": [{"schema": "public", "table_name": "orders", "name": "id", "table_id": 16385}],
        "relationships": [{
            "id": 16400, "constraint_name": "orders_customer_id_fkey", "source_schema": "public",
            "source_table_name": "orders", "source_columns": ["customer_id"], "target_table_schema": "public",
            "target_table_name": "customers", "target_columns": ["id"]}],
        "columns": [
            {"table_id": 16385, "schema": "public", "table": "orders", "id": "16385.1", "ordinal_position": 1,
             "name": "id", "default_value": None, "data_type": "bigint", "format": "int8", "is_identity": True,
             "identity_generation": "ALWAYS", "is_generated": False, "is_nullable": False, "is_updatable": True,
             "is_unique": False, "enums": [], "check": None, "comment": None},
            {"table_id": 16385, "schema": "public", "table": "orders", "id": "16385.2", "ordinal_position": 2,
             "name": "status", "default_value": "'new'::text", "data_type": "USER-DEFINED", "format": "order_status",
             "is_identity": False, "identity_generation": None, "is_generated": False, "is_nullable": True,
             "is_updatable": True, "is_unique": False, "enums": ["new", "paid"], "check": None, "comment": None},
        ],
    },
    {
        "id": 16386, "schema": "public", "name": "customers", "rls_enabled": True, "rls_forced": False,
        "replica_identity": "DEFAULT", "bytes": 16384, "size": "16 kB", "live_rows_estimate": 7,
        "dead_rows_estimate": 1, "comment": None,
        "primary_keys": [{"schema": "public", "table_name": "customers", "name": "id", "table_id": 16386}],
        "relationships": [], "columns": [],
    },
]

EXTENSION_ROWS = [
    {"name": "pg_stat_statements", "schema": "extensions", "default_version": "1.10", "installed_version": "1.10", "comment": "track planning and execution statistics"},
    {"name": "postgis", "schema": None, "default_version": "3.3.2", "installed_version": None, "comment": "PostGIS geometry and geography spatial types and functions"},
]

SECURITY_LINTS = [
    {"name": "rls_disabled_in_public", "title": "RLS Disabled in Public", "level": "ERROR", "facing": "EXTERNAL",
     "categories": ["SECURITY"], "description": "Detects cases where row level security (RLS) has not been enabled on tables in schemas exposed to PostgREST",
     "detail": "Table `public.orders` is public, but RLS has not been enabled.",
     "remediation": "https://supabase.com/docs/guides/database/database-linter?lint=0013_rls_disabled_in_public",
     "metadata": {"name": "orders", "schema": "public", "type": "table"}, "cache_key": "rls_disabled_in_public_public_orders"},
    {"name": "function_search_path_mutable", "title": "Function Search Path Mutable", "level": "WARN", "facing": "EXTERNAL",
     "categories": ["SECURITY"], "description": "Detects functions where the search_path parameter is not set.",
     "detail": "Function `public.handle_new_user` has a role mutable search_path",
     "remediation": "https://supabase.com/docs/guides/database/database-linter?lint=0011_function_search_path_mutable",
     "metadata": {"name": "handle_new_user", "schema": "public", "type": "function"}, "cache_key": "function_search_path_mutable_public_handle_new_user"},
    {"name": "auth_leaked_password_protection", "title": "Leaked Password Protection Disabled", "level": "INFO", "facing": "EXTERNAL",
     "categories": ["SECURITY"], "description": "Leaked password protection is currently disabled.",
     "detail": "Supabase Auth prevents the use of compromised passwords by checking against HaveIBeenPwned.",
     "remediation": "https://supabase.com/docs/guides/database/database-linter?lint=0021_auth_leaked_password_protection",
     "metadata": {"type": "auth", "entity": "Auth"}, "cache_key": "auth_leaked_password_protection"},
]

PERFORMANCE_LINTS = [
    {"name": "unindexed_foreign_keys", "title": "Unindexed foreign keys", "level": "INFO", "facing": "EXTERNAL",
     "categories": ["PERFORMANCE"], "description": "Identifies foreign key constraints without a covering index",
     "detail": "Table `public.orders` has a foreign key `orders_customer_id_fkey` without a covering index.",
     "remediation": "https://supabase.com/docs/guides/database/database-linter?lint=0001_unindexed_foreign_keys",
     "metadata": {"name": "orders", "schema": "public", "type": "table", "fkey_name": "orders_customer_id_fkey"},
     "cache_key": "unindexed_foreign_keys_public_orders_orders_customer_id_fkey"},
]


def function_meta(ref, slug, version=3):
    prefix = f"file:///tmp/user_fn_{ref}_fn-1_3/source/"
    return {
        "id": "fn-1", "slug": slug, "name": slug, "status": "ACTIVE", "version": version,
        "created_at": 1700000000000, "updated_at": 1700000001000, "verify_jwt": True,
        "import_map": True, "entrypoint_path": prefix + "index.ts", "import_map_path": prefix + "deno.json",
    }


def multipart(files):
    out = b""
    for name, content, ctype in files:
        out += f"--{BOUNDARY}\r\n".encode()
        out += f'Content-Disposition: form-data; name="file"; filename="{name}"\r\n'.encode()
        out += f"Content-Type: {ctype}\r\n\r\n".encode()
        out += content.encode("utf-8") + b"\r\n"
    out += f"--{BOUNDARY}--\r\n".encode()
    return out


def run_pg(sql, parameters):
    """E2E_PG=1: run the received pg-meta SQL against the local mcp-pg container."""
    def quote(value):
        return "'" + str(value).replace("'", "''") + "'"

    substituted = re.sub(r"\$(\d+)", lambda m: quote(parameters[int(m.group(1)) - 1]), sql)
    wrapped = f"select coalesce(json_agg(t), '[]'::json) from ({substituted}) t"
    proc = subprocess.run(
        ["podman", "exec", "-i", "mcp-pg", "psql", "-U", "mcp", "-d", "mcp", "-At", "-v", "ON_ERROR_STOP=1"],
        input=wrapped.encode(), capture_output=True, timeout=60,
    )
    if proc.returncode != 0:
        return None, proc.stderr.decode(errors="replace")[:500]
    return json.loads(proc.stdout.decode() or "[]"), None


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    # -- helpers --------------------------------------------------------
    def send_bytes(self, code, body, ctype, extra=None):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        for key, value in (extra or {}).items():
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(body)

    def send_json(self, code, obj, extra=None):
        self.send_bytes(code, json.dumps(obj).encode("utf-8"), "application/json", extra)

    def record(self, parsed, query, body_text, body_json):
        entry = {
            "method": self.command,
            "path": parsed.path,
            "target": self.path,
            "query": query,
            "headers": {k.lower(): v for k, v in self.headers.items()},
            "body": body_text,
            "json": body_json,
        }
        with LOCK:
            with open(LOG_PATH, "a", encoding="utf-8") as log:
                log.write(json.dumps(entry, ensure_ascii=False) + "\n")

    # -- verbs ----------------------------------------------------------
    def do_GET(self):
        self.dispatch(b"")

    def do_POST(self):
        self.dispatch(self.read_body())

    def read_body(self):
        """Reads a Content-Length or chunked request body (wasmtime sends
        outbound bodies chunked, exactly like a real client may)."""
        if "chunked" in (self.headers.get("Transfer-Encoding") or "").lower():
            chunks = []
            while True:
                line = self.rfile.readline().strip()
                size = int(line.split(b";")[0] or b"0", 16)
                if size == 0:
                    while self.rfile.readline() not in (b"\r\n", b"\n", b""):
                        pass
                    break
                chunks.append(self.rfile.read(size))
                self.rfile.readline()  # CRLF that terminates the chunk
            return b"".join(chunks)
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length) if length > 0 else b""

    def dispatch(self, raw):
        parsed = urlparse(self.path)
        query = {k: v[0] for k, v in parse_qs(parsed.query, keep_blank_values=True).items()}
        body_text = raw.decode("utf-8", errors="replace")
        try:
            body_json = json.loads(body_text) if body_text else None
        except ValueError:
            body_json = None
        self.record(parsed, query, body_text, body_json)
        path = parsed.path

        if path == "/health":
            return self.send_json(200, {"ok": True})

        # Slow project: stalls longer than the 2 s outbound deadline the e2e
        # gives one instance (MCP_OUTBOUND_TIMEOUT_MS=2000), before the auth
        # check so any token reaches it.
        if path.startswith("/v1/projects/ssssssssssssssssssss"):
            time.sleep(4)

        auth = self.headers.get("Authorization", "")
        if auth == "Bearer sbp_forbidden":
            return self.send_json(403, {"message": "Forbidden"})
        if auth != f"Bearer {TOKEN}":
            # Distinctive text so the e2e can prove the upstream message is
            # carried through (the mapped prefix alone always says Unauthorized).
            return self.send_json(401, {"message": "fixture rejected the bearer token"})

        if path == "/v1/organizations":
            return self.send_json(200, ORGS)
        if path == "/v1/projects":
            return self.send_json(200, PROJECTS)

        m = re.match(r"^/v1/projects/([^/]+)(/.*)?$", path)
        if not m:
            return self.send_json(404, {"message": f"no fixture route for {path}"})
        ref, rest = m.group(1), m.group(2) or ""

        if ref in REF_FAULTS:
            code, payload, extra = REF_FAULTS[ref]
            return self.send_json(code, payload, extra)
        if ref == "bbbbbbbbbbbbbbbbbbbb":
            return self.send_bytes(200, b"this is not json {", "text/plain")

        if rest == "":
            project = next((p for p in PROJECTS if p["id"] == ref), None)
            if project is None:
                project = dict(PROJECTS[0], id=ref, ref=ref, name=f"Project {ref}")
            return self.send_json(200, project)

        if rest == "/database/query":
            return self.database_query(ref, body_json or {})
        if rest == "/database/migrations":
            if self.command == "GET":
                return self.send_json(200, [
                    {"version": "20240101000000", "name": "init"},
                    {"version": "20240202000000", "name": "add_orders"},
                ])
            q = (body_json or {}).get("query", "")
            if "nope" in q:
                return self.send_json(500, {"message": "Failed to execute query: relation \"nope\" does not exist"})
            return self.send_json(200, {})
        if rest == "/analytics/endpoints/logs":
            return self.logs(ref, query)
        if rest == "/advisors/security":
            if ref == "uuuuuuuuuuuuuuuuuuuu":
                return self.send_json(200, {"weird": True})
            return self.send_json(200, {"lints": SECURITY_LINTS})
        if rest == "/advisors/performance":
            return self.send_json(200, {"lints": PERFORMANCE_LINTS})
        if rest == "/api-keys":
            if query.get("reveal") != "false":
                return self.send_json(400, {"message": "reveal=false is required by this fixture"})
            if ref == "kkkkkkkkkkkkkkkkkkkk":
                return self.send_json(200, [{"name": "default-secret", "api_key": None, "type": "secret", "id": "secret-1"}])
            return self.send_json(200, [
                {"name": "anon", "api_key": "eyJanon.legacy.jwt", "type": "legacy", "id": "anon-id"},
                {"name": "service_role", "api_key": "eyJservice.role.jwt", "type": "legacy", "id": "service-id"},
                {"name": "default", "api_key": "sb_publishable_abc123", "type": "publishable", "id": "pub-1", "description": "Main publishable key"},
                {"name": "default-secret", "api_key": None, "type": "secret", "id": "secret-1", "description": "sb_secret_hidden"},
            ])
        if rest == "/api-keys/legacy":
            if ref == "mmmmmmmmmmmmmmmmmmmm":
                return self.send_json(500, {"message": "legacy status unavailable"})
            return self.send_json(200, {"enabled": False})
        if rest == "/types/typescript":
            schemas = query.get("included_schemas", "")
            if ref == "hhhhhhhhhhhhhhhhhhhh":
                # Larger than the bridge's 4 MiB outbound cap: the server must
                # surface the size-cap error, not an allowedHosts hint.
                types = "export type Database = {\n" + ("x" * (4 * 1024 * 1024 + 65536)) + "}\n"
            elif ref == "tttttttttttttttttttt":
                types = "export type Database = {\n" + ("  // padding ✓\n" * 90000) + "}\n"
            else:
                types = f"export type Database = {{ /* schemas: {schemas} */ public: {{ Tables: {{ orders: {{ Row: {{ id: number }} }} }} }} }}"
            return self.send_json(200, {"types": types})
        if rest == "/functions":
            return self.send_json(200, [function_meta(ref, "hello-world")])
        fm = re.match(r"^/functions/([^/]+)(/body)?$", rest)
        if fm:
            slug, body = fm.group(1), fm.group(2)
            # Non-object 2xx metadata bodies: the server must answer with a
            # tool error, never trap (serde_json IndexMut on a non-object).
            odd_shapes = {"array-fn": [], "string-fn": "hello", "number-fn": 42, "null-fn": None}
            if slug in odd_shapes and not body:
                return self.send_json(200, odd_shapes[slug])
            if slug not in ("hello-world", "big-fn", "grumpy-fn", "raw-fn", "strver-fn"):
                return self.send_json(404, {"message": "Edge Function not found"})
            if not body:
                # strver-fn: the platform's `version` arrives as a JSON string.
                version = "3" if slug == "strver-fn" else 3
                return self.send_json(200, function_meta(ref, slug, version))
            if slug == "grumpy-fn" or self.headers.get("Accept") != "multipart/form-data":
                return self.send_json(406, {"message": "Invalid Accept header. Must be multipart/form-data"})
            if slug == "raw-fn":
                return self.send_bytes(200, "console.log('raw body ✓')".encode(), "text/plain; charset=utf-8")
            if slug == "big-fn":
                files = [("index.ts", "// big\n" + ("x" * 600 * 1024), "application/typescript")]
            else:
                files = [
                    (f"/tmp/user_fn_{ref}_fn-1_3/source/index.ts",
                     "// ✓ héllo from the fixture\nDeno.serve(async (req: Request) => new Response('ok'));\n",
                     "application/typescript"),
                    ("source/deno.json", '{ "imports": { "@supabase/supabase-js": "jsr:@supabase/supabase-js@2" } }', "application/json"),
                ]
            return self.send_bytes(200, multipart(files), f"multipart/form-data; boundary={BOUNDARY}")

        return self.send_json(404, {"message": f"no fixture route for {path}"})

    def database_query(self, ref, body):
        query = body.get("query", "") or ""
        parameters = body.get("parameters") or []
        read_only = body.get("read_only")
        lowered = query.lstrip().lower()
        if "nope" in query:
            return self.send_json(400, {
                "error": 'ERROR: 42P01: relation "nope" does not exist',
                "message": 'relation "nope" does not exist',
                "code": "42P01",
                "formattedError": 'ERROR: 42P01: relation "nope" does not exist',
                "position": "15",
            })
        if read_only and lowered.startswith(("insert", "update", "delete", "create", "drop", "alter", "truncate")):
            verb = lowered.split()[0].upper()
            return self.send_json(400, {
                "error": f"ERROR: 25006: cannot execute {verb} in a read-only transaction",
                "message": f"cannot execute {verb} in a read-only transaction",
                "code": "25006",
                "formattedError": f"ERROR: 25006: cannot execute {verb} in a read-only transaction",
            })
        if "many_rows" in query:
            return self.send_json(201, [{"n": i, "label": f"row-{i}"} for i in range(1500)])
        is_tables = "pg_total_relation_size" in query
        is_extensions = "pg_available_extensions" in query
        if is_tables or is_extensions:
            # Ref gggg… + E2E_PG=1: run the borrowed SQL on the local mcp-pg
            # container instead of returning canned rows.
            if ref == "gggggggggggggggggggg" and os.environ.get("E2E_PG") == "1":
                rows, err = run_pg(query, parameters)
                if err is not None:
                    return self.send_json(400, {"message": f"local postgres rejected the pg-meta SQL: {err}", "code": "E2EPG"})
                return self.send_json(201, rows)
            return self.send_json(201, TABLE_ROWS if is_tables else EXTENSION_ROWS)
        return self.send_json(201, [{
            "query": query,
            "parameters": parameters,
            "read_only": read_only,
            "user_agent": self.headers.get("User-Agent"),
            "content_type": self.headers.get("Content-Type"),
            "accept": self.headers.get("Accept"),
        }])

    def logs(self, ref, query):
        start = query.get("iso_timestamp_start")
        end = query.get("iso_timestamp_end")
        if (start is None) != (end is None):
            return self.send_json(400, {"message": "both iso_timestamp_start and iso_timestamp_end are required"})
        sql = query.get("sql", "")
        if ref == "llllllllllllllllllll" or "boom" in sql:
            return self.send_json(200, {"result": [], "error": "bad clickhouse"})
        return self.send_json(200, {
            "result": [{
                "id": "log-1", "timestamp": 1700000000000000, "event_message": "hello ✓ from logs",
                "sql": sql, "iso_timestamp_start": start, "iso_timestamp_end": end,
            }],
            "error": None,
        })


if __name__ == "__main__":
    # ThreadingHTTPServer: the harness fires 8 concurrent tool calls.
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
