//! What the plugin remembers for one component instance.
//!
//! `jco:node` is a stateful contract: a guest sets an environment variable and reads it back,
//! opens a file and reads from the descriptor, starts a server and is called when a request
//! arrives. None of that may leak between instances, and none of it may change the host process
//! (a guest's `process.chdir` or `process.env.X = ...` is its own business), so every instance
//! gets its own [`NodeInstance`] holding a virtual process and the tables behind its handles.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::File,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};

use tokio::sync::mpsc;

use crate::callbacks::Job;

/// One interface of `jco:node@0.1.0`, as a thing a workload can be granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Capability {
    ChildProcess,
    Cluster,
    Console,
    CryptoKdf,
    Dgram,
    Dns,
    Ffi,
    Fs,
    Http,
    Http2,
    Inspector,
    Os,
    Process,
    Sqlite,
    Timers,
    Tls,
    TraceEvents,
    Tty,
    V8,
    Wasi,
    WorkerThreads,
    Zlib,
}

impl Capability {
    pub const ALL: [Capability; 22] = [
        Capability::ChildProcess,
        Capability::Cluster,
        Capability::Console,
        Capability::CryptoKdf,
        Capability::Dgram,
        Capability::Dns,
        Capability::Ffi,
        Capability::Fs,
        Capability::Http,
        Capability::Http2,
        Capability::Inspector,
        Capability::Os,
        Capability::Process,
        Capability::Sqlite,
        Capability::Timers,
        Capability::Tls,
        Capability::TraceEvents,
        Capability::Tty,
        Capability::V8,
        Capability::Wasi,
        Capability::WorkerThreads,
        Capability::Zlib,
    ];

    /// The interface's name in WIT, e.g. `child-process`.
    pub fn interface(self) -> &'static str {
        match self {
            Capability::ChildProcess => "child-process",
            Capability::Cluster => "cluster",
            Capability::Console => "console",
            Capability::CryptoKdf => "crypto-kdf",
            Capability::Dgram => "dgram",
            Capability::Dns => "dns",
            Capability::Ffi => "ffi",
            Capability::Fs => "fs",
            Capability::Http => "http",
            Capability::Http2 => "http2",
            Capability::Inspector => "inspector",
            Capability::Os => "os",
            Capability::Process => "process",
            Capability::Sqlite => "sqlite",
            Capability::Timers => "timers",
            Capability::Tls => "tls",
            Capability::TraceEvents => "trace-events",
            Capability::Tty => "tty",
            Capability::V8 => "v8",
            Capability::Wasi => "wasi",
            Capability::WorkerThreads => "worker-threads",
            Capability::Zlib => "zlib",
        }
    }

    /// The `node:` module the interface backs, as Jco's refusal messages name it.
    pub fn module(self) -> &'static str {
        match self {
            Capability::ChildProcess => "node:child_process",
            Capability::Cluster => "node:cluster",
            Capability::Console => "node:console",
            Capability::CryptoKdf => "node:crypto",
            Capability::Dgram => "node:dgram",
            Capability::Dns => "node:dns",
            Capability::Ffi => "node:ffi",
            Capability::Fs => "node:fs",
            Capability::Http => "node:http",
            Capability::Http2 => "node:http2",
            Capability::Inspector => "node:inspector",
            Capability::Os => "node:os",
            Capability::Process => "node:process",
            Capability::Sqlite => "node:sqlite",
            Capability::Timers => "node:timers",
            Capability::Tls => "node:tls",
            Capability::TraceEvents => "node:trace_events",
            Capability::Tty => "node:tty",
            Capability::V8 => "node:v8",
            Capability::Wasi => "node:wasi",
            Capability::WorkerThreads => "node:worker_threads",
            Capability::Zlib => "node:zlib",
        }
    }

    /// The code Jco's guest shims raise when the interface has no provider:
    /// `ERR_JCO_<MODULE>_ADAPTER_REQUIRED`.
    pub fn denied_code(self) -> String {
        let module = self.module().trim_start_matches("node:").to_uppercase();
        format!("ERR_JCO_{module}_ADAPTER_REQUIRED")
    }

    pub fn from_interface(name: &str) -> Option<Capability> {
        Capability::ALL.into_iter().find(|capability| capability.interface() == name)
    }
}

