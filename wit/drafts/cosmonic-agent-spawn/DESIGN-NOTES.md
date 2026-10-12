# `cosmonic:agent/spawn` (draft)

A scaffold for the design jam on 2026-10-13. Everything here is up for argument.
The WIT is in [`spawn.wit`](spawn.wit) and [`world.wit`](world.wit). It declares
`cosmonic:agent@0.4.0-draft` and uses the published `cosmonic:agent@0.3.0` types
through `wkg.lock`, so the published package is unchanged. CI does not build
`wit/drafts/`. To check it locally:

```sh
WKG_CONFIG_FILE=$PWD/wit/wkg-registries.toml \
  node .github/scripts/wit/build.mjs wit/drafts/cosmonic-agent-spawn agent
```

(`build.mjs` needs `WKG_CONFIG_FILE` for its `wash wit build` step too, because
no default registry knows the `cosmonic` namespace. That gap will bite the first
published package that depends on `cosmonic:agent`.)

## What this replaces, and why it is agent-scoped

We had planned a general `wasmcloud:host` scheduling interface that lets a
workload define and start other workloads. `spawn` is the narrower replacement:
a caller describes an agent, its tools, and their grants, and gets back a
session it owns. Victor's point from standup is the reason. A guest that can
start any workload can ask for anything a workload can do, including plugins,
so the blast radius (everything a mistake or an attack can reach) is the whole
workload API. A guest that can only spawn an agent with a declared tool list
can only ask for that, and the request is semantic enough to write policy
against. How the host fulfils a spawn stays out of the contract: a wasmCloud
workload today, possibly a remote agent later.

This is not session durability. That work iterates on the existing
`cosmonic:agent/session` interface and is separate.

## Decisions encoded in the draft

- **The resource is a `session`.** Bailey's rule is that a resource defines a
  lifetime and a proper noun. A spawned thing has one agent, one model binding,
  one tool set, one context, and one lifetime, and the spawner owns it and keeps
  talking to it. That is a session. It is a new handle inside `spawn` rather
  than a reuse of the `session` interface, because that interface has no handle:
  it is the calling workload's own journal, exposed as free functions. The child
  still writes its journal there. The handle reuses its `entry` type, so
  `follow` streams the child's journal in the format agents already write
  (`agent.step.v1` and friends).
- **Declarative.** The tool set is fixed at spawn. Adding tools later is a
  follow-up.
- **Every component is pinned by digest.** An approval cannot be swapped out by
  moving a tag.
- **Grants are per component.** The agent gets top-level `allowed-hosts`. Each
  tool carries its own allowed hosts, volume mounts (with read-only),
  interfaces, and secret names. Empty means nothing; nothing is inherited.
- **Names, never secrets.** `model` is a host-level inference binding by name,
  the same string `models.open` takes. Secrets are names the host injects into
  the component that needs them.
- **The caller is implicit.** The plugin receives the caller's workload
  definition from the host, not from a parameter, so a guest cannot forge it.
- **Attenuation.** The caller may request more than it holds but cannot grant
  it. The host is the sole grantor, by policy or a person. Over-ceiling requests
  return `needs-approval` carrying an approval bound to the request's digest;
  the caller resends the identical request once approved.
- **Non-exhaustive everywhere.** Every error and status variant ends in
  `other(string)`. `status` is a variant rather than an enum for that reason.
- **`cancel` is not `drop`.** `cancel` stops the current turn and leaves the
  session open. Dropping the handle ends the session.
- **`spawn` resolves at acceptance**, like `chat`, not when components are up.

## Usage sketch

Victor's shape, `a = new agent(); a.addTool(...); a.prompt(...); a.spawn()`,
is a builder in the SDK over one declarative record. Pseudo-Rust:

```rust
let mut a = Agent::new(ComponentRef::pinned("ghcr.io/acme/agents/coder:1.2.0", "sha256:9f…"))
    .model("bedrock-sonnet");                       // binding name, no key
a.add_tool("repo-fs", ComponentRef::pinned("ghcr.io/cosmonic/tools/fs:0.2.0", "sha256:1c…"),
    Grants { volumes: vec![mount("workspace", "/src", true)], ..Grants::none() });
a.add_tool("github", ComponentRef::pinned("ghcr.io/cosmonic/tools/github:0.1.0", "sha256:7a…"),
    Grants { allowed_hosts: vec!["api.github.com".into()], secrets: vec!["github-token".into()],
             ..Grants::none() });
a.prompt(Prompt::Text("Review the open PR and summarize risks.".into()));

// a.spawn() is spawn::spawn(a.into_request())
let session = match a.spawn().await {
    Ok(s) => s,
    Err(SpawnError::NeedsApproval(ap)) => { wait_for_human(&ap.id).await; a.spawn().await? }
    Err(e) => return Err(e.into()),
};
session.send(user("Start with the auth changes.")).await?;
let mut journal = session.follow(0).await?;
while let Some(entry) = journal.next().await { render(entry) }
drop(session);                                       // ends the child
```

