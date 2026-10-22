//! End-to-end test: load the plugin into a real host, deploy the ticker
//! workload beside it, and check that schedules fire the way `cosmonic:cron`
//! says they do.
//!
//! ```sh
//! WASH=/path/to/wash cargo e2e
//! ```
//!
//! `cargo e2e` is an alias for `cargo test --test e2e -- --ignored --nocapture`.
//! The test is ignored by default because it needs a `wash` built with the
//! `host-component-plugins` feature, which the released binaries are not, and
//! because it takes up to a minute and a half, most of it waiting for a crontab
//! minute to come round.
//!
//! It is one test and not one per rule. Starting a host is the slow part, so
//! the rules are checked in order against a single run of it. Every check is
//! reported, and the test fails at the end if any of them did.
#![cfg(unix)]

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustix::process::{Pid, Signal, kill_process};

/// Where the ticker's `.wash/config.yaml` has `wash dev` listen.
const BASE: &str = "http://127.0.0.1:8787";

/// One `ticker: fired ...` line: a firing as the workload's handler saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fired {
    id: String,
    name: String,
    scheduled_at: i64,
    missed: u32,
    attempt: u32,
}

impl Fired {
    fn parse(line: &str) -> Option<Self> {
        let (_, fields) = line.split_once("ticker: fired ")?;
        let field = |key: &str| {
            fields
                .split_whitespace()
                .find_map(|field| field.strip_prefix(key)?.strip_prefix('='))
        };
        Some(Self {
            id: field("id")?.to_string(),
            name: field("name")?.to_string(),
            scheduled_at: field("scheduled-at")?.parse().ok()?,
            missed: field("missed")?.parse().ok()?,
            attempt: field("attempt")?.parse().ok()?,
        })
    }
}

/// One line of the ticker's `GET /schedules`: a schedule the plugin holds.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Listed {
    name: String,
    /// `declared` or `registered`.
    origin: String,
    schedule: String,
    /// When it is next due, in seconds on the system clock.
    next: Option<i64>,
}

impl Listed {
    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.split('\t');
        let (name, origin, schedule) = (fields.next()?, fields.next()?, fields.next()?);
        let next = match fields.next()?.strip_prefix("next=")? {
            "none" => None,
            at => Some(at.parse().ok()?),
        };
        Some(Self {
            name: name.to_string(),
            origin: origin.to_string(),
            schedule: schedule.to_string(),
            next,
        })
    }

    fn is(&self, origin: &str, schedule: &str) -> bool {
        self.origin == origin && self.schedule == schedule
    }
}

/// The host: `wash dev` run in the ticker's directory, logging to a file.
struct Host {
    child: Option<Child>,
    log: PathBuf,
}

impl Host {
    fn start(wash: &str, workload: &Path) -> Self {
        let log =
            std::env::temp_dir().join(format!("cron-scheduler-e2e-{}.log", std::process::id()));
        let stdout = File::create(&log).expect("create the host log");
        let stderr = stdout
            .try_clone()
            .expect("share the host log between stdout and stderr");
        let child = Command::new(wash)
            .arg("dev")
            .current_dir(workload)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .unwrap_or_else(|err| panic!("could not run `{wash} dev`: {err}"));
        Self {
            child: Some(child),
            log,
        }
    }

    /// Whether the host process has ended.
    fn exited(&mut self) -> bool {
        self.child
            .as_mut()
            .is_none_or(|child| !matches!(child.try_wait(), Ok(None)))
    }

    /// Everything the host, the plugin and the workload have logged so far.
    fn log(&self) -> String {
        String::from_utf8_lossy(&fs::read(&self.log).unwrap_or_default()).into_owned()
    }

    /// The log lines that contain every one of `parts`, in that order.
    fn lines(&self, parts: &[&str]) -> Vec<String> {
        self.log()
            .lines()
            .filter(|line| contains_in_order(line, parts))
            .map(str::to_string)
            .collect()
    }

    fn logged(&self, parts: &[&str]) -> bool {
        !self.lines(parts).is_empty()
    }

