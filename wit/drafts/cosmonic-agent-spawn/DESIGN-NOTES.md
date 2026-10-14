# `cosmonic:agent-spawn`: design notes

**Status: not published, for discussion.** Started as the scaffold for the
design jam on 2026-10-13. The WIT is split by interface (see
[README.md](README.md) for the file and world layout and worked scenarios). It
declares `cosmonic:agent-spawn@0.1.0` and uses the published
`cosmonic:agent@0.3.0` and `wasi:clocks@0.3.0` through `wkg.lock`. The published
packages are unchanged, and CI does not build `wit/drafts/`. To check it locally:

```sh
WKG_CONFIG_FILE=$PWD/wit/wkg-registries.toml \
  node .github/scripts/wit/build.mjs wit/drafts/cosmonic-agent-spawn agent-spawn
```

A native wash-runtime plugin implements the package in a local spike
(`~/cosmonic/probes/spawn-spike`, not published): `spawn`, `spawn-confined` and
`tool-discovery.list` work end to end in 15 scenarios; `tool-loading.load` is
specified and answers `unavailable`.

## What changed in this revision

1. **"Sealed" is now "confined"** everywhere: `spawn-confined`,
   `confined-spawner`, `confined-outcome`, `confined-run`, and the spike's code
   and tests. Lampson's confinement problem (1973) is the exact property we
   want: a program that cannot pass what it learned to anyone but the
   recipients its owner chose. "Sealed" already means something else in
   object-capability systems: in the E language a sealer/unsealer pair wraps a
   value in a box only the matching unsealer opens. That is closer to an opaque
   reference than to a confined worker, and the two ideas meet in this design
   (see "The reverse case"), so the collision would mislead.
2. **No "draft" in any identifier.** The package is `cosmonic:agent-spawn@0.1.0`,
   matching its directory. Not `cosmonic:agent@0.4.0`: open PR #71 moves the
   whole agent package to 0.4.0 and is close to publishing, two directories
   cannot both declare and publish the same package and version, and an
   unsettled draft should neither hold 0.4.0 back nor ride along with it. A
   separate package also settles old question (k): bindgen generated both
   `agent0_3_0` and `agent0_4_0_draft` because the draft was a second version
   of `cosmonic:agent`. With one version per package the Rust modules are plain
   `agent` and `agent_spawn`. The "not published" status lives in the prose.
   Moving the interfaces into `cosmonic:agent` later is a rename.
3. **Worlds organize the package.** One file per interface, and worlds per role
   composed with `include`: `spawner`, `confined-spawner`, `tool-discoverer`,
   `tool-loader`, `harness` (all of them), `runnable-agent`, `tool-provider`,
   and the implementer worlds `spawn-provider` and `tool-broker`. The old
   `spawn-middleware` world is gone: a guest component in front of the
   implementer would become the caller, and its own grants the ceiling, which
   contradicts "policy wrappers are host-installed".
4. **Tool discovery and loading**, each its own interface and world (below).
5. **"Turn" is now "run"**: `agent.run`, `started-run`, `run-outcome`,
   `run-error`, `run-options`, `limits.max-runs`, `finish.max-runs`, and the
   child's export `runnable` (was `turn`). `send` is `agent.run`, `send-error`
   is `start-error`. The survey is below.
6. **The reverse case and research grounding** are new sections. The interface
   did not change for them.
7. **A confined outcome is a finish code and nothing else**, in the WIT, these
   notes and the spike. The spike used to return step and tool-call counts
   coarsened to powers of two and send `steps` progress frames during confined
   runs; it now returns `finish` only and sends only `working` and `blocked`.
   Finish only, because a confined child's every count is a channel it can
   steer to its caller, and nothing in the implementation needed the counts.

Smaller changes: `spawn-error` is `admission-error`, shared by `spawn`,
`spawn-confined` and `tool-loading.load`; `limit-scope` gained `loaded-tools`;
the spike now detaches instead of withdrawing when the caller drops an
approval's `decision`, as the WIT already said.

### Alignment with open PRs

