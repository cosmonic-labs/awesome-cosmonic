# minimal-agent

The smallest useful agent on the `cosmonic:agent@0.4.0` interfaces: a host hands it a run, it calls a model the host chooses in a loop with the tools the workload grants, streams its progress back as typed events, and remembers the conversation for the next run. Built as a WebAssembly component (`wasm32-wasip2`, WASI 0.3) that exports `cosmonic:agent/run@0.4.0` and nothing else, for Cosmonic Desktop and wasmCloud.

The point is what the component does not hold. It names no model endpoint, carries no API key, declares no outbound network access (`allowedHosts: []`) and serves no HTTP. It asks the host for a model by an alias, `default`, and the workload's binding decides what that alias means. Its tools run elsewhere with their own grants, and it reaches them only through the host. The conversation lives in a session store that the host binds to this one workload, so the agent cannot read another workload's history and the host deletes it with the workload. Only the host can start a run: there is no ingress for anything else to reach.

About 1,800 lines of Rust in [`src/`](src/), starting from the tool loop in [`src/turn.rs`](src/turn.rs), building to a 325 KB component (122 KB gzipped), with three dependencies besides serde: `wasip3` for the WASI 0.3 clock and random numbers, `wit-bindgen` for the agent interfaces and the run export, and `serde_json`.

`cosmonic:agent@0.4.0` and its `run` interface are not published yet, and `run` is marked unstable (feature `agent-run`). The 0.2.0 release of this agent, on the published 0.3.0 interfaces, serves the older `POST /task` HTTP convention instead.

## What it shows

- A run takes the person's message and run options, and streams text deltas, reasoning deltas, each whole step and running counts while the agent works, then resolves to an outcome: the reply, the model that answered, the counts and why it finished.
- The model can call the workload's tools, as many times as it needs to (up to eight model calls a run unless the driver says otherwise), and each call and its result appear as steps.
- The next run continues the same conversation, tool results included, even on a fresh instance after a restart, because the history is in the session rather than in the instance.
- Every run is recorded in the session journal in the conventions the `session` and `run` interfaces describe, so a driver can redraw the conversation from the journal alone.

## How a run works

