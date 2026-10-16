//! The HTTP side: `POST /task` as an adapter over a run, request parsing,
//! response bodies, and the NDJSON lines a `/task` response streams. Kept for
//! hosts that do not call the typed `run` export yet.

use crate::a3::inference_types::{UserMessage, UserPart};
use crate::a3::session;
use crate::describe::Describe;
use crate::journal::{Step, random_id};
use crate::model::MODEL_ALIAS;
use crate::sink::Lines;
use crate::turn::{self, DEFAULT_MAX_STEPS, Request as RunRequest, StartError};
use serde::{Deserialize, Serialize};
use wasip3::http::types::{ErrorCode, Fields, Request, Response};
use wasip3::wit_bindgen::{StreamResult, StreamWriter};

/// Required on `POST /task`. A browser cannot add a custom header to a
/// cross-origin request without a CORS preflight this agent never answers, so
/// a web page the person happens to visit cannot drive the session.
pub(crate) const SESSION_HEADER: &str = "x-cosmonic-agent-session";

pub(crate) const USAGE: &str = "POST /task with {\"task\": \"...\"}";

/// One line of the `/task` response.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum Line<'a> {
    /// A piece of reply text as it arrives.
    Delta { text: &'a str },
    /// A finished message. An assistant step replaces the deltas shown so far.
    Step(&'a Step<'a>),
    /// The turn is stored.
    Done {
        context_messages: usize,
        answer: &'a str,
        turns: u32,
        input_tokens: u32,
        output_tokens: u32,
        model: ModelInfo<'a>,
    },
    /// The turn failed.
    Failed { error: &'a str },
}

#[derive(Serialize)]
pub(crate) struct ModelInfo<'a> {
    pub(crate) alias: &'a str,
    pub(crate) provider: &'a str,
    pub(crate) resolved: Option<&'a str>,
}

/// The body of every error response: `{"error": "..."}`.
#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

/// `POST /task`'s body. Other fields are ignored.
#[derive(Deserialize)]
struct TaskRequest {
    task: String,
}

/// A `/task` turn's wall-clock bound, as the Desktop relay has it.
const TASK_DURATION_NS: u64 = 10 * 60 * 1_000_000_000;

/// `POST /task`: check the request, open a run, and stream its lines.
pub(crate) async fn task(request: Request) -> Response {
    if request.get_headers().get(SESSION_HEADER).is_empty() {
        return error_response(403, &format!("missing {SESSION_HEADER} header"));
    }
    let task = match read_body(request).await.ok().and_then(|b| parse_task(&b)) {
        Some(task) => task,
        None => return error_response(400, USAGE),
    };
    let req = RunRequest {
        run_id: random_id(),
        message: UserMessage {
            content: vec![UserPart::Text(task.clone())],
            name: None,
        },
        text: task,
        instructions: None,
        schema: None,
        max_steps: DEFAULT_MAX_STEPS,
        max_total_tokens: None,
        max_duration: Some(TASK_DURATION_NS),
        trace_parent: None,
    };
    let accepted = match turn::accept(&req).await {
        Ok(accepted) => accepted,
        Err(StartError::Busy) => {
            return error_response(409, "another request is writing this session; retry");
        }
        // A fresh id never repeats.
        Err(StartError::Duplicate) => return error_response(409, "this run already started"),
        Err(StartError::Unavailable(e)) => return error_response(500, &e),
    };

    // Return the response now and write its lines as the run happens.
    let (response, body) = streaming(200, "application/x-ndjson");
    wit_bindgen::spawn_local(async move {
        let mut lines = Lines(body);
        match turn::run(&mut lines, &req, accepted).await {
            Ok(f) => {
                let done = Line::Done {
                    context_messages: f.context_messages,
                    answer: &f.answer,
                    turns: f.counts.steps,
                    input_tokens: f.counts.usage.prompt.unwrap_or(0),
                    output_tokens: f.counts.usage.completion.unwrap_or(0),
                    model: ModelInfo {
                        alias: MODEL_ALIAS,
                        provider: "cosmonic",
                        resolved: Some(f.resolved.as_str()).filter(|m| !m.is_empty()),
                    },
                };
                send(&mut lines.0, &done).await;
            }
            Err(failure) => {
                let error = failure.describe();
                send(&mut lines.0, &Line::Failed { error: &error }).await;
            }
        }
    });
    response
}

/// Write one NDJSON line. False once the client has gone away.
pub(crate) async fn send(body: &mut StreamWriter<u8>, line: &Line<'_>) -> bool {
    let mut bytes = to_json(line);
    bytes.push(b'\n');
    body.write_all(bytes).await.is_empty()
}

/// Serializing this file's types cannot fail: they have no maps with
/// non-string keys and no custom serializers.
pub(crate) fn to_json(value: &impl Serialize) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_default()
}

pub(crate) fn parse_task(body: &[u8]) -> Option<String> {
    let request: TaskRequest = serde_json::from_slice(body).ok()?;
    let task = request.task.trim();
    (!task.is_empty()).then(|| task.to_string())
}

pub(crate) async fn read_body(request: Request) -> Result<Vec<u8>, ErrorCode> {
    let (result_tx, result_rx) = wasip3::wit_future::new(|| Ok(()));
    let (mut stream, trailers) = Request::consume_body(request, result_rx);
    drop(result_tx);
    let mut buf = Vec::with_capacity(4096);
    loop {
        buf.reserve(4096);
        let (status, returned) = stream.read(buf).await;
        buf = returned;
        if !matches!(status, StreamResult::Complete(_)) {
            break;
        }
    }
    trailers.await?;
    Ok(buf)
}

/// A response whose body is written after it is returned.
pub(crate) fn streaming(status: u16, content_type: &str) -> (Response, StreamWriter<u8>) {
    let headers = [("content-type".to_string(), content_type.as_bytes().to_vec())];
    let fields = Fields::from_list(&headers).unwrap_or_else(|_| Fields::new());
    let (body_tx, body_rx) = wasip3::wit_stream::new();
    let (trailers_tx, trailers_rx) = wasip3::wit_future::new(|| Ok(None));
    let (response, _sent) = Response::new(fields, Some(body_rx), trailers_rx);
    drop(trailers_tx);
    let _ = response.set_status_code(status);
    (response, body_tx)
}

pub(crate) fn store_failed(e: &session::Error) -> Response {
    error_response(500, &format!("the session store failed: {}", e.describe()))
}

pub(crate) fn error_response(status: u16, error: &str) -> Response {
    respond(status, "application/json", to_json(&ErrorBody { error }))
}

pub(crate) fn respond(status: u16, content_type: &str, bytes: Vec<u8>) -> Response {
    let (response, mut body) = streaming(status, content_type);
    wit_bindgen::spawn_local(async move {
        let _ = body.write_all(bytes).await;
    });
    response
}
