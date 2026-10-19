//! Parse schedules, time zones, durations, names, and binding configuration.
//!
//! Clock and WIT dependencies are excluded so parsing runs in host tests.

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

/// Minimum allowed `min-interval`, keeping the dispatch loop from constantly
/// rescheduling.
pub const HARD_MIN_INTERVAL: u64 = 10 * NANOS_PER_MILLI;

/// A point on the system clock: seconds and nanoseconds since the Unix epoch,
/// the same shape as `wasi:clocks/system-clock.instant`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Wall {
    pub seconds: i64,
    pub nanoseconds: u32,
}

impl Wall {
    /// Return this instant plus `nanos` nanoseconds.
    pub fn plus_nanos(self, nanos: u64) -> Self {
        let total = i128::from(self.seconds) * i128::from(NANOS_PER_SEC)
            + i128::from(self.nanoseconds)
            + i128::from(nanos);
        Self {
            seconds: i64::try_from(total.div_euclid(i128::from(NANOS_PER_SEC))).unwrap_or(i64::MAX),
            nanoseconds: total.rem_euclid(i128::from(NANOS_PER_SEC)) as u32,
        }
    }

    /// Return this instant minus `nanos` nanoseconds.
    pub fn minus_nanos(self, nanos: u64) -> Self {
        let total = i128::from(self.seconds) * i128::from(NANOS_PER_SEC)
            + i128::from(self.nanoseconds)
            - i128::from(nanos);
        Self {
            seconds: total.div_euclid(i128::from(NANOS_PER_SEC)) as i64,
            nanoseconds: total.rem_euclid(i128::from(NANOS_PER_SEC)) as u32,
        }
    }

    /// Nanoseconds until `later`, or zero if it is at or before `self`.
    pub fn nanos_until(self, later: Self) -> u64 {
        let from = i128::from(self.seconds) * i128::from(NANOS_PER_SEC) + i128::from(self.nanoseconds);
        let to = i128::from(later.seconds) * i128::from(NANOS_PER_SEC) + i128::from(later.nanoseconds);
        u64::try_from(to - from).unwrap_or(if to > from { u64::MAX } else { 0 })
    }
}

/// Schedule validation errors corresponding to `schedule-error` cases.
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

/// Look up an IANA time zone by name.
fn parse_zone(name: &str) -> Result<Tz, SpecError> {
    name.parse()
        .map_err(|_| SpecError::InvalidSchedule(format!("unknown time zone '{name}'")))
}

/// A 28-year search window covering every month, day, and weekday combination in this
/// century, including leap days.
const PROBE_YEARS: (i32, i32) = (2000, 2027);

/// Check whether a parsed expression ever matches, using [`PROBE_YEARS`] to bound the
/// search on the shared dispatch loop.
fn ever_matches(expression: &str) -> bool {
    // croner only takes a year field alongside a seconds field.
    let confined = format!("0 {expression} {}-{}", PROBE_YEARS.0, PROBE_YEARS.1);
    let Ok(probe) = CronParser::builder()
        .seconds(Seconds::Required)
        .year(Year::Required)
        .build()
        .parse(&confined)
    else {
        // Only an expression that is not made of fields, a nickname such as
        // `@daily`, parses alone and not with a year after it. Those match.
        return true;
    };
    Utc.with_ymd_and_hms(PROBE_YEARS.0, 1, 1, 0, 0, 0)
        .single()
        .is_some_and(|start| probe.find_next_occurrence(&start, true).is_ok())
}

