//! The scheduler's state: which workload holds which schedules, when each is
//! next due, and what a finished delivery does to it.
//!
//! The engine never reads a clock or calls a workload. It is told the time and
//! answers with what is due, so the same code runs under `cargo test` with a
//! clock the test controls. The component's dispatch loop is the only caller.

use std::collections::{BTreeMap, btree_map};

use crate::spec::{Binding, Limits, Spec, SpecError, Wall, validate_name};

/// How long the loop may sleep on the monotonic clock while a `cron` schedule
/// is pending, in nanoseconds. Crontab times are wall-clock times, and a
/// monotonic sleep does not notice the system clock being corrected, so the
/// loop wakes at least this often to look again.
const WALL_RESYNC: u64 = 30_000_000_000;

/// The most occurrences of one schedule the engine will walk through to count
/// what was missed. Past this the count saturates rather than stall a loop
/// that fell far behind.
const MAX_CATCH_UP: u32 = 1_000;

/// One reading of both clocks, taken together.
#[derive(Clone, Copy, Debug)]
pub struct Now {
    /// `wasi:clocks/monotonic-clock.now`, in nanoseconds.
    pub mono: u64,
    /// `wasi:clocks/system-clock.now`.
    pub wall: Wall,
}

/// One occasion on which a schedule came due.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Firing {
    pub name: String,
    pub scheduled_at: Wall,
    pub missed: u32,
    pub attempt: u32,
}

/// A firing to deliver, and the identity of the schedule it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dispatch {
    pub workload: String,
    /// Which registration of `firing.name` this is. A name can be removed and
    /// registered again while a delivery is in flight; the epoch is how that
    /// delivery's result is kept from landing on its successor.
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
    /// The handler failed and does not want the firing again, or the call
    /// could not be completed.
    Failed,
    /// The workload could not be called at all, so nothing was delivered.
    Undeliverable,
}

/// What the engine did with a finished delivery, for the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completion {
    /// Nothing further is owed for this firing.
    Settled,
    /// The firing will be redelivered after this many nanoseconds.
    RetryIn(u64),
    /// The handler asked for a retry and has used every attempt.
    RetriesExhausted,
    /// The handler asked for a retry, but the schedule's next firing is due
    /// first, so that is delivered instead.
    RetrySuperseded,
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

/// One schedule as `list-registrations` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registration {
    pub name: String,
    pub spec: Spec,
    pub declared: bool,
}

/// When a schedule is next due. Each kind is measured on the clock it is
/// defined against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Due {
    /// A `cron` schedule's next matching instant.
    Wall(Wall),
    /// An `interval` schedule's next mark on the monotonic clock.
    Mono(u64),
    /// A `cron` schedule with no later match.
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
    in_flight: bool,
    retry: Option<Retry>,
}

#[derive(Debug)]
struct Workload {
    limits: Limits,
    /// Whether the binding grants `scheduler`, as opposed to `handler` alone.
    can_register: bool,
    entries: BTreeMap<String, Entry>,
}

#[derive(Debug, Default)]
pub struct Engine {
    workloads: BTreeMap<String, Workload>,
    epochs: u64,
}

impl Engine {
    pub const fn new() -> Self {
        Self { workloads: BTreeMap::new(), epochs: 0 }
    }

    fn entry(&mut self, spec: Spec, declared: bool, now: Now) -> Entry {
        self.epochs += 1;
        let due = match &spec {
            Spec::Cron(cron) => cron.next_after(now.wall).map_or(Due::Never, Due::Wall),
            Spec::Interval(period) => Due::Mono(now.mono.saturating_add(*period)),
        };
        Entry {
            spec,
            declared,
            epoch: self.epochs,
            due,
            missed: 0,
            in_flight: false,
            retry: None,
        }
    }

    /// Take a workload's binding as its declared schedules and limits.
    ///
    /// Safe to repeat: a declared schedule the binding still spells the same
    /// way keeps its timing, so a replayed bind does not restart every
    /// interval. Schedules the workload registered for itself are kept.
    pub fn bind(&mut self, workload: &str, binding: Binding, can_register: bool, now: Now) {
        let mut entries = self
            .workloads
            .remove(workload)
            .map(|w| w.entries)
            .unwrap_or_default();
        entries.retain(|name, entry| {
            if entry.declared {
                binding.declared.get(name) == Some(&entry.spec)
            } else {
                // A name the binding now declares belongs to the binding.
                !binding.declared.contains_key(name)
            }
        });
        for (name, spec) in binding.declared {
            if let btree_map::Entry::Vacant(slot) = entries.entry(name) {
                slot.insert(self.entry(spec, true, now));
            }
        }
        self.workloads.insert(
            workload.to_string(),
            Workload { limits: binding.limits, can_register, entries },
        );
    }

