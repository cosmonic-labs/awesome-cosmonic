//! A run: open it in the journal (`accept`), run the tool loop (`work`),
//! then record how it ended (`conclude`). The typed `run` export (`run.rs`)
//! and `POST /task` (`wire.rs`) both drive it, each through its own `Sink`.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::a3::inference_types::{
    AssistantContent, AssistantMessage, GenerationError, Message, RequestError, UserMessage,
    UserPart,
};
use crate::a3::models;
use crate::a3::session::{self, Snapshot};
use crate::compat::results_down;
use crate::conversation::{Conversation, StoredCall, Turn, to_messages};
use crate::describe::Describe;
use crate::journal::{
    Journal, OP_KIND, OP_RESULT_KIND, Op, OpResult, OpenError, RUN_KIND, RUN_RESULT_KIND,
    RunEntry, RunResult, STEP_KIND, Step, entry, random_id, recover, step,
};
use crate::model::{Answer, MODEL_ALIAS, answer, describe_failed, describe_refused, options, text_of};
use crate::sink::{Counts, Sink};
use crate::tools::{Called, call_tool, offered_tools, stored_result, tool_calls};

/// Model calls per run when the driver sets no `max-steps`. The last one is
/// made without tools, so a run ends with an answer.
pub(crate) const DEFAULT_MAX_STEPS: u32 = 8;
/// The most model calls a driver may ask for.
pub(crate) const MAX_STEPS_CAP: u32 = 64;
/// The longest run id: the commit id `run:<run-id>` must fit the session's
/// 128-byte limit.
pub(crate) const MAX_RUN_ID_BYTES: usize = 124;

/// One run at a time in this instance. The driver holds the real lock; this
/// is the backstop against two drivers on one workload. Exact while the
/// workload keeps one warm instance (`poolSize: 1`); otherwise the session's
/// conditional commits stop the second run.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Held while a run is in progress; releases the lock when dropped.
pub(crate) struct Running(());

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

fn acquire() -> Option<Running> {
    RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .ok()
        .map(|_| Running(()))
}

/// What a driver asks for, already validated.
pub(crate) struct Request {
    pub(crate) run_id: String,
    /// The person's message, as the model is given it.
    pub(crate) message: UserMessage,
    /// Its text, for the journal and the stored conversation.
    pub(crate) text: String,
    pub(crate) instructions: Option<String>,
    pub(crate) schema: Option<String>,
    pub(crate) max_steps: u32,
    pub(crate) max_total_tokens: Option<u64>,
    /// Nanoseconds from acceptance.
    pub(crate) max_duration: Option<u64>,
    pub(crate) trace_parent: Option<String>,
}

