//! `jco:node/tty`: the terminals on the host's standard descriptors.
//!
//! Descriptors 0 to 2 are the host process's own, as they are for Jco's Node provider; nothing
//! else is a terminal. A host whose standard streams are not terminals (a wasmCloud host under a
//! service manager, say) therefore answers `isatty` with `false` and refuses to open a handle the
//! way `uv_tty_init` does.

use std::{collections::HashMap, io, time::Duration};

use wash_runtime::wasmtime;

use crate::{
    bindings::jco::node::tty::{Direction, Errno, Error, ErrorInfo, Host, TerminalSize},
    errno::{self, UvError},
    hosts::denied_message,
    plugin::NodeView,
    state::{Capability, lock},
};

type TtyResult<T> = wasmtime::Result<Result<T, Error>>;

/// How long to wait before retrying a descriptor that is in non-blocking mode.
const RETRY: Duration = Duration::from_millis(5);

/// Open handles, and the terminal settings to put back when raw mode ends.
#[derive(Default)]
pub(crate) struct Terminals {
    handles: HashMap<(u32, bool), u32>,
    cooked: HashMap<u32, libc::termios>,
}

impl Drop for Terminals {
    fn drop(&mut self) {
        for (fd, settings) in self.cooked.drain() {
            // SAFETY: `settings` is the `termios` read from this descriptor earlier.
            unsafe { libc::tcsetattr(fd as i32, libc::TCSADRAIN, &settings) };
        }
    }
}

fn is_terminal(fd: u32) -> bool {
    // SAFETY: `isatty` only inspects the descriptor.
    fd <= 2 && unsafe { libc::isatty(fd as i32) } == 1
}

fn uv(error: UvError, syscall: &str) -> Error {
    Error {
        name: "Error".into(),
        message: format!("{syscall} {}: {}", error.code, error.description),
        code: Some(error.code.into()),
        errno: Some(Errno::Number(error.node_errno())),
        syscall: Some(syscall.into()),
        info: None,
    }
}

fn last(syscall: &str) -> Error {
    uv(errno::from_io(&io::Error::last_os_error()), syscall)
}

fn no_handle(syscall: &str, fd: u32) -> Error {
    Error {
        name: "Error".into(),
        message: format!("{syscall} EBADF: no open terminal handle for fd {fd}"),
        code: Some("EBADF".into()),
        errno: Some(Errno::Number(-9)),
        syscall: Some(syscall.into()),
        info: None,
    }
}

/// Node's `ERR_TTY_INIT_FAILED`, raised when a descriptor is not a terminal.
fn init_failed() -> Error {
    let invalid = errno::from_errno(libc::EINVAL);
    Error {
        name: "SystemError".into(),
        message: format!(
            "TTY initialization failed: uv_tty_init returned {} ({})",
            invalid.code, invalid.description
        ),
        code: Some("ERR_TTY_INIT_FAILED".into()),
        errno: Some(Errno::Number(invalid.node_errno())),
        syscall: Some("uv_tty_init".into()),
        info: Some(ErrorInfo {
            errno: Some(Errno::Number(invalid.node_errno())),
            code: Some(invalid.code.into()),
            message: Some(invalid.description.into()),
            syscall: Some("uv_tty_init".into()),
        }),
    }
}

impl NodeView<'_> {
    fn tty_granted(&self) -> Result<(), Error> {
        if self.node.allows(Capability::Tty) {
            return Ok(());
        }
        Err(Error {
            name: "Error".into(),
            message: denied_message(Capability::Tty),
            code: Some(Capability::Tty.denied_code()),
            errno: None,
            syscall: None,
            info: None,
        })
    }
}

macro_rules! granted {
    ($view:expr) => {
        if let Err(error) = $view.tty_granted() {
            return Ok(Err(error));
        }
    };
}

