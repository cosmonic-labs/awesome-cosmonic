//! Reading schedules: crontab expressions, time zones, periods, names, and
//! the binding configuration that declares them.
//!
//! Nothing here touches a clock or a WIT binding, so all of it runs under
//! `cargo test` on the host.

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, TimeZone as _, Utc};
use chrono_tz::Tz;
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};

const NANOS_PER_MILLI: u64 = 1_000_000;
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// The longest name a schedule may have, in bytes.
pub const MAX_NAME_LEN: usize = 64;

/// The shortest period any binding may lower `min-interval` to. Below this the
/// dispatch loop would spend its time rescheduling rather than waiting.
pub const HARD_MIN_INTERVAL: u64 = 10 * NANOS_PER_MILLI;

/// A point on the system clock: seconds and nanoseconds since the Unix epoch,
/// the same shape as `wasi:clocks/system-clock.instant`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Wall {
    pub seconds: i64,
    pub nanoseconds: u32,
}

impl Wall {
    /// This instant moved back by `nanos`.
    pub fn minus_nanos(self, nanos: u64) -> Self {
        let total = i128::from(self.seconds) * i128::from(NANOS_PER_SEC)
            + i128::from(self.nanoseconds)
            - i128::from(nanos);
        Self {
            seconds: total.div_euclid(i128::from(NANOS_PER_SEC)) as i64,
            nanoseconds: total.rem_euclid(i128::from(NANOS_PER_SEC)) as u32,
        }
    }

    /// Nanoseconds from `self` until `later`, or zero if `later` is not later.
    pub fn nanos_until(self, later: Self) -> u64 {
        let from = i128::from(self.seconds) * i128::from(NANOS_PER_SEC) + i128::from(self.nanoseconds);
        let to = i128::from(later.seconds) * i128::from(NANOS_PER_SEC) + i128::from(later.nanoseconds);
        u64::try_from(to - from).unwrap_or(if to > from { u64::MAX } else { 0 })
    }
}

/// Why a schedule was refused, mirroring the cases of `schedule-error` a
/// schedule alone can produce.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpecError {
    InvalidName(String),
    InvalidSchedule(String),
    Unsupported(String),
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(detail) => write!(f, "invalid name: {detail}"),
            Self::InvalidSchedule(detail) => write!(f, "invalid schedule: {detail}"),
            Self::Unsupported(detail) => write!(f, "unsupported schedule: {detail}"),
        }
    }
}

/// A parsed crontab schedule. Keeps the text it was given, so a listing
/// returns what the caller wrote rather than a normalized form.
#[derive(Clone, Debug)]
pub struct CronSpec {
    expression: String,
    timezone: Option<String>,
    cron: Cron,
    tz: Tz,
}

impl CronSpec {
    /// Parse a five-field crontab expression read in `timezone`, or in UTC
    /// when there is none.
    ///
    /// # Errors
    ///
    /// `InvalidSchedule` when the expression does not parse, the zone is not
    /// in the IANA database, or the expression can never match.
    pub fn parse(expression: &str, timezone: Option<&str>) -> Result<Self, SpecError> {
        let tz = match timezone {
            None => Tz::UTC,
            Some(name) => name.parse::<Tz>().map_err(|_| {
                SpecError::InvalidSchedule(format!("unknown time zone '{name}'"))
            })?,
        };
        // Five fields exactly: a seconds field would promise a resolution the
        // contract gives to `interval`, and a year field is no crontab.
        let cron = CronParser::builder()
            .seconds(Seconds::Disallowed)
            .year(Year::Disallowed)
            .build()
            .parse(expression)
            .map_err(|e| SpecError::InvalidSchedule(format!("'{expression}': {e}")))?;
        let spec = Self {
            expression: expression.to_string(),
            timezone: timezone.map(str::to_string),
            cron,
            tz,
        };
        // `0 0 30 2 *` parses and never fires. Registering it would succeed
        // and then do nothing, which is the failure a caller cannot see.
        if spec.next_after(Wall { seconds: 0, nanoseconds: 0 }).is_none() {
            return Err(SpecError::InvalidSchedule(format!(
                "'{expression}' never matches a date"
            )));
        }
        Ok(spec)
    }

