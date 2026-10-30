// B-SDK-LANG-EVAL C# golden probe driver: golden byte-equal, roundtrip,
// boundary, and fail-closed cases against the frozen v1-beta goldens
// (ADR-0014). Mirrors sdk/go/sabi/golden_test.go and sdk/go/wire/wire_test.go
// case-for-case; runs as a dependency-free console harness and exits
// non-zero if any case fails.
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using Llmos.Sabi.Probe;

// ----------------------------------------------------------------------
// Tiny assertion harness (stand-in for a test framework, which the probe
// deliberately does not pull in as a dependency).
// ----------------------------------------------------------------------

int passed = 0;
int failed = 0;
var failures = new List<string>();

void Run(string name, Action body)
{
    try
    {
        body();
        Interlocked.Increment(ref passed);
        Console.WriteLine($"[PASS] {name}");
    }
    catch (Exception ex)
    {
        Interlocked.Increment(ref failed);
        failures.Add(name);
        Console.WriteLine($"[FAIL] {name}: {ex.Message}");
    }
}

static void Check(bool cond, string what)
{
    if (!cond)
    {
        throw new Exception(what);
    }
}

static void CheckBytes(byte[] got, byte[] want, string what)
{
    if (!got.AsSpan().SequenceEqual(want))
    {
        throw new Exception($"{what}:\n got  {Hex(got)}\n want {Hex(want)}");
    }
}

static string Hex(byte[] b) => Convert.ToHexString(b).ToLowerInvariant();

// ----------------------------------------------------------------------
// Helpers mirroring the Go probe's golden_test.go.
// ----------------------------------------------------------------------

// Loads one frozen hex golden from the repository. The golden files are
// frozen (ADR-0014) and read-only for this probe.
static byte[] GoldenBytes(string name)
{
    string root = FindRepoRoot();
    string raw = File.ReadAllText(Path.Combine(root, "schema", "golden", name)).Trim();
    return Convert.FromHexString(raw);
}

static string FindRepoRoot()
{
    DirectoryInfo? dir = new(AppContext.BaseDirectory);
    for (int i = 0; i < 12 && dir != null; i++, dir = dir.Parent)
    {
        if (File.Exists(Path.Combine(dir.FullName, "schema", "golden", "nlos.sabi.Envelope-v1.hex")))
        {
            return dir.FullName;
        }
    }
    throw new Exception(
        $"repo root not found: schema/golden/nlos.sabi.Envelope-v1.hex not visible from {AppContext.BaseDirectory}");
}

// Seq returns [off, off+1, ..., off+n-1], the nominal-ID pattern used by the
// frozen goldens.
static byte[] Seq(byte off, int n)
{
    var b = new byte[n];
    for (int i = 0; i < n; i++)
    {
        b[i] = (byte)(off + i);
    }
    return b;
}

static byte[] Repeat(byte b, int n)
{
    var r = new byte[n];
    Array.Fill(r, b);
    return r;
}

static byte[] Concat(params byte[][] parts)
{
    int len = parts.Sum(p => p.Length);
    var result = new byte[len];
    int off = 0;
    foreach (byte[] p in parts)
    {
        Array.Copy(p, 0, result, off, p.Length);
        off += p.Length;
    }
    return result;
}

static void RoundtripGolden(string name, byte[] golden, Func<byte[], byte[]> decodeAndEncode)
{
    byte[] reencoded = decodeAndEncode(golden);
    CheckBytes(reencoded, golden, $"re-encode of decoded {name} diverged");
}

static void Throws(Action body, string what)
{
    try
    {
        body();
    }
    catch (Exception)
    {
        return;
    }
    throw new Exception($"{what}: expected a failure, got none");
}

static void ThrowsCompat(Action body, string expectedCode, string what)
{
    try
    {
        body();
    }
    catch (SabiCompatibilityException ex)
    {
        Check(ex.Code == expectedCode, $"{what}: expected code {expectedCode}, got {ex.Code} ({ex.Message})");
        return;
    }
    throw new Exception($"{what}: expected SabiCompatibilityException({expectedCode}), got none");
}

static string Sha256Hex(byte[] data) => Hex(SHA256.HashData(data));

static JsonDocument LoadEpochV10Manifest()
{
    string root = FindRepoRoot();
    string raw = File.ReadAllText(Path.Combine(root, "schema", "golden", "epochs", "v1.0", "manifest.json"));
    return JsonDocument.Parse(raw);
}

static byte[] EpochV10Bytes(string name)
{
    string root = FindRepoRoot();
    using JsonDocument doc = LoadEpochV10Manifest();
    JsonElement? matched = null;
    foreach (JsonElement vec in doc.RootElement.GetProperty("vectors").EnumerateArray())
    {
        if (vec.GetProperty("file").GetString() == name)
        {
            matched = vec;
            break;
        }
    }
    Check(matched != null, $"vector {name} missing from v1.0 manifest.json");
    JsonElement meta = matched!.Value;

    string fullPath = Path.Combine(root, "schema", "golden", "epochs", "v1.0", name);
    byte[] rawFile = File.ReadAllBytes(fullPath);
    string gotFileSha = Sha256Hex(rawFile);
    string wantFileSha = meta.GetProperty("sha256_file").GetString() ?? "";
    Check(gotFileSha == wantFileSha, $"{name} sha256_file mismatch: got {gotFileSha}, want {wantFileSha}");

    byte[] wire = Convert.FromHexString(Encoding.UTF8.GetString(rawFile).Trim());
    int wantLen = meta.GetProperty("byte_length").GetInt32();
    Check(wire.Length == wantLen, $"{name} byte_length mismatch: got {wire.Length}, want {wantLen}");

    string gotWireSha = Sha256Hex(wire);
    string wantWireSha = meta.GetProperty("sha256_wire").GetString() ?? "";
    Check(gotWireSha == wantWireSha, $"{name} sha256_wire mismatch: got {gotWireSha}, want {wantWireSha}");
    return wire;
}

// ----------------------------------------------------------------------
// Wire-primitive cases (mirror sdk/go/wire/wire_test.go).
// ----------------------------------------------------------------------