- **#71** (agent interruption and dispatch outcomes, 0.4.0) touches only
  `wit/cosmonic-agent`. This package borrows its vocabulary where the same
  situation arises: cancellation is cooperative and is a request, not a
  guarantee; an outcome already recorded wins the race against a cancel;
  cancellation does not roll back effects; `admission-error` and `start-error`
  both mean "nothing ran", the same promise as #71's `not-dispatched`; and a
  cancelled confined run may already have delivered part of its results to the
  sink, which is #71's "outcome unknown". `tool-workload` takes `call-error`
  from whatever `cosmonic:agent` version it depends on, so moving the
  dependency to 0.4.0 picks up `not-dispatched` and `outcome-unknown` without
  a separate change.
- **#70** (notifications 0.3.0): Victor asked for worlds and example scenarios
  in package READMEs. README.md follows that, with WIT excerpts beside each
  scenario. The `person` sink is expected to deliver through notifications on
  Desktop (question (b)).
- **cosmonic-labs/sandboxed-agents#24** (merged): each tool exports a
  per-tool interface shaped like the planned `tool-workload`, ships as its own
  image with its own grants, and the `toolbox` guest router is kept only for
  compatibility. This package defines `tool-workload` in exactly that shape
  and makes the host, not a guest, list and dispatch across tools.

## Information-flow rules

The implementer enforces these; the native plugin sees the caller's workload
definition. The WIT states them on `types`.

- **Reach.** Read reach is the union, over the agent and all its tools, of read
  grants: volumes, secrets, interfaces classified as reading, and the model
  binding. Egress reach is the union of allowed hosts and writing interfaces. A
  read-write volume counts both ways.
- **Default ceiling.** The child's read reach and egress reach (or a loaded
  tool's) must each fit inside the caller's. Within the ceiling, results flowing
  up are not a leak. Containment compares what a grant reaches, not its name:
  two workloads can call different storage `workspace`, so a volume fits only
  if the same storage backs one of the caller's mounts. A grant the implementer
  cannot see or compare (today: secrets and interface grants) is beyond the
  ceiling.
- **Beyond the ceiling: `needs-approval`.** Approval is explicit
  declassification (deciding that data may cross a boundary it otherwise could
  not). The digest covers the caller's identity and the whole request,
  including `output { schema, max-bytes }`, `limits` and, for a confined spawn,
  the `sink`; for a load, the `tool-request`. The `approval` carries a
  `decision` future: await it, then resend the identical request. Dropping it
  only stops waiting; the approval stays pending until it is decided or
  expires, and a grant admits one identical request from the same caller.
  Policy may hold an outcome until a person or the auditor releases it; the
  parent sees `blocked(outcome-release)`.
- **Downward flow.** Messages from `agent.run` are caller-controlled content
  that can reach the child's egress. An approval for egress beyond the ceiling
  says so.
- **Plainly:** redaction is not a security boundary, because the model
  paraphrases. The final reply is a declassification channel by nature.
- **Steerable channels.** Counts, the finish code, the error kind, an
  `output-rejected` over `max-bytes`, and timing are all chosen by the child.
  Minor for an open agent; for a confined agent they are the whole channel, so
  a confined outcome carries only `finish` and its progress carries no counts.
- **Caller identity** is the stable workload identity (namespace, name,
  component name), never instance ids. Policy wrappers are host-installed.

## Open vs confined

| | `spawn` returns `agent` (open) | `spawn-confined` (one run, no resource) |
| --- | --- | --- |
| Use when | the caller needs the result: research, review, a subtask | the child needs grants whose results the caller must never see: a privileged worker that writes to prod, files a ticket, or reads data the caller cannot |
| Granted by | `import spawn` (world `spawner`) | `import spawn-confined` (world `confined-spawner`) |
| Back to the caller | final reply parts, optional `structured`, counts | `working` and `blocked` progress, and a `confined-outcome` (finish) only |
| Results go to | the caller, within `output` | an implementer-named `sink`: `person`, `approval-queue`, or a named `volume` |
| Guarantee | the child's reach fits inside the caller's, unless approved as declassification | no content channel to the caller; the sink is not caller-readable; the child's writable reach does not overlap the caller's readable reach; distinct credentials and egress identities |

Both need approval for grants beyond the caller's ceiling. Confinement is not
airtight. A child and caller that share an external service, or watch timing,
can still signal each other. "Distinct credentials and egress identities" has
no enforcement in wash-runtime today: two workloads that reach the same host
look the same to it. The host may implement `spawn-confined` later and return
`unavailable` until then.

## Runs: the `runnable-agent` world

