//! The conversation as stored in the session's state, its caps, and the
//! messages the model is given from it.

use crate::agent::inference_types::{
    AssistantContent, AssistantMessage, AssistantPart, Message, ToolCall, ToolOutput, ToolResult,
    UserMessage, UserPart,
};
use crate::journal::to_json;
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

/// The run's instructions, the system prompt and the stored conversation, as
/// model messages. A driver's instructions come first, ahead of the agent's
/// own.
pub(crate) fn to_messages(conversation: &Conversation, instructions: Option<&str>) -> Vec<Message> {
    let mut messages: Vec<Message> = instructions
        .map(|i| Message::System(i.to_string()))
        .into_iter()
        .collect();
    messages.push(Message::System(SYSTEM_PROMPT.into()));
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
                            citations: vec![],
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

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// `n` exchanges: a person's message and the reply to it.
    fn exchanges(n: usize) -> Conversation {
        let turns = (0..n)
            .flat_map(|i| {
                [
                    Turn::new("user", format!("question {i}")),
                    Turn::new("assistant", format!("answer {i}")),
                ]
            })
            .collect();
        Conversation { turns }
    }

    #[test]
    fn trim_drops_whole_exchanges_oldest_first() {
        let mut c = exchanges(30);
        c.trim();
        assert_eq!(c.turns.len(), MAX_TURNS);
        assert_eq!(c.turns[0].role, "user");
        assert_eq!(c.turns[0].text, "question 10");
    }

    #[test]
    fn halve_keeps_the_newest_half() {
        let mut c = exchanges(4);
        assert!(c.halve());
        assert_eq!(c.turns.len(), 4);
        assert_eq!(c.turns[0].text, "question 2");
        assert!(!Conversation::default().halve());
    }

    #[test]
    fn instructions_come_before_the_system_prompt() {
        let messages = to_messages(&exchanges(1), Some("Be brief."));
        assert_eq!(messages.len(), 4);
        assert!(matches!(&messages[0], Message::System(s) if s == "Be brief."));
        assert!(matches!(&messages[1], Message::System(s) if s == SYSTEM_PROMPT));
        assert!(matches!(&messages[2], Message::User(_)));
        assert!(matches!(&messages[3], Message::Assistant(_)));
    }
}
