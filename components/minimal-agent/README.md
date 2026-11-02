# minimal-agent

The smallest useful agent harness on `cosmonic:agent@0.4.2`. An agent harness is the code that runs an agent's loop: it takes a person's message, calls a model, calls the tools the model asks for, and repeats until the model answers. This one is a WebAssembly component (`wasm32-wasip2`, WASI 0.3) in about 2,000 lines of Rust, and it builds to a 326 KB component.

It holds nothing of its own. It names no model endpoint, carries no API key, has no network access (`allowedHosts: []`), reads no files and serves no HTTP. The host gives it a model by the alias `default`, the tools the workload grants, and a session store for the conversation. Only the host can start a run.

## What it imports and exports

[`wit/world.wit`](wit/world.wit) is the full list of what it can call:

| Import | For |
| --- | --- |
| `cosmonic:agent/inference-types`, `models`, `chat` | A model, by the alias `default` |
| `cosmonic:agent/tools` | The tools the workload grants |
| `cosmonic:agent/session` | This workload's conversation and journal |
| `wasi:clocks/monotonic-clock` 0.3 | The run's time limit |
| `wasi:random/random` 0.3 | Ids for calls and commits |

It exports one interface, `cosmonic:agent/run@0.4.2`: a message in, a stream of events while the agent works (text as it arrives, each tool call and result, token counts), and an outcome at the end. `run` is unstable in `cosmonic:agent` 0.4 (feature `agent-run`), so its shape may still change.

`wasm-tools component wit` also lists the type-only interfaces `cosmonic:agent/tool-types`, `cosmonic:agent/run-types` and `wasi:clocks/types`, which grant nothing, and fourteen `wasi:cli`, `wasi:io` and `wasi:clocks` 0.2 imports that Rust's standard library brings in to print a panic message. None of them reach the network or the filesystem.

## How it works

Each run reads the stored conversation from the session, opens the model, lists the tools, and loops: call the model, run any tool calls it asks for, send the results back. It stops when the model answers, or after eight model calls unless the driver sets another limit (at most 64). Every model call and tool call is written to the session journal before and after it happens, so the next run continues the conversation even on a fresh instance after a restart, and a tool call cut off by a restart is never repeated. Start reading at [`src/turn.rs`](src/turn.rs).

To stay minimal it keeps no working plan, never summarizes old context (it drops the oldest half when the model's context is full), and never asks the person a question.

## Prerequisites

- [`wash`](https://wasmcloud.com/docs/installation) 2.8.0 or newer
- Rust 1.94 or newer with the `wasm32-wasip2` target: `rustup target add wasm32-wasip2`

## Build

```shell
export WKG_CONFIG_FILE=$PWD/../../wit/wkg-registries.toml
wash build
```

`wash build` fetches `cosmonic:agent@0.4.2` from `ghcr.io` (pinned by digest in [`wkg.lock`](wkg.lock)) into `wit/deps`, then writes the component to `target/wasm32-wasip2/release/minimal_agent.wasm`. The variable tells it where the `cosmonic` packages are published.

## Test

```shell
cargo test
```

covers the parts of a run that need no host. [`test-host/`](test-host/) runs the built component end to end in a small [Wasmtime](https://wasmtime.dev) host that plays the model, the session store and four tools, starting every run on a fresh instance:

```shell
cd test-host
cargo run -- ../target/wasm32-wasip2/release/minimal_agent.wasm
```

## Run it in Cosmonic Desktop

1. Push the component to Desktop's built-in registry:

   ```shell
   wash oci push --insecure oci.localhost:8200/examples/minimal-agent:dev \
     target/wasm32-wasip2/release/minimal_agent.wasm
   ```

2. In **Agents**, open the agent chooser (the **Agent:** button, which shows the current agent's name) and select **Add a new agent…**.
3. Enter a **Name**, enter `oci.localhost:8200/examples/minimal-agent:dev` as the **Image**, and select **Check and save**. Desktop checks the component's imports and exports and refuses an agent that can reach the network or the filesystem.
4. Start a session with it and send a message.

Desktop runs each session as its own workload: this component, named `agent`, with `allowedHosts: []`, plus the session store, the model you picked and the session's tools, each granted to `agent` alone. [`manifests/workload.yaml`](manifests/workload.yaml) shows the shape of that workload. You don't apply it yourself: Desktop writes it and adds the binding that drives `run`.

By default the model comes from a local [Ollama](https://ollama.com). Tool calling needs a model that supports it.

## Run it in Cosmonic Control

Agent support in Cosmonic Control is experimental and not in a release yet. It drives the same `run` export over HTTP or NATS, with the session, model and tools served by Control's agent plugins.

## License

Apache-2.0.
