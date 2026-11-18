//! One module per `jco:node` interface.

pub mod child_process;
pub mod cluster;
pub mod console;
pub mod crypto_kdf;
pub mod dgram;
pub mod dns;
pub mod ffi;
pub mod fs;
pub mod http;
pub mod http2;
pub mod inspector;
pub mod os;
pub mod process;
pub mod sqlite;
pub mod timers;
pub mod tls;
pub(crate) mod tls_config;
pub mod trace_events;
pub mod tty;
pub mod v8;
pub mod wasi;
pub mod worker_threads;
pub mod zlib;

use wash_runtime::wasmtime;

use crate::state::Capability;

/// The message Jco's default providers refuse with.
pub(crate) fn denied_message(capability: Capability) -> String {
    format!("{} requires an application-provided host adapter", capability.module())
}

/// The `denied` case Jco's default providers refuse with on interfaces whose error is a variant.
pub(crate) fn denied_variant(capability: Capability) -> String {
    format!("map jco:node/{}@0.1.0 to a host adapter to grant it", capability.interface())
}

/// Refuse a call on an interface whose functions return no `result`. Jco's JavaScript host
/// throws a coded `Error` here; a WIT function with no error case has nothing to carry one in,
/// so the call traps with the same code and message.
pub(crate) fn denied_trap(capability: Capability) -> wasmtime::Error {
    wasmtime::Error::msg(format!(
        "{}: {} (grant jco:node/{} to the workload)",
        capability.denied_code(),
        denied_message(capability),
        capability.interface()
    ))
}
