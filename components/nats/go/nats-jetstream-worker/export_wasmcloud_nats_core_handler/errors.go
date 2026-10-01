// Formatting for `wasmcloud:nats` errors.
//
// A NatsError is a WIT variant, so Go prints it as an opaque struct unless it
// is spelled out. These render the forms the host's own messages use, which is
// what makes a denied grant greppable: `Denied(Denial { reason: NotGranted,
// target: Subject, name: ">" })`.

package export_wasmcloud_nats_core_handler

import (
	"fmt"

	"wit_component/wasmcloud_nats_types"
)

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
	case wasmcloud_nats_types.NatsErrorAlreadySettled:
		return "AlreadySettled"
	case wasmcloud_nats_types.NatsErrorAckOwnedByHost:
		return "AckOwnedByHost"
	case wasmcloud_nats_types.NatsErrorUnexpected:
		return fmt.Sprintf("Unexpected(%q)", e.Unexpected())
	default:
		// Never call a payload accessor here. The generated accessors panic on
		// a tag mismatch, and a panic traps the instance, so a variant added to
		// the WIT would turn a formatted error into a dead component.
		return fmt.Sprintf("NatsError(tag=%d)", e.Tag())
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