/// A parsed crontab schedule that preserves the original expression and time zone for
/// listings.
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
        let tz = timezone.map_or(Ok(Tz::UTC), parse_zone)?;
        // Five fields exactly: a seconds field would promise a resolution the
        // contract gives to `interval`, and a year field is no crontab.
        let cron = CronParser::builder()
            .seconds(Seconds::Disallowed)
            .year(Year::Disallowed)
            .build()
            .parse(expression)
            .map_err(|e| SpecError::InvalidSchedule(format!("'{expression}': {e}")))?;
        // `0 0 30 2 *` parses and never fires. Registering it would succeed
        // and then do nothing, which is the failure a caller cannot see.
        if !ever_matches(expression) {
            return Err(SpecError::InvalidSchedule(format!(
                "'{expression}' never matches a date"
            )));
        }
        Ok(Self {
            expression: expression.to_string(),
            timezone: timezone.map(str::to_string),
            cron,
            tz,
        })
    }

    pub fn expression(&self) -> &str {
        &self.expression
    }

    pub fn timezone(&self) -> Option<&str> {
        self.timezone.as_deref()
    }

    /// The first matching instant after `after`, or `None` if no later match exists.
    pub fn next_after(&self, after: Wall) -> Option<Wall> {
        let utc: DateTime<Utc> = Utc.timestamp_opt(after.seconds, after.nanoseconds).single()?;
        let local = utc.with_timezone(&self.tz);
        let next = self.cron.find_next_occurrence(&local, false).ok()?;
        Some(Wall {
            seconds: next.timestamp(),
            nanoseconds: next.timestamp_subsec_nanos(),
        })
    }

    /// The latest matching instant at or before `at`, or `None` if no earlier match
    /// exists.
    pub fn last_at_or_before(&self, at: Wall) -> Option<Wall> {
        let utc: DateTime<Utc> = Utc.timestamp_opt(at.seconds, at.nanoseconds).single()?;
        let local = utc.with_timezone(&self.tz);
        let last = self.cron.find_previous_occurrence(&local, true).ok()?;
        Some(Wall {
            seconds: last.timestamp(),
            nanoseconds: last.timestamp_subsec_nanos(),
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

/// Validate a name against `schedule-name`.
///
/// # Errors
///
/// `InvalidName` for empty names, names longer than [`MAX_NAME_LEN`], or characters
/// other than ASCII letters, digits, `-`, `_`, and `.`.
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

/// Parse durations such as `500ms`, `30s`, or `1h30m` into nanoseconds.
///
/// # Errors
///
/// A diagnostic message for a missing number, missing or unknown unit, or total
/// exceeding `duration` capacity.
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

/// Schedule limits and retry policy for one workload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Maximum concurrent registrations through `scheduler.register`.
    pub max_registrations: usize,
    /// Minimum `interval` period and `once-after` delay in nanoseconds.
    pub min_interval: u64,
    /// Maximum delivery attempts per firing, including the first.
    pub retry_max_attempts: u32,
    /// Initial retry delay in nanoseconds, doubling with each redelivery.
    pub retry_backoff: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_registrations: 32,
            min_interval: NANOS_PER_SEC,
            retry_max_attempts: 3,
            retry_backoff: NANOS_PER_SEC,
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

/// Declared schedules and limits from a workload's `cosmonic:cron` binding.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Binding {
    /// The schedules it declares, by name.
    pub declared: BTreeMap<String, Spec>,
    pub limits: Limits,
}

impl Binding {
    /// Parse one binding entry's `config` key/value pairs.
    ///
    /// # Errors
    ///
    /// See [`Binding::parse_entries`].
    pub fn parse<'a>(
        config: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, String> {
        Self::parse_entries([config])
    }

    /// Parse all `cosmonic:cron` binding entries for a workload.
    ///
    /// Entries declare schedules separately but share limits, each set by at most one
    /// entry.
    /// Each `timezone` applies only to its entry's `schedule.*` keys.
    ///
    /// # Errors
    ///
    /// A diagnostic message for the first unknown key, invalid value, duplicate
    /// schedule name, or limit set by multiple entries, causing the bind to fail.
    pub fn parse_entries<'a, E>(entries: impl IntoIterator<Item = E>) -> Result<Self, String>
    where
        E: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let entries: Vec<Vec<(&str, &str)>> = entries
            .into_iter()
            .map(|entry| entry.into_iter().map(|(key, value)| (key, value.trim())).collect())
            .collect();

        // Limits first, across every entry: a schedule in one entry has to be
        // checked against a `min-interval` that another may set.
        let mut limits = Limits::default();
        let mut set: Vec<&str> = Vec::new();
        for (key, value) in entries.iter().flatten().copied() {
            if key.starts_with("schedule.") || key.starts_with("interval.") || key == "timezone" {
                continue;
            }
            if set.contains(&key) {
                // Two values for one limit cannot both hold, and choosing one
                // would quietly drop what the other entry asked for.
                return Err(format!(
                    "{key} is set by more than one cosmonic:cron entry; set it on one"
                ));
            }
            set.push(key);
            limits.set(key, value)?;
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
        for entry in &entries {
            let timezone = entry.iter().find(|(key, _)| *key == "timezone").map(|(_, zone)| *zone);
            if let Some(zone) = timezone {
                // Checked here and not only when a crontab uses it, so a
                // misspelled zone is refused even in an entry with none.
                parse_zone(zone).map_err(|e| format!("timezone: {e}"))?;
            }
            for (key, value) in entry.iter().copied() {
                if let Some(name) = key.strip_prefix("schedule.") {
                    let cron =
                        CronSpec::parse(value, timezone).map_err(|e| format!("{key}: {e}"))?;
                    declare(key.to_string(), name, Spec::Cron(cron))?;
                } else if let Some(name) = key.strip_prefix("interval.") {
                    let period = parse_duration(value).map_err(|e| format!("{key}: {e}"))?;
                    if period == 0 {
                        return Err(format!("{key}: a period cannot be zero"));
                    }
                    declare(key.to_string(), name, Spec::Interval(period))?;
                }
            }
        }
        Ok(Self { declared, limits })
    }
}

impl Limits {
    /// Set one limit from its binding key.
    fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let count = |least: u32| match value.parse::<u32>() {
            Ok(count) if count >= least => Ok(count),
            _ if least == 0 => Err(format!("{key}: '{value}' is not a count")),
            _ => Err(format!("{key}: '{value}' is not a count of at least {least}")),
        };
        let duration = || parse_duration(value).map_err(|e| format!("{key}: {e}"));
        match key {
            "max-registrations" => self.max_registrations = count(0)? as usize,
            "min-interval" => {
                let period = duration()?;
                if period < HARD_MIN_INTERVAL {
                    return Err(format!(
                        "min-interval: {} is below the {} this plugin supports",
                        format_duration(period),
                        format_duration(HARD_MIN_INTERVAL)
                    ));
                }
                self.min_interval = period;
            }
            "retry.max-attempts" => self.retry_max_attempts = count(1)?,
            "retry.backoff" => self.retry_backoff = duration()?,
            other => {
                return Err(format!(
                    "unknown key '{other}'; expected schedule.<name>, interval.<name>, \
                     timezone, max-registrations, min-interval, retry.max-attempts \
                     or retry.backoff"
                ));
            }
        }
        Ok(())
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
    fn cron_finds_the_latest_match_at_or_before() {
        let spec = CronSpec::parse("*/15 * * * *", None).unwrap();
        assert_eq!(spec.last_at_or_before(at(MONDAY + 900)), Some(at(MONDAY + 900)));
        assert_eq!(spec.last_at_or_before(at(MONDAY + 1_799)), Some(at(MONDAY + 900)));
        assert_eq!(spec.last_at_or_before(at(MONDAY + 899)), Some(at(MONDAY)));
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

    /// An instant in 2026, given in UTC.
    fn utc(month: u32, day: u32, hour: u32, minute: u32) -> Wall {
        at(Utc.with_ymd_and_hms(2026, month, day, hour, minute, 0).unwrap().timestamp())
    }

    /// The next `count` matches after `from`.
    fn matches(spec: &CronSpec, from: Wall, count: usize) -> Vec<Wall> {
        let mut found = Vec::new();
        let mut after = from;
        for _ in 0..count {
            after = spec.next_after(after).unwrap();
            found.push(after);
        }
        found
    }

    // The README states what follows as the plugin's behaviour. It is the
    // parser's, not this crate's, so these are here to notice an upgrade
    // changing it. In New York in 2026 the clocks go from 02:00 to 03:00 on
    // 8 March, and from 02:00 back to 01:00 on 1 November.

    #[test]
    fn a_time_of_day_that_daylight_saving_skips_fires_once_that_day() {
        let spec = CronSpec::parse("30 2 * * *", Some("America/New_York")).unwrap();
        // 02:30 does not occur on the 8th. The firing is not dropped: it
        // comes at 03:00, the first moment after the hour that was skipped.
        assert_eq!(
            matches(&spec, utc(3, 7, 12, 0), 2),
            [utc(3, 8, 7, 0), utc(3, 9, 6, 30)]
        );
    }

    #[test]
    fn a_time_of_day_that_daylight_saving_repeats_fires_once_that_day() {
        let spec = CronSpec::parse("30 1 * * *", Some("America/New_York")).unwrap();
        // 01:30 occurs twice on the 1st, at 05:30 and 06:30 UTC. Only the
        // first is a firing.
        assert_eq!(
            matches(&spec, utc(10, 31, 12, 0), 2),
            [utc(11, 1, 5, 30), utc(11, 2, 6, 30)]
        );
    }

    #[test]
    fn a_schedule_through_the_hour_keeps_its_rate_across_daylight_saving() {
        let spec = CronSpec::parse("*/30 * * * *", Some("America/New_York")).unwrap();
        // Half an hour apart in elapsed time both ways: nothing is invented
        // for the hour that is skipped, and the repeated hour is run through
        // twice.
        assert_eq!(
            matches(&spec, utc(3, 8, 6, 0), 3),
            [utc(3, 8, 6, 30), utc(3, 8, 7, 0), utc(3, 8, 7, 30)]
        );
        assert_eq!(
            matches(&spec, utc(11, 1, 4, 30), 5),
            [utc(11, 1, 5, 0), utc(11, 1, 5, 30), utc(11, 1, 6, 0), utc(11, 1, 6, 30), utc(11, 1, 7, 0)]
        );
    }

    #[test]
    fn cron_accepts_more_than_the_contract_promises() {
        // Also stated in the README: `?` for a day field, 7 for Sunday, and
        // the nicknames. 2026-10-18 is the Sunday after `MONDAY`.
        let sunday = at(MONDAY + 6 * 86_400);
        for expression in ["0 0 * * 7", "0 0 ? * 0", "0 0 ? * sun", "@weekly"] {
            let spec = CronSpec::parse(expression, None).unwrap();
            assert_eq!(spec.next_after(at(MONDAY)), Some(sunday), "{expression:?}");
        }
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
    fn cron_accepts_what_matches_only_rarely() {
        for expression in [
            "0 0 29 2 *",  // leap days
            "0 0 31 * *",  // seven months a year
            "0 0 29 2 1",  // POSIX: the 29th of February, or any Monday
            "@daily",
        ] {
            assert!(CronSpec::parse(expression, None).is_ok(), "{expression:?}");
        }
        let leap = CronSpec::parse("0 0 29 2 *", None).unwrap();
        // 2028-02-29T00:00:00Z.
        assert_eq!(leap.next_after(at(MONDAY)), Some(at(1_835_395_200)));
    }

    #[test]
    fn cron_refuses_every_spelling_of_a_date_that_does_not_exist() {
        for minute in 0..60 {
            for expression in [
                format!("{minute} 0 30 2 *"),
                format!("{minute} 0 31 2,4,6,9,11 *"),
                format!("{minute} 0 30,31 feb *"),
            ] {
                assert!(
                    matches!(CronSpec::parse(&expression, None), Err(SpecError::InvalidSchedule(_))),
                    "{expression:?} should be refused"
                );
            }
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
    fn a_misspelled_zone_is_refused_even_with_no_crontab_to_use_it() {
        for config in [
            vec![("timezone", "America/New_Yrok")],
            vec![("timezone", "America/New_Yrok"), ("interval.poll", "30s")],
        ] {
            let err = Binding::parse(config.clone()).unwrap_err();
            assert!(err.contains("timezone") && err.contains("unknown time zone"), "{config:?}: {err}");
        }
    }

    #[test]
    fn each_entry_reads_its_crontabs_in_its_own_zone() {
        let binding = Binding::parse_entries([
            vec![("schedule.report", "0 9 * * *"), ("timezone", "America/New_York")],
            vec![("schedule.sync", "0 9 * * *"), ("timezone", "Asia/Tokyo")],
            vec![("schedule.plain", "0 9 * * *")],
        ])
        .unwrap();
        let zone = |name: &str| match binding.declared.get(name) {
            Some(Spec::Cron(spec)) => spec.timezone().map(str::to_string),
            other => panic!("{name} should be a cron schedule, got {other:?}"),
        };
        assert_eq!(zone("report").as_deref(), Some("America/New_York"));
        assert_eq!(zone("sync").as_deref(), Some("Asia/Tokyo"));
        assert_eq!(zone("plain"), None);
    }

    #[test]
    fn limits_are_shared_by_every_entry_whichever_one_sets_them() {
        // The interval is in the first entry; the floor that admits it is in
        // the second.
        let binding = Binding::parse_entries([
            vec![("interval.fast", "100ms")],
            vec![("min-interval", "50ms"), ("retry.max-attempts", "7")],
        ])
        .unwrap();
        assert_eq!(binding.declared.get("fast"), Some(&Spec::Interval(100 * NANOS_PER_MILLI)));
        assert_eq!(binding.limits.retry_max_attempts, 7);
    }

    #[test]
    fn entries_may_not_disagree() {
        for (entries, expect) in [
            (
                [vec![("retry.max-attempts", "5")], vec![("retry.max-attempts", "9")]],
                "retry.max-attempts is set by more than one",
            ),
            (
                [vec![("schedule.a", "0 2 * * *")], vec![("interval.a", "30s")]],
                "declared more than once",
            ),
        ] {
            let err = Binding::parse_entries(entries.clone()).unwrap_err();
            assert!(err.contains(expect), "{entries:?} gave {err:?}");
        }
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
