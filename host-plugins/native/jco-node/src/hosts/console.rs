//! `jco:node/console`: the guest's `console` writes to the component's own WASI stdout and
//! stderr, so whatever the embedder did with those streams (inherit, capture, redirect) applies.

use bytes::Bytes;
use wash_runtime::wasmtime;
use wasmtime_wasi::{
    cli::WasiCliCtxView,
    p2::bindings::cli::{stderr, stdout, terminal_stderr, terminal_stdout},
};

use crate::{
    bindings::jco::node::console::{Host, OutputDestination},
    hosts::denied_trap,
    plugin::NodeView,
    state::Capability,
};

impl NodeView<'_> {
    pub(crate) fn cli(&mut self) -> WasiCliCtxView<'_> {
        WasiCliCtxView { ctx: self.ctx.ctx.cli(), table: self.table }
    }

    /// Write all of `data` to one of the component's standard streams.
    pub(crate) async fn write_stdio(&mut self, destination: OutputDestination, data: Vec<u8>) -> wasmtime::Result<()> {
        let stream = match destination {
            OutputDestination::Stdout => stdout::Host::get_stdout(&mut self.cli())?,
            OutputDestination::Stderr => stderr::Host::get_stderr(&mut self.cli())?,
        };
        let written = self.table.get_mut(&stream)?.blocking_write_and_flush(Bytes::from(data)).await;
        self.table.delete(stream)?;
        // A closed stream is the embedder's choice, not a guest fault: Node drops the write too.
        let _ = written;
        Ok(())
    }

    pub(crate) fn stdio_is_terminal(&mut self, destination: OutputDestination) -> wasmtime::Result<bool> {
        Ok(match destination {
            OutputDestination::Stdout => match terminal_stdout::Host::get_terminal_stdout(&mut self.cli())? {
                Some(terminal) => {
                    self.table.delete(terminal)?;
                    true
                }
                None => false,
            },
            OutputDestination::Stderr => match terminal_stderr::Host::get_terminal_stderr(&mut self.cli())? {
                Some(terminal) => {
                    self.table.delete(terminal)?;
                    true
                }
                None => false,
            },
        })
    }
}

/// `tty.WriteStream.prototype.getColorDepth` for the common cases: the guest's own environment
/// decides, as it does in Node.
pub(crate) fn color_depth(environment: &std::collections::BTreeMap<String, String>, is_terminal: bool) -> u8 {
    if let Some(force) = environment.get("FORCE_COLOR") {
        return match force.as_str() {
            "" | "1" | "true" => 4,
            "2" => 8,
            "3" => 24,
            _ => 1,
        };
    }
    if environment.contains_key("NODE_DISABLE_COLORS") || environment.contains_key("NO_COLOR") {
        return 1;
    }
    if !is_terminal || environment.get("TERM").map(String::as_str) == Some("dumb") {
        return 1;
    }
    if environment.get("COLORTERM").is_some_and(|value| value == "truecolor" || value == "24bit") {
        return 24;
    }
    match environment.get("TERM") {
        Some(term) if term.ends_with("-256color") || term.ends_with("256") => 8,
        _ => 4,
    }
}

impl Host for NodeView<'_> {
    async fn write(&mut self, destination: OutputDestination, value: String) -> wasmtime::Result<()> {
        if !self.node.allows(Capability::Console) {
            return Err(denied_trap(Capability::Console));
        }
        self.write_stdio(destination, value.into_bytes()).await
    }

    async fn is_terminal(&mut self, destination: OutputDestination) -> wasmtime::Result<bool> {
        if !self.node.allows(Capability::Console) {
            return Err(denied_trap(Capability::Console));
        }
        self.stdio_is_terminal(destination)
    }

    async fn color_depth(&mut self, destination: OutputDestination) -> wasmtime::Result<u8> {
        if !self.node.allows(Capability::Console) {
            return Err(denied_trap(Capability::Console));
        }
        let is_terminal = self.stdio_is_terminal(destination)?;
        Ok(color_depth(&self.node.process().environment, is_terminal))
    }
}

#[cfg(test)]
mod tests {
    use super::color_depth;
    use std::collections::BTreeMap;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn a_pipe_has_no_colors() {
        assert_eq!(color_depth(&env(&[("TERM", "xterm-256color")]), false), 1);
    }

    #[test]
    fn the_environment_picks_the_depth_on_a_terminal() {
        assert_eq!(color_depth(&env(&[("TERM", "xterm-256color")]), true), 8);
        assert_eq!(color_depth(&env(&[("COLORTERM", "truecolor")]), true), 24);
        assert_eq!(color_depth(&env(&[("NO_COLOR", "1")]), true), 1);
        assert_eq!(color_depth(&env(&[("FORCE_COLOR", "3")]), false), 24);
    }
}
