//! One module per `jco:node` interface.

pub mod child_process;
pub mod console;
pub mod crypto_kdf;
pub mod fs;
pub mod os;
pub mod process;
pub mod timers;
pub mod trace_events;
pub mod tty;
pub mod wasi;

use wash_runtime::wasmtime;

use crate::state::Capability;

/// The message Jco's default providers refuse with.
pub(crate) fn denied_message(capability: Capability) -> String {
    format!("{} requires an application-provided host adapter", capability.module())
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
