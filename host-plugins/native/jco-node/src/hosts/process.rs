//! `jco:node/process`: a process of the guest's own.
//!
//! Jco's JavaScript provider hands the guest the embedding Node process itself, so a guest's
//! `process.env.X = 1`, `process.chdir` or `process.exit` act on the host. A host plugin serves
//! many workloads from one process, so here those act on a per-instance virtual process instead:
//! the environment starts as the workload's, the working directory is tracked per instance, and
//! `exit` ends the instance. What is read-only and harmless (pid, ids, resource usage) is real.

use std::path::Path;

use serde_json::json;
use sysinfo::{MemoryRefreshKind, RefreshKind, System};
use wash_runtime::wasmtime;

use crate::{
    bindings::jco::node::{
        console::OutputDestination,
        process::{
            ClockTime, CpuUsageRecord, Errno, Error, ExitCode, Flag, Host, Id, Identity, Mask, MemoryUsageRecord,
            Path as ProcessPath, ProcessMetadata, ReportOption, ReportOptions, ReportValue, ResourceUsageRecord,
            Signal, State, Warning,
        },
    },
    errno::{self, UvError},
    hosts::{denied_message, os},
    plugin::NodeView,
    state::{self, Capability},
};

type ProcessResult<T> = wasmtime::Result<Result<T, Error>>;

/// Flags Node accepts in `NODE_OPTIONS`, as `process.allowedNodeEnvironmentFlags` lists them.
const ALLOWED_FLAGS: &str = include_str!("../data/allowed-node-flags.txt");

fn plain(name: &str, message: String, code: &str) -> Error {
    Error { name: name.into(), message, code: Some(code.into()), errno: None, syscall: None, path: None }
}

fn denied() -> Error {
    plain("Error", denied_message(Capability::Process), &Capability::Process.denied_code())
}

fn unsupported(api: &str, reason: &str) -> Error {
    plain(
        "Error",
        format!("{api} is not supported in a WebAssembly component: {reason}"),
        "ERR_JCO_UNSUPPORTED_NODE_API",
    )
}

fn uv(error: UvError, syscall: &str, message: String) -> Error {
    Error {
        name: "Error".into(),
        message,
        code: Some(error.code.into()),
        errno: Some(Errno::Number(error.node_errno())),
        syscall: Some(syscall.into()),
        path: None,
    }
}

fn syscall_error(error: UvError, syscall: &str) -> Error {
    uv(error, syscall, errno::syscall_message(error, syscall, None, None))
}

fn rusage(who: libc::c_int) -> libc::rusage {
    // SAFETY: `rusage` is plain data, valid when zeroed.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `usage` is a valid, writable `rusage`; a bad `who` only leaves it zeroed.
    unsafe { libc::getrusage(who, &mut usage) };
    usage
}

fn micros(time: libc::timeval) -> f64 {
    time.tv_sec as f64 * 1e6 + time.tv_usec as f64
}

fn cpu_usage(who: libc::c_int, previous: Option<CpuUsageRecord>) -> CpuUsageRecord {
    let usage = rusage(who);
    let mut record = CpuUsageRecord { user: micros(usage.ru_utime), system: micros(usage.ru_stime) };
    if let Some(previous) = previous {
        record.user -= previous.user;
        record.system -= previous.system;
    }
    record
}

fn resident_set_size() -> f64 {
    let pid = sysinfo::Pid::from_u32(std::process::id());
    let mut system = System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).map_or(0.0, |process| process.memory() as f64)
}

/// The cgroup memory limit, or 0 when there is none, as `process.constrainedMemory()` reports.
fn constrained_memory() -> f64 {
    ["/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory/memory.limit_in_bytes"]
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .filter_map(|text| text.trim().parse::<f64>().ok())
        // An unlimited cgroup v1 reports a page-rounded i64::MAX.
        .find(|limit| *limit < 9.0e18)
        .unwrap_or(0.0)
}

/// Signal names and numbers, shared with `os.constants.signals`.
pub(crate) fn signal_number(signal: &Signal) -> Result<i32, Error> {
    match signal {
        Signal::Number(number) => Ok(*number),
        Signal::Name(name) => os::signals()
            .into_iter()
            .find(|entry| entry.name == *name)
            .map(|entry| entry.value as i32)
            .ok_or_else(|| plain("TypeError", format!("Unknown signal: {name}"), "ERR_UNKNOWN_SIGNAL")),
    }
}