    /// Wait up to `seconds` for a log line containing `parts` in order.
    fn wait_for(&self, parts: &[&str], seconds: u64) -> bool {
        wait_until(seconds, || self.logged(parts))
    }

    /// The firings of one schedule the workload has handled, oldest first.
    fn fired(&self, name: &str) -> Vec<Fired> {
        self.log()
            .lines()
            .filter_map(Fired::parse)
            .filter(|fired| fired.name == name)
            .collect()
    }

    /// Wait up to `seconds` for a firing of `name` that satisfies `wanted`.
    fn wait_for_firing(&self, name: &str, seconds: u64, wanted: impl Fn(&Fired) -> bool) -> bool {
        wait_until(seconds, || self.fired(name).iter().any(&wanted))
    }

    /// Interrupt the host and wait for it to go.
    ///
    /// An interrupt and not a kill: the host unbinds its workloads on the way
    /// down, and the plugin dropping their schedules is one of the checks.
    fn stop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = kill_process(Pid::from_child(&child), Signal::INT);
        if !wait_until(15, || !matches!(child.try_wait(), Ok(None))) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Host {
    /// So a check that panics does not leave a host holding the port.
    fn drop(&mut self) {
        self.stop();
    }
}

/// The ticker workload's HTTP routes, which drive `scheduler` on its behalf.
struct Ticker(ureq::Agent);

impl Ticker {
    fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            // A refusal is an answer to check, not a failed request.
            .http_status_as_error(false)
            // The host is on loopback whatever the environment says.
            .proxy(None)
            .build();
        Self(config.into())
    }

    /// The status and body of a `GET`, or `None` if the host did not answer.
    fn get(&self, path: &str) -> Option<(u16, String)> {
        let mut response = self.0.get(format!("{BASE}{path}")).call().ok()?;
        let body = response.body_mut().read_to_string().ok()?;
        Some((response.status().as_u16(), body))
    }

    /// The status and body of a `POST`, or `None` if the host did not answer.
    fn post(&self, path: &str) -> Option<(u16, String)> {
        let mut response = self.0.post(format!("{BASE}{path}")).send_empty().ok()?;
        let body = response.body_mut().read_to_string().ok()?;
        Some((response.status().as_u16(), body))
    }

    /// `scheduler.list-registrations`, as the workload sees it.
    fn listing(&self) -> Vec<Listed> {
        let (_, body) = self.get("/schedules").unwrap_or_default();
        body.lines().filter_map(Listed::parse).collect()
    }

    /// The listing's entry for one schedule, if it is held.
    fn held(&self, name: &str) -> Option<Listed> {
        self.listing()
            .into_iter()
            .find(|listed| listed.name == name)
    }
}

/// One run of the host, and what has been checked against it so far.
struct Run {
    host: Host,
    ticker: Ticker,
    passed: usize,
    failed: Vec<String>,
}

impl Run {
    fn section(&self, title: &str) {
        println!("{title}");
    }

    fn check(&mut self, what: impl AsRef<str>, ok: bool) {
        self.check_or(what, ok, String::new);
    }

    /// A check that has something to say about why it failed.
    fn check_or(&mut self, what: impl AsRef<str>, ok: bool, detail: impl FnOnce() -> String) {
        let what = what.as_ref();
        if ok {
            self.passed += 1;
            println!("  ok    {what}");
            return;
        }
        println!("  FAIL  {what}");
        let detail = detail();
        if !detail.is_empty() {
            println!("        {detail}");
        }
        self.failed.push(what.to_string());
    }

    /// Expect a `POST` to answer with a status and a body containing `text`.
    fn expect(&mut self, what: &str, path: &str, status: u16, text: &str) {
        let got = self.ticker.post(path);
        let ok = matches!(&got, Some((code, body)) if *code == status && body.contains(text));
        self.check_or(what, ok, || {
            format!("expected {status} containing '{text}', got {got:?}")
        });
    }
}

/// Whether `line` contains every one of `parts`, in that order.
fn contains_in_order(line: &str, parts: &[&str]) -> bool {
    let mut rest = line;
    parts.iter().all(|part| match rest.find(part) {
        Some(at) => {
            rest = &rest[at + part.len()..];
            true
        }
        None => false,
    })
}

