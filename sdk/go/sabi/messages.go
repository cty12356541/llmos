// Package sabi contains hand-written Go mirrors of the frozen v1-beta
// nlos.sabi.v1 message family (schema/nlos/sabi/v1/*.proto) plus the additive
// ADR-0011 PrincipalHandshake family and the frozen ServiceDirectory resolve
// entry message, sufficient for the B-SDK-LANG-EVAL Go golden probe against
// schema/golden/*.hex.
//
// This is a probe, NOT the full Go SDK: only the wire surface exercised by
// the frozen goldens is implemented, marshaling is deterministic by
// construction (ascending field number, proto3 implicit presence, packed
// repeated scalars), and the common-context oneof is modeled as two plain
// pointer fields guarded at marshal time. See docs/evidence/stage-b/
// b-sdk-go-001-golden-probe.md for the probe scope and known limitations.
package sabi

import (
	"errors"
	"fmt"
	"strings"
)

// Sentinel errors returned by ValidateSchemaIdentity ([COMPAT-VER-002]).
var (
	ErrMissingSchemaIdentity        = errors.New("sabi: missing or empty schema identity")
	ErrUnknownSchema                = errors.New("sabi: unknown or mismatched schema name")
	ErrAbiVersion                   = errors.New("sabi: E_ABI_VERSION unsupported major version")
	ErrUnsupportedCriticalExtension = errors.New("sabi: unsupported critical extension id")
)

// ChannelDescriptor describes one registered SABI v1.0 channel.
type ChannelDescriptor struct {
	Name                        string
	Major                       uint32
	Minor                       uint32
	SupportedCriticalExtensions []uint32
}

// SabiV1Channels mirrors nlos-schema::REGISTRY (all 9 frozen SABI v1.0 channels).
var SabiV1Channels = map[string]ChannelDescriptor{
	"nlos.sabi.Envelope":           {Name: "nlos.sabi.Envelope", Major: 1, Minor: 1},
	"nlos.sabi.ServiceDirectory":   {Name: "nlos.sabi.ServiceDirectory", Major: 1, Minor: 0},
	"nlos.sabi.OperationControl":   {Name: "nlos.sabi.OperationControl", Major: 1, Minor: 0},
	"nlos.sabi.SystemControl":      {Name: "nlos.sabi.SystemControl", Major: 1, Minor: 7},
	"nlos.sabi.TakeoverControl":    {Name: "nlos.sabi.TakeoverControl", Major: 1, Minor: 0},
	"nlos.sabi.WaitControl":        {Name: "nlos.sabi.WaitControl", Major: 1, Minor: 0},
	"nlos.sabi.PrincipalHandshake": {Name: "nlos.sabi.PrincipalHandshake", Major: 1, Minor: 0},
	"nlos.sabi.DriverProvider":     {Name: "nlos.sabi.DriverProvider", Major: 1, Minor: 0},
	"nlos.sabi.ChannelControl":     {Name: "nlos.sabi.ChannelControl", Major: 1, Minor: 0},
}

// ValidateSchemaIdentity enforces [COMPAT-VER-002]: unknown major and unknown
// critical_extension_ids fail closed; unknown non_critical_extension_ids and
// higher minor versions are accepted.
func ValidateSchemaIdentity(id *SchemaIdentity, expectedName string) error {
	if id == nil || id.Name == "" || strings.ContainsRune(id.Name, '\x00') {
		return ErrMissingSchemaIdentity
	}
	if expectedName != "" && id.Name != expectedName {
		return fmt.Errorf("%w: got %q, want %q", ErrUnknownSchema, id.Name, expectedName)
	}
	desc, ok := SabiV1Channels[id.Name]
	if !ok {
		return fmt.Errorf("%w: %q", ErrUnknownSchema, id.Name)
	}
	if id.Major != desc.Major {
		return fmt.Errorf("%w: schema %q major=%d (want %d)", ErrAbiVersion, id.Name, id.Major, desc.Major)
	}
	for _, ext := range id.CriticalExtensionIDs {
		if ext == 0 || !containsUint32(desc.SupportedCriticalExtensions, ext) {
			return fmt.Errorf("%w: schema %q critical_extension_id=%d", ErrUnsupportedCriticalExtension, id.Name, ext)
		}
	}
	return nil
}

func containsUint32(xs []uint32, target uint32) bool {
	for _, x := range xs {
		if x == target {
			return true
		}
	}
	return false
}

// SabiErrorCode mirrors the frozen nlos.sabi.v1.SabiErrorCode enum.
type SabiErrorCode uint32

