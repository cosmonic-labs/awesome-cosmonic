//! `jco:node/child-process`: `spawnSync`, on which the guest builds `execSync` and `execFileSync`.
//!
//! The child runs on the host with the guest's own environment and working directory unless the
//! call names others, so granting this interface grants whatever the host user may run.

use std::{
    io::Write as _,
    os::unix::process::{CommandExt as _, ExitStatusExt as _},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use wash_runtime::wasmtime;

use crate::{
    bindings::jco::node::child_process::{Host, ProcessError, SpawnOptions, SpawnOutput},
    errno::{self, UvError},
    hosts::{denied_trap, os},
    plugin::NodeView,
    state::Capability,
};

/// How often a waiting `spawnSync` looks at the child, its deadline and its output limit.
const POLL: Duration = Duration::from_millis(2);

fn spawn_error(error: UvError, file: &str, args: &[String]) -> ProcessError {
    ProcessError {
        name: "Error".into(),
        message: format!("spawnSync {file} {}", error.code),
        code: Some(error.code.into()),
        errno: Some(error.node_errno() as i32),
        syscall: Some(format!("spawnSync {file}")),
        path: Some(file.into()),
        spawnargs: args.to_vec(),
    }
}

fn signal_number(signal: &str) -> i32 {
    if let Ok(number) = signal.parse::<i32>() {
        return number;
    }
    os::signals().into_iter().find(|entry| entry.name == signal).map_or(libc::SIGTERM, |entry| entry.value as i32)
}

fn signal_name(number: i32) -> String {
    os::signals()
        .into_iter()
        .find(|entry| entry.value as i32 == number)
        .map_or_else(|| number.to_string(), |entry| entry.name)
}

fn stdio(kind: Option<&String>) -> Stdio {
    match kind.map(String::as_str) {
        Some("inherit") => Stdio::inherit(),
        Some("ignore") => Stdio::null(),
        _ => Stdio::piped(),
    }
}

/// Collect a pipe on its own thread, counting what arrives against the output limit.
fn collect(
    pipe: Option<impl std::io::Read + Send + 'static>,
    limit: u64,
    overflowed: Arc<AtomicBool>,
) -> Option<thread::JoinHandle<Vec<u8>>> {
    let mut pipe = pipe?;
    Some(thread::spawn(move || {
        let mut data = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    data.extend_from_slice(&chunk[..read]);
                    if data.len() as u64 > limit {
                        // Node keeps what fits and reports ENOBUFS.
                        data.truncate(limit as usize);
                        overflowed.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        }
        data
    }))
}

fn kill(child: &Child, signal: i32) {
    // SAFETY: `kill` takes a pid and a signal number; the child has not been reaped yet.
    unsafe { libc::kill(child.id() as libc::pid_t, signal) };
}

impl Host for NodeView<'_> {
    async fn spawn_sync(
        &mut self,
        command: String,
        args: Vec<String>,
        options: SpawnOptions,
    ) -> wasmtime::Result<SpawnOutput> {
        if !self.node.allows(Capability::ChildProcess) {
            // The function returns no `result`, so a refusal has nowhere to go but a trap.
            return Err(denied_trap(Capability::ChildProcess));
        }
        // With a shell, Node runs `<shell> -c "<command> <args...>"`.
        let (file, arguments) = match &options.shell {
            Some(shell) => {
                let line = std::iter::once(command.as_str()).chain(args.iter().map(String::as_str));
                let shell = if shell.is_empty() { "/bin/sh".to_owned() } else { shell.clone() };
                (shell, vec!["-c".to_owned(), line.collect::<Vec<_>>().join(" ")])
            }
            None => (command, args),
        };
        let (cwd, environment) = {
            let process = self.node.process();
            let cwd = match &options.cwd {
                Some(cwd) if options.cwd_is_url => {
                    std::path::PathBuf::from(cwd.strip_prefix("file://").unwrap_or(cwd))
                }
                Some(cwd) => process.cwd.join(cwd),
                None => process.cwd.clone(),
            };
            let environment: Vec<(String, String)> = match &options.env {
                Some(environment) => environment.clone(),
                None => process.environment.iter().map(|(name, value)| (name.clone(), value.clone())).collect(),
            };
            (cwd, environment)
        };
        // A child found through PATH is found through the PATH it will run with.
        let program = match environment.iter().find(|(name, _)| name == "PATH") {
            Some((_, path)) if !file.contains('/') => std::env::split_paths(path)
                .map(|directory| directory.join(&file))
                .find(|candidate| candidate.is_file())
                .unwrap_or_else(|| file.clone().into()),
            _ => file.clone().into(),
        };
        let kinds = options.stdio.clone().unwrap_or_default();
        let mut builder = Command::new(program);
        builder
            .arg0(&file)
            .args(&arguments)
            .current_dir(&cwd)
            .env_clear()
            .envs(environment)
            .stdin(stdio(kinds.first()))
            .stdout(stdio(kinds.get(1)))
            .stderr(stdio(kinds.get(2)));
        if let Some(uid) = options.uid {
            builder.uid(uid);
        }
        if let Some(gid) = options.gid {
            builder.gid(gid);
        }
        let failed = |error: UvError| SpawnOutput {
            // Node reports pid 0 for a child that never started.
            pid: Some(0),
            status: None,
            signal: None,
            stdout: None,
            stderr: None,
            error: Some(spawn_error(error, &file, &arguments)),
        };
        if !cwd.is_dir() {
            return Ok(failed(errno::from_errno(libc::ENOENT)));
        }
        let mut child = match builder.spawn() {
            Ok(child) => child,
            Err(error) => return Ok(failed(errno::from_io(&error))),
        };
        let pid = child.id();
        let writer = child.stdin.take().map(|mut stdin| {
            let input = options.input.clone().unwrap_or_default();
            // Dropping `stdin` closes the pipe, which is the child's end of input.
            thread::spawn(move || drop(stdin.write_all(&input)))
        });
        let overflowed = Arc::new(AtomicBool::new(false));
        let stdout = collect(child.stdout.take(), options.max_buffer, overflowed.clone());
        let stderr = collect(child.stderr.take(), options.max_buffer, overflowed.clone());
        let deadline = (options.timeout > 0).then(|| Instant::now() + Duration::from_millis(options.timeout));
        let kill_signal = signal_number(&options.kill_signal);
        let mut error = None;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(_) => break None,
            }
            if error.is_none() {
                if overflowed.load(Ordering::SeqCst) {
                    error = Some(errno::from_errno(libc::ENOBUFS));
                    kill(&child, kill_signal);
                } else if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    error = Some(errno::from_errno(libc::ETIMEDOUT));
                    kill(&child, kill_signal);
                }
            }
            thread::sleep(POLL);
        };
        if let Some(writer) = writer {
            let _ = writer.join();
        }
        let join = |handle: Option<thread::JoinHandle<Vec<u8>>>| handle.map(|handle| handle.join().unwrap_or_default());
        let (stdout, stderr) = (join(stdout), join(stderr));
        if error.is_none() && overflowed.load(Ordering::SeqCst) {
            error = Some(errno::from_errno(libc::ENOBUFS));
        }
        Ok(SpawnOutput {
            pid: Some(pid),
            status: status.and_then(|status| status.code()),
            signal: status.and_then(|status| status.signal()).map(signal_name),
            stdout,
            stderr,
            error: error.map(|error| spawn_error(error, &file, &arguments)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_failures_read_like_node() {
        let error = spawn_error(errno::from_errno(libc::ENOENT), "nope", &["a".into()]);
        assert_eq!(error.message, "spawnSync nope ENOENT");
        assert_eq!(error.errno, Some(-2));
        assert_eq!(error.syscall.as_deref(), Some("spawnSync nope"));
        assert_eq!(error.spawnargs, ["a"]);
    }

    #[test]
    fn kill_signals_may_be_names_or_numbers() {
        assert_eq!(signal_number("SIGKILL"), libc::SIGKILL);
        assert_eq!(signal_number("9"), 9);
        assert_eq!(signal_name(libc::SIGTERM), "SIGTERM");
    }
}
