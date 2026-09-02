---
name: docker-mcp
description: Inspect and control the user's local Docker Engine or podman daemon — list/inspect/log/stat containers, run/start/stop/restart/kill/remove them (when writes are enabled), list/inspect/pull/remove images, list networks and volumes, disk usage. Use when a task mentions Docker, podman, containers, images, a Dockerfile's runtime, container logs or ports, and to interpret this server's errors.
---

# Using the docker-mcp MCP server

This server is a sandboxed WebAssembly component on Cosmonic Desktop that
speaks the **Docker Engine REST API** (v1.44 by default; podman's compat API
is the same surface) over plain HTTP to the daemon on the user's machine. It
is **stateless**: nothing you set on one call carries into the next. It
reaches exactly one daemon — whatever `DOCKER_HOST` points at (default
`http://host.wasmcloud.internal:2375`).

Full argument tables: [references/TOOLS.md](references/TOOLS.md). Error
catalogue: [references/ERRORS.md](references/ERRORS.md). Daemon-side setup
(TCP listener, socket proxy, grants): [references/SETUP.md](references/SETUP.md).

## Start here

1. **Call `version` first.** It proves the daemon is reachable and reports
   `engine` (`docker` | `podman`), `api_version`, `min_api_version` and
   `api_version_ok` — whether the configured `DOCKER_API_VERSION` sits inside
   `[MinAPIVersion, ApiVersion]`. If it is outside, every other tool fails
   with HTTP 400 until the operator changes the named config; do not retry.
   A transport error here (`could not reach the Docker daemon`) is a grant
   or listener problem, also permanent until a human acts.
2. `info` for the daemon's shape (rootless? cgroup v2? log driver? counts).
3. Orient with `list_containers` (all=true by default, so exited ones show),
   `list_images`, `list_networks`, `list_volumes`, then drill in with
   `inspect_*`, `container_logs`, `container_stats`.
4. Write tools (`run_container`, `start/stop/restart/kill/remove_container`,
   `pull_image`, `remove_image`) are **refused by default**
   (`DOCKER_READ_ONLY=true`). The refusal names the config to flip; nothing
   is sent to the daemon. `version` reports `read_only`.

## Transport facts you cannot infer

- The daemon's unix socket is invisible to a workload. The server dials a TCP
  listener (`dockerd -H tcp://127.0.0.1:2375`, `podman system service --time 0
  tcp://127.0.0.1:2375`, or a docker-socket-proxy) through
  `host.wasmcloud.internal:2375`, which needs three Desktop grants:
  `allowedHosts ["host.wasmcloud.internal:2375"]`, `allowedHostLoopbackPorts
  ["2375"]`, and Settings -> Security -> *allow host loopback*.
  `HttpRequestDenied` / `denied` / `connection refused` / DNS failures all
  mean one of the four legs is missing — see SETUP.md. With the Security
  toggle off, Desktop reports a **DNS error** (`DnsError ... address not
  available`) for `host.wasmcloud.internal` — that is the expected first-run
  error, not a typo in `DOCKER_HOST`.
