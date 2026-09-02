# Error catalogue — docker-mcp

Every failure is a tool error (`isError: true`) whose text starts with what
happened and ends with what to do. The tool matches on the daemon's `message`
text as well as the status code, because Docker and podman disagree on codes
for the same condition. Retry only where this table says so.

## Transport (nothing reached the daemon, or the exchange was cut)

| Text contains | Meaning | Action | Retry? |
|---|---|---|---|
| `could not reach the Docker daemon ... denied` / `HttpRequestDenied` / policy | The workload is not allowed to dial `host.wasmcloud.internal:2375`: `allowedHosts`, `allowedHostLoopbackPorts` or the Desktop Security "allow host loopback" toggle is missing | Fix the manifest / toggle ([SETUP.md](SETUP.md)) | no |
| `... connection refused` / `connect` | Nothing listens on 127.0.0.1:2375 on the host (daemon only on the unix socket, or the proxy is down) | Start the TCP listener; verify `curl http://127.0.0.1:2375/_ping` on the host | no |
| `... DnsError ... address not available` for `host.wasmcloud.internal` (text: `what a closed loopback door looks like`) | The loopback door is closed: on Cosmonic Desktop the sentinel name only resolves once Settings -> Security -> "allow host loopback" is on and `allowedHostLoopbackPorts` grants the port. This is the exact error a fresh install shows | Flip the toggle / add the grant ([SETUP.md](SETUP.md)); do not retry until a human did | no |
| DNS failure for any other name | `DOCKER_HOST` names a host the sandbox cannot resolve (`localhost`, a typo) | Use `host.wasmcloud.internal` | no |
| `... timed out after N ms` | The exchange outlived `MCP_OUTBOUND_TIMEOUT_MS` (pull, `wait`, a long stop) | Pre-pull with the CLI, raise the deadline, re-run (layers already pulled are reused); for wait use `container_logs` | once, if the operation is idempotent |
| `response body exceeded the outbound size limit` | `MCP_OUTBOUND_MAX_BYTES` (4 MiB) tripped | Avoid `raw=true`, narrow filters, or raise the cap | no |
| `docker-mcp is misconfigured: DOCKER_HOST uses the unix:// scheme` | Only http(s) can be dialled from the sandbox | Point `DOCKER_HOST` at a TCP listener | no |
| `docker-mcp is misconfigured: DOCKER_API_VERSION must look like v1.44` | Bad named config | Fix it | no |
| `docker-mcp is misconfigured: DOCKER_REGISTRY_AUTH ... neither a JSON object nor base64url` | A placeholder or garbage credential is registered (`version` reports it as `registry_auth_valid: false` before any pull) | Register a real credential as `docker-mcp-registry-auth` or remove the secretFrom | no |

## HTTP 400

| Message | Meaning | Action |
|---|---|---|
| `client version 1.43 is too old. Minimum supported API version is 1.44` | `DOCKER_API_VERSION` below the daemon's minimum (Docker 29+ default 1.44 unless the operator set `DOCKER_MIN_API_VERSION`) | `version` shows the window (it falls back to the unversioned route and the `/_ping` `Api-Version` header); operator sets `DOCKER_API_VERSION` inside it |
| `client version 1.5x is too new. Maximum supported API version is 1.44` | Above the daemon's `ApiVersion` (older Docker) | Lower `DOCKER_API_VERSION` |
| `failed to parse "X-Registry-Auth" header ... unexpected EOF` | The daemon could not base64-decode the auth header (it needs padded url-safe base64). docker-mcp always sends that form, so this means the registered secret is not JSON/base64 at all or a proxy rewrote the header | The tool maps it to "could not parse the X-Registry-Auth header" with the credential hint; check `version` -> `registry_auth_valid` |
| `Bad parameters: you must choose at least one stream` | both stdout and stderr false | The tool refuses this client-side; never reaches the daemon |
| `invalid filter` / (podman 500) `failed to decode filter parameters` | filters not an object of string arrays / unknown key | Use `{"status":["running"]}` shapes; the tool refuses unknown keys before dialling |
| anything else | malformed request | Check the parameters in [TOOLS.md](TOOLS.md) |

## HTTP 401 / 403

| Status + message | Meaning | Action |
|---|---|---|
| 401 (any) / 500 `unable to retrieve auth token: invalid username/password` (podman) / 500 `unauthorized: incorrect username or password` (Docker) | The daemon parsed `X-Registry-Auth` and the registry rejected it | Rotate the `docker-mcp-registry-auth` secret; remember it is sent on every pull, public images included |
| 403 plain `Forbidden` (text/html or empty) on any endpoint | A docker-socket-proxy denies that API section (`CONTAINERS`/`IMAGES`/`INFO`/`NETWORKS`/`VOLUMES`/`SYSTEM=0`) or all writes (`POST=0`) | Enable the flag on the proxy; for lifecycle tools also `POST=1` + `ALLOW_START/ALLOW_STOP/ALLOW_RESTARTS=1`. Reads never need POST |
| 403 `denied: requested access to the resource is denied` on pull (podman) | Repository missing or private | Same as the 404 pull case below |

