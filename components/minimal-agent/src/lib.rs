//! A minimal sandboxed agent on `cosmonic:agent@0.3.0`.
//!
//! `POST /task` with `{"task": "..."}` runs one turn: the model the host
//! resolves for the alias `default` answers, calling the tools the workload
//! grants as often as it needs to, and the reply streams back as NDJSON. The
//! turn is recorded in the workload's session so the next request continues
//! the same conversation. `GET /history` returns the steps recorded so far.
//! Both follow the protocol the Cosmonic Desktop Agents view speaks.
//!
//! The component names no endpoint, holds no key and opens no connection: the
//! model, the credential, the tools and the session store all belong to the
//! host.

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "minimal-agent",
        generate_all,
    });
}

use bindings::cosmonic::agent::inference_types::{
    AssistantContent, AssistantMessage, AssistantPart, Attachment, ChatOptions, ChunkDelta,
    Completion, ContextOverflow, GenerationError, Message, Model, Quota, RateLimit, RequestError,
    RequestMeta, ResponseFormat, SizeLimit, ToolCall, ToolChoice, ToolDef, ToolOutput, ToolResult,
    UnrepresentablePolicy, UserMessage, UserPart,
};
use bindings::cosmonic::agent::models::OpenError;
use bindings::cosmonic::agent::session::{
    self, CommitOptions, CommitResult, Entry, JournalEnd, NewEntry, Snapshot, StateCondition,
};
use bindings::cosmonic::agent::tools::CallError;
use bindings::cosmonic::agent::{chat, models, tools};
use bindings::{wit_future, wit_stream};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use wasip3::http::types::{ErrorCode, Fields, Method, Request, Response};
use wasip3::wit_bindgen::{StreamResult, StreamWriter};

/// The alias the host maps to a concrete model. Never a model id: which model
/// answers is the operator's choice, made in the workload's binding.
const MODEL_ALIAS: &str = "default";

/// Tells the model what it is, so it does not offer to browse or run code.
const SYSTEM_PROMPT: &str = "You are a small agent running as a sandboxed WebAssembly \
    component. You have no network access of your own: you reach the outside world only \
    through the tools you are given, and you may have none. Use a tool when it helps, and \
    never claim a result a tool did not return. Answer concisely.";

/// Model calls per turn. The last one is made without tools, so the turn
/// always ends with an answer.
const MAX_MODEL_CALLS: u32 = 8;

/// Required on `POST /task`. A browser cannot add a custom header to a
/// cross-origin request without a CORS preflight this agent never answers, so
/// a web page the person happens to visit cannot drive the session.
const SESSION_HEADER: &str = "x-cosmonic-agent-session";

/// The journal kind a UI draws: `{role, text, blocks, tools}`.
const STEP_KIND: &str = "agent.step.v1";
/// A model or tool call, written before it is made.
const OP_KIND: &str = "agent.op.v1";
/// How that call ended, written after it returns.
const OP_RESULT_KIND: &str = "agent.op-result.v1";

const USAGE: &str = "POST /task with {\"task\": \"...\"}";

/// The stored conversation is kept under both limits, oldest exchanges first
/// out, so it neither outgrows the session nor crowds the model's context.
const MAX_TURNS: usize = 40;
const MAX_STATE_BYTES: usize = 64 * 1024;

/// The conversation, as this agent stores it in the session's opaque state.
#[derive(Default, Serialize, Deserialize)]
struct Conversation {
    turns: Vec<Turn>,
}

/// One stored message: a `user` or `assistant` message, with any tool calls
/// the assistant made, or the `tool` results that answered them. Tool turns
/// are kept so a later turn knows what the tools returned; attachments are
/// kept as a note, not bytes. A flat struct rather than an enum tagged by
/// `role`, because serde buffers tagged enums through a generic value type
/// that would add about 10 KB to the component.
#[derive(Default, Serialize, Deserialize)]
struct Turn {
    role: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    calls: Vec<StoredCall>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    results: Vec<StoredResult>,
}

