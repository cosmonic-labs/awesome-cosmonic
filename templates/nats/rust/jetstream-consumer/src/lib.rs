//! JetStream Consumer — Durable at-least-once consumption from a JetStream stream, with ack control.
//!
//! WHEN TO USE THIS
//! You need delivery guarantees. JetStream retains messages, redelivers on failure, and — critically — paces delivery by acknowledgement, so a slow consumer is throttled instead of overrun.
//!
//! WHEN NOT TO
//! Do not use it for latency-critical request paths, and do not assume exactly-once. Redelivery is real; handlers must be idempotent.
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
    world: "jetstream-consumer",
    generate_all,
});

use exports::wasmcloud::nats::jetstream_handler::Guest as JetstreamHandler;
use wasmcloud::nats::jetstream::{self, MessageHandle};
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

impl JetstreamHandler for Component {
    async fn handle_message(handle: MessageHandle) -> Result<(), String> {
        let msg = handle.message();
        let sequence = handle.sequence();
        let delivery = handle.delivery_count();

        if field(&msg.body, "trap").is_some() {
            panic!("injected trap at sequence {sequence}");
        }
        if let Some(n) = field(&msg.body, "fail").and_then(|v| v.parse::<u32>().ok())
            && delivery <= n
        {
            return Err(format!(
                "injected failure {delivery}/{n} at sequence {sequence}"
            ));
        }
        if let Some(hold) = field(&msg.body, "hold").and_then(|v| v.parse::<u64>().ok())
            && hold > 0
        {
            std::thread::sleep(std::time::Duration::from_millis(hold));
        }

        let run = field(&msg.body, "run").unwrap_or_else(|| "na".to_string());
        // Delivery count in the SUBJECT: redelivery becomes countable
        // server-side from the stream's subject map, no body reads needed.
        let receipt = NatsMessage {
            subject: format!("done.js-sink.{run}.d{delivery}"),
            body: format!("seq={sequence};bytes={}", msg.body.len()).into_bytes(),
            reply_to: None,
            headers: None,
        };
        jetstream::publish(receipt)
            .await
            .map_err(|e| format!("receipt publish failed: {e:?}"))?;

        // Under `ack-mode: auto` the Ok return acks; under `manual` the body
        // must say which settle path to take or the message times out to
        // redelivery after the 30s ack-wait (itself a scenario).
        if field(&msg.body, "macksync").is_some() {
            handle
                .ack_sync()
                .await
                .map_err(|e| format!("ack-sync failed: {e:?}"))?;
        } else if field(&msg.body, "mack").is_some() {
            handle.ack().await.map_err(|e| format!("ack failed: {e:?}"))?;
        }
        Ok(())
    }
}

export!(Component);
