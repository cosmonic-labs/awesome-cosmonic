// Shared rig helpers, ported 1:1 from the Rust track's components.
//
// `Field` is the body header parser every component shares; `ErrString`
// reproduces Rust's `{e:?}` rendering of `nats-error` so receipt and error
// text is comparable across tracks.
package export_wasmcloud_nats_kv_handler

import (
	"fmt"
	"strconv"
	"strings"

	"wit_component/wasmcloud_nats_types"
)

// Field reads `key=` out of the body's header prefix. Only the first `limit`
// bytes are scanned so a 900 KiB payload costs nothing to parse. Scanning
// stops at `pad=`, exactly as the Rust `field()` does.
func Field(body []uint8, key string, limit int) (string, bool) {
	if len(body) < limit {
		limit = len(body)
	}
	text := string(body[:limit])
	for _, part := range strings.Split(text, ";") {
		k, v, found := strings.Cut(part, "=")
		if !found {
			return "", false
		}
		if k == "pad" {
			return "", false
		}
		if k == key {
			return v, true
		}
	}
	return "", false
}

// FieldOr is Field with a default, matching `unwrap_or_else`.
func FieldOr(body []uint8, key string, limit int, def string) string {
	if v, ok := Field(body, key, limit); ok {
		return v
	}
	return def
}

// FieldU64 matches `.and_then(|v| v.parse().ok()).unwrap_or(def)`.
func FieldU64(body []uint8, key string, limit int, def uint64) uint64 {
	if v, ok := Field(body, key, limit); ok {
		if n, err := strconv.ParseUint(v, 10, 64); err == nil {
			return n
		}
	}
	return def
}

// ErrString renders a nats-error the way Rust's `{:?}` does, so guest-visible
// error text lines up between tracks.
func ErrString(e wasmcloud_nats_types.NatsError) string {
	switch e.Tag() {
	case wasmcloud_nats_types.NatsErrorConnection:
		return fmt.Sprintf("Connection(%q)", e.Connection())
	case wasmcloud_nats_types.NatsErrorTimeout:
		return fmt.Sprintf("Timeout(%q)", e.Timeout())
	case wasmcloud_nats_types.NatsErrorNoResponders:
		return "NoResponders"
	case wasmcloud_nats_types.NatsErrorDenied:
		d := e.Denied()
		return fmt.Sprintf("Denied(Denial { reason: %s, target: %s, name: %q })",
			denialReason(d.Reason), deniedResource(d.Target), d.Name)
	case wasmcloud_nats_types.NatsErrorMaxPayloadExceeded:
		return fmt.Sprintf("MaxPayloadExceeded(%d)", e.MaxPayloadExceeded())
	case wasmcloud_nats_types.NatsErrorInvalidHeader:
		return fmt.Sprintf("InvalidHeader(%q)", e.InvalidHeader())
	case wasmcloud_nats_types.NatsErrorJetstream:
		return fmt.Sprintf("Jetstream(%q)", e.Jetstream())
	case wasmcloud_nats_types.NatsErrorKeyNotFound:
		return "KeyNotFound"
	case wasmcloud_nats_types.NatsErrorRevisionMismatch:
		return fmt.Sprintf("RevisionMismatch(%d)", e.RevisionMismatch())
	case wasmcloud_nats_types.NatsErrorNoMessages:
		return "NoMessages"
	case wasmcloud_nats_types.NatsErrorLimitExceeded:
		return fmt.Sprintf("LimitExceeded(%q)", e.LimitExceeded())
	case wasmcloud_nats_types.NatsErrorNotFound:
		return fmt.Sprintf("NotFound(%q)", e.NotFound())
	case wasmcloud_nats_types.NatsErrorUnsupportedByServer:
		return fmt.Sprintf("UnsupportedByServer(%q)", e.UnsupportedByServer())
	case wasmcloud_nats_types.NatsErrorDisconnected:
		return "Disconnected"
	default:
		return fmt.Sprintf("Unexpected(%q)", e.Unexpected())
	}
}

func denialReason(r wasmcloud_nats_types.DenialReason) string {
	switch r {
	case wasmcloud_nats_types.DenialReasonReserved:
		return "Reserved"
	case wasmcloud_nats_types.DenialReasonNotGranted:
		return "NotGranted"
	default:
		return "WildcardNotAllowed"
	}
}

func deniedResource(t wasmcloud_nats_types.DeniedResource) string {
	switch t.Tag() {
	case wasmcloud_nats_types.DeniedResourceSubject:
		return "Subject"
	case wasmcloud_nats_types.DeniedResourceStream:
		return "Stream"
	case wasmcloud_nats_types.DeniedResourceBucket:
		return "Bucket"
	default:
		// A stored JetStream message: the payload is its stream sequence and
		// the denial's `name` is the stream it lives in.
		return fmt.Sprintf("Message#%d", t.Message())
	}
}

// parseU32 mirrors Rust's `v.parse::<u32>().ok()`.
func parseU32(s string) (uint32, error) {
	n, err := strconv.ParseUint(s, 10, 32)
	return uint32(n), err
}

// FieldU32 matches `.and_then(|v| v.parse().ok()).unwrap_or(def)` for u32.
func FieldU32(body []uint8, key string, limit int, def uint32) uint32 {
	if v, ok := Field(body, key, limit); ok {
		if n, err := parseU32(v); err == nil {
			return n
		}
	}
	return def
}
