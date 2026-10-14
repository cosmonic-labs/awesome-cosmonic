# Spawn and tools for `cosmonic:agent`: design notes

**Status: not published, for discussion.** Started as the scaffold for the
design jam on 2026-10-13. The WIT is split by interface (see
[README.md](README.md) for the file and world layout and worked scenarios). It
declares `cosmonic:agent@0.5.0`, the next version after 0.4.0 (PR #71), and uses
the published `cosmonic:agent@0.3.0` and `wasi:clocks@0.3.0` through `wkg.lock`.
The published packages are unchanged, and CI neither builds nor publishes
`wit/drafts/`, so this directory cannot collide with the real `cosmonic:agent`
package. To check it locally:

```sh
WKG_CONFIG_FILE=$PWD/wit/wkg-registries.toml \
  node .github/scripts/wit/build.mjs wit/drafts/cosmonic-agent-spawn agent
```

A native wash-runtime plugin implements the interfaces in a local spike
(`~/cosmonic/probes/spawn-spike`, not published): `spawn`, `spawn-confined` and
`tool-discovery.list` work end to end in 17 scenarios; `tool-loading.load` is
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
2. **No "draft" in any identifier; the interfaces are `cosmonic:agent@0.5.0`.**
   Eric asked for them in `cosmonic:agent` itself (`cosmonic:agent/spawn`,
   `cosmonic:agent/spawn-confined`, `cosmonic:agent/tool-discovery` and so on)
   at the next unpublished version after #71 takes the package to 0.4.0. The
   shared types interface is `spawn-types`, not a bare `types`, so it reads
   clearly beside `inference-types`. The files stay in
   `wit/drafts/cosmonic-agent-spawn/`, outside the `wit.yml` matrix, so CI
   never builds or publishes a second `cosmonic:agent` directory. The
   "not published" status lives in the prose.
   **Build approach: depend on 0.3.0, vendor nothing.** A WIT package may use
   another version of itself: each `package@version` is a distinct package to
   the resolver. These files `use cosmonic:agent/inference-types@0.3.0` and
   `cosmonic:agent/tools@0.3.0`, and the worlds `include
   cosmonic:agent/agent@0.3.0`. `build.mjs` fetches 0.3.0 from the registry,
   reproduces `wkg.lock` unchanged, and builds `cosmonic:agent@0.5.0`; the spike
   generates Rust modules `agent0_3_0` and `agent0_5_0`. Redefining the 0.3.0
   types inside 0.5.0 was the fallback and was not needed. The cost is the one
   noted in old question (k): a component importing these interfaces carries
   two `cosmonic:agent` versions until the interfaces join the real package,
   when the self-dependency disappears.
3. **Worlds organize the package.** One file per interface, and worlds per role
   composed with `include`: `spawner`, `confined-spawner`, `tool-discoverer`,
   `tool-loader`, `harness` (all of them), `runnable-agent`, `tool-provider`,
   and the implementer worlds `spawn-provider` and `tool-provider-host`
   (renamed from `tool-broker`, since "broker" is reserved for host-wide
   services). The old
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
approval's `decision`, as the WIT already said; `tool-discovery.list` takes an
optional query. The former open questions are now stated defaults (see
"Defaults, open to review").

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
| OpenAI Assistants API (deprecated in favor of Responses) | run: "an execution run on a thread"; threads hold messages | run step: "a step in execution of a run" | none | [`run.py`](https://github.com/openai/openai-python/blob/main/src/openai/types/beta/threads/run.py), [`run_step.py`](https://github.com/openai/openai-python/blob/main/src/openai/types/beta/threads/runs/run_step.py), [migration guide](https://developers.openai.com/api/docs/assistants/migration) |
| LangGraph Platform (LangSmith Deployment) | run: "an invocation of an assistant"; a thread is "a persistent conversation container that maintains state across multiple runs" | super-step (in the graph) | `recursion_limit` | [runs](https://docs.langchain.com/langsmith/runs), [threads](https://docs.langchain.com/langsmith/use-threads) |
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

**Decision: "run" for the unit, "step" for one model call.** Both words keep
meanings people already know:

- **Run.** The OpenAI Assistants API organized work as threads, then runs, then
  run steps: a run is "an execution run on a thread" and a run step is "a step
  in execution of a run", the same run and step pair used here. LangGraph
  Platform has the same shape: a thread "maintains state across multiple
  runs", and a run is "an invocation of an assistant". The OpenAI Agents SDK
  (`Runner.run`, "a single logical turn in a chat conversation") and AutoGen
  (`run(task)`) call the same unit a run, and ADK's "invocation" means the same.
- **Step.** One model call and the tools it asks for: Vercel AI SDK steps
  ("one model generation and the execution of any local tools"), Assistants
  run steps, and LangGraph super-steps (one iteration over the graph). It
  already named the bound, `max-steps`.

"Turn", by contrast, is inconsistent across SDKs, as the table shows, and the
one bound most harness authors know by that name, `max_turns`, means our
`max-steps`. `limits.max-steps` says so in its doc comment. The Assistants API
is deprecated in favor of the Responses API (its runs become responses and its
run steps become items), but the run and step vocabulary it popularized is
still what LangGraph Platform and the Agents SDK use.

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
  grant are absent. An optional `query` keeps the answer small: it matches
  offer names, summaries, and tool names and descriptions, ignoring case.
  Asking starts nothing. Discovery is its own import because
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
sees every offer as beyond, filters by query (a match never reveals a
never-grant entry), spawns an offer unchanged without approval, and checks
that `load` answers `unavailable`. Loading is not implemented: the
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
nothing else here has yet. Both stay open as question (n). The pattern is
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
  The transitivity rule is why a confined child may import `spawn-confined`
  but never `spawn` (default 2).
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

## Defaults, open to review

Each former open question below now has a stated default with its reasoning.
Every one is **default, open to review**: the package and the spike follow it
until someone argues it down.

**1. `tool-workload` folds into `tools`.** *Default, open to review.* A later
`cosmonic:agent` version changes `tools.list` to return
`result<list<tool-def>, call-error>` and retires the separate `tool-workload`
name. Two interfaces that differ only in whether discovery can fail would
split every tool and every host in two for no lasting reason; sandboxed-agents#24
already shows the fallible shape works, and its toolbox adapter has to trap
when a child's discovery fails because 0.3.0's `list` cannot report it. Until
then `tool-request` accepts either export.

**2. Confinement is transitive, and spawn depth is capped.** *Default, open to
review.* A confined child may import `spawn-confined` and never `spawn`.
Lampson's transitivity rule requires that anything a confined program calls is
confined too. Open `spawn` checks a grandchild only against its immediate
caller's ceiling, and a grantor approving it sees the confined child as the
caller; nothing re-checks the confinement rules against the original caller,
so an approved grandchild could gain reach the original caller shares.
`spawn-confined` applies those rules at every level, and its results go to a
sink rather than back up the chain. The host refuses a
confined child whose component imports `spawn` (it does not start), declares
only `spawn-confined` for confined children and both for open ones, and
refuses `spawn` from a confined caller at call time as a second check. Depth is
capped by the host (the spike's default is 1: children of a workload the person
started may not spawn at all) and returns `over-limit(depth)` with the cap.
*Enforced in* the spike's bind hook and `spawn`; *tested by*
`confinement_is_transitive_and_depth_is_capped` (an open child refused at depth
1; at depth 2 a child spawns a grandchild and the grandchild is refused; a
confined child importing `spawn` does not start and nothing reaches the sink).

**3. Discovery shows what could be granted, marks it, and filters.** *Default,
open to review.* `tool-discovery.list` returns offers that fit the caller's
ceiling and offers beyond it that policy could approve, each marked with its
fit, and hides anything never grantable, even from a matching query. Showing
approvable offers lets a harness plan a request a person can approve instead of
guessing; hiding never-grantable ones keeps the catalog from advertising what
no one will get. `list(query: option<string>)` keeps answers small: the query
matches names, summaries, and tool names and descriptions, ignoring case, and
the host may cap the count. *Enforced and tested in*
`discovery_lists_against_the_callers_ceiling`.

**4. A parent cannot load tools into a child it spawned.** *Default, open to
review.* Spawning stays declarative: a parent that wants different tools spawns
a new child. Loading into a child would make its tool set change under it and
make the parent a grantor of sorts for another agent's reach. Revisit once
loading exists at all. Stated in `tool-loading`.

**5. An approval grant is single use and bound to the request digest.**
*Default, open to review.* A grant admits one identical request from the same
caller before it expires, and nothing else: another caller, or the same caller
with any field changed (labels included), is a different digest and needs its
own approval. Standing permission belongs in policy a person wrote, where it
can be read, reviewed and revoked, not in a reusable approval that quietly
widens what a caller can do. Dropping `decision` stops waiting and does not
withdraw. *Enforced in* the spike's admission; *tested by*
`approval_grant_is_single_use_and_bound_to_the_digest`,
`ceiling_needs_approval_then_identical_resend` and
`dropping_the_decision_keeps_the_approval_pending`.

**6. Confined children may not reach hosts the caller can reach.** *Default,
open to review.* "The child's writable reach does not overlap the caller's
readable reach", read strictly, refuses any allowed host the caller can also
reach, because a shared host is a channel both can read and write. Distinct
egress identities would let a host tell the two apart, but wash-runtime does
not enforce them today, so the conservative reading stands until it does.
Stated in `spawn-confined`; *enforced in* the spike's confinement rules;
*tested by* `confined_run_to_sink`.

### Still open

**(a) One workload for agent and tools, or one per tool?** Both run. Labels fix
the tool set when the agent is built and suit presets; one workload per tool,
with the host routing `tools`, takes a tool set chosen at spawn time and is
what `tool-loading` needs. Proposal: one workload per tool for `spawn` and
`load`, labeled single-workload placement for presets.

**(b) Sink semantics.** `person` likely delivers through Desktop's
notifications (`cosmonic:notifications`, #70) and `approval-queue` through its
approvals view. `volume` is enough once volumes are host-level objects.

**(c) Lifetime versus store.** A resource dies with its store, so a parent that
lives for one request kills its children (and its loaded tools) when the
request ends. Options: a reattachable id scoped to the parent's session, or
request-scoped `spawn` now and session-scoped spawning later.

**(e) Progress stream or host UI only.** `steps` counts lag unless the
implementer meters the child's model calls, and `max-tokens` can only be
enforced when a run ends unless it does.

**(g) Taint propagation, long term.** Narrowing egress as a child reads needs
runtime revocation of `allowedHosts`.

**(j) Style.** These interfaces follow the WIT design skill
(`other(option<string>)`, `@since`); 0.3.0 and #71 do not. Align when they join
the real package.

**(l) Labeled imports for preset agents.** A zero-key `spawn` under a labeled
import, checked before instantiation. Worth adding?

**(n) Reverse case in the interface?** A closed-schema output mode with `reply`
withheld, or opaque handles a parent can pass on but not read. Not added; see
"The reverse case".

Settled earlier: (d) containment for allowed hosts, (h) the native plugin with
the caller as its stable workload identity, (i) confined as one run, (k) the
two-version binding names, (m) detach on drop.

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