fn exit_status(code: &Option<state::ExitCode>) -> i32 {
    match code {
        None => 0,
        Some(state::ExitCode::Number(number)) => *number as i32,
        Some(state::ExitCode::Text(text)) => text.trim().parse::<f64>().map_or(0, |number| number as i32),
    }
}

fn from_wit_exit(code: ExitCode) -> state::ExitCode {
    match code {
        ExitCode::Number(number) => state::ExitCode::Number(number),
        ExitCode::Text(text) => state::ExitCode::Text(text),
    }
}

/// Parse a `.env` file the way `process.loadEnvFile` does: `KEY=value` lines, `#` comments,
/// an optional `export ` prefix, and single, double or backtick quotes around a value.
pub(crate) fn parse_env_file(text: &str) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").map_or(line, str::trim_start);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = value.trim();
        let quoted = ['"', '\'', '`'].into_iter().find_map(|quote| {
            let rest = value.strip_prefix(quote)?;
            let end = rest.find(quote)?;
            let inner = &rest[..end];
            Some(if quote == '"' { inner.replace("\\n", "\n") } else { inner.to_owned() })
        });
        let value = quoted.unwrap_or_else(|| value.split(" #").next().unwrap_or("").trim_end().to_owned());
        entries.push((key.to_owned(), value));
    }
    entries
}

impl NodeView<'_> {
    fn process_granted(&self) -> Result<(), Error> {
        if self.node.allows(Capability::Process) { Ok(()) } else { Err(denied()) }
    }

    fn report(&self, trigger: &str, error: Option<&Warning>) -> serde_json::Value {
        let process = self.node.process();
        let identity = &self.node.identity;
        let environment: serde_json::Map<String, serde_json::Value> = if process.report.exclude_env {
            Default::default()
        } else {
            process.environment.iter().map(|(k, v)| (k.clone(), json!(v))).collect()
        };
        json!({
            "header": {
                "reportVersion": 5,
                "event": error.map_or("JavaScript API", |error| error.message.as_str()),
                "trigger": trigger,
                "filename": serde_json::Value::Null,
                "processId": std::process::id(),
                "threadId": 0,
                "cwd": process.cwd.to_string_lossy(),
                "commandLine": identity.argv,
                "nodejsVersion": identity.node_version,
                "wordSize": usize::BITS,
                "arch": os::arch(),
                "platform": os::platform(),
                "componentVersions": { "node": identity.node_version.trim_start_matches('v') },
                "release": { "name": "node" },
                "host": "",
            },
            "javascriptStack": {
                "message": error.map_or("No stack.", |error| error.message.as_str()),
                "stack": error.and_then(|error| error.stack.clone()).map_or_else(
                    || vec!["Unavailable.".to_owned()],
                    |stack| stack.lines().skip(1).map(|line| line.trim().to_owned()).collect(),
                ),
            },
            "javascriptHeap": {},
            "nativeStack": [],
            "resourceUsage": {
                "userCpuSeconds": micros(rusage(libc::RUSAGE_SELF).ru_utime) / 1e6,
                "kernelCpuSeconds": micros(rusage(libc::RUSAGE_SELF).ru_stime) / 1e6,
                "maxRss": rusage(libc::RUSAGE_SELF).ru_maxrss,
            },
            "libuv": [],
            "workers": [],
            "environmentVariables": environment,
            "userLimits": {},
            "sharedObjects": [],
        })
    }
}

macro_rules! granted {
    ($view:expr) => {
        if let Err(error) = $view.process_granted() {
            return Ok(Err(error));
        }
    };
}

