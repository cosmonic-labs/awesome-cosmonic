#!/usr/bin/env python3
"""Hermetic Docker Engine API fixture for docker-mcp's e2e suite.

Impersonates a Docker Engine (API 1.47, min 1.44) on 127.0.0.1:<port>, with a
few podman-shaped answers mixed in. Every request is recorded and served back
on GET /_log as JSON (method, path, query, selected headers, body) so the
tests can assert on the API-version prefix, query encoding, clamps, the
X-Registry-Auth header and the create body. POST /_reset clears the log.

Threaded: the harness fires 8 concurrent calls.
"""
import base64
import json
import struct
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

PORT = int(sys.argv[1])
LOG = []
LOCK = threading.Lock()
STATE = {"pulled": set()}

API_VERSION = "1.47"
MIN_API_VERSION = "1.44"


def frame(stream, payload):
    return bytes([stream, 0, 0, 0]) + struct.pack(">I", len(payload)) + payload


# web: non-TTY, both streams interleaved, a UTF-8 line, a 300 KiB frame and a
# trailing truncated frame (header says 100 bytes, 10 present).
WEB_LOGS = (
    frame(1, b"web: started\n")
    + frame(2, b"web: warn 1\n")
    + frame(1, "héllo ✓\n".encode("utf-8"))
    + frame(2, b"web: warn 2\n")
    + frame(1, b"x" * (300 * 1024) + b"\n")
    + frame(1, b"tail-line\n")
    + bytes([1, 0, 0, 0])
    + struct.pack(">I", 100)
    + b"truncated!"
)
TTY_LOGS = b"tty-out\r\ntty-err\r\n"
DONE_LOGS = frame(1, b"done-out\n") + frame(2, b"done-err\n")

CONTAINERS = {
    "web": {
        "Id": "web123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd",
        "Names": ["/web"],
        "Image": "nginx:1.27",
        "ImageID": "sha256:img300",
        "Command": "nginx -g 'daemon off;'",
        "Created": 1788600000,
        "Ports": [{"IP": "127.0.0.1", "PrivatePort": 80, "PublicPort": 8080, "Type": "tcp"}],
        "Labels": {"app": "web"},
        "State": "running",
        "Status": "Up 2 hours",
        "tty": False,
        "env": ["PATH=/usr/bin", "NGINX_VERSION=1.27"],
    },
    "tty": {
        "Id": "tty123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd",
        "Names": ["/tty"],
        "Image": "alpine:3.20",
        "ImageID": "sha256:img200",
        "Command": "sh",
        "Created": 1788500000,
        "Ports": [],
        "Labels": {},
        "State": "exited",
        "Status": "Exited (0) 3 hours ago",
        "tty": True,
        "env": ["PATH=/bin"],
    },
    "envy": {
        "Id": "envy23456789abcdef0123456789abcdef0123456789abcdef0123456789abcd",
        "Names": ["/envy"],
        "Image": "postgres:16",
        "ImageID": "sha256:img100",
        "Command": "postgres",
        "Created": 1788400000,
        "Ports": [{"PrivatePort": 5432, "Type": "tcp"}],
        "Labels": {},
        "State": "running",
        "Status": "Up 1 hour",
        "tty": False,
        "env": ["PASSWORD=hunter2", "TOKEN=abc", "API_KEY=k", "PLAIN=ok"],
    },
    "stopped-podman": {
        "Id": "stop3456789abcdef0123456789abcdef0123456789abcdef0123456789abcd",
        "Names": ["/stopped-podman"],
        "Image": "alpine:3.20",
        "ImageID": "sha256:img200",
        "Command": "sleep 1",
        "Created": 1788300000,
        "Ports": None,
        "Labels": {},
        "State": "stopped",
        "Status": "stopped",
        "tty": False,
        "env": [],
    },
    "nologs": {
        "Id": "nolo3456789abcdef0123456789abcdef0123456789abcdef0123456789abcd",
        "Names": ["/nologs"],
        "Image": "alpine:3.20",
        "ImageID": "sha256:img200",
        "Command": "sleep 1",
        "Created": 1788200000,
        "Ports": [],
        "Labels": {},
        "State": "running",
        "Status": "Up",
        "tty": False,
        "env": [],
    },
    "createdabc123def4567890": {
        "Id": "createdabc123def4567890abcdef0123456789abcdef0123456789abcdef01",
        "Names": ["/job1"],
        "Image": "alpine:3.20",
        "ImageID": "sha256:img200",
        "Command": "sh -c 'echo hi'",
        "Created": 1788700000,
        "Ports": [],
        "Labels": {},
        "State": "exited",
        "Status": "Exited (3) 1 second ago",
        "tty": False,
        "env": [],
    },
    "slowwait0000000000000000": {
        "Id": "slowwait0000000000000000abcdef0123456789abcdef0123456789abcdef01",
        "Names": ["/slow"],
        "Image": "slowwait:1",
        "ImageID": "sha256:img200",
        "Command": "sleep 100",
        "Created": 1788700001,
        "Ports": [],
        "Labels": {},
        "State": "running",
        "Status": "Up",
        "tty": False,
        "env": [],
    },
}