/// The text parts of a message, joined. An attachment is named, not copied.
pub(crate) fn user_text(message: &UserMessage) -> String {
    message
        .content
        .iter()
        .map(|p| match p {
            UserPart::Text(t) => t.clone(),
            UserPart::Attachment(a) => format!("[{} attachment]", a.media_type),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Why a run did not start. Nothing was written.
pub(crate) enum StartError {
    Busy,
    Duplicate,
    Unavailable(String),
}

/// A run the journal has opened.
pub(crate) struct Accepted {
    conversation: Conversation,
    snapshot: Snapshot,
    journal: Journal,
    /// The monotonic clock at acceptance, in nanoseconds.
    started: u64,
    _running: Running,
}

fn now() -> u64 {
    wasip3::clocks::monotonic_clock::now()
}

/// Open the run: take the lock, read the session, close what a run cut off
/// by a restart left open, and commit the person's message with
/// `agent.run.v1` under the commit id `run:<run-id>`.
pub(crate) async fn accept(req: &Request) -> Result<Accepted, StartError> {
    let running = acquire().ok_or(StartError::Busy)?;
    let unavailable =
        |e: session::Error| StartError::Unavailable(format!("the session store failed: {}", e.describe()));
    let snapshot = session::current().await.map_err(unavailable)?;
    let conversation = snapshot
        .state
        .as_ref()
        .and_then(|s| serde_json::from_slice::<Conversation>(&s.value).ok())
        .unwrap_or_default();
    let mut entries = recover(&snapshot).await.map_err(unavailable)?;
    let run = Some(req.run_id.as_str());
    entries.push(entry(STEP_KIND, &step("user", &req.text, run)));
    entries.push(entry(RUN_KIND, &RunEntry { run: &req.run_id }));
    let mut journal = Journal::new(snapshot.end);
    match journal.open(entries, &req.run_id).await {
        Ok(()) => {}
        Err(OpenError::Duplicate) => return Err(StartError::Duplicate),
        // Another writer is in the session.
        Err(OpenError::Stale) => return Err(StartError::Busy),
        Err(OpenError::Store(e)) => return Err(unavailable(e)),
    }
    Ok(Accepted {
        conversation,
        snapshot,
        journal,
        started: now(),
        _running: running,
    })
}

/// Why a run that started ended without a reply.
pub(crate) enum Failure {
    Cancelled,
    NoModel(String),
    Refused(RequestError),
    Generation(GenerationError),
    NotRecorded(String),
    Superseded,
    OutputRejected(String),
}

impl Failure {
    /// For a person to read, and for the journal.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Cancelled => "the run was cancelled".into(),
            Self::NoModel(d) | Self::NotRecorded(d) => d.clone(),
            Self::Refused(e) => describe_refused(e),
            Self::Generation(e) => describe_failed(e),
            Self::Superseded => "another request is writing this session; retry".into(),
            Self::OutputRejected(d) => format!("the reply did not match the schema: {d}"),
        }
    }

    /// The `run-error` case, for `agent.run-result.v1`.
    fn case(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::NoModel(_) => "no-model",
            Self::Refused(_) => "model-refused",
            Self::Generation(_) => "model-failed",
            Self::NotRecorded(_) => "not-recorded",
            Self::Superseded => "superseded",
            Self::OutputRejected(_) => "output-rejected",
        }
    }
}

/// Why a run that ended with a reply ended.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Finish {
    Done,
    MaxSteps,
    MaxTotalTokens,
    MaxDuration,
}

impl Finish {
    fn name(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::MaxSteps => "max-steps",
            Self::MaxTotalTokens => "max-total-tokens",
            Self::MaxDuration => "max-duration",
        }
    }
}

/// A run that ended with a reply.
pub(crate) struct Finished {
    /// The reply's text.
    pub(crate) answer: String,
    /// The reply: the last model message, without tool calls.
    pub(crate) reply: AssistantMessage,
    /// The reply as JSON, when the driver set a schema.
    pub(crate) structured: Option<String>,
    /// This run's messages, to append to the stored conversation.
    turns: Vec<Turn>,
    pub(crate) counts: Counts,
    /// The messages in context once the reply is added.
    pub(crate) context_messages: usize,
    /// The concrete model that served the last call.
    pub(crate) resolved: String,
    pub(crate) finish: Finish,
}

/// Run the loop, then record how it ended. The sink sees the reply as a final
/// step before the outcome is recorded.
pub(crate) async fn run(
    sink: &mut impl Sink,
    req: &Request,
    mut acc: Accepted,
) -> Result<Finished, Failure> {
    let worked = work(sink, req, &mut acc).await;
    conclude(sink, req, acc, worked).await
}

