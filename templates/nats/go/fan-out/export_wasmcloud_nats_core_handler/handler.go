// Fan-Out / Amplifier — Receive one message and republish it to many — the classic scatter pattern.
//
// WHEN TO USE THIS
// One input event needs to become many units of downstream work: notify N subscribers, shard a job, or trigger a parallel pipeline.
//
// WHEN NOT TO
// Do not use it with core publish at scale without reading the warning below. This is the pattern that produced the campaign's largest data loss.
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
	"wit_component/wasmcloud_nats_core"
	"wit_component/wasmcloud_nats_types"
)

func HandleMessage(msg wasmcloud_nats_types.NatsMessage) witTypes.Result[witTypes.Unit, string] {
	fanout := FieldU64(msg.Body, "fanout", 256, 25)
	for i := uint64(0); i < fanout; i++ {
		r := wasmcloud_nats_core.Publish(wasmcloud_nats_types.NatsMessage{
			Subject: "fan.work",
			Body:    msg.Body,
			ReplyTo: witTypes.None[string](),
			Headers: witTypes.None[[]wasmcloud_nats_types.HeaderEntry](),
		})
		if r.IsErr() {
			return witTypes.Err[witTypes.Unit, string](
				fmt.Sprintf("fan-out publish failed: %s", ErrString(r.Err())))
		}
	}
	return witTypes.Ok[witTypes.Unit, string](witTypes.Unit{})
}
