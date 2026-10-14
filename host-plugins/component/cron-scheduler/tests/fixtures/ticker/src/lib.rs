//! Test workload for the cron-scheduler plugin.
//!
//! Export `handler` for scheduled firings and expose `scheduler` over HTTP
//! so tests can register schedules over HTTP:
//!
//! - `GET  /schedules`                              every schedule held, a line each,
//!   as `name<TAB>declared|registered<TAB>schedule<TAB>next=<seconds|none>`
//! - `POST /register?name=N&every-ms=MS`            an `interval` schedule
//! - `POST /register?name=N&cron=EXPR[&tz=ZONE]`    a `cron` schedule
//! - `POST /register?name=N&once-after-ms=MS`       a `once-after` schedule
//! - `POST /register?name=N&once-at=UNIX_SECONDS`   a `once-at` schedule
//! - `POST /register?name=N&other=TEXT`             a schedule kind nobody provides
//! - `POST /register-calendar?name=N[&minute-step=S]` a typed calendar schedule
//! - `POST /unregister?name=N`
//!
//! Log each firing to stderr as `ticker: fired ...` for the end-to-end test;
//! the schedule name selects handler behavior:
//!
//! - `retry-*` answers `retry` until the third attempt
//! - `fail-*`  answers `failed`
//! - `slow-*`  takes 2.5 seconds
//! - `trap-*`  panics
//! - `again-*` registers itself again as a one-second `once-after`
//! - anything else answers `ok`

mod bindings {
    #![allow(unsafe_code)]
    wit_bindgen::generate!({ world: "ticker", generate_all });
}

use bindings::cosmonic::cron::scheduler;
use bindings::cosmonic::cron::types::{
    CalendarPart, CalendarRange, CalendarSchedule, CronSchedule, Firing, Months,
    Schedule, ScheduleError, Weekdays,
};
use bindings::exports::cosmonic::cron::handler::{Guest as HandlerGuest, HandlerError};
use bindings::exports::wasi::http::handler::Guest as HttpGuest;
use bindings::wasi::clocks::monotonic_clock;
use bindings::wasi::clocks::system_clock::Instant;
use bindings::wasi::http::types::{ErrorCode, Fields, Method, Request, Response};

struct Ticker;

impl HandlerGuest for Ticker {
    async fn handle(firing: Firing) -> Result<(), HandlerError> {
        eprintln!(
            "ticker: fired name={} scheduled-at={} missed={} attempt={}",
            firing.name, firing.scheduled_at.seconds, firing.missed, firing.attempt
        );
        match firing.name.split('-').next() {
            Some("retry") if firing.attempt < 3 => {
                Err(HandlerError::Retry(Some(format!("attempt {} of 3", firing.attempt))))
            }
            Some("fail") => Err(HandlerError::Failed(Some("this schedule always fails".into()))),
            Some("slow") => {
                monotonic_clock::wait_for(2_500_000_000).await;
                eprintln!("ticker: finished name={}", firing.name);
                Ok(())
            }
            Some("trap") => panic!("deliberate trap in the handler"),
            Some("again") => scheduler::register(firing.name, Schedule::OnceAfter(1_000_000_000))
                .await
                .map_err(|err| HandlerError::Failed(Some(describe(&err)))),
            _ => Ok(()),
        }
    }
}

impl HttpGuest for Ticker {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let target = request.get_path_with_query().unwrap_or_else(|| "/".to_string());
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        let name = || param(query, "name").unwrap_or_default();

        let result = match (request.get_method(), path) {
            (Method::Get, "/schedules") => list().await,
            (Method::Post, "/register") => match schedule_from(query) {
                Some(schedule) => scheduler::register(name(), schedule).await.map(|()| "ok\n".into()),
                None => {
                    let kinds = "every-ms, cron, once-after-ms, once-at or other";
                    return Ok(respond(400, format!("give one of {kinds}\n")));
                }
            },
            (Method::Post, "/register-calendar") => match calendar_from(query) {
                Some(schedule) => scheduler::register_calendar(name(), schedule)
                    .await
                    .map(|()| "ok\n".into()),
                None => return Ok(respond(400, "invalid calendar parameters\n".into())),
            },
            (Method::Post, "/unregister") => {
                scheduler::unregister(name()).await.map(|()| "ok\n".into())
            }
            _ => return Ok(respond(404, "not found\n".into())),
        };
        Ok(match result {
            Ok(body) => respond(200, body),
            Err(err) => respond(status_of(&err), format!("{}\n", describe(&err))),
        })
    }
}