    pub fn expression(&self) -> &str {
        &self.expression
    }

    pub fn timezone(&self) -> Option<&str> {
        self.timezone.as_deref()
    }

    /// The first instant strictly after `after` that the expression matches,
    /// or `None` if it has no later match.
    pub fn next_after(&self, after: Wall) -> Option<Wall> {
        let utc: DateTime<Utc> = Utc.timestamp_opt(after.seconds, after.nanoseconds).single()?;
        let local = utc.with_timezone(&self.tz);
        let next = self.cron.find_next_occurrence(&local, false).ok()?;
        Some(Wall {
            seconds: next.timestamp(),
            nanoseconds: next.timestamp_subsec_nanos(),
        })
    }
}

impl PartialEq for CronSpec {
    fn eq(&self, other: &Self) -> bool {
        self.expression == other.expression && self.timezone == other.timezone
    }
}

impl Eq for CronSpec {}

/// When a schedule fires.
// A parsed crontab is far larger than a period, but a host holds a handful of
// these and never moves them in bulk, so boxing would only add an allocation.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Spec {
    Cron(CronSpec),
    /// A period in nanoseconds.
    Interval(u64),
    /// A single firing at this instant.
    OnceAt(Wall),
    /// A single firing after this many nanoseconds.
    OnceAfter(u64),
}

impl Spec {
    /// Whether the schedule fires a single time and is then spent.
    pub fn is_once(&self) -> bool {
        matches!(self, Self::OnceAt(_) | Self::OnceAfter(_))
    }
}

/// Check a schedule name against what `schedule-name` permits.
///
/// # Errors
///
/// `InvalidName` when it is empty, longer than [`MAX_NAME_LEN`], or holds a
/// character other than an ASCII letter, a digit, `-`, `_` or `.`.
pub fn validate_name(name: &str) -> Result<(), SpecError> {
    if name.is_empty() {
        return Err(SpecError::InvalidName("a schedule name cannot be empty".to_string()));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(SpecError::InvalidName(format!(
            "'{name}' is {} characters; the limit is {MAX_NAME_LEN}",
            name.len()
        )));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    {
        return Err(SpecError::InvalidName(format!(
            "'{name}' contains '{bad}'; use ASCII letters, digits, '-', '_' and '.'"
        )));
    }
    Ok(())
}

/// Read a period such as `500ms`, `30s` or `1h30m` into nanoseconds.
///
/// # Errors
///
/// A message naming what could not be read: no number, no unit, an unknown
/// unit, or a total past what a `duration` holds.
pub fn parse_duration(text: &str) -> Result<u64, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("a period cannot be empty".to_string());
    }
    let mut total: u64 = 0;
    let mut rest = text;
    while !rest.is_empty() {
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 {
            return Err(format!("'{text}': expected a number before '{rest}'"));
        }
        let number: u64 = rest[..digits]
            .parse()
            .map_err(|_| format!("'{text}': '{}' is too large", &rest[..digits]))?;
        rest = &rest[digits..];
        let unit_len = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_alphabetic()).len();
        let scale = match &rest[..unit_len] {
            "ms" => NANOS_PER_MILLI,
            "s" => NANOS_PER_SEC,
            "m" => 60 * NANOS_PER_SEC,
            "h" => 3_600 * NANOS_PER_SEC,
            "d" => 86_400 * NANOS_PER_SEC,
            "" => return Err(format!("'{text}': {number} needs a unit (ms, s, m, h or d)")),
            other => return Err(format!("'{text}': unknown unit '{other}' (use ms, s, m, h or d)")),
        };
        rest = &rest[unit_len..];
        total = number
            .checked_mul(scale)
            .and_then(|part| total.checked_add(part))
            .ok_or_else(|| format!("'{text}' is too long a period"))?;
    }
    Ok(total)
}

