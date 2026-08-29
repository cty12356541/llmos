package sabi

import (
	"bytes"
	"encoding/hex"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// goldenBytes loads one frozen hex golden from the repository. The golden
// files are frozen (ADR-0014) and read-only for this probe.
func goldenBytes(t *testing.T, name string) []byte {
	t.Helper()
	root, err := filepath.Abs(filepath.Join("..", "..", ".."))
	if err != nil {
		t.Fatalf("resolve repo root: %v", err)
	}
	raw, err := os.ReadFile(filepath.Join(root, "schema", "golden", name))
	if err != nil {
		t.Fatalf("read golden %s: %v", name, err)
	}
	b, err := hex.DecodeString(strings.TrimSpace(string(raw)))
	if err != nil {
		t.Fatalf("decode golden %s: %v", name, err)
	}
	return b
}

// seq returns [off, off+1, ..., off+n-1], the nominal-ID pattern used by the
// frozen goldens.
func seq(off byte, n int) []byte {
	b := make([]byte, n)
	for i := range b {
		b[i] = off + byte(i)
	}
	return b
}

func repeat(b byte, n int) []byte {
	return bytes.Repeat([]byte{b}, n)
}

// roundtripGolden asserts that a fresh decode of the frozen golden survives a
// re-encode byte-identically (decode -> marshal == golden).
func roundtripGolden(t *testing.T, name string, golden []byte, decode func([]byte) ([]byte, error)) {
	t.Helper()
	raw, err := decode(golden)
	if err != nil {
		t.Fatalf("decode golden %s: %v", name, err)
	}
	if !bytes.Equal(raw, golden) {
		t.Fatalf("re-encode of decoded %s diverged:\n got  %x\n want %x", name, raw, golden)
	}
}

func TestEnvelopeGolden(t *testing.T) {
	golden := goldenBytes(t, "nlos.sabi.Envelope-v1.hex")

	envelope := &Envelope{
		Schema: &SchemaIdentity{
			Name:                    "nlos.sabi.Envelope",
			Major:                   1,
			NonCriticalExtensionIDs: []uint32{42},
		},
		RequestID: seq(0x00, 16),
		Service:   "operation",
		Method:    "get",
		Payload:   []byte("abc"),
	}
	if got := envelope.Marshal(); !bytes.Equal(got, golden) {
		t.Fatalf("encode Envelope-v1 != golden:\n got  %x\n want %x", got, golden)
	}

	var decoded Envelope
	if err := decoded.Unmarshal(golden); err != nil {
		t.Fatalf("decode golden: %v", err)
	}
	if decoded.Schema == nil ||
		decoded.Schema.Name != "nlos.sabi.Envelope" ||
		decoded.Schema.Major != 1 ||
		decoded.Schema.Minor != 0 ||
		len(decoded.Schema.CriticalExtensionIDs) != 0 ||
		len(decoded.Schema.NonCriticalExtensionIDs) != 1 ||
		decoded.Schema.NonCriticalExtensionIDs[0] != 42 {
		t.Fatalf("decoded schema identity mismatch: %+v", decoded.Schema)
	}
	if !bytes.Equal(decoded.RequestID, seq(0x00, 16)) {
		t.Fatalf("decoded request_id mismatch: %x", decoded.RequestID)
	}
	if decoded.Service != "operation" || decoded.Method != "get" {
		t.Fatalf("decoded service/method mismatch: %q/%q", decoded.Service, decoded.Method)
	}
	if !bytes.Equal(decoded.Payload, []byte("abc")) {
		t.Fatalf("decoded payload mismatch: %x", decoded.Payload)
	}
	if decoded.RequestContext != nil || decoded.ResponseContext != nil {
		t.Fatalf("oneof common_context must be unset in Envelope-v1 golden")
	}
	roundtripGolden(t, "Envelope-v1", golden, func(b []byte) ([]byte, error) {
		var m Envelope
		if err := m.Unmarshal(b); err != nil {
			return nil, err
		}
		return m.Marshal(), nil
	})
}

func TestEnvelopeCommonRequestGolden(t *testing.T) {
	golden := goldenBytes(t, "nlos.sabi.Envelope-common-request-v1.hex")

	envelope := &Envelope{
		Schema:    &SchemaIdentity{Name: "nlos.sabi.Envelope", Major: 1, Minor: 1},
		RequestID: seq(0x00, 16),
		Service:   "operation",
		Method:    "cancel",
		RequestContext: &SabiRequestContext{
			Caller: &CallerIdentity{
				PrincipalID:       repeat(0x01, 16),
				ApplicationID:     repeat(0x02, 16),
				ProcessID:         repeat(0x03, 16),
				ProcessGeneration: 7,
			},
			ActivityContext: []byte("trace"),
			TaskExecutionBinding: &TaskExecutionBinding{
				TaskAttemptID:             repeat(0x04, 16),
				TaskAuthorityTerm:         9,
				TaskControlEpoch:          10,
				CancelEpoch:               11,
				PermitEpoch:               12,
				IsolationDomainGeneration: 13,
			},
			CorrelationID:               repeat(0x05, 16),
			IdempotencyKey:              repeat(0x06, 16),
			DeadlineMonotonicNS:         123456,
			CapabilityHandles:           []*CapabilityHandle{{Slot: 11, Generation: 2}},
			ReservationHandle:           &CapabilityHandle{Slot: 12, Generation: 3},
			ProposalOrInputDigestSHA256: repeat(0x07, 32),
		},
		Payload: []byte("abc"),
	}
	if got := envelope.Marshal(); !bytes.Equal(got, golden) {
		t.Fatalf("encode Envelope-common-request-v1 != golden:\n got  %x\n want %x", got, golden)
	}

	var decoded Envelope
	if err := decoded.Unmarshal(golden); err != nil {
		t.Fatalf("decode golden: %v", err)
	}
	rc := decoded.RequestContext
	if rc == nil || decoded.ResponseContext != nil {
		t.Fatalf("request arm must be the set oneof: %+v", decoded)
	}
	if rc.Caller == nil || rc.Caller.ProcessGeneration != 7 {
		t.Fatalf("caller process_generation mismatch: %+v", rc.Caller)
	}
	if !bytes.Equal(rc.IdempotencyKey, repeat(0x06, 16)) {
		t.Fatalf("idempotency key mismatch: %x", rc.IdempotencyKey)
	}
	if rc.DeadlineMonotonicNS != 123456 {
		t.Fatalf("deadline mismatch: %d", rc.DeadlineMonotonicNS)
	}
	if len(rc.CapabilityHandles) != 1 || rc.CapabilityHandles[0].Slot != 11 || rc.CapabilityHandles[0].Generation != 2 {
		t.Fatalf("capability handles mismatch: %+v", rc.CapabilityHandles)
	}
	if rc.ReservationHandle == nil || rc.ReservationHandle.Slot != 12 || rc.ReservationHandle.Generation != 3 {
		t.Fatalf("reservation handle mismatch: %+v", rc.ReservationHandle)
	}
	if b := rc.TaskExecutionBinding; b == nil ||
		b.TaskAuthorityTerm != 9 || b.TaskControlEpoch != 10 || b.CancelEpoch != 11 ||
		b.PermitEpoch != 12 || b.IsolationDomainGeneration != 13 {
		t.Fatalf("task execution binding mismatch: %+v", b)
	}
	roundtripGolden(t, "Envelope-common-request-v1", golden, func(b []byte) ([]byte, error) {
		var m Envelope
		if err := m.Unmarshal(b); err != nil {
			return nil, err
		}
		return m.Marshal(), nil
	})
}

func TestEnvelopeCommonUncertainGolden(t *testing.T) {
	golden := goldenBytes(t, "nlos.sabi.Envelope-common-uncertain-v1.hex")

	envelope := &Envelope{
		Schema:    &SchemaIdentity{Name: "nlos.sabi.Envelope", Major: 1, Minor: 1},
		RequestID: seq(0x00, 16),
		Service:   "operation",
		Method:    "cancel",
		ResponseContext: &SabiResponseContext{
			CorrelationID: repeat(0x05, 16),
			Operation:     &OperationReference{OperationID: repeat(0x08, 16), Generation: 4},
			Receipts:      []*ReceiptReference{{ReceiptID: repeat(0x09, 16)}},
			Failure: &SabiFailure{
				Code:        SabiErrorCodeUncertain,
				Retry:       RetryDirectiveQueryOperationOrRetrySameIdempotencyKey,
				SafeMessage: "outcome requires reconciliation",
			},
		},
	}
	if got := envelope.Marshal(); !bytes.Equal(got, golden) {
		t.Fatalf("encode Envelope-common-uncertain-v1 != golden:\n got  %x\n want %x", got, golden)
	}

	var decoded Envelope
	if err := decoded.Unmarshal(golden); err != nil {
		t.Fatalf("decode golden: %v", err)
	}
	rc := decoded.ResponseContext
	if rc == nil || decoded.RequestContext != nil {
		t.Fatalf("response arm must be the set oneof: %+v", decoded)
	}
	if rc.Operation == nil || rc.Operation.Generation != 4 {
		t.Fatalf("operation generation mismatch: %+v", rc.Operation)
	}
	if len(rc.Receipts) != 1 || !bytes.Equal(rc.Receipts[0].ReceiptID, repeat(0x09, 16)) {
		t.Fatalf("receipts mismatch: %+v", rc.Receipts)
	}
	if rc.Failure == nil ||
		rc.Failure.Code != SabiErrorCodeUncertain ||
		rc.Failure.Retry != RetryDirectiveQueryOperationOrRetrySameIdempotencyKey ||
		rc.Failure.SafeMessage != "outcome requires reconciliation" {
		t.Fatalf("failure mismatch: %+v", rc.Failure)
	}
	if len(decoded.Payload) != 0 {
		t.Fatalf("uncertain golden carries no payload, got %x", decoded.Payload)
	}
	roundtripGolden(t, "Envelope-common-uncertain-v1", golden, func(b []byte) ([]byte, error) {
		var m Envelope
		if err := m.Unmarshal(b); err != nil {
			return nil, err
		}
		return m.Marshal(), nil
	})
}

func TestPrincipalHandshakeGolden(t *testing.T) {
	golden := goldenBytes(t, "nlos.sabi.PrincipalHandshake-v1.hex")

	attestation := &PrincipalHandshakeAttestation{
		Schema:         &SchemaIdentity{Name: "nlos.sabi.PrincipalHandshake", Major: 1},
		PrincipalID:    seq(0x00, 16),
		Nonce:          repeat(0xa5, 32),
		ChannelBinding: []byte("unix:///tmp/nlos-handshake.sock"),
		Signature:      repeat(0xcd, 64),
	}
	if got := attestation.Marshal(); !bytes.Equal(got, golden) {
		t.Fatalf("encode PrincipalHandshake-v1 != golden:\n got  %x\n want %x", got, golden)
	}

	var decoded PrincipalHandshakeAttestation
	if err := decoded.Unmarshal(golden); err != nil {
		t.Fatalf("decode golden: %v", err)
	}
	if decoded.Schema == nil ||
		decoded.Schema.Name != "nlos.sabi.PrincipalHandshake" ||
		decoded.Schema.Major != 1 ||
		decoded.Schema.Minor != 0 {
		t.Fatalf("decoded schema identity mismatch: %+v", decoded.Schema)
	}
	if !bytes.Equal(decoded.PrincipalID, seq(0x00, 16)) {
		t.Fatalf("principal_id mismatch: %x", decoded.PrincipalID)
	}
	if !bytes.Equal(decoded.Nonce, repeat(0xa5, 32)) {
		t.Fatalf("nonce mismatch: %x", decoded.Nonce)
	}
	if string(decoded.ChannelBinding) != "unix:///tmp/nlos-handshake.sock" {
		t.Fatalf("channel binding mismatch: %q", decoded.ChannelBinding)
	}
	if !bytes.Equal(decoded.Signature, repeat(0xcd, 64)) {
		t.Fatalf("signature mismatch: %x", decoded.Signature)
	}
	roundtripGolden(t, "PrincipalHandshake-v1", golden, func(b []byte) ([]byte, error) {
		var m PrincipalHandshakeAttestation
		if err := m.Unmarshal(b); err != nil {
			return nil, err
		}
		return m.Marshal(), nil
	})
}

// TestUnknownFieldPreservedAcrossRoundtrip mirrors the cross-language
// conformance case: a trailing unknown field (field 100, varint 7) appended
// to the frozen Envelope golden must survive decode + re-encode verbatim.
func TestUnknownFieldPreservedAcrossRoundtrip(t *testing.T) {
	golden := goldenBytes(t, "nlos.sabi.Envelope-v1.hex")
	extended := append(append([]byte{}, golden...), 0xa0, 0x06, 0x07)

	var decoded Envelope
	if err := decoded.Unmarshal(extended); err != nil {
		t.Fatalf("unknown field must decode as capture, got %v", err)
	}
	if !bytes.Equal(decoded.UnknownFields, []byte{0xa0, 0x06, 0x07}) {
		t.Fatalf("unknown field bytes mismatch: %x", decoded.UnknownFields)
	}
	if got := decoded.Marshal(); !bytes.Equal(got, extended) {
		t.Fatalf("re-encode with unknown field diverged:\n got  %x\n want %x", got, extended)
	}
}

func TestOneofBothArmsPanics(t *testing.T) {
	defer func() {
		if recover() == nil {
			t.Fatal("marshal with both oneof arms set must panic")
		}
	}()
	(&Envelope{
		RequestContext:  &SabiRequestContext{},
		ResponseContext: &SabiResponseContext{},
	}).Marshal()
}

func TestDecodeTruncatedFailsClosed(t *testing.T) {
	golden := goldenBytes(t, "nlos.sabi.Envelope-v1.hex")
	// Offsets that land inside a field value: 1 = lone schema tag, 28 = one
	// byte into the 16-byte request_id, 63 = four bytes of the 5-byte
	// payload value. A cut that lands exactly on a field boundary (e.g. 27)
	// is a complete valid prefix and must decode without error.
	for _, cut := range []int{1, 28, 63} {
		var m Envelope
		if err := m.Unmarshal(golden[:cut]); err == nil {
			t.Fatalf("truncated input (cut=%d) must fail closed", cut)
		}
	}
	var prefix Envelope
	if err := prefix.Unmarshal(golden[:27]); err != nil {
		t.Fatalf("field-boundary prefix must decode: %v", err)
	}
}

func TestDecodeGroupWireTypeFailsClosed(t *testing.T) {
	// Tag 0x0b = field 1, wire type 3 (group start): rejected, never skipped.
	var m PrincipalHandshakeAttestation
	if err := m.Unmarshal([]byte{0x0b, 0x08, 0x01, 0x0c}); err == nil {
		t.Fatal("group wire type must fail closed")
	}
}

func TestDecodeUint32OverflowRejected(t *testing.T) {
	// SchemaIdentity.Major encoded as varint 2^32: rejected, not truncated.
	overflow := []byte{0x0a, 0x00, 0x10, 0x80, 0x80, 0x80, 0x80, 0x10}
	var m SchemaIdentity
	if err := m.Unmarshal(overflow); err == nil {
		t.Fatal("uint32 overflow must be rejected")
	}
}

// TestLengthBoundary128 pins the length-prefix varint boundary: a 128-byte
// payload needs a two-byte length prefix (0x80 0x01) after the field-15 tag.
func TestLengthBoundary128(t *testing.T) {
	envelope := &Envelope{
		Schema:  &SchemaIdentity{Name: "n", Major: 1},
		Payload: repeat(0xee, 128),
	}
	// schema body: 0a 01 'n' 10 01 -> wrapped in Envelope field 1: 0a 05 + 5 = 7
	const schemaLen = 7
	const payloadOverhead = 3 // tag 0x7a + 2-byte varint length
	if got := len(envelope.Marshal()); got != schemaLen+payloadOverhead+128 {
		t.Fatalf("unexpected total length %d", got)
	}
	var decoded Envelope
	if err := decoded.Unmarshal(envelope.Marshal()); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if !bytes.Equal(decoded.Payload, repeat(0xee, 128)) {
		t.Fatal("128-byte payload did not survive the roundtrip")
	}
}
