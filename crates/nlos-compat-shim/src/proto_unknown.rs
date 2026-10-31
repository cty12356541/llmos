//! Safe, bounded Protobuf wire-tag scanner and injector for `[COMPAT-VER-002]`
//! lossless unknown field preservation across out-of-core epoch translation.
//!
//! Standard `prost::Message::decode` followed by `encode_to_vec()` drops
//! unrecognized Protobuf field tags. Under `[COMPAT-VER-002]` and `ADR-0027`
//! section 2.1, an out-of-core compatibility shim rewriting `SchemaIdentity.major`
//! (`N-1` / `N-2` <-> `N`) must preserve both `SchemaIdentity.non_critical_extension_ids`
//! and any unknown raw Protobuf wire fields attached to `Envelope`, `SchemaIdentity`,
//! `SabiRequestContext`, or `SabiResponseContext`.

use std::error::Error;
use std::fmt;

use nlos_schema::MAX_ENVELOPE_BYTES;
use nlos_schema::sabi::v1::{Envelope, envelope};
use prost::Message;

const WIRE_TYPE_VARINT: u8 = 0;
const WIRE_TYPE_64BIT: u8 = 1;
const WIRE_TYPE_LENGTH_DELIMITED: u8 = 2;
const WIRE_TYPE_32BIT: u8 = 5;

const ENVELOPE_KNOWN_TAGS: &[u32] = &[1, 2, 3, 4, 5, 6, 15];
const SCHEMA_IDENTITY_KNOWN_TAGS: &[u32] = &[1, 2, 3, 4, 5];
const REQUEST_CONTEXT_KNOWN_TAGS: &[u32] = &[1, 2, 3, 4, 5, 6, 7, 8, 9];
const RESPONSE_CONTEXT_KNOWN_TAGS: &[u32] = &[1, 2, 3, 4];

/// One raw unrecognized Protobuf wire field (`tag+wire_type` prefix plus value bytes).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawUnknownField {
    /// Protobuf field number (`key >> 3`).
    pub field_number: u32,
    /// Protobuf wire type (`key & 0x07`).
    pub wire_type: u8,
    /// Exact raw wire slice covering both the key varint and the field payload.
    pub raw_bytes: Vec<u8>,
}

/// All unrecognized Protobuf wire fields extracted from an [`Envelope`] frame
/// and its standard metadata submessages.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExtractedEnvelopeUnknowns {
    /// Unrecognized tags on the outer `nlos.sabi.v1.Envelope` message.
    pub envelope_fields: Vec<RawUnknownField>,
    /// Unrecognized tags inside `Envelope.schema` (`SchemaIdentity`, tag `1`).
    pub schema_fields: Vec<RawUnknownField>,
    /// Unrecognized tags inside `Envelope.request_context` (`SabiRequestContext`, tag `5`).
    pub request_context_fields: Vec<RawUnknownField>,
    /// Unrecognized tags inside `Envelope.response_context` (`SabiResponseContext`, tag `6`).
    pub response_context_fields: Vec<RawUnknownField>,
}

impl ExtractedEnvelopeUnknowns {
    /// Returns `true` when neither the outer envelope nor any nested metadata
    /// submessage carried unknown Protobuf tags.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.envelope_fields.is_empty()
            && self.schema_fields.is_empty()
            && self.request_context_fields.is_empty()
            && self.response_context_fields.is_empty()
    }

    /// Total byte length of all preserved unknown Protobuf wire fields.
    #[must_use]
    pub fn total_raw_bytes(&self) -> usize {
        let sum_slice = |items: &[RawUnknownField]| -> usize {
            items.iter().map(|item| item.raw_bytes.len()).sum()
        };
        sum_slice(&self.envelope_fields)
            + sum_slice(&self.schema_fields)
            + sum_slice(&self.request_context_fields)
            + sum_slice(&self.response_context_fields)
    }
}

/// Wire-scanning errors surfaced when inspecting raw Protobuf tag streams.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WireScanError {
    /// Frame exceeds `MAX_ENVELOPE_BYTES`.
    FrameTooLarge { actual: usize, maximum: usize },
    /// Truncated varint or length-delimited slice.
    TruncatedField,
    /// Varint exceeded 64 bits or 10 encoded bytes.
    VarintOverflow,
    /// Field number was `0` or exceeded `i32::MAX` (`536_870_911` max valid Protobuf field).
    InvalidFieldNumber(u64),
    /// Unsupported or deprecated Protobuf wire type (such as `3`/`4` groups or `6`/`7`).
    UnsupportedWireType(u8),
}