    /// Drop everything held for a workload. Unknown ids are ignored.
    pub fn unbind(&mut self, workload: &str) {
        self.workloads.remove(workload);
    }

    /// `scheduler.register`.
    ///
    /// # Errors
    ///
    /// `AccessDenied` when the workload holds no grant to schedule or the name
    /// is a declared one, `Spec` when the name or schedule is refused, and
    /// `LimitExceeded` when the workload already holds its ceiling.
    pub fn register(
        &mut self,
        workload: &str,
        name: &str,
        spec: Spec,
        now: Now,
    ) -> Result<(), RegisterError> {
        let held = self.registrable(workload)?;
        validate_name(name)?;
        if spec == Spec::Interval(0) {
            return Err(SpecError::InvalidSchedule("a period cannot be zero".to_string()).into());
        }
        held.limits.check(&spec)?;
        match held.entries.get(name) {
            Some(existing) if existing.declared => return Err(RegisterError::AccessDenied),
            // The same schedule again changes nothing, phase included.
            Some(existing) if existing.spec == spec => return Ok(()),
            Some(_) => {}
            None => {
                let registered = held.entries.values().filter(|e| !e.declared).count();
                if registered >= held.limits.max_registrations {
                    return Err(RegisterError::LimitExceeded);
                }
            }
        }
        let entry = self.entry(spec, false, now);
        if let Some(held) = self.workloads.get_mut(workload) {
            held.entries.insert(name.to_string(), entry);
        }
        Ok(())
    }

    /// `scheduler.unregister`. Removing a name that is not held succeeds.
    ///
    /// # Errors
    ///
    /// `AccessDenied` when the workload holds no grant to schedule or the name
    /// is a declared one.
    pub fn unregister(&mut self, workload: &str, name: &str) -> Result<(), RegisterError> {
        let held = self.registrable(workload)?;
        match held.entries.get(name) {
            Some(existing) if existing.declared => Err(RegisterError::AccessDenied),
            _ => {
                held.entries.remove(name);
                Ok(())
            }
        }
    }

    /// `scheduler.list-registrations`, ordered by name.
    ///
    /// # Errors
    ///
    /// `AccessDenied` when the workload holds no grant to schedule.
    pub fn list(&mut self, workload: &str) -> Result<Vec<Registration>, RegisterError> {
        Ok(self
            .registrable(workload)?
            .entries
            .iter()
            .map(|(name, entry)| Registration {
                name: name.clone(),
                spec: entry.spec.clone(),
                declared: entry.declared,
            })
            .collect())
    }

    fn registrable(&mut self, workload: &str) -> Result<&mut Workload, RegisterError> {
        match self.workloads.get_mut(workload) {
            Some(held) if held.can_register => Ok(held),
            _ => Err(RegisterError::AccessDenied),
        }
    }

