//! The typed `cosmonic:agent/run@0.4.0` export: check what the driver sent,
//! open the run, then work while the driver reads the events, and resolve
//! the outcome after the last commit.

use crate::agent::inference_types::{
    AssistantContent, AssistantMessage, AttachmentSource, UserMessage, UserPart,
};
use crate::agent::run_types::{
    CitedReplyText, Finish as WitFinish, ReplyPart, RunError, RunEvent, RunOptions, RunOutcome,
    ServedModel, StartError as WitStartError, StartedRun,
};
use crate::bindings::exports::cosmonic::agent::run::Guest;
use crate::bindings::{wit_future, wit_stream};
use crate::events::Events;
use crate::model::MODEL_ALIAS;
use crate::turn::{
    self, DEFAULT_MAX_STEPS, Failure, Finish, Finished, MAX_RUN_ID_BYTES, MAX_STEPS_CAP, Request,
    StartError,
};

pub(crate) struct Runner;

impl Guest for Runner {
    async fn run(message: UserMessage, options: RunOptions) -> Result<StartedRun, WitStartError> {
        let req = validate(message, options).map_err(WitStartError::InvalidInput)?;
        let accepted = turn::accept(&req).await.map_err(|e| match e {
            StartError::Busy => WitStartError::Busy,
            StartError::Duplicate => WitStartError::Duplicate,
            StartError::Unavailable(d) => WitStartError::Unavailable(d),
        })?;

        let (events_tx, events_rx) = wit_stream::new::<RunEvent>();
        // An outcome the agent never writes means it stopped without
        // finishing; a driver reads that as an interrupted run.
        let (outcome_tx, outcome_rx) = wit_future::new::<Result<RunOutcome, RunError>>(|| {
            Err(RunError::Failed(
                "the agent stopped before the run ended".into(),
            ))
        });
        // The run goes on after `run` returns; the driver follows it through
        // the stream and the future.
        wit_bindgen::spawn_local(async move {
            let mut events = Events(events_tx);
            let ended = turn::run(&mut events, &req, accepted).await;
            // The stream closes before the outcome resolves.
            drop(events);
            let _ = outcome_tx
                .write(ended.map(outcome).map_err(run_error))
                .await;
        });
        Ok((events_rx, outcome_rx))
    }
}

/// Check what the driver sent. Construction is lenient; this is where a
/// message or options the agent cannot take are refused, before anything is
/// written.
pub(crate) fn validate(message: UserMessage, options: RunOptions) -> Result<Request, String> {
    let run_id = options.run_id;
    if run_id.is_empty() || run_id.len() > MAX_RUN_ID_BYTES {
        return Err(format!("a run id is 1 to {MAX_RUN_ID_BYTES} bytes"));
    }
    if run_id.chars().any(char::is_control) {
        return Err("a run id has no control characters".into());
    }
    let has_text = message.content.iter().any(|p| match p {
        UserPart::Text(t) => !t.trim().is_empty(),
        UserPart::Attachment(_) => false,
    });
    if !has_text {
        return Err("the message has no text".into());
    }
    // A URL would have the host fetch on the agent's behalf, which this agent
    // does not ask for. Bytes and host-issued blobs are passed on as they are.
    if message.content.iter().any(
        |p| matches!(p, UserPart::Attachment(a) if matches!(a.source, AttachmentSource::Url(_))),
    ) {
        return Err("attachments are taken as bytes or blobs, not URLs".into());
    }
    let max_steps = match options.max_steps {
        None => DEFAULT_MAX_STEPS,
        Some(0) => return Err("max-steps is at least 1".into()),
        Some(n) => n.min(MAX_STEPS_CAP),
    };
    if let Some(schema) = &options.schema
        && serde_json::from_str::<serde_json::Value>(schema).is_err()
    {
        return Err("the schema is not JSON".into());
    }
    Ok(Request {
        text: turn::user_text(&message),
        run_id,
        message,
        instructions: options.instructions.filter(|i| !i.trim().is_empty()),
        schema: options.schema,
        max_steps,
        max_total_tokens: options.max_total_tokens,
        max_duration: options.max_duration,
        effort: options.effort,
        trace_parent: options.trace_parent,
    })
}

/// The final reply as the outcome carries it: every part a person sees, and
/// no tool use, reasoning or continuation data.
pub(crate) fn reply_parts(reply: AssistantMessage) -> Vec<ReplyPart> {
    reply
        .content
        .into_iter()
        .filter_map(|p| match p.content {
            AssistantContent::Text(text) if p.citations.is_empty() => Some(ReplyPart::Text(text)),
            AssistantContent::Text(text) => Some(ReplyPart::CitedText(CitedReplyText {
                text,
                citations: p.citations,
            })),
            AssistantContent::Refusal(text) => Some(ReplyPart::Refusal(text)),
            AssistantContent::Attachment(a) => Some(ReplyPart::Attachment(a)),
            AssistantContent::Other(block) => Some(ReplyPart::Opaque(block)),
            AssistantContent::ToolUse(_) | AssistantContent::Reasoning(_) => None,
        })
        .collect()
}

