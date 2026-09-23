// JetStream Pull Worker — Guest-paced batch processing — you decide when and how much to fetch.
//
// WHEN TO USE THIS
// You want to control the pace and batch size rather than have the host push at you. Good for expensive per-batch work, rate-limited downstreams, and anything that benefits from amortizing setup across a batch.
//
// WHEN NOT TO
// Do not use plain `fetch(batch)` on a stream with large messages. See the warning below — it is the single most dangerous call in this interface.
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
	"wit_component/wasmcloud_nats_types"
)

func receipt(run string, body string) witTypes.Result[witTypes.Unit, string] {
	msg := wasmcloud_nats_types.NatsMessage{
		Subject: fmt.Sprintf("done.js-pull.%s", run),
		Body:    []uint8(body),
		ReplyTo: witTypes.None[string](),
		Headers: witTypes.None[[]wasmcloud_nats_types.HeaderEntry](),
	}
	if r := wasmcloud_nats_jetstream.Publish(msg); r.IsErr() {
		return witTypes.Err[witTypes.Unit, string](
			fmt.Sprintf("receipt publish failed: %s", ErrString(r.Err())))
	}
	return witTypes.Ok[witTypes.Unit, string](witTypes.Unit{})
}

func HandleMessage(msg wasmcloud_nats_types.NatsMessage) witTypes.Result[witTypes.Unit, string] {
	run := FieldOr(msg.Body, "run", 512, "na")
	stream, ok := Field(msg.Body, "stream", 512)
	if !ok {
		return witTypes.Err[witTypes.Unit, string]("trigger missing stream=")
	}
	consumer, ok := Field(msg.Body, "consumer", 512)
	if !ok {
		return witTypes.Err[witTypes.Unit, string]("trigger missing consumer=")
	}
	batch := FieldU32(msg.Body, "batch", 512, 100)
	maxBytes := FieldU64(msg.Body, "maxbytes", 512, 0)
	timeoutMs := FieldU32(msg.Body, "timeoutms", 512, 5000)
	rounds := FieldU32(msg.Body, "rounds", 512, 1000)
	infoEvery := FieldU32(msg.Body, "infoevery", 512, 10)

	opened := wasmcloud_nats_jetstream.OpenPullConsumer(stream, consumer)
	if opened.IsErr() {
		detail := ErrString(opened.Err())
		if r := receipt(run, fmt.Sprintf("error=open;detail=%s", detail)); r.IsErr() {
			return r
		}
		return witTypes.Err[witTypes.Unit, string](
			fmt.Sprintf("open-pull-consumer failed: %s", detail))
	}
	puller := opened.Ok()
	// Release the consumer resource when the handler ends; without it every
	// trigger leaks a host-side pull-consumer handle.
	defer puller.Drop()

	var total uint64
	for round := uint32(0); round < rounds; round++ {
		var fetched witTypes.Result[wasmcloud_nats_jetstream.FetchedBatch, wasmcloud_nats_types.NatsError]
		if maxBytes > 0 {
			fetched = puller.FetchWithLimits(batch, maxBytes, timeoutMs)
		} else {
			fetched = puller.Fetch(batch, timeoutMs)
		}
		if fetched.IsErr() {
			if r := receipt(run, fmt.Sprintf("round=%d;error=fetch;detail=%s",
				round, ErrString(fetched.Err()))); r.IsErr() {
				return r
			}
			break
		}
		batchResult := fetched.Ok()
		got := uint64(len(batchResult.Messages))
		total += got
		for _, handle := range batchResult.Messages {
			r := handle.Ack()
			// Acking does not release the handle: fetched bytes stay pinned
			// against the binding's subscription-capacity-bytes until the
			// message-handle is dropped (see the nats-error limit-exceeded docs).
			// Un-dropped handles exhaust the budget and fetch silently stalls.
			handle.Drop()
			if r.IsErr() {
				return witTypes.Err[witTypes.Unit, string](
					fmt.Sprintf("pull ack failed: %s", ErrString(r.Err())))
			}
		}
		if r := receipt(run, fmt.Sprintf("round=%d;fetched=%d;stop=%s",
			round, got, fetchStop(batchResult.Stop))); r.IsErr() {
			return r
		}

		if infoEvery > 0 && round%infoEvery == 0 {
			if info := puller.Info(); info.IsErr() {
				if r := receipt(run, fmt.Sprintf("round=%d;error=info;detail=%s",
					round, ErrString(info.Err()))); r.IsErr() {
					return r
				}
			}
		}
		if batchResult.Stop == wasmcloud_nats_jetstream.FetchStopDrained && got == 0 {
			break
		}
	}
	return receipt(run, fmt.Sprintf("total=%d", total))
}

func fetchStop(s wasmcloud_nats_jetstream.FetchStop) string {
	switch s {
	case wasmcloud_nats_jetstream.FetchStopBatchFilled:
		return "batch-filled"
	case wasmcloud_nats_jetstream.FetchStopDrained:
		return "drained"
	default:
		return "byte-limit"
	}
}
