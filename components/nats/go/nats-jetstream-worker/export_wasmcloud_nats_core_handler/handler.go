// JetStream pull worker — the guest sets the pace.
//
// A core NATS message on demo.worker.run triggers a drain: the component opens
// the pull consumer named below, fetches batches until the stream is caught up
// or maxRounds is reached, and acknowledges each message.
//
// Pull is the right shape only when the guest needs to control the rate. A push
// consumer (nats-jetstream-consumer) is simpler, and is what you want unless
// you can say why you need this one.
//
// The stream, consumer and bounds are constants rather than fields read out of
// the trigger message. A drain loop whose target and size are chosen by whoever
// can publish to the trigger subject is a denial of service with extra steps.
//
// GO LIMITATION YOU MUST KNOW: a handler that parks on a timer TRAPS. The
// fetch timeout below is the host's, not Go's, which is why it is safe; do not
// add time.Sleep between rounds.
//
// START HERE: HandleMessage is the only thing you need to change.

package export_wasmcloud_nats_core_handler

import (
	"fmt"

	witTypes "go.bytecodealliance.org/pkg/wit/types"
	"wit_component/wasmcloud_nats_jetstream"
	"wit_component/wasmcloud_nats_types"
)

const (
	// The stream and durable consumer this worker drains. stream-allow on the
	// binding has to cover the stream, and subject-allow has to cover the
	// consumer's filter, or opening it is denied at attach.
	stream   = "DEMO"
	consumer = "demo-worker"

	// Messages per fetch, and how long the host waits for a batch to fill.
	//
	// A fetch materialises batch × message size in host memory, so 100 is only
	// right for small payloads: at 1 MB messages it asks for 100 MB and the
	// host refuses with limit-exceeded. Drop it to single digits as payloads
	// grow.
	batch          = 100
	fetchTimeoutMs = 5000

	// A ceiling on one drain, so a single trigger cannot start unbounded work.
	// With batch that is 10,000 messages, which against a live feed can run for
	// minutes on a poolSize: 1 component. Size both to your own throughput.
	maxRounds = 100
)

func HandleMessage(_ wasmcloud_nats_types.NatsMessage) witTypes.Result[witTypes.Unit, string] {
	opened := wasmcloud_nats_jetstream.OpenPullConsumer(stream, consumer)
	if opened.IsErr() {
		return witTypes.Err[witTypes.Unit, string](fmt.Sprintf(
			"open pull consumer %s/%s failed: %s", stream, consumer, ErrString(opened.Err())))
	}
	puller := opened.Ok()
	// The consumer handle holds host resources until it is dropped.
	defer puller.Drop()

	acked := 0
	for round := 0; round < maxRounds; round++ {
		fetched := puller.Fetch(batch, fetchTimeoutMs)
		if fetched.IsErr() {
			// The consumer had nothing within the timeout. That is how a drain
			// ends, not a failure: fetch reports an empty result as no-messages
			// rather than as an empty batch.
			if fetched.Err().Tag() == wasmcloud_nats_types.NatsErrorNoMessages {
				break
			}
			return witTypes.Err[witTypes.Unit, string](
				fmt.Sprintf("fetch failed: %s", ErrString(fetched.Err())))
		}
		result := fetched.Ok()

		for _, handle := range result.Messages {
			// Replace this with your own processing. Acknowledge after the
			// work, not before: an ack is a promise it is done.
			if r := handle.Ack(); r.IsErr() {
				handle.Drop()
				return witTypes.Err[witTypes.Unit, string](
					fmt.Sprintf("ack failed: %s", ErrString(r.Err())))
			}
			// Load-bearing, not tidiness. An un-dropped handle holds its share
			// of the subscription byte budget, and fetch stalls silently once
			// that budget is exhausted.
			handle.Drop()
			acked++
		}

		// Drained with nothing in hand means the consumer is caught up.
		if result.Stop == wasmcloud_nats_jetstream.FetchStopDrained && len(result.Messages) == 0 {
			break
		}
	}

	receipt := wasmcloud_nats_types.NatsMessage{
		Subject: "done.demo.worker",
		Body:    []uint8(fmt.Sprintf("acked=%d", acked)),
		ReplyTo: witTypes.None[string](),
		Headers: witTypes.None[[]wasmcloud_nats_types.HeaderEntry](),
	}
	if r := wasmcloud_nats_jetstream.Publish(receipt); r.IsErr() {
		return witTypes.Err[witTypes.Unit, string](
			fmt.Sprintf("receipt publish failed: %s", ErrString(r.Err())))
	}
	return witTypes.Ok[witTypes.Unit, string](witTypes.Unit{})
}
