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

## What this replaces, and why agent-scoped

We had planned a general `wasmcloud:host` scheduling interface that lets a
workload define and start other workloads. `spawn` is the narrower replacement:
a caller describes an agent, its tools and their grants, and gets back a handle
it owns. Victor's point is the reason. A guest that can start any workload can
ask for anything a workload can do, so the blast radius (everything a mistake or
an attack can reach) is the whole workload API. A guest that can only spawn an
agent with a declared tool list can only ask for that, and the request is
semantic enough to write policy against. Session durability is separate work.

## Decisions

- **Native plugin.** Settled: the spawn plugin is native on Desktop and
  Control. It needs the host's workload API and the caller's identity anyway.
  The WIT is still written so another component can implement it (see the
  conformance section).
- **The handle is `resource agent`** (decided). Bailey's rule: a resource
  defines a lifetime and a proper noun; if she sees a resource she tries to
  remove it; it must be cohesive across the other APIs.
  - *Why not one session resource across APIs.* WIT cannot scope methods by
    capability, so the parent's view and the child's own journal would share
    one method set. `session@0.3.0` deliberately has no handle, and a session
    resource would collide with the durability work.
  - *Why `agent` survives removal.* It carries ownership and lifetime across
    turns: dropping it ends the child. Nothing else expresses that.
  - *Why `sealed-agent` did not.* A sealed agent's caller cannot see results,
    so a second turn is blind steering. `spawn-sealed` now takes one task and
    returns progress plus an outcome future; dropping the stream ends the child.
- **One method, `send`, shaped like `chat.reply`.** It returns a contentless
  `stream<progress>` and a `future` of the outcome. Dropping the stream (or the
  agent) cancels the turn. One turn at a time (`busy`). The outcome holds final
  reply parts only (text, attachment, other), optional `structured` JSON
  checked against the request's schema, counts, and why it finished. Removed
  earlier by the removal test: `id`, `status`, `cancel`, `follow`, the initial
  `task`, and prompt-as-component.
- **No child-authored text anywhere the parent sees it** except the final
  reply. Progress, errors and reasons are implementer-authored.
- **Observation is host-side.** A host HTTP API: `/v1/agents` (lineage, request
  digest, approval, effective grants, state) and `/v1/agents/{id}/journal` (a
  server-sent event follow). Authorized for the person and the auditor, never
  the parent guest. The auditor workload may later get a privileged `observe`
  interface, never in `spawner`.
- **Digest-pinned components, per-component grants, names not secrets.** Empty
  grants nothing. `model` is a binding name as `models.open` takes it. `labels`
  are for logs and display, never policy input.

## Information-flow rules at spawn

The implementer enforces these; the native plugin sees the caller's workload
definition. The WIT states them on `spawn-types`.

- **Reach.** Read reach is the union, over the agent and all its tools, of read
  grants: volumes, secrets, interfaces classified as reading, and the model
  binding. Egress reach is the union of allowed hosts and writing interfaces. A
  read-write volume counts both ways.
- **Default ceiling.** The child's read reach and egress reach must each fit
  inside the caller's. Within the ceiling, results flowing up are not a leak.
- **Beyond the ceiling: `needs-approval`.** Approval is explicit
  declassification (deciding that data may cross a boundary it otherwise could
  not). The digest covers the caller's identity and the whole request,
  including `output { schema, max-bytes }`, `limits { max-turns, max-tokens,
  deadline }` and, for a sealed spawn, the `sink`. The `approval` carries a
  `decision` future: await it, then resend the identical request. Dropping it
  withdraws a pending request. Policy may hold an outcome until a person or the
  auditor releases it; the parent sees `blocked(outcome-release)`.
- **Downward flow.** Turns from `send` are caller-controlled content that can
  reach the child's egress. An approval for egress beyond the ceiling says so.
- **Plainly:** redaction is not a security boundary, because the model
  paraphrases. The final reply is a declassification channel by nature.

## Open vs sealed

| | `spawn` returns `agent` (open) | `spawn-sealed` (one run, no resource) |
| --- | --- | --- |
| Use when | the caller needs the result: research, review, a subtask | the child needs grants whose results the caller must never see: a privileged worker that writes to prod, files a ticket, or reads data the caller cannot |
| Granted by | `import spawn` (world `spawner`) | `import spawn-sealed` (world `sealed-spawner`) |
| Back to the caller | final reply parts, optional `structured`, counts | progress and a `sealed-outcome` (coarsened counts, finish) only |
| Results go to | the caller, within `output` | an implementer-named `sink`: `person`, `approval-queue`, or a named `volume` |
| Guarantee | the child's reach fits inside the caller's, unless approved as declassification | no content channel to the caller; the sink is not caller-readable; the child's writable reach does not overlap the caller's readable reach; distinct credentials and egress identities |

Both need approval for grants beyond the caller's ceiling. Sealed is not
airtight: a child and caller that share an external service, or watch timing,
can still signal each other. The host may implement sealed later and return
`unavailable` until then.

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
    .limits(Limits { max_turns: Some(3), max_tokens: Some(200_000), deadline: Some(mins(10)) })
    .spawn().await?;            // spawn::spawn(request); NeedsApproval -> await decision, resend

