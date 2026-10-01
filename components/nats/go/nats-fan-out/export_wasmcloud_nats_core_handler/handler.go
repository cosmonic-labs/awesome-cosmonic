// Fan-out — one event in, several downstream subjects out.
//
// The host delivers each message on demo.events and this republishes it to
// every subject in targets. The fan is fixed by the component and bounded by
// the binding's subject-allow grant, deliberately rather than read out of the
// message: a fan width chosen by whoever can publish to the subject is an
// amplification attack with extra steps.
//
// A partial failure is visible but not repairable: the publishes that already
// landed stay landed, and core NATS will not redeliver the trigger. Make each
// downstream consumer idempotent instead of expecting all-or-none.
//
// GO LIMITATION YOU MUST KNOW: a handler that parks on a timer TRAPS.
// time.Sleep and friends fail the delivery; await
// wasi:clocks/monotonic-clock instead.
//
// START HERE: HandleMessage is the only thing you need to change.

package export_wasmcloud_nats_core_handler

import (
	"fmt"

	witTypes "go.bytecodealliance.org/pkg/wit/types"
	"wit_component/wasmcloud_nats_core"
	"wit_component/wasmcloud_nats_types"
)

// The subjects each event is copied to. Every one must be covered by
// subject-allow on the binding, or the publish is denied at the host.
var targets = [...]string{
	"demo.events.audit",
	"demo.events.index",
	"demo.events.notify",
}

func HandleMessage(msg wasmcloud_nats_types.NatsMessage) witTypes.Result[witTypes.Unit, string] {
	for _, target := range targets {
		// Replace this with your own processing: filtering per target,
		// reshaping the payload, or dropping one branch entirely.
		out := wasmcloud_nats_types.NatsMessage{
			Subject: target,
			Body:    msg.Body,
			ReplyTo: witTypes.None[string](),
			Headers: witTypes.None[[]wasmcloud_nats_types.HeaderEntry](),
		}
		if r := wasmcloud_nats_core.Publish(out); r.IsErr() {
			return witTypes.Err[witTypes.Unit, string](
				fmt.Sprintf("fan-out publish to %s failed: %s", target, ErrString(r.Err())))
		}
	}
	return witTypes.Ok[witTypes.Unit, string](witTypes.Unit{})
}
