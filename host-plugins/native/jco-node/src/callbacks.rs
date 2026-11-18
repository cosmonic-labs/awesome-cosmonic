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
//!
//! A guest answer that is not ready yet (an HTTP response still being computed, a body chunk not
//! yet written) is polled again by a later job rather than waited for, so one slow handler never
//! holds up the instance's other callbacks.

use std::{collections::HashMap, sync::Arc, time::Duration};

use tokio::sync::{mpsc, oneshot};
use wash_runtime::{
    engine::ctx::SharedCtx,
    wasmtime::{
        Store,
        component::{Instance, ResourceAny},
    },
};

use crate::{
    hosts::http::{HttpConnection, IncomingRequest, OutgoingResponse, Reply},
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

mod http_guest {
    wash_runtime::wasmtime::component::bindgen!({
        path: "wit",
        world: "http-guest",
        wasmtime_crate: wash_runtime::wasmtime,
        exports: { default: async },
        with: { "jco:node/http": crate::bindings::jco::node::http },
    });
}

use http_guest::exports::jco::node::http_callbacks as http_callbacks;

/// How long to wait before asking the guest again about an answer it has not finished.
const REPOLL: Duration = Duration::from_millis(1);

/// Something the host owes the guest a callback for.
pub enum Job {
    /// A timer scheduled through `jco:node/timers` came due.
    Timer { callback: u32 },
    /// A request for the listener a server was created with.
    HttpRequest(Box<IncomingRequest>),
    /// Ask again whether a response is ready.
    HttpPoll { token: u64 },
    /// Ask again for the next piece of a streamed response body.
    HttpBody { token: u64 },
    /// A server closed: release the listener it redeemed.
    HttpRetire { listener: u32 },
}

struct PendingResponse {
    pending: ResourceAny,
    reply: oneshot::Sender<Reply>,
}

struct PendingBody {
    body: ResourceAny,
    sink: mpsc::UnboundedSender<Option<Vec<u8>>>,
}

/// Delivers queued [`Job`]s to one instance's callback exports.
pub struct Pump {
    node: Arc<NodeInstance>,
    queue: mpsc::UnboundedReceiver<Job>,
    instance: Instance,
    timers: Option<timers_guest::TimersGuest>,
    http: Option<Arc<http_guest::HttpGuest>>,
    listeners: HashMap<u32, ResourceAny>,
    responses: HashMap<u64, PendingResponse>,
    bodies: HashMap<u64, PendingBody>,
    next_token: u64,
}

fn reply_error(error: &http_callbacks::Error) -> String {
    error.message.clone()
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
        Ok(Self {
            node,
            queue,
            instance: *instance,
            timers: None,
            http: None,
            listeners: HashMap::new(),
            responses: HashMap::new(),
            bodies: HashMap::new(),
            next_token: 0,
        })
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

    /// Queue `job` again after `delay`.
    fn later(&self, job: Job, delay: Duration) {
        let jobs = self.node.jobs.clone();
        if delay.is_zero() {
            let _ = jobs.send(job);
            return;
        }
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = jobs.send(job);
        });
    }

    fn token(&mut self) -> u64 {
        self.next_token += 1;
        self.next_token
    }

    fn http_guest(&mut self, store: &mut Store<SharedCtx>) -> anyhow::Result<Arc<http_guest::HttpGuest>> {
        if self.http.is_none() {
            self.http = Some(Arc::new(http_guest::HttpGuest::new(&mut *store, &self.instance)?));
        }
        Ok(self.http.clone().expect("set above"))
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
            Job::HttpRequest(request) => self.http_request(store, *request).await?,
            Job::HttpPoll { token } => {
                if let Some(PendingResponse { pending, reply }) = self.responses.remove(&token) {
                    self.poll_response(store, pending, reply).await?;
                }
            }
            Job::HttpBody { token } => self.poll_body(store, token).await?,
            Job::HttpRetire { listener } => {
                if let Some(listener) = self.listeners.remove(&listener) {
                    listener.resource_drop_async(&mut *store).await?;
                }
            }
        }
        Ok(())
    }

    async fn http_request(&mut self, store: &mut Store<SharedCtx>, request: IncomingRequest) -> anyhow::Result<()> {
        let guest = self.http_guest(store)?;
        let callbacks = guest.jco_node_http_callbacks();
        let listener = match self.listeners.get(&request.listener) {
            Some(listener) => *listener,
            None => match callbacks.call_take_request_listener(&mut *store, request.listener).await? {
                Some(listener) => {
                    self.listeners.insert(request.listener, listener);
                    listener
                }
                None => {
                    let _ = request.reply.send(Err("Callback registration is not active".into()));
                    return Ok(());
                }
            },
        };
        let connection =
            store.data_mut().table.push(HttpConnection { shared: request.connection.clone(), owned: false })?;
        let incoming = http_callbacks::IncomingRequest {
            method: request.method,
            url: request.url,
            http_version: request.http_version,
            headers: request
                .headers
                .into_iter()
                .map(|(name, value)| http_callbacks::Header { name, value })
                .collect(),
            body: request.body,
            remote_address: request.remote.map(|remote| remote.ip().to_string()),
            remote_port: request.remote.map(|remote| remote.port()),
            connection: Some(connection),
        };
        let pending = callbacks.request_listener().call_start(&mut *store, listener, &incoming).await?;
        self.poll_response(store, pending, request.reply).await
    }

    async fn poll_response(
        &mut self,
        store: &mut Store<SharedCtx>,
        pending: ResourceAny,
        reply: oneshot::Sender<Reply>,
    ) -> anyhow::Result<()> {
        if reply.is_closed() {
            // The client went away before the guest answered.
            pending.resource_drop_async(&mut *store).await?;
            return Ok(());
        }
        let guest = self.http_guest(store)?;
        let polled = guest.jco_node_http_callbacks().pending_response().call_poll(&mut *store, pending).await?;
        match polled {
            Ok(None) => {
                let token = self.token();
                self.responses.insert(token, PendingResponse { pending, reply });
                self.later(Job::HttpPoll { token }, REPOLL);
            }
            Ok(Some(response)) => {
                pending.resource_drop_async(&mut *store).await?;
                let stream = match response.body_stream {
                    Some(body) => {
                        let (sink, chunks) = mpsc::unbounded_channel();
                        let token = self.token();
                        self.bodies.insert(token, PendingBody { body, sink });
                        self.later(Job::HttpBody { token }, Duration::ZERO);
                        Some(chunks)
                    }
                    None => None,
                };
                let _ = reply.send(Ok(OutgoingResponse {
                    status: response.status_code,
                    message: response.status_message,
                    headers: response.headers.into_iter().map(|header| (header.name, header.value)).collect(),
                    body: response.body,
                    stream,
                }));
            }
            Err(error) => {
                pending.resource_drop_async(&mut *store).await?;
                let _ = reply.send(Err(reply_error(&error)));
            }
        }
        Ok(())
    }

    async fn poll_body(&mut self, store: &mut Store<SharedCtx>, token: u64) -> anyhow::Result<()> {
        let Some(PendingBody { body, sink }) = self.bodies.remove(&token) else { return Ok(()) };
        if sink.is_closed() {
            body.resource_drop_async(&mut *store).await?;
            return Ok(());
        }
        let guest = self.http_guest(store)?;
        match guest.jco_node_http_callbacks().response_body().call_poll(&mut *store, body).await? {
            Ok(http_callbacks::BodyEvent::Pending) => {
                self.bodies.insert(token, PendingBody { body, sink });
                self.later(Job::HttpBody { token }, REPOLL);
            }
            Ok(http_callbacks::BodyEvent::Chunk(data)) => {
                let _ = sink.send(Some(data));
                self.bodies.insert(token, PendingBody { body, sink });
                self.later(Job::HttpBody { token }, Duration::ZERO);
            }
            Ok(http_callbacks::BodyEvent::End) => {
                let _ = sink.send(None);
                body.resource_drop_async(&mut *store).await?;
            }
            Err(_) => {
                // Closing the stream without its end tells the connection the body failed.
                drop(sink);
                body.resource_drop_async(&mut *store).await?;
            }
        }
        Ok(())
    }
}
