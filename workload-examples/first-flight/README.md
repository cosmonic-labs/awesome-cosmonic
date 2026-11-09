# First Flight

The page a freshly launched workload serves back on your own machine: the "you're
flying" moment of the getting-started walkthrough in
[Cosmonic Desktop](https://cosmonic.com/). This is the source behind the
**First Flight** entry in the Desktop Launchpad.

It is a single Rust `wasi:http` component (no other capabilities) that serves one
self-contained HTML page: brand-styled, gently animated, and honest about being a
live program rather than a static file. The page shows the workload's name, the
host it is answering on, a live clock, and a **Send another request** button that
issues a genuine request the component handles.

The component is published as an OCI artifact at
`ghcr.io/cosmonic-labs/first-flight`. It was written for Cosmonic Desktop and
Cosmonic Control, but it runs on any WebAssembly runtime that supports the
Component Model and the [WASI](https://wasi.dev/) HTTP API.

Cosmonic Control is built on [wasmCloud](https://wasmcloud.com/), an Incubating
project at the [Cloud Native Computing Foundation (CNCF)](https://www.cncf.io/).

## Routes

| Route | Response |
| --- | --- |
| `GET /` | The First Flight HTML page. The workload name (from `FIRST_FLIGHT_NAME`, default `first-flight`) and the request authority are reflected into the page. |
| `GET /count` | `{"ok":true}`. The **Send another request** button calls this, so each press is a real round trip the component handles. |
| anything else | `404` |

The component is deliberately stateless: the runtime instantiates it per
invocation, so the page's tallies are scoped to the visit and every increment
corresponds to a genuine request. Nothing is faked, and no store or extra
capability is required.

## Build

Requires [`wash`](https://wasmcloud.com/docs/installation) and Rust with the
`wasm32-wasip2` target.

```shell
wash build
# or, directly:
cargo build --target wasm32-wasip2 --release
```

The component is written to `target/wasm32-wasip2/release/first_flight.wasm`.

## Run locally

```shell
# A local wasmCloud dev host on http://127.0.0.1:8000
wash dev

# or any WASI HTTP runtime, for example
wasmtime serve -Scli target/wasm32-wasip2/release/first_flight.wasm
```

Then open the served address in a browser.

## Deploy on Cosmonic Desktop

[`manifests/workload.yaml`](manifests/workload.yaml) is the Workload manifest
for the **First Flight** card. Apply it and open
`http://first-flight.localhost:8200`.

It runs the published image, pinned by digest:
`ghcr.io/cosmonic-labs/first-flight:0.1.2`. The Launchpad card itself now runs
`0.1.3`, which differs only in one line of page copy. To run your own build
instead, run `wash build`, promote the project in Cosmonic Desktop, and swap the
`image` reference for the one promote gives you.

## Deploy on Cosmonic Control

[`manifests/component.yaml`](manifests/component.yaml) is the Kubernetes path for
the same component: an `HTTPTrigger` running the published image behind an
ingress host. Apply it to a cluster running Cosmonic Control:

```shell
kubectl apply -f manifests/component.yaml
```

[`values.http-trigger.yaml`](values.http-trigger.yaml) holds the same settings as
values for the `http-trigger` Helm chart, if you deploy with Helm instead.
