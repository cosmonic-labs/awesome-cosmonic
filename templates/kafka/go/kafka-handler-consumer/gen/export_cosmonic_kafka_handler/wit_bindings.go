// Template implementation — this file is YOURS to edit (it was generated as a
// stub by `componentize-go bindings --generate-stubs`; regenerating without
// that flag leaves it alone).
//
// Handler-mode (push) Kafka consumer semantics:
//   - return Ok(None)   => record handled, offset stored
//   - Transient error   => host seeks back and redelivers
//   - Permanent error   => dead-letter.topic (configure one!) and advance
//   - panic             => treated as transient; a deterministic panic wedges
//                          the partition — validate input, don't panic.
// Do NOT open a Producer here: a fresh Kafka client per record costs ~100 ms
// (measured ~1200x a plain dispatch). Use kafka-pull-service to produce.
package export_cosmonic_kafka_handler

import (
	"unicode/utf8"

	witTypes "go.bytecodealliance.org/pkg/wit/types"
	"wit_component/cosmonic_kafka_handler"
	"wit_component/cosmonic_kafka_types"
)

func Handle(records []cosmonic_kafka_types.ConsumedRecord) witTypes.Result[witTypes.Option[int64], cosmonic_kafka_handler.HandlerError] {
	for _, rec := range records {
		if rec.Value.IsNone() {
			continue // tombstone
		}
		value := rec.Value.Some()
		if !utf8.Valid(value) {
			return witTypes.Err[witTypes.Option[int64], cosmonic_kafka_handler.HandlerError](
				cosmonic_kafka_handler.MakeHandlerErrorPermanent(
					witTypes.Some("value is not valid UTF-8")))
		}
		// process value ...
		_ = value
	}
	return witTypes.Ok[witTypes.Option[int64], cosmonic_kafka_handler.HandlerError](witTypes.None[int64]())
}
