//! `jco:node/os`: what `node:os` reports about the machine the component runs on.

use std::ffi::CStr;

use sysinfo::{CpuRefreshKind, MemoryRefreshKind, Networks, RefreshKind, System};
use wash_runtime::wasmtime;

use crate::{
    bindings::jco::node::os::{
        ByteOrder, ConstantEntry, CpuInfo, CpuTimes, Errno, Error, ErrorInfo, Host, LoadAverage, NetworkFamily,
        NetworkInterface, OsConstants, StaticProperties, UserInfoRecord, UserInfoValue,
    },
    errno::{self, UvError},
    hosts::denied_message,
    plugin::NodeView,
    state::Capability,
};

type OsResult<T> = wasmtime::Result<Result<T, Error>>;

fn denied() -> Error {
    Error {
        name: "Error".into(),
        message: denied_message(Capability::Os),
        code: Some(Capability::Os.denied_code()),
        errno: None,
        syscall: None,
        info: None,
    }
}

/// Node's `SystemError` for a failed libuv OS call: the detail lives in `info`.
fn system_error(error: UvError, syscall: &str) -> Error {
    Error {
        name: "SystemError".into(),
        message: format!("A system error occurred: {syscall} returned {} ({})", error.code, error.description),
        code: Some("ERR_SYSTEM_ERROR".into()),
        errno: Some(Errno::Number(error.node_errno())),
        syscall: Some(syscall.into()),
        info: Some(ErrorInfo {
            errno: Some(Errno::Number(error.node_errno())),
            code: Some(error.code.into()),
            message: Some(error.description.into()),
            syscall: Some(syscall.into()),
        }),
    }
}

fn last_os_error(syscall: &str) -> Error {
    system_error(errno::from_io(&std::io::Error::last_os_error()), syscall)
}

fn entry(name: &str, value: i64) -> ConstantEntry {
    ConstantEntry { name: name.into(), value }
}

macro_rules! signals {
    ($($name:ident),* $(,)?) => { vec![$(entry(stringify!($name), i64::from(libc::$name)),)*] };
}

/// `os.constants.signals`, in the order Node lists them.
pub(crate) fn signals() -> Vec<ConstantEntry> {
    let mut list = signals![
        SIGHUP, SIGINT, SIGQUIT, SIGILL, SIGTRAP, SIGABRT, SIGIOT, SIGBUS, SIGFPE, SIGKILL, SIGUSR1, SIGSEGV,
        SIGUSR2, SIGPIPE, SIGALRM, SIGTERM, SIGCHLD,
    ];
    #[cfg(target_os = "linux")]
    list.extend(signals![SIGSTKFLT]);
    list.extend(signals![
        SIGCONT, SIGSTOP, SIGTSTP, SIGTTIN, SIGTTOU, SIGURG, SIGXCPU, SIGXFSZ, SIGVTALRM, SIGPROF, SIGWINCH, SIGIO,
    ]);
    #[cfg(target_os = "linux")]
    list.extend(signals![SIGPOLL, SIGPWR]);
    list.extend(signals![SIGSYS]);
    list
}

fn constants() -> OsConstants {
    OsConstants {
        uv_udp_reuseaddr: 4,
        dlopen: vec![
            entry("RTLD_LAZY", i64::from(libc::RTLD_LAZY)),
            entry("RTLD_NOW", i64::from(libc::RTLD_NOW)),
            entry("RTLD_GLOBAL", i64::from(libc::RTLD_GLOBAL)),
            entry("RTLD_LOCAL", i64::from(libc::RTLD_LOCAL)),
        ],
        errno: errno::all().iter().map(|error| entry(error.code, i64::from(error.errno))).collect(),
        signals: signals(),
        priority: vec![
            entry("PRIORITY_LOW", 19),
            entry("PRIORITY_BELOW_NORMAL", 10),
            entry("PRIORITY_NORMAL", 0),
            entry("PRIORITY_ABOVE_NORMAL", -7),
            entry("PRIORITY_HIGH", -14),
            entry("PRIORITY_HIGHEST", -20),
        ],
    }
}

/// `process.arch` / `os.arch()` for the host's architecture.
pub(crate) fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        "powerpc64" => "ppc64",
        "riscv64" => "riscv64",
        "s390x" => "s390x",
        "loongarch64" => "loong64",
        other => other,
    }
}