impl fmt::Display for WireScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameTooLarge { actual, maximum } => write!(
                formatter,
                "protobuf frame size {actual} exceeds bound {maximum}"
            ),
            Self::TruncatedField => formatter.write_str("truncated protobuf wire field"),
            Self::VarintOverflow => formatter.write_str("protobuf varint overflow"),
            Self::InvalidFieldNumber(number) => {
                write!(formatter, "invalid protobuf field number {number}")
            }
            Self::UnsupportedWireType(wire_type) => {
                write!(formatter, "unsupported protobuf wire type {wire_type}")
            }
        }
    }
}

impl Error for WireScanError {}

/// Scans a raw `nlos.sabi.v1.Envelope` wire buffer and extracts all unknown
/// Protobuf fields on the outer `Envelope` and its nested `SchemaIdentity`,
/// `SabiRequestContext`, and `SabiResponseContext` submessages.
///
/// # Errors
///
/// Fails closed with [`WireScanError`] on oversized buffers, truncated fields,
/// invalid field numbers, varint overflow, or unsupported group wire types.
pub fn extract_envelope_unknown_fields(
    wire: &[u8],
) -> Result<ExtractedEnvelopeUnknowns, WireScanError> {
    if wire.len() > MAX_ENVELOPE_BYTES {
        return Err(WireScanError::FrameTooLarge {
            actual: wire.len(),
            maximum: MAX_ENVELOPE_BYTES,
        });
    }

    let mut extracted = ExtractedEnvelopeUnknowns::default();
    let mut cursor = 0_usize;
    while cursor < wire.len() {
        let parsed = next_wire_field(wire, cursor)?;
        if ENVELOPE_KNOWN_TAGS.contains(&parsed.field_number) {
            if parsed.wire_type == WIRE_TYPE_LENGTH_DELIMITED {
                match parsed.field_number {
                    1 => scan_unknown_submessage(
                        parsed.value_slice,
                        SCHEMA_IDENTITY_KNOWN_TAGS,
                        &mut extracted.schema_fields,
                    )?,
                    5 => scan_unknown_submessage(
                        parsed.value_slice,
                        REQUEST_CONTEXT_KNOWN_TAGS,
                        &mut extracted.request_context_fields,
                    )?,
                    6 => scan_unknown_submessage(
                        parsed.value_slice,
                        RESPONSE_CONTEXT_KNOWN_TAGS,
                        &mut extracted.response_context_fields,
                    )?,
                    _ => {}
                }
            }
        } else {
            extracted.envelope_fields.push(RawUnknownField {
                field_number: parsed.field_number,
                wire_type: parsed.wire_type,
                raw_bytes: wire[cursor..parsed.next_offset].to_vec(),
            });
        }
        cursor = parsed.next_offset;
    }
    Ok(extracted)
}

/// Encodes an [`Envelope`] together with previously extracted unknown Protobuf
/// wire fields so unknown non-critical tags survive epoch translation losslessly.
///
/// # Errors
///
/// Returns [`WireScanError::FrameTooLarge`] if the resulting frame exceeds
/// [`MAX_ENVELOPE_BYTES`].
pub fn encode_envelope_with_unknown_fields(
    envelope: &Envelope,
    unknowns: &ExtractedEnvelopeUnknowns,
) -> Result<Vec<u8>, WireScanError> {
    if unknowns.is_empty() {
        let out = envelope.encode_to_vec();
        if out.len() > MAX_ENVELOPE_BYTES {
            return Err(WireScanError::FrameTooLarge {
                actual: out.len(),
                maximum: MAX_ENVELOPE_BYTES,
            });
        }
        return Ok(out);
    }

    let mut out = Vec::new();

    if let Some(schema) = &envelope.schema {
        let mut schema_bytes = schema.encode_to_vec();
        append_raw_fields(&mut schema_bytes, &unknowns.schema_fields);
        encode_length_delimited_field(1, &schema_bytes, &mut out);
    }

    let stripped = Envelope {
        schema: None,
        request_id: envelope.request_id.clone(),
        service: envelope.service.clone(),
        method: envelope.method.clone(),
        common_context: None,
        payload: Vec::new(),
    };
    out.extend_from_slice(&stripped.encode_to_vec());

    match &envelope.common_context {
        Some(envelope::CommonContext::RequestContext(req_ctx)) => {
            let mut ctx_bytes = req_ctx.encode_to_vec();
            append_raw_fields(&mut ctx_bytes, &unknowns.request_context_fields);
            encode_length_delimited_field(5, &ctx_bytes, &mut out);
        }
        Some(envelope::CommonContext::ResponseContext(resp_ctx)) => {
            let mut ctx_bytes = resp_ctx.encode_to_vec();
            append_raw_fields(&mut ctx_bytes, &unknowns.response_context_fields);
            encode_length_delimited_field(6, &ctx_bytes, &mut out);
        }
        None => {}
    }

    if !envelope.payload.is_empty() {
        encode_length_delimited_field(15, &envelope.payload, &mut out);
    }

    append_raw_fields(&mut out, &unknowns.envelope_fields);

    if out.len() > MAX_ENVELOPE_BYTES {
        return Err(WireScanError::FrameTooLarge {
            actual: out.len(),
            maximum: MAX_ENVELOPE_BYTES,
        });
    }
    Ok(out)
}