    /// Everything due at `now`, each marked as in flight until
    /// [`Engine::complete`] is told how its delivery ended.
    pub fn poll(&mut self, now: Now) -> Vec<Dispatch> {
        let mut due = Vec::new();
        for (workload, held) in &mut self.workloads {
            for (name, entry) in &mut held.entries {
                let firing = match entry.advance(now) {
                    Some((scheduled_at, skipped)) => {
                        // The schedule itself came due, which outranks a
                        // redelivery of the firing before it.
                        entry.retry = None;
                        if entry.in_flight {
                            entry.missed = entry.missed.saturating_add(skipped).saturating_add(1);
                            continue;
                        }
                        let missed = entry.missed.saturating_add(skipped);
                        entry.missed = 0;
                        Firing { name: name.clone(), scheduled_at, missed, attempt: 1 }
                    }
                    None => match &entry.retry {
                        Some(retry) if !entry.in_flight && now.mono >= retry.due => {
                            let mut firing = retry.firing.clone();
                            firing.attempt += 1;
                            entry.retry = None;
                            firing
                        }
                        _ => continue,
                    },
                };
                entry.in_flight = true;
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
        let Some(entry) = held
            .entries
            .get_mut(&dispatch.firing.name)
            .filter(|entry| entry.epoch == dispatch.epoch)
        else {
            return Completion::Stale;
        };
        entry.in_flight = false;
        match outcome {
            Outcome::Handled | Outcome::Failed => Completion::Settled,
            Outcome::Undeliverable => {
                // Never delivered, so it joins what the next firing reports.
                entry.missed = entry
                    .missed
                    .saturating_add(dispatch.firing.missed)
                    .saturating_add(1);
                Completion::Settled
            }
            Outcome::Retry => {
                if dispatch.firing.attempt >= held.limits.retry_max_attempts {
                    return Completion::RetriesExhausted;
                }
                let doublings = dispatch.firing.attempt.saturating_sub(1).min(32);
                let backoff = held.limits.retry_backoff.saturating_mul(1 << doublings);
                if backoff >= entry.until_due(now).unwrap_or(u64::MAX) {
                    return Completion::RetrySuperseded;
                }
                entry.retry = Some(Retry {
                    due: now.mono.saturating_add(backoff),
                    firing: dispatch.firing.clone(),
                });
                Completion::RetryIn(backoff)
            }
        }
    }

    /// How long the loop may sleep before something could be due, in
    /// nanoseconds, or `None` when nothing is scheduled at all.
    pub fn next_wake(&self, now: Now) -> Option<u64> {
        self.workloads
            .values()
            .flat_map(|held| held.entries.values())
            .flat_map(|entry| {
                let regular = match entry.due {
                    // Capped, because the wait is on the monotonic clock and
                    // this deadline is on the system clock.
                    Due::Wall(_) => entry.until_due(now).map(|wait| wait.min(WALL_RESYNC)),
                    _ => entry.until_due(now),
                };
                let retry = entry
                    .retry
                    .as_ref()
                    .filter(|_| !entry.in_flight)
                    .map(|retry| retry.due.saturating_sub(now.mono));
                [regular, retry]
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

    /// If the schedule has come due, move it to its next occurrence and
    /// return when the latest due occurrence was scheduled, with how many
    /// earlier ones were passed over on the way.
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
                        // Too far behind to walk: resume from the present.
                        Some(next) if next <= now.wall => break cron.next_after(now.wall),
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
        assert_eq!(engine.list("wl"), Err(RegisterError::AccessDenied));
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

        assert_eq!(engine.unregister("wl", "poll"), Ok(()));
        assert!(fire(&mut engine, at(200)).is_empty());
        assert_eq!(engine.next_wake(at(200)), None);
        // Removing it again is not an error.
        assert_eq!(engine.unregister("wl", "poll"), Ok(()));
    }

    #[test]
    fn registering_the_same_schedule_again_keeps_its_phase() {
        let mut engine = bound(&[], true);
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(0)).unwrap();
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(7)).unwrap();
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
        assert_eq!(engine.list("wl"), Ok(vec![]));
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
        assert_eq!(engine.register("wl", "a", Spec::Interval(20 * SEC), at(0)), Ok(()));
        engine.unregister("wl", "b").unwrap();
        assert_eq!(engine.register("wl", "c", Spec::Interval(10 * SEC), at(0)), Ok(()));
    }

    #[test]
    fn declared_schedules_are_listed_but_not_the_guests_to_change() {
        let mut engine = bound(&[("nightly", cron("0 2 * * *"))], true);
        engine.register("wl", "poll", Spec::Interval(10 * SEC), at(0)).unwrap();
        assert_eq!(
            engine.list("wl").unwrap(),
            [
                Registration { name: "nightly".into(), spec: cron("0 2 * * *"), declared: true },
                Registration { name: "poll".into(), spec: Spec::Interval(10 * SEC), declared: false },
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
        assert_eq!(engine.list("other"), Ok(vec![]));
        assert_eq!(engine.unregister("other", "poll"), Ok(()));
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
            engine.list("wl").unwrap(),
            [
                Registration { name: "kept".into(), spec: cron("0 3 * * *"), declared: true },
                // The binding now owns the name the guest had registered.
                Registration { name: "new".into(), spec: Spec::Interval(30 * SEC), declared: true },
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
}
