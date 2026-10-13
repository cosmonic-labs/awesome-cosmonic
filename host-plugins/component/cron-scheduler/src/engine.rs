//! Track workload schedules, due times, and delivery results.
//!
//! The dispatch loop supplies clock readings and delivers the returned firings,
//! allowing host tests to control time.

use std::collections::{BTreeMap, btree_map};

use crate::spec::{Binding, Limits, Spec, SpecError, Wall, format_duration, validate_name};

/// Maximum monotonic sleep in nanoseconds before rechecking a pending system-clock
/// deadline for clock corrections.
const WALL_RESYNC: u64 = 30_000_000_000;

/// Maximum occurrences counted per schedule during catch-up, keeping the shared loop
/// responsive and leaving longer gaps as lower bounds.
const MAX_CATCH_UP: u32 = 100;

/// Backward system-clock step threshold in nanoseconds for rescheduling cron from the
/// new time.
///
/// Smaller steps wait for the previous time to avoid duplicate firings; the three-hour
/// threshold follows Vixie cron.
const CLOCK_RESET: u64 = 3 * 3_600 * 1_000_000_000;

/// One reading of both clocks, taken together.
#[derive(Clone, Copy, Debug)]
pub struct Now {
    /// `wasi:clocks/monotonic-clock.now`, in nanoseconds.
    pub mono: u64,
    /// `wasi:clocks/system-clock.now`.
    pub wall: Wall,
}

/// A scheduled firing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Firing {
    pub name: String,
    pub scheduled_at: Wall,
    pub missed: u32,
    pub attempt: u32,
}

/// A firing and the identity of its source registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dispatch {
    pub workload: String,
    /// Registration identity that prevents a completed delivery from affecting a
    /// replacement under the same name.
    pub epoch: u64,
    pub firing: Firing,
}

/// How a delivery ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The handler returned `ok`.
    Handled,
    /// The handler asked for the firing again.
    Retry,
    /// The handler failed without requesting a retry, or the call could not complete.
    Failed,
    /// The workload could not be called at all, so nothing was delivered.
    Undeliverable,
}

/// A completed delivery's result for logging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completion {
    /// No further delivery is needed for this firing.
    Settled,
    /// The firing will be redelivered after this many nanoseconds.
    RetryIn(u64),
    /// The handler asked for a retry and has used every attempt.
    RetriesExhausted,
    /// The next scheduled firing superseded the requested retry.
    RetrySuperseded,
    /// A repeating firing could not reach the workload.
    ///
    /// `first` indicates the previous firing was delivered, allowing one log entry per
    /// unreachable stretch.
    Undelivered { first: bool },
    /// The schedule was removed or replaced while the delivery was in flight.
    Stale,
}

/// Why `scheduler` refused a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegisterError {
    Spec(SpecError),
    AccessDenied,
    LimitExceeded,
}

impl From<SpecError> for RegisterError {
    fn from(err: SpecError) -> Self {
        Self::Spec(err)
    }
}

/// Why the engine automatically removed a schedule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// The one-shot firing completed or exhausted its retries.
    Fired,
    /// The one-shot firing could not reach the workload.
    Undelivered,
    /// It was held for the binding's `max-lifetime`.
    MaxLifetime,
    /// It delivered the binding's `max-firings`.
    MaxFirings,
    /// A new binding revoked `scheduler` or raised `min-interval` beyond this schedule.
    Rebound,
}

/// An automatically removed schedule for logging.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ended {
    pub workload: String,
    pub name: String,
    pub reason: EndReason,
}

/// One schedule as `list-registrations` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registration {
    pub name: String,
    pub spec: Spec,
    pub declared: bool,
    /// The next due time on the system clock, or `None` if no firing remains.
    pub next: Option<Wall>,
}

/// The next due time on the schedule's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Due {
    /// A `cron` schedule's next matching instant, or a `once-at` schedule's
    /// instant.
    Wall(Wall),
    /// The next mark on the monotonic clock of an `interval` or `once-after`
    /// schedule.
    Mono(u64),
    /// No future firing: the schedule is spent or has no later cron match.
    Never,
}

#[derive(Debug)]
struct Retry {
    /// Monotonic mark at which to redeliver.
    due: u64,
    firing: Firing,
}

#[derive(Debug)]
struct Entry {
    spec: Spec,
    declared: bool,
    epoch: u64,
    due: Due,
    /// Firings skipped since the last one delivered.
    missed: u32,
    retry: Option<Retry>,
    /// Whether another occurrence superseded the dispatched firing, preventing retries.
    overtaken: bool,
    /// Whether the latest firing could not be delivered.
    unreachable: bool,
    /// Firings delivered so far, counted against `max-firings`.
    fired: u32,
    /// Whether the current delivery is a first attempt that adds to `fired` on
    /// completion, used to forecast remaining firings.
    counting: bool,
    /// Whether a one-shot or `max-firings` limit reached its final firing, retaining
    /// the entry until that firing settles.
    spent: bool,
    /// Monotonic expiration under `max-lifetime`, unset for declared schedules.
    expires: Option<u64>,
}

#[derive(Debug, Default)]
struct Workload {
    limits: Limits,
    /// Whether the binding grants `scheduler`, as opposed to `handler` alone.
    can_register: bool,
    entries: BTreeMap<String, Entry>,
    /// Running deliveries mapped by schedule name to registration epoch.
    ///
    /// Tracking names separately from entries prevents concurrent handlers across
    /// replacement or removal.
    busy: BTreeMap<String, u64>,
}

#[derive(Debug, Default)]
pub struct Engine {
    workloads: BTreeMap<String, Workload>,
    epochs: u64,
    /// Schedules removed since [`Engine::take_ended`] was last called.
    ended: Vec<Ended>,
    /// The system clock as of the last poll, to notice it being set back.
    last_wall: Option<Wall>,
}

impl Engine {
    pub const fn new() -> Self {
        Self { workloads: BTreeMap::new(), epochs: 0, ended: Vec::new(), last_wall: None }
    }

    /// Create a schedule starting at `now`, with the optional registration
    /// `max-lifetime`.
    fn entry(&mut self, spec: Spec, lifetime: Option<u64>, now: Now) -> Entry {
        self.epochs += 1;
        let due = match &spec {
            Spec::Cron(cron) => cron.next_after(now.wall).map_or(Due::Never, Due::Wall),
            Spec::OnceAt(at) => Due::Wall(*at),
            Spec::Interval(wait) | Spec::OnceAfter(wait) => {
                Due::Mono(now.mono.saturating_add(*wait))
            }
        };
        Entry {
            spec,
            declared: false,
            epoch: self.epochs,
            due,
            missed: 0,
            retry: None,
            overtaken: false,
            unreachable: false,
            fired: 0,
            counting: false,
            spent: false,
            expires: lifetime.map(|lifetime| now.mono.saturating_add(lifetime)),
        }
    }

    /// Drain schedules removed automatically since the previous call, including spent
    /// one-shots and schedules ended by limits or rebinding.
    pub fn take_ended(&mut self) -> Vec<Ended> {
        std::mem::take(&mut self.ended)
    }

    /// Apply a workload's declared schedules and limits.
    ///
    /// Unchanged declared schedules keep their timing across repeated binds.
    /// Registered schedules remain only if permitted by the new binding:
    ///
    /// - Revoking `scheduler` removes all registrations.
    /// - Raising `min-interval` removes shorter schedules.
    /// - Lowering `max-registrations` removes the newest excess registrations.
    /// - Lowering `max-firings` removes schedules already at the limit.
    /// - `max-lifetime` restarts from `now` without extending existing deadlines.
    pub fn bind(&mut self, workload: &str, binding: Binding, can_register: bool, now: Now) {
        let Workload { mut entries, busy, .. } =
            self.workloads.remove(workload).unwrap_or_default();
        entries.retain(|name, entry| {
            if entry.declared {
                return binding.declared.get(name) == Some(&entry.spec);
            }
            if binding.declared.contains_key(name) {
                // A name the binding now declares belongs to the binding.
                return false;
            }
            let permitted = can_register && binding.limits.check(&entry.spec).is_ok();
            if !permitted {
                self.ended.push(Ended {
                    workload: workload.to_string(),
                    name: name.clone(),
                    reason: EndReason::Rebound,
                });
            }
            permitted
        });
        // Oldest first, so that when there are more than `max-registrations`
        // now allows, the ones registered last are the ones to go.
        let mut registered: Vec<(u64, String)> = entries
            .iter()
            .filter(|(_, entry)| !entry.declared)
            .map(|(name, entry)| (entry.epoch, name.clone()))
            .collect();
        registered.sort();
        for (kept, (_, name)) in registered.into_iter().enumerate() {
            let Some(entry) = entries.get_mut(&name) else { continue };
            let reason = if kept >= binding.limits.max_registrations {
                Some(EndReason::Rebound)
            } else if binding.limits.max_firings.is_some_and(|max| entry.fired >= max) {
                Some(EndReason::MaxFirings)
            } else {
                None
            };
            match reason {
                Some(reason) => {
                    entries.remove(&name);
                    self.ended.push(Ended { workload: workload.to_string(), name, reason });
                }
                None => {
                    entry.expires = binding.limits.max_lifetime.map(|lifetime| {
                        let renewed = now.mono.saturating_add(lifetime);
                        entry.expires.map_or(renewed, |held| held.min(renewed))
                    });
                }
            }
        }
        for (name, spec) in binding.declared {
            if let btree_map::Entry::Vacant(slot) = entries.entry(name) {
                let mut entry = self.entry(spec, None, now);
                entry.declared = true;
                slot.insert(entry);
            }
        }
        self.workloads.insert(
            workload.to_string(),
            Workload { limits: binding.limits, can_register, entries, busy },
        );
    }

    /// Remove all schedules for a workload, ignoring unknown IDs.
    pub fn unbind(&mut self, workload: &str) {
        self.workloads.remove(workload);
    }

