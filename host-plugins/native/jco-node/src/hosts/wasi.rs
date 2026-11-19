//! `jco:node/wasi`: the checks `new WASI(options)` makes when it initialises uvwasi.
//!
//! Nothing is kept: the contract hands the host initialisation alone, since a component cannot
//! link a nested module to the context. What matters is failing the way `uvwasi_init` fails.

use std::{fs, io};

use wash_runtime::wasmtime;

use crate::{
    bindings::jco::node::wasi::{Errno, Error, Host, Options},
    hosts::denied_message,
    plugin::NodeView,
    state::Capability,
};

/// A `uvwasi_init` failure: uvwasi's own errno names and numbers, not the host's.
fn uvwasi(name: &str, number: i64) -> Error {
    Error {
        name: "Error".into(),
        message: format!("UVWASI_{name}, uvwasi_init"),
        code: Some(format!("UVWASI_{name}")),
        errno: Some(Errno::Number(number)),
        syscall: Some("uvwasi_init".into()),
    }
}

fn from_io(error: &io::Error) -> Error {
    match error.raw_os_error() {
        Some(libc::ENOENT) => uvwasi("ENOENT", 44),
        Some(libc::ENOTDIR) => uvwasi("ENOTDIR", 54),
        Some(libc::EACCES) => uvwasi("EACCES", 2),
        Some(libc::ELOOP) => uvwasi("ELOOP", 32),
        Some(libc::ENAMETOOLONG) => uvwasi("ENAMETOOLONG", 37),
        Some(libc::EMFILE) => uvwasi("EMFILE", 33),
        Some(libc::ENFILE) => uvwasi("ENFILE", 41),
        Some(libc::EPERM) => uvwasi("EPERM", 63),
        _ => uvwasi("EIO", 29),
    }
}

impl Host for NodeView<'_> {
    async fn init(&mut self, options: Options) -> wasmtime::Result<Result<(), Error>> {
        if !self.node.allows(Capability::Wasi) {
            return Ok(Err(Error {
                name: "Error".into(),
                message: denied_message(Capability::Wasi),
                code: Some(Capability::Wasi.denied_code()),
                errno: None,
                syscall: None,
            }));
        }
        // uvwasi takes over the three standard descriptors first: each must be open.
        for descriptor in [options.stdin, options.stdout, options.stderr] {
            if descriptor > 2 && self.node.descriptors().get(descriptor).is_none() {
                return Ok(Err(uvwasi("EBADF", 8)));
            }
        }
        // Then it opens every preopen, which must be a directory it may read.
        for (_, host) in &options.preopens {
            let path = self.node.resolve(host);
            match fs::metadata(&path) {
                Err(error) => return Ok(Err(from_io(&error))),
                Ok(metadata) if !metadata.is_dir() => return Ok(Err(uvwasi("ENOTDIR", 54))),
                Ok(_) => {}
            }
            if let Err(error) = fs::read_dir(&path) {
                return Ok(Err(from_io(&error)));
            }
        }
        Ok(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_use_uvwasi_names_and_numbers() {
        let error = from_io(&io::Error::from_raw_os_error(libc::ENOENT));
        assert_eq!(error.message, "UVWASI_ENOENT, uvwasi_init");
        assert_eq!(error.code.as_deref(), Some("UVWASI_ENOENT"));
        assert!(matches!(error.errno, Some(Errno::Number(44))));
    }
}
