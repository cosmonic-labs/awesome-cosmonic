# kafka-host-plugin

A Kafka capability for wasmCloud, built as a **host component plugin** — a Wasm
component that speaks the native Kafka wire protocol over `wasi:sockets`, so the
Kafka client runs inside the sandbox rather than in privileged host code.

> **Status: experimental, but working.** Producing, consuming, and consumer-group
> membership are all verified end to end against a live broker — Redpanda and
> Apache Kafka 4.3.1 — from a wasm workload, over `wasi:sockets` (see
> [Test results](#test-results)). There is **no Kafka client library**: every
> protocol API is encoded here, because no published pure-Rust client both
> builds for `wasm32-wasip2` and speaks a current broker's protocol. See
> [Client library](#client-library-there-isnt-one). No TLS or SASL yet.

## Why this shape

Every serverless platform solves Kafka the same way, because a Kafka client is
inherently stateful — TCP connections to every broker, cached topic metadata,
partition leadership, consumer-group membership with heartbeats. None of that
survives a per-request instance. So the client goes somewhere long-lived and the
ephemeral user code gets handed a batch:

| Platform | Where the Kafka client runs |
|---|---|
| AWS Lambda | Lambda service's own event pollers (native TCP into your VPC) |
| Azure Functions | Functions host process, `Confluent.Kafka` → `librdkafka` |
| Spin | `spin-trigger-kafka`, a native host binary (abandoned 2023) |

A wasmCloud host plugin is the same split — a single pinned, host-scoped
instance serving many ephemeral workloads. The difference here is that the
poller is *itself a sandboxed component*, versioned and shipped like any other,
rather than privileged platform infrastructure. That is the part no other
platform does.

[`example/`](example/) is a workload that publishes and consumes over HTTP, and
imports nothing Kafka-shaped but `cosmonic:kafka`.

## Interfaces

**`cosmonic:kafka/producer`** — Kafka-native. Keys, partitions, offsets survive.

```wit
send: async func(topic: string, key: option<list<u8>>, value: list<u8>)
    -> result<produce-ack, kafka-error>;
send-batch: async func(topic: string, records: list<tuple<option<list<u8>>, list<u8>>>)
    -> result<list<produce-ack>, kafka-error>;
```

**`cosmonic:kafka/consumer`** — pull-based.

```wit
poll: async func(max-records: u32, timeout-ms: u32) -> result<list<kafka-record>, kafka-error>;
commit: async func() -> result<_, kafka-error>;
```

### Everything is `async func`, and it has to be

A host component plugin's capabilities are installed on a calling workload's
linker as *concurrent* host functions. A plain `func` therefore cannot bind at
all: the workload fails to deploy with

```
component imports instance `cosmonic:kafka/producer@0.1.0`, but a matching
implementation was not found in the linker
  0: instance export `send` has the wrong type
  1: type mismatch with async
```

Two things follow. First, a caller must have somewhere to await from, so a
workload importing this needs an async export — `wasi:http/handler@0.3.0`, not
p2's `incoming-handler`. Under a sync-lifted p2 export the request's response
channel is gone before the plugin answers, and the request fails with no trap to
read. Second, **there is no `wasmcloud:messaging/consumer` export here.** That
interface is defined upstream with plain `func`, so a drop-in publish path would
need it redeclared `async` — which makes it a different package from the one
already-written workloads import, and so not a drop-in at all.

**`cosmonic:kafka/handler`** — the push direction, exported by a *workload* and
called by the plugin.

```wit
handle: async func(records: list<kafka-record>) -> result<_, string>;
```

This is what makes the plugin a trigger rather than something you poll. Its own
`wasi:cli/run` holds the consumer group, polls, and hands each batch to a
workload that exports `handle`, so the workload runs when there is work instead
of having to ask for it. Returning an error means the batch was not processed:
the plugin leaves the offsets alone and the records come back. Delivery is
at-least-once, so `handle` must be idempotent.

Enable it with `trigger: "on"`; without that the plugin is pull-only and the
loop never starts. See [Trigger](#trigger).

## Configuration

Delivered over the plugin's `wasi:config/store` import from its own `config:`
block in the host manifest. Not environment variables: a plugin store is built
without an environment, so `wasi:cli/environment` is present but always empty.

| Key | Required | Meaning |
|---|---|---|
| `bootstrap-servers` | yes | Comma-separated `host:port` |
| `topics` | for consuming | Comma-separated topics to subscribe |
| `consumer-group` | no | Defaults to `wasmcloud-kafka-plugin` |
| `partition-assignment` | no | `group` (default) joins the consumer group; `static` takes every partition |
| `session-timeout-ms` | no | Group session timeout, default 45000 |
| `producer-acks` | no | `all` (default), `one`, or `none` |
| `producer-compression` | no | `none` (default), `gzip`, or `snappy` |
| `trigger` | no | `on` starts the dispatch loop; off by default |
| `trigger-batch-size` | no | Records per dispatched batch (default 32) |
| `trigger-max-inflight` | no | Concurrent batches across partitions (default 8) |
| `trigger-max-attempts` | no | Redeliveries before dead-lettering (default 3) |
| `trigger-dlq-topic` | no | Defaults to `<topic>.dlq` |

`producer-acks` defaults to `all`, not the more usual `one`, because `send`
hands the caller a `produce-ack` — and a caller holding an offset has been told
its record is safe. Under `one` that is not true: the leader acknowledges before
its followers have the record, so losing the leader in that window loses a
record the workload was told had landed. `all` is only as strong as the topic's
`min.insync.replicas`, and on a single-broker cluster it is exactly `one`. Use
`none` only for a caller that ignores the ack; no offset is assigned, so `send`
reports `-1`.

`producer-compression` defaults to `none` because it is the setting that cannot
surprise anyone — a consumer too old for the codec fails on read rather than at
produce time. The codecs offered are the same ones the fetch path decodes, so
records this plugin writes are records it can read back, and both are pure Rust
(`flate2`, `snap`) so neither costs a dependency the decoder did not already
need.

```yaml
dev:
  host_plugins:
    - id: cosmonic-kafka
      file: ../target/wasm32-wasip2/release/kafka_host_plugin.wasm
      config:
        bootstrap-servers: 192.168.1.10:9092
```

**A broker on `127.0.0.1` is not reachable.** A component's connect to a
loopback address is served by the host's in-process virtual network — which
components use to reach *each other* — and never touches the OS loopback. There
is no Kafka listening there, so it surfaces as
`connection("No host reachable")`. Give the plugin an address that routes, and
make sure the broker *advertises* that same address: a Kafka client takes the
bootstrap address only to fetch cluster metadata, then reconnects to whatever
that metadata names.

If the plugin resolves a broker by name rather than by address, the host must
allow it — DNS is denied by default and gated by `allowedIpNameLookups` on the
plugin spec. Outbound TCP *connect* needs no allowlist; the loader denies only
`tcp-bind` and `udp-bind`.

## Consumer groups

The consumer **joins its group** — `JoinGroup`, `SyncGroup`, `Heartbeat`,
`LeaveGroup` — rather than assigning itself every partition.

This is what makes running the plugin on more than one host safe. Static
assignment is simpler and is silently wrong the moment a second consumer exists:
both read every partition, every record is handled twice, and the two commit
over each other's offsets so neither's progress means anything. Nothing detects
it. Joining the group makes the coordinator divide the partitions instead, and
puts a **generation** on every commit — so a member whose partitions were taken
away while a batch was in flight is answered `ILLEGAL_GENERATION` rather than
being allowed to move the new owner's offset past records it never saw. The
plugin treats that as a signal to rejoin, and the records are redelivered to
whoever owns the partition now.

Assignment is computed client-side by whichever member the coordinator elects
leader, which is why the strategy travels as a protocol name. This plugin
implements **`range`**, Java's classic default, byte-compatible with
`RangeAssignor` — so the group can be shared with stock consumers rather than
only with copies of itself:

```console
$ rpk group describe wasmcloud-kafka-plugin
STATE     Stable
BALANCER  range
MEMBERS   2

TOPIC  PARTITION  MEMBER-ID
demo   0          console-consumer-593cb94e-…      <-- stock Java consumer
demo   1          console-consumer-593cb94e-…
demo   2          wasmcloud-kafka-plugin-3ba07a65-…
```

That group was formed with the plugin as leader: the assignment above is the one
`src/group.rs` computed, and the Java consumer accepted it and read from it. Of
12 records published across the two, each was handled exactly once — 8 by the
console consumer, 4 by the plugin, none by both.

Set `partition-assignment: static` to opt out and take every partition
unconditionally. It is one fewer moving part for a single-host deployment, and
it is the configuration that double-reads if that assumption ever stops holding.

**Heartbeats are sent from the poll path**, not from a background thread —
there isn't one to have. In practice the trigger loop polls far more often than
the interval (a third of `session-timeout-ms`), but a handler that occupies the
plugin for longer than the session timeout will be evicted mid-batch and its
partitions reassigned. Raise `session-timeout-ms` past the slowest handler, or
leave the group with `partition-assignment: static`.

## Trigger

With `trigger: "on"`, the plugin's own `wasi:cli/run` polls the configured
topics and pushes each batch into a workload that exports
`cosmonic:kafka/handler`. Nothing calls the plugin; the plugin calls you.

```yaml
config:
  bootstrap-servers: 192.168.1.10:9092
  topics: demo
  trigger: "on"
  trigger-batch-size: "8"
```

Exporting `handler` is all the workload has to do. `wash dev` derives the
workload's host interfaces from the component's exports as well as its imports,
so `cosmonic:kafka` reaches the manifest with `handler` among its interface
names on its own — no `dev.host_interfaces` block. See
[`example/.wash/config.yaml`](example/.wash/config.yaml).

Three properties are worth knowing before relying on it:

- **Offsets move only after `handle` returns `ok`.** An error, a stopped
  workload, or a plugin restart mid-batch all leave the offsets where they were,
  so the records are redelivered. That is at-least-once, and it makes an
  idempotent handler a requirement rather than a nicety.
- **Dispatch is concurrent across partitions**, one in-flight batch per
  partition, capped by `trigger-max-inflight` (default 8). Partitions are the
  unit because Kafka orders records within one and commits one high-water mark
  per one: they are exactly the pieces that can be handled, and committed,
  independently. More parallelism therefore means more partitions — the same
  answer Kafka gives every other consumer. A failed batch rewinds only its own
  partition; its neighbours still commit.
- **One handler workload at a time.** A target handle scopes a whole task, so
  batches all go to one workload id; the concurrency is across that workload's
  *instances*, which is what the host spins up per in-flight call.

The loop suspends between passes (`wasi:clocks/monotonic-clock.wait-for`) rather
than spinning. That is not a nicety either: the trigger shares the plugin's store
with every workload's capability calls, and a loop with no suspension point
starves them — a `producer.send` from an HTTP workload simply never returns.

## Build

```console
wash build --skip-fetch
```

`--skip-fetch` because `cosmonic:kafka` is local-only and has no registry to
resolve from; every WIT dependency is vendored under `wit/deps/`.

Two build constraints are load-bearing:

- **`wasm32-wasip2`.** `std::net` only lowers to `wasi:sockets` on p2. A wasip1
  core module wrapped with the reactor adapter has no TCP connect at all.
- **wit-bindgen ≥ 0.60.** Older releases encode an `async func` as the extern
  name `[async]send`, which no current `wit-component` will decode; the build
  fails at componentization with "not in kebab case" after compiling cleanly.

## Deploy

Requires a `wash` built with the `host-component-plugins` feature. Release
binaries ship default features only, so this means building it yourself:

```console
cargo install --path ./crates/wash --features host-component-plugins
```

Then either point a dev session at it (see [`example/`](example/)) or load it
into a host directly:

```console
wash host --host-plugin id=cosmonic-kafka,file=./target/wasm32-wasip2/release/kafka_host_plugin.wasm
```

## Test results

Measured against Redpanda in Docker, on `wash` built from
[wasmCloud#5442](https://github.com/wasmCloud/wasmCloud/pull/5442) with
`host-component-plugins`, Rust stable 1.97.1, `wasm32-wasip2`.

### Producing through the plugin, from a workload

`example/` publishing over HTTP, with the records read back by an independent
`rpk` consumer to confirm they are really in the topic:

```console
$ curl -X POST 'localhost:8000/publish?topic=demo' --data 'hello from a wasm workload'
published 26 bytes to demo partition 0 offset 0
$ curl -X POST 'localhost:8000/publish?topic=demo&key=user-1' --data 'keyed record'
published 12 bytes to demo partition 0 offset 1

$ rpk topic consume demo --num 2 --format '%p:%o key=%k value=%v\n'
0:0 key= value=hello from a wasm workload
0:1 key=user-1 value=keyed record
```

Keys, partitions, and broker-assigned offsets all survive the store boundary
intact, and so do the error variants: a missing `bootstrap-servers` arrives at
the workload as `not-configured`, an unreachable broker as `connection`.

### Consuming through the plugin, from a workload

`example/`'s `/consume` route, against the records produced above. Offsets
advance across polls, and the group's committed offset is visible to ordinary
Kafka tooling:

```console
$ curl 'localhost:8000/consume?max=5'
0:0 ts=1786321802345 key=-       value=hello from a wasm workload
0:1 ts=1786321810527 key=user-1  value=keyed record
...
(5 records)

$ curl 'localhost:8000/consume?max=10&commit=1'
0:11 ts=1786415420966 key=acks-all value=durable produce
(3 records)
committed

$ rpk group describe wasmcloud-kafka-plugin
TOPIC  PARTITION  CURRENT-OFFSET  LOG-END-OFFSET  LAG
demo   0          12              12              0
```

Restarting the host rebuilds the plugin's store from nothing, and the consumer
resumes where the group left off rather than replaying from zero — because the
offsets live in Kafka, not in the plugin:

```console
$ curl 'localhost:8000/consume?max=10'          # after a full restart
(0 records)
$ curl -X POST 'localhost:8000/publish?topic=demo&key=post-restart' --data '...'
published 27 bytes to demo partition 0 offset 13
$ curl 'localhost:8000/consume?max=10&commit=1'
0:13 ts=1786417668125 key=post-restart value=published after the restart
(1 records)
```

Per-record timestamps survive too, which they could not before: v2 record
batches carry a base timestamp plus a per-record delta.

### The trigger pushing into a workload

With `trigger: "on"`, nothing polls the plugin. Publishing is enough to make the
workload run: the plugin's loop picks the record up and calls the workload's
`handle`, which here republishes to `demo.processed` so the delivery is
observable after the ephemeral instance is gone.

```console
$ curl -X POST 'localhost:8000/publish?topic=demo&key=trig' --data 'pushed by trigger'
published 17 bytes to demo partition 0 offset 15

$ rpk topic consume demo.processed --num 1 --format '%o key=%k value=%v\n'
0 key=trig value=handled offset 15: pushed by trigger
```

A burst of 10 arrives complete and in order, and the group's offset ends at the
log end with no lag:

```console
$ for i in $(seq 1 10); do curl -X POST "localhost:8000/publish?topic=demo&key=burst-$i" --data "burst event $i"; done
$ rpk topic consume demo.processed --offset 1 --num 10 --format '%o key=%k value=%v\n'
1  key=burst-1  value=handled offset 16: burst event 1
...
10 key=burst-10 value=handled offset 25: burst event 10

$ rpk group describe wasmcloud-kafka-plugin
TOPIC  PARTITION  CURRENT-OFFSET  LOG-END-OFFSET  LAG
demo   0          26              26              0
```

Restarting the host afterwards reprocesses nothing —
`demo.processed` still holds exactly 11 records — because the committed offset,
not the plugin's memory, is what says the work was done.

### What works

| Broker | Produce | Consume | Commit | Group membership |
|---|---|---|---|---|
| Redpanda (latest) | ✅ | ✅ | ✅ | ✅ |
| Apache Kafka 4.3.1 | ✅ | ✅ | ✅ | ✅ |

Kafka 4.3.1 verified end to end: records produced by the plugin are read back by
Kafka's own `kafka-console-consumer.sh`, and the offsets the plugin commits show
up in `kafka-consumer-groups.sh` against this plugin's member id — as a real
group member, not as an anonymous simple consumer.

Group membership was verified the only way that proves anything: by putting a
**stock Java `kafka-console-consumer` into the same group** and watching the
plugin (the elected leader) hand it a disjoint half of the partitions, which it
accepted and read from. Every record went to exactly one of the two.

In every case tested, wasm behaved identically to native — `wasi:sockets` never
diverged from a host TCP stack.

## Client library: there isn't one

**No Kafka client library.** Every API is encoded here, against `std::net`.

That was not the plan. `kafka-rust` 0.10.0 hardcodes
`const API_VERSION: i16 = 0` and parses only message sets whose magic byte is
`0` (*"this covers kafka 0.8 and 0.9"*). Four of its APIs are consequently
refused by a current broker, and which four depends on the broker — which is why
Redpanda produced happily and Kafka 4.x did not. Once those four were written by
hand, the dependency was doing two small requests, and the crate whose
obsolescence had caused every protocol bug in this project was still in the
build. It is gone; `Metadata` and `OffsetFetch` replaced it in `src/meta.rs`.

| API | kafka-rust sent | Redpanda floor | Kafka 4.3.1 floor | Sent here | Where |
|---|---|---|---|---|---|
| `Produce` | v0 | v0 ✅ | **v3** | v3 | `src/produce.rs` |
| `Fetch` | v0 | **v4** | **v4** | v4 | `src/fetch.rs` |
| `ListOffsets` | v0 | v0 ✅ | **v1** | v1 | `src/fetch.rs` |
| `OffsetCommit` | v1 | v1 ✅ | **v2** | v2 | `src/fetch.rs` |
| `FindCoordinator` | v0 | v0 | v0 | v0 | `src/fetch.rs` |
| `Metadata` | — | v0 | v0 | v0 | `src/meta.rs` |
| `OffsetFetch` | — | v1 | **v1** | v1 | `src/meta.rs` |
| `JoinGroup` / `SyncGroup` / `Heartbeat` / `LeaveGroup` | — | v0 | v0 | v0 | `src/group.rs` |

The group APIs are sent at v0 deliberately: v0 predates flexible versions, so
there are no tagged fields or compact strings to encode, and Kafka 4.3.1 still
advertises `JoinGroup(11): 0 to 9`. The later versions add incremental-rebalance
machinery this plugin does not use.

**Kafka 4.3.1's `ApiVersions` cannot be trusted for `Produce`**: it advertises
`v0..v13` and then rejects a v0 request with
`UnsupportedVersionException: unsupported version 0`, *closing the socket*
rather than answering. That closed socket is the whole reason every failure in
this project surfaced as an unhelpful `UnexpectedEof` — the client never gets an
error code to report.

Asking a broker what it accepts still explains most of it:

```
$ ./apiversions            # raw ApiVersions request, no client library
  Produce          (key  0)  v0..v7
  Fetch            (key  1)  v4..v13  <-- v0 NOT accepted
  ListOffsets      (key  2)  v0..v6
  Metadata         (key  3)  v0..v12
  FindCoordinator  (key 10)  v0..v4
  JoinGroup        (key 11)  v0..v6
  SyncGroup        (key 14)  v0..v4
```

`Produce` still accepts v0, so producing works. `Fetch` has a floor of **v4**,
so every consume attempt is refused before a record is ever parsed — which is
the `UnexpectedEof`, not a protocol subtlety.

Stepping through the APIs individually shows how narrow the gap actually is:

| Step | API | Result |
|---|---|---|
| 1 | Metadata | ✅ |
| 2 | ListOffsets | ✅ |
| 3 | **Fetch** (no group) | ❌ `UnexpectedEof` |
| 4a | FindCoordinator + JoinGroup + SyncGroup + OffsetFetch | ✅ group joined |
| 4b | Consumer poll | ❌ same `UnexpectedEof` |

**Consumer-group coordination already works against a modern broker.** So does
metadata, offset lookup, and producing. `Fetch` is the only broken API.

Confirmed by implementing it: a raw `Fetch` v4 request plus a v2 `RecordBatch`
decoder (~150 lines, no client library) reads the topic cleanly —

```
Fetch v4 accepted: 1099 byte response
topic=demo partition=0 error=0 high_watermark=12 record_set=1047B
  offset=0   ts=1786321802345  key=-          value=hello from a wasm workload
  offset=1   ts=1786321810527  key=user-1     value=keyed record
  ...
decoded 12 records
```

The first fix stayed smaller than replacing the client — send each refused API at
a version the broker accepts, keep the library for the rest — and that split did
not survive contact. Each new requirement landed on the *hand-written* side,
because the library's side was the obsolete one: `ListOffsets` for resetting a
position, `OffsetCommit` for committing one, `FindCoordinator` for knowing where
to send it. What remained was `Metadata` and `OffsetFetch`, two small requests
whose encodings are 60 lines each. Keeping a dependency for that — a dependency
that had caused every protocol bug so far — was the worse trade.

| Concern | Where |
|---|---|
| `Produce` v3 + v2 record batch encoding | `src/produce.rs` |
| `Fetch` v4 + record decoding, `ListOffsets` v1, `OffsetCommit` v2, `FindCoordinator` | `src/fetch.rs` |
| `Metadata`, `OffsetFetch` v1 | `src/meta.rs` |
| `JoinGroup`, `SyncGroup`, `Heartbeat`, `LeaveGroup`, `range` assignment | `src/group.rs` |

Producing carries its own v2 `RecordBatch` encoder, including the CRC-32C the
format requires (the older format used CRC-32, and confusing the two is reported
by the broker as a corrupt record). Keyed records are partitioned with Kafka's
own murmur2, so a key sent from here lands where any other client would send it
— otherwise ordering would silently break for everyone else reading the topic.

Partitions come from group membership rather than being taken statically, and
commits carry the generation that fences them — see
[Consumer groups](#consumer-groups) for why that matters and what it cost.

The decoder handles what a real topic contains, and refuses by name what it does
not:

- **All four codecs**: gzip, snappy, lz4, zstd, each with a pure-Rust decoder,
  because a C-backed one would not link for `wasm32-wasip2` — the constraint
  that rules out `rdkafka` in the first place. Snappy arrives in two shapes and
  both are handled: one raw block (what Redpanda sends) and the Java client's
  framed stream. An unrecognised codec is named, not walked as plain bytes.
- **CRCs are not verified.** The batch CRC is CRC32C over the body; the framing
  is length-delimited, so a corrupt batch surfaces as a decode error instead.
- **Headers are skipped**, since `cosmonic:kafka/types` has nowhere to put them.

Upstreaming `Fetch` v4 into `kafka-rust` would still be worth doing for everyone
else on `wasm32-wasip2`, and `rskafka` growing a transport abstraction so it can
run off tokio would be a better answer than either. Neither would retire this
code now that it also carries group membership.

For completeness, the clients that were ruled out:

| Client | Builds for wasip2 | Modern broker | Detail |
|---|---|---|---|
| `rdkafka` | ❌ | — | librdkafka's cmake builds **host** objects; `wasm-component-ld` fails with `archive member 'rdkafka_broker.o' is neither Wasm object file nor LLVM bitcode`. Cross-compiling it would then still need threads (thread-per-broker + poll loop; wasip2 has none) and OpenSSL. |
| `kafka-rust` | ✅ | partly | **Was used, now removed.** Produces fine against Redpanda; four APIs too old for a current broker. See above. |
| `rskafka` | ❌ | ✅ | Pulls tokio's `net`: *"Only features sync,macros,io-util,rt,time are supported on wasm."* |

### TLS

Not wired up, and no longer a dependency's problem now that there is no
dependency. Historically: `kafka-rust` 0.10.0's `security` feature pulls `openssl-sys`,
whose C build cannot target wasip2 — hence `default-features = false`. The path
is open though: `rustls` + `ring` + `webpki-roots` compiles clean to
`wasm32-wasip2`. kafka-rust master already defaults `security` to rustls; it
fails on wasip2 only because the feature also pulls `rustls-native-certs`, which
has no WASI platform implementation. Cfg'ing that out in favour of
`webpki-roots` is a small upstream patch. TLS is required for MSK and Confluent
Cloud, so this is not optional for production use.

## Known limitations

- Trigger fan-out is bounded by partition count: a single-partition topic
  dispatches one batch at a time however high `trigger-max-inflight` is set.
- Fan-out multiplies workload *instances*, but work those instances do back
  through this plugin re-serialises on its single-threaded store. The example
  handler republishes every record through `producer.send`, so it gains only
  ~12% from six-way fan-out (30,000 records: 17s sequential, 15s concurrent).
  A handler whose work is its own — computation, or imports served elsewhere —
  is what fan-out is for.
- **The pull interface and the trigger share one cursor.** `consumer.poll` and
  the trigger loop are the same consumer in the same store, so with
  `trigger: "on"` a record goes to whichever asks first and each side sees only
  part of the stream — with nothing logged, because neither is wrong on its own.
  Use one or the other per plugin deployment.
- **The default consumer group is a constant** (`wasmcloud-kafka-plugin`), so
  two unrelated deployments that both leave `consumer-group` unset now join the
  *same* group and split its partitions between them. That is a worse failure
  than the double-read it replaced: each sees a fraction of the records and
  cannot tell. Set `consumer-group` explicitly for anything but a single
  deployment.
- Heartbeats ride the poll path rather than a background thread, so a handler
  that occupies the plugin for longer than `session-timeout-ms` is evicted from
  the group mid-batch. See [Consumer groups](#consumer-groups).
- Only the `range` assignment strategy. A group whose other members offer only
  `roundrobin` or `cooperative-sticky` will not find a common protocol.
- Rebalances are eager, not cooperative: a rejoin drops every partition and
  reloads positions, so an in-flight batch for a partition this member keeps is
  redelivered rather than resumed. At-least-once already requires an idempotent
  handler, so this costs duplicate work, not correctness.
- No TLS or SASL yet.
- No `wasmcloud:messaging` drop-in — see [above](#everything-is-async-func-and-it-has-to-be).
- An absent key and an empty key are distinct in both directions, now that
  records are encoded and decoded here rather than by a client library.
- Blocking client on a cooperative executor: every operation is declared `async`
  for the ABI, but the body blocks, so a slow call occupies the plugin instance
  for its whole round trip. Fine for one publisher; revisit before serving many
  concurrently.
- **`JoinGroup` is the worst case of that.** The coordinator holds the request
  open until every member has rejoined or the rebalance times out, and this
  plugin blocks on it — so a rebalance stalls every workload's capability calls
  for as long as it takes, up to `session-timeout-ms`. Startup pays a smaller
  version of this: the broker's `group.initial.rebalance.delay.ms` (3s by
  default on Kafka) before the first join returns.

## License

Apache-2.0. See [LICENSE](LICENSE).
