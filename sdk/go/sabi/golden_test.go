package sabi

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
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

// TestServiceDirectoryResolveRequestGolden pins the frozen ServiceDirectory
// resolve entry golden, mirroring the Rust compatibility test and the
// TypeScript/Python conformance case (schema identity + service name only;
// endpoints stay hidden until negotiation).
func TestServiceDirectoryResolveRequestGolden(t *testing.T) {
	golden := goldenBytes(t, "nlos.sabi.ServiceDirectory.ResolveRequest-v1.hex")

	request := &ResolveServiceRequest{
		Schema:  &SchemaIdentity{Name: "nlos.sabi.ServiceDirectory", Major: 1},
		Service: "operation",
	}
	if got := request.Marshal(); !bytes.Equal(got, golden) {
		t.Fatalf("encode ServiceDirectory.ResolveRequest-v1 != golden:\n got  %x\n want %x", got, golden)
	}

	var decoded ResolveServiceRequest
	if err := decoded.Unmarshal(golden); err != nil {
		t.Fatalf("decode golden: %v", err)
	}
	if decoded.Schema == nil ||
		decoded.Schema.Name != "nlos.sabi.ServiceDirectory" ||
		decoded.Schema.Major != 1 ||
		decoded.Schema.Minor != 0 {
		t.Fatalf("decoded schema identity mismatch: %+v", decoded.Schema)
	}
	if decoded.Service != "operation" {
		t.Fatalf("decoded service mismatch: %q", decoded.Service)
	}
	roundtripGolden(t, "ServiceDirectory.ResolveRequest-v1", golden, func(b []byte) ([]byte, error) {
		var m ResolveServiceRequest
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

// ---------------------------------------------------------------------------
// Stage E W77-E1B: schema/golden/epochs/v1.0/ 9/9 channel golden corpus &
// COMPAT-VER-002 bidirectional compatibility tests.
// ---------------------------------------------------------------------------

type epochLegacyGoldenMeta struct {
	Path       string `json:"path"`
	ByteLength int    `json:"byte_length"`
	SHA256Wire string `json:"sha256_wire"`
	SHA256File string `json:"sha256_file"`
}

type epochVectorMeta struct {
	File        string `json:"file"`
	Channel     string `json:"channel"`
	Message     string `json:"message"`
	SchemaMajor uint32 `json:"schema_major"`
	SchemaMinor uint32 `json:"schema_minor"`
	ByteLength  int    `json:"byte_length"`
	SHA256Wire  string `json:"sha256_wire"`
	SHA256File  string `json:"sha256_file"`
}

type epochManifest struct {
	Schema            string                  `json:"schema"`
	Epoch             string                  `json:"epoch"`
	SabiMajor         uint32                  `json:"sabi_major"`
	Immutable         bool                    `json:"immutable"`
	RequiredChannels  []string                `json:"required_channels"`
	LegacyRootGoldens []epochLegacyGoldenMeta `json:"legacy_root_goldens"`
	Vectors           []epochVectorMeta       `json:"vectors"`
}

func repoRoot(t *testing.T) string {
	t.Helper()
	root, err := filepath.Abs(filepath.Join("..", "..", ".."))
	if err != nil {
		t.Fatalf("resolve repo root: %v", err)
	}
	return root
}

func loadEpochV10Manifest(t *testing.T) epochManifest {
	t.Helper()
	raw, err := os.ReadFile(filepath.Join(repoRoot(t), "schema", "golden", "epochs", "v1.0", "manifest.json"))
	if err != nil {
		t.Fatalf("read v1.0 manifest.json: %v", err)
	}
	var m epochManifest
	if err := json.Unmarshal(raw, &m); err != nil {
		t.Fatalf("parse v1.0 manifest.json: %v", err)
	}
	return m
}

func epochV10Bytes(t *testing.T, name string) []byte {
	t.Helper()
	m := loadEpochV10Manifest(t)
	var meta *epochVectorMeta
	for i := range m.Vectors {
		if m.Vectors[i].File == name {
			meta = &m.Vectors[i]
			break
		}
	}
	if meta == nil {
		t.Fatalf("vector %s missing from v1.0 manifest.json", name)
	}
	fullPath := filepath.Join(repoRoot(t), "schema", "golden", "epochs", "v1.0", name)
	rawFile, err := os.ReadFile(fullPath)
	if err != nil {
		t.Fatalf("read epoch golden %s: %v", name, err)
	}
	sumFile := sha256.Sum256(rawFile)
	if got := hex.EncodeToString(sumFile[:]); got != meta.SHA256File {
		t.Fatalf("%s sha256_file mismatch: got %s, want %s", name, got, meta.SHA256File)
	}
	wire, err := hex.DecodeString(strings.TrimSpace(string(rawFile)))
	if err != nil {
		t.Fatalf("decode epoch golden %s: %v", name, err)
	}
	if len(wire) != meta.ByteLength {
		t.Fatalf("%s byte_length mismatch: got %d, want %d", name, len(wire), meta.ByteLength)
	}
	sumWire := sha256.Sum256(wire)
	if got := hex.EncodeToString(sumWire[:]); got != meta.SHA256Wire {
		t.Fatalf("%s sha256_wire mismatch: got %s, want %s", name, got, meta.SHA256Wire)
	}
	return wire
}

func TestEpochV10ManifestAndFingerprints(t *testing.T) {
	m := loadEpochV10Manifest(t)
	if m.Schema != "nlos.golden.epoch_manifest.v1" || m.Epoch != "v1.0" || m.SabiMajor != 1 || !m.Immutable {
		t.Fatalf("unexpected v1.0 manifest header: %+v", m)
	}
	if len(m.RequiredChannels) != len(SabiV1Channels) {
		t.Fatalf("required_channels count=%d, want %d", len(m.RequiredChannels), len(SabiV1Channels))
	}
	for _, ch := range m.RequiredChannels {
		if _, ok := SabiV1Channels[ch]; !ok {
			t.Fatalf("unexpected channel in manifest: %q", ch)
		}
	}
	if len(m.LegacyRootGoldens) != 7 {
		t.Fatalf("legacy_root_goldens count=%d, want 7", len(m.LegacyRootGoldens))
	}
	root := repoRoot(t)
	for _, lg := range m.LegacyRootGoldens {
		rawFile, err := os.ReadFile(filepath.Join(root, filepath.FromSlash(lg.Path)))
		if err != nil {
			t.Fatalf("read legacy golden %s: %v", lg.Path, err)
		}
		sumFile := sha256.Sum256(rawFile)
		if got := hex.EncodeToString(sumFile[:]); got != lg.SHA256File {
			t.Fatalf("%s sha256_file=%s, want %s", lg.Path, got, lg.SHA256File)
		}
		wire, err := hex.DecodeString(strings.TrimSpace(string(rawFile)))
		if err != nil {
			t.Fatalf("decode legacy golden %s: %v", lg.Path, err)
		}
		sumWire := sha256.Sum256(wire)
		if len(wire) != lg.ByteLength || hex.EncodeToString(sumWire[:]) != lg.SHA256Wire {
			t.Fatalf("%s wire fingerprint mismatch", lg.Path)
		}
	}
	covered := make(map[string]bool)
	for _, vec := range m.Vectors {
		_ = epochV10Bytes(t, vec.File)
		covered[vec.Channel] = true
	}
	if len(covered) != 9 {
		t.Fatalf("epoch v1.0 covered %d channels, want 9", len(covered))
	}
}

func TestEpochV10EnvelopeGoldens(t *testing.T) {
	g1 := epochV10Bytes(t, "nlos.sabi.Envelope-v1.0.hex")
	env1 := &Envelope{
		Schema: &SchemaIdentity{
			Name:                    "nlos.sabi.Envelope",
			Major:                   1,
			Minor:                   1,
			NonCriticalExtensionIDs: []uint32{42},
		},
		RequestID: seq(0x10, 16),
		Service:   "system_control",
		Method:    "get",
		RequestContext: &SabiRequestContext{
			Caller: &CallerIdentity{
				PrincipalID:       repeat(0x01, 16),
				ApplicationID:     repeat(0x02, 16),
				ProcessID:         repeat(0x03, 16),
				ProcessGeneration: 7,
			},
			ActivityContext: []byte("w77-epoch-v1.0"),
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
			DeadlineMonotonicNS:         500000,
			CapabilityHandles:           []*CapabilityHandle{{Slot: 11, Generation: 2}},
			ReservationHandle:           &CapabilityHandle{Slot: 12, Generation: 3},
			ProposalOrInputDigestSHA256: repeat(0x07, 32),
		},
		Payload: []byte("sabi-v1.0-payload"),
	}
	if got := env1.Marshal(); !bytes.Equal(got, g1) {
		t.Fatalf("encode Envelope-v1.0 != golden:\n got  %x\n want %x", got, g1)
	}
	var dec1 Envelope
	if err := dec1.Unmarshal(g1); err != nil {
		t.Fatalf("decode Envelope-v1.0: %v", err)
	}
	if err := ValidateSchemaIdentity(dec1.Schema, "nlos.sabi.Envelope"); err != nil {
		t.Fatalf("validate Envelope-v1.0 schema: %v", err)
	}
	if dec1.Service != "system_control" || dec1.Method != "get" || string(dec1.Payload) != "sabi-v1.0-payload" {
		t.Fatalf("decoded Envelope-v1.0 mismatch: %+v", dec1)
	}
	if !bytes.Equal(dec1.Marshal(), g1) {
		t.Fatal("re-encode Envelope-v1.0 diverged")
	}

	g2 := epochV10Bytes(t, "nlos.sabi.Envelope-unknown-noncritical-v1.0.hex")
	env2 := &Envelope{
		Schema: &SchemaIdentity{
			Name:                    "nlos.sabi.Envelope",
			Major:                   1,
			Minor:                   1,
			NonCriticalExtensionIDs: []uint32{42, 7001, 9999},
		},
		RequestID:     seq(0x20, 16),
		Service:       "operation",
		Method:        "query",
		Payload:       []byte("ext-probe"),
		UnknownFields: []byte{0xa0, 0x06, 0x07},
	}
	if got := env2.Marshal(); !bytes.Equal(got, g2) {
		t.Fatalf("encode Envelope-unknown-noncritical-v1.0 != golden:\n got  %x\n want %x", got, g2)
	}
	var dec2 Envelope
	if err := dec2.Unmarshal(g2); err != nil {
		t.Fatalf("decode Envelope-unknown-noncritical-v1.0: %v", err)
	}
	if err := ValidateSchemaIdentity(dec2.Schema, "nlos.sabi.Envelope"); err != nil {
		t.Fatalf("validate Envelope-unknown-noncritical-v1.0 schema: %v", err)
	}
	if len(dec2.Schema.NonCriticalExtensionIDs) != 3 ||
		dec2.Schema.NonCriticalExtensionIDs[0] != 42 ||
		dec2.Schema.NonCriticalExtensionIDs[1] != 7001 ||
		dec2.Schema.NonCriticalExtensionIDs[2] != 9999 {
		t.Fatalf("non-critical extensions mismatch: %+v", dec2.Schema.NonCriticalExtensionIDs)
	}
	if !bytes.Equal(dec2.UnknownFields, []byte{0xa0, 0x06, 0x07}) {
		t.Fatalf("captured unknown fields mismatch: %x", dec2.UnknownFields)
	}
	if !bytes.Equal(dec2.Marshal(), g2) {
		t.Fatal("re-encode Envelope-unknown-noncritical-v1.0 diverged")
	}
}

func TestEpochV10ServiceDirectoryGolden(t *testing.T) {
	golden := epochV10Bytes(t, "nlos.sabi.ServiceDirectory.ResolveServiceRequest-v1.0.hex")
	req := &ResolveServiceRequest{
		Schema:  &SchemaIdentity{Name: "nlos.sabi.ServiceDirectory", Major: 1},
		Service: "system_control",
	}
	if got := req.Marshal(); !bytes.Equal(got, golden) {
		t.Fatalf("encode != golden: %x != %x", got, golden)
	}
	var dec ResolveServiceRequest
	if err := dec.Unmarshal(golden); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if err := ValidateSchemaIdentity(dec.Schema, "nlos.sabi.ServiceDirectory"); err != nil {
		t.Fatalf("validate schema: %v", err)
	}
	if dec.Service != "system_control" || !bytes.Equal(dec.Marshal(), golden) {
		t.Fatalf("ServiceDirectory v1.0 roundtrip mismatch: %+v", dec)
	}
}

func TestEpochV10OperationControlGolden(t *testing.T) {
	golden := epochV10Bytes(t, "nlos.sabi.OperationControl.QueryOperationRequest-v1.0.hex")
	req := &QueryOperationRequest{
		Schema:    &SchemaIdentity{Name: "nlos.sabi.OperationControl", Major: 1},
		Operation: &OperationReference{OperationID: seq(0x30, 16), Generation: 5},
	}
	if got := req.Marshal(); !bytes.Equal(got, golden) {
		t.Fatalf("encode != golden: %x != %x", got, golden)
	}
	var dec QueryOperationRequest
	if err := dec.Unmarshal(golden); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if err := ValidateSchemaIdentity(dec.Schema, "nlos.sabi.OperationControl"); err != nil {
		t.Fatalf("validate schema: %v", err)
	}
	if dec.Operation == nil || !bytes.Equal(dec.Operation.OperationID, seq(0x30, 16)) || dec.Operation.Generation != 5 {
		t.Fatalf("decoded QueryOperationRequest mismatch: %+v", dec.Operation)
	}
	if !bytes.Equal(dec.Marshal(), golden) {
		t.Fatal("re-encode QueryOperationRequest diverged")
	}
}

func TestEpochV10SystemControlGolden(t *testing.T) {
	golden := epochV10Bytes(t, "nlos.sabi.SystemControl.GetSystemControlRequest-v1.0.hex")
	req := &GetSystemControlRequest{
		Schema:           &SchemaIdentity{Name: "nlos.sabi.SystemControl", Major: 1, Minor: 7},
		View:             SystemControlViewTaskAttemptList,
		AlertLimit:       16,
		TargetID:         seq(0x40, 16),
		PlanID:           seq(0x50, 16),
		TargetGeneration: 4,
	}
	if got := req.Marshal(); !bytes.Equal(got, golden) {
		t.Fatalf("encode != golden: %x != %x", got, golden)
	}
	var dec GetSystemControlRequest
	if err := dec.Unmarshal(golden); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if err := ValidateSchemaIdentity(dec.Schema, "nlos.sabi.SystemControl"); err != nil {
		t.Fatalf("validate schema: %v", err)
	}
	if dec.Schema.Minor != 7 || dec.View != SystemControlViewTaskAttemptList || dec.AlertLimit != 16 ||
		!bytes.Equal(dec.TargetID, seq(0x40, 16)) || !bytes.Equal(dec.PlanID, seq(0x50, 16)) ||
		dec.TargetGeneration != 4 {
		t.Fatalf("decoded GetSystemControlRequest mismatch: %+v", dec)
	}
	if !bytes.Equal(dec.Marshal(), golden) {
		t.Fatal("re-encode GetSystemControlRequest diverged")
	}
}

func TestEpochV10TakeoverControlGolden(t *testing.T) {
	golden := epochV10Bytes(t, "nlos.sabi.TakeoverControl.SubmitBarrierObservationRequest-v1.0.hex")
	req := &SubmitBarrierObservationRequest{
		Schema: &SchemaIdentity{Name: "nlos.sabi.TakeoverControl", Major: 1},
		Target: &BarrierObservationTarget{
			TakeoverReceiptID:     repeat(0x61, 16),
			ParticipantType:       1,
			ParticipantID:         repeat(0x62, 16),
			ParticipantGeneration: 3,
			AdmissionReceiptID:    repeat(0x63, 16),
		},
		Evidence: &BarrierObservationEvidence{
			RemoteReceiptID: repeat(0x64, 16),
			BarrierDigest:   repeat(0x65, 32),
			ObservedAtMS:    1730000000000,
		},
		Signature: &BarrierObservationSignature{
			SignerPrincipalID:     repeat(0x66, 16),
			SignerControlDomainID: repeat(0x67, 16),
			SignerKeyID:           repeat(0x68, 16),
			Signature:             repeat(0x69, 64),
		},
	}
	if got := req.Marshal(); !bytes.Equal(got, golden) {
		t.Fatalf("encode != golden: %x != %x", got, golden)
	}
	var dec SubmitBarrierObservationRequest
	if err := dec.Unmarshal(golden); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if err := ValidateSchemaIdentity(dec.Schema, "nlos.sabi.TakeoverControl"); err != nil {
		t.Fatalf("validate schema: %v", err)
	}
	if dec.Target == nil || dec.Target.ParticipantType != 1 || dec.Target.ParticipantGeneration != 3 ||
		dec.Evidence == nil || dec.Evidence.ObservedAtMS != 1730000000000 ||
		dec.Signature == nil || !bytes.Equal(dec.Signature.Signature, repeat(0x69, 64)) {
		t.Fatalf("decoded SubmitBarrierObservationRequest mismatch: %+v", dec)
	}
	if !bytes.Equal(dec.Marshal(), golden) {
		t.Fatal("re-encode SubmitBarrierObservationRequest diverged")
	}
}

func TestEpochV10WaitControlGolden(t *testing.T) {
	golden := epochV10Bytes(t, "nlos.sabi.WaitControl.InspectWaitRequest-v1.0.hex")
	req := &InspectWaitRequest{
		Schema: &SchemaIdentity{Name: "nlos.sabi.WaitControl", Major: 1},
		WaitID: seq(0x70, 16),
	}
	if got := req.Marshal(); !bytes.Equal(got, golden) {
		t.Fatalf("encode != golden: %x != %x", got, golden)
	}
	var dec InspectWaitRequest
	if err := dec.Unmarshal(golden); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if err := ValidateSchemaIdentity(dec.Schema, "nlos.sabi.WaitControl"); err != nil {
		t.Fatalf("validate schema: %v", err)
	}
	if !bytes.Equal(dec.WaitID, seq(0x70, 16)) || !bytes.Equal(dec.Marshal(), golden) {
		t.Fatal("InspectWaitRequest roundtrip mismatch")
	}
}

func TestEpochV10PrincipalHandshakeDualGoldens(t *testing.T) {
	gChal := epochV10Bytes(t, "nlos.sabi.PrincipalHandshake.Challenge-v1.0.hex")
	chal := &PrincipalHandshakeChallenge{
		Schema: &SchemaIdentity{Name: "nlos.sabi.PrincipalHandshake", Major: 1},
		Nonce:  repeat(0xa5, 32),
	}
	if got := chal.Marshal(); !bytes.Equal(got, gChal) {
		t.Fatalf("encode Challenge != golden: %x != %x", got, gChal)
	}
	var decChal PrincipalHandshakeChallenge
	if err := decChal.Unmarshal(gChal); err != nil {
		t.Fatalf("decode Challenge: %v", err)
	}
	if err := ValidateSchemaIdentity(decChal.Schema, "nlos.sabi.PrincipalHandshake"); err != nil {
		t.Fatalf("validate Challenge schema: %v", err)
	}
	if !bytes.Equal(decChal.Nonce, repeat(0xa5, 32)) || !bytes.Equal(decChal.Marshal(), gChal) {
		t.Fatal("Challenge roundtrip mismatch")
	}

	gAtt := epochV10Bytes(t, "nlos.sabi.PrincipalHandshake.Attestation-v1.0.hex")
	att := &PrincipalHandshakeAttestation{
		Schema: &SchemaIdentity{
			Name:                    "nlos.sabi.PrincipalHandshake",
			Major:                   1,
			NonCriticalExtensionIDs: []uint32{101},
		},
		PrincipalID:    seq(0x80, 16),
		Nonce:          repeat(0xa5, 32),
		ChannelBinding: []byte("unix:///tmp/nlos-handshake-v1.0.sock"),
		Signature:      repeat(0xcd, 64),
	}
	if got := att.Marshal(); !bytes.Equal(got, gAtt) {
		t.Fatalf("encode Attestation != golden: %x != %x", got, gAtt)
	}
	var decAtt PrincipalHandshakeAttestation
	if err := decAtt.Unmarshal(gAtt); err != nil {
		t.Fatalf("decode Attestation: %v", err)
	}
	if err := ValidateSchemaIdentity(decAtt.Schema, "nlos.sabi.PrincipalHandshake"); err != nil {
		t.Fatalf("validate Attestation schema: %v", err)
	}
	if len(decAtt.Schema.NonCriticalExtensionIDs) != 1 || decAtt.Schema.NonCriticalExtensionIDs[0] != 101 ||
		!bytes.Equal(decAtt.PrincipalID, seq(0x80, 16)) ||
		string(decAtt.ChannelBinding) != "unix:///tmp/nlos-handshake-v1.0.sock" ||
		!bytes.Equal(decAtt.Marshal(), gAtt) {
		t.Fatal("Attestation v1.0 roundtrip mismatch")
	}
}

func TestEpochV10DriverProviderGolden(t *testing.T) {
	golden := epochV10Bytes(t, "nlos.sabi.DriverProvider.Request-v1.0.hex")
	req := &DriverProviderRequest{
		Schema: &SchemaIdentity{Name: "nlos.sabi.DriverProvider", Major: 1},
		Register: &DriverProviderRegisterRequest{
			OperationID:            repeat(0x91, 16),
			OperationGeneration:    2,
			OwnerFiberID:           repeat(0x92, 16),
			OwnerFiberGeneration:   3,
			CancellationScopeID:    repeat(0x93, 16),
			CancellationGeneration: 4,
		},
	}
	if got := req.Marshal(); !bytes.Equal(got, golden) {
		t.Fatalf("encode != golden: %x != %x", got, golden)
	}
	var dec DriverProviderRequest
	if err := dec.Unmarshal(golden); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if err := ValidateSchemaIdentity(dec.Schema, "nlos.sabi.DriverProvider"); err != nil {
		t.Fatalf("validate schema: %v", err)
	}
	if dec.Register == nil || dec.Complete != nil ||
		!bytes.Equal(dec.Register.OperationID, repeat(0x91, 16)) ||
		dec.Register.OperationGeneration != 2 ||
		dec.Register.OwnerFiberGeneration != 3 ||
		dec.Register.CancellationGeneration != 4 ||
		!bytes.Equal(dec.Marshal(), golden) {
		t.Fatalf("DriverProviderRequest roundtrip mismatch: %+v", dec.Register)
	}
}

func TestDriverProviderOneofBothArmsPanics(t *testing.T) {
	defer func() {
		if recover() == nil {
			t.Fatal("marshal with both DriverProviderRequest oneof arms set must panic")
		}
	}()
	(&DriverProviderRequest{
		Register: &DriverProviderRegisterRequest{},
		Complete: &DriverProviderCompleteRequest{},
	}).Marshal()
}

func TestEpochV10ChannelControlGolden(t *testing.T) {
	golden := epochV10Bytes(t, "nlos.sabi.ChannelControl.CreateChannelRequestWire-v1.0.hex")
	req := &CreateChannelRequestWire{
		Schema:         &SchemaIdentity{Name: "nlos.sabi.ChannelControl", Major: 1},
		CapacityBytes:  65536,
		PolicyDigest:   repeat(0xb1, 32),
		IdempotencyKey: repeat(0xb2, 16),
		CreatedAtMS:    1730000000000,
	}
	if got := req.Marshal(); !bytes.Equal(got, golden) {
		t.Fatalf("encode != golden: %x != %x", got, golden)
	}
	var dec CreateChannelRequestWire
	if err := dec.Unmarshal(golden); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if err := ValidateSchemaIdentity(dec.Schema, "nlos.sabi.ChannelControl"); err != nil {
		t.Fatalf("validate schema: %v", err)
	}
	if dec.CapacityBytes != 65536 ||
		!bytes.Equal(dec.PolicyDigest, repeat(0xb1, 32)) ||
		!bytes.Equal(dec.IdempotencyKey, repeat(0xb2, 16)) ||
		dec.CreatedAtMS != 1730000000000 ||
		!bytes.Equal(dec.Marshal(), golden) {
		t.Fatalf("CreateChannelRequestWire roundtrip mismatch: %+v", dec)
	}
}

func TestEpochV10UnknownFieldRoundtripAllChannels(t *testing.T) {
	tail := []byte{0xa0, 0x06, 0x07}
	cases := []struct {
		file string
		run  func([]byte) ([]byte, error)
	}{
		{"nlos.sabi.Envelope-v1.0.hex", func(b []byte) ([]byte, error) {
			var m Envelope
			err := m.Unmarshal(b)
			return m.Marshal(), err
		}},
		{"nlos.sabi.ServiceDirectory.ResolveServiceRequest-v1.0.hex", func(b []byte) ([]byte, error) {
			var m ResolveServiceRequest
			err := m.Unmarshal(b)
			return m.Marshal(), err
		}},
		{"nlos.sabi.OperationControl.QueryOperationRequest-v1.0.hex", func(b []byte) ([]byte, error) {
			var m QueryOperationRequest
			err := m.Unmarshal(b)
			return m.Marshal(), err
		}},
		{"nlos.sabi.SystemControl.GetSystemControlRequest-v1.0.hex", func(b []byte) ([]byte, error) {
			var m GetSystemControlRequest
			err := m.Unmarshal(b)
			return m.Marshal(), err
		}},
		{"nlos.sabi.TakeoverControl.SubmitBarrierObservationRequest-v1.0.hex", func(b []byte) ([]byte, error) {
			var m SubmitBarrierObservationRequest
			err := m.Unmarshal(b)
			return m.Marshal(), err
		}},
		{"nlos.sabi.WaitControl.InspectWaitRequest-v1.0.hex", func(b []byte) ([]byte, error) {
			var m InspectWaitRequest
			err := m.Unmarshal(b)
			return m.Marshal(), err
		}},
		{"nlos.sabi.PrincipalHandshake.Challenge-v1.0.hex", func(b []byte) ([]byte, error) {
			var m PrincipalHandshakeChallenge
			err := m.Unmarshal(b)
			return m.Marshal(), err
		}},
		{"nlos.sabi.PrincipalHandshake.Attestation-v1.0.hex", func(b []byte) ([]byte, error) {
			var m PrincipalHandshakeAttestation
			err := m.Unmarshal(b)
			return m.Marshal(), err
		}},
		{"nlos.sabi.DriverProvider.Request-v1.0.hex", func(b []byte) ([]byte, error) {
			var m DriverProviderRequest
			err := m.Unmarshal(b)
			return m.Marshal(), err
		}},
		{"nlos.sabi.ChannelControl.CreateChannelRequestWire-v1.0.hex", func(b []byte) ([]byte, error) {
			var m CreateChannelRequestWire
			err := m.Unmarshal(b)
			return m.Marshal(), err
		}},
	}
	for _, tc := range cases {
		base := epochV10Bytes(t, tc.file)
		extended := append(append([]byte{}, base...), tail...)
		reencoded, err := tc.run(extended)
		if err != nil {
			t.Fatalf("%s decode with unknown field failed: %v", tc.file, err)
		}
		if !bytes.Equal(reencoded, extended) {
			t.Fatalf("%s re-encode dropped unknown field:\n got  %x\n want %x", tc.file, reencoded, extended)
		}
	}
}

func TestCompatVer002SchemaIdentityValidation(t *testing.T) {
	if err := ValidateSchemaIdentity(nil, ""); !errors.Is(err, ErrMissingSchemaIdentity) {
		t.Fatalf("nil identity must fail with ErrMissingSchemaIdentity, got %v", err)
	}
	if err := ValidateSchemaIdentity(&SchemaIdentity{Name: "nlos.sabi.Unknown", Major: 1}, ""); !errors.Is(err, ErrUnknownSchema) {
		t.Fatalf("unknown schema must fail with ErrUnknownSchema, got %v", err)
	}
	for ch := range SabiV1Channels {
		for _, badMajor := range []uint32{0, 2, 99} {
			err := ValidateSchemaIdentity(&SchemaIdentity{Name: ch, Major: badMajor}, ch)
			if !errors.Is(err, ErrAbiVersion) {
				t.Fatalf("%s major=%d must fail with ErrAbiVersion, got %v", ch, badMajor, err)
			}
		}
		for _, badCrit := range [][]uint32{{7001}, {0}, {42, 7001}} {
			err := ValidateSchemaIdentity(&SchemaIdentity{
				Name:                 ch,
				Major:                1,
				CriticalExtensionIDs: badCrit,
			}, ch)
			if !errors.Is(err, ErrUnsupportedCriticalExtension) {
				t.Fatalf("%s critical=%v must fail with ErrUnsupportedCriticalExtension, got %v", ch, badCrit, err)
			}
		}
		if err := ValidateSchemaIdentity(&SchemaIdentity{
			Name:                    ch,
			Major:                   1,
			Minor:                   99,
			NonCriticalExtensionIDs: []uint32{42, 7001, 9999},
		}, ch); err != nil {
			t.Fatalf("%s non-critical extensions must pass, got %v", ch, err)
		}
	}
}
