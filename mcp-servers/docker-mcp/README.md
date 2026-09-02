# docker-mcp

An MCP server for the user's local **Docker Engine or podman daemon**, running
as a sandboxed WebAssembly component on
[Cosmonic Desktop](https://cosmonic.com/docs/desktop). It speaks the Docker
Engine REST API (v1.44 by default; podman's compat API is the same surface)
over plain HTTP and lets an agent list, inspect, log and stat containers,
control their lifecycle (opt-in), list/inspect/pull/remove images, list
networks and volumes, and read disk usage.

Built from [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28 (stateless streamable HTTP), exports
`wasi:http/handler@0.3.0`, serves a discovery document on `GET /` and
`GET /health`, and publishes its playbook as a skill at
`skill://docker-mcp/SKILL.md`.

Reachable on Cosmonic Desktop at <http://docker-mcp.localhost:8200/>.

## How it reaches the daemon

The daemon's unix socket is invisible to a workload, so the server dials a
**plain-HTTP TCP listener on 127.0.0.1:2375** through the Desktop loopback
sentinel `host.wasmcloud.internal:2375`. The Engine API has no
authentication of its own — the listener plus the Desktop grants *are* the
access control — which is why every write tool is refused until the operator
sets `DOCKER_READ_ONLY=false`. Both Docker and podman warn that a TCP
listener without mTLS is root-equivalent for any local process: bind it to
127.0.0.1 only, keep writes off unless lifecycle control is wanted, and
prefer the socket proxy with `POST=0`.

TLS daemons with client certificates (`tcp://:2376`) and `unix://`/`ssh://`
hosts are unsupported (no client-cert auth, no sockets, webpki roots only).

## Tools

| Tool | Params | Output | Gated? |
|---|---|---|---|
| `version` | none | `engine` docker/podman, `version`, `api_version`, `min_api_version`, `api_version_ok`, os/arch/kernel, `read_only`, `hint` | no |
| `info` | `raw?` | trimmed `/info`: ServerVersion, Name, OS, arch, NCPU, MemTotal, container/image counts, drivers, cgroup, rootless, Runtimes, Warnings | no |
| `list_containers` | `all?` (default true), `limit?` 1..500, `size?`, `filters?` | `containers[{id, names, image, command, state, status, created, ports, labels}]` | no |
| `inspect_container` | `id`, `size?`, `include_env_values?`, `raw?` | trimmed state/config/host_config/mounts/network; secret-looking Env values redacted | no |
| `container_logs` | `id`, `tail?` 1..5000, `since?`, `until?`, `timestamps?`, `stdout?`, `stderr?`, `max_bytes?` | demultiplexed `stdout`, `stderr`, `combined`, `truncated`, `tty` | no |
| `container_stats` | `id`, `raw?` | `cpu_percent`, memory usage/limit/percent, net rx/tx, block read/write, `pids` | no |
| `run_container` | `image`, `name?`, `cmd?`, `entrypoint?`, `env?`, `labels?`, `working_dir?`, `user?`, `ports?`, `restart_policy?`, `memory_bytes?`, `cpus?`, `platform?`, `wait?`, `pull_if_missing?` | `id`, `started`, `warnings`; with wait: `exit_code`, `stdout`, `stderr` | `DOCKER_READ_ONLY` |
| `start_container` | `id` | `changed` (304 = already running) | `DOCKER_READ_ONLY` |
| `stop_container` | `id`, `timeout?` 0..300, `signal?` | `changed`, `timeout` | `DOCKER_READ_ONLY` |
| `restart_container` | `id`, `timeout?`, `signal?` | `changed` | `DOCKER_READ_ONLY` |
| `kill_container` | `id`, `signal?` (SIGKILL) | `signal`, `changed` | `DOCKER_READ_ONLY` |
| `remove_container` | `id`, `force?`, `volumes?` | `removed` | `DOCKER_READ_ONLY` |
| `list_images` | `all?`, `digests?`, `filters?`, `limit?` 1..1000 | `images[{id, repo_tags, created, size, size_human, containers, dangling}]` newest first | no |
| `inspect_image` | `name`, `include_env_values?`, `raw?` | tags, digests, size, os/arch, trimmed Config, `rootfs_layers` | no |
| `pull_image` | `image`, `platform?`, `raw_progress?` | `layers{id: status}`, `digest`, `status[]`, `defaulted_to_latest` | `DOCKER_READ_ONLY` |
| `remove_image` | `name`, `force?`, `noprune?` | `untagged[]`, `deleted[]` | `DOCKER_READ_ONLY` |
| `list_networks` | `filters?` | `networks[{id, name, driver, scope, internal, attachable, ipam[]}]` | no |
| `list_volumes` | `filters?` | `volumes[{name, driver, mountpoint, scope, created_at}]`, `warnings` | no |
| `system_df` | `detail?` | per-category totals/active/size/reclaimable, `layers_size`; detail lists items | no |

Every success carries `structuredContent` plus a readable text block; upstream
and policy failures are `isError: true` with an actionable message (the error
catalogue lives in the skill). Full argument tables:
`skill://docker-mcp/references/TOOLS.md`.

Design choices worth knowing: `list_containers` defaults `all=true`; non-TTY
logs are demultiplexed from the Engine's 8-byte-framed stream (TTY logs are
raw); `container_stats` uses `stream=false` so `cpu_percent` matches `docker
stats`; `run_container` has **no** privileged/cap_add/binds/host-network
fields on purpose, always sets `Tty=false`, binds published ports to
127.0.0.1 unless a host ip is given, and `wait=true` uses the daemon's
blocking wait endpoint (bounded by `MCP_OUTBOUND_TIMEOUT_MS`); `pull_image`
substitutes `latest` for a missing tag (an empty tag pulls every tag) and
turns a mid-stream `{error}` line into an error; `inspect_*` redact Env
values whose key looks like a credential unless `include_env_values=true`.

## Skill

- `skill://index.json` — catalog
- `skill://docker-mcp/SKILL.md` — call `version` first, ids/filters/logs/stats
  facts, write-tool semantics, Docker-vs-podman differences, the error catalogue
- `skill://docker-mcp/references/TOOLS.md` — argument/output tables, filter keys, validation
- `skill://docker-mcp/references/ERRORS.md` — every distinguishable failure and what to do
- `skill://docker-mcp/references/SETUP.md` — the daemon-side TCP listener / proxy recipes and the grants

## Configuration

| Env var | Kind | Default | Required |
|---|---|---|---|
| `DOCKER_HOST` | named config | `http://host.wasmcloud.internal:2375` | no (the e2e points it at a fixture; `http://`/`https://` only) |
| `DOCKER_API_VERSION` | named config | `v1.44` | no — must satisfy MinAPIVersion <= value <= ApiVersion from `version`; Docker 29+ needs >= v1.44, podman ignores it |
| `DOCKER_READ_ONLY` | named config | `true` | no — anything but `false`/`0`/`no`/`off` keeps run/start/stop/restart/kill/remove_container, pull_image, remove_image refused |
| `DOCKER_REGISTRY_AUTH` | secret ref `docker-mcp-registry-auth` | — | no — only for private-image pulls; JSON `{"username","password","serveraddress"}` (or `{"identitytoken"}`), raw or base64 in any variant (the server re-encodes it as **padded** base64url, which is what dockerd/podman's Go `base64.URLEncoding` decoder requires); sent only as `X-Registry-Auth` on `POST /images/create`; `version` reports `registry_auth_valid` |
| `MCP_OUTBOUND_TIMEOUT_MS` | named config | `120000` (template default 30000) | no — pull is synchronous and Docker cancels it on disconnect; also bounds `wait` and stop grace periods |
| `MCP_OUTBOUND_MAX_BYTES` | named config | `4194304` | no — raise for `raw=true`/`system_df` on daemons with thousands of images |
| `MCP_ALLOWED_HOSTS` | named config | `docker-mcp.localhost` | yes (DNS-rebinding guard = ingress host) |
| `RUST_LOG` | named config | `info` | no |

`GET /` carries a `credentials` block (presence only, never values) naming
the optional registry ref, its env var, `"required": false`, `"validate":
"version"` (the `version` tool shape-checks the secret without dialling a
registry and reports `registry_auth_valid` / `registry_auth_error`), and
where to get a token.

### Optional: register the registry credential

Only for private images. Create a read-only token (Docker Hub:
<https://app.docker.com/settings/personal-access-tokens>; GHCR: a PAT with
`read:packages`), then paste the JSON in Cosmonic Desktop -> Secrets as
`docker-mcp-registry-auth` (env `DOCKER_REGISTRY_AUTH`), or:

```console
$ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock     # Linux; macOS: "$HOME/Library/Application Support/Cosmonic/cosmonicd.sock"
$ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs \
    -H 'Content-Type: application/json' \
    -d '{"name":"docker-mcp-registry-auth","uri":"keychain://cosmonic/docker-mcp-registry-auth","env":"DOCKER_REGISTRY_AUTH","value":"{\"username\":\"<user>\",\"password\":\"<token>\",\"serveraddress\":\"index.docker.io\"}"}'
```

or with the MCP tool: `cosmonic_set_secret name=docker-mcp-registry-auth
uri=keychain://cosmonic/docker-mcp-registry-auth env=DOCKER_REGISTRY_AUTH
value='{...}'`. Then uncomment `secretFrom: [{name: docker-mcp-registry-auth}]`
in `deploy/workload.yaml` and re-apply. A placeholder or malformed value is
refused by `pull_image` as "misconfigured" before anything is sent; a wrong
one surfaces the registry's denial plus the same instructions.

## Outbound policy and the loopback grant

`allowedHosts` lists exactly one host: `host.wasmcloud.internal:2375` (the
daemon's TCP listener on this machine). Reaching a service on the developer's
own machine needs three things, in both `deploy/workload.yaml` and Desktop:

1. `localResources.allowedHosts: ["host.wasmcloud.internal:2375"]`
2. `localResources.allowedHostLoopbackPorts: ["2375"]`
3. Desktop Settings -> Security -> **allow host loopback** (default off;
   `PUT /v1/egress {"allow_host_loopback": true}`)

Until all three line up, every tool returns `could not reach the Docker
daemon ...` with that checklist. With the Security toggle off, Desktop 0.5.27
reports it as `wasi:http error: ErrorCode::DnsError(... "address not
available")` for `host.wasmcloud.internal` — the sentinel name does not
resolve until the door is open, so that DNS error is the expected first-run
state, not a typo. `connection refused` means nothing listens on
127.0.0.1:2375. No volumes and no other host interfaces are used.

### Expose the daemon on 127.0.0.1:2375

Pick one (details and the security notes in the skill's `references/SETUP.md`):

- **podman** (rootless is fine and the safer choice):
  `podman system service --time 0 tcp://127.0.0.1:2375`
- **Docker Engine on Linux**: a systemd drop-in with
  `ExecStart=/usr/bin/dockerd -H fd:// -H tcp://127.0.0.1:2375` **or**
  `daemon.json` `"hosts"` — never both, they conflict and dockerd will not start.
- **Docker Desktop (macOS/Windows)** or any daemon, read-only by default:
  ```console
  $ docker run -d --name dockerproxy --restart unless-stopped \
      -v /var/run/docker.sock:/var/run/docker.sock -p 127.0.0.1:2375:2375 \
      -e CONTAINERS=1 -e IMAGES=1 -e INFO=1 -e NETWORKS=1 -e VOLUMES=1 -e SYSTEM=1 \
      tecnativa/docker-socket-proxy
  ```
  add `-e POST=1 -e ALLOW_START=1 -e ALLOW_STOP=1 -e ALLOW_RESTARTS=1` only
  with `DOCKER_READ_ONLY=false`. Or `socat TCP-LISTEN:2375,bind=127.0.0.1,fork
  UNIX-CONNECT:/var/run/docker.sock`.

Verify on the host: `curl http://127.0.0.1:2375/_ping` prints `OK`.

## Build and test

```console
$ cargo build --release          # target/wasm32-wasip2/release/docker_mcp.wasm
$ cargo fmt --check && cargo clippy --all-features -- -D warnings
$ scripts/e2e.sh                 # hermetic: scripts/fixture.py impersonates the Engine API
$ E2E_LIVE=1 scripts/e2e.sh      # + live cases against http://127.0.0.1:2375 (DOCKER_LIVE_HOST to override)
```

The hermetic suite runs six wasmtime instances (writes enabled with a
registry credential; the read-only default against an unreachable host; no
credential with a 5 s deadline at `RUST_LOG=debug`; `DOCKER_API_VERSION=v1.43`
with a placeholder credential; a pre-encoded standard-base64 unpadded
credential; a short-lived one whose `DOCKER_HOST` does not resolve, for the
DNS-failure hint) and asserts on the fixture's request log: the `/v1.44/` prefix,
percent-encoded JSON `filters`, clamps (`limit=500`, `tail=5000`, `t=25`),
the **padded** base64url `X-Registry-Auth` header (the fixture, like
dockerd/podman, answers 400 `failed to parse "X-Registry-Auth"` to anything
else), the create body (Tty false, no Privileged, 127.0.0.1 port bindings),
log demultiplexing including a truncated final frame and a 300 KiB frame,
stats arithmetic, every mapped status code (304/400/403/404/409/429/500 and
podman's 500 shapes), the version-window fallback, the outbound timeout, the
read-only gate, and log hygiene: after `run_container` with a secret-bearing
`env` (INFO and DEBUG levels) the wasmtime stderr logs are grepped for the
value and for any serialised params struct. `E2E_LIVE=1` adds a bogus-credential
pull against the real daemon and asserts the registry, not a header parser,
rejected it. `cargo test` is not used (wasm target).

## Deploy on Cosmonic Desktop

```console
$ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock
$ cd mcp-servers/docker-mcp && cargo build --release
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects \
    -H 'Content-Type: application/json' -d "{\"path\":\"$PWD\"}"
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects/docker-mcp/promote \
    -H 'Content-Type: application/json' -d '{"ref":"docker-mcp:0.1.0","rebuild":true}'
# → {"image":"oci.localhost:8200/apps/docker-mcp:0.1.0@sha256:…", ...}
```

Then apply [`deploy/workload.yaml`](deploy/workload.yaml) with `image`
replaced by that pinned reference (`cosmonic_apply_workload`, or `POST
/v1/workloads`), flip Settings -> Security -> allow host loopback, and verify:

```console
$ curl -s http://docker-mcp.localhost:8200/ | jq .status
"ok"
$ curl -s -X POST http://docker-mcp.localhost:8200/ -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/list' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
```

Call a tool (routes by Host header; `version` is the connectivity check):

```console
$ curl -s -X POST http://docker-mcp.localhost:8200/ -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: version' \
    -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"version","arguments":{},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
data: {"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","content":[{"type":"text","text":"podman 5.8.2 (API 1.44, min 1.24), linux arm64 5.14.0-687.42.1.el9_8.aarch64, configured v1.44 -> ok"}],"structuredContent":{"engine":"podman","api_version_ok":true,...},"isError":false}}
```

With the loopback door still off the same call answers `isError: true` with
`could not reach the Docker daemon for /v1.44/version: wasi:http error:
ErrorCode::DnsError(DnsErrorPayload { rcode: Some("address not available"), ...
})` followed by the three-step checklist ending in `(3) Cosmonic Desktop
Settings -> Security -> 'allow host loopback' is on` — the expected state on a
fresh machine.

### Connect a client

```console
$ claude mcp add --transport http docker-mcp http://docker-mcp.localhost:8200/
```

Claude Desktop (`claude_desktop_config.json`):
`{"mcpServers":{"docker-mcp":{"type":"http","url":"http://docker-mcp.localhost:8200/"}}}`.
Cosmonic Desktop also detects the `mcp.ai/*` labels and can register the
server into detected coding agents from its UI.

## Borrowed from

- [moby/moby `api/swagger.yaml`](https://github.com/moby/moby/blob/master/api/swagger.yaml)
  (Apache-2.0): endpoint list, query semantics, status codes, the
  attach/logs frame format (`[type, 0, 0, 0, u32 BE length][payload]`) and
  the `X-Registry-Auth` shape (padded base64url JSON, decoded by
  `registry.DecodeAuthConfig` with Go's `base64.URLEncoding`).
- [Tecnativa/docker-socket-proxy](https://github.com/Tecnativa/docker-socket-proxy)
  (Apache-2.0): the read-only exposure recipe and its section flags.
- [QuantGeekDev/docker-mcp](https://github.com/QuantGeekDev/docker-mcp)
  (MIT): tool naming ideas (list/create/run/logs). No code (it shells out to
  the CLI).
- [ckreiling/mcp-server-docker](https://github.com/ckreiling/mcp-server-docker)
  (GPL-3.0): **ideas only, no code** — the tool surface and the rule of
  refusing privileged/cap-add/host mounts and treating Env as secret-bearing.
- Why an Engine-API server: Docker's official
  [docker/mcp-gateway](https://github.com/docker/mcp-gateway) (MIT) runs
  other MCP servers in containers and
  [docker/hub-mcp](https://github.com/docker/hub-mcp) (Apache-2.0) wraps the
  Hub registry API; neither exposes the Engine.

This port is Apache-2.0 (see `LICENSE`).

## Known limitations

- Requires a plain-HTTP TCP listener on the daemon side (a real setup step on
  Docker Desktop) and the global Security loopback toggle; no unix socket,
  ssh, or mTLS.
- `pull_image` is one bounded exchange (`MCP_OUTBOUND_TIMEOUT_MS`, 120 s):
  Docker cancels the pull when the connection closes, so multi-GB images on
  slow links should be pre-pulled with the CLI. Huge progress streams can hit
  the 4 MiB body cap.
- No streaming: `follow=true` logs, `stream=true` stats, attach and exec are
  not exposed. `run_container wait=true` only suits short commands.
- Read-only for networks and volumes (no create/remove/connect); no
  build/commit/push/export; no swarm.
- Docker vs podman divergence is handled by message matching (409 vs 500,
  304 vs 204 on start, `stopped` state, 403 vs 404 on pull, no BuildCache in
  df); the live suite here covers podman 5.8, Docker shapes are covered by
  the fixture.
- Env redaction is a key-name heuristic (`pass|secret|token|key|credential|auth`);
  `raw=true` output is not redacted.
