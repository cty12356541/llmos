// Hand-written deterministic encoders and fail-closed decoders for the probe
// message set. Mirrors sdk/go/sabi/marshal.go and sdk/go/sabi/unmarshal.go.
//
// Determinism contract (must stay byte-identical with protobuf deterministic
// serialization, the basis of the frozen goldens):
//
//   - fields are emitted strictly in ascending field-number order;
//   - proto3 implicit presence: zero-valued scalars and empty
//     strings/bytes/repeateds are omitted;
//   - message fields use reference presence and are emitted (even when
//     empty) exactly when non-null;
//   - repeated uint32 is emitted packed;
//   - unknown fields captured at decode time are appended verbatim at the
//     end, mirroring protobuf-go.
//
// Decode contract:
//
//   - the target is reset first (decode-into-fresh semantics);
//   - scalar fields use last-wins on duplicate occurrences;
//   - repeated fields accumulate across occurrences (packed and unpacked
//     encodings are both accepted for repeated uint32);
//   - unknown fields of supported wire types are captured verbatim (tag +
//     value) into UnknownFields; group wire types fail closed;
//   - values that overflow 32-bit fields are rejected instead of truncated;
//   - field number 0 is rejected (reserved);
//   - a known field number arriving with an unexpected wire type is treated
//     as unknown and captured verbatim (never interpreted).
namespace Llmos.Sabi.Probe;

public static class SabiCodec
{
    private static void ThrowField(string msg, int field) =>
        throw new InvalidOperationException($"sabi: {msg}: unexpected encoding of field {field}");

    // Converts a decoded varint to uint32, rejecting overflow.
    private static uint ToU32(ulong v)
    {
        if (v > uint.MaxValue)
        {
            throw new InvalidOperationException($"sabi: varint {v} overflows uint32");
        }
        return (uint)v;
    }

    // Appends one raw unknown field (tag + value) to dst.
    private static byte[] AppendUnknown(byte[] dst, byte[] buf, int tagStart, int valuePos, int valueLen)
    {
        int oldLen = dst.Length;
        int extra = (valuePos - tagStart) + valueLen;
        Array.Resize(ref dst, oldLen + extra);
        Array.Copy(buf, tagStart, dst, oldLen, extra);
        return dst;
    }

    private static void PutString(List<byte> b, int field, string v)
    {
        Wire.PutTag(b, field, WireType.Len);
        Wire.PutBytes(b, System.Text.Encoding.UTF8.GetBytes(v));
    }

    private static void PutBytesField(List<byte> b, int field, byte[] v)
    {
        Wire.PutTag(b, field, WireType.Len);
        Wire.PutBytes(b, v);
    }

    private static void PutUint32(List<byte> b, int field, uint v)
    {
        Wire.PutTag(b, field, WireType.Varint);
        Wire.PutUvarint(b, v);
    }

    private static void PutUint64(List<byte> b, int field, ulong v)
    {
        Wire.PutTag(b, field, WireType.Varint);
        Wire.PutUvarint(b, v);
    }

    private static void PutPackedUint32(List<byte> b, int field, uint[] vs)
    {
        var body = new List<byte>();
        foreach (uint v in vs)
        {
            Wire.PutUvarint(body, v);
        }
        Wire.PutTag(b, field, WireType.Len);
        Wire.PutBytes(b, body.ToArray());
    }

    // Wraps an already-encoded sub-message body in its tag and length.
    private static void PutMessage(List<byte> b, int field, byte[] body)
    {
        Wire.PutTag(b, field, WireType.Len);
        Wire.PutBytes(b, body);
    }

    // Reads a repeated uint32 payload, accepting both the packed
    // (length-delimited) and unpacked (single varint) forms; wt selects.
    private static (uint[] Values, int Consumed) DecodePackedUint32(byte[] b, int pos, WireType wt)
    {
        if (wt == WireType.Varint)
        {
            (ulong v, int n) = Wire.Uvarint(b, pos);
            return (new[] { ToU32(v) }, n);
        }
        (byte[] payload, int total) = Wire.Bytes(b, pos);
        var values = new List<uint>();
        int i = 0;
        while (i < payload.Length)
        {
            (ulong x, int m) = Wire.Uvarint(payload, i);
            values.Add(ToU32(x));
            i += m;
        }
        return (values.ToArray(), total);
    }

