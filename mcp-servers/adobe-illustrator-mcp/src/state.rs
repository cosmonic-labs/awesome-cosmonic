//! Bridge state: the command queue that connects an MCP tool call to the
//! Illustrator bridge.
//!
//! A tool call and the panel poll that services it are two *separate* HTTP
//! requests, and the host is free to route them to different instances of
//! this component. So the queue lives in a host `wasi:keyvalue` bucket rather
//! than in linear memory — the same reason the rest of the component keeps no
//! per-session state.
//!
//! The shape is deliberately tiny: one pending command, one latest result, a
//! sequence counter, and two poll timestamps. There is exactly one After
//! Effects instance on the other end, so there is nothing to shard.

use serde_json::{json, Value};

/// `wasi:keyvalue` bindings. The `wasip3` crate generates the `wasi:http`
/// service world for us; this adds the one import it does not cover. See
/// `wit/world.wit`.
mod bindings {
    wit_bindgen::generate!({
        world: "bridge-state",
        path: "wit",
        generate_all,
    });
}

use bindings::wasi::keyvalue::store::{self, Bucket};

/// Bucket identifier. Cosmonic Desktop maps this to its in-memory backend;
/// override with `MCP_BRIDGE_BUCKET` for a host that names buckets otherwise.
fn bucket_name() -> String {
    std::env::var("MCP_BRIDGE_BUCKET").unwrap_or_else(|_| "in_memory".to_owned())
}

/// Namespace for this server's keys.
///
/// Cosmonic Desktop's default bucket is host-wide, so a second Illustrator
/// bridge deployed alongside this one would otherwise read and overwrite the
/// same command queue — each stealing commands meant for the other's panel.
/// Override with `MCP_BRIDGE_KEY_PREFIX` when running more than one.
fn key(suffix: &str) -> String {
    let prefix =
        std::env::var("MCP_BRIDGE_KEY_PREFIX").unwrap_or_else(|_| "illustrator-mcp".to_owned());
    format!("{prefix}:{suffix}")
}

const KEY_COMMAND: &str = "command";
const KEY_RESULT: &str = "result";
const KEY_SEQ: &str = "seq";
const KEY_LAST_POLL: &str = "last-poll-ms";
const KEY_LAST_STALE_POLL: &str = "last-stale-poll-ms";
const KEY_CLIENT: &str = "client";

/// How often [`wait_for_result`] re-reads the store while waiting.
const POLL_INTERVAL_MS: u64 = 200;

fn bucket() -> Result<Bucket, String> {
    store::open(&bucket_name()).map_err(|err| {
        format!(
            "failed to open keyvalue bucket {:?}: {err:?} — is the wasi:keyvalue \
             hostInterface declared in the Workload manifest?",
            bucket_name()
        )
    })
}

/// Wall-clock milliseconds since the Unix epoch.
///
/// Every timestamp in the store is wall-clock, not monotonic: the two sides of
/// a comparison are usually written by *different* component instances, and
/// each instance has its own monotonic epoch.
pub fn wall_ms() -> u64 {
    let now = wasip3::clocks::system_clock::now();
    // Pre-epoch clocks are not a thing we need to represent; clamp rather
    // than let a negative second count wrap the unsigned arithmetic below.
    let seconds = u64::try_from(now.seconds).unwrap_or(0);
    seconds
        .saturating_mul(1000)
        .saturating_add(u64::from(now.nanoseconds) / 1_000_000)
}

/// Monotonic milliseconds. Only comparable within a single invocation.
fn mono_ms() -> u64 {
    wasip3::clocks::monotonic_clock::now() / 1_000_000
}

fn get_json(suffix: &str) -> Result<Option<Value>, String> {
    let key = key(suffix);
    let raw = bucket()?
        .get(&key)
        .map_err(|err| format!("keyvalue get({key}) failed: {err:?}"))?;
    Ok(raw.and_then(|bytes| serde_json::from_slice(&bytes).ok()))
}

fn set_json(suffix: &str, value: &Value) -> Result<(), String> {
    let key = key(suffix);
    let bytes = serde_json::to_vec(value).map_err(|err| format!("serialize {key}: {err}"))?;
    bucket()?
        .set(&key, &bytes)
        .map_err(|err| format!("keyvalue set({key}) failed: {err:?}"))
}

fn next_seq() -> Result<u64, String> {
    let next = get_json(KEY_SEQ)?
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .saturating_add(1);
    set_json(KEY_SEQ, &json!(next))?;
    Ok(next)
}

