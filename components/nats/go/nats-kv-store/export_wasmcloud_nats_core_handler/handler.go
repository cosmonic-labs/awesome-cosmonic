// KV store — write records from a subject into a JetStream KV bucket.
//
// Each message on demo.records.<key> becomes one key in the bucket: the part
// of the subject after the prefix is the key, and the body is the value. A KV
// bucket is a JetStream stream underneath, so a write is durable and every key
// keeps a revision history.
//
// bucket-allow on the binding is what grants access to the bucket, and it is
// separate from subject-allow on purpose: being able to publish to a subject
// does not grant reading or writing the bucket that captures it.
//
// GO LIMITATION YOU MUST KNOW: a handler that parks on a timer TRAPS. Await
// wasi:clocks/monotonic-clock rather than using time.Sleep.
//
// START HERE: HandleMessage is the only thing you need to change.

package export_wasmcloud_nats_core_handler

import (
	"fmt"
	"strings"

	witTypes "go.bytecodealliance.org/pkg/wit/types"
	"wit_component/wasmcloud_nats_jetstream"
	"wit_component/wasmcloud_nats_kv"
	"wit_component/wasmcloud_nats_types"
)

const (
	// The bucket this component writes to. It has to exist already, since a
	// bucket is created by an operator and not by the guest, and bucket-allow
	// must name it.
	bucketName = "demo"

	// Subject prefix stripped to form the key: demo.records.order-1 -> order-1.
	subjectPrefix = "demo.records."
)

func HandleMessage(msg wasmcloud_nats_types.NatsMessage) witTypes.Result[witTypes.Unit, string] {
	ok := witTypes.Ok[witTypes.Unit, string](witTypes.Unit{})

	// A subject with no key suffix, or one whose key would contain the
	// separator, has nothing to store. Core NATS will not redeliver, so accept
	// it rather than returning an error nobody acts on.
	if !strings.HasPrefix(msg.Subject, subjectPrefix) {
		return ok
	}
	key := strings.TrimPrefix(msg.Subject, subjectPrefix)
	if key == "" || strings.Contains(key, ".") {
		return ok
	}

	opened := wasmcloud_nats_kv.Open(bucketName)
	if opened.IsErr() {
		return witTypes.Err[witTypes.Unit, string](
			fmt.Sprintf("open bucket %s failed: %s", bucketName, ErrString(opened.Err())))
	}
	bucket := opened.Ok()
	defer bucket.Drop()

	// Replace this with your own processing. Put overwrites blindly; Create
	// fails if the key already exists, and Update takes the revision you read,
	// which is how you get compare-and-swap.
	put := bucket.Put(key, msg.Body)
	if put.IsErr() {
		return witTypes.Err[witTypes.Unit, string](
			fmt.Sprintf("put %s failed: %s", key, ErrString(put.Err())))
	}

	receipt := wasmcloud_nats_types.NatsMessage{
		Subject: "done.demo.records",
		Body:    []uint8(fmt.Sprintf("key=%s;revision=%d", key, put.Ok())),
		ReplyTo: witTypes.None[string](),
		Headers: witTypes.None[[]wasmcloud_nats_types.HeaderEntry](),
	}
	if r := wasmcloud_nats_jetstream.Publish(receipt); r.IsErr() {
		return witTypes.Err[witTypes.Unit, string](
			fmt.Sprintf("receipt publish failed: %s", ErrString(r.Err())))
	}
	return ok
}
