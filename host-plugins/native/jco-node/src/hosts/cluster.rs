//! `jco:node/cluster`: workers that are fresh instances of the same component in processes of
//! their own, as Node's `cluster.fork()` re-executes its entry.
//!
//! Starting a process is the embedder's decision, so the plugin forks only through a
//! [`WorkerLauncher`] it is given; without one `fork` is unsupported. The primary and each worker
//! talk over a Unix socket pair, one JSON object per line:
//!
//! - worker to primary: `{"online":true}`, `{"message":<json>}`
//! - primary to worker: `{"message":<json>}`, `{"disconnect":true}`
//!
//! Closing the channel is a disconnect. Events are queued for the guest's `drain-events`, in the
//! order Node emits them: fork, online, messages, disconnect, exit.

use std::{
    collections::{BTreeMap, VecDeque},
    io::Write as _,
    os::{
        fd::{AsRawFd as _, FromRawFd as _, OwnedFd},
        unix::{net::UnixStream, process::CommandExt as _},
    },
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt as _, BufReader},
    sync::Notify,
};
use wash_runtime::wasmtime;

use crate::{
    bindings::jco::node::cluster::{Error, Event, ExitInfo, Host, MessageInfo, Settings, WorkerInfo, WorkerState},
    hosts::denied_trap,
    plugin::NodeView,
    state::{Capability, lock},
};

/// In a worker's environment: its id.
pub const WORKER_ID_ENV: &str = "JCO_NODE_CLUSTER_WORKER";
/// In a worker's environment: the descriptor of its channel to the primary.
pub const CHANNEL_ENV: &str = "JCO_NODE_CLUSTER_CHANNEL";

/// How an embedder starts a cluster worker: a command that runs the same component afresh, with
/// `env` added to the guest's environment. The plugin adds the worker's id and channel.
pub trait WorkerLauncher: Send + Sync {
    fn command(&self, id: u32, env: &[(String, String)]) -> std::process::Command;
}

struct Worker {
    state: WorkerState,
    connected: bool,
    dead: bool,
    exited_after_disconnect: bool,
    channel: Option<UnixStream>,
    pid: Option<u32>,
}

impl Worker {
    fn info(&self, id: u32) -> WorkerInfo {
        WorkerInfo {
            id,
            state: self.state,
            exited_after_disconnect: self.exited_after_disconnect,
            connected: self.connected,
            dead: self.dead,
        }
    }
}

enum Role {
    Primary { launcher: Option<Arc<dyn WorkerLauncher>> },
    Worker { id: u32, channel: Mutex<Option<UnixStream>> },
}

/// One process's side of a cluster, shared by its instances.
pub struct Cluster {
    role: Role,
    settings: Mutex<Option<Settings>>,
    workers: Mutex<BTreeMap<u32, Worker>>,
    events: Mutex<VecDeque<Event>>,
    next: AtomicU32,
    disconnected: AtomicBool,
    closed: Notify,
}

impl Default for Cluster {
    fn default() -> Self {
        Self::with_role(Role::Primary { launcher: None })
    }
}

impl std::fmt::Debug for Cluster {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Cluster").field("primary", &self.is_primary()).finish_non_exhaustive()
    }
}

fn write_line(channel: &mut UnixStream, value: &Value) -> std::io::Result<()> {
    channel.write_all(format!("{value}\n").as_bytes())
}

fn signal_number(name: &str) -> Option<i32> {
    Some(match name {
        "SIGTERM" | "" => libc::SIGTERM,
        "SIGKILL" => libc::SIGKILL,
        "SIGINT" => libc::SIGINT,
        "SIGHUP" => libc::SIGHUP,
        "SIGQUIT" => libc::SIGQUIT,
        "SIGUSR1" => libc::SIGUSR1,
        "SIGUSR2" => libc::SIGUSR2,
        _ => return None,
    })
}

