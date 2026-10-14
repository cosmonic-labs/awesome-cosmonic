# Spawn and tools for `cosmonic:agent`: design notes

**Status: not published, for discussion.** Started as the scaffold for the
design jam on 2026-10-13. The WIT is split by interface (see
[README.md](README.md) for the file and world layout and worked scenarios). It
declares `cosmonic:agent@0.5.0`, the next version after 0.4.0 (PR #71), and uses
the published `cosmonic:agent@0.3.0` and `wasi:clocks@0.3.0` through `wkg.lock`.
The published packages are unchanged, and CI neither builds nor publishes
`wit/drafts/`, so this directory cannot collide with the real `cosmonic:agent`
package. Every interface, type, function and world is marked
`@unstable(feature = agent-spawn)`, the WIT-native way to say "not published"
without putting "draft" in an identifier.

**Version tracking.** `cosmonic:agent@0.4.0` (#71) is not published yet (the
registry has 0.3.0 only, checked 2026-10-14), so these files track 0.3.0 until
0.4.0 publishes, then move to 0.4.0 so harness bindings share `user-message`
and the other inference types instead of carrying two versions. Merge them into
the real package before anyone builds on this. To check it locally:

```sh
WKG_CONFIG_FILE=$PWD/wit/wkg-registries.toml \
  node .github/scripts/wit/build.mjs wit/drafts/cosmonic-agent-spawn agent
```

A native wash-runtime plugin implements the interfaces in a local spike
(`~/cosmonic/probes/spawn-spike`, not published): `spawn` and
`tool-discovery.list` work end to end, a host-side dispatcher serves agents'
`tools` imports from toolsets that each run as their own workload, and
`tool-loading.load` is specified and answers `unavailable`.

## What changed in this revision

1. **Confined spawn is dropped.** The `spawn-confined` interface, the
   `confined-spawn-client` world, and its sink, outcome and run types are gone from
   the WIT, the worlds and the spike. Why, and when to reopen it, is in
   "Considered and dropped: confined spawn".
2. **No "draft" in any identifier; the interfaces are `cosmonic:agent@0.5.0`.**
   Eric asked for them in `cosmonic:agent` itself (`cosmonic:agent/spawn`,
   `cosmonic:agent/tool-discovery`, `cosmonic:agent/tool-loading` and so on)
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
   composed with `include`. The capability worlds `spawn-client`,
   `tool-discovery-client` and `tool-loading-client` each hold only their own
   import and include neither `cosmonic:agent/agent` nor each other, so each
   is exactly one grant; an agent writes `include agent; include spawn-client;`. `harness`
   includes the agent world and all three. `runnable-agent` is the world a
   spawned child targets, and `tool-provider` is one toolset component. The
   implementer worlds are `spawn-provider`, `tool-discovery-provider` and
   `tool-loading-provider`; the last also exports `cosmonic:agent/tools`,
   because loading only works when the same implementer serves the caller's
   `tools` import. (They replace `tool-provider-host`, formerly
   `tool-broker`: world names carry neither "host" nor "broker", which is
   reserved for host-wide services.) Import-side worlds end in `-client`
   because it pairs with `-provider` and matches the published
   `inference-client`. `tool-provider` keeps its name: it is the published
   0.3.0 world, and the per-tool components of sandboxed-agents#24 target it.
   The old `spawn-middleware` world is gone: a guest component in front of the
   implementer would become the caller, and its own grants the ceiling, which
   contradicts "policy wrappers are host-installed".
4. **Tool discovery and loading**, each its own interface and world (below).
5. **"Turn" is now "run"**: `agent.run`, `started-run`, `run-outcome`,
   `run-error`, `run-options`, `budget.max-runs`, and the child's export
   `run` (was `turn`, then `runnable`). `send` is `agent.run`, `send-error` is
   `start-error`. The survey is below.
6. **The reverse case and research grounding** are new sections. The interface
   did not change for them.
7. **Tool naming is three layers: identity, alias, display.** Calls are routed
   and authorized by the toolset's workload identity; the model sees an
   alias that carries no authority, its own name first, namespaced only on a
   later collision, and never renamed; people see display metadata.
   `toolset-spec` gains `expose-as`, the operator's override, and `name` is a
   DNS label of at most 32 characters. See "Tool naming: identity, alias,
   display".

Smaller changes: `spawn-error` is `admission-error`, shared by `spawn`
and `tool-loading.load`; `limit-scope` gained `loaded-toolsets`;
the spike now detaches instead of withdrawing when the caller drops an
approval's `decision`, as the WIT already said; `tool-discovery.list` takes an
optional query. The former open questions are now stated defaults (see
"Defaults, open to review").

### Design-skill review fixes

From a review against the WIT interface design skill:

- **One word for a component that offers tools: toolset.** `tool-request` is
  `toolset-spec`, `loaded-tool` is `loaded-toolset`, `tool-offer` is
  `toolset-offer`, and `spawn-request.tools` is `toolsets`. "Tool" stays for a
  single definition or call, as in 0.3.0 (`tool-def`, `tool-call`).
- **Accessors use `get-`.** `loaded-toolset.names` is `get-names`.
- **Aliases, not bindings, for models.** `spawn-request.model` is
  `model-alias`, matching 0.3.0's `models.open(alias)` and `unknown-alias`.
  "Binding" is kept only for credential bindings, a different idea (see
  "Follow-ups").
- **No unreachable finish code.** `finish.max-runs` is gone: an agent out of
  runs is refused on its next `agent.run`, never mid-run.
- **A dedicated exhaustion type.** `start-error.exhausted` carries `budget`
  (`runs`, `tokens`, `duration`, `other`), only what can leave an agent unable
  to take another run, instead of the whole `finish`.
- **A permanent failure case.** `start-error.failed(string)`: the child has
  ended for good and will never accept another run. The spike returns it for
  every run after a child fails to start.
- **`approval.summary`**, not `detail`: it is shown to a person, like
  `toolset-offer.summary`, unlike diagnostic detail strings.
- **Worlds**: capability worlds hold only their own import, `tool-loading-client` no
  longer includes discovery, and `tool-provider-host` is split in two (item 3
  above).

Second pass, the should-fix items:

- **`budget`, not `limits`.** The record is `budget`, with `max-runs`,
  `max-steps`, `max-total-tokens` and `max-duration` (was `deadline`);
  `finish.max-duration` and `finish.max-total-tokens` match. "Limit" is kept
  for the implementer's own caps (`limit-scope`, `over-limit`). The exhaustion
  variant is `budget-item`.
- **`max-total-tokens`** counts `usage.prompt-tokens` plus
  `usage.completion-tokens` over every model call of every run; cached,
  cache-write and reasoning tokens are parts of those and are not added again.
  0.3.0's `chat-options.max-tokens` bounds what one call generates, a
  different thing.
- **The resource stays `agent`**, not `subagent`. A spawned child is a full
  agent: the same world, the same run model, and it can spawn in turn within
  any operator limits, so "sub" names a relationship, not a distinct abstraction.
  Prior art agrees: Claude subagents and Google ADK `sub_agents` are plain
  agents in a hierarchy, and the OpenAI Agents SDK has only `Agent`. The
  resource is scoped to the `spawn` interface, so it does not collide with the
  `agent` world. Prose says "the caller" and "the child".
- **`tool-workload.list` has its own `list-error`** (`unavailable`, `other`).
  A read has no effects to be unsure of, so `call-error`, which #71 turns into
  `not-dispatched` and `outcome-unknown`, does not fit it.
- **The child's export is `run`**, after `wasi:cli/run`. The world keeps the
  name `runnable-agent`: "a runnable agent exports `run`" reads right, and
  renaming the world to `run-agent` would read as a verb.
- **The child's `run` returns `result<started-run, start-error>`**, symmetric
  with `agent.run`, so a child can refuse (`busy`, `invalid-input`) without
  starting. `start-error` moved to `spawn-types` so both share it, and gained
  `invalid-input(string)` for a message or options that cannot be taken.
- **`decision.denied`**, not `refused`, matching `access-denied` and 0.3.0's
  `denied`.
- **The agent component gets full `grants`**, like a toolset (was hosts only).
  The spike enforces it to the same degree as toolset grants: hosts and
  volumes are checked against the ceiling and wired into the agent's
  workload; secrets and interfaces always count as beyond the ceiling.
- **`run-counts`**, not `step-counts`, and the progress case is `counts`.
- **`ceiling-fit`** with `within-ceiling` and `beyond-ceiling`. Fit describes
  reach; policy may still require approval within the ceiling.
- **`@unstable(feature = agent-spawn)`** on every interface and world, and on
  every item inside them: wasm-tools refuses an ungated type or `use` inside a
  gated interface when the feature is off, which is also how WASI writes its
  own unstable interfaces (wasi:clocks `timezone`). `@unstable` replaces
  `@since` on these items, since an item carries one gate; `@since` returns
  when the interfaces are stabilized. Consumers opt in: wasmtime's `bindgen!`
  enables all features but its generated `add_to_linker` takes a
  `LinkOptions` the host must set `agent_spawn(true)` on, and wit-bindgen
  takes `features: ["agent-spawn"]`.

### Alignment with open PRs

- **#71** (agent interruption and dispatch outcomes, 0.4.0) touches only
  `wit/cosmonic-agent`. This package borrows its vocabulary where the same
  situation arises: cancellation is cooperative and is a request, not a
  guarantee; an outcome already recorded wins the race against a cancel;
  cancellation does not roll back effects; `admission-error` and `start-error`
  both mean "nothing ran", the same promise as #71's `not-dispatched`.
  `tool-workload.call` takes `call-error` from whatever `cosmonic:agent`
  version it depends on, so moving the dependency to 0.4.0 changes `call` to
  `not-dispatched` and `outcome-unknown`, and implementers and callers must be
  updated for it; it is not a free change. `tool-workload.list` has its own
  `list-error`, so the move does not touch it.
- **#70** (notifications 0.3.0): Victor asked for worlds and example scenarios
  in package READMEs. README.md follows that, with WIT excerpts beside each
  scenario.
- **cosmonic-labs/sandboxed-agents#24** (merged): each tool exports a
  per-tool interface shaped like the planned `tool-workload`, ships as its own
  image with its own grants, and the `toolbox` guest router is kept only for
  compatibility. This package defines `tool-workload` in exactly that shape
  and makes the host, not a guest, list and dispatch across tools.

## Information-flow rules

The implementer enforces these; the native plugin sees the caller's workload
definition. The WIT states them on `spawn-types`.

- **Reach.** Read reach is the union, over the agent and all its toolsets, of
  read grants: volumes, secrets, interfaces classified as reading, and the
  model alias. Egress reach is the union of allowed hosts and writing interfaces. A
  read-write volume counts both ways.
- **Default ceiling.** The child's read reach and egress reach (or a loaded
  toolset's) must each fit inside the caller's. Within the ceiling, results flowing
  up are not a leak. Containment compares what a grant reaches, not its name:
  two workloads can call different storage `workspace`, so a volume fits only
  if the same storage backs one of the caller's mounts. A grant the implementer
  cannot see or compare (today: secrets and interface grants) is beyond the
  ceiling.
- **Beyond the ceiling: `needs-approval`.** Approval is explicit
  declassification (deciding that data may cross a boundary it otherwise could
  not). The digest covers the caller's identity and the whole request,
  including `output { schema, max-bytes }` and `budget`; for a load, the
  `toolset-spec`. The `approval` carries a
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
  Each is a narrow channel, minor next to the reply itself.
- **Caller identity** is the stable workload identity (namespace, name,
  component name), never instance ids. Policy wrappers are host-installed.

## Runs: the `runnable-agent` world

`agent.run` needs a way to hand the child work. The 0.3.0 `agent` world only
imports, so without an export an implementer has to speak each agent's own HTTP
protocol through an ingress. The spike does exactly that with minimal-agent, and
the bridge drops the instructions, attachments and output schema, leaves the
child reachable on the ingress, and cannot pass `max-steps`. The package adds
`run`, an export named after `wasi:cli/run`, with
`run(message, options) -> result<started-run, start-error>`, and a
`runnable-agent` world (`agent` plus `export run`) that spawned children
target. The child can refuse a run without starting it, exactly as `agent.run`
can refuse its caller. It is additive. Cancelling a run means the agent stops
its calls before the outcome resolves to `cancelled`, which is what lets
`agent.run` promise that two runs never overlap inside the child. The spike
does not call the export yet, because minimal-agent does not have it.

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
`max-steps`. `budget.max-steps` says so in its doc comment. The Assistants API
is deprecated in favor of the Responses API (its runs become responses and its
run steps become items), but the run and step vocabulary it popularized is
still what LangGraph Platform and the Agents SDK use.

## Tool discovery and loading

Bailey's direction is dynamic tools: host-mediated discovery and loading,
approval-gated. Eric does not want a guest-side router. Spawning starts
declarative. The package keeps all three.

- **`tool-discovery.list`** returns the offers this caller may use or hand
  down. The implementer keeps the catalog (toolsets a person installed or
  policy allows, each a pinned component with the grants it would be given).
  Each `toolset-offer` carries a ready-made `toolset-spec`, an
  implementer-authored summary, the toolset's own tool definitions as
  catalogued, and its `ceiling-fit` (`within-ceiling` or `beyond-ceiling`).
  Offers policy would never grant are absent. An optional `query` keeps the
  answer small: it matches offer names, summaries, and tool names and
  descriptions, ignoring case. Asking starts nothing. Discovery is its own
  import because knowing what exists is information.
- **Tool descriptions are untrusted text.** A toolset's author writes its tool
  names, descriptions and schemas, and a model reads them. Discovery puts more
  of that text in front of models, so the implementer should show provenance
  (the pinned digest) and may curate what it catalogues.
- **`tool-loading.load`** loads a toolset into the caller while it runs. It is
  a separate interface and world (`tool-loading-client`) so no world gets it by
  accident, and `tool-loading-client` does not include discovery: a caller may be
  allowed to load toolsets it is given without browsing the catalog. It is not
  a method on the spawn `agent` handle: a parent that wants a child with more
  toolsets spawns a new child. Admission matches `spawn` (ceiling, approval
  digest over the caller and the `toolset-spec`), and policy may ask for
  approval even within the ceiling, since new code is new risk even when its
  reach is not. The implementer starts the toolset as its own component and
  offers its tools through the caller's existing `cosmonic:agent/tools`
  import, so it must serve that import too; `tool-loading-provider` exports
  both. `loaded-toolset.get-names` returns the aliases it assigned (see
  "Tool naming: identity, alias, display"); they are stable for the toolset's
  lifetime in the session. Dropping the handle unloads the toolset.
- **No guest router.** The implementer lists and dispatches across toolset
  workloads, as the spike's dispatcher does: each toolset runs as its own
  workload with its own grants, and the agent's `tools` import is served by
  the host.

Spike coverage: `discovery_lists_against_the_callers_ceiling` registers five
catalog entries and checks fits for a caller that reads `workspace` and reaches
`allowed.test` (a reader and a network toolset inside, a writer and an unknown
host beyond, a never-grant entry absent), checks that a caller with no reach
sees every offer as beyond, filters by query (a match never reveals a
never-grant entry), spawns an offer unchanged without approval, and checks
that `load` answers `unavailable`. Loading is not implemented: the
parent is not a spawned child, so its `tools` import is not routed by the
plugin, and serving it needs the runtime work listed at the end.

## Tool naming: identity, alias, display

An agent with several toolsets sees one flat list of tools, and two toolsets
can define the same tool name. Naming has to settle three questions that look
like one: which component a call reaches and with what authority, what string
the model selects a tool by, and what a person is shown. They are kept apart.

**Identity carries authority, and only the host sees it.** Each toolset runs
as its own workload, and that workload is the toolset's service principal:
its workload identity, as the implementer started it, with its deployment
incarnation and pinned digest. Every tool call is routed, authorized,
attributed and audited by that identity. The implementer's routing table maps
each alias to a toolset workload and the tool's own name, and a call is
checked against that table and nothing else. Audit records name the toolset
workload, the tool's own name, and the calling agent's workload and session.
Because each toolset is its own workload, each can also present its own egress
identity: workload identity built on mTLS (wasmCloud/wasmCloud#5653) and client
identity for outbound HTTPS, per host or per workload (wasmCloud/wasmCloud#5588),
apply to a toolset workload as to any other.

**Aliases are the model's selection key and carry no authority.** A guest
sees only authorized opaque aliases: strings in its own `tools` list that the
implementer resolves through the routing table. Neither an alias nor a
toolset's declared name ever feeds a routing or grant decision. Deriving
authority from a name would turn a naming bug into a security bug: a toolset
that managed to claim another's name, or a rename that reused one, would
inherit calls and grants meant for something else. The rules:

- A tool is offered under its own name if no alias in the agent's session has
  it yet.
- Only a later toolset whose tool collides is namespaced, as
  `<toolset>__<tool>`, and only that tool.
- An alias, once assigned, is fixed for the session. Adding a toolset never
  changes it, and an alias stays reserved for its tool even after its toolset
  is gone, so the conversation never sees one alias mean two tools. The table
  is persisted with the session state, so a restart or restore gives the same
  aliases.
- Toolsets declared up front are assigned in declaration order, which is
  deterministic and visible in configuration.
- An alias must be a valid model tool name: only `[A-Za-z0-9_-]`, at most 64
  characters. A tool with no valid free alias refuses the spawn or load.
- `toolset-spec.expose-as` is the operator's override: a prefix placed before
  every tool's own name, where the empty string means the own names. It wins
  over bare first. An alias it asks for that is already taken refuses the
  spawn or load with `invalid-request`; the implementer never renames it.

A prefix was chosen over a per-tool alias map because it is one string, it
needs no knowledge of a toolset's tool names before the toolset lists them,
and it covers the cases that matter: keep the own names (`""`), or claim a
namespace (`"gh__"`). Renaming single tools would also leave descriptions that
mention the old names.

**Display is metadata for people.** `tool-def.title` passes through
unchanged; a toolset that gives none gets the tool's own name as its title. A
human surface (the agent UI, an approval prompt, a log meant for people)
looks the alias up in the host and renders "title · toolset", never the wire
alias. Display names are metadata, never used for authorization: two tools may
share a title and still reach different workloads. Descriptions stay the
toolset author's words; the implementer does not prefix them.

**Trade-off.** Which toolset keeps a bare name depends on attach order: the
first one to claim it. That is deterministic and visible, but it is a choice
the order makes for the operator. `expose-as` lets the operator make it
explicitly. Tool descriptions should not name sibling tools, because the
model may know a sibling by an alias that is not its own name (the
`distance_km` tool in sandboxed-agents#24 tells the model to call
`iss_position` first).

Spike coverage: `bare_first_aliases_and_routing_by_identity`,
`a_later_attach_never_renames`, `aliases_survive_an_agent_restart`,
`expose_as_overrides_and_refuses_conflicts` and `alias_rules` in
`plugin/tests/toolsets.rs`, against the published minimal-agent 0.2.0 and the
unchanged sandboxed-agents#24 tools.

## The reverse case: quarantining untrusted content

Most of the agent-security literature addresses this case: a privileged agent
delegates the handling of untrusted content to a quarantined child with no
grants, and accepts only constrained output. Willison's Dual LLM pattern,
CaMeL, FIDES and the design-patterns paper all build on it (see "Research
grounding").

**What we support.** A `spawn` with no toolsets and empty `grants` is a
quarantined LLM in Willison's sense: it reaches nothing but its model, it
cannot act, and the host checks its answer against `output.schema` and
`max-bytes`. With a schema of enums, numbers and booleans, and harness code
that reads only `structured`, what returns to the privileged side is a few bits
of typed data, which is the low-capacity output FIDES and the map-reduce
pattern recommend.

**What we do not support.**
- *Free text still flows.* A string field in the schema, or the `reply` parts
  that every `run-outcome` carries, can hold injected instructions. The host
  does not check that a schema is closed, and it always returns `reply`. Only
  the harness code keeps them out of the privileged model's context.
- *No opaque references.* CaMeL and FIDES let the privileged side hold a
  variable it can pass to a tool but never read, and track where each value
  came from. Nothing here does that: a parent reads every outcome it
  receives, and there is no value it can hand to a second child without
  reading it.
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
supported by convention, and Scenario 2 in README.md shows how.

**What we never claim.** This does not block prompt injection. A quarantined
child can still be manipulated into a wrong answer within its schema; the
design limits what a manipulated child can do and say, not whether it is
manipulated. Side channels are not solved.

## Considered and dropped: confined spawn

**What it was.** `spawn-confined` started a one-run child whose results never
reached its caller and went to a sink instead (a person, an approval queue, or a
volume the caller could not read). That is Lampson's confinement property, with
the roles turned around: the caller is the party kept out.

**Why it was dropped.** The proposed use cases did not hold up.

- *The deploy worker and the ticket filer* wanted privilege separation, not
  confinement. Open `spawn` already gives that: the child's grants are checked
  against the ceiling, anything beyond it needs an approval bound to a digest,
  and secrets are named, never handed over. Those callers also want the result
  back.
- *The worker that handles a secret* is better served by keeping the secret
  away from every model and every component. In order of preference:
  1. Host-brokered credentials: the component names a credential binding and
     the host attaches the credential to outbound requests, the way model keys
     already work.
  2. A deterministic tool component with its own grants and a narrow WIT
     return type that carries no secret material, writing generated material
     straight into the secret store. The component model's typed interface is
     the confinement.
  3. Only when a model must read secret data: open `spawn` with
     structured-only, reply-withheld output and a closed schema (question (n)).
     This narrows the channel; it does not close it.
- *An auditor reviewing an agent* is started by host-side policy, not by the
  agent under review, so it needs no agent-facing spawn power.
- What confined would still add on top of open spawn (sink routing, checks that
  a child's writes never overlap its caller's reads, a guarantee that nothing
  ever comes back) does not justify its own interface, world and
  implementation.

**Reopen it** if a concrete use case appears that none of the above covers.

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
  determine the inputs to every channel). A quarantined child (the reverse
  case) is his confined service: its output schema is the legitimate channel,
  and its finish code, error kind and timing are his covert channels. The
  confined spawn this package once proposed reversed his roles and was
  dropped (see "Considered and dropped: confined spawn").
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
- **Quarantine (reverse):** triage an inbound email into a category and an
  urgency flag; extract a number from a scraped page; classify an
  attachment as safe or not before the privileged agent decides what to do.

### What these notes do not claim

Nothing here confines a child: its reply returns to its parent by design.
Covert and timing channels, and services a parent and child can both reach,
stay open, as Lampson, EROS, CaMeL and FIDES each say of their own systems.
Nothing here blocks prompt injection.

## Defaults, open to review

Each former open question below now has a stated default with its reasoning.
Every one is **default, open to review**: the package and the spike follow it
until someone argues it down.

**1. Folding `tool-workload` into `tools`: no longer a default.** It is now
open question (r), for Bailey.

**2. Caller-set budgets, optional operator limits, no budget carving.**
*Default, open to review.* Each spawn carries a budget its caller chooses
(`max-runs`, `max-steps`, `max-total-tokens`, `max-duration`), and it does not
have to fit inside the caller's own remaining budget. The spawn tree may be
bounded by optional operator limits, set in host or policy configuration and
unset by default: depth, total agents per caller tree, and concurrent agents
per caller. When one is set and reached, `spawn` returns `over-limit` with the
matching `limit-scope` (`depth`, `total-agents`, `concurrent-agents`) and the
limit. Privilege attenuation stays mandatory: a child's grants fit inside its
caller's ceiling unless a grantor approves.

Why: a spawned agent is just an agent, so nothing about being spawned should
give it a smaller or different budget model than any other agent. How big a
tree may grow is an operator decision about cost and capacity, made where the
operator already sets provider spend limits, not something each caller should
have to compute. And cost is not a security boundary: carving budgets would
complicate every harness without protecting anything that the ceiling and the
approval digest do not already protect.

*Enforced in* the spike's admission (`SpikeConfig::max_depth`, `max_agents`,
`max_concurrent`, all `None` by default); *tested by* `spawn_depth_is_capped`
(with `max_depth` set to 1, a child is refused; with 2, a grandchild is
refused), `no_limit_configured_allows_deep_chains` (with no limit set, a
chain four agents deep spawns and the leaf answers),
`max_agents_counts_the_whole_tree` (descendants and ended agents count toward
the tree's total; another caller is another tree) and
`over_limit_concurrent_agents`.

Prior art:

- **Claude Code and the Claude Agent SDK** let subagents spawn subagents and
  bound the tree with operator settings: `CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH`
  (default 3; 1 stops subagents from spawning), `CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS`
  (default 20), and an optional query-wide spend cap, `max_budget_usd`, with no
  limit by default ([subagents, "Cap subagent depth, concurrency, and
  spend"](https://code.claude.com/docs/en/agent-sdk/subagents)). These are
  limits on the whole tree, set by whoever runs the query, not budgets each
  subagent carves for its children. The docs note that earlier releases lack
  some of these limits or default differently.
- **Google ADK, CrewAI's hierarchical process and LangGraph subgraphs** fix the
  topology in code: ADK sub-agents and workflow agents that "operate based on
  predefined logic" ([workflow agents](https://adk.dev/agents/workflow-agents/)),
  a CrewAI manager that delegates among the agents listed on the crew
  ([hierarchical process](https://docs.crewai.com/en/learn/hierarchical-process)),
  and LangGraph subgraphs compiled into the parent graph
  ([subgraphs](https://docs.langchain.com/oss/python/langgraph/use-subgraphs)).
- **Iteration caps are per run and not inherited:** the OpenAI Agents SDK's
  `max_turns` ([running agents](https://openai.github.io/openai-agents-python/running_agents/)),
  LangGraph's `recursion_limit`
  ([graph API](https://docs.langchain.com/oss/python/langgraph/graph-api)),
  AutoGen's termination conditions
  ([termination](https://microsoft.github.io/autogen/stable/user-guide/agentchat-user-guide/tutorial/termination.html)),
  and CrewAI's per-agent `max_iter`, "Maximum iterations before the agent must
  provide its best answer. Default is 20"
  ([agents](https://docs.crewai.com/en/concepts/agents)).
- None of these carve a child's budget out of its parent's. Spend limits, where
  they exist, sit outside the agent definitions: a query-wide cap in the Claude
  Agent SDK, and provider or account limits elsewhere.

**3. Discovery shows what could be granted, marks it, and filters.** *Default,
open to review.* `tool-discovery.list` returns offers within the caller's
ceiling and offers beyond it that policy could approve, each marked with its
`ceiling-fit`, and hides anything never grantable, even from a matching query. Showing
approvable offers lets a harness plan a request a person can approve instead of
guessing; hiding never-grantable ones keeps the catalog from advertising what
no one will get. `list(query: option<string>)` keeps answers small: the query
matches names, summaries, and tool names and descriptions, ignoring case, and
the implementer may cap the count. *Enforced and tested in*
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

### Still open

**(a) One workload for agent and toolsets, or one per toolset?** Both run. Labels fix
the toolsets when the agent is built and suit presets; one workload per toolset,
with the host routing `tools`, takes toolsets chosen at spawn time and is
what `tool-loading` needs. Proposal: one workload per toolset for `spawn` and
`load`, labeled single-workload placement for presets.

**(c) Lifetime versus store.** A resource dies with its store, so a parent that
lives for one request kills its children (and its loaded tools) when the
request ends. Options: a reattachable id scoped to the parent's session, or
request-scoped `spawn` now and session-scoped spawning later.

**(e) Progress stream or host UI only.** `counts` frames lag unless the
implementer meters the child's model calls, and `max-total-tokens` can only be
enforced when a run ends unless it does.

**(g) Taint propagation, long term.** Narrowing egress as a child reads needs
runtime revocation of `allowedHosts`.

**(j) Style.** These interfaces follow the WIT design skill
(`other(option<string>)`, gates on every item); 0.3.0 and #71 do not. Align
when they join the real package. One known mismatch: this package says
`access-denied` in `admission-error` and `denied` in `decision`, while 0.3.0
uses a bare `denied(string)` for its refusals (`models.open-error`,
`tools.call-error`).

**(l) Labeled imports for preset agents.** A zero-key `spawn` under a labeled
import, checked before instantiation. Worth adding?

**(n) Structured-only output for `spawn`?** An output mode that withholds
`reply` and requires a closed schema (enums, numbers, booleans), so the host
rather than harness code enforces the quarantine pattern and an approval can
state how few bits come back; and opaque handles a parent can pass on but not
read. Neither is added; see "The reverse case". It is also the third option in
"Considered and dropped" for a model that must read secret data.

**(r) Fold `tool-workload` into `tools`, and at which version?** For Bailey.
On 2026-10-11 Eric chose a sibling interface so the published 0.3.0 `tools`
contract stays untouched: Goose, Pi, minimal-agent and Desktop's tools plugin
all import it, and the later switch is meant to be a rename plus an agent
rebuild. Victor's per-tool components in sandboxed-agents#24 are shaped to
match `tool-workload`. Changing `tools` itself belongs to the next real
`cosmonic:agent` version (#71 or later), and Bailey has said she is split on
the tools interface. The case for folding: two interfaces that differ only in
whether `list` can fail split every toolset and implementer in two, and the
toolbox adapter in #24 has to trap because 0.3.0's `list` cannot report a
failure. Until it is decided, `toolset-spec` accepts either export.

Settled earlier: (d) containment for allowed hosts, (h) the native plugin with
the caller as its stable workload identity, (k) the
two-version generated module names, (m) detach on drop.

## Follow-ups

- **Per-toolset secret bindings for toolset components.** A toolset's grants
  name secrets, but Desktop's custom-agent tool form has no secret field today.
- **Host-brokered credentials for tool egress.** A toolset component holds a
  credential reference: a stable logical name, not secret data. The operator
  creates a credential binding that maps the reference to a credential, scoped
  to one workload deployment. At call time the host resolves the reference for
  the calling workload and applies the credential to the outbound request,
  as an injected Authorization header, an mTLS client identity, or a signed
  request. The guest never sees the value. The host owns the authentication
  headers, so a value the guest supplies for one of them is rejected. This
  extends the named model-alias approach to toolsets, and it rests on each
  toolset being its own workload with its own identity (wasmCloud/wasmCloud#5653,
  wasmCloud/wasmCloud#5588).

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
  enforcing `max-total-tokens` mid-run.
- **Types-only imports.** A spawn-client imports `inference-types` only for types;
  two plugins that both link it into one component collide.
- **Host-routed `tools` for any workload** (for `tool-loading`). Today the spike
  routes `tools` only for children it started. Loading into an arbitrary caller
  needs the host to serve that caller's `tools` import and add or remove
  dispatch targets while it runs.