    // ------------------------------------------------------------------
    // SchemaIdentity
    // ------------------------------------------------------------------

    public static byte[] Marshal(SchemaIdentity m)
    {
        var b = new List<byte>();
        if (m.Name != "")
        {
            PutString(b, 1, m.Name);
        }
        if (m.Major != 0)
        {
            PutUint32(b, 2, m.Major);
        }
        if (m.Minor != 0)
        {
            PutUint32(b, 3, m.Minor);
        }
        if (m.CriticalExtensionIDs.Length > 0)
        {
            PutPackedUint32(b, 4, m.CriticalExtensionIDs);
        }
        if (m.NonCriticalExtensionIDs.Length > 0)
        {
            PutPackedUint32(b, 5, m.NonCriticalExtensionIDs);
        }
        b.AddRange(m.UnknownFields);
        return b.ToArray();
    }

    public static void Unmarshal(SchemaIdentity m, byte[] buf)
    {
        const string msg = "SchemaIdentity";
        Reset(m);
        int i = 0;
        while (i < buf.Length)
        {
            int tagStart = i;
            (ulong tag, int tagLen) = Wire.Uvarint(buf, i);
            i += tagLen;
            int field = (int)(tag >> 3);
            var wt = (WireType)(tag & 7);
            if (field == 0)
            {
                throw new WireException(WireError.FieldZero, $"sabi: {msg}: field number 0");
            }
            switch (field)
            {
                case 1 when wt == WireType.Len:
                    (byte[] v, int n1) = Wire.Bytes(buf, i);
                    m.Name = System.Text.Encoding.UTF8.GetString(v);
                    i += n1;
                    break;
                case 2 when wt == WireType.Varint:
                    (ulong v2, int n2) = Wire.Uvarint(buf, i);
                    m.Major = ToU32(v2);
                    i += n2;
                    break;
                case 3 when wt == WireType.Varint:
                    (ulong v3, int n3) = Wire.Uvarint(buf, i);
                    m.Minor = ToU32(v3);
                    i += n3;
                    break;
                case 4 when wt is WireType.Len or WireType.Varint:
                    (uint[] vs4, int n4) = DecodePackedUint32(buf, i, wt);
                    m.CriticalExtensionIDs = m.CriticalExtensionIDs.Concat(vs4).ToArray();
                    i += n4;
                    break;
                case 5 when wt is WireType.Len or WireType.Varint:
                    (uint[] vs5, int n5) = DecodePackedUint32(buf, i, wt);
                    m.NonCriticalExtensionIDs = m.NonCriticalExtensionIDs.Concat(vs5).ToArray();
                    i += n5;
                    break;
                default:
                    int nu = Wire.SkipValue(buf, i, wt);
                    m.UnknownFields = AppendUnknown(m.UnknownFields, buf, tagStart, i, nu);
                    i += nu;
                    break;
            }
        }
    }

    private static void Reset(SchemaIdentity m)
    {
        m.Name = "";
        m.Major = 0;
        m.Minor = 0;
        m.CriticalExtensionIDs = Array.Empty<uint>();
        m.NonCriticalExtensionIDs = Array.Empty<uint>();
        m.UnknownFields = Array.Empty<byte>();
    }

    // ------------------------------------------------------------------
    // CallerIdentity
    // ------------------------------------------------------------------

    public static byte[] Marshal(CallerIdentity m)
    {
        var b = new List<byte>();
        if (m.PrincipalID.Length > 0)
        {
            PutBytesField(b, 1, m.PrincipalID);
        }
        if (m.ApplicationID.Length > 0)
        {
            PutBytesField(b, 2, m.ApplicationID);
        }
        if (m.ProcessID.Length > 0)
        {
            PutBytesField(b, 3, m.ProcessID);
        }
        if (m.ProcessGeneration != 0)
        {
            PutUint64(b, 4, m.ProcessGeneration);
        }
        b.AddRange(m.UnknownFields);
        return b.ToArray();
    }