/// The interfaces one workload component may use. Everything else is linked but refuses, the way
/// Jco's default providers do, so a guest can catch the refusal instead of failing to start.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grants(HashSet<Capability>);

impl Grants {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn all() -> Self {
        Self(Capability::ALL.into_iter().collect())
    }

    pub fn grant(&mut self, capability: Capability) {
        self.0.insert(capability);
    }

    pub fn allows(&self, capability: Capability) -> bool {
        self.0.contains(&capability)
    }

    pub fn iter(&self) -> impl Iterator<Item = Capability> + '_ {
        let mut sorted: Vec<_> = self.0.iter().copied().collect();
        sorted.sort();
        sorted.into_iter()
    }
}

impl FromIterator<Capability> for Grants {
    fn from_iter<I: IntoIterator<Item = Capability>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// How the guest's `process` describes itself. Nothing here is read from the host unless the
/// embedder put it there.
#[derive(Debug, Clone)]
pub struct ProcessIdentity {
    /// `process.argv`; Node always has at least the executable and the script.
    pub argv: Vec<String>,
    /// `process.execPath`.
    pub exec_path: String,
    /// `process.execArgv`.
    pub exec_argv: Vec<String>,
    /// Where the guest starts; `process.chdir` moves it without moving the host.
    pub cwd: PathBuf,
    /// The Node release the guest is told it runs on, with the leading `v`.
    pub node_version: String,
}

impl Default for ProcessIdentity {
    fn default() -> Self {
        Self {
            argv: vec!["node".into()],
            exec_path: "/usr/local/bin/node".into(),
            exec_argv: Vec::new(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            node_version: crate::NODE_VERSION.into(),
        }
    }
}

/// What an instance is created from: fixed when its component is bound to the plugin.
#[derive(Debug, Clone, Default)]
pub struct InstanceTemplate {
    pub grants: Grants,
    pub identity: ProcessIdentity,
    /// The workload's environment; the guest's `process.env` starts as a copy.
    pub environment: BTreeMap<String, String>,
}

/// An open file behind a guest-visible descriptor.
#[derive(Debug)]
pub struct OpenFile {
    pub file: File,
    /// The path it was opened with, for error messages.
    pub path: String,
    /// Opened with `O_APPEND`, where positioned writes still append.
    pub append: bool,
}

/// The guest's process, as far as it can tell.
#[derive(Debug)]
pub struct VirtualProcess {
    pub environment: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub title: String,
    pub debug_port: f64,
    pub exit_code: Option<ExitCode>,
    pub flags: HashMap<&'static str, bool>,
    pub source_maps_enabled: bool,
    pub umask: u32,
    pub report: ReportOptions,
}

/// `process.exitCode`, which Node lets be a number or a numeric string.
#[derive(Debug, Clone, PartialEq)]
pub enum ExitCode {
    Number(f64),
    Text(String),
}

#[derive(Debug, Clone)]
pub struct ReportOptions {
    pub compact: bool,
    pub directory: String,
    pub filename: String,
    pub signal: String,
    pub report_on_fatal_error: bool,
    pub report_on_signal: bool,
    pub report_on_uncaught_exception: bool,
    pub exclude_env: bool,
    pub exclude_network: bool,
}

impl Default for ReportOptions {
    fn default() -> Self {
        Self {
            compact: false,
            directory: String::new(),
            filename: String::new(),
            signal: "SIGUSR2".into(),
            report_on_fatal_error: false,
            report_on_signal: false,
            report_on_uncaught_exception: false,
            exclude_env: false,
            exclude_network: false,
        }
    }
}

/// Descriptors below this are the standard streams, which the guest reaches through WASI.
const FIRST_DESCRIPTOR: u32 = 3;

#[derive(Debug)]
pub struct Descriptors {
    open: HashMap<u32, OpenFile>,
}

impl Default for Descriptors {
    fn default() -> Self {
        Self { open: HashMap::new() }
    }
}

impl Descriptors {
    /// Hand out the lowest free descriptor, as POSIX does.
    pub fn insert(&mut self, file: OpenFile) -> u32 {
        let mut descriptor = FIRST_DESCRIPTOR;
        while self.open.contains_key(&descriptor) {
            descriptor += 1;
        }
        self.open.insert(descriptor, file);
        descriptor
    }