Run("wire: TestVarintCanonicalEncoding", () =>
{
    (ulong, byte[])[] cases =
    {
        (0, new byte[] { 0x00 }),
        (1, new byte[] { 0x01 }),
        (127, new byte[] { 0x7f }),
        (128, new byte[] { 0x80, 0x01 }),
        (300, new byte[] { 0xac, 0x02 }),
        (16383, new byte[] { 0xff, 0x7f }),
        (16384, new byte[] { 0x80, 0x80, 0x01 }),
        (int.MaxValue, new byte[] { 0xff, 0xff, 0xff, 0xff, 0x07 }),
        (uint.MaxValue, new byte[] { 0xff, 0xff, 0xff, 0xff, 0x0f }),
        (long.MaxValue, new byte[] { 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f }),
        (ulong.MaxValue, new byte[] { 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01 }),
    };
    foreach ((ulong v, byte[] want) in cases)
    {
        var b = new List<byte>();
        Wire.PutUvarint(b, v);
        CheckBytes(b.ToArray(), want, $"PutUvarint({v})");
        (ulong got, int n) = Wire.Uvarint(b.ToArray(), 0);
        Check(got == v && n == b.Count, $"Uvarint({Hex(b.ToArray())}) roundtrip");
    }
});

Run("wire: TestVarintDecodeAcceptsNonMinimal", () =>
{
    (ulong v, int n) = Wire.Uvarint(new byte[] { 0x80, 0x00 }, 0);
    Check(v == 0 && n == 2, $"non-minimal zero: got ({v}, {n})");
    (v, n) = Wire.Uvarint(new byte[] { 0x81, 0x80, 0x80, 0x00 }, 0);
    Check(v == 1 && n == 4, $"non-minimal one: got ({v}, {n})");
});

Run("wire: TestVarintDecodeErrors", () =>
{
    byte[] overflow = Concat(Repeat(0x80, 10), new byte[] { 0x01 });
    Throws(() => Wire.Uvarint(overflow, 0), "11-byte varint");
    Throws(
        () => Wire.Uvarint(new byte[] { 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02 }, 0),
        "10th-byte overflow");
    for (int cut = 1; cut < 10; cut++)
    {
        int c = cut;
        Throws(() => Wire.Uvarint(Repeat(0x80, c), 0), $"truncated varint (cut={c})");
    }
    Throws(() => Wire.Uvarint(Array.Empty<byte>(), 0), "empty input");
});

Run("wire: TestTagBoundaries", () =>
{
    var b = new List<byte>();
    Wire.PutTag(b, 15, WireType.Len);
    CheckBytes(b.ToArray(), new byte[] { 0x7a }, "field 15 key");
    b.Clear();
    Wire.PutTag(b, 16, WireType.Len);
    CheckBytes(b.ToArray(), new byte[] { 0x82, 0x01 }, "field 16 key");
    foreach (int field in new[] { 1, 15, 16, 2047, 2048, 536870911 })
    {
        var key = new List<byte>();
        Wire.PutTag(key, field, WireType.Len);
        (ulong v, int n) = Wire.Uvarint(key.ToArray(), 0);
        Check(n == key.Count, $"field {field} key {Hex(key.ToArray())} did not decode");
        Check((int)(v >> 3) == field && (WireType)(v & 7) == WireType.Len,
            $"field {field} key decoded as ({v >> 3}, {v & 7})");
    }
});

Run("wire: TestSkipValueUnknownFields", () =>
{
    Check(Wire.SkipValue(new byte[] { 0xac, 0x02 }, 0, WireType.Varint) == 2, "skip varint");
    Check(Wire.SkipValue(new byte[] { 0x03, 0xde, 0xad, 0xbe }, 0, WireType.Len) == 4, "skip len");
    Check(Wire.SkipValue(new byte[4], 0, WireType.Fixed32) == 4, "skip fixed32");
    Check(Wire.SkipValue(new byte[8], 0, WireType.Fixed64) == 8, "skip fixed64");
    Throws(() => Wire.SkipValue(Array.Empty<byte>(), 0, WireType.SGroup), "skip sgroup");
    Throws(() => Wire.SkipValue(Array.Empty<byte>(), 0, WireType.EGroup), "skip egroup");
    Throws(() => Wire.SkipValue(new byte[] { 0x05, 0x01 }, 0, WireType.Len), "skip truncated len");
});

Run("wire: TestBytesHelpers", () =>
{
    byte[] b = { 0x03, 0xaa, 0xbb, 0xcc, 0xff };
    (byte[] v, int n) = Wire.Bytes(b, 0);
    Check(n == 4, $"Bytes consumed {n}, want 4");
    CheckBytes(v, new byte[] { 0xaa, 0xbb, 0xcc }, "Bytes payload");
    Throws(() => Wire.Bytes(new byte[] { 0x05, 0x01 }, 0), "Bytes truncated");
});

// ----------------------------------------------------------------------
// Golden cases (mirror sdk/go/sabi/golden_test.go case-for-case; the golden
// gate is the full five-file sabi set the Go lane asserts: Envelope-v1,
// Envelope-common-request-v1, Envelope-common-uncertain-v1,
// PrincipalHandshake-v1, and ServiceDirectory.ResolveRequest-v1).
// ----------------------------------------------------------------------

Run("sabi: TestEnvelopeGolden (byte-equal + decode fields + roundtrip)", () =>
{
    byte[] golden = GoldenBytes("nlos.sabi.Envelope-v1.hex");

    var envelope = new Envelope
    {
        Schema = new SchemaIdentity
        {
            Name = "nlos.sabi.Envelope",
            Major = 1,
            NonCriticalExtensionIDs = new uint[] { 42 },
        },
        RequestID = Seq(0x00, 16),
        Service = "operation",
        Method = "get",
        Payload = Encoding.UTF8.GetBytes("abc"),
    };
    CheckBytes(SabiCodec.Marshal(envelope), golden, "encode Envelope-v1 != golden");

    var decoded = new Envelope();
    SabiCodec.Unmarshal(decoded, golden);
    Check(decoded.Schema != null, "decoded schema identity missing");
    Check(decoded.Schema!.Name == "nlos.sabi.Envelope", "decoded schema name");
    Check(decoded.Schema.Major == 1, "decoded schema major");
    Check(decoded.Schema.Minor == 0, "decoded schema minor");
    Check(decoded.Schema.CriticalExtensionIDs.Length == 0, "decoded critical extensions");
    Check(decoded.Schema.NonCriticalExtensionIDs.Length == 1 && decoded.Schema.NonCriticalExtensionIDs[0] == 42,
        "decoded non-critical extensions");
    CheckBytes(decoded.RequestID, Seq(0x00, 16), "decoded request_id");
    Check(decoded.Service == "operation" && decoded.Method == "get", "decoded service/method");
    CheckBytes(decoded.Payload, Encoding.UTF8.GetBytes("abc"), "decoded payload");
    Check(decoded.RequestContext == null && decoded.ResponseContext == null,
        "oneof common_context must be unset in Envelope-v1 golden");

    RoundtripGolden("Envelope-v1", golden, g =>
    {
        var m = new Envelope();
        SabiCodec.Unmarshal(m, g);
        return SabiCodec.Marshal(m);
    });
});

