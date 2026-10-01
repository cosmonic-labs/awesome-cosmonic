// Core subscriber — the component runs once per message on a NATS subject.
//
// Core NATS is fire-and-forget. There is no acknowledgement, no redelivery and
// no ordering: if this handler traps, or the host's subscription buffer
// overflows, that message is gone. Returning an error is recorded in the
// host's logs but changes nothing for the sender. Reach for
// nats-jetstream-consumer when losing a message is not acceptable.
//
// GO LIMITATION YOU MUST KNOW (measured, reproduces on every toolchain tested)
// A handler that parks on a timer TRAPS. time.Sleep, time.After in a select,
// context.WithTimeout, and any retry, backoff or rate limit built on them will
// fail the delivery with "async-lifted export failed to produce a result".
// Duration is fine: a 500ms busy-wait completes normally. Only *waiting* on a
// timer breaks. Await wasi:clocks/monotonic-clock instead.
//
// START HERE: HandleMessage is the only thing you need to change.

package export_wasmcloud_nats_core_handler

import (
	"fmt"
	"unicode/utf8"

	witTypes "go.bytecodealliance.org/pkg/wit/types"
	"wit_component/wasmcloud_nats_jetstream"
	"wit_component/wasmcloud_nats_types"
)

// Where results go. A constant rather than something derived from the incoming
// message: the rest of this set fixes its publish targets in code so a sender
// cannot steer them, and this is no different.
const receiptSubject = "done.demo.events"

func HandleMessage(msg wasmcloud_nats_types.NatsMessage) witTypes.Result[witTypes.Unit, string] {
	// Replace this with your own processing.
	//
	// Malformed input is treated as handled rather than as an error: core NATS
	// will not redeliver it, so an error here buys a log line and nothing else.
	if !utf8.Valid(msg.Body) {
		return witTypes.Ok[witTypes.Unit, string](witTypes.Unit{})
	}
	chars := utf8.RuneCount(msg.Body)

	// Publishing the result to JetStream makes it durable where the incoming
	// message was not. A stream has to capture `done.>` for this to succeed —
	// see the README. Drop it if the work has its own output.
	receipt := wasmcloud_nats_types.NatsMessage{
		Subject: receiptSubject,
		Body:    []uint8(fmt.Sprintf("chars=%d", chars)),
		ReplyTo: witTypes.None[string](),
		Headers: witTypes.None[[]wasmcloud_nats_types.HeaderEntry](),
	}
	if r := wasmcloud_nats_jetstream.Publish(receipt); r.IsErr() {
		return witTypes.Err[witTypes.Unit, string](
			fmt.Sprintf("receipt publish failed: %s", ErrString(r.Err())))
	}
	return witTypes.Ok[witTypes.Unit, string](witTypes.Unit{})
}
