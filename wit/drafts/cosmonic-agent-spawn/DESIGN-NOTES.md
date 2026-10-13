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
    .limits(Limits { max_turns: Some(3), max_tokens: Some(200_000), deadline: Some(mins(10)) })
    .spawn().await?;            // spawn::spawn(request); NeedsApproval -> await decision, resend

// run() = send(turn), drain progress for display, await the outcome
let review: Review = a.run(user("Start with the auth changes.")).await?.structured()?;
drop(a);                        // ends the child
```

## Open questions

**(a) One workload for agent and tools, or one per tool?** 

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

**(h) Virtualization versus a native plugin.** The skill says an interface must
not rely on ambient host identity. The ceiling is defined against "the
caller", which only the host knows; passing identity as a parameter would be
forgeable. We kept the native plugin and defined the caller as the component
the import is bound to. 

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