fn outcome(f: Finished) -> RunOutcome {
    RunOutcome {
        counts: f.counts.to_wit(),
        reply: reply_parts(f.reply),
        structured: f.structured,
        finish: match f.finish {
            Finish::Done => WitFinish::Done,
            Finish::MaxSteps => WitFinish::MaxSteps,
            Finish::MaxTotalTokens => WitFinish::MaxTotalTokens,
            Finish::MaxDuration => WitFinish::MaxDuration,
            Finish::Refusal => WitFinish::Refusal,
            Finish::ContentFilter => WitFinish::ContentFilter,
        },
        model: (!f.resolved.is_empty()).then(|| ServedModel {
            alias: MODEL_ALIAS.into(),
            model: f.resolved,
        }),
    }
}

fn run_error(f: Failure) -> RunError {
    match f {
        Failure::Cancelled => RunError::Cancelled,
        Failure::NoModel(d) => RunError::NoModel(d),
        Failure::Refused(e) => RunError::ModelRefused(e),
        Failure::Generation(e) => RunError::ModelFailed(e),
        Failure::NotRecorded(d) => RunError::NotRecorded(d),
        Failure::Superseded => RunError::Superseded,
        Failure::OutputRejected(d) => RunError::OutputRejected(d),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::agent::inference_types::{AssistantPart, Attachment, Citation, Reasoning};

    fn options(run_id: &str) -> RunOptions {
        RunOptions {
            run_id: run_id.into(),
            instructions: None,
            schema: None,
            max_steps: None,
            max_total_tokens: None,
            max_duration: None,
            effort: None,
            trace_parent: None,
        }
    }

    fn text(t: &str) -> UserMessage {
        UserMessage {
            content: vec![UserPart::Text(t.into())],
            name: None,
        }
    }

    fn with_attachment(source: AttachmentSource) -> UserMessage {
        let mut m = text("see this");
        m.content.push(UserPart::Attachment(Attachment {
            media_type: "image/png".into(),
            source,
            name: Some("photo.png".into()),
        }));
        m
    }

    #[test]
    fn a_plain_request_takes_the_defaults() {
        let req = validate(text("hello"), options("r1")).unwrap();
        assert_eq!(req.run_id, "r1");
        assert_eq!(req.text, "hello");
        assert_eq!(req.max_steps, DEFAULT_MAX_STEPS);
    }

    #[test]
    fn a_run_id_must_fit_the_commit_id() {
        assert!(validate(text("hi"), options("")).is_err());
        assert!(validate(text("hi"), options(&"x".repeat(MAX_RUN_ID_BYTES))).is_ok());
        assert!(validate(text("hi"), options(&"x".repeat(MAX_RUN_ID_BYTES + 1))).is_err());
        assert!(validate(text("hi"), options("a\nb")).is_err());
    }

    #[test]
    fn a_message_needs_text() {
        assert!(validate(text("  "), options("r1")).is_err());
    }

    #[test]
    fn attachments_are_bytes_or_blobs_not_urls() {
        let url = with_attachment(AttachmentSource::Url("https://example.com/a.png".into()));
        assert!(validate(url, options("r1")).is_err());
        let blob = with_attachment(AttachmentSource::Blob("blob-1".into()));
        assert!(validate(blob, options("r1")).is_ok());
        let bytes = with_attachment(AttachmentSource::Bytes(vec![0]));
        assert!(validate(bytes, options("r1")).is_ok());
    }

    #[test]
    fn max_steps_is_at_least_one_and_capped() {
        let mut o = options("r1");
        o.max_steps = Some(0);
        assert!(validate(text("hi"), o).is_err());
        let mut o = options("r1");
        o.max_steps = Some(1000);
        assert_eq!(validate(text("hi"), o).unwrap().max_steps, MAX_STEPS_CAP);
    }

    #[test]
    fn a_schema_must_be_json() {
        let mut o = options("r1");
        o.schema = Some("{not json".into());
        assert!(validate(text("hi"), o).is_err());
    }

    #[test]
    fn the_reply_keeps_what_a_person_sees() {
        let part = |content, citations| AssistantPart {
            content,
            opaque: Some("continuation".into()),
            citations,
        };
        let citation = Citation {
            url: Some("https://example.com".into()),
            title: None,
            cited_text: None,
            span: None,
        };
        let reply = AssistantMessage {
            content: vec![
                part(
                    AssistantContent::Reasoning(Reasoning {
                        summary: vec!["thinking".into()],
                    }),
                    vec![],
                ),
                part(AssistantContent::Text("plain".into()), vec![]),
                part(AssistantContent::Text("sourced".into()), vec![citation]),
                part(AssistantContent::Refusal("not that".into()), vec![]),
            ],
        };
        let parts = reply_parts(reply);
        assert_eq!(parts.len(), 3);
        assert!(matches!(&parts[0], ReplyPart::Text(t) if t == "plain"));
        assert!(
            matches!(&parts[1], ReplyPart::CitedText(c) if c.text == "sourced" && c.citations.len() == 1)
        );
        assert!(matches!(&parts[2], ReplyPart::Refusal(t) if t == "not that"));
    }
}
