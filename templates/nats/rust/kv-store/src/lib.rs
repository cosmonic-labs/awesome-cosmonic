//! KV Store Client — Read and write a NATS KV bucket — get, put, CAS update, delete, history.
//!
//! WHEN TO USE THIS
//! You need durable key/value state that outlives an instance. NATS KV gives you revisions (so compare-and-swap works), history, and a watch channel other components can subscribe to.
//!
//! WHEN NOT TO
//! Do not treat it as a database. Listings are capped host-side, and there are no queries — only key lookups and prefix watches.
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
    world: "kv-store",
    generate_all,
});

use exports::wasmcloud::nats::core_handler::Guest as CoreHandler;
use wasmcloud::nats::jetstream;
use wasmcloud::nats::kv;
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

fn make_value(run: &str, size: usize) -> Vec<u8> {
    let mut v = format!("run={run};pad=").into_bytes();
    v.resize(v.len().max(size), b'x');
    v
}

impl CoreHandler for Component {
    async fn handle_message(msg: NatsMessage) -> Result<(), String> {
        let run = field(&msg.body, "run").unwrap_or_else(|| "na".to_string());
        let Some(bucket_name) = field(&msg.body, "bucket") else {
            return Err("trigger missing bucket=".to_string());
        };
        let op = field(&msg.body, "op").unwrap_or_else(|| "put".to_string());
        let ops: u64 = field(&msg.body, "ops")
            .and_then(|v| v.parse().ok())
            .unwrap_or(100);
        let size: usize = field(&msg.body, "size")
            .and_then(|v| v.parse().ok())
            .unwrap_or(128);
        let prefix = field(&msg.body, "prefix").unwrap_or_else(|| "k".to_string());

        let bucket = kv::open(bucket_name.clone())
            .await
            .map_err(|e| format!("open bucket {bucket_name} failed: {e:?}"))?;

        let mut ok: u64 = 0;
        let mut err: u64 = 0;
        let mut first_err = String::new();
        let mut record = |r: Result<(), String>| {
            match r {
                Ok(()) => ok += 1,
                Err(e) => {
                    if first_err.is_empty() {
                        first_err = e;
                    }
                    err += 1;
                }
            };
        };

        for i in 0..ops {
            let key = format!("{prefix}-{i}");
            let r: Result<(), String> = match op.as_str() {
                "put" => bucket
                    .put(key, make_value(&run, size))
                    .await
                    .map(|_| ())
                    .map_err(|e| format!("{e:?}")),
                "get" => bucket.get(key).await.map(|_| ()).map_err(|e| format!("{e:?}")),
                "cas" => match bucket.get(key.clone()).await {
                    Ok(entry) => bucket
                        .update(key, make_value(&run, size), entry.revision)
                        .await
                        .map(|_| ())
                        .map_err(|e| format!("{e:?}")),
                    Err(e) => Err(format!("cas read: {e:?}")),
                },
                "del" => bucket.delete(key).await.map_err(|e| format!("{e:?}")),
                "purge" => bucket.purge(key).await.map_err(|e| format!("{e:?}")),
                // D7 probe: history on a key with no history hangs the guest
                // call on the QA-anchored build — run this op with a small
                // `ops` and a harness-side timeout.
                "history" => bucket.history(key).await.map(|_| ()).map_err(|e| format!("{e:?}")),
                // `keys` takes a subject-pattern filter over the key space;
                // `>` is every key. The listing is capped host-side at 1000,
                // and `truncated` distinguishes a partial page from a complete
                // one — narrow the filter to walk a larger bucket.
                "keys" => bucket.keys(">".to_string()).await.map(|_| ()).map_err(|e| format!("{e:?}")),
                "status" => bucket.status().await.map(|_| ()).map_err(|e| format!("{e:?}")),
                other => Err(format!("unknown op {other}")),
            };
            record(r);
        }

        let mut body = format!("op={op};ok={ok};err={err}");
        if !first_err.is_empty() {
            first_err.truncate(300);
            body.push_str(&format!(";first_err={first_err}"));
        }
        let receipt = NatsMessage {
            subject: format!("done.kv-worker.{run}"),
            body: body.into_bytes(),
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
