//! The component: WIT bindings around the [`Engine`].
//!
//! Three things call into this instance, all on the one store the host keeps
//! for the plugin:
//!
//! - a workload calling `scheduler`, which edits the engine's schedules;
//! - the host calling the lifecycle hooks as workloads bind and unbind; and
//! - the host driving `wasi:cli/run`, the loop that sleeps until something is
//!   due and delivers it.
//!
//! Only the loop delivers. The other two change what the engine holds and
//! then wake the loop, so every call into a workload is made from one task
//! and nothing outlives the call that asked for it.

mod bindings {
    #![allow(unsafe_code)]
    wit_bindgen::generate!({ world: "plugin", generate_all });
}

use std::cell::RefCell;
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
    CronSchedule, Registration, Schedule, ScheduleError,
};
use bindings::exports::wasi::cli::run::Guest as RunGuest;
use bindings::exports::wasmcloud::host::workload_lifecycle::{
    Guest as LifecycleGuest, WorkloadInfo,
};
use bindings::wasi::clocks::system_clock::Instant;
use bindings::wasi::clocks::{monotonic_clock, system_clock};
use bindings::wasmcloud::host::identity;
use bindings::wasmcloud::host::workload_call::{self, Target};

use crate::engine::{Completion, Dispatch, Engine, Now, Outcome, RegisterError};
use crate::spec::{Binding, CronSpec, Spec, SpecError, Wall, format_duration};

/// The interface this plugin calls on a workload, spelled the way
/// `workload-call` names it.
const HANDLER: &str = "cosmonic:cron/handler@0.1.0";

/// The shortest the loop sleeps, in nanoseconds, so a schedule that is due
/// "now" by a rounding error cannot spin it.
const MIN_SLEEP: u64 = 1_000_000;

thread_local! {
    static ENGINE: RefCell<Engine> = const { RefCell::new(Engine::new()) };
    /// How `scheduler` calls and lifecycle hooks tell the loop the schedules
    /// changed. The loop takes the receiving end when it starts.
    static WAKE: (UnboundedSender<()>, RefCell<Option<UnboundedReceiver<()>>>) = {
        let (tx, rx) = unbounded();
        (tx, RefCell::new(Some(rx)))
    };
}

fn now() -> Now {
    let wall = system_clock::now();
    Now {
        mono: monotonic_clock::now(),
        wall: Wall { seconds: wall.seconds, nanoseconds: wall.nanoseconds },
    }
}

/// Have the loop look at the schedules again rather than sleep out a wait it
/// computed before they changed.
fn wake() {
    WAKE.with(|(tx, _)| {
        // Fails only if the loop has ended, and then there is nothing to wake.
        let _ = tx.unbounded_send(());
    });
}

fn log(message: impl AsRef<str>) {
    eprintln!("cron-scheduler: {}", message.as_ref());
}

fn to_spec(schedule: Schedule) -> Result<Spec, SpecError> {
    match schedule {
        Schedule::Cron(cron) => {
            CronSpec::parse(&cron.expression, cron.timezone.as_deref()).map(Spec::Cron)
        }
        Schedule::Interval(period) => Ok(Spec::Interval(period)),
        Schedule::Other(text) => Err(SpecError::Unsupported(format!(
            "'{text}': this plugin provides cron and interval schedules"
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
    }
}

fn describe(spec: &Spec) -> String {
    match spec {
        Spec::Cron(cron) => match cron.timezone() {
            Some(zone) => format!("cron '{}' in {zone}", cron.expression()),
            None => format!("cron '{}'", cron.expression()),
        },
        Spec::Interval(period) => format!("every {}", format_duration(*period)),
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

/// Whether any component of `workload` exports `handler`, so a firing has
/// somewhere to land.
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
        ENGINE.with_borrow_mut(|engine| engine.register(&workload, &name, spec, now()))?;
        log(format!("workload {workload} registered '{name}': {described}"));
        wake();
        Ok(())
    }

    async fn unregister(name: String) -> Result<(), ScheduleError> {
        let workload = identity::get_workload_id();
        ENGINE.with_borrow_mut(|engine| engine.unregister(&workload, &name))?;
        log(format!("workload {workload} unregistered '{name}'"));
        wake();
        Ok(())
    }

    async fn list_registrations() -> Result<Vec<Registration>, ScheduleError> {
        let workload = identity::get_workload_id();
        let held = ENGINE.with_borrow_mut(|engine| engine.list(&workload))?;
        Ok(held
            .into_iter()
            .map(|held| Registration {
                name: held.name,
                schedule: from_spec(held.spec),
                declared: held.declared,
            })
            .collect())
    }
}

impl LifecycleGuest for Plugin {
    /// Read the workload's declared schedules out of its binding. An error
    /// here fails the deploy, which is the point: a schedule that cannot be
    /// read is refused while someone is looking, not silently never fired.
    async fn on_workload_bind(workload: WorkloadInfo) -> Result<(), String> {
        let ours = || {
            workload
                .interfaces
                .iter()
                .filter(|binding| binding.namespace == "cosmonic" && binding.package == "cron")
        };
        let config = ours().flat_map(|binding| {
            binding.config.iter().map(|(key, value)| (key.as_str(), value.as_str()))
        });
        let binding = Binding::parse(config).map_err(|err| {
            format!("cosmonic:cron binding of workload '{}': {err}", workload.name)
        })?;
        let can_register =
            ours().any(|binding| binding.interfaces.iter().any(|name| name == "scheduler"));

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
        wake();
        Ok(())
    }

    async fn on_workload_unbind(id: String) {
        ENGINE.with_borrow_mut(|engine| engine.unbind(&id));
        log(format!("unbound workload {id}"));
        wake();
    }
}

/// Deliver one firing and report how it went.
///
/// The target handle is opened and the call started without an `await`
/// between them. A handle scopes the calls of the task that holds it, and
/// every delivery runs on the loop's task, so that is what keeps one
/// delivery's call from being routed by another's handle.
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

/// Tell the engine a delivery ended, and log anything but plain success.
fn settle(dispatch: &Dispatch, outcome: Outcome, detail: Option<String>) {
    let completion = ENGINE.with_borrow_mut(|engine| engine.complete(dispatch, outcome, now()));
    let Dispatch { workload, firing, .. } = dispatch;
    let detail = detail.map(|d| format!(": {d}")).unwrap_or_default();
    let what = match (outcome, completion) {
        (Outcome::Handled, _) | (_, Completion::Stale) => return,
        (Outcome::Undeliverable, _) => "skipped, the workload is not running".to_string(),
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
            let now = now();
            let due = ENGINE.with_borrow_mut(|engine| engine.poll(now));
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
