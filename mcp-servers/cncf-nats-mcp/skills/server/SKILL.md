---
name: nats-mcp-server-v1
description: Operate and diagnose a NATS deployment through this server — find out why a stream is growing, a consumer is behind, or messages are being redelivered; read and publish on subjects, streams and KV buckets; and interpret `denied` grant refusals. Use when connected to this server and deciding which tool to call, or when a NATS call failed and the reason is not obvious.
---

# Using the nats-mcp-server-v1 MCP server

This server reaches NATS through a **host capability binding**, not over the
network. It opens no sockets and holds no credentials. Everything it may touch
is fixed by grants in its Workload manifest, which it cannot widen.

It is **stateless**: every request is self-contained and there is no session.

## Start with a diagnosis, not a query

If the user's problem is "something is wrong with NATS" and you do not yet know
what, call **`nats_diagnose`** first. It samples twice, applies rules, and
returns findings that each carry a `severity`, the `evidence` behind the
verdict, and a `remedy`. It is far better than reading counters yourself,
because most NATS faults are only visible as a *change*:

- 9,000 messages in a stream is meaningless. 9,000 and climbing at 900/min
  with no consumer is a disk filling up.
- `num_pending: 200` is meaningless. 200 and rising is a consumer that will
  never catch up.

Every rate in this server is measured across a real sampling window
(`sample_ms`, default 2s, max 15s). A single reading cannot tell those apart,
so do not try.

**Read the `capability` block on the report before you trust a clean result.**
It tells you which rules could run. A report with `retention_rules: false` did
not check whether streams are bounded — it says so, and says what would enable
it. `healthy: true` at a low capability level means "nothing I could check was
wrong", not "nothing is wrong".

## What this server can see, and how to widen it

Capability is progressive. The binding has **no list call**, so the server
cannot enumerate what exists unless it is told or shown:

| `capability.level` | How it knows | What runs |
|---|---|---|
| `caller-supplied` | names you pass in | consumer, backlog and pruning rules on what you named |
| `hinted` | `MCP_NATS_STREAMS` / `_CONSUMERS` / `_BUCKETS` on the workload | the same, with zero-argument calls working |
| `monitoring` | the NATS monitoring port | every stream on the server, plus retention and server-wide rules |

If a diagnosis comes back thin, the fix is in `capability.unlocks`. Report it
to the user rather than guessing at what you cannot see — the missing piece is
an operator change, not something to retry.

At `caller-supplied` and `hinted` levels, retention configuration is
unreadable, so `unbounded-stream` cannot be checked. In its place you get
`stream-no-pruning-observed`, an **inference** from the fact that
`first_sequence` has never advanced. Treat it as a prompt to confirm with
`nats stream info <name>`, not as proof.

## Reading a stream without breaking it

This is the one way to cause real damage here, so be deliberate:

- **`jetstream_scan`** — browse a stream. Creates no consumer, acknowledges
  nothing, changes nothing. **This is the default choice.**
- **`jetstream_get_message`** — one message by sequence. Also harmless.
- **`jetstream_fetch`** — drives a *real* pull consumer. `settle: "ack"`
  consumes messages permanently. Even the default `settle: "none"` stalls the
  consumer for its full `ack_wait` before those messages come back.

Use `fetch` only when the task is genuinely to consume, or to reproduce what a
worker sees. To look at contents, use `scan`.

## When a call is refused

Failures carry a stable `code`. Branch on that, not on the message text.

`denied` is the one that most often looks like a bug and is not. Grants are
deny-by-default and set by whoever deployed the workload; the message names
which grant key would have to change (`subject-allow`, `stream-allow`,
`bucket-allow`). **Retrying never helps.** Two flavours matter:

- *not granted* — an operator can widen it. Say which key, and stop.
- *reserved* — `$SYS`, the JetStream API, the KV key space. No grant can ever
  open these. Do not suggest widening anything; the server-wide tools exist
  precisely because this door is shut.

Call **`nats_check_access`** to find out what is reachable before assuming a
denial is a misconfiguration. It probes each stream and bucket and returns a
granted/denied map.

Other codes worth knowing: `no-responders` (nothing is listening — unlike
`timeout`, retrying fails identically), `revision-mismatch` (a CAS conflict
that carries the current revision, so retry without re-reading),
`max-payload-exceeded` (the server's limit, headers included), `no-messages`
(the fetch ran fine and there was nothing to give — an empty result, not a
failure).

## Writing safely

- `nats_publish` resolves when the message is written to the connection. There
  is no delivery confirmation. If you need one, use `jetstream_publish`, which
  returns the stream and sequence.
- Use `msg_id` on `jetstream_publish` for idempotency — a repeat inside the
  stream's duplicate window is acked as `duplicate` and stored once.
- Use `kv_update` (compare-and-swap), not `kv_put`, whenever the new value
  depends on the old one. `kv_put` is last-write-wins and will silently clobber
  a concurrent writer.
- `kv_purge` destroys a key's history irreversibly. `kv_delete` leaves a
  tombstone and keeps it.

Subjects you publish to must be **literal**. `*` and `>` are subscription
patterns; a wildcard publish is refused.

## Reporting back

Findings are already written for a human — pass the `summary` and `remedy`
through rather than re-deriving them, and keep the `evidence` numbers when the
user needs to judge severity. Say plainly which capability level produced the
result.

Full per-tool detail: [Tools](references/TOOLS.md).
