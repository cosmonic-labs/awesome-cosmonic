//! One model call: the alias, the request options, and the streamed reply.

use crate::agent::chat;
use crate::agent::inference_types::{
    AssistantContent, AssistantMessage, ChatOptions, ChunkDelta, Completion, Effort,
    GenerationError, Message, Model, RequestError, RequestMeta, ResponseFormat, ToolChoice,
    ToolDef, UnrepresentablePolicy,
};
use crate::bindings::{wit_future, wit_stream};
use crate::describe::Describe;
use crate::events::Events;

/// The alias the host maps to a concrete model. Never a model id: which model
/// answers is the operator's choice, made in the workload's binding.
pub(crate) const MODEL_ALIAS: &str = "default";

/// How a model call ended.
pub(crate) enum Answer {
    Done(Completion),
    /// Refused before it was accepted.
    Refused(RequestError),
    /// Failed after it was accepted.
    Failed(GenerationError),
    /// The driver dropped the event stream while the reply streamed.
    Gone,
}

impl Answer {
    /// The `agent.op-result.v1` outcome to record.
    pub(crate) fn outcome(&self) -> &'static str {
        match self {
            Answer::Done(_) => "succeeded",
            Answer::Refused(_) | Answer::Failed(_) => "failed",
            Answer::Gone => "cancelled",
        }
    }
}

/// One chat call, streaming its text and reasoning to the driver as deltas.
pub(crate) async fn answer(
    events: &mut Events,
    model: &Model,
    messages: Vec<Message>,
    options: ChatOptions,
) -> Answer {
    // Streams do not buffer, so the messages are written while `chat` is
    // pending, then `messages-done` says the conversation is complete.
    let (mut msg_tx, msg_rx) = wit_stream::new();
    let (done_tx, done_rx) = wit_future::new(|| Err("the agent stopped writing".to_string()));
    wit_bindgen::spawn_local(async move {
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
        Err(e) => return Answer::Refused(e),
    };

    while let Some(chunk) = chunks.next().await {
        // Text and reasoning are streamed. Everything else is in the
        // completion, and so in the step that follows.
        let delivered = match chunk.delta {
            ChunkDelta::Text(text) if !text.is_empty() => events.text(text).await,
            ChunkDelta::ReasoningText(text) if !text.is_empty() => events.reasoning(text).await,
            _ => true,
        };
        if !delivered {
            // Dropping the chunk stream cancels generation at the host.
            return Answer::Gone;
        }
    }
    drop(chunks);

    // Read the outcome after the last chunk, as the interface asks.
    match outcome.await {
        Ok(completion) => Answer::Done(completion),
        Err(e) => Answer::Failed(e),
    }
}

/// A failed model call, for a person to read.
pub(crate) fn describe_refused(e: &RequestError) -> String {
    format!("the model refused the request: {}", e.describe())
}

pub(crate) fn describe_failed(e: &GenerationError) -> String {
    format!("generation failed: {}", e.describe())
}

/// The assembled reply's text, from the completion rather than the deltas.
pub(crate) fn text_of(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|part| match &part.content {
            // A refusal is shown as the model's text, never as an answer.
            AssistantContent::Text(text) | AssistantContent::Refusal(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Backend defaults for everything but the effort the driver chose, and the
/// workload's tools. `last` offers none, so the model has to answer.
pub(crate) fn options(
    offered: &[ToolDef],
    last: bool,
    effort: Option<Effort>,
    trace_parent: Option<&str>,
) -> ChatOptions {
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
        parallel_tool_calls: None,
        thinking: None,
        effort,
        meta: RequestMeta {
            trace_parent: trace_parent.map(str::to_string),
            deadline_ms: None,
            cache_key: None,
            cache_points: vec![],
            labels: vec![],
        },
        on_unrepresentable: UnrepresentablePolicy::Adapt,
    }
}