IMAGES = [
    {
        "Id": "sha256:img300000000000000000000000000000000000000000000000000000000000",
        "ParentId": "",
        "RepoTags": ["nginx:1.27", "docker.io/library/nginx:1.27"],
        "RepoDigests": ["nginx@sha256:aaaa"],
        "Created": 300,
        "Size": 150000000,
        "SharedSize": -1,
        "Labels": {"maintainer": "nginx"},
        "Containers": 1,
    },
    {
        "Id": "sha256:img100000000000000000000000000000000000000000000000000000000000",
        "ParentId": "",
        "RepoTags": ["<none>:<none>"],
        "RepoDigests": [],
        "Created": 100,
        "Size": 2048,
        "SharedSize": -1,
        "Labels": None,
        "Containers": 0,
    },
    {
        "Id": "sha256:img200000000000000000000000000000000000000000000000000000000000",
        "ParentId": "",
        "RepoTags": ["alpine:3.20", "docker.io/library/alpine:3.20"],
        "RepoDigests": ["alpine@sha256:bbbb"],
        "Created": 200,
        "Size": 8000000,
        "SharedSize": -1,
        "Labels": {},
        "Containers": -1,
    },
]

ALPINE_INSPECT = {
    "Id": "sha256:img200000000000000000000000000000000000000000000000000000000000",
    "RepoTags": ["alpine:3.20", "docker.io/library/alpine:3.20"],
    "RepoDigests": ["alpine@sha256:bbbb"],
    "Created": "2026-08-01T00:00:00Z",
    "Size": 8000000,
    "Architecture": "arm64",
    "Os": "linux",
    "Variant": "v8",
    "Author": "",
    "Config": {
        "Cmd": ["/bin/sh"],
        "Entrypoint": None,
        "Env": ["PATH=/bin", "SECRET_KEY=abc"],
        "ExposedPorts": None,
        "Labels": None,
        "WorkingDir": "",
        "User": "",
        "Volumes": None,
    },
    "RootFS": {"Type": "layers", "Layers": ["sha256:l1", "sha256:l2"]},
    "Metadata": {"LastTagTime": "0001-01-01T00:00:00Z"},
}


def container_row(c):
    row = {k: v for k, v in c.items() if k not in ("tty", "env")}
    row["NetworkSettings"] = {"Networks": {"bridge": {"IPAddress": "172.17.0.2"}}}
    row["Mounts"] = []
    return row


