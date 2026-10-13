//! A wasmCloud host component plugin for `cosmonic:cron@0.1.0`.
//!
//! [`spec`] and [`engine`] accept clock readings without WIT imports, so they run in
//! host tests.
//! The wasm-only `plugin` module provides WIT bindings, reads `wasi:clocks`, and
//! dispatches due firings.

pub mod calendar;
pub mod engine;
pub mod spec;

#[cfg(target_arch = "wasm32")]
mod plugin;
