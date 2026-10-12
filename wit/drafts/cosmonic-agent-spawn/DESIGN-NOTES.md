# `cosmonic:agent/spawn` (draft)

A scaffold for the design jam on 2026-10-13, up for argument. The WIT is in
[`spawn.wit`](spawn.wit) and [`world.wit`](world.wit). It declares
`cosmonic:agent@0.4.0-draft` and uses published `cosmonic:agent@0.3.0` types
through `wkg.lock`; the published package is unchanged and CI does not build
`wit/drafts/`. To check it locally (the `WKG_CONFIG_FILE` is needed because no
default registry knows the `cosmonic` namespace, a gap `build.mjs` will hit for
any package that depends on `cosmonic:agent`):

```sh
WKG_CONFIG_FILE=$PWD/wit/wkg-registries.toml \
  node .github/scripts/wit/build.mjs wit/drafts/cosmonic-agent-spawn agent
```

## What this replaces, and why agent-scoped

We had planned a general `wasmcloud:host` scheduling interface that lets a
workload define and start other workloads. `spawn` is the narrower replacement:
a caller describes an agent, its tools and their grants, and gets back a handle
it owns. Victor's point is the reason. A guest that can start any workload can
ask for anything a workload can do, so the blast radius (everything a mistake or
an attack can reach) is the whole workload API. A guest that can only spawn an
agent with a declared tool list can only ask for that, and the request is
semantic enough to write policy against. This is not session durability, which
iterates on the existing `cosmonic:agent/session` separately.

## Decisions

- **Native plugin.** Settled: the spawn plugin is native on Desktop and
  Control. It needs the host's workload API and the caller's identity anyway.
- **The handle is `resource agent`.** Bailey's full rule: a resource defines a
  lifetime and a proper noun; if she sees a resource she tries to remove it; it
  must be cohesive across the other APIs. Applied:
  - *Why not one session resource across APIs.* WIT cannot scope methods by
    capability, so one resource has one method set; the parent's view and the
    child's own journal would have to share it. `session@0.3.0` deliberately has
    no handle: its scope comes from the host-side workload id, so there is
    nothing to forge or leak. And a session resource would collide with the
    durability work.
  - *Why `agent` survives removal.* It carries ownership and lifetime: dropping
    it ends the child. Nothing else in the API expresses that.
  - Fallback name `delegate`, if the clash with `world agent` bothers Bailey.
- **One method, `send`, shaped like `chat.reply`.** It returns a contentless
  `stream<progress>` and a `future` of the outcome. Dropping the stream cancels
  the turn and the agent stays idle. One turn at a time (`busy`). The outcome
  holds final reply parts only (text, attachment, other: no tool use, reasoning,
  or continuation data), optional `structured` JSON validated against the
  request's schema, usage, step and tool-call counts, and why it finished.
  Removed by the removal test: `id`, `status`, `cancel`, `follow`, the initial
  `task`, and prompt-as-component (later).
- **No child-authored text anywhere the parent sees it** except the final reply.
  Progress, errors and reasons are host-authored codes or strings. Child text
  lives only on the host observation path.
- **Observation is host-side.** A host HTTP API: `/v1/agents` (lineage, request
  digest, approval, effective grants, state) and `/v1/agents/{id}/journal` (an
  SSE follow, meaning a server-sent event stream). Authorized for the person and
  the auditor role, never the parent guest. The tier-3 auditor workload may get
  an optional privileged `observe` or `audit` interface, not in `spawner`.
- **Digest-pinned components, per-component grants, names not secrets.** Empty
  grants nothing. `model` is a binding name as `models.open` takes it. `labels`
  are for logs and display, never policy input. Errors end in `other(string)`.

## Information-flow rules at spawn

The host enforces these; the native plugin sees the caller's workload definition.

- **Reach.** Read reach is the union, over the agent and all its tools, of read
  grants: volumes, secrets, interfaces the host classifies as reading, and the
  model binding. Egress reach is the union of allowed hosts and writing
  interfaces. A read-write volume counts both ways.
