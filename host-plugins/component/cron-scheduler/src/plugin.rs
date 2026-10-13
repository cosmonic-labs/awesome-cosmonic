//! WIT bindings around the [`Engine`].
//!
//! The host keeps one plugin store for:
//!
//! - Workload calls to `scheduler`.
//! - Lifecycle hooks for workload binding and cleanup.
//! - The `wasi:cli/run` dispatch loop.
//!
//! Scheduler calls and lifecycle hooks update the engine and wake the loop, which makes
//! all workload calls from one task and keeps them within the initiating call.

mod bindings {
    #![allow(unsafe_code)]
    wit_bindgen::generate!({ world: "plugin", generate_all });
}

use std::cell::{Cell, RefCell};
use std::future::pending;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::stream::FuturesUnordered;
use futures::{FutureExt as _, StreamExt as _, pin_mut, select};

// `types` is bound twice, because the world both exports it (for `scheduler`)
// and imports it (for `handler`). What the plugin serves uses the exported
// copy; the firing it hands a workload uses the imported one.
use bindings::cosmonic::cron::handler::{self, HandlerError};
use bindings::cosmonic::cron::types::Firing;
use bindings::exports::cosmonic::cron::scheduler::Guest as SchedulerGuest;
use bindings::exports::cosmonic::cron::types::{
    CronSchedule, Instant as ServedInstant, Registration, Schedule, ScheduleError,
};
use bindings::exports::wasi::cli::run::Guest as RunGuest;
use bindings::exports::wasmcloud::host::workload_lifecycle::{
    Guest as LifecycleGuest, WorkloadInfo,
};
use bindings::wasi::clocks::system_clock::Instant;
use bindings::wasi::clocks::{monotonic_clock, system_clock};
use bindings::wasmcloud::host::identity;
use bindings::wasmcloud::host::workload_call::{self, Target};

use crate::engine::{Completion, Dispatch, EndReason, Engine, Now, Outcome, RegisterError};
use crate::spec::{Binding, CronSpec, Spec, SpecError, Wall, format_duration};

/// The handler interface name used by `workload-call`.
const HANDLER: &str = "cosmonic:cron/handler@0.1.0";

/// Minimum loop sleep in nanoseconds, preventing rounding errors from causing a busy
/// loop.
const MIN_SLEEP: u64 = 1_000_000;

thread_local! {
    static ENGINE: RefCell<Engine> = const { RefCell::new(Engine::new()) };
    /// Wake channel for scheduler calls and lifecycle hooks, with its receiver owned by
    /// the loop.
    static WAKE: (UnboundedSender<()>, RefCell<Option<UnboundedReceiver<()>>>) = {
        let (tx, rx) = unbounded();
        (tx, RefCell::new(Some(rx)))
    };
    /// Whether a wake is pending, coalescing changes until the loop handles them.
    static WAKE_QUEUED: Cell<bool> = const { Cell::new(false) };
}

fn now() -> Now {
    let wall = system_clock::now();
    Now {
        mono: monotonic_clock::now(),
        wall: Wall { seconds: wall.seconds, nanoseconds: wall.nanoseconds },
    }
}

/// Wake the loop to recompute deadlines after schedules change.
fn wake() {
    if WAKE_QUEUED.replace(true) {
        return;
    }
    WAKE.with(|(tx, _)| {
        // Fails only if the loop has ended, and then there is nothing to wake.
        let _ = tx.unbounded_send(());
    });
}

/// The longest line the plugin writes to its log, in characters.
const MAX_LOG_LINE: usize = 512;

/// Write a log line with control characters replaced and length capped to prevent
/// workload-supplied text from forging or flooding logs.
fn log(message: impl AsRef<str>) {
    let line: String = message
        .as_ref()
        .chars()
        .take(MAX_LOG_LINE)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    eprintln!("cron-scheduler: {line}");
}

fn to_spec(schedule: Schedule) -> Result<Spec, SpecError> {
    match schedule {
        Schedule::Cron(cron) => {
            CronSpec::parse(&cron.expression, cron.timezone.as_deref()).map(Spec::Cron)
        }
        Schedule::Interval(period) => Ok(Spec::Interval(period)),
        Schedule::OnceAt(at) => Ok(Spec::OnceAt(Wall {
            seconds: at.seconds,
            nanoseconds: at.nanoseconds,
        })),
        Schedule::OnceAfter(delay) => Ok(Spec::OnceAfter(delay)),
        Schedule::Other(text) => Err(SpecError::Unsupported(format!(
            "'{text}': this plugin provides cron, interval, once-at and once-after schedules"
        ))),
    }
}

fn from_spec(spec: Spec) -> Schedule {
    match spec {
        Spec::Cron(cron) => Schedule::Cron(CronSchedule {
            expression: cron.expression().to_string(),
            timezone: cron.timezone().map(str::to_string),
        }),
        Spec::Interval(period) => Schedule::Interval(period),
        Spec::OnceAt(at) => Schedule::OnceAt(ServedInstant {
            seconds: at.seconds,
            nanoseconds: at.nanoseconds,
        }),
        Spec::OnceAfter(delay) => Schedule::OnceAfter(delay),
    }
}

