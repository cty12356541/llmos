package sabi

import (
	"fmt"

	"example.com/llmos/sdk-go/wire"
)

// This file implements the hand-written deterministic encoders for the probe
// message set. Determinism contract (must stay byte-identical with protobuf
// deterministic serialization, the basis of the frozen goldens):
//
//   - fields are emitted strictly in ascending field-number order;
//   - proto3 implicit presence: zero-valued scalars and empty
//     strings/bytes/repeateds are omitted;
//   - message fields use pointer presence and are emitted (even when empty)
//     exactly when non-nil;
//   - repeated uint32 is emitted packed;
//   - unknown fields captured at decode time are appended verbatim at the
//     end, mirroring protobuf-go.

func putString(b []byte, field int, v string) []byte {
	b = wireTag(b, field)
	return wire.PutBytes(b, []byte(v))
}

func putBytesField(b []byte, field int, v []byte) []byte {
	b = wireTag(b, field)
	return wire.PutBytes(b, v)
}

func putUint64(b []byte, field int, v uint64) []byte {
	b = putVarintTag(b, field)
	return wire.PutUvarint(b, v)
}

func putUint32(b []byte, field int, v uint32) []byte {
	return putUint64(b, field, uint64(v))
}

func putPackedUint32(b []byte, field int, vs []uint32) []byte {
	var body []byte
	for _, v := range vs {
		body = wire.PutUvarint(body, uint64(v))
	}
	b = wireTag(b, field)
	return wire.PutBytes(b, body)
}

// putMessage wraps an already-encoded sub-message body in its tag and length.
func putMessage(b []byte, field int, body []byte) []byte {
	b = wireTag(b, field)
	return wire.PutBytes(b, body)
}

func wireTag(b []byte, field int) []byte {
	return wire.PutTag(b, field, wire.TypeLen)
}

// putVarintTag emits a tag for a varint-valued field.
func putVarintTag(b []byte, field int) []byte {
	return wire.PutTag(b, field, wire.TypeVarint)
}

// Marshal encodes m deterministically, as specified in marshal.go.
func (m *SchemaIdentity) Marshal() []byte {
	if m == nil {
		return nil
	}
	var b []byte
	if m.Name != "" {
		b = putString(b, 1, m.Name)
	}
	if m.Major != 0 {
		b = putVarintTag(b, 2)
		b = wire.PutUvarint(b, uint64(m.Major))
	}
	if m.Minor != 0 {
		b = putVarintTag(b, 3)
		b = wire.PutUvarint(b, uint64(m.Minor))
	}
	if len(m.CriticalExtensionIDs) > 0 {
		b = putPackedUint32(b, 4, m.CriticalExtensionIDs)
	}
	if len(m.NonCriticalExtensionIDs) > 0 {
		b = putPackedUint32(b, 5, m.NonCriticalExtensionIDs)
	}
	return append(b, m.UnknownFields...)
}

// Marshal encodes m deterministically, as specified in marshal.go.
func (m *CallerIdentity) Marshal() []byte {
	if m == nil {
		return nil
	}
	var b []byte
	if len(m.PrincipalID) > 0 {
		b = putBytesField(b, 1, m.PrincipalID)
	}
	if len(m.ApplicationID) > 0 {
		b = putBytesField(b, 2, m.ApplicationID)
	}
	if len(m.ProcessID) > 0 {
		b = putBytesField(b, 3, m.ProcessID)
	}
	if m.ProcessGeneration != 0 {
		b = putUint64(b, 4, m.ProcessGeneration)
	}
	return append(b, m.UnknownFields...)
}

// Marshal encodes m deterministically, as specified in marshal.go.
func (m *TaskExecutionBinding) Marshal() []byte {
	if m == nil {
		return nil
	}
	var b []byte
	if len(m.TaskAttemptID) > 0 {
		b = putBytesField(b, 1, m.TaskAttemptID)
	}
	if m.TaskAuthorityTerm != 0 {
		b = putUint64(b, 2, m.TaskAuthorityTerm)
	}
	if m.TaskControlEpoch != 0 {
		b = putUint64(b, 3, m.TaskControlEpoch)
	}
	if m.CancelEpoch != 0 {
		b = putUint64(b, 4, m.CancelEpoch)
	}
	if m.PermitEpoch != 0 {
		b = putUint64(b, 5, m.PermitEpoch)
	}
	if m.IsolationDomainGeneration != 0 {
		b = putUint64(b, 6, m.IsolationDomainGeneration)
	}
	return append(b, m.UnknownFields...)
}

// Marshal encodes m deterministically, as specified in marshal.go.
func (m *CapabilityHandle) Marshal() []byte {
	if m == nil {
		return nil
	}
	var b []byte
	if m.Slot != 0 {
		b = putUint64(b, 1, m.Slot)
	}
	if m.Generation != 0 {
		b = putUint64(b, 2, m.Generation)
	}
	return append(b, m.UnknownFields...)
}