## Open questions for the jam

Each has options and a lean. The leans are mine, to be argued with.

**(a) One workload for agent and tools, or one per tool?** Bailey's diagram
and today's Desktop session put the prompt, agent, tools, and volume mount in
one workload. On 10/10 we chose a workload per tool, so each tool gets its own
grants and egress identity, and to avoid the concurrent linked-call race in
wash-runtime (the `active_ctx` TODO, where two calls into the same store at once
can see the wrong context). *Lean:* the WIT does not care, because grants are
already per tool and `allowedHosts` is per component even inside one workload.
Ship a workload per tool until the race is fixed, then let the plugin choose.

**(b) Native plugin or Wasm host component plugin?** Bailey prefers native for
first-party code because session handles are faster. Our 10/10 call was a Wasm
host component plugin, and Desktop does not load those yet. *Lean:* native for
the spawn plugin on both Desktop and Control, since it needs the host's workload
API and the caller's identity anyway. Keep Wasm host plugins for the part
customers swap out, the policy checks.

**(c) Where do the policy tiers hook, and what does an auditor see?** Tier 1
is semantic, inside the plugin at spawn (never allow this tool, never this
host). Tier 2 is workload admission (OPA or Kyverno on Control, a component
allow-list plugin on Desktop). Tier 3 is an agentic auditor, a separate workload
with its own prompt that shares no goal with the spawner. *Lean:* keep the
auditor out of `spawn`. Define a sibling interface the auditor exports, roughly
`review(caller-definition, request, request-digest) -> allow | deny(reason) |
needs-approval`, and a matching audit event for every decision. Only workloads
declared privileged to scheduling may export it. Replayed, already audited
workloads skip tier 3.

**(d) Session only, or a separate agent resource?** Victor asked whether one
agent can hold several sessions, for example comparing two models. *Lean:*
session only. Comparing two models is two spawns. It is a two-way door; a
separate `agent` resource can come later as a new interface.

**(e) How does this relate to `tools` and the `tool-workload` sibling?** The
draft says a tool component exports `cosmonic:agent/tools`, and the child
imports `tools` as agents do today, with the host bridging. *Lean:* accept
`tool-workload` exports too while it is a sibling, so the later switch is a
rename. Ask whether Bailey wants to iterate `tools` now, since she said she is
split on parts of it. Dynamic loading would later be a `session` method such as
`add-tool`, going through the same approval path.

**(f) How do spawned sessions get inference credentials?** Options: the child
may use only the caller's own binding; any binding the host allows the caller;
or any binding, with ones the caller lacks going to approval. *Lean:* treat a
binding name like any other grant. Within the caller's set passes policy;
outside it returns `needs-approval`. Usage is attributed to the parent session
through labels.

**(g) Does the child die with the parent's handle?** Options: yes, structured
like a scoped task (the draft); a `detach` that lets a child outlive the handle;
or host-owned sessions the parent reattaches to by id. *Lean:* dies with the
handle for v0. Deleting the parent's workload ends its children. Grandchildren
are allowed only if the child's own grants include `spawn`, with a depth limit
reported as `over-limit`.

**Also worth a minute:**

- **Can the parent read what the child read?** `follow` gives the parent the
  child's journal, including tool output. If a child tool reads a volume the
  parent cannot, that data reaches the parent. Options: the parent sees only
  the child's final replies, or the host redacts tool output the parent's
  grants would not allow.
- **"A few other things" in the request:** agent config, limits (turns,
  deadline, tokens), and whether a prompt component is worth keeping.
- **`blocked` status:** is it enough for the UX when a child waits on a person?

## Naming

Bailey's rule: "broker" and "gateway" mean host-wide (a credential broker is
one). Workload-tenanted things are not brokers. So this is the spawn plugin, its
caller is the spawner, and the reviewing workload is the auditor.