Run("sabi: TestEnvelopeCommonRequestGolden (byte-equal + decode fields + roundtrip)", () =>
{
    byte[] golden = GoldenBytes("nlos.sabi.Envelope-common-request-v1.hex");

    var envelope = new Envelope
    {
        Schema = new SchemaIdentity { Name = "nlos.sabi.Envelope", Major = 1, Minor = 1 },
        RequestID = Seq(0x00, 16),
        Service = "operation",
        Method = "cancel",
        RequestContext = new SabiRequestContext
        {
            Caller = new CallerIdentity
            {
                PrincipalID = Repeat(0x01, 16),
                ApplicationID = Repeat(0x02, 16),
                ProcessID = Repeat(0x03, 16),
                ProcessGeneration = 7,
            },
            ActivityContext = Encoding.UTF8.GetBytes("trace"),
            TaskExecutionBinding = new TaskExecutionBinding
            {
                TaskAttemptID = Repeat(0x04, 16),
                TaskAuthorityTerm = 9,
                TaskControlEpoch = 10,
                CancelEpoch = 11,
                PermitEpoch = 12,
                IsolationDomainGeneration = 13,
            },
            CorrelationID = Repeat(0x05, 16),
            IdempotencyKey = Repeat(0x06, 16),
            DeadlineMonotonicNS = 123456,
            CapabilityHandles = new List<CapabilityHandle> { new() { Slot = 11, Generation = 2 } },
            ReservationHandle = new CapabilityHandle { Slot = 12, Generation = 3 },
            ProposalOrInputDigestSHA256 = Repeat(0x07, 32),
        },
        Payload = Encoding.UTF8.GetBytes("abc"),
    };
    CheckBytes(SabiCodec.Marshal(envelope), golden, "encode Envelope-common-request-v1 != golden");

    var decoded = new Envelope();
    SabiCodec.Unmarshal(decoded, golden);
    var rc = decoded.RequestContext;
    Check(rc != null && decoded.ResponseContext == null, "request arm must be the set oneof");
    Check(rc!.Caller != null && rc.Caller.ProcessGeneration == 7, "decoded caller process_generation");
    CheckBytes(rc.IdempotencyKey, Repeat(0x06, 16), "decoded idempotency key");
    Check(rc.DeadlineMonotonicNS == 123456, "decoded deadline");
    Check(rc.CapabilityHandles.Count == 1 && rc.CapabilityHandles[0].Slot == 11 && rc.CapabilityHandles[0].Generation == 2,
        "decoded capability handles");
    Check(rc.ReservationHandle != null && rc.ReservationHandle.Slot == 12 && rc.ReservationHandle.Generation == 3,
        "decoded reservation handle");
    var binding = rc.TaskExecutionBinding;
    Check(binding != null && binding.TaskAuthorityTerm == 9 && binding.TaskControlEpoch == 10 &&
        binding.CancelEpoch == 11 && binding.PermitEpoch == 12 && binding.IsolationDomainGeneration == 13,
        "decoded task execution binding");

    RoundtripGolden("Envelope-common-request-v1", golden, g =>
    {
        var m = new Envelope();
        SabiCodec.Unmarshal(m, g);
        return SabiCodec.Marshal(m);
    });
});

Run("sabi: TestEnvelopeCommonUncertainGolden (byte-equal + decode fields + roundtrip)", () =>
{
    byte[] golden = GoldenBytes("nlos.sabi.Envelope-common-uncertain-v1.hex");

    var envelope = new Envelope
    {
        Schema = new SchemaIdentity { Name = "nlos.sabi.Envelope", Major = 1, Minor = 1 },
        RequestID = Seq(0x00, 16),
        Service = "operation",
        Method = "cancel",
        ResponseContext = new SabiResponseContext
        {
            CorrelationID = Repeat(0x05, 16),
            Operation = new OperationReference { OperationID = Repeat(0x08, 16), Generation = 4 },
            Receipts = new List<ReceiptReference> { new() { ReceiptID = Repeat(0x09, 16) } },
            Failure = new SabiFailure
            {
                Code = SabiErrorCode.Uncertain,
                Retry = RetryDirective.QueryOperationOrRetrySameIdempotencyKey,
                SafeMessage = "outcome requires reconciliation",
            },
        },
    };
    CheckBytes(SabiCodec.Marshal(envelope), golden, "encode Envelope-common-uncertain-v1 != golden");

    var decoded = new Envelope();
    SabiCodec.Unmarshal(decoded, golden);
    var rc = decoded.ResponseContext;
    Check(rc != null && decoded.RequestContext == null, "response arm must be the set oneof");
    Check(rc!.Operation != null && rc.Operation.Generation == 4, "decoded operation generation");
    Check(rc.Receipts.Count == 1 && rc.Receipts[0].ReceiptID.AsSpan().SequenceEqual(Repeat(0x09, 16)),
        "decoded receipts");
    Check(rc.Failure != null &&
        rc.Failure.Code == SabiErrorCode.Uncertain &&
        rc.Failure.Retry == RetryDirective.QueryOperationOrRetrySameIdempotencyKey &&
        rc.Failure.SafeMessage == "outcome requires reconciliation",
        "decoded failure face");
    Check(decoded.Payload.Length == 0, "uncertain golden carries no payload");

    RoundtripGolden("Envelope-common-uncertain-v1", golden, g =>
    {
        var m = new Envelope();
        SabiCodec.Unmarshal(m, g);
        return SabiCodec.Marshal(m);
    });
});

