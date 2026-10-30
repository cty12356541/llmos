// Hand-written C# mirrors of the frozen v1-beta nlos.sabi.v1 message family
// (schema/nlos/sabi/v1/*.proto) plus the additive ADR-0011
// PrincipalHandshake family and the frozen ServiceDirectory resolve entry
// message, sufficient for the B-SDK-LANG-EVAL C# golden probe against
// schema/golden/*.hex.
//
// This is a probe, NOT the full C# SDK: only the wire surface exercised by
// the frozen goldens is implemented, marshaling is deterministic by
// construction (ascending field number, proto3 implicit presence, packed
// repeated scalars), and the common-context oneof is modeled as two plain
// reference fields guarded at marshal time. Mirrors sdk/go/sabi/messages.go.
// See docs/evidence/stage-b/b-sdk-csharp-001-golden-probe.md for the probe
// scope and known limitations.
namespace Llmos.Sabi.Probe;

// Typed compatibility failure raised by SabiRegistry.ValidateSchemaIdentity ([COMPAT-VER-002]).
public sealed class SabiCompatibilityException : Exception
{
    public string Code { get; }

    public SabiCompatibilityException(string code, string message) : base(message) => Code = code;
}

public sealed record ChannelDescriptor(
    string Name,
    uint Major,
    uint Minor,
    uint[] SupportedCriticalExtensions);

public static class SabiRegistry
{
    public const string CodeMissingSchemaIdentity = "MISSING_SCHEMA_IDENTITY";
    public const string CodeUnknownSchema = "UNKNOWN_SCHEMA";
    public const string CodeAbiVersion = "E_ABI_VERSION";
    public const string CodeUnsupportedCriticalExtension = "UNSUPPORTED_CRITICAL_EXTENSION";

    // Mirrors nlos-schema::REGISTRY (all 9 frozen SABI v1.0 channels).
    public static readonly IReadOnlyDictionary<string, ChannelDescriptor> SabiV1Channels =
        new Dictionary<string, ChannelDescriptor>(StringComparer.Ordinal)
        {
            ["nlos.sabi.Envelope"] = new("nlos.sabi.Envelope", 1, 1, Array.Empty<uint>()),
            ["nlos.sabi.ServiceDirectory"] = new("nlos.sabi.ServiceDirectory", 1, 0, Array.Empty<uint>()),
            ["nlos.sabi.OperationControl"] = new("nlos.sabi.OperationControl", 1, 0, Array.Empty<uint>()),
            ["nlos.sabi.SystemControl"] = new("nlos.sabi.SystemControl", 1, 7, Array.Empty<uint>()),
            ["nlos.sabi.TakeoverControl"] = new("nlos.sabi.TakeoverControl", 1, 0, Array.Empty<uint>()),
            ["nlos.sabi.WaitControl"] = new("nlos.sabi.WaitControl", 1, 0, Array.Empty<uint>()),
            ["nlos.sabi.PrincipalHandshake"] = new("nlos.sabi.PrincipalHandshake", 1, 0, Array.Empty<uint>()),
            ["nlos.sabi.DriverProvider"] = new("nlos.sabi.DriverProvider", 1, 0, Array.Empty<uint>()),
            ["nlos.sabi.ChannelControl"] = new("nlos.sabi.ChannelControl", 1, 0, Array.Empty<uint>()),
        };

    // Enforces [COMPAT-VER-002]: unknown major and unknown critical_extension_ids
    // fail closed; unknown non_critical_extension_ids and higher minor versions
    // are accepted.
    public static void ValidateSchemaIdentity(SchemaIdentity? id, string expectedName = "")
    {
        if (id == null || string.IsNullOrEmpty(id.Name) || id.Name.Contains('\0'))
        {
            throw new SabiCompatibilityException(
                CodeMissingSchemaIdentity,
                "sabi: missing or empty schema identity");
        }
        if (expectedName != "" && !string.Equals(id.Name, expectedName, StringComparison.Ordinal))
        {
            throw new SabiCompatibilityException(
                CodeUnknownSchema,
                $"sabi: mismatched schema name: got {id.Name}, want {expectedName}");
        }
        if (!SabiV1Channels.TryGetValue(id.Name, out ChannelDescriptor? desc))
        {
            throw new SabiCompatibilityException(
                CodeUnknownSchema,
                $"sabi: unknown schema name: {id.Name}");
        }
        if (id.Major != desc.Major)
        {
            throw new SabiCompatibilityException(
                CodeAbiVersion,
                $"sabi: E_ABI_VERSION unsupported major={id.Major} for {id.Name} (want {desc.Major})");
        }
        foreach (uint ext in id.CriticalExtensionIDs)
        {
            if (ext == 0 || Array.IndexOf(desc.SupportedCriticalExtensions, ext) < 0)
            {
                throw new SabiCompatibilityException(
                    CodeUnsupportedCriticalExtension,
                    $"sabi: unsupported critical_extension_id={ext} for {id.Name}");
            }
        }
    }
}

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