## HTTP 404

| Path / message | Meaning | Action |
|---|---|---|
| `/containers/{id}/...`: Docker `No such container: x`; podman `no container with name or ID "x" found: no such container` | Wrong id/name, auto-removed (`AutoRemove`) or already deleted | `list_containers all=true` (+ `filters {"name":[..]}`) and use the 12-char id |
| `/containers/create`: Docker `No such image: ref`; podman `no such image: ...: image not known` | Not in the local store; create never pulls | `pull_image` with an explicit tag, or `run_container pull_if_missing=true` |
| `/images/create`: `pull access denied for X, repository does not exist or may require 'docker login': denied: requested access to the resource is denied` | Wrong name **or** private image without a valid credential — Docker conflates them deliberately | Check registry/namespace/name; for private images register `docker-mcp-registry-auth` with `serveraddress` = the registry host |
| `/images/{name}/json` or `DELETE /images/{name}`: `No such image` | Not local | `list_images` shows what is there |

## HTTP 409 (and podman's 500 equivalents)

| Message | Meaning | Action |
|---|---|---|
| kill: `Cannot kill container: x: Container x is not running` (podman 500: `can only kill running containers. ... container state improper`) | Signal to a non-running container | `inspect_container` for `state.Status`; use `start_container` or `remove_container` |
| remove: `You cannot remove a running container x. Stop the container before attempting removal or force remove` (podman **500**: `cannot remove container ...: container state improper`, cause `container state improper`) | Running/paused and `force=false` | `stop_container` first, or `remove_container force=true` |
| remove image: `conflict: unable to delete <id> (must be forced) - image is being used by stopped container <cid>` / `... is referenced in multiple repositories` | A stopped container or another tag references the image | `remove_image force=true`, or remove the stopped container |
| remove image: `conflict: unable to delete <id> (cannot be forced) - image is being used by running container <cid>` | A running container uses it; force cannot override | stop/remove that container, then `remove_image` |
| create: `Conflict. The container name "/x" is already in use by container ...` | Name taken | Pick another name or remove the old container |

## HTTP 500 (daemon-side)

| Message | Meaning | Action |
|---|---|---|
| `container is stopped` (podman stats) | podman refuses stats for non-running containers; Docker returns zeros | Only stat running containers |
| `configured logging driver does not support reading` | Log driver is none/syslog/awslogs/... | Cannot read through the API; recreate with json-file or read at the driver's sink |
| `makechan: size out of range` (podman logs) | A negative/non-numeric `tail` reached the daemon | The tool clamps tail; if seen, `DOCKER_HOST` is a proxy rewriting queries |
| pull stream ends with `{"error": ..., "errorDetail": {"message": "manifest for X not found: manifest unknown"}}` (HTTP 200!) | Bad tag/digest, or platform unavailable | Fix the tag; try `platform=linux/amd64` etc. The tool surfaces it as `pull of X failed mid-stream` |
| anything else | Transient daemon failure | Retry once after a few seconds; then check the daemon logs |

## Non-errors worth recognising

| Signal | Meaning |
|---|---|
| HTTP 304 on start/stop | Already running / already stopped. Reported as `changed: false`; continue |
| `warnings[]` on run_container (e.g. `The requested image's platform (linux/arm64/v8) does not match the detected host platform`) | The container started but may run under emulation or fail; check `container_logs` |
| `defaulted_to_latest: true` on pull_image | No tag given; `latest` was pulled |
| `timeout_note` on stop/restart | The requested grace period was capped to the outbound deadline |
| `truncated: true` on logs/lists | Output was bounded; narrow the request |
| `env_redacted: true` | Some Env values were replaced by `***` |

## Server gates (nothing was sent)

| Text | Action |
|---|---|
| `docker-mcp is read-only (DOCKER_READ_ONLY=true, the default): <tool> was not sent` | The operator sets `DOCKER_READ_ONLY: "false"` under `localResources.environment.config` in `deploy/workload.yaml` and re-applies (plus `POST=1` on a socket proxy) |
| `unknown filter key "x" for list_...; allowed: ...` | Use one of the allowed keys |
| `image must be a reference like ...` / `id must match ...` / `port "x": ...` / `signal must be ...` | Parameter validation; fix the value |