/// Queues a command for the panel. Returns the command id.
pub fn queue_command(command: &str, args: &Value) -> Result<u64, String> {
    let id = next_seq()?;
    set_json(
        KEY_COMMAND,
        &json!({
            "id": id,
            "command": command,
            "args": args,
            "status": "pending",
            "queuedAtMs": wall_ms(),
        }),
    )?;
    Ok(id)
}

/// Registers a polling panel and reports whether it is the current one.
///
/// The highest client id seen wins, so reloading the panel supersedes the
/// previous instance rather than both racing to claim commands. A client id of
/// 0 (a panel that doesn't identify itself) never supersedes a real one.
pub fn register_client(client_id: u64) -> Result<bool, String> {
    let current = get_json(KEY_CLIENT)?.and_then(|v| v.as_u64()).unwrap_or(0);
    if client_id > current {
        set_json(KEY_CLIENT, &json!(client_id))?;
        return Ok(true);
    }
    Ok(client_id == current)
}

/// Records a poll from the bridge panel.
///
/// `served` distinguishes a panel we can dispatch commands to from one we
/// refuse (outdated protocol version, or superseded by a newer instance).
/// Stale polls get their own key so they never make the bridge look connected,
/// while still letting `bridge_status` tell "a stale panel is polling" apart
/// from "no panel is running" — which otherwise look identical, and have
/// opposite fixes.
pub fn record_poll(served: bool) -> Result<(), String> {
    let key = if served {
        KEY_LAST_POLL
    } else {
        KEY_LAST_STALE_POLL
    };
    set_json(key, &json!(wall_ms()))
}

/// Hands the pending command to the panel, marking it dispatched.
pub fn take_pending_command() -> Result<Option<Value>, String> {
    let Some(mut command) = get_json(KEY_COMMAND)? else {
        return Ok(None);
    };
    if command.get("status").and_then(Value::as_str) != Some("pending") {
        return Ok(None);
    }
    if let Some(object) = command.as_object_mut() {
        object.insert("status".into(), json!("dispatched"));
    }
    set_json(KEY_COMMAND, &command)?;
    Ok(Some(command))
}

/// Stores a result reported by the panel for command `id`, stamped so waiting
/// tool calls can match it to the command they queued.
pub fn store_result(id: Option<u64>, body: &[u8]) -> Result<(), String> {
    let mut result: Value = serde_json::from_slice(body)
        .unwrap_or_else(|_| json!({ "raw": String::from_utf8_lossy(body) }));
    if let Some(object) = result.as_object_mut() {
        if let Some(id) = id {
            object.insert("_commandId".into(), json!(id));
        }
        object.insert("_receivedAtMs".into(), json!(wall_ms()));
    }
    set_json(KEY_RESULT, &result)?;

    if let Some(mut command) = get_json(KEY_COMMAND)? {
        let matches = id.is_none() || command.get("id").and_then(Value::as_u64) == id;
        if matches {
            if let Some(object) = command.as_object_mut() {
                object.insert("status".into(), json!("completed"));
            }
            set_json(KEY_COMMAND, &command)?;
        }
    }
    Ok(())
}

/// The most recent result, whatever command produced it.
pub fn latest_result() -> Result<Option<Value>, String> {
    get_json(KEY_RESULT)
}

/// Waits up to `timeout_ms` for the result of command `id`.
///
/// Called from tokio-world tool code, so the wait goes through
/// [`crate::bridge::sleep`] rather than `tokio::time` — see that function for
/// why. The deadline is monotonic and local to this invocation, which is
/// correct here because both reads happen on the same instance.
pub async fn wait_for_result(id: u64, timeout_ms: u64) -> Result<Option<Value>, String> {
    let deadline = mono_ms().saturating_add(timeout_ms);
    loop {
        if let Some(result) = get_json(KEY_RESULT)? {
            if result.get("_commandId").and_then(Value::as_u64) == Some(id) {
                return Ok(Some(result));
            }
        }
        if mono_ms() >= deadline {
            return Ok(None);
        }
        crate::bridge::sleep(POLL_INTERVAL_MS).await;
    }
}

/// Milliseconds since a serviceable panel last polled, if one ever has.
pub fn last_poll_age_ms() -> Result<Option<u64>, String> {
    poll_age(KEY_LAST_POLL)
}

/// Milliseconds since a panel we refused to serve last polled, if one ever has.
pub fn last_stale_poll_age_ms() -> Result<Option<u64>, String> {
    poll_age(KEY_LAST_STALE_POLL)
}

fn poll_age(key: &str) -> Result<Option<u64>, String> {
    Ok(get_json(key)?
        .and_then(|v| v.as_u64())
        .map(|then| wall_ms().saturating_sub(then)))
}

/// The currently queued command, if any, in whatever state it is in.
pub fn current_command() -> Result<Option<Value>, String> {
    get_json(KEY_COMMAND)
}