def container_inspect(name, c):
    running = c["State"] == "running"
    return {
        "Id": c["Id"],
        "Created": "2026-09-01T10:00:00.000000000Z",
        "Path": c["Command"].split(" ")[0],
        "Args": c["Command"].split(" ")[1:],
        "State": {
            "Status": c["State"],
            "Running": running,
            "Paused": False,
            "Restarting": False,
            "OOMKilled": False,
            "Dead": False,
            "Pid": 4242 if running else 0,
            "ExitCode": 0 if running else 3,
            "Error": "",
            "StartedAt": "2026-09-01T10:00:01.000000000Z",
            "FinishedAt": "0001-01-01T00:00:00Z" if running else "2026-09-01T11:00:00Z",
        },
        "Image": c["ImageID"],
        "Name": "/" + name,
        "RestartCount": 0,
        "Platform": "linux",
        "Mounts": [{"Type": "volume", "Name": "vol1", "Source": "/var/lib/x", "Destination": "/data", "Mode": "", "RW": True}],
        "Config": {
            "Hostname": c["Id"][:12],
            "User": "",
            "Tty": c["tty"],
            "Env": c["env"],
            "Cmd": c["Command"].split(" "),
            "Image": c["Image"],
            "WorkingDir": "/",
            "Entrypoint": None,
            "Labels": c["Labels"],
            "ExposedPorts": {"80/tcp": {}} if name == "web" else None,
        },
        "HostConfig": {
            "Binds": None,
            "NetworkMode": "bridge",
            "PortBindings": {"80/tcp": [{"HostIp": "127.0.0.1", "HostPort": "8080"}]} if name == "web" else {},
            "RestartPolicy": {"Name": "no", "MaximumRetryCount": 0},
            "AutoRemove": False,
            "Privileged": False,
            "Memory": 0,
            "NanoCpus": 0,
            "LogConfig": {"Type": "json-file", "Config": {}},
        },
        "NetworkSettings": {
            "Ports": {"80/tcp": [{"HostIp": "127.0.0.1", "HostPort": "8080"}]} if name == "web" else {},
            "Networks": {"bridge": {"IPAddress": "172.17.0.2", "Gateway": "172.17.0.1", "MacAddress": "02:42:ac:11:00:02", "NetworkID": "net1234567890abcdef"}},
        },
    }


STATS = {
    "read": "2026-09-01T10:00:05.000000000Z",
    "name": "/web",
    "id": "web123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd",
    "cpu_stats": {"cpu_usage": {"total_usage": 200}, "system_cpu_usage": 800, "online_cpus": 1},
    "precpu_stats": {"cpu_usage": {"total_usage": 100}, "system_cpu_usage": 400, "online_cpus": 1},
    "memory_stats": {"usage": 600, "limit": 1000, "stats": {"inactive_file": 100}},
    "networks": {"eth0": {"rx_bytes": 10, "tx_bytes": 20}, "eth1": {"rx_bytes": 5, "tx_bytes": 5}},
    "blkio_stats": {"io_service_bytes_recursive": [{"op": "Read", "value": 7}, {"op": "Write", "value": 9}, {"op": "Sync", "value": 99}]},
    "pids_stats": {"current": 3},
}

VERSION = {
    "Platform": {"Name": "Docker Engine - Community"},
    "Components": [{"Name": "Engine", "Version": "27.5.1", "Details": {"ApiVersion": API_VERSION, "MinAPIVersion": MIN_API_VERSION}}],
    "Version": "27.5.1",
    "ApiVersion": API_VERSION,
    "MinAPIVersion": MIN_API_VERSION,
    "GitCommit": "abc",
    "GoVersion": "go1.22",
    "Os": "linux",
    "Arch": "arm64",
    "KernelVersion": "6.1.0",
    "BuildTime": "2026-01-01T00:00:00.000000000+00:00",
}