/// `process.platform` / `os.platform()` for the host's operating system.
pub(crate) fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        "solaris" | "illumos" => "sunos",
        other => other,
    }
}

fn uname() -> Result<libc::utsname, Error> {
    // SAFETY: `utsname` is plain bytes, valid when zeroed, and `uname` fills it in.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is a valid, writable `utsname`.
    if unsafe { libc::uname(&mut name) } != 0 {
        return Err(last_os_error("uv_os_uname"));
    }
    Ok(name)
}

fn field(bytes: &[libc::c_char]) -> String {
    // SAFETY: `uname` NUL-terminates every field it writes.
    unsafe { CStr::from_ptr(bytes.as_ptr()) }.to_string_lossy().into_owned()
}

/// The password-database entry of the user the host runs as.
pub(crate) struct Passwd {
    pub username: Vec<u8>,
    pub uid: u32,
    pub gid: u32,
    pub shell: Option<Vec<u8>>,
    pub homedir: Vec<u8>,
}

pub(crate) fn passwd() -> Result<Passwd, UvError> {
    // SAFETY: `geteuid` has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let mut buffer = vec![0u8; 4096];
    loop {
        // SAFETY: `passwd` is plain data, valid when zeroed; `getpwuid_r` fills it in.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is to live storage of the stated size, and the strings
        // `entry` ends up pointing at live in `buffer`, which outlives their use below.
        let status = unsafe {
            libc::getpwuid_r(uid, &mut entry, buffer.as_mut_ptr().cast(), buffer.len(), &mut found)
        };
        if status == libc::ERANGE && buffer.len() < 1 << 20 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if status != 0 {
            return Err(errno::from_errno(status));
        }
        if found.is_null() {
            return Err(errno::from_errno(libc::ENOENT));
        }
        let bytes = |pointer: *const libc::c_char| -> Option<Vec<u8>> {
            // SAFETY: a non-null field of a found entry is a NUL-terminated string in `buffer`.
            (!pointer.is_null()).then(|| unsafe { CStr::from_ptr(pointer) }.to_bytes().to_vec())
        };
        return Ok(Passwd {
            username: bytes(entry.pw_name).unwrap_or_default(),
            uid: entry.pw_uid,
            gid: entry.pw_gid,
            shell: bytes(entry.pw_shell),
            homedir: bytes(entry.pw_dir).unwrap_or_default(),
        });
    }
}

fn netmask(family_bits: u32, prefix: u8, v6: bool) -> String {
    if v6 {
        let mask = if prefix == 0 { 0 } else { u128::MAX << (family_bits - u32::from(prefix)) };
        std::net::Ipv6Addr::from(mask).to_string()
    } else {
        let mask = if prefix == 0 { 0 } else { u32::MAX << (family_bits - u32::from(prefix)) };
        std::net::Ipv4Addr::from(mask).to_string()
    }
}

impl NodeView<'_> {
    fn os_granted(&self) -> Result<(), Error> {
        if self.node.allows(Capability::Os) { Ok(()) } else { Err(denied()) }
    }
}

macro_rules! granted {
    ($view:expr) => {
        if let Err(error) = $view.os_granted() {
            return Ok(Err(error));
        }
    };
}