impl Turn {
    fn new(role: &str, text: String) -> Self {
        Self {
            role: role.into(),
            text,
            ..Self::default()
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoredCall {
    id: String,
    name: String,
    /// JSON text, as the model wrote it.
    arguments: String,
}

#[derive(Serialize, Deserialize)]
struct StoredResult {
    call_id: String,
    text: String,
    #[serde(default)]
    is_error: bool,
}

impl Conversation {
    /// Drop the oldest exchange: the first turn and the replies that follow it.
    fn drop_oldest(&mut self) -> bool {
        if self.turns.is_empty() {
            return false;
        }
        let next = self.turns.iter().skip(1).position(|t| t.role == "user");
        self.turns.drain(..next.map_or(self.turns.len(), |i| i + 1));
        true
    }

    /// Keep the conversation under `MAX_TURNS` and `MAX_STATE_BYTES`.
    fn trim(&mut self) {
        while (self.turns.len() > MAX_TURNS || to_json(self).len() > MAX_STATE_BYTES)
            && self.drop_oldest()
        {}
    }

    /// Drop the oldest half, for a model whose context the conversation
    /// overflowed. False if there was nothing to drop.
    fn halve(&mut self) -> bool {
        let before = self.turns.len();
        while self.turns.len() > before / 2 && self.drop_oldest() {}
        self.turns.len() < before
    }
}

/// One line of the `/task` response.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Line<'a> {
    /// A piece of reply text as it arrives.
    Delta { text: &'a str },
    /// A finished message. An assistant step replaces the deltas shown so far.
    Step(&'a Step<'a>),
    /// The turn is stored.
    Done {
        context_messages: usize,
        answer: &'a str,
        turns: u32,
        input_tokens: u32,
        output_tokens: u32,
        model: ModelInfo<'a>,
    },
    /// The turn failed.
    Failed { error: &'a str },
}

/// An `agent.step.v1` entry and the `step` line: the shape a UI draws.
#[derive(Serialize)]
struct Step<'a> {
    /// `user`, `assistant`, `tool` or `error`.
    role: &'a str,
    text: &'a str,
    /// The kinds of content: `text`, `toolRequest`, `toolResponse`, `error`.
    blocks: &'a [&'a str],
    /// The tools an assistant step asks for, or a tool step answers.
    tools: &'a [&'a str],
}

/// `agent.op.v1`: a model or tool call is about to be made. A tool call's op
/// is its call id.
#[derive(Serialize)]
struct Op<'a> {
    op: &'a str,
    /// `model` or `tool`.
    target: &'a str,
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<Box<RawValue>>,
}

/// `agent.op-result.v1`: how that call ended.
#[derive(Serialize)]
struct OpResult<'a> {
    op: &'a str,
    /// `succeeded`, `failed`, `cancelled` or `unresolved`.
    outcome: &'a str,
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

#[derive(Serialize)]
struct ModelInfo<'a> {
    alias: &'a str,
    provider: &'a str,
    resolved: Option<&'a str>,
}

/// The body of every error response: `{"error": "..."}`.
#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

/// `POST /task`'s body. Other fields are ignored.
#[derive(Deserialize)]
struct TaskRequest {
    task: String,
}

struct Agent;

impl wasip3::exports::http::handler::Guest for Agent {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let path = request.get_path_with_query().unwrap_or_default();
        Ok(match (request.get_method(), path.as_str()) {
            (Method::Get, "/history") => history().await,
            (Method::Post, "/task") => run_task(request).await,
            _ => respond(404, "text/plain; charset=utf-8", USAGE.into()),
        })
    }
}

wasip3::http::service::export!(Agent);

