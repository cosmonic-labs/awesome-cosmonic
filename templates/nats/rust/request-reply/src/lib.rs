//! Request / Reply — Answer NATS requests — an RPC endpoint that scales to zero between calls.
//!
//! WHEN TO USE THIS
//! You want a service other components or clients call and wait on. The host delivers the request, you publish the answer to the requester's reply subject. Per-request instantiation means it costs nothing when idle.
//!
//! WHEN NOT TO
//! Do not use it for work longer than the caller's timeout, and do not use it for fire-and-forget notifications — a reply nobody awaits is wasted work.
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
    world: "request-reply",
    generate_all,
});

use exports::wasmcloud::nats::core_handler::Guest as CoreHandler;
use wasmcloud::nats::core;
use wasmcloud::nats::types::NatsMessage;

struct Component;

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
        let Some(reply_to) = msg.reply_to else {
            // Delivered without a reply subject — a plain publish landed on
            // the request subject. Nothing to answer.
            return Ok(());
        };
        if let Some(hold) = field(&msg.body, "hold").and_then(|v| v.parse::<u64>().ok())
            && hold > 0
        {
            std::thread::sleep(std::time::Duration::from_millis(hold));
        }
        let reply = NatsMessage {
            subject: reply_to,
            body: format!("echo:{}", msg.body.len()).into_bytes(),
            reply_to: None,
            headers: None,
        };
        core::publish(reply)
            .await
            .map_err(|e| format!("reply publish failed: {e:?}"))
    }
}

export!(Component);