Run("sabi: TestPrincipalHandshakeGolden (byte-equal + decode fields + roundtrip)", () =>
{
    byte[] golden = GoldenBytes("nlos.sabi.PrincipalHandshake-v1.hex");

    var attestation = new PrincipalHandshakeAttestation
    {
        Schema = new SchemaIdentity { Name = "nlos.sabi.PrincipalHandshake", Major = 1 },
        PrincipalID = Seq(0x00, 16),
        Nonce = Repeat(0xa5, 32),
        ChannelBinding = Encoding.UTF8.GetBytes("unix:///tmp/nlos-handshake.sock"),
        Signature = Repeat(0xcd, 64),
    };
    CheckBytes(SabiCodec.Marshal(attestation), golden, "encode PrincipalHandshake-v1 != golden");

    var decoded = new PrincipalHandshakeAttestation();
    SabiCodec.Unmarshal(decoded, golden);
    Check(decoded.Schema != null, "decoded schema identity missing");
    Check(decoded.Schema!.Name == "nlos.sabi.PrincipalHandshake", "decoded schema name");
    Check(decoded.Schema.Major == 1, "decoded schema major");
    Check(decoded.Schema.Minor == 0, "decoded schema minor");
    CheckBytes(decoded.PrincipalID, Seq(0x00, 16), "decoded principal_id");
    CheckBytes(decoded.Nonce, Repeat(0xa5, 32), "decoded nonce");
    Check(decoded.ChannelBinding.AsSpan().SequenceEqual(Encoding.UTF8.GetBytes("unix:///tmp/nlos-handshake.sock")),
        "decoded channel binding");
    CheckBytes(decoded.Signature, Repeat(0xcd, 64), "decoded signature");

    RoundtripGolden("PrincipalHandshake-v1", golden, g =>
    {
        var m = new PrincipalHandshakeAttestation();
        SabiCodec.Unmarshal(m, g);
        return SabiCodec.Marshal(m);
    });
});

Run("sabi: TestServiceDirectoryResolveRequestGolden (byte-equal + decode fields + roundtrip)", () =>
{
    byte[] golden = GoldenBytes("nlos.sabi.ServiceDirectory.ResolveRequest-v1.hex");

    var request = new ResolveServiceRequest
    {
        Schema = new SchemaIdentity { Name = "nlos.sabi.ServiceDirectory", Major = 1 },
        Service = "operation",
    };
    CheckBytes(SabiCodec.Marshal(request), golden, "encode ServiceDirectory.ResolveRequest-v1 != golden");

    var decoded = new ResolveServiceRequest();
    SabiCodec.Unmarshal(decoded, golden);
    Check(decoded.Schema != null, "decoded schema identity missing");
    Check(decoded.Schema!.Name == "nlos.sabi.ServiceDirectory", "decoded schema name");
    Check(decoded.Schema.Major == 1, "decoded schema major");
    Check(decoded.Schema.Minor == 0, "decoded schema minor");
    Check(decoded.Service == "operation", "decoded service");

    RoundtripGolden("ServiceDirectory.ResolveRequest-v1", golden, g =>
    {
        var m = new ResolveServiceRequest();
        SabiCodec.Unmarshal(m, g);
        return SabiCodec.Marshal(m);
    });
});

Run("sabi: TestUnknownFieldPreservedAcrossRoundtrip", () =>
{
    byte[] golden = GoldenBytes("nlos.sabi.Envelope-v1.hex");
    byte[] extended = Concat(golden, new byte[] { 0xa0, 0x06, 0x07 });

    var decoded = new Envelope();
    SabiCodec.Unmarshal(decoded, extended);
    CheckBytes(decoded.UnknownFields, new byte[] { 0xa0, 0x06, 0x07 }, "unknown field bytes");
    CheckBytes(SabiCodec.Marshal(decoded), extended, "re-encode with unknown field diverged");
});

Run("sabi: TestOneofBothArmsThrows", () =>
{
    Throws(
        () => SabiCodec.Marshal(new Envelope
        {
            RequestContext = new SabiRequestContext(),
            ResponseContext = new SabiResponseContext(),
        }),
        "marshal with both oneof arms set must throw");
});

Run("sabi: TestDecodeTruncatedFailsClosed", () =>
{
    byte[] golden = GoldenBytes("nlos.sabi.Envelope-v1.hex");
    // Offsets that land inside a field value: 1 = lone schema tag, 28 = one
    // byte into the 16-byte request_id, 63 = four bytes of the 5-byte
    // payload value. A cut that lands exactly on a field boundary (e.g. 27)
    // is a complete valid prefix and must decode without error.
    foreach (int cut in new[] { 1, 28, 63 })
    {
        var m = new Envelope();
        byte[] slice = golden[..cut];
        Throws(() => SabiCodec.Unmarshal(m, slice), $"truncated input (cut={cut}) must fail closed");
    }
    var prefix = new Envelope();
    SabiCodec.Unmarshal(prefix, golden[..27]);
});

Run("sabi: TestDecodeGroupWireTypeFailsClosed", () =>
{
    // Tag 0x0b = field 1, wire type 3 (group start): rejected, never skipped.
    var m = new PrincipalHandshakeAttestation();
    Throws(() => SabiCodec.Unmarshal(m, new byte[] { 0x0b, 0x08, 0x01, 0x0c }),
        "group wire type must fail closed");
});

Run("sabi: TestDecodeUint32OverflowRejected", () =>
{
    // SchemaIdentity.Major encoded as varint 2^32: rejected, not truncated.
    var m = new SchemaIdentity();
    Throws(() => SabiCodec.Unmarshal(m, new byte[] { 0x0a, 0x00, 0x10, 0x80, 0x80, 0x80, 0x80, 0x10 }),
        "uint32 overflow must be rejected");
});