/// The tool loop: call the model, make the tool calls it asks for, hand it
/// the results, and repeat until it answers without asking for more or a
/// limit ends the run.
async fn work(
    sink: &mut impl Sink,
    req: &Request,
    acc: &mut Accepted,
) -> Result<Finished, Failure> {
    let run = Some(req.run_id.as_str());
    let expired = |acc: &Accepted| {
        req.max_duration
            .is_some_and(|d| now().saturating_sub(acc.started) >= d)
    };
    // The host resolves the alias and checks this workload may use it.
    let model = models::open(MODEL_ALIAS.to_string())
        .await
        .map_err(|e| Failure::NoModel(format!("no model for the alias {MODEL_ALIAS}: {}", e.describe())))?;
    let offered = match offered_tools().await {
        Ok(tools) => tools,
        // Recorded for the person, and the run goes on without tools.
        Err(e) => {
            let text = format!("listing the tools failed, so this run has none: {}", e.describe());
            let seq = acc
                .journal
                .record(vec![entry(STEP_KIND, &step("error", &text, run))])
                .await;
            if !sink.error(&text, seq).await {
                return Err(Failure::Cancelled);
            }
            Vec::new()
        }
    };
    let instructions = instructions(req);

    // This run's messages as the model sees them, assistant messages exactly
    // as returned and tool results with their attachments, and as stored.
    let mut live = vec![Message::User(req.message.clone())];
    let mut turns = vec![Turn::new("user", req.text.clone())];
    let mut counts = Counts::default();
    let mut resolved = String::new();
    let mut context_messages = 0;
    let mut shortened = false;
    loop {
        if expired(acc) {
            // Out of time before the next call: end with what there is.
            return Ok(Finished {
                answer: String::new(),
                reply: AssistantMessage { content: vec![] },
                structured: None,
                turns,
                counts,
                context_messages,
                resolved,
                finish: Finish::MaxDuration,
            });
        }
        counts.steps += 1;
        let last = counts.steps >= req.max_steps;
        let mut messages = to_messages(&acc.conversation, instructions.as_deref());
        messages.extend(live.iter().cloned());
        context_messages = messages.len() + 1;

        let op = random_id();
        let call = Op {
            op: &op,
            target: "model",
            name: MODEL_ALIAS,
            arguments: None,
        };
        acc.journal
            .append(vec![entry(OP_KIND, &call)])
            .await
            .map_err(|()| Failure::Superseded)?;
        let opts = options(&offered, last, req.trace_parent.as_deref());
        let reply = answer(sink, &model, messages, opts).await;
        let outcome = reply.outcome();
        acc.journal
            .record(vec![entry(OP_RESULT_KIND, &OpResult { op: &op, outcome })])
            .await;
        let completion = match reply {
            Answer::Done(completion) => completion,
            // Too long for the model: forget the oldest half of the stored
            // conversation and try once more.
            Answer::Refused(RequestError::ContextLengthExceeded(_))
                if !shortened && acc.conversation.halve() =>
            {
                shortened = true;
                continue;
            }
            Answer::Refused(e) => return Err(Failure::Refused(e)),
            Answer::Failed(e) => return Err(Failure::Generation(e)),
            Answer::Gone => return Err(Failure::Cancelled),
        };
        counts.usage.add(&completion.usage);
        resolved = completion.model.clone();
        if !sink.counts(counts).await {
            return Err(Failure::Cancelled);
        }
        let text = text_of(&completion.message);
        let asked = tool_calls(&completion.message);
        let spent = req
            .max_total_tokens
            .is_some_and(|max| counts.usage.total() >= max);
        let late = expired(acc);
        // The last call offers no tools; a request made anyway is dropped,
        // as are requests past a token or time limit.
        if asked.is_empty() || last || spent || late {
            let finish = match () {
                () if asked.is_empty() => Finish::Done,
                () if spent => Finish::MaxTotalTokens,
                () if late => Finish::MaxDuration,
                () => Finish::MaxSteps,
            };
            turns.push(Turn::new("assistant", text.clone()));
            return Ok(Finished {
                answer: text,
                reply: without_tool_calls(completion.message),
                structured: None,
                turns,
                counts,
                context_messages,
                resolved,
                finish,
            });
        }

        let names: Vec<&str> = asked.iter().map(|c| c.name.as_str()).collect();
        let request = Step::assistant(&text, &names, run);
        let seq = acc.journal.record(vec![entry(STEP_KIND, &request)]).await;
        if !sink.assistant(&completion.message, seq).await {
            return Err(Failure::Cancelled);
        }

        let mut results = Vec::new();
        for call in &asked {
            counts.tool_calls += 1;
            match call_tool(sink, &mut acc.journal, call, run).await {
                Called::Done(result) => results.push(result),
                Called::Superseded => return Err(Failure::Superseded),
                Called::Gone => return Err(Failure::Cancelled),
            }
        }
        let calls = asked
            .into_iter()
            .map(|c| StoredCall {
                id: c.id,
                name: c.name,
                arguments: c.arguments,
            })
            .collect();
        turns.push(Turn {
            calls,
            ..Turn::new("assistant", text)
        });
        turns.push(Turn {
            results: results.iter().map(stored_result).collect(),
            ..Turn::new("tool", String::new())
        });
        live.push(Message::Assistant(completion.message));
        live.push(Message::Tool(results_down(&results)));
    }
}

