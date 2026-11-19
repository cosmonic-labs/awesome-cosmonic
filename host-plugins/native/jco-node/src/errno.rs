//! libuv's view of OS errors.
//!
//! Node reports a failed system call as `CODE: description, syscall 'path'` with a negative
//! `errno`, and the guest half of `jco:node` rebuilds its `Error` from exactly those fields. The
//! names and descriptions here are libuv's (`uv_err_name`, `uv_strerror`), not the C library's,
//! because the two differ in wording and Node programs match on libuv's.

use std::io;

/// One libuv error: its symbolic name, its description, and the (positive) OS errno.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UvError {
    pub code: &'static str,
    pub description: &'static str,
    pub errno: i32,
}

impl UvError {
    /// The value Node exposes as `error.errno`: the negated OS errno.
    pub fn node_errno(&self) -> i64 {
        -i64::from(self.errno)
    }
}

macro_rules! uv_errors {
    ($(($name:ident, $description:literal)),* $(,)?) => {
        const TABLE: &[UvError] = &[
            $(UvError { code: stringify!($name), description: $description, errno: libc::$name },)*
        ];
    };
}

uv_errors![
    (E2BIG, "argument list too long"),
    (EACCES, "permission denied"),
    (EADDRINUSE, "address already in use"),
    (EADDRNOTAVAIL, "address not available"),
    (EAFNOSUPPORT, "address family not supported"),
    (EAGAIN, "resource temporarily unavailable"),
    (EALREADY, "connection already in progress"),
    (EBADF, "bad file descriptor"),
    (EBUSY, "resource busy or locked"),
    (ECANCELED, "operation canceled"),
    (ECONNABORTED, "software caused connection abort"),
    (ECONNREFUSED, "connection refused"),
    (ECONNRESET, "connection reset by peer"),
    (EDESTADDRREQ, "destination address required"),
    (EEXIST, "file already exists"),
    (EFAULT, "bad address in system call argument"),
    (EFBIG, "file too large"),
    (EHOSTUNREACH, "host is unreachable"),
    (EINTR, "interrupted system call"),
    (EINVAL, "invalid argument"),
    (EIO, "i/o error"),
    (EISCONN, "socket is already connected"),
    (EISDIR, "illegal operation on a directory"),
    (ELOOP, "too many symbolic links encountered"),
    (EMFILE, "too many open files"),
    (EMSGSIZE, "message too long"),
    (ENAMETOOLONG, "name too long"),
    (ENETDOWN, "network is down"),
    (ENETUNREACH, "network is unreachable"),
    (ENFILE, "file table overflow"),
    (ENOBUFS, "no buffer space available"),
    (ENODEV, "no such device"),
    (ENOENT, "no such file or directory"),
    (ENOMEM, "not enough memory"),
    (ENOPROTOOPT, "protocol not available"),
    (ENOSPC, "no space left on device"),
    (ENOSYS, "function not implemented"),
    (ENOTCONN, "socket is not connected"),
    (ENOTDIR, "not a directory"),
    (ENOTEMPTY, "directory not empty"),
    (ENOTSOCK, "socket operation on non-socket"),
    (ENOTSUP, "operation not supported on socket"),
    (EOVERFLOW, "value too large for defined data type"),
    (EPERM, "operation not permitted"),
    (EPIPE, "broken pipe"),
    (EPROTO, "protocol error"),
    (EPROTONOSUPPORT, "protocol not supported"),
    (EPROTOTYPE, "protocol wrong type for socket"),
    (ERANGE, "result too large"),
    (EROFS, "read-only file system"),
    (ESPIPE, "invalid seek"),
    (ESRCH, "no such process"),
    (ETIMEDOUT, "connection timed out"),
    (ETXTBSY, "text file is busy"),
    (EXDEV, "cross-device link not permitted"),
    (ENXIO, "no such device or address"),
    (EMLINK, "too many links"),
    (ENOTTY, "inappropriate ioctl for device"),
    (EILSEQ, "illegal byte sequence"),
    (ESOCKTNOSUPPORT, "socket type not supported"),
    (ENODATA, "no data available"),
];

/// What libuv reports for an errno it has no name for.
const UNKNOWN: UvError = UvError { code: "UNKNOWN", description: "unknown error", errno: 4094 };

/// Look an OS errno up; an unrecognised one is libuv's `UNKNOWN`.
pub fn from_errno(errno: i32) -> UvError {
    TABLE.iter().copied().find(|entry| entry.errno == errno).unwrap_or(UNKNOWN)
}

/// Look a symbolic name such as `ENOENT` up.
pub fn from_code(code: &str) -> Option<UvError> {
    TABLE.iter().copied().find(|entry| entry.code == code)
}

/// The libuv error for a failed `std::io` operation.
pub fn from_io(error: &io::Error) -> UvError {
    if let Some(errno) = error.raw_os_error() {
        return from_errno(errno);
    }
    let errno = match error.kind() {
        io::ErrorKind::NotFound => libc::ENOENT,
        io::ErrorKind::PermissionDenied => libc::EACCES,
        io::ErrorKind::AlreadyExists => libc::EEXIST,
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => libc::EINVAL,
        io::ErrorKind::TimedOut => libc::ETIMEDOUT,
        io::ErrorKind::ConnectionRefused => libc::ECONNREFUSED,
        io::ErrorKind::ConnectionReset => libc::ECONNRESET,
        io::ErrorKind::ConnectionAborted => libc::ECONNABORTED,
        io::ErrorKind::NotConnected => libc::ENOTCONN,
        io::ErrorKind::AddrInUse => libc::EADDRINUSE,
        io::ErrorKind::AddrNotAvailable => libc::EADDRNOTAVAIL,
        io::ErrorKind::BrokenPipe => libc::EPIPE,
        io::ErrorKind::WouldBlock => libc::EAGAIN,
        io::ErrorKind::Interrupted => libc::EINTR,
        io::ErrorKind::Unsupported => libc::ENOSYS,
        io::ErrorKind::OutOfMemory => libc::ENOMEM,
        _ => return UNKNOWN,
    };
    from_errno(errno)
}

/// Every errno with a libuv name, as `os.constants.errno` lists them.
pub fn all() -> &'static [UvError] {
    TABLE
}

/// Node's message for a failed system call: `ENOENT: no such file or directory, open '/x'`,
/// with `-> '/y'` appended for the two-path calls (`rename`, `link`, `copyfile`, ...).
pub fn syscall_message(error: UvError, syscall: &str, path: Option<&str>, dest: Option<&str>) -> String {
    let mut message = format!("{}: {}, {syscall}", error.code, error.description);
    if let Some(path) = path {
        message.push_str(&format!(" '{path}'"));
        if let Some(dest) = dest {
            message.push_str(&format!(" -> '{dest}'"));
        }
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_a_path_error_the_way_node_does() {
        let error = from_errno(libc::ENOENT);
        assert_eq!(error.node_errno(), -2);
        assert_eq!(
            syscall_message(error, "open", Some("/missing"), None),
            "ENOENT: no such file or directory, open '/missing'"
        );
        assert_eq!(
            syscall_message(from_errno(libc::EXDEV), "rename", Some("/a"), Some("/b")),
            "EXDEV: cross-device link not permitted, rename '/a' -> '/b'"
        );
    }

    #[test]
    fn an_unnamed_errno_is_unknown() {
        assert_eq!(from_errno(9999).code, "UNKNOWN");
        assert_eq!(from_code("EPERM").map(|e| e.errno), Some(libc::EPERM));
    }
}