Run("sabi: TestLengthBoundary128", () =>
{
    var envelope = new Envelope
    {
        Schema = new SchemaIdentity { Name = "n", Major = 1 },
        Payload = Repeat(0xee, 128),
    };
    // schema body: 0a 01 'n' 10 01 -> wrapped in Envelope field 1: 0a 05 + 5 = 7
    const int schemaLen = 7;
    const int payloadOverhead = 3; // tag 0x7a + 2-byte varint length
    byte[] encoded = SabiCodec.Marshal(envelope);
    Check(encoded.Length == schemaLen + payloadOverhead + 128, $"unexpected total length {encoded.Length}");
    var decoded = new Envelope();
    SabiCodec.Unmarshal(decoded, encoded);
    CheckBytes(decoded.Payload, Repeat(0xee, 128), "128-byte payload did not survive the roundtrip");
});

// ----------------------------------------------------------------------
// W77-E1B Epoch v1.0 9-channel golden corpus + COMPAT-VER-002 gates.
// ----------------------------------------------------------------------

Run("sabi: TestEpochV10ManifestAndFingerprints", () =>
{
    using JsonDocument doc = LoadEpochV10Manifest();
    JsonElement rootElem = doc.RootElement;
    Check(rootElem.GetProperty("schema").GetString() == "nlos.golden.epoch_manifest.v1", "manifest schema");
    Check(rootElem.GetProperty("epoch").GetString() == "v1.0", "manifest epoch");
    Check(rootElem.GetProperty("sabi_major").GetInt32() == 1, "manifest sabi_major");
    Check(rootElem.GetProperty("immutable").GetBoolean(), "manifest immutable");

    var reqChannels = new List<string>();
    foreach (JsonElement ch in rootElem.GetProperty("required_channels").EnumerateArray())
    {
        string chName = ch.GetString() ?? "";
        Check(SabiRegistry.SabiV1Channels.ContainsKey(chName), $"unexpected manifest channel {chName}");
        reqChannels.Add(chName);
    }
    Check(reqChannels.Count == SabiRegistry.SabiV1Channels.Count, "required_channels count");

    string repoRoot = FindRepoRoot();
    int legacyCount = 0;
    foreach (JsonElement lg in rootElem.GetProperty("legacy_root_goldens").EnumerateArray())
    {
        legacyCount++;
        string relPath = (lg.GetProperty("path").GetString() ?? "").Replace('/', Path.DirectorySeparatorChar);
        byte[] rawFile = File.ReadAllBytes(Path.Combine(repoRoot, relPath));
        Check(Sha256Hex(rawFile) == lg.GetProperty("sha256_file").GetString(), $"legacy {relPath} sha256_file");
        byte[] wire = Convert.FromHexString(Encoding.UTF8.GetString(rawFile).Trim());
        Check(wire.Length == lg.GetProperty("byte_length").GetInt32(), $"legacy {relPath} byte_length");
        Check(Sha256Hex(wire) == lg.GetProperty("sha256_wire").GetString(), $"legacy {relPath} sha256_wire");
    }
    Check(legacyCount == 7, $"legacy_root_goldens count={legacyCount}, want 7");

    var covered = new HashSet<string>(StringComparer.Ordinal);
    int vectorCount = 0;
    foreach (JsonElement vec in rootElem.GetProperty("vectors").EnumerateArray())
    {
        vectorCount++;
        string file = vec.GetProperty("file").GetString() ?? "";
        _ = EpochV10Bytes(file);
        covered.Add(vec.GetProperty("channel").GetString() ?? "");
    }
    Check(vectorCount == 11, $"epoch v1.0 vector count={vectorCount}, want 11");
    Check(covered.Count == 9, $"epoch v1.0 covered channels={covered.Count}, want 9");
});

Run("sabi: TestEpochV10EnvelopeGoldens", () =>
{
    byte[] g1 = EpochV10Bytes("nlos.sabi.Envelope-v1.0.hex");
    var env1 = new Envelope
    {
        Schema = new SchemaIdentity
        {
            Name = "nlos.sabi.Envelope",
            Major = 1,
            Minor = 1,
            NonCriticalExtensionIDs = new uint[] { 42 },
        },
        RequestID = Seq(0x10, 16),
        Service = "system_control",
        Method = "get",
        RequestContext = new SabiRequestContext
        {
            Caller = new CallerIdentity
            {
                PrincipalID = Repeat(0x01, 16),
                ApplicationID = Repeat(0x02, 16),
                ProcessID = Repeat(0x03, 16),
                ProcessGeneration = 7,
            },
            ActivityContext = Encoding.UTF8.GetBytes("w77-epoch-v1.0"),
            TaskExecutionBinding = new TaskExecutionBinding
            {
                TaskAttemptID = Repeat(0x04, 16),
                TaskAuthorityTerm = 9,
                TaskControlEpoch = 10,
                CancelEpoch = 11,
                PermitEpoch = 12,
                IsolationDomainGeneration = 13,
            },
            CorrelationID = Repeat(0x05, 16),
            IdempotencyKey = Repeat(0x06, 16),
            DeadlineMonotonicNS = 500000,
            CapabilityHandles = new List<CapabilityHandle> { new() { Slot = 11, Generation = 2 } },
            ReservationHandle = new CapabilityHandle { Slot = 12, Generation = 3 },
            ProposalOrInputDigestSHA256 = Repeat(0x07, 32),
        },
        Payload = Encoding.UTF8.GetBytes("sabi-v1.0-payload"),
    };
    CheckBytes(SabiCodec.Marshal(env1), g1, "encode Envelope-v1.0 != golden");

    var dec1 = new Envelope();
    SabiCodec.Unmarshal(dec1, g1);
    SabiRegistry.ValidateSchemaIdentity(dec1.Schema, "nlos.sabi.Envelope");
    Check(dec1.Service == "system_control" && dec1.Method == "get", "decoded Envelope-v1.0 service/method");
    Check(Encoding.UTF8.GetString(dec1.Payload) == "sabi-v1.0-payload", "decoded Envelope-v1.0 payload");
    CheckBytes(SabiCodec.Marshal(dec1), g1, "re-encode Envelope-v1.0 != golden");

    byte[] g2 = EpochV10Bytes("nlos.sabi.Envelope-unknown-noncritical-v1.0.hex");
    var env2 = new Envelope
    {
        Schema = new SchemaIdentity
        {
            Name = "nlos.sabi.Envelope",
            Major = 1,
            Minor = 1,
            NonCriticalExtensionIDs = new uint[] { 42, 7001, 9999 },
        },
        RequestID = Seq(0x20, 16),
        Service = "operation",
        Method = "query",
        Payload = Encoding.UTF8.GetBytes("ext-probe"),
        UnknownFields = new byte[] { 0xa0, 0x06, 0x07 },
    };
    CheckBytes(SabiCodec.Marshal(env2), g2, "encode Envelope-unknown-noncritical-v1.0 != golden");

    var dec2 = new Envelope();
    SabiCodec.Unmarshal(dec2, g2);
    SabiRegistry.ValidateSchemaIdentity(dec2.Schema, "nlos.sabi.Envelope");
    Check(dec2.Schema!.NonCriticalExtensionIDs.SequenceEqual(new uint[] { 42, 7001, 9999 }),
        "decoded non-critical extensions");
    CheckBytes(dec2.UnknownFields, new byte[] { 0xa0, 0x06, 0x07 }, "decoded unknown fields");
    CheckBytes(SabiCodec.Marshal(dec2), g2, "re-encode Envelope-unknown-noncritical-v1.0 != golden");
});

