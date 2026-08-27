// Template implementation — this file is YOURS to edit (generated as a stub by
// `componentize-go bindings --generate-stubs`).
//
// Exactly-once consume → transform → produce (read-process-write). Per batch:
// TransactionBegin → produce outputs (through the producer) → SendOffsets
// (one past the last processed input) → Commit. Outputs and input offsets
// commit atomically; downstream `read_committed` readers never see a partial
// batch. Requires `transactional.id` in the workload's kafka config (stable
// per pipeline — it is the fencing token) and downstream consumers using
// isolation.level=read_committed.
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

// transform is your processing — a pure function of the input record.
func transform(rec *cosmonic_kafka_types.ConsumedRecord) cosmonic_kafka_types.ProduceRecord {
	return cosmonic_kafka_types.ProduceRecord{
		Partition: witTypes.None[int32](),
		Key:       rec.Key,
		Value:     rec.Value,
		Headers:   rec.Headers,
		Timestamp: witTypes.None[int64](),
	}
}

// commitPositions: one past the last record seen per input partition, carrying
// the leader epoch through for offset fencing.
func commitPositions(batch []cosmonic_kafka_types.ConsumedRecord) []cosmonic_kafka_types.PartitionOffset {
	type pk struct {
		topic     string
		partition int32
	}
	latest := map[pk]*cosmonic_kafka_types.ConsumedRecord{}
	for i := range batch {
		rec := &batch[i]
		k := pk{rec.Topic, rec.Partition}
		if cur, seen := latest[k]; !seen || rec.Offset > cur.Offset {
			latest[k] = rec
		}
	}
	out := make([]cosmonic_kafka_types.PartitionOffset, 0, len(latest))
	for k, rec := range latest {
		out = append(out, cosmonic_kafka_types.PartitionOffset{
			Topic:       k.topic,
			Partition:   k.partition,
			Offset:      rec.Offset + 1,
			LeaderEpoch: rec.LeaderEpoch,
			Metadata:    witTypes.None[[]uint8](),
		})
	}
	return out
}

func processBatch(producer *cosmonic_kafka_producer.Producer, outTopic, groupID string, batch []cosmonic_kafka_types.ConsumedRecord) bool {
	begun := cosmonic_kafka_producer.TransactionBegin(producer)
	if begun.IsErr() {
		return false
	}
	txn := begun.Ok()
	outputs := make([]cosmonic_kafka_types.ProduceRecord, len(batch))
	for i := range batch {
		outputs[i] = transform(&batch[i])
	}
	ok := producer.SendBatch(outTopic, outputs).IsOk() &&
		txn.SendOffsets(commitPositions(batch), groupID).IsOk()
	if ok {
		return txn.Commit().IsOk()
	}
	// Abort keeps the pipeline consistent; uncommitted input offsets mean the
	// batch is redelivered and reprocessed.
	txn.Abort()
	return true
}

func Run() witTypes.Result[witTypes.Unit, witTypes.Unit] {
	inTopic := env("IN_TOPIC", "input")
	outTopic := env("OUT_TOPIC", "output")
	groupID := env("GROUP_ID", "txn-pipeline-g1")
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
	// transactional.id arrives via the host-side config merge.
	pOpened := cosmonic_kafka_producer.ProducerOpen(nil)
	if pOpened.IsErr() {
		return fail
	}
	producer := pOpened.Ok()
	defer producer.Drop()

	batch := make([]cosmonic_kafka_types.ConsumedRecord, 0, batchSize)
	buf := make([]cosmonic_kafka_types.ConsumedRecord, batchSize)
	for {
		n := records.Read(buf)
		if n == 0 {
			break
		}
		batch = append(batch, buf[:n]...)
		if len(batch) >= batchSize {
			if !processBatch(producer, outTopic, groupID, batch) {
				return fail
			}
			batch = batch[:0]
		}
	}
	if len(batch) > 0 && !processBatch(producer, outTopic, groupID, batch) {
		return fail
	}
	return witTypes.Ok[witTypes.Unit, witTypes.Unit](witTypes.Unit{})
}
