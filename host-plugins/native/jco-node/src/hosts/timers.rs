//! `jco:node/timers`: host-kept timers that fire into the guest's `timers-callbacks` export.
//!
//! A timer firing is not allowed to enter the guest from inside one of its own imports, so it
//! only queues a job; [`crate::callbacks::Pump`] makes the call between guest calls.

use std::{collections::HashMap, time::Duration};

use tokio::task::JoinHandle;
use wash_runtime::wasmtime;

use crate::{
    bindings::jco::node::timers::{Error, Host},
    callbacks::Job,
    plugin::NodeView,
    state::{Capability, lock},
};

#[derive(Default)]
pub(crate) struct Timers {
    next: u32,
    running: HashMap<u32, JoinHandle<()>>,
}

impl Drop for Timers {
    fn drop(&mut self) {
        for (_, timer) in self.running.drain() {
            timer.abort();
        }
    }
}

impl Host for NodeView<'_> {
    async fn schedule(&mut self, delay_ms: u32, repeat: bool, callback: u32) -> wasmtime::Result<Result<u32, Error>> {
        if !self.node.allows(Capability::Timers) {
            return Ok(Err(Error {
                name: "Error".into(),
                message: "node:timers host timers are not supported by this host".into(),
                code: Some("ERR_JCO_UNSUPPORTED_NODE_API".into()),
            }));
        }
        let jobs = self.node.jobs.clone();
        // Node clamps a delay below 1ms to 1ms; a zero-period interval would also spin.
        let delay = Duration::from_millis(u64::from(delay_ms.max(1)));
        let mut timers = lock(&self.node.timers);
        timers.next += 1;
        let id = timers.next;
        let node = std::sync::Arc::downgrade(&self.node);
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(delay).await;
                if !repeat {
                    // Forget the handle first, as Node deletes a one-shot before running it.
                    if let Some(node) = node.upgrade() {
                        lock(&node.timers).running.remove(&id);
                    }
                }
                if jobs.send(Job::Timer { callback }).is_err() || !repeat {
                    break;
                }
            }
        });
        timers.running.insert(id, task);
        Ok(Ok(id))
    }

    async fn cancel(&mut self, id: u32) -> wasmtime::Result<()> {
        if let Some(timer) = lock(&self.node.timers).running.remove(&id) {
            timer.abort();
        }
        Ok(())
    }

    async fn set_ref(&mut self, _id: u32, _referenced: bool) -> wasmtime::Result<()> {
        // Referencing only decides whether a timer keeps a Node process alive. The embedder
        // decides how long an instance lives here, so there is nothing to record.
        Ok(())
    }
}
