# Interface packages

One directory per WIT package, with one file per interface and a `world.wit`.
Components use path overrides in `wkg.toml` and ignore generated `wit/deps`.
These packages target WASI 0.3 and publish to `ghcr.io/cosmonic-labs/<namespace>/<package>`.
All declared operations, including resource metadata methods, use `async func`.
Continuous feeds use native WIT streams and futures; notification responses
arrive on a future or stream.

| Package | Contract |
| --- | --- |
| [`cosmonic:agent`](cosmonic-agent/world.wit) 0.4.2 | An API for calling models, tools, and durable sessions without tying a component to one inference provider. |
| [`cosmonic:cron`](cosmonic-cron/world.wit) 0.1.0 | An API for being called on crontab, interval and one-shot schedules, declared with a workload or registered at run time, with time spelled in `wasi:clocks@0.3.0` types. |
| [`cosmonic:kafka`](cosmonic-kafka/world.wit) 0.5.1 | An API for publishing, consuming, and handling Kafka records through host-owned, binding-scoped clients, with optional transactions. |
| [`cosmonic:notifications`](cosmonic-notifications/world.wit) 0.3.0 | Portable user notifications with caller-scoped response queues, actions, replies, and native futures and streams. |

## Inference and agent worlds

| World | Imports | Exports |
| --- | --- | --- |
| `inference-client` | `inference-types`, `model-admin`, `chat`, `embeddings` | None |
| `inference-provider` | None | `inference-types`, `model-admin`, `chat`, `embeddings` |
| `chat-provider` | None | `inference-types`, `model-admin`, `chat` |
| `embeddings-provider` | None | `inference-types`, `model-admin`, `embeddings` |
| `agent` | `inference-types`, `models`, `chat`, `tools`, `session` | None |
| `tool-provider` | `tool-types` and the inference types it references (types only) | `tools` |
| `runnable-agent` (unstable, `agent-run`) | Those of `agent` | `run` |
| `run-client` (unstable, `agent-run`) | `run` | None |
| `compactable-agent` (unstable, `agent-run`) | Those of `agent` | `run`, `compact` |
| `compact-client` (unstable, `agent-run`) | `run`, `compact` | None |

A serving component that needs chat and embeddings uses `inference-client`; a
full provider implements `inference-provider`. A caller that only needs chat or
embeddings can import those interfaces directly and use the smaller providers.
A provider serving one operation still implements the other interface and
reports unsupported models there.

`run` is the typed export a host calls to hand an agent one unit of work: a
message in, the agent's loop of model and tool calls, a final reply out. It
streams text deltas and whole steps while the agent works, then resolves to an
outcome or a typed error. It replaces the `POST /task` HTTP convention as the
contract between an agent and whatever drives it. `run`, `compact`, `run-types` and
the four worlds that use them are marked `@unstable(feature = agent-run)`: a
consumer enables that feature to see them. The design note for the package,
including how tools and typed tools work, is
[`cosmonic-agent/README.md`](cosmonic-agent/README.md).

`model-admin` exposes the trusted catalog and preparation controls. Preparing a
model can load local weights or open a remote connection. A remote provider can
return an empty progress stream. Agents acquire models by workload-scoped alias
through `models`; they do not import model administration.

`model-residency` is an optional trusted interface for local providers. It
reports residency, holds independent pins, and drains existing reservations
before unloading. It is not included in the inference or agent worlds; hosts
allow it explicitly. Dropping an inference handle releases only its reservation.
Providers control retention and sharing, including for direct WIT callers.

## Model ownership and supported operations

`inference-types.model` is a provider-owned resource ready for inference. The
provider exports its resource definition alongside chat and embeddings so
composition preserves resource identity. A handle retains its original provider
if an alias changes. WIT resource identity does not implement routing: a host
router owns wrapper handles, retains the underlying provider handle, and forwards
calls to that provider. The host also enforces alias access, workload isolation,
attachment egress, and credential policy.

Both catalog entries and prepared handles expose the same `model-description`.
Catalog limits may be unknown; a prepared handle reports its effective limits.
Supported operations describe the provider/model pair, not just the model's training.
Optional fields distinguish unknown from unsupported: `accepts = some(empty)`
means text-only, and `supported-options = some(empty)` means no optional knobs.
Supported options can still reject values outside their range. If an option or
message part cannot be represented, `on-unrepresentable` controls refusal or
reported adaptation. Providers must not silently discard unsupported features.

## Agent 0.4 migration

What changed from 0.3.0 and what a consumer updates:
[`cosmonic-agent/docs/migrating-to-0.4.md`](cosmonic-agent/docs/migrating-to-0.4.md).

## Usage

```text
client -> cosmonic:agent inference -> local provider
                                   -> remote provider

agent -> authorized model aliases + chat + tools + session
```

`cosmonic:kafka` imports `wasi:cli` and `wasmcloud:host`, `cosmonic:cron`
imports those, `wasi:clocks`, and `wasi:random`, while `cosmonic:agent` and
`cosmonic:notifications` import `wasi:clocks` for `duration`. Each package's
`wkg.lock` pins them by digest. The build script fetches them and checks the
lock still reproduces:

```sh
node .github/scripts/wit/build.mjs wit/cosmonic-notifications cosmonic:notifications
node .github/scripts/wit/build.mjs wit/cosmonic-kafka kafka
node .github/scripts/wit/build.mjs wit/cosmonic-agent agent
node .github/scripts/wit/build.mjs wit/cosmonic-cron cosmonic:cron
```

## Scheduling worlds

| World | Imports | Exports |
| --- | --- | --- |
| `scheduled-handler` | None | `handler` |
| `scheduler-client` | `scheduler` | `handler` |
| `host-plugin` | `handler`, `wasi:clocks`, `wasi:random`, `wasmcloud:host` | `types`, `scheduler`, `wasi:cli/run`, `wasmcloud:host/workload-lifecycle` |

A workload whose schedules are known at deploy time declares them in its
binding and implements `scheduled-handler`. One that decides at run time
implements `scheduler-client`. [`cron-scheduler`](../host-plugins/component/cron-scheduler/)
implements `host-plugin`.

## Notification worlds

| World | Imports | Exports |
| --- | --- | --- |
| `notification-consumer` | `notifier`, `events` | None |
| `notification-provider` | None | `notifier`, `events` |

For consumer and provider requirements and an example, see the
[notifications contract](cosmonic-notifications/README.md).

`cosmonic:notify@0.2.0` stays published at `ghcr.io/cosmonic-labs/cosmonic/notify`
for existing consumers; `cosmonic:notifications` supersedes it and `notify` receives
no further releases.
