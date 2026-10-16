//! The typed `cosmonic:agent/run@0.4.0` export: validate the request, open
//! the run, then work while the driver reads the events, and resolve the
//! outcome after the last commit.

use crate::a4::inference_types::{AttachmentSource, UserMessage, UserPart};
use crate::a4::run_types::{
    Finish as WitFinish, ReplyPart, RunError, RunEvent, RunOptions, RunOutcome, ServedModel,
    StartError as WitStartError, StartedRun,
};
use crate::bindings::exports::cosmonic::agent0_4_0::run::Guest;
use crate::bindings::{wit_future, wit_stream};
use crate::compat::{generation_error_up, request_error_up, user_down};
use crate::model::MODEL_ALIAS;
use crate::sink::Events;
use crate::turn::{
    self, DEFAULT_MAX_STEPS, Failure, Finish, Finished, MAX_RUN_ID_BYTES, MAX_STEPS_CAP, Request,
    StartError,
};

pub(crate) struct Runner;

impl Guest for Runner {
    async fn run(
        message: UserMessage,
        options: RunOptions,
    ) -> Result<StartedRun, WitStartError> {
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
            Err(RunError::Failed("the agent stopped before the run ended".into()))
        });
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
fn validate(message: UserMessage, options: RunOptions) -> Result<Request, String> {
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
    // The model call takes attachments by bytes; a URL would have the host
    // fetch on the agent's behalf, which this agent does not ask for.
    if message.content.iter().any(|p| {
        matches!(p, UserPart::Attachment(a) if matches!(a.source, AttachmentSource::Url(_)))
    }) {
        return Err("attachments are taken as bytes, not URLs".into());
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
    let message = user_down(message);
    Ok(Request {
        text: turn::user_text(&message),
        run_id,
        message,
        instructions: options.instructions.filter(|i| !i.trim().is_empty()),
        schema: options.schema,
        max_steps,
        max_total_tokens: options.max_total_tokens,
        max_duration: options.max_duration,
        trace_parent: options.trace_parent,
    })
}

fn outcome(f: Finished) -> RunOutcome {
    use crate::a3::inference_types::AssistantContent;
    let reply = f
        .reply
        .content
        .into_iter()
        .filter_map(|p| match p.content {
            AssistantContent::Text(t) => Some(ReplyPart::Text(t)),
            AssistantContent::Attachment(a) => {
                Some(ReplyPart::Attachment(crate::a4::inference_types::Attachment {
                    media_type: a.media_type,
                    source: match a.source {
                        crate::a3::inference_types::AttachmentSource::Bytes(b) => {
                            AttachmentSource::Bytes(b)
                        }
                        crate::a3::inference_types::AttachmentSource::Url(u) => {
                            AttachmentSource::Url(u)
                        }
                    },
                }))
            }
            _ => None,
        })
        .collect();
    RunOutcome {
        reply,
        structured: f.structured,
        counts: f.counts.to_wit(),
        finish: match f.finish {
            Finish::Done => WitFinish::Done,
            Finish::MaxSteps => WitFinish::MaxSteps,
            Finish::MaxTotalTokens => WitFinish::MaxTotalTokens,
            Finish::MaxDuration => WitFinish::MaxDuration,
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
        Failure::Refused(e) => RunError::ModelRefused(request_error_up(e)),
        Failure::Generation(e) => RunError::ModelFailed(generation_error_up(e)),
        Failure::NotRecorded(d) => RunError::NotRecorded(d),
        Failure::Superseded => RunError::Superseded,
        Failure::OutputRejected(d) => RunError::OutputRejected(d),
    }
}
