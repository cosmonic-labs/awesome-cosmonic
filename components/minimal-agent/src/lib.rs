//! A minimal sandboxed agent on `cosmonic:agent@0.3.0`.
//!
//! `POST /task` with `{"task": "..."}` runs one chat turn against the model the
//! host resolves for the alias `default`, streams the reply back as plain text,
//! and records the turn in the workload's session so the next request continues
//! the same conversation.
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
    AssistantContent, AssistantMessage, AssistantPart, ChatOptions, ChunkDelta, Message,
    RequestMeta, ResponseFormat, UnrepresentablePolicy, UserMessage, UserPart,
};
use bindings::cosmonic::agent::session::{
    self, CommitOptions, CommitResult, JournalEnd, NewEntry, Snapshot, StateCondition,
};
use bindings::cosmonic::agent::{chat, models};
use bindings::{wit_future, wit_stream};
use serde::{Deserialize, Serialize};
use serde_json::json;
use wasip3::http::types::{ErrorCode, Fields, Method, Request, Response};
use wasip3::wit_bindgen::{StreamResult, StreamWriter};

/// The alias the host maps to a concrete model. Never a model id: which model
/// answers is the operator's choice, made in the workload's binding.
const MODEL_ALIAS: &str = "default";

const SYSTEM_PROMPT: &str = "You are a concise, helpful assistant.";

const USAGE: &str = "POST /task with {\"task\": \"...\"}\n";

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
            (Method::Post, "/task") => run_task(request).await,
            _ => respond(404, USAGE.into()),
        })
    }
}

wasip3::http::service::export!(Agent);

async fn run_task(request: Request) -> Response {
    let task = match read_body(request).await.ok().and_then(|b| parse_task(&b)) {
        Some(task) => task,
        None => return respond(400, USAGE.into()),
    };

    // What the session holds now: the conversation so far, and the journal end
    // and state version this turn's commits are conditioned on.
    let snapshot = match session::current().await {
        Ok(snapshot) => snapshot,
        Err(e) => return respond(500, format!("session unavailable: {e:?}\n")),
    };
    let conversation = snapshot
        .state
        .as_ref()
        .and_then(|s| serde_json::from_slice::<Conversation>(&s.value).ok())
        .unwrap_or_default();

    // The host resolves the alias and checks this workload may use it.
    let model = match models::open(MODEL_ALIAS.to_string()).await {
        Ok(model) => model,
        Err(e) => return respond(502, format!("no model for {MODEL_ALIAS:?}: {e:?}\n")),
    };

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
            return respond(
                409,
                "another request is writing this session; retry\n".into(),
            );
        }
        Err(e) => return respond(500, format!("session write failed: {e:?}\n")),
    };

    // Streams do not buffer, so the messages are written while `chat` is
    // pending, then `messages-done` says the conversation is complete.
    let messages = to_messages(&conversation, &task);
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
            let text = format!("the model refused the request: {e:?}");
            close_turn(&op, "failed", "error", &text, &snapshot, end, None).await;
            return respond(502, format!("{text}\n"));
        }
    };

    // Return the response now and write its body as the reply arrives.
    let (response, mut body) = streaming(200, "text/plain; charset=utf-8");
    wit_bindgen::spawn(async move {
        let _model = model;
        let mut client_gone = false;
        while let Some(chunk) = chunks.next().await {
            // Only text is streamed. Anything else is in the completion.
            if let ChunkDelta::Text(text) = chunk.delta
                && !body.write_all(text.into_bytes()).await.is_empty()
            {
                // The client went away. Dropping the chunk stream cancels
                // generation at the host.
                client_gone = true;
                break;
            }
        }
        drop(chunks);
        if client_gone {
            close_turn(
                &op,
                "cancelled",
                "error",
                "the client disconnected",
                &snapshot,
                end,
                None,
            )
            .await;
            return;
        }

        // Read the outcome after the last chunk, as the interface asks.
        let note = match outcome.await {
            Ok(completion) => {
                let reply = text_of(&completion.message);
                let mut next = conversation;
                next.turns.push(Turn {
                    role: "user".into(),
                    text: task,
                });
                next.turns.push(Turn {
                    role: "assistant".into(),
                    text: reply.clone(),
                });
                close_turn(
                    &op,
                    "succeeded",
                    "assistant",
                    &reply,
                    &snapshot,
                    end,
                    Some(next),
                )
                .await
            }
            Err(e) => {
                let text = format!("generation failed: {e:?}");
                close_turn(&op, "failed", "error", &text, &snapshot, end, None).await;
                Some(text)
            }
        };
        if let Some(note) = note {
            let _ = body.write_all(format!("\n[{note}]").into_bytes()).await;
        }
        let _ = body.write_all(b"\n".to_vec()).await;
    });
    response
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

fn respond(status: u16, text: String) -> Response {
    let (response, mut body) = streaming(status, "text/plain; charset=utf-8");
    wit_bindgen::spawn(async move {
        let _ = body.write_all(text.into_bytes()).await;
    });
    response
}