// Marshal encodes m deterministically, as specified in marshal.go.
func (m *SabiRequestContext) Marshal() []byte {
	if m == nil {
		return nil
	}
	var b []byte
	if m.Caller != nil {
		b = putMessage(b, 1, m.Caller.Marshal())
	}
	if len(m.ActivityContext) > 0 {
		b = putBytesField(b, 2, m.ActivityContext)
	}
	if m.TaskExecutionBinding != nil {
		b = putMessage(b, 3, m.TaskExecutionBinding.Marshal())
	}
	if len(m.CorrelationID) > 0 {
		b = putBytesField(b, 4, m.CorrelationID)
	}
	if len(m.IdempotencyKey) > 0 {
		b = putBytesField(b, 5, m.IdempotencyKey)
	}
	if m.DeadlineMonotonicNS != 0 {
		b = putUint64(b, 6, m.DeadlineMonotonicNS)
	}
	for _, h := range m.CapabilityHandles {
		if h != nil {
			b = putMessage(b, 7, h.Marshal())
		}
	}
	if m.ReservationHandle != nil {
		b = putMessage(b, 8, m.ReservationHandle.Marshal())
	}
	if len(m.ProposalOrInputDigestSHA256) > 0 {
		b = putBytesField(b, 9, m.ProposalOrInputDigestSHA256)
	}
	return append(b, m.UnknownFields...)
}

// Marshal encodes m deterministically, as specified in marshal.go.
func (m *SabiFailure) Marshal() []byte {
	if m == nil {
		return nil
	}
	var b []byte
	if m.Code != 0 {
		b = putUint32(b, 1, uint32(m.Code))
	}
	if m.Retry != 0 {
		b = putUint32(b, 2, uint32(m.Retry))
	}
	if m.SafeMessage != "" {
		b = putString(b, 3, m.SafeMessage)
	}
	return append(b, m.UnknownFields...)
}

// Marshal encodes m deterministically, as specified in marshal.go.
func (m *OperationReference) Marshal() []byte {
	if m == nil {
		return nil
	}
	var b []byte
	if len(m.OperationID) > 0 {
		b = putBytesField(b, 1, m.OperationID)
	}
	if m.Generation != 0 {
		b = putUint64(b, 2, m.Generation)
	}
	return append(b, m.UnknownFields...)
}

// Marshal encodes m deterministically, as specified in marshal.go.
func (m *ReceiptReference) Marshal() []byte {
	if m == nil {
		return nil
	}
	var b []byte
	if len(m.ReceiptID) > 0 {
		b = putBytesField(b, 1, m.ReceiptID)
	}
	return append(b, m.UnknownFields...)
}

// Marshal encodes m deterministically, as specified in marshal.go.
func (m *SabiResponseContext) Marshal() []byte {
	if m == nil {
		return nil
	}
	var b []byte
	if len(m.CorrelationID) > 0 {
		b = putBytesField(b, 1, m.CorrelationID)
	}
	if m.Operation != nil {
		b = putMessage(b, 2, m.Operation.Marshal())
	}
	for _, r := range m.Receipts {
		if r != nil {
			b = putMessage(b, 3, r.Marshal())
		}
	}
	if m.Failure != nil {
		b = putMessage(b, 4, m.Failure.Marshal())
	}
	return append(b, m.UnknownFields...)
}

// Marshal encodes m deterministically, as specified in marshal.go. It panics
// if both arms of the common_context oneof are set; the probe treats that as
// an unrepresentable state rather than a typed error.
func (m *Envelope) Marshal() []byte {
	if m == nil {
		return nil
	}
	if m.RequestContext != nil && m.ResponseContext != nil {
		panic(fmt.Sprintf("sabi: Envelope %q oneof common_context has both arms set", m.SchemaIdentityName()))
	}
	var b []byte
	if m.Schema != nil {
		b = putMessage(b, 1, m.Schema.Marshal())
	}
	if len(m.RequestID) > 0 {
		b = putBytesField(b, 2, m.RequestID)
	}
	if m.Service != "" {
		b = putString(b, 3, m.Service)
	}
	if m.Method != "" {
		b = putString(b, 4, m.Method)
	}
	if m.RequestContext != nil {
		b = putMessage(b, 5, m.RequestContext.Marshal())
	}
	if m.ResponseContext != nil {
		b = putMessage(b, 6, m.ResponseContext.Marshal())
	}
	if len(m.Payload) > 0 {
		b = putBytesField(b, 15, m.Payload)
	}
	return append(b, m.UnknownFields...)
}

// SchemaIdentityName returns the schema name carried by the Envelope, for
// diagnostics; empty when no schema identity is set.
func (m *Envelope) SchemaIdentityName() string {
	if m == nil || m.Schema == nil {
		return ""
	}
	return m.Schema.Name
}

// Marshal encodes m deterministically, as specified in marshal.go.
func (m *PrincipalHandshakeAttestation) Marshal() []byte {
	if m == nil {
		return nil
	}
	var b []byte
	if m.Schema != nil {
		b = putMessage(b, 1, m.Schema.Marshal())
	}
	if len(m.PrincipalID) > 0 {
		b = putBytesField(b, 2, m.PrincipalID)
	}
	if len(m.Nonce) > 0 {
		b = putBytesField(b, 3, m.Nonce)
	}
	if len(m.ChannelBinding) > 0 {
		b = putBytesField(b, 4, m.ChannelBinding)
	}
	if len(m.Signature) > 0 {
		b = putBytesField(b, 5, m.Signature)
	}
	return append(b, m.UnknownFields...)
}