// Mirrors nlos.sabi.v1.ResolveServiceRequest, the frozen ServiceDirectory
// resolve entry message (schema name on the wire:
// "nlos.sabi.ServiceDirectory", v1 minor 0).
public sealed class ResolveServiceRequest
{
    public SchemaIdentity? Schema { get; set; }
    public string Service { get; set; } = "";

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

// Mirrors nlos.sabi.v1.PrincipalHandshakeChallenge
// (schema name on the wire: "nlos.sabi.PrincipalHandshake", v1 minor 0).
public sealed class PrincipalHandshakeChallenge
{
    public SchemaIdentity? Schema { get; set; }
    public byte[] Nonce { get; set; } = Array.Empty<byte>();

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

// Mirrors nlos.sabi.v1.QueryOperationRequest
// (schema name on the wire: "nlos.sabi.OperationControl", v1 minor 0).
public sealed class QueryOperationRequest
{
    public SchemaIdentity? Schema { get; set; }
    public OperationReference? Operation { get; set; }

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

// Selected wire values of nlos.sabi.v1.SystemControlView.
public enum SystemControlView : uint
{
    Unspecified = 0,
    ArtifactCommitRecovery = 1,
    TaskNode = 5,
    TaskAttemptList = 12,
}

// Mirrors nlos.sabi.v1.GetSystemControlRequest
// (schema name on the wire: "nlos.sabi.SystemControl", v1 minor 7).
public sealed class GetSystemControlRequest
{
    public SchemaIdentity? Schema { get; set; }
    public SystemControlView View { get; set; }
    public uint AlertLimit { get; set; }
    public byte[] TargetID { get; set; } = Array.Empty<byte>();
    public byte[] PlanID { get; set; } = Array.Empty<byte>();
    public ulong TargetGeneration { get; set; }

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

// Mirrors nlos.sabi.v1.BarrierObservationTarget.
public sealed class BarrierObservationTarget
{
    public byte[] TakeoverReceiptID { get; set; } = Array.Empty<byte>();
    public uint ParticipantType { get; set; }
    public byte[] ParticipantID { get; set; } = Array.Empty<byte>();
    public ulong ParticipantGeneration { get; set; }
    public byte[] AdmissionReceiptID { get; set; } = Array.Empty<byte>();

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

// Mirrors nlos.sabi.v1.BarrierObservationEvidence.
public sealed class BarrierObservationEvidence
{
    public byte[] RemoteReceiptID { get; set; } = Array.Empty<byte>();
    public byte[] BarrierDigest { get; set; } = Array.Empty<byte>();
    public long ObservedAtMS { get; set; }

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

// Mirrors nlos.sabi.v1.BarrierObservationSignature.
public sealed class BarrierObservationSignature
{
    public byte[] SignerPrincipalID { get; set; } = Array.Empty<byte>();
    public byte[] SignerControlDomainID { get; set; } = Array.Empty<byte>();
    public byte[] SignerKeyID { get; set; } = Array.Empty<byte>();
    public byte[] Signature { get; set; } = Array.Empty<byte>();

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

// Mirrors nlos.sabi.v1.SubmitBarrierObservationRequest
// (schema name on the wire: "nlos.sabi.TakeoverControl", v1 minor 0).
public sealed class SubmitBarrierObservationRequest
{
    public SchemaIdentity? Schema { get; set; }
    public BarrierObservationTarget? Target { get; set; }
    public BarrierObservationEvidence? Evidence { get; set; }
    public BarrierObservationSignature? Signature { get; set; }

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

// Mirrors nlos.sabi.v1.InspectWaitRequest
// (schema name on the wire: "nlos.sabi.WaitControl", v1 minor 0).
public sealed class InspectWaitRequest
{
    public SchemaIdentity? Schema { get; set; }
    public byte[] WaitID { get; set; } = Array.Empty<byte>();

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

// Mirrors nlos.sabi.v1.DriverProviderRegisterRequest.
public sealed class DriverProviderRegisterRequest
{
    public byte[] OperationID { get; set; } = Array.Empty<byte>();
    public ulong OperationGeneration { get; set; }
    public byte[] OwnerFiberID { get; set; } = Array.Empty<byte>();
    public ulong OwnerFiberGeneration { get; set; }
    public byte[] CancellationScopeID { get; set; } = Array.Empty<byte>();
    public ulong CancellationGeneration { get; set; }

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

// Mirrors nlos.sabi.v1.DriverProviderCompleteRequest.
public sealed class DriverProviderCompleteRequest
{
    public byte[] OperationID { get; set; } = Array.Empty<byte>();
    public ulong OperationGeneration { get; set; }
    public byte[] CallbackID { get; set; } = Array.Empty<byte>();
    public byte[] Seed { get; set; } = Array.Empty<byte>();

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

// Mirrors nlos.sabi.v1.DriverProviderRequest
// (schema name on the wire: "nlos.sabi.DriverProvider", v1 minor 0).
// At most one arm of the request oneof may be non-null (guarded at Marshal).
public sealed class DriverProviderRequest
{
    public SchemaIdentity? Schema { get; set; }
    public DriverProviderRegisterRequest? Register { get; set; } // oneof arm (field 2)
    public DriverProviderCompleteRequest? Complete { get; set; } // oneof arm (field 4)

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}

// Mirrors nlos.sabi.v1.CreateChannelRequestWire
// (schema name on the wire: "nlos.sabi.ChannelControl", v1 minor 0).
public sealed class CreateChannelRequestWire
{
    public SchemaIdentity? Schema { get; set; }
    public ulong CapacityBytes { get; set; }
    public byte[] PolicyDigest { get; set; } = Array.Empty<byte>();
    public byte[] IdempotencyKey { get; set; } = Array.Empty<byte>();
    public ulong CreatedAtMS { get; set; }

    public byte[] UnknownFields { get; set; } = Array.Empty<byte>();
}