    /// Register a schedule, returning whether it changed.
    ///
    /// An identical active registration returns `false`.
    ///
    /// # Errors
    ///
    /// `AccessDenied` for a missing scheduling grant or declared name, `Spec` for an
    /// invalid name or schedule, and `LimitExceeded` at the registration limit.
    pub fn register(
        &mut self,
        workload: &str,
        name: &str,
        spec: Spec,
        now: Now,
    ) -> Result<bool, RegisterError> {
        let held = self.registrable(workload)?;
        validate_name(name)?;
        if spec == Spec::Interval(0) {
            return Err(SpecError::InvalidSchedule("a period cannot be zero".to_string()).into());
        }
        if let Spec::OnceAt(at) = &spec
            && at.nanoseconds >= 1_000_000_000
        {
            // Not an instant at all, and the ordering of instants below
            // depends on the field being in range.
            return Err(SpecError::InvalidSchedule(format!(
                "{} nanoseconds is a second or more",
                at.nanoseconds
            ))
            .into());
        }
        held.limits.check(&spec)?;
        let wait = match &spec {
            Spec::OnceAt(at) if *at <= now.wall => {
                return Err(SpecError::InvalidSchedule(
                    "the instant is not in the future".to_string(),
                )
                .into());
            }
            Spec::OnceAt(at) => Some(now.wall.nanos_until(*at)),
            Spec::OnceAfter(wait) => Some(*wait),
            Spec::Cron(_) | Spec::Interval(_) => None,
        };
        let lifetime = held.limits.max_lifetime;
        if let (Some(wait), Some(lifetime)) = (wait, lifetime)
            && wait >= lifetime
        {
            // Accepting it would be accepting a schedule that cannot fire.
            return Err(SpecError::InvalidSchedule(format!(
                "it would fire in {}, after the {} this workload's schedules are held",
                format_duration(wait),
                format_duration(lifetime)
            ))
            .into());
        }
        match held.entries.get(name) {
            Some(existing) if existing.declared => return Err(RegisterError::AccessDenied),
            // The same schedule again changes nothing, phase included. Unless
            // it has had its last firing: then there is nothing left to keep,
            // and "the same again" is a request for a new one. That is what a
            // one-shot's handler is doing when it registers itself again.
            Some(existing) if existing.spec == spec && !existing.spent => return Ok(false),
            Some(_) => {}
            None => {
                let registered = held.entries.values().filter(|e| !e.declared).count();
                if registered >= held.limits.max_registrations {
                    return Err(RegisterError::LimitExceeded);
                }
            }
        }
        let entry = self.entry(spec, lifetime, now);
        if let Some(held) = self.workloads.get_mut(workload) {
            held.entries.insert(name.to_string(), entry);
        }
        Ok(true)
    }

    /// Remove a registration, returning `false` if it is already absent.
    ///
    /// # Errors
    ///
    /// `AccessDenied` for a missing scheduling grant or declared name, and `Spec` for
    /// an invalid name.
    pub fn unregister(&mut self, workload: &str, name: &str) -> Result<bool, RegisterError> {
        let held = self.registrable(workload)?;
        validate_name(name)?;
        match held.entries.get(name) {
            Some(existing) if existing.declared => Err(RegisterError::AccessDenied),
            _ => Ok(held.entries.remove(name).is_some()),
        }
    }

    /// List registrations by name with next due times forecast at `now`.
    ///
    /// # Errors
    ///
    /// `AccessDenied` when the workload lacks a scheduling grant.
    pub fn list(&mut self, workload: &str, now: Now) -> Result<Vec<Registration>, RegisterError> {
        let held = self.registrable(workload)?;
        let max_firings = held.limits.max_firings;
        Ok(held
            .entries
            .iter()
            .map(|(name, entry)| Registration {
                name: name.clone(),
                spec: entry.spec.clone(),
                declared: entry.declared,
                next: entry.next(max_firings, held.busy.contains_key(name), now),
            })
            .collect())
    }

    fn registrable(&mut self, workload: &str) -> Result<&mut Workload, RegisterError> {
        match self.workloads.get_mut(workload) {
            Some(held) if held.can_register => Ok(held),
            _ => Err(RegisterError::AccessDenied),
        }
    }

    /// Return firings due at `now`, marked in flight until [`Engine::complete`] records
    /// their results.
    pub fn poll(&mut self, now: Now) -> Vec<Dispatch> {
        let reset = self
            .last_wall
            .is_some_and(|last| now.wall.nanos_until(last) > CLOCK_RESET);
        self.last_wall = Some(now.wall);

        let mut due = Vec::new();
        for (workload, held) in &mut self.workloads {
            let Workload { entries, busy, .. } = held;
            // `max-lifetime` comes first, so a schedule whose time is up is
            // removed rather than fired once more on its way out. A delivery
            // already in flight finishes; its result finds nothing to settle.
            entries.retain(|name, entry| {
                let expired = entry.expires.is_some_and(|expires| now.mono >= expires);
                if expired {
                    self.ended.push(Ended {
                        workload: workload.clone(),
                        name: name.clone(),
                        reason: EndReason::MaxLifetime,
                    });
                }
                !expired
            });
            for (name, entry) in entries {
                if reset && let (Spec::Cron(cron), Due::Wall(_)) = (&entry.spec, entry.due) {
                    // The clock was set back past what this was waiting for.
                    entry.due = cron.next_after(now.wall).map_or(Due::Never, Due::Wall);
                }
                let in_flight = busy.contains_key(name);
                if in_flight && entry.spec.is_once() {
                    // A one-shot has a single firing to give, so it waits for
                    // the handler to be free rather than being counted as
                    // missed. Only reachable when the name was registered
                    // again while its previous firing was still running.
                    continue;
                }
                let firing = match entry.advance(now) {
                    Some((scheduled_at, skipped)) => {
                        // The schedule itself came due, which outranks a
                        // redelivery of the firing before it.
                        entry.retry = None;
                        if in_flight {
                            entry.missed = entry.missed.saturating_add(skipped).saturating_add(1);
                            entry.overtaken = true;
                            continue;
                        }
                        let missed = entry.missed.saturating_add(skipped);
                        entry.missed = 0;
                        entry.overtaken = false;
                        Firing { name: name.clone(), scheduled_at, missed, attempt: 1 }
                    }
                    None => match &entry.retry {
                        Some(retry) if !in_flight && now.mono >= retry.due => {
                            let mut firing = retry.firing.clone();
                            firing.attempt += 1;
                            entry.retry = None;
                            firing
                        }
                        _ => continue,
                    },
                };
                entry.counting = firing.attempt == 1;
                busy.insert(name.clone(), entry.epoch);
                due.push(Dispatch { workload: workload.clone(), epoch: entry.epoch, firing });
            }
        }
        due
    }

    /// Record how the delivery of `dispatch` ended.
    pub fn complete(&mut self, dispatch: &Dispatch, outcome: Outcome, now: Now) -> Completion {
        let Some(held) = self.workloads.get_mut(&dispatch.workload) else {
            return Completion::Stale;
        };
        let name = &dispatch.firing.name;
        // The handler is free again whichever entry now holds the name.
        if held.busy.get(name) == Some(&dispatch.epoch) {
            held.busy.remove(name);
        }
        let Some(entry) = held.entries.get_mut(name) else {
            return Completion::Stale;
        };
        // The loop sets no timer for a schedule whose handler is running, so
        // the periods that elapsed meanwhile are counted here, where there is
        // a firing to compare them with. A one-shot is left alone: its firing
        // was waiting for the handler, and is delivered now.
        if !entry.spec.is_once()
            && let Some((_, skipped)) = entry.advance(now)
        {
            entry.retry = None;
            entry.missed = entry.missed.saturating_add(skipped).saturating_add(1);
            entry.overtaken = true;
        }
        if entry.epoch != dispatch.epoch {
            return Completion::Stale;
        }
        entry.counting = false;

        let delivered = outcome != Outcome::Undeliverable;
        let first_attempt = dispatch.firing.attempt == 1;
        // A firing counts against `max-firings` once, when it is first
        // delivered: not when it is redelivered, and not when it never
        // arrived.
        if delivered && first_attempt {
            entry.fired = entry.fired.saturating_add(1);
        }
        let was_unreachable = std::mem::replace(&mut entry.unreachable, !delivered);
        if !entry.declared && held.limits.max_firings.is_some_and(|max| entry.fired >= max) {
            entry.spent = true;
        }
        let end = match (entry.spec.is_once(), outcome) {
            (true, Outcome::Undeliverable) => Some(EndReason::Undelivered),
            (true, _) => Some(EndReason::Fired),
            (false, _) if entry.spent => Some(EndReason::MaxFirings),
            (false, _) => None,
        };
        if end.is_some() {
            // No firing follows this one, so nothing can supersede its retry.
            entry.due = Due::Never;
            entry.overtaken = false;
        }

        let completion = match outcome {
            Outcome::Handled | Outcome::Failed => Completion::Settled,
            Outcome::Undeliverable => {
                if first_attempt {
                    // Never delivered, so it joins what the next firing
                    // reports. A redelivery that could not be made is
                    // different: that firing did reach the handler once.
                    entry.missed = entry
                        .missed
                        .saturating_add(dispatch.firing.missed)
                        .saturating_add(1);
                }
                if end.is_some() {
                    Completion::Settled
                } else {
                    Completion::Undelivered { first: !was_unreachable }
                }
            }
            Outcome::Retry if dispatch.firing.attempt >= held.limits.retry_max_attempts => {
                Completion::RetriesExhausted
            }
            // The schedule came due again while this firing was being
            // handled. That firing was skipped, and this one is behind it.
            Outcome::Retry if entry.overtaken => Completion::RetrySuperseded,
            Outcome::Retry => {
                let doublings = dispatch.firing.attempt.saturating_sub(1).min(32);
                let backoff = held.limits.retry_backoff.saturating_mul(1 << doublings);
                if backoff >= entry.until_due(now).unwrap_or(u64::MAX) {
                    Completion::RetrySuperseded
                } else {
                    entry.retry = Some(Retry {
                        due: now.mono.saturating_add(backoff),
                        firing: dispatch.firing.clone(),
                    });
                    Completion::RetryIn(backoff)
                }
            }
        };

        // A spent schedule stays only as long as a redelivery is still owed.
        if let Some(reason) = end
            && !matches!(completion, Completion::RetryIn(_))
        {
            held.entries.remove(name);
            self.ended.push(Ended {
                workload: dispatch.workload.clone(),
                name: name.clone(),
                reason,
            });
        }
        completion
    }

