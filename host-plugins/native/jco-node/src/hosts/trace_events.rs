//! `jco:node/trace-events`: category sets a guest enables, and the trace file they produce.
//!
//! Jco's Node provider turns on V8 and Node's own trace agent, so its events describe that
//! runtime. A Rust host has neither, so the events recorded here are the plugin's own: calls
//! other `jco:node` hosts report through [`Tracing::record`] while their category is enabled.
//! They are written, in the Trace Event Format Node uses, to `node_trace.1.log` in the guest's
//! working directory once the last session stops.

use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    time::Instant,
};

use serde_json::json;
use wash_runtime::wasmtime::{self, component::Resource};

use crate::{
    bindings::jco::node::trace_events::{Error, Host, HostTraceSession, TraceSession},
    hosts::denied_message,
    plugin::NodeView,
    state::{Capability, lock},
};

type TraceResult<T> = wasmtime::Result<Result<T, Error>>;

struct Event {
    category: String,
    name: String,
    at_micros: u128,
}

/// One instance's enabled categories and the events captured under them.
#[derive(Default)]
pub(crate) struct Tracing {
    next: u32,
    /// Running sessions and the categories each owns.
    sessions: HashMap<u32, Vec<String>>,
    events: Vec<Event>,
    wrote: bool,
}

impl Tracing {
    fn enabled(&self) -> BTreeMap<&str, usize> {
        let mut counts = BTreeMap::new();
        for category in self.sessions.values().flatten() {
            *counts.entry(category.as_str()).or_insert(0) += 1;
        }
        counts
    }

    /// Note a host call under `category`, if a session has that category enabled.
    pub(crate) fn record(&mut self, started: Instant, category: &str, name: &str) {
        if self.sessions.values().flatten().any(|enabled| enabled == category) {
            self.events.push(Event {
                category: category.to_owned(),
                name: name.to_owned(),
                at_micros: started.elapsed().as_micros(),
            });
        }
    }

    fn stop(&mut self, session: u32, directory: PathBuf) {
        if self.sessions.remove(&session).is_none() || !self.sessions.is_empty() {
            return;
        }
        // Node's writer creates the file with the first flush and appends afterwards; one file
        // per instance is rewritten whole here, which reads the same.
        let pid = std::process::id();
        let mut events = vec![json!({
            "pid": pid, "tid": pid, "ts": 0, "tts": 0, "ph": "M", "cat": "__metadata",
            "name": "process_name", "dur": 0, "tdur": 0, "args": { "name": "node" },
        })];
        events.extend(self.events.iter().map(|event| {
            json!({
                "pid": pid, "tid": pid, "ts": event.at_micros as u64, "tts": 0, "ph": "X",
                "cat": event.category, "name": event.name, "dur": 0, "tdur": 0, "args": {},
            })
        }));
        // Tracing must never fail the guest's work, so a file that cannot be written is skipped.
        let _ = std::fs::write(directory.join("node_trace.1.log"), json!({ "traceEvents": events }).to_string());
        self.wrote = true;
    }
}

impl NodeView<'_> {
    fn trace_granted(&self) -> Result<(), Error> {
        if self.node.allows(Capability::TraceEvents) {
            return Ok(());
        }
        Err(Error {
            name: "Error".into(),
            code: Some(Capability::TraceEvents.denied_code()),
            message: denied_message(Capability::TraceEvents),
        })
    }

    fn stop_session(&self, session: u32) {
        let directory = self.node.process().cwd.clone();
        lock(&self.node.tracing).stop(session, directory);
    }
}

impl HostTraceSession for NodeView<'_> {
    async fn start(&mut self, categories: Vec<String>) -> TraceResult<Resource<TraceSession>> {
        if let Err(error) = self.trace_granted() {
            return Ok(Err(error));
        }
        let mut tracing = lock(&self.node.tracing);
        tracing.next += 1;
        let session = tracing.next;
        tracing.sessions.insert(session, categories);
        Ok(Ok(Resource::new_own(session)))
    }

    async fn stop(&mut self, self_: Resource<TraceSession>) -> TraceResult<()> {
        if let Err(error) = self.trace_granted() {
            return Ok(Err(error));
        }
        self.stop_session(self_.rep());
        Ok(Ok(()))
    }

    async fn drop(&mut self, rep: Resource<TraceSession>) -> wasmtime::Result<()> {
        self.stop_session(rep.rep());
        Ok(())
    }
}

impl Host for NodeView<'_> {
    async fn release(&mut self, session: Resource<TraceSession>) -> wasmtime::Result<()> {
        self.stop_session(session.rep());
        Ok(())
    }

    async fn get_enabled_categories(&mut self) -> TraceResult<Option<String>> {
        if let Err(error) = self.trace_granted() {
            return Ok(Err(error));
        }
        let tracing = lock(&self.node.tracing);
        let categories: Vec<&str> = tracing.enabled().into_keys().collect();
        Ok(Ok((!categories.is_empty()).then(|| categories.join(","))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_are_a_sorted_union_released_per_owner() {
        let mut tracing = Tracing::default();
        tracing.sessions.insert(1, vec!["b".into(), "a".into(), "a".into()]);
        tracing.sessions.insert(2, vec!["a".into()]);
        assert_eq!(tracing.enabled().into_keys().collect::<Vec<_>>(), ["a", "b"]);
        let directory = std::env::temp_dir().join(format!("jco-node-trace-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        tracing.stop(1, directory.clone());
        assert_eq!(tracing.enabled().into_keys().collect::<Vec<_>>(), ["a"]);
        assert!(!tracing.wrote);
        tracing.record(Instant::now(), "a", "open");
        tracing.stop(2, directory.clone());
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(directory.join("node_trace.1.log")).unwrap()).unwrap();
        assert_eq!(written["traceEvents"][1]["cat"], "a");
        std::fs::remove_dir_all(directory).unwrap();
    }
}