    public static void Unmarshal(CallerIdentity m, byte[] buf)
    {
        const string msg = "CallerIdentity";
        Reset(m);
        int i = 0;
        while (i < buf.Length)
        {
            int tagStart = i;
            (ulong tag, int tagLen) = Wire.Uvarint(buf, i);
            i += tagLen;
            int field = (int)(tag >> 3);
            var wt = (WireType)(tag & 7);
            if (field == 0)
            {
                throw new WireException(WireError.FieldZero, $"sabi: {msg}: field number 0");
            }
            switch (field)
            {
                case 1 when wt == WireType.Len:
                    (m.PrincipalID, int n1) = Wire.Bytes(buf, i);
                    i += n1;
                    break;
                case 2 when wt == WireType.Len:
                    (m.ApplicationID, int n2) = Wire.Bytes(buf, i);
                    i += n2;
                    break;
                case 3 when wt == WireType.Len:
                    (m.ProcessID, int n3) = Wire.Bytes(buf, i);
                    i += n3;
                    break;
                case 4 when wt == WireType.Varint:
                    (m.ProcessGeneration, int n4) = Wire.Uvarint(buf, i);
                    i += n4;
                    break;
                default:
                    int nu = Wire.SkipValue(buf, i, wt);
                    m.UnknownFields = AppendUnknown(m.UnknownFields, buf, tagStart, i, nu);
                    i += nu;
                    break;
            }
        }
    }

    private static void Reset(CallerIdentity m)
    {
        m.PrincipalID = Array.Empty<byte>();
        m.ApplicationID = Array.Empty<byte>();
        m.ProcessID = Array.Empty<byte>();
        m.ProcessGeneration = 0;
        m.UnknownFields = Array.Empty<byte>();
    }

    // ------------------------------------------------------------------
    // TaskExecutionBinding
    // ------------------------------------------------------------------

    public static byte[] Marshal(TaskExecutionBinding m)
    {
        var b = new List<byte>();
        if (m.TaskAttemptID.Length > 0)
        {
            PutBytesField(b, 1, m.TaskAttemptID);
        }
        if (m.TaskAuthorityTerm != 0)
        {
            PutUint64(b, 2, m.TaskAuthorityTerm);
        }
        if (m.TaskControlEpoch != 0)
        {
            PutUint64(b, 3, m.TaskControlEpoch);
        }
        if (m.CancelEpoch != 0)
        {
            PutUint64(b, 4, m.CancelEpoch);
        }
        if (m.PermitEpoch != 0)
        {
            PutUint64(b, 5, m.PermitEpoch);
        }
        if (m.IsolationDomainGeneration != 0)
        {
            PutUint64(b, 6, m.IsolationDomainGeneration);
        }
        b.AddRange(m.UnknownFields);
        return b.ToArray();
    }

    public static void Unmarshal(TaskExecutionBinding m, byte[] buf)
    {
        const string msg = "TaskExecutionBinding";
        Reset(m);
        int i = 0;
        while (i < buf.Length)
        {
            int tagStart = i;
            (ulong tag, int tagLen) = Wire.Uvarint(buf, i);
            i += tagLen;
            int field = (int)(tag >> 3);
            var wt = (WireType)(tag & 7);
            if (field == 0)
            {
                throw new WireException(WireError.FieldZero, $"sabi: {msg}: field number 0");
            }
            switch (field)
            {
                case 1 when wt == WireType.Len:
                    (m.TaskAttemptID, int n1) = Wire.Bytes(buf, i);
                    i += n1;
                    break;
                case >= 2 and <= 6 when wt == WireType.Varint:
                    (ulong v, int n) = Wire.Uvarint(buf, i);
                    switch (field)
                    {
                        case 2: m.TaskAuthorityTerm = v; break;
                        case 3: m.TaskControlEpoch = v; break;
                        case 4: m.CancelEpoch = v; break;
                        case 5: m.PermitEpoch = v; break;
                        case 6: m.IsolationDomainGeneration = v; break;
                    }
                    i += n;
                    break;
                default:
                    int nu = Wire.SkipValue(buf, i, wt);
                    m.UnknownFields = AppendUnknown(m.UnknownFields, buf, tagStart, i, nu);
                    i += nu;
                    break;
            }
        }
    }

    private static void Reset(TaskExecutionBinding m)
    {
        m.TaskAttemptID = Array.Empty<byte>();
        m.TaskAuthorityTerm = 0;
        m.TaskControlEpoch = 0;
        m.CancelEpoch = 0;
        m.PermitEpoch = 0;
        m.IsolationDomainGeneration = 0;
        m.UnknownFields = Array.Empty<byte>();
    }

    // ------------------------------------------------------------------
    // CapabilityHandle
    // ------------------------------------------------------------------