// run() = send(turn), drain progress for display, await the outcome
let review: Review = a.run(user("Start with the auth changes.")).await?.structured()?;
drop(a);                        // ends the child
```

## Conformance with the WIT design skill

Checked against Bailey's `wit-interface-design` skill (cosmonic-labs/skills
PR #12). What changed and why:

- **One power per interface.** Open and sealed spawning are independently
  grantable, so they are now two interfaces, `spawn` and `spawn-sealed`, with
  shared types in `spawn-types`. A world that imports only `spawn-sealed`
  cannot read results, and the import list says so. New world `sealed-spawner`.
- **Virtualizable, documented by implementer and caller.** Docs say "the
  implementer" and "implementer-authored", not "the host". New world
  `spawn-middleware` (`import spawn; export spawn`) shows a policy wrapper or
  test double in front of the native plugin.
- **Reuse WASI types.** `limits.deadline-ms: u64` is now
  `deadline: option<duration>` from `wasi:clocks/types@0.3.0`, which the hosts
  pin. `wkg.lock` gained `wasi:clocks`.
- **No polling.** "Resend once approved" needed an out-of-band wait.
  `approval.decision` is now a `future<decision>`.
- **Typed errors, split by phase.** `send` returns `send-error` (`busy`,
  `exhausted`, `unavailable`) and the outcome future returns `turn-error`,
  mirroring chat's `request-error` and `generation-error`. `denied` is
  `access-denied` (the grant is partial). `over-limit.what: string` is a typed
  `limit-scope` the caller can act on. The sealed success type no longer holds
  failure codes (`sealed-finish` became `sealed-outcome` plus `turn-error`).
- **Catch-alls.** Diagnostic catch-alls are `other(option<string>)`, last.
  `sink.other(string)` stays a string because it names a destination.
- **Remove a resource if you can.** `sealed-agent` is gone (see Decisions).
- **Async and cancellation by drop.** Every function was already async. Docs
  now state the stream-then-future order, that a closed stream is not success,
  that the outcome can be awaited without draining, and what dropping the
  stream, the outcome, the agent, or the approval future does.
- **Versioning and docs.** `@since(version = 0.4.0-draft)` on every interface
  and world (wit-parser refuses `@since(version = 0.4.0)` in a `-draft`
  package); flip to `0.4.0` on release. Every type, case, field and function
  has a `///` comment, with OCI and JSON Schema references.

Already compliant: kebab-case and verb-first names, singular resource nouns,
`variant` plus `other` for every open set (no `enum`), all functions async,
symmetric provider and consumer shapes, the observe/control split (the parent
has no journal), and a new interface rather than widening an old one.

## Open questions for the jam

**(a) One workload for agent and tools, or one per tool?** Bailey's diagram and
today's Desktop session use one workload; on 10/10 we chose one per tool. A
separate investigation of the wash-runtime `active_ctx` race is underway, and
we will not decide on the basis of that bug. The WIT is agnostic.

**(b) Sink semantics.** What do `person` and `approval-queue` deliver to on
Desktop versus Control? Is `volume` enough for machine consumers?

**(c) Lifetime versus store.** A resource dies with its store, so a parent that
lives for one request kills its children when the request ends. Do we need
host-owned agents a parent can reattach to? Decide together with durability.

**(d) What "fits inside" means.** Strict subset, or policy-defined containment
that understands wildcards (is `api.github.com` inside `*.github.com`)?

**(e) Progress stream or host UI only.** Is `blocked` worth telling the parent?

**(f) `tools` and the `tool-workload` sibling.** Accept `tool-workload` too
while it is a sibling, so the later switch is a rename. Dynamic loading later
becomes a method on the handle, through approval.

**(g) Taint propagation, long term.** Narrowing egress as a child reads would
beat a fixed ceiling. It needs runtime revocation of `allowedHosts`.

Where the skill and our decisions pull apart:

**(h) Virtualization versus a native plugin.** The skill says an interface must
not rely on ambient host identity. The ceiling is defined against "the
caller", which only the host knows; passing identity as a parameter would be
forgeable. We kept the native plugin and defined the caller as the component
the import is bound to. Is that acceptable, or does Bailey want identity in the
contract?

**(i) Sealed as one run.** The removal test took out `sealed-agent`. Revert if
we want multi-turn sealed workers.

**(j) Cohesion with 0.3.0.** Published 0.3.0 uses `other(string)`, `denied`,
an `inference-types` interface rather than `types`, and no `@since`. The draft
follows the skill, so the package is now mixed. Do we align 0.3.0 in the same
release (breaking, so 0.4.0), or keep the draft on 0.3.0's style?

**(k) 0.3.1 or 0.4.0?** The draft only adds interfaces, which the skill allows
in a patch. It needs 0.4.0 only if (j) aligns the existing interfaces.

**(l) Labeled imports for preset agents.** The skill prefers a labeled import
over a key parameter for a singular thing. A workload that always spawns the
same reviewer could import `reviewer: cosmonic:agent/preset` with a zero-key
`spawn: async func(instructions: string) -> result<agent, spawn-error>`,
checked before instantiation with no `needs-approval` at runtime. Keep dynamic
`spawn` for agents chosen at runtime. Worth adding?

**(m) Does dropping `approval.decision` withdraw the request?** The draft says
yes (cancellation by drop). That loses an approval a person gives after the
parent's request has ended.

## Naming

Bailey's rule: "broker" and "gateway" mean host-wide (a credential broker is
one). Workload-tenanted things are not brokers. So: the spawn plugin, the caller
(spawner), the child agent, and the auditor.