- **Default ceiling.** The child's read reach and egress reach must each fit
  inside the caller's. Within the ceiling, results flowing up are not a leak,
  since the caller could read the same data itself.
- **Beyond the ceiling: `needs-approval`.** Approval is explicit
  declassification (deciding that data may cross a boundary it otherwise could
  not). The digest covers the caller's identity, the request, its
  `output: output-spec { schema, max-bytes }` and its
  `limits { max-turns, max-tokens, deadline-ms }`. The approval detail says
  "results of this agent (schema S, at most N bytes) return to <caller>".
  Policy may hold an outcome until a person or the auditor releases it; the
  parent sees `blocked(outcome-release)`.
- **Downward flow.** Turns from `send` are caller-controlled content that can
  reach the child's egress. An approval for egress beyond the ceiling says so.
- **Plainly:** redaction is not a security boundary, because the model
  paraphrases. The final reply is a declassification channel by nature.

## Usage sketch

Victor's `a = new agent(); a.addTool(...); a.prompt(...); a.spawn()` becomes an
SDK builder over one declarative record. Pseudo-Rust:

```rust
#[derive(Deserialize, JsonSchema)]
struct Review { risks: Vec<String>, verdict: String }

let a = Agent::new(pinned("ghcr.io/acme/agents/reviewer:1.2.0", "sha256:9f…"))
    .model("bedrock-sonnet")                                    // binding name
    .instructions("Review the PR and list risks.")
    .tool("repo-fs", pinned("ghcr.io/cosmonic/tools/fs:0.2.0", "sha256:1c…"),
          Grants::none().volume("workspace", "/src", ReadOnly))
    .output::<Review>().max_bytes(16_384)                       // schema + ceiling
    .limits(Limits { max_turns: Some(3), max_tokens: Some(200_000), deadline_ms: Some(600_000) })
    .spawn().await?;            // spawn::spawn(request); NeedsApproval -> resend after release

// run() = send(turn), drain progress for display, await the outcome
let review: Review = a.run(user("Start with the auth changes.")).await?.structured()?;
drop(a);                        // ends the child
```

## Open questions for the jam

**(a) One workload for agent and tools, or one per tool?** Bailey's diagram and
today's Desktop session use one workload; on 10/10 we chose one per tool. A
separate investigation of the wash-runtime `active_ctx` race (concurrent calls
into one store seeing the wrong context) is underway, and we will not decide on
the basis of that bug. The WIT is agnostic: grants are per component either way.

**(b) Sealed agents, later.** A `spawn-sealed` returning a handle with no
content channel, whose output goes to a host-named sink. For a child that must
hold grants the parent should never see results of. Proposed for v1 unless
Bailey wants it now.

**(c) `agent` or `delegate`** for the handle name.

**(d) Lifetime versus store.** A resource dies with its store, so a parent that
lives for one request kills its children when the request ends. Do we need
host-owned agents a parent can reattach to? Decide together with durability.

**(e) What "fits inside" means.** Strict subset, or policy-defined containment
that understands wildcards (is `api.github.com` inside `*.github.com`)?

**(f) Progress stream or host UI only.** Is `blocked` worth telling the parent,
or should blocked state live only in the host UI?

**(g) `tools` and the `tool-workload` sibling.** Tool components export
`cosmonic:agent/tools` today; accept `tool-workload` too while it is a sibling,
so the later switch is a rename. Ask whether Bailey wants to iterate `tools`
now. Dynamic loading later becomes a method on the handle, through approval.

**(h) Taint propagation, long term.** Labelling data with where it came from
and narrowing egress as a child reads would beat a fixed ceiling. It needs
runtime revocation of `allowedHosts` in wash-runtime.

## Naming

Bailey's rule: "broker" and "gateway" mean host-wide (a credential broker is
one). Workload-tenanted things are not brokers. So: the spawn plugin, the caller
(spawner), the child agent, and the auditor.
