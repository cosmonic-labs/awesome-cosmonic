# Task Manager (hosted)

A small **stateful** WebAssembly component for Cosmonic Desktop: a to-do list
served over HTTP that persists to the host key-value store (`wasi:keyvalue`).
There is no database to run and no outbound network. It reaches only the HTTP
trigger and the store its Workload grants it.

- `GET /` the UI
- `GET /api/tasks` list
- `POST /api/tasks?title=…` add
- `POST /api/tasks/toggle?id=…` toggle done
- `POST /api/tasks/delete?id=…` delete

## Build

```
wash build   # -> target/wasm32-wasip2/release/task_manager.wasm
```

The world pins `wasi:http/incoming-handler@0.2.2` to match the version the
Desktop host serves.

## Run on Cosmonic Desktop

Requires **Cosmonic Desktop 0.5.21 or newer** (the release carrying wash-runtime
2.7.0). Apply [`manifests/workload.yaml`](manifests/workload.yaml), then open
`http://task-manager.localhost:8200`.

That manifest runs the published image, digest-pinned:
`ghcr.io/cosmonic-labs/components/task-manager:0.1.3`. To run your own build
instead, `wash build`, promote the result, and swap the `image` reference for
the one promote gives you.

Both `wasi:http/incoming-handler` and `wasi:keyvalue/store` are declared in the
manifest. A non-ambient import that is not declared fails to link, so leaving
the key-value entry out gives you a workload that never starts rather than one
that fails on first write.

Verified live on 0.5.21: the HTTP trigger routes and add, toggle, and delete all
round-trip through the host key-value store.