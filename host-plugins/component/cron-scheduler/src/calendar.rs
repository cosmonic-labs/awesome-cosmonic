//! Validate typed calendar fields and compile them into the shared cron engine.

use std::collections::BTreeSet;

use crate::spec::{CronSpec, SpecError};

/// One selection within a calendar field.
#[derive(Clone, Debug)]
pub enum Part {
    Any,
    At(u8),
    Range { start: u8, end: u8 },
    Every(u8),
    Stepped { start: u8, end: u8, step: u8 },
}

/// Weekdays numbered for POSIX cron.
#[derive(Clone, Copy, Debug)]
pub enum Weekday {
    Sunday = 0,
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
}

/// Months numbered for POSIX cron.
#[derive(Clone, Copy, Debug)]
pub enum Month {
    January = 1,
    February,
    March,
    April,
    May,
    June,
    July,
    August,
    September,
    October,
    November,
    December,
}

/// A calendar schedule with POSIX day matching and optional UTC-default time zone.
#[derive(Clone, Debug)]
pub struct Calendar {
    pub minutes: Vec<Part>,
    pub hours: Vec<Part>,
    pub days_of_month: Option<Vec<Part>>,
    pub weekdays: Option<Vec<Weekday>>,
    pub months: Option<Vec<Month>>,
    pub timezone: Option<String>,
}

impl Calendar {
    /// Compile validated fields into a normalized five-field cron schedule.
    ///
    /// # Errors
    ///
    /// `InvalidSchedule` for empty selections, invalid values, ranges or steps,
    /// unknown time zones, or schedules that never match.
    pub fn compile(&self) -> Result<CronSpec, SpecError> {
        let minutes = selector("minutes", &self.minutes, 0, 59)?;
        let hours = selector("hours", &self.hours, 0, 23)?;
        let days = self.days_of_month.as_ref().map_or_else(
            || Ok("*".to_string()),
            |parts| selector("days-of-month", parts, 1, 31),
        )?;
        let weekdays = match &self.weekdays {
            // All seven weekdays are a wildcard under the POSIX day-field OR rule.
            Some(days)
                if days
                    .iter()
                    .map(|day| *day as u8)
                    .collect::<BTreeSet<_>>()
                    .len()
                    == 7 =>
            {
                "*".to_string()
            }
            Some(days) => named("weekdays", days.iter().map(|day| *day as u8))?,
            None => "*".to_string(),
        };
        let months = self.months.as_ref().map_or_else(
            || Ok("*".to_string()),
            |months| named("months", months.iter().map(|month| *month as u8)),
        )?;
        CronSpec::parse(
            &format!("{minutes} {hours} {days} {months} {weekdays}"),
            self.timezone.as_deref(),
        )
    }
}

fn invalid(field: &str, detail: &str) -> SpecError {
    SpecError::InvalidSchedule(format!("{field}: {detail}"))
}

fn named(field: &str, values: impl Iterator<Item = u8>) -> Result<String, SpecError> {
    let values: BTreeSet<_> = values.collect();
    if values.is_empty() {
        return Err(invalid(field, "selection cannot be empty"));
    }
    Ok(values
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(","))
}