fn scan_unknown_submessage(
    bytes: &[u8],
    known_tags: &[u32],
    sink: &mut Vec<RawUnknownField>,
) -> Result<(), WireScanError> {
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        let parsed = next_wire_field(bytes, cursor)?;
        if !known_tags.contains(&parsed.field_number) {
            sink.push(RawUnknownField {
                field_number: parsed.field_number,
                wire_type: parsed.wire_type,
                raw_bytes: bytes[cursor..parsed.next_offset].to_vec(),
            });
        }
        cursor = parsed.next_offset;
    }
    Ok(())
}

struct ParsedField<'a> {
    field_number: u32,
    wire_type: u8,
    value_slice: &'a [u8],
    next_offset: usize,
}

fn next_wire_field(bytes: &[u8], start: usize) -> Result<ParsedField<'_>, WireScanError> {
    const MAX_PROTOBUF_FIELD_NUMBER: u64 = 536_870_911;

    let (key, after_key) = decode_varint(bytes, start)?;
    let wire_type = u8::try_from(key & 0x07).unwrap_or(u8::MAX);
    let raw_field_number = key >> 3;
    if raw_field_number == 0 || raw_field_number > MAX_PROTOBUF_FIELD_NUMBER {
        return Err(WireScanError::InvalidFieldNumber(raw_field_number));
    }
    let field_number = u32::try_from(raw_field_number)
        .map_err(|_| WireScanError::InvalidFieldNumber(raw_field_number))?;

    match wire_type {
        WIRE_TYPE_VARINT => {
            let (_, after_val) = decode_varint(bytes, after_key)?;
            Ok(ParsedField {
                field_number,
                wire_type,
                value_slice: &bytes[after_key..after_val],
                next_offset: after_val,
            })
        }
        WIRE_TYPE_64BIT => {
            let end = after_key
                .checked_add(8)
                .filter(|&end| end <= bytes.len())
                .ok_or(WireScanError::TruncatedField)?;
            Ok(ParsedField {
                field_number,
                wire_type,
                value_slice: &bytes[after_key..end],
                next_offset: end,
            })
        }
        WIRE_TYPE_LENGTH_DELIMITED => {
            let (len_u64, after_len) = decode_varint(bytes, after_key)?;
            let len = usize::try_from(len_u64).map_err(|_| WireScanError::TruncatedField)?;
            let end = after_len
                .checked_add(len)
                .filter(|&end| end <= bytes.len())
                .ok_or(WireScanError::TruncatedField)?;
            Ok(ParsedField {
                field_number,
                wire_type,
                value_slice: &bytes[after_len..end],
                next_offset: end,
            })
        }
        WIRE_TYPE_32BIT => {
            let end = after_key
                .checked_add(4)
                .filter(|&end| end <= bytes.len())
                .ok_or(WireScanError::TruncatedField)?;
            Ok(ParsedField {
                field_number,
                wire_type,
                value_slice: &bytes[after_key..end],
                next_offset: end,
            })
        }
        other => Err(WireScanError::UnsupportedWireType(other)),
    }
}

fn decode_varint(bytes: &[u8], start: usize) -> Result<(u64, usize), WireScanError> {
    let mut value = 0_u64;
    let mut shift = 0_u32;
    let mut index = start;
    for step in 0..10_u8 {
        let byte = *bytes.get(index).ok_or(WireScanError::TruncatedField)?;
        index += 1;
        if step == 9 && byte > 1 {
            return Err(WireScanError::VarintOverflow);
        }
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, index));
        }
        shift += 7;
    }
    Err(WireScanError::VarintOverflow)
}

fn encode_varint(mut value: u64, sink: &mut Vec<u8>) {
    while value >= 0x80 {
        let low_seven = u8::try_from(value & 0x7F).unwrap_or(0);
        sink.push(low_seven | 0x80);
        value >>= 7;
    }
    sink.push(u8::try_from(value).unwrap_or(0));
}

fn encode_length_delimited_field(field_number: u32, payload: &[u8], sink: &mut Vec<u8>) {
    let key = (u64::from(field_number) << 3) | u64::from(WIRE_TYPE_LENGTH_DELIMITED);
    encode_varint(key, sink);
    encode_varint(u64::try_from(payload.len()).unwrap_or(u64::MAX), sink);
    sink.extend_from_slice(payload);
}

fn append_raw_fields(sink: &mut Vec<u8>, fields: &[RawUnknownField]) {
    for field in fields {
        sink.extend_from_slice(&field.raw_bytes);
    }
}
