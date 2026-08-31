// KV Watcher — React to changes in a NATS KV bucket — put, delete, and purge events.
//
// WHEN TO USE THIS
// You want a component invoked whenever a key changes — cache invalidation, config reload, projection updates, change-data-capture. The host maintains the watch; you just handle events.
//
// WHEN NOT TO
// Do not use it as a work queue. Watch delivery follows KV semantics, not queue semantics, and a purge or a history-trimmed key can collapse several logical changes into one event.
//
// GO LIMITATION YOU MUST KNOW (measured, reproduces on every toolchain tested)
// A handler that parks on a timer TRAPS. time.Sleep, time.After in a select,
// context.WithTimeout, and any retry/backoff or rate-limit built on them will
// fail the delivery with "async-lifted export failed to produce a result".
// Duration is fine — a 500ms busy-wait and 200M arithmetic iterations both
// complete normally. Only *waiting* on a timer breaks. See docs/limitations.md.
//
// START HERE: the Handle* function below is the only thing you need to change.

package export_wasmcloud_nats_kv_handler

import (
	"fmt"

	witTypes "go.bytecodealliance.org/pkg/wit/types"
	"wit_component/wasmcloud_nats_jetstream"
	"wit_component/wasmcloud_nats_kv"
	"wit_component/wasmcloud_nats_types"
)

func HandleEvent(bucket string, entry wasmcloud_nats_kv.Entry) witTypes.Result[witTypes.Unit, string] {
	run := FieldOr(entry.Value, "run", 256, "na")
	receipt := wasmcloud_nats_types.NatsMessage{
		Subject: fmt.Sprintf("done.kv-watch.%s", run),
		Body: []uint8(fmt.Sprintf("bucket=%s;key=%s;op=%s;rev=%d",
			bucket, entry.Key, kvOperation(entry.Operation), entry.Revision)),
		ReplyTo: witTypes.None[string](),
		Headers: witTypes.None[[]wasmcloud_nats_types.HeaderEntry](),
	}
	if r := wasmcloud_nats_jetstream.Publish(receipt); r.IsErr() {
		return witTypes.Err[witTypes.Unit, string](
			fmt.Sprintf("receipt publish failed: %s", ErrString(r.Err())))
	}
	return witTypes.Ok[witTypes.Unit, string](witTypes.Unit{})
}

// Matches Rust's `{:?}` rendering of the `operation` enum.
func kvOperation(op wasmcloud_nats_kv.KvOperation) string {
	switch op {
	case wasmcloud_nats_kv.KvOperationPut:
		return "Put"
	case wasmcloud_nats_kv.KvOperationDelete:
		return "Delete"
	default:
		return "Purge"
	}
}
