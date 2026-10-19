//! The workload's tools: what the host offers, making a call the model asks
//! for, and its output as text.

use crate::agent::inference_types::{
    AssistantContent, AssistantMessage, Attachment, ToolCall, ToolDef, ToolOutput, ToolResult,
};
use crate::agent::tool_types::{CallError, ListError};
use crate::agent::tools;
use crate::conversation::StoredResult;
use crate::describe::Describe;
use crate::events::Events;
use crate::journal::{
    Journal, OP_KIND, OP_RESULT_KIND, Op, OpResult, STEP_KIND, Step, arguments_value, entry,
};

/// What the host offers, described to the model as the provider describes
/// it. Empty when the workload grants no tools; an error when listing failed,
/// which the run records before it carries on without tools.
pub(crate) async fn offered_tools() -> Result<Vec<ToolDef>, ListError> {
    Ok(tools::list()
        .await?
        .into_iter()
        .map(|t| ToolDef {
            name: t.name,
            title: t.title,
            description: t.description,
            input: t.input,
            output_schema: t.output_schema,
        })
        .collect())
}

/// How one tool call ended, for the run.
pub(crate) enum Called {
    Done(ToolResult),
    /// The journal moved on under this run; it stops before the next call.
    Superseded,
    /// The driver dropped the event stream after the call returned.
    Gone,
}

/// Make one tool call the model asked for, recorded around the dispatch: the
/// op before, so a restart knows it may have happened, and the result after.
pub(crate) async fn call_tool(
    events: &mut Events,
    journal: &mut Journal,
    call: &ToolCall,
    run: &str,
) -> Called {
    let op = Op {
        op: &call.id,
        target: "tool",
        name: &call.name,
        arguments: Some(arguments_value(&call.arguments)),
    };
    if journal.append(vec![entry(OP_KIND, &op)]).await.is_err() {
        return Called::Superseded;
    }

    let (output, is_error, outcome) = match tools::call(call.clone()).await {
        Ok(r) if r.is_error => (r.output, true, "failed"),
        Ok(r) => (r.output, false, "succeeded"),
        // Not made: the model gets the reason, so it can try again.
        Err(e @ CallError::NotDispatched(_)) => {
            (vec![ToolOutput::Text(e.describe())], true, "failed")
        }
        // Made, or perhaps made, and whether it took effect is not known.
        Err(e @ CallError::OutcomeUnknown(_)) => {
            (vec![ToolOutput::Text(e.describe())], true, "unresolved")
        }
    };
    let text = output_text(&output);
    let names = [call.name.as_str()];
    let response = Step {
        role: "tool",
        text: &text,
        blocks: &["toolResponse"],
        tools: &names,
        run: Some(run),
    };
    let entries = vec![
        entry(
            OP_RESULT_KIND,
            &OpResult {
                op: &call.id,
                outcome,
            },
        ),
        entry(STEP_KIND, &response),
    ];
    let seq = journal.record(entries).await;
    let result = ToolResult {
        call_id: call.id.clone(),
        output,
        is_error,
    };
    if !events.tools(vec![result.clone()], seq).await {
        return Called::Gone;
    }
    Called::Done(result)
}

/// The tool calls in a reply, in order.
pub(crate) fn tool_calls(message: &AssistantMessage) -> Vec<ToolCall> {
    message
        .content
        .iter()
        .filter_map(|part| match &part.content {
            AssistantContent::ToolUse(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

/// A tool's output as text: the model sees attachments themselves, the
/// journal and the stored conversation a note naming each one.
fn output_text(output: &[ToolOutput]) -> String {
    output
        .iter()
        .map(|o| match o {
            ToolOutput::Text(text) => text.clone(),
            ToolOutput::Attachment(Attachment { media_type, .. }) => {
                format!("[{media_type} attachment]")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn stored_result(result: &ToolResult) -> StoredResult {
    StoredResult {
        call_id: result.call_id.clone(),
        text: output_text(&result.output),
        is_error: result.is_error,
    }
}