fn describe(spec: &Spec) -> String {
    match spec {
        Spec::Cron(cron) => match cron.timezone() {
            Some(zone) => format!("cron '{}' in {zone}", cron.expression()),
            None => format!("cron '{}'", cron.expression()),
        },
        Spec::Interval(period) => format!("every {}", format_duration(*period)),
        Spec::OnceAt(at) => format!("once at {}s since the epoch", at.seconds),
        Spec::OnceAfter(delay) => format!("once in {}", format_duration(*delay)),
    }
}

impl From<SpecError> for ScheduleError {
    fn from(err: SpecError) -> Self {
        match err {
            SpecError::InvalidName(detail) => Self::InvalidName(detail),
            SpecError::InvalidSchedule(detail) => Self::InvalidSchedule(detail),
            SpecError::Unsupported(detail) => Self::Unsupported(detail),
        }
    }
}

impl From<RegisterError> for ScheduleError {
    fn from(err: RegisterError) -> Self {
        match err {
            RegisterError::Spec(err) => err.into(),
            RegisterError::AccessDenied => Self::AccessDenied,
            RegisterError::LimitExceeded => Self::LimitExceeded,
        }
    }
}

/// Whether any component in `workload` exports `handler`.
fn exports_handler(workload: &str) -> bool {
    workload_call::callable()
        .into_iter()
        .any(|(id, interfaces)| id == workload && interfaces.iter().any(|i| i == HANDLER))
}

struct Plugin;

impl SchedulerGuest for Plugin {
    async fn register(name: String, schedule: Schedule) -> Result<(), ScheduleError> {
        let workload = identity::get_workload_id();
        let spec = to_spec(schedule)?;
        if !exports_handler(&workload) {
            return Err(ScheduleError::NoHandler);
        }
        let described = describe(&spec);
        // A component is told to register every time it starts, so most calls
        // change nothing. Those are neither logged nor a reason to wake the
        // loop: a workload that could write a line per call could bury the
        // lines that say why a schedule stopped.
        if ENGINE.with_borrow_mut(|engine| engine.register(&workload, &name, spec, now()))? {
            log(format!("workload {workload} registered '{name}': {described}"));
            wake();
        }
        Ok(())
    }

    async fn unregister(name: String) -> Result<(), ScheduleError> {
        let workload = identity::get_workload_id();
        if ENGINE.with_borrow_mut(|engine| engine.unregister(&workload, &name))? {
            log(format!("workload {workload} unregistered '{name}'"));
            wake();
        }
        Ok(())
    }

    async fn list_registrations() -> Result<Vec<Registration>, ScheduleError> {
        let workload = identity::get_workload_id();
        let held = ENGINE.with_borrow_mut(|engine| engine.list(&workload, now()))?;
        Ok(held
            .into_iter()
            .map(|held| Registration {
                name: held.name,
                schedule: from_spec(held.spec),
                declared: held.declared,
                next: held.next.map(|at| ServedInstant {
                    seconds: at.seconds,
                    nanoseconds: at.nanoseconds,
                }),
            })
            .collect())
    }
}

impl LifecycleGuest for Plugin {
    /// Parse declared schedules from the binding, failing deployment on invalid
    /// configuration.
    async fn on_workload_bind(workload: WorkloadInfo) -> Result<(), String> {
        let ours = || {
            workload
                .interfaces
                .iter()
                .filter(|binding| binding.namespace == "cosmonic" && binding.package == "cron")
        };
        let entries = ours().map(|binding| {
            binding.config.iter().map(|(key, value)| (key.as_str(), value.as_str()))
        });
        let binding = Binding::parse_entries(entries).map_err(|err| {
            format!("cosmonic:cron binding of workload '{}': {err}", workload.name)
        })?;
        let grants = |interface: &str| {
            ours().any(|binding| binding.interfaces.iter().any(|name| name == interface))
        };
        let can_register = grants("scheduler");
        if !grants("handler") && (can_register || !binding.declared.is_empty()) {
            // Without `handler` the host routes no call from this plugin to
            // the workload, so every firing would be skipped, forever and
            // quietly. Refusing the bind is how the mistake gets seen.
            return Err(format!(
                "cosmonic:cron binding of workload '{}': no entry lists the `handler` \
                 interface, so its schedules could never be delivered",
                workload.name
            ));
        }

        let declared: Vec<String> = binding
            .declared
            .iter()
            .map(|(name, spec)| format!("'{name}' {}", describe(spec)))
            .collect();
        log(format!(
            "bound workload {} ({}): {}{}",
            workload.id,
            workload.name,
            if declared.is_empty() { "no declared schedules".to_string() } else { declared.join(", ") },
            if can_register { "; may register its own" } else { "" },
        ));
        ENGINE.with_borrow_mut(|engine| engine.bind(&workload.id, binding, can_register, now()));
        log_ended();
        wake();
        Ok(())
    }