INFO = {
    "ID": "fixture-id",
    "Containers": 4,
    "ContainersRunning": 2,
    "ContainersPaused": 0,
    "ContainersStopped": 2,
    "Images": 3,
    "Driver": "overlay2",
    "Plugins": {"Volume": ["local"], "Network": ["bridge", "host"], "Log": ["json-file"]},
    "MemoryLimit": True,
    "CgroupDriver": "systemd",
    "CgroupVersion": "2",
    "NCPU": 8,
    "MemTotal": 17179869184,
    "DockerRootDir": "/var/lib/docker",
    "Name": "fixture-host",
    "Labels": [],
    "ServerVersion": "27.5.1",
    "Runtimes": {"runc": {"path": "runc"}, "io.containerd.runc.v2": {"path": "runc"}},
    "DefaultRuntime": "runc",
    "Swarm": {"NodeID": "", "LocalNodeState": "inactive"},
    "OperatingSystem": "Fixture Linux",
    "OSType": "linux",
    "Architecture": "aarch64",
    "KernelVersion": "6.1.0",
    "LoggingDriver": "json-file",
    "Rootless": False,
    "Warnings": ["WARNING: No swap limit support"],
}

NETWORKS = [
    {
        "Name": "bridge",
        "Id": "net1234567890abcdef0123456789abcdef",
        "Created": "2026-09-01T00:00:00Z",
        "Scope": "local",
        "Driver": "bridge",
        "EnableIPv6": False,
        "IPAM": {"Driver": "default", "Config": [{"Subnet": "172.17.0.0/16", "Gateway": "172.17.0.1"}]},
        "Internal": False,
        "Attachable": False,
        "Containers": {"web": {}},
        "Labels": {},
    },
    {
        "Name": "custom",
        "Id": "net234567890abcdef0123456789abcdef0",
        "Created": "2026-09-01T00:00:00Z",
        "Scope": "local",
        "Driver": "bridge",
        "EnableIPv6": True,
        "IPAM": {"Driver": "default", "Config": [{"Subnet": "10.5.0.0/24", "Gateway": "10.5.0.1"}, {"Subnet": "fd00::/64", "Gateway": "fd00::1"}]},
        "Internal": True,
        "Attachable": True,
        "Containers": {},
        "Labels": {"team": "e2e"},
    },
]

VOLUMES = {
    "Volumes": [
        {"CreatedAt": "2026-09-01T00:00:00Z", "Driver": "local", "Labels": {"app": "web"}, "Mountpoint": "/var/lib/docker/volumes/vol1/_data", "Name": "vol1", "Options": {}, "Scope": "local"},
        {"CreatedAt": "2026-09-02T00:00:00Z", "Driver": "local", "Labels": None, "Mountpoint": "/var/lib/docker/volumes/vol2/_data", "Name": "vol2", "Options": None, "Scope": "local", "UsageData": {"Size": 20, "RefCount": 0}},
    ],
    "Warnings": ["fixture volume warning"],
}

DF = {
    "LayersSize": 150,
    "Images": [
        {"Id": "sha256:img300000000000000000000000000000000000000000000000000000000000", "RepoTags": ["nginx:1.27"], "Size": 100, "SharedSize": 0, "Containers": 1},
        {"Id": "sha256:img100000000000000000000000000000000000000000000000000000000000", "RepoTags": ["<none>:<none>"], "Size": 50, "SharedSize": 0, "Containers": 0},
    ],
    "Containers": [
        {"Id": "web123456789abcdef", "Names": ["/web"], "Image": "nginx:1.27", "State": "running", "SizeRw": 10, "SizeRootFs": 110},
        {"Id": "tty123456789abcdef", "Names": ["/tty"], "Image": "alpine:3.20", "State": "exited", "SizeRw": 5, "SizeRootFs": 55},
    ],
    "Volumes": [
        {"Name": "vol1", "Driver": "local", "UsageData": {"Size": 30, "RefCount": 1}},
        {"Name": "vol2", "Driver": "local", "UsageData": {"Size": 20, "RefCount": 0}},
    ],
    "BuildCache": [{"ID": "bc1", "Type": "regular", "Size": 40, "InUse": False, "Shared": False}],
}


