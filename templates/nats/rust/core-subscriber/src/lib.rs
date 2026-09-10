//! Core Subscriber — Receive fire-and-forget core NATS messages on a subject and do work per message.
//!
//! WHEN TO USE THIS
//! You have a stream of events on a NATS subject and want a component invoked per message. No acknowledgement, no redelivery, no ordering guarantees — the cheapest possible consumer.
//!
//! WHEN NOT TO
//! Do not use this when losing a message matters. Core NATS has no ack and no redelivery: if the handler traps, or the subscription buffer overflows, the message is gone silently.
//!
//! This implementation is extracted from the `nats-2.8-testing` campaign,
//! where it ran as part of 186 measured cells against the
//! wasmcloud:nats driver. See docs/tuning.md for the operational envelope and
//! the findings behind it.
//!
//! START HERE: the `handle_*` function below is the only thing you need to
//! change. Everything above it is binding glue.

wit_bindgen::generate!({
    path: "wit",
    world: "core-subscriber",
    generate_all,
});

use exports::wasmcloud::nats::core_handler::Guest as CoreHandler;
use wasmcloud::nats::jetstream;
use wasmcloud::nats::types::NatsMessage;

struct Component;

/// Reads `key=` out of the body's header prefix. Only the first 256 bytes are
/// scanned so a 900 KiB payload costs nothing to parse.
fn field(body: &[u8], key: &str) -> Option<String> {
    let head = &body[..body.len().min(256)];
    let text = String::from_utf8_lossy(head);
    for part in text.split(';') {
        let (k, v) = part.split_once('=')?;
        if k == "pad" {
            return None;
        }
        if k == key {
            return Some(v.to_string());
        }
    }
    None
}

impl CoreHandler for Component {
    async fn handle_message(msg: NatsMessage) -> Result<(), String> {
        let run = field(&msg.body, "run").unwrap_or_else(|| "na".to_string());
        let hold_ms: u64 = field(&msg.body, "hold")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if hold_ms > 0 {
            // Deliberately blocking: the residency experiments need the
            // delivery to stay in flight, holding its admission permit.
            std::thread::sleep(std::time::Duration::from_millis(hold_ms));
        }

        let receipt = NatsMessage {
            subject: format!("done.core-sink.{run}"),
            body: format!("subject={};bytes={}", msg.subject, msg.body.len()).into_bytes(),
            reply_to: None,
            headers: None,
        };
        jetstream::publish(receipt)
            .await
            .map(|_| ())
            .map_err(|e| format!("receipt publish failed: {e:?}"))
    }
}

export!(Component);
