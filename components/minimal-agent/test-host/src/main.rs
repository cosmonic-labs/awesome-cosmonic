//! Scripted end-to-end checks for minimal-agent's `cosmonic:agent/run`
//! export. Each run gets a fresh instance and only the host's session
//! survives, as on Cosmonic Desktop.
//!
//! cargo run -- ../target/wasm32-wasip2/release/minimal_agent.wasm
mod host;

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use tokio::sync::{mpsc, oneshot};
use wasmtime::component::{
    Component, FutureConsumer, Linker, ResourceTable, Source, StreamConsumer, StreamResult,
};
use wasmtime::{Config, Engine, Store, StoreContextMut};
use wasmtime_wasi::WasiCtxBuilder;

use host::run_types::{
    Finish, ReplyPart, RunError, RunEvent, RunOptions, RunOutcome, StartError, StepContent,
};
use host::types::{Effort, Message, UserMessage, UserPart};
use host::{MinimalAgent, Shared, State, lock};

/// Reads a run's events; with a limit, drops the stream after that many,
/// which is how a driver cancels a run.
struct EventSink {
    tx: mpsc::UnboundedSender<RunEvent>,
    limit: Option<usize>,
}

impl<D> StreamConsumer<D> for EventSink {
    type Item = RunEvent;
    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: StoreContextMut<D>,
        mut source: Source<'_, RunEvent>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let this = self.get_mut();
        while source.remaining(&mut store) > 0 {
            if this.limit == Some(0) {
                return Poll::Ready(Ok(StreamResult::Dropped));
            }
            let mut one = None;
            source.read(&mut store, &mut one)?;
            if let Some(ev) = one {
                let _ = this.tx.send(ev);
                this.limit = this.limit.map(|n| n.saturating_sub(1));
            }
        }
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

type Outcome = std::result::Result<RunOutcome, RunError>;
struct OutcomeSink(Arc<Mutex<Option<oneshot::Sender<Outcome>>>>);

impl<D> FutureConsumer<D> for OutcomeSink {
    type Item = Outcome;
    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        store: StoreContextMut<D>,
        mut source: Source<'_, Outcome>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<()>> {
        let mut one = None;
        source.read(store, &mut one)?;
        let tx = self.0.lock().ok().and_then(|mut tx| tx.take());
        if let (Some(v), Some(tx)) = (one, tx) {
            let _ = tx.send(v);
        }
        Poll::Ready(Ok(()))
    }
}

enum Ran {
    Started {
        events: Vec<RunEvent>,
        outcome: Outcome,
    },
    Refused(StartError),
}

/// How the driver asks for a run.
struct Ask<'a> {
    id: &'a str,
    text: &'a str,
    schema: Option<&'a str>,
    effort: Option<Effort>,
    /// Drop the event stream after this many events.
    limit: Option<usize>,
}

impl<'a> Ask<'a> {
    fn new(id: &'a str, text: &'a str) -> Self {
        Self {
            id,
            text,
            schema: None,
            effort: None,
            limit: None,
        }
    }
}

struct Fixture {
    engine: Engine,
    component: Component,
    linker: Linker<State>,
    host: Shared,
}

impl Fixture {
    fn new(path: &str) -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model_async(true);
        let engine = Engine::new(&config)?;
        let component = Component::from_file(&engine, path)
            .map_err(|e| anyhow::anyhow!("loading {path}: {e}"))?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        wasmtime_wasi::p3::add_to_linker(&mut linker)?;
        host::add_to_linker(&mut linker)?;
        Ok(Self {
            engine,
            component,
            linker,
            host: Shared::default(),
        })
    }

    fn host(&self) -> std::sync::MutexGuard<'_, host::Host> {
        lock(&self.host)
    }

    async fn run(&self, ask: Ask<'_>) -> Result<Ran> {
        let mut store = Store::new(
            &self.engine,
            State {
                table: ResourceTable::new(),
                wasi: WasiCtxBuilder::new().inherit_stderr().build(),
                host: self.host.clone(),
            },
        );
        let agent =
            MinimalAgent::instantiate_async(&mut store, &self.component, &self.linker).await?;
        let message = UserMessage {
            content: vec![UserPart::Text(ask.text.into())],
            name: None,
        };
        let options = RunOptions {
            run_id: ask.id.into(),
            instructions: Some("Be brief.".into()),
            schema: ask.schema.map(str::to_string),
            max_steps: Some(8),
            max_total_tokens: None,
            max_duration: Some(60_000_000_000),
            effort: ask.effort,
            trace_parent: None,
        };
        let limit = ask.limit;
        let ran = tokio::time::timeout(
            Duration::from_secs(60),
            store.run_concurrent(async |acc| -> Result<Ran> {
                let started = agent
                    .cosmonic_agent_run()
                    .call_run(acc, message, options)
                    .await?;
                let (events, outcome) = match started {
                    Ok(p) => p,
                    Err(e) => return Ok(Ran::Refused(e)),
                };
                let (tx, mut rx) = mpsc::unbounded_channel();
                let (otx, orx) = oneshot::channel();
                acc.with(|mut s| -> Result<()> {
                    events.pipe(&mut s, EventSink { tx, limit })?;
                    outcome.pipe(&mut s, OutcomeSink(Arc::new(Mutex::new(Some(otx)))))?;
                    Ok(())
                })?;
                let mut seen = Vec::new();
                while let Some(ev) = rx.recv().await {
                    seen.push(ev);
                }
                let outcome = orx
                    .await
                    .context("the outcome future closed without a value")?;
                Ok(Ran::Started {
                    events: seen,
                    outcome,
                })
            }),
        )
        .await
        .context("the run timed out")???;
        Ok(ran)
    }
}

