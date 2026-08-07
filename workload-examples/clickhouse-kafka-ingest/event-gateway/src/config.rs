//! Runtime configuration from `wasi:config/store`.
//!
//! Populated by `workload.config` / `workload.environment.configFrom` in
//! `.wash/config.yaml` locally, and by the WorkloadDeployment in a cluster.
//! Nothing here is baked in at build time, which is the point: the same
//! `.wasm` artifact runs against a local Redpanda and a production broker.
//!
//! `wasi:config` has no 0.3.0 release yet, so this stays on the 0.2 interface
//! while HTTP moves to p3. Mixed-version imports are the expected state during
//! the transition.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use crate::bindings::wasi::config::store::get_all;

/// Redpanda's HTTP Proxy as published by `infra/docker-compose.yaml`.
const DEFAULT_PROXY_URL: &str = "http://localhost:18082";
const DEFAULT_TOPIC: &str = "clickstream.events";
/// Bounds `POST /events/simulate?count=`. A component that will happily build
/// a 10-million-record JSON body in memory is a denial of service against
/// itself.
const DEFAULT_MAX_BATCH: usize = 5_000;

pub(crate) struct Config {
    /// `host:port`. `wasi:http@0.3.0` builds a request from scheme, authority,
    /// and path rather than a single URL string, so the configured URL is
    /// split once here.
    pub(crate) proxy_authority: String,
    pub(crate) topic: String,
    pub(crate) max_batch: usize,
}

impl Config {
    /// Human-readable target, for `/healthz`.
    pub(crate) fn produce_url(&self) -> String {
        format!("http://{}/topics/{}", self.proxy_authority, self.topic)
    }
}

static CONFIG: OnceLock<Config> = OnceLock::new();

pub(crate) fn config() -> &'static Config {
    CONFIG.get_or_init(build_config)
}

fn build_config() -> Config {
    let entries: BTreeMap<String, String> = get_all().unwrap_or_default().into_iter().collect();

    let proxy_url = entries
        .get("KAFKA_PROXY_URL")
        .filter(|v| !v.is_empty())
        .map(String::as_str)
        .unwrap_or(DEFAULT_PROXY_URL);

    Config {
        proxy_authority: authority_of(proxy_url),
        topic: entries
            .get("KAFKA_TOPIC")
            .filter(|v| !v.is_empty())
            .cloned()
            .unwrap_or_else(|| DEFAULT_TOPIC.to_string()),
        max_batch: entries
            .get("MAX_BATCH")
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_BATCH),
    }
}

/// `http://host:port/path` -> `host:port`. Accepts a bare `host:port` too, so
/// either form works in configuration.
fn authority_of(url: &str) -> String {
    let without_scheme = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url);
    without_scheme
        .split(['/', '?'])
        .next()
        .unwrap_or(without_scheme)
        .to_string()
}