/// The driver's instructions, and the schema the reply must satisfy.
fn instructions(req: &Request) -> Option<String> {
    let schema = req.schema.as_ref().map(|s| {
        format!(
            "Your final answer must be only a JSON value that satisfies this JSON Schema, with \
             no other text: {s}"
        )
    });
    match (req.instructions.clone(), schema) {
        (None, None) => None,
        (Some(i), None) => Some(i),
        (None, Some(s)) => Some(s),
        (Some(i), Some(s)) => Some(format!("{i}\n\n{s}")),
    }
}

fn without_tool_calls(mut message: AssistantMessage) -> AssistantMessage {
    message
        .content
        .retain(|p| !matches!(p.content, AssistantContent::ToolUse(_)));
    message
}

/// The reply as JSON. A fenced code block around it is taken off. Whether it
/// satisfies the schema is the driver's to check: this agent carries no JSON
/// Schema validator.
fn structured(answer: &str) -> Result<String, String> {
    let trimmed = answer.trim();
    let inner = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|s| s.strip_suffix("```"))
        .unwrap_or(trimmed)
        .trim();
    serde_json::from_str::<serde_json::Value>(inner)
        .map(|_| inner.to_string())
        .map_err(|e| format!("not JSON ({e})"))
}

/// Record how the run ended, before its outcome resolves, so a driver that
/// redraws from the journal when it arrives finds the run recorded.
async fn conclude(
    sink: &mut impl Sink,
    req: &Request,
    mut acc: Accepted,
    worked: Result<Finished, Failure>,
) -> Result<Finished, Failure> {
    let run = req.run_id.as_str();
    let worked = worked.and_then(|mut f| {
        if req.schema.is_some() {
            f.structured = Some(structured(&f.answer).map_err(Failure::OutputRejected)?);
        }
        Ok(f)
    });
    let mut finished = match worked {
        Ok(finished) => finished,
        Err(failure) => {
            let text = failure.describe();
            let outcome = match failure {
                Failure::Cancelled => "cancelled",
                _ => "failed",
            };
            let result = RunResult {
                run,
                outcome,
                finish: None,
                error: Some(failure.case()),
            };
            acc.journal
                .record(vec![
                    entry(STEP_KIND, &step("error", &text, Some(run))),
                    entry(RUN_RESULT_KIND, &result),
                ])
                .await;
            return Err(failure);
        }
    };

    // The finished message replaces the deltas shown so far. Sent whether or
    // not the reader is still there: the outcome is recorded either way.
    let _ = sink.assistant(&finished.reply, None).await;

    let mut next = std::mem::take(&mut acc.conversation);
    next.turns.extend(std::mem::take(&mut finished.turns));
    next.trim();
    let answer = step("assistant", &finished.answer, Some(run));
    let result = RunResult {
        run,
        outcome: "done",
        finish: Some(finished.finish.name()),
        error: None,
    };
    let entries = vec![entry(STEP_KIND, &answer), entry(RUN_RESULT_KIND, &result)];
    if let Some(note) = acc.journal.save(entries, &next, &acc.snapshot).await {
        return Err(Failure::NotRecorded(note));
    }
    Ok(finished)
}
