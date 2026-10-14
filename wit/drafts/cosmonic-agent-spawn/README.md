# Spawn and tools for `cosmonic:agent` 0.5.0

**Status: not published, for discussion.** These interfaces are proposed for
`cosmonic:agent@0.5.0`, the next version after 0.4.0 (PR #71). They live in
`wit/drafts/` and stay out of the `wit.yml` build and publish matrix, so they
cannot collide with the real `cosmonic:agent` package. No identifier carries
"draft"; generated Rust modules are `agent0_5_0` beside `agent0_3_0`. Rationale,
research and the stated defaults are in [DESIGN-NOTES.md](DESIGN-NOTES.md).

An agent running in a sandbox often needs help: a reviewer for one file, a worker
that files a ticket with credentials the agent must never hold, or a tool it did
not start with. This package gives a sandboxed agent four separate powers, each
its own import, so a workload is granted exactly the ones its world names:

- **spawn**: start a sub-agent and read its final replies;
- **spawn-confined**: start a one-run worker whose results go to a person, an
  approval queue or a volume, never back to the caller;
- **tool-discovery**: ask which tools are available to it;
- **tool-loading**: load a tool into itself while it runs.

Every tool is a component with its own grants (allowed hosts, volumes,
interfaces, secret names). The host is the only grantor: a request can ask for
more than the caller has, but only the host, by policy or a person, can approve
it. Credentials never pass through these interfaces; models and secrets are
named and the host resolves them.

These files depend on the published `cosmonic:agent@0.3.0` for messages, usage
and tool definitions, and on `wasi:clocks@0.3.0` for `duration`, both pinned in
`wkg.lock`. A WIT package may depend on another version of itself, so nothing
from 0.3.0 is copied in. When 0.4.0 publishes, the dependency moves to it, and
when the interfaces join the real package the dependency disappears.

## Vocabulary

| Term | Meaning here |
| --- | --- |
| run | One unit of work: a message in, the agent's loop of model and tool calls, a final reply out. `agent.run` starts one. Other systems call this a run (OpenAI Assistants API, OpenAI Agents SDK, LangGraph Platform, AutoGen), an invocation (Google ADK) or a turn (Codex app-server). |
| step | One model call inside a run, as in Assistants run steps and Vercel AI SDK steps. `limits.max-steps` bounds them; the OpenAI Agents SDK, Claude Agent SDK and Goose call that bound `max_turns`. |
| reach | What a request's grants can read (read reach) and send data to (egress reach). |
| ceiling | By default a child's or a loaded tool's reach must fit inside the caller's own. |
| confined | A child that cannot pass what it learned back to its caller, after Lampson's confinement problem (1973). Partial: timing and shared services remain. |
| caller | The component the import is bound to, identified by workload namespace, workload name and component name. |

## Worlds

| World | Imports | Exports | Who targets it |
| --- | --- | --- | --- |
| `spawner` | `cosmonic:agent/agent@0.3.0`, `spawn` | none | An agent that delegates and reads the results |
| `confined-spawner` | `cosmonic:agent/agent@0.3.0`, `spawn-confined` | none | An agent that hands privileged work off and never sees it |
| `tool-discoverer` | `cosmonic:agent/agent@0.3.0`, `tool-discovery` | none | An agent that may see the catalog |
| `tool-loader` | `tool-discoverer`, `tool-loading` | none | An agent that may change its own tool set |
| `harness` | `spawner`, `confined-spawner`, `tool-loader` | none | A top-level harness with every power |
| `runnable-agent` | `cosmonic:agent/agent@0.3.0` | `runnable` | Any agent a host can spawn |
| `tool-provider` | none | `tool-workload` | One tool component |
| `spawn-provider` | none | `spawn`, `spawn-confined` | The host plugin, or a test double |
| `tool-provider-host` | none | `tool-discovery`, `tool-loading` | The host side of discovery and loading |

Worlds compose with `include`. A coding agent that delegates and can itself be
spawned targets two of them:

```wit
package acme:coder@1.0.0;

world coder {
    include cosmonic:agent/spawner@0.5.0;
    include cosmonic:agent/runnable-agent@0.5.0;
}
```

A reviewer that should never spawn anything targets only `runnable-agent`. A
deploy agent that must not read what its worker does imports `confined-spawner`
and not `spawner`, so it has no way to spawn an open agent at all.

There is no guest-side policy world. A component placed between a caller and the
implementer would become the caller, and its own grants would become the
ceiling, so policy layers are installed by the host.

## Scenario 1: delegate a review and read the verdict

A coding agent asks a reviewer for a structured verdict. The reviewer gets one
tool, a file reader with read-only access to the workspace volume the coder
already reads, so the request fits inside the coder's ceiling and needs no
approval.

```wit
interface spawn {
    resource agent {
        run: async func(message: user-message) -> result<started-run, start-error>;
    }
    spawn: async func(request: spawn-request) -> result<agent, admission-error>;
}

// in `spawn-types`
type started-run = tuple<stream<progress>, future<result<run-outcome, run-error>>>;
record run-outcome {
    reply: list<reply-part>,
    structured: option<json>,
    counts: step-counts,
    finish: finish,
}
```

Pseudo-Rust:

```rust
#[derive(Deserialize, JsonSchema)]
struct Review { risks: Vec<String>, verdict: Verdict }

let reviewer = Agent::new(pinned("ghcr.io/acme/agents/reviewer:1.2.0", "sha256:9f…"))
    .model("bedrock-sonnet")                        // a binding name, never a key
    .instructions("Review the PR and list risks.")
    .tool("repo-fs", pinned("ghcr.io/cosmonic/tools/fs:0.2.0", "sha256:1c…"),
          Grants::none().volume("workspace", "/src", ReadOnly))
    .output::<Review>().max_bytes(16_384)           // schema plus size ceiling
    .limits(Limits { max_runs: Some(3), max_steps: Some(12),
                     max_tokens: Some(200_000), deadline: Some(mins(10)) })
    .spawn().await?;      // needs-approval: await `decision`, resend the same request

// run = agent.run(message), drain progress for display, await the outcome
let review: Review = reviewer.run(user("Start with the auth changes.")).await?.structured()?;
drop(reviewer);           // ends the child and stops its workloads
```

What the coder sees: contentless progress (`working`, `blocked`, step counts),
then the final reply and the structured verdict. It never sees the reviewer's
tool calls, tool output, or journal. Those stay on the host's observation path
for the person who owns the workload.

## Scenario 2: hand off a privileged write the caller must never see

An on-call agent decides a production config needs a change. A worker with the
deploy credentials makes it and reports to the approval queue. The on-call
agent learns only whether the run finished.

```wit
interface spawn-confined {
    variant sink { person, approval-queue, volume(string), other(string) }
    record confined-outcome { finish: finish }
    type confined-run = tuple<stream<progress>, future<result<confined-outcome, run-error>>>;
    spawn-confined: async func(request: spawn-request, sink: sink, task: user-message)
        -> result<confined-run, admission-error>;
}
```

The worker's grants (the deploy secret name, the production API host) are
beyond the on-call agent's ceiling, so `spawn-confined` returns
`needs-approval`. The approval's `detail` says the results go to the approval
queue, not to the caller, and its digest covers the request and the sink. Once
a person grants it, the identical request is admitted. The confined run's
progress carries only `working` and `blocked`, and its outcome is a finish code:
no reply, no counts, because every count is something the worker could steer to
signal its caller. The host also refuses a sink the caller can read, a worker
that could write anywhere the caller reads, and (until child and caller have
distinct egress identities) a worker that could reach any host the caller can.
The worker itself may hand work on only through `spawn-confined`: a confined
child that imports `spawn` does not start.

## Scenario 3: read untrusted content in quarantine

The agent-security literature mostly addresses the reverse case: a privileged
agent hands untrusted content (an inbound email, a scraped page) to a child
with no grants and accepts only a constrained answer. This package supports it
by convention:

- spawn the reader with no tools and no `allowed-hosts`, so it can reach
  nothing but its model;
- set `output.schema` to enums, numbers and booleans only, and a small
  `max-bytes`;
- have the harness code read only `structured`, and never put `reply` text into
  the privileged model's context.

```rust
#[derive(Deserialize, JsonSchema)]
struct Triage { category: Category, urgent: bool, confidence: u8 }   // no strings

let reader = Agent::new(pinned(QUARANTINE_READER, "sha256:…"))
    .model("local-small").output::<Triage>().max_bytes(256)
    .limits(Limits { max_runs: Some(1), max_steps: Some(1), ..Default::default() })
    .spawn().await?;
let triage: Triage = reader.run(user(&untrusted_email)).await?.structured()?;
```

The host enforces the empty reach and the schema; it does not enforce that the
schema has no free strings, and it always returns `reply`. Where the privileged
side must act on the content without reading it, use `spawn-confined` with
`sink: person`. DESIGN-NOTES.md says what this does and does not defend against.

## Scenario 4: find a tool, then use it

A harness asks what it can use before deciding what to give a child.

```wit
interface tool-discovery {
    variant fit { within-ceiling, needs-approval, other(option<string>) }
    record tool-offer {
        request: tool-request,
        summary: string,
        tools: list<tool-def>,
        fit: fit,
    }
    %list: async func(query: option<string>) -> result<list<tool-offer>, discovery-error>;
}
```

The host answers for this caller only: offers that fit its ceiling, and offers
beyond it that policy could approve, each marked with its fit. Anything policy
would never grant is hidden, even from a matching query. `query` keeps the
answer small by matching names, summaries and tool descriptions. Asking starts
nothing. An offer's
`request` is a ready-made `tool-request` (pinned component and grants), so the
harness passes it to `spawn` unchanged and the approval digest matches what the
host showed. Tool names and descriptions are written by the tool's author:
treat them as untrusted text when they reach a model.

## Scenario 5: load a tool into a running agent

Spawning is declarative: a child's tools are fixed when it starts. Changing a
running agent's own tool set is a separate power in a separate world.

```wit
interface tool-loading {
    resource loaded-tool {
        names: async func() -> list<string>;
    }
    load: async func(request: tool-request) -> result<loaded-tool, admission-error>;
}
```

`load` follows the same admission as `spawn`: within the ceiling it may load at
once, beyond it goes to a grantor, and policy may ask for approval anyway. The
host starts the tool as its own component and offers its functions through the
caller's existing `cosmonic:agent/tools` import, so the agent never carries a
router. Dropping the handle unloads the tool. A spawned child never receives
`tool-loading` in this version, and a parent cannot load a tool into a child it
spawned; it spawns a new child with a different tool set instead.

## Tools

A tool is one component exporting `tool-workload`, which is
`cosmonic:agent/tools@0.3.0` with one change: `list` returns a `result`, so a
tool that cannot describe itself fails instead of disappearing. This is the
shape the tools in cosmonic-labs/sandboxed-agents#24 export today under
per-tool names. The default, open to review, is that a later `cosmonic:agent`
version folds this change into `tools` itself and retires the separate name. `tool-request.component` also accepts a
component exporting `cosmonic:agent/tools@0.3.0`, for compatibility.

## Files

| File | Contents |
| --- | --- |
| [`spawn-types.wit`](spawn-types.wit) | `spawn-types`: the contract (caller, reach, ceiling, attenuation, authorship) and every shared record |
| [`spawn.wit`](spawn.wit) | `spawn` and the `agent` resource |
| [`spawn-confined.wit`](spawn-confined.wit) | `spawn-confined`, sinks, the confined outcome |
| [`runnable.wit`](runnable.wit) | `runnable`, the export a spawned child provides |
| [`tool-workload.wit`](tool-workload.wit) | `tool-workload`, one tool's surface |
| [`tool-discovery.wit`](tool-discovery.wit) | `tool-discovery` |
| [`tool-loading.wit`](tool-loading.wit) | `tool-loading` |
| [`world.wit`](world.wit) | The worlds above |

Every file declares `package cosmonic:agent@0.5.0;`, the same as the real
package directory will once these interfaces move there.

## Build locally

`build.mjs` needs `WKG_CONFIG_FILE` for a package that depends on `cosmonic:*`:

```sh
WKG_CONFIG_FILE=$PWD/wit/wkg-registries.toml \
  node .github/scripts/wit/build.mjs wit/drafts/cosmonic-agent-spawn agent
```

It fetches the two dependencies pinned in `wkg.lock`, checks the lock still
reproduces, validates the package, and writes `dist/agent.wasm` (declaring
`cosmonic:agent@0.5.0`). Never publish that file: it holds only these
interfaces, not the whole package.
