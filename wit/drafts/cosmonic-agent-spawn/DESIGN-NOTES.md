# `cosmonic:agent/spawn` (draft)

A scaffold for the design jam on 2026-10-13, up for argument. The WIT is in
[`spawn.wit`](spawn.wit) and [`world.wit`](world.wit). It declares
`cosmonic:agent@0.4.0-draft` and uses published `cosmonic:agent@0.3.0` and
`wasi:clocks@0.3.0` types through `wkg.lock`; the published package is
unchanged and CI does not build `wit/drafts/`. To check it locally (`build.mjs`
needs `WKG_CONFIG_FILE` for any package that depends on `cosmonic:*`):

```sh
WKG_CONFIG_FILE=$PWD/wit/wkg-registries.toml \
  node .github/scripts/wit/build.mjs wit/drafts/cosmonic-agent-spawn agent
```

## Information-flow rules at spawn

The implementer enforces these; the native plugin sees the caller's workload
definition. The WIT states them on `spawn-types`.

- **Reach.** Read reach is the union, over the agent and all its tools, of read
  grants: volumes, secrets, interfaces classified as reading, and the model
  binding. Egress reach is the union of allowed hosts and writing interfaces. A
  read-write volume counts both ways.
- **Default ceiling.** The child's read reach and egress reach must each fit
  inside the caller's. Within the ceiling, results flowing up are not a leak.
  Containment compares what a grant reaches, not its name: two workloads can
  call different storage `workspace`, so a volume fits only if the same
  storage backs one of the caller's mounts. A grant the implementer cannot
  see or compare (today: secrets and interface grants) is beyond the ceiling.
- **Beyond the ceiling: `needs-approval`.** Approval is explicit
  declassification (deciding that data may cross a boundary it otherwise could
  not). The digest covers the caller's identity and the whole request,
  including `output { schema, max-bytes }`, `limits { max-turns, max-tokens,
  deadline }` and, for a sealed spawn, the `sink`. The `approval` carries a
  `decision` future: await it, then resend the identical request. Dropping it
  only stops waiting; the approval stays pending until it is decided or
  expires, and a grant admits one identical request from the same caller. Policy may hold an outcome until a person or the
  auditor releases it; the parent sees `blocked(outcome-release)`.
- **Downward flow.** Turns from `send` are caller-controlled content that can
  reach the child's egress. An approval for egress beyond the ceiling says so.
- **Plainly:** redaction is not a security boundary, because the model
  paraphrases. The final reply is a declassification channel by nature.
- **Steerable channels.** Counts, the finish code, the error kind, an
  `output-rejected` over `max-bytes`, and timing are all chosen by the child.
  Minor for an open agent; for a sealed agent they are the whole channel, so
  a sealed outcome carries only `finish` and its progress carries no counts.
- **Caller identity** is the stable workload identity (namespace, name,
  component name), never instance ids. A guest policy wrapper in front of the
  implementer would become the caller and its grants the ceiling, so policy
  wrappers are host-installed.

## Open vs sealed

| | `spawn` returns `agent` (open) | `spawn-sealed` (one run, no resource) |
| --- | --- | --- |
| Use when | the caller needs the result: research, review, a subtask | the child needs grants whose results the caller must never see: a privileged worker that writes to prod, files a ticket, or reads data the caller cannot |
| Granted by | `import spawn` (world `spawner`) | `import spawn-sealed` (world `sealed-spawner`) |
| Back to the caller | final reply parts, optional `structured`, counts | `working` and `blocked` progress, and a `sealed-outcome` (finish) only |
| Results go to | the caller, within `output` | an implementer-named `sink`: `person`, `approval-queue`, or a named `volume` |
| Guarantee | the child's reach fits inside the caller's, unless approved as declassification | no content channel to the caller; the sink is not caller-readable; the child's writable reach does not overlap the caller's readable reach; distinct credentials and egress identities |

Both need approval for grants beyond the caller's ceiling. Sealed is not
airtight: a child and caller that share an external service, or watch timing,
can still signal each other. The host may implement sealed later and return
`unavailable` until then.