fn started(ran: Ran) -> Result<(Vec<RunEvent>, RunOutcome)> {
    match ran {
        Ran::Started {
            events,
            outcome: Ok(outcome),
        } => Ok((events, outcome)),
        Ran::Started {
            outcome: Err(e), ..
        } => bail!("the run failed: {e:?}"),
        Ran::Refused(e) => bail!("the run did not start: {e:?}"),
    }
}

fn reply_text(outcome: &RunOutcome) -> String {
    outcome
        .reply
        .iter()
        .filter_map(|p| match p {
            ReplyPart::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

fn describe(ev: &RunEvent) -> String {
    match ev {
        RunEvent::TextDelta(t) => format!("text-delta {t:?}"),
        RunEvent::ReasoningDelta(t) => format!("reasoning-delta {t:?}"),
        RunEvent::Step(s) => {
            let what = match &s.content {
                StepContent::Assistant(m) => format!("assistant, {} parts", m.content.len()),
                StepContent::Tool(r) => format!("tool, {} results", r.len()),
                StepContent::Error(t) => format!("error {t:?}"),
                StepContent::User(_) | StepContent::Other(_) => "other".into(),
            };
            format!("step {what} (seq {:?})", s.seq)
        }
        RunEvent::Counts(c) => format!("counts steps={} tools={}", c.steps, c.tool_calls),
        RunEvent::Compacted(_) => "compacted".into(),
        RunEvent::Plan(_) => "plan".into(),
        RunEvent::Other(o) => format!("other {o}"),
    }
}

/// One run with a tool call: streamed deltas, the call through the host's
/// tools import, its output back to the model, and the journal around it.
async fn tool_turn(f: &Fixture) -> Result<()> {
    let (events, outcome) = started(f.run(Ask::new("r1", "echo hello")).await?)?;
    for ev in &events {
        println!("    {}", describe(ev));
    }
    ensure!(
        reply_text(&outcome) == "The tool said: hello",
        "reply: {:?}",
        outcome.reply
    );
    ensure!(matches!(outcome.finish, Finish::Done));
    ensure!(
        outcome.counts.steps == 2 && outcome.counts.tool_calls == 1,
        "counts {:?}",
        outcome.counts
    );
    ensure!(outcome.counts.usage.prompt_tokens == Some(200));
    ensure!(
        outcome
            .model
            .as_ref()
            .is_some_and(|m| m.alias == host::ALIAS && m.model == host::MODEL)
    );
    ensure!(
        events
            .iter()
            .any(|e| matches!(e, RunEvent::ReasoningDelta(t) if t == "Planning the call.")),
        "no reasoning delta"
    );
    ensure!(
        events
            .iter()
            .any(|e| matches!(e, RunEvent::TextDelta(t) if t == "Using the tool.")),
        "no text delta"
    );
    ensure!(
        events.iter().any(|e| matches!(e, RunEvent::Step(s)
            if s.seq.is_some()
                && matches!(&s.content, StepContent::Tool(r) if r.first().is_some_and(|r| r.call_id == "call-1")))),
        "no tool step with its journal seq"
    );
    ensure!(
        matches!(events.last(), Some(RunEvent::Step(s)) if matches!(s.content, StepContent::Assistant(_))),
        "the final reply is not the last event"
    );

    let host = f.host();
    ensure!(
        host.tool_calls.len() == 1 && host.tool_calls[0].name == "echo",
        "host tool calls: {:?}",
        host.tool_calls
    );
    ensure!(host.chats.len() == 2, "{} model calls", host.chats.len());
    let first = &host.chats[0];
    ensure!(
        first.tools == ["echo", "explode"],
        "tools offered: {:?}",
        first.tools
    );
    ensure!(
        matches!(&first.messages[0], Message::System(s) if s == "Be brief."),
        "the driver's instructions come first"
    );
    ensure!(
        matches!(host.chats[1].messages.last(), Some(Message::Tool(r)) if r[0].call_id == "call-1" && !r[0].is_error)
    );

    let kinds = host.kinds(0);
    println!("    journal: {}", kinds.join(" "));
    ensure!(
        kinds
            == [
                "agent.step.v1",
                "agent.run.v1",
                "agent.op.v1",
                "agent.op-result.v1",
                "agent.step.v1",
                "agent.op.v1",
                "agent.op-result.v1",
                "agent.step.v1",
                "agent.op.v1",
                "agent.op-result.v1",
                "agent.step.v1",
                "agent.run-result.v1",
            ],
        "journal kinds"
    );
    let user = host.entries[0].json();
    ensure!(
        user["role"] == "user" && user["text"] == "echo hello" && user["run"] == "r1",
        "{user}"
    );
    let tool_op = host.entries[5].json();
    ensure!(
        tool_op["op"] == "call-1" && tool_op["arguments"]["text"] == "hello",
        "{tool_op}"
    );
    let result = host.entries.last().context("empty journal")?.json();
    ensure!(
        result["run"] == "r1" && result["outcome"] == "done" && result["finish"] == "done",
        "{result}"
    );
    let roles: Vec<String> = host
        .turns()
        .iter()
        .map(|t| t["role"].as_str().unwrap_or("?").to_string())
        .collect();
    ensure!(
        roles == ["user", "assistant", "tool", "assistant"],
        "stored turns: {roles:?}"
    );
    Ok(())
}

/// The next run, on a fresh instance, continues from the stored conversation.
async fn second_turn(f: &Fixture) -> Result<()> {
    let ask = Ask {
        effort: Some(Effort::Low),
        ..Ask::new("r2", "and now plain text")
    };
    let (_, outcome) = started(f.run(ask).await?)?;
    ensure!(
        reply_text(&outcome) == "Reply to: and now plain text",
        "reply: {:?}",
        outcome.reply
    );
    let host = f.host();
    let last = host.chats.last().context("no model call")?;
    // Instructions and system prompt, the first run's four messages, and this
    // one.
    ensure!(
        last.messages.len() == 7,
        "the model saw {} messages",
        last.messages.len()
    );
    ensure!(
        matches!(last.effort, Some(Effort::Low)),
        "effort: {:?}",
        last.effort
    );
    ensure!(host.turns().len() == 6);
    Ok(())
}

/// A repeated run id is refused before anything is written.
async fn duplicate(f: &Fixture) -> Result<()> {
    let before = f.host().entries.len();
    match f.run(Ask::new("r1", "echo again")).await? {
        Ran::Refused(StartError::Duplicate) => {}
        Ran::Refused(e) => bail!("expected duplicate, got {e:?}"),
        Ran::Started { .. } => bail!("a repeated run id started"),
    }
    ensure!(
        f.host().entries.len() == before,
        "a duplicate wrote to the session"
    );
    Ok(())
}

/// A message with no text is invalid input, and nothing is written.
async fn invalid(f: &Fixture) -> Result<()> {
    let before = f.host().entries.len();
    match f.run(Ask::new("r-empty", "   ")).await? {
        Ran::Refused(StartError::InvalidInput(_)) => {}
        Ran::Refused(e) => bail!("expected invalid-input, got {e:?}"),
        Ran::Started { .. } => bail!("an empty message started a run"),
    }
    ensure!(f.host().entries.len() == before);
    Ok(())
}

/// A request the model refuses fails the run as model-refused, recorded.
async fn refused(f: &Fixture) -> Result<()> {
    let from = f.host().entries.len();
    match f.run(Ask::new("r3", "refuse please")).await? {
        Ran::Started {
            outcome: Err(RunError::ModelRefused(_)),
            ..
        } => {}
        Ran::Started { outcome, .. } => bail!("expected model-refused, got {outcome:?}"),
        Ran::Refused(e) => bail!("did not start: {e:?}"),
    }
    let host = f.host();
    let result = host.entries.last().context("empty journal")?.json();
    ensure!(
        result["outcome"] == "failed" && result["error"] == "model-refused",
        "{result}"
    );
    let steps = host
        .kinds(from)
        .iter()
        .filter(|k| **k == "agent.step.v1")
        .count();
    ensure!(steps == 2, "the user and error steps, got {steps}");
    Ok(())
}

/// A reply the model declines finishes `refusal`, keeps the refusal text,
/// and carries no structured answer even when a schema was asked for.
async fn declined(f: &Fixture) -> Result<()> {
    let ask = Ask {
        schema: Some(r#"{"type": "object"}"#),
        ..Ask::new("r4", "decline this")
    };
    let (_, outcome) = started(f.run(ask).await?)?;
    ensure!(
        matches!(outcome.finish, Finish::Refusal),
        "finish: {:?}",
        outcome.finish
    );
    ensure!(
        matches!(outcome.reply.as_slice(), [ReplyPart::Refusal(t)] if t == "I can't help with that."),
        "reply: {:?}",
        outcome.reply
    );
    ensure!(outcome.structured.is_none());
    let result = f.host().entries.last().context("empty journal")?.json();
    ensure!(result["finish"] == "refusal", "{result}");
    Ok(())
}

/// With a schema, the reply comes back as structured JSON.
async fn schema(f: &Fixture) -> Result<()> {
    let ask = Ask {
        schema: Some(r#"{"type": "object"}"#),
        ..Ask::new("r5", "json please")
    };
    let (_, outcome) = started(f.run(ask).await?)?;
    ensure!(
        outcome.structured.as_deref() == Some("{\"ok\": true}"),
        "structured: {:?}",
        outcome.structured
    );
    let first = f.host();
    let asked = first.chats.last().context("no model call")?;
    ensure!(
        matches!(&asked.messages[0], Message::System(s) if s.contains("JSON Schema")),
        "the schema is not in the instructions"
    );
    Ok(())
}

/// A reply that ends pause-turn is handed back and the model called again.
async fn paused(f: &Fixture) -> Result<()> {
    let (events, outcome) = started(f.run(Ask::new("r6", "pause for a moment")).await?)?;
    ensure!(
        reply_text(&outcome) == "Carrying on after the pause.",
        "reply: {:?}",
        outcome.reply
    );
    ensure!(
        outcome.counts.steps == 2,
        "two model calls: {:?}",
        outcome.counts
    );
    ensure!(matches!(outcome.finish, Finish::Done));
    let assistant_steps = events
        .iter()
        .filter(
            |e| matches!(e, RunEvent::Step(s) if matches!(s.content, StepContent::Assistant(_))),
        )
        .count();
    ensure!(
        assistant_steps == 2,
        "the paused reply and the final one, got {assistant_steps}"
    );
    let host = f.host();
    ensure!(
        matches!(
            host.chats.last().and_then(|c| c.messages.last()),
            Some(Message::Assistant(_))
        ),
        "the paused reply was not handed back"
    );
    Ok(())
}

/// Dropping the event stream cancels the run: it ends `cancelled`, recorded.
async fn cancelled(f: &Fixture) -> Result<()> {
    let ask = Ask {
        limit: Some(1),
        ..Ask::new("r7", "a long answer")
    };
    match f.run(ask).await? {
        Ran::Started {
            outcome: Err(RunError::Cancelled),
            ..
        } => {}
        Ran::Started { outcome, .. } => bail!("expected cancelled, got {outcome:?}"),
        Ran::Refused(e) => bail!("did not start: {e:?}"),
    }
    let host = f.host();
    let result = host.entries.last().context("empty journal")?.json();
    ensure!(
        result["run"] == "r7" && result["outcome"] == "cancelled",
        "{result}"
    );
    let op_result = host
        .entries
        .iter()
        .rev()
        .find(|e| e.kind == "agent.op-result.v1")
        .context("no op result")?
        .json();
    ensure!(op_result["outcome"] == "cancelled", "{op_result}");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "../target/wasm32-wasip2/release/minimal_agent.wasm".into());
    let f = Fixture::new(&path)?;
    let checks: [(&str, _); 9] = [
        ("a run with a tool call", tool_turn(&f).await),
        ("the next run continues", second_turn(&f).await),
        ("a repeated run id is a duplicate", duplicate(&f).await),
        ("an empty message is invalid input", invalid(&f).await),
        ("a refused request fails the run", refused(&f).await),
        ("a declined reply finishes refusal", declined(&f).await),
        ("a schema gives a structured reply", schema(&f).await),
        ("a paused reply is continued", paused(&f).await),
        ("dropping the events cancels", cancelled(&f).await),
    ];
    let mut failed = 0;
    for (name, result) in &checks {
        match result {
            Ok(()) => println!("ok   {name}"),
            Err(e) => {
                failed += 1;
                println!("FAIL {name}: {e:#}");
            }
        }
    }
    println!("{} passed, {failed} failed", checks.len() - failed);
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}
