package sabi

import (
	"fmt"
	"math"

	"example.com/llmos/sdk-go/wire"
)

// This file implements the hand-written decoders for the probe message set.
// Decode contract:
//
//   - the target is reset first (decode-into-fresh semantics);
//   - scalar fields use last-wins on duplicate occurrences;
//   - repeated fields accumulate across occurrences (packed and unpacked
//     encodings are both accepted for repeated uint32);
//   - unknown fields of supported wire types are captured verbatim
//     (tag + value) into UnknownFields; group wire types fail closed;
//   - values that overflow 32-bit fields are rejected instead of truncated;
//   - field number 0 is rejected (reserved).

// errField reports a field that arrived with an unexpected wire type, which
// for these frozen messages means corrupt input (the field numbers and their
// types are pinned by the frozen schema).
func errField(msg string, field int) error {
	return fmt.Errorf("sabi: %s: unexpected encoding of field %d", msg, field)
}

// u32 converts a decoded varint to uint32, rejecting overflow.
func u32(v uint64) (uint32, error) {
	if v > math.MaxUint32 {
		return 0, fmt.Errorf("sabi: varint %d overflows uint32", v)
	}
	return uint32(v), nil
}

// scan iterates the fields of one message value, dispatching to fn.
func scan(msg string, b []byte, fn func(field int, wt byte, value []byte, rest []byte) (int, error)) error {
	i := 0
	for i < len(b) {
		tag, n, err := wire.Uvarint(b[i:])
		if err != nil {
			return fmt.Errorf("sabi: %s: %w", msg, err)
		}
		i += n
		field, wt := int(tag>>3), byte(tag&7)
		if field == 0 {
			return fmt.Errorf("sabi: %s: %w", msg, wire.ErrFieldZero)
		}
		consumed, err := fn(field, wt, b[i:], b[i-n:i])
		if err != nil {
			return err
		}
		i += consumed
	}
	return nil
}

// captureUnknown appends the raw bytes of one unknown field (tag + value).
func captureUnknown(dst []byte, tag []byte, value []byte, consumed int) []byte {
	raw := make([]byte, 0, len(tag)+consumed)
	raw = append(raw, tag...)
	raw = append(raw, value[:consumed]...)
	return append(dst, raw...)
}

// decodeString reads one length-delimited field as a string.
func decodeString(b []byte) (string, int, error) {
	v, n, err := wire.Bytes(b)
	if err != nil {
		return "", 0, err
	}
	return string(v), n, nil
}

// decodeBytes reads one length-delimited field as a fresh byte copy.
func decodeBytes(b []byte) ([]byte, int, error) {
	v, n, err := wire.Bytes(b)
	if err != nil {
		return nil, 0, err
	}
	out := make([]byte, len(v))
	copy(out, v)
	return out, n, nil
}

// decodeVarint reads one varint field.
func decodeVarint(b []byte) (uint64, int, error) {
	return wire.Uvarint(b)
}

// decodePackedUint32 reads a repeated uint32 payload, accepting both the
// packed (length-delimited) and unpacked (single varint) forms; wt selects.
func decodePackedUint32(b []byte, wt byte) ([]uint32, int, error) {
	if wt == wire.TypeVarint {
		v, n, err := wire.Uvarint(b)
		if err != nil {
			return nil, 0, err
		}
		u, err := u32(v)
		if err != nil {
			return nil, 0, err
		}
		return []uint32{u}, n, nil
	}
	v, n, err := wire.Bytes(b)
	if err != nil {
		return nil, 0, err
	}
	var out []uint32
	i := 0
	for i < len(v) {
		x, m, err := wire.Uvarint(v[i:])
		if err != nil {
			return nil, 0, err
		}
		u, err := u32(x)
		if err != nil {
			return nil, 0, err
		}
		out = append(out, u)
		i += m
	}
	return out, n, nil
}

