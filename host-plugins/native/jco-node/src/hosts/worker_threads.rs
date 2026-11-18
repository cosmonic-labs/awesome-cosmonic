//! `jco:node/worker-threads`: refused, with a reason, even when granted.
//!
//! A worker's body is JavaScript (a script path, or source with `eval: true`) that Jco's Node
//! provider hands to a real Node `Worker` on the host. This host has no JavaScript engine of its
//! own to run that script in, so `createWorker` fails as an unsupported API instead of starting
//! something that would not run the guest's code.

use wash_runtime::wasmtime::{self, component::Resource};

use crate::{
    bindings::jco::node::worker_threads::{Error, Host, HostWorker, Worker, WorkerInfo},
    hosts::denied_message,
    plugin::NodeView,
    state::Capability,
};

fn coded(code: String, message: String) -> Error {
    Error { name: "Error".into(), message, code: Some(code), errno: None, syscall: None }
}

impl Host for NodeView<'_> {
    async fn create_worker(
        &mut self,
        _filename: String,
        _options: String,
        _environment: String,
        _listener: u32,
    ) -> wasmtime::Result<Result<Resource<Worker>, Error>> {
        let capability = Capability::WorkerThreads;
        if !self.node.allows(capability) {
            return Ok(Err(coded(capability.denied_code(), denied_message(capability))));
        }
        Ok(Err(coded(
            "ERR_JCO_UNSUPPORTED_NODE_API".into(),
            "node:worker_threads Worker is not supported by this host: a worker runs JavaScript on the host, \
             and this host has no JavaScript engine outside the component"
                .into(),
        )))
    }
}

/// No worker can be created, so none of these is reachable with a live handle.
impl HostWorker for NodeView<'_> {
    async fn post_message(&mut self, _worker: Resource<Worker>, _value: String) -> wasmtime::Result<Result<(), Error>> {
        Err(wasmtime::Error::msg("no node:worker_threads worker exists"))
    }

    async fn terminate(&mut self, _worker: Resource<Worker>) -> wasmtime::Result<()> {
        Err(wasmtime::Error::msg("no node:worker_threads worker exists"))
    }

    async fn set_ref(&mut self, _worker: Resource<Worker>, _value: bool) -> wasmtime::Result<()> {
        Err(wasmtime::Error::msg("no node:worker_threads worker exists"))
    }

    async fn info(&mut self, _worker: Resource<Worker>) -> wasmtime::Result<WorkerInfo> {
        Err(wasmtime::Error::msg("no node:worker_threads worker exists"))
    }

    async fn drop(&mut self, _worker: Resource<Worker>) -> wasmtime::Result<()> {
        Ok(())
    }
}
