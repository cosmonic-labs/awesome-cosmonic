//! The tokio ↔ component-model-async bridge.
//!
//! This component hosts two async worlds on one thread:
//!
//! - the **component-model** world: the `wasi:http` export, body streams, and
//!   outbound `wasi:http/client.send` calls, all driven by the host through
//!   the WASI p3 async ABI;
//! - the **tokio** world: `rmcp`'s protocol machinery and tool code, driven by
//!   a single-threaded tokio runtime.
//!
//! A tokio-world future must never await a WASI p3 future directly (the host
//! cannot make progress while the thread is blocked inside the runtime), so
//! this module provides the two crossing points:
//!
//! - [`drive`] — runs a tokio-world future to completion from component-model
//!   context. It repeatedly enters `Runtime::block_on` with a `select!` over
//!   the future and the job queue: whenever tool code submits a job,
//!   `block_on` returns, the job runs against the real host bindings in
//!   component-model context, and the tokio future is resumed with the reply.
//! - [`submit`] — the tool-facing API: enqueue a unit of component-model work,
//!   await its result. [`outbound::fetch`] (HTTP) and every operation in
//!   [`crate::nats`] are built on it.
use std::future::Future;
use std::pin::{pin, Pin};

use tokio::sync::{mpsc, oneshot};

/// A unit of component-model work queued by tool code for the driver to run.
///
/// Boxed as a closure rather than a concrete request type so the bridge stays
/// agnostic of *which* capability is being called: outbound HTTP and every
/// `wasmcloud:nats` operation ride this one queue. Invoking the closure
/// produces the future to poll in component-model context; it owns its own
/// reply channel, so the driver only has to run it to completion.
///
/// The closure is `Send` (it crosses the queue); the future it returns need
/// not be, since it is created and awaited on the same thread — WASI has no
/// others. That matters because host resource handles (a `kv::Bucket`, a
/// `jetstream::MessageHandle`) are not `Send` and must never escape into the
/// tokio world: keeping them inside the future confines them to the one
/// context that may legally touch them.
type Job = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()>>> + Send>;

type JobReceiver = std::sync::Mutex<mpsc::UnboundedReceiver<Job>>;

fn job_queue() -> &'static (mpsc::UnboundedSender<Job>, JobReceiver) {
    use std::sync::OnceLock;
    static QUEUE: OnceLock<(mpsc::UnboundedSender<Job>, JobReceiver)> = OnceLock::new();
    QUEUE.get_or_init(|| {
        let (tx, rx) = mpsc::unbounded_channel();
        (tx, std::sync::Mutex::new(rx))
    })
}

fn job_sender() -> mpsc::UnboundedSender<Job> {
    job_queue().0.clone()
}

fn with_job_receiver<T>(f: impl FnOnce(&mut mpsc::UnboundedReceiver<Job>) -> T) -> T {
    let mut guard = job_queue().1.lock().expect("job receiver poisoned");
    f(&mut guard)
}

/// Discards jobs queued by a previous exchange that ended before they were
/// serviced (e.g. the peer dropped the response stream mid-body and the pump
/// returned early). Dropping a job drops the reply sender it captured, which
/// resolves the stale tool's [`submit`] with [`Closed`].
pub fn drain_stale_jobs() {
    with_job_receiver(|rx| {
        let mut dropped = 0usize;
        while rx.try_recv().is_ok() {
            dropped += 1;
        }
        if dropped > 0 {
            tracing::warn!(dropped, "discarded stale jobs from a previous exchange");
        }
    });
}

/// Serializes MCP exchanges within one component instance. The bridge drives
/// one tokio-world computation at a time; concurrent scaling comes from the
/// host running more instances (`poolSize`), not intra-instance concurrency.
pub fn request_lock() -> std::sync::Arc<tokio::sync::Mutex<()>> {
    use std::sync::{Arc, OnceLock};
    static LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Lazily-constructed single-threaded tokio runtime.
///
/// WASI has no threads, so this is a `current_thread` runtime; every
/// `block_on` in [`drive`] also runs tasks spawned with `tokio::spawn`.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    use std::sync::OnceLock;
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("failed to build single-threaded tokio runtime")
    })
}

/// The bridge driver went away before the job ran (component teardown, or a
/// previous exchange's jobs being drained).
#[derive(Debug)]
pub struct Closed;

/// Queues `work` for the bridge driver and awaits its result, from **tool
/// (tokio) context**.
///
/// `work` is not called here: it is called by [`drive`] once it has re-entered
/// component-model context, which is the only place host imports may be
/// awaited. Everything the operation needs must therefore be owned by the
/// closure, and only plain data may come back — see [`Job`].
pub async fn submit<T, F, Fut>(work: F) -> Result<T, Closed>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
    T: Send + 'static,
{
    let (reply_tx, reply_rx) = oneshot::channel();
    let job: Job = Box::new(move || {
        Box::pin(async move {
            // A send error means the caller stopped awaiting; the result is
            // simply dropped.
            let _ = reply_tx.send(work().await);
        })
    });
    job_sender().send(job).map_err(|_| Closed)?;
    reply_rx.await.map_err(|_| Closed)
}

