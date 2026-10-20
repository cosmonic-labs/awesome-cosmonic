# Migrating to cosmonic:agent 0.4.0

0.4.0 is a breaking release. It adds typed chat interruption and tool dispatch
outcomes, moves the tool records into `tool-types`, reshapes tool input and
arguments, carries citations and refusals, and adds the unstable `run` and
`compact` exports behind `agent-run`. Update imports from
`cosmonic:agent@0.3.0` to `cosmonic:agent@0.4.0`. The published 0.3.0 artifact
remains unchanged.

- Rename `inference-types.capabilities` and `model-description.capabilities` to
  `supported-operations`. The flags remain `chat` and `embeddings`.
- Handle `cancelled` and `timed-out` in both chat error types. Before acceptance,
  errors return from `chat`; afterward, they resolve the reply's outcome future.
  Dropping reply chunks requests cooperative cancellation. A terminal outcome
  already recorded wins the race. Abandoning a pending call may return no result.
- Match tool errors as `not-dispatched(dispatch-error)` or
  `outcome-unknown(outcome-error)`. The first confirms that this attempt produced
  no execution or effects; the second includes uncertain dispatch and lost replies.
  Both preserve typed cancellation, timeout, upstream failure, and diagnostic detail.
- Handle `tools.list` failing with `list-error` (`unavailable`, `denied`,
  `other`) instead of reading an empty list as "no tools". The same `tools`
  interface is imported by agents and exported by tool providers.
- Import tool records from the function-less `tool-types` interface, where
  `tool-def`, `tool-response` and the error types now live. A tool provider
  no longer imports `tools` just for its types.
- Keep completed execution failures in `tool-response.is-error`, with their output
  available to the model. Cancellation and timeout do not imply rollback.
- Optionally export `run` (unstable, `agent-run`) and target `runnable-agent`.
  A run writes `agent.run.v1` and `agent.run-result.v1`, now reserved in
  `session`, and streams `run-event.compacted` after it commits an
  `agent.compaction.v1` entry. The summary stays in the journal and is
  untrusted model output. `compaction-info.trigger` and the entry's `trigger`
  say why the agent summarized: `threshold`, `overflow` or `requested`.
- Optionally also export `compact` (unstable, `agent-run`) and target
  `compactable-agent`, so a driver can ask for a summary between runs with an
  optional `focus` from the person. It accepts by committing `agent.compact.v1`
  under commit id `compact:<run-id>` before its model call, and ends every
  accepted compaction with at most one `agent.compact-result.v1` (none means
  it was cut off by a crash, which a driver treats as failed); a summary's
  `agent.compaction.v1` (trigger `requested`) shares that final commit with the
  replacement state. A refused or content-filtered summary is never committed:
  no `compacted` event, the state unchanged, and the compaction ends failed.
  Compaction ids are a namespace apart from run ids, and a driver that gets
  `duplicate` reads the outcome from the journal.
- Match the new `reply-part` cases `cited-text` (text with its citations) and
  `refusal`, and the new `finish` cases `refusal` and `content-filter`. A
  driver that reads only `run-outcome` keeps citations and can tell a refusal
  from an answer. Neither finish carries `structured`, and neither is
  `output-rejected`. Both describe the call that produced the run's last
  assistant step, so a refused summary never makes a run finish `refusal`;
  they take precedence over a limit reached by that same call and over
  `input-required`.
- Treat tool output as possibly truncated. A host may cap a tool's output and
  append a short text note stating the original size.
- Match `other(string)` in every variant; `run-types` no longer uses
  `other(option<string>)`. Every stable interface, world and type carries
  `@since` with the version that introduced its current shape: 0.3.0 for what
  0.3.0 had unchanged, 0.4.0 for `inference-types` and `tools`, which changed
  incompatibly, and for `tool-types` and the types new in this release.
- Record an unknown tool outcome or abandoned call as an unresolved operation.
  Retry with the same id and arguments only when trusted configuration authorizes
  repeat detection. Provider annotations cannot authorize replay. A not-dispatched
  result describes this attempt and does not erase effects from earlier attempts.
- Match `attachment-source.stored`: an opaque, session-sealed id for content
  the host stores. Pass it through unchanged and never persist the bytes behind
  it.
- Set or ignore `attachment.display-name`, an optional filename shown to
  people. It is untrusted text, never a path.
- Match the new `finish-reason` cases `refusal`, `content-filter` and
  `pause-turn`. On `pause-turn`, append the message and call again.
- Match `assistant-content.refusal`, the model's reason for declining. Show it
  as the model's text; it never satisfies a response format.
- Set `chat-options.parallel-tool-calls` (none keeps the provider default) and
  match `request-option.parallel-tool-calls`.
- Set `request-meta.cache-points` (empty for none) to mark prompt-cache
  breakpoints by `part-ref`.
- Set `assistant-part.citations` (empty for none) and render any citations as
  untrusted links on the text they support.
- Set `run-options.effort` (none leaves the agent's choice) and pass it on as
  `chat-options.effort`.
- Match `run-event.plan`, the agent's whole current plan for display; each
  event replaces the last. An agent commits `agent.plan.v1` (reserved in
  `session`) with the same entries before streaming it, so a driver that
  reloads shows the latest plan from the journal.
- Describe each `tool-def`'s input with `input: tool-input` in
  `inference-types` and `tool-types`: `json-schema(schema)` keeps today's
  behavior; `grammar(grammar)` takes text constrained by a `grammar` (`lark` or
  `regex`), as OpenAI custom tools such as Codex's `apply_patch` do; and
  `text` takes plain, unconstrained text. A call's `arguments` are a
  `tool-arguments` variant: `json(object)` for a JSON Schema tool, `text(raw)`
  for a `grammar` or `text` one, never JSON parsed. Providers without native
  support for text tools refuse or wrap the tool in a one-string JSON schema
  and report `option-ignored(freeform-tools)`. The grammar steers the model and
  is not validation, so tools validate their input. Match
  `request-option.freeform-tools`.
