# minimal-agent

The smallest useful agent on the `cosmonic:agent@0.3.0` interfaces: it takes a task over HTTP, runs a model the host chooses in a loop with the tools the workload grants, streams the reply back, and remembers the conversation for the next request. Built as a WebAssembly component (`wasi:http/handler@0.3.0`, `wasm32-wasip2`) for Cosmonic Desktop and wasmCloud.

The point is what the component does not hold. It names no model endpoint, carries no API key and declares no outbound network access (`allowedHosts: []`). It asks the host for a model by an alias, `default`, and the workload's binding decides what that alias means. Its tools are other components in the same workload, each with its own grants: the example manifest pairs it with the ISS tools, which may reach one host, while the agent beside them may reach none. The conversation lives in a session store that the host binds to this one workload, so the agent cannot read another workload's history and the host deletes it with the workload.

About 1,200 lines of Rust in [`src/`](src/), starting from the tool loop in [`src/turn.rs`](src/turn.rs), building to a 281 KB component (102 KB gzipped), with three dependencies besides serde: `wasip3` for the HTTP export, `wit-bindgen` for the agent interfaces, and `serde_json`.

## What it shows

- `POST /task` with `{"task": "..."}` streams the model's reply as NDJSON lines while it is generated.
- The model can call the workload's tools, as many times as it needs to (up to eight model calls a turn), and each call and its result appear as steps.
- A second request continues the same conversation, tool results included, even after the workload restarts, because the history is in the session rather than in the instance.
- `GET /history` returns every step recorded so far.
- It speaks the protocol of the Cosmonic Desktop **Agents** view, so Desktop can run it as an agent session. See [Running it in the Desktop Agents view](#running-it-in-the-desktop-agents-view).

## How a turn works

1. `session.current` returns the stored conversation and the journal end and state version the turn's writes are conditioned on.
2. A first commit closes anything a turn cut off by a restart left open (see below) and appends the person's message (`agent.step.v1`), so a turn that fails partway still shows what was asked.
3. `models.open("default")` returns a model handle. The host resolves the alias and checks that this workload may use it. `tools.list` returns what the workload's tool provider offers, which the model is shown as it is described.
4. Each model call is journaled as an `agent.op.v1` before it is made and an `agent.op-result.v1` after. `chat.chat` takes the messages as a WIT `stream<message>`, closed by a `future` once they are all written, and returns a `stream<chunk>` of reply deltas plus a `future` holding the assembled completion. Each text delta is written to the HTTP response as a `delta` line.
5. If the completion asks for tools, each call is journaled as an `agent.op.v1` carrying its arguments, made with `tools.call`, and closed with an `agent.op-result.v1` and a `tool` step. The results go back to the model, and the loop repeats. The eighth model call is made with no tools offered, so a turn always ends with an answer.
6. A last commit records the answer and replaces the stored conversation in the same atomic write.

Every write is conditioned on the journal end the previous one left, so no call is made over another writer's entries. If something else did write the session, the turn makes no further calls, records what it already did without conditions, and keeps the other writer's conversation instead of overwriting it.

The stored conversation keeps each turn's text, tool calls and tool results, at most 40 messages and 64 KiB, oldest exchanges dropped first. If the model refuses a request as too long for its context, the agent drops the oldest half and tries once more. Within a turn the model sees each assistant message exactly as the completion returned it and each tool result with its attachments; the plot tool's image reaches the model this way. The stored copy keeps a note in place of an attachment and leaves out reasoning and continuation data.

### After a restart

A restart can cut a turn off between a call's `agent.op.v1` and its result. The next turn finds those open ops after the stored state's `through` and closes them as the published `cosmonic:agent/session` interface asks (its source is [`wit/cosmonic-agent/session.wit`](../../wit/cosmonic-agent/session.wit) in this repository):

- An open model call is closed as `cancelled`. Asking a model again is safe.
- An open tool call is closed as `unresolved`, with an `error` step telling the person the call was interrupted. Whether it took effect is unknown, so it is never made again, under its own id or a new one.

Desktop sends one task at a time. If two turns do overlap, the later one closes the earlier one's open calls this way, and the earlier one stops at its next conditional write.

## What it can reach

The component imports seven interfaces, and they are the whole list of what it can do:

| Interface | For |
| --- | --- |
| `cosmonic:agent/inference-types`, `models`, `chat` | A model, by the alias `default` |
| `cosmonic:agent/tools` | The tools the workload links to it |
| `cosmonic:agent/session` | This workload's conversation and journal |
| `wasi:http/types` | Answering HTTP. Without `wasi:http/client` it cannot send a request |
| `wasi:random/random` | Ids for calls and commits |

`wasm-tools component wit` also lists fourteen `wasi:cli`, `wasi:io` and `wasi:clocks` 0.2 imports. Rust's standard library brings them in to print a panic message to stderr; the agent's code never calls them. Desktop's agent import allowlist admits them.

## Why the interfaces are named

[`wit/world.wit`](wit/world.wit) imports `inference-types`, `models`, `chat`, `tools` and `session` by name. That is the same set as the published `cosmonic:agent/agent` world, and naming them one by one puts the list of capabilities an operator reviews in this repository, where it reads at a glance.

A component that imports an interface needs it bound when it is deployed, so a workload running this agent declares where `tools` comes from: a tool component, as in the example manifest, or, for an agent with no tools, a host entry under which the host serves an empty tool list.

## Prerequisites

- [`wash`](https://wasmcloud.com/docs/installation) (tested with wash 2.8.0) or [`wkg`](https://github.com/bytecodealliance/wasm-pkg-tools) (tested with 0.16.0)
- Rust 1.94 or newer and the `wasm32-wasip2` target: `rustup target add wasm32-wasip2` (tested with 1.97.1)

## Build

```shell
export WKG_CONFIG_FILE=$PWD/../../wit/wkg-registries.toml
wash wit fetch
wash build
```

`wash build` fetches again, so the variable has to be set for both commands.

The component is written to `target/wasm32-wasip2/release/minimal_agent.wasm`.

`cosmonic:agent@0.3.0` is fetched from `ghcr.io/cosmonic-labs/cosmonic/agent`, where this repository publishes it. [`wit/wkg-registries.toml`](../../wit/wkg-registries.toml) maps the `cosmonic` namespace there; outside this repository, use a copy of that file. [`wkg.lock`](wkg.lock) pins the package by digest, and `wit/deps` is generated and gitignored.

### Smaller still

The release profile in [`Cargo.toml`](Cargo.toml) is already tuned for size. [Binaryen](https://github.com/WebAssembly/binaryen)'s `wasm-opt` can take the build to about 230 KB (95 KB gzipped) with no change in behavior. It optimizes core modules, not components, so link the core module, optimize it, and wrap it again:

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

## Run on Cosmonic Desktop

Desktop serves inference and the session store. Out of the box its default model source is a local [Ollama](https://ollama.com), so have Ollama running with a model pulled, then apply [`manifests/workload.yaml`](manifests/workload.yaml).

The manifest declares the HTTP host name, the session store and the inference interfaces as host entries, and the ISS tools as a second component:

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
    config:
      components: agent
  - namespace: cosmonic
    package: agent
    version: 0.3.0
    interfaces: [inference-types, models, chat]
    config:
      components: agent
      # backend: <one of the host's named model sources>
components:
  - name: agent
    image: ghcr.io/cosmonic-labs/components/minimal-agent:0.2.0
    localResources:
      allowedHosts: []
  - name: tools
    image: ghcr.io/cosmonic/agents/tools@sha256:cde3c61c2632096f2976906d63665c39c6b826abf52fd58e55bdb7b317070508
    localResources:
      allowedHosts: [https://api.wheretheiss.at]
```

`components: agent` grants the session and inference entries to the component named `agent` only, the way Desktop scopes its agent sessions, so the tools component cannot read the conversation or spend the model budget. Without a `backend` on the inference entry, the alias resolves to the host's default model source. Set `backend` to use one of the host's named sources instead, and `model` to pick a model on it. The agent's code does not change either way.

The agent's `tools` import links to the `tools` component's export. That component is the one Desktop's ISS tracker uses: `iss_position` calls a live API, `distance_km` is arithmetic, and `plot_ground_track` returns an image. It may reach `api.wheretheiss.at` and nothing else, and because `allowedHosts` is per component, the agent still reaches nothing. Any component that exports `cosmonic:agent/tools@0.3.0` can take its place. To run the agent with no tools, delete the component and add the `tools` host entry that is commented out in the manifest.

The image in the manifest is published by this repository's CI. To run your own build, push it to Desktop's built-in registry and point `image` at it:

```shell
wash oci push --insecure oci.localhost:8200/examples/minimal-agent:dev \
  target/wasm32-wasip2/release/minimal_agent.wasm
```

## Try it

```shell
curl -N -H 'Host: minimal-agent.localhost' -H 'x-cosmonic-agent-session: 1' \
  -d '{"task": "Where is the International Space Station right now?"}' \
  http://127.0.0.1:8200/task

curl -N -H 'Host: minimal-agent.localhost' -H 'x-cosmonic-agent-session: 1' \
  -d '{"task": "How far is that from London?"}' \
  http://127.0.0.1:8200/task

curl -H 'Host: minimal-agent.localhost' http://127.0.0.1:8200/history
```

`-N` turns off curl's buffering, so the steps and `delta` lines appear as they happen. The first answer comes from a call to `iss_position`. The second can use that position, because the tool's result is in the session with the rest of the conversation. Tool calling needs a model that supports it. Delete the workload and the conversation goes with it.

## Running it in the Desktop Agents view

Desktop drives an agent session over HTTP through the session's host name (`agent-<session id>.localhost`) and reads the conversation it draws from the host's session store, not from the agent. A full specification of the protocol is being written for the Cosmonic Desktop repository, and a link will follow here. In summary:

- `GET /history` answers `200` with `{"steps": [...]}`. Desktop uses it as the readiness probe for a new session.
- `POST /task` takes `{"task": "..."}` and requires the `x-cosmonic-agent-session` header. Without it the agent answers `403`. A web page cannot add that header to a cross-origin request without a CORS preflight, which the agent never answers, so a page the person happens to visit cannot drive the session.
- `/task` answers with `application/x-ndjson`, one JSON object per line:
  - `{"type":"delta","text":"..."}` for each piece of reply text as it arrives.
  - `{"type":"step","role":"assistant","text":"...","blocks":["toolRequest"],"tools":["iss_position"]}` when the model asks for tools, and `{"type":"step","role":"tool","text":"...","blocks":["toolResponse"],"tools":["iss_position"]}` for each result.
  - `{"type":"step","role":"assistant","text":"...","blocks":["text"],"tools":[]}` for the finished answer, which replaces the deltas shown so far.
  - `{"type":"done","context_messages":5,"answer":"...","turns":2,"input_tokens":20,"output_tokens":10,"model":{"alias":"default","provider":"cosmonic","resolved":"..."}}` once the turn is stored. `turns` counts the model calls the turn made.
  - `{"type":"failed","error":"..."}` instead of `done` when the turn fails.
- Every step is also written to the session journal as an `agent.step.v1` entry with the same `role`, `text`, `blocks` and `tools` fields, including the person's message and any `error` step. That journal is what the view draws.

To run it there, add it as a custom agent: in Settings, under Sandboxed agents, choose **Add an agent**, give its image, and use **Add a tool** for each tool component and the hosts it may reach. Desktop checks the image against its agent import allowlist before saving it, and builds each session's workload with the agent granted no hosts.

## License

Apache-2.0.