    public static byte[] Marshal(CapabilityHandle m)
    {
        var b = new List<byte>();
        if (m.Slot != 0)
        {
            PutUint64(b, 1, m.Slot);
        }
        if (m.Generation != 0)
        {
            PutUint64(b, 2, m.Generation);
        }
        b.AddRange(m.UnknownFields);
        return b.ToArray();
    }

    public static void Unmarshal(CapabilityHandle m, byte[] buf)
    {
        const string msg = "CapabilityHandle";
        Reset(m);
        int i = 0;
        while (i < buf.Length)
        {
            int tagStart = i;
            (ulong tag, int tagLen) = Wire.Uvarint(buf, i);
            i += tagLen;
            int field = (int)(tag >> 3);
            var wt = (WireType)(tag & 7);
            if (field == 0)
            {
                throw new WireException(WireError.FieldZero, $"sabi: {msg}: field number 0");
            }
            switch (field)
            {
                case 1 when wt == WireType.Varint:
                    (m.Slot, int n1) = Wire.Uvarint(buf, i);
                    i += n1;
                    break;
                case 2 when wt == WireType.Varint:
                    (m.Generation, int n2) = Wire.Uvarint(buf, i);
                    i += n2;
                    break;
                default:
                    int nu = Wire.SkipValue(buf, i, wt);
                    m.UnknownFields = AppendUnknown(m.UnknownFields, buf, tagStart, i, nu);
                    i += nu;
                    break;
            }
        }
    }

    private static void Reset(CapabilityHandle m)
    {
        m.Slot = 0;
        m.Generation = 0;
        m.UnknownFields = Array.Empty<byte>();
    }

    // ------------------------------------------------------------------
    // SabiRequestContext
    // ------------------------------------------------------------------

    public static byte[] Marshal(SabiRequestContext m)
    {
        var b = new List<byte>();
        if (m.Caller != null)
        {
            PutMessage(b, 1, Marshal(m.Caller));
        }
        if (m.ActivityContext.Length > 0)
        {
            PutBytesField(b, 2, m.ActivityContext);
        }
        if (m.TaskExecutionBinding != null)
        {
            PutMessage(b, 3, Marshal(m.TaskExecutionBinding));
        }
        if (m.CorrelationID.Length > 0)
        {
            PutBytesField(b, 4, m.CorrelationID);
        }
        if (m.IdempotencyKey.Length > 0)
        {
            PutBytesField(b, 5, m.IdempotencyKey);
        }
        if (m.DeadlineMonotonicNS != 0)
        {
            PutUint64(b, 6, m.DeadlineMonotonicNS);
        }
        foreach (CapabilityHandle h in m.CapabilityHandles)
        {
            if (h != null)
            {
                PutMessage(b, 7, Marshal(h));
            }
        }
        if (m.ReservationHandle != null)
        {
            PutMessage(b, 8, Marshal(m.ReservationHandle));
        }
        if (m.ProposalOrInputDigestSHA256.Length > 0)
        {
            PutBytesField(b, 9, m.ProposalOrInputDigestSHA256);
        }
        b.AddRange(m.UnknownFields);
        return b.ToArray();
    }

