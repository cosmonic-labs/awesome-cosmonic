//! Runtime configuration from `wasi:config/store`.
//!
//! The password comes through the same interface as everything else. Locally
//! that is `workload.config`; in a cluster it is
//! `workload.environment.secretFrom`, and the component code does not change.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use crate::bindings::wasi::config::store::get_all;

const DEFAULT_URL: &str = "http://localhost:8123";
const DEFAULT_DATABASE: &str = "analytics";
const DEFAULT_USER: &str = "analytics";
/// Matches the compose default so the example runs with nothing exported.
const DEFAULT_PASSWORD: &str = "analytics";
/// Origin the dashboard posts to for the "generate traffic" button. Only ever
/// interpolated into the page as JSON, never into SQL.
const DEFAULT_GATEWAY_URL: &str = "http://localhost:8000";

pub(crate) struct Config {
    /// `host:port`. `wasi:http@0.3.0` builds a request from scheme, authority,
    /// and path rather than a single URL string, so the configured URL is
    /// split once here.
    pub(crate) authority: String,
    pub(crate) url: String,
    pub(crate) database: String,
    pub(crate) user: String,
    pub(crate) password: String,
    pub(crate) gateway_url: String,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

pub(crate) fn config() -> &'static Config {
    CONFIG.get_or_init(build_config)
}

fn build_config() -> Config {
    let entries: BTreeMap<String, String> = get_all().unwrap_or_default().into_iter().collect();
    let get = |key: &str, default: &str| {
        entries
            .get(key)
            .filter(|v| !v.is_empty())
            .cloned()
            .unwrap_or_else(|| default.to_string())
    };

    let url = get("CLICKHOUSE_URL", DEFAULT_URL);
    Config {
        authority: authority_of(&url),
        url,
        database: get("CLICKHOUSE_DATABASE", DEFAULT_DATABASE),
        user: get("CLICKHOUSE_USER", DEFAULT_USER),
        password: get("CLICKHOUSE_PASSWORD", DEFAULT_PASSWORD),
        gateway_url: get("GATEWAY_URL", DEFAULT_GATEWAY_URL),
    }
}

/// `http://host:port/path` -> `host:port`. Accepts a bare `host:port` too, so
/// either form works in configuration.
fn authority_of(url: &str) -> String {
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    without_scheme
        .split(['/', '?'])
        .next()
        .unwrap_or(without_scheme)
        .to_string()
}
