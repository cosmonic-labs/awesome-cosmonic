# Request Bin

A disposable endpoint that records exactly what was sent to it.

Point a webhook, a form, or a misbehaving integration at a bin URL and read back
the method, path, headers and body of every request it received. It answers the
question you cannot answer from your own logs: what did they actually send me?

Requests persist in the host key-value store (`wasi:keyvalue`), so a bin survives
the component scaling to zero. It declares no outbound network access: it can be
reached, and it can write to its bucket, and that is the whole of it. Its
Launchpad card reads `OUTBOUND none`.

## Routes

| Route | Response |
| --- | --- |
| `GET /` | The browser UI, served inline. |
| `POST /api/bins` | Create a bin. Returns `{ "id": "..." }`. |
| `ANY /b/<id>` | Record a request into that bin. This is the URL you hand out. |
| `GET /api/bins/<id>` | The recorded requests, newest first. |
| `DELETE /api/bins/<id>` | Forget the bin. |
| `GET /healthz` | `ok` |

```console
$ ID=$(curl -sX POST http://request-bin.localhost:8200/api/bins | jq -r .id)
$ curl -X POST -H 'X-Demo: hello' -d '{"event":"ping"}' \
    http://request-bin.localhost:8200/b/$ID
{"recorded":true}
$ curl -s http://request-bin.localhost:8200/api/bins/$ID
[{"receivedAt":…,"method":"POST","path":"/b/…","headers":{…},"body":"{\"event\":\"ping\"}"}]
```

Each header name maps to an array of values, so a repeated `Set-Cookie` or
`X-Forwarded-For` is recorded in full rather than collapsed to one. A binary body
is reported as `<N bytes of binary data>` rather than mangled into the JSON, and
`bodyIsText` says which you got.

A bin is a debugging scratchpad, not an archive. It keeps the newest 50
requests and the first 64 KiB of each body (`bodyTruncated` tells you when a
body was cut). Sending to a bin that does not exist returns `404`.

## Build

Requires [`wash`](https://wasmcloud.com/docs/installation) and Rust with the
`wasm32-wasip2` target.

```console
$ wash build
```

`wash build` fetches the WIT packages listed in [`wkg.lock`](wkg.lock) into
`wit/deps`, then writes the component to
`target/wasm32-wasip2/release/request_bin.wasm`. It is a WASI 0.2 component: it
exports `wasi:http/incoming-handler@0.2.2` and imports `wasi:keyvalue/store`.
It stays on WASI 0.2 because the WASI 0.3 HTTP surface does not yet compose
with an extra `wasi:keyvalue` import.

## Run on Cosmonic Desktop

Apply [`manifests/workload.yaml`](manifests/workload.yaml), then open
`http://request-bin.localhost:8200`.

Both `wasi:http/incoming-handler` and `wasi:keyvalue/store` are declared in that
manifest. A non-ambient import (one the host does not provide to every
component automatically) fails to link if the manifest does not declare it. So
leaving the key-value entry out gives you a workload that never starts, rather
than one that fails on first write.

That manifest runs the published image,
`ghcr.io/cosmonic-labs/components/request-bin:0.1.1`, pinned by digest. To run
your own build instead, run `wash build`, promote the project in Cosmonic
Desktop, and swap the `image` reference for the one promote gives you.