impl Host for NodeView<'_> {
    async fn get_static_properties(&mut self) -> OsResult<StaticProperties> {
        // Static POSIX values reveal nothing about the machine, so Jco serves them even when the
        // rest of the interface is refused; a guest cannot import `node:os` at all without them.
        Ok(Ok(StaticProperties { eol: "\n".into(), dev_null: "/dev/null".into(), constants: constants() }))
    }

    async fn arch(&mut self) -> OsResult<String> {
        granted!(self);
        Ok(Ok(arch().into()))
    }

    async fn available_parallelism(&mut self) -> OsResult<u32> {
        granted!(self);
        Ok(Ok(std::thread::available_parallelism().map_or(1, |count| count.get() as u32)))
    }

    async fn cpus(&mut self) -> OsResult<Vec<CpuInfo>> {
        granted!(self);
        let system = System::new_with_specifics(RefreshKind::new().with_cpu(CpuRefreshKind::everything()));
        let times = cpu_times();
        Ok(Ok(system
            .cpus()
            .iter()
            .enumerate()
            .map(|(index, cpu)| CpuInfo {
                model: cpu.brand().to_owned(),
                speed: cpu.frequency() as u32,
                times: times.get(index).cloned().unwrap_or(CpuTimes { user: 0, nice: 0, sys: 0, idle: 0, irq: 0 }),
            })
            .collect()))
    }

    async fn endianness(&mut self) -> OsResult<ByteOrder> {
        granted!(self);
        Ok(Ok(if cfg!(target_endian = "big") { ByteOrder::Be } else { ByteOrder::Le }))
    }

    async fn freemem(&mut self) -> OsResult<u64> {
        granted!(self);
        let system = System::new_with_specifics(RefreshKind::new().with_memory(MemoryRefreshKind::everything()));
        Ok(Ok(system.available_memory()))
    }

    async fn get_priority(&mut self, pid: i32) -> OsResult<i32> {
        granted!(self);
        // -1 is a legal priority, so failure can only be told apart through errno.
        // SAFETY: `__errno_location` returns this thread's errno, which is always writable.
        unsafe { *errno_location() = 0 };
        // SAFETY: `getpriority` takes plain integers.
        let priority = unsafe { libc::getpriority(libc::PRIO_PROCESS, pid as libc::id_t) };
        // SAFETY: as above.
        let failed = unsafe { *errno_location() };
        if priority == -1 && failed != 0 {
            return Ok(Err(system_error(errno::from_errno(failed), "uv_os_getpriority")));
        }
        Ok(Ok(priority))
    }

    async fn homedir(&mut self) -> OsResult<String> {
        granted!(self);
        if let Some(home) = self.node.process().environment.get("HOME").filter(|home| !home.is_empty()) {
            return Ok(Ok(home.clone()));
        }
        Ok(passwd()
            .map(|entry| String::from_utf8_lossy(&entry.homedir).into_owned())
            .map_err(|error| system_error(error, "uv_os_homedir")))
    }

    async fn hostname(&mut self) -> OsResult<String> {
        granted!(self);
        Ok(uname().map(|name| field(&name.nodename)))
    }

    async fn loadavg(&mut self) -> OsResult<LoadAverage> {
        granted!(self);
        let load = System::load_average();
        Ok(Ok(LoadAverage { one: load.one, five: load.five, fifteen: load.fifteen }))
    }

    async fn machine(&mut self) -> OsResult<String> {
        granted!(self);
        Ok(uname().map(|name| field(&name.machine)))
    }

    async fn network_interfaces(&mut self) -> OsResult<Vec<NetworkInterface>> {
        granted!(self);
        let networks = Networks::new_with_refreshed_list();
        let mut interfaces = Vec::new();
        for (name, data) in &networks {
            let mac = data.mac_address().to_string().to_lowercase();
            for network in data.ip_networks() {
                let v6 = network.addr.is_ipv6();
                let internal = network.addr.is_loopback();
                let address = network.addr.to_string();
                interfaces.push(NetworkInterface {
                    name: name.clone(),
                    cidr: Some(format!("{address}/{}", network.prefix)),
                    netmask: netmask(if v6 { 128 } else { 32 }, network.prefix, v6),
                    address,
                    family: if v6 { NetworkFamily::Ipv6 } else { NetworkFamily::Ipv4 },
                    mac: mac.clone(),
                    internal,
                    scopeid: v6.then_some(0),
                });
            }
        }
        // Loopback first, then by name, which is the order Linux hands libuv its interfaces in.
        interfaces.sort_by(|a, b| (!a.internal, &a.name, a.family as u8).cmp(&(!b.internal, &b.name, b.family as u8)));
        Ok(Ok(interfaces))
    }

    async fn platform(&mut self) -> OsResult<String> {
        granted!(self);
        Ok(Ok(platform().into()))
    }

    async fn release(&mut self) -> OsResult<String> {
        granted!(self);
        Ok(uname().map(|name| field(&name.release)))
    }

    async fn set_priority(&mut self, pid: i32, priority: i32) -> OsResult<()> {
        granted!(self);
        // A guest may only renice the process it believes it is.
        if pid != 0 && pid as u32 != std::process::id() {
            return Ok(Err(system_error(errno::from_errno(libc::EPERM), "uv_os_setpriority")));
        }
        // SAFETY: `setpriority` takes plain integers.
        if unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, priority) } != 0 {
            return Ok(Err(last_os_error("uv_os_setpriority")));
        }
        Ok(Ok(()))
    }

    async fn tmpdir(&mut self) -> OsResult<String> {
        granted!(self);
        let process = self.node.process();
        let mut path = ["TMPDIR", "TMP", "TEMP"]
            .iter()
            .find_map(|name| process.environment.get(*name).filter(|value| !value.is_empty()).cloned())
            .unwrap_or_else(|| "/tmp".into());
        if path.len() > 1 && path.ends_with('/') {
            path.pop();
        }
        Ok(Ok(path))
    }

    async fn totalmem(&mut self) -> OsResult<u64> {
        granted!(self);
        let system = System::new_with_specifics(RefreshKind::new().with_memory(MemoryRefreshKind::everything()));
        Ok(Ok(system.total_memory()))
    }

    async fn type_(&mut self) -> OsResult<String> {
        granted!(self);
        Ok(uname().map(|name| field(&name.sysname)))
    }

    async fn uptime(&mut self) -> OsResult<f64> {
        granted!(self);
        Ok(Ok(System::uptime() as f64))
    }

    async fn user_info(&mut self, encoding: Option<String>) -> OsResult<UserInfoRecord> {
        granted!(self);
        let entry = match passwd() {
            Ok(entry) => entry,
            Err(error) => return Ok(Err(system_error(error, "uv_os_get_passwd"))),
        };
        let buffer = encoding.as_deref() == Some("buffer");
        let value = |bytes: Vec<u8>| {
            if buffer {
                UserInfoValue::Bytes(bytes)
            } else {
                UserInfoValue::Text(String::from_utf8_lossy(&bytes).into_owned())
            }
        };
        Ok(Ok(UserInfoRecord {
            username: value(entry.username),
            uid: i64::from(entry.uid),
            gid: i64::from(entry.gid),
            shell: entry.shell.map(value),
            homedir: value(entry.homedir),
        }))
    }

    async fn version(&mut self) -> OsResult<String> {
        granted!(self);
        Ok(uname().map(|name| field(&name.version)))
    }
}

