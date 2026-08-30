// Minimal protobuf binary wire primitives for the B-SDK-LANG-EVAL C# golden
// probe (frozen wire v1-beta, ADR-0014).
//
// Probe scope (hand-written; mirrors sdk/go/wire/wire.go): varint scalars,
// length-delimited values, and tag keys only. Fixed32/fixed64 values are
// skipped on input but never emitted; group wire types (3/4) are rejected
// fail-closed. Zigzag, maps, extensions, and service stubs are out of scope.
// This is a probe, not the full SDK protobuf stack.
namespace Llmos.Sabi.Probe;

// Wire-level failure kinds, mirroring the Go probe's sentinel errors.
public enum WireError
{
    Truncated,
    Overflow,
    BadWire,
    FieldZero,
}

public sealed class WireException : Exception
{
    public WireError Kind { get; }

    public WireException(WireError kind, string message) : base(message) => Kind = kind;
}

// Protobuf wire types.
public enum WireType : byte
{
    Varint = 0,
    Fixed64 = 1,
    Len = 2,
    SGroup = 3,
    EGroup = 4,
    Fixed32 = 5,
}

public static class Wire
{
    // Appends v in minimal protobuf base-128 varint form (1-10 bytes).
    // Output is always canonical: no redundant continuation bytes, matching
    // the deterministic serialization the frozen goldens were pinned with.
    public static void PutUvarint(List<byte> b, ulong v)
    {
        while (v >= 0x80)
        {
            b.Add((byte)(v | 0x80));
            v >>= 7;
        }
        b.Add((byte)v);
    }

    // Decodes one base-128 varint at b[pos]. Following protobuf decoder
    // semantics, non-minimal (padded) encodings are accepted on input;
    // truncation and more than 10 bytes are errors. The 10th byte may only
    // carry the final value bit.
    public static (ulong Value, int Consumed) Uvarint(byte[] b, int pos)
    {
        ulong v = 0;
        for (int i = 0; i < 10; i++)
        {
            if (pos + i >= b.Length)
            {
                throw new WireException(WireError.Truncated, "wire: truncated buffer");
            }
            byte c = b[pos + i];
            if (i == 9)
            {
                if (c > 1)
                {
                    throw new WireException(WireError.Overflow, "wire: varint exceeds 10 bytes");
                }
                return (v | (ulong)c << 63, 10);
            }
            v |= (ulong)(c & 0x7f) << (7 * i);
            if (c < 0x80)
            {
                return (v, i + 1);
            }
        }
        throw new WireException(WireError.Truncated, "wire: truncated buffer");
    }

    // Appends the varint key packing a field number and wire type.
    public static void PutTag(List<byte> b, int field, WireType wt) =>
        PutUvarint(b, ((ulong)field << 3) | (ulong)wt);

    // Appends a length-delimited value: varint length prefix + payload.
    public static void PutBytes(List<byte> b, byte[] data)
    {
        PutUvarint(b, (ulong)data.Length);
        b.AddRange(data);
    }

    // Reads one length-delimited value at b[pos]; returns the payload as a
    // fresh copy plus the total bytes consumed.
    public static (byte[] Payload, int Consumed) Bytes(byte[] b, int pos)
    {
        (ulong n, int header) = Uvarint(b, pos);
        if (b.Length - pos - header < (long)n)
        {
            throw new WireException(WireError.Truncated, "wire: truncated buffer");
        }
        var payload = new byte[n];
        Array.Copy(b, pos + header, payload, 0, (long)n);
        return (payload, header + (int)n);
    }

    // Returns the number of bytes occupied by the value of one field with
    // the given wire type, starting at b[pos] (after its tag). Wire types
    // 1 and 5 are skipped so that unknown fields of any future additive
    // extension survive a decode/encode cycle; group types are rejected.
    public static int SkipValue(byte[] b, int pos, WireType wt)
    {
        switch (wt)
        {
            case WireType.Varint:
                return Uvarint(b, pos).Consumed;
            case WireType.Fixed64:
                if (b.Length - pos < 8)
                {
                    throw new WireException(WireError.Truncated, "wire: truncated buffer");
                }
                return 8;
            case WireType.Len:
                (_, int n) = Bytes(b, pos);
                return n;
            case WireType.Fixed32:
                if (b.Length - pos < 4)
                {
                    throw new WireException(WireError.Truncated, "wire: truncated buffer");
                }
                return 4;
            default:
                throw new WireException(WireError.BadWire, "wire: unsupported wire type");
        }
    }
}
