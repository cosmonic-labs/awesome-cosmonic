# Interface packages

One directory per WIT package, with one file per interface and a `world.wit`.
Components use path overrides in `wkg.toml` and ignore generated `wit/deps`.
These packages target WASI 0.3 and publish to `ghcr.io/cosmonic-labs/<namespace>/<package>`.
All declared operations, including resource metadata methods, use `async func`.
Continuous feeds use native WIT streams and futures; bounded notification waits
return directly from async operations.

| Package | Contract |
| --- | --- |
| [`cosmonic:agent`](cosmonic-agent/world.wit) 0.3.0 | An API for calling models, tools, and durable sessions without tying a component to one inference provider. |
| [`cosmonic:kafka`](cosmonic-kafka/world.wit) 0.5.1 | An API for publishing, consuming, and handling Kafka records through host-owned, binding-scoped clients, with optional transactions. |
| [`cosmonic:notifications`](cosmonic-notifications/README.md) 0.3.0 | Portable user notifications with owned sessions, actions, replies, and native async waits. |

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

`cosmonic:agent` and `cosmonic:notifications` are self-contained:

```sh
wasm-tools component wit wit/cosmonic-agent
```

`cosmonic:kafka` imports `wasi:cli` and `wasmcloud:host`, which `wkg.lock` pins by
digest. The build script fetches them and checks the lock still reproduces:

```sh
node .github/scripts/wit/build.mjs wit/cosmonic-kafka kafka
```

## Notification worlds

| World | Imports | Exports |
| --- | --- | --- |
| `notification-consumer` | `notifier` | None |
| `notification-provider` | None | `notifier` |

The provider owns each `session`; a wrapper retains its downstream session and
preserves isolation and lifetime. See the [contract and WASI architecture review](cosmonic-notifications/README.md).
