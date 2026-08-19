# kafka-publisher-example

An HTTP workload that publishes to Kafka through the
[`kafka-host-plugin`](../) next door. It exists to exercise the producer path
end to end under `wash dev`.

The interesting part is what the workload does *not* have: no Kafka client, no
broker address, no connection to keep warm. It imports
`cosmonic:kafka/producer` and the host routes each call across a store boundary
into the plugin, which owns all of that. The workload stays ephemeral.

## Requirements

- A `wash` built with the `host-component-plugins` feature — release binaries
  ship default features only:
  ```console
  cargo install --path ./crates/wash --features host-component-plugins
  ```
  Without it, `wash dev` refuses the config with *"dev.host_plugins requires a
  wash build with the `host-component-plugins` feature"*.
- A Kafka-protocol broker reachable at a **routable** address (see below).

## Run

Start a broker. Note the two addresses: it must *listen* somewhere reachable and
*advertise* that same address, because a Kafka client uses the bootstrap address
only to fetch cluster metadata and then reconnects to whatever that metadata
names.

```console
IP=$(ipconfig getifaddr en0)          # or `hostname -I | awk '{print $1}'` on Linux

docker run -d --name kafka-demo -p 29092:29092 \
  docker.redpanda.com/redpandadata/redpanda:latest \
  redpanda start --overprovisioned --smp 1 --memory 1G --node-id 0 --check=false \
    --kafka-addr external://0.0.0.0:29092 \
    --advertise-kafka-addr external://$IP:29092

docker exec kafka-demo rpk topic create demo --brokers $IP:29092
```

Put that same `$IP:29092` in `bootstrap-servers` in
[`.wash/config.yaml`](.wash/config.yaml). **Do not use `127.0.0.1`** — a
component's connect to a loopback address is served by the host's in-process
virtual network rather than the OS, so it fails as
`connection("No host reachable")`.

Build the plugin, then start the dev session from this directory:

```console
cd .. && wash build --skip-fetch && cd example
wash dev
```

`wash dev` builds and deploys *this* component; it only loads the plugin from
the path in `dev.host_plugins`, so the plugin has to be built first.

## Use

```console
$ curl -X POST 'localhost:8000/publish?topic=demo' --data 'hello from a wasm workload'
published 26 bytes to demo partition 0 offset 0

$ curl -X POST 'localhost:8000/publish?topic=demo&key=user-1' --data 'keyed record'
published 12 bytes to demo partition 0 offset 1
```

The partition and offset come back from the broker, through the plugin, to the
workload. Read them back independently to confirm:

```console
$ docker exec kafka-demo rpk topic consume demo --brokers $IP:29092 --num 2 \
    --format '%p:%o key=%k value=%v\n'
0:0 key= value=hello from a wasm workload
0:1 key=user-1 value=keyed record
```

Pull them back with `/consume`. The plugin holds the offsets, so this workload
can be rebuilt between polls without losing its place — and each poll picks up
where the last one stopped:

```console
$ curl 'localhost:8000/consume?max=2'
0:0 ts=1786321802345 key=-      value=hello from a wasm workload
0:1 ts=1786321810527 key=user-1 value=keyed record
(2 records)
```

`poll` marks records consumed but does not commit them — `&commit=1` does that,
because only the caller knows whether it actually dealt with them. Committed
offsets go to the group, so they survive a restart and show up in ordinary Kafka
tooling:

```console
$ curl 'localhost:8000/consume?max=10&commit=1'
...
committed
$ docker exec kafka-demo rpk group describe wasmcloud-kafka-plugin --brokers $IP:29092
TOPIC  PARTITION  CURRENT-OFFSET  LOG-END-OFFSET  LAG
demo   0          12              12              0
```

Consuming needs `topics` set on the plugin in
[`.wash/config.yaml`](.wash/config.yaml); it is what the plugin subscribes to.

Failures keep their shape: a missing `topic` is a 400 from the workload, and a
plugin-side `kafka-error` comes back as a 502 naming which variant it was —
`not-configured` for a missing `bootstrap-servers`, `connection` for a broker it
cannot reach.

## The push direction

