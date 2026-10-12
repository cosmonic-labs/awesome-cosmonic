//! `cosmonic:cron@0.1.0` as a wasmCloud host component plugin.
//!
//! The crate is two halves. [`spec`] and [`engine`] are the schedule logic:
//! they read no clock and import nothing, so `cargo test` runs them on the
//! host. `plugin` is the component: it holds the WIT bindings, reads
//! `wasi:clocks`, and delivers what the engine says is due. It only exists in
//! a wasm build.

pub mod engine;
pub mod spec;

#[cfg(target_arch = "wasm32")]
mod plugin;
