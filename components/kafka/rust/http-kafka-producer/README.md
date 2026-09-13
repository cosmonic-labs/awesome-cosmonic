# http-kafka-producer (Rust)

See `../../README.md` for when to choose this pattern over the others.

## Build

Prereqs: Rust 1.85+, `rustup target add wasm32-wasip2`, and `wash` 2.7+.
This template was built with `wash 2.7.0` and tested against Cosmonic Control
0.11.0 with `cosmonic:kafka@0.5.0`.

The `cosmonic:kafka@0.5.0` WIT comes from a registry rather than from this
repository, and `wash` has to be told which registry serves the `cosmonic`
namespace. Export that once per shell — `wash build` fetches too, so
prefixing a single `wash wit fetch` is not enough:

```sh
export WKG_CONFIG_FILE="$PWD/../../wkg-registries.toml"
wash build                   # fetches the WIT, then runs .wash/config.yaml
# component: target/wasm32-wasip2/release/http_kafka_producer.wasm
```

`wkg.lock` pins the exact versions and `wit/deps/` is gitignored, so the
first build is what populates it. `cargo build --release` produces the same
component; `.wash/config.yaml` just names that command and where its output
lands, which is what lets tooling find the artifact without being told.

To stop passing the variable, merge the entries from
[`../../wkg-registries.toml`](../../wkg-registries.toml) into
`~/.config/wasm-pkg/config.toml` once.

## Deploy

- **Cosmonic Desktop**: submit `workload.yaml` through its workload API or MCP
  integration.
- **Kubernetes** (wasmCloud runtime-operator / Cosmonic Control): run
  `kubectl apply -f deploy/workload-deployment.yaml`.

Both point at the component published from this template. Set the broker and
topic placeholders before applying a manifest. Once you change the source,
build it, push it to your own registry, and replace the image reference.

The broker address and topic names in the manifests are placeholders. The
workload's `cosmonic:kafka` entry under `hostInterfaces` is where the
connection lives — broker, credentials, client policy, and the topic grant.
The component cannot supply or override these values. Use `secretFrom` for the
credential rather than inlining it.

See [the pattern guide](../../README.md#where-the-broker-and-credentials-are-configured)
for binding and topic-grant rules.

## Design notes

Producer functions use the binding's host-owned native client directly. The
component creates no Kafka client per HTTP request.

## API

- `POST /produce?topic=T&key=K&value=V` sends one record and returns its
  `partition:offset` acknowledgement.
- `POST /produce-batch?topic=T&count=N&size=S` sends 1–10,000 records with
  values up to 1 MiB and a total value payload up to 16 MiB. It returns one
  delivery result per line. The response can contain both successes and
  failures because batch outcomes are positional.

The requested topic must be present in the binding's `topics` grant.
