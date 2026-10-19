//! A scripted `cosmonic:agent@0.4.0` host for minimal-agent: a model that
//! answers from the conversation it is sent, an in-memory session that keeps
//! the conditional commit rules, and two tools. Adapted from the scripted
//! host of the slim Codex agent in cosmonic-labs/sandboxed-agents.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{mpsc, oneshot};
use wasmtime::component::{Accessor, HasSelf, Resource, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "wit-host",
    world: "minimal-agent",
    imports: { default: trappable },
    exports: { default: async },
    with: {
        "cosmonic:agent/inference-types@0.4.0.model": ModelHandle,
        "wasi:clocks": wasmtime_wasi::p3::bindings::clocks,
    },
    require_store_data_send: true,
});

pub use cosmonic::agent::{
    chat, inference_types as types, models, run_types, session, tool_types, tools,
};
use types::{
    AssistantContent, AssistantMessage, AssistantPart, Chunk, ChunkDelta, Completion, FinishReason,
    GenerationError, Grammar, GrammarSyntax, Message, RequestError, ToolArguments, ToolCall,
    ToolInput, Usage,
};

pub const ALIAS: &str = "default";
pub const MODEL: &str = "scripted-model";

pub struct ModelHandle;

#[derive(Clone, Debug)]
pub struct Entry {
    pub seq: u64,
    pub kind: String,
    pub data: String,
}

impl Entry {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.data).unwrap_or_default()
    }
}

#[derive(Clone)]
pub struct StoredState {
    pub value: Vec<u8>,
    pub version: String,
    pub through: Option<u64>,
}

/// One chat request as the scripted model received it.
#[derive(Clone, Debug)]
pub struct ChatRecord {
    pub messages: Vec<Message>,
    pub tools: Vec<String>,
    /// Each offered tool's input, in the order of `tools`.
    pub inputs: Vec<ToolInput>,
    pub effort: Option<types::Effort>,
}

/// Everything the host holds, shared by every instance: only this survives
/// from one run to the next, as on Desktop.
#[derive(Default)]
pub struct Host {
    pub entries: Vec<Entry>,
    pub state: Option<StoredState>,
    pub seq: u64,
    pub versions: u64,
    pub commits: HashMap<String, session::CommitResult>,
    pub chats: Vec<ChatRecord>,
    pub tool_calls: Vec<ToolCall>,
}

impl Host {
    pub fn end(&self) -> session::JournalEnd {
        match self.entries.last() {
            Some(entry) => session::JournalEnd::At(entry.seq),
            None => session::JournalEnd::Empty,
        }
    }

    fn snapshot(&self) -> session::Snapshot {
        session::Snapshot {
            state: self.state.as_ref().map(|s| session::State {
                value: s.value.clone(),
                version: s.version.clone(),
                through: match s.through {
                    Some(seq) => session::JournalEnd::At(seq),
                    None => session::JournalEnd::Empty,
                },
            }),
            end: self.end(),
        }
    }

    fn commit(
        &mut self,
        entries: Vec<session::NewEntry>,
        state: Option<Vec<u8>>,
        options: session::CommitOptions,
    ) -> Result<session::CommitResult, session::Error> {
        // A repeated commit id gets the original result, as the session
        // interface says.
        if let Some(id) = &options.commit_id
            && let Some(result) = self.commits.get(id)
        {
            return Ok(result.clone());
        }
        if state.is_some() && (options.require_state.is_none() || options.require_end.is_none()) {
            return Err(session::Error::InvalidArgument(
                "a state write requires both conditions".into(),
            ));
        }
        if let Some(end) = &options.require_end
            && !same_end(end, &self.end())
        {
            return Ok(session::CommitResult::Stale(self.snapshot()));
        }
        if let Some(condition) = &options.require_state {
            let current = self.state.as_ref().map(|s| s.version.as_str());
            let matches = match condition {
                session::StateCondition::Absent => current.is_none(),
                session::StateCondition::Version(v) => current == Some(v.as_str()),
            };
            if !matches {
                return Ok(session::CommitResult::Stale(self.snapshot()));
            }
        }
        for entry in entries {
            self.seq += 1;
            self.entries.push(Entry {
                seq: self.seq,
                kind: entry.kind,
                data: String::from_utf8_lossy(&entry.data).into_owned(),
            });
        }
        let mut version = None;
        if let Some(value) = state {
            self.versions += 1;
            let v = format!("v{}", self.versions);
            self.state = Some(StoredState {
                value,
                version: v.clone(),
                through: match self.end() {
                    session::JournalEnd::At(seq) => Some(seq),
                    session::JournalEnd::Empty => None,
                },
            });
            version = Some(v);
        }
        let result = session::CommitResult::Committed(session::Committed {
            end: self.end(),
            version,
        });
        if let Some(id) = options.commit_id {
            self.commits.insert(id, result.clone());
        }
        Ok(result)
    }

