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

A binary body is reported as `<N bytes of binary data>` rather than mangled into
the JSON, and `bodyIsText` says which you got.

## Build

```console
$ wash build
```

The component is written to `target/wasm32-wasip2/release/request_bin.wasm`. It is
a WASI p2 component: it exports `wasi:http/incoming-handler` and imports
`wasi:keyvalue/store`.

## Run on Cosmonic Desktop

Apply [`manifests/workload.yaml`](manifests/workload.yaml), then open
`http://request-bin.localhost:8200`.

Both `wasi:http/incoming-handler` and `wasi:keyvalue/store` are declared in that
manifest. A non-ambient import that is not declared fails to link, so leaving the
key-value entry out gives you a workload that never starts rather than one that
fails on first write.

That manifest runs the published image. To run your own build instead,
`wash build`, promote it, and swap the `image` reference for the one promote
gives you.
