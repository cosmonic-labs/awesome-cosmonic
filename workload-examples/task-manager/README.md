# Task Manager

A small **stateful** component for Cosmonic Desktop: a to-do list served over
HTTP that persists to the host key-value store (`wasi:keyvalue`). There is no
database to run and no outbound network. It reaches only the HTTP trigger and
the store its Workload grants it.

## Routes

| Route | Response |
| --- | --- |
| `GET /` | The browser UI, served inline. |
| `GET /api/tasks` | The task list as JSON. |
| `POST /api/tasks?title=…` | Add a task. Returns the updated list. |
| `POST /api/tasks/toggle?id=…` | Mark a task done, or not done. Returns the updated list. |
| `POST /api/tasks/delete?id=…` | Delete a task. Returns the updated list. |

## Build

Requires [`wash`](https://wasmcloud.com/docs/installation) and Rust 1.85 or
newer (the crate uses edition 2024) with the `wasm32-wasip2` target.

```console
$ wash build
```

The component is written to `target/wasm32-wasip2/release/task_manager.wasm`.
The world pins `wasi:http/incoming-handler@0.2.2` to match the version the
Cosmonic Desktop host serves, and imports only `wasi:keyvalue/store` (importing
`wasi:keyvalue/atomics` as well stops the host from routing the HTTP trigger).

## Run on Cosmonic Desktop

Requires **Cosmonic Desktop 0.5.21 or newer**. Apply
[`manifests/workload.yaml`](manifests/workload.yaml), then open
`http://task-manager.localhost:8200`.

That manifest runs the published image,
`ghcr.io/cosmonic-labs/components/task-manager:0.1.3`, pinned by digest. To run
your own build instead, run `wash build`, promote the project in Cosmonic
Desktop, and swap the `image` reference for the one promote gives you.

Both `wasi:http/incoming-handler` and `wasi:keyvalue/store` are declared in the
manifest. A non-ambient import (one the host does not provide to every
component automatically) fails to link if the manifest does not declare it. So
leaving the key-value entry out gives you a workload that never starts, rather
than one that fails on first write.