Run("sabi: TestEpochV10ServiceDirectoryGolden", () =>
{
    byte[] golden = EpochV10Bytes("nlos.sabi.ServiceDirectory.ResolveServiceRequest-v1.0.hex");
    var req = new ResolveServiceRequest
    {
        Schema = new SchemaIdentity { Name = "nlos.sabi.ServiceDirectory", Major = 1 },
        Service = "system_control",
    };
    CheckBytes(SabiCodec.Marshal(req), golden, "encode ServiceDirectory-v1.0 != golden");

    var dec = new ResolveServiceRequest();
    SabiCodec.Unmarshal(dec, golden);
    SabiRegistry.ValidateSchemaIdentity(dec.Schema, "nlos.sabi.ServiceDirectory");
    Check(dec.Service == "system_control", "decoded ServiceDirectory-v1.0 service");
    CheckBytes(SabiCodec.Marshal(dec), golden, "re-encode ServiceDirectory-v1.0 != golden");
});

Run("sabi: TestEpochV10OperationControlGolden", () =>
{
    byte[] golden = EpochV10Bytes("nlos.sabi.OperationControl.QueryOperationRequest-v1.0.hex");
    var req = new QueryOperationRequest
    {
        Schema = new SchemaIdentity { Name = "nlos.sabi.OperationControl", Major = 1 },
        Operation = new OperationReference { OperationID = Seq(0x30, 16), Generation = 5 },
    };
    CheckBytes(SabiCodec.Marshal(req), golden, "encode OperationControl-v1.0 != golden");

    var dec = new QueryOperationRequest();
    SabiCodec.Unmarshal(dec, golden);
    SabiRegistry.ValidateSchemaIdentity(dec.Schema, "nlos.sabi.OperationControl");
    Check(dec.Operation != null && dec.Operation.Generation == 5, "decoded OperationControl generation");
    CheckBytes(dec.Operation!.OperationID, Seq(0x30, 16), "decoded OperationControl operation_id");
    CheckBytes(SabiCodec.Marshal(dec), golden, "re-encode OperationControl-v1.0 != golden");
});

Run("sabi: TestEpochV10SystemControlGolden", () =>
{
    byte[] golden = EpochV10Bytes("nlos.sabi.SystemControl.GetSystemControlRequest-v1.0.hex");
    var req = new GetSystemControlRequest
    {
        Schema = new SchemaIdentity { Name = "nlos.sabi.SystemControl", Major = 1, Minor = 7 },
        View = SystemControlView.TaskAttemptList,
        AlertLimit = 16,
        TargetID = Seq(0x40, 16),
        PlanID = Seq(0x50, 16),
        TargetGeneration = 4,
    };
    CheckBytes(SabiCodec.Marshal(req), golden, "encode SystemControl-v1.0 != golden");

    var dec = new GetSystemControlRequest();
    SabiCodec.Unmarshal(dec, golden);
    SabiRegistry.ValidateSchemaIdentity(dec.Schema, "nlos.sabi.SystemControl");
    Check(dec.Schema!.Minor == 7 && dec.View == SystemControlView.TaskAttemptList &&
          dec.AlertLimit == 16 && dec.TargetGeneration == 4,
        "decoded SystemControl fields");
    CheckBytes(dec.TargetID, Seq(0x40, 16), "decoded SystemControl target_id");
    CheckBytes(dec.PlanID, Seq(0x50, 16), "decoded SystemControl plan_id");
    CheckBytes(SabiCodec.Marshal(dec), golden, "re-encode SystemControl-v1.0 != golden");
});

Run("sabi: TestEpochV10TakeoverControlGolden", () =>
{
    byte[] golden = EpochV10Bytes("nlos.sabi.TakeoverControl.SubmitBarrierObservationRequest-v1.0.hex");
    var req = new SubmitBarrierObservationRequest
    {
        Schema = new SchemaIdentity { Name = "nlos.sabi.TakeoverControl", Major = 1 },
        Target = new BarrierObservationTarget
        {
            TakeoverReceiptID = Repeat(0x61, 16),
            ParticipantType = 1,
            ParticipantID = Repeat(0x62, 16),
            ParticipantGeneration = 3,
            AdmissionReceiptID = Repeat(0x63, 16),
        },
        Evidence = new BarrierObservationEvidence
        {
            RemoteReceiptID = Repeat(0x64, 16),
            BarrierDigest = Repeat(0x65, 32),
            ObservedAtMS = 1730000000000L,
        },
        Signature = new BarrierObservationSignature
        {
            SignerPrincipalID = Repeat(0x66, 16),
            SignerControlDomainID = Repeat(0x67, 16),
            SignerKeyID = Repeat(0x68, 16),
            Signature = Repeat(0x69, 64),
        },
    };
    CheckBytes(SabiCodec.Marshal(req), golden, "encode TakeoverControl-v1.0 != golden");

    var dec = new SubmitBarrierObservationRequest();
    SabiCodec.Unmarshal(dec, golden);
    SabiRegistry.ValidateSchemaIdentity(dec.Schema, "nlos.sabi.TakeoverControl");
    Check(dec.Target != null && dec.Target.ParticipantType == 1 && dec.Target.ParticipantGeneration == 3,
        "decoded TakeoverControl target");
    Check(dec.Evidence != null && dec.Evidence.ObservedAtMS == 1730000000000L,
        "decoded TakeoverControl evidence");
    Check(dec.Signature != null, "decoded TakeoverControl signature");
    CheckBytes(dec.Signature!.Signature, Repeat(0x69, 64), "decoded TakeoverControl signature bytes");
    CheckBytes(SabiCodec.Marshal(dec), golden, "re-encode TakeoverControl-v1.0 != golden");
});

