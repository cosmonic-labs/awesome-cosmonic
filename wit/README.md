# Interface packages

One directory per WIT package, with one file per interface and a `world.wit`.
Components use path overrides in `wkg.toml` and ignore generated `wit/deps`.
These packages target WASI 0.3 and publish to `ghcr.io/cosmonic-labs/cosmonic/<package>`.
All declared operations, including resource metadata methods, use `async func`.
Long-lived operations use native WIT streams and futures.

| Package | Contract |
| --- | --- |
| [`cosmonic:agent`](cosmonic-agent/world.wit) 0.3.0 | An API for calling models, tools, and durable sessions without tying a component to one inference provider. |
| [`cosmonic:cron`](cosmonic-cron/world.wit) 0.1.0 | An API for being called on crontab, interval and one-shot schedules, declared with a workload or registered at run time, with time spelled in `wasi:clocks@0.3.0` types. |
| [`cosmonic:kafka`](cosmonic-kafka/world.wit) 0.5.1 | An API for publishing, consuming, and handling Kafka records through host-owned, binding-scoped clients, with optional transactions. |
| [`cosmonic:notify`](cosmonic-notify/notify.wit) 0.2.0 | An API for sending notifications to users and getting their responses, with actions such as links and callbacks. |

## Inference and agent worlds

| World | Imports | Exports |
| --- | --- | --- |
| `inference-client` | `inference-types`, `model-admin`, `chat`, `embeddings` | None |
| `inference-provider` | None | `inference-types`, `model-admin`, `chat`, `embeddings` |
| `chat-provider` | None | `inference-types`, `model-admin`, `chat` |
| `embeddings-provider` | None | `inference-types`, `model-admin`, `embeddings` |
| `agent` | `inference-types`, `models`, `chat`, `tools`, `session` | None |
| `tool-provider` | Inference resource types referenced by `tools` | `tools` |

A serving component that needs chat and embeddings uses `inference-client`; a
full provider implements `inference-provider`. A caller that only needs chat or
embeddings can import those interfaces directly and use the smaller providers.
A provider serving one operation still implements the other interface and
reports unsupported models there.

`model-admin` exposes the trusted catalog and preparation controls. Preparing a
model can load local weights or open a remote connection. A remote provider can
return an empty progress stream. Agents acquire models by workload-scoped alias
through `models`; they do not import model administration.

`model-residency` is an optional trusted interface for local providers. It
reports residency, holds independent pins, and drains existing reservations
before unloading. It is not included in the inference or agent worlds; hosts
grant it explicitly. Dropping an inference handle releases only its reservation.
Providers control retention and sharing, including for direct WIT callers.

## Model ownership and capabilities

`inference-types.model` is a provider-owned resource ready for inference. The
provider exports its resource definition alongside chat and embeddings so
composition preserves resource identity. A handle retains its original provider
if an alias changes. WIT resource identity does not implement routing: a host
router owns wrapper handles, retains the underlying provider handle, and forwards
calls to that provider. The host also enforces alias access, workload isolation,
attachment egress, and credential policy.

Both catalog entries and prepared handles expose the same `model-description`.
Catalog limits may be unknown; a prepared handle reports its effective limits.
Capabilities describe the provider/model pair, not just the model's training.
Optional fields distinguish unknown from unsupported: `accepts = some(empty)`
means text-only, and `supported-options = some(empty)` means no optional knobs.
Supported options can still reject values outside their range. If an option or
message part cannot be represented, `on-unrepresentable` controls refusal or
reported adaptation. Providers must not silently discard unsupported features.

## Usage

```text
client -> cosmonic:agent inference -> local provider
                                   -> remote provider

agent -> authorized model aliases + chat + tools + session
```

`cosmonic:agent` and `cosmonic:notify` are self-contained:

```sh
wasm-tools component wit wit/cosmonic-agent
```

`cosmonic:kafka` imports `wasi:cli` and `wasmcloud:host`, and `cosmonic:cron`
imports those and `wasi:clocks`. Each package's `wkg.lock` pins them by digest.
The build script fetches them and checks the lock still reproduces:

```sh
node .github/scripts/wit/build.mjs wit/cosmonic-kafka kafka
node .github/scripts/wit/build.mjs wit/cosmonic-cron cron
```

## Scheduling worlds

| World | Imports | Exports |
| --- | --- | --- |
| `scheduled-handler` | None | `handler` |
| `scheduler-client` | `scheduler` | `handler` |
| `host-plugin` | `handler`, `wasi:clocks`, `wasmcloud:host` | `types`, `scheduler`, `wasi:cli/run`, `wasmcloud:host/workload-lifecycle` |

A workload whose schedules are known at deploy time declares them in its
binding and implements `scheduled-handler`. One that decides at run time
implements `scheduler-client`. [`cron-scheduler`](../host-plugins/component/cron-scheduler/)
implements `host-plugin`.