    public static void Unmarshal(SabiRequestContext m, byte[] buf)
    {
        const string msg = "SabiRequestContext";
        Reset(m);
        int i = 0;
        while (i < buf.Length)
        {
            int tagStart = i;
            (ulong tag, int tagLen) = Wire.Uvarint(buf, i);
            i += tagLen;
            int field = (int)(tag >> 3);
            var wt = (WireType)(tag & 7);
            if (field == 0)
            {
                throw new WireException(WireError.FieldZero, $"sabi: {msg}: field number 0");
            }
            switch (field)
            {
                case 1 when wt == WireType.Len:
                    (byte[] v1, int n1) = Wire.Bytes(buf, i);
                    m.Caller = new CallerIdentity();
                    Unmarshal(m.Caller, v1);
                    i += n1;
                    break;
                case 2 when wt == WireType.Len:
                    (m.ActivityContext, int n2) = Wire.Bytes(buf, i);
                    i += n2;
                    break;
                case 3 when wt == WireType.Len:
                    (byte[] v3, int n3) = Wire.Bytes(buf, i);
                    m.TaskExecutionBinding = new TaskExecutionBinding();
                    Unmarshal(m.TaskExecutionBinding, v3);
                    i += n3;
                    break;
                case 4 when wt == WireType.Len:
                    (m.CorrelationID, int n4) = Wire.Bytes(buf, i);
                    i += n4;
                    break;
                case 5 when wt == WireType.Len:
                    (m.IdempotencyKey, int n5) = Wire.Bytes(buf, i);
                    i += n5;
                    break;
                case 6 when wt == WireType.Varint:
                    (m.DeadlineMonotonicNS, int n6) = Wire.Uvarint(buf, i);
                    i += n6;
                    break;
                case 7 when wt == WireType.Len:
                    (byte[] v7, int n7) = Wire.Bytes(buf, i);
                    var h = new CapabilityHandle();
                    Unmarshal(h, v7);
                    m.CapabilityHandles.Add(h);
                    i += n7;
                    break;
                case 8 when wt == WireType.Len:
                    (byte[] v8, int n8) = Wire.Bytes(buf, i);
                    m.ReservationHandle = new CapabilityHandle();
                    Unmarshal(m.ReservationHandle, v8);
                    i += n8;
                    break;
                case 9 when wt == WireType.Len:
                    (m.ProposalOrInputDigestSHA256, int n9) = Wire.Bytes(buf, i);
                    i += n9;
                    break;
                default:
                    int nu = Wire.SkipValue(buf, i, wt);
                    m.UnknownFields = AppendUnknown(m.UnknownFields, buf, tagStart, i, nu);
                    i += nu;
                    break;
            }
        }
    }

    private static void Reset(SabiRequestContext m)
    {
        m.Caller = null;
        m.ActivityContext = Array.Empty<byte>();
        m.TaskExecutionBinding = null;
        m.CorrelationID = Array.Empty<byte>();
        m.IdempotencyKey = Array.Empty<byte>();
        m.DeadlineMonotonicNS = 0;
        m.CapabilityHandles = new List<CapabilityHandle>();
        m.ReservationHandle = null;
        m.ProposalOrInputDigestSHA256 = Array.Empty<byte>();
        m.UnknownFields = Array.Empty<byte>();
    }

    // ------------------------------------------------------------------
    // SabiFailure
    // ------------------------------------------------------------------

    public static byte[] Marshal(SabiFailure m)
    {
        var b = new List<byte>();
        if (m.Code != 0)
        {
            PutUint32(b, 1, (uint)m.Code);
        }
        if (m.Retry != 0)
        {
            PutUint32(b, 2, (uint)m.Retry);
        }
        if (m.SafeMessage != "")
        {
            PutString(b, 3, m.SafeMessage);
        }
        b.AddRange(m.UnknownFields);
        return b.ToArray();
    }

    public static void Unmarshal(SabiFailure m, byte[] buf)
    {
        const string msg = "SabiFailure";
        Reset(m);
        int i = 0;
        while (i < buf.Length)
        {
            int tagStart = i;
            (ulong tag, int tagLen) = Wire.Uvarint(buf, i);
            i += tagLen;
            int field = (int)(tag >> 3);
            var wt = (WireType)(tag & 7);
            if (field == 0)
            {
                throw new WireException(WireError.FieldZero, $"sabi: {msg}: field number 0");
            }
            switch (field)
            {
                case 1 when wt == WireType.Varint:
                    (ulong v1, int n1) = Wire.Uvarint(buf, i);
                    m.Code = (SabiErrorCode)ToU32(v1);
                    i += n1;
                    break;
                case 2 when wt == WireType.Varint:
                    (ulong v2, int n2) = Wire.Uvarint(buf, i);
                    m.Retry = (RetryDirective)ToU32(v2);
                    i += n2;
                    break;
                case 3 when wt == WireType.Len:
                    (byte[] v, int n3) = Wire.Bytes(buf, i);
                    m.SafeMessage = System.Text.Encoding.UTF8.GetString(v);
                    i += n3;
                    break;
                default:
                    int nu = Wire.SkipValue(buf, i, wt);
                    m.UnknownFields = AppendUnknown(m.UnknownFields, buf, tagStart, i, nu);
                    i += nu;
                    break;
            }
        }
    }

    private static void Reset(SabiFailure m)
    {
        m.Code = 0;
        m.Retry = 0;
        m.SafeMessage = "";
        m.UnknownFields = Array.Empty<byte>();
    }

    // ------------------------------------------------------------------
    // OperationReference
    // ------------------------------------------------------------------

