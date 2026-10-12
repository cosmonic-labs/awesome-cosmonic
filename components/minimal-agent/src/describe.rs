//! Error messages a person can read, for every error the agent reports.

use crate::bindings::cosmonic::agent::inference_types::{
    ContextOverflow, GenerationError, Quota, RateLimit, RequestError, SizeLimit,
};
use crate::bindings::cosmonic::agent::models::OpenError;
use crate::bindings::cosmonic::agent::session;
use crate::bindings::cosmonic::agent::tools::CallError;

/// Error messages a person can read. The generated types implement `Display`
/// and `Debug` too, but both print Rust syntax, and the formatting code behind
/// them is a good part of the component's size.
pub(crate) trait Describe {
    fn describe(&self) -> String;
}

impl Describe for OpenError {
    fn describe(&self) -> String {
        match self {
            Self::UnknownAlias(d) => format!("the host has no model by that alias ({d})"),
            Self::Denied(d) => format!("this workload may not use it ({d})"),
            Self::Unavailable(d) => format!("the model is unavailable ({d})"),
            Self::TooLarge(limit) => format!("the alias is over the {limit}-byte limit"),
            Self::Other(d) => d.clone(),
        }
    }
}

impl Describe for RequestError {
    fn describe(&self) -> String {
        match self {
            Self::ContextLengthExceeded(c) => c.describe(),
            Self::InvalidOptions(d) => format!("invalid options ({d})"),
            Self::MessagesIncomplete(d) => format!("the conversation was incomplete ({d})"),
            Self::Unrepresentable(changes) => {
                let details: Vec<&str> = changes.iter().map(|c| c.detail.as_str()).collect();
                format!(
                    "the provider cannot carry this request ({})",
                    details.join("; ")
                )
            }
            Self::NotAuthenticated(d) => {
                format!("the host's credentials for the provider were rejected ({d})")
            }
            Self::RateLimited(r) => format!("rate limited: {}", r.describe()),
            Self::QuotaExhausted(q) => q.describe(),
            Self::TemporarilyUnavailable(r) => format!("temporarily unavailable: {}", r.describe()),
            Self::TooLarge(s) => s.describe(),
            Self::Other(d) => d.clone(),
        }
    }
}

impl Describe for GenerationError {
    fn describe(&self) -> String {
        match self {
            Self::RateLimited(r) => format!("rate limited: {}", r.describe()),
            Self::TemporarilyUnavailable(r) => format!("temporarily unavailable: {}", r.describe()),
            Self::ContextLengthExceeded(c) => c.describe(),
            Self::Other(d) => d.clone(),
        }
    }
}

impl Describe for ContextOverflow {
    fn describe(&self) -> String {
        format!(
            "the conversation is too long for the model ({})",
            self.detail
        )
    }
}

impl Describe for RateLimit {
    fn describe(&self) -> String {
        match self.retry_after_seconds {
            Some(s) => format!("{}; retry in {s}s", self.detail),
            None => self.detail.clone(),
        }
    }
}

impl Describe for Quota {
    fn describe(&self) -> String {
        let out = format!("the provider account is out of quota ({})", self.detail);
        match &self.top_up_url {
            Some(url) => format!("{out}; top up at {url}"),
            None => out,
        }
    }
}

impl Describe for SizeLimit {
    fn describe(&self) -> String {
        format!(
            "the {} is over the host's {}-byte limit",
            self.what, self.limit
        )
    }
}

impl Describe for session::Error {
    fn describe(&self) -> String {
        match self {
            Self::TooLarge(t) => format!("the session is full ({}-byte limit)", t.limit),
            Self::InvalidArgument(d) => format!("invalid request ({d})"),
            Self::Unavailable(d) => format!("unavailable ({d})"),
            Self::Other(d) => d.clone(),
        }
    }
}

impl Describe for CallError {
    fn describe(&self) -> String {
        match self {
            Self::UnknownTool(name) => format!("there is no tool named {name}"),
            Self::InvalidArguments(d) => format!("invalid arguments: {d}"),
            Self::Upstream(d) => format!("the tool could not run: {d}"),
            Self::Denied(d) => format!("the call was declined: {d}"),
            Self::Cancelled => "the call was cancelled".into(),
            Self::TimedOut => "the call timed out".into(),
            Self::Other(d) => d.clone(),
        }
    }
}
