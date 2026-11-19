# jco-node

The host side of `jco:node@0.1.0` as a native wasmCloud host plugin.

[Jco](https://github.com/bytecodealliance/jco) builds components from JavaScript
that use Node.js builtins (`node:fs`, `node:http`, `node:cluster`, …).
Where a builtin needs the outside world,
the component imports a `jco:node/<interface>@0.1.0` interface,
and Jco's transpiled output serves it from Node.
This crate serves the same contract from Rust,
so those components run on a `wash-runtime` host with no JavaScript engine outside the component.

Work in progress: see [Status](#status).

## Contents

- `src/plugin.rs`: `JcoNode`, the `HostPlugin`, and `add_to_linker` for embedders that link by hand.
- `src/hosts/`: one module per `jco:node` interface.
- `src/callbacks.rs`: `Pump`, which delivers host events (timers, HTTP requests, datagrams, TLS socket events)
  into the guest's callback exports between calls.
- `src/wasi_tls.rs`: `NodeTlsProvider`, a `wasi:tls` provider that behaves as Jco's Node one,
  for components whose `node:https` runs over `wasi:sockets`.
- `src/bin/jco-node-run.rs`: a minimal embedder that runs one component and calls its exports.
  The test suites drive it.
- `wit/`: the plugin's world; vendored WIT under `wit/deps/` (provenance in [`wit/deps/README.md`](wit/deps/README.md)).
- `tests/conformance/`: Jco's own fixtures and expectations, run on this host.
- `tests/examples/`: Jco's example components and their e2e tests, run on this host.

## Requirements

- Rust 1.95 or later.
- For the tests:
  Node.js 22 or later,
  and a Jco checkout with `packages/jco` built (`pnpm install && pnpm build`), named by `JCO_DIR`.
  The example suite also needs each example's dependencies installed (`pnpm install` in the Jco checkout).

Tested against `wash-runtime` 2.12.1
(wasmCloud at `3903d076d656ff4abea412b6f0fae139b535e7ed`, pinned in `Cargo.toml`)
with wasmtime 48.

## Build

```console
cargo build
```

## Use

### In a host

Register the plugin;
a workload names the interfaces it grants in `host_interfaces`:

```rust
let plugin: Arc<dyn HostPlugin> = Arc::new(jco_node_host::JcoNode::new());
```

```yaml
hostInterfaces:
  - namespace: jco
    package: node
    interfaces: [console, fs, http, process]
    version: 0.1.0
    config:
      deny: process   # listed, but refused
```

Every `jco:node` interface is linked whether granted or not,
so a component instantiates.
A call to an interface that isn't granted refuses
the way Jco's denial providers do:
an `ERR_JCO_<NAME>_ADAPTER_REQUIRED` error, or a trap where the interface has no error channel.
To list an interface but refuse it, name it in `deny`, comma separated.

Callbacks (timer fires, incoming HTTP requests, datagrams, TLS socket events, cluster events) are queued by the host
and must be delivered between guest calls with `callbacks::Pump`;
see `jco-node-run` for the loop.

`wasi:tls`, and `jco:node/tls`'s `start-tls` that `node:https` over `wasi:sockets` uses,
run on the engine's `wasi:tls` provider.
To have them behave as under Jco's Node provider
(ALPN `http/1.1`, chosen CAs, a handshake timeout, Node's error messages),
install `wasi_tls::NodeTlsProvider`:

```rust
let provider = NodeTlsProvider::new(&[ca_pem], Duration::from_secs(10))?;
let engine = Engine::builder().with_tls_provider(SharedTlsProvider::new(provider));
```

### From the command line

```console
cargo run --bin jco-node-run -- app.wasm --grant os,process --invoke run --args '[]'
cargo run --bin jco-node-run -- server.wasm --grant http --reply-fd 3 3>replies   # session mode
```

`jco-node-run --help` lists the options.

## Test

```console
cargo test --lib
JCO_DIR=/path/to/jco node --test --test-concurrency=1 tests/conformance/*.test.mjs
JCO_DIR=/path/to/jco node --test --test-concurrency=1 tests/examples/examples.test.mjs
```

Fixtures are componentized from the Jco checkout on first use
and cached in `target/conformance/` (about half a minute each).
Each suite names the Jco test it ports;
expectations are Jco's.
`guest.test.mjs` covers fixtures whose builtins live entirely in the component,
which must run here with nothing granted.

## Status

Every `jco:node@0.1.0` interface is implemented.
Where the host can't match Node, it says so in the same terms Jco's providers use:

- `inspector`: no inspector protocol backend.
  `open` and `session.connect` are refused as unavailable.
- `worker-threads`: a worker's body is JavaScript, and this host has no engine outside the component,
  so `new Worker()` fails with `ERR_JCO_UNSUPPORTED_NODE_API`.
- `tls`: TLS sockets and servers run on rustls.
  This covers client certificates, ALPN, SNI contexts, `checkServerIdentity`,
  peer certificates (with the self-signed `issuerCertificate` cycle) and keying-material export.
  So do secure contexts and `startTLS` upgrades over `wasi:tls`.
  Unsupported, as rustls has no equivalent:
  - session export and resumption (`getSession` and `getTLSTicket` return `undefined`, and `setSession` is refused)
  - renegotiation (it fails)
  - `enableTrace`, and `setKeyCert` on a live socket
  - `getFinished` and `getPeerFinished` (they return `undefined`)

  `getCACertificates("bundled")` lists the system store.
  `https` and `http2` serve and connect over TLS themselves.
- `cluster`: forking needs an embedder-supplied `WorkerLauncher`.
  `jco-node-run` re-runs itself on the same component, as Node re-runs its entry,
  and talks to the worker over a socket pair.
  Without a launcher, `fork` is unsupported.
- `ffi`: native calls take at most eight integer and eight floating-point arguments,
  on x86-64 and AArch64 System V.
- Imports block, so a guest can't be a client of a server in its own instance.

### Coverage

Run here, all passing:
every `jco:node` interface's fixture granted and refused
(child-process, cluster, console, dgram, dns, ffi, fs, http, http2, https, inspector, os, process, sqlite,
tls, trace-events, tty, v8, wasi, worker-threads, zlib),
`node:https` over `wasi:sockets` with `wasi:tls` (https-wasi-tls),
Express, the VFS in direct and WASI modes,
the guest-side builtins (assert, buffer, errors, events, globals, module, path, querystring, readline,
repl, stream, string_decoder, url's cwd case, vm),
and the examples node-mcp-server, node-grpc-server, node-sveltekit-server and nodejs-platform
through their own e2e tests.

Marked todo, by design:
inspector `eval` (no protocol backend),
real worker threads (JavaScript bodies),
and http over `wasi:sockets` alone (see below).
https-wasi-tls's public-network case runs only with `JCO_NODE_PUBLIC_NETWORK=1`.
Its resource-counter assertions read internals of Jco's JS provider.
They're replaced by what they stand for, observed from outside:
the peer server's handshakes and connections, and the host exiting cleanly.
Under wash-runtime, a guest's `127.0.0.1` is its own virtual network,
so the guest reaches the test's servers as `host.wasmcloud.internal`
on ports granted with `jco-node-run --allow-host-loopback`.

Not ported:
url's comparison against native Node 24;
and fixtures Jco builds with bespoke bundling and no `jco:node` imports
(net, path-p3, perf-hooks, stream-classic, test, timers, util).

Jco's WASI WIT differs from upstream in two places the plugin papers over
for components that import `jco:node`:
`wasi:tls/types@0.2.0-draft` gains `is-available`,
and `wasi:sockets/network@0.2.10` gains `network.noop`.
A component that imports neither `jco:node` nor a plugin-bound interface
needs its host's WASI to provide them.
