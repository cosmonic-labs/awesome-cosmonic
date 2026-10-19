//! A minimal sandboxed agent on `cosmonic:agent@0.4.0`.
//!
//! A host hands the agent a run through the typed `cosmonic:agent/run`
//! export: one message from the person. The model the host resolves for the
//! alias `default` answers, calling the tools the workload grants as often as
//! it needs to, and the run streams its progress as events, then resolves to
//! an outcome. Everything is recorded in the workload's session, so the next
//! run continues the same conversation.
//!
//! The component names no endpoint, holds no key, opens no connection and
//! serves no HTTP: the model, the credential, the tools and the session store
//! all belong to the host, and only the host can start a run.
//!
//! This file is the bindings and the export. The rest, by responsibility:
//!
//! - `turn`: a run and its tool loop. Start reading here.
//! - `run`: the `run` export: checking what the driver sent, and the outcome.
//! - `events`: what a run streams while it works, and its counters.
//! - `model`: one model call, streamed.
//! - `tools`: listing the workload's tools and making a call.
//! - `journal`: the session protocol's entries, conditional commits, and
//!   recovery after a restart.
//! - `conversation`: the stored conversation and its caps.
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

    use crate::run::Runner;
    export!(Runner);
}

pub(crate) use bindings::cosmonic::agent;

mod conversation;
mod describe;
mod events;
mod journal;
mod model;
mod run;
mod tools;
mod turn;
