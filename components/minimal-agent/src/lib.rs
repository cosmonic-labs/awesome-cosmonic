//! A minimal sandboxed agent on `cosmonic:agent@0.3.0`.
//!
//! `POST /task` with `{"task": "..."}` runs one turn: the model the host
//! resolves for the alias `default` answers, calling the tools the workload
//! grants as often as it needs to, and the reply streams back as NDJSON. The
//! turn is recorded in the workload's session so the next request continues
//! the same conversation. `GET /history` returns the steps recorded so far.
//! Both follow the protocol the Cosmonic Desktop Agents view speaks.
//!
//! The component names no endpoint, holds no key and opens no connection: the
//! model, the credential, the tools and the session store all belong to the
//! host.
//!
//! This file is the HTTP export and its routes. The rest, by responsibility:
//!
//! - `turn`: a turn and its tool loop. Start reading here.
//! - `model`: one model call, streamed.
//! - `tools`: listing the workload's tools and making a call.
//! - `journal`: the session protocol's entries, conditional commits,
//!   recovery after a restart, and `/history`.
//! - `conversation`: the stored conversation and its caps.
//! - `wire`: request parsing, responses, and the NDJSON lines.
//! - `describe`: error messages a person can read.

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "minimal-agent",
        generate_all,
    });
}

mod conversation;
mod describe;
mod journal;
mod model;
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
            (Method::Post, "/task") => turn::run_task(request).await,
            _ => respond(404, "text/plain; charset=utf-8", USAGE.into()),
        })
    }
}

wasip3::http::service::export!(Agent);