// Unmarshal decodes one SchemaIdentity value.
func (m *SchemaIdentity) Unmarshal(b []byte) error {
	const msg = "SchemaIdentity"
	*m = SchemaIdentity{}
	return scan(msg, b, func(field int, wt byte, value, tag []byte) (int, error) {
		switch {
		case field == 1 && wt == wire.TypeLen:
			s, n, err := decodeString(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.Name = s
			return n, nil
		case field == 2 && wt == wire.TypeVarint:
			v, n, err := decodeVarint(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			u, err := u32(v)
			if err != nil {
				return 0, err
			}
			m.Major = u
			return n, nil
		case field == 3 && wt == wire.TypeVarint:
			v, n, err := decodeVarint(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			u, err := u32(v)
			if err != nil {
				return 0, err
			}
			m.Minor = u
			return n, nil
		case field == 4 && (wt == wire.TypeLen || wt == wire.TypeVarint):
			vs, n, err := decodePackedUint32(value, wt)
			if err != nil {
				return 0, err
			}
			m.CriticalExtensionIDs = append(m.CriticalExtensionIDs, vs...)
			return n, nil
		case field == 5 && (wt == wire.TypeLen || wt == wire.TypeVarint):
			vs, n, err := decodePackedUint32(value, wt)
			if err != nil {
				return 0, err
			}
			m.NonCriticalExtensionIDs = append(m.NonCriticalExtensionIDs, vs...)
			return n, nil
		default:
			n, err := wire.SkipValue(value, wt)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.UnknownFields = captureUnknown(m.UnknownFields, tag, value, n)
			return n, nil
		}
	})
}

// Unmarshal decodes one CallerIdentity value.
func (m *CallerIdentity) Unmarshal(b []byte) error {
	const msg = "CallerIdentity"
	*m = CallerIdentity{}
	return scan(msg, b, func(field int, wt byte, value, tag []byte) (int, error) {
		switch {
		case field == 1 && wt == wire.TypeLen:
			v, n, err := decodeBytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.PrincipalID = v
			return n, nil
		case field == 2 && wt == wire.TypeLen:
			v, n, err := decodeBytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.ApplicationID = v
			return n, nil
		case field == 3 && wt == wire.TypeLen:
			v, n, err := decodeBytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.ProcessID = v
			return n, nil
		case field == 4 && wt == wire.TypeVarint:
			v, n, err := decodeVarint(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.ProcessGeneration = v
			return n, nil
		default:
			n, err := wire.SkipValue(value, wt)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.UnknownFields = captureUnknown(m.UnknownFields, tag, value, n)
			return n, nil
		}
	})
}

// Unmarshal decodes one TaskExecutionBinding value.
func (m *TaskExecutionBinding) Unmarshal(b []byte) error {
	const msg = "TaskExecutionBinding"
	*m = TaskExecutionBinding{}
	uintFields := map[int]*uint64{
		2: &m.TaskAuthorityTerm,
		3: &m.TaskControlEpoch,
		4: &m.CancelEpoch,
		5: &m.PermitEpoch,
		6: &m.IsolationDomainGeneration,
	}
	return scan(msg, b, func(field int, wt byte, value, tag []byte) (int, error) {
		switch {
		case field == 1 && wt == wire.TypeLen:
			v, n, err := decodeBytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.TaskAttemptID = v
			return n, nil
		case field >= 2 && field <= 6 && wt == wire.TypeVarint:
			v, n, err := decodeVarint(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			*uintFields[field] = v
			return n, nil
		default:
			n, err := wire.SkipValue(value, wt)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.UnknownFields = captureUnknown(m.UnknownFields, tag, value, n)
			return n, nil
		}
	})
}

// Unmarshal decodes one CapabilityHandle value.
func (m *CapabilityHandle) Unmarshal(b []byte) error {
	const msg = "CapabilityHandle"
	*m = CapabilityHandle{}
	return scan(msg, b, func(field int, wt byte, value, tag []byte) (int, error) {
		switch {
		case field == 1 && wt == wire.TypeVarint:
			v, n, err := decodeVarint(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.Slot = v
			return n, nil
		case field == 2 && wt == wire.TypeVarint:
			v, n, err := decodeVarint(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.Generation = v
			return n, nil
		default:
			n, err := wire.SkipValue(value, wt)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.UnknownFields = captureUnknown(m.UnknownFields, tag, value, n)
			return n, nil
		}
	})
}

// Unmarshal decodes one SabiRequestContext value.
func (m *SabiRequestContext) Unmarshal(b []byte) error {
	const msg = "SabiRequestContext"
	*m = SabiRequestContext{}
	return scan(msg, b, func(field int, wt byte, value, tag []byte) (int, error) {
		switch {
		case field == 1 && wt == wire.TypeLen:
			v, n, err := wire.Bytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.Caller = &CallerIdentity{}
			if err := m.Caller.Unmarshal(v); err != nil {
				return 0, err
			}
			return n, nil
		case field == 2 && wt == wire.TypeLen:
			v, n, err := decodeBytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.ActivityContext = v
			return n, nil
		case field == 3 && wt == wire.TypeLen:
			v, n, err := wire.Bytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.TaskExecutionBinding = &TaskExecutionBinding{}
			if err := m.TaskExecutionBinding.Unmarshal(v); err != nil {
				return 0, err
			}
			return n, nil
		case field == 4 && wt == wire.TypeLen:
			v, n, err := decodeBytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.CorrelationID = v
			return n, nil
		case field == 5 && wt == wire.TypeLen:
			v, n, err := decodeBytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.IdempotencyKey = v
			return n, nil
		case field == 6 && wt == wire.TypeVarint:
			v, n, err := decodeVarint(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.DeadlineMonotonicNS = v
			return n, nil
		case field == 7 && wt == wire.TypeLen:
			v, n, err := wire.Bytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			h := &CapabilityHandle{}
			if err := h.Unmarshal(v); err != nil {
				return 0, err
			}
			m.CapabilityHandles = append(m.CapabilityHandles, h)
			return n, nil
		case field == 8 && wt == wire.TypeLen:
			v, n, err := wire.Bytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.ReservationHandle = &CapabilityHandle{}
			if err := m.ReservationHandle.Unmarshal(v); err != nil {
				return 0, err
			}
			return n, nil
		case field == 9 && wt == wire.TypeLen:
			v, n, err := decodeBytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.ProposalOrInputDigestSHA256 = v
			return n, nil
		default:
			n, err := wire.SkipValue(value, wt)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.UnknownFields = captureUnknown(m.UnknownFields, tag, value, n)
			return n, nil
		}
	})
}

// Unmarshal decodes one SabiFailure value.
func (m *SabiFailure) Unmarshal(b []byte) error {
	const msg = "SabiFailure"
	*m = SabiFailure{}
	return scan(msg, b, func(field int, wt byte, value, tag []byte) (int, error) {
		switch {
		case field == 1 && wt == wire.TypeVarint:
			v, n, err := decodeVarint(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			u, err := u32(v)
			if err != nil {
				return 0, err
			}
			m.Code = SabiErrorCode(u)
			return n, nil
		case field == 2 && wt == wire.TypeVarint:
			v, n, err := decodeVarint(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			u, err := u32(v)
			if err != nil {
				return 0, err
			}
			m.Retry = RetryDirective(u)
			return n, nil
		case field == 3 && wt == wire.TypeLen:
			s, n, err := decodeString(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.SafeMessage = s
			return n, nil
		default:
			n, err := wire.SkipValue(value, wt)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.UnknownFields = captureUnknown(m.UnknownFields, tag, value, n)
			return n, nil
		}
	})
}

// Unmarshal decodes one OperationReference value.
func (m *OperationReference) Unmarshal(b []byte) error {
	const msg = "OperationReference"
	*m = OperationReference{}
	return scan(msg, b, func(field int, wt byte, value, tag []byte) (int, error) {
		switch {
		case field == 1 && wt == wire.TypeLen:
			v, n, err := decodeBytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.OperationID = v
			return n, nil
		case field == 2 && wt == wire.TypeVarint:
			v, n, err := decodeVarint(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.Generation = v
			return n, nil
		default:
			n, err := wire.SkipValue(value, wt)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.UnknownFields = captureUnknown(m.UnknownFields, tag, value, n)
			return n, nil
		}
	})
}

// Unmarshal decodes one ReceiptReference value.
func (m *ReceiptReference) Unmarshal(b []byte) error {
	const msg = "ReceiptReference"
	*m = ReceiptReference{}
	return scan(msg, b, func(field int, wt byte, value, tag []byte) (int, error) {
		switch {
		case field == 1 && wt == wire.TypeLen:
			v, n, err := decodeBytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.ReceiptID = v
			return n, nil
		default:
			n, err := wire.SkipValue(value, wt)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.UnknownFields = captureUnknown(m.UnknownFields, tag, value, n)
			return n, nil
		}
	})
}

// Unmarshal decodes one SabiResponseContext value.
func (m *SabiResponseContext) Unmarshal(b []byte) error {
	const msg = "SabiResponseContext"
	*m = SabiResponseContext{}
	return scan(msg, b, func(field int, wt byte, value, tag []byte) (int, error) {
		switch {
		case field == 1 && wt == wire.TypeLen:
			v, n, err := decodeBytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.CorrelationID = v
			return n, nil
		case field == 2 && wt == wire.TypeLen:
			v, n, err := wire.Bytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.Operation = &OperationReference{}
			if err := m.Operation.Unmarshal(v); err != nil {
				return 0, err
			}
			return n, nil
		case field == 3 && wt == wire.TypeLen:
			v, n, err := wire.Bytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			r := &ReceiptReference{}
			if err := r.Unmarshal(v); err != nil {
				return 0, err
			}
			m.Receipts = append(m.Receipts, r)
			return n, nil
		case field == 4 && wt == wire.TypeLen:
			v, n, err := wire.Bytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.Failure = &SabiFailure{}
			if err := m.Failure.Unmarshal(v); err != nil {
				return 0, err
			}
			return n, nil
		default:
			n, err := wire.SkipValue(value, wt)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.UnknownFields = captureUnknown(m.UnknownFields, tag, value, n)
			return n, nil
		}
	})
}

// Unmarshal decodes one Envelope value.
func (m *Envelope) Unmarshal(b []byte) error {
	const msg = "Envelope"
	*m = Envelope{}
	return scan(msg, b, func(field int, wt byte, value, tag []byte) (int, error) {
		switch {
		case field == 1 && wt == wire.TypeLen:
			v, n, err := wire.Bytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.Schema = &SchemaIdentity{}
			if err := m.Schema.Unmarshal(v); err != nil {
				return 0, err
			}
			return n, nil
		case field == 2 && wt == wire.TypeLen:
			v, n, err := decodeBytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.RequestID = v
			return n, nil
		case field == 3 && wt == wire.TypeLen:
			s, n, err := decodeString(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.Service = s
			return n, nil
		case field == 4 && wt == wire.TypeLen:
			s, n, err := decodeString(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.Method = s
			return n, nil
		case field == 5 && wt == wire.TypeLen:
			v, n, err := wire.Bytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.RequestContext = &SabiRequestContext{}
			if err := m.RequestContext.Unmarshal(v); err != nil {
				return 0, err
			}
			return n, nil
		case field == 6 && wt == wire.TypeLen:
			v, n, err := wire.Bytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.ResponseContext = &SabiResponseContext{}
			if err := m.ResponseContext.Unmarshal(v); err != nil {
				return 0, err
			}
			return n, nil
		case field == 15 && wt == wire.TypeLen:
			v, n, err := decodeBytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.Payload = v
			return n, nil
		default:
			n, err := wire.SkipValue(value, wt)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.UnknownFields = captureUnknown(m.UnknownFields, tag, value, n)
			return n, nil
		}
	})
}

// Unmarshal decodes one PrincipalHandshakeAttestation value.
func (m *PrincipalHandshakeAttestation) Unmarshal(b []byte) error {
	const msg = "PrincipalHandshakeAttestation"
	*m = PrincipalHandshakeAttestation{}
	return scan(msg, b, func(field int, wt byte, value, tag []byte) (int, error) {
		switch {
		case field == 1 && wt == wire.TypeLen:
			v, n, err := wire.Bytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.Schema = &SchemaIdentity{}
			if err := m.Schema.Unmarshal(v); err != nil {
				return 0, err
			}
			return n, nil
		case field == 2 && wt == wire.TypeLen:
			v, n, err := decodeBytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.PrincipalID = v
			return n, nil
		case field == 3 && wt == wire.TypeLen:
			v, n, err := decodeBytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.Nonce = v
			return n, nil
		case field == 4 && wt == wire.TypeLen:
			v, n, err := decodeBytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.ChannelBinding = v
			return n, nil
		case field == 5 && wt == wire.TypeLen:
			v, n, err := decodeBytes(value)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.Signature = v
			return n, nil
		default:
			n, err := wire.SkipValue(value, wt)
			if err != nil {
				return 0, fmt.Errorf("sabi: %s: %w", msg, err)
			}
			m.UnknownFields = captureUnknown(m.UnknownFields, tag, value, n)
			return n, nil
		}
	})
}