/// `{"steps": [...]}`: every step in the session journal, oldest first.
async fn history() -> Response {
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

async fn run_task(request: Request) -> Response {
    if request.get_headers().get(SESSION_HEADER).is_empty() {
        return error_response(403, &format!("missing {SESSION_HEADER} header"));
    }
    let task = match read_body(request).await.ok().and_then(|b| parse_task(&b)) {
        Some(task) => task,
        None => return error_response(400, USAGE),
    };

    // What the session holds now: the conversation so far, and the journal end
    // and state version this turn's commits are conditioned on.
    let snapshot = match session::current().await {
        Ok(snapshot) => snapshot,
        Err(e) => return store_failed(&e),
    };
    let conversation = snapshot
        .state
        .as_ref()
        .and_then(|s| serde_json::from_slice::<Conversation>(&s.value).ok())
        .unwrap_or_default();

    // Close whatever a turn cut off by a restart left open, then record the
    // person's message, before any call is made.
    let mut entries = match recover(&snapshot).await {
        Ok(entries) => entries,
        Err(e) => return store_failed(&e),
    };
    entries.push(entry(STEP_KIND, &step("user", &task)));
    let mut journal = Journal {
        end: snapshot.end,
        stale: false,
    };
    if let Err(error) = journal.append(entries).await {
        return error_response(409, &error);
    }

    // Return the response now and write its lines as the turn happens.
    let (response, body) = streaming(200, "application/x-ndjson");
    wit_bindgen::spawn(turn(body, task, conversation, snapshot, journal));
    response
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
async fn recover(snapshot: &Snapshot) -> Result<Vec<NewEntry>, session::Error> {
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
struct Journal {
    end: JournalEnd,
    /// Another writer moved the journal on, so no further call is made.
    stale: bool,
}

impl Journal {
    /// Append entries that must come before a call: only if nothing else has
    /// written the session since this turn's last write.
    async fn append(&mut self, entries: Vec<NewEntry>) -> Result<(), String> {
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
    async fn record(&mut self, entries: Vec<NewEntry>) {
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
    async fn save(
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

/// How a model call ended.
enum Answer {
    Done(Completion),
    Failed(String),
    /// The model refused the request as too long for its context window.
    TooLong(String),
    /// The client went away while the reply streamed.
    Gone,
}

impl Answer {
    /// The `agent.op-result.v1` outcome to record.
    fn outcome(&self) -> &'static str {
        match self {
            Answer::Done(_) => "succeeded",
            Answer::Failed(_) | Answer::TooLong(_) => "failed",
            Answer::Gone => "cancelled",
        }
    }
}

/// What a finished turn hands to be saved and reported.
#[derive(Default)]
struct Finished {
    answer: String,
    /// This turn's messages, to append to the stored conversation.
    turns: Vec<Turn>,
    model_calls: u32,
    context_messages: usize,
    input_tokens: u32,
    output_tokens: u32,
    resolved: String,
}

/// Run the turn, then record it and finish the response with `done` or
/// `failed`.
async fn turn(
    mut body: StreamWriter<u8>,
    task: String,
    mut conversation: Conversation,
    before: Snapshot,
    mut journal: Journal,
) {
    let finished = match run(&mut body, task, &mut conversation, &mut journal).await {
        Ok(finished) => finished,
        Err(error) => {
            journal
                .record(vec![entry(STEP_KIND, &step("error", &error))])
                .await;
            send(&mut body, &Line::Failed { error: &error }).await;
            return;
        }
    };

    // The finished message replaces the deltas the client has shown so far.
    let answer = step("assistant", &finished.answer);
    send(&mut body, &Line::Step(&answer)).await;

    let mut next = conversation;
    next.turns.extend(finished.turns);
    next.trim();
    // Recorded before `done` is sent, so a client that redraws on `done`
    // finds the turn already stored.
    let entries = vec![entry(STEP_KIND, &answer)];
    if let Some(error) = journal.save(entries, &next, &before).await {
        send(&mut body, &Line::Failed { error: &error }).await;
        return;
    }
    let done = Line::Done {
        context_messages: finished.context_messages,
        answer: &finished.answer,
        turns: finished.model_calls,
        input_tokens: finished.input_tokens,
        output_tokens: finished.output_tokens,
        model: ModelInfo {
            alias: MODEL_ALIAS,
            provider: "cosmonic",
            resolved: Some(finished.resolved.as_str()).filter(|m| !m.is_empty()),
        },
    };
    send(&mut body, &done).await;
}

/// The tool loop: call the model, make the tool calls it asks for, hand it
/// the results, and repeat until it answers without asking for more.
async fn run(
    body: &mut StreamWriter<u8>,
    task: String,
    conversation: &mut Conversation,
    journal: &mut Journal,
) -> Result<Finished, String> {
    // The host resolves the alias and checks this workload may use it.
    let model = models::open(MODEL_ALIAS.to_string())
        .await
        .map_err(|e| format!("no model for the alias {MODEL_ALIAS}: {}", e.describe()))?;
    let offered = offered_tools().await;

    // This turn's messages as the model sees them, assistant messages exactly
    // as returned and tool results with their attachments, and as stored.
    let mut live = vec![user_message(task.clone())];
    let mut turns = vec![Turn::new("user", task)];
    let mut finished = Finished::default();
    let mut shortened = false;
    loop {
        finished.model_calls += 1;
        let last = finished.model_calls >= MAX_MODEL_CALLS;
        let mut messages = to_messages(conversation);
        messages.extend(live.iter().cloned());
        // The messages in context once the reply is added.
        finished.context_messages = messages.len() + 1;

        let op = random_id();
        let call = Op {
            op: &op,
            target: "model",
            name: MODEL_ALIAS,
            arguments: None,
        };
        journal.append(vec![entry(OP_KIND, &call)]).await?;
        let reply = answer(body, &model, messages, options(&offered, last)).await;
        let outcome = reply.outcome();
        journal
            .record(vec![entry(OP_RESULT_KIND, &OpResult { op: &op, outcome })])
            .await;
        let completion = match reply {
            Answer::Done(completion) => completion,
            // Too long for the model: forget the oldest half of the stored
            // conversation and try once more.
            Answer::TooLong(_) if !shortened && conversation.halve() => {
                shortened = true;
                continue;
            }
            Answer::Failed(error) | Answer::TooLong(error) => return Err(error),
            Answer::Gone => return Err("the client disconnected".into()),
        };
        let usage = &completion.usage;
        finished.input_tokens += usage.prompt_tokens.unwrap_or(0);
        finished.output_tokens += usage.completion_tokens.unwrap_or(0);
        finished.resolved = completion.model.clone();
        let text = text_of(&completion.message);
        // The last call offers no tools; a request made anyway is dropped.
        let calls: Vec<ToolCall> = match last {
            true => Vec::new(),
            false => tool_calls(&completion.message),
        };
        if calls.is_empty() {
            turns.push(Turn::new("assistant", text.clone()));
            finished.answer = text;
            finished.turns = turns;
            return Ok(finished);
        }

        let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
        let blocks: &[&str] = match text.is_empty() {
            true => &["toolRequest"],
            false => &["text", "toolRequest"],
        };
        let request = Step {
            role: "assistant",
            text: &text,
            blocks,
            tools: &names,
        };
        journal.record(vec![entry(STEP_KIND, &request)]).await;
        send(body, &Line::Step(&request)).await;

        let mut results = Vec::new();
        for call in &calls {
            results.push(call_tool(body, journal, call).await?);
        }
        let calls = calls
            .into_iter()
            .map(|c| StoredCall {
                id: c.id,
                name: c.name,
                arguments: c.arguments,
            })
            .collect();
        turns.push(Turn {
            calls,
            ..Turn::new("assistant", text)
        });
        turns.push(Turn {
            results: results.iter().map(stored_result).collect(),
            ..Turn::new("tool", String::new())
        });
        live.push(Message::Assistant(completion.message));
        live.push(Message::Tool(results));
    }
}

/// What the workload's tool provider offers, described to the model as the
/// provider describes it. None when the workload grants no tools.
async fn offered_tools() -> Vec<ToolDef> {
    tools::list()
        .await
        .into_iter()
        .map(|t| ToolDef {
            name: t.name,
            title: t.title,
            description: t.description,
            input_schema: t.input_schema,
            output_schema: t.output_schema,
        })
        .collect()
}

/// Make one tool call the model asked for, recorded around the dispatch: the
/// op before, so a restart knows it may have happened, and the result after.
async fn call_tool(
    body: &mut StreamWriter<u8>,
    journal: &mut Journal,
    call: &ToolCall,
) -> Result<ToolResult, String> {
    let op = Op {
        op: &call.id,
        target: "tool",
        name: &call.name,
        arguments: Some(json_value(&call.arguments)),
    };
    journal.append(vec![entry(OP_KIND, &op)]).await?;

    let (output, is_error, outcome) = match tools::call(call.clone()).await {
        Ok(r) if r.is_error => (r.output, true, "failed"),
        Ok(r) => (r.output, false, "succeeded"),
        // Cut off before it returned: it was made, and whether it took effect
        // is not known.
        Err(e @ (CallError::Cancelled | CallError::TimedOut)) => {
            (vec![ToolOutput::Text(e.describe())], true, "unresolved")
        }
        // Not dispatched. The model gets the reason, so it can try again.
        Err(e) => (vec![ToolOutput::Text(e.describe())], true, "failed"),
    };
    let text = output_text(&output);
    let names = [call.name.as_str()];
    let response = Step {
        role: "tool",
        text: &text,
        blocks: &["toolResponse"],
        tools: &names,
    };
    let entries = vec![
        entry(
            OP_RESULT_KIND,
            &OpResult {
                op: &call.id,
                outcome,
            },
        ),
        entry(STEP_KIND, &response),
    ];
    journal.record(entries).await;
    if !send(body, &Line::Step(&response)).await {
        return Err("the client disconnected".into());
    }
    Ok(ToolResult {
        call_id: call.id.clone(),
        output,
        is_error,
    })
}

/// One chat call, streaming its text to the client as `delta` lines.
async fn answer(
    body: &mut StreamWriter<u8>,
    model: &Model,
    messages: Vec<Message>,
    options: ChatOptions,
) -> Answer {
    // Streams do not buffer, so the messages are written while `chat` is
    // pending, then `messages-done` says the conversation is complete.
    let (mut msg_tx, msg_rx) = wit_stream::new();
    let (done_tx, done_rx) = wit_future::new(|| Err("the agent stopped writing".to_string()));
    wit_bindgen::spawn(async move {
        let unwritten = msg_tx.write_all(messages).await;
        drop(msg_tx);
        let verdict = if unwritten.is_empty() {
            Ok(())
        } else {
            Err("messages lost".into())
        };
        let _ = done_tx.write(verdict).await;
    });
    let (mut chunks, outcome) = match chat::chat(model, msg_rx, done_rx, options).await {
        Ok(reply) => reply,
        Err(e @ RequestError::ContextLengthExceeded(_)) => return Answer::TooLong(e.describe()),
        Err(e) => {
            return Answer::Failed(format!("the model refused the request: {}", e.describe()));
        }
    };

    while let Some(chunk) = chunks.next().await {
        // Only text is streamed. Anything else is in the completion.
        if let ChunkDelta::Text(text) = chunk.delta
            && !text.is_empty()
            && !send(body, &Line::Delta { text: &text }).await
        {
            // Dropping the chunk stream cancels generation at the host.
            return Answer::Gone;
        }
    }
    drop(chunks);

    // Read the outcome after the last chunk, as the interface asks.
    match outcome.await {
        Ok(completion) => Answer::Done(completion),
        Err(e) => Answer::Failed(format!("generation failed: {}", e.describe())),
    }
}

/// Write one NDJSON line. False once the client has gone away.
async fn send(body: &mut StreamWriter<u8>, line: &Line<'_>) -> bool {
    let mut bytes = to_json(line);
    bytes.push(b'\n');
    body.write_all(bytes).await.is_empty()
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

fn entry(kind: &str, data: &impl Serialize) -> NewEntry {
    NewEntry {
        kind: kind.into(),
        data: to_json(data),
    }
}

/// A plain `user`, `assistant` or `error` step.
fn step<'a>(role: &'a str, text: &'a str) -> Step<'a> {
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

/// Serializing this file's types cannot fail: they have no maps with
/// non-string keys and no custom serializers.
fn to_json(value: &impl Serialize) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_default()
}

/// The model's tool arguments as a JSON value, or as a JSON string when they
/// are not valid JSON, so the journal entry is always well-formed.
fn json_value(text: &str) -> Box<RawValue> {
    RawValue::from_string(text.to_string())
        .or_else(|_| serde_json::value::to_raw_value(text))
        .unwrap_or_default()
}

fn random_id() -> String {
    use wasip3::random::random::get_random_u64;
    format!("{:016x}{:016x}", get_random_u64(), get_random_u64())
}

/// The system prompt and the stored conversation, as model messages.
fn to_messages(conversation: &Conversation) -> Vec<Message> {
    let mut messages = vec![Message::System(SYSTEM_PROMPT.into())];
    for turn in &conversation.turns {
        messages.push(match turn.role.as_str() {
            "assistant" => {
                let text =
                    (!turn.text.is_empty()).then(|| AssistantContent::Text(turn.text.clone()));
                let calls = turn.calls.iter().map(|c| {
                    AssistantContent::ToolUse(ToolCall {
                        id: c.id.clone(),
                        name: c.name.clone(),
                        arguments: c.arguments.clone(),
                    })
                });
                let content = text.into_iter().chain(calls);
                Message::Assistant(AssistantMessage {
                    content: content
                        .map(|content| AssistantPart {
                            content,
                            opaque: None,
                        })
                        .collect(),
                })
            }
            "tool" => Message::Tool(
                turn.results
                    .iter()
                    .map(|r| ToolResult {
                        call_id: r.call_id.clone(),
                        output: vec![ToolOutput::Text(r.text.clone())],
                        is_error: r.is_error,
                    })
                    .collect(),
            ),
            _ => user_message(turn.text.clone()),
        });
    }
    messages
}

fn user_message(text: String) -> Message {
    Message::User(UserMessage {
        content: vec![UserPart::Text(text)],
        name: None,
    })
}

/// The assembled reply's text, from the completion rather than the deltas.
fn text_of(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|part| match &part.content {
            AssistantContent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// The tool calls in a reply, in order.
fn tool_calls(message: &AssistantMessage) -> Vec<ToolCall> {
    message
        .content
        .iter()
        .filter_map(|part| match &part.content {
            AssistantContent::ToolUse(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

/// A tool's output as text: the model sees attachments themselves, the
/// journal and the stored conversation a note naming each one.
fn output_text(output: &[ToolOutput]) -> String {
    output
        .iter()
        .map(|o| match o {
            ToolOutput::Text(text) => text.clone(),
            ToolOutput::Attachment(Attachment { media_type, .. }) => {
                format!("[{media_type} attachment]")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn stored_result(result: &ToolResult) -> StoredResult {
    StoredResult {
        call_id: result.call_id.clone(),
        text: output_text(&result.output),
        is_error: result.is_error,
    }
}

/// Backend defaults for everything, and the workload's tools. `last` offers
/// none, so the model has to answer.
fn options(offered: &[ToolDef], last: bool) -> ChatOptions {
    ChatOptions {
        max_tokens: None,
        temperature: None,
        top_k: None,
        top_p: None,
        min_p: None,
        frequency_penalty: None,
        presence_penalty: None,
        repeat_penalty: None,
        penalty_last_n: None,
        logit_bias: vec![],
        seed: None,
        stop: vec![],
        response_format: ResponseFormat::Text,
        tools: if last { vec![] } else { offered.to_vec() },
        tool_choice: last.then_some(ToolChoice::None),
        thinking: None,
        effort: None,
        meta: RequestMeta {
            trace_parent: None,
            deadline_ms: None,
            cache_key: None,
            labels: vec![],
        },
        on_unrepresentable: UnrepresentablePolicy::Adapt,
    }
}

fn parse_task(body: &[u8]) -> Option<String> {
    let request: TaskRequest = serde_json::from_slice(body).ok()?;
    let task = request.task.trim();
    (!task.is_empty()).then(|| task.to_string())
}

async fn read_body(request: Request) -> Result<Vec<u8>, ErrorCode> {
    let (result_tx, result_rx) = wasip3::wit_future::new(|| Ok(()));
    let (mut stream, trailers) = Request::consume_body(request, result_rx);
    drop(result_tx);
    let mut buf = Vec::with_capacity(4096);
    loop {
        buf.reserve(4096);
        let (status, returned) = stream.read(buf).await;
        buf = returned;
        if !matches!(status, StreamResult::Complete(_)) {
            break;
        }
    }
    trailers.await?;
    Ok(buf)
}

/// A response whose body is written after it is returned.
fn streaming(status: u16, content_type: &str) -> (Response, StreamWriter<u8>) {
    let headers = [("content-type".to_string(), content_type.as_bytes().to_vec())];
    let fields = Fields::from_list(&headers).unwrap_or_else(|_| Fields::new());
    let (body_tx, body_rx) = wasip3::wit_stream::new();
    let (trailers_tx, trailers_rx) = wasip3::wit_future::new(|| Ok(None));
    let (response, _sent) = Response::new(fields, Some(body_rx), trailers_rx);
    drop(trailers_tx);
    let _ = response.set_status_code(status);
    (response, body_tx)
}

fn store_failed(e: &session::Error) -> Response {
    error_response(500, &format!("the session store failed: {}", e.describe()))
}

fn error_response(status: u16, error: &str) -> Response {
    respond(status, "application/json", to_json(&ErrorBody { error }))
}

fn respond(status: u16, content_type: &str, bytes: Vec<u8>) -> Response {
    let (response, mut body) = streaming(status, content_type);
    wit_bindgen::spawn(async move {
        let _ = body.write_all(bytes).await;
    });
    response
}

/// Error messages a person can read. The generated types implement `Display`
/// and `Debug` too, but both print Rust syntax, and the formatting code behind
/// them is a good part of the component's size.
trait Describe {
    fn describe(&self) -> String;
}

impl Describe for OpenError {
    fn describe(&self) -> String {
        match self {
            Self::UnknownAlias(d) => format!("the host has no model by that alias ({d})"),
            Self::Denied(d) => format!("this workload may not use it ({d})"),
            Self::Unavailable(d) => format!("the model is unavailable ({d})"),
            Self::TooLarge(limit) => format!("the alias is over the {limit}-byte limit"),
            Self::Other(d) => d.clone(),
        }
    }
}

impl Describe for RequestError {
    fn describe(&self) -> String {
        match self {
            Self::ContextLengthExceeded(c) => c.describe(),
            Self::InvalidOptions(d) => format!("invalid options ({d})"),
            Self::MessagesIncomplete(d) => format!("the conversation was incomplete ({d})"),
            Self::Unrepresentable(changes) => {
                let details: Vec<&str> = changes.iter().map(|c| c.detail.as_str()).collect();
                format!(
                    "the provider cannot carry this request ({})",
                    details.join("; ")
                )
            }
            Self::NotAuthenticated(d) => {
                format!("the host's credentials for the provider were rejected ({d})")
            }
            Self::RateLimited(r) => format!("rate limited: {}", r.describe()),
            Self::QuotaExhausted(q) => q.describe(),
            Self::TemporarilyUnavailable(r) => format!("temporarily unavailable: {}", r.describe()),
            Self::TooLarge(s) => s.describe(),
            Self::Other(d) => d.clone(),
        }
    }
}

impl Describe for GenerationError {
    fn describe(&self) -> String {
        match self {
            Self::RateLimited(r) => format!("rate limited: {}", r.describe()),
            Self::TemporarilyUnavailable(r) => format!("temporarily unavailable: {}", r.describe()),
            Self::ContextLengthExceeded(c) => c.describe(),
            Self::Other(d) => d.clone(),
        }
    }
}

impl Describe for ContextOverflow {
    fn describe(&self) -> String {
        format!(
            "the conversation is too long for the model ({})",
            self.detail
        )
    }
}

impl Describe for RateLimit {
    fn describe(&self) -> String {
        match self.retry_after_seconds {
            Some(s) => format!("{}; retry in {s}s", self.detail),
            None => self.detail.clone(),
        }
    }
}

impl Describe for Quota {
    fn describe(&self) -> String {
        let out = format!("the provider account is out of quota ({})", self.detail);
        match &self.top_up_url {
            Some(url) => format!("{out}; top up at {url}"),
            None => out,
        }
    }
}

impl Describe for SizeLimit {
    fn describe(&self) -> String {
        format!(
            "the {} is over the host's {}-byte limit",
            self.what, self.limit
        )
    }
}

impl Describe for session::Error {
    fn describe(&self) -> String {
        match self {
            Self::TooLarge(t) => format!("the session is full ({}-byte limit)", t.limit),
            Self::InvalidArgument(d) => format!("invalid request ({d})"),
            Self::Unavailable(d) => format!("unavailable ({d})"),
            Self::Other(d) => d.clone(),
        }
    }
}

impl Describe for CallError {
    fn describe(&self) -> String {
        match self {
            Self::UnknownTool(name) => format!("there is no tool named {name}"),
            Self::InvalidArguments(d) => format!("invalid arguments: {d}"),
            Self::Upstream(d) => format!("the tool could not run: {d}"),
            Self::Denied(d) => format!("the call was declined: {d}"),
            Self::Cancelled => "the call was cancelled".into(),
            Self::TimedOut => "the call timed out".into(),
            Self::Other(d) => d.clone(),
        }
    }
}
