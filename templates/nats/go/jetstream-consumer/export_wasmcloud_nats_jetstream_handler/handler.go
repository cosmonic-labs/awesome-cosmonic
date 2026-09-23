// JetStream Consumer — Durable at-least-once consumption from a JetStream stream, with ack control.
//
// WHEN TO USE THIS
// You need delivery guarantees. JetStream retains messages, redelivers on failure, and — critically — paces delivery by acknowledgement, so a slow consumer is throttled instead of overrun.
//
// WHEN NOT TO
// Do not use it for latency-critical request paths, and do not assume exactly-once. Redelivery is real; handlers must be idempotent.
//
// GO LIMITATION YOU MUST KNOW (measured, reproduces on every toolchain tested)
// A handler that parks on a timer TRAPS. time.Sleep, time.After in a select,
// context.WithTimeout, and any retry/backoff or rate-limit built on them will
// fail the delivery with "async-lifted export failed to produce a result".
// Duration is fine — a 500ms busy-wait and 200M arithmetic iterations both
// complete normally. Only *waiting* on a timer breaks. See docs/limitations.md.
//
// START HERE: the Handle* function below is the only thing you need to change.

package export_wasmcloud_nats_jetstream_handler

import (
	"fmt"
	"time"

	witTypes "go.bytecodealliance.org/pkg/wit/types"
	"wit_component/wasmcloud_nats_jetstream"
	"wit_component/wasmcloud_nats_types"
)

func HandleMessage(handle *wasmcloud_nats_jetstream.MessageHandle) witTypes.Result[witTypes.Unit, string] {
	msg := handle.Message()
	sequence := handle.Sequence()
	delivery := handle.DeliveryCount()

	if _, ok := Field(msg.Body, "trap", 256); ok {
		panic(fmt.Sprintf("injected trap at sequence %d", sequence))
	}
	if n, ok := Field(msg.Body, "fail", 256); ok {
		if v, err := parseU32(n); err == nil && uint64(delivery) <= uint64(v) {
			return witTypes.Err[witTypes.Unit, string](
				fmt.Sprintf("injected failure %d/%d at sequence %d", delivery, v, sequence))
		}
	}
	if hold := FieldU64(msg.Body, "hold", 256, 0); hold > 0 {
		time.Sleep(time.Duration(hold) * time.Millisecond)
	}

	run := FieldOr(msg.Body, "run", 256, "na")
	// Delivery count in the SUBJECT: redelivery becomes countable
	// server-side from the stream's subject map, no body reads needed.
	receipt := wasmcloud_nats_types.NatsMessage{
		Subject: fmt.Sprintf("done.js-sink.%s.d%d", run, delivery),
		Body:    []uint8(fmt.Sprintf("seq=%d;bytes=%d", sequence, len(msg.Body))),
		ReplyTo: witTypes.None[string](),
		Headers: witTypes.None[[]wasmcloud_nats_types.HeaderEntry](),
	}
	if r := wasmcloud_nats_jetstream.Publish(receipt); r.IsErr() {
		return witTypes.Err[witTypes.Unit, string](
			fmt.Sprintf("receipt publish failed: %s", ErrString(r.Err())))
	}

	// Under `ack-mode: auto` the Ok return acks; under `manual` the body
	// must say which settle path to take or the message times out to
	// redelivery after the 30s ack-wait (itself a scenario).
	if _, ok := Field(msg.Body, "macksync", 256); ok {
		if r := handle.AckSync(); r.IsErr() {
			return witTypes.Err[witTypes.Unit, string](
				fmt.Sprintf("ack-sync failed: %s", ErrString(r.Err())))
		}
	} else if _, ok := Field(msg.Body, "mack", 256); ok {
		if r := handle.Ack(); r.IsErr() {
			return witTypes.Err[witTypes.Unit, string](
				fmt.Sprintf("ack failed: %s", ErrString(r.Err())))
		}
	}
	return witTypes.Ok[witTypes.Unit, string](witTypes.Unit{})
}
