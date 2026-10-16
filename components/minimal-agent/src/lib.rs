//! A minimal sandboxed agent on `cosmonic:agent`.
//!
//! A run takes a message: the model the host resolves for the alias `default`
//! answers, calling the tools the workload grants as often as it needs to,
//! and the reply streams back. The run is recorded in the workload's session
//! so the next one continues the same conversation.
//!
//! Two ways in. A host calls the typed `cosmonic:agent/run@0.4.0` export
//! (`run`), which streams events and resolves to an outcome. Hosts that do
//! not call it yet send `POST /task` with `{"task": "..."}`, which runs the
//! same loop and streams NDJSON lines; `GET /history` returns the steps
//! recorded so far. Both follow the protocol the Cosmonic Desktop Agents view
//! speaks.
//!
//! The component names no endpoint, holds no key and opens no connection: the
//! model, the credential, the tools and the session store all belong to the
//! host.
//!
//! This file is the exports. The rest, by responsibility:
//!
//! - `turn`: a run and its tool loop. Start reading here.
//! - `run`: the typed `run` export.
//! - `sink`: where a run's progress goes, events or NDJSON lines.
//! - `model`: one model call, streamed.
//! - `tools`: listing the workload's tools and making a call.
//! - `journal`: the session protocol's entries, conditional commits,
//!   recovery after a restart, and `/history`.
//! - `conversation`: the stored conversation and its caps.
//! - `compat`: the 0.3.0 and 0.4.0 message types, converted.
//! - `wire`: `POST /task`, request parsing, responses, and the NDJSON lines.
//! - `describe`: error messages a person can read.

// The generated export glue is unsafe by nature; the rest of the crate is not.
#[allow(unsafe_code)]
mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "minimal-agent",
        // `run` and its types are unstable in cosmonic:agent@0.4.0.
        features: ["agent-run"],
        generate_all,
    });
}

/// The interfaces Desktop serves today: model, chat and session.
pub(crate) use bindings::cosmonic::agent0_3_0 as a3;
/// The folded tools, and the `run` export's types.
pub(crate) use bindings::cosmonic::agent0_4_0 as a4;

mod compat;
mod conversation;
mod describe;
mod journal;
mod model;
mod run;
mod sink;
mod tools;
mod turn;
mod wire;

use wasip3::http::types::{ErrorCode, Method, Request, Response};
use wire::{USAGE, respond};

struct Agent;

impl wasip3::exports::http::handler::Guest for Agent {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let path = request.get_path_with_query().unwrap_or_default();
        Ok(match (request.get_method(), path.as_str()) {
            (Method::Get, "/history") => journal::history().await,
            (Method::Post, "/task") => wire::task(request).await,
            _ => respond(404, "text/plain; charset=utf-8", USAGE.into()),
        })
    }
}

wasip3::http::service::export!(Agent);
// The export glue the bindings generate is unsafe by nature, so it lives in
// a module of its own that allows it; the rest of the crate denies it.
#[allow(unsafe_code)]
mod export {
    use crate::run::Runner;
    crate::bindings::export!(Runner with_types_in crate::bindings);
}