    /// Nanoseconds until the next possible firing, or `None` if nothing is scheduled.
    pub fn next_wake(&self, now: Now) -> Option<u64> {
        self.workloads
            .values()
            .flat_map(|held| {
                held.entries
                    .iter()
                    .map(|(name, entry)| (entry, held.busy.contains_key(name)))
            })
            .flat_map(|(entry, in_flight)| {
                let regular = match entry.due {
                    // Waiting on its handler, not on a clock. Whatever comes
                    // due meanwhile is skipped or held, and the delivery
                    // finishing is what wakes the loop to settle that. A
                    // timer here would wake it every period to do nothing.
                    _ if in_flight => None,
                    // Capped, because the wait is on the monotonic clock and
                    // this deadline is on the system clock.
                    Due::Wall(_) => entry.until_due(now).map(|wait| wait.min(WALL_RESYNC)),
                    _ => entry.until_due(now),
                };
                let retry = entry
                    .retry
                    .as_ref()
                    .filter(|_| !in_flight)
                    .map(|retry| retry.due.saturating_sub(now.mono));
                // So an expired schedule leaves the listing when its time is
                // up, not whenever the loop next happens to wake.
                let expiry = entry.expires.map(|expires| expires.saturating_sub(now.mono));
                [regular, retry, expiry]
            })
            .flatten()
            .min()
    }
}

impl Entry {
    /// Nanoseconds until the schedule itself is next due.
    fn until_due(&self, now: Now) -> Option<u64> {
        match self.due {
            Due::Wall(at) => Some(now.wall.nanos_until(at)),
            Due::Mono(mark) => Some(mark.saturating_sub(now.mono)),
            Due::Never => None,
        }
    }

    /// Forecast the next due time on the system clock at `now`, or `None` if no firing
    /// remains.
    ///
    /// `in_flight` indicates a running handler for this name; a time at or before `now`
    /// indicates a firing awaiting dispatch or a one-shot waiting for that handler.
    ///
    /// The forecast cannot predict future handler duration or whether the current
    /// delivery will count toward the firing limit.
    fn next(&self, max_firings: Option<u32>, in_flight: bool, now: Now) -> Option<Wall> {
        // Its last firing has come due, or is the one being delivered.
        let begun = self.fired.saturating_add(u32::from(self.counting));
        if self.spent || (!self.declared && max_firings.is_some_and(|max| begun >= max)) {
            return None;
        }
        let (wait, at) = match (&self.spec, self.due) {
            (_, Due::Never) => return None,
            // What a repeating schedule comes due for while its handler is
            // running is skipped, so the next firing is the one after that.
            (Spec::Cron(cron), Due::Wall(due)) if in_flight && now.wall >= due => {
                let next = cron.next_after(now.wall)?;
                (now.wall.nanos_until(next), next)
            }
            (Spec::Interval(period), Due::Mono(mark)) if in_flight && now.mono >= mark => {
                let wait = period - (now.mono - mark) % period;
                (wait, now.wall.plus_nanos(wait))
            }
            (_, Due::Wall(at)) => (now.wall.nanos_until(at), at),
            (_, Due::Mono(mark)) if mark >= now.mono => {
                let wait = mark - now.mono;
                (wait, now.wall.plus_nanos(wait))
            }
            (_, Due::Mono(mark)) => (0, now.wall.minus_nanos(now.mono - mark)),
        };
        // `max-lifetime` removes a schedule before a firing due at that mark.
        if self.expires.is_some_and(|expires| now.mono.saturating_add(wait) >= expires) {
            return None;
        }
        Some(at)
    }