fn signal_name(number: i32) -> String {
    match number {
        libc::SIGTERM => "SIGTERM",
        libc::SIGKILL => "SIGKILL",
        libc::SIGINT => "SIGINT",
        libc::SIGHUP => "SIGHUP",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGABRT => "SIGABRT",
        libc::SIGSEGV => "SIGSEGV",
        _ => return format!("SIG{number}"),
    }
    .into()
}

/// Lines from a channel, until it closes.
async fn lines(channel: UnixStream, mut each: impl FnMut(Value)) {
    let Ok(()) = channel.set_nonblocking(true) else { return };
    let Ok(channel) = tokio::net::UnixStream::from_std(channel) else { return };
    let mut lines = BufReader::new(channel).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Ok(value) = serde_json::from_str(&line) {
            each(value);
        }
    }
}

impl Cluster {
    fn with_role(role: Role) -> Self {
        Self {
            role,
            settings: Mutex::new(None),
            workers: Mutex::default(),
            events: Mutex::default(),
            next: AtomicU32::new(0),
            disconnected: AtomicBool::new(false),
            closed: Notify::new(),
        }
    }

    /// The primary process, forking workers through `launcher`.
    pub fn primary(launcher: Arc<dyn WorkerLauncher>) -> Arc<Self> {
        Arc::new(Self::with_role(Role::Primary { launcher: Some(launcher) }))
    }

    /// A worker, from what its primary put in its environment: `None` in a primary.
    pub fn from_environment() -> anyhow::Result<Option<Arc<Self>>> {
        let Some(id) = std::env::var_os(WORKER_ID_ENV) else { return Ok(None) };
        let id: u32 = id.to_string_lossy().parse()?;
        let fd: i32 = std::env::var(CHANNEL_ENV)?.parse()?;
        // SAFETY: the primary passed this descriptor for the channel and nothing else owns it.
        let channel = UnixStream::from(unsafe { OwnedFd::from_raw_fd(fd) });
        let cluster = Arc::new(Self::with_role(Role::Worker { id, channel: Mutex::new(Some(channel.try_clone()?)) }));
        let reader = cluster.clone();
        tokio::spawn(async move {
            lines(channel, |value| {
                if let Some(message) = value.get("message") {
                    lock(&reader.events).push_back(Event::Message(MessageInfo { id, json: message.to_string() }));
                }
            })
            .await;
            reader.close_worker_channel();
        });
        Ok(Some(cluster))
    }

    pub fn is_primary(&self) -> bool {
        matches!(self.role, Role::Primary { .. })
    }

    /// Tell the primary this worker is up. A worker's embedder calls this once its instance is.
    pub fn announce_online(&self) {
        if let Role::Worker { channel, .. } = &self.role {
            if let Some(channel) = lock(channel).as_mut() {
                let _ = write_line(channel, &json!({ "online": true }));
            }
        }
    }