// Frozen wire values of nlos.sabi.v1.SabiErrorCode.
const (
	SabiErrorCodeUnspecified     SabiErrorCode = 0
	SabiErrorCodeAbiVersion      SabiErrorCode = 1
	SabiErrorCodeInvalidArgument SabiErrorCode = 2
	SabiErrorCodeNotFound        SabiErrorCode = 3
	SabiErrorCodeRights          SabiErrorCode = 4
	SabiErrorCodeState           SabiErrorCode = 5
	SabiErrorCodeBudget          SabiErrorCode = 6
	SabiErrorCodeQuota           SabiErrorCode = 7
	SabiErrorCodeFenced          SabiErrorCode = 8
	SabiErrorCodeDeadline        SabiErrorCode = 9
	SabiErrorCodeCancelled       SabiErrorCode = 10
	SabiErrorCodeConflict        SabiErrorCode = 11
	SabiErrorCodeDurability      SabiErrorCode = 12
	SabiErrorCodeUncertain       SabiErrorCode = 13
	SabiErrorCodeDriver          SabiErrorCode = 14
	SabiErrorCodeHostLost        SabiErrorCode = 15
	SabiErrorCodeNotSupported    SabiErrorCode = 16
	SabiErrorCodeRetry           SabiErrorCode = 17
	SabiErrorCodePartial         SabiErrorCode = 18
	SabiErrorCodeEffectUnknown   SabiErrorCode = 19
)

// RetryDirective mirrors the frozen nlos.sabi.v1.RetryDirective enum.
type RetryDirective uint32

// Frozen wire values of nlos.sabi.v1.RetryDirective.
const (
	RetryDirectiveUnspecified                             RetryDirective = 0
	RetryDirectiveDoNotRetry                              RetryDirective = 1
	RetryDirectiveRetrySameIdempotencyKey                 RetryDirective = 2
	RetryDirectiveQueryOperationOrRetrySameIdempotencyKey RetryDirective = 3
)

// SchemaIdentity mirrors nlos.sabi.v1.SchemaIdentity.
type SchemaIdentity struct {
	Name                    string   `wire:"1"` // string
	Major                   uint32   `wire:"2"` // varint
	Minor                   uint32   `wire:"3"` // varint
	CriticalExtensionIDs    []uint32 `wire:"4"` // packed varint
	NonCriticalExtensionIDs []uint32 `wire:"5"` // packed varint

	// UnknownFields preserves raw unknown field bytes (tag + value) captured
	// during decode; marshal appends them verbatim after the known fields,
	// matching protobuf-go deterministic serialization.
	UnknownFields []byte `wire:"-"`
}

// CallerIdentity mirrors nlos.sabi.v1.CallerIdentity.
type CallerIdentity struct {
	PrincipalID       []byte `wire:"1"`
	ApplicationID     []byte `wire:"2"`
	ProcessID         []byte `wire:"3"`
	ProcessGeneration uint64 `wire:"4"`

	UnknownFields []byte `wire:"-"`
}

// TaskExecutionBinding mirrors nlos.sabi.v1.TaskExecutionBinding.
type TaskExecutionBinding struct {
	TaskAttemptID             []byte `wire:"1"`
	TaskAuthorityTerm         uint64 `wire:"2"`
	TaskControlEpoch          uint64 `wire:"3"`
	CancelEpoch               uint64 `wire:"4"`
	PermitEpoch               uint64 `wire:"5"`
	IsolationDomainGeneration uint64 `wire:"6"`

	UnknownFields []byte `wire:"-"`
}

// CapabilityHandle mirrors nlos.sabi.v1.CapabilityHandle.
type CapabilityHandle struct {
	Slot       uint64 `wire:"1"`
	Generation uint64 `wire:"2"`

	UnknownFields []byte `wire:"-"`
}

// SabiRequestContext mirrors nlos.sabi.v1.SabiRequestContext.
type SabiRequestContext struct {
	Caller                      *CallerIdentity       `wire:"1"`
	ActivityContext             []byte                `wire:"2"`
	TaskExecutionBinding        *TaskExecutionBinding `wire:"3"`
	CorrelationID               []byte                `wire:"4"`
	IdempotencyKey              []byte                `wire:"5"`
	DeadlineMonotonicNS         uint64                `wire:"6"`
	CapabilityHandles           []*CapabilityHandle   `wire:"7"`
	ReservationHandle           *CapabilityHandle     `wire:"8"`
	ProposalOrInputDigestSHA256 []byte                `wire:"9"`

	UnknownFields []byte `wire:"-"`
}