fn selector(field: &str, parts: &[Part], min: u8, max: u8) -> Result<String, SpecError> {
    if parts.is_empty() {
        return Err(invalid(field, "selection cannot be empty"));
    }
    let mut values = BTreeSet::new();
    let mut any = false;
    for part in parts {
        let (start, end, step) = match *part {
            Part::Any => {
                any = true;
                (min, max, 1)
            }
            Part::At(value) => (value, value, 1),
            Part::Range { start, end } => (start, end, 1),
            Part::Every(step) => (min, max, step),
            Part::Stepped { start, end, step } => (start, end, step),
        };
        if start < min || end > max || start > end {
            return Err(invalid(
                field,
                &format!("range must be ordered and within {min}-{max}"),
            ));
        }
        if step == 0 {
            return Err(invalid(field, "step must be positive"));
        }
        values.extend((start..=end).step_by(usize::from(step)));
    }
    // A wildcard must remain distinct from a restricted day field covering all values.
    if any {
        Ok("*".to_string())
    } else {
        named(field, values.into_iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, Now};
    use crate::spec::{Binding, Limits, Spec, Wall};
    use chrono::{TimeZone as _, Utc};

    const MONDAY: i64 = 1_791_763_200;

    fn calendar() -> Calendar {
        Calendar {
            minutes: vec![Part::At(0)],
            hours: vec![Part::At(9)],
            days_of_month: None,
            weekdays: None,
            months: None,
            timezone: None,
        }
    }

    fn wall(seconds: i64) -> Wall {
        Wall {
            seconds,
            nanoseconds: 0,
        }
    }

    #[test]
    fn selectors_compile_to_the_same_occurrences_as_cron_text() {
        let calendar = Calendar {
            minutes: vec![Part::Every(15)],
            hours: vec![Part::Range { start: 9, end: 17 }],
            weekdays: Some(vec![Weekday::Monday, Weekday::Wednesday, Weekday::Friday]),
            timezone: Some("America/New_York".into()),
            ..calendar()
        };
        let typed = calendar.compile().unwrap();
        let text = CronSpec::parse("*/15 9-17 * * mon,wed,fri", Some("America/New_York")).unwrap();
        let mut after = wall(MONDAY);
        for _ in 0..100 {
            let next = typed.next_after(after).unwrap();
            assert_eq!(Some(next), text.next_after(after));
            after = next;
        }
    }

    #[test]
    fn selectors_normalize_order_duplicates_and_overlapping_ranges() {
        let calendar = Calendar {
            minutes: vec![
                Part::At(30),
                Part::Stepped {
                    start: 0,
                    end: 45,
                    step: 15,
                },
                Part::At(0),
            ],
            months: Some(vec![Month::December, Month::January, Month::December]),
            weekdays: Some(vec![Weekday::Friday, Weekday::Monday, Weekday::Friday]),
            ..calendar()
        };
        assert_eq!(
            calendar.compile().unwrap().expression(),
            "0,15,30,45 9 * 1,12 1,5"
        );
    }

    #[test]
    fn typed_fields_preserve_fixed_and_recurring_daylight_saving_behavior() {
        let spring = wall(
            Utc.with_ymd_and_hms(2026, 3, 7, 12, 0, 0)
                .unwrap()
                .timestamp(),
        );
        let fall = wall(
            Utc.with_ymd_and_hms(2026, 10, 31, 12, 0, 0)
                .unwrap()
                .timestamp(),
        );
        for (minutes, hours, expression, from, count) in [
            (
                vec![Part::At(30)],
                vec![Part::At(2)],
                "30 2 * * *",
                spring,
                3,
            ),
            (vec![Part::At(30)], vec![Part::At(1)], "30 1 * * *", fall, 3),
            (
                vec![Part::Every(30)],
                vec![Part::Any],
                "*/30 * * * *",
                spring,
                80,
            ),
            (
                vec![Part::Every(30)],
                vec![Part::Any],
                "*/30 * * * *",
                fall,
                80,
            ),
        ] {
            let calendar = Calendar {
                minutes,
                hours,
                timezone: Some("America/New_York".into()),
                ..calendar()
            };
            let typed = calendar.compile().unwrap();
            let text = CronSpec::parse(expression, Some("America/New_York")).unwrap();
            let mut after = from;
            for _ in 0..count {
                let next = typed.next_after(after).unwrap();
                assert_eq!(Some(next), text.next_after(after), "{expression}");
                after = next;
            }
        }
    }

    #[test]
    fn restricted_day_fields_match_with_or() {
        let calendar = Calendar {
            days_of_month: Some(vec![Part::At(13)]),
            weekdays: Some(vec![Weekday::Monday]),
            ..calendar()
        };
        let cron = calendar.compile().unwrap();
        assert_eq!(cron.next_after(wall(MONDAY)), Some(wall(MONDAY + 9 * 3600)));
        assert_eq!(
            cron.next_after(wall(MONDAY + 9 * 3600)),
            Some(wall(MONDAY + 86_400 + 9 * 3600))
        );
    }

    #[test]
    fn wildcards_and_full_restricted_day_selections_remain_distinct() {
        let mut calendar = Calendar {
            days_of_month: Some(vec![Part::Any]),
            weekdays: Some(vec![Weekday::Friday]),
            ..calendar()
        };
        assert_eq!(
            calendar.compile().unwrap().next_after(wall(MONDAY)),
            Some(wall(MONDAY + 4 * 86_400 + 9 * 3600))
        );
        calendar.days_of_month = Some(vec![Part::Range { start: 1, end: 31 }]);
        assert_eq!(
            calendar.compile().unwrap().next_after(wall(MONDAY)),
            Some(wall(MONDAY + 9 * 3600))
        );
    }

    #[test]
    fn all_seven_weekdays_leave_the_weekday_field_unrestricted() {
        let all = vec![
            Weekday::Sunday,
            Weekday::Monday,
            Weekday::Tuesday,
            Weekday::Wednesday,
            Weekday::Thursday,
            Weekday::Friday,
            Weekday::Saturday,
        ];
        let mut calendar = Calendar {
            days_of_month: Some(vec![Part::At(13)]),
            weekdays: Some(all),
            ..calendar()
        };
        let unset = Calendar {
            weekdays: None,
            ..calendar.clone()
        };
        assert_eq!(calendar.compile().unwrap(), unset.compile().unwrap());
        assert_eq!(
            calendar.compile().unwrap().next_after(wall(MONDAY)),
            Some(wall(MONDAY + 86_400 + 9 * 3600))
        );
        calendar.weekdays.as_mut().unwrap().push(Weekday::Sunday);
        assert_eq!(calendar.compile().unwrap(), unset.compile().unwrap());
    }

    #[test]
    fn invalid_selectors_are_rejected_even_alongside_a_wildcard() {
        for (min, max) in [(0, 59), (0, 23), (1, 31)] {
            let mut invalid_parts = vec![
                Part::At(max + 1),
                Part::Range {
                    start: max,
                    end: min,
                },
                Part::Range {
                    start: min,
                    end: max + 1,
                },
                Part::Every(0),
                Part::Stepped {
                    start: min,
                    end: max,
                    step: 0,
                },
            ];
            if min > 0 {
                invalid_parts.push(Part::At(min - 1));
            }
            assert!(selector("field", &[], min, max).is_err());
            for part in invalid_parts {
                assert!(selector("field", &[Part::Any, part], min, max).is_err());
            }
        }
    }

    #[test]
    fn empty_flags_unknown_zones_and_impossible_dates_are_rejected() {
        for calendar in [
            Calendar {
                weekdays: Some(vec![]),
                ..calendar()
            },
            Calendar {
                months: Some(vec![]),
                ..calendar()
            },
            Calendar {
                timezone: Some("Mars/Phobos".into()),
                ..calendar()
            },
            Calendar {
                days_of_month: Some(vec![Part::At(30)]),
                months: Some(vec![Month::February]),
                ..calendar()
            },
        ] {
            assert!(matches!(
                calendar.compile(),
                Err(SpecError::InvalidSchedule(_))
            ));
        }
    }

    #[test]
    fn typed_and_text_registration_share_names_and_limits() {
        let mut engine = Engine::new();
        let limits = Limits {
            max_registrations: 1,
            ..Limits::default()
        };
        let now = |secs: u64| Now {
            mono: secs * 1_000_000_000,
            wall: wall(MONDAY + secs as i64),
        };
        engine.bind(
            "workload",
            Binding {
                limits,
                ..Binding::default()
            },
            true,
            now(0),
        );
        let typed = calendar().compile().unwrap();
        let text = CronSpec::parse(typed.expression(), typed.timezone()).unwrap();
        assert_eq!(
            engine.register("workload", "job", Spec::Cron(typed), now(0)),
            Ok(true)
        );
        assert_eq!(
            engine.register("workload", "job", Spec::Cron(text.clone()), now(10)),
            Ok(false)
        );
        assert_eq!(
            engine.register("workload", "other", Spec::Cron(text), now(10)),
            Err(crate::engine::RegisterError::LimitExceeded)
        );
    }
}
