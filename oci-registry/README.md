# oci-registry

> An [OCI Distribution Spec v1.1](https://github.com/opencontainers/distribution-spec) registry as a single **wasmCloud v2** component. Stores content on a local disk volume; scales to zero.

[![CI](https://github.com/LiamRandall/oci-registry/actions/workflows/ci.yml/badge.svg)](https://github.com/LiamRandall/oci-registry/actions/workflows/ci.yml)
![License](https://img.shields.io/badge/license-Apache--2.0-blue)

A complete, dependency-free container/artifact registry for local development —
push and pull with `wash`, `oras`, `docker`, or `crane`. It is one Rust
WebAssembly component without a database. The whole
thing is ~360 KB of wasm.

## What it is

- **One component.** Exports `wasi:http/incoming-handler`; the host runs it.
- **Scale-to-zero.** A pure reactor: the host instantiates it per request and
  tears it down afterwards, so nothing is resident when idle. All state lives on
  disk, which is what makes the stateless-per-request model possible.
- **Local disk storage.** Content-addressed blobs and manifests under a mounted
  volume (`/data`). Pushed images survive restarts.
- **Spec-compliant.** Implements the OCI distribution API: blobs (monolithic +
  chunked uploads, cross-repo mount), manifests (tags + digests), tag &
  catalog listing with pagination, deletes, and the v1.1 **referrers** API.
- **Local-dev niceties.** A browse UI at `/`, a JSON browse API, and `/healthz`.

## Quickstart (`wash dev`)

```sh
make dev          # builds the component and serves it at 127.0.0.1:8080
```

In another terminal:

```sh
# push any wasm component or artifact
wash oci push --insecure 127.0.0.1:8080/hello/world:0.1.0 ./component.wasm

# or with oras
oras push --plain-http 127.0.0.1:8080/hello/world:0.1.0 ./component.wasm:application/wasm

# browse it
open http://127.0.0.1:8080/

# inspect via the API
curl 127.0.0.1:8080/v2/_catalog
curl 127.0.0.1:8080/v2/hello/world/tags/list
```

## Architecture

```
                 ┌──────────────────────────────────────────────┐
   OCI client    │              wasmCloud v2 host                │
  (wash / oras   │   HTTP ingress ─► wasi:http/incoming-handler  │
   docker / …)   │                       │                       │
      │  HTTP    │                       ▼                       │
      └─────────►│            ┌─────────────────────┐            │
                 │            │  oci-registry (wasm) │  reactor:  │
                 │            │  OCI Distribution    │  per-req,  │
                 │            │  Spec v1.1 + UI      │  scales→0  │
                 │            └──────────┬──────────┘            │
                 │                       │ wasi:filesystem        │
                 └───────────────────────┼────────────────────────┘
                                         ▼
                              /data (host disk volume)
                                blobs/sha256/<hex>
                                repos/<name>/_manifests/{revisions,tags}
                                uploads/<id>
```

The request path is split so the entire registry is unit-tested on the host
with no wasm runtime ([`src/oci.rs`](components/registry/src/oci.rs) `dispatch`
is pure; only [`src/lib.rs`](components/registry/src/lib.rs)'s thin glue is
wasm-only).

## API surface

| Method            | Path                                          | Purpose |
|-------------------|-----------------------------------------------|---------|
| GET               | `/v2/`                                         | API version check |
| GET               | `/v2/_catalog`                                 | List repositories (`n`/`last` paginated) |
| GET               | `/v2/<name>/tags/list`                         | List tags (`n`/`last` paginated) |
| GET/HEAD          | `/v2/<name>/manifests/<ref>`                   | Pull manifest (tag or digest) |
| PUT               | `/v2/<name>/manifests/<ref>`                   | Push manifest |
| DELETE            | `/v2/<name>/manifests/<ref>`                   | Delete manifest or tag |
| GET/HEAD          | `/v2/<name>/blobs/<digest>`                    | Pull blob |
| DELETE            | `/v2/<name>/blobs/<digest>`                    | Delete blob |
| POST              | `/v2/<name>/blobs/uploads/`                    | Start upload / monolithic / mount |
| PATCH/PUT/GET/DELETE | `/v2/<name>/blobs/uploads/<id>`             | Chunked upload session |
| GET               | `/v2/<name>/referrers/<digest>`                | OCI 1.1 referrers index |
| GET               | `/` · `/healthz` · `/api/repositories`         | Browse UI · health · JSON browse |

Errors use the spec envelope: `{"errors":[{"code","message"}]}`.

## Deploy on Cosmonic Desktop

The component publishes to GHCR on tagged release. The deployment artifact is
[`deploy/workload.yaml`](deploy/workload.yaml)
(`runtime.wasmcloud.dev/v1alpha1`, portable to Cosmonic Control).

The easiest path — `make deploy` creates the storage directory, applies the
Workload, waits for it to run, and prints how to reach it:

```sh
make deploy        # pulls ghcr.io/liamrandall/oci-registry, data in ~/.cosmonic/oci-registry-data
make undeploy      # remove it (on-disk data preserved)
```

Under the hood it POSTs to the Cosmonic Desktop daemon's unix socket
(`/v1/workloads`, JSON). The daemon pulls the image, pins it by digest, mounts
the `hostPath` volume, and starts the reactor.

Once running, the host's HTTP ingress routes by `Host` header:

```sh
curl -H 'Host: oci-registry.localhost' http://127.0.0.1:8200/v2/
wash oci push --insecure oci-registry.localhost/myimage:tag ./component.wasm
```

The Workload mounts a `hostPath` volume at `/data`, so the registry's content
lives on your local disk and survives restarts. (Use an `ephemeral` volume
instead if you don't need persistence — see the manifest.)

## Development

```sh
make build              # cargo build --target wasm32-wasip2 --release
make test               # unit tests (host) + integration (wash dev + oras)
make test-unit          # just the host-target tests
make lint               # clippy -D warnings
make dev                # run under wash dev
```

Requires the `wasm32-wasip2` Rust target (pinned in `rust-toolchain.toml`),
`wash` ≥ 2.2, and `oras` (for the integration test).

## CI & releases

- **CI** (`ci.yml`): fmt, clippy, host tests, build, and a real `wash dev` +
  `oras` push/pull round trip on every push/PR.
- **Release** (`release.yml`): tag `vX.Y.Z` → re-runs CI, publishes
  `ghcr.io/liamrandall/oci-registry:{latest,X.Y.Z}` with SLSA provenance
  attestation, and cuts a GitHub Release with the `.wasm` attached.

## License

Apache-2.0