    async fn on_workload_unbind(id: String) {
        ENGINE.with_borrow_mut(|engine| engine.unbind(&id));
        log(format!("unbound workload {id}"));
        wake();
    }
}

/// Deliver one firing and report its result.
///
/// Opening the target and starting its call without an intervening `await` prevents
/// another delivery's task-scoped handle from changing the route.
async fn deliver(dispatch: Dispatch) -> (Dispatch, Outcome, Option<String>) {
    let Some(target) = Target::open(&dispatch.workload, HANDLER) else {
        // Bound but not running: still deploying, stopped, or redeployed.
        return (dispatch, Outcome::Undeliverable, None);
    };
    let firing = Firing {
        name: dispatch.firing.name.clone(),
        scheduled_at: Instant {
            seconds: dispatch.firing.scheduled_at.seconds,
            nanoseconds: dispatch.firing.scheduled_at.nanoseconds,
        },
        missed: dispatch.firing.missed,
        attempt: dispatch.firing.attempt,
    };
    let result = handler::handle(firing).await;
    drop(target);
    match result {
        Ok(()) => (dispatch, Outcome::Handled, None),
        Err(HandlerError::Retry(detail)) => (dispatch, Outcome::Retry, detail),
        Err(HandlerError::Failed(detail) | HandlerError::Other(detail)) => {
            (dispatch, Outcome::Failed, detail)
        }
    }
}

/// Log automatically removed schedules, providing the only removal record because
/// workloads receive no notification.
fn log_ended() {
    for ended in ENGINE.with_borrow_mut(Engine::take_ended) {
        let why = match ended.reason {
            // A one-shot doing what it was registered to do is not news.
            EndReason::Fired => continue,
            EndReason::Undelivered => "was dropped, the workload was not running when it came due",
            EndReason::MaxLifetime => "was removed, it was held for the workload's max-lifetime",
            EndReason::MaxFirings => "was removed, it delivered the workload's max-firings",
            EndReason::Rebound => "was removed, the workload's new binding does not permit it",
        };
        log(format!("'{}' of workload {} {why}", ended.name, ended.workload));
    }
}

/// Tell the engine a delivery ended, and log anything but plain success.
fn settle(dispatch: &Dispatch, outcome: Outcome, detail: Option<String>) {
    let completion = ENGINE.with_borrow_mut(|engine| engine.complete(dispatch, outcome, now()));
    log_ended();
    let Dispatch { workload, firing, .. } = dispatch;
    let detail = detail.map(|d| format!(": {d}")).unwrap_or_default();
    let what = match (outcome, completion) {
        (Outcome::Handled, _) | (_, Completion::Stale) => return,
        // Said when it starts and not again each period; a dropped one-shot
        // is reported by `log_ended`.
        (Outcome::Undeliverable, Completion::Undelivered { first: true }) => {
            "is being skipped, the workload cannot be called".to_string()
        }
        (Outcome::Undeliverable, _) => return,
        (Outcome::Failed, _) => format!("failed{detail}"),
        (Outcome::Retry, Completion::RetryIn(wait)) => {
            format!("will be retried in {}{detail}", format_duration(wait))
        }
        (Outcome::Retry, Completion::RetriesExhausted) => {
            format!("gave up after {} attempts{detail}", firing.attempt)
        }
        (Outcome::Retry, _) => {
            format!("not retried, its next firing is due first{detail}")
        }
    };
    log(format!("'{}' of workload {workload} {what}", firing.name));
}

impl RunGuest for Plugin {
    async fn run() -> Result<(), ()> {
        let Some(mut woken) = WAKE.with(|(_, rx)| rx.borrow_mut().take()) else {
            log("the dispatch loop was started twice");
            return Err(());
        };
        log("dispatch loop started");
        let mut deliveries = FuturesUnordered::new();
        loop {
            // Cleared before looking, so a change made from here on queues a
            // wake of its own and cannot be missed. Any wake still queued is
            // for a change this pass is about to see.
            WAKE_QUEUED.set(false);
            while let Ok(()) = woken.try_recv() {}
            let now = now();
            let due = ENGINE.with_borrow_mut(|engine| engine.poll(now));
            log_ended();
            deliveries.extend(due.into_iter().map(deliver));

            let wait = ENGINE.with_borrow(|engine| engine.next_wake(now));
            let sleep = async {
                match wait {
                    Some(wait) => monotonic_clock::wait_for(wait.max(MIN_SLEEP)).await,
                    // Nothing is scheduled, so only a change can be next.
                    None => pending().await,
                }
            }
            .fuse();
            pin_mut!(sleep);

            select! {
                () = sleep => {}
                () = woken.select_next_some() => {}
                (dispatch, outcome, detail) = deliveries.select_next_some() => {
                    settle(&dispatch, outcome, detail);
                }
            }
        }
    }
}

bindings::export!(Plugin with_types_in bindings);