/// Runs a tokio-world future to completion from component-model context,
/// servicing jobs submitted by tool code along the way.
///
/// Must only be called while holding [`request_lock`].
pub async fn drive<T>(future: impl Future<Output = T>) -> T {
    enum Step<T> {
        Done(T),
        Job(Job),
    }

    let mut future = pin!(future);
    loop {
        let step = runtime().block_on(async {
            tokio::select! {
                biased;
                job = poll_next_job() => Step::Job(job),
                value = &mut future => Step::Done(value),
            }
        });
        match step {
            Step::Done(value) => return value,
            Step::Job(job) => {
                // Component-model context again: run the queued work against
                // the real host bindings. The job reports its own result.
                job().await;
            }
        }
    }
}

async fn poll_next_job() -> Job {
    std::future::poll_fn(|cx| with_job_receiver(|rx| rx.poll_recv(cx)))
        .await
        .expect("job queue sender side is static and never closes")
}

/// The deadline elapsed before the raced future completed.
#[derive(Debug)]
pub struct TimedOut;

/// Races a future against a `wasi:clocks` monotonic deadline.
///
/// For **component-model context only** (the pump, `outbound::perform`) —
/// tokio-world futures get their timeouts from `tokio::time` inside the
/// runtime instead. Without deadlines here, a peer that stalls (an upstream
/// that never responds, a client that stops reading its response stream)
/// would park the instance forever while it holds the request lock.
pub async fn timeout<F: Future>(millis: u64, future: F) -> Result<F::Output, TimedOut> {
    use std::task::Poll;
    let mut future = pin!(future);
    let mut deadline = pin!(wasip3::clocks::monotonic_clock::wait_for(
        millis.saturating_mul(1_000_000)
    ));
    std::future::poll_fn(|cx| {
        if let Poll::Ready(value) = future.as_mut().poll(cx) {
            return Poll::Ready(Ok(value));
        }
        if deadline.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(TimedOut));
        }
        Poll::Pending
    })
    .await
}

pub mod outbound {
    //! Outbound HTTP for tool code, backed directly by the `wasi:http@0.3.0`
    //! client bindings (per-workload `allowedHosts` policy applies).
    //!
    //! ```rust,ignore
    //! let response = crate::bridge::outbound::fetch(
    //!     http::Request::get("https://api.example.com/data").body(Bytes::new())?,
    //! )
    //! .await?;
    //! ```

    use bytes::Bytes;
    use http_body_util::{BodyExt as _, Full};
    use wasip3::http_compat::{http_from_wasi_response, http_into_wasi_request};

    /// Default upper bound on a buffered outbound response body, overridable
    /// with `MCP_OUTBOUND_MAX_BYTES`. Some upstreams (e.g. SEC EDGAR's
    /// `company_tickers.json` or `submissions` documents) exceed the 4 MiB
    /// request-side default, so tools that expect large payloads can raise it.
    const DEFAULT_MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

    fn max_response_bytes() -> usize {
        std::env::var("MCP_OUTBOUND_MAX_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_RESPONSE_BYTES)
    }

    /// Deadline for one outbound exchange (connect through body read),
    /// overridable with `MCP_OUTBOUND_TIMEOUT_MS`. Without it, an upstream
    /// that accepts the connection and never responds would wedge the
    /// instance (the tokio world is frozen while the bridge performs I/O).
    const DEFAULT_TIMEOUT_MS: u64 = 30_000;

    /// Errors surfaced to tool code for a failed outbound exchange.
    #[derive(Debug, thiserror::Error)]
    pub enum Error {
        /// The WASI host rejected or failed the request (DNS, TLS, policy —
        /// e.g. a host missing from the workload's `allowedHosts` — etc.).
        #[error("wasi:http error: {0}")]
        Wasi(String),
        /// The exchange did not complete within the outbound deadline.
        #[error("outbound request timed out after {0} ms")]
        TimedOut(u64),
        /// The response body exceeded the outbound size limit.
        #[error("response body exceeded the outbound size limit")]
        ResponseTooLarge,
        /// The bridge driver went away before replying (component teardown).
        #[error("outbound bridge unavailable")]
        BridgeClosed,
    }

    impl From<wasip3::http::types::ErrorCode> for Error {
        fn from(code: wasip3::http::types::ErrorCode) -> Self {
            Self::Wasi(code.to_string())
        }
    }

    /// Performs an outbound HTTP request from **tool (tokio) context**.
    ///
    /// The request is queued to the bridge driver, performed over
    /// `wasi:http/client.send`, and the response body is buffered.
    pub async fn fetch(request: http::Request<Bytes>) -> Result<http::Response<Bytes>, Error> {
        super::submit(move || perform(request))
            .await
            .unwrap_or(Err(Error::BridgeClosed))
    }

    /// Performs the exchange in **component-model context**. Internal to the
    /// bridge driver.
    pub(super) async fn perform(
        request: http::Request<Bytes>,
    ) -> Result<http::Response<Bytes>, Error> {
        let timeout_ms = std::env::var("MCP_OUTBOUND_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        let max_bytes = max_response_bytes();
        super::timeout(timeout_ms, async move {
            let wasi_request = http_into_wasi_request(request.map(Full::new))?;
            let wasi_response = wasip3::http::client::send(wasi_request).await?;
            let response = http_from_wasi_response(wasi_response)?;
            let (parts, body) = response.into_parts();
            let bytes = http_body_util::Limited::new(body, max_bytes)
                .collect()
                .await
                .map_err(|err| {
                    if err.is::<http_body_util::LengthLimitError>() {
                        Error::ResponseTooLarge
                    } else {
                        Error::Wasi(err.to_string())
                    }
                })?
                .to_bytes();
            Ok(http::Response::from_parts(parts, bytes))
        })
        .await
        .unwrap_or(Err(Error::TimedOut(timeout_ms)))
    }
}
