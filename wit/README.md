# Interface packages

One directory per WIT package, with one file per interface and a `world.wit`.
Components use path overrides in `wkg.toml` and ignore generated `wit/deps`.
These packages target WASI 0.3 and publish to `ghcr.io/cosmonic-labs/cosmonic/<package>`.
All declared operations, including resource metadata methods, use `async func`.
Long-lived operations use native WIT streams and futures.

| Package | Contract |
| --- | --- |
| [`cosmonic:agent`](cosmonic-agent/world.wit) 0.4.0 | An API for calling models, tools, and durable sessions without tying a component to one inference provider. |
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
contract between an agent and whatever drives it. `run`, `run-types` and the
two worlds that use them are marked `@unstable(feature = agent-run)`: a
consumer enables that feature to see them. The design note is
[`drafts/cosmonic-agent-run/README.md`](drafts/cosmonic-agent-run/README.md).

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

## Agent 0.4.0 migration

The agent package adds typed chat interruption outcomes and explicit tool dispatch
outcomes. Update imports from `cosmonic:agent@0.3.0` to `cosmonic:agent@0.4.0`.
The published 0.3.0 artifact remains unchanged.

- Rename `inference-types.capabilities` and `model-description.capabilities` to
  `supported-operations`. The flags remain `chat` and `embeddings`.
- Handle `cancelled` and `timed-out` in both chat error types. Before acceptance,
  errors return from `chat`; afterward, they resolve the reply's outcome future.
  Dropping reply chunks requests cooperative cancellation. A terminal outcome
  already recorded wins the race. Abandoning a pending call may return no result.
- Match tool errors as `not-dispatched(dispatch-error)` or
  `outcome-unknown(outcome-error)`. The first confirms that this attempt produced
  no execution or effects; the second includes uncertain dispatch and lost replies.
  Both preserve typed cancellation, timeout, upstream failure, and diagnostic detail.
- Handle `tools.list` failing with `list-error` (`unavailable`, `denied`,
  `other`) instead of reading an empty list as "no tools". The same `tools`
  interface is imported by agents and exported by tool providers.
- Import tool records from the function-less `tool-types` interface, where
  `tool-def`, `tool-response` and the error types now live. A tool provider
  no longer imports `tools` just for its types.
- Keep completed execution failures in `tool-response.is-error`, with their output
  available to the model. Cancellation and timeout do not imply rollback.
- Optionally export `run` (unstable, `agent-run`) and target `runnable-agent`.
  A run writes `agent.run.v1` and `agent.run-result.v1`, now reserved in
  `session`, and streams `run-event.compacted` after it commits an
  `agent.compaction.v1` entry. The summary stays in the journal and is
  untrusted model output. `compaction-info.trigger` and the entry's `trigger`
  say why the agent summarized: `threshold`, `overflow` or `requested`.
- Optionally also export `compact` (unstable, `agent-run`) and target
  `compactable-agent`, so a driver can ask for a summary between runs with an
  optional `focus` from the person. It writes `agent.compaction.v1` with
  trigger `requested` under commit id `compact:<run-id>`.
- Treat tool output as possibly truncated. A host may cap a tool's output and
  append a short text note stating the original size.
- Match `other(string)` in every variant; `run-types` no longer uses
  `other(option<string>)`. Stable interfaces and worlds carry
  `@since(version = 0.4.0)`.
- Record an unknown tool outcome or abandoned call as an unresolved operation.
  Retry with the same id and arguments only when trusted configuration authorizes
  repeat detection. Provider annotations cannot authorize replay. A not-dispatched
  result describes this attempt and does not erase effects from earlier attempts.
- Match `attachment-source.blob`: an opaque, session-sealed id for content the
  host stores. Pass it through unchanged and never persist the bytes behind it.
- Set or ignore `attachment.name`, an optional display filename. It is untrusted
  text, never a path.
- Match the new `finish-reason` cases `refusal`, `content-filter` and
  `pause-turn`. On `pause-turn`, append the message and call again.
- Match `assistant-content.refusal`, the model's reason for declining. Show it
  as the model's text; it never satisfies a response format.
- Set `chat-options.parallel-tool-calls` (none keeps the provider default) and
  match `request-option.parallel-tool-calls`.
- Set `request-meta.cache-points` (empty for none) to mark prompt-cache
  breakpoints by `part-ref`.
- Set `assistant-part.citations` (empty for none) and render any citations as
  untrusted links on the text they support.
- Set `run-options.effort` (none leaves the agent's choice) and pass it on as
  `chat-options.effort`.
- Match `run-event.plan`, the agent's whole current plan for display; each
  event replaces the last.

## Usage

```text
client -> cosmonic:agent inference -> local provider
                                   -> remote provider

agent -> authorized model aliases + chat + tools + session
```

`cosmonic:notify` is self-contained:

```sh
wasm-tools component wit wit/cosmonic-notify
```

`cosmonic:kafka` imports `wasi:cli` and `wasmcloud:host`, and `cosmonic:agent`
imports `wasi:clocks` (for the `duration` in `run-types`), which each
package's `wkg.lock` pins by digest. The build script fetches them and checks
the lock still reproduces:

```sh
node .github/scripts/wit/build.mjs wit/cosmonic-kafka kafka
node .github/scripts/wit/build.mjs wit/cosmonic-agent agent
```
