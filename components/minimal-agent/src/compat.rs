//! Conversions between the two versions of `cosmonic:agent` this agent uses.
//!
//! The model and the session are 0.3.0 (what Cosmonic Desktop serves today);
//! the tools and the `run` export are 0.4.0. The message types are the same
//! shape in both, so each conversion is a field-by-field copy.

use crate::{a3, a4};

use a3::inference_types as t3;
use a4::inference_types as t4;

fn attachment_down(a: t4::Attachment) -> t3::Attachment {
    t3::Attachment {
        media_type: a.media_type,
        source: match a.source {
            t4::AttachmentSource::Bytes(b) => t3::AttachmentSource::Bytes(b),
            t4::AttachmentSource::Url(u) => t3::AttachmentSource::Url(u),
        },
    }
}

fn attachment_up(a: t3::Attachment) -> t4::Attachment {
    t4::Attachment {
        media_type: a.media_type,
        source: match a.source {
            t3::AttachmentSource::Bytes(b) => t4::AttachmentSource::Bytes(b),
            t3::AttachmentSource::Url(u) => t4::AttachmentSource::Url(u),
        },
    }
}

/// The person's message, as the 0.3.0 model call takes it.
pub(crate) fn user_down(m: t4::UserMessage) -> t3::UserMessage {
    t3::UserMessage {
        content: m
            .content
            .into_iter()
            .map(|p| match p {
                t4::UserPart::Text(s) => t3::UserPart::Text(s),
                t4::UserPart::Attachment(a) => t3::UserPart::Attachment(attachment_down(a)),
            })
            .collect(),
        name: m.name,
    }
}

/// A model reply, as a `run` step carries it. Continuation data (`opaque`
/// and provider blocks) is left out: a driver ignores it.
pub(crate) fn assistant_up(m: &t3::AssistantMessage) -> t4::AssistantMessage {
    t4::AssistantMessage {
        content: m
            .content
            .iter()
            .filter_map(|p| {
                let content = match &p.content {
                    t3::AssistantContent::Text(s) => t4::AssistantContent::Text(s.clone()),
                    t3::AssistantContent::Reasoning(r) => {
                        t4::AssistantContent::Reasoning(t4::Reasoning {
                            summary: r.summary.clone(),
                        })
                    }
                    t3::AssistantContent::ToolUse(c) => t4::AssistantContent::ToolUse(call_up(c)),
                    t3::AssistantContent::Attachment(a) => {
                        t4::AssistantContent::Attachment(attachment_up(a.clone()))
                    }
                    t3::AssistantContent::Other(_) => return None,
                };
                Some(t4::AssistantPart {
                    content,
                    opaque: None,
                })
            })
            .collect(),
    }
}

pub(crate) fn call_up(c: &t3::ToolCall) -> t4::ToolCall {
    t4::ToolCall {
        id: c.id.clone(),
        name: c.name.clone(),
        arguments: c.arguments.clone(),
    }
}

/// Tool results, as the 0.3.0 model call takes them.
pub(crate) fn results_down(results: &[t4::ToolResult]) -> Vec<t3::ToolResult> {
    results
        .iter()
        .map(|r| t3::ToolResult {
            call_id: r.call_id.clone(),
            output: r
                .output
                .iter()
                .map(|o| match o {
                    t4::ToolOutput::Text(s) => t3::ToolOutput::Text(s.clone()),
                    t4::ToolOutput::Attachment(a) => {
                        t3::ToolOutput::Attachment(attachment_down(a.clone()))
                    }
                })
                .collect(),
            is_error: r.is_error,
        })
        .collect()
}

/// A tool the workload offers, as the 0.3.0 model call describes it.
pub(crate) fn tool_def_down(d: a4::tool_types::ToolDef) -> t3::ToolDef {
    t3::ToolDef {
        name: d.name,
        title: d.title,
        description: d.description,
        input_schema: d.input_schema,
        output_schema: d.output_schema,
    }
}