/// Poll `done` until it holds or `seconds` have passed, and say which.
fn wait_until(seconds: u64, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(200));
    }
}

fn pause(millis: u64) {
    sleep(Duration::from_millis(millis));
}

/// A listing's `next`, for a check's label.
fn shown(next: Option<i64>) -> String {
    next.map_or("none".to_string(), |at| at.to_string())
}

/// The system clock, in whole seconds.
fn unix_now() -> i64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock set after 1970");
    i64::try_from(since.as_secs()).expect("a clock set before the year 292277026596")
}

/// `wash build` in a project directory.
fn build(wash: &str, project: &Path) {
    let output = Command::new(wash).arg("build").current_dir(project).output().unwrap_or_else(|err| {
        panic!("could not run `{wash} build`: {err}. Set WASH to a wash built with host-component-plugins.")
    });
    assert!(
        output.status.success(),
        "`{wash} build` failed in {}:\n{}",
        project.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "needs a wash built with host-component-plugins: WASH=/path/to/wash cargo e2e"]
#[allow(clippy::too_many_lines)]
fn schedules_fire_the_way_the_contract_says() {
    let wash = std::env::var("WASH").unwrap_or_else(|_| "wash".to_string());
    let plugin = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workload = plugin.join("tests/fixtures/ticker");

    println!("building the plugin and the ticker workload");
    build(&wash, plugin);
    build(&wash, &workload);

    let mut host = Host::start(&wash, &workload);
    println!("starting the host (log: {})", host.log.display());
    let ticker = Ticker::new();
    let serving = |ticker: &Ticker| matches!(ticker.get("/schedules"), Some((200, _)));
    wait_until(60, || host.exited() || serving(&ticker));
    assert!(
        serving(&ticker),
        "the host never served the workload:\n{}",
        host.log()
    );
    let mut run = Run {
        host,
        ticker,
        passed: 0,
        failed: Vec::new(),
    };

    run.section("declared schedules");
    run.check(
        "the plugin's dispatch loop started",
        run.host.logged(&["cron-scheduler: dispatch loop started"]),
    );
    let listed = run.ticker.listing();
    let now = unix_now();
    let held = |name: &str| listed.iter().find(|listed| listed.name == name);
    let beat = held("heartbeat");
    let minutely = held("minutely");
    run.check(
        "the binding's interval is listed as declared",
        beat.is_some_and(|held| held.is("declared", "interval 5000ms")),
    );
    run.check(
        "the binding's crontab is listed as declared",
        minutely.is_some_and(|held| held.is("declared", "cron * * * * *")),
    );
    let next_beat = beat.and_then(|held| held.next);
    let next_minute = minutely.and_then(|held| held.next);
    run.check(
        format!(
            "an interval's next firing is within its period ({})",
            shown(next_beat)
        ),
        next_beat.is_some_and(|at| (now - 1..=now + 5).contains(&at)),
    );
    run.check_or(
        format!(
            "a crontab's next firing is the coming minute boundary ({})",
            shown(next_minute)
        ),
        next_minute.is_some_and(|at| (now - 1..=now + 60).contains(&at) && at % 60 == 0),
        || format!("listed at {now}"),
    );
    run.check(
        "a declared interval fires without the workload registering anything",
        run.host.wait_for_firing("heartbeat", 8, |_| true),
    );

    run.section("registering at run time");
    // The loop has just begun a five-second sleep for the next heartbeat. A
    // registration has to cut that short, or these firings arrive late.
    run.expect(
        "register an interval",
        "/register?name=poll&every-ms=500",
        200,
        "ok",
    );
    pause(1_700);
    let polls = run.host.fired("poll").len();
    run.check_or(
        format!("a new schedule wakes the sleeping loop ({polls} firings in 1.7s)"),
        polls >= 2,
        || "expected at least 2".to_string(),
    );
    run.expect(
        "registering the same schedule again is accepted",
        "/register?name=poll&every-ms=500",
        200,
        "ok",
    );
    run.check(
        "and is not logged a second time, having changed nothing",
        run.host.lines(&["registered 'poll'"]).len() == 1,
    );
    run.check(
        "a registered schedule is listed as registered",
        run.ticker
            .held("poll")
            .is_some_and(|held| held.is("registered", "interval 500ms") && held.next.is_some()),
    );

    run.section("refusals");
    run.expect(
        "a name with a space",
        "/register?name=bad%20name&every-ms=1000",
        400,
        "invalid-name",
    );
    run.expect(
        "a period under min-interval",
        "/register?name=fast&every-ms=100",
        400,
        "invalid-schedule",
    );
    run.expect(
        "a malformed crontab",
        "/register?name=c&cron=not+a+cron",
        400,
        "invalid-schedule",
    );
    run.expect(
        "a crontab that never matches",
        "/register?name=c&cron=0+0+30+2+*",
        400,
        "invalid-schedule",
    );
    run.expect(
        "an unknown time zone",
        "/register?name=c&cron=0+9+*+*+*&tz=Mars/Phobos",
        400,
        "invalid-schedule",
    );
    run.expect(
        "a schedule kind the plugin lacks",
        "/register?name=o&other=every+full+moon",
        400,
        "unsupported",
    );
    run.expect(
        "replacing a declared schedule",
        "/register?name=heartbeat&every-ms=1000",
        403,
        "access-denied",
    );
    run.expect(
        "removing a declared schedule",
        "/unregister?name=heartbeat",
        403,
        "access-denied",
    );
    run.expect(
        "removing a name no schedule could have",
        "/unregister?name=bad%20name",
        400,
        "invalid-name",
    );

    run.section("unregistering stops the calls");
    run.expect(
        "unregister the interval",
        "/unregister?name=poll",
        200,
        "ok",
    );
    pause(300);
    let before = run.host.fired("poll").len();
    pause(1_500);
    let after = run.host.fired("poll").len();
    run.check_or("no firing after unregister", before == after, || {
        format!("{} more arrived", after - before)
    });

    run.section("typed calendar schedules");
    let calendar_path = "/register-calendar?name=calendar&minute-step=15&hour-start=9&hour-end=17&day=13&weekdays=62&months=2049&tz=America/New_York";
    run.expect("register typed calendar fields", calendar_path, 200, "ok");
    run.check(
        "named flags and selectors are listed as a normalized cron schedule",
        run.ticker.held("calendar").is_some_and(|held| {
            held.is(
                "registered",
                "cron 0,15,30,45 9,10,11,12,13,14,15,16,17 13 1,12 1,2,3,4,5 in America/New_York",
            )
        }),
    );
    run.expect("repeat typed registration", calendar_path, 200, "ok");
    run.check(
        "an unchanged typed registration is not logged again",
        run.host.lines(&["registered 'calendar'"]).len() == 1,
    );
    run.expect(
        "replace it through the text API",
        "/register?name=calendar&cron=*+*+*+*+*",
        200,
        "ok",
    );
    run.expect(
        "register the same schedule through the typed API",
        "/register-calendar?name=calendar",
        200,
        "ok",
    );
    run.check(
        "text and typed registrations share the same name and identity",
        run.host.lines(&["registered 'calendar'"]).len() == 2,
    );
    let typed = run.ticker.held("calendar");
    run.check(
        "default typed fields preserve the text schedule in listings",
        typed
            .as_ref()
            .is_some_and(|held| held.is("registered", "cron * * * * *")),
    );
    run.expect(
        "typed registration cannot replace a declared schedule",
        "/register-calendar?name=minutely",
        403,
        "access-denied",
    );
    run.expect(
        "a typed zero step",
        "/register-calendar?name=invalid&minute-step=0",
        400,
        "invalid-schedule",
    );
    run.expect(
        "a typed hour outside its range",
        "/register-calendar?name=invalid&hour-start=24&hour-end=25",
        400,
        "invalid-schedule",
    );
    run.expect(
        "a typed day outside its range",
        "/register-calendar?name=invalid&day=0",
        400,
        "invalid-schedule",
    );
    run.expect(
        "empty weekday flags",
        "/register-calendar?name=invalid&weekdays=0",
        400,
        "invalid-schedule",
    );
    run.expect(
        "empty month flags",
        "/register-calendar?name=invalid&months=0",
        400,
        "invalid-schedule",
    );
    run.expect(
        "a typed unknown time zone",
        "/register-calendar?name=invalid&tz=Mars/Phobos",
        400,
        "invalid-schedule",
    );
    run.expect(
        "remove the typed registration through the shared API",
        "/unregister?name=calendar",
        200,
        "ok",
    );

    run.section("limits");
    run.expect(
        "a slow interval",
        "/register?name=second&every-ms=60000",
        200,
        "ok",
    );
    run.expect(
        "a crontab in a named zone",
        "/register?name=ny&cron=0+9+*+*+mon-fri&tz=America/New_York",
        200,
        "ok",
    );
    // 09:00 in New York is 13:00 or 14:00 UTC, depending on daylight saving.
    run.check(
        "the zone is kept in the listing, and its next firing is 09:00 there",
        run.ticker.held("ny").is_some_and(|held| {
            held.is("registered", "cron 0 9 * * mon-fri in America/New_York")
                && held.next.is_some_and(|at| {
                    at > unix_now() && at % 3_600 == 0 && matches!(at % 86_400 / 3_600, 13 | 14)
                })
        }),
    );
    run.expect(
        "a third registration",
        "/register?name=third&every-ms=60000",
        200,
        "ok",
    );
    run.expect(
        "a fourth, past max-registrations",
        "/register?name=fourth&every-ms=60000",
        429,
        "limit-exceeded",
    );
    run.expect(
        "typed registration shares max-registrations",
        "/register-calendar?name=fourth",
        429,
        "limit-exceeded",
    );
    run.expect("unregister", "/unregister?name=third", 200, "ok");
    run.expect("unregister again", "/unregister?name=third", 200, "ok");
    run.expect("unregister the crontab", "/unregister?name=ny", 200, "ok");
    run.expect(
        "unregister the slow interval",
        "/unregister?name=second",
        200,
        "ok",
    );

    run.section("retry");
    run.expect(
        "register a handler that asks for a retry",
        "/register?name=retry-job&every-ms=4000",
        200,
        "ok",
    );
    run.host
        .wait_for_firing("retry-job", 9, |fired| fired.attempt == 3);
    let attempts: Vec<Fired> = run.host.fired("retry-job").into_iter().take(3).collect();
    run.check(
        "the firing is delivered three times",
        attempts.iter().map(|fired| fired.attempt).eq([1, 2, 3]),
    );
    run.check(
        "every delivery carries the same scheduled-at",
        attempts.first().is_some_and(|first| {
            attempts
                .iter()
                .all(|fired| fired.scheduled_at == first.scheduled_at)
        }),
    );
    run.check(
        "redeliveries preserve the firing ID",
        attempts.first().is_some_and(|first| {
            !first.id.is_empty() && attempts.iter().all(|fired| fired.id == first.id)
        }),
    );
    run.check(
        "the backoff starts at retry.backoff",
        run.host
            .logged(&["'retry-job' ", " will be retried in 500ms: attempt 1 of 3"]),
    );
    run.check(
        "the backoff doubles",
        run.host
            .logged(&["'retry-job' ", " will be retried in 1s: attempt 2 of 3"]),
    );
    run.expect("unregister it", "/unregister?name=retry-job", 200, "ok");
    run.expect(
        "register a handler that always asks for a retry",
        "/register?name=exhaust-job&every-ms=4000",
        200,
        "ok",
    );
    run.check(
        "an exhausted firing is counted in the next firing's missed",
        run.host.wait_for_firing("exhaust-job", 12, |fired| {
            fired.attempt == 1 && fired.missed == 1
        }),
    );
    run.check(
        "retry exhaustion is logged",
        run.host
            .logged(&["'exhaust-job' ", " gave up after 3 attempts"]),
    );
    run.expect("unregister it", "/unregister?name=exhaust-job", 200, "ok");

    run.section("failure");
    run.expect(
        "register a handler that fails",
        "/register?name=fail-job&every-ms=1000",
        200,
        "ok",
    );
    run.check(
        "the failure is logged with the handler's detail",
        run.host
            .wait_for(&["'fail-job' ", " failed: this schedule always fails"], 4),
    );
    pause(1_200);
    let failures = run.host.fired("fail-job");
    run.check(
        "a failed firing is not redelivered",
        failures.iter().all(|fired| fired.attempt == 1),
    );
    run.check(
        "the schedule carries on after a failure",
        failures.len() >= 2,
    );
    run.expect("unregister it", "/unregister?name=fail-job", 200, "ok");

    run.section("overlap");
    let beats = run.host.fired("heartbeat").len();
    run.expect(
        "register a handler slower than its period",
        "/register?name=slow-job&every-ms=1000",
        200,
        "ok",
    );
    // Fires at 1s and runs until 3.5s, so the firings at 2s and 3s are skipped
    // and the one at 4s reports them.
    run.check(
        "skipped firings are counted in the next one",
        run.host
            .wait_for_firing("slow-job", 8, |fired| fired.missed == 2),
    );
    // A delivery starts with `fired` and ends with `finished`. If one began
    // before the last had ended, two of a kind would be next to each other.
    let started: Vec<bool> = run
        .host
        .log()
        .lines()
        .filter_map(|line| {
            if line.contains("ticker: fired name=slow-job ") {
                Some(true)
            } else if line.contains("ticker: finished name=slow-job") {
                Some(false)
            } else {
                None
            }
        })
        .collect();
    run.check(
        "no two deliveries of one schedule overlap",
        started.windows(2).all(|pair| pair[0] != pair[1]),
    );
    run.expect("unregister it", "/unregister?name=slow-job", 200, "ok");
    run.check(
        "other schedules kept firing while it ran",
        run.host.fired("heartbeat").len() > beats,
    );

    run.section("a handler that traps");
    run.expect(
        "register a handler that panics",
        "/register?name=trap-job&every-ms=1000",
        200,
        "ok",
    );
    run.check(
        "the trap reaches the plugin as an error, not a crash",
        run.host.wait_for(&["'trap-job' ", " failed"], 5),
    );
    run.expect("unregister it", "/unregister?name=trap-job", 200, "ok");
    run.check(
        "the plugin is still serving",
        matches!(run.ticker.get("/schedules"), Some((200, _))),
    );
    run.check(
        "the plugin was not restarted",
        run.host.lines(&["dispatch loop started"]).len() == 1,
    );

    run.section("one-shot schedules");
    run.expect(
        "register a delay",
        "/register?name=once-job&once-after-ms=1000",
        200,
        "ok",
    );
    run.check(
        "a pending one-shot is listed",
        run.ticker
            .held("once-job")
            .is_some_and(|held| held.is("registered", "once-after 1000ms") && held.next.is_some()),
    );
    run.expect(
        "registering it again is accepted",
        "/register?name=once-job&once-after-ms=1000",
        200,
        "ok",
    );
    run.check(
        "it fires",
        run.host.wait_for_firing("once-job", 4, |_| true),
    );
    pause(2_200);
    run.check(
        "it fires exactly once",
        run.host.fired("once-job").len() == 1,
    );
    run.check(
        "it is removed once it has fired",
        run.ticker.held("once-job").is_none(),
    );
    let at = unix_now() + 3;
    run.expect(
        "register an instant",
        &format!("/register?name=at-job&once-at={at}"),
        200,
        "ok",
    );
    run.check(
        "it is listed with that instant as its next firing",
        run.ticker.held("at-job").is_some_and(|held| {
            held.is("registered", &format!("once-at {at}")) && held.next == Some(at)
        }),
    );
    run.check(
        "it fires with that instant as scheduled-at",
        run.host
            .wait_for_firing("at-job", 6, |fired| fired.scheduled_at == at),
    );
    run.expect(
        "an instant in the past",
        &format!("/register?name=late&once-at={}", at - 3_600),
        400,
        "invalid-schedule",
    );
    run.expect(
        "a delay under min-interval",
        "/register?name=soon&once-after-ms=100",
        400,
        "invalid-schedule",
    );
    run.expect(
        "a one-shot whose handler asks for a retry",
        "/register?name=retry-once&once-after-ms=1000",
        200,
        "ok",
    );
    run.check(
        "it is redelivered up to the third attempt",
        run.host
            .wait_for_firing("retry-once", 6, |fired| fired.attempt == 3),
    );
    pause(500);
    run.check(
        "it was delivered three times in all",
        run.host.fired("retry-once").len() == 3,
    );
    run.check(
        "it is removed after the last attempt",
        run.ticker.held("retry-once").is_none(),
    );
    // The handler registers its own name again, with the same delay, each time.
    run.expect(
        "a one-shot whose handler registers itself again",
        "/register?name=again-job&once-after-ms=1000",
        200,
        "ok",
    );
    run.check(
        "it keeps being called",
        run.host.wait_for_firing("again-job", 3, |_| true),
    );
    pause(2_600);
    let again = run.host.fired("again-job").len();
    run.check_or(
        format!("each firing schedules the next ({again} firings)"),
        again >= 3,
        || "expected at least 3".to_string(),
    );
    run.expect("unregister it", "/unregister?name=again-job", 200, "ok");

    run.section("crontab (waiting for the minute to turn)");
    let turned = run.host.wait_for_firing("minutely", 65, |_| true);
    run.check_or("a declared crontab fires", turned, || {
        "no firing within 65s".to_string()
    });
    if let Some(first) = run.host.fired("minutely").first().cloned() {
        run.check(
            format!(
                "scheduled-at is the minute boundary ({})",
                first.scheduled_at
            ),
            first.scheduled_at % 60 == 0,
        );
        run.check(
            format!(
                "the firing the listing forecast arrived, with that scheduled-at ({})",
                shown(next_minute)
            ),
            next_minute.is_some_and(|at| {
                run.host
                    .wait_for_firing("minutely", 62, |fired| fired.scheduled_at == at)
            }),
        );
    }

    run.section("interval rate");
    // Counted between the first and last heartbeat, not from when the host was
    // launched: how long it took to start says nothing about the schedule.
    let beats = run.host.fired("heartbeat");
    let span = match (beats.first(), beats.last()) {
        (Some(first), Some(last)) => last.scheduled_at - first.scheduled_at,
        _ => 0,
    };
    run.check(
        format!("the 5s heartbeat fired {} times over {span}s", beats.len()),
        beats.len() >= 3 && i64::try_from(beats.len()).is_ok_and(|count| count == span / 5 + 1),
    );
    let mut gaps: Vec<i64> = beats
        .windows(2)
        .map(|pair| pair[1].scheduled_at - pair[0].scheduled_at)
        .collect();
    gaps.sort_unstable();
    gaps.dedup();
    run.check(
        format!("consecutive heartbeats are 5s apart (gaps: {gaps:?})"),
        gaps == [5],
    );
    run.check(
        "distinct heartbeats have distinct firing IDs",
        beats
            .iter()
            .map(|fired| &fired.id)
            .collect::<BTreeSet<_>>()
            .len()
            == beats.len(),
    );
    run.check(
        "no heartbeat was skipped",
        beats.iter().all(|fired| fired.missed == 0),
    );

    run.section("shutdown");
    run.host.stop();
    run.check(
        "the workload's schedules are dropped when it stops",
        run.host.logged(&["cron-scheduler: unbound workload"]),
    );

    let Run {
        host,
        passed,
        failed,
        ..
    } = run;
    assert!(
        failed.is_empty(),
        "{} of {} checks failed (host log kept at {}):\n  {}",
        failed.len(),
        passed + failed.len(),
        host.log.display(),
        failed.join("\n  ")
    );
    println!("PASS ({passed} checks)");
    let _ = fs::remove_file(&host.log);
}