#[cfg(target_os = "linux")]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: always returns a valid pointer to the calling thread's errno.
    unsafe { libc::__errno_location() }
}

#[cfg(not(target_os = "linux"))]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: always returns a valid pointer to the calling thread's errno.
    unsafe { libc::__error() }
}

/// Per-CPU times in milliseconds, as Node reports them. Linux exposes them in `/proc/stat`;
/// elsewhere they read as zero.
fn cpu_times() -> Vec<CpuTimes> {
    let Ok(stat) = std::fs::read_to_string("/proc/stat") else {
        return Vec::new();
    };
    // SAFETY: `sysconf` takes a plain integer.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    let to_ms = |value: Option<&str>| value.and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 1000 / ticks;
    stat.lines()
        .filter(|line| line.starts_with("cpu") && line.as_bytes().get(3).is_some_and(u8::is_ascii_digit))
        .map(|line| {
            let mut fields = line.split_whitespace().skip(1);
            let user = to_ms(fields.next());
            let nice = to_ms(fields.next());
            let sys = to_ms(fields.next());
            let idle = to_ms(fields.next());
            let _iowait = fields.next();
            let irq = to_ms(fields.next());
            CpuTimes { user, nice, sys, idle, irq }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_os_call_is_a_node_system_error() {
        let error = system_error(errno::from_errno(libc::ESRCH), "uv_os_getpriority");
        assert_eq!(error.name, "SystemError");
        assert_eq!(error.code.as_deref(), Some("ERR_SYSTEM_ERROR"));
        assert_eq!(error.message, "A system error occurred: uv_os_getpriority returned ESRCH (no such process)");
        let info = error.info.unwrap();
        assert_eq!(info.code.as_deref(), Some("ESRCH"));
        assert!(matches!(info.errno, Some(Errno::Number(-3))));
    }

    #[test]
    fn netmasks_come_from_prefix_lengths() {
        assert_eq!(netmask(32, 24, false), "255.255.255.0");
        assert_eq!(netmask(128, 64, true), "ffff:ffff:ffff:ffff::");
    }

    #[test]
    fn the_host_user_has_a_name() {
        // The build user may have no passwd entry in a container; then the lookup must fail
        // cleanly rather than read garbage.
        if let Ok(entry) = passwd() {
            assert!(!entry.username.is_empty());
        }
    }
}
