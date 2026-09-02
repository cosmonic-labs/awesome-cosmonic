# Tool reference — docker-mcp

Every tool returns `structuredContent` plus a readable text block. Failures
are `isError: true` with the message described in [ERRORS.md](ERRORS.md).
"Gated" tools are refused with `docker-mcp is read-only` until the workload
runs with `DOCKER_READ_ONLY=false`; nothing is sent to the daemon.

| Tool | Params | Upstream | Output (structuredContent) | Gated |
|---|---|---|---|---|
| `version` | none | `GET /{v}/version` (falls back to unversioned `/version` + `/_ping` on a version-window 400) | `engine` docker\|podman, `version`, `api_version`, `min_api_version`, `configured_api_version`, `api_version_ok`, `os`, `arch`, `kernel_version`, `components[]`, `docker_host`, `read_only`, `registry_auth_configured`, `registry_auth_valid` + `registry_auth_error` when a credential is set (shape check, never the value), `hint` when outside the window | no |
| `info` | `raw?` bool | `GET /{v}/info` | ServerVersion, Name, ID, OperatingSystem, OSType, Architecture, KernelVersion, NCPU, MemTotal(+Human), Containers/Running/Paused/Stopped, Images, Driver, CgroupVersion, CgroupDriver, Rootless, LoggingDriver, DefaultRuntime, Runtimes (names), SwarmLocalNodeState, Warnings | no |
| `list_containers` | `all?` (default **true**), `limit?` 1..500 (100), `size?`, `filters?` object | `GET /{v}/containers/json?all&limit&size&filters` | `count`, `limit`, `containers[{id, names, image, command, state, status, created, ports[], labels, size_rw?, size_root_fs?}]` | no |
| `inspect_container` | `id`, `size?`, `include_env_values?`, `raw?` | `GET /{v}/containers/{id}/json?size` | `id`, `full_id`, `name`, `created`, `path`, `args`, `state{Status, Running, Paused, ExitCode, Error, StartedAt, FinishedAt, Health, ...}`, `image`, `config{Image, Cmd, Entrypoint, WorkingDir, User, Tty, Labels, ExposedPorts, Env(redacted)}`, `env_redacted`, `host_config{RestartPolicy, PortBindings, Binds, NetworkMode, Privileged, AutoRemove, Memory, NanoCpus, CapAdd, CapDrop, LogConfig}`, `mounts[]`, `network_settings{ports, networks{name: {ip_address, gateway, mac_address, network_id}}}` | no |
| `container_logs` | `id`, `tail?` 1..5000 (200), `since?`, `until?` (unix seconds or RFC 3339), `timestamps?`, `stdout?` (true), `stderr?` (true), `max_bytes?` 1024..1048576 (65536) | `GET /{v}/containers/{id}/json` (Tty), then `GET /{v}/containers/{id}/logs?follow=false&stdout&stderr&tail&since&until&timestamps` | `tty`, `tail`, `stdout`, `stderr`, `combined` (`out|`/`err|` prefixed when both), `truncated`, `bytes`, `total_bytes`, `frames`, `partial_frame` | no |
| `container_stats` | `id`, `raw?` | `GET /{v}/containers/{id}/stats?stream=false&one-shot=false` | `read`, `cpu_percent`, `online_cpus`, `memory_usage(+_human)`, `memory_limit(+_human)`, `memory_percent`, `network_rx_bytes`, `network_tx_bytes`, `block_read_bytes`, `block_write_bytes`, `pids`, `raw?` | no |
| `run_container` | `image` (explicit tag), `name?`, `cmd?[]`, `entrypoint?[]`, `env?` (`["K=V"]` or `{K: V}`, <= 200 / 8 KiB), `labels?`, `working_dir?`, `user?`, `ports?[]` `[hostip:]hostport:containerport[/proto]`, `restart_policy?` no\|always\|unless-stopped\|on-failure, `memory_bytes?` >= 6 MiB, `cpus?`, `platform?`, `wait?` bool, `pull_if_missing?` bool | `POST /{v}/containers/create?name&platform` (Tty false, AttachStd* false, AutoRemove false, PortBindings HostIp default 127.0.0.1), `POST .../start`; with `wait`: `POST .../wait?condition=not-running` then the logs path (tail 200); with `pull_if_missing`: the pull_image path once on 404 | `id`, `full_id`, `name`, `image`, `warnings[]`, `started`, `already_running`, `ports[]`, `pulled?`, and with wait: `exited`, `exit_code`, `wait_error`, `stdout`, `stderr`, `logs_truncated` (or `exited:false` + `note` on deadline) | **yes** |
| `start_container` | `id` | `POST /{v}/containers/{id}/start` -> 204 \| 304 | `id`, `changed`, `note` | **yes** |
| `stop_container` | `id`, `timeout?` 0..300 (10; capped to deadline-5 s), `signal?` | `POST /{v}/containers/{id}/stop?t&signal` -> 204 \| 304 | `id`, `changed`, `timeout`, `note`, `timeout_note?` | **yes** |
| `restart_container` | `id`, `timeout?`, `signal?` | `POST /{v}/containers/{id}/restart?t&signal` -> 204 | `id`, `changed`, `timeout`, `timeout_note?` | **yes** |
| `kill_container` | `id`, `signal?` (SIGKILL) | `POST /{v}/containers/{id}/kill?signal` -> 204 \| 409 | `id`, `signal`, `changed` | **yes** |
| `remove_container` | `id`, `force?`, `volumes?` | `DELETE /{v}/containers/{id}?v&force` -> 204 \| 409 (podman 500) | `id`, `removed`, `force`, `volumes` | **yes** |
| `list_images` | `all?`, `digests?`, `filters?`, `limit?` 1..1000 (200) | `GET /{v}/images/json?all&digests&shared-size=false&filters` | `count`, `total`, `limit`, `truncated`, `images[{id, repo_tags, repo_digests?, created, size, size_human, containers, labels, dangling}]` newest first | no |
| `inspect_image` | `name`, `include_env_values?`, `raw?` | `GET /{v}/images/{name}/json` | `id`, `full_id`, `repo_tags`, `repo_digests`, `created`, `size(+_human)`, `architecture`, `os`, `variant`, `author`, `config{Cmd, Entrypoint, Env(redacted), ExposedPorts, Labels, WorkingDir, User, Volumes, StopSignal}`, `rootfs_layers` | no |
| `pull_image` | `image` `repo[:tag\|@digest]`, `platform?`, `raw_progress?` | `POST /{v}/images/create?fromImage&tag&platform` (+ `X-Registry-Auth` when configured) -> 200 NDJSON | `image`, `repo`, `tag`, `defaulted_to_latest`, `registry_auth_sent`, `digest`, `layers{id: status}`, `status[]`, `progress_lines`, `note?`, `raw_progress?` | **yes** |
| `remove_image` | `name`, `force?`, `noprune?` | `DELETE /{v}/images/{name}?force&noprune` -> 200 `[{Untagged}\|{Deleted}]` | `name`, `force`, `untagged[]`, `deleted[]` (short ids), `changes` | **yes** |
| `list_networks` | `filters?` | `GET /{v}/networks?filters` | `count`, `networks[{id, name, driver, scope, internal, attachable, enable_ipv6, ipam[{subnet, gateway}], containers, labels, created}]` | no |
| `list_volumes` | `filters?` | `GET /{v}/volumes?filters` | `count`, `volumes[{name, driver, mountpoint, scope, created_at, labels, options, usage?}]`, `warnings[]` | no |
| `system_df` | `detail?` | `GET /{v}/system/df` | `layers_size(+_human)`, per category `images`/`containers`/`volumes`/`build_cache` `{total, active, size, size_human, reclaimable, reclaimable_human, reported}`; with detail: `image_items[]` (200 largest), `container_items[]`, `volume_items[]` | no |

