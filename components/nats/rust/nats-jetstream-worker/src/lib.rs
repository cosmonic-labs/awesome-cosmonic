//! JetStream pull worker — the guest sets the pace.
//!
//! A core NATS message on `demo.worker.run` triggers a drain: the component
//! opens the pull consumer named below, fetches batches until the stream is
//! caught up or `MAX_ROUNDS` is reached, and acknowledges each message.
//!
//! Pull is the right shape only when the guest needs to control the rate. A
//! push consumer (`nats-jetstream-consumer`) is simpler, and is what you want
//! unless you can say why you need this one.
//!
//! The stream, consumer and bounds are constants rather than fields read out
//! of the trigger message. A drain loop whose target and size are chosen by
//! whoever can publish to the trigger subject is a denial of service with
//! extra steps — the same reason the fan-out template fixes its targets.

mod bindings {
    use super::Component;
    wit_bindgen::generate!({ world: "jetstream-worker", generate_all });
    export!(Component);
}

use bindings::exports::wasmcloud::nats::core_handler::Guest as CoreHandler;
use bindings::wasmcloud::nats::jetstream::{self, FetchStop};
use bindings::wasmcloud::nats::types::NatsError;
use bindings::wasmcloud::nats::types::NatsMessage;

struct Component;

/// The stream and durable consumer this worker drains. `stream-allow` on the
/// binding has to cover the stream, or opening the consumer is denied.
const STREAM: &str = "DEMO";
const CONSUMER: &str = "demo-worker";

/// Messages per fetch, and how long the host waits for a batch to fill.
///
/// A fetch materialises `BATCH × message size` in host memory, so 100 is only
/// right for small payloads: at 1 MB messages it asks for 100 MB and the host
/// refuses with `limit-exceeded`. Drop it to single digits as payloads grow.
const BATCH: u32 = 100;
const FETCH_TIMEOUT_MS: u32 = 5_000;

/// A ceiling on one drain, so a single trigger cannot start unbounded work.
/// With `BATCH` that is 10,000 messages, which against a live feed can run for
/// minutes on a `poolSize: 1` component. Size both to your own throughput.
const MAX_ROUNDS: u32 = 100;

impl CoreHandler for Component {
    async fn handle_message(_trigger: NatsMessage) -> Result<(), String> {
        let puller = jetstream::open_pull_consumer(STREAM.to_string(), CONSUMER.to_string())
            .await
            .map_err(|e| format!("open pull consumer {STREAM}/{CONSUMER} failed: {e:?}"))?;

        let mut acked: u64 = 0;
        for _ in 0..MAX_ROUNDS {
            let batch = match puller.fetch(BATCH, FETCH_TIMEOUT_MS).await {
                Ok(batch) => batch,
                // The consumer had nothing within the timeout. That is how a
                // drain ends, not a failure: `fetch` reports an empty result
                // as `no-messages` rather than as an empty batch, so treating
                // it as an error would fail every successful drain.
                Err(NatsError::NoMessages) => break,
                Err(e) => return Err(format!("fetch failed: {e:?}")),
            };

            let fetched = batch.messages.len();
            // Consume the handles by value so each one drops at the end of its
            // iteration. An un-dropped handle holds its share of the
            // subscription byte budget, and fetch stalls silently once that
            // budget is exhausted — so this is load-bearing, not tidiness.
            for handle in batch.messages {
                // Replace this with your own processing. Acknowledge after the
                // work, not before: an ack is a promise it is done.
                handle
                    .ack()
                    .await
                    .map_err(|e| format!("ack failed: {e:?}"))?;
                acked += 1;
            }

            // Drained with nothing in hand means the consumer is caught up.
            if matches!(batch.stop, FetchStop::Drained) && fetched == 0 {
                break;
            }
        }

        jetstream::publish(NatsMessage {
            subject: "done.demo.worker".to_string(),
            body: format!("acked={acked}").into_bytes(),
            reply_to: None,
            headers: None,
        })
        .await
        .map(|_| ())
        .map_err(|e| format!("receipt publish failed: {e:?}"))
    }
}
