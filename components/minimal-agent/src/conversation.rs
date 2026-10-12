//! The conversation as stored in the session's state, its caps, and the
//! messages the model is given from it.

use crate::bindings::cosmonic::agent::inference_types::{
    AssistantContent, AssistantMessage, AssistantPart, Message, ToolCall, ToolOutput, ToolResult,
    UserMessage, UserPart,
};
use crate::wire::to_json;
use serde::{Deserialize, Serialize};

/// Tells the model what it is, so it does not offer to browse or run code.
const SYSTEM_PROMPT: &str = "You are a small agent running as a sandboxed WebAssembly \
    component. You have no network access of your own: you reach the outside world only \
    through the tools you are given, and you may have none. Use a tool when it helps, and \
    never claim a result a tool did not return. Answer concisely.";

/// The stored conversation is kept under both limits, oldest exchanges first
/// out, so it neither outgrows the session nor crowds the model's context.
const MAX_TURNS: usize = 40;
const MAX_STATE_BYTES: usize = 64 * 1024;

/// The conversation, as this agent stores it in the session's opaque state.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Conversation {
    pub(crate) turns: Vec<Turn>,
}

/// One stored message: a `user` or `assistant` message, with any tool calls
/// the assistant made, or the `tool` results that answered them. Tool turns
/// are kept so a later turn knows what the tools returned; attachments are
/// kept as a note, not bytes. A flat struct rather than an enum tagged by
/// `role`, because serde buffers tagged enums through a generic value type
/// that would add about 10 KB to the component.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Turn {
    pub(crate) role: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) calls: Vec<StoredCall>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) results: Vec<StoredResult>,
}

impl Turn {
    pub(crate) fn new(role: &str, text: String) -> Self {
        Self {
            role: role.into(),
            text,
            ..Self::default()
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct StoredCall {
    pub(crate) id: String,
    pub(crate) name: String,
    /// JSON text, as the model wrote it.
    pub(crate) arguments: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct StoredResult {
    pub(crate) call_id: String,
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) is_error: bool,
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
    pub(crate) fn trim(&mut self) {
        while (self.turns.len() > MAX_TURNS || to_json(self).len() > MAX_STATE_BYTES)
            && self.drop_oldest()
        {}
    }

    /// Drop the oldest half, for a model whose context the conversation
    /// overflowed. False if there was nothing to drop.
    pub(crate) fn halve(&mut self) -> bool {
        let before = self.turns.len();
        while self.turns.len() > before / 2 && self.drop_oldest() {}
        self.turns.len() < before
    }
}

/// The system prompt and the stored conversation, as model messages.
pub(crate) fn to_messages(conversation: &Conversation) -> Vec<Message> {
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

pub(crate) fn user_message(text: String) -> Message {
    Message::User(UserMessage {
        content: vec![UserPart::Text(text)],
        name: None,
    })
}