    public static byte[] Marshal(OperationReference m)
    {
        var b = new List<byte>();
        if (m.OperationID.Length > 0)
        {
            PutBytesField(b, 1, m.OperationID);
        }
        if (m.Generation != 0)
        {
            PutUint64(b, 2, m.Generation);
        }
        b.AddRange(m.UnknownFields);
        return b.ToArray();
    }

    public static void Unmarshal(OperationReference m, byte[] buf)
    {
        const string msg = "OperationReference";
        Reset(m);
        int i = 0;
        while (i < buf.Length)
        {
            int tagStart = i;
            (ulong tag, int tagLen) = Wire.Uvarint(buf, i);
            i += tagLen;
            int field = (int)(tag >> 3);
            var wt = (WireType)(tag & 7);
            if (field == 0)
            {
                throw new WireException(WireError.FieldZero, $"sabi: {msg}: field number 0");
            }
            switch (field)
            {
                case 1 when wt == WireType.Len:
                    (m.OperationID, int n1) = Wire.Bytes(buf, i);
                    i += n1;
                    break;
                case 2 when wt == WireType.Varint:
                    (m.Generation, int n2) = Wire.Uvarint(buf, i);
                    i += n2;
                    break;
                default:
                    int nu = Wire.SkipValue(buf, i, wt);
                    m.UnknownFields = AppendUnknown(m.UnknownFields, buf, tagStart, i, nu);
                    i += nu;
                    break;
            }
        }
    }

    private static void Reset(OperationReference m)
    {
        m.OperationID = Array.Empty<byte>();
        m.Generation = 0;
        m.UnknownFields = Array.Empty<byte>();
    }

    // ------------------------------------------------------------------
    // ReceiptReference
    // ------------------------------------------------------------------

    public static byte[] Marshal(ReceiptReference m)
    {
        var b = new List<byte>();
        if (m.ReceiptID.Length > 0)
        {
            PutBytesField(b, 1, m.ReceiptID);
        }
        b.AddRange(m.UnknownFields);
        return b.ToArray();
    }

    public static void Unmarshal(ReceiptReference m, byte[] buf)
    {
        const string msg = "ReceiptReference";
        Reset(m);
        int i = 0;
        while (i < buf.Length)
        {
            int tagStart = i;
            (ulong tag, int tagLen) = Wire.Uvarint(buf, i);
            i += tagLen;
            int field = (int)(tag >> 3);
            var wt = (WireType)(tag & 7);
            if (field == 0)
            {
                throw new WireException(WireError.FieldZero, $"sabi: {msg}: field number 0");
            }
            switch (field)
            {
                case 1 when wt == WireType.Len:
                    (m.ReceiptID, int n1) = Wire.Bytes(buf, i);
                    i += n1;
                    break;
                default:
                    int nu = Wire.SkipValue(buf, i, wt);
                    m.UnknownFields = AppendUnknown(m.UnknownFields, buf, tagStart, i, nu);
                    i += nu;
                    break;
            }
        }
    }

    private static void Reset(ReceiptReference m)
    {
        m.ReceiptID = Array.Empty<byte>();
        m.UnknownFields = Array.Empty<byte>();
    }

    // ------------------------------------------------------------------
    // SabiResponseContext
    // ------------------------------------------------------------------

    public static byte[] Marshal(SabiResponseContext m)
    {
        var b = new List<byte>();
        if (m.CorrelationID.Length > 0)
        {
            PutBytesField(b, 1, m.CorrelationID);
        }
        if (m.Operation != null)
        {
            PutMessage(b, 2, Marshal(m.Operation));
        }
        foreach (ReceiptReference r in m.Receipts)
        {
            if (r != null)
            {
                PutMessage(b, 3, Marshal(r));
            }
        }
        if (m.Failure != null)
        {
            PutMessage(b, 4, Marshal(m.Failure));
        }
        b.AddRange(m.UnknownFields);
        return b.ToArray();
    }