/// The ceilings and retry policy one workload's schedules run under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// How many schedules the workload's components may hold through
    /// `scheduler.register` at once.
    pub max_registrations: usize,
    /// The shortest `interval` period, in nanoseconds.
    pub min_interval: u64,
    /// Deliveries of one firing, counting the first.
    pub retry_max_attempts: u32,
    /// Delay before the first redelivery, in nanoseconds. Doubles after.
    pub retry_backoff: u64,
    /// How long a registered schedule is held, in nanoseconds. `None` holds
    /// it until it is removed.
    pub max_lifetime: Option<u64>,
    /// How many firings a registered schedule delivers before it is removed.
    /// `None` puts no bound on them.
    pub max_firings: Option<u32>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_registrations: 32,
            min_interval: NANOS_PER_SEC,
            retry_max_attempts: 3,
            retry_backoff: NANOS_PER_SEC,
            max_lifetime: None,
            max_firings: None,
        }
    }
}

impl Limits {
    /// Check a schedule against these limits.
    ///
    /// # Errors
    ///
    /// `InvalidSchedule` for an `interval` period or `once-after` delay
    /// shorter than `min_interval`.
    pub fn check(&self, spec: &Spec) -> Result<(), SpecError> {
        match spec {
            Spec::Interval(wait) | Spec::OnceAfter(wait) if *wait < self.min_interval => {
                Err(SpecError::InvalidSchedule(format!(
                    "{} is shorter than the {} this workload is permitted",
                    format_duration(*wait),
                    format_duration(self.min_interval)
                )))
            }
            _ => Ok(()),
        }
    }
}

/// Everything one workload's `cosmonic:cron` binding configuration says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Binding {
    /// The schedules it declares, by name.
    pub declared: BTreeMap<String, Spec>,
    pub limits: Limits,
}

impl Binding {
    /// Read a binding's `config` key/value pairs.
    ///
    /// # Errors
    ///
    /// A message for the first key that is not recognized, value that cannot
    /// be read, or name declared twice. The caller fails the bind with it.
    pub fn parse<'a>(
        config: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, String> {
        let mut limits = Limits::default();
        let mut timezone: Option<&str> = None;
        let mut crons: Vec<(&str, &str)> = Vec::new();
        let mut intervals: Vec<(&str, &str)> = Vec::new();

        for (key, value) in config {
            let value = value.trim();
            if let Some(name) = key.strip_prefix("schedule.") {
                crons.push((name, value));
            } else if let Some(name) = key.strip_prefix("interval.") {
                intervals.push((name, value));
            } else {
                match key {
                    "timezone" => timezone = Some(value),
                    "max-registrations" => {
                        limits.max_registrations = value
                            .parse()
                            .map_err(|_| format!("max-registrations: '{value}' is not a count"))?;
                    }
                    "min-interval" => {
                        let period = parse_duration(value).map_err(|e| format!("min-interval: {e}"))?;
                        if period < HARD_MIN_INTERVAL {
                            return Err(format!(
                                "min-interval: {} is below the {} this plugin supports",
                                format_duration(period),
                                format_duration(HARD_MIN_INTERVAL)
                            ));
                        }
                        limits.min_interval = period;
                    }
                    "retry.max-attempts" => {
                        limits.retry_max_attempts = match value.parse() {
                            Ok(attempts) if attempts >= 1 => attempts,
                            _ => {
                                return Err(format!(
                                    "retry.max-attempts: '{value}' is not a count of at least 1"
                                ));
                            }
                        };
                    }
                    "retry.backoff" => {
                        limits.retry_backoff =
                            parse_duration(value).map_err(|e| format!("retry.backoff: {e}"))?;
                    }
                    "max-lifetime" => {
                        limits.max_lifetime = match parse_duration(value) {
                            Ok(0) => return Err("max-lifetime: cannot be zero".to_string()),
                            Ok(lifetime) => Some(lifetime),
                            Err(e) => return Err(format!("max-lifetime: {e}")),
                        };
                    }
                    "max-firings" => {
                        limits.max_firings = match value.parse() {
                            Ok(firings) if firings >= 1 => Some(firings),
                            _ => {
                                return Err(format!(
                                    "max-firings: '{value}' is not a count of at least 1"
                                ));
                            }
                        };
                    }
                    other => {
                        return Err(format!(
                            "unknown key '{other}'; expected schedule.<name>, interval.<name>, \
                             timezone, max-registrations, min-interval, max-lifetime, \
                             max-firings, retry.max-attempts or retry.backoff"
                        ));
                    }
                }
            }
        }

        let mut declared = BTreeMap::new();
        let mut declare = |key: String, name: &str, spec: Spec| -> Result<(), String> {
            validate_name(name).map_err(|e| format!("{key}: {e}"))?;
            limits.check(&spec).map_err(|e| format!("{key}: {e}"))?;
            if declared.insert(name.to_string(), spec).is_some() {
                return Err(format!("{key}: '{name}' is declared more than once"));
            }
            Ok(())
        };
        for (name, expression) in crons {
            let key = format!("schedule.{name}");
            let spec = CronSpec::parse(expression, timezone).map_err(|e| format!("{key}: {e}"))?;
            declare(key, name, Spec::Cron(spec))?;
        }
        for (name, period) in intervals {
            let key = format!("interval.{name}");
            let period = parse_duration(period).map_err(|e| format!("{key}: {e}"))?;
            if period == 0 {
                return Err(format!("{key}: a period cannot be zero"));
            }
            declare(key, name, Spec::Interval(period))?;
        }
        Ok(Self { declared, limits })
    }
}