impl Host for NodeView<'_> {
    async fn is_tty(&mut self, fd: u32) -> TtyResult<bool> {
        granted!(self);
        Ok(Ok(is_terminal(fd)))
    }

    async fn open(&mut self, fd: u32, direction: Direction) -> TtyResult<()> {
        granted!(self);
        if !is_terminal(fd) {
            return Ok(Err(init_failed()));
        }
        *lock(&self.node.terminals).handles.entry((fd, matches!(direction, Direction::Read))).or_insert(0) += 1;
        Ok(Ok(()))
    }

    async fn close(&mut self, fd: u32, direction: Direction) -> TtyResult<()> {
        granted!(self);
        let reading = matches!(direction, Direction::Read);
        let mut terminals = lock(&self.node.terminals);
        let Some(count) = terminals.handles.get_mut(&(fd, reading)) else { return Ok(Ok(())) };
        *count -= 1;
        if *count > 0 {
            return Ok(Ok(()));
        }
        terminals.handles.remove(&(fd, reading));
        if reading && let Some(settings) = terminals.cooked.remove(&fd) {
            // SAFETY: `settings` is the `termios` read from this descriptor when raw mode began.
            unsafe { libc::tcsetattr(fd as i32, libc::TCSADRAIN, &settings) };
        }
        Ok(Ok(()))
    }

    async fn window_size(&mut self, fd: u32) -> TtyResult<TerminalSize> {
        granted!(self);
        if !lock(&self.node.terminals).handles.contains_key(&(fd, false)) {
            return Ok(Err(no_handle("getWindowSize", fd)));
        }
        // SAFETY: `winsize` is plain data, valid when zeroed.
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: `TIOCGWINSZ` writes one `winsize` through the pointer.
        if unsafe { libc::ioctl(fd as i32, libc::TIOCGWINSZ, &mut size) } != 0 {
            return Ok(Err(last("getWindowSize")));
        }
        Ok(Ok(TerminalSize { columns: u32::from(size.ws_col), rows: u32::from(size.ws_row) }))
    }

    async fn set_raw_mode(&mut self, fd: u32, enabled: bool) -> TtyResult<()> {
        granted!(self);
        let mut terminals = lock(&self.node.terminals);
        if !terminals.handles.contains_key(&(fd, true)) {
            return Ok(Err(no_handle("setRawMode", fd)));
        }
        if !enabled {
            if let Some(settings) = terminals.cooked.remove(&fd) {
                // SAFETY: `settings` is the `termios` read from this descriptor earlier.
                if unsafe { libc::tcsetattr(fd as i32, libc::TCSADRAIN, &settings) } != 0 {
                    return Ok(Err(last("setRawMode")));
                }
            }
            return Ok(Ok(()));
        }
        if terminals.cooked.contains_key(&fd) {
            return Ok(Ok(()));
        }
        // SAFETY: `termios` is plain data, valid when zeroed, and `tcgetattr` fills it in.
        let mut settings: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `settings` is a valid, writable `termios`.
        if unsafe { libc::tcgetattr(fd as i32, &mut settings) } != 0 {
            return Ok(Err(last("setRawMode")));
        }
        let cooked = settings;
        // The same changes libuv's `UV_TTY_MODE_RAW` makes.
        settings.c_iflag &= !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON);
        settings.c_oflag |= libc::ONLCR;
        settings.c_cflag |= libc::CS8;
        settings.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
        settings.c_cc[libc::VMIN] = 1;
        settings.c_cc[libc::VTIME] = 0;
        // SAFETY: `settings` is a fully initialised `termios`.
        if unsafe { libc::tcsetattr(fd as i32, libc::TCSADRAIN, &settings) } != 0 {
            return Ok(Err(last("setRawMode")));
        }
        terminals.cooked.insert(fd, cooked);
        Ok(Ok(()))
    }

    async fn read(&mut self, fd: u32, max_bytes: u32) -> TtyResult<Vec<u8>> {
        granted!(self);
        if fd > 2 {
            return Ok(Err(uv(errno::from_errno(libc::EBADF), "read")));
        }
        // The read blocks, so it runs off the async workers.
        let read = tokio::task::spawn_blocking(move || {
            let mut buffer = vec![0u8; max_bytes.max(1) as usize];
            loop {
                // SAFETY: `buffer` is writable for the length passed.
                let count = unsafe { libc::read(fd as i32, buffer.as_mut_ptr().cast(), buffer.len()) };
                if count >= 0 {
                    buffer.truncate(count as usize);
                    return Ok(buffer);
                }
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(libc::EAGAIN) => std::thread::sleep(RETRY),
                    Some(libc::EINTR) => {}
                    // A hung-up pseudo-terminal reads as EIO on Linux; that is the end of input.
                    Some(libc::EIO) => return Ok(Vec::new()),
                    _ => return Err(uv(errno::from_io(&error), "read")),
                }
            }
        })
        .await?;
        Ok(read)
    }

    async fn write(&mut self, fd: u32, data: Vec<u8>) -> TtyResult<()> {
        granted!(self);
        if fd > 2 {
            return Ok(Err(uv(errno::from_errno(libc::EBADF), "write")));
        }
        let mut offset = 0;
        while offset < data.len() {
            // SAFETY: the pointer and length describe the unwritten tail of `data`.
            let count = unsafe { libc::write(fd as i32, data[offset..].as_ptr().cast(), data.len() - offset) };
            if count >= 0 {
                offset += count as usize;
                continue;
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EAGAIN) => tokio::time::sleep(RETRY).await,
                Some(libc::EINTR) => {}
                _ => return Ok(Err(uv(errno::from_io(&error), "write"))),
            }
        }
        Ok(Ok(()))
    }

    async fn environment(&mut self) -> TtyResult<Vec<(String, String)>> {
        granted!(self);
        Ok(Ok(self.node.process().environment.iter().map(|(name, value)| (name.clone(), value.clone())).collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_terminal_fails_like_uv_tty_init() {
        let error = init_failed();
        assert_eq!(error.name, "SystemError");
        assert_eq!(error.message, "TTY initialization failed: uv_tty_init returned EINVAL (invalid argument)");
        assert!(matches!(error.errno, Some(Errno::Number(-22))));
        assert_eq!(error.info.unwrap().code.as_deref(), Some("EINVAL"));
    }
}