Run("sabi: TestEpochV10WaitControlGolden", () =>
{
    byte[] golden = EpochV10Bytes("nlos.sabi.WaitControl.InspectWaitRequest-v1.0.hex");
    var req = new InspectWaitRequest
    {
        Schema = new SchemaIdentity { Name = "nlos.sabi.WaitControl", Major = 1 },
        WaitID = Seq(0x70, 16),
    };
    CheckBytes(SabiCodec.Marshal(req), golden, "encode WaitControl-v1.0 != golden");

    var dec = new InspectWaitRequest();
    SabiCodec.Unmarshal(dec, golden);
    SabiRegistry.ValidateSchemaIdentity(dec.Schema, "nlos.sabi.WaitControl");
    CheckBytes(dec.WaitID, Seq(0x70, 16), "decoded WaitControl wait_id");
    CheckBytes(SabiCodec.Marshal(dec), golden, "re-encode WaitControl-v1.0 != golden");
});

Run("sabi: TestEpochV10PrincipalHandshakeDualGoldens", () =>
{
    byte[] gChal = EpochV10Bytes("nlos.sabi.PrincipalHandshake.Challenge-v1.0.hex");
    var chal = new PrincipalHandshakeChallenge
    {
        Schema = new SchemaIdentity { Name = "nlos.sabi.PrincipalHandshake", Major = 1 },
        Nonce = Repeat(0xa5, 32),
    };
    CheckBytes(SabiCodec.Marshal(chal), gChal, "encode Challenge-v1.0 != golden");

    var decChal = new PrincipalHandshakeChallenge();
    SabiCodec.Unmarshal(decChal, gChal);
    SabiRegistry.ValidateSchemaIdentity(decChal.Schema, "nlos.sabi.PrincipalHandshake");
    CheckBytes(decChal.Nonce, Repeat(0xa5, 32), "decoded Challenge nonce");
    CheckBytes(SabiCodec.Marshal(decChal), gChal, "re-encode Challenge-v1.0 != golden");

    byte[] gAtt = EpochV10Bytes("nlos.sabi.PrincipalHandshake.Attestation-v1.0.hex");
    var att = new PrincipalHandshakeAttestation
    {
        Schema = new SchemaIdentity
        {
            Name = "nlos.sabi.PrincipalHandshake",
            Major = 1,
            NonCriticalExtensionIDs = new uint[] { 101 },
        },
        PrincipalID = Seq(0x80, 16),
        Nonce = Repeat(0xa5, 32),
        ChannelBinding = Encoding.UTF8.GetBytes("unix:///tmp/nlos-handshake-v1.0.sock"),
        Signature = Repeat(0xcd, 64),
    };
    CheckBytes(SabiCodec.Marshal(att), gAtt, "encode Attestation-v1.0 != golden");

    var decAtt = new PrincipalHandshakeAttestation();
    SabiCodec.Unmarshal(decAtt, gAtt);
    SabiRegistry.ValidateSchemaIdentity(decAtt.Schema, "nlos.sabi.PrincipalHandshake");
    Check(decAtt.Schema!.NonCriticalExtensionIDs.SequenceEqual(new uint[] { 101 }), "decoded Attestation ext");
    CheckBytes(decAtt.PrincipalID, Seq(0x80, 16), "decoded Attestation principal_id");
    Check(Encoding.UTF8.GetString(decAtt.ChannelBinding) == "unix:///tmp/nlos-handshake-v1.0.sock",
        "decoded Attestation channel_binding");
    CheckBytes(SabiCodec.Marshal(decAtt), gAtt, "re-encode Attestation-v1.0 != golden");
});

Run("sabi: TestEpochV10DriverProviderGolden", () =>
{
    byte[] golden = EpochV10Bytes("nlos.sabi.DriverProvider.Request-v1.0.hex");
    var req = new DriverProviderRequest
    {
        Schema = new SchemaIdentity { Name = "nlos.sabi.DriverProvider", Major = 1 },
        Register = new DriverProviderRegisterRequest
        {
            OperationID = Repeat(0x91, 16),
            OperationGeneration = 2,
            OwnerFiberID = Repeat(0x92, 16),
            OwnerFiberGeneration = 3,
            CancellationScopeID = Repeat(0x93, 16),
            CancellationGeneration = 4,
        },
    };
    CheckBytes(SabiCodec.Marshal(req), golden, "encode DriverProvider-v1.0 != golden");

    var dec = new DriverProviderRequest();
    SabiCodec.Unmarshal(dec, golden);
    SabiRegistry.ValidateSchemaIdentity(dec.Schema, "nlos.sabi.DriverProvider");
    Check(dec.Register != null && dec.Complete == null, "decoded DriverProvider register arm");
    CheckBytes(dec.Register!.OperationID, Repeat(0x91, 16), "decoded DriverProvider operation_id");
    Check(dec.Register.OperationGeneration == 2 &&
          dec.Register.OwnerFiberGeneration == 3 &&
          dec.Register.CancellationGeneration == 4,
        "decoded DriverProvider generations");
    CheckBytes(SabiCodec.Marshal(dec), golden, "re-encode DriverProvider-v1.0 != golden");
});

Run("sabi: TestDriverProviderOneofBothArmsThrows", () =>
{
    Throws(
        () => SabiCodec.Marshal(new DriverProviderRequest
        {
            Register = new DriverProviderRegisterRequest(),
            Complete = new DriverProviderCompleteRequest(),
        }),
        "marshal DriverProviderRequest with both oneof arms set must throw");
});

