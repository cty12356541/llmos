// Package sabi contains hand-written Go mirrors of the frozen v1-beta
// nlos.sabi.v1 message family (schema/nlos/sabi/v1/*.proto) plus the additive
// ADR-0011 PrincipalHandshake family, sufficient for the B-SDK-LANG-EVAL Go
// golden probe against schema/golden/*.hex.
//
// This is a probe, NOT the full Go SDK: only the wire surface exercised by
// the frozen goldens is implemented, marshaling is deterministic by
// construction (ascending field number, proto3 implicit presence, packed
// repeated scalars), and the common-context oneof is modeled as two plain
// pointer fields guarded at marshal time. See docs/evidence/stage-b/
// b-sdk-go-001-golden-probe.md for the probe scope and known limitations.
package sabi

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
