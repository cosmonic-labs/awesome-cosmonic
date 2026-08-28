# s3-host-plugin

An **S3 backend for `wasmcloud:blobstore`**, built as a host component plugin —
a Wasm component that signs and sends S3 requests itself, so a workload names a
container and an object and never sees a credential.

> **Status: experimental.** Verified end to end against
> [RustFS](https://github.com/rustfs/rustfs) running locally, archiving Kafka
> batches through [`../kafka/example/`](../kafka/example/). Writes are buffered
> and streams both ways: 256 MiB uploads in ~3s and reads back byte-exact, with
> peak memory of one 8 MiB part regardless of object size. No TLS testing
> against a hosted provider yet.

## Why this shape

The Kafka plugin next door earns its keep by holding a *stateful* client. S3 is
stateless request/response, so the argument here is different: it is about where
the credentials live, and about being swappable.

The plugin's config is operator-controlled, and so is its `allowedHosts` policy.
A workload gets containers and objects; the access key, the secret, the region,
and the endpoint stay on the other side of the store boundary. Swapping RustFS
for AWS is a config change to the plugin, and swapping S3 for the host's
filesystem backend is a deployment change — neither is a rebuild of any workload
that stores an object.

## Interface

`wasmcloud:blobstore@0.1.0`, unmodified. There is no interface of this project's
own, which is the point: a workload written against the standard blobstore gets
S3 by deploying this plugin, and the host's built-in filesystem or NATS backend
by not deploying it. The workload is identical either way.

Two properties of that package make it servable from a plugin at all:

- **It is fully `async func`.** A plugin's capabilities are installed on a
  caller's linker as concurrent host functions, so a sync interface fails to
  bind with "type mismatch with async".
- **Object bodies are `stream<u8>`**, not resource wrappers. A `stream<u8>`
  crosses a plugin/workload store boundary; an arbitrary resource handle does
  not. Its one `container` resource is fine because a plugin's *exported*
  resources are proxied across that boundary by the host.

`wasi:blobstore@0.2.0-draft` has neither property — sync functions, and four
resources including `incoming-value`/`outgoing-value` around the bodies — so it
cannot be served this way regardless of backend.

## Configuration

Delivered over the plugin's `wasi:config/store` import from its own `config:`
block. Not environment variables: a plugin store is built without an
environment.

| Key | Required | Meaning |
|---|---|---|
| `endpoint` | yes | Base URL, e.g. `http://192.168.1.10:9100` |
| `access-key` | yes | |
| `secret-key` | yes | |
| `region` | no | Signing region, default `us-east-1` |
| `write-strategy` | no | `multipart` (default), `chunked`, or `buffered` |
| `write-strategy.<container>` | no | Overrides the above for one container |

### Upload strategies

`write-data` takes a `stream<u8>` and no length, which is what decides this:

- **`multipart`** (default) — `CreateMultipartUpload` → N × `UploadPart` →
  `CompleteMultipartUpload`. Each part carries its own length, so nothing has to
  be known in advance. Peak memory is one part (8 MiB) whatever the object's
  size. **The only strategy that works for a stream of unknown length.**
- **`chunked`** — one `PUT` with `Content-Encoding: aws-chunked` and the
  `STREAMING-UNSIGNED-PAYLOAD-TRAILER` sentinel. One round trip and a small
  buffer, but it must declare `x-amz-decoded-content-length` before the first
  byte, so the total size has to be known up front. Streaming escapes
  *buffering*, not *knowing the length*. This plugin buffers up to 64 MiB
  trying to learn it and then refuses by name. **RustFS rejects it outright**
  (closes the connection mid-body), so treat it as unproven.
- **`buffered`** — collect and `PUT`. The baseline; bounded by memory.

```yaml
- id: cosmonic-s3
  file: ../../s3/target/wasm32-wasip2/release/s3_host_plugin.wasm
  config:
    endpoint: http://192.168.1.10:9100
    access-key: rustfsadmin
    secret-key: rustfsadmin
  allowedHosts:
    - http://192.168.1.10:9100
```

**`allowedHosts` is required**, and is stricter than a workload's: an omitted
list denies every outbound host rather than allowing all, because a plugin is
operator-controlled and more privileged than what it serves. It is also the
thing that bounds where those credentials can be sent.

**Do not use `127.0.0.1`.** A component's connect to a loopback address is
served by the host's in-process virtual network and never reaches the OS, so it
fails as a connection error. Use an address that routes.

## Running against RustFS

```console
IP=$(ipconfig getifaddr en0)        # or `hostname -I | awk '{print $1}'` on Linux

docker run -d --name rustfs-demo -p 9100:9000 \
  -e RUSTFS_ACCESS_KEY=rustfsadmin -e RUSTFS_SECRET_KEY=rustfsadmin \
  rustfs/rustfs:latest

AWS_ACCESS_KEY_ID=rustfsadmin AWS_SECRET_ACCESS_KEY=rustfsadmin \
  aws --endpoint-url http://$IP:9100 s3 mb s3://kafka-archive
```

Then point `endpoint` and `allowedHosts` at `http://$IP:9100`.
[`../kafka/example/`](../kafka/example/) uses this plugin to archive each Kafka
batch, and is the end-to-end exercise:

```console
$ aws --endpoint-url http://$IP:9100 s3 ls s3://kafka-archive/ --recursive
2026-08-17 12:34:34  194 demo/partition-0/000000000000.txt
2026-08-17 12:34:34  169 demo/partition-1/000000000000.txt
2026-08-17 12:34:34  194 demo/partition-2/000000000000.txt

$ aws --endpoint-url http://$IP:9100 s3 cp s3://kafka-archive/demo/partition-0/000000000000.txt -
0 key4 archive event 4
1 key6 archive event 6
...
```

## Build

```console
wash build --skip-fetch
```

`--skip-fetch` because every WIT dependency is vendored under `wit/deps/`,
including `wasmcloud:blobstore` itself.

## Implementation notes

Two things were not obvious and cost a debugging cycle each:

- **The transport is p2 `wasi:http/outgoing-handler@0.2.2`, not p3.** A plugin's
  egress is served by the host's own outgoing handler, which is wired to the p2
  interface. Importing `wasi:http/handler@0.3.0` instead fails the plugin's load
  with "instance export `handle` has the wrong type" — the instance resolves,
  the function does not.
- **The request body must be written *after* `outgoing_handler::handle()`, not
  before.** Nothing drains an outgoing body until the request is in flight, so
  writing first works for a small body that fits the internal buffer and
  deadlocks forever for anything larger. This presents as a hang with no error,
  at no particular size — 600 bytes worked here and 1 MiB did not.
- **`content-length` must be set explicitly.** Without it the transport falls
  back to chunked encoding and S3 answers `411 MissingContentLength`; it will
  not take a body whose length it does not know up front.

SigV4 is hand-rolled (`src/sigv4.rs`), and **not** because the AWS SDK fails to
build: `aws-sdk-s3` with `default-features = false` plus `aws-smithy-wasm` does
compile for `wasm32-wasip2` (the trap is `rt-tokio`, which drags in tokio's
`net`). The reason is its transport. `aws-smithy-wasm`'s HTTP client is built on
`wstd`, which brings its own reactor and panics with *"Reactor::current must be
called within a wstd runtime"* when driven by wit-bindgen's executor — which is
what a plugin's async exports run on. Using the SDK here would mean writing a
smithy `HttpClient` over wit-bindgen's `wasi:http` first; that is a genuinely
attractive next step, since it would bring retries, pagination, and modelled
errors, and would retire this module.

`hmac` and `sha2` are the only dependencies, both pure Rust.

## Known limitations

- **A hard trap leaks a multipart upload.** Every error path aborts the upload
  (an RAII guard, so a path added later cannot forget), but a wasm trap runs no
  destructors and the plugin's store is rebuilt from scratch. A bucket lifecycle
  rule expiring incomplete uploads is the only backstop for that case.
- **`chunked` is unproven**: structurally it needs the length up front, and
  RustFS rejects it regardless. See [Upload strategies](#upload-strategies).
- `object-info` gets its size from a prefixed listing rather than a `HEAD`,
  because this transport does not surface response headers. `created-at` is
  reported as 0 for both objects and containers rather than invented.
- Listings are not paginated, so a container with more objects than one
  `ListObjectsV2` page returns is truncated.
- `delete-objects` issues one request per key rather than S3's batch delete,
  which needs an MD5 of the request body.
- No TLS testing against a hosted provider; only plain HTTP to a local server.
- Listing and error parsing use a minimal XML scan, not a parser. Fine for S3's
  flat, machine-generated documents; wrong the moment it needs namespaces.

## License

Apache-2.0. See [LICENSE](LICENSE).
