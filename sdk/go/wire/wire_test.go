package wire

import (
	"bytes"
	"math"
	"testing"
)

func TestVarintCanonicalEncoding(t *testing.T) {
	cases := []struct {
		v    uint64
		want []byte
	}{
		{0, []byte{0x00}},
		{1, []byte{0x01}},
		{127, []byte{0x7f}},
		{128, []byte{0x80, 0x01}},
		{300, []byte{0xac, 0x02}},
		{16383, []byte{0xff, 0x7f}},
		{16384, []byte{0x80, 0x80, 0x01}},
		{math.MaxInt32, []byte{0xff, 0xff, 0xff, 0xff, 0x07}},
		{math.MaxUint32, []byte{0xff, 0xff, 0xff, 0xff, 0x0f}},
		{math.MaxInt64, []byte{0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f}},
		{math.MaxUint64, []byte{0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01}},
	}
	for _, c := range cases {
		got := PutUvarint(nil, c.v)
		if !bytes.Equal(got, c.want) {
			t.Errorf("PutUvarint(%d) = %x, want %x", c.v, got, c.want)
		}
		v, n, err := Uvarint(got)
		if err != nil || n != len(got) || v != c.v {
			t.Errorf("Uvarint(%x) = (%d, %d, %v), want (%d, %d, nil)", got, v, n, err, c.v, len(got))
		}
	}
}

func TestVarintDecodeAcceptsNonMinimal(t *testing.T) {
	// Protobuf decoders accept padded encodings: 0x80 0x00 == 0.
	v, n, err := Uvarint([]byte{0x80, 0x00})
	if err != nil || v != 0 || n != 2 {
		t.Fatalf("non-minimal zero: got (%d, %d, %v)", v, n, err)
	}
	v, n, err = Uvarint([]byte{0x81, 0x80, 0x80, 0x00})
	if err != nil || v != 1 || n != 4 {
		t.Fatalf("non-minimal one: got (%d, %d, %v)", v, n, err)
	}
}

func TestVarintDecodeErrors(t *testing.T) {
	// 11 bytes of continuation: beyond the 64-bit limit.
	overflow := bytes.Repeat([]byte{0x80}, 10)
	overflow = append(overflow, 0x01)
	if _, _, err := Uvarint(overflow); err != ErrOverflow {
		t.Fatalf("11-byte varint: got %v, want ErrOverflow", err)
	}
	// The 10th byte may only carry the final value bit.
	if _, _, err := Uvarint([]byte{0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02}); err != ErrOverflow {
		t.Fatalf("10th-byte overflow: got %v, want ErrOverflow", err)
	}
	// Truncation: every prefix of a valid varint that ends on a
	// continuation byte must report ErrTruncated.
	for cut := 1; cut < 10; cut++ {
		if _, _, err := Uvarint(bytes.Repeat([]byte{0x80}, cut)); err != ErrTruncated {
			t.Fatalf("truncated varint (cut=%d): got %v, want ErrTruncated", cut, err)
		}
	}
	if _, _, err := Uvarint(nil); err != ErrTruncated {
		t.Fatalf("empty input: got %v, want ErrTruncated", err)
	}
}

func TestTagBoundaries(t *testing.T) {
	// Field 15 is the largest single-byte key for wire type 2; field 16
	// needs a second key byte.
	if got := PutTag(nil, 15, TypeLen); !bytes.Equal(got, []byte{0x7a}) {
		t.Fatalf("field 15 key = %x, want 7a", got)
	}
	if got := PutTag(nil, 16, TypeLen); !bytes.Equal(got, []byte{0x82, 0x01}) {
		t.Fatalf("field 16 key = %x, want 8201", got)
	}
	// Multi-byte field numbers roundtrip.
	for _, field := range []int{1, 15, 16, 2047, 2048, 536870911 /* 2^29-1 */} {
		key := PutTag(nil, field, TypeLen)
		v, n, err := Uvarint(key)
		if err != nil || n != len(key) {
			t.Fatalf("field %d key %x did not decode", field, key)
		}
		if got := int(v >> 3); got != field || byte(v&7) != TypeLen {
			t.Fatalf("field %d key decoded as (%d, %d)", field, got, v&7)
		}
	}
}

func TestSkipValueUnknownFields(t *testing.T) {
	// varint
	if n, err := SkipValue([]byte{0xac, 0x02}, TypeVarint); err != nil || n != 2 {
		t.Fatalf("skip varint: (%d, %v)", n, err)
	}
	// length-delimited
	if n, err := SkipValue([]byte{0x03, 0xde, 0xad, 0xbe}, TypeLen); err != nil || n != 4 {
		t.Fatalf("skip len: (%d, %v)", n, err)
	}
	// fixed32 / fixed64 (not emitted by the probe, but must be skippable so
	// additive extensions using them survive capture)
	if n, err := SkipValue(make([]byte, 4), TypeFixed32); err != nil || n != 4 {
		t.Fatalf("skip fixed32: (%d, %v)", n, err)
	}
	if n, err := SkipValue(make([]byte, 8), TypeFixed64); err != nil || n != 8 {
		t.Fatalf("skip fixed64: (%d, %v)", n, err)
	}
	// groups fail closed
	if _, err := SkipValue(nil, TypeSGroup); err != ErrBadWire {
		t.Fatalf("skip sgroup: got %v", err)
	}
	if _, err := SkipValue(nil, TypeEGroup); err != ErrBadWire {
		t.Fatalf("skip egroup: got %v", err)
	}
	// truncated length-delimited
	if _, err := SkipValue([]byte{0x05, 0x01}, TypeLen); err != ErrTruncated {
		t.Fatalf("skip truncated len: got %v", err)
	}
}

func TestBytesHelpers(t *testing.T) {
	b := []byte{0x03, 0xaa, 0xbb, 0xcc, 0xff}
	v, n, err := Bytes(b)
	if err != nil || n != 4 || !bytes.Equal(v, []byte{0xaa, 0xbb, 0xcc}) {
		t.Fatalf("Bytes: (%x, %d, %v)", v, n, err)
	}
	if _, _, err := Bytes([]byte{0x05, 0x01}); err != ErrTruncated {
		t.Fatalf("Bytes truncated: got %v", err)
	}
	// Payload aliases the input; decodeBytes in package sabi is responsible
	// for copying, this layer documents the aliasing.
	if &v[0] != &b[1] {
		t.Fatal("Bytes must alias the input buffer")
	}
}
