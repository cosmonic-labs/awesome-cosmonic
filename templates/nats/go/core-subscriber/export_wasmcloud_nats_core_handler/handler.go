// Core Subscriber — Receive fire-and-forget core NATS messages on a subject and do work per message.
//
// WHEN TO USE THIS
// You have a stream of events on a NATS subject and want a component invoked per message. No acknowledgement, no redelivery, no ordering guarantees — the cheapest possible consumer.
//
// WHEN NOT TO
// Do not use this when losing a message matters. Core NATS has no ack and no redelivery: if the handler traps, or the subscription buffer overflows, the message is gone silently.
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
	"time"

	witTypes "go.bytecodealliance.org/pkg/wit/types"
	"wit_component/wasmcloud_nats_jetstream"
	"wit_component/wasmcloud_nats_types"
)

func HandleMessage(msg wasmcloud_nats_types.NatsMessage) witTypes.Result[witTypes.Unit, string] {
	run := FieldOr(msg.Body, "run", 256, "na")
	holdMs := FieldU64(msg.Body, "hold", 256, 0)
	if holdMs > 0 {
		// The residency experiments need the delivery to stay in flight,
		// holding its admission permit. See NOTES.md: Go's sleep suspends the
		// task rather than the instance, unlike the Rust track's
		// `std::thread::sleep`.
		time.Sleep(time.Duration(holdMs) * time.Millisecond)
	}

	receipt := wasmcloud_nats_types.NatsMessage{
		Subject: fmt.Sprintf("done.core-sink.%s", run),
		Body:    []uint8(fmt.Sprintf("subject=%s;bytes=%d", msg.Subject, len(msg.Body))),
		ReplyTo: witTypes.None[string](),
		Headers: witTypes.None[[]wasmcloud_nats_types.HeaderEntry](),
	}
	if r := wasmcloud_nats_jetstream.Publish(receipt); r.IsErr() {
		return witTypes.Err[witTypes.Unit, string](
			fmt.Sprintf("receipt publish failed: %s", ErrString(r.Err())))
	}
	return witTypes.Ok[witTypes.Unit, string](witTypes.Unit{})
}