// SabiFailure mirrors nlos.sabi.v1.SabiFailure.
type SabiFailure struct {
	Code        SabiErrorCode  `wire:"1"`
	Retry       RetryDirective `wire:"2"`
	SafeMessage string         `wire:"3"`

	UnknownFields []byte `wire:"-"`
}

// OperationReference mirrors nlos.sabi.v1.OperationReference.
type OperationReference struct {
	OperationID []byte `wire:"1"`
	Generation  uint64 `wire:"2"`

	UnknownFields []byte `wire:"-"`
}

// ReceiptReference mirrors nlos.sabi.v1.ReceiptReference.
type ReceiptReference struct {
	ReceiptID []byte `wire:"1"`

	UnknownFields []byte `wire:"-"`
}

// SabiResponseContext mirrors nlos.sabi.v1.SabiResponseContext.
type SabiResponseContext struct {
	CorrelationID []byte              `wire:"1"`
	Operation     *OperationReference `wire:"2"`
	Receipts      []*ReceiptReference `wire:"3"`
	Failure       *SabiFailure        `wire:"4"`

	UnknownFields []byte `wire:"-"`
}

// Envelope mirrors nlos.sabi.v1.Envelope. The common_context oneof is
// modeled as two pointer arms: at most one may be non-nil (guarded at
// marshal time); both nil means the oneof is unset.
type Envelope struct {
	Schema          *SchemaIdentity      `wire:"1"`
	RequestID       []byte               `wire:"2"`
	Service         string               `wire:"3"`
	Method          string               `wire:"4"`
	RequestContext  *SabiRequestContext  `wire:"5"` // oneof arm
	ResponseContext *SabiResponseContext `wire:"6"` // oneof arm
	Payload         []byte               `wire:"15"`

	UnknownFields []byte `wire:"-"`
}

// PrincipalHandshakeAttestation mirrors
// nlos.sabi.v1.PrincipalHandshakeAttestation (ADR-0011 additive family;
// the schema name carried on the wire is "nlos.sabi.PrincipalHandshake").
type PrincipalHandshakeAttestation struct {
	Schema         *SchemaIdentity `wire:"1"`
	PrincipalID    []byte          `wire:"2"`
	Nonce          []byte          `wire:"3"`
	ChannelBinding []byte          `wire:"4"`
	Signature      []byte          `wire:"5"`

	UnknownFields []byte `wire:"-"`
}

// ResolveServiceRequest mirrors nlos.sabi.v1.ResolveServiceRequest, the
// frozen ServiceDirectory resolve entry message (schema name on the wire:
// "nlos.sabi.ServiceDirectory", v1 minor 0).
type ResolveServiceRequest struct {
	Schema  *SchemaIdentity `wire:"1"`
	Service string          `wire:"2"`

	UnknownFields []byte `wire:"-"`
}

// PrincipalHandshakeChallenge mirrors nlos.sabi.v1.PrincipalHandshakeChallenge
// (schema name on the wire: "nlos.sabi.PrincipalHandshake", v1 minor 0).
type PrincipalHandshakeChallenge struct {
	Schema *SchemaIdentity `wire:"1"`
	Nonce  []byte          `wire:"2"`

	UnknownFields []byte `wire:"-"`
}

// QueryOperationRequest mirrors nlos.sabi.v1.QueryOperationRequest
// (schema name on the wire: "nlos.sabi.OperationControl", v1 minor 0).
type QueryOperationRequest struct {
	Schema    *SchemaIdentity     `wire:"1"`
	Operation *OperationReference `wire:"2"`

	UnknownFields []byte `wire:"-"`
}

// SystemControlView mirrors selected frozen nlos.sabi.v1.SystemControlView values.
type SystemControlView uint32

// Selected wire values of nlos.sabi.v1.SystemControlView.
const (
	SystemControlViewUnspecified            SystemControlView = 0
	SystemControlViewArtifactCommitRecovery SystemControlView = 1
	SystemControlViewTaskNode               SystemControlView = 5
	SystemControlViewTaskAttemptList        SystemControlView = 12
)

// GetSystemControlRequest mirrors nlos.sabi.v1.GetSystemControlRequest
// (schema name on the wire: "nlos.sabi.SystemControl", v1 minor 7).
type GetSystemControlRequest struct {
	Schema           *SchemaIdentity   `wire:"1"`
	View             SystemControlView `wire:"2"`
	AlertLimit       uint32            `wire:"3"`
	TargetID         []byte            `wire:"4"`
	PlanID           []byte            `wire:"5"`
	TargetGeneration uint64            `wire:"6"`

	UnknownFields []byte `wire:"-"`
}

