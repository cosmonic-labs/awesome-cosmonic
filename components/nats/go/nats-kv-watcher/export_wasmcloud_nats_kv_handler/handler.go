// KV watcher — react to every change in a JetStream KV bucket.
//
// The host owns the watch (kv-watches: demo:> on the binding, meaning every
// key in the demo bucket) and calls this once per change. The entry carries
// the key, the new value, its revision, and which operation produced it.
//
// Watches replay. A watcher starting fresh sees the current value of every key
// before it sees anything new, and a redelivery can repeat one. Make the
// reaction idempotent, and compare Revision when order matters: the revision
// is the bucket's own sequence, so a lower one is stale.
//
// GO LIMITATION YOU MUST KNOW: a handler that parks on a timer TRAPS. Await
// wasi:clocks/monotonic-clock rather than using time.Sleep.
//
// START HERE: HandleEvent is the only thing you need to change.

package export_wasmcloud_nats_kv_handler

import (
	"fmt"

	witTypes "go.bytecodealliance.org/pkg/wit/types"
	"wit_component/wasmcloud_nats_jetstream"
	"wit_component/wasmcloud_nats_kv"
	"wit_component/wasmcloud_nats_types"
)

func HandleEvent(bucket string, entry wasmcloud_nats_kv.Entry) witTypes.Result[witTypes.Unit, string] {
	// Replace this with your own processing. Branch on the operation rather
	// than on whether there are bytes: a delete and a purge both arrive with an
	// empty value, and so does a put of an empty value.
	var what string
	switch entry.Operation {
	case wasmcloud_nats_kv.KvOperationPut:
		what = fmt.Sprintf("put bytes=%d", len(entry.Value))
	case wasmcloud_nats_kv.KvOperationDelete:
		what = "delete"
	case wasmcloud_nats_kv.KvOperationPurge:
		what = "purge"
	default:
		what = "unknown"
	}

	receipt := wasmcloud_nats_types.NatsMessage{
		Subject: "done.demo.kv",
		Body: []uint8(fmt.Sprintf("bucket=%s;key=%s;revision=%d;%s",
			bucket, entry.Key, entry.Revision, what)),
		ReplyTo: witTypes.None[string](),
		Headers: witTypes.None[[]wasmcloud_nats_types.HeaderEntry](),
	}
	if r := wasmcloud_nats_jetstream.Publish(receipt); r.IsErr() {
		return witTypes.Err[witTypes.Unit, string](
			fmt.Sprintf("receipt publish failed: %s", ErrString(r.Err())))
	}
	return witTypes.Ok[witTypes.Unit, string](witTypes.Unit{})
}
