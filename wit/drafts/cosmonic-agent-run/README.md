# A typed run export for `cosmonic:agent`

**Status: in `cosmonic:agent@0.4.0`, unstable.** This note is the design of
a typed export, `cosmonic:agent/run`, that replaces the HTTP convention as
the contract between a sandboxed agent and whatever drives it. The WIT lives
in the 0.4.0 package beside the tools fold and the dispatch outcomes, with
every item marked `@unstable(feature = agent-run)`: 0.4.0 can publish with it,
and a consumer sees it only by enabling the feature. [Decisions](#decisions)
records the review questions and how each was settled.

To check it locally (the build fetches `wasi:clocks` and checks `wkg.lock`):

```sh
node .github/scripts/wit/build.mjs wit/cosmonic-agent agent
wasm-tools component wit dist/agent.wasm --all-features
```

| File | What it holds |
| --- | --- |
| [`run-types.wit`](../../cosmonic-agent/run-types.wit) | Function-less types: `run-options`, `run-event`, `step`, `run-outcome`, `start-error`, `run-error`, `started-run` |
| [`run.wit`](../../cosmonic-agent/run.wit) | The `run` interface an agent exports, with the journal rules |
| [`world.wit`](../../cosmonic-agent/world.wit) | `runnable-agent` (the `agent` world plus `export run`) and `run-client` (`import run`) |
| [`session.wit`](../../cosmonic-agent/session.wit) | `agent.run.v1` and `agent.run-result.v1` added to the reserved entry kinds |

## Why

Today Cosmonic Desktop drives a sandboxed agent over HTTP. The agent exports
`wasi:http/handler`, answers `POST /task` with `{"task": "..."}` by streaming
NDJSON lines (one JSON object per line: `delta`, `step`, `done`, `failed`),
answers `GET /history` as a readiness check, and refuses requests without the
`x-cosmonic-agent-session` header. The full protocol is the "Sandboxed agent
protocol" page of the Desktop docs.

That convention has three problems:

- **It is untyped.** Nothing checks a line's fields. Every client reimplements
  the parsing, and every agent reimplements the serialization, the header
  check, and the status codes. Goose, Pi, and the minimal agent already
  disagree on extra fields (`answer`, `turns`, `context_messages`, `state`).
- **It ties an agent to HTTP and to an ingress.** The agent needs a routable
  host name, so it is reachable by anything that can reach the ingress. The
  CSRF header exists only because Desktop's ingress listens on localhost,
  where a web page can send a cross-origin `POST`.
- **It assumes a chat request starts the work.** On Cosmonic Control the
  enterprise cases start a run from an event: a support agent reacting to a
  new ticket, or an event-driven "virtual employee" consuming messages over
  NATS. Those need to hand the agent work from a trigger, not to impersonate
  a browser.

A typed export fixes all three. The host calls the agent's export directly,
the same way a host calls a `wasi:http` handler or a tool's `tools.call`, so
there is no ingress, no header, and no line format to agree on. HTTP becomes
one adapter among several. Everything else (spawned children, Control
triggers, Desktop's next relay) is downstream of this contract, which is why
it comes first.

## The contract in four parts

Following the WIT design skill, a component's contract has four parts.

| Part | For a runnable agent |
| --- | --- |
| Triggers (exports) | `run(message, options)`: one unit of work. Nothing else starts it. |
| Responses | A stream of `run-event`s the driver owns, then a future of `run-outcome` or `run-error`. |
| Side effects (imports) | Unchanged: the `agent` world's `models`, `chat`, `tools`, `session`. The export adds no power. |
| Errors | `start-error` (refused, nothing ran) and `run-error` (failed after it started), both typed variants ending in `other`. |

## The interface

```wit
interface run {
    use inference-types.{user-message};
    use run-types.{run-options, started-run, start-error};

    run: async func(
        message: user-message,
        options: run-options,
    ) -> result<started-run, start-error>;
}

// in run-types
type started-run = tuple<stream<run-event>, future<result<run-outcome, run-error>>>;
```

The shape is `chat`'s: a call that is accepted or refused, then a stream and a
future. It is also exactly the shape of the `run` export in the spawn draft
(awesome-cosmonic PR #76), which this replaces; see
[One interface for hosts and parents](#one-interface-for-hosts-and-parents).

### Naming: run, not turn

The unit is a **run**: a message in, the agent's loop of model calls and tool
calls, and a final reply out. One model call is a **step**. The spawn draft
settled this vocabulary after surveying the OpenAI Assistants API (runs and
run steps), LangGraph (runs on threads), the OpenAI Agents SDK
(`Runner.run`), and others. On Desktop one run is one turn of the
conversation, so "turn" stays in Desktop's user-facing words, but the contract
says run. The interface is named after `wasi:cli/run`.

### Input

- **`message: user-message`**, from `inference-types`: text and attachment
  parts plus an optional author `name`. A chat message is one text part. An
  event is whatever the adapter makes of it: a text part, or an attachment
  with the event's media type (`application/json` for a ticket) so the model
  sees structured data, with `name` set to the source (`zendesk`, a subject).
- **`options: run-options`**:

| Field | Meaning |
| --- | --- |
| `run-id: string` | Unique in the session, chosen by the driver. A UUID, or the event's own id so a redelivered event is recognized. |
| `instructions: option<string>` | Instructions for this run, ahead of the agent's own. A spawned child gets its task here. |
| `schema: option<json>` | JSON Schema the reply must satisfy; the answer comes back as `run-outcome.structured`. |
| `max-steps: option<u32>` | Model calls in this run. Ends the run with `finish.max-steps`. |
| `max-total-tokens: option<u64>` | Prompt plus completion tokens across the run. Ends it with `finish.max-total-tokens`. |
| `max-duration: option<duration>` | Wall clock from acceptance (when `run` returns ok). Ends it with `finish.max-duration` and what it has. |
| `trace-parent: option<string>` | W3C traceparent, passed on to the agent's model calls. |

**There is no session id.** The session a run works in is the agent's
`cosmonic:agent/session` import. The host chooses the session a run works in
and binds it to the caller; a run never names one (decision 2). Naming a
session in the input would be a forgeable key; the WIT design skill's rule is
that a single thing a component may touch is a bound import, not a key
parameter. An agent must not rely on in-memory state surviving between runs:
everything it needs to continue is in its session.

### Streamed events

```wit
variant run-event {
    text-delta(string),
    reasoning-delta(string),
    step(step),
    counts(run-counts),
    compacted(compaction-info),
    other(option<string>),
}

record compaction-info {
    through: seq-range,      // {start, end}: the inclusive seq range summarized
    kept: u32,
    tokens-before: option<u64>,
    tokens-after: option<u64>,
    seq: option<u64>,        // the agent.compaction.v1 entry, when committed first
}

record step {
    content: step-content,
    seq: option<u64>,
}

variant step-content {
    assistant(assistant-message),
    tool(list<tool-result>),
    user(user-message),      // reserved: a message taken in mid-run
    error(string),
    other(option<string>),
}
```

- **`text-delta`** and **`reasoning-delta`** are text as the model produces
  it. A driver shows them as a reply in progress until the next `step`, which
  carries the whole message and replaces them. This is today's `delta`, with
  reasoning kept apart from the answer.
- **`step`** is one whole message, typed with the inference types the agent
  already holds: an assistant message (text, reasoning summary, tool calls
  with their arguments, attachments) or the tool results that answer it. An
  `error` step is an error the agent records in the conversation. The person's
  own message is never sent back; the driver already has it. A `user` step is
  reserved for a message the agent takes in during the run (see
  [Review outcomes](#review-outcomes-2026-10-16)); a driver renders it as the
  person's message.
- **`counts`** is cumulative after each model call: steps, tool calls, token
  usage. It doubles as a heartbeat.
- **`compacted`** says the agent replaced older context with a summary. It
  carries the same fields as the `agent.compaction.v1` journal entry except
  the summary and the model: the summary is model output, untrusted, and lives
  only in the journal, where a driver reads it if it shows it at all.

There is no separate status event. "Working" is implied by an open stream.
"Blocked" (a tool call waiting on a person's approval, a held outcome) is
something the host knows and the agent does not, so a driver authors it
itself, as the spawn draft's `progress.blocked` does.

### Outcome

```wit
record run-outcome {
    reply: list<reply-part>,
    structured: option<json>,
    counts: run-counts,
    finish: finish,          // done, input-required, max-steps, max-total-tokens, max-duration, other
    model: option<served-model>,  // alias and the concrete model, for display
}
```

A run that reaches a limit ends with an outcome, not an error: the limit is a
reason to stop, and the reply so far is useful. `reply` is the last assistant
step's reply without its tool calls. `counts` and `model` are what the agent
reports about itself; a host that meters model calls has the authoritative
record, which is where Desktop already reads a reply's cost from.

### Errors

`start-error` means nothing ran and nothing new was written:

| Case | When |
| --- | --- |
| `busy` | A run is in progress in this session, or a cancelled one has not stopped yet. |
| `duplicate` | This `run-id` already started here. Its outcome is in the journal. |
| `invalid-input(string)` | An empty message, an attachment the agent does not take, a schema it cannot follow. |
| `unavailable(string)` | The session could not be read, for example. Retry may succeed. |
| `other(option<string>)` | Treat as permanent. |

`run-error` means the run started and then failed. The person's message and
every step already sent are in the journal, and effects are not rolled back:

| Case | When |
| --- | --- |
| `cancelled` | The driver dropped the events stream. |
| `no-model(string)` | `models.open` refused the alias. |
| `model-refused(request-error)` | A model call was refused before acceptance. The inference case says what fixes it: `rate-limited` (wait), `quota-exhausted` (operator, with a top-up link), `not-authenticated`, `context-length-exceeded`. |
| `model-failed(generation-error)` | A model call failed after acceptance. |
| `not-recorded(string)` | The session refused a write the run needed, so the agent stopped rather than act without a record. |
| `superseded` | Another writer changed the session mid-run; the agent stopped before its next call. |
| `output-rejected(string)` | The reply did not satisfy `schema`. |
| `failed(string)` | Unclassified. |
| `other(option<string>)` | Treat as permanent. |

Reusing `request-error` and `generation-error` is the main gain over the
`failed` line, whose only field is a string. A client can now show a top-up
link for exhausted quota, or retry after a rate limit, without parsing text.
Tool failures do not end a run: they go back to the model as results, as
today.

### Acceptance

`run` resolves once the agent has committed the person's message (and
`agent.run.v1`, below) to the session. This is the promise minimal-agent, Goose
and Pi already keep before they answer `200`: a run that fails later still
shows what was asked. A refusal writes nothing.

### Cancellation

Dropping the events stream requests cancellation, as dropping `chat`'s chunk
stream does. The agent stops its model call (dropping the chunk stream, which
cancels generation at the host) and its pending tool calls, records each as the
session conventions say (`cancelled`, or `unresolved` when effects are
unknown), and resolves the outcome to `cancelled` only once nothing of the run
is still running. That last rule is what lets `busy` promise two runs never
overlap. An outcome already recorded wins the race. Dropping the outcome future
as well only stops listening; the agent still winds down and records the run.

An agent learns the stream was dropped when its next write to it fails. While
the model streams that is within milliseconds; while a tool call is pending it
is when the call returns. A driver that cannot wait that long cancels the
call itself after a grace period, as Desktop's spike does, at the cost of the
instance (see [Spike results](#spike-results)).

A driver that wants only the outcome still reads and discards events, because
dropping the stream means "stop". Events follow `chat`'s backpressure rule:
drain them before awaiting the outcome, and the agent may wait on a slow
reader.

### One run at a time

`busy` is enforced in two places. The **driver** is the authority
(decision 7): Desktop already refuses a second turn per session in its relay,
and the daemon takes that over. The host's lock has to cover every path into
a session, not only the typed one. The **agent** checks too, as defence
against two drivers on one workload (an HTTP adapter and a NATS trigger, say).
Its check is in memory, so it only sees runs on its own instance. A warm pool
of one does not cap a workload at one instance: the spike found that while a
run holds the warm instance, a request on the agent's ingress is served by a
second one. The session's conditional commits are what catch that: a second
run that slips past loses its next commit as `stale`, and both runs end
`superseded`, with every step kept in the journal.

### Limits

The agent enforces `max-steps`, `max-total-tokens` and `max-duration` itself,
so it can end gracefully with a reply. They are advisory, because the agent
counts its own steps and tokens. A driver enforces what it can meter: Desktop's
daemon keeps its own deadline a little past `max-duration` and cancels there,
and caps the bytes of each event and of the whole run as it reads them (today's
8 MB per line and 64 MB per turn). The spawn draft states the same split for
its budget.

### The journal

The agent still commits `agent.step.v1` for the person's message and every
step, and `agent.op.v1`/`agent.op-result.v1` around each model and tool call.
Nothing in the session conventions changes. The run adds two entry kinds:

- `agent.run.v1` `{ "run": "<run-id>" }`, in the commit that opens the run,
  with the person's message. That commit uses `run:<run-id>` as its
  `commit-id`, so a retained duplicate returns the original result without
  writing, which is how the agent recognizes `duplicate` cheaply. It can also
  find the entry by reading the journal.
- `agent.run-result.v1` `{ "run", "outcome": "done" | "failed" | "cancelled",
  "finish" | "error" }`, in the commit that ends it, written before the
  outcome resolves.

`agent.step.v1` entries gain an optional `run` field naming the run that wrote
them. Readers skip fields they do not know, so this is additive.

**Progress is a live view, the journal is the record.** Events carry content
(they do not merely point at journal entries) for three reasons: deltas are
never journaled; a spawn implementer and a NATS adapter must not need read
access to the session to drive a run; and a step can be sent before it is
committed, as Pi and the minimal agent do today. Each step carries the `seq`
of its journal entry when the agent committed it first, so a driver that also
follows the journal can match the two. A reopened conversation is drawn from
the journal, as today. The run entries give hosts what `openCalls` gives
Desktop for calls: a run with `agent.run.v1` and no `agent.run-result.v1` was
cut off, and the next run closes it as unresolved.

## One interface for hosts and parents

The spawn draft already has a `run` export that spawned children target. This
draft is the same function with the same name, the same two arguments, and the
same stream-plus-future shape. It differs in what flows back:

| | Spawn draft's `run` export | This draft |
| --- | --- | --- |
| Options | `instructions`, `schema`, `max-steps` | Those, plus `run-id`, `max-total-tokens`, `max-duration`, `trace-parent` |
| Progress | Contentless: `working`, `blocked`, `counts` | `text-delta`, `reasoning-delta`, `step`, `counts` |
| Errors | `start-error` and `run-error` shared with the parent's view | Agent-authored and typed: model errors, `not-recorded`, `superseded`, `duplicate` |

**Proposal: one export, two views.** The agent exports this `run`, and it is
the only interface a runnable agent implements. Every driver calls it: a host
for a top-level session, and the spawn implementer for a child. A parent agent
never calls a child's export. It calls `spawn`'s `agent.run`, which the
implementer serves by calling the child's export and narrowing what comes back.

The narrowing is why the parent's types must stay separate. The spawn draft's
information-flow rule is that nothing child-authored reaches the parent except
the final reply: progress, errors and reasons are implementer-authored. Its
contentless `progress` type makes that a property of the type, not a promise.
If the parent received `run-event`, the type would admit text deltas and
steps that policy promises never arrive. Desktop and Control, on the other
hand, need the live text and steps for a person to watch. So:

- **The export is rich** (this draft). The spawn draft drops its own `run`
  interface and `runnable-agent` world and depends on these.
- **`spawn-types` keeps the parent's view:** `progress`, `started-run`, and
  its own `start-error` (adding `exhausted` and `failed`, which only an
  implementer that holds a budget can know) and `run-error` (adding
  `withheld` and `unavailable`, implementer-authored).
- **They share the plain records:** `run-outcome`, `run-counts`, `finish` and
  `reply-part` move to `run-types`, and `spawn-types` uses them. They are
  identical today, except that `run-outcome` gains `model`, which the
  implementer fills from its own metering.
- **The implementer maps one to the other.** It mints the child's `run-id` (so
  `duplicate` never reaches the parent), turns the parent's lifetime budget
  into per-run options (remaining tokens become `max-total-tokens`, remaining
  time becomes `max-duration`), reads the child's events to emit `working` and
  `counts`, discards deltas and steps, replaces the child's error details with
  its own, and checks the outcome against `output.schema` and `max-bytes`.

A person who wants to watch a child work does it in the host's own view of the
child's session, never through the parent.

## From HTTP and NDJSON to the typed export

| Today | Typed export |
| --- | --- |
| `POST /task` `{"task": s}` | `run(user-message { content: [text(s)], name: none }, options)`; the driver mints `run-id` |
| `x-cosmonic-agent-session` header, `403` without it | Not needed: there is no ingress. Only the HTTP adapter keeps the check. |
| `400` for a bad body | `start-error.invalid-input` |
| `409` when the session kept changing (Goose, Pi) or was being written (minimal agent) | `start-error.busy` |
| `500` before the stream | `start-error.unavailable` or `other` |
| `200` with an NDJSON body | `ok(started-run)` |
| `delta` `{text}` | `run-event.text-delta` |
| (none) | `run-event.reasoning-delta` |
| `step` role `assistant`, `text`, `blocks` (`text`, `toolRequest`, `thinking`), `tools` (names) | `step` with `assistant(assistant-message)`: text parts, reasoning summaries, and `tool-use` parts carrying the call's id, name and arguments |
| `step` role `tool`, `blocks` (`toolResponse`, `image`), `images` (data URLs) | `step` with `tool(list<tool-result>)`: text and attachment outputs, with the bytes |
| `imagesDropped` | No field. An agent that leaves an attachment out says so in a text part; a driver that drops one over its cap marks it in its own view. |
| `step` role `error`, or `error` in `blocks` | `step` with `error(string)` |
| `step` role `user` | Never sent, as today |
| `done` `input_tokens`, `output_tokens` | `run-outcome.counts.usage.prompt-tokens`, `completion-tokens` |
| `done` `model` `{alias, provider, resolved}` | `run-outcome.model` `{alias, model}`; `provider` was always `cosmonic` |
| `done` `answer` | `run-outcome.reply` |
| `done` `turns` | `run-outcome.counts.steps` |
| `done` `context_messages`, `state` (Pi) | Dropped: the agent's own state, in the session |
| `failed` `{error}` | `err(run-error)`, typed |
| Stream closed without `done` or `failed` | The outcome future closes without a value: the instance died. The driver reports an interrupted run, and the next run closes it in the journal. |
| Stop drops the connection | Drop the events stream |
| `GET /history` readiness poll, every 1.5 s for 90 s | None. The host knows when the workload's export can be called. |
| `GET /history` body, `DELETE /history` (Pi) | Not part of the contract. The conversation is read from the host's copy of the journal, and the session is deleted with the workload. |
| 10 minutes per turn, wall clock | `run-options.max-duration`, plus the driver's own deadline just past it |
| 8 MB per line, 64 MB per turn | The driver's caps per event and per run |
| One turn per session (Desktop's relay) | `start-error.busy`, and the driver serializes |

### What the `POST /task` adapter does

The adapter keeps every existing client working, byte for byte. Given an agent
that exports `run`:

1. Refuse without `x-cosmonic-agent-session` (`403`). Parse `{"task": s}` or
   answer `400`.
2. Call `run` with a one-text-part message and `run-options { run-id: <new
   UUID>, max-duration: 10 minutes }`.
3. Map a refusal to a status: `busy` and `duplicate` to `409`,
   `invalid-input` to `400`, `unavailable` to `503`, `other` to `500`, each with
   `{"error": ...}`.
4. Answer `200` with `application/x-ndjson` and write one line per event:
   `text-delta` as `delta`; `step` as a `step` line, deriving `text`, `blocks`,
   `tools`, and `images` (only `image/png`, `jpeg`, `gif`, `webp` attachments,
   as data URLs); `reasoning-delta` and `counts` as nothing (Desktop ignores
   lines it does not know, but there is no reason to send them).
5. On the outcome write `done` (`input_tokens`, `output_tokens`, `model`,
   `answer`, `turns`); on an error write `failed` with the case and its detail.
6. When the client goes away, drop the events stream.

It is a hundred or so lines, written once. It lives **in the host**
(decision 8): Desktop's daemon serves a route that calls the export and
writes today's lines, and Control's HTTP ingress can do the same, so an agent
eventually needs no HTTP handler at all. Two other places were considered:

- **In the agent.** The agent keeps exporting `wasi:http/handler`, and its
  handler calls its own `run`. This is what agents do during the transition,
  so older Desktops keep working, but it is not where the adapter ends up.
- **As a composed component** targeting `run-client` plus an export of
  `wasi:http/handler`, plugged in front of any agent with `wac`. This is the
  virtualizable form, and stays possible: the contract does not change.

## How a host drives it

### Cosmonic Desktop

The daemon calls the agent's export through wash-runtime's plugin-initiated
dispatch, the mechanism Desktop's toolset dispatcher already uses to call
tool exports. A host plugin resolves a dispatch target for a workload item
while the workload resolves, then runs a `GuestCall` (its own code, on the
instance the host picks, under that store's `Accessor`) for each call.

The runtime side is public, in
[wasmCloud/wasmCloud](https://github.com/wasmCloud/wasmCloud) at `dad8b72`,
the commit Desktop pins (paths under `crates/wash-runtime/src/`):

| What | Where |
| --- | --- |
| Plugin-initiated dispatch, and why it exists (push-mode capabilities) | `engine/dispatch.rs:1` to `:9` |
| A plugin resolves a dispatch target when the workload resolves | `engine/workload.rs:2018` (`dispatch_target(item_id, plugin)`) |
| The `GuestCall` trait, and its default deadline | `engine/dispatch.rs:139`, `:157`; the default is `WASH_EPHEMERAL_CALL_TIMEOUT_SECS`, 600 s (`timeouts.rs:78`), the same 10 minutes as Desktop's turn cap |
| Cancellation retires the instance, because Wasmtime cannot stop one guest task without dropping its store | `engine/dispatch.rs:43` to `:46` (bytecodealliance/wasmtime#11833) |

On the Desktop side, the toolset dispatcher already declares the `tools`
export it drives in its plugin world, resolves a dispatch target per toolset,
finds the export by name and calls it with typed values (`call_concurrent`),
and sets and cancels the call's deadline. Desktop's inference plugin already
consumes a guest's `stream<T>` and `future<T>` (the `messages` stream `chat`
takes).

So it is feasible with no runtime change. What the daemon adds:

- **A run plugin** that claims `cosmonic:agent/run`, bound by a manifest entry
  (`interfaces: [run]`, `components: agent`) the way the tools dispatcher is.
- **A long-lived `GuestCall`.** The returned stream and future belong to the
  store the accessor drives, so the job pipes them inside `call` and forwards
  events over a channel. The job must not return until the outcome resolves:
  when a dispatched call's future ends, its store stops being driven, and the
  agent's background task with it. Its `deadline()` is `max-duration` plus a
  margin.
- **Stop as cooperative cancellation.** Stop drops the `StreamReader` inside the
  job, then the job awaits the outcome, so the agent records the cancellation.
  The dispatch cancel handle is the fallback when the agent does not stop in
  time. It retires the instance, which costs nothing durable: the agent's state
  is in the session.
- **One run per session in the daemon**, moved from the app's relay, so
  every path into a session shares it.
- **A control-plane route**, for example `POST /v1/workloads/{ns}/{name}/runs`
  streaming the events and `DELETE .../runs/{run-id}` to stop. Main relays it
  to the renderer the way `agentRelay.js` relays the ingress today. The control
  socket is not reachable from a web page, so the CSRF header has nothing left
  to defend. For a first cut the route can emit today's NDJSON lines through
  the adapter mapping, so the renderer's `parseAgentLine` does not change.
- **Detection.** The daemon's `/v1/inspect` already reads an image's WIT. An
  agent exporting `cosmonic:agent/run` is driven through the route; one that
  exports only `wasi:http/handler` keeps the relay. The import allowlist accepts
  the new export, alone or beside the handler.
- **Readiness** comes from the workload's state rather than `GET /history`
  polling.

### Cosmonic Control

- **HTTP.** An ingress adapter serves `POST /task` (and, if wanted, a typed
  JSON endpoint) by calling the export, as above. Authentication is the
  ingress's job, not a header the agent checks; workload identity on mTLS
  (wasmCloud#5653) is the path for service callers.
- **Events over NATS.** A host plugin subscribes on subjects the operator
  binds, and for each message calls `run`. This is the push-mode case
  wash-runtime's dispatch module was written for (`dispatch.rs:1` to `:9`),
  and the shape of Control's Kafka plugin, which drives the `cosmonic:kafka`
  handler export the same way. Per message:
  - The message becomes a `user-message`: its payload as an attachment with
    its content type (or as text), `name` from the subject.
  - `run-id` is the message's id (the JetStream sequence or a `Nats-Msg-Id`
    header), so a redelivery returns `duplicate` instead of running twice.
  - `trace-parent` comes from the message headers.
  - Events are drained, and optionally republished to a progress subject.
  - The outcome is published to the reply subject (request and reply) or a
    result subject, and then the message is acked.
  - `busy` naks with a delay, so redelivery is the backpressure. `duplicate`
    acks.
- **Which conversation an event belongs to.** The host chooses the session.
  A host-side router maps a conversation key from the event (a ticket id, a
  subject token) to its own session; see
  [Review outcomes](#review-outcomes-2026-10-16).

### A parent agent, through spawn

The parent imports `spawn` and calls `agent.run`. The implementer calls the
child's `run` export through the same dispatch path as Desktop's daemon and
narrows the result as described in
[One interface for hosts and parents](#one-interface-for-hosts-and-parents).
The spawn spike bridges to minimal-agent's HTTP today, which drops the
instructions, attachments and schema and cannot pass `max-steps`; the export
removes that bridge.

## Versioning

**It lands in 0.4.0, unstable** (decision 1), with #71's dispatch outcomes and
the tools fold.

- **One migration, not two.** Desktop and all three agents port to 0.4.0
  anyway (the fold moves the tool records to `tool-types` and makes
  `tools.list` fallible; #71 changes dispatch outcomes). Adding the export in
  the same port means one allowlist change and one round of agent releases.
- **The same Desktop work.** The 0.4.0 port is when the daemon's dispatcher
  starts calling `tools@0.4.0` exports. Calling `run` is the same mechanism.
- **It is additive.** It changes no existing interface and no existing world.
  `agent` stays imports-only; `runnable-agent` is a new world.
- **Unstable, not `@since`.** The WIT design skill keeps `@since` for what a
  release commits to and `@unstable(feature = ...)` for experimental items.
  0.4.0 has not published, so `@since(version = 0.4.0)` was possible, but the
  run surface has only been exercised by one agent and one host spike. Marked
  unstable, 0.4.0 can publish on #71's schedule whether or not `run` is
  settled: a consumer that does not enable `agent-run` never sees it, and
  promoting it later is `@since(version = 0.4.x)` with no break. The built
  package keeps the gated items, so enabling the feature (`features` in
  `wit-bindgen`, `--features agent-run` in `wasm-tools`) is all a consumer
  does.
- **0.5.0 is the spawn draft's version.** Spawn depends on `run`, not the
  other way round, so `run` lands first.

One side effect: `cosmonic:agent` gains a dependency on
`wasi:clocks/types@0.3.0` for `duration`, pinned in the package's `wkg.lock`.
The alternative was a `max-duration-ms` field, as `request-meta.deadline-ms`
does in the same package; reusing the WASI type follows the skill.

## Migration plan

Desktop serves both for a transition: the export when the agent has it, else
HTTP. Agents add the export and keep their HTTP handler as the adapter until
older Desktops age out.

### Minimal agent (first, in awesome-cosmonic)

The spike did the items marked done, on a branch that is not merged.

- [x] Export `run@0.4.0` beside the `wasi:http/handler` export. Model, chat
      and session stay at 0.3.0 until Desktop serves 0.4.0; tools are 0.4.0.
- [x] Factor the turn into `run(message, options)`: commit the person's
      message and `agent.run.v1` (commit id `run:<run-id>`), then return the
      stream and future; refuse with `busy`, `duplicate` or `invalid-input`
      before writing anything.
- [x] Stream `text-delta`, `step` (with `seq` when committed first) and
      `counts`; write `agent.run-result.v1` before resolving the outcome.
- [x] Honor `instructions`, `schema` (asked for, and checked to be JSON),
      `max-steps`, `max-total-tokens`, `max-duration`, `trace-parent`.
- [x] Cancellation: when a write to the events stream fails because the reader
      is gone, drop the chunk stream, record the op as cancelled, resolve
      `cancelled`.
- [x] Make `POST /task` the adapter over the same run, inside the agent (the
      transition form; decision 8 moves it to the host).
- [ ] Target `runnable-agent` once Desktop serves 0.4.0 inference and
      session.
- [ ] A test double that plays the host: a Wasmtime test calling the export,
      or a `run-client` component.

### Goose (sandboxed-agents)

- [ ] Port to 0.4.0; map its `Line` values onto `run-event`, `TurnError::Busy`
      onto `busy`, and Goose's own turn cap onto `max-steps`.
- [ ] Use the same helper crate for `POST /task`.

### Pi (sandboxed-agents)

- [ ] Check first that the JS toolchain can export a function returning
      `stream<run-event>` and a `future` of a variant over WASI 0.3. This is the
      riskiest step of the plan.
- [ ] Map `runAgentLoop` events: `text_delta` to `text-delta`, an assistant
      `message_end` to an `assistant` step, a `toolResult` to a `tool` step;
      `Busy` to `busy`, `Superseded` to `superseded`; wire the
      `AbortController` to the stream being dropped.
- [ ] Use the JS helper for `POST /task`.

### Desktop

The spike did the items marked done, behind a flag.

- [x] Inspect detects the `cosmonic:agent/run` export; the allowlist accepts
      it alone or with `wasi:http/handler`.
- [x] Daemon: the run plugin, the long-lived `GuestCall`, the per-session
      guard, and the `runs` route.
- [x] Main: `agents:task` uses the daemon route for agents with the export
      and the ingress otherwise.
- [x] Renderer: no change while the route emits today's lines; then accept
      typed events (reasoning, tool arguments, typed errors) when wanted.
- [ ] The host's lock also gates the ingress path, or the `POST /task`
      adapter moves into the daemon.
- [ ] Serve 0.4.0 inference and session, so agents can target
      `runnable-agent` whole.
- [ ] Readiness from workload state for agents with the export.
- [ ] Manifest: the `run` host interface entry, and the 0.4.0 `tools` entry,
      for agents that have them; the `wasi:http/handler` entry only for agents
      without the export.
- [ ] Docs: the agent protocol page gains the typed export and marks HTTP as
      the compatibility path.
- [ ] After a release or two, drop the HTTP path for Desktop's own agents.

## Spike results

A spike on a branch of the minimal agent, and of Cosmonic Desktop behind a
flag, exercised the contract end to end against a local model. Desktop's
daemon gained a run plugin that drives the export through wash-runtime's
plugin-initiated dispatch, holds the one-run lock, and serves a route on its
control socket that streams today's NDJSON lines, so the app's renderer did
not change. The app's relay uses that route when the agent exports `run` and
the ingress otherwise.

What worked as designed:

- A run with a tool call streamed text deltas and whole steps (tool call with
  its arguments, tool result) and resolved to an outcome with counts and the
  served model. The journal held `agent.run.v1` in the opening commit, the
  `run` field on every step, and `agent.run-result.v1` before the outcome.
- A repeated run id was refused as `duplicate` from the session's commit-id
  ledger: the returned journal end is not past the end the run read, which
  tells a retained commit from a fresh one without reading the journal.
- A second run while one was in progress was refused as `busy` by the host
  without reaching the agent.
- Stopping a run, by closing the route's response or by an explicit stop,
  dropped the event stream; the agent recorded the model call as cancelled,
  wrote its run result, and resolved `cancelled`. The next run was accepted
  about 25 ms later.
- Agents without the export (Goose, Pi) were refused as not runnable before
  anything ran, and their ingress path kept working.
- Importing `chat`, `models` and `session` at 0.3.0 while exporting `run` at
  0.4.0 works: the component model treats the two versions as separate
  interfaces, and the agent converts message types at the edges. The host
  links the export's function-less type imports (`run-types`, the 0.4.0
  `inference-types`, `wasi:clocks/types`) as empty instances.
- Under backpressure the host reserves room before it takes an event, so the
  agent waits on a slow reader instead of the host buffering without bound.

What the spike changed or found:

- **Cancellation is seen at the next write.** See
  [Cancellation](#cancellation). The host's forced cancel after a grace period
  covers a run stuck in a long tool call. A way for a writer to wait for its
  reader to go away, without writing, would close the gap; nothing in this
  WIT depends on it.
- **The agent's lock does not span instances**, and the host's lock must
  cover the ingress too. See [One run at a time](#one-run-at-a-time). Moving
  the `POST /task` adapter into the host (decision 8) removes the second path.
- **A run whose last commit lost the race must not record `done`.** The first
  cut fell back to recording its last entries unconditionally, run result
  included, while it resolved `superseded`. The fallback now writes a failed
  run result naming the error.
- **The commit-id ledger is bounded.** A run id older than what the session
  retains is not recognized as a duplicate. A driver that redelivers very old
  events needs its own dedupe, or the agent can scan the journal for
  `agent.run.v1`.
- **Unpublished packages are vendored.** `wkg` overrides name a package, not a
  version, so a component that imports 0.3.0 from the registry and 0.4.0 from
  a path vendors 0.4.0 until it publishes.

## Choices made, and the alternatives

- **Steps reuse `assistant-message` and `tool-result`** instead of a display
  type mirroring the `step` line. A client gets tool arguments, reasoning, and
  attachment bytes without a second schema. The cost is continuation data
  (`assistant-part.opaque`) a driver ignores; the agent may leave it out.
- **Events carry content** instead of pointing at journal entries, for the
  reasons under [The journal](#the-journal).
- **`run-id` is required and driver-chosen**, so event redelivery has a
  natural key and every run has a name in logs and the journal.
- **`run-options` is a record**, like `chat-options` and the spawn draft's
  `run-options`.
- **No `blocked` event from the agent**, since the host is the one that knows.

## Decisions

The review questions, settled. Reasons kept short.

1. **Version: 0.4.0**, unstable behind `agent-run`. One migration instead of
   two; see [Versioning](#versioning).
2. **The host chooses the session.** `run` takes no session handle; the
   session stays the agent's own import, bound by the host, one per workload
   today and one per conversation through a host router later. A key
   parameter would be forgeable, and the host attributes everything else by
   caller, not by a value the guest passes. See
   [Review outcomes](#review-outcomes-2026-10-16).
3. **`agent.run.v1` and `agent.run-result.v1` are reserved** in `session.wit`
   in the same release, with the optional `run` field on `agent.step.v1`.
   Hosts that redraw from the journal need to rely on them.
4. **`run-options` stays a record**, consistent with `chat`'s request options.
   A need it cannot carry comes as a new function or a new minor version,
   which is cheaper to reason about than a resource with accessors.
5. **Steps reuse `assistant-message`** and the existing inference types. One
   schema for messages, and the continuation data is cheap to ignore.
6. **Shared types live in `run-types`.** Spawn uses `run-outcome`,
   `run-counts`, `finish`, `reply-part` and `served-model` directly, and
   names its own `progress`, `started-run`, `start-error` and `run-error`
   only because they genuinely differ: the parent's view carries no
   child-written content, and its errors are implementer-authored.
7. **The host holds the one-run-per-session lock** (Desktop's daemon). The
   agent's own `busy` check is a backstop against two drivers on one
   session.
8. **The `POST /task` adapter lives in the host**, as a Desktop daemon route,
   so agents eventually need no HTTP handler. During the transition the
   daemon drives the typed export when the agent has it and falls back to
   HTTP otherwise.
9. **Backpressure follows `chat`.** The agent may wait on a slow event
   reader. Spawn's implementer buffers, so spawn never blocks a parent.
10. **`max-duration` counts from acceptance**, after `run` returns ok, so
    time spent reading the session before the run starts is not charged to
    it.

## Future options

- **Many conversations per agent.** Event-driven agents on Control (a
  conversation per ticket, a "virtual employee" consuming a subject) are
  served by the host-side router in
  [Review outcomes](#review-outcomes-2026-10-16), with no change to `run`.
- **A host-held session lease**, so an agent can enforce one run at a time
  across instances itself rather than relying on the driver and conditional
  commits.
- **The adapter as a composed component**, for hosts that want `POST /task`
  without implementing it.

## Review outcomes (2026-10-16)

Three adversarial reviews tested the contract before publishing: input that
arrives while a run is in progress, how runs bind to sessions, and whether a
JavaScript agent can export `run`. None of them changed the signature of
`run`. Their conclusions, with the confidence each review stated:

**No input stream on `run`** (high). Every kind of mid-run input sorts into
one of three groups, and only the third needs new WIT, later and additively.

- Approvals stay at the host's gates, tool dispatch and egress. A held
  `tools.call` waits while the host asks the person in its own words, then
  returns the result or `not-dispatched(denied)`. An agent never receives an
  approval, so it can neither forge one nor draw a permission prompt with
  text it wrote. MCP, the OpenAI Agents SDK, LangGraph and A2A all keep
  approvals out of the agent's channel or end the turn for them.
- A clarifying question is a tool the host serves on the existing `tools`
  import, the shape of Codex's `request_user_input` and Claude's
  `AskUserQuestion`. When no one can answer during the run, the agent
  finishes `input-required` and the answer starts the next run; this maps to
  A2A's `input-required` task state (medium to high).
- Steering is pulled, never pushed. Every harness that has it (Codex, Claude
  Code, Goose, Pi, Cursor) drains a queue after a step's tool calls and
  before the next model call. It comes later as a pull import,
  `run-input.take`, in an opt-in world; until then the host queues the
  message for the next run, or cancels and restarts. The `user` step is
  reserved now for those messages (medium).
- When a host cancels a run, it completes the pending calls it serves (a held
  tool call, a question) with `cancelled`, which closes most of the gap the
  spike found between a dropped stream and the agent's next write. Time
  waiting on a person counts toward `max-duration`.

**No session handle on `run`** (high on keeping it out, medium to high on the
router). A handle on `run` would give the guest many conversations while the
host still attributes model spend, tool calls, credentials and cancellation to
one caller, so one prompt-injected ticket could reach another's context. The
platforms that sell isolation (Cloudflare Agents, AWS Bedrock AgentCore,
Temporal) instead give each conversation its own unit, addressed by key. The
host does the same with a router: it maps a tenant-scoped conversation key to
its own session, creates the session on demand, runs one run at a time per
key, stops idle instances, and resumes from the journal. That is additive
later. What had to change now is wording: the host chooses the session (not
"one workload is one session"), an agent must not rely on in-memory state
surviving between runs, and `run-id` is unique per session.

**Pi exports `run` natively** (high). A probe built the real 0.4.0 WIT with
componentize-qjs, the toolchain Pi already uses, and drove it under the
Wasmtime version Desktop runs: streaming events, the outcome future,
`duplicate`, `invalid-input`, `busy`, and cancellation (a write resolves to 0
once the driver drops the stream) all behaved as the WIT says. StarlingMonkey
cannot yet: it does not implement async function exports
([ComponentizeJS#335](https://github.com/bytecodealliance/ComponentizeJS/issues/335)).
Two findings for hosts and agents: a JS agent imports all of
`inference-types`, including the `model` resource, so Desktop defines that
resource when it links the 0.4.0 types for a run-only agent; and a JS
exception from `run` that is not a WIT value traps the instance, so an agent
maps unknown failures to `other` or `unavailable`. The review suggested
naming the stream and future types (`type run-events = stream<run-event>`) for
readable JS constants. That was tried and reverted: Wasmtime 48's bindgen,
which Desktop's host uses, does not compile a type alias of a stream or a
future, so the names would break Rust hosts for a cosmetic gain.

**Added now, before publishing:** `finish.input-required`, the reserved
`step-content.user`, and the wording on sessions, approvals, waits and
cancellation. Desktop's spike defines the 0.4.0 `model` resource when it links
a run agent's type imports, and a JS agent built with componentize-qjs
against this WIT now instantiates and runs there.

**Additive later:**

- Router configuration on the trigger binding (key extraction, idle stop,
  per-key queue, retention) and a conversations route (`POST
  /conversations/{key}/runs`).
- `run-input`, a pull import for steering, in its own opt-in world.
- A typed `ask` interface, if a host wants one beyond the ask tool.
- Desktop serving inference and session at 0.4.0, so agents target
  `runnable-agent` whole.

**Question for Bailey.** Does adding a case to a variant after publishing
break consumers already compiled against it? We believe it does, because the
component model checks types structurally and has no variant subtyping. One
direction is now tested: Desktop's host, built with the new `step-content`
(five cases), refused to call the `run` export of a JS agent built before the
`user` case was added, with "expected variant of 5 cases, found 4 cases". The
other direction (an old host, a new agent) is untested. That is why both new
cases are added now rather than later.