/// A period as text, in the largest unit that divides it evenly.
pub fn format_duration(nanos: u64) -> String {
    const UNITS: [(u64, &str); 5] = [
        (86_400 * NANOS_PER_SEC, "d"),
        (3_600 * NANOS_PER_SEC, "h"),
        (60 * NANOS_PER_SEC, "m"),
        (NANOS_PER_SEC, "s"),
        (NANOS_PER_MILLI, "ms"),
    ];
    for (scale, unit) in UNITS {
        if nanos >= scale && nanos.is_multiple_of(scale) {
            return format!("{}{unit}", nanos / scale);
        }
    }
    format!("{nanos}ns")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> Wall {
        Wall { seconds, nanoseconds: 0 }
    }

    // 2026-10-12T00:00:00Z, a Monday.
    const MONDAY: i64 = 1_791_763_200;

    #[test]
    fn cron_finds_the_next_match_strictly_after() {
        let spec = CronSpec::parse("*/15 * * * *", None).unwrap();
        assert_eq!(spec.next_after(at(MONDAY)), Some(at(MONDAY + 900)));
        assert_eq!(spec.next_after(at(MONDAY + 1)), Some(at(MONDAY + 900)));
        assert_eq!(spec.next_after(at(MONDAY + 900)), Some(at(MONDAY + 1800)));
    }

    #[test]
    fn cron_ignores_sub_second_time_when_matching() {
        let spec = CronSpec::parse("* * * * *", None).unwrap();
        let just_before = Wall { seconds: MONDAY + 59, nanoseconds: 999_999_999 };
        assert_eq!(spec.next_after(just_before), Some(at(MONDAY + 60)));
    }

    #[test]
    fn cron_reads_fields_in_the_named_zone() {
        // 09:00 in New York is 13:00 UTC while daylight saving is in effect.
        let spec = CronSpec::parse("0 9 * * *", Some("America/New_York")).unwrap();
        assert_eq!(spec.next_after(at(MONDAY)), Some(at(MONDAY + 13 * 3600)));
        let utc = CronSpec::parse("0 9 * * *", None).unwrap();
        assert_eq!(utc.next_after(at(MONDAY)), Some(at(MONDAY + 9 * 3600)));
    }

    #[test]
    fn cron_accepts_names_and_ranges() {
        let spec = CronSpec::parse("30 8 * * mon-fri", None).unwrap();
        // From Friday 09:00 the next weekday 08:30 is Monday's.
        let friday_9am = MONDAY + 4 * 86_400 + 9 * 3600;
        assert_eq!(
            spec.next_after(at(friday_9am)),
            Some(at(MONDAY + 7 * 86_400 + 8 * 3600 + 1800))
        );
    }

    #[test]
    fn cron_restricting_both_day_fields_matches_either() {
        // The POSIX rule: day 15 of the month, or any Monday.
        let spec = CronSpec::parse("0 0 15 * mon", None).unwrap();
        // Monday the 12th at midnight is `MONDAY` itself, so the next match
        // after it is Thursday the 15th, before the following Monday.
        assert_eq!(spec.next_after(at(MONDAY)), Some(at(MONDAY + 3 * 86_400)));
    }

    #[test]
    fn cron_refuses_what_it_cannot_schedule() {
        for (expression, zone) in [
            ("* * * *", None),
            ("* * * * * *", None),
            ("61 * * * *", None),
            ("not a schedule", None),
            ("0 0 30 2 *", None),
            ("0 9 * * *", Some("Mars/Olympus_Mons")),
        ] {
            assert!(
                matches!(CronSpec::parse(expression, zone), Err(SpecError::InvalidSchedule(_))),
                "{expression:?} in {zone:?} should be refused"
            );
        }
    }

    #[test]
    fn cron_equality_is_what_the_caller_wrote() {
        let a = CronSpec::parse("0 2 * * *", None).unwrap();
        assert_eq!(a, CronSpec::parse("0 2 * * *", None).unwrap());
        assert_ne!(a, CronSpec::parse("0 3 * * *", None).unwrap());
        assert_ne!(a, CronSpec::parse("0 2 * * *", Some("UTC")).unwrap());
    }

    #[test]
    fn durations_parse_and_format() {
        assert_eq!(parse_duration("500ms"), Ok(500 * NANOS_PER_MILLI));
        assert_eq!(parse_duration("30s"), Ok(30 * NANOS_PER_SEC));
        assert_eq!(parse_duration(" 1h30m "), Ok(5_400 * NANOS_PER_SEC));
        assert_eq!(parse_duration("2d"), Ok(172_800 * NANOS_PER_SEC));
        for bad in ["", "30", "s", "5 s", "1.5s", "30sec", "-5s", "99999999999999999999s", "999999999999d"] {
            assert!(parse_duration(bad).is_err(), "{bad:?} should be refused");
        }
        assert_eq!(format_duration(5_400 * NANOS_PER_SEC), "90m");
        assert_eq!(format_duration(250 * NANOS_PER_MILLI), "250ms");
        assert_eq!(format_duration(1), "1ns");
    }

    #[test]
    fn names_are_checked() {
        for good in ["a", "nightly-report", "poll_inbox.v2", &"x".repeat(MAX_NAME_LEN)] {
            assert_eq!(validate_name(good), Ok(()), "{good:?}");
        }
        for bad in ["", "has space", "slash/y", "ünï", &"x".repeat(MAX_NAME_LEN + 1)] {
            assert!(matches!(validate_name(bad), Err(SpecError::InvalidName(_))), "{bad:?}");
        }
    }

    #[test]
    fn wall_arithmetic_borrows_across_the_second() {
        let t = Wall { seconds: 10, nanoseconds: 100 };
        assert_eq!(t.minus_nanos(200), Wall { seconds: 9, nanoseconds: 999_999_900 });
        assert_eq!(at(-1).minus_nanos(1), Wall { seconds: -2, nanoseconds: 999_999_999 });
        assert_eq!(at(5).nanos_until(at(7)), 2 * NANOS_PER_SEC);
        assert_eq!(at(7).nanos_until(at(5)), 0);
    }

    #[test]
    fn binding_declares_schedules_and_limits() {
        let binding = Binding::parse([
            ("schedule.nightly", "0 2 * * *"),
            ("interval.poll", "30s"),
            ("timezone", "Europe/Berlin"),
            ("max-registrations", "4"),
            ("min-interval", "5s"),
            ("retry.max-attempts", "5"),
            ("retry.backoff", "250ms"),
            ("max-lifetime", "24h"),
            ("max-firings", "500"),
        ])
        .unwrap();
        assert_eq!(
            binding.declared.get("nightly"),
            Some(&Spec::Cron(CronSpec::parse("0 2 * * *", Some("Europe/Berlin")).unwrap()))
        );
        assert_eq!(binding.declared.get("poll"), Some(&Spec::Interval(30 * NANOS_PER_SEC)));
        assert_eq!(
            binding.limits,
            Limits {
                max_registrations: 4,
                min_interval: 5 * NANOS_PER_SEC,
                retry_max_attempts: 5,
                retry_backoff: 250 * NANOS_PER_MILLI,
                max_lifetime: Some(86_400 * NANOS_PER_SEC),
                max_firings: Some(500),
            }
        );
    }

    #[test]
    fn binding_applies_the_zone_whatever_order_the_keys_arrive_in() {
        let binding = Binding::parse([("timezone", "Asia/Tokyo"), ("schedule.a", "0 9 * * *")]).unwrap();
        let Some(Spec::Cron(spec)) = binding.declared.get("a") else {
            panic!("schedule.a should be a cron schedule");
        };
        assert_eq!(spec.timezone(), Some("Asia/Tokyo"));
    }

    #[test]
    fn empty_binding_is_valid_and_uses_defaults() {
        let binding = Binding::parse([]).unwrap();
        assert!(binding.declared.is_empty());
        assert_eq!(binding.limits, Limits::default());
    }

    #[test]
    fn binding_refuses_what_would_silently_never_fire() {
        for (config, expect) in [
            (vec![("schedul.nightly", "0 2 * * *")], "unknown key"),
            (vec![("schedule.nightly", "0 2 * *")], "schedule.nightly"),
            (vec![("schedule.bad name", "0 2 * * *")], "invalid name"),
            (vec![("interval.poll", "30")], "needs a unit"),
            (vec![("interval.poll", "0s")], "cannot be zero"),
            (vec![("interval.poll", "100ms")], "shorter than"),
            (vec![("schedule.x", "0 2 * * *"), ("interval.x", "30s")], "more than once"),
            (vec![("timezone", "Nowhere/Land"), ("schedule.a", "0 2 * * *")], "unknown time zone"),
            (vec![("min-interval", "1ms")], "below"),
            (vec![("retry.max-attempts", "0")], "at least 1"),
            (vec![("max-lifetime", "0s")], "cannot be zero"),
            (vec![("max-lifetime", "forever")], "max-lifetime"),
            (vec![("max-firings", "0")], "at least 1"),
            (vec![("max-registrations", "many")], "not a count"),
        ] {
            let err = Binding::parse(config.clone()).unwrap_err();
            assert!(err.contains(expect), "{config:?} gave {err:?}, expected {expect:?}");
        }
    }

    #[test]
    fn limits_hold_a_one_shot_delay_to_the_interval_floor() {
        let limits = Limits::default();
        assert!(limits.check(&Spec::OnceAfter(NANOS_PER_SEC / 2)).is_err());
        assert_eq!(limits.check(&Spec::OnceAfter(NANOS_PER_SEC)), Ok(()));
        // An instant has no rate to bound.
        assert_eq!(limits.check(&Spec::OnceAt(at(MONDAY))), Ok(()));
    }

    #[test]
    fn binding_min_interval_admits_a_faster_declared_poll() {
        let binding = Binding::parse([("interval.fast", "100ms"), ("min-interval", "50ms")]).unwrap();
        assert_eq!(binding.declared.get("fast"), Some(&Spec::Interval(100 * NANOS_PER_MILLI)));
    }
}
