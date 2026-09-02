# Conventions for MCP servers in this directory

Every server here is built from
[mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs)
(rmcp 3.x, MCP 2026-07-28, `wasi:http/handler@0.3.0`) and deployed on
[Cosmonic Desktop](https://cosmonic.com/docs/desktop). Read the template's
`skills/building-mcp-servers/SKILL.md` first; this document is the
repo-specific delta. `scripts/new-server.sh <name>` applies all of it.

## Naming

- Directory = crate name = workload name = DNS label:
  `<vendor-optional>-<name>-mcp`, lowercase kebab-case (`notion-mcp`,
  `atlassian-jira-mcp`, `official-filesystem-mcp`). The wasm is
  `target/wasm32-wasip2/release/<name_with_underscores>.wasm`.
- Reachable at `http://<name>.localhost:8200/`. The manifest's ingress is
  `host: "<name>.localhost"` — no port in the host, no `.cosmonic.sh` variant.
  `MCP_ALLOWED_HOSTS` is the same `<name>.localhost` (matches any port).
- The served skill is `skill://<name>/SKILL.md` (URI name = package name).

## Layout

```
<name>/
├── .cargo/config.toml     # wasm32-wasip2 default target (unchanged)
├── .wash/config.yaml      # build + the `workload:` block Desktop uses for the
│                          #   dev loop / promote draft: env, allowedHosts,
│                          #   hostInterfaces (for postgres etc.)
├── deploy/workload.yaml   # THE manifest (there is no root workload.yaml)
├── docs/                  # optional; auth.md carried from the template
├── scripts/e2e.sh         # sources ../../scripts/mcp_e2e_lib.sh
├── skills/server/         # SKILL.md + references/ served over MCP
├── src/                   # lib.rs bridge.rs discovery.rs server.rs skills.rs telemetry.rs (+ your client module)
├── Cargo.toml Cargo.lock LICENSE README.md .gitignore
```

Keep `lib.rs`, `bridge.rs`, `discovery.rs`, `skills.rs`, `telemetry.rs` as the
template ships them unless the server genuinely needs a change (postgres
needs a bridge extension; say so in the README). Put the upstream client in
its own module (`src/notion.rs`, `src/jira.rs`, …) and keep `server.rs` to
tool definitions + result rendering, like the template's shape.

## Configuration: named config vs secret references

Two kinds of settings, both read from the environment with `std::env::var`:

| Kind | Examples | Where it lives in `deploy/workload.yaml` |
|---|---|---|
| **Named config** (not secret) | site URL, workspace/team id, allowed folders, region, upstream base URL | `localResources.environment.config` (literal) — or `configFrom: [{name}]` for a Desktop named config |
| **Secret reference** | API tokens, passwords, access keys | `localResources.environment.secretFrom: [{name: <ref>}]` — **never** a literal value |

Rules:

- Env var names are `UPPER_SNAKE`, prefixed by the product: `NOTION_TOKEN`,
  `JIRA_SITE`, `SLACK_BOT_TOKEN`, `SUPABASE_ACCESS_TOKEN`, `OBSIDIAN_API_KEY`,
  `DOCKER_HOST`, `AWS_ACCESS_KEY_ID`, `FS_ALLOWED_DIRS`.
- Secret **reference names** are `<name>-<purpose>`: `notion-mcp-token`,
  `atlassian-jira-mcp-api-token`, `aws-cloud-mcp-secret-access-key`. The ref
  carries the env var name (`env:`), so the manifest only says `secretFrom:
  [{name: notion-mcp-token}]`.
- Register a ref with the `cosmonic_set_secret` MCP tool or the daemon API:
  ```console
  $ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs \
      -H 'Content-Type: application/json' \
      -d '{"name":"notion-mcp-token","uri":"keychain://cosmonic/notion-mcp-token","env":"NOTION_TOKEN","value":"<token>"}'
  ```
  (`SOCK` is `/run/user/<uid>/cosmonic/cosmonicd.sock` on Linux,
  `~/Library/Application Support/Cosmonic/cosmonicd.sock` on macOS.)
  Backends: `keychain://cosmonic/<name>` (value stored write-only),
  `env://VAR`, `op://vault/item/field`, `aws-sm://region/secret-id`.
- A **missing** secret must produce a distinct, actionable tool error
  ("`NOTION_TOKEN` is not set. Create an internal integration at … and
  register it as the `notion-mcp-token` secret"), never a crash or a raw 401.
  An **invalid** one surfaces the upstream's message plus the same hint.
- Every upstream base URL is overridable by env (`NOTION_BASE_URL`, …),
  defaulting to the real host, so the e2e can point at a local fixture.
- Never compile a credential into the component; never commit one to any
  manifest or README.

## Credentials: self-describing, testable (Layer 1)

Every server that needs a credential makes the setup discoverable and
verifiable without reading a README:

1. **`GET /` carries a `credentials` block** (presence only, never values):

   ```json
   "credentials": [{
     "ref": "notion-mcp-token", "env": "NOTION_TOKEN", "kind": "bearer-token",
     "status": "configured",
     "description": "Internal integration secret; share each page/database with the integration",
     "obtainUrl": "https://www.notion.so/profile/integrations",
     "scopes": ["read content", "update content", "insert content"],
     "validate": "check_auth"
   }]
   ```

   `status` is `configured` when the env var is set and non-empty, else
   `missing`. Add the block by extending `discovery::document` with a
   `credentials()` function in the server crate; keep the field names above so
   Desktop and agents can rely on them. The same list goes into
   `deploy/workload.yaml` as the annotation
   `desktop.cosmonic.com/credentials` (a JSON array with `ref`, `env`,
   `description`, `obtainUrl`, `scopes`) so a tool can show it *before* the
   workload runs.
2. **A `check_auth` tool** (no arguments) that calls the cheapest identity
   endpoint the upstream has (`GET /users/me`, `auth.test`, `myself`, …) and
   returns `structuredContent` with `status: ok|missing|invalid|insufficient`,
   the identity (account, workspace/site, bot name), granted scopes or
   permissions where the API reports them, expiry when known, and a
   `remediation` string naming the ref, the env var, the `obtainUrl`, and the
   exact `cosmonic_set_secret` call. The skill tells agents to call it first
   and never to retry a `missing`/`invalid` result.
3. **Deep links that pre-fill the vendor side** wherever the vendor supports
   it (Slack: a `https://api.slack.com/apps?new_app=1&manifest_yaml=…` link
   that creates the app with the exact scopes; Notion: the new-integration
   page; Atlassian: the API-token page; Supabase: the tokens page). Put them
   in the README, the skill, `check_auth` remediation, and `obtainUrl`.
4. **One ref per account, not per server**: `atlassian-api-token` serves Jira
   and Confluence; a Google refresh token serves every Google server.
5. **The value never transits an agent when a UI exists**: READMEs and skills
   say to paste it in Desktop → Secrets (or use `env://` / `op://` refs) and
   then re-run `check_auth`; `cosmonic_set_secret` is the fallback.
6. **Annotation rules** (Desktop parses `desktop.cosmonic.com/credentials`
   leniently and treats it as untrusted display metadata, never as
   authorization — see the Desktop design doc `docs/CREDENTIALS-DESIGN.md`,
   §6.2): a JSON array of at most 16 entries; `kind` is `bearer-token`,
   `api-key`, `basic` or `connection`; `ref` (slug ≤ 63) is prompted for only
   when a component or hostInterface `secretFrom` actually names it; `env`
   ≤ 64 chars of `[A-Z0-9_]`; `description` ≤ 200 chars; `obtainUrl` https
   only, ≤ 512 chars; `scopes` ≤ 32 strings; `validate` is `check_auth`.
   For a `connection` entry (Layer 3), `label` must equal the
   `cosmonic:credentials` hostInterface `name`, and `provider` / `scopes`
   must agree with the binding.
7. **Never echo a token.** No tool result, log line, discovery document or
   `check_auth` payload may contain a credential value or an access token; on
   a signature/auth failure return the upstream's error code and your
   remediation, never the request you signed. When Layer 3 tokens arrive,
   compare your needs against `bound-scopes`, not `scopes` (the token may
   carry the account's wider grant), and keep the token in a type with no
   `Display`/`Debug`/`Serialize`.

## Outbound policy (`allowedHosts`)

`allowedHosts` is deny-all when empty. List every host a tool dials in **both**
`deploy/workload.yaml` and `.wash/config.yaml`'s `workload.allowedHosts`.
Entries are `host` or `host:port`; a scheme pins it (`https://api.notion.com`).

### Services on the developer's own machine (Obsidian, Docker, …)

A workload reaches the machine's loopback only through the sentinel name
`host.wasmcloud.internal` (never `localhost`/`127.0.0.1`, which mean the
workload's own virtual network), and only when three things line up:

1. `allowedHosts: ["host.wasmcloud.internal:27123"]` (the HTTP allow-list),
2. `allowedHostLoopbackPorts: ["27123"]` on the component (`"PORT"` or
   `"PORT/tcp"`; no ranges/wildcards), and
3. the host-level door: Desktop Settings → Security → *allow host loopback*
   (`PUT /v1/egress {"allow_host_loopback": true}`), default **off**.

So a server that talks to a local app defaults its base URL to
`http://host.wasmcloud.internal:<port>`, documents the three-step grant, and
its e2e (which runs under wasmtime, where `127.0.0.1` is the real loopback)
overrides the base URL to its fixture.

## Host filesystem (`spec.volumes`)

A component sees no host filesystem unless the manifest mounts one:

```yaml
spec:
  volumes:
    - name: docs
      hostPath: { path: /home/me/Documents }   # must already exist; not created
  components:
    - name: official-filesystem-mcp
      localResources:
        volumeMounts:
          - { name: docs, mountPath: /docs, readOnly: false }
```

Inside the component the directory is the WASI preopen `/docs`
(`std::fs` works). The e2e reproduces this with `wasmtime serve --dir
host_dir::/docs`. Desktop recommends host paths under
`~/.local/share/cosmonic/volumes/<workload>` (Linux) /
`~/Library/Application Support/Cosmonic/volumes/<workload>` (macOS).

## Host capabilities (`wasmcloud:postgres`)

Desktop registers `wasmcloud:postgres@0.2.0` (async, WASI p3) in
multiplex-only mode: a workload declares a **named** host interface carrying
its own connection URL, and the component imports the interface under the same
name (component-model `(implements ..)`):

```yaml
spec:
  hostInterfaces:
    - namespace: wasmcloud
      package: postgres
      version: "0.2.0"
      name: db                       # the import label
      interfaces: [types, query, prepared]
      secretFrom: [{ name: postgres-mcp-url }]   # provides `url`
```

```wit
world postgres-mcp {
  import wasmcloud:postgres/types@0.2.0;
  import db: wasmcloud:postgres/query@0.2.0;
}
```

The `url` config key (`postgres://user:pass@host:5432/db?sslmode=…`) comes from
a secret ref whose `env` is literally `url` — hostInterfaces accept
`configFrom`/`secretFrom` exactly like component environments. Host calls are
WASI p3 futures, so they must run in component-model context: extend
`bridge.rs` with a generic host-call job (a boxed local future the driver
polls between tokio turns) rather than awaiting them from tool code.

## Skills over MCP (mandatory)

`skills/server/SKILL.md` is served at `skill://<name>/SKILL.md`. Frontmatter
`name` = package name; `description` = one line of *when to use this server*.
The body is operating knowledge — which tool first, how to sequence, the error
catalogue with what to do about each, upstream quirks (rate limits, id
formats, pagination cursors, block-size limits). Put long tables in
`skills/server/references/*.md` and list every file in `src/skills.rs`
`SKILLS`. The e2e fails a SKILL.md that still carries the template's
boilerplate comment.

## Default route

`GET /` and `GET /health` return the discovery document (`src/discovery.rs`,
unchanged): status, server name/version/description, spec version, endpoints,
tool names, skills. Keep `Cargo.toml` `description` meaningful — it is what
`GET /` shows.

## Tests

`scripts/e2e.sh` sources `../../scripts/mcp_e2e_lib.sh` and must stay green:

- `framework_tests <tools…>` (protocol, spec enforcement, robustness,
  8-way concurrency on `FIRST_TOOL_*` — point that at an outbound tool),
- `discovery_tests <a-tool>`, `skills_tests <name> <references/…>`,
- your tool cases: happy path, malformed params, at least one adversarial case
  per tool (boundary values, huge input, unicode, injection-shaped strings),
  pagination/limit clamping, upstream error mapping, and the **missing
  secret** path on the guard instance (started without the secret),
- `guard_tests`, `mcp_harness_report`.

Upstream calls go to a **threaded** Python fixture (`ThreadingHTTPServer`)
started by the script and selected with the base-URL override, so the suite is
hermetic. Where a live upstream is keyless and cheap, one live smoke case may
be added behind `E2E_LIVE=1`.

`cargo fmt --check`, `cargo clippy --all-features -- -D warnings`, and
`cargo build --release` are the compile gates. `cargo test` is not used (wasm
target).

## README

What it does; a tool table (name, params, output); the skill URI; the config
table (env var, named config vs secret ref, default, required?); the exact
secret registration command; `allowedHosts` (and loopback / volume grants if
any); build + test commands; deploy on Desktop (`deploy/workload.yaml`, the
promote flow, the `<name>.localhost:8200` URL); example curl calls including
`GET /`; how to connect a client; what was borrowed from which upstream
project and its license.

The client registration lines every README carries (the server is stateless,
so any streamable-HTTP client works):

```console
$ claude mcp add --transport http <name> http://<name>.localhost:8200/
```

Claude Desktop (`claude_desktop_config.json`):
`{"mcpServers":{"<name>":{"type":"http","url":"http://<name>.localhost:8200/"}}}`.
Cosmonic Desktop also detects the `mcp.ai/*` labels and can register the
server into detected coding agents from its UI.

## Deploy on Cosmonic Desktop (local build)

```console
$ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock         # Linux
$ cd mcp-servers/<name> && cargo build --release
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects \
    -H 'Content-Type: application/json' -d "{\"path\":\"$PWD\"}"   # id = dir name
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects/<name>/promote \
    -H 'Content-Type: application/json' -d '{"ref":"<name>:0.1.0"}'
# → {"image":"oci.localhost:8200/apps/<name>:0.1.0@sha256:…", "workload": {…}}
```

Then apply `deploy/workload.yaml` with `image` replaced by that pinned ref
(`cosmonic_apply_workload`, or `POST /v1/workloads` with the JSON/YAML body),
after registering any secret refs it names. Never apply a name that already
exists unless you mean to replace it (`GET /v1/workloads` first). Verify:

```console
$ curl -s http://<name>.localhost:8200/ | jq .status
$ curl -s -X POST http://<name>.localhost:8200/ -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/list' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
```

## Licensing

Everything here is Apache-2.0 (a `LICENSE` file per directory). Borrow only
from MIT/Apache/BSD sources and say what came from where in the README.
