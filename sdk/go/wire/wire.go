// Package wire implements the minimal protobuf binary wire primitives used by
// the B-SDK-LANG-EVAL Go golden probe.
//
// Probe scope (route B, hand-written — protoc/protoc-gen-go/buf are not
// installed on this machine): varint scalars, length-delimited values, and
// tag keys only. Fixed32/fixed64 values are skipped on input but never
// emitted; group wire types (3/4) are rejected fail-closed. Zigzag, maps,
// extensions, and service stubs are out of scope. This is a probe, not the
// full SDK protobuf stack.
package wire

import "errors"

// Protobuf wire types.
const (
	TypeVarint  = 0
	TypeFixed64 = 1
	TypeLen     = 2
	TypeSGroup  = 3
	TypeEGroup  = 4
	TypeFixed32 = 5
)

// Errors reported by this package.
var (
	// ErrTruncated reports a buffer that ends in the middle of a field.
	ErrTruncated = errors.New("wire: truncated buffer")
	// ErrOverflow reports a varint longer than the 10-byte 64-bit limit.
	ErrOverflow = errors.New("wire: varint exceeds 10 bytes")
	// ErrBadWire reports a wire type the probe does not support on input
	// (group start/end); decoders fail closed on it.
	ErrBadWire = errors.New("wire: unsupported wire type")
	// ErrFieldZero reports the reserved field number 0.
	ErrFieldZero = errors.New("wire: field number 0")
)

// PutUvarint appends v in minimal protobuf base-128 varint form (1-10 bytes).
// Output is always canonical: no redundant continuation bytes, matching the
// deterministic serialization of protobuf-go.
func PutUvarint(b []byte, v uint64) []byte {
	for v >= 0x80 {
		b = append(b, byte(v)|0x80)
		v >>= 7
	}
	return append(b, byte(v))
}

// Uvarint decodes one base-128 varint from the front of b and returns the
// value together with the number of bytes consumed. Following protobuf
// decoder semantics, non-minimal (padded) encodings are accepted on input;
// truncation and more than 10 bytes are errors.
func Uvarint(b []byte) (uint64, int, error) {
	var v uint64
	for i := 0; i < len(b) && i < 10; i++ {
		c := b[i]
		if i == 9 {
			// The 10th byte contributes exactly one value bit.
			if c > 1 {
				return 0, 0, ErrOverflow
			}
			return v | uint64(c)<<63, 10, nil
		}
		v |= uint64(c&0x7f) << (7 * i)
		if c < 0x80 {
			return v, i + 1, nil
		}
	}
	return 0, 0, ErrTruncated
}

// PutTag appends the varint key packing a field number and wire type.
func PutTag(b []byte, field int, wireType byte) []byte {
	return PutUvarint(b, uint64(field)<<3|uint64(wireType))
}

// PutBytes appends a length-delimited value: varint length prefix + payload.
func PutBytes(b []byte, data []byte) []byte {
	b = PutUvarint(b, uint64(len(data)))
	return append(b, data...)
}

// Bytes reads one length-delimited value from the front of b and returns the
// payload slice (aliasing b) plus the total bytes consumed.
func Bytes(b []byte) ([]byte, int, error) {
	n, header, err := Uvarint(b)
	if err != nil {
		return nil, 0, err
	}
	if uint64(len(b)-header) < n {
		return nil, 0, ErrTruncated
	}
	return b[header : header+int(n)], header + int(n), nil
}

// SkipValue returns the number of bytes occupied by the value of one field
// with the given wire type, starting at the front of b (after its tag).
// Wire types 1 and 5 are skipped so that unknown fields of any future
// additive extension survive a decode/encode cycle; group types are rejected.
func SkipValue(b []byte, wireType byte) (int, error) {
	switch wireType {
	case TypeVarint:
		_, n, err := Uvarint(b)
		return n, err
	case TypeFixed64:
		if len(b) < 8 {
			return 0, ErrTruncated
		}
		return 8, nil
	case TypeLen:
		_, n, err := Bytes(b)
		return n, err
	case TypeFixed32:
		if len(b) < 4 {
			return 0, ErrTruncated
		}
		return 4, nil
	default:
		return 0, ErrBadWire
	}
}
