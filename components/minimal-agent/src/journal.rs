//! The session journal: the entries the session protocol defines, this
//! turn's conditional commits, recovery after a restart, and `/history`.

use crate::bindings::cosmonic::agent::session::{
    self, CommitOptions, CommitResult, Entry, JournalEnd, NewEntry, Snapshot, StateCondition,
};
use crate::conversation::Conversation;
use crate::describe::Describe;
use crate::wire::{respond, store_failed, to_json};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use wasip3::http::types::Response;

/// The journal kind a UI draws: `{role, text, blocks, tools}`.
pub(crate) const STEP_KIND: &str = "agent.step.v1";
/// A model or tool call, written before it is made.
pub(crate) const OP_KIND: &str = "agent.op.v1";
/// How that call ended, written after it returns.
pub(crate) const OP_RESULT_KIND: &str = "agent.op-result.v1";

/// An `agent.step.v1` entry and the `step` line: the shape a UI draws.
#[derive(Serialize)]
pub(crate) struct Step<'a> {
    /// `user`, `assistant`, `tool` or `error`.
    pub(crate) role: &'a str,
    pub(crate) text: &'a str,
    /// The kinds of content: `text`, `toolRequest`, `toolResponse`, `error`.
    pub(crate) blocks: &'a [&'a str],
    /// The tools an assistant step asks for, or a tool step answers.
    pub(crate) tools: &'a [&'a str],
}

/// `agent.op.v1`: a model or tool call is about to be made. A tool call's op
/// is its call id.
#[derive(Serialize)]
pub(crate) struct Op<'a> {
    pub(crate) op: &'a str,
    /// `model` or `tool`.
    pub(crate) target: &'a str,
    pub(crate) name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) arguments: Option<Box<RawValue>>,
}

/// `agent.op-result.v1`: how that call ended.
#[derive(Serialize)]
pub(crate) struct OpResult<'a> {
    pub(crate) op: &'a str,
    /// `succeeded`, `failed`, `cancelled` or `unresolved`.
    pub(crate) outcome: &'a str,
}

/// An op or op-result entry read back from the journal.
#[derive(Deserialize)]
struct OpRecord {
    op: String,
    #[serde(default)]
    target: String,
    #[serde(default)]
    name: String,
}

/// `{"steps": [...]}`: every step in the session journal, oldest first.
pub(crate) async fn history() -> Response {
    let entries = match read_journal(0).await {
        Ok(entries) => entries,
        Err(e) => return store_failed(&e),
    };
    // Each step is already JSON, so it is copied into the array as stored.
    let mut steps = b"{\"steps\":[".to_vec();
    for entry in entries {
        // Skip anything that is not well-formed JSON rather than break the array.
        let valid = str::from_utf8(&entry.data).is_ok()
            && serde_json::from_slice::<IgnoredAny>(&entry.data).is_ok();
        if entry.kind == STEP_KIND && valid {
            if !steps.ends_with(b"[") {
                steps.push(b',');
            }
            steps.extend_from_slice(&entry.data);
        }
    }
    steps.extend_from_slice(b"]}");
    respond(200, "application/json", steps)
}

/// Every journal entry from `start` on, oldest first.
async fn read_journal(mut start: u64) -> Result<Vec<Entry>, session::Error> {
    let mut entries = Vec::new();
    loop {
        // A short page does not mean the end; an empty one does.
        let page = session::read(start, 1000).await?;
        let Some(last) = page.last() else {
            return Ok(entries);
        };
        start = last.seq.saturating_add(1);
        entries.extend(page);
    }
}

/// Close the calls a turn cut off by a restart left open, as the session
/// protocol asks. Asking a model again is safe, so an open model call is
/// closed as cancelled. Whether an open tool call took effect is unknown, so
/// it is closed as unresolved, with an error step for the person; it is never
/// made again, under its own id or a new one.
///
/// Desktop sends one task at a time. If two turns do overlap, the later one
/// closes the earlier one's open calls here, and the earlier one stops at its
/// next conditional write.
pub(crate) async fn recover(snapshot: &Snapshot) -> Result<Vec<NewEntry>, session::Error> {
    // The stored state covers everything up to `through`.
    let start = match snapshot.state.as_ref().map(|s| s.through) {
        Some(JournalEnd::At(seq)) => seq.saturating_add(1),
        _ => 0,
    };
    let mut open: Vec<OpRecord> = Vec::new();
    for entry in read_journal(start).await? {
        let Ok(record) = serde_json::from_slice::<OpRecord>(&entry.data) else {
            continue;
        };
        match entry.kind.as_str() {
            OP_KIND => open.push(record),
            OP_RESULT_KIND => open.retain(|o| o.op != record.op),
            _ => {}
        }
    }
    let mut entries = Vec::new();
    for call in open {
        let model = call.target == "model";
        let outcome = if model { "cancelled" } else { "unresolved" };
        entries.push(entry(
            OP_RESULT_KIND,
            &OpResult {
                op: &call.op,
                outcome,
            },
        ));
        if !model {
            let text = format!(
                "A restart interrupted the {} tool call {}. Whether it took effect is \
                 unknown, and it was not made again.",
                call.name, call.op
            );
            entries.push(entry(STEP_KIND, &step("error", &text)));
        }
    }
    Ok(entries)
}