    /// The stored conversation's turns, as JSON.
    pub fn turns(&self) -> Vec<serde_json::Value> {
        let state: serde_json::Value = self
            .state
            .as_ref()
            .and_then(|s| serde_json::from_slice(&s.value).ok())
            .unwrap_or_default();
        state["turns"].as_array().cloned().unwrap_or_default()
    }

    /// The journal entry kinds from index `from` on.
    pub fn kinds(&self, from: usize) -> Vec<&str> {
        self.entries[from..]
            .iter()
            .map(|e| e.kind.as_str())
            .collect()
    }
}

fn same_end(a: &session::JournalEnd, b: &session::JournalEnd) -> bool {
    match (a, b) {
        (session::JournalEnd::Empty, session::JournalEnd::Empty) => true,
        (session::JournalEnd::At(x), session::JournalEnd::At(y)) => x == y,
        _ => false,
    }
}

pub type Shared = Arc<Mutex<Host>>;

pub fn lock(shared: &Shared) -> MutexGuard<'_, Host> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct State {
    pub table: ResourceTable,
    pub wasi: WasiCtx,
    pub host: Shared,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

pub fn add_to_linker(linker: &mut wasmtime::component::Linker<State>) -> wasmtime::Result<()> {
    types::add_to_linker::<_, HasSelf<_>>(linker, |s| s)?;
    models::add_to_linker::<_, HasSelf<_>>(linker, |s| s)?;
    chat::add_to_linker::<_, HasSelf<_>>(linker, |s| s)?;
    session::add_to_linker::<_, HasSelf<_>>(linker, |s| s)?;
    tool_types::add_to_linker::<_, HasSelf<_>>(linker, |s| s)?;
    tools::add_to_linker::<_, HasSelf<_>>(linker, |s| s)?;
    run_types::add_to_linker::<_, HasSelf<_>>(linker, |s| s)?;
    Ok(())
}

fn description() -> types::ModelDescription {
    types::ModelDescription {
        model: Some(MODEL.into()),
        supported_operations: types::SupportedOperations::CHAT,
        context_length: Some(8192),
        max_output_tokens: Some(1024),
        tools: Some(true),
        reasoning: Some(true),
        accepts: Some(vec![]),
        response_formats: None,
        supported_options: None,
    }
}

impl types::Host for State {}
impl run_types::Host for State {}
impl tool_types::Host for State {}