`{v}` is `DOCKER_API_VERSION` (`v1.44`). Every list endpoint's `filters`
value is a JSON object of string arrays, URL-encoded by the tool.

## Filter keys accepted per tool

| Tool | Keys |
|---|---|
| `list_containers` | ancestor, before, expose, exited, health, id, isolation, is-task, label, name, network, publish, since, status, volume |
| `list_images` | before, dangling, label, reference, since, until |
| `list_networks` | dangling, driver, id, label, name, scope, type |
| `list_volumes` | dangling, driver, label, name |

## Validation performed before anything is sent

- Container ids/names: `^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$`.
- Image references: `^[A-Za-z0-9][A-Za-z0-9._:/@+-]{0,255}$`, no empty, `.`
  or `..` path segments; `/` is preserved in the API path.
- Signals: `SIGTERM`/`TERM`/`9`-style (`^(SIG)?[A-Z0-9]{1,12}$` or 1-3 digits).
- Platform: `os[/arch[/variant]]`, lowercase alphanumerics.
- Ports: `[hostip:]hostport:containerport[/tcp|udp|sctp]`, IPs must parse,
  container port 1..65535, host port 0..65535 (0 = daemon picks), <= 64 entries.
- Env: POSIX names (`^[A-Za-z_][A-Za-z0-9_]*$`), no control characters,
  <= 200 entries and 8 KiB total. Labels: <= 100, keys/values <= 512 chars.
- cmd/entrypoint: <= 256 elements of <= 4096 bytes. `cpus` in (0, 1024];
  `memory_bytes` >= 6 MiB. `until` must be later than `since`.
- Both `stdout` and `stderr` false is refused client-side.

## Sizes and truncation

- `raw=true` documents are capped at 200 KiB of JSON (`truncated: true` with a
  `raw_prefix`). Outbound bodies are capped by `MCP_OUTBOUND_MAX_BYTES`
  (4 MiB): `raw` inspect of a huge container, `system_df` on a daemon with
  thousands of images, or a pull progress stream with hundreds of layers can
  hit it (`response body exceeded the outbound size limit`).
- Log payloads are bounded by `max_bytes` (newest bytes kept).
- Progress: the last 200 lines when `raw_progress=true`; at most 500 layers
  are tracked.