// BarrierObservationTarget mirrors nlos.sabi.v1.BarrierObservationTarget.
type BarrierObservationTarget struct {
	TakeoverReceiptID     []byte `wire:"1"`
	ParticipantType       uint32 `wire:"2"`
	ParticipantID         []byte `wire:"3"`
	ParticipantGeneration uint64 `wire:"4"`
	AdmissionReceiptID    []byte `wire:"5"`

	UnknownFields []byte `wire:"-"`
}

// BarrierObservationEvidence mirrors nlos.sabi.v1.BarrierObservationEvidence.
type BarrierObservationEvidence struct {
	RemoteReceiptID []byte `wire:"1"`
	BarrierDigest   []byte `wire:"2"`
	ObservedAtMS    int64  `wire:"3"`

	UnknownFields []byte `wire:"-"`
}

// BarrierObservationSignature mirrors nlos.sabi.v1.BarrierObservationSignature.
type BarrierObservationSignature struct {
	SignerPrincipalID     []byte `wire:"1"`
	SignerControlDomainID []byte `wire:"2"`
	SignerKeyID           []byte `wire:"3"`
	Signature             []byte `wire:"4"`

	UnknownFields []byte `wire:"-"`
}

// SubmitBarrierObservationRequest mirrors nlos.sabi.v1.SubmitBarrierObservationRequest
// (schema name on the wire: "nlos.sabi.TakeoverControl", v1 minor 0).
type SubmitBarrierObservationRequest struct {
	Schema    *SchemaIdentity              `wire:"1"`
	Target    *BarrierObservationTarget    `wire:"2"`
	Evidence  *BarrierObservationEvidence  `wire:"3"`
	Signature *BarrierObservationSignature `wire:"4"`

	UnknownFields []byte `wire:"-"`
}

// InspectWaitRequest mirrors nlos.sabi.v1.InspectWaitRequest
// (schema name on the wire: "nlos.sabi.WaitControl", v1 minor 0).
type InspectWaitRequest struct {
	Schema *SchemaIdentity `wire:"1"`
	WaitID []byte          `wire:"2"`

	UnknownFields []byte `wire:"-"`
}

// DriverProviderRegisterRequest mirrors nlos.sabi.v1.DriverProviderRegisterRequest.
type DriverProviderRegisterRequest struct {
	OperationID            []byte `wire:"1"`
	OperationGeneration    uint64 `wire:"2"`
	OwnerFiberID           []byte `wire:"3"`
	OwnerFiberGeneration   uint64 `wire:"4"`
	CancellationScopeID    []byte `wire:"5"`
	CancellationGeneration uint64 `wire:"6"`

	UnknownFields []byte `wire:"-"`
}

// DriverProviderCompleteRequest mirrors nlos.sabi.v1.DriverProviderCompleteRequest.
type DriverProviderCompleteRequest struct {
	OperationID         []byte `wire:"1"`
	OperationGeneration uint64 `wire:"2"`
	CallbackID          []byte `wire:"3"`
	Seed                []byte `wire:"4"`

	UnknownFields []byte `wire:"-"`
}

// DriverProviderRequest mirrors nlos.sabi.v1.DriverProviderRequest
// (schema name on the wire: "nlos.sabi.DriverProvider", v1 minor 0).
// At most one arm of the request oneof may be non-nil (guarded at Marshal).
type DriverProviderRequest struct {
	Schema   *SchemaIdentity                `wire:"1"`
	Register *DriverProviderRegisterRequest `wire:"2"` // oneof arm
	Complete *DriverProviderCompleteRequest `wire:"4"` // oneof arm

	UnknownFields []byte `wire:"-"`
}

// CreateChannelRequestWire mirrors nlos.sabi.v1.CreateChannelRequestWire
// (schema name on the wire: "nlos.sabi.ChannelControl", v1 minor 0).
type CreateChannelRequestWire struct {
	Schema         *SchemaIdentity `wire:"1"`
	CapacityBytes  uint64          `wire:"2"`
	PolicyDigest   []byte          `wire:"3"`
	IdempotencyKey []byte          `wire:"4"`
	CreatedAtMS    uint64          `wire:"5"`

	UnknownFields []byte `wire:"-"`
}
