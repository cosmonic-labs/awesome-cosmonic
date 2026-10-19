//! What a run streams to its driver while it works, and the counters it
//! reports.
//!
//! Every method returns false once the driver has dropped the stream. That is
//! a cancellation: the run stops its calls and ends `cancelled`.

use crate::agent::inference_types::{AssistantMessage, ToolResult, Usage};
use crate::agent::run_types::{RunCounts, RunEvent, Step, StepContent};
use wit_bindgen::StreamWriter;

/// The run's event stream.
pub(crate) struct Events(pub(crate) StreamWriter<RunEvent>);

impl Events {
    async fn put(&mut self, event: RunEvent) -> bool {
        self.0.write_one(event).await.is_none()
    }

    /// Reply text as the model produces it.
    pub(crate) async fn text(&mut self, text: String) -> bool {
        self.put(RunEvent::TextDelta(text)).await
    }

    /// Reasoning summary text as the model produces it.
    pub(crate) async fn reasoning(&mut self, text: String) -> bool {
        self.put(RunEvent::ReasoningDelta(text)).await
    }

    /// A whole model reply, which replaces the deltas shown so far. `seq` is
    /// its journal entry's, when it was committed first.
    pub(crate) async fn assistant(&mut self, message: AssistantMessage, seq: Option<u64>) -> bool {
        self.step(StepContent::Assistant(message), seq).await
    }

    /// The results of the tool calls in the assistant step before it.
    pub(crate) async fn tools(&mut self, results: Vec<ToolResult>, seq: Option<u64>) -> bool {
        self.step(StepContent::Tool(results), seq).await
    }

    /// An error the run recorded in the conversation.
    pub(crate) async fn error(&mut self, text: String, seq: Option<u64>) -> bool {
        self.step(StepContent::Error(text), seq).await
    }

    /// Cumulative counters, after each model call.
    pub(crate) async fn counts(&mut self, counts: &Counts) -> bool {
        self.put(RunEvent::Counts(counts.to_wit())).await
    }

    async fn step(&mut self, content: StepContent, seq: Option<u64>) -> bool {
        self.put(RunEvent::Step(Step { content, seq })).await
    }
}

/// The run's counters so far.
pub(crate) struct Counts {
    /// Model calls made.
    pub(crate) steps: u32,
    pub(crate) tool_calls: u32,
    /// Token usage summed over the model calls. None in a field means no call
    /// reported it.
    pub(crate) usage: Usage,
}

impl Default for Counts {
    fn default() -> Self {
        Self {
            steps: 0,
            tool_calls: 0,
            usage: Usage {
                prompt_tokens: None,
                cached_prompt_tokens: None,
                cache_write_tokens: None,
                completion_tokens: None,
                reasoning_tokens: None,
            },
        }
    }
}

impl Counts {
    pub(crate) fn add(&mut self, u: &Usage) {
        fn sum(a: Option<u32>, b: Option<u32>) -> Option<u32> {
            match (a, b) {
                (None, None) => None,
                (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
            }
        }
        let t = &mut self.usage;
        t.prompt_tokens = sum(t.prompt_tokens, u.prompt_tokens);
        t.cached_prompt_tokens = sum(t.cached_prompt_tokens, u.cached_prompt_tokens);
        t.cache_write_tokens = sum(t.cache_write_tokens, u.cache_write_tokens);
        t.completion_tokens = sum(t.completion_tokens, u.completion_tokens);
        t.reasoning_tokens = sum(t.reasoning_tokens, u.reasoning_tokens);
    }

    /// Prompt plus completion tokens, what `max-total-tokens` bounds.
    pub(crate) fn total_tokens(&self) -> u64 {
        u64::from(self.usage.prompt_tokens.unwrap_or(0))
            + u64::from(self.usage.completion_tokens.unwrap_or(0))
    }

    pub(crate) fn to_wit(&self) -> RunCounts {
        RunCounts {
            steps: self.steps,
            tool_calls: self.tool_calls,
            usage: self.usage,
        }
    }
}