    public static void Unmarshal(SabiResponseContext m, byte[] buf)
    {
        const string msg = "SabiResponseContext";
        Reset(m);
        int i = 0;
        while (i < buf.Length)
        {
            int tagStart = i;
            (ulong tag, int tagLen) = Wire.Uvarint(buf, i);
            i += tagLen;
            int field = (int)(tag >> 3);
            var wt = (WireType)(tag & 7);
            if (field == 0)
            {
                throw new WireException(WireError.FieldZero, $"sabi: {msg}: field number 0");
            }
            switch (field)
            {
                case 1 when wt == WireType.Len:
                    (m.CorrelationID, int n1) = Wire.Bytes(buf, i);
                    i += n1;
                    break;
                case 2 when wt == WireType.Len:
                    (byte[] v2, int n2) = Wire.Bytes(buf, i);
                    m.Operation = new OperationReference();
                    Unmarshal(m.Operation, v2);
                    i += n2;
                    break;
                case 3 when wt == WireType.Len:
                    (byte[] v3, int n3) = Wire.Bytes(buf, i);
                    var r = new ReceiptReference();
                    Unmarshal(r, v3);
                    m.Receipts.Add(r);
                    i += n3;
                    break;
                case 4 when wt == WireType.Len:
                    (byte[] v4, int n4) = Wire.Bytes(buf, i);
                    m.Failure = new SabiFailure();
                    Unmarshal(m.Failure, v4);
                    i += n4;
                    break;
                default:
                    int nu = Wire.SkipValue(buf, i, wt);
                    m.UnknownFields = AppendUnknown(m.UnknownFields, buf, tagStart, i, nu);
                    i += nu;
                    break;
            }
        }
    }

    private static void Reset(SabiResponseContext m)
    {
        m.CorrelationID = Array.Empty<byte>();
        m.Operation = null;
        m.Receipts = new List<ReceiptReference>();
        m.Failure = null;
        m.UnknownFields = Array.Empty<byte>();
    }

    // ------------------------------------------------------------------
    // Envelope
    // ------------------------------------------------------------------

    // Encodes m deterministically. Throws InvalidOperationException if both
    // arms of the common_context oneof are set; the probe treats that as an
    // unrepresentable state rather than a typed error (mirrors the Go
    // probe's panic).
    public static byte[] Marshal(Envelope m)
    {
        if (m.RequestContext != null && m.ResponseContext != null)
        {
            throw new InvalidOperationException(
                $"sabi: Envelope {SchemaIdentityName(m)} oneof common_context has both arms set");
        }
        var b = new List<byte>();
        if (m.Schema != null)
        {
            PutMessage(b, 1, Marshal(m.Schema));
        }
        if (m.RequestID.Length > 0)
        {
            PutBytesField(b, 2, m.RequestID);
        }
        if (m.Service != "")
        {
            PutString(b, 3, m.Service);
        }
        if (m.Method != "")
        {
            PutString(b, 4, m.Method);
        }
        if (m.RequestContext != null)
        {
            PutMessage(b, 5, Marshal(m.RequestContext));
        }
        if (m.ResponseContext != null)
        {
            PutMessage(b, 6, Marshal(m.ResponseContext));
        }
        if (m.Payload.Length > 0)
        {
            PutBytesField(b, 15, m.Payload);
        }
        b.AddRange(m.UnknownFields);
        return b.ToArray();
    }

    // Returns the schema name carried by the Envelope, for diagnostics.
    private static string SchemaIdentityName(Envelope m) => m.Schema?.Name ?? "";

    public static void Unmarshal(Envelope m, byte[] buf)
    {
        const string msg = "Envelope";
        Reset(m);
        int i = 0;
        while (i < buf.Length)
        {
            int tagStart = i;
            (ulong tag, int tagLen) = Wire.Uvarint(buf, i);
            i += tagLen;
            int field = (int)(tag >> 3);
            var wt = (WireType)(tag & 7);
            if (field == 0)
            {
                throw new WireException(WireError.FieldZero, $"sabi: {msg}: field number 0");
            }
            switch (field)
            {
                case 1 when wt == WireType.Len:
                    (byte[] v1, int n1) = Wire.Bytes(buf, i);
                    m.Schema = new SchemaIdentity();
                    Unmarshal(m.Schema, v1);
                    i += n1;
                    break;
                case 2 when wt == WireType.Len:
                    (m.RequestID, int n2) = Wire.Bytes(buf, i);
                    i += n2;
                    break;
                case 3 when wt == WireType.Len:
                    (byte[] v3, int n3) = Wire.Bytes(buf, i);
                    m.Service = System.Text.Encoding.UTF8.GetString(v3);
                    i += n3;
                    break;
                case 4 when wt == WireType.Len:
                    (byte[] v4, int n4) = Wire.Bytes(buf, i);
                    m.Method = System.Text.Encoding.UTF8.GetString(v4);
                    i += n4;
                    break;
                case 5 when wt == WireType.Len:
                    (byte[] v5, int n5) = Wire.Bytes(buf, i);
                    m.RequestContext = new SabiRequestContext();
                    Unmarshal(m.RequestContext, v5);
                    i += n5;
                    break;
                case 6 when wt == WireType.Len:
                    (byte[] v6, int n6) = Wire.Bytes(buf, i);
                    m.ResponseContext = new SabiResponseContext();
                    Unmarshal(m.ResponseContext, v6);
                    i += n6;
                    break;
                case 15 when wt == WireType.Len:
                    (m.Payload, int n15) = Wire.Bytes(buf, i);
                    i += n15;
                    break;
                default:
                    int nu = Wire.SkipValue(buf, i, wt);
                    m.UnknownFields = AppendUnknown(m.UnknownFields, buf, tagStart, i, nu);
                    i += nu;
                    break;
            }
        }
    }

