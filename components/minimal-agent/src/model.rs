//! One model call: the alias, the request options, and the streamed reply.

use crate::bindings::cosmonic::agent::chat;
use crate::bindings::cosmonic::agent::inference_types::{
    AssistantContent, AssistantMessage, ChatOptions, ChunkDelta, Completion, Message, Model,
    RequestError, RequestMeta, ResponseFormat, ToolChoice, ToolDef, UnrepresentablePolicy,
};
use crate::bindings::{wit_future, wit_stream};
use crate::describe::Describe;
use crate::wire::{Line, send};
use wasip3::wit_bindgen::StreamWriter;

/// The alias the host maps to a concrete model. Never a model id: which model
/// answers is the operator's choice, made in the workload's binding.
pub(crate) const MODEL_ALIAS: &str = "default";

/// How a model call ended.
pub(crate) enum Answer {
    Done(Completion),
    Failed(String),
    /// The model refused the request as too long for its context window.
    TooLong(String),
    /// The client went away while the reply streamed.
    Gone,
}

impl Answer {
    /// The `agent.op-result.v1` outcome to record.
    pub(crate) fn outcome(&self) -> &'static str {
        match self {
            Answer::Done(_) => "succeeded",
            Answer::Failed(_) | Answer::TooLong(_) => "failed",
            Answer::Gone => "cancelled",
        }
    }
}

/// One chat call, streaming its text to the client as `delta` lines.
pub(crate) async fn answer(
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

/// The assembled reply's text, from the completion rather than the deltas.
pub(crate) fn text_of(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|part| match &part.content {
            AssistantContent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Backend defaults for everything, and the workload's tools. `last` offers
/// none, so the model has to answer.
pub(crate) fn options(offered: &[ToolDef], last: bool) -> ChatOptions {
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
