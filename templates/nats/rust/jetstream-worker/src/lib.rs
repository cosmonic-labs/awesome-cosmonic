//! JetStream Pull Worker — Guest-paced batch processing — you decide when and how much to fetch.
//!
//! WHEN TO USE THIS
//! You want to control the pace and batch size rather than have the host push at you. Good for expensive per-batch work, rate-limited downstreams, and anything that benefits from amortizing setup across a batch.
//!
//! WHEN NOT TO
//! Do not use plain `fetch(batch)` on a stream with large messages. See the warning below — it is the single most dangerous call in this interface.
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
    world: "jetstream-worker",
    generate_all,
});

use exports::wasmcloud::nats::core_handler::Guest as CoreHandler;
use wasmcloud::nats::jetstream::{self, FetchStop};
use wasmcloud::nats::types::NatsMessage;

struct Component;

fn field(body: &[u8], key: &str) -> Option<String> {
    let head = &body[..body.len().min(512)];
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

async fn receipt(run: &str, body: String) -> Result<(), String> {
    let msg = NatsMessage {
        subject: format!("done.js-pull.{run}"),
        body: body.into_bytes(),
        reply_to: None,
        headers: None,
    };
    jetstream::publish(msg)
        .await
        .map(|_| ())
        .map_err(|e| format!("receipt publish failed: {e:?}"))
}

impl CoreHandler for Component {
    async fn handle_message(msg: NatsMessage) -> Result<(), String> {
        let run = field(&msg.body, "run").unwrap_or_else(|| "na".to_string());
        let Some(stream) = field(&msg.body, "stream") else {
            return Err("trigger missing stream=".to_string());
        };
        let Some(consumer) = field(&msg.body, "consumer") else {
            return Err("trigger missing consumer=".to_string());
        };
        let batch: u32 = field(&msg.body, "batch")
            .and_then(|v| v.parse().ok())
            .unwrap_or(100);
        let max_bytes: u64 = field(&msg.body, "maxbytes")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let timeout_ms: u32 = field(&msg.body, "timeoutms")
            .and_then(|v| v.parse().ok())
            .unwrap_or(5000);
        let rounds: u32 = field(&msg.body, "rounds")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        let info_every: u32 = field(&msg.body, "infoevery")
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);

        let puller = match jetstream::open_pull_consumer(stream.clone(), consumer.clone()).await {
            Ok(p) => p,
            Err(e) => {
                receipt(&run, format!("error=open;detail={e:?}")).await?;
                return Err(format!("open-pull-consumer failed: {e:?}"));
            }
        };

        let mut total: u64 = 0;
        for round in 0..rounds {
            let fetched = if max_bytes > 0 {
                puller.fetch_with_limits(batch, max_bytes, timeout_ms).await
            } else {
                puller.fetch(batch, timeout_ms).await
            };
            let batch_result = match fetched {
                Ok(b) => b,
                Err(e) => {
                    receipt(&run, format!("round={round};error=fetch;detail={e:?}")).await?;
                    break;
                }
            };
            let got = batch_result.messages.len() as u64;
            total += got;
            for handle in &batch_result.messages {
                handle
                    .ack()
                    .await
                    .map_err(|e| format!("pull ack failed: {e:?}"))?;
            }
            let stop = match batch_result.stop {
                FetchStop::BatchFilled => "batch-filled",
                FetchStop::Drained => "drained",
                FetchStop::ByteLimit => "byte-limit",
            };
            receipt(&run, format!("round={round};fetched={got};stop={stop}")).await?;

            if info_every > 0 && round % info_every == 0 {
                if let Err(e) = puller.info().await {
                    receipt(&run, format!("round={round};error=info;detail={e:?}")).await?;
                }
            }
            if matches!(batch_result.stop, FetchStop::Drained) && got == 0 {
                break;
            }
        }
        receipt(&run, format!("total={total}")).await
    }
}

export!(Component);
