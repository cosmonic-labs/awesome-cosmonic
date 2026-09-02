//! The tokio ↔ component-model-async bridge.
//!
//! This component hosts two async worlds on one thread:
//!
//! - the **component-model** world: the `wasi:http` export, body streams,
//!   outbound `wasi:http/client.send` calls, and — specific to this server —
//!   the `wasmcloud:postgres@0.2.0` host calls (`db.query`,
//!   `db-prepared.exec`, …), all driven by the host through the WASI p3
//!   async ABI;
//! - the **tokio** world: `rmcp`'s protocol machinery and tool code, driven by
//!   a single-threaded tokio runtime.
//!
//! A tokio-world future must never await a WASI p3 future directly (the host
//! cannot make progress while the thread is blocked inside the runtime), so
//! this module provides the crossing points:
//!
//! - [`drive`] — runs a tokio-world future to completion from component-model
//!   context. It repeatedly enters `Runtime::block_on` with a `select!` over
//!   the future and the job queue: whenever tool code submits a job,
//!   `block_on` returns, the job is performed with the real host bindings in
//!   component-model context, and the tokio future is resumed with the reply.
//! - [`outbound::fetch`] — the tool-facing API for outbound HTTP (the
//!   template's original job kind; unused by this server's tools but kept
//!   intact). See [`outbound`].
//! - [`host_call`] — **the postgres extension**: a generic job kind that runs
//!   an arbitrary boxed local future (a `wasmcloud:postgres` call and its row
//!   stream consumption) in component-model context under a deadline, and
//!   hands the typed result back to tool code. See [`crate::postgres`].
use std::future::Future;
use std::pin::{pin, Pin};

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

/// An outbound HTTP exchange queued by tool code for the component-model
/// driver to perform.
type HttpJob = (
    http::Request<Bytes>,
    oneshot::Sender<Result<http::Response<Bytes>, outbound::Error>>,
);

/// A host call queued by tool code: a closure that, when invoked in
/// component-model context, produces the future to drive there. The closure
/// itself is `Send` (it only captures plain data plus the reply sender); the
/// future it returns need not be — it is created and polled on this thread.
type HostJob = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()>>> + Send>;

/// One unit of work for the driver.
enum Job {
    Http(HttpJob),
    Host(HostJob),
}

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
/// returned early). Dropping the reply sender resolves the stale tool's
/// `fetch`/`host_call` with a bridge-closed error.
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

/// Runs a tokio-world future to completion from component-model context,
/// servicing the jobs (outbound HTTP, host calls) submitted by tool code
/// along the way.
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
            // Component-model context again: perform the exchange with the
            // real wasi:http client bindings.
            Step::Job(Job::Http((request, reply))) => {
                let _ = reply.send(outbound::perform(request).await);
            }
            // Component-model context: run the host call. The job's own
            // future delivers the result (or its timeout) to the tool.
            Step::Job(Job::Host(job)) => job().await,
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
/// For **component-model context only** (the pump, `outbound::perform`, the
/// host-call job) — tokio-world futures get their timeouts from `tokio::time`
/// inside the runtime instead. Without deadlines here, a peer that stalls (an
/// upstream that never responds, a database that never delivers a row, a
/// client that stops reading its response stream) would park the instance
/// forever while it holds the request lock.
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

/// Why a [`host_call`] produced no result.
#[derive(Debug, thiserror::Error)]
pub enum HostCallError {
    /// The host did not complete the call within the deadline. The future was
    /// dropped (for a query, that drops the row stream so the host stops
    /// fetching), but the statement may still be running server-side.
    #[error("host call timed out after {0} ms")]
    TimedOut(u64),
    /// The bridge driver went away before replying (component teardown or a
    /// stale job discarded after the client dropped the exchange).
    #[error("host-call bridge unavailable")]
    BridgeClosed,
}

/// Performs a host call from **tool (tokio) context**.
///
/// `make` is invoked in component-model context and returns the future to
/// drive there — typically a `wasmcloud:postgres` binding call followed by
/// consumption of its row stream and completion future. The future runs under
/// a `wasi:clocks` deadline of `timeout_ms`; its output is sent back over a
/// oneshot and awaited here in the tokio world, so tool code never touches a
/// WASI p3 future directly.
///
/// ```rust,ignore
/// let rows = crate::bridge::host_call(30_000, move || async move {
///     let (columns, mut rows, done) = bindings::db::query(sql, params).await?;
///     // ... consume rows ...
///     done.await
/// })
/// .await;
/// ```
pub async fn host_call<T, F, Fut>(timeout_ms: u64, make: F) -> Result<T, HostCallError>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
{
    let (reply_tx, reply_rx) = oneshot::channel::<Result<T, HostCallError>>();
    let job: HostJob = Box::new(move || {
        Box::pin(async move {
            let outcome = timeout(timeout_ms, make())
                .await
                .map_err(|TimedOut| HostCallError::TimedOut(timeout_ms));
            let _ = reply_tx.send(outcome);
        })
    });
    job_sender()
        .send(Job::Host(job))
        .map_err(|_| HostCallError::BridgeClosed)?;
    reply_rx.await.map_err(|_| HostCallError::BridgeClosed)?
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
    //!
    //! This server's tools do not dial out (the database is reached through
    //! the `wasmcloud:postgres` host interface, and the workload's
    //! `allowedHosts` stays empty), but the path is kept intact so the bridge
    //! remains the template's.

    use bytes::Bytes;
    use http_body_util::{BodyExt as _, Full};
    use wasip3::http_compat::{http_from_wasi_response, http_into_wasi_request};

    /// Default upper bound on a buffered outbound response body, overridable
    /// with `MCP_OUTBOUND_MAX_BYTES`.
    const DEFAULT_MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

    fn max_response_bytes() -> usize {
        std::env::var("MCP_OUTBOUND_MAX_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_RESPONSE_BYTES)
    }

    /// Deadline for one outbound exchange (connect through body read),
    /// overridable with `MCP_OUTBOUND_TIMEOUT_MS`.
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
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        super::job_sender()
            .send(super::Job::Http((request, reply_tx)))
            .map_err(|_| Error::BridgeClosed)?;
        reply_rx.await.map_err(|_| Error::BridgeClosed)?
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
