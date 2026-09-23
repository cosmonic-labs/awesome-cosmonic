//! Fan-Out / Amplifier — Receive one message and republish it to many — the classic scatter pattern.
//!
//! WHEN TO USE THIS
//! One input event needs to become many units of downstream work: notify N subscribers, shard a job, or trigger a parallel pipeline.
//!
//! WHEN NOT TO
//! Do not use it with core publish at scale without reading the warning below. This is the pattern that produced the campaign's largest data loss.
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
    world: "fan-out",
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
        let fanout: u32 = field(&msg.body, "fanout")
            .and_then(|v| v.parse().ok())
            .unwrap_or(25);
        for _ in 0..fanout {
            core::publish(NatsMessage {
                subject: "fan.work".to_string(),
                body: msg.body.clone(),
                reply_to: None,
                headers: None,
            })
            .await
            .map_err(|e| format!("fan-out publish failed: {e:?}"))?;
        }
        Ok(())
    }
}

export!(Component);