def pull_stream(repo, tag):
    lines = [
        {"status": "Pulling from library/%s" % repo, "id": tag},
        {"status": "Pulling fs layer", "progressDetail": {}, "id": "aaaa1111"},
        {"status": "Downloading", "progressDetail": {"current": 1, "total": 2}, "progress": "[=> ]", "id": "aaaa1111"},
        {"status": "Pull complete", "progressDetail": {}, "id": "aaaa1111"},
        {"status": "Already exists", "progressDetail": {}, "id": "bbbb2222"},
        {"status": "Digest: sha256:abc123"},
        {"status": "Status: Downloaded newer image for %s:%s" % (repo, tag)},
    ]
    return ("\n".join(json.dumps(l) for l in lines) + "\n").encode()


BROKEN_STREAM = (
    json.dumps({"status": "Pulling from library/broken", "id": "1"}) + "\n"
    + json.dumps({"error": "manifest for broken:1 not found: manifest unknown: manifest unknown", "errorDetail": {"message": "manifest for broken:1 not found: manifest unknown: manifest unknown"}}) + "\n"
).encode()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    # --- helpers ---------------------------------------------------------
    def _send(self, status, body=b"", ctype="application/json", extra=None):
        if isinstance(body, (dict, list)):
            body = json.dumps(body).encode()
        elif isinstance(body, str):
            body = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Api-Version", API_VERSION)
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()
        if body:
            self.wfile.write(body)

    def _json(self, status, obj, extra=None):
        self._send(status, obj, extra=extra)

    def _record(self, method, body):
        parts = urlsplit(self.path)
        entry = {
            "method": method,
            "path": self.path,
            "raw_path": parts.path,
            "raw_query": parts.query,
            "query": parse_qs(parts.query, keep_blank_values=True),
            "headers": {k.lower(): v for k, v in self.headers.items() if k.lower() in ("x-registry-auth", "content-type", "user-agent", "accept", "host", "transfer-encoding")},
            "body": body.decode("utf-8", "replace") if body else "",
        }
        with LOCK:
            LOG.append(entry)
        return entry

    def _read_body(self):
        # The wasi:http client sends POST bodies with chunked transfer
        # encoding; an unread body would corrupt the keep-alive connection.
        if "chunked" in (self.headers.get("Transfer-Encoding") or "").lower():
            body = b""
            while True:
                line = self.rfile.readline().strip()
                size = int(line.split(b";")[0] or b"0", 16) if line else 0
                if size == 0:
                    # Trailer section (if any) ends with an empty line.
                    while self.rfile.readline().strip():
                        pass
                    return body
                body += self.rfile.read(size)
                self.rfile.readline()  # CRLF after the chunk
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""

    # --- verbs -----------------------------------------------------------
    def do_GET(self):
        parts = urlsplit(self.path)
        if parts.path == "/_log":
            with LOCK:
                return self._json(200, list(LOG))
        self.dispatch("GET", self._read_body())

    def do_POST(self):
        parts = urlsplit(self.path)
        body = self._read_body()
        if parts.path == "/_reset":
            with LOCK:
                LOG.clear()
            STATE["pulled"].clear()
            return self._json(200, {"ok": True})
        self.dispatch("POST", body)

    def do_DELETE(self):
        self.dispatch("DELETE", self._read_body())

    def dispatch(self, method, body):
        entry = self._record(method, body)
        parts = urlsplit(self.path)
        path = parts.path
        q = entry["query"]
        # API version prefix handling.
        version = None
        if path.startswith("/v1."):
            version, _, rest = path[1:].partition("/")
            path = "/" + rest
            vnum = version[1:]
            if vnum == "1.43":
                return self._json(400, {"message": "client version 1.43 is too old. Minimum supported API version is 1.44, please upgrade your client to a newer version"})
            if vnum == "1.99":
                return self._json(400, {"message": "client version 1.99 is too new. Maximum supported API version is %s" % API_VERSION})
        try:
            self.route(method, path, q, body, parts.query)
        except BrokenPipeError:
            pass

    def route(self, method, path, q, body, raw_query):
        first = lambda k, d=None: q.get(k, [d])[0]

        if path == "/_ping":
            return self._send(200, "OK", ctype="text/plain; charset=utf-8")
        if path == "/version":
            return self._json(200, VERSION)
        if path == "/info":
            return self._json(200, INFO)

        # ---- containers ----
        if path == "/containers/json":
            rows = [container_row(c) for c in CONTAINERS.values() if c["State"] != "exited" or first("all") == "true"]
            rows = rows[:4]
            rows[0] = dict(rows[0])
            rows[0]["Labels"] = dict(rows[0]["Labels"], query=raw_query)
            try:
                limit = int(first("limit", "0"))
            except ValueError:
                limit = 0
            if limit > 0:
                rows = rows[:limit]
            return self._json(200, rows)
        if path == "/containers/create":
            try:
                spec = json.loads(body or b"{}")
            except ValueError:
                return self._json(400, {"message": "invalid JSON body"})
            image = spec.get("Image", "")
            if first("name") == "taken":
                return self._json(409, {"message": 'Conflict. The container name "/taken" is already in use by container "abc". You have to remove (or rename) that container to be able to reuse that name.'})
            if image == "missing:1":
                return self._json(404, {"message": "No such image: missing:1"})
            if image == "pullme:1" and "pullme" not in STATE["pulled"]:
                return self._json(404, {"message": "No such image: pullme:1"})
            if image == "slowwait:1":
                return self._json(201, {"Id": CONTAINERS["slowwait0000000000000000"]["Id"], "Warnings": []})
            return self._json(201, {"Id": CONTAINERS["createdabc123def4567890"]["Id"], "Warnings": ["fixture warning"]})
        if path.startswith("/containers/"):
            rest = path[len("/containers/"):]
            cid, _, action = rest.partition("/")
            key = None
            for name, c in CONTAINERS.items():
                if cid == name or c["Id"].startswith(cid):
                    key = name
                    break
            if cid == "forbidden-section":
                return self._send(403, "<html><body>Forbidden</body></html>", ctype="text/html")
            if cid == "ratelimit":
                return self._json(429, {"message": "too many requests"}, extra={"Retry-After": "7"})
            if cid == "boom":
                return self._json(500, {"message": "boom"})
            if cid == "podman-run" and method == "DELETE":
                return self._json(500, {"cause": "container state improper", "message": "cannot remove container podman-run as it is running - running or paused containers cannot be removed without force: container state improper", "response": 500})
            if key is None:
                if cid == "gone" or True:
                    return self._json(404, {"message": "No such container: %s" % cid})
            c = CONTAINERS[key]
            if method == "GET" and action == "json":
                return self._json(200, container_inspect(key, c))
            if method == "GET" and action == "logs":
                if key == "nologs":
                    return self._json(500, {"message": "configured logging driver does not support reading"})
                if key == "tty":
                    return self._send(200, TTY_LOGS, ctype="application/octet-stream")
                if key == "createdabc123def4567890":
                    return self._send(200, DONE_LOGS, ctype="application/octet-stream")
                return self._send(200, WEB_LOGS, ctype="application/octet-stream")
            if method == "GET" and action == "stats":
                if key == "stopped-podman":
                    return self._json(500, {"cause": "container is stopped", "message": "container is stopped", "response": 500})
                return self._json(200, STATS)
            if method == "POST" and action == "start":
                if key == "web":
                    return self._send(304)
                return self._send(204)
            if method == "POST" and action == "stop":
                if key == "tty":
                    return self._send(304)
                return self._send(204)
            if method == "POST" and action == "restart":
                return self._send(204)
            if method == "POST" and action == "kill":
                if key == "tty":
                    return self._json(409, {"message": "Cannot kill container: tty: Container %s is not running" % c["Id"]})
                return self._send(204)
            if method == "POST" and action.startswith("wait"):
                if key == "slowwait0000000000000000":
                    time.sleep(8)
                return self._json(200, {"StatusCode": 3, "Error": None})
            if method == "DELETE" and action == "":
                if key == "web" and first("force") != "true":
                    return self._json(409, {"message": "You cannot remove a running container %s. Stop the container before attempting removal or force remove" % c["Id"]})
                return self._send(204)
            return self._json(404, {"message": "page not found"})

        # ---- images ----
        if path == "/images/create":
            repo = first("fromImage", "")
            tag = first("tag", "")
            auth = self.headers.get("X-Registry-Auth")
            if auth is not None or repo == "badauthparse":
                # Real daemons decode the header with Go's base64.URLEncoding
                # (padded): anything else is a 400 before the registry is
                # consulted. Mirror podman's exact shape so the test locks in
                # the padded form.
                try:
                    if repo == "badauthparse":
                        raise ValueError("forced")
                    decoded = base64.urlsafe_b64decode(auth.encode("ascii"))  # strict: no re-padding
                    if len(auth) % 4 != 0 or not decoded.startswith(b"{"):
                        raise ValueError("bad padding or not JSON")
                    json.loads(decoded)
                except Exception:
                    return self._json(400, {
                        "cause": "unexpected EOF",
                        "message": "failed to parse \"X-Registry-Auth\" header for /v1.44/images/create?fromImage=%s&tag=%s: unexpected EOF" % (repo, tag),
                        "response": 400,
                    })
            if repo == "private/secret" and not auth:
                return self._json(404, {"message": "pull access denied for private/secret, repository does not exist or may require 'docker login': denied: requested access to the resource is denied"})
            if repo == "broken":
                return self._send(200, BROKEN_STREAM)
            if repo == "slow":
                time.sleep(8)
            STATE["pulled"].add(repo)
            return self._send(200, pull_stream(repo, tag))
        if path == "/images/json":
            rows = [dict(i) for i in IMAGES]
            rows[0]["Labels"] = dict(rows[0]["Labels"] or {}, query=raw_query)
            return self._json(200, rows)
        if path.startswith("/images/") and path.endswith("/json"):
            name = path[len("/images/"):-len("/json")]
            if name in ("docker.io/library/alpine:3.20", "alpine:3.20", "img200000000") or name.startswith("sha256:img200"):
                return self._json(200, ALPINE_INSPECT)
            return self._json(404, {"message": "No such image: %s" % name})
        if path.startswith("/images/") and method == "DELETE":
            name = path[len("/images/"):]
            if name == "inuse:1" and first("force") != "true":
                return self._json(409, {"message": "conflict: unable to remove repository reference \"inuse:1\" (must be forced) - container abc123 is using its referenced image img200"})
            if name == "running:1":
                return self._json(409, {"message": "conflict: unable to delete img300 (cannot be forced) - image is being used by running container web123456789"})
            if name == "gone:1":
                return self._json(404, {"message": "No such image: gone:1"})
            return self._json(200, [{"Untagged": name}, {"Deleted": "sha256:img200000000000000000000000000000000000000000000000000000000000"}])

        # ---- networks / volumes / df ----
        if path == "/networks":
            rows = [dict(n) for n in NETWORKS]
            rows[0]["Labels"] = {"query": raw_query}
            return self._json(200, rows)
        if path == "/volumes":
            doc = json.loads(json.dumps(VOLUMES))
            doc["Volumes"][0]["Labels"]["query"] = raw_query
            return self._json(200, doc)
        if path == "/system/df":
            return self._json(200, DF)

        return self._json(404, {"message": "page not found"})


if __name__ == "__main__":
    server = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    server.daemon_threads = True
    server.serve_forever()
