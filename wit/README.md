# Interface packages

One directory per WIT package, with one file per interface and a `world.wit`.
Components use path overrides in `wkg.toml` and ignore generated `wit/deps`.
These packages target WASI 0.3 and publish to `ghcr.io/cosmonic-labs/cosmonic/<package>`.
All declared operations, including resource metadata methods, use `async func`.
Long-lived operations use native WIT streams and futures.

| Package | Contract |
| --- | --- |
| [`cosmonic:agent`](cosmonic-agent/world.wit) 0.3.0 | Provider-independent inference, authorized model aliases, tools, and durable sessions. |
| [`cosmonic:notify`](cosmonic-notify/notify.wit) 0.2.0 | Notifications and calls to action, available through an explicit host grant. |

## Inference and agent worlds

| World | Imports | Exports |
| --- | --- | --- |
| `inference-client` | `inference-types`, `model-admin`, `chat`, `embeddings` | None |
| `inference-provider` | None | `inference-types`, `model-admin`, `chat`, `embeddings` |
| `chat-provider` | None | `inference-types`, `model-admin`, `chat` |
| `embeddings-provider` | None | `inference-types`, `model-admin`, `embeddings` |
| `agent` | `inference-types`, `models`, `chat`, `tools`, `session` | None |
| `tool-provider` | Inference resource types referenced by `tools` | `tools` |

The OpenAI server uses `inference-client`. The llama adapter and scripted fake
backend implement `inference-provider`. A caller that only needs chat or
embeddings can import those interfaces directly and use the smaller providers.
The OpenAI server needs both interfaces; a provider serving one operation still
implements the other interface and reports unsupported models there.

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

## Implementation boundary

[Architecture diagram](cosmonic-agent/docs/agent-inference-architecture.svg)

The llama.cpp engine's tokenization, sampling, grammar conversion, and context
cache interfaces are internal to the llama provider. Other providers implement
the inference contract without reproducing that engine API.

```text
openai-server -> cosmonic:agent inference -> llama-inference
                                          -> another local provider
                                          -> a remote provider

agent -> authorized model aliases + chat + tools + session
```

The package is self-contained:

```sh
wasm-tools component wit wit/cosmonic-agent
```