impl types::HostModel for State {
    fn drop(&mut self, rep: Resource<ModelHandle>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl<T: Send> types::HostModelWithStore<T> for HasSelf<State> {
    async fn describe(
        _store: &Accessor<T, Self>,
        _rep: Resource<ModelHandle>,
    ) -> wasmtime::Result<types::ModelDescription> {
        Ok(description())
    }
}

impl models::Host for State {}

impl<T: Send> models::HostWithStore<T> for HasSelf<State> {
    async fn open(
        store: &Accessor<T, Self>,
        alias: String,
    ) -> wasmtime::Result<Result<Resource<ModelHandle>, models::OpenError>> {
        if alias != ALIAS {
            return Ok(Err(models::OpenError::UnknownAlias(alias)));
        }
        store.with(|mut s| Ok(Ok(s.get().table.push(ModelHandle)?)))
    }

    async fn describe(
        _store: &Accessor<T, Self>,
        alias: String,
    ) -> wasmtime::Result<Result<types::ModelDescription, models::OpenError>> {
        if alias != ALIAS {
            return Ok(Err(models::OpenError::UnknownAlias(alias)));
        }
        Ok(Ok(description()))
    }
}

// ----- chat: the scripted model -----

struct MessageCollector {
    messages: Vec<Message>,
    out: Option<oneshot::Sender<Vec<Message>>>,
}

impl Drop for MessageCollector {
    fn drop(&mut self) {
        if let Some(tx) = self.out.take() {
            let _ = tx.send(std::mem::take(&mut self.messages));
        }
    }
}

impl<D> wasmtime::component::StreamConsumer<D> for MessageCollector {
    type Item = Message;

    fn poll_consume(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        mut store: wasmtime::StoreContextMut<D>,
        mut src: wasmtime::component::Source<Self::Item>,
        _finish: bool,
    ) -> std::task::Poll<wasmtime::Result<wasmtime::component::StreamResult>> {
        let this = self.get_mut();
        while src.remaining(&mut store) > 0 {
            let mut one = None;
            src.read(&mut store, &mut one)?;
            this.messages.extend(one);
        }
        std::task::Poll::Ready(Ok(wasmtime::component::StreamResult::Completed))
    }
}

struct DoneConsumer {
    out: Option<oneshot::Sender<Result<(), String>>>,
}

impl<D> wasmtime::component::FutureConsumer<D> for DoneConsumer {
    type Item = Result<(), String>;

    fn poll_consume(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        mut store: wasmtime::StoreContextMut<D>,
        mut src: wasmtime::component::Source<'_, Self::Item>,
        _finish: bool,
    ) -> std::task::Poll<wasmtime::Result<()>> {
        let mut one = None;
        src.read(&mut store, &mut one)?;
        if let Some(tx) = self.get_mut().out.take() {
            let _ = tx.send(one.unwrap_or(Ok(())));
        }
        std::task::Poll::Ready(Ok(()))
    }
}

struct ChunkProducer {
    chunks: mpsc::UnboundedReceiver<Chunk>,
}

impl<D: 'static> wasmtime::component::StreamProducer<D> for ChunkProducer {
    type Item = Chunk;
    type Buffer = Option<Chunk>;

    fn poll_produce<'a>(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        mut store: wasmtime::StoreContextMut<'a, D>,
        mut dst: wasmtime::component::Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> std::task::Poll<wasmtime::Result<wasmtime::component::StreamResult>> {
        use std::task::Poll;
        use wasmtime::component::StreamResult;
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }
        if dst.remaining(&mut store) == Some(0) {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        match self.get_mut().chunks.poll_recv(cx) {
            Poll::Ready(Some(chunk)) => {
                dst.set_buffer(Some(chunk));
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Ready(None) => Poll::Ready(Ok(StreamResult::Dropped)),
            Poll::Pending => Poll::Pending,
        }
    }
}

fn chunk(delta: ChunkDelta) -> Chunk {
    Chunk {
        delta,
        adaptations: Vec::new(),
        finish_reason: None,
        usage: None,
    }
}

fn part(content: AssistantContent) -> AssistantPart {
    AssistantPart {
        content,
        opaque: None,
        citations: Vec::new(),
    }
}

fn completion(parts: Vec<AssistantPart>, finish: FinishReason) -> Completion {
    Completion {
        finish_reason: finish,
        usage: Usage {
            prompt_tokens: Some(100),
            cached_prompt_tokens: None,
            cache_write_tokens: None,
            completion_tokens: Some(10),
            reasoning_tokens: None,
        },
        message: AssistantMessage { content: parts },
        adaptations: Vec::new(),
        model: MODEL.into(),
    }
}

enum Script {
    Refuse(RequestError),
    Reply {
        chunks: Vec<Chunk>,
        outcome: Result<Completion, GenerationError>,
    },
}

/// A reply streamed in two text chunks.
fn text_reply(reply: String, finish: FinishReason) -> Script {
    let (a, b) = reply.split_at(reply.len() / 2);
    Script::Reply {
        chunks: vec![
            chunk(ChunkDelta::Text(a.into())),
            chunk(ChunkDelta::Text(b.into())),
        ],
        outcome: Ok(completion(
            vec![part(AssistantContent::Text(reply.clone()))],
            finish,
        )),
    }
}

fn user_text(message: &Message) -> Option<String> {
    match message {
        Message::User(user) => Some(
            user.content
                .iter()
                .filter_map(|part| match part {
                    types::UserPart::Text(text) => Some(text.as_str()),
                    types::UserPart::Attachment(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

/// The scripted model. Decides from the last message:
/// - a tool result: report it back as text;
/// - its own paused reply: carry on and finish;
/// - "echo X": a reasoning delta, a text part, and a call to `echo`;
/// - "shout X": a call to the plain text `shout` with X as its text;
/// - "pause": a reply that ends `pause-turn`;
/// - "decline": a refusal part, ending `refusal`;
/// - "json": a JSON object in a fenced code block;
/// - "refuse": the request refused before it is accepted;
/// - anything else: a two-chunk text reply.
fn script(messages: &[Message]) -> Script {
    let last = messages.last();
    if let Some(Message::Tool(results)) = last {
        let text = results
            .iter()
            .flat_map(|r| r.output.iter())
            .filter_map(|o| match o {
                types::ToolOutput::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        return text_reply(format!("The tool said: {text}"), FinishReason::Stop);
    }
    if let Some(Message::Assistant(_)) = last {
        return text_reply("Carrying on after the pause.".into(), FinishReason::Stop);
    }
    let text = last.and_then(user_text).unwrap_or_default();
    if let Some(rest) = text.strip_prefix("echo ") {
        let call = ToolCall {
            id: "call-1".into(),
            name: "echo".into(),
            arguments: ToolArguments::Json(serde_json::json!({ "text": rest }).to_string()),
        };
        return Script::Reply {
            chunks: vec![
                chunk(ChunkDelta::ReasoningText("Planning the call.".into())),
                chunk(ChunkDelta::Text("Using the tool.".into())),
            ],
            outcome: Ok(completion(
                vec![
                    part(AssistantContent::Text("Using the tool.".into())),
                    part(AssistantContent::ToolUse(call)),
                ],
                FinishReason::ToolCalls,
            )),
        };
    }
    if let Some(rest) = text.strip_prefix("shout ") {
        let call = ToolCall {
            id: "call-shout".into(),
            name: "shout".into(),
            arguments: ToolArguments::Text(rest.into()),
        };
        return Script::Reply {
            chunks: vec![],
            outcome: Ok(completion(
                vec![part(AssistantContent::ToolUse(call))],
                FinishReason::ToolCalls,
            )),
        };
    }
    if text.starts_with("pause") {
        return text_reply("Working on it.".into(), FinishReason::PauseTurn);
    }
    if text.starts_with("decline") {
        return Script::Reply {
            chunks: vec![],
            outcome: Ok(completion(
                vec![part(AssistantContent::Refusal(
                    "I can't help with that.".into(),
                ))],
                FinishReason::Refusal,
            )),
        };
    }
    if text.starts_with("json") {
        return text_reply("```json\n{\"ok\": true}\n```".into(), FinishReason::Stop);
    }
    if text.starts_with("refuse") {
        return Script::Refuse(RequestError::RateLimited(types::RateLimit {
            retry_after_seconds: Some(1),
            detail: "scripted refusal".into(),
        }));
    }
    text_reply(format!("Reply to: {text}"), FinishReason::Stop)
}

impl chat::Host for State {}

impl<T: Send> chat::HostWithStore<T> for HasSelf<State> {
    async fn chat(
        store: &Accessor<T, Self>,
        _model: Resource<ModelHandle>,
        messages: wasmtime::component::StreamReader<Message>,
        messages_done: wasmtime::component::FutureReader<Result<(), String>>,
        options: types::ChatOptions,
    ) -> wasmtime::Result<
        Result<
            (
                wasmtime::component::StreamReader<Chunk>,
                wasmtime::component::FutureReader<Result<Completion, GenerationError>>,
            ),
            RequestError,
        >,
    > {
        let (messages_tx, messages_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();
        let host = store.with(|mut s| -> wasmtime::Result<_> {
            let host = Arc::clone(&s.get().host);
            messages.pipe(
                &mut s,
                MessageCollector {
                    messages: Vec::new(),
                    out: Some(messages_tx),
                },
            )?;
            messages_done.pipe(&mut s, DoneConsumer { out: Some(done_tx) })?;
            Ok(host)
        })?;
        let messages = messages_rx.await.unwrap_or_default();
        if let Ok(Err(why)) = done_rx.await {
            return Ok(Err(RequestError::MessagesIncomplete(why)));
        }
        lock(&host).chats.push(ChatRecord {
            messages: messages.clone(),
            tools: options.tools.iter().map(|t| t.name.clone()).collect(),
            inputs: options.tools.iter().map(|t| t.input.clone()).collect(),
            effort: options.effort,
        });
        let (chunks, outcome) = match script(&messages) {
            Script::Refuse(error) => return Ok(Err(error)),
            Script::Reply { chunks, outcome } => (chunks, outcome),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        for c in chunks {
            let _ = tx.send(c);
        }
        drop(tx);
        store.with(|mut s| {
            let chunks =
                wasmtime::component::StreamReader::new(&mut s, ChunkProducer { chunks: rx })?;
            let outcome = wasmtime::component::FutureReader::new(&mut s, async move {
                Ok::<_, wasmtime::Error>(outcome)
            })?;
            Ok(Ok((chunks, outcome)))
        })
    }
}

// ----- session -----

impl session::Host for State {}

impl<T: Send> session::HostWithStore<T> for HasSelf<State> {
    async fn current(
        store: &Accessor<T, Self>,
    ) -> wasmtime::Result<Result<session::Snapshot, session::Error>> {
        Ok(Ok(store.with(|mut s| lock(&s.get().host).snapshot())))
    }

    async fn commit(
        store: &Accessor<T, Self>,
        entries: Vec<session::NewEntry>,
        state: Option<Vec<u8>>,
        options: session::CommitOptions,
    ) -> wasmtime::Result<Result<session::CommitResult, session::Error>> {
        Ok(store.with(|mut s| lock(&s.get().host).commit(entries, state, options)))
    }

    async fn read(
        store: &Accessor<T, Self>,
        start: u64,
        max: u32,
    ) -> wasmtime::Result<Result<Vec<session::Entry>, session::Error>> {
        Ok(Ok(store.with(|mut s| {
            lock(&s.get().host)
                .entries
                .iter()
                .filter(|e| e.seq >= start)
                .take(max as usize)
                .map(|e| session::Entry {
                    seq: e.seq,
                    author: "agent".into(),
                    at_ms: 0,
                    kind: e.kind.clone(),
                    data: e.data.clone().into_bytes(),
                })
                .collect()
        })))
    }

    async fn follow(
        _store: &Accessor<T, Self>,
        _start: u64,
    ) -> wasmtime::Result<Result<wasmtime::component::StreamReader<session::Entry>, session::Error>>
    {
        Ok(Err(session::Error::Unavailable(
            "this host does not follow".into(),
        )))
    }

    async fn clear(store: &Accessor<T, Self>) -> wasmtime::Result<Result<(), session::Error>> {
        store.with(|mut s| {
            let mut host = lock(&s.get().host);
            host.entries.clear();
            host.state = None;
            host.commits.clear();
        });
        Ok(Ok(()))
    }
}

// ----- tools: echo, and one that always fails -----

impl tools::Host for State {}

fn tool_def(name: &str, description: &str, input: ToolInput) -> tools::ToolDef {
    tool_types::ToolDef {
        name: name.into(),
        title: None,
        description: description.into(),
        input,
        output_schema: None,
        annotations: tool_types::ToolAnnotations {
            read_only: Some(true),
            destructive: Some(false),
            idempotent: Some(true),
            open_world: Some(false),
        },
        version: None,
    }
}

impl<T: Send> tools::HostWithStore<T> for HasSelf<State> {
    async fn list(
        _store: &Accessor<T, Self>,
    ) -> wasmtime::Result<Result<Vec<tools::ToolDef>, tools::ListError>> {
        Ok(Ok(vec![
            tool_def(
                "echo",
                "Echo the text back",
                ToolInput::JsonSchema(
                    r#"{"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}"#.into(),
                ),
            ),
            tool_def(
                "explode",
                "Always fails",
                ToolInput::JsonSchema(r#"{"type": "object"}"#.into()),
            ),
            tool_def("shout", "Say the text louder", ToolInput::Text),
            tool_def(
                "count",
                "Count to a number",
                ToolInput::Grammar(Grammar {
                    syntax: GrammarSyntax::Regex,
                    definition: "[0-9]+".into(),
                }),
            ),
        ]))
    }

    async fn call(
        store: &Accessor<T, Self>,
        request: ToolCall,
    ) -> wasmtime::Result<Result<tools::ToolResponse, tools::CallError>> {
        store.with(|mut s| lock(&s.get().host).tool_calls.push(request.clone()));
        let (text, is_error) = match (request.name.as_str(), &request.arguments) {
            ("echo", ToolArguments::Json(json)) => {
                let arguments: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
                (
                    arguments["text"].as_str().unwrap_or_default().to_string(),
                    false,
                )
            }
            ("shout", ToolArguments::Text(text)) => (text.to_uppercase(), false),
            ("echo" | "shout", other) => {
                return Ok(Err(tool_types::CallError::NotDispatched(
                    tool_types::DispatchError::InvalidArguments(format!(
                        "wrong kind of arguments: {other:?}"
                    )),
                )));
            }
            ("explode", _) => ("the tool exploded".to_string(), true),
            (other, _) => {
                return Ok(Err(tool_types::CallError::NotDispatched(
                    tool_types::DispatchError::UnknownTool(other.to_string()),
                )));
            }
        };
        Ok(Ok(tool_types::ToolResponse {
            output: vec![types::ToolOutput::Text(text)],
            structured: None,
            is_error,
        }))
    }
}