1. The driver calls `run(message, options)`. The agent checks them and refuses what it cannot take (`invalid-input`) before writing anything.
2. `session.current` returns the stored conversation and the journal end and state version the run's writes are conditioned on.
3. The run opens with one commit that closes anything a run cut off by a restart left open (see below), and records the person's message (`agent.step.v1`) and `agent.run.v1`, under the commit id `run:<run-id>`. A run id the session has seen before is refused as `duplicate`, and a run already in progress as `busy`. Once this commit lands, `run` returns the event stream and the outcome future, so a run that fails later still shows what was asked.
4. `models.open("default")` returns a model handle. The host resolves the alias and checks that this workload may use it. `tools.list` returns what the workload's tools offer, which the model is shown as it is described. If listing fails, the run records an error step and goes on without tools.
5. Each model call is journaled as an `agent.op.v1` before it is made and an `agent.op-result.v1` after. `chat.chat` takes the messages as a WIT `stream<message>`, closed by a `future` once they are all written, and returns a `stream<chunk>` of reply deltas plus a `future` holding the assembled completion. Text and reasoning deltas become `text-delta` and `reasoning-delta` events, and `counts` follows each call.
6. If the completion asks for tools, the reply is recorded and streamed as an assistant step, and each call is journaled as an `agent.op.v1` carrying its arguments (a JSON object, or a freeform tool's text as a JSON string), passed to `tools.call`, and closed with an `agent.op-result.v1` and a tool step. The results go back to the model, and the loop repeats. The last model call is made with no tools offered, so a run always ends with an answer. A reply the provider paused (`pause-turn`) goes back to the model unchanged for it to carry on.
7. A last commit records the answer and `agent.run-result.v1`, and replaces the stored conversation in the same atomic write. The outcome resolves only after it, so a driver that redraws from the journal when the outcome arrives finds the run recorded.

Every write is conditioned on the journal end the previous one left, so no call is made over another writer's entries. If something else did write the session, the run makes no further calls, records what it already did without conditions, keeps the other writer's conversation instead of overwriting it, and fails as `superseded`.

The stored conversation keeps each run's text, tool calls and tool results, at most 40 messages and 64 KiB, oldest exchanges dropped first. If the model refuses a request as too long for its context, the agent drops the oldest half and tries once more. Within a run the model sees each assistant message exactly as the completion returned it and each tool result with its attachments. The stored copy keeps a note in place of an attachment and leaves out reasoning and continuation data.

### How a run ends

| Outcome | When |
| --- | --- |
| `done` | The model answered without asking for tools. |
| `max-steps`, `max-total-tokens`, `max-duration` | A limit from the run options stopped the run before a call it would otherwise have made. |
| `refusal`, `content-filter` | The model declined, or a provider filter withheld its reply. The refusal text is in the reply, and there is no structured answer. |
| `cancelled` (error) | The driver dropped the event stream. The agent stops its calls and records them first. |
| `no-model`, `model-refused`, `model-failed` (errors) | The alias was refused, or a model call failed, with what the host said. |
| `output-rejected` (error) | The driver set a schema and the reply was not JSON. |
| `not-recorded`, `superseded` (errors) | The session refused a write, or another writer changed it during the run. |

The run options it honors: `instructions` (ahead of its own system prompt), `schema` (the reply is returned as `structured`; the agent checks it is JSON and leaves checking it against the schema to the driver), `max-steps`, `max-total-tokens`, `max-duration`, `effort` (passed to the model) and `trace-parent` (passed with each model call).

### After a restart

A restart can cut a run off between a call's `agent.op.v1` and its result. The next run finds those open ops after the stored state's `through` and closes them as the `cosmonic:agent/session` interface asks (its source is [`wit/cosmonic-agent/session.wit`](../../wit/cosmonic-agent/session.wit) in this repository):

- An open model call is closed as `cancelled`. Asking a model again is safe.
- An open tool call is closed as `unresolved`, with an `error` step telling the person the call was interrupted. Whether it took effect is unknown, so it is never made again, under its own id or a new one.

The driver holds the lock and sends one run at a time. The agent checks too, and if two runs do overlap anyway, the later one closes the earlier one's open calls this way, and the earlier one stops at its next conditional write.

### What it leaves out

It is the reference example, so it keeps to what every agent needs:

- **No plan.** It keeps no working plan, so it never sends the `plan` event or writes `agent.plan.v1`.
- **No summaries.** When the context overflows it drops the oldest half of the conversation rather than summarizing it, so it never sends `compacted` or writes `agent.compaction.v1`, and it does not export `compact`.
- **No questions.** It never asks the person anything, so it never finishes `input-required`.
- **No HTTP.** There is no `POST /task`, `GET /history` or NDJSON. A driver reads the conversation from the session journal.

## What it can reach

[`wit/world.wit`](wit/world.wit) is the whole list of what it can do:

| Interface | For |
| --- | --- |
| `cosmonic:agent/inference-types`, `models`, `chat` | A model, by the alias `default` |
| `cosmonic:agent/tools` | The tools the workload grants |
| `cosmonic:agent/session` | This workload's conversation and journal |
| `wasi:clocks/monotonic-clock` 0.3 | The run's time limit |
| `wasi:random/random` 0.3 | Ids for calls and commits |

`wasm-tools component wit` also lists `cosmonic:agent/tool-types`, `cosmonic:agent/run-types` and `wasi:clocks/types`, which hold only types and grant nothing, and fourteen `wasi:cli`, `wasi:io` and `wasi:clocks` 0.2 imports. Rust's standard library brings those in to print a panic message to stderr; the agent's code never calls them.

The one export is `cosmonic:agent/run@0.4.0`.

## Why the interfaces are named

`wit/world.wit` imports `inference-types`, `models`, `chat`, `tools` and `session` by name and exports `run`. That is the same set as the `cosmonic:agent/runnable-agent` world, and naming them one by one puts the list of capabilities an operator reviews in this repository, where it reads at a glance.

A component that imports an interface needs it bound when it is deployed, so a workload running this agent declares where `tools` comes from: a host entry, as in the example manifest, or a component that exports `cosmonic:agent/tools@0.4.0`.

## Prerequisites

- [`wash`](https://wasmcloud.com/docs/installation) (tested with wash 2.8.0) or [`wkg`](https://github.com/bytecodealliance/wasm-pkg-tools) (tested with 0.16.0)
- Rust 1.94 or newer and the `wasm32-wasip2` target: `rustup target add wasm32-wasip2` (tested with 1.97.1)

## Build

```shell
wash build
```

The component is written to `target/wasm32-wasip2/release/minimal_agent.wasm`.

`wash build` fetches the WIT first. Until `cosmonic:agent@0.4.0` is published, [`wkg.toml`](wkg.toml) points it at [`wit/cosmonic-agent`](../../wit/cosmonic-agent) in this repository, so the build needs this checkout and no registry for it. [`wkg.lock`](wkg.lock) pins `wasi:clocks`, which the run types use, by digest, and `wit/deps` is generated and gitignored. To fetch without building, run `wash wit fetch`.

### Smaller still

The release profile in [`Cargo.toml`](Cargo.toml) is already tuned for size. [Binaryen](https://github.com/WebAssembly/binaryen)'s `wasm-opt` can take the build to about 270 KB (113 KB gzipped) with no change in behavior. It optimizes core modules, not components, so link the core module, optimize it, and wrap it again:

```shell
RUSTFLAGS=-Clink-arg=--skip-wit-component \
  cargo build --target wasm32-wasip2 --release --target-dir target/core
wasm-opt -Oz --enable-bulk-memory --enable-sign-ext --enable-mutable-globals \
  --enable-nontrapping-float-to-int --enable-multivalue --enable-reference-types \
  --strip-debug --strip-producers \
  target/core/wasm32-wasip2/release/minimal_agent.wasm -o target/core/opt.wasm
wasm-tools component new target/core/opt.wasm -o target/core/component.wasm
wasm-tools strip --all target/core/component.wasm -o target/minimal_agent.opt.wasm
```

Name the features rather than passing `--all-features`: with every feature on, `wasm-opt` can write encodings, such as compact imports, that the runtime rejects when the component loads. The last step also drops the debugging names. None of this is part of `wash build`, because it needs Binaryen and [`wasm-tools`](https://github.com/bytecodealliance/wasm-tools) installed.

## Test

The unit tests cover the parts of a run that need no host: how a reply ends a run, what the agent accepts, the outcome's reply and the stored conversation's caps.

```shell
cargo test
```

[`test-host/`](test-host/) drives the built component end to end. It is a small [Wasmtime](https://wasmtime.dev) host that plays the model, the session store and three tools, and starts each run on a fresh instance, so only the session survives from one run to the next, as on Desktop. It checks a run with a tool call, a second run continuing the conversation, a repeated run id, an empty message, a refused model call, a declined reply, a schema, a paused reply, a freeform tool call and cancellation, and prints each run's events and journal as it goes:

```shell
wash build
cd test-host
cargo run -- ../target/wasm32-wasip2/release/minimal_agent.wasm
```

## Run on Cosmonic Desktop

A run needs a driver. On Cosmonic Desktop that is the Agents view: it starts each conversation as a workload whose component named `agent` is the agent, and, in a build that drives typed runs, sends each message the person writes as a run through the agent's `run` export and draws the conversation from the session journal. The host binds the `run` export itself, for agent sessions only; a workload manifest cannot ask for it.

[`manifests/workload.yaml`](manifests/workload.yaml) shows the workload such a session runs: the session, inference and tools entries at `cosmonic:agent@0.4.0`, each granted to the `agent` component alone, and the agent with no hosts:

```yaml
hostInterfaces:
  - namespace: cosmonic
    package: agent
    version: 0.4.0
    interfaces: [session]
    config:
      components: agent
  - namespace: cosmonic
    package: agent
    version: 0.4.0
    interfaces: [inference-types, models, chat]
    config:
      components: agent
      # backend: <one of the host's named model sources>
  - namespace: cosmonic
    package: agent
    version: 0.4.0
    interfaces: [tools]
    config:
      components: agent
components:
  - name: agent
    image: ghcr.io/cosmonic-labs/components/minimal-agent:0.3.0
    localResources:
      allowedHosts: []
```

`components: agent` grants each entry to the component named `agent` only, the way Desktop scopes its agent sessions, so no other component in the workload can read the conversation or spend the model budget. Without a `backend` on the inference entry, the alias resolves to the host's default model source. Set `backend` to use one of the host's named sources instead, and `model` to pick a model on it. The agent's code does not change either way.

Out of the box Desktop's default model source is a local [Ollama](https://ollama.com), so have Ollama running with a model pulled. Tool calling needs a model that supports it.

The image in the manifest is published by this repository's CI. To run your own build, push it to Desktop's built-in registry and point `image` at it:

```shell
wash oci push --insecure oci.localhost:8200/examples/minimal-agent:dev \
  target/wasm32-wasip2/release/minimal_agent.wasm
```

## License

Apache-2.0.
