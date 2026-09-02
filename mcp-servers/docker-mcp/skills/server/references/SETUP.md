# Daemon setup — reaching Docker / podman from the sandbox

The component can only make outbound HTTP through the Desktop policy; it
cannot open the daemon's unix socket, run a subprocess, or present a client
certificate. So the daemon (or a proxy in front of it) must listen on plain
HTTP at `127.0.0.1:2375`, and the workload must be granted the loopback door.

**Security note (from both the Docker and podman documentation):** a TCP
listener without mTLS is root-equivalent for any process on the machine. Bind
it to `127.0.0.1` only, keep `DOCKER_READ_ONLY=true` unless lifecycle control
is wanted, and prefer the socket proxy with `POST=0`.

## 1. A TCP listener on 127.0.0.1:2375

Pick one:

### podman (rootless works and is the safer choice)

```console
$ podman system service --time 0 tcp://127.0.0.1:2375
```

(`--time 0` keeps it running; add it to a user systemd unit for persistence.)
podman reports `ApiVersion 1.44` and accepts any `/vX.YY/` prefix.

### Docker Engine on Linux

Either a systemd drop-in (**not** together with `daemon.json` `hosts` — the
two conflict and dockerd will not start):

```ini
# /etc/systemd/system/docker.service.d/tcp.conf
[Service]
ExecStart=
ExecStart=/usr/bin/dockerd -H fd:// -H tcp://127.0.0.1:2375
```

```console
$ sudo systemctl daemon-reload && sudo systemctl restart docker
```

or `daemon.json` (`{"hosts": ["fd://", "tcp://127.0.0.1:2375"]}`) with the
`-H fd://` removed from the unit. Docker 29+ rejects API versions below 1.44
unless `DOCKER_MIN_API_VERSION` is set on the daemon.

### Docker Desktop (macOS / Windows) or any daemon: docker-socket-proxy

Docker Desktop exposes no TCP endpoint by default. Run
[Tecnativa/docker-socket-proxy](https://github.com/Tecnativa/docker-socket-proxy)
(Apache-2.0), which also gives per-section read-only control:

```console
$ docker run -d --name dockerproxy --restart unless-stopped \
    -v /var/run/docker.sock:/var/run/docker.sock \
    -p 127.0.0.1:2375:2375 \
    -e CONTAINERS=1 -e IMAGES=1 -e INFO=1 -e NETWORKS=1 -e VOLUMES=1 -e SYSTEM=1 \
    tecnativa/docker-socket-proxy
```

Add `-e POST=1 -e ALLOW_START=1 -e ALLOW_STOP=1 -e ALLOW_RESTARTS=1` only when
the workload runs with `DOCKER_READ_ONLY=false`. The proxy answers plain
`403 Forbidden` for denied sections (the tool maps it).

A one-liner alternative: `socat TCP-LISTEN:2375,bind=127.0.0.1,fork
UNIX-CONNECT:/var/run/docker.sock`.

### Verify on the host

```console
$ curl -s http://127.0.0.1:2375/_ping        # OK
$ curl -s http://127.0.0.1:2375/v1.44/version | jq '.ApiVersion, .MinAPIVersion'
```

## 2. The workload grants (deploy/workload.yaml)

```yaml
localResources:
  environment:
    config:
      DOCKER_HOST: "http://host.wasmcloud.internal:2375"   # never localhost/127.0.0.1
      DOCKER_API_VERSION: "v1.44"
      DOCKER_READ_ONLY: "true"                             # "false" enables writes
  allowedHosts:
    - "host.wasmcloud.internal:2375"
  allowedHostLoopbackPorts:
    - "2375"
```

`.wash/config.yaml`'s `workload:` block mirrors the same lists.

## 3. The host-level door

Cosmonic Desktop Settings -> Security -> **allow host loopback** (default
off; API: `PUT /v1/egress {"allow_host_loopback": true}`). This is a global
toggle, not per workload.

## Diagnosing

| `version` says | Missing leg |
|---|---|
| `denied` / `HttpRequestDenied` | allowedHosts, allowedHostLoopbackPorts or the Security toggle |
| `connection refused` | no listener on 127.0.0.1:2375 (step 1) |
| `DnsError ... address not available` for `host.wasmcloud.internal` | the Security "allow host loopback" toggle is off (or the port is missing from `allowedHostLoopbackPorts`): the sentinel name does not resolve until the door is open — this is what a fresh install shows (verified on Desktop 0.5.27) |
| DNS failure for another name | `DOCKER_HOST` does not use `host.wasmcloud.internal` |
| HTTP 400 `client version ... too old/new` | `DOCKER_API_VERSION` outside `[MinAPIVersion, ApiVersion]` |
| HTTP 403 `Forbidden` | socket proxy section flag / `POST` |
| `api_version_ok: true`, engine + version shown | everything works |

## Optional: private registry pulls

`pull_image` sends `X-Registry-Auth` when the `docker-mcp-registry-auth`
secret (env `DOCKER_REGISTRY_AUTH`) is registered:

```console
$ cosmonic_set_secret name=docker-mcp-registry-auth \
    uri=keychain://cosmonic/docker-mcp-registry-auth env=DOCKER_REGISTRY_AUTH \
    value='{"username":"<user>","password":"<read-only token>","serveraddress":"index.docker.io"}'
```

then add `secretFrom: [{name: docker-mcp-registry-auth}]` under
`localResources.environment` and re-apply. Docker Hub tokens:
<https://app.docker.com/settings/personal-access-tokens> (read-only scope);
GHCR: a PAT with `read:packages` and `serveraddress` `ghcr.io`. Public images
never need it. The value may be the raw JSON or its base64 in any variant
(`echo -n '{...}' | base64` included): the server decodes it and re-emits
padded url-safe base64, the only form the daemon's Go `base64.URLEncoding`
decoder accepts. Run `version` after registering: `registry_auth_valid: true`
means the shape is right; the credential itself is only verified by the
registry on the first `pull_image`. Note the header is sent on every pull
once configured, so a wrong credential breaks public pulls too.

## Not supported

- `unix://` / `ssh://` `DOCKER_HOST` values (no sockets or subprocesses).
- `tcp://:2376` with client certificates (no client-cert auth in wasi:http)
  or self-signed server certificates (webpki roots only). Front such daemons
  with a local plain-HTTP proxy.
- Streaming (`follow=true` logs, `stream=true` stats, attach/exec):
  every call is one bounded exchange.
