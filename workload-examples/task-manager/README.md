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

> **Status:** the component is complete and correct — it builds, deploys, reaches
> Running, and the key-value store links. But the HTTP trigger does not route on
> Cosmonic Desktop today, and the cause is a host/toolchain bug, not this code:
> a component built with current `wash`/`wit-component` (0.251) that imports
> **both `wasi:cli/*` and `wasi:keyvalue`** silently loses its HTTP binding on
> the 2.7.0 host. Controls: a `wasi:cli`-only component binds; a
> `wasi:keyvalue`-only component binds; the older-toolchain `keyvalue-counter`
> (cli + keyvalue, built with wit-component 0.202) binds. The `wasi:cli` imports
> here come from `std`/`serde_json`. Fix belongs in the daemon (see the
> "unbind all plugins" behavior); this entry lands in the Launchpad once that
> ships. Image: `ghcr.io/cosmonic-labs/components/task-manager:0.1.3`.