## Turns: the `runnable-agent` world

`send` and `spawn-sealed` need a way to hand the child a turn. The 0.3.0
`agent` world only imports, so without one an implementer has to speak each
agent's own HTTP protocol through the host ingress. That bridge drops the
instructions, attachments and output schema, leaves the child reachable on
the ingress, and works for one agent at a time. The draft adds `turn`, an
export with `run(message, options)` returning progress and an outcome, and a
`runnable-agent` world (`agent` plus `export turn`) that spawned children
target. It is additive, so it does not force a breaking release. Cancelling a
turn means the agent stops its calls before the outcome resolves to
`cancelled`, which is what lets `send` promise that two turns never overlap
inside the child.

## Usage sketch

Pseudo-Rust:

```rust
#[derive(Deserialize, JsonSchema)]
struct Review { risks: Vec<String>, verdict: String }

let a = Agent::new(pinned("ghcr.io/acme/agents/reviewer:1.2.0", "sha256:9f…"))
    .model("bedrock-sonnet")                                    // binding name
    .instructions("Review the PR and list risks.")
    .tool("repo-fs", pinned("ghcr.io/cosmonic/tools/fs:0.2.0", "sha256:1c…"),
          Grants::none().volume("workspace", "/src", ReadOnly))
    .output::<Review>().max_bytes(16_384)                       // schema + ceiling
    .limits(Limits { max_turns: Some(3), max_steps: Some(12), max_tokens: Some(200_000),
                     deadline: Some(mins(10)) })
    .spawn().await?;            // spawn::spawn(request); NeedsApproval -> await decision, resend

// run() = send(turn), drain progress for display, await the outcome
let review: Review = a.run(user("Start with the auth changes.")).await?.structured()?;
drop(a);                        // ends the child
```

## Open questions

