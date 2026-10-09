//! A minimal sandboxed agent on `cosmonic:agent@0.3.0`.
//!
//! `POST /task` with `{"task": "..."}` runs one chat turn against the model the
//! host resolves for the alias `default`, streams the reply back as NDJSON, and
//! records the turn in the workload's session so the next request continues the
//! same conversation. `GET /history` returns the steps recorded so far. Both
//! follow the protocol the Cosmonic Desktop Agents view speaks.
//!
//! The component names no endpoint, holds no key and opens no connection: the
//! model, the credential and the session store all belong to the host.

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "minimal-agent",
        generate_all,
    });
}

use bindings::cosmonic::agent::inference_types::{
    AssistantContent, AssistantMessage, AssistantPart, ChatOptions, ChunkDelta, Completion,
    Message, RequestMeta, ResponseFormat, UnrepresentablePolicy, UserMessage, UserPart,
};
use bindings::cosmonic::agent::session::{
    self, CommitOptions, CommitResult, JournalEnd, NewEntry, Snapshot, StateCondition,
};
use bindings::cosmonic::agent::{chat, models};
use bindings::{wit_future, wit_stream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wasip3::http::types::{ErrorCode, Fields, Method, Request, Response};
use wasip3::wit_bindgen::{StreamResult, StreamWriter};

/// The alias the host maps to a concrete model. Never a model id: which model
/// answers is the operator's choice, made in the workload's binding.
const MODEL_ALIAS: &str = "default";

const SYSTEM_PROMPT: &str = "You are a concise, helpful assistant.";

/// Required on `POST /task`. A browser cannot add a custom header to a
/// cross-origin request without a CORS preflight this agent never answers, so
/// a web page the person happens to visit cannot drive the session.
const SESSION_HEADER: &str = "x-cosmonic-agent-session";

/// The journal kind a UI draws: `{role, text, blocks, tools}`.
const STEP_KIND: &str = "agent.step.v1";

const USAGE: &str = "POST /task with {\"task\": \"...\"}";

/// The conversation, as this agent stores it in the session's opaque state.
#[derive(Default, Serialize, Deserialize)]
struct Conversation {
    turns: Vec<Turn>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Turn {
    /// `user` or `assistant`.
    role: String,
    text: String,
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
    let mut steps = Vec::new();
    let mut start = 0;
    loop {
        // A short page does not mean the end; an empty one does.
        let page = match session::read(start, 1000).await {
            Ok(page) if page.is_empty() => break,
            Ok(page) => page,
            Err(e) => return json_response(500, json!({ "error": format!("{e:?}") })),
        };
        for entry in page {
            start = entry.seq.saturating_add(1);
            if entry.kind == STEP_KIND
                && let Ok(step) = serde_json::from_slice::<Value>(&entry.data)
            {
                steps.push(step);
            }
        }
    }
    json_response(200, json!({ "steps": steps }))
}

async fn run_task(request: Request) -> Response {
    if request.get_headers().get(SESSION_HEADER).is_empty() {
        let error = format!("missing {SESSION_HEADER} header");
        return json_response(403, json!({ "error": error }));
    }
    let task = match read_body(request).await.ok().and_then(|b| parse_task(&b)) {
        Some(task) => task,
        None => return json_response(400, json!({ "error": USAGE })),
    };

    // What the session holds now: the conversation so far, and the journal end
    // and state version this turn's commits are conditioned on.
    let snapshot = match session::current().await {
        Ok(snapshot) => snapshot,
        Err(e) => return json_response(500, json!({ "error": format!("{e:?}") })),
    };
    let conversation = snapshot
        .state
        .as_ref()
        .and_then(|s| serde_json::from_slice::<Conversation>(&s.value).ok())
        .unwrap_or_default();

    // Record the person's message and the model call before making it, so a
    // turn that fails partway still shows what was asked.
    let op = random_id();
    let started = commit(
        vec![
            step("user", &task),
            entry(
                "agent.op.v1",
                json!({ "op": op, "target": "model", "name": MODEL_ALIAS }),
            ),
        ],
        None,
        None,
        Some(snapshot.end),
    )
    .await;
    let end = match started {
        Ok(CommitResult::Committed(c)) => c.end,
        Ok(CommitResult::Stale(_)) => {
            let error = "another request is writing this session; retry";
            return json_response(409, json!({ "error": error }));
        }
        Err(e) => return json_response(500, json!({ "error": format!("{e:?}") })),
    };

    // Return the response now and write its lines as the turn happens.
    let (response, body) = streaming(200, "application/x-ndjson");
    wit_bindgen::spawn(turn(body, task, conversation, snapshot, op, end));
    response
}

/// How the model call ended.
enum Answer {
    Done(Completion),
    /// `outcome` is the `agent.op-result.v1` outcome to record.
    Failed {
        outcome: &'static str,
        error: String,
    },
    /// The client went away while the reply streamed.
    Gone,
}

/// Run the turn, then record it and finish the response with `done` or
/// `failed`.
async fn turn(
    mut body: StreamWriter<u8>,
    task: String,
    conversation: Conversation,
    before: Snapshot,
    op: String,
    end: JournalEnd,
) {
    let messages = to_messages(&conversation, &task);
    let (outcome, role, text, completion) = match answer(&mut body, messages).await {
        Answer::Done(c) => ("succeeded", "assistant", text_of(&c.message), Some(c)),
        Answer::Failed { outcome, error } => (outcome, "error", error, None),
        Answer::Gone => ("cancelled", "error", "the client disconnected".into(), None),
    };
    let Some(completion) = completion else {
        close_turn(&op, outcome, role, &text, &before, end, None).await;
        send(&mut body, json!({ "type": "failed", "error": text })).await;
        return;
    };

    // The finished message replaces the deltas the client has shown so far.
    let step_line = json!({
        "type": "step", "role": role, "text": text, "blocks": ["text"], "tools": [],
    });
    send(&mut body, step_line).await;

    let mut next = conversation;
    next.turns.push(Turn {
        role: "user".into(),
        text: task,
    });
    next.turns.push(Turn {
        role: "assistant".into(),
        text: text.clone(),
    });
    let context_messages = next.turns.len() + 1;
    // Recorded before `done` is sent, so a client that redraws on `done`
    // finds the turn already stored.
    if let Some(error) = close_turn(&op, outcome, role, &text, &before, end, Some(next)).await {
        send(&mut body, json!({ "type": "failed", "error": error })).await;
        return;
    }
    let usage = &completion.usage;
    let resolved = Some(completion.model.clone()).filter(|m| !m.is_empty());
    let done = json!({
        "type": "done",
        "context_messages": context_messages,
        "answer": text,
        "turns": 1,
        "input_tokens": usage.prompt_tokens.unwrap_or(0),
        "output_tokens": usage.completion_tokens.unwrap_or(0),
        "model": { "alias": MODEL_ALIAS, "provider": "cosmonic", "resolved": resolved },
    });
    send(&mut body, done).await;
}

/// One chat call, streaming its text to the client as `delta` lines.
async fn answer(body: &mut StreamWriter<u8>, messages: Vec<Message>) -> Answer {
    // The host resolves the alias and checks this workload may use it.
    let model = match models::open(MODEL_ALIAS.to_string()).await {
        Ok(model) => model,
        Err(e) => {
            let error = format!("no model for {MODEL_ALIAS:?}: {e:?}");
            // Never dispatched, so the call is recorded as cancelled.
            return Answer::Failed {
                outcome: "cancelled",
                error,
            };
        }
    };

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
    let (mut chunks, outcome) = match chat::chat(&model, msg_rx, done_rx, options()).await {
        Ok(reply) => reply,
        Err(e) => {
            let error = format!("the model refused the request: {e:?}");
            return Answer::Failed {
                outcome: "failed",
                error,
            };
        }
    };

    while let Some(chunk) = chunks.next().await {
        // Only text is streamed. Anything else is in the completion.
        if let ChunkDelta::Text(text) = chunk.delta
            && !text.is_empty()
            && !send(body, json!({ "type": "delta", "text": text })).await
        {
            // Dropping the chunk stream cancels generation at the host.
            return Answer::Gone;
        }
    }
    drop(chunks);

    // Read the outcome after the last chunk, as the interface asks.
    match outcome.await {
        Ok(completion) => Answer::Done(completion),
        Err(e) => {
            let error = format!("generation failed: {e:?}");
            Answer::Failed {
                outcome: "failed",
                error,
            }
        }
    }
}

/// Write one NDJSON line. False once the client has gone away.
async fn send(body: &mut StreamWriter<u8>, line: Value) -> bool {
    let mut bytes = line.to_string().into_bytes();
    bytes.push(b'\n');
    body.write_all(bytes).await.is_empty()
}

/// Close the model call opened by this turn and record what came of it. With a
/// new conversation, the state moves forward in the same atomic commit. Returns
/// a note for the client when the turn could not be saved as intended.
async fn close_turn(
    op: &str,
    outcome: &str,
    role: &str,
    text: &str,
    before: &Snapshot,
    end: JournalEnd,
    next: Option<Conversation>,
) -> Option<String> {
    let entries = vec![
        entry(
            "agent.op-result.v1",
            json!({ "op": op, "outcome": outcome }),
        ),
        step(role, text),
    ];
    let Some(next) = next else {
        // Entry-only: the call still has to be recorded even if another writer
        // moved the journal on, so this commit carries no conditions.
        return match commit(entries, None, None, None).await {
            Ok(_) => None,
            Err(e) => Some(format!("not recorded: {e:?}")),
        };
    };
    let state = serde_json::to_vec(&next).unwrap_or_default();
    let require = match &before.state {
        Some(s) => StateCondition::Version(s.version.clone()),
        None => StateCondition::Absent,
    };
    match commit(entries.clone(), Some(state), Some(require), Some(end)).await {
        Ok(CommitResult::Committed(_)) => None,
        // Another writer committed in between. This agent cannot merge two
        // conversations, so it keeps theirs and leaves this turn in the journal.
        Ok(CommitResult::Stale(_)) => match commit(entries, None, None, None).await {
            Ok(_) => {
                Some("another request changed the session; this turn is journaled only".into())
            }
            Err(e) => Some(format!("not recorded: {e:?}")),
        },
        Err(e) => Some(format!("not saved: {e:?}")),
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

fn entry(kind: &str, data: serde_json::Value) -> NewEntry {
    NewEntry {
        kind: kind.into(),
        data: data.to_string().into_bytes(),
    }
}

/// An `agent.step.v1` entry, the shape a UI reading the journal draws.
fn step(role: &str, text: &str) -> NewEntry {
    let blocks = if role == "error" { ["error"] } else { ["text"] };
    entry(
        "agent.step.v1",
        json!({ "role": role, "text": text, "blocks": blocks, "tools": [] }),
    )
}

fn random_id() -> String {
    use wasip3::random::random::get_random_u64;
    format!("{:016x}{:016x}", get_random_u64(), get_random_u64())
}

fn to_messages(conversation: &Conversation, task: &str) -> Vec<Message> {
    let mut messages = vec![Message::System(SYSTEM_PROMPT.into())];
    let turns = conversation.turns.iter().cloned();
    for turn in turns.chain([Turn {
        role: "user".into(),
        text: task.into(),
    }]) {
        messages.push(if turn.role == "assistant" {
            Message::Assistant(AssistantMessage {
                content: vec![AssistantPart {
                    content: AssistantContent::Text(turn.text),
                    opaque: None,
                }],
            })
        } else {
            Message::User(UserMessage {
                content: vec![UserPart::Text(turn.text)],
                name: None,
            })
        });
    }
    messages
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

/// Backend defaults for everything; no tools.
fn options() -> ChatOptions {
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
        tools: vec![],
        tool_choice: None,
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
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let task = value.get("task")?.as_str()?.trim();
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

fn json_response(status: u16, value: Value) -> Response {
    respond(status, "application/json", value.to_string().into_bytes())
}

fn respond(status: u16, content_type: &str, bytes: Vec<u8>) -> Response {
    let (response, mut body) = streaming(status, content_type);
    wit_bindgen::spawn(async move {
        let _ = body.write_all(bytes).await;
    });
    response
}