    /// Advance a due schedule and return its latest due time and the number of earlier
    /// occurrences skipped.
    fn advance(&mut self, now: Now) -> Option<(Wall, u32)> {
        match (&self.spec, self.due) {
            (Spec::Cron(cron), Due::Wall(first)) if now.wall >= first => {
                let mut latest = first;
                let mut skipped = 0;
                let next = loop {
                    match cron.next_after(latest) {
                        Some(next) if next <= now.wall && skipped < MAX_CATCH_UP => {
                            latest = next;
                            skipped += 1;
                        }
                        Some(next) if next <= now.wall => {
                            // Too far behind to walk. Jump to the most recent
                            // match, so the firing still names the occurrence
                            // it stands for, and leave the count where it
                            // stopped: a lower bound.
                            latest = cron.last_at_or_before(now.wall).unwrap_or(next);
                            break cron.next_after(now.wall);
                        }
                        next => break next,
                    }
                };
                self.due = next.map_or(Due::Never, Due::Wall);
                Some((latest, skipped))
            }
            (Spec::Interval(period), Due::Mono(first)) if now.mono >= first => {
                let late = now.mono - first;
                let skipped = late / period;
                let latest = first + skipped * period;
                self.due = Due::Mono(latest.saturating_add(*period));
                // The period elapsed `now.mono - latest` ago; say when that
                // was on the system clock.
                let scheduled_at = now.wall.minus_nanos(now.mono - latest);
                Some((scheduled_at, u32::try_from(skipped).unwrap_or(u32::MAX)))
            }
            (Spec::OnceAt(at), Due::Wall(_)) if now.wall >= *at => {
                self.due = Due::Never;
                self.spent = true;
                Some((*at, 0))
            }
            (Spec::OnceAfter(_), Due::Mono(mark)) if now.mono >= mark => {
                self.due = Due::Never;
                self.spent = true;
                Some((now.wall.minus_nanos(now.mono - mark), 0))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::CronSpec;

    const SEC: u64 = 1_000_000_000;
    // 2026-10-12T00:00:00Z.
    const T0: i64 = 1_791_763_200;

    /// A clock where the monotonic and system clocks advance together,
    /// `secs` seconds after `T0`.
    fn at(secs: u64) -> Now {
        Now {
            mono: 1_000 * SEC + secs * SEC,
            wall: Wall { seconds: T0 + secs as i64, nanoseconds: 0 },
        }
    }

    fn cron(expression: &str) -> Spec {
        Spec::Cron(CronSpec::parse(expression, None).unwrap())
    }

    fn bound(declared: &[(&str, Spec)], can_register: bool) -> Engine {
        let mut engine = Engine::new();
        let binding = Binding {
            declared: declared.iter().map(|(n, s)| (n.to_string(), s.clone())).collect(),
            limits: Limits::default(),
        };
        engine.bind("wl", binding, can_register, at(0));
        engine
    }

    /// The system clock reading of `at(secs)`, as a listing reports it.
    fn when(secs: u64) -> Option<Wall> {
        Some(at(secs).wall)
    }

    /// When `name` is next due, as listed at `now`.
    fn next(engine: &mut Engine, name: &str, now: Now) -> Option<Wall> {
        let listed = engine.list("wl", now).unwrap();
        listed.into_iter().find(|r| r.name == name).unwrap_or_else(|| panic!("{name} is not listed")).next
    }

    fn names(due: &[Dispatch]) -> Vec<&str> {
        due.iter().map(|d| d.firing.name.as_str()).collect()
    }

    /// Poll and settle every delivery as handled.
    fn fire(engine: &mut Engine, now: Now) -> Vec<Dispatch> {
        let due = engine.poll(now);
        for dispatch in &due {
            engine.complete(dispatch, Outcome::Handled, now);
        }
        due
    }

    #[test]
    fn interval_fires_one_period_after_it_starts_then_every_period() {
        let mut engine = bound(&[("poll", Spec::Interval(10 * SEC))], false);
        assert!(fire(&mut engine, at(0)).is_empty());
        assert!(fire(&mut engine, at(9)).is_empty());
        assert_eq!(engine.next_wake(at(9)), Some(SEC));

        let due = fire(&mut engine, at(10));
        assert_eq!(names(&due), ["poll"]);
        assert_eq!(due[0].firing.scheduled_at, at(10).wall);
        assert_eq!((due[0].firing.missed, due[0].firing.attempt), (0, 1));

        assert!(fire(&mut engine, at(19)).is_empty());
        assert_eq!(names(&fire(&mut engine, at(20))), ["poll"]);
    }

    #[test]
    fn interval_keeps_its_rate_when_the_loop_wakes_late() {
        let mut engine = bound(&[("poll", Spec::Interval(10 * SEC))], false);
        // Woken 3s late: the firing is the one due at 10, and the next is
        // still due at 20, not 23.
        let due = fire(&mut engine, at(13));
        assert_eq!(due[0].firing.scheduled_at, at(10).wall);
        assert_eq!(engine.next_wake(at(13)), Some(7 * SEC));
    }

    #[test]
    fn interval_counts_periods_it_slept_through() {
        let mut engine = bound(&[("poll", Spec::Interval(10 * SEC))], false);
        // Due at 10, 20, 30 and 40; only the last is delivered.
        let due = fire(&mut engine, at(45));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].firing.scheduled_at, at(40).wall);
        assert_eq!(due[0].firing.missed, 3);
        assert_eq!(engine.next_wake(at(45)), Some(5 * SEC));
    }

    #[test]
    fn cron_fires_at_the_matching_instant() {
        let mut engine = bound(&[("quarter", cron("*/15 * * * *"))], false);
        assert_eq!(engine.next_wake(at(890)), Some(10 * SEC));
        assert!(fire(&mut engine, at(899)).is_empty());
        let due = fire(&mut engine, at(900));
        assert_eq!(names(&due), ["quarter"]);
        assert_eq!(due[0].firing.scheduled_at, at(900).wall);
        assert!(fire(&mut engine, at(901)).is_empty());
        assert_eq!(names(&fire(&mut engine, at(1800))), ["quarter"]);
    }

    #[test]
    fn cron_scheduled_at_is_the_match_not_the_wake_time() {
        let mut engine = bound(&[("minutely", cron("* * * * *"))], false);
        let due = fire(&mut engine, at(62));
        assert_eq!(due[0].firing.scheduled_at, at(60).wall);
    }

    #[test]
    fn cron_counts_matches_it_slept_through() {
        let mut engine = bound(&[("minutely", cron("* * * * *"))], false);
        // Matches at 60, 120, 180, 240, 300.
        let due = fire(&mut engine, at(305));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].firing.scheduled_at, at(300).wall);
        assert_eq!(due[0].firing.missed, 4);
    }

    #[test]
    fn cron_sleep_is_capped_so_a_corrected_clock_is_noticed() {
        let engine = bound(&[("nightly", cron("0 2 * * *"))], false);
        assert_eq!(engine.next_wake(at(0)), Some(WALL_RESYNC));
    }

    #[test]
    fn cron_follows_the_system_clock_when_it_jumps() {
        let mut engine = bound(&[("nightly", cron("0 2 * * *"))], false);
        // The monotonic clock has moved one second; the system clock was set
        // forward to 02:00.
        let jumped = Now { mono: at(1).mono, wall: at(2 * 3600).wall };
        assert_eq!(names(&fire(&mut engine, jumped)), ["nightly"]);
    }

    #[test]
    fn a_schedule_never_overlaps_itself() {
        let mut engine = bound(&[("poll", Spec::Interval(10 * SEC))], false);
        let first = engine.poll(at(10));
        assert_eq!(first.len(), 1);
        // Still being handled when the next two periods elapse.
        assert!(engine.poll(at(20)).is_empty());
        assert!(engine.poll(at(30)).is_empty());
        engine.complete(&first[0], Outcome::Handled, at(35));
        // The next delivery reports the two it stood in for.
        let next = engine.poll(at(40));
        assert_eq!(next[0].firing.missed, 2);
        engine.complete(&next[0], Outcome::Handled, at(41));
        assert_eq!(engine.poll(at(50))[0].firing.missed, 0);
    }

    #[test]
    fn different_schedules_fire_together() {
        let mut engine = bound(
            &[("a", Spec::Interval(10 * SEC)), ("b", Spec::Interval(10 * SEC))],
            false,
        );
        let due = engine.poll(at(10));
        assert_eq!(names(&due), ["a", "b"]);
        // `a` still running does not hold `b` back.
        engine.complete(&due[1], Outcome::Handled, at(11));
        assert_eq!(names(&engine.poll(at(20))), ["b"]);
    }

    #[test]
    fn retry_redelivers_with_backoff_and_a_rising_attempt() {
        let mut engine = bound(&[("poll", Spec::Interval(60 * SEC))], false);
        let first = engine.poll(at(60));
        assert_eq!(engine.complete(&first[0], Outcome::Retry, at(60)), Completion::RetryIn(SEC));
        assert_eq!(engine.next_wake(at(60)), Some(SEC));
        assert!(engine.poll(at(60)).is_empty());

        let second = engine.poll(at(61));
        assert_eq!(second[0].firing.attempt, 2);
        assert_eq!(second[0].firing.scheduled_at, first[0].firing.scheduled_at);
        // The backoff doubles.
        assert_eq!(engine.complete(&second[0], Outcome::Retry, at(61)), Completion::RetryIn(2 * SEC));
        let third = engine.poll(at(63));
        assert_eq!(third[0].firing.attempt, 3);
        // Three attempts is the default ceiling.
        assert_eq!(engine.complete(&third[0], Outcome::Retry, at(63)), Completion::RetriesExhausted);
        assert!(engine.poll(at(70)).is_empty());
        // The schedule itself is unaffected.
        let next = engine.poll(at(120));
        assert_eq!((next[0].firing.attempt, next[0].firing.missed), (1, 0));
    }

    #[test]
    fn retry_gives_way_to_the_next_firing() {
        let mut engine = Engine::new();
        let binding = Binding {
            declared: [("poll".to_string(), Spec::Interval(2 * SEC))].into(),
            limits: Limits { retry_backoff: 5 * SEC, ..Limits::default() },
        };
        engine.bind("wl", binding, false, at(0));
        let first = engine.poll(at(2));
        // The next firing is due in 2s; a 5s backoff would land after it.
        assert_eq!(engine.complete(&first[0], Outcome::Retry, at(2)), Completion::RetrySuperseded);
        let next = engine.poll(at(4));
        assert_eq!((next[0].firing.attempt, next[0].firing.missed), (1, 0));
    }

    #[test]
    fn a_pending_retry_is_dropped_when_the_schedule_comes_due() {
        let mut engine = bound(&[("poll", Spec::Interval(10 * SEC))], false);
        let first = engine.poll(at(10));
        assert_eq!(engine.complete(&first[0], Outcome::Retry, at(18)), Completion::RetryIn(SEC));
        // The loop wakes after both the retry and the next period are due.
        let due = engine.poll(at(21));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].firing.attempt, 1);
        engine.complete(&due[0], Outcome::Handled, at(21));
        assert!(engine.poll(at(22)).is_empty());
    }

    #[test]
    fn failed_is_not_redelivered() {
        let mut engine = bound(&[("poll", Spec::Interval(10 * SEC))], false);
        let first = engine.poll(at(10));
        assert_eq!(engine.complete(&first[0], Outcome::Failed, at(10)), Completion::Settled);
        assert!(engine.poll(at(15)).is_empty());
        assert_eq!(engine.poll(at(20))[0].firing.missed, 0);
    }

    #[test]
    fn an_undeliverable_firing_is_reported_as_missed() {
        let mut engine = bound(&[("poll", Spec::Interval(10 * SEC))], false);
        for secs in [10, 20] {
            let due = engine.poll(at(secs));
            engine.complete(&due[0], Outcome::Undeliverable, at(secs));
        }
        assert_eq!(engine.poll(at(30))[0].firing.missed, 2);
    }

    #[test]
    fn register_needs_the_scheduler_grant() {
        let mut engine = bound(&[], false);
        assert_eq!(
            engine.register("wl", "poll", Spec::Interval(10 * SEC), at(0)),
            Err(RegisterError::AccessDenied)
        );
        assert_eq!(engine.unregister("wl", "poll"), Err(RegisterError::AccessDenied));
        assert_eq!(engine.list("wl", at(0)), Err(RegisterError::AccessDenied));
        assert_eq!(
            engine.register("stranger", "poll", Spec::Interval(10 * SEC), at(0)),
            Err(RegisterError::AccessDenied)
        );
    }

    #[test]
    fn registered_schedules_fire_and_can_be_removed() {
        let mut engine = bound(&[], true);
        assert_eq!(engine.next_wake(at(0)), None);
        engine.register("wl", "poll", Spec::Interval(5 * SEC), at(100)).unwrap();
        assert_eq!(engine.next_wake(at(100)), Some(5 * SEC));
        assert_eq!(names(&fire(&mut engine, at(105))), ["poll"]);

        assert_eq!(engine.unregister("wl", "poll"), Ok(true));
        assert!(fire(&mut engine, at(200)).is_empty());
        assert_eq!(engine.next_wake(at(200)), None);
        // Removing it again is not an error, and reports nothing changed.
        assert_eq!(engine.unregister("wl", "poll"), Ok(false));
    }

    #[test]
    fn registering_the_same_schedule_again_keeps_its_phase() {
        let mut engine = bound(&[], true);
        assert_eq!(engine.register("wl", "poll", Spec::Interval(10 * SEC), at(0)), Ok(true));
        // Accepted, and reported as having changed nothing.
        assert_eq!(engine.register("wl", "poll", Spec::Interval(10 * SEC), at(7)), Ok(false));
        assert_eq!(names(&fire(&mut engine, at(10))), ["poll"]);
    }

    #[test]
    fn registering_a_different_schedule_restarts_it() {
        let mut engine = bound(&[], true);
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(0)).unwrap();
        engine.register("wl", "poll", Spec::Interval(20 * SEC), at(7)).unwrap();
        assert!(fire(&mut engine, at(10)).is_empty());
        assert!(fire(&mut engine, at(26)).is_empty());
        assert_eq!(names(&fire(&mut engine, at(27))), ["poll"]);
    }

    #[test]
    fn a_result_for_a_replaced_schedule_is_ignored() {
        let mut engine = bound(&[], true);
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(0)).unwrap();
        let old = engine.poll(at(10));
        engine.register("wl", "poll", Spec::Interval(5 * SEC), at(11)).unwrap();
        assert_eq!(engine.complete(&old[0], Outcome::Retry, at(12)), Completion::Stale);
        // The replacement was never in flight, so it fires on time.
        assert_eq!(names(&engine.poll(at(16))), ["poll"]);

        engine.unbind("wl");
        assert_eq!(engine.complete(&old[0], Outcome::Handled, at(17)), Completion::Stale);
    }

    #[test]
    fn register_validates_before_it_accepts() {
        let mut engine = bound(&[], true);
        let refused = |result| matches!(result, Err(RegisterError::Spec(_)));
        assert!(refused(engine.register("wl", "", Spec::Interval(10 * SEC), at(0))));
        assert!(refused(engine.register("wl", "bad name", Spec::Interval(10 * SEC), at(0))));
        assert!(refused(engine.register("wl", "poll", Spec::Interval(0), at(0))));
        // Shorter than the default one-second floor.
        assert!(refused(engine.register("wl", "poll", Spec::Interval(SEC / 2), at(0))));
        assert_eq!(engine.list("wl", at(0)), Ok(vec![]));
    }

    #[test]
    fn registrations_are_capped_and_declared_ones_do_not_count() {
        let mut engine = Engine::new();
        let binding = Binding {
            declared: [("declared".to_string(), cron("0 2 * * *"))].into(),
            limits: Limits { max_registrations: 2, ..Limits::default() },
        };
        engine.bind("wl", binding, true, at(0));
        engine.register("wl", "a", Spec::Interval(10 * SEC), at(0)).unwrap();
        engine.register("wl", "b", Spec::Interval(10 * SEC), at(0)).unwrap();
        assert_eq!(
            engine.register("wl", "c", Spec::Interval(10 * SEC), at(0)),
            Err(RegisterError::LimitExceeded)
        );
        // Replacing one already held is not a new registration.
        assert_eq!(engine.register("wl", "a", Spec::Interval(20 * SEC), at(0)), Ok(true));
        engine.unregister("wl", "b").unwrap();
        assert_eq!(engine.register("wl", "c", Spec::Interval(10 * SEC), at(0)), Ok(true));
    }

    #[test]
    fn declared_schedules_are_listed_but_not_the_guests_to_change() {
        let mut engine = bound(&[("nightly", cron("0 2 * * *"))], true);
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(0)).unwrap();
        assert_eq!(
            engine.list("wl", at(0)).unwrap(),
            [
                Registration {
                    name: "nightly".into(),
                    spec: cron("0 2 * * *"),
                    declared: true,
                    next: when(2 * 3600),
                },
                Registration {
                    name: "poll".into(),
                    spec: Spec::Interval(10 * SEC),
                    declared: false,
                    next: when(10),
                },
            ]
        );
        assert_eq!(
            engine.register("wl", "nightly", Spec::Interval(10 * SEC), at(0)),
            Err(RegisterError::AccessDenied)
        );
        assert_eq!(engine.unregister("wl", "nightly"), Err(RegisterError::AccessDenied));
    }

    #[test]
    fn workloads_are_kept_apart() {
        let mut engine = bound(&[], true);
        engine.bind("other", Binding::default(), true, at(0));
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(0)).unwrap();
        assert_eq!(engine.list("other", at(0)), Ok(vec![]));
        assert_eq!(engine.unregister("other", "poll"), Ok(false));
        let due = engine.poll(at(10));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].workload, "wl");
    }

    #[test]
    fn a_replayed_bind_keeps_timing_and_registrations() {
        let declared = [("poll", Spec::Interval(10 * SEC))];
        let mut engine = bound(&declared, true);
        engine.register("wl", "mine", Spec::Interval(10 * SEC), at(0)).unwrap();

        let binding = Binding {
            declared: declared.iter().map(|(n, s)| (n.to_string(), s.clone())).collect(),
            limits: Limits::default(),
        };
        engine.bind("wl", binding, true, at(7));
        // Neither interval restarted at 7.
        assert_eq!(names(&fire(&mut engine, at(10))), ["mine", "poll"]);
    }

    #[test]
    fn a_rebind_takes_the_new_declarations() {
        let mut engine = bound(&[("old", Spec::Interval(10 * SEC)), ("kept", cron("0 2 * * *"))], true);
        engine.register("wl", "new", Spec::Interval(10 * SEC), at(0)).unwrap();

        let binding = Binding {
            declared: [
                ("kept".to_string(), cron("0 3 * * *")),
                ("new".to_string(), Spec::Interval(30 * SEC)),
            ]
            .into(),
            limits: Limits::default(),
        };
        engine.bind("wl", binding, true, at(5));
        assert_eq!(
            engine.list("wl", at(5)).unwrap(),
            [
                Registration {
                    name: "kept".into(),
                    spec: cron("0 3 * * *"),
                    declared: true,
                    next: when(3 * 3600),
                },
                // The binding now owns the name the guest had registered, and
                // its period runs from the bind.
                Registration {
                    name: "new".into(),
                    spec: Spec::Interval(30 * SEC),
                    declared: true,
                    next: when(35),
                },
            ]
        );
    }

    #[test]
    fn unbind_stops_everything_and_tolerates_strangers() {
        let mut engine = bound(&[("poll", Spec::Interval(10 * SEC))], true);
        engine.unbind("wl");
        engine.unbind("never-bound");
        assert!(engine.poll(at(100)).is_empty());
        assert_eq!(engine.next_wake(at(100)), None);
    }

    /// A workload that may register, under limits of the test's choosing.
    fn limited(limits: Limits) -> Engine {
        let mut engine = Engine::new();
        engine.bind("wl", Binding { declared: BTreeMap::new(), limits }, true, at(0));
        engine
    }

    fn ended(engine: &mut Engine) -> Vec<(String, EndReason)> {
        engine.take_ended().into_iter().map(|e| (e.name, e.reason)).collect()
    }

    fn listed(engine: &mut Engine) -> Vec<String> {
        engine.list("wl", at(0)).unwrap().into_iter().map(|r| r.name).collect()
    }

    #[test]
    fn once_after_fires_a_single_time_and_is_gone() {
        let mut engine = bound(&[], true);
        engine.register("wl", "check", Spec::OnceAfter(10 * SEC), at(0)).unwrap();
        assert_eq!(engine.next_wake(at(0)), Some(10 * SEC));
        assert!(fire(&mut engine, at(9)).is_empty());

        let due = fire(&mut engine, at(10));
        assert_eq!(names(&due), ["check"]);
        assert_eq!(due[0].firing.scheduled_at, at(10).wall);
        assert_eq!((due[0].firing.missed, due[0].firing.attempt), (0, 1));

        assert_eq!(ended(&mut engine), [("check".to_string(), EndReason::Fired)]);
        assert!(listed(&mut engine).is_empty());
        assert!(fire(&mut engine, at(100)).is_empty());
        assert_eq!(engine.next_wake(at(100)), None);
    }

    #[test]
    fn once_after_is_listed_until_its_firing_settles() {
        let mut engine = bound(&[], true);
        engine.register("wl", "check", Spec::OnceAfter(10 * SEC), at(0)).unwrap();
        let due = engine.poll(at(10));
        // Still being handled: held, and not due a second time.
        assert_eq!(listed(&mut engine), ["check"]);
        assert!(engine.poll(at(30)).is_empty());
        engine.complete(&due[0], Outcome::Failed, at(31));
        assert!(listed(&mut engine).is_empty());
    }

    #[test]
    fn once_after_reports_when_the_delay_elapsed_if_the_loop_wakes_late() {
        let mut engine = bound(&[], true);
        engine.register("wl", "check", Spec::OnceAfter(10 * SEC), at(0)).unwrap();
        let due = fire(&mut engine, at(14));
        assert_eq!(due[0].firing.scheduled_at, at(10).wall);
        assert_eq!(due[0].firing.missed, 0);
    }

    #[test]
    fn once_at_fires_at_its_instant_on_the_system_clock() {
        let mut engine = bound(&[], true);
        engine.register("wl", "launch", Spec::OnceAt(at(3_600).wall), at(0)).unwrap();
        // A deadline on the system clock, so the sleep is capped like cron's.
        assert_eq!(engine.next_wake(at(0)), Some(WALL_RESYNC));
        assert!(fire(&mut engine, at(3_599)).is_empty());
        let due = fire(&mut engine, at(3_602));
        assert_eq!(due[0].firing.scheduled_at, at(3_600).wall);
        assert!(listed(&mut engine).is_empty());
    }

    #[test]
    fn once_at_refuses_an_instant_that_has_passed() {
        let mut engine = bound(&[], true);
        for past in [at(50).wall, at(100).wall] {
            assert!(matches!(
                engine.register("wl", "late", Spec::OnceAt(past), at(100)),
                Err(RegisterError::Spec(SpecError::InvalidSchedule(_)))
            ));
        }
        assert!(listed(&mut engine).is_empty());
    }

    #[test]
    fn registering_a_pending_one_shot_again_does_not_restart_it() {
        let mut engine = bound(&[], true);
        engine.register("wl", "check", Spec::OnceAfter(10 * SEC), at(0)).unwrap();
        engine.register("wl", "check", Spec::OnceAfter(10 * SEC), at(7)).unwrap();
        assert_eq!(names(&fire(&mut engine, at(10))), ["check"]);
        // Once spent, the same call schedules a new one.
        engine.register("wl", "check", Spec::OnceAfter(10 * SEC), at(11)).unwrap();
        assert!(fire(&mut engine, at(20)).is_empty());
        assert_eq!(names(&fire(&mut engine, at(21))), ["check"]);
    }

    #[test]
    fn a_one_shot_is_retried_with_nothing_to_supersede_it() {
        // A backoff this long would lose to the next firing of an interval.
        let mut engine = limited(Limits { retry_backoff: 60 * SEC, ..Limits::default() });
        engine.register("wl", "check", Spec::OnceAfter(10 * SEC), at(0)).unwrap();

        let first = engine.poll(at(10));
        assert_eq!(engine.complete(&first[0], Outcome::Retry, at(10)), Completion::RetryIn(60 * SEC));
        assert_eq!(listed(&mut engine), ["check"]);
        let second = engine.poll(at(70));
        assert_eq!(second[0].firing.attempt, 2);
        assert_eq!(second[0].firing.scheduled_at, first[0].firing.scheduled_at);
        assert_eq!(engine.complete(&second[0], Outcome::Retry, at(70)), Completion::RetryIn(120 * SEC));
        let third = engine.poll(at(190));
        assert_eq!(engine.complete(&third[0], Outcome::Retry, at(190)), Completion::RetriesExhausted);

        assert_eq!(ended(&mut engine), [("check".to_string(), EndReason::Fired)]);
        assert!(listed(&mut engine).is_empty());
    }

    #[test]
    fn a_one_shot_that_cannot_be_delivered_is_dropped() {
        let mut engine = bound(&[], true);
        engine.register("wl", "check", Spec::OnceAfter(10 * SEC), at(0)).unwrap();
        let due = engine.poll(at(10));
        engine.complete(&due[0], Outcome::Undeliverable, at(10));
        assert_eq!(ended(&mut engine), [("check".to_string(), EndReason::Undelivered)]);
        assert!(engine.poll(at(100)).is_empty());
    }

    #[test]
    fn a_one_shot_counts_against_max_registrations_until_it_fires() {
        let mut engine = limited(Limits { max_registrations: 1, ..Limits::default() });
        engine.register("wl", "check", Spec::OnceAfter(10 * SEC), at(0)).unwrap();
        assert_eq!(
            engine.register("wl", "other", Spec::Interval(10 * SEC), at(0)),
            Err(RegisterError::LimitExceeded)
        );
        fire(&mut engine, at(10));
        assert_eq!(engine.register("wl", "other", Spec::Interval(10 * SEC), at(10)), Ok(true));
    }

    #[test]
    fn max_firings_removes_a_schedule_after_its_last_firing() {
        let mut engine = limited(Limits { max_firings: Some(3), ..Limits::default() });
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(0)).unwrap();
        for secs in [10, 20] {
            assert_eq!(names(&fire(&mut engine, at(secs))), ["poll"]);
            assert!(ended(&mut engine).is_empty());
        }
        assert_eq!(names(&fire(&mut engine, at(30))), ["poll"]);
        assert_eq!(ended(&mut engine), [("poll".to_string(), EndReason::MaxFirings)]);
        assert!(listed(&mut engine).is_empty());
        assert!(fire(&mut engine, at(40)).is_empty());
        // Registering it again is a new schedule with a fresh count.
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(40)).unwrap();
        assert_eq!(names(&fire(&mut engine, at(50))), ["poll"]);
        assert!(ended(&mut engine).is_empty());
    }

    #[test]
    fn max_firings_counts_firings_not_deliveries() {
        let mut engine = limited(Limits { max_firings: Some(2), ..Limits::default() });
        engine.register("wl", "poll", Spec::Interval(60 * SEC), at(0)).unwrap();

        // One firing, delivered twice.
        let first = engine.poll(at(60));
        engine.complete(&first[0], Outcome::Retry, at(60));
        let again = engine.poll(at(61));
        assert_eq!(again[0].firing.attempt, 2);
        engine.complete(&again[0], Outcome::Handled, at(61));
        assert_eq!(listed(&mut engine), ["poll"]);

        // A firing that never arrived is not one of the two either.
        let lost = engine.poll(at(120));
        engine.complete(&lost[0], Outcome::Undeliverable, at(120));
        assert_eq!(listed(&mut engine), ["poll"]);

        fire(&mut engine, at(180));
        assert_eq!(ended(&mut engine), [("poll".to_string(), EndReason::MaxFirings)]);
    }

    #[test]
    fn the_last_firing_under_max_firings_can_still_be_retried() {
        let mut engine = limited(Limits { max_firings: Some(1), ..Limits::default() });
        engine.register("wl", "poll", Spec::Interval(2 * SEC), at(0)).unwrap();
        let first = engine.poll(at(2));
        // The next period would be due before a 1s backoff... but there is no
        // next period, so the retry stands and the schedule is held for it.
        assert_eq!(engine.complete(&first[0], Outcome::Retry, at(3)), Completion::RetryIn(SEC));
        assert_eq!(listed(&mut engine), ["poll"]);
        let again = engine.poll(at(4));
        assert_eq!((names(&again), again[0].firing.attempt), (vec!["poll"], 2));
        engine.complete(&again[0], Outcome::Handled, at(4));
        assert_eq!(ended(&mut engine), [("poll".to_string(), EndReason::MaxFirings)]);
        assert!(engine.poll(at(10)).is_empty());
    }

    #[test]
    fn max_firings_does_not_apply_to_declared_schedules() {
        let mut engine = Engine::new();
        let binding = Binding {
            declared: [("beat".to_string(), Spec::Interval(10 * SEC))].into(),
            limits: Limits { max_firings: Some(1), max_lifetime: Some(15 * SEC), ..Limits::default() },
        };
        engine.bind("wl", binding, true, at(0));
        for secs in [10, 20, 30] {
            assert_eq!(names(&fire(&mut engine, at(secs))), ["beat"]);
        }
        assert!(ended(&mut engine).is_empty());
    }

    #[test]
    fn max_lifetime_removes_a_schedule_without_a_last_firing() {
        let mut engine = limited(Limits { max_lifetime: Some(25 * SEC), ..Limits::default() });
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(0)).unwrap();
        fire(&mut engine, at(10));
        fire(&mut engine, at(20));
        // The loop is woken for the expiry itself, not only the next period.
        assert_eq!(engine.next_wake(at(20)), Some(5 * SEC));
        assert!(fire(&mut engine, at(25)).is_empty());
        assert_eq!(ended(&mut engine), [("poll".to_string(), EndReason::MaxLifetime)]);
        assert!(listed(&mut engine).is_empty());
        assert!(fire(&mut engine, at(30)).is_empty());
    }

    #[test]
    fn max_lifetime_is_not_extended_by_registering_the_same_schedule() {
        let mut engine = limited(Limits { max_lifetime: Some(25 * SEC), ..Limits::default() });
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(0)).unwrap();
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(20)).unwrap();
        assert!(fire(&mut engine, at(30)).is_empty());
        assert!(listed(&mut engine).is_empty());
        // Gone, so this one is new and gets a lifetime of its own.
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(30)).unwrap();
        assert_eq!(names(&fire(&mut engine, at(40))), ["poll"]);
    }

    #[test]
    fn max_lifetime_restarts_when_the_schedule_is_replaced() {
        let mut engine = limited(Limits { max_lifetime: Some(25 * SEC), ..Limits::default() });
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(0)).unwrap();
        engine.register("wl", "poll", Spec::Interval(5 * SEC), at(20)).unwrap();
        assert_eq!(names(&fire(&mut engine, at(25))), ["poll"]);
        assert_eq!(names(&fire(&mut engine, at(40))), ["poll"]);
        assert!(fire(&mut engine, at(45)).is_empty());
    }

    #[test]
    fn a_delivery_in_flight_when_the_lifetime_ends_is_left_to_finish() {
        let mut engine = limited(Limits { max_lifetime: Some(15 * SEC), ..Limits::default() });
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(0)).unwrap();
        let running = engine.poll(at(10));
        assert!(engine.poll(at(15)).is_empty());
        // Asking for a retry no longer schedules one.
        assert_eq!(engine.complete(&running[0], Outcome::Retry, at(16)), Completion::Stale);
        assert!(engine.poll(at(30)).is_empty());
    }

    #[test]
    fn a_one_shot_that_would_fire_after_its_lifetime_is_refused() {
        let mut engine = limited(Limits { max_lifetime: Some(60 * SEC), ..Limits::default() });
        let refused = |result| matches!(result, Err(RegisterError::Spec(SpecError::InvalidSchedule(_))));
        assert!(refused(engine.register("wl", "a", Spec::OnceAfter(60 * SEC), at(0))));
        assert!(refused(engine.register("wl", "b", Spec::OnceAt(at(90).wall), at(0))));
        assert_eq!(engine.register("wl", "c", Spec::OnceAfter(59 * SEC), at(0)), Ok(true));
        assert_eq!(engine.register("wl", "d", Spec::OnceAt(at(30).wall), at(0)), Ok(true));
        assert_eq!(names(&fire(&mut engine, at(30))), ["d"]);
        assert_eq!(names(&fire(&mut engine, at(59))), ["c"]);
    }

    /// A reading where the two clocks are set independently, in seconds.
    fn clock(mono_secs: u64, wall_secs: i64) -> Now {
        Now {
            mono: 1_000 * SEC + mono_secs * SEC,
            wall: Wall { seconds: T0 + wall_secs, nanoseconds: 0 },
        }
    }

    fn rebind(engine: &mut Engine, limits: Limits, can_register: bool, now: Now) {
        engine.bind("wl", Binding { declared: BTreeMap::new(), limits }, can_register, now);
    }

    #[test]
    fn a_one_shot_handler_can_register_itself_again() {
        let mut engine = bound(&[], true);
        engine.register("wl", "check", Spec::OnceAfter(10 * SEC), at(0)).unwrap();
        let first = engine.poll(at(10));

        // From inside the handler: the same name, the same delay. The firing
        // being handled is spent, so this is a new schedule, not a no-op.
        assert_eq!(engine.register("wl", "check", Spec::OnceAfter(10 * SEC), at(11)), Ok(true));
        assert_eq!(engine.complete(&first[0], Outcome::Handled, at(12)), Completion::Stale);
        assert_eq!(listed(&mut engine), ["check"]);

        assert!(engine.poll(at(20)).is_empty());
        let second = engine.poll(at(21));
        assert_eq!((names(&second), second[0].firing.attempt), (vec!["check"], 1));
        engine.complete(&second[0], Outcome::Handled, at(21));
        assert!(listed(&mut engine).is_empty());
    }

    #[test]
    fn a_schedule_at_max_firings_can_be_registered_again_from_its_last_firing() {
        let mut engine = limited(Limits { max_firings: Some(1), ..Limits::default() });
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(0)).unwrap();
        let last = engine.poll(at(10));
        // It asks for a retry, so it is spent but still held.
        assert_eq!(engine.complete(&last[0], Outcome::Retry, at(10)), Completion::RetryIn(SEC));
        assert_eq!(engine.register("wl", "poll", Spec::Interval(10 * SEC), at(10)), Ok(true));
        // A fresh schedule: the old one's retry went with it.
        assert!(engine.poll(at(11)).is_empty());
        assert_eq!(names(&fire(&mut engine, at(20))), ["poll"]);
    }

    #[test]
    fn a_retry_is_refused_once_the_schedule_came_due_during_the_attempt() {
        let mut engine = bound(&[("poll", Spec::Interval(10 * SEC))], false);
        let first = engine.poll(at(10));
        assert_eq!(engine.complete(&first[0], Outcome::Retry, at(16)), Completion::RetryIn(SEC));
        let second = engine.poll(at(17));
        assert_eq!(second[0].firing.attempt, 2);
        // The period elapses while the second attempt is still running.
        assert!(engine.poll(at(20)).is_empty());
        // The backoff would fit before the period after that, but the firing
        // is already one behind.
        assert_eq!(engine.complete(&second[0], Outcome::Retry, at(23)), Completion::RetrySuperseded);
        assert!(engine.poll(at(25)).is_empty());
        let next = engine.poll(at(30));
        assert_eq!((next[0].firing.attempt, next[0].firing.missed), (1, 1));
        // The fresh firing is not held to what happened to the last one.
        assert_eq!(engine.complete(&next[0], Outcome::Retry, at(30)), Completion::RetryIn(SEC));
    }

    #[test]
    fn replacing_a_schedule_does_not_let_it_overlap_the_firing_in_flight() {
        let mut engine = bound(&[], true);
        engine.register("wl", "poll", Spec::Interval(60 * SEC), at(0)).unwrap();
        let old = engine.poll(at(60));
        engine.register("wl", "poll", Spec::Interval(SEC), at(61)).unwrap();
        // Due at 62 and 63, with the old handler still running.
        assert!(engine.poll(at(62)).is_empty());
        assert!(engine.poll(at(63)).is_empty());
        assert_eq!(engine.complete(&old[0], Outcome::Handled, at(64)), Completion::Stale);
        let next = engine.poll(at(65));
        assert_eq!(names(&next), ["poll"]);
        assert_eq!(next[0].firing.missed, 3);
    }

    #[test]
    fn removing_and_registering_again_does_not_overlap_either() {
        let mut engine = bound(&[], true);
        engine.register("wl", "poll", Spec::Interval(60 * SEC), at(0)).unwrap();
        let old = engine.poll(at(60));
        engine.unregister("wl", "poll").unwrap();
        engine.register("wl", "poll", Spec::Interval(SEC), at(61)).unwrap();
        assert!(engine.poll(at(62)).is_empty());
        engine.complete(&old[0], Outcome::Handled, at(62));
        assert_eq!(names(&engine.poll(at(63))), ["poll"]);
    }

    #[test]
    fn a_one_shot_waits_for_the_firing_in_flight_instead_of_being_lost() {
        let mut engine = bound(&[], true);
        engine.register("wl", "check", Spec::OnceAfter(10 * SEC), at(0)).unwrap();
        let old = engine.poll(at(10));
        engine.register("wl", "check", Spec::OnceAfter(SEC), at(11)).unwrap();
        // Due at 12, but the previous firing's handler is still running.
        assert!(engine.poll(at(12)).is_empty());
        // Nothing to set a timer for: the delivery finishing wakes the loop.
        assert_eq!(engine.next_wake(at(12)), None);
        engine.complete(&old[0], Outcome::Handled, at(20));
        let waited = engine.poll(at(20));
        assert_eq!(names(&waited), ["check"]);
        assert_eq!(waited[0].firing.scheduled_at, at(12).wall);
    }

    #[test]
    fn cron_far_behind_fires_for_the_latest_match_with_a_capped_count() {
        let mut engine = bound(&[("minutely", cron("* * * * *"))], false);
        // Seventy hours on, as after a laptop slept through a weekend.
        let woke = at(70 * 3_600 + 30);
        let due = engine.poll(woke);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].firing.scheduled_at, at(70 * 3_600).wall);
        // 4,199 were skipped; the count stops where the walk did.
        assert_eq!(due[0].firing.missed, MAX_CATCH_UP);
        engine.complete(&due[0], Outcome::Handled, woke);
        assert_eq!(engine.next_wake(woke), Some(30 * SEC));
    }

    #[test]
    fn cron_is_rescheduled_when_the_clock_is_set_far_back() {
        let mut engine = Engine::new();
        let binding = Binding {
            declared: [("minutely".to_string(), cron("* * * * *"))].into(),
            limits: Limits::default(),
        };
        // 05:00, with the next match at 05:01.
        engine.bind("wl", binding, false, clock(0, 5 * 3_600));
        assert!(engine.poll(clock(0, 5 * 3_600)).is_empty());

        // A second later the clock reads 01:00.
        assert!(engine.poll(clock(1, 3_600)).is_empty());
        assert_eq!(engine.next_wake(clock(1, 3_600)), Some(WALL_RESYNC));
        // It fires at 01:01 by the new clock, not four hours later.
        let due = engine.poll(clock(61, 3_660));
        assert_eq!(names(&due), ["minutely"]);
        assert_eq!(due[0].firing.scheduled_at, clock(0, 3_660).wall);
    }

    #[test]
    fn cron_waits_out_a_small_step_back_instead_of_repeating_itself() {
        let mut engine = Engine::new();
        let binding = Binding {
            declared: [("minutely".to_string(), cron("* * * * *"))].into(),
            limits: Limits::default(),
        };
        engine.bind("wl", binding, false, clock(0, 5 * 3_600));
        assert!(engine.poll(clock(0, 5 * 3_600)).is_empty());

        // Back one hour, as at the end of daylight saving: 04:00 to 05:00 has
        // already been run once.
        assert!(engine.poll(clock(1, 4 * 3_600)).is_empty());
        assert!(engine.poll(clock(61, 4 * 3_600 + 60)).is_empty());
        let due = engine.poll(clock(3_661, 5 * 3_600 + 60));
        assert_eq!(due[0].firing.scheduled_at, clock(0, 5 * 3_600 + 60).wall);
    }

    #[test]
    fn a_rebind_without_the_scheduler_grant_drops_registered_schedules() {
        let mut engine = bound(&[("beat", Spec::Interval(10 * SEC))], true);
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(0)).unwrap();

        let binding = Binding {
            declared: [("beat".to_string(), Spec::Interval(10 * SEC))].into(),
            limits: Limits::default(),
        };
        engine.bind("wl", binding, false, at(5));
        assert_eq!(ended(&mut engine), [("poll".to_string(), EndReason::Rebound)]);
        // Only the declared schedule is left to fire.
        assert_eq!(names(&fire(&mut engine, at(10))), ["beat"]);
    }

    #[test]
    fn a_rebind_with_a_higher_floor_drops_what_it_rules_out() {
        let mut engine = bound(&[], true);
        engine.register("wl", "fast", Spec::Interval(SEC), at(0)).unwrap();
        engine.register("wl", "slow", Spec::Interval(10 * SEC), at(0)).unwrap();

        rebind(&mut engine, Limits { min_interval: 5 * SEC, ..Limits::default() }, true, at(0));
        assert_eq!(ended(&mut engine), [("fast".to_string(), EndReason::Rebound)]);
        assert_eq!(listed(&mut engine), ["slow"]);
    }

    #[test]
    fn a_rebind_applies_a_new_max_lifetime_to_what_is_registered() {
        let mut engine = bound(&[], true);
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(0)).unwrap();

        rebind(&mut engine, Limits { max_lifetime: Some(20 * SEC), ..Limits::default() }, true, at(10));
        assert_eq!(names(&fire(&mut engine, at(20))), ["poll"]);
        assert!(fire(&mut engine, at(30)).is_empty());
        assert_eq!(ended(&mut engine), [("poll".to_string(), EndReason::MaxLifetime)]);
    }

    #[test]
    fn a_rebind_does_not_extend_a_lifetime_and_can_lift_it() {
        let limits = Limits { max_lifetime: Some(20 * SEC), ..Limits::default() };
        let mut engine = limited(limits.clone());
        engine.register("wl", "poll", Spec::Interval(8 * SEC), at(0)).unwrap();

        // The same binding again, as a replay does: still ends at 20.
        rebind(&mut engine, limits, true, at(15));
        assert_eq!(names(&fire(&mut engine, at(16))), ["poll"]);
        assert!(fire(&mut engine, at(20)).is_empty());
        assert!(listed(&mut engine).is_empty());

        let mut engine = limited(Limits { max_lifetime: Some(20 * SEC), ..Limits::default() });
        engine.register("wl", "poll", Spec::Interval(8 * SEC), at(0)).unwrap();
        rebind(&mut engine, Limits::default(), true, at(15));
        assert_eq!(names(&fire(&mut engine, at(24))), ["poll"]);
    }

    #[test]
    fn a_rebind_keeps_a_firing_in_flight_from_overlapping() {
        let mut engine = bound(&[("beat", Spec::Interval(10 * SEC))], false);
        let running = engine.poll(at(10));
        let binding = Binding {
            declared: [("beat".to_string(), Spec::Interval(10 * SEC))].into(),
            limits: Limits::default(),
        };
        engine.bind("wl", binding, false, at(15));
        assert!(engine.poll(at(20)).is_empty());
        engine.complete(&running[0], Outcome::Handled, at(21));
        assert_eq!(engine.poll(at(30))[0].firing.missed, 1);
    }

    #[test]
    fn unregister_checks_the_name_it_is_given() {
        let mut engine = bound(&[], true);
        for bad in ["", "two\nlines", &"x".repeat(1_000)] {
            assert!(matches!(
                engine.unregister("wl", bad),
                Err(RegisterError::Spec(SpecError::InvalidName(_)))
            ));
        }
    }

    #[test]
    fn nothing_wakes_the_loop_for_a_schedule_whose_handler_is_running() {
        let mut engine = bound(&[("poll", Spec::Interval(10 * SEC))], false);
        let running = engine.poll(at(10));
        // Due again at 20 and 30, but there is nothing to do about that
        // until the handler returns.
        assert_eq!(engine.next_wake(at(10)), None);
        // With no poll in between, the periods are counted when it does.
        engine.complete(&running[0], Outcome::Handled, at(35));
        assert_eq!(engine.next_wake(at(35)), Some(5 * SEC));
        assert_eq!(engine.poll(at(40))[0].firing.missed, 2);
    }

    #[test]
    fn a_retry_is_refused_when_the_period_elapsed_unseen_during_the_attempt() {
        let mut engine = bound(&[("poll", Spec::Interval(10 * SEC))], false);
        let running = engine.poll(at(10));
        // The handler takes 12s, then asks for a retry. No poll saw 20 pass.
        assert_eq!(engine.complete(&running[0], Outcome::Retry, at(22)), Completion::RetrySuperseded);
        assert_eq!(engine.poll(at(30))[0].firing.missed, 1);
    }

    #[test]
    fn a_replacement_counts_what_it_skipped_while_it_waited() {
        let mut engine = bound(&[], true);
        engine.register("wl", "poll", Spec::Interval(60 * SEC), at(0)).unwrap();
        let old = engine.poll(at(60));
        engine.register("wl", "poll", Spec::Interval(SEC), at(61)).unwrap();
        // No poll while the old handler runs: nothing is waking the loop.
        assert_eq!(engine.next_wake(at(61)), None);
        assert_eq!(engine.complete(&old[0], Outcome::Handled, at(64)), Completion::Stale);
        assert!(engine.poll(at(64)).is_empty());
        let next = engine.poll(at(65));
        assert_eq!((names(&next), next[0].firing.missed), (vec!["poll"], 3));
    }

    #[test]
    fn an_unreachable_workload_is_reported_once_until_it_is_reached_again() {
        let mut engine = bound(&[("poll", Spec::Interval(10 * SEC))], false);
        let mut settle = |secs, outcome| {
            let due = engine.poll(at(secs));
            engine.complete(&due[0], outcome, at(secs))
        };
        assert_eq!(settle(10, Outcome::Undeliverable), Completion::Undelivered { first: true });
        assert_eq!(settle(20, Outcome::Undeliverable), Completion::Undelivered { first: false });
        assert_eq!(settle(30, Outcome::Handled), Completion::Settled);
        assert_eq!(settle(40, Outcome::Undeliverable), Completion::Undelivered { first: true });
    }

    #[test]
    fn a_redelivery_that_cannot_be_made_is_not_a_missed_firing() {
        let mut engine = bound(&[("poll", Spec::Interval(60 * SEC))], false);
        let first = engine.poll(at(60));
        engine.complete(&first[0], Outcome::Retry, at(60));
        let again = engine.poll(at(61));
        assert_eq!(again[0].firing.attempt, 2);
        // The firing reached the handler once; only its retry was lost.
        engine.complete(&again[0], Outcome::Undeliverable, at(61));
        assert_eq!(engine.poll(at(120))[0].firing.missed, 0);
    }

    #[test]
    fn once_at_refuses_nanoseconds_that_are_not_a_fraction_of_a_second() {
        let mut engine = bound(&[], true);
        for seconds in [T0 - 2, T0 + 60] {
            let instant = Wall { seconds, nanoseconds: 4_000_000_000 };
            assert!(matches!(
                engine.register("wl", "odd", Spec::OnceAt(instant), at(0)),
                Err(RegisterError::Spec(SpecError::InvalidSchedule(_)))
            ));
        }
        let instant = Wall { seconds: T0 + 60, nanoseconds: 999_999_999 };
        assert_eq!(engine.register("wl", "fine", Spec::OnceAt(instant), at(0)), Ok(true));
    }

    #[test]
    fn a_rebind_with_fewer_registrations_drops_the_newest() {
        let mut engine = bound(&[("beat", Spec::Interval(10 * SEC))], true);
        // Registered in this order, which is not the order of their names.
        for name in ["m", "z", "a", "q"] {
            engine.register("wl", name, Spec::Interval(10 * SEC), at(0)).unwrap();
        }
        let binding = Binding {
            declared: [("beat".to_string(), Spec::Interval(10 * SEC))].into(),
            limits: Limits { max_registrations: 2, ..Limits::default() },
        };
        engine.bind("wl", binding, true, at(1));
        assert_eq!(
            ended(&mut engine),
            [("a".to_string(), EndReason::Rebound), ("q".to_string(), EndReason::Rebound)]
        );
        // The declared schedule is not one of the two.
        assert_eq!(listed(&mut engine), ["beat", "m", "z"]);
    }

    #[test]
    fn a_rebind_with_fewer_firings_drops_what_has_already_had_them() {
        let mut engine = bound(&[], true);
        engine.register("wl", "busy", Spec::Interval(10 * SEC), at(0)).unwrap();
        fire(&mut engine, at(10));
        fire(&mut engine, at(20));
        engine.register("wl", "fresh", Spec::Interval(10 * SEC), at(20)).unwrap();

        rebind(&mut engine, Limits { max_firings: Some(2), ..Limits::default() }, true, at(21));
        assert_eq!(ended(&mut engine), [("busy".to_string(), EndReason::MaxFirings)]);
        assert_eq!(listed(&mut engine), ["fresh"]);
    }

    #[test]
    fn each_kind_of_schedule_is_listed_with_when_it_is_next_due() {
        let mut engine = bound(&[("nightly", cron("0 2 * * *"))], true);
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(0)).unwrap();
        engine.register("wl", "later", Spec::OnceAfter(20 * SEC), at(0)).unwrap();
        engine.register("wl", "then", Spec::OnceAt(at(30).wall), at(0)).unwrap();
        // Asking later does not move any of them.
        for now in [at(0), at(3)] {
            assert_eq!(next(&mut engine, "nightly", now), when(2 * 3600));
            assert_eq!(next(&mut engine, "poll", now), when(10));
            assert_eq!(next(&mut engine, "later", now), when(20));
            assert_eq!(next(&mut engine, "then", now), when(30));
        }
    }

    #[test]
    fn next_is_the_scheduled_at_a_crontab_firing_then_carries() {
        let mut engine = bound(&[("hourly", cron("0 * * * *"))], true);
        let forecast = next(&mut engine, "hourly", at(100));
        // The loop wakes a little late; the firing still names the match.
        let due = fire(&mut engine, at(3_605));
        assert_eq!(Some(due[0].firing.scheduled_at), forecast);
        assert_eq!(next(&mut engine, "hourly", at(3_605)), when(2 * 3600));
    }

    #[test]
    fn next_moves_on_each_time_an_interval_fires() {
        let mut engine = bound(&[], true);
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(0)).unwrap();
        fire(&mut engine, at(10));
        assert_eq!(next(&mut engine, "poll", at(10)), when(20));
        fire(&mut engine, at(23));
        // Late, and the rate is kept.
        assert_eq!(next(&mut engine, "poll", at(23)), when(30));
    }

    #[test]
    fn next_passes_over_what_comes_due_while_the_handler_is_running() {
        let mut engine = bound(&[("minutely", cron("* * * * *"))], true);
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(0)).unwrap();
        let due = engine.poll(at(60));
        assert_eq!(names(&due), ["minutely", "poll"]);
        // Both handlers are still running. Until their periods pass, the
        // next firing is the one the schedule says.
        assert_eq!(next(&mut engine, "minutely", at(61)), when(120));
        assert_eq!(next(&mut engine, "poll", at(61)), when(70));
        // Once they have, those firings are skipped, not delivered late.
        assert_eq!(next(&mut engine, "minutely", at(130)), when(180));
        assert_eq!(next(&mut engine, "poll", at(85)), when(90));
        assert_eq!(next(&mut engine, "poll", at(90)), when(100));
    }

    #[test]
    fn a_one_shot_has_no_next_once_its_firing_is_being_handled() {
        let mut engine = bound(&[], true);
        engine.register("wl", "later", Spec::OnceAfter(10 * SEC), at(0)).unwrap();
        assert_eq!(next(&mut engine, "later", at(9)), when(10));
        let due = engine.poll(at(10));
        assert_eq!(names(&due), ["later"]);
        // Still listed, until the firing settles.
        assert_eq!(next(&mut engine, "later", at(10)), None);
        // And while a redelivery of it is owed.
        assert_eq!(engine.complete(&due[0], Outcome::Retry, at(10)), Completion::RetryIn(SEC));
        assert_eq!(next(&mut engine, "later", at(10)), None);
    }

    #[test]
    fn a_one_shot_waiting_for_its_handler_is_listed_at_the_instant_it_was_due() {
        let mut engine = bound(&[], true);
        engine.register("wl", "job", Spec::Interval(10 * SEC), at(0)).unwrap();
        let running = engine.poll(at(10));
        // Replaced while its firing is still being handled.
        engine.register("wl", "job", Spec::OnceAfter(5 * SEC), at(11)).unwrap();
        assert!(engine.poll(at(20)).is_empty());
        assert_eq!(next(&mut engine, "job", at(20)), when(16));
        engine.complete(&running[0], Outcome::Handled, at(21));
        let due = engine.poll(at(21));
        assert_eq!(due[0].firing.scheduled_at, at(16).wall);
    }

    #[test]
    fn a_schedule_max_lifetime_will_end_first_has_no_next() {
        let mut engine = limited(Limits { max_lifetime: Some(12 * SEC), ..Limits::default() });
        engine.register("wl", "forgotten", Spec::Interval(60 * SEC), at(0)).unwrap();
        engine.register("wl", "brisk", Spec::Interval(5 * SEC), at(0)).unwrap();
        assert_eq!(next(&mut engine, "forgotten", at(0)), None);
        assert_eq!(next(&mut engine, "brisk", at(0)), when(5));
        fire(&mut engine, at(5));
        assert_eq!(next(&mut engine, "brisk", at(5)), when(10));
        fire(&mut engine, at(10));
        // Due at 15, and removed at 12.
        assert_eq!(next(&mut engine, "brisk", at(10)), None);
        assert!(engine.poll(at(15)).is_empty());
    }

    #[test]
    fn a_schedule_on_its_last_firing_under_max_firings_has_no_next() {
        let mut engine = limited(Limits { max_firings: Some(2), ..Limits::default() });
        engine.register("wl", "capped", Spec::Interval(10 * SEC), at(0)).unwrap();
        fire(&mut engine, at(10));
        assert_eq!(next(&mut engine, "capped", at(10)), when(20));
        // The second is the last, and says so while it is being handled: a
        // handler that lists its own schedule is not told of a firing that
        // will not come.
        let due = engine.poll(at(20));
        assert_eq!(next(&mut engine, "capped", at(20)), None);
        // A redelivery is the same firing, so it changes nothing.
        assert_eq!(engine.complete(&due[0], Outcome::Retry, at(20)), Completion::RetryIn(SEC));
        assert_eq!(next(&mut engine, "capped", at(20)), None);
    }

    #[test]
    fn a_redelivery_in_flight_is_not_counted_as_another_firing() {
        let mut engine = limited(Limits { max_firings: Some(2), ..Limits::default() });
        engine.register("wl", "capped", Spec::Interval(10 * SEC), at(0)).unwrap();
        let due = engine.poll(at(10));
        assert_eq!(engine.complete(&due[0], Outcome::Retry, at(10)), Completion::RetryIn(SEC));
        // The first firing is being delivered a second time. One more is
        // still to come.
        let again = engine.poll(at(11));
        assert_eq!(again[0].firing.attempt, 2);
        assert_eq!(next(&mut engine, "capped", at(11)), when(20));
    }

    #[test]
    fn a_declared_schedule_always_has_a_next() {
        let mut engine = Engine::new();
        let binding = Binding {
            declared: [("beat".to_string(), Spec::Interval(60 * SEC))].into(),
            limits: Limits { max_lifetime: Some(12 * SEC), max_firings: Some(1), ..Limits::default() },
        };
        engine.bind("wl", binding, true, at(0));
        assert_eq!(next(&mut engine, "beat", at(0)), when(60));
        fire(&mut engine, at(60));
        assert_eq!(next(&mut engine, "beat", at(60)), when(120));
    }

    #[test]
    fn next_for_an_interval_is_read_off_the_system_clock_as_it_stands() {
        let mut engine = bound(&[], true);
        engine.register("wl", "poll", Spec::Interval(10 * SEC), clock(0, 0)).unwrap();
        // Four seconds on, the system clock has been set forward 96 more.
        // The period still has six seconds to run.
        assert_eq!(next(&mut engine, "poll", clock(4, 100)), Some(clock(0, 106).wall));
    }
}
