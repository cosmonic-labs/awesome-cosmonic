//! JetStream consumer — delivery with acknowledgement and redelivery.
//!
//! Unlike core NATS, JetStream paces delivery by acknowledgement: a slow
//! consumer is throttled rather than overrun, and an unacknowledged message
//! comes back. The handler is called with a handle rather than a bare message,
//! carrying the stream sequence and how many times this delivery has been
//! attempted.
//!
//! Acknowledgement follows `ack-mode` on the binding:
//!   - `auto`, which this template ships: returning `Ok` acknowledges, and
//!     returning `Err` does not, so the message is redelivered once the
//!     consumer's ack-wait elapses.
//!   - `manual`: the handler settles the message itself with `ack`, `nak` or
//!     `term`, and a message that returns `Ok` without settling still times
//!     out to redelivery.
//!
//! Processing is at-least-once, so make it idempotent: redelivery after a
//! partial success is ordinary, not exceptional.

mod bindings {
    use super::Component;
    wit_bindgen::generate!({ world: "jetstream-consumer", generate_all });
    export!(Component);
}

use bindings::exports::wasmcloud::nats::jetstream_handler::Guest as JetstreamHandler;
use bindings::wasmcloud::nats::jetstream::{self, MessageHandle};
use bindings::wasmcloud::nats::types::{HeaderEntry, NatsMessage};

struct Component;

/// How many attempts a message gets before it is accepted and dropped rather
/// than replayed forever. A payload that will never parse is not fixed by
/// redelivering it, and a message the consumer keeps retrying blocks progress.
///
/// This only covers the parse failure below. The real backstop is
/// `max-deliver` on the binding, which the manifest sets to the same number:
/// it caps redelivery for every failure, including one this code cannot
/// anticipate — a receipt publish that keeps failing because the stream is
/// missing or the grant is too narrow would otherwise retry forever.
const MAX_DELIVERIES: u32 = 5;

impl JetstreamHandler for Component {
    async fn handle_message(handle: MessageHandle) -> Result<(), String> {
        let msg = handle.message();
        let sequence = handle.sequence();

        // Replace this with your own processing, and keep it idempotent: this
        // may be the second or the fifth time this sequence has arrived.
        let chars = match std::str::from_utf8(&msg.body) {
            Ok(text) => text.chars().count(),
            Err(_) => {
                if handle.delivery_count() >= MAX_DELIVERIES {
                    // Give up on it. Under `ack-mode: auto` returning `Ok`
                    // acknowledges, which takes it out of redelivery; under
                    // `manual` this is where `handle.term()` belongs.
                    return Ok(());
                }
                return Err(format!("sequence {sequence}: payload is not valid UTF-8"));
            }
        };

        jetstream::publish(NatsMessage {
            subject: "done.demo.processed".to_string(),
            body: format!("seq={sequence};chars={chars}").into_bytes(),
            reply_to: None,
            // Idempotency, demonstrated rather than just preached: JetStream
            // discards a second message carrying a `Nats-Msg-Id` it has seen
            // inside the stream's duplicate window. Without it, every
            // redelivery of this sequence would append another receipt.
            headers: Some(vec![HeaderEntry {
                name: "Nats-Msg-Id".to_string(),
                value: format!("demo-processed-{sequence}"),
            }]),
        })
        .await
        .map(|_| ())
        .map_err(|e| format!("receipt publish failed: {e:?}"))
    }
}
