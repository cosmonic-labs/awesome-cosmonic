//! A turn: start it from a `/task` request, then run the tool loop. `run`
//! is the loop: call the model, make the tool calls it asks for, repeat.

use crate::bindings::cosmonic::agent::inference_types::{Message, ToolCall};
use crate::bindings::cosmonic::agent::models;
use crate::bindings::cosmonic::agent::session::{self, Snapshot};
use crate::conversation::{Conversation, StoredCall, Turn, to_messages, user_message};
use crate::describe::Describe;
use crate::journal::{
    Journal, OP_KIND, OP_RESULT_KIND, Op, OpResult, STEP_KIND, Step, entry, random_id, recover,
    step,
};
use crate::model::{Answer, MODEL_ALIAS, answer, options, text_of};
use crate::tools::{call_tool, offered_tools, stored_result, tool_calls};
use crate::wire::{
    Line, ModelInfo, SESSION_HEADER, USAGE, error_response, parse_task, read_body, send,
    store_failed, streaming,
};
use wasip3::http::types::{Request, Response};
use wasip3::wit_bindgen::StreamWriter;

/// Model calls per turn. The last one is made without tools, so the turn
/// always ends with an answer.
const MAX_MODEL_CALLS: u32 = 8;

/// `POST /task`: check the request, open the turn in the journal, and run it
/// while the response streams.
pub(crate) async fn run_task(request: Request) -> Response {
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
    let mut journal = Journal::new(snapshot.end);
    if let Err(error) = journal.append(entries).await {
        return error_response(409, &error);
    }

    // Return the response now and write its lines as the turn happens.
    let (response, body) = streaming(200, "application/x-ndjson");
    wit_bindgen::spawn(turn(body, task, conversation, snapshot, journal));
    response
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