`agent.run` and `spawn-confined` need a way to hand the child work. The 0.3.0
`agent` world only imports, so without an export an implementer has to speak
each agent's own HTTP protocol through the host ingress. The spike does exactly
that with minimal-agent, and the bridge drops the instructions, attachments and
output schema, leaves the child reachable on the ingress, and cannot pass
`max-steps`. The package adds `runnable`, an export with
`run(message, options)` returning progress and an outcome, and a
`runnable-agent` world (`agent` plus `export runnable`) that spawned children
target. It is additive. Cancelling a run means the agent stops its calls before
the outcome resolves to `cancelled`, which is what lets `agent.run` promise
that two runs never overlap inside the child.

## Naming the unit of work: "run", not "turn" or "prompt"

The question was whether the unit `agent.run` starts (a message in, the agent's
loop, a final reply out) should be called a turn or a prompt. The survey, from
primary docs:

| System | One request and the work it causes | One model call in the loop | Bound on the loop | Source |
| --- | --- | --- | --- | --- |
| OpenAI Agents SDK | run (`Runner.run`), "a single logical turn in a chat conversation" | turn ("agent-loop turns (LLM calls)") | `max_turns`, default 10, `MaxTurnsExceeded` | [running agents](https://openai.github.io/openai-agents-python/running_agents/) |
| Claude Agent SDK | `query()` | turn: "one round trip inside the loop" (output with tool calls, tools run, results fed back) | `max_turns`, counts tool-use turns, `error_max_turns` | [agent loop](https://code.claude.com/docs/en/agent-sdk/agent-loop) |
| Codex app-server | turn: "a single user request and the agent work that follows"; turns hold items, threads hold turns | item | none found | [app-server](https://learn.chatgpt.com/docs/app-server.md) |
| LangGraph | `invoke` | super-step, "a single iteration over the graph nodes" | `recursion_limit` (super-steps) | [graph API](https://docs.langchain.com/oss/python/langgraph/graph-api) |
| AutoGen AgentChat | `run(task)` returning `TaskResult` | turn (one agent speaks) | `max_turns`, termination conditions | [teams](https://microsoft.github.io/autogen/stable/reference/python/autogen_agentchat.teams.html) |
| Google ADK | invocation: "everything that happens in response to a single user query" | event | (not confirmed) | [event loop](https://adk.dev/runtime/event-loop/) |
| Vercel AI SDK | `generateText` | step: "one model generation and the execution of any local tools" | `stopWhen`, `isStepCount` (was `maxSteps`) | [tools](https://ai-sdk.dev/docs/ai-sdk-core/tools-and-tool-calling) |
| MCP | `sampling/createMessage` is one model call; prompts are templates (`prompts/get`) | not defined | none | [sampling](https://modelcontextprotocol.io/specification/2025-06-18/client/sampling), [prompts](https://modelcontextprotocol.io/specification/2025-06-18/server/prompts) |
| A2A | task ("the core unit of action"), message, `contextId` | not defined | none | [specification](https://a2a-protocol.org/latest/specification/) |
| Goose | `Agent::reply` | turn | `GOOSE_MAX_TURNS`, "consecutive turns … without user input", default 1000 | `aaif-goose/goose`, `crates/goose/src/agents/agent.rs` |
| Pi | `agent.prompt()` (`agent_start` … `agent_end`) | turn: "one LLM call + tool executions" | none in the README | [pi-mono agent README](https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/agent/README.md) |

"Turn" names three different things: one model round trip (OpenAI internally,
Claude, Goose, Pi), one user request (Codex), and one agent's speaking slot
(AutoGen). Worse for us, the old `max-turns` counted `send` calls, while
`max_turns` in the OpenAI Agents SDK, the Claude Agent SDK and Goose bounds
model calls, which is our `max-steps`. Anyone porting a harness would set the
wrong bound. "Prompt" is worse still: an MCP prompt is a template, the system
prompt is the instructions, and only Pi uses `prompt()` for the unit.

**Decision: "run" for the unit, "step" for one model call.** "Run" is what the
OpenAI Agents SDK (`Runner.run`) and AutoGen (`run(task)`) call the same unit,
ADK's "invocation" means the same, and it has no second meaning in this
package. "Step" matches the Vercel AI SDK and LangGraph and already named the
bound. `limits.max-steps` documents that other SDKs call it `max_turns`.

## Tool discovery and loading

Bailey's direction is dynamic tools: host-mediated discovery and loading,
approval-gated. Eric does not want a guest-side router. Spawning starts
declarative. The package keeps all three.

- **`tool-discovery.list`** returns the offers this caller may use or hand
  down. The host keeps the catalog (tools a person installed or policy allows,
  each a pinned component with the grants the host would give it). Each offer
  carries a ready-made `tool-request`, an implementer-authored summary, the
  tool's own definitions as catalogued, and its `fit` against the caller's
  ceiling (`within-ceiling` or `needs-approval`). Offers policy would never
  grant are absent. Asking starts nothing. Discovery is its own import because
  knowing what exists is information.
- **Tool descriptions are untrusted text.** A tool's author writes its names,
  descriptions and schemas, and a model reads them. Discovery puts more of
  that text in front of models, so the host should show provenance (the pinned
  digest) and may curate what it catalogues.
- **`tool-loading.load`** loads a tool into the caller while it runs. It is a
  separate interface and world (`tool-loader`) so no world gets it by
  accident, and it is not a method on the spawn `agent` handle: a parent that
  wants a child with more tools spawns a new child. Admission matches `spawn`
  (ceiling, approval digest over the caller and the `tool-request`), and policy
  may ask for approval even within the ceiling, since new code is new risk even
  when its reach is not. The host starts the tool as its own component and
  offers its functions through the caller's existing `cosmonic:agent/tools`
  import. That needs the caller's `tools` import to be host-routed, not linked
  to a sibling component. Dropping the `loaded-tool` handle unloads it.
- **No guest router.** The host lists and dispatches across tool workloads, as
  the spike's router already does for spawned children. Every tool is a
  component with its own grants.

Spike coverage: `discovery_lists_against_the_callers_ceiling` registers five
catalog entries and checks fits for a caller that reads `workspace` and reaches
`allowed.test` (a reader and a network tool inside, a writer and an unknown
host beyond, a never-grant entry absent), checks that a caller with no reach
sees every offer as beyond, spawns an offer unchanged without approval, and
checks that `load` answers `unavailable`. Loading is not implemented: the
parent is not a spawned child, so its `tools` import is not routed by the
plugin, and serving it needs the runtime work listed at the end.

## The reverse case: quarantining untrusted content

Most of the agent-security literature addresses the opposite direction from
`spawn-confined`: a privileged agent delegates the handling of untrusted
content to a quarantined child with no grants, and accepts only constrained
output. Willison's Dual LLM pattern, CaMeL, FIDES and the design-patterns paper
all build on it (see "Research grounding").

**What we support.** An open `spawn` with no tools and no `allowed-hosts` is a
quarantined LLM in Willison's sense: it reaches nothing but its model, it
cannot act, and the host checks its answer against `output.schema` and
`max-bytes`. With a schema of enums, numbers and booleans, and harness code
that reads only `structured`, what returns to the privileged side is a few bits
of typed data, which is the low-capacity output FIDES and the map-reduce
pattern recommend. Where the privileged side must pass the content on without
reading it, `spawn-confined` with `sink: person` delivers it to a person, not
to the caller.

**What we do not support.**
- *Free text still flows.* A string field in the schema, or the `reply` parts
  that every `run-outcome` carries, can hold injected instructions. The host
  does not check that a schema is closed, and it always returns `reply`. Only
  the harness code keeps them out of the privileged model's context.
- *No opaque references.* CaMeL and FIDES let the privileged side hold a
  variable it can pass to a tool but never read, and track where each value
  came from. Nothing here does that: a parent either reads an outcome
  (`spawn`) or never gets it (`spawn-confined`). There is no value it can hand
  to a second child without reading it.
- *No data-flow tracking.* CaMeL's guarantee comes from extracting control flow
  from the trusted query and tagging every value with its sources. This
  package checks reach at spawn time; it does not follow data through a run.
- *Steerable channels remain.* Even a boolean answer comes with a finish code,
  an error kind and timing the quarantined child chooses.

**Interface changes considered, not made.** A schema mode restricted to enums,
numbers, booleans and references, with `reply` withheld, would let the host
enforce the pattern instead of trusting harness code. Its clearest benefit is
for a grantor: a request whose output channel is a few bits is a much smaller
declassification to approve. An opaque handle (a host-held value the parent can
pass into another request but not read) would cover the CaMeL and FIDES case.
Neither clearly strengthens the model enough today. The first only helps when
the harness code is not trusted, and then the harness's own grants are the
larger problem; the second needs host-side value storage and provenance that
nothing else here has yet. Both are listed as question (n). The pattern is
supported by convention, and Scenario 3 in README.md shows how.

**What we never claim.** This does not block prompt injection. A quarantined
child can still be manipulated into a wrong answer within its schema; the
design limits what a manipulated child can do and say, not whether it is
manipulated. Side channels are not solved.

## Research grounding

### Classic

- **Confinement.** Lampson, "A Note on the Confinement Problem", CACM 16(10),
  1973 ([text](https://www.bwlampson.site/11-Confinement/WebPage.html)): the
  problem of "confining a program during its execution so that it cannot
  transmit information to any other program except its caller". He sorts the
  ways out into storage channels, legitimate channels (his example is the
  bill), and covert channels "not intended for information transfer at all",
  such as a service "varying its ratio of computing to input/output or its
  paging rate". His rules include transitivity (an untrusted program a confined
  one calls must be confined too) and masking (the caller must be able to
  determine the inputs to every channel). `spawn-confined` reverses the roles
  (the caller is the party kept out) but the property is his: the
  `confined-outcome` is the legitimate channel, kept to a finish code, and the
  shared services and timing we name as residual risk are his covert channels.
  The transitivity rule is why a confined child should not be able to spawn an
  open grandchild that reports to the caller (question (o)).
- **Information-flow control and declassification.** Myers and Liskov,
  "Protecting Privacy using the Decentralized Label Model", ACM TOSEM 9(4),
  2000 ([PDF](https://cs.brown.edu/courses/csci2390/2023/readings/jif.pdf)),
  following their SOSP 1997 paper
  ([page](https://www.cs.cornell.edu/andru/papers/iflow-sosp97/paper.html)).
  Labels are policies "of the form owner: readers", and the model "permits
  principals to declassify their own data, rather than requiring a trusted
  subject": "a principal may weaken only the policies that it has itself
  provided". Our approval is a declassification decided by the host, the one
  principal that owns every grant; there are no per-datum labels. They also say
  Jif "does not control certain covert channels that are more difficult to
  identify".
- **Capability confinement.** Shapiro and Weber, "Verifying the EROS
  Confinement Mechanism", IEEE S&P 2000
  ([PDF](https://www.cs.yale.edu/flint/cs428/doc/eros-verify.pdf)). The EROS
  constructor certifies that what it builds holds only capabilities that convey
  no mutate authority, plus a declared set of "known holes", so the product is
  confined except through the holes its requester approved. The KeyKOS factory
  did the same ([KeyKOS nanokernel
  paper](https://pdos.csail.mit.edu/6.828/2009/readings/keykos.pdf): "It is
  possible for a client to determine whether an object created by a factory is
  secure."). A `spawn-request` is our constructor: the child gets exactly the
  grants listed, nothing inherited, and anything beyond the caller's ceiling is
  a hole a grantor approves by digest. Shapiro and Weber state plainly: "This
  paper does not address the issue of covert channel suppression."
- **Sealers.** Miller's E provides "sealer/unsealer pairs… The sealer is like
  an encryption key, and the unsealer like a decryption key"
  ([erights.org](http://www.erights.org/elib/capability/ode/ode-capabilities.html)).
  That is the meaning "sealed" would have collided with.

### Contemporary

- **Dual LLM.** Willison, "The Dual LLM pattern for building AI assistants that
  can resist prompt injection", April 2023
  ([post](https://simonwillison.net/2023/Apr/25/dual-llm-pattern/)). A
  privileged LLM with tools, a quarantined LLM that "does not have access to
  tools, and is expected to have the potential to go rogue at any moment", and
  a controller that is "regular software, not a language model". The privileged
  LLM sees only variable names such as `$VAR1`. He says it "isn't a 100%
  reliable solution".
- **CaMeL.** Debenedetti et al., "Defeating Prompt Injections by Design",
  Google DeepMind, 2025 ([arXiv 2503.18813](https://arxiv.org/abs/2503.18813)).
  It "explicitly extracts the control and data flows from the (trusted)
  query", so untrusted data "can never impact the program flow"; its quarantined
  LLM "has no tool access" and answers in a predefined schema; values carry
  capability tags. It completes 77% of AgentDojo tasks with provable security,
  against 84% undefended. Its own limits: "CaMeL is vulnerable to side-channel
  attacks" (§7) and "prompt injection attacks are not fully solved" (§9.3).
- **FIDES.** Costa et al., "Securing AI Agents with Information-Flow Control",
  Microsoft, 2025 ([arXiv 2505.23643](https://arxiv.org/abs/2505.23643)).
  Confidentiality and integrity labels on every value, a `Hide` operation that
  replaces content with a variable passed by reference, and a quarantined LLM
  whose result has "a known type", ordered bool, enum, string: "Boolean and
  enumeration types can carry a bounded amount of information". It concedes it
  "does not prevent implicit flows due to data-dependent control flow".
- **Design patterns.** Beurer-Kellner et al., "Design Patterns for Securing LLM
  Agents against Prompt Injections", 2025
  ([arXiv 2506.08837](https://arxiv.org/html/2506.08837)). Principle: once an
  agent has ingested untrusted input, it "must be constrained so that it is
  impossible for that input to trigger any consequential actions". Six
  patterns: action-selector (map a request to fixed actions, no feedback),
  plan-then-execute (fix the plan before reading untrusted data), LLM
  map-reduce (an isolated LLM per untrusted item, outputs constrained, for
  example to a number), dual LLM, code-then-execute (the LLM writes a program
  that calls tools and sub-LLMs), and context-minimization (drop the user
  prompt from context once it has chosen the first action). Map-reduce and dual
  LLM map onto open `spawn` with no reach; plan-then-execute and
  code-then-execute live in the harness, above this package.

### How the SDKs delegate

- **OpenAI Agents SDK** has two modes
  ([multi-agent](https://openai.github.io/openai-agents-python/multi_agent/),
  [handoffs](https://openai.github.io/openai-agents-python/handoffs/)). With
  `Agent.as_tool()` a manager "keeps control of the conversation" and the
  specialist returns a result. With a handoff the specialist "becomes the
  active agent for the rest of the turn", with the full history by default.
  `spawn` is agents-as-tools: the parent keeps control and gets the final reply.
  There is no handoff here; a child never takes over its parent's conversation.
- **Claude Agent SDK subagents**
  ([subagents](https://code.claude.com/docs/en/agent-sdk/subagents)):
  "intermediate tool calls and results stay inside the subagent; only its final
  message returns to the parent", and the only content passed down is the
  prompt string. `spawn` keeps the same shape and adds what an in-process SDK
  cannot: each tool in its own sandbox with its own grants, a ceiling checked
  by the host, and approvals bound to a digest.

### Use cases

- **Open spawn:** a reviewer with read-only access to the repo returns a
  structured verdict; a researcher with one allowed host returns a summary; a
  test runner with the workspace volume returns pass or fail and a log excerpt.
- **Confined spawn:** a deploy worker holding production credentials applies a
  change and reports to the approval queue; a ticket filer posts to an issue
  tracker the caller cannot reach; an auditor reads a vault the caller cannot
  and writes findings to a volume only a person reads.
- **Quarantine (reverse):** triage an inbound email into a category and an
  urgency flag; extract a number from a scraped page; classify an
  attachment as safe or not before the privileged agent decides what to do.

### What these notes do not claim

Confinement is partial: covert and timing channels, and services both sides can
reach, stay open, as Lampson, EROS, CaMeL and FIDES each say of their own
systems. "Distinct credentials and egress identities" for confined children is
not enforced by wash-runtime today. Nothing here blocks prompt injection.

## Open questions

Letters are kept from the previous revision so earlier discussion still lines up.

**(a) One workload for agent and tools, or one per tool?** Both run. One
workload works when the agent imports `tools` once per tool under labels that
name the tool components (`(implements ..)`, wasmCloud#5593); a single unlabeled
import with several exporters is refused. Labels fix the number and names of
tools when the agent is built, suit preset agents, and cannot take a loaded
tool. One workload per tool, with the host routing `tools` calls, takes a tool
set chosen at spawn time, makes `tool-loading` possible, and lets the host see
every tool call for accounting. Proposal: one workload per tool for `spawn` and
`load`; labeled single-workload placement for presets.

**(b) Sink semantics.** What do `person` and `approval-queue` deliver to on
Desktop versus Control? Is `volume` enough for machine consumers? A spike
suggests Desktop's notifications inbox (`cosmonic:notifications`, #70) for
`person` and its approvals view for `approval-queue`; `volume` is enough once
volumes are host-level objects, since "not one the caller can read" is
otherwise checked by name.

**(c) Lifetime versus store.** A resource dies with its store, so a parent that
lives for one request kills its children when the request ends. minimal-agent
gets a fresh store per request, so it can never keep a child across its own
runs, and wasmtime does not run a host resource's drop when its store is torn
down. Options: a reattachable id scoped to the parent's session, or documenting
`spawn` as request-scoped and adding session-scoped spawning later. The same
applies to `loaded-tool`.

**(d) What "fits inside" means.** Containment, as drafted: `*.github.com`
contains `api.github.com` but not `github.com`, decided next to the runtime's
host matcher so the two cannot drift.

**(e) Progress stream or host UI only.** `steps` counts lag unless the
implementer meters the child's model calls, and `max-tokens` can only be
enforced when a run ends unless it does.

**(f) Resolved.** `tool-workload` is defined here in the shape of
sandboxed-agents#24, and `tool-request` accepts it or `cosmonic:agent/tools`.
Dynamic loading is `tool-loading`, not a method on the handle. Open: where
`tool-workload` finally lives (proposed: `cosmonic:agent`).

**(g) Taint propagation, long term.** Narrowing egress as a child reads would
beat a fixed ceiling. It needs runtime revocation of `allowedHosts`.

**(h) Virtualization versus a native plugin.** Kept the native plugin, with the
caller defined as the component the import is bound to, by stable workload
identity.

**(i) Confined as one run.** Kept. Separately, "the child's writable reach does
not overlap the caller's readable reach" refuses any allowed host the caller
can also reach, and "distinct credentials and egress identities" has no
enforcement in wash-runtime today. Decide whether shared hosts are allowed when
egress identities differ.

**(j) Cohesion with `cosmonic:agent`.** This package follows the WIT design
skill (`other(option<string>)`, `@since`, a `types` interface); 0.3.0 does not.
#71 keeps 0.3.0's style. Align when the interfaces move into `cosmonic:agent`.

**(k) Resolved** by the separate package (see "What changed").

**(l) Labeled imports for preset agents.** A workload that always spawns the
same reviewer could import `reviewer: cosmonic:agent-spawn/preset` with a
zero-key `spawn`, checked before instantiation with no `needs-approval` at
runtime. Worth adding?

**(m) Partly resolved.** Dropping `approval.decision` detaches, in the WIT and
now in the spike. Open: single-use grant (as drafted) or valid until expiry.

**(n) Reverse case in the interface?** A closed-schema output mode with `reply`
withheld, or opaque handles a parent can pass on but not read. Not added; see
"The reverse case".

**(o) Nesting and confinement.** Lampson's transitivity rule says a confined
child's own children must be confined too. The package does not yet say what a
confined child may spawn, and the spike forbids nesting (`max_depth` 1).
Proposal: a confined child gets no `spawn` at all, only `spawn-confined` to a
sink that is not its caller.

**(p) Discovery scope.** Should `tool-discovery.list` show offers beyond the
ceiling at all, or only `within-ceiling` ones unless policy says otherwise? The
spike shows both, with their fit. Should it take a query, or is the catalog
small enough to list?

**(q) Who loads into whom.** `load` attaches to the caller only. Should a
parent be able to load into a child it spawned? That would make the child's
tool set non-declarative and the parent a grantor of sorts; the draft says no.

## What implementing it needs from the runtime

From building it as a native plugin on wash-runtime:

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
  enforcing `max-tokens` mid-run.
- **Types-only imports.** A spawner imports `inference-types` only for types;
  two plugins that both link it into one component collide.
- **Host-routed `tools` for any workload** (for `tool-loading`). Today the spike
  routes `tools` only for children it started. Loading into an arbitrary caller
  needs the host to serve that caller's `tools` import and add or remove
  dispatch targets while it runs.
- **Egress identity** (for confinement). A way to give a child's outbound
  requests an identity distinct from its caller's, so "distinct egress
  identities" can mean something.
