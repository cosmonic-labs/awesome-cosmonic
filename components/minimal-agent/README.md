# minimal-agent

The smallest useful agent on the `cosmonic:agent@0.3.0` interfaces: it takes a task over HTTP, runs one chat turn against a model the host chooses, streams the reply back, and remembers the conversation for the next request. Built as a WebAssembly component (`wasi:http/handler@0.3.0`, `wasm32-wasip2`) for Cosmonic Desktop and wasmCloud.

The point is what the component does not hold. It names no model endpoint, carries no API key and declares no outbound network access (`allowedHosts: []`). It asks the host for a model by an alias, `default`, and the workload's binding decides what that alias means. The conversation lives in a session store that the host binds to this one workload, so the agent cannot read another workload's history and the host deletes it with the workload.

About 400 lines of Rust in [`src/lib.rs`](src/lib.rs), with three dependencies besides serde: `wasip3` for the HTTP export, `wit-bindgen` for the agent interfaces, and `serde_json`.

## What it shows

- `POST /task` with `{"task": "..."}` streams the model's reply as plain text while it is generated.
- A second request continues the same conversation, including after the workload restarts, because the history is in the session rather than in the instance.
- Any other request gets `404` with a usage line. A body without a `task` gets `400`.

## How a turn works

1. `session.current` returns the stored conversation and the journal end and state version the turn's writes are conditioned on.
2. `models.open("default")` returns a model handle. The host resolves the alias and checks that this workload may use it.
3. A first commit appends the person's message (`agent.step.v1`) and the model call (`agent.op.v1`) to the session journal, before the call is made, so a turn that fails partway still shows what was asked.
4. `chat.chat` takes the messages as a WIT `stream<message>`, closed by a `future` once they are all written, and returns a `stream<chunk>` of reply deltas plus a `future` holding the assembled completion. The text deltas are written straight into the HTTP response body.
5. A second commit records the call's result (`agent.op-result.v1`) and the reply, and replaces the stored conversation in the same atomic write. It only applies if nothing else wrote the session in between. If something did, the agent keeps the other writer's conversation and leaves its own turn in the journal instead of overwriting it.

The stored conversation is plain text turns. A fuller agent keeps each assistant message exactly as the completion returned it, including reasoning and continuation data, and closes any model call a crashed turn left open when it next starts. The comments in [`session.wit`](../../wit/cosmonic-agent/session.wit) describe that protocol.

## Why a custom world

[`wit/world.wit`](wit/world.wit) imports `inference-types`, `models`, `chat` and `session` by name instead of including the published `cosmonic:agent/agent` world. That world also imports `tools`, and a component that imports an interface needs it bound when it is deployed. Naming only the interfaces the code calls keeps the workload from having to declare a tool provider it never uses, and keeps the list of capabilities an operator reviews down to what the agent can actually do.

## Prerequisites

- [`wash`](https://wasmcloud.com/docs/installation) (tested with wash 2.8.0) or [`wkg`](https://github.com/bytecodealliance/wasm-pkg-tools) (tested with 0.16.0)
- Rust 1.94 or newer and the `wasm32-wasip2` target: `rustup target add wasm32-wasip2` (tested with 1.97.1)

## Build

```shell
wash wit fetch
wash build
```

The component is written to `target/wasm32-wasip2/release/minimal_agent.wasm`.

`cosmonic:agent@0.3.0` is not published yet. Until it is, [`wkg.toml`](wkg.toml) points the fetch at this repository's own [`wit/cosmonic-agent`](../../wit/cosmonic-agent/), so build from a full clone rather than a single-folder `wash new` checkout. `wit/deps` is generated and gitignored. Once the package is on `ghcr.io/cosmonic-labs/cosmonic/agent`, delete the override and the fetch resolves it from the registry.

## Run on Cosmonic Desktop

Desktop serves inference and the session store. Out of the box its default model source is a local [Ollama](https://ollama.com), so have Ollama running with a model pulled, then apply [`manifests/workload.yaml`](manifests/workload.yaml).

The manifest declares the HTTP host name, the session store and the inference interfaces as separate entries:

```yaml
hostInterfaces:
  - namespace: wasi
    package: http
    interfaces: [handler]
    config:
      host: minimal-agent.localhost
  - namespace: cosmonic
    package: agent
    version: 0.3.0
    interfaces: [session]
  - namespace: cosmonic
    package: agent
    version: 0.3.0
    interfaces: [inference-types, models, chat]
    # config:
    #   backend: <one of the host's named model sources>
```

With no `config` on the inference entry, the alias resolves to the host's default model source. Set `backend` to use one of the host's named sources instead, and `model` to pick a model on it. The agent's code does not change either way.

The image in the manifest is published by this repository's CI. To run your own build, push it to Desktop's built-in registry and point `image` at it:

```shell
wash oci push --insecure oci.localhost:8200/examples/minimal-agent:dev \
  target/wasm32-wasip2/release/minimal_agent.wasm
```

## Try it

```shell
curl -N -H 'Host: minimal-agent.localhost' \
  -d '{"task": "My name is Ada. Remember it."}' \
  http://127.0.0.1:8200/task

curl -N -H 'Host: minimal-agent.localhost' \
  -d '{"task": "What is my name?"}' \
  http://127.0.0.1:8200/task
```

`-N` turns off curl's buffering, so the reply appears as the model generates it. The second answer knows the name from the first, because both turns are in the session. Delete the workload and the conversation goes with it.

## License

Apache-2.0.