async fn list() -> Result<String, ScheduleError> {
    let held = scheduler::list_registrations().await?;
    Ok(held
        .into_iter()
        .map(|held| {
            let origin = if held.declared { "declared" } else { "registered" };
            let schedule = match held.schedule {
                Schedule::Cron(CronSchedule { expression, timezone: Some(zone) }) => {
                    format!("cron {expression} in {zone}")
                }
                Schedule::Cron(CronSchedule { expression, timezone: None }) => {
                    format!("cron {expression}")
                }
                Schedule::Interval(period) => format!("interval {}ms", period / 1_000_000),
                Schedule::OnceAt(at) => format!("once-at {}", at.seconds),
                Schedule::OnceAfter(delay) => format!("once-after {}ms", delay / 1_000_000),
                Schedule::Other(text) => format!("other {text}"),
            };
            let next = held.next.map_or("none".to_string(), |at| at.seconds.to_string());
            format!("{}\t{origin}\t{schedule}\tnext={next}\n", held.name)
        })
        .collect())
}

fn schedule_from(query: &str) -> Option<Schedule> {
    if let Some(millis) = param(query, "every-ms") {
        return Some(Schedule::Interval(millis.parse::<u64>().ok()? * 1_000_000));
    }
    if let Some(millis) = param(query, "once-after-ms") {
        return Some(Schedule::OnceAfter(millis.parse::<u64>().ok()? * 1_000_000));
    }
    if let Some(seconds) = param(query, "once-at") {
        return Some(Schedule::OnceAt(Instant { seconds: seconds.parse().ok()?, nanoseconds: 0 }));
    }
    if let Some(expression) = param(query, "cron") {
        return Some(Schedule::Cron(CronSchedule { expression, timezone: param(query, "tz") }));
    }
    param(query, "other").map(Schedule::Other)
}

fn calendar_from(query: &str) -> Option<CalendarSchedule> {
    let minutes = match param(query, "minute-step") {
        Some(step) => vec![CalendarPart::Every(step.parse().ok()?)],
        None => vec![CalendarPart::Any],
    };
    let hours = match param(query, "hour-start") {
        Some(start) => vec![CalendarPart::Range(CalendarRange {
            start: start.parse().ok()?,
            end: param(query, "hour-end")?.parse().ok()?,
        })],
        None => vec![CalendarPart::Any],
    };
    let days_of_month = match param(query, "day") {
        Some(day) => Some(vec![CalendarPart::At(day.parse().ok()?)]),
        None => None,
    };
    let weekdays = match param(query, "weekdays") {
        Some(bits) => Some(Weekdays::from_bits(bits.parse().ok()?)?),
        None => None,
    };
    let months = match param(query, "months") {
        Some(bits) => Some(Months::from_bits(bits.parse().ok()?)?),
        None => None,
    };
    Some(CalendarSchedule {
        minutes,
        hours,
        days_of_month,
        weekdays,
        months,
        timezone: param(query, "tz"),
    })
}

fn describe(err: &ScheduleError) -> String {
    match err {
        ScheduleError::InvalidName(detail) => format!("invalid-name: {detail}"),
        ScheduleError::InvalidSchedule(detail) => format!("invalid-schedule: {detail}"),
        ScheduleError::Unsupported(detail) => format!("unsupported: {detail}"),
        ScheduleError::AccessDenied => "access-denied".to_string(),
        ScheduleError::LimitExceeded => "limit-exceeded".to_string(),
        ScheduleError::NoHandler => "no-handler".to_string(),
        ScheduleError::Other(detail) => format!("other: {}", detail.clone().unwrap_or_default()),
    }
}

fn status_of(err: &ScheduleError) -> u16 {
    match err {
        ScheduleError::AccessDenied => 403,
        ScheduleError::LimitExceeded => 429,
        ScheduleError::Other(_) => 500,
        _ => 400,
    }
}

/// One query parameter, percent-decoded.
fn param(query: &str, name: &str) -> Option<String> {
    let raw = query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })?;
    let mut bytes = Vec::with_capacity(raw.len());
    let mut rest = raw.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        rest = tail;
        match byte {
            b'+' => bytes.push(b' '),
            b'%' if rest.len() >= 2 => {
                let hex = std::str::from_utf8(&rest[..2]).ok()?;
                bytes.push(u8::from_str_radix(hex, 16).ok()?);
                rest = &rest[2..];
            }
            other => bytes.push(other),
        }
    }
    String::from_utf8(bytes).ok()
}

fn respond(status: u16, body: String) -> Response {
    let headers = Fields::new();
    let _ = headers.set("content-type", &[b"text/plain; charset=utf-8".to_vec()]);
    let (mut tx, rx) = bindings::wit_stream::new();
    let (trailers_tx, trailers_rx) = bindings::wit_future::new(|| Ok(None));
    wit_bindgen::spawn_local(async move {
        tx.write_all(body.into_bytes()).await;
        drop(tx);
        let _ = trailers_tx.write(Ok(None)).await;
    });
    let (response, _sent) = Response::new(headers, Some(rx), trailers_rx);
    let _ = response.set_status_code(status);
    response
}

bindings::export!(Ticker with_types_in bindings);
