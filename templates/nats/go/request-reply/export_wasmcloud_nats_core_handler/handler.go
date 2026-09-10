// Request / Reply — Answer NATS requests — an RPC endpoint that scales to zero between calls.
//
// WHEN TO USE THIS
// You want a service other components or clients call and wait on. The host delivers the request, you publish the answer to the requester's reply subject. Per-request instantiation means it costs nothing when idle.
//
// WHEN NOT TO
// Do not use it for work longer than the caller's timeout, and do not use it for fire-and-forget notifications — a reply nobody awaits is wasted work.
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
	"wit_component/wasmcloud_nats_core"
	"wit_component/wasmcloud_nats_types"
)

func HandleMessage(msg wasmcloud_nats_types.NatsMessage) witTypes.Result[witTypes.Unit, string] {
	if msg.ReplyTo.IsNone() {
		// Delivered without a reply subject — a plain publish landed on
		// the request subject. Nothing to answer.
		return witTypes.Ok[witTypes.Unit, string](witTypes.Unit{})
	}
	replyTo := msg.ReplyTo.Some()
	if hold := FieldU64(msg.Body, "hold", 256, 0); hold > 0 {
		time.Sleep(time.Duration(hold) * time.Millisecond)
	}
	reply := wasmcloud_nats_types.NatsMessage{
		Subject: replyTo,
		Body:    []uint8(fmt.Sprintf("echo:%d", len(msg.Body))),
		ReplyTo: witTypes.None[string](),
		Headers: witTypes.None[[]wasmcloud_nats_types.HeaderEntry](),
	}
	if r := wasmcloud_nats_core.Publish(reply); r.IsErr() {
		return witTypes.Err[witTypes.Unit, string](
			fmt.Sprintf("reply publish failed: %s", ErrString(r.Err())))
	}
	return witTypes.Ok[witTypes.Unit, string](witTypes.Unit{})
}