impl Host for NodeView<'_> {
    async fn metadata(&mut self) -> ProcessResult<ProcessMetadata> {
        granted!(self);
        let identity = &self.node.identity;
        let version = identity.node_version.trim_start_matches('v').to_owned();
        let features = json!({
            "inspector": false, "debug": false, "tls": true, "cached_builtins": true,
            "require_module": true, "typescript": false,
        });
        Ok(Ok(ProcessMetadata {
            arch: os::arch().into(),
            platform: os::platform().into(),
            pid: std::process::id(),
            ppid: std::os::unix::process::parent_id(),
            argv: identity.argv.clone(),
            argv0: identity.argv.first().cloned().unwrap_or_else(|| "node".into()),
            exec_argv: identity.exec_argv.clone(),
            exec_path: identity.exec_path.clone(),
            version: identity.node_version.clone(),
            versions: vec![("node".into(), version.clone()), ("wasmtime".into(), "48".into())],
            release_json: json!({
                "name": "node",
                "sourceUrl": format!("https://nodejs.org/download/release/v{version}/node-v{version}.tar.gz"),
                "headersUrl": format!("https://nodejs.org/download/release/v{version}/node-v{version}-headers.tar.gz"),
            })
            .to_string(),
            config_json: json!({ "target_defaults": {}, "variables": {} }).to_string(),
            features_json: features.to_string(),
            has_permission: false,
        }))
    }

    async fn get_state(&mut self) -> ProcessResult<State> {
        granted!(self);
        let process = self.node.process();
        let flag = |name: &str| process.flags.get(name).copied();
        Ok(Ok(State {
            title: process.title.clone(),
            debug_port: process.debug_port,
            exit_code: process.exit_code.clone().map(|code| match code {
                state::ExitCode::Number(number) => ExitCode::Number(number),
                state::ExitCode::Text(text) => ExitCode::Text(text),
            }),
            no_deprecation: flag("no-deprecation"),
            throw_deprecation: flag("throw-deprecation"),
            trace_deprecation: flag("trace-deprecation"),
            trace_process_warnings: flag("trace-process-warnings"),
            source_maps_enabled: process.source_maps_enabled,
            connected: None,
        }))
    }

    async fn set_title(&mut self, value: String) -> ProcessResult<()> {
        granted!(self);
        self.node.process().title = value;
        Ok(Ok(()))
    }

    async fn set_debug_port(&mut self, value: f64) -> ProcessResult<()> {
        granted!(self);
        self.node.process().debug_port = value;
        Ok(Ok(()))
    }

    async fn set_exit_code(&mut self, value: Option<ExitCode>) -> ProcessResult<()> {
        granted!(self);
        self.node.process().exit_code = value.map(from_wit_exit);
        Ok(Ok(()))
    }

    async fn set_flag(&mut self, name: Flag, value: bool) -> ProcessResult<()> {
        granted!(self);
        let name = match name {
            Flag::NoDeprecation => "no-deprecation",
            Flag::ThrowDeprecation => "throw-deprecation",
            Flag::TraceDeprecation => "trace-deprecation",
            Flag::TraceProcessWarnings => "trace-process-warnings",
        };
        self.node.process().flags.insert(name, value);
        Ok(Ok(()))
    }

    async fn env_entries(&mut self) -> ProcessResult<Vec<(String, String)>> {
        granted!(self);
        Ok(Ok(self.node.process().environment.iter().map(|(k, v)| (k.clone(), v.clone())).collect()))
    }

    async fn env_get(&mut self, name: String) -> ProcessResult<Option<String>> {
        granted!(self);
        Ok(Ok(self.node.process().environment.get(&name).cloned()))
    }

    async fn env_set(&mut self, name: String, value: Option<String>) -> ProcessResult<()> {
        granted!(self);
        let mut process = self.node.process();
        match value {
            Some(value) => process.environment.insert(name, value),
            None => process.environment.remove(&name),
        };
        Ok(Ok(()))
    }

    async fn cwd(&mut self) -> ProcessResult<String> {
        granted!(self);
        Ok(Ok(self.node.process().cwd.to_string_lossy().into_owned()))
    }

    async fn chdir(&mut self, directory: String) -> ProcessResult<()> {
        granted!(self);
        let target = self.node.resolve(&directory);
        let from = self.node.process().cwd.to_string_lossy().into_owned();
        let failure = |error: UvError| {
            let mut failed =
                uv(error, "chdir", errno::syscall_message(error, "chdir", Some(&from), Some(&directory)));
            failed.path = Some(from.clone());
            failed
        };
        let resolved = match std::fs::canonicalize(&target) {
            Ok(resolved) => resolved,
            Err(error) => return Ok(Err(failure(errno::from_io(&error)))),
        };
        if !resolved.is_dir() {
            return Ok(Err(failure(errno::from_errno(libc::ENOTDIR))));
        }
        self.node.process().cwd = resolved;
        Ok(Ok(()))
    }

    async fn cpu_usage(&mut self, previous: Option<CpuUsageRecord>) -> ProcessResult<CpuUsageRecord> {
        granted!(self);
        Ok(Ok(cpu_usage(libc::RUSAGE_SELF, previous)))
    }

    async fn thread_cpu_usage(&mut self, previous: Option<CpuUsageRecord>) -> ProcessResult<CpuUsageRecord> {
        granted!(self);
        #[cfg(target_os = "linux")]
        let who = libc::RUSAGE_THREAD;
        #[cfg(not(target_os = "linux"))]
        let who = libc::RUSAGE_SELF;
        Ok(Ok(cpu_usage(who, previous)))
    }

    async fn memory_usage(&mut self) -> ProcessResult<MemoryUsageRecord> {
        granted!(self);
        // There is no V8 heap to describe; the resident set is the one figure that is real.
        Ok(Ok(MemoryUsageRecord {
            rss: resident_set_size(),
            heap_total: 0.0,
            heap_used: 0.0,
            external: 0.0,
            array_buffers: 0.0,
        }))
    }

    async fn rss(&mut self) -> ProcessResult<f64> {
        granted!(self);
        Ok(Ok(resident_set_size()))
    }

    async fn resource_usage(&mut self) -> ProcessResult<ResourceUsageRecord> {
        granted!(self);
        let usage = rusage(libc::RUSAGE_SELF);
        Ok(Ok(ResourceUsageRecord {
            user_cpu_time: micros(usage.ru_utime),
            system_cpu_time: micros(usage.ru_stime),
            max_rss: usage.ru_maxrss as f64,
            shared_memory_size: usage.ru_ixrss as f64,
            unshared_data_size: usage.ru_idrss as f64,
            unshared_stack_size: usage.ru_isrss as f64,
            minor_page_fault: usage.ru_minflt as f64,
            major_page_fault: usage.ru_majflt as f64,
            swapped_out: usage.ru_nswap as f64,
            fs_read: usage.ru_inblock as f64,
            fs_write: usage.ru_oublock as f64,
            ipc_sent: usage.ru_msgsnd as f64,
            ipc_received: usage.ru_msgrcv as f64,
            signals_count: usage.ru_nsignals as f64,
            voluntary_context_switches: usage.ru_nvcsw as f64,
            involuntary_context_switches: usage.ru_nivcsw as f64,
        }))
    }

    async fn hrtime(&mut self) -> ProcessResult<ClockTime> {
        granted!(self);
        // SAFETY: `timespec` is plain data, valid when zeroed.
        let mut now: libc::timespec = unsafe { std::mem::zeroed() };
        // SAFETY: `now` is a valid, writable `timespec`.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
        Ok(Ok(ClockTime { seconds: now.tv_sec as f64, nanoseconds: now.tv_nsec as u32 }))
    }

    async fn uptime(&mut self) -> ProcessResult<f64> {
        granted!(self);
        Ok(Ok(self.node.started.elapsed().as_secs_f64()))
    }

    async fn available_memory(&mut self) -> ProcessResult<f64> {
        granted!(self);
        let system = System::new_with_specifics(RefreshKind::new().with_memory(MemoryRefreshKind::everything()));
        Ok(Ok(system.available_memory() as f64))
    }

    async fn constrained_memory(&mut self) -> ProcessResult<f64> {
        granted!(self);
        Ok(Ok(constrained_memory()))
    }

    async fn get_active_resources_info(&mut self) -> ProcessResult<Vec<String>> {
        granted!(self);
        Ok(Ok(Vec::new()))
    }

    async fn get_id(&mut self, kind: Identity) -> ProcessResult<u32> {
        granted!(self);
        // SAFETY: the id getters have no preconditions and cannot fail.
        Ok(Ok(unsafe {
            match kind {
                Identity::Uid => libc::getuid(),
                Identity::Euid => libc::geteuid(),
                Identity::Gid => libc::getgid(),
                Identity::Egid => libc::getegid(),
            }
        }))
    }

    async fn set_id(&mut self, kind: Identity, _value: Id) -> ProcessResult<()> {
        granted!(self);
        // The ids belong to the host process and every workload in it.
        let syscall = match kind {
            Identity::Uid => "setuid",
            Identity::Euid => "seteuid",
            Identity::Gid => "setgid",
            Identity::Egid => "setegid",
        };
        Ok(Err(syscall_error(errno::from_errno(libc::EPERM), syscall)))
    }

    async fn getgroups(&mut self) -> ProcessResult<Vec<u32>> {
        granted!(self);
        // SAFETY: a zero-length query only returns the count.
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        if count < 0 {
            return Ok(Err(syscall_error(errno::from_io(&std::io::Error::last_os_error()), "getgroups")));
        }
        let mut groups = vec![0 as libc::gid_t; count as usize];
        // SAFETY: `groups` has room for `count` entries.
        let written = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
        if written < 0 {
            return Ok(Err(syscall_error(errno::from_io(&std::io::Error::last_os_error()), "getgroups")));
        }
        groups.truncate(written as usize);
        // Node appends the effective gid when the kernel's list leaves it out.
        // SAFETY: `getegid` cannot fail.
        let effective = unsafe { libc::getegid() };
        if !groups.contains(&effective) {
            groups.push(effective);
        }
        Ok(Ok(groups))
    }

    async fn setgroups(&mut self, _groups: Vec<Id>) -> ProcessResult<()> {
        granted!(self);
        Ok(Err(syscall_error(errno::from_errno(libc::EPERM), "setgroups")))
    }

    async fn initgroups(&mut self, _user: Id, _extra_group: Id) -> ProcessResult<()> {
        granted!(self);
        Ok(Err(syscall_error(errno::from_errno(libc::EPERM), "initgroups")))
    }

    async fn kill(&mut self, pid: i32, signal: Signal) -> ProcessResult<bool> {
        granted!(self);
        let number = match signal_number(&signal) {
            Ok(number) => number,
            Err(error) => return Ok(Err(error)),
        };
        let own = pid == std::process::id() as i32 || pid == 0;
        if number == 0 {
            // Signal 0 only asks whether the process exists and may be signalled.
            // SAFETY: `kill` takes plain integers; signal 0 delivers nothing.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return Ok(Err(syscall_error(errno::from_io(&std::io::Error::last_os_error()), "kill")));
            }
            return Ok(Ok(true));
        }
        if !own {
            // Other processes are not the guest's to signal.
            return Ok(Err(syscall_error(errno::from_errno(libc::EPERM), "kill")));
        }
        // A signal to itself ends the guest the way an unhandled signal ends a process, with
        // the shell's 128+n status, and leaves the host standing.
        Err(wasmtime_wasi::I32Exit(128 + number).into())
    }

    async fn umask(&mut self, mask: Mask) -> ProcessResult<u32> {
        granted!(self);
        let new = match mask {
            Mask::Number(number) => number,
            Mask::Text(text) => match u32::from_str_radix(&text, 8) {
                Ok(number) => number,
                Err(_) => {
                    return Ok(Err(plain(
                        "TypeError",
                        format!("The argument 'mask' must be a 32-bit unsigned integer or an octal string. Received '{text}'"),
                        "ERR_INVALID_ARG_VALUE",
                    )));
                }
            },
        };
        let mut process = self.node.process();
        let old = process.umask;
        process.umask = new & 0o777;
        Ok(Ok(old))
    }

    async fn exit(&mut self, code: Option<ExitCode>) -> ProcessResult<()> {
        granted!(self);
        let status = match code {
            Some(code) => exit_status(&Some(from_wit_exit(code))),
            None => exit_status(&self.node.process().exit_code),
        };
        Err(wasmtime_wasi::I32Exit(status).into())
    }

    async fn abort(&mut self) -> ProcessResult<()> {
        granted!(self);
        // SIGABRT's conventional status; the host itself must not abort.
        Err(wasmtime_wasi::I32Exit(128 + libc::SIGABRT).into())
    }

    async fn execve(
        &mut self,
        _file: String,
        _args: Vec<String>,
        _env: Option<Vec<(String, String)>>,
    ) -> ProcessResult<()> {
        granted!(self);
        Ok(Err(unsupported("process.execve", "replacing the host process would end every workload on it")))
    }

    async fn load_env_file(&mut self, path: Option<ProcessPath>) -> ProcessResult<()> {
        granted!(self);
        if !self.node.allows(Capability::Fs) {
            return Ok(Err(unsupported("process.loadEnvFile", "reading the file needs jco:node/fs to be granted")));
        }
        let given = match path {
            None => ".env".to_owned(),
            Some(ProcessPath::Text(text)) => text,
            Some(ProcessPath::Url(url)) => url.strip_prefix("file://").unwrap_or(&url).to_owned(),
            Some(ProcessPath::Bytes(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
        };
        let text = match std::fs::read_to_string(self.node.resolve(&given)) {
            Ok(text) => text,
            Err(error) => {
                let error = errno::from_io(&error);
                let mut failed = uv(error, "open", errno::syscall_message(error, "open", Some(&given), None));
                failed.path = Some(given);
                return Ok(Err(failed));
            }
        };
        let mut process = self.node.process();
        for (key, value) in parse_env_file(&text) {
            // Variables already set win over the file, as in Node.
            process.environment.entry(key).or_insert(value);
        }
        Ok(Ok(()))
    }

    async fn set_source_maps_enabled(&mut self, value: bool) -> ProcessResult<()> {
        granted!(self);
        self.node.process().source_maps_enabled = value;
        Ok(Ok(()))
    }

    async fn emit_warning(&mut self, warning: Warning) -> ProcessResult<()> {
        granted!(self);
        let (silenced, trace) = {
            let process = self.node.process();
            let flag = |name: &str| process.flags.get(name).copied().unwrap_or(false);
            (
                warning.name == "DeprecationWarning" && flag("no-deprecation"),
                flag("trace-process-warnings") || (warning.name == "DeprecationWarning" && flag("trace-deprecation")),
            )
        };
        if silenced {
            return Ok(Ok(()));
        }
        let pid = std::process::id();
        let mut text = match (&warning.stack, trace) {
            (Some(stack), true) => format!("(node:{pid}) {stack}\n"),
            _ => {
                let code = warning.code.as_ref().map(|code| format!("[{code}] ")).unwrap_or_default();
                format!("(node:{pid}) {code}{}: {}\n", warning.name, warning.message)
            }
        };
        if let Some(detail) = &warning.detail {
            text.push_str(detail);
            text.push('\n');
        }
        self.write_stdio(OutputDestination::Stderr, text.into_bytes()).await?;
        Ok(Ok(()))
    }

    async fn permission_has(&mut self, _scope: String, _reference: Option<ProcessPath>) -> ProcessResult<bool> {
        granted!(self);
        Ok(Err(unsupported("process.permission.has", "the host was started without --permission")))
    }

    async fn get_report(&mut self, error: Option<Warning>) -> ProcessResult<String> {
        granted!(self);
        Ok(Ok(self.report("GetReport", error.as_ref()).to_string()))
    }

    async fn write_report(&mut self, filename: Option<String>, error: Option<Warning>) -> ProcessResult<String> {
        granted!(self);
        if !self.node.allows(Capability::Fs) {
            return Ok(Err(unsupported(
                "process.report.writeReport",
                "writing the file needs jco:node/fs to be granted",
            )));
        }
        let (directory, configured) = {
            let process = self.node.process();
            (process.report.directory.clone(), process.report.filename.clone())
        };
        let name = filename.filter(|name| !name.is_empty()).or((!configured.is_empty()).then_some(configured));
        let name = name.unwrap_or_else(|| {
            let seconds = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs());
            format!("report.{seconds}.{}.0.001.json", std::process::id())
        });
        let path = if Path::new(&name).is_absolute() || directory.is_empty() {
            self.node.resolve(&name)
        } else {
            self.node.resolve(&directory).join(&name)
        };
        let mut report = self.report("API", error.as_ref());
        report["header"]["filename"] = json!(name);
        let body = serde_json::to_string_pretty(&report).unwrap_or_default();
        if let Err(error) = std::fs::write(&path, body) {
            let error = errno::from_io(&error);
            let shown = path.to_string_lossy().into_owned();
            let mut failed = uv(error, "open", errno::syscall_message(error, "open", Some(&shown), None));
            failed.path = Some(shown);
            return Ok(Err(failed));
        }
        Ok(Ok(name))
    }

    async fn get_report_options(&mut self) -> ProcessResult<ReportOptions> {
        granted!(self);
        let report = self.node.process().report.clone();
        Ok(Ok(ReportOptions {
            compact: report.compact,
            directory: report.directory,
            filename: report.filename,
            signal: report.signal,
            report_on_fatal_error: report.report_on_fatal_error,
            report_on_signal: report.report_on_signal,
            report_on_uncaught_exception: report.report_on_uncaught_exception,
            exclude_env: report.exclude_env,
            exclude_network: report.exclude_network,
        }))
    }

    async fn set_report_option(&mut self, name: ReportOption, value: ReportValue) -> ProcessResult<()> {
        granted!(self);
        let mut process = self.node.process();
        let report = &mut process.report;
        match (name, value) {
            (ReportOption::Compact, ReportValue::Boolean(value)) => report.compact = value,
            (ReportOption::Directory, ReportValue::Text(value)) => report.directory = value,
            (ReportOption::Filename, ReportValue::Text(value)) => report.filename = value,
            (ReportOption::Signal, ReportValue::Text(value)) => report.signal = value,
            (ReportOption::ReportOnFatalError, ReportValue::Boolean(value)) => report.report_on_fatal_error = value,
            (ReportOption::ReportOnSignal, ReportValue::Boolean(value)) => report.report_on_signal = value,
            (ReportOption::ReportOnUncaughtException, ReportValue::Boolean(value)) => {
                report.report_on_uncaught_exception = value;
            }
            (ReportOption::ExcludeEnv, ReportValue::Boolean(value)) => report.exclude_env = value,
            (ReportOption::ExcludeNetwork, ReportValue::Boolean(value)) => report.exclude_network = value,
            (name, _) => {
                return Ok(Err(plain(
                    "TypeError",
                    format!("The report option {name:?} was given a value of the wrong type"),
                    "ERR_INVALID_ARG_TYPE",
                )));
            }
        }
        Ok(Ok(()))
    }

    async fn allowed_flags(&mut self) -> ProcessResult<Vec<String>> {
        granted!(self);
        Ok(Ok(ALLOWED_FLAGS.lines().map(str::to_owned).collect()))
    }

    async fn allowed_flag(&mut self, value: String) -> ProcessResult<bool> {
        granted!(self);
        // Node matches a flag given with a value (`--max-http-header-size=1`) by its name.
        let name = value.split('=').next().unwrap_or(&value);
        Ok(Ok(ALLOWED_FLAGS.lines().any(|flag| flag == name)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_files_parse_like_node() {
        let entries = parse_env_file(
            "# comment\nPLAIN=value\nexport EXPORTED = spaced \nQUOTED=\"two\\nlines\"\nSINGLE='a # b'\nTAIL=x # trailing\nEMPTY=\n",
        );
        assert_eq!(
            entries,
            [
                ("PLAIN", "value"),
                ("EXPORTED", "spaced"),
                ("QUOTED", "two\nlines"),
                ("SINGLE", "a # b"),
                ("TAIL", "x"),
                ("EMPTY", ""),
            ]
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
        );
    }

    #[test]
    fn signals_resolve_by_name_and_reject_unknown_ones() {
        assert_eq!(signal_number(&Signal::Name("SIGTERM".into())).unwrap(), libc::SIGTERM);
        let error = signal_number(&Signal::Name("JCO_BAD_SIGNAL".into())).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("ERR_UNKNOWN_SIGNAL"));
    }

    #[test]
    fn exit_codes_may_be_numeric_strings() {
        assert_eq!(exit_status(&Some(state::ExitCode::Text("7".into()))), 7);
        assert_eq!(exit_status(&Some(state::ExitCode::Number(23.0))), 23);
        assert_eq!(exit_status(&None), 0);
    }
}
