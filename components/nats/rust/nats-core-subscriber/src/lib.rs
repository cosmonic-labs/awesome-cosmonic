//! Core subscriber — the component runs once per message on a NATS subject.
//!
//! Core NATS is fire-and-forget. There is no acknowledgement, no redelivery
//! and no ordering: if this handler traps, or the host's subscription buffer
//! overflows, that message is gone. Returning an error is recorded in the
//! host's logs but changes nothing for the sender. Reach for
//! `nats-jetstream-consumer` when losing a message is not acceptable.
//!
//! The host owns the subscription (`core-subscriptions` on the binding) and
//! bounds how many deliveries are in flight at once (`max-in-flight`). Each
//! delivery beyond the warm pool runs on its own fresh instance, so resident
//! memory scales with `max-in-flight` × instance footprint.

mod bindings {
    use super::Component;
    wit_bindgen::generate!({ world: "core-subscriber", generate_all });
    export!(Component);
}

use bindings::exports::wasmcloud::nats::core_handler::Guest as CoreHandler;
use bindings::wasmcloud::nats::jetstream;
use bindings::wasmcloud::nats::types::NatsMessage;

struct Component;

/// Where results go. A constant rather than something derived from the
/// incoming message: the rest of this set fixes its publish targets in code so
/// a sender cannot steer them, and this is no different.
const RECEIPT_SUBJECT: &str = "done.demo.events";

impl CoreHandler for Component {
    async fn handle_message(msg: NatsMessage) -> Result<(), String> {
        // Replace this with your own processing.
        //
        // Malformed input is treated as handled rather than as an error: core
        // NATS will not redeliver it, so an error here buys a log line and
        // nothing else.
        let Ok(text) = std::str::from_utf8(&msg.body) else {
            return Ok(());
        };

        // Publishing the result to JetStream makes it durable where the
        // incoming message was not. A stream has to capture `done.>` for this
        // to succeed — see the README. Drop it if the work has its own output.
        jetstream::publish(NatsMessage {
            subject: RECEIPT_SUBJECT.to_string(),
            body: format!("chars={}", text.chars().count()).into_bytes(),
            reply_to: None,
            headers: None,
        })
        .await
        .map(|_| ())
        .map_err(|e| format!("receipt publish failed: {e:?}"))
    }
}