With `trigger: "on"` in [`.wash/config.yaml`](.wash/config.yaml), nothing has to
poll: the plugin's own loop pushes each batch into this component's
`cosmonic:kafka/handler` export. Publishing is enough to make it run.

The handler also archives each batch through `wasmcloud:blobstore` — the
standard interface, so the same code stores to S3, the filesystem, or NATS
depending only on which plugins are deployed. It republishes to
`<topic>.processed` too, because a workload instance is ephemeral and will not
survive to answer a later question about what it saw:

```console
$ curl -X POST 'localhost:8000/publish?topic=demo&key=trig' --data 'pushed by trigger'
published 17 bytes to demo partition 0 offset 15
$ docker exec kafka-demo rpk topic consume demo.processed --brokers $IP:29092 --num 1 \
    --format '%o key=%k value=%v\n'
0 key=trig value=handled offset 15: pushed by trigger
```

Returning an error from `handle` means the plugin does not commit, so the batch
comes back. That makes the transform above idempotent-by-construction: rerunning
it writes the same record to the same output topic.

Note that the config declares no `dev.host_interfaces`. `wash dev` derives a
workload's host interfaces from the component's exports as well as its imports,
so the exported `handler` reaches the manifest beside the imported `producer`,
`consumer`, and `types`, and the plugin's trigger has a target to dispatch to.

## Two plugins, one workload

The handler also archives each batch to object storage through
[`cosmonic:s3`](../../s3/), so the dev session binds *two* plugins to this one
component. Neither credential set is visible here: Kafka's brokers and S3's
access key both live in the plugins' own `config:` blocks.

One object per batch, keyed by topic, partition, and first offset — which is
what makes the handler idempotent under the at-least-once redelivery the trigger
promises. Replaying a batch after a crash overwrites the same object instead of
appending a second copy:

```console
$ aws --endpoint-url http://$IP:9100 s3 ls s3://kafka-archive/ --recursive
demo/partition-0/000000000000.txt
demo/partition-1/000000000000.txt
demo/partition-2/000000000000.txt

$ aws --endpoint-url http://$IP:9100 s3 cp s3://kafka-archive/demo/partition-0/000000000000.txt -
0 key4 archive event 4
1 key6 archive event 6
...
```

Running this needs RustFS up and the `kafka-archive` bucket created — see the
[S3 plugin README](../../s3/README.md).

## Large objects, and the two upload strategies

`/bigwrite` and `/bigread` exist to prove the streaming claim at a size where
buffering would fail. The bytes are generated into the stream rather than built
first, so neither the workload nor the plugin ever holds the object:

```console
$ curl -X POST 'localhost:8000/bigwrite?mode=multipart&mb=256'
wrote 256 MiB to bench-multipart/256mb.bin via multipart      # ~3s

$ curl 'localhost:8000/bigread?mode=multipart&mb=256'
read 268435456 bytes from bench-multipart
```

`mode` selects which container is written, and the plugin's
`write-strategy.<container>` config maps that container to an upload strategy —
so the same payload can be pushed through `multipart` and `chunked` and
compared. The short version of that comparison: multipart handles a stream of
unknown length, chunked cannot, and RustFS rejects chunked anyway. See the
[S3 plugin README](../../s3/README.md#upload-strategies).

Both routes need the `bench-multipart` and `bench-chunked` buckets to exist.

## A note on the WIT

`wit/deps/cosmonic-kafka/package.wit` is a byte-identical copy of the plugin's
own `../wit/deps/cosmonic-kafka/package.wit`. WIT dependencies resolve from a
crate's own `wit/deps/`, so a consumer needs its own copy; keep the two in sync
when the interface changes.

## Why this component is `wasi:http/handler@0.3.0`

A host component plugin's capabilities are installed on this component's linker
as *concurrent* host functions, so calling `producer::send` means awaiting it —
and only an async export has somewhere to await from. Under p2's sync-lifted
`incoming-handler` the request's response channel is dropped before the plugin
answers, and the request fails with no trap to point at. The same constraint is
why `cosmonic:kafka`'s functions are declared `async func` at all; see the
[plugin README](../README.md#everything-is-async-func-and-it-has-to-be).

## License

Apache-2.0. See [../LICENSE](../LICENSE).
