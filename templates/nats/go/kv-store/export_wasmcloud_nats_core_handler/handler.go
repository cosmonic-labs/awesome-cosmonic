// KV Store Client — Read and write a NATS KV bucket — get, put, CAS update, delete, history.
//
// WHEN TO USE THIS
// You need durable key/value state that outlives an instance. NATS KV gives you revisions (so compare-and-swap works), history, and a watch channel other components can subscribe to.
//
// WHEN NOT TO
// Do not treat it as a database. Listings are capped host-side, and there are no queries — only key lookups and prefix watches.
//
// GO LIMITATION YOU MUST KNOW (measured, reproduces on every toolchain tested)
// A handler that parks on a timer TRAPS. time.Sleep, time.After in a select,
// context.WithTimeout, and any retry/backoff or rate-limit built on them will
// fail the delivery with "async-lifted export failed to produce a result".
// Duration is fine — a 500ms busy-wait and 200M arithmetic iterations both
// complete normally. Only *waiting* on a timer breaks. See docs/limitations.md.
//
// START HERE: the Handle* function below is the only thing you need to change.

package export_wasmcloud_nats_core_handler

import (
	"fmt"

	witTypes "go.bytecodealliance.org/pkg/wit/types"
	"wit_component/wasmcloud_nats_jetstream"
	"wit_component/wasmcloud_nats_kv"
	"wit_component/wasmcloud_nats_types"
)

func makeValue(run string, size int) []uint8 {
	v := []uint8(fmt.Sprintf("run=%s;pad=", run))
	for len(v) < size {
		v = append(v, 'x')
	}
	return v
}

func HandleMessage(msg wasmcloud_nats_types.NatsMessage) witTypes.Result[witTypes.Unit, string] {
	run := FieldOr(msg.Body, "run", 512, "na")
	bucketName, ok := Field(msg.Body, "bucket", 512)
	if !ok {
		return witTypes.Err[witTypes.Unit, string]("trigger missing bucket=")
	}
	op := FieldOr(msg.Body, "op", 512, "put")
	ops := FieldU64(msg.Body, "ops", 512, 100)
	size := int(FieldU64(msg.Body, "size", 512, 128))
	prefix := FieldOr(msg.Body, "prefix", 512, "k")

	opened := wasmcloud_nats_kv.Open(bucketName)
	if opened.IsErr() {
		return witTypes.Err[witTypes.Unit, string](
			fmt.Sprintf("open bucket %s failed: %s", bucketName, ErrString(opened.Err())))
	}
	bucket := opened.Ok()

	var okCount, errCount uint64
	firstErr := ""
	record := func(e string) {
		if e == "" {
			okCount++
			return
		}
		if firstErr == "" {
			firstErr = e
		}
		errCount++
	}

	for i := uint64(0); i < ops; i++ {
		key := fmt.Sprintf("%s-%d", prefix, i)
		var e string
		switch op {
		case "put":
			if r := bucket.Put(key, makeValue(run, size)); r.IsErr() {
				e = ErrString(r.Err())
			}
		case "get":
			if r := bucket.Get(key); r.IsErr() {
				e = ErrString(r.Err())
			}
		case "cas":
			if got := bucket.Get(key); got.IsErr() {
				e = fmt.Sprintf("cas read: %s", ErrString(got.Err()))
			} else if r := bucket.Update(key, makeValue(run, size), got.Ok().Revision); r.IsErr() {
				e = ErrString(r.Err())
			}
		case "del":
			if r := bucket.Delete(key); r.IsErr() {
				e = ErrString(r.Err())
			}
		case "purge":
			if r := bucket.Purge(key); r.IsErr() {
				e = ErrString(r.Err())
			}
		// D7 probe: history on a key with no history hangs the guest
		// call on the QA-anchored build — run this op with a small
		// `ops` and a harness-side timeout.
		case "history":
			if r := bucket.History(key); r.IsErr() {
				e = ErrString(r.Err())
			}
		case "keys":
			// `Keys` takes a subject-pattern filter over the key space; `>`
			// is every key. The listing is capped host-side at 1000, and the
			// page's `truncated` flag distinguishes a partial page from a
			// complete one — narrow the filter to walk a larger bucket.
			if r := bucket.Keys(">"); r.IsErr() {
				e = ErrString(r.Err())
			}
		case "status":
			if r := bucket.Status(); r.IsErr() {
				e = ErrString(r.Err())
			}
		default:
			e = fmt.Sprintf("unknown op %s", op)
		}
		record(e)
	}

	body := fmt.Sprintf("op=%s;ok=%d;err=%d", op, okCount, errCount)
	if firstErr != "" {
		if len(firstErr) > 300 {
			firstErr = firstErr[:300]
		}
		body += fmt.Sprintf(";first_err=%s", firstErr)
	}
	receipt := wasmcloud_nats_types.NatsMessage{
		Subject: fmt.Sprintf("done.kv-worker.%s", run),
		Body:    []uint8(body),
		ReplyTo: witTypes.None[string](),
		Headers: witTypes.None[[]wasmcloud_nats_types.HeaderEntry](),
	}
	if r := wasmcloud_nats_jetstream.Publish(receipt); r.IsErr() {
		return witTypes.Err[witTypes.Unit, string](
			fmt.Sprintf("receipt publish failed: %s", ErrString(r.Err())))
	}
	return witTypes.Ok[witTypes.Unit, string](witTypes.Unit{})
}