    pub fn get(&self, descriptor: u32) -> Option<&OpenFile> {
        self.open.get(&descriptor)
    }

    pub fn get_mut(&mut self, descriptor: u32) -> Option<&mut OpenFile> {
        self.open.get_mut(&descriptor)
    }

    pub fn remove(&mut self, descriptor: u32) -> Option<OpenFile> {
        self.open.remove(&descriptor)
    }
}

/// Everything behind one component instance's `jco:node` imports.
pub struct NodeInstance {
    pub grants: Grants,
    pub identity: ProcessIdentity,
    pub started: Instant,
    pub process: Mutex<VirtualProcess>,
    pub descriptors: Mutex<Descriptors>,
    /// Work for the guest's callback exports, delivered by [`crate::callbacks::Pump`].
    pub jobs: mpsc::UnboundedSender<Job>,
    pub(crate) job_queue: Mutex<Option<mpsc::UnboundedReceiver<Job>>>,
    /// Per-interface state that only its own module understands.
    pub(crate) timers: Mutex<crate::hosts::timers::Timers>,
    pub(crate) terminals: Mutex<crate::hosts::tty::Terminals>,
    pub(crate) tracing: Mutex<crate::hosts::trace_events::Tracing>,
}

impl NodeInstance {
    pub fn new(template: &InstanceTemplate) -> Arc<Self> {
        let (jobs, queue) = mpsc::unbounded_channel();
        Arc::new(Self {
            grants: template.grants.clone(),
            identity: template.identity.clone(),
            started: Instant::now(),
            process: Mutex::new(VirtualProcess {
                environment: template.environment.clone(),
                cwd: template.identity.cwd.clone(),
                title: "node".into(),
                debug_port: 9229.0,
                exit_code: None,
                flags: HashMap::new(),
                source_maps_enabled: false,
                umask: 0o022,
                report: ReportOptions::default(),
            }),
            descriptors: Mutex::default(),
            jobs,
            job_queue: Mutex::new(Some(queue)),
            timers: Mutex::default(),
            terminals: Mutex::default(),
            tracing: Mutex::default(),
        })
    }

    pub fn allows(&self, capability: Capability) -> bool {
        self.grants.allows(capability)
    }

    pub fn process(&self) -> MutexGuard<'_, VirtualProcess> {
        lock(&self.process)
    }

    pub fn descriptors(&self) -> MutexGuard<'_, Descriptors> {
        lock(&self.descriptors)
    }

    /// Resolve a guest path against the guest's working directory.
    pub fn resolve(&self, path: &str) -> PathBuf {
        let path = std::path::Path::new(path);
        if path.is_absolute() { path.to_path_buf() } else { self.process().cwd.join(path) }
    }
}

/// Lock a mutex whose holder cannot leave it inconsistent: every critical section here is a
/// plain field update, so a poisoned lock still guards valid data.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denied_codes_match_jco() {
        assert_eq!(Capability::Os.denied_code(), "ERR_JCO_OS_ADAPTER_REQUIRED");
        assert_eq!(Capability::ChildProcess.denied_code(), "ERR_JCO_CHILD_PROCESS_ADAPTER_REQUIRED");
        assert_eq!(Capability::from_interface("trace-events"), Some(Capability::TraceEvents));
    }

    #[test]
    fn instances_do_not_share_a_process() {
        let template = InstanceTemplate::default();
        let first = NodeInstance::new(&template);
        let second = NodeInstance::new(&template);
        first.process().environment.insert("ONLY_FIRST".into(), "1".into());
        assert!(!second.process().environment.contains_key("ONLY_FIRST"));
    }
}
