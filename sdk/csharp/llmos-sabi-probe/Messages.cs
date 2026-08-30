// Hand-written C# mirrors of the frozen v1-beta nlos.sabi.v1 message family
// (schema/nlos/sabi/v1/*.proto) plus the additive ADR-0011
// PrincipalHandshake family, sufficient for the B-SDK-LANG-EVAL C# golden
// probe against schema/golden/*.hex.
//
// This is a probe, NOT the full C# SDK: only the wire surface exercised by
// the frozen goldens is implemented, marshaling is deterministic by
// construction (ascending field number, proto3 implicit presence, packed
// repeated scalars), and the common-context oneof is modeled as two plain
// reference fields guarded at marshal time. Mirrors sdk/go/sabi/messages.go.
// See docs/evidence/stage-b/b-sdk-csharp-001-golden-probe.md for the probe
// scope and known limitations.
namespace Llmos.Sabi.Probe;

// Mirrors the frozen nlos.sabi.v1.SabiErrorCode enum.
public enum SabiErrorCode : uint
{
    Unspecified = 0,
    AbiVersion = 1,
    InvalidArgument = 2,
    NotFound = 3,
    Rights = 4,
    State = 5,
    Budget = 6,
    Quota = 7,
    Fenced = 8,
    Deadline = 9,
    Cancelled = 10,
    Conflict = 11,
    Durability = 12,
    Uncertain = 13,
    Driver = 14,
    HostLost = 15,
    NotSupported = 16,
    Retry = 17,
    Partial = 18,
    EffectUnknown = 19,
}

// Mirrors the frozen nlos.sabi.v1.RetryDirective enum.
public enum RetryDirective : uint
{
    Unspecified = 0,
    DoNotRetry = 1,
    RetrySameIdempotencyKey = 2,
    QueryOperationOrRetrySameIdempotencyKey = 3,
}

public sealed class SchemaIdentity
{
    public string Name { get; set; } = "";
    public uint Major { get; set; }
    public uint Minor { get; set; }
    public uint[] CriticalExtensionIDs { get; set; } = Array.Empty<uint>();
    public uint[] NonCriticalExtensionIDs { get; set; } = Array.Empty<uint>();

    // Raw unknown field bytes (tag + value) captured during decode; marshal
    // appends them verbatim after the known fields, mirroring protobuf-go
    // deterministic serialization.
    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

public sealed class CallerIdentity
{
    public byte[] PrincipalID { get; set; } = Array.Empty<byte>();
    public byte[] ApplicationID { get; set; } = Array.Empty<byte>();
    public byte[] ProcessID { get; set; } = Array.Empty<byte>();
    public ulong ProcessGeneration { get; set; }

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

public sealed class TaskExecutionBinding
{
    public byte[] TaskAttemptID { get; set; } = Array.Empty<byte>();
    public ulong TaskAuthorityTerm { get; set; }
    public ulong TaskControlEpoch { get; set; }
    public ulong CancelEpoch { get; set; }
    public ulong PermitEpoch { get; set; }
    public ulong IsolationDomainGeneration { get; set; }

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

public sealed class CapabilityHandle
{
    public ulong Slot { get; set; }
    public ulong Generation { get; set; }

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

public sealed class SabiRequestContext
{
    public CallerIdentity? Caller { get; set; }
    public byte[] ActivityContext { get; set; } = Array.Empty<byte>();
    public TaskExecutionBinding? TaskExecutionBinding { get; set; }
    public byte[] CorrelationID { get; set; } = Array.Empty<byte>();
    public byte[] IdempotencyKey { get; set; } = Array.Empty<byte>();
    public ulong DeadlineMonotonicNS { get; set; }
    public List<CapabilityHandle> CapabilityHandles { get; set; } = new();
    public CapabilityHandle? ReservationHandle { get; set; }
    public byte[] ProposalOrInputDigestSHA256 { get; set; } = Array.Empty<byte>();

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

public sealed class SabiFailure
{
    public SabiErrorCode Code { get; set; }
    public RetryDirective Retry { get; set; }
    public string SafeMessage { get; set; } = "";

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

public sealed class OperationReference
{
    public byte[] OperationID { get; set; } = Array.Empty<byte>();
    public ulong Generation { get; set; }

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

public sealed class ReceiptReference
{
    public byte[] ReceiptID { get; set; } = Array.Empty<byte>();

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

public sealed class SabiResponseContext
{
    public byte[] CorrelationID { get; set; } = Array.Empty<byte>();
    public OperationReference? Operation { get; set; }
    public List<ReceiptReference> Receipts { get; set; } = new();
    public SabiFailure? Failure { get; set; }

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

// Mirrors nlos.sabi.v1.Envelope. The common_context oneof is modeled as two
// reference arms: at most one may be non-null (guarded at marshal time);
// both null means the oneof is unset.
public sealed class Envelope
{
    public SchemaIdentity? Schema { get; set; }
    public byte[] RequestID { get; set; } = Array.Empty<byte>();
    public string Service { get; set; } = "";
    public string Method { get; set; } = "";
    public SabiRequestContext? RequestContext { get; set; }  // oneof arm
    public SabiResponseContext? ResponseContext { get; set; } // oneof arm
    public byte[] Payload { get; set; } = Array.Empty<byte>();

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

// Mirrors nlos.sabi.v1.PrincipalHandshakeAttestation (ADR-0011 additive
// family; the schema name carried on the wire is
// "nlos.sabi.PrincipalHandshake").
public sealed class PrincipalHandshakeAttestation
{
    public SchemaIdentity? Schema { get; set; }
    public byte[] PrincipalID { get; set; } = Array.Empty<byte>();
    public byte[] Nonce { get; set; } = Array.Empty<byte>();
    public byte[] ChannelBinding { get; set; } = Array.Empty<byte>();
    public byte[] Signature { get; set; } = Array.Empty<byte>();

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}
