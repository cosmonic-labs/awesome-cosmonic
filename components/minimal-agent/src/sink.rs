//! Where a run's progress goes while it works: the typed `run` export's
//! event stream, or the NDJSON lines of a `POST /task` response. The run
//! loop (`turn`) writes to a `Sink` and does not know which.
//!
//! Every method returns false once the reader has gone away. For the typed
//! export that is a cancellation: the run stops its calls and ends
//! `cancelled`.

use crate::compat::{Usage, assistant_up, usage_up};
use crate::journal::Step;
use crate::wire::{Line, send};
use crate::{a3, a4};
use a4::run_types::{RunCounts, RunEvent, Step as RunStep, StepContent};
use wasip3::wit_bindgen::StreamWriter;

/// The run's counters so far.
#[derive(Clone, Copy, Default)]
pub(crate) struct Counts {
    pub(crate) steps: u32,
    pub(crate) tool_calls: u32,
    pub(crate) usage: Usage,
}

impl Counts {
    pub(crate) fn to_wit(self) -> RunCounts {
        RunCounts {
            steps: self.steps,
            tool_calls: self.tool_calls,
            usage: usage_up(&self.usage),
        }
    }
}

pub(crate) trait Sink {
    /// Reply text as the model produces it.
    async fn delta(&mut self, text: &str) -> bool;
    /// A whole model reply. `seq` is its journal entry's, when committed first.
    async fn assistant(&mut self, message: &a3::inference_types::AssistantMessage, seq: Option<u64>)
    -> bool;
    /// The results of the tool calls in the assistant step before it.
    async fn tools(
        &mut self,
        results: &[a4::inference_types::ToolResult],
        step: &Step<'_>,
        seq: Option<u64>,
    ) -> bool;
    /// An error the run recorded in the conversation.
    async fn error(&mut self, text: &str, seq: Option<u64>) -> bool;
    /// Cumulative counters, after each model call.
    async fn counts(&mut self, counts: Counts) -> bool;
}

/// The typed export's event stream.
pub(crate) struct Events(pub(crate) StreamWriter<RunEvent>);

impl Events {
    async fn put(&mut self, event: RunEvent) -> bool {
        self.0.write_one(event).await.is_none()
    }
}

impl Sink for Events {
    async fn delta(&mut self, text: &str) -> bool {
        self.put(RunEvent::TextDelta(text.to_string())).await
    }

    async fn assistant(
        &mut self,
        message: &a3::inference_types::AssistantMessage,
        seq: Option<u64>,
    ) -> bool {
        let content = StepContent::Assistant(assistant_up(message));
        self.put(RunEvent::Step(RunStep { content, seq })).await
    }

    async fn tools(
        &mut self,
        results: &[a4::inference_types::ToolResult],
        _step: &Step<'_>,
        seq: Option<u64>,
    ) -> bool {
        let content = StepContent::Tool(results.to_vec());
        self.put(RunEvent::Step(RunStep { content, seq })).await
    }

    async fn error(&mut self, text: &str, seq: Option<u64>) -> bool {
        let content = StepContent::Error(text.to_string());
        self.put(RunEvent::Step(RunStep { content, seq })).await
    }

    async fn counts(&mut self, counts: Counts) -> bool {
        self.put(RunEvent::Counts(counts.to_wit())).await
    }
}

/// A `POST /task` response body: today's NDJSON lines.
pub(crate) struct Lines(pub(crate) StreamWriter<u8>);

impl Sink for Lines {
    async fn delta(&mut self, text: &str) -> bool {
        send(&mut self.0, &Line::Delta { text }).await
    }

    async fn assistant(
        &mut self,
        message: &a3::inference_types::AssistantMessage,
        _seq: Option<u64>,
    ) -> bool {
        let text = crate::model::text_of(message);
        let names = crate::tools::call_names(message);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let step = Step::assistant(&text, &names, None);
        send(&mut self.0, &Line::Step(&step)).await
    }

    async fn tools(
        &mut self,
        _results: &[a4::inference_types::ToolResult],
        step: &Step<'_>,
        _seq: Option<u64>,
    ) -> bool {
        send(&mut self.0, &Line::Step(step)).await
    }

    async fn error(&mut self, text: &str, _seq: Option<u64>) -> bool {
        let step = crate::journal::step("error", text, None);
        send(&mut self.0, &Line::Step(&step)).await
    }

    async fn counts(&mut self, _counts: Counts) -> bool {
        // Not a line Desktop reads.
        true
    }
}
