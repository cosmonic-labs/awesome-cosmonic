//! Fan-out — one event in, several downstream subjects out.
//!
//! The host delivers each message on `demo.events` and this republishes it to
//! every subject in `TARGETS`. The fan is fixed by the component and bounded
//! by the binding's `subject-allow` grant, deliberately rather than read out
//! of the message: a fan width chosen by whoever can publish to the subject is
//! an amplification attack with extra steps.
//!
//! A partial failure is visible but not repairable — the publishes that
//! already landed stay landed, and core NATS will not redeliver the trigger.
//! Make each downstream consumer idempotent instead of expecting all-or-none.

mod bindings {
    use super::Component;
    wit_bindgen::generate!({ world: "fan-out", generate_all });
    export!(Component);
}

use bindings::exports::wasmcloud::nats::core_handler::Guest as CoreHandler;
use bindings::wasmcloud::nats::core;
use bindings::wasmcloud::nats::types::NatsMessage;

struct Component;

/// The subjects each event is copied to. Every one must be covered by
/// `subject-allow` on the binding, or the publish is denied at the host.
const TARGETS: [&str; 3] = [
    "demo.events.audit",
    "demo.events.index",
    "demo.events.notify",
];

impl CoreHandler for Component {
    async fn handle_message(msg: NatsMessage) -> Result<(), String> {
        for target in TARGETS {
            // Replace this with your own processing — filtering per target,
            // reshaping the payload, or dropping one branch entirely.
            core::publish(NatsMessage {
                subject: target.to_string(),
                body: msg.body.clone(),
                reply_to: None,
                headers: None,
            })
            .await
            .map_err(|e| format!("fan-out publish to {target} failed: {e:?}"))?;
        }
        Ok(())
    }
}
