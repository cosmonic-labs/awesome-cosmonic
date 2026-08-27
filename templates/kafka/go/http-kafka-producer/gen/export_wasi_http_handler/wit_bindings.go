// Template implementation — this file is YOURS to edit (generated as a stub by
// `componentize-go bindings --generate-stubs`).
//
// HTTP → Kafka producer. Routes:
//   POST /produce?topic=T&key=K&value=V        one record via Send
//   POST /produce-batch?topic=T&count=N&size=S N records via SendBatch
//
// Performance notes (measured on wasmCloud 2.8 / cosmonic:kafka 0.3.0):
//   - ProducerOpen builds a full Kafka client (~100 ms). One open per request
//     caps an instance near ~10 req/s; SendBatch amortizes it (30k+ records/s).
//   - Keep maxConcurrency at default (1) for this shape; poolSize>1 together
//     with maxConcurrency>1 measured 14x SLOWER on a client-per-request
//     producer. Scale with poolSize, replicas, or batching.
package export_wasi_http_handler

import (
	"fmt"
	"net/url"
	"strconv"

	witTypes "go.bytecodealliance.org/pkg/wit/types"
	"wit_component/cosmonic_kafka_producer"
	"wit_component/cosmonic_kafka_types"
	"wit_component/wasi_http_types"
)

func Handle(request *wasi_http_types.Request) witTypes.Result[*wasi_http_types.Response, wasi_http_types.ErrorCode] {
	pq := request.GetPathWithQuery().SomeOr("")
	u, err := url.Parse(pq)
	if err != nil {
		return ok(resp(400, "bad path"))
	}
	q := u.Query()
	method := request.GetMethod()
	switch {
	case method.Tag() == wasi_http_types.MethodPost && u.Path == "/produce":
		return ok(produce(q))
	case method.Tag() == wasi_http_types.MethodPost && u.Path == "/produce-batch":
		return ok(produceBatch(q))
	default:
		return ok(resp(404, "no such route"))
	}
}

func produce(q url.Values) *wasi_http_types.Response {
	topic, key, value := q.Get("topic"), q.Get("key"), q.Get("value")
	if topic == "" || key == "" || value == "" {
		return resp(400, "topic, key and value required")
	}
	// The host merges the workload's kafka config over this empty list —
	// bootstrap.servers etc. come from the manifest, not the code.
	opened := cosmonic_kafka_producer.ProducerOpen(nil)
	if opened.IsErr() {
		return resp(500, fmt.Sprintf("open failed: %v", opened.Err().Code.Tag()))
	}
	producer := opened.Ok()
	defer producer.Drop()
	sent := producer.Send(topic, record([]byte(key), []byte(value)))
	if sent.IsErr() {
		return resp(500, fmt.Sprintf("send failed: %v", sent.Err().Code.Tag()))
	}
	ack := sent.Ok()
	return resp(200, fmt.Sprintf("%d:%d", ack.Partition, ack.Offset))
}

func produceBatch(q url.Values) *wasi_http_types.Response {
	topic := q.Get("topic")
	if topic == "" {
		return resp(400, "topic required")
	}
	count, _ := strconv.Atoi(q.Get("count"))
	if count <= 0 {
		count = 100
	}
	size, _ := strconv.Atoi(q.Get("size"))
	if size <= 0 {
		size = 64
	}
	opened := cosmonic_kafka_producer.ProducerOpen(nil)
	if opened.IsErr() {
		return resp(500, fmt.Sprintf("open failed: %v", opened.Err().Code.Tag()))
	}
	producer := opened.Ok()
	defer producer.Drop()
	records := make([]cosmonic_kafka_types.ProduceRecord, count)
	pad := make([]byte, size)
	for i := range pad {
		pad[i] = 'x'
	}
	for i := range records {
		records[i] = record([]byte(fmt.Sprintf("k%d", i)), pad)
	}
	sent := producer.SendBatch(topic, records)
	if sent.IsErr() {
		return resp(500, fmt.Sprintf("send-batch failed: %v", sent.Err().Code.Tag()))
	}
	body := ""
	for _, outcome := range sent.Ok() {
		if outcome.IsOk() {
			body += "ok\n"
		} else {
			body += fmt.Sprintf("%v\n", outcome.Err().Code.Tag())
		}
	}
	return resp(200, body)
}

func record(key, value []byte) cosmonic_kafka_types.ProduceRecord {
	return cosmonic_kafka_types.ProduceRecord{
		Partition: witTypes.None[int32](),
		Key:       witTypes.Some(key),
		Value:     witTypes.Some(value),
		Headers:   nil,
		Timestamp: witTypes.None[int64](),
	}
}

func resp(status uint16, body string) *wasi_http_types.Response {
	headers := wasi_http_types.FieldsFromList(nil).Ok()
	bodyTx, bodyRx := wasi_http_types.MakeStreamU8()
	trailersTx, trailersRx := wasi_http_types.MakeFutureResultOptionFieldsErrorCode()
	go func() {
		bodyTx.WriteAll([]byte(body))
		bodyTx.Drop()
		trailersTx.Write(witTypes.Ok[witTypes.Option[*wasi_http_types.Fields], wasi_http_types.ErrorCode](
			witTypes.None[*wasi_http_types.Fields]()))
	}()
	response, _ := wasi_http_types.ResponseNew(headers, witTypes.Some(bodyRx), trailersRx)
	response.SetStatusCode(status)
	return response
}

func ok(r *wasi_http_types.Response) witTypes.Result[*wasi_http_types.Response, wasi_http_types.ErrorCode] {
	return witTypes.Ok[*wasi_http_types.Response, wasi_http_types.ErrorCode](r)
}
