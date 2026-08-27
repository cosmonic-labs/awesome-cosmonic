// Template implementation — this file is YOURS to edit (generated as a stub by
// `componentize-go bindings --generate-stubs`).
//
// Pull-mode consume → transform → produce Service. One long-lived instance
// owns one consumer and ONE long-lived producer — the pattern that avoids
// per-record client bootstrap (~100 ms each, measured) and lets SendBatch
// batch. At-least-once: offsets are stored as records enter the stream;
// Commit(nil) commits stored positions after the batch's outputs are acked.
//
// Config via env (localResources.environment.config): IN_TOPIC, OUT_TOPIC,
// DLQ_TOPIC, GROUP_ID, BATCH_SIZE. Kafka connection config is merged in by
// the host from hostInterfaces[].config and cannot be overridden here.
package export_wasi_cli_0_3_0_run

import (
	"strconv"

	witTypes "go.bytecodealliance.org/pkg/wit/types"
	"wit_component/cosmonic_kafka_consumer"
	"wit_component/cosmonic_kafka_producer"
	"wit_component/cosmonic_kafka_types"
	"wit_component/wasi_cli_0_2_0_environment"
)

func env(key, def string) string {
	for _, kv := range wasi_cli_0_2_0_environment.GetEnvironment() {
		if kv.F0 == key {
			return kv.F1
		}
	}
	return def
}

func cfg(key, value string) cosmonic_kafka_types.ConfigEntry {
	return cosmonic_kafka_types.ConfigEntry{Key: key, Value: value}
}

// transform is your processing. Return (record, true) to emit, or
// (zero, false) to dead-letter the input.
func transform(rec *cosmonic_kafka_types.ConsumedRecord) (cosmonic_kafka_types.ProduceRecord, bool) {
	return cosmonic_kafka_types.ProduceRecord{
		Partition: witTypes.None[int32](),
		Key:       rec.Key,
		Value:     rec.Value,
		Headers:   rec.Headers,
		Timestamp: witTypes.None[int64](),
	}, true
}

func Run() witTypes.Result[witTypes.Unit, witTypes.Unit] {
	inTopic := env("IN_TOPIC", "input")
	outTopic := env("OUT_TOPIC", "output")
	dlqTopic := env("DLQ_TOPIC", "input.dlq")
	groupID := env("GROUP_ID", "pull-service-g1")
	batchSize, err := strconv.Atoi(env("BATCH_SIZE", "100"))
	if err != nil || batchSize < 1 {
		batchSize = 100
	}

	fail := witTypes.Err[witTypes.Unit, witTypes.Unit](witTypes.Unit{})

	opened := cosmonic_kafka_consumer.ConsumerOpen([]cosmonic_kafka_types.ConfigEntry{
		cfg("group.id", groupID),
		cfg("auto.offset.reset", "earliest"),
		cfg("enable.auto.commit", "false"),
	})
	if opened.IsErr() {
		return fail
	}
	consumer := opened.Ok()
	// Keep the consumer resource referenced for the life of the loop: Go's GC
	// otherwise collects it (nothing below reads the variable), its finalizer
	// drops the host-side consumer, and the record stream silently ends.
	defer consumer.Drop()
	if consumer.Subscribe([]string{inTopic}).IsErr() {
		return fail
	}
	recordsResult := consumer.Records()
	if recordsResult.IsErr() {
		return fail
	}
	records := recordsResult.Ok().F0
	pOpened := cosmonic_kafka_producer.ProducerOpen(nil)
	if pOpened.IsErr() {
		return fail
	}
	producer := pOpened.Ok()
	defer producer.Drop()

	pending := make([]cosmonic_kafka_types.ProduceRecord, 0, batchSize)
	buf := make([]cosmonic_kafka_types.ConsumedRecord, batchSize)
	for {
		// Read blocks until at least one record arrives; returns 0 when the
		// stream ends (host shutdown).
		n := records.Read(buf)
		if n == 0 {
			break
		}
		for i := uint32(0); i < n; i++ {
			rec := &buf[i]
			if out, keep := transform(rec); keep {
				pending = append(pending, out)
			} else {
				dead := cosmonic_kafka_types.ProduceRecord{
					Partition: witTypes.None[int32](),
					Key:       rec.Key,
					Value:     rec.Value,
					Headers:   rec.Headers,
					Timestamp: witTypes.None[int64](),
				}
				producer.Send(dlqTopic, dead)
			}
		}
		if len(pending) >= batchSize {
			if producer.SendBatch(outTopic, pending).IsOk() {
				consumer.Commit(nil)
			}
			pending = pending[:0]
		}
	}
	if len(pending) > 0 && producer.SendBatch(outTopic, pending).IsOk() {
		consumer.Commit(nil)
	}
	return witTypes.Ok[witTypes.Unit, witTypes.Unit](witTypes.Unit{})
}
