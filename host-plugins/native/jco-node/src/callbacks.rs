//! Calling back into the guest.
//!
//! Several `jco:node` interfaces are two-way: the guest registers a listener and the host calls
//! one of the guest's `*-callbacks` exports when something happens (a timer fires, a request
//! arrives). The contract forbids doing that while the guest is inside one of its own imports,
//! so the host functions only queue a [`Job`], and the embedder runs a [`Pump`] between its own
//! calls into the instance to deliver them:
//!
//! ```ignore
//! let mut pump = Pump::attach(&mut store, &instance)?;
//! while let Some(job) = pump.next().await {
//!     pump.deliver(&mut store, job).await?;
//! }
//! ```

use std::sync::Arc;

use tokio::sync::mpsc;
use wash_runtime::{
    engine::ctx::SharedCtx,
    wasmtime::{Store, component::Instance},
};

use crate::{
    plugin::{JcoNode, PLUGIN_ID},
    state::{NodeInstance, lock},
};

mod timers_guest {
    wash_runtime::wasmtime::component::bindgen!({
        path: "wit",
        world: "timers-guest",
        wasmtime_crate: wash_runtime::wasmtime,
        exports: { default: async },
    });
}

/// Something the host owes the guest a callback for.
#[derive(Debug)]
pub enum Job {
    /// A timer scheduled through `jco:node/timers` came due.
    Timer { callback: u32 },
}

/// Delivers queued [`Job`]s to one instance's callback exports.
pub struct Pump {
    node: Arc<NodeInstance>,
    queue: mpsc::UnboundedReceiver<Job>,
    instance: Instance,
    timers: Option<timers_guest::TimersGuest>,
}

impl Pump {
    /// Take the job queue of the instance running in `store`. An instance has one queue, so a
    /// second pump for the same store is an error.
    pub fn attach(store: &mut Store<SharedCtx>, instance: &Instance) -> anyhow::Result<Self> {
        let plugin = store.data().active_ctx.try_get_plugin::<JcoNode>(PLUGIN_ID)?;
        let node = plugin.attach(store.data_mut());
        let queue = lock(&node.job_queue)
            .take()
            .ok_or_else(|| anyhow::anyhow!("this instance's callbacks are already being delivered"))?;
        Ok(Self { node, queue, instance: *instance, timers: None })
    }

    /// The instance state this pump serves.
    pub fn node(&self) -> &Arc<NodeInstance> {
        &self.node
    }

    /// Wait for the next job. Pending forever while nothing is queued; `None` only once the
    /// instance's state is gone.
    pub async fn next(&mut self) -> Option<Job> {
        self.queue.recv().await
    }

    /// A job that is already waiting, without blocking.
    pub fn try_next(&mut self) -> Option<Job> {
        self.queue.try_recv().ok()
    }

    /// Call the guest export `job` is for. An error is a trap in the guest, after which the
    /// instance must not be entered again.
    pub async fn deliver(&mut self, store: &mut Store<SharedCtx>, job: Job) -> anyhow::Result<()> {
        match job {
            Job::Timer { callback } => {
                if self.timers.is_none() {
                    self.timers = Some(timers_guest::TimersGuest::new(&mut *store, &self.instance)?);
                }
                if let Some(guest) = &self.timers {
                    guest.jco_node_timers_callbacks().call_dispatch(&mut *store, callback).await?;
                }
            }
        }
        Ok(())
    }
}
