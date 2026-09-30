//! Request/reply — answer NATS requests on a subject.
//!
//! The host owns the subscription (`core-subscriptions` on the binding) and
//! calls this once per request. A request carries the subject its sender is
//! listening on; the answer is an ordinary publish back to it, which is what
//! `_INBOX.>` in `subject-allow` grants.
//!
//! There is no redelivery. If this handler traps, or returns before
//! publishing, the requester waits out its own timeout and gets nothing — so
//! the reply publish is the last thing that happens, after the work.

mod bindings {
    use super::Component;
    wit_bindgen::generate!({ world: "request-reply", generate_all });
    export!(Component);
}

use bindings::exports::wasmcloud::nats::core_handler::Guest as CoreHandler;
use bindings::wasmcloud::nats::core;
use bindings::wasmcloud::nats::types::NatsMessage;

struct Component;

impl CoreHandler for Component {
    async fn handle_message(msg: NatsMessage) -> Result<(), String> {
        let Some(reply_to) = msg.reply_to else {
            // A plain publish landed on the request subject. There is nobody
            // waiting, so there is nothing to answer.
            return Ok(());
        };

        // Replace this with your own processing. Whatever it produces has to
        // end up in `body`: one reply, to the subject the requester named.
        let body = match std::str::from_utf8(&msg.body) {
            Ok(text) => format!("received {} characters", text.chars().count()),
            Err(_) => "received a non-UTF-8 payload".to_string(),
        };

        core::publish(NatsMessage {
            subject: reply_to,
            body: body.into_bytes(),
            // Load-bearing. `subject-allow` has to include the subscription
            // subject, so a caller can set `reply_to: demo.requests` and make
            // this answer itself. Sending no reply subject is what stops that
            // after one bounce — the reply early-returns above. Never widen
            // `subject-allow` to `>`, which would make this an open relay.
            reply_to: None,
            headers: None,
        })
        .await
        .map_err(|e| format!("reply publish failed: {e:?}"))
    }
}
