// B-SDK-LANG-EVAL C# golden probe driver: golden byte-equal, roundtrip,
// boundary, and fail-closed cases against the frozen v1-beta goldens
// (ADR-0014). Mirrors sdk/go/sabi/golden_test.go and sdk/go/wire/wire_test.go
// case-for-case; runs as a dependency-free console harness and exits
// non-zero if any case fails.
using System.Text;
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
// Golden cases (mirror sdk/go/sabi/golden_test.go; the mandated golden gate
// is the two files also required of this probe: Envelope-v1 and
// PrincipalHandshake-v1).
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