pub(crate) fn usage_up(u: &Usage) -> t4::Usage {
    t4::Usage {
        prompt_tokens: u.prompt,
        cached_prompt_tokens: u.cached_prompt,
        cache_write_tokens: u.cache_write,
        completion_tokens: u.completion,
        reasoning_tokens: u.reasoning,
    }
}

/// Token usage summed over a run's model calls. None means no call reported
/// that field.
#[derive(Clone, Copy, Default)]
pub(crate) struct Usage {
    pub(crate) prompt: Option<u32>,
    pub(crate) cached_prompt: Option<u32>,
    pub(crate) cache_write: Option<u32>,
    pub(crate) completion: Option<u32>,
    pub(crate) reasoning: Option<u32>,
}

impl Usage {
    pub(crate) fn add(&mut self, u: &t3::Usage) {
        fn sum(a: Option<u32>, b: Option<u32>) -> Option<u32> {
            match (a, b) {
                (None, None) => None,
                (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
            }
        }
        self.prompt = sum(self.prompt, u.prompt_tokens);
        self.cached_prompt = sum(self.cached_prompt, u.cached_prompt_tokens);
        self.cache_write = sum(self.cache_write, u.cache_write_tokens);
        self.completion = sum(self.completion, u.completion_tokens);
        self.reasoning = sum(self.reasoning, u.reasoning_tokens);
    }

    /// Prompt plus completion tokens, what `max-total-tokens` bounds.
    pub(crate) fn total(&self) -> u64 {
        u64::from(self.prompt.unwrap_or(0)) + u64::from(self.completion.unwrap_or(0))
    }
}

fn overflow_up(c: t3::ContextOverflow) -> t4::ContextOverflow {
    t4::ContextOverflow {
        prompt_tokens: c.prompt_tokens,
        context_length: c.context_length,
        detail: c.detail,
    }
}

fn rate_up(r: t3::RateLimit) -> t4::RateLimit {
    t4::RateLimit {
        retry_after_seconds: r.retry_after_seconds,
        detail: r.detail,
    }
}

/// A refused model call, in the run's typed error. `unrepresentable` carries
/// a list of adaptations whose detail this agent already describes, so it
/// travels as `other` with that description rather than as a second copy of
/// the adaptation types.
pub(crate) fn request_error_up(e: t3::RequestError) -> t4::RequestError {
    use crate::describe::Describe;
    match e {
        t3::RequestError::ContextLengthExceeded(c) => {
            t4::RequestError::ContextLengthExceeded(overflow_up(c))
        }
        t3::RequestError::InvalidOptions(d) => t4::RequestError::InvalidOptions(d),
        t3::RequestError::MessagesIncomplete(d) => t4::RequestError::MessagesIncomplete(d),
        e @ t3::RequestError::Unrepresentable(_) => t4::RequestError::Other(e.describe()),
        t3::RequestError::NotAuthenticated(d) => t4::RequestError::NotAuthenticated(d),
        t3::RequestError::RateLimited(r) => t4::RequestError::RateLimited(rate_up(r)),
        t3::RequestError::QuotaExhausted(q) => t4::RequestError::QuotaExhausted(t4::Quota {
            top_up_url: q.top_up_url,
            detail: q.detail,
        }),
        t3::RequestError::TemporarilyUnavailable(r) => {
            t4::RequestError::TemporarilyUnavailable(rate_up(r))
        }
        t3::RequestError::TooLarge(s) => t4::RequestError::TooLarge(t4::SizeLimit {
            limit: s.limit,
            what: s.what,
        }),
        t3::RequestError::Other(d) => t4::RequestError::Other(d),
    }
}

pub(crate) fn generation_error_up(e: t3::GenerationError) -> t4::GenerationError {
    match e {
        t3::GenerationError::RateLimited(r) => t4::GenerationError::RateLimited(rate_up(r)),
        t3::GenerationError::TemporarilyUnavailable(r) => {
            t4::GenerationError::TemporarilyUnavailable(rate_up(r))
        }
        t3::GenerationError::ContextLengthExceeded(c) => {
            t4::GenerationError::ContextLengthExceeded(overflow_up(c))
        }
        t3::GenerationError::Other(d) => t4::GenerationError::Other(d),
    }
}