**(a) One workload for agent and tools, or one per tool?** Both run. One
workload works when the agent imports `tools` once per tool under labels that
name the tool components (`(implements ..)`, wasmCloud#5593); a single
unlabeled import with several exporters is refused. Labels fix the number and
names of tools when the agent is built, a missing or renamed component stops
the workload from starting, and swapping a tool restarts the agent, so labels
suit preset agents with a fixed tool set (see (l)). One workload per tool, with
the implementer routing `tools` calls, takes a tool set chosen at spawn time,
lets tools be added later, and lets the implementer see every tool call for
accounting. Each tool keeps its own grants either way. Proposal: one workload
per tool for `spawn`; labeled single-workload placement for presets.

**(b) Sink semantics.** What do `person` and `approval-queue` deliver to on
Desktop versus Control? Is `volume` enough for machine consumers? A spike
suggests Desktop's notifications inbox for `person` and its approvals view for
`approval-queue`; `volume` is enough once volumes are host-level objects, since
"not one the caller can read" is otherwise checked by name.

**(c) Lifetime versus store.** A resource dies with its store, so a parent that
lives for one request kills its children when the request ends. Do we need
host-owned agents a parent can reattach to? Decide together with durability.
This is not hypothetical: minimal-agent gets a fresh store per request, so it
can never keep a child across its own turns. Also, wasmtime does not run a
host resource's drop when its store is torn down, so the implementer has to
watch the caller's store to avoid leaking children. The draft now states that.
Options: a reattachable id scoped to the parent's session, or documenting
`spawn` as request-scoped and adding session-scoped spawning later.

**(d) What "fits inside" means.** Strict subset, or policy-defined containment
that understands wildcards (is `api.github.com` inside `*.github.com`)? The
draft now takes containment: `*.github.com` contains `api.github.com` but not
`github.com`, decided in one place next to the runtime's host matcher so the
two cannot drift.

**(e) Progress stream or host UI only.** Is `blocked` worth telling the parent?
Note that `steps` counts lag unless the implementer meters the child's model
calls, and `max-tokens` can only be enforced when a turn ends unless it does.
Metering means the spawn implementer owns or observes the child's inference
binding.

**(f) `tools` and the `tool-workload` sibling.** Accept `tool-workload` too
while it is a sibling, so the later switch is a rename. Dynamic loading later
becomes a method on the handle, through approval. The draft now accepts both
on `tool-request.component`.

**(g) Taint propagation, long term.** Narrowing egress as a child reads would
beat a fixed ceiling. It needs runtime revocation of `allowedHosts`.

**(h) Virtualization versus a native plugin.** The skill says an interface must
not rely on ambient host identity. The ceiling is defined against "the
caller", which only the host knows; passing identity as a parameter would be
forgeable. We kept the native plugin and defined the caller as the component
the import is bound to. Implementing it, host identity was reliable and
never forgeable, but the ids at hand are per instance and change on every
start, so a digest over them never matches after a restart. The draft now
names the stable workload identity, and requires policy wrappers to be
host-installed.

**(i) Sealed as one run.** The removal test took out `sealed-agent`. Revert if
we want multi-turn sealed workers. Building it found nothing that wanted a
second turn, and delivering to the sink once per run is natural. Separately,
"the child's writable reach does not overlap the caller's readable reach"
refuses any allowed host the caller can also reach, and "distinct credentials
and egress identities" has no enforcement in wash-runtime today. Decide
whether shared hosts are allowed when egress identities differ.

**(j) Cohesion with 0.3.0.** Published 0.3.0 uses `other(string)`, `denied`,
an `inference-types` interface rather than `types`, and no `@since`. The draft
follows the skill, so the package is now mixed. Do we align 0.3.0 in the same
release (breaking, so 0.4.0), or keep the draft on 0.3.0's style? The mixed
style caused no implementation problem; mixed versions did (see (k)).

**(k) 0.3.1 or 0.4.0?** The draft only adds interfaces, which the skill allows
in a patch. It needs 0.4.0 only if (j) aligns the existing interfaces. In
practice a component importing the draft also imports 0.3.0 `inference-types`,
so bindings carry two package versions side by side. A 0.3.1 resolves as one
package; a 0.4.0 should move every interface together so no component mixes
two `user-message` types. The `turn` export is additive and does not force
0.4.0.

**(l) Labeled imports for preset agents.** The skill prefers a labeled import
over a key parameter for a singular thing. A workload that always spawns the
same reviewer could import `reviewer: cosmonic:agent/preset` with a zero-key
`spawn: async func(instructions: string) -> result<agent, spawn-error>`,
checked before instantiation with no `needs-approval` at runtime. Keep dynamic
`spawn` for agents chosen at runtime. Worth adding?

**(m) Does dropping `approval.decision` withdraw the request?** The draft now
says no: dropping it stops waiting, and the approval stays pending until it is
decided or expires. Withdrawing on drop lost every approval a person gave
after the parent's request ended, which with per-request parents is nearly
all of them. If explicit withdrawal is wanted, make `approval` a resource
whose drop withdraws and keep `decision` a plain future. Open: single-use
grant (as drafted) or valid until expiry.

## What implementing it needs from the runtime

From building the draft as a native plugin on wash-runtime:

- **Volume identity.** Plugins see a caller's mount names, not the storage
  behind them, so the ceiling for volumes cannot be enforced. Needed: resolved
  mount sources exposed to plugins at bind, and host-level named volumes
  (Desktop: a managed list; Control: a named volume object).
- **The rest of a caller's reach.** Plugins cannot see secret names, interface
  bindings or their config, and there is no "this allowed-host entry contains
  that one" predicate.
- **Store teardown.** A supported signal that a caller's store is gone, since
  resource drop does not run on teardown.
- **A host handle for native plugins.** A plugin cannot start or stop
  workloads without the embedder injecting a handle, and nothing ties the
  workloads a plugin starts to their owner. Needed: owned workloads that stop
  with their owner.
- **Metering.** A per-workload hook on model calls, for live counts and for
  enforcing `max-tokens` mid-turn.
- **Types-only imports.** A spawner imports `inference-types` only for types;
  two plugins that both link it into one component collide.