    /// Resolves once a worker's channel to its primary has closed: the worker should exit.
    pub async fn disconnected(&self) {
        loop {
            let notified = self.closed.notified();
            if self.disconnected.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    fn close_worker_channel(&self) {
        if let Role::Worker { id, channel } = &self.role {
            if lock(channel).take().is_some() || !self.disconnected.load(Ordering::SeqCst) {
                lock(&self.events).push_back(Event::Disconnect(*id));
            }
        }
        self.disconnected.store(true, Ordering::SeqCst);
        self.closed.notify_waiters();
    }

    fn fork(self: &Arc<Self>, env: Vec<(String, String)>) -> Result<WorkerInfo, Error> {
        let Role::Primary { launcher } = &self.role else {
            return Err(Error::Unsupported("cluster.fork() is only available in the primary".into()));
        };
        let Some(launcher) = launcher else {
            return Err(Error::Unsupported("this host does not start cluster workers".into()));
        };
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let (ours, theirs) = UnixStream::pair().map_err(|error| Error::Failed(error.to_string()))?;
        let fd = theirs.as_raw_fd();
        let mut command = launcher.command(id, &env);
        command.env(WORKER_ID_ENV, id.to_string()).env(CHANNEL_ENV, fd.to_string()).stdin(Stdio::null());
        // SAFETY: only async-signal-safe calls run between fork and exec.
        unsafe {
            command.pre_exec(move || {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = tokio::process::Command::from(command)
            .spawn()
            .map_err(|error| Error::Failed(format!("starting worker {id}: {error}")))?;
        drop(theirs);
        let writer = ours.try_clone().map_err(|error| Error::Failed(error.to_string()))?;
        let worker = Worker {
            state: WorkerState::None,
            connected: true,
            dead: false,
            exited_after_disconnect: false,
            channel: Some(writer),
            pid: child.id(),
        };
        let info = worker.info(id);
        lock(&self.workers).insert(id, worker);
        lock(&self.events).push_back(Event::Fork(id));
        let cluster = self.clone();
        tokio::spawn(async move {
            lines(ours, |value| {
                if value.get("online").is_some() {
                    if let Some(worker) = lock(&cluster.workers).get_mut(&id) {
                        worker.state = WorkerState::Online;
                    }
                    lock(&cluster.events).push_back(Event::Online(id));
                } else if let Some(message) = value.get("message") {
                    lock(&cluster.events).push_back(Event::Message(MessageInfo { id, json: message.to_string() }));
                }
            })
            .await;
            if let Some(worker) = lock(&cluster.workers).get_mut(&id) {
                worker.connected = false;
                worker.channel = None;
                worker.state = WorkerState::Disconnected;
            }
            lock(&cluster.events).push_back(Event::Disconnect(id));
            let status = child.wait().await;
            use std::os::unix::process::ExitStatusExt as _;
            let (code, signal) = match status {
                Ok(status) => (status.code().unwrap_or(0), status.signal().map(signal_name).unwrap_or_default()),
                Err(_) => (1, String::new()),
            };
            if let Some(worker) = lock(&cluster.workers).get_mut(&id) {
                worker.dead = true;
                worker.state = WorkerState::Dead;
            }
            lock(&cluster.events).push_back(Event::Exit(ExitInfo { id, code, signal }));
        });
        Ok(info)
    }

    fn send(&self, id: u32, json: &str) -> Result<(), Error> {
        let message: Value = serde_json::from_str(json).map_err(|error| Error::Failed(error.to_string()))?;
        let line = json!({ "message": message });
        match &self.role {
            Role::Worker { channel, .. } => match lock(channel).as_mut() {
                Some(channel) => write_line(channel, &line).map_err(|error| Error::Failed(error.to_string())),
                None => Err(Error::Failed("the IPC channel is closed".into())),
            },
            Role::Primary { .. } => {
                let mut workers = lock(&self.workers);
                let worker = workers.get_mut(&id).ok_or(Error::NoSuchWorker(id))?;
                match worker.channel.as_mut() {
                    Some(channel) => write_line(channel, &line).map_err(|error| Error::Failed(error.to_string())),
                    None => Err(Error::Failed(format!("worker {id} is disconnected"))),
                }
            }
        }
    }

    fn disconnect(&self, id: u32) -> Result<(), Error> {
        match &self.role {
            Role::Worker { id: own, .. } if *own == id => {
                self.close_worker_channel();
                Ok(())
            }
            Role::Worker { .. } => Err(Error::NoSuchWorker(id)),
            Role::Primary { .. } => {
                let mut workers = lock(&self.workers);
                let worker = workers.get_mut(&id).ok_or(Error::NoSuchWorker(id))?;
                worker.exited_after_disconnect = true;
                if let Some(mut channel) = worker.channel.take() {
                    let _ = write_line(&mut channel, &json!({ "disconnect": true }));
                    let _ = channel.shutdown(std::net::Shutdown::Write);
                }
                Ok(())
            }
        }
    }
}

impl NodeView<'_> {
    fn cluster_allowed(&self) -> wasmtime::Result<&Arc<Cluster>> {
        if !self.node.allows(Capability::Cluster) {
            return Err(denied_trap(Capability::Cluster));
        }
        Ok(&self.node.cluster)
    }
}

impl Host for NodeView<'_> {
    async fn is_primary(&mut self) -> wasmtime::Result<bool> {
        Ok(self.cluster_allowed()?.is_primary())
    }

    async fn current_worker(&mut self) -> wasmtime::Result<Option<WorkerInfo>> {
        let cluster = self.cluster_allowed()?;
        Ok(match &cluster.role {
            Role::Primary { .. } => None,
            Role::Worker { id, .. } => {
                let connected = !cluster.disconnected.load(Ordering::SeqCst);
                Some(WorkerInfo {
                    id: *id,
                    state: if connected { WorkerState::Online } else { WorkerState::Disconnected },
                    exited_after_disconnect: false,
                    connected,
                    dead: false,
                })
            }
        })
    }

    async fn fork(&mut self, env: Vec<(String, String)>) -> wasmtime::Result<Result<WorkerInfo, Error>> {
        Ok(self.cluster_allowed()?.clone().fork(env))
    }

    async fn list_workers(&mut self) -> wasmtime::Result<Vec<WorkerInfo>> {
        let cluster = self.cluster_allowed()?;
        Ok(lock(&cluster.workers).iter().map(|(id, worker)| worker.info(*id)).collect())
    }

    async fn get_worker(&mut self, id: u32) -> wasmtime::Result<Result<WorkerInfo, Error>> {
        let cluster = self.cluster_allowed()?;
        Ok(lock(&cluster.workers).get(&id).map(|worker| worker.info(id)).ok_or(Error::NoSuchWorker(id)))
    }

    async fn send(&mut self, id: u32, json: String) -> wasmtime::Result<Result<(), Error>> {
        Ok(self.cluster_allowed()?.send(id, &json))
    }

    async fn disconnect_worker(&mut self, id: u32) -> wasmtime::Result<Result<(), Error>> {
        Ok(self.cluster_allowed()?.disconnect(id))
    }

    async fn disconnect_all(&mut self) -> wasmtime::Result<Result<(), Error>> {
        let cluster = self.cluster_allowed()?;
        let ids: Vec<u32> = lock(&cluster.workers).keys().copied().collect();
        for id in ids {
            cluster.disconnect(id).ok();
        }
        Ok(Ok(()))
    }

    async fn kill(&mut self, id: u32, signal: String) -> wasmtime::Result<Result<(), Error>> {
        let cluster = self.cluster_allowed()?;
        let Some(number) = signal_number(&signal) else {
            return Ok(Err(Error::Failed(format!("Unknown signal: {signal}"))));
        };
        let pid = match lock(&cluster.workers).get(&id) {
            Some(worker) => worker.pid,
            None => return Ok(Err(Error::NoSuchWorker(id))),
        };
        if let Some(pid) = pid {
            // SAFETY: signalling a child process this host started.
            unsafe { libc::kill(pid as i32, number) };
        }
        Ok(Ok(()))
    }

    async fn get_settings(&mut self) -> wasmtime::Result<Settings> {
        let cluster = self.cluster_allowed()?.clone();
        let argv = &self.node.identity.argv;
        Ok(lock(&cluster.settings).clone().unwrap_or_else(|| Settings {
            silent: false,
            args: argv.iter().skip(2).cloned().collect(),
            cwd: String::new(),
            // Node's default everywhere but Windows.
            scheduling_policy: 2,
        }))
    }

    async fn set_settings(&mut self, value: Settings) -> wasmtime::Result<Result<(), Error>> {
        *lock(&self.cluster_allowed()?.settings) = Some(value);
        Ok(Ok(()))
    }

    async fn drain_events(&mut self) -> wasmtime::Result<Vec<Event>> {
        Ok(lock(&self.cluster_allowed()?.events).drain(..).collect())
    }
}