Run("sabi: TestEpochV10ChannelControlGolden", () =>
{
    byte[] golden = EpochV10Bytes("nlos.sabi.ChannelControl.CreateChannelRequestWire-v1.0.hex");
    var req = new CreateChannelRequestWire
    {
        Schema = new SchemaIdentity { Name = "nlos.sabi.ChannelControl", Major = 1 },
        CapacityBytes = 65536,
        PolicyDigest = Repeat(0xb1, 32),
        IdempotencyKey = Repeat(0xb2, 16),
        CreatedAtMS = 1730000000000UL,
    };
    CheckBytes(SabiCodec.Marshal(req), golden, "encode ChannelControl-v1.0 != golden");

    var dec = new CreateChannelRequestWire();
    SabiCodec.Unmarshal(dec, golden);
    SabiRegistry.ValidateSchemaIdentity(dec.Schema, "nlos.sabi.ChannelControl");
    Check(dec.CapacityBytes == 65536 && dec.CreatedAtMS == 1730000000000UL,
        "decoded ChannelControl scalar fields");
    CheckBytes(dec.PolicyDigest, Repeat(0xb1, 32), "decoded ChannelControl policy_digest");
    CheckBytes(dec.IdempotencyKey, Repeat(0xb2, 16), "decoded ChannelControl idempotency_key");
    CheckBytes(SabiCodec.Marshal(dec), golden, "re-encode ChannelControl-v1.0 != golden");
});

Run("sabi: TestEpochV10UnknownFieldRoundtripAllChannels", () =>
{
    byte[] tail = { 0xa0, 0x06, 0x07 };
    (string File, Func<byte[], byte[]> Run)[] cases =
    {
        ("nlos.sabi.Envelope-v1.0.hex", b =>
        {
            var m = new Envelope();
            SabiCodec.Unmarshal(m, b);
            return SabiCodec.Marshal(m);
        }),
        ("nlos.sabi.ServiceDirectory.ResolveServiceRequest-v1.0.hex", b =>
        {
            var m = new ResolveServiceRequest();
            SabiCodec.Unmarshal(m, b);
            return SabiCodec.Marshal(m);
        }),
        ("nlos.sabi.OperationControl.QueryOperationRequest-v1.0.hex", b =>
        {
            var m = new QueryOperationRequest();
            SabiCodec.Unmarshal(m, b);
            return SabiCodec.Marshal(m);
        }),
        ("nlos.sabi.SystemControl.GetSystemControlRequest-v1.0.hex", b =>
        {
            var m = new GetSystemControlRequest();
            SabiCodec.Unmarshal(m, b);
            return SabiCodec.Marshal(m);
        }),
        ("nlos.sabi.TakeoverControl.SubmitBarrierObservationRequest-v1.0.hex", b =>
        {
            var m = new SubmitBarrierObservationRequest();
            SabiCodec.Unmarshal(m, b);
            return SabiCodec.Marshal(m);
        }),
        ("nlos.sabi.WaitControl.InspectWaitRequest-v1.0.hex", b =>
        {
            var m = new InspectWaitRequest();
            SabiCodec.Unmarshal(m, b);
            return SabiCodec.Marshal(m);
        }),
        ("nlos.sabi.PrincipalHandshake.Challenge-v1.0.hex", b =>
        {
            var m = new PrincipalHandshakeChallenge();
            SabiCodec.Unmarshal(m, b);
            return SabiCodec.Marshal(m);
        }),
        ("nlos.sabi.PrincipalHandshake.Attestation-v1.0.hex", b =>
        {
            var m = new PrincipalHandshakeAttestation();
            SabiCodec.Unmarshal(m, b);
            return SabiCodec.Marshal(m);
        }),
        ("nlos.sabi.DriverProvider.Request-v1.0.hex", b =>
        {
            var m = new DriverProviderRequest();
            SabiCodec.Unmarshal(m, b);
            return SabiCodec.Marshal(m);
        }),
        ("nlos.sabi.ChannelControl.CreateChannelRequestWire-v1.0.hex", b =>
        {
            var m = new CreateChannelRequestWire();
            SabiCodec.Unmarshal(m, b);
            return SabiCodec.Marshal(m);
        }),
    };
    foreach ((string file, Func<byte[], byte[]> runCase) in cases)
    {
        byte[] extended = Concat(EpochV10Bytes(file), tail);
        CheckBytes(runCase(extended), extended, $"{file} unknown-field roundtrip");
    }
});

Run("sabi: TestCompatVer002SchemaIdentityValidation", () =>
{
    ThrowsCompat(
        () => SabiRegistry.ValidateSchemaIdentity(null),
        SabiRegistry.CodeMissingSchemaIdentity,
        "null schema identity");
    ThrowsCompat(
        () => SabiRegistry.ValidateSchemaIdentity(new SchemaIdentity { Name = "nlos.sabi.Unknown", Major = 1 }),
        SabiRegistry.CodeUnknownSchema,
        "unknown schema name");
    foreach (string ch in SabiRegistry.SabiV1Channels.Keys)
    {
        foreach (uint badMajor in new uint[] { 0, 2, 99 })
        {
            ThrowsCompat(
                () => SabiRegistry.ValidateSchemaIdentity(new SchemaIdentity { Name = ch, Major = badMajor }, ch),
                SabiRegistry.CodeAbiVersion,
                $"{ch} major={badMajor}");
        }
        foreach (uint[] badCrit in new[] { new uint[] { 7001 }, new uint[] { 0 }, new uint[] { 42, 7001 } })
        {
            ThrowsCompat(
                () => SabiRegistry.ValidateSchemaIdentity(
                    new SchemaIdentity { Name = ch, Major = 1, CriticalExtensionIDs = badCrit }, ch),
                SabiRegistry.CodeUnsupportedCriticalExtension,
                $"{ch} critical_extension_ids");
        }
        SabiRegistry.ValidateSchemaIdentity(
            new SchemaIdentity
            {
                Name = ch,
                Major = 1,
                Minor = 99,
                NonCriticalExtensionIDs = new uint[] { 42, 7001, 9999 },
            },
            ch);
    }
});

// ----------------------------------------------------------------------
// Summary.
// ----------------------------------------------------------------------

Console.WriteLine();
Console.WriteLine($"cs golden probe: {passed} passed, {failed} failed (total {passed + failed})");
if (failed > 0)
{
    Console.WriteLine("failed cases: " + string.Join(", ", failures));
    return 1;
}
return 0;
