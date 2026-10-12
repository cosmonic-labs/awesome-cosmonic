//! The workload's tools: what the provider offers, making a call the model
//! asks for, and its output as text.

use crate::bindings::cosmonic::agent::inference_types::{
    AssistantContent, AssistantMessage, Attachment, ToolCall, ToolDef, ToolOutput, ToolResult,
};
use crate::bindings::cosmonic::agent::tools::{self, CallError};
use crate::conversation::StoredResult;
use crate::describe::Describe;
use crate::journal::{
    Journal, OP_KIND, OP_RESULT_KIND, Op, OpResult, STEP_KIND, Step, entry, json_value,
};
use crate::wire::{Line, send};
use wasip3::wit_bindgen::StreamWriter;

/// What the workload's tool provider offers, described to the model as the
/// provider describes it. None when the workload grants no tools.
pub(crate) async fn offered_tools() -> Vec<ToolDef> {
    tools::list()
        .await
        .into_iter()
        .map(|t| ToolDef {
            name: t.name,
            title: t.title,
            description: t.description,
            input_schema: t.input_schema,
            output_schema: t.output_schema,
        })
        .collect()
}

/// Make one tool call the model asked for, recorded around the dispatch: the
/// op before, so a restart knows it may have happened, and the result after.
pub(crate) async fn call_tool(
    body: &mut StreamWriter<u8>,
    journal: &mut Journal,
    call: &ToolCall,
) -> Result<ToolResult, String> {
    let op = Op {
        op: &call.id,
        target: "tool",
        name: &call.name,
        arguments: Some(json_value(&call.arguments)),
    };
    journal.append(vec![entry(OP_KIND, &op)]).await?;

    let (output, is_error, outcome) = match tools::call(call.clone()).await {
        Ok(r) if r.is_error => (r.output, true, "failed"),
        Ok(r) => (r.output, false, "succeeded"),
        // Cut off before it returned: it was made, and whether it took effect
        // is not known.
        Err(e @ (CallError::Cancelled | CallError::TimedOut)) => {
            (vec![ToolOutput::Text(e.describe())], true, "unresolved")
        }
        // Not dispatched. The model gets the reason, so it can try again.
        Err(e) => (vec![ToolOutput::Text(e.describe())], true, "failed"),
    };
    let text = output_text(&output);
    let names = [call.name.as_str()];
    let response = Step {
        role: "tool",
        text: &text,
        blocks: &["toolResponse"],
        tools: &names,
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
    journal.record(entries).await;
    if !send(body, &Line::Step(&response)).await {
        return Err("the client disconnected".into());
    }
    Ok(ToolResult {
        call_id: call.id.clone(),
        output,
        is_error,
    })
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