- `DOCKER_HOST=localhost` or `127.0.0.1` always fails from a workload (that
  is the workload's own network). TLS daemons with client certificates
  (tcp://:2376) are unsupported: the sandbox cannot present a client cert and
  self-signed CAs fail.
- Every path carries the API version prefix (`/v1.44/...`). Docker 29+
  rejects anything below 1.44 (`client version 1.43 is too old`), any daemon
  rejects a version above its `ApiVersion`. **podman ignores the prefix
  entirely**, so `api_version_ok=false` on podman is harmless; on Docker it is
  fatal.

## Ids, references, filters

- Use the **12-char short id** from the listings, or the name. Names in the
  raw API carry a leading `/`; the tools strip it. Ids are validated
  `^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$` before anything is sent.
- Image references are `name[:tag|@sha256:digest]` or an id. `pull_image`
  without a tag/digest pulls `latest` and says so (`defaulted_to_latest`),
  because an empty tag would make the daemon pull **every** tag. Always give
  an explicit tag.
- Filters are a JSON object of string arrays, never `key=value`:
  `{"status":["running"],"label":["app=web"]}`; a bare string is accepted for
  one value. Unknown keys are refused client-side with the allowed list.
  `ancestor` matches an image name or id; `reference` on images takes globs
  (`alpine:*`). Container `status` values: created, restarting, running,
  removing, paused, exited, dead (podman also reports `stopped`).
- `limit` is capped at 500 (containers) / 1000 (images, applied after sorting
  newest first); `tail` at 5000; negative or `all` values are never sent
  (podman crashes with 500 `makechan: size out of range` on a negative tail).

## Logs

- Non-TTY containers' logs are **not plain text** upstream: a stream of
  8-byte-framed stdout/stderr chunks. `container_logs` inspects `Config.Tty`
  first and demultiplexes, returning `stdout`, `stderr` and `combined`
  (arrival order, each line prefixed `out|` / `err|` when both streams carry
  data). TTY containers return raw text with CRLF normalised.
- Output is bounded by `max_bytes` (1 KiB..1 MiB, default 64 KiB) keeping the
  **newest** bytes; `truncated: true` with `total_bytes` says how much was
  dropped. Lower `tail` or add `since` rather than raising `max_bytes` first.
- Only json-file/journald log drivers can be read; other drivers answer 500
  `configured logging driver does not support reading` — permanent.
- `partial_frame: true` means the daemon's stream ended inside a frame; the
  text is still usable, just cut.

## Stats

- `container_stats` uses `stream=false` (one sample that already carries the
  previous CPU sample). `cpu_percent = cpu_delta / system_delta * online_cpus
  * 100`, exactly like `docker stats`; memory excludes the page cache
  (`inactive_file`). One-shot mode is deliberately not used: it zeroes CPU%.
- On Docker a stopped container returns zeros; on podman it is HTTP 500
  `container is stopped`. Check `state` in `list_containers` first.

## Writes (all refused when `DOCKER_READ_ONLY=true`)

- `run_container` = create + start (`docker run -d`). The schema has **no**
  privileged / cap_add / binds / volumes / host-network / devices on purpose;
  Tty is always false (so logs demux) and AutoRemove false (so logs remain
  readable). Published ports bind to **127.0.0.1** unless a host ip is given;
  host port `0` lets the daemon choose. `wait=true` blocks on the daemon's
  wait endpoint until exit (bounded by the outbound deadline, 120 s in the
  manifest) and returns `exit_code` plus the last 200 log lines — only for
  short commands; a timeout leaves the container running and says so.
  `pull_if_missing=true` pulls once on `No such image` and retries.
- 304 on start/stop means "already in that state": reported as
  `changed=false`, not an error. podman answers 204 to start on an exited
  container (it restarts it).
- `stop_container`/`restart_container` hold the request open for `timeout`
  seconds; the tool caps it to the outbound deadline minus 5 s and reports
  `timeout_note` when it had to.
- `kill_container` only works on running containers (409 otherwise).
- `remove_container` of a running container without `force` is Docker 409 /
  podman 500 `container state improper`; both map to "stop it or force=true".
- `pull_image` is synchronous: bounded by `MCP_OUTBOUND_TIMEOUT_MS`. On Docker
  a closed connection **cancels** the pull, so a timeout on a multi-GB image
  leaves nothing behind; pre-pull big images with the CLI or raise the
  deadline. Failures arrive two ways: HTTP 404 `pull access denied` (Docker)
  / 403 `denied` (podman) before the stream — wrong name or private image —
  or HTTP 200 with a final `{error, errorDetail}` line (bad tag), which the
  tool turns into an error too. Private registries need the optional
  `docker-mcp-registry-auth` secret (`DOCKER_REGISTRY_AUTH`), sent only as
  `X-Registry-Auth` on that call, always re-encoded as padded base64url
  (the daemon's Go decoder rejects unpadded values with HTTP 400 `failed to
  parse "X-Registry-Auth"`). When it is set the header goes on EVERY pull,
  public images included, so a wrong credential breaks all pulls: `version`
  reports `registry_auth_valid` (shape only) and a registry rejection comes
  back as 401/403/404/500 with the credential hint.
- `remove_image` 409s: "must be forced" (stopped container / multiple tags)
  -> `force=true`; "cannot be forced" (running container) -> stop that
  container first, force does not help.

## Secrets in the output

Container and image `Env` routinely hold credentials. `inspect_container` and
`inspect_image` replace the value of any key matching
pass/secret/token/key/credential/auth with `***` (`env_redacted: true`) unless
`include_env_values=true` — pass it only when the user explicitly asks for a
value. `raw=true` output is not redacted.

## Error catalogue (short form)

| You see | It means | Do |
|---|---|---|
| `could not reach the Docker daemon ... denied/refused` | a grant is missing or no TCP listener | Follow SETUP.md; do not retry |
| `could not reach the Docker daemon ... DnsError ... address not available` | the Security "allow host loopback" door is off | Ask the user to flip it (SETUP.md step 3); do not retry |
| `... timed out` | pull/wait/stop outlived the deadline | Pre-pull, raise `MCP_OUTBOUND_TIMEOUT_MS`, use container_logs |
| HTTP 400 `client version X is too old/new` | `DOCKER_API_VERSION` outside the window | `version` shows the window; operator changes the config |
| HTTP 403 plain Forbidden | docker-socket-proxy denies the section/POST | Enable the flag on the proxy |
| HTTP 404 on /containers/ | wrong id, auto-removed | `list_containers all=true` |
| HTTP 404 on create `No such image` | not in the local store | `pull_image` with a tag or `pull_if_missing` |
| HTTP 404/403 on pull `pull access denied` | wrong name or private image | Check the reference; register `docker-mcp-registry-auth` |
| HTTP 200 pull with `manifest unknown` | bad tag/digest/platform | Fix the tag |
| HTTP 304 | already running/stopped | `changed=false`; continue |
| HTTP 409 kill `not running` | signal to a stopped container | start or remove instead |
| HTTP 409 / podman 500 remove running | running without force | stop first or `force=true` |
| HTTP 409 image `must be forced` / `cannot be forced` | referenced by stopped/running container | `force=true` / stop the container |
| HTTP 500 `container is stopped` (podman stats) | not running | only stat running containers |
| HTTP 500 `logging driver does not support reading` | non-json-file driver | cannot read through the API |
| HTTP 429 | a proxy rate-limits | honour Retry-After |
| `docker-mcp is read-only` | server gate, nothing sent | operator sets `DOCKER_READ_ONLY=false` |
| `docker-mcp is misconfigured` | bad DOCKER_HOST scheme / API version / registry credential | operator fixes the named config or secret |

Details and the podman-vs-Docker shapes: [references/ERRORS.md](references/ERRORS.md).

## Reading results

- `"isError": true` inside a `result` — the tool ran and failed; the text is
  written for you: surface it.
- JSON-RPC `error` `-32602` — the request itself was malformed (missing/ill-typed
  params object). Fix the call; it is not an outage.
- HTTP `403 Forbidden` before any JSON-RPC body — the `Host` header did not
  match `MCP_ALLOWED_HOSTS`. HTTP `413` — request body over the transport cap.
- Every success carries `structuredContent`; the text block is a readable
  rendering (the log text itself for `container_logs`).
