# Task Manager (hosted)

A small **stateful** WebAssembly component for Cosmonic Desktop: a to-do list
served over HTTP that persists to the host key-value store (`wasi:keyvalue`).
No database, no outbound network — it reaches only the HTTP trigger and the
store its Workload grants it.

- `GET /` — the UI
- `GET /api/tasks` — list
- `POST /api/tasks?title=…` — add
- `POST /api/tasks/toggle?id=…` — toggle done
- `POST /api/tasks/delete?id=…` — delete

## Build

```
wash build   # -> target/wasm32-wasip2/release/task_manager.wasm
```

The world pins `wasi:http/incoming-handler@0.2.9` to match the version the
Desktop host serves.

> **Status:** builds and deploys (reaches Running, key-value store links), but
> the HTTP trigger is not yet routing on Cosmonic Desktop when built with the
> current `wash`/`wit-component` toolchain + a `wasi:keyvalue` import — under
> investigation (a host-side http+keyvalue binding issue). An http-only
> component of the same shape routes fine.
