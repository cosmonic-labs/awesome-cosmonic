// JetStream consumer — delivery with acknowledgement and redelivery.
//
// Unlike core NATS, JetStream paces delivery by acknowledgement: a slow
// consumer is throttled rather than overrun, and an unacknowledged message
// comes back. The handler is called with a handle rather than a bare message,
// carrying the stream sequence and how many times this delivery has been
// attempted.
//
// Acknowledgement follows ack-mode on the binding:
//   - auto, which this template ships: returning Ok acknowledges, and
//     returning Err does not, so the message is redelivered once the
//     consumer's ack-wait elapses.
//   - manual: the handler settles the message itself with Ack, Nak or Term,
//     and a message that returns Ok without settling still times out to
//     redelivery.
//
// Processing is at-least-once, so make it idempotent: redelivery after a
// partial success is ordinary, not exceptional.
//
// GO LIMITATION YOU MUST KNOW: a handler that parks on a timer TRAPS, and a
// trap here is a failed delivery that will be redelivered. Await
// wasi:clocks/monotonic-clock rather than using time.Sleep.
//
// START HERE: HandleMessage is the only thing you need to change.

package export_wasmcloud_nats_jetstream_handler

import (
	"fmt"
	"unicode/utf8"

	witTypes "go.bytecodealliance.org/pkg/wit/types"
	"wit_component/wasmcloud_nats_jetstream"
	"wit_component/wasmcloud_nats_types"
)

// How many attempts a message gets before it is accepted and dropped rather
// than replayed forever. A payload that will never parse is not fixed by
// redelivering it, and a message the consumer keeps retrying blocks progress.
//
// This only covers the parse failure below. The real backstop is max-deliver
// on the binding, which the manifest sets to the same number: it caps
// redelivery for every failure, including one this code cannot anticipate.
const maxDeliveries = 5

func HandleMessage(handle *wasmcloud_nats_jetstream.MessageHandle) witTypes.Result[witTypes.Unit, string] {
	msg := handle.Message()
	sequence := handle.Sequence()

	// Replace this with your own processing, and keep it idempotent: this may
	// be the second or the fifth time this sequence has arrived.
	if !utf8.Valid(msg.Body) {
		if handle.DeliveryCount() >= maxDeliveries {
			// Give up on it. Under ack-mode: auto returning Ok acknowledges,
			// which takes it out of redelivery; under manual this is where
			// handle.Term() belongs.
			return witTypes.Ok[witTypes.Unit, string](witTypes.Unit{})
		}
		return witTypes.Err[witTypes.Unit, string](
			fmt.Sprintf("sequence %d: payload is not valid UTF-8", sequence))
	}
	chars := utf8.RuneCount(msg.Body)

	receipt := wasmcloud_nats_types.NatsMessage{
		Subject: "done.demo.processed",
		Body:    []uint8(fmt.Sprintf("seq=%d;chars=%d", sequence, chars)),
		ReplyTo: witTypes.None[string](),
		// Idempotency, demonstrated rather than just preached: JetStream
		// discards a second message carrying a Nats-Msg-Id it has seen inside
		// the stream's duplicate window. Without it, every redelivery of this
		// sequence would append another receipt.
		Headers: witTypes.Some([]wasmcloud_nats_types.HeaderEntry{{
			Name:  "Nats-Msg-Id",
			Value: fmt.Sprintf("demo-processed-%d", sequence),
		}}),
	}
	if r := wasmcloud_nats_jetstream.Publish(receipt); r.IsErr() {
		return witTypes.Err[witTypes.Unit, string](
			fmt.Sprintf("receipt publish failed: %s", ErrString(r.Err())))
	}
	return witTypes.Ok[witTypes.Unit, string](witTypes.Unit{})
}