/// This turn's writes to the session journal. Each is conditioned on the
/// journal end the last one left, so nothing is dispatched over another
/// writer's entries.
pub(crate) struct Journal {
    end: JournalEnd,
    /// Another writer moved the journal on, so no further call is made.
    stale: bool,
}

impl Journal {
    /// A journal whose first write is conditioned on `end`.
    pub(crate) fn new(end: JournalEnd) -> Self {
        Self { end, stale: false }
    }

    /// Append entries that must come before a call: only if nothing else has
    /// written the session since this turn's last write.
    pub(crate) async fn append(&mut self, entries: Vec<NewEntry>) -> Result<(), String> {
        if !self.stale
            && let Ok(CommitResult::Committed(c)) =
                commit(entries, None, None, Some(self.end)).await
        {
            self.end = c.end;
            return Ok(());
        }
        self.stale = true;
        Err("another request is writing this session; retry".into())
    }

    /// Record what already happened. A call that was made must be recorded
    /// even if another writer moved the journal on, without conditions if
    /// need be.
    pub(crate) async fn record(&mut self, entries: Vec<NewEntry>) {
        let require = (!self.stale).then_some(self.end);
        match commit(entries.clone(), None, None, require).await {
            Ok(CommitResult::Committed(c)) => self.end = c.end,
            Ok(CommitResult::Stale(_)) => {
                self.stale = true;
                let _ = commit(entries, None, None, None).await;
            }
            Err(_) => {}
        }
    }

    /// Record the turn's last entries and move the stored conversation forward
    /// in the same atomic commit. Returns a note for the client when the turn
    /// could not be saved as intended.
    pub(crate) async fn save(
        &mut self,
        entries: Vec<NewEntry>,
        next: &Conversation,
        before: &Snapshot,
    ) -> Option<String> {
        let require = match &before.state {
            Some(s) => StateCondition::Version(s.version.clone()),
            None => StateCondition::Absent,
        };
        if !self.stale {
            let state = Some(to_json(next));
            match commit(entries.clone(), state, Some(require), Some(self.end)).await {
                Ok(CommitResult::Committed(_)) => return None,
                Ok(CommitResult::Stale(_)) => {}
                Err(e) => return Some(format!("not saved: {}", e.describe())),
            }
        }
        // Another writer committed in between. This agent cannot merge two
        // conversations, so it keeps theirs and leaves this turn in the journal.
        self.record(entries).await;
        Some("another request changed the session; this turn is journaled only".into())
    }
}

async fn commit(
    entries: Vec<NewEntry>,
    state: Option<Vec<u8>>,
    require_state: Option<StateCondition>,
    require_end: Option<JournalEnd>,
) -> Result<CommitResult, session::Error> {
    let options = CommitOptions {
        require_state,
        require_end,
        // A fresh id per commit lets the host drop a retried duplicate.
        commit_id: Some(random_id()),
    };
    session::commit(entries, state, options).await
}

pub(crate) fn entry(kind: &str, data: &impl Serialize) -> NewEntry {
    NewEntry {
        kind: kind.into(),
        data: to_json(data),
    }
}

/// A plain `user`, `assistant` or `error` step.
pub(crate) fn step<'a>(role: &'a str, text: &'a str) -> Step<'a> {
    Step {
        role,
        text,
        blocks: if role == "error" {
            &["error"]
        } else {
            &["text"]
        },
        tools: &[],
    }
}

/// The model's tool arguments as a JSON value, or as a JSON string when they
/// are not valid JSON, so the journal entry is always well-formed.
pub(crate) fn json_value(text: &str) -> Box<RawValue> {
    RawValue::from_string(text.to_string())
        .or_else(|_| serde_json::value::to_raw_value(text))
        .unwrap_or_default()
}

pub(crate) fn random_id() -> String {
    use wasip3::random::random::get_random_u64;
    format!("{:016x}{:016x}", get_random_u64(), get_random_u64())
}
