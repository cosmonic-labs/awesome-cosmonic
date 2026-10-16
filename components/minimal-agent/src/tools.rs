//! The workload's tools: what the host offers, making a call the model asks
//! for, and its output as text. Tools are `cosmonic:agent/tools@0.4.0`.

use crate::a3::inference_types::{AssistantContent, AssistantMessage, ToolCall, ToolDef};
use crate::a4::inference_types::{Attachment, ToolOutput, ToolResult};
use crate::a4::tool_types::{CallError, ListError};
use crate::a4::tools;
use crate::compat::{call_up, tool_def_down};
use crate::conversation::StoredResult;
use crate::describe::Describe;
use crate::journal::{
    Journal, OP_KIND, OP_RESULT_KIND, Op, OpResult, STEP_KIND, Step, entry, json_value,
};
use crate::sink::Sink;

/// What the host offers, described to the model as the provider describes
/// it. Empty when the workload grants no tools; an error when listing failed,
/// which the run records and carries on without tools.
pub(crate) async fn offered_tools() -> Result<Vec<ToolDef>, ListError> {
    Ok(tools::list()
        .await?
        .into_iter()
        .map(tool_def_down)
        .collect())
}

/// How one tool call ended, for the run.
pub(crate) enum Called {
    Done(ToolResult),
    /// The journal moved on under this run; it stops before the next call.
    Superseded,
    /// The reader went away after the call returned.
    Gone,
}

/// Make one tool call the model asked for, recorded around the dispatch: the
/// op before, so a restart knows it may have happened, and the result after.
pub(crate) async fn call_tool(
    sink: &mut impl Sink,
    journal: &mut Journal,
    call: &ToolCall,
    run: Option<&str>,
) -> Called {
    let op = Op {
        op: &call.id,
        target: "tool",
        name: &call.name,
        arguments: Some(json_value(&call.arguments)),
    };
    if journal.append(vec![entry(OP_KIND, &op)]).await.is_err() {
        return Called::Superseded;
    }

    let (output, is_error, outcome) = match tools::call(call_up(call)).await {
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
        run,
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
    if !sink
        .tools(std::slice::from_ref(&result), &response, seq)
        .await
    {
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

/// The names of the tools a reply asks for.
pub(crate) fn call_names(message: &AssistantMessage) -> Vec<String> {
    tool_calls(message).into_iter().map(|c| c.name).collect()
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
