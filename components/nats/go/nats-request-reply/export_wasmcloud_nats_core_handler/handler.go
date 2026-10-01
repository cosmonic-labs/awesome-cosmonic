// Request/reply — answer NATS requests on a subject.
//
// The host owns the subscription (core-subscriptions on the binding) and calls
// this once per request. A request carries the subject its sender is listening
// on; the answer is an ordinary publish back to it, which is what `_INBOX.>`
// in subject-allow grants.
//
// There is no redelivery. If this handler traps, or returns before publishing,
// the requester waits out its own timeout and gets nothing, so the reply
// publish is the last thing that happens, after the work.
//
// GO LIMITATION YOU MUST KNOW: a handler that parks on a timer TRAPS.
// time.Sleep, time.After in a select and context.WithTimeout all fail the
// delivery. Await wasi:clocks/monotonic-clock instead.
//
// START HERE: HandleMessage is the only thing you need to change.

package export_wasmcloud_nats_core_handler

import (
	"fmt"
	"unicode/utf8"

	witTypes "go.bytecodealliance.org/pkg/wit/types"
	"wit_component/wasmcloud_nats_core"
	"wit_component/wasmcloud_nats_types"
)

func HandleMessage(msg wasmcloud_nats_types.NatsMessage) witTypes.Result[witTypes.Unit, string] {
	if msg.ReplyTo.IsNone() {
		// A plain publish landed on the request subject. There is nobody
		// waiting, so there is nothing to answer.
		return witTypes.Ok[witTypes.Unit, string](witTypes.Unit{})
	}
	replyTo := msg.ReplyTo.Some()

	// Replace this with your own processing. Whatever it produces has to end up
	// in body: one reply, to the subject the requester named.
	var body string
	if utf8.Valid(msg.Body) {
		body = fmt.Sprintf("received %d characters", utf8.RuneCount(msg.Body))
	} else {
		body = "received a non-UTF-8 payload"
	}

	reply := wasmcloud_nats_types.NatsMessage{
		Subject: replyTo,
		Body:    []uint8(body),
		// Load-bearing. subject-allow has to include the subscription subject,
		// so a caller can set reply-to to it and make this answer itself.
		// Sending no reply subject is what stops that after one bounce: the
		// reply early-returns above. Never widen subject-allow to `>`, which
		// would make this an open relay.
		ReplyTo: witTypes.None[string](),
		Headers: witTypes.None[[]wasmcloud_nats_types.HeaderEntry](),
	}
	if r := wasmcloud_nats_core.Publish(reply); r.IsErr() {
		return witTypes.Err[witTypes.Unit, string](
			fmt.Sprintf("reply publish failed: %s", ErrString(r.Err())))
	}
	return witTypes.Ok[witTypes.Unit, string](witTypes.Unit{})
}