    private static void Reset(Envelope m)
    {
        m.Schema = null;
        m.RequestID = Array.Empty<byte>();
        m.Service = "";
        m.Method = "";
        m.RequestContext = null;
        m.ResponseContext = null;
        m.Payload = Array.Empty<byte>();
        m.UnknownFields = Array.Empty<byte>();
    }

    // ------------------------------------------------------------------
    // PrincipalHandshakeAttestation
    // ------------------------------------------------------------------

    public static byte[] Marshal(PrincipalHandshakeAttestation m)
    {
        var b = new List<byte>();
        if (m.Schema != null)
        {
            PutMessage(b, 1, Marshal(m.Schema));
        }
        if (m.PrincipalID.Length > 0)
        {
            PutBytesField(b, 2, m.PrincipalID);
        }
        if (m.Nonce.Length > 0)
        {
            PutBytesField(b, 3, m.Nonce);
        }
        if (m.ChannelBinding.Length > 0)
        {
            PutBytesField(b, 4, m.ChannelBinding);
        }
        if (m.Signature.Length > 0)
        {
            PutBytesField(b, 5, m.Signature);
        }
        b.AddRange(m.UnknownFields);
        return b.ToArray();
    }

    public static void Unmarshal(PrincipalHandshakeAttestation m, byte[] buf)
    {
        const string msg = "PrincipalHandshakeAttestation";
        Reset(m);
        int i = 0;
        while (i < buf.Length)
        {
            int tagStart = i;
            (ulong tag, int tagLen) = Wire.Uvarint(buf, i);
            i += tagLen;
            int field = (int)(tag >> 3);
            var wt = (WireType)(tag & 7);
            if (field == 0)
            {
                throw new WireException(WireError.FieldZero, $"sabi: {msg}: field number 0");
            }
            switch (field)
            {
                case 1 when wt == WireType.Len:
                    (byte[] v, int n1) = Wire.Bytes(buf, i);
                    m.Schema = new SchemaIdentity();
                    Unmarshal(m.Schema, v);
                    i += n1;
                    break;
                case 2 when wt == WireType.Len:
                    (m.PrincipalID, int n2) = Wire.Bytes(buf, i);
                    i += n2;
                    break;
                case 3 when wt == WireType.Len:
                    (m.Nonce, int n3) = Wire.Bytes(buf, i);
                    i += n3;
                    break;
                case 4 when wt == WireType.Len:
                    (m.ChannelBinding, int n4) = Wire.Bytes(buf, i);
                    i += n4;
                    break;
                case 5 when wt == WireType.Len:
                    (m.Signature, int n5) = Wire.Bytes(buf, i);
                    i += n5;
                    break;
                default:
                    int nu = Wire.SkipValue(buf, i, wt);
                    m.UnknownFields = AppendUnknown(m.UnknownFields, buf, tagStart, i, nu);
                    i += nu;
                    break;
            }
        }
    }

    private static void Reset(PrincipalHandshakeAttestation m)
    {
        m.Schema = null;
        m.PrincipalID = Array.Empty<byte>();
        m.Nonce = Array.Empty<byte>();
        m.ChannelBinding = Array.Empty<byte>();
        m.Signature = Array.Empty<byte>();
        m.UnknownFields = Array.Empty<byte>();
    }
}
