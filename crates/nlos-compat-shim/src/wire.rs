//! Bounded safe-Rust Protobuf TLV scanner and surgical `SchemaIdentity.major`
//! wire rewriter.
//!
//! Standard `prost::Message::decode` followed by `encode_to_vec` drops unknown
//! Protobuf tags (such as future additive fields in `N` or client probe tags like
//! `a00607` = field `100`, varint `7`). This module scans top-level and nested
//! `SchemaIdentity` TLV records directly on `&[u8]` so `SchemaIdentity.major`
//! can be rewritten across epochs while preserving every other byte—including
//! unknown fields and `non_critical_extension_ids`—100% verbatim.

use std::ops::Range;

use nlos_schema::{CompatibilityError, sabi};
use prost::Message;

const WIRE_TYPE_VARINT: u8 = 0;
const WIRE_TYPE_FIXED64: u8 = 1;
const WIRE_TYPE_LEN: u8 = 2;
const WIRE_TYPE_FIXED32: u8 = 5;
const MAX_FIELD_NUMBER: u64 = (1_u64 << 29) - 1;

/// One parsed Protobuf TLV record referencing byte ranges in the input slice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScannedTlv {
    pub field_number: u32,
    pub wire_type: u8,
    pub full_range: Range<usize>,
    pub value_range: Range<usize>,
}

/// Summary produced when rewriting `SchemaIdentity.major` inside a SABI wire frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RewrittenFrame {
    pub original_identity: sabi::v1::SchemaIdentity,
    pub rewritten_wire: Vec<u8>,
    pub top_level_field_numbers: Vec<u32>,
    pub unknown_field_numbers: Vec<u32>,
    pub unknown_field_bytes: usize,
}

/// Scans all top-level Protobuf TLV records in `wire`, failing closed on
/// truncated varints, deprecated start/end group wire types, invalid field `0`,
/// or length-delimited slices exceeding `wire.len()`.
pub(crate) fn scan_tlvs(wire: &[u8]) -> Result<Vec<ScannedTlv>, CompatibilityError> {
    let mut cursor = 0_usize;
    let mut records = Vec::new();
    while cursor < wire.len() {
        let record_start = cursor;
        let (tag, after_tag) = read_varint(wire, cursor)?;
        let wire_type = tag.to_le_bytes()[0] & 0x07;
        let raw_field = tag >> 3;
        if raw_field == 0 || raw_field > MAX_FIELD_NUMBER {
            return Err(CompatibilityError::MalformedProtobuf(format!(
                "invalid protobuf field number {raw_field}"
            )));
        }
        let field_number = u32::try_from(raw_field).map_err(|_| {
            CompatibilityError::MalformedProtobuf("protobuf field number overflow".to_owned())
        })?;
        let value_range = match wire_type {
            WIRE_TYPE_VARINT => {
                let (_, end) = read_varint(wire, after_tag)?;
                after_tag..end
            }
            WIRE_TYPE_FIXED64 => {
                let end = after_tag.checked_add(8).ok_or_else(|| {
                    CompatibilityError::MalformedProtobuf("fixed64 length overflow".to_owned())
                })?;
                if end > wire.len() {
                    return Err(CompatibilityError::MalformedProtobuf(
                        "truncated fixed64 field".to_owned(),
                    ));
                }
                after_tag..end
            }
            WIRE_TYPE_LEN => {
                let (len_u64, val_start) = read_varint(wire, after_tag)?;
                let len = usize::try_from(len_u64).map_err(|_| {
                    CompatibilityError::MalformedProtobuf(
                        "length-delimited size overflow".to_owned(),
                    )
                })?;
                let end = val_start.checked_add(len).ok_or_else(|| {
                    CompatibilityError::MalformedProtobuf(
                        "length-delimited range overflow".to_owned(),
                    )
                })?;
                if end > wire.len() {
                    return Err(CompatibilityError::MalformedProtobuf(
                        "truncated length-delimited field".to_owned(),
                    ));
                }
                val_start..end
            }
            WIRE_TYPE_FIXED32 => {
                let end = after_tag.checked_add(4).ok_or_else(|| {
                    CompatibilityError::MalformedProtobuf("fixed32 length overflow".to_owned())
                })?;
                if end > wire.len() {
                    return Err(CompatibilityError::MalformedProtobuf(
                        "truncated fixed32 field".to_owned(),
                    ));
                }
                after_tag..end
            }
            other => {
                return Err(CompatibilityError::MalformedProtobuf(format!(
                    "unsupported protobuf wire type {other}"
                )));
            }
        };
        cursor = value_range.end;
        records.push(ScannedTlv {
            field_number,
            wire_type,
            full_range: record_start..cursor,
            value_range,
        });
    }
    Ok(records)
}

/// Surgically rewrites top-level field `1` (`SchemaIdentity.major`) to `new_major`
/// while preserving all remaining top-level TLV bytes (including unknown fields)
/// and all other `SchemaIdentity` fields byte-for-byte.
pub(crate) fn rewrite_top_level_schema_major(
    wire: &[u8],
    new_major: u32,
    known_field_numbers: &[u32],
) -> Result<RewrittenFrame, CompatibilityError> {
    let tlvs = scan_tlvs(wire)?;
    let schema_index = tlvs
        .iter()
        .rposition(|tlv| tlv.field_number == 1)
        .ok_or(CompatibilityError::MissingSchemaIdentity)?;
    let schema_tlv = &tlvs[schema_index];
    if schema_tlv.wire_type != WIRE_TYPE_LEN {
        return Err(CompatibilityError::MalformedProtobuf(
            "SchemaIdentity field 1 must be length-delimited".to_owned(),
        ));
    }
    let schema_bytes = &wire[schema_tlv.value_range.clone()];
    let original_identity = sabi::v1::SchemaIdentity::decode(schema_bytes)
        .map_err(|error| CompatibilityError::MalformedProtobuf(error.to_string()))?;
    let rewritten_schema = rewrite_schema_identity_bytes(schema_bytes, new_major)?;

    let mut rewritten_wire = Vec::with_capacity(wire.len() + 8);
    let mut top_level_field_numbers = Vec::with_capacity(tlvs.len());
    let mut unknown_field_numbers = Vec::new();
    let mut unknown_field_bytes = 0_usize;

    for (idx, tlv) in tlvs.iter().enumerate() {
        if !top_level_field_numbers.contains(&tlv.field_number) {
            top_level_field_numbers.push(tlv.field_number);
        }
        if !known_field_numbers.contains(&tlv.field_number) {
            if !unknown_field_numbers.contains(&tlv.field_number) {
                unknown_field_numbers.push(tlv.field_number);
            }
            unknown_field_bytes = unknown_field_bytes.saturating_add(tlv.full_range.len());
        }
        if idx == schema_index {
            write_len_delimited_field(&mut rewritten_wire, 1, &rewritten_schema);
        } else if tlv.field_number == 1 {
            // Skip superseded duplicate SchemaIdentity slot if repeated on wire.
        } else {
            rewritten_wire.extend_from_slice(&wire[tlv.full_range.clone()]);
        }
    }

    Ok(RewrittenFrame {
        original_identity,
        rewritten_wire,
        top_level_field_numbers,
        unknown_field_numbers,
        unknown_field_bytes,
    })
}

/// Surgically rewrites `ExchangeRequest` / `ExchangeResponse` (where top-level
/// field `1` is `Envelope`, whose field `1` is `SchemaIdentity`) to `new_major`,
/// preserving unknown fields on both the outer wrapper and inner `Envelope`.
pub(crate) fn rewrite_exchange_wrapper_schema_major(
    wire: &[u8],
    new_major: u32,
    envelope_known_fields: &[u32],
) -> Result<RewrittenFrame, CompatibilityError> {
    let outer_tlvs = scan_tlvs(wire)?;
    let env_index = outer_tlvs
        .iter()
        .rposition(|tlv| tlv.field_number == 1)
        .ok_or(CompatibilityError::MissingExchangeEnvelope)?;
    let env_tlv = &outer_tlvs[env_index];
    if env_tlv.wire_type != WIRE_TYPE_LEN {
        return Err(CompatibilityError::MalformedProtobuf(
            "Exchange envelope field 1 must be length-delimited".to_owned(),
        ));
    }

    let inner = rewrite_top_level_schema_major(
        &wire[env_tlv.value_range.clone()],
        new_major,
        envelope_known_fields,
    )?;

    let mut rewritten_wire = Vec::with_capacity(wire.len() + 8);
    let mut unknown_field_numbers = inner.unknown_field_numbers;
    let mut unknown_field_bytes = inner.unknown_field_bytes;

    for (idx, tlv) in outer_tlvs.iter().enumerate() {
        if tlv.field_number != 1 {
            if !unknown_field_numbers.contains(&tlv.field_number) {
                unknown_field_numbers.push(tlv.field_number);
            }
            unknown_field_bytes = unknown_field_bytes.saturating_add(tlv.full_range.len());
        }
        if idx == env_index {
            write_len_delimited_field(&mut rewritten_wire, 1, &inner.rewritten_wire);
        } else if tlv.field_number != 1 {
            rewritten_wire.extend_from_slice(&wire[tlv.full_range.clone()]);
        }
    }

    Ok(RewrittenFrame {
        original_identity: inner.original_identity,
        rewritten_wire,
        top_level_field_numbers: inner.top_level_field_numbers,
        unknown_field_numbers,
        unknown_field_bytes,
    })
}

/// Replaces `Envelope.payload` (top-level field `15`) while keeping all other
/// `Envelope` TLV records—including unknown trailing fields—verbatim.
pub(crate) fn replace_envelope_payload_bytes(
    envelope_wire: &[u8],
    new_payload: &[u8],
) -> Result<Vec<u8>, CompatibilityError> {
    let tlvs = scan_tlvs(envelope_wire)?;
    let mut output = Vec::with_capacity(envelope_wire.len() + new_payload.len());
    let mut replaced = false;
    for tlv in &tlvs {
        if tlv.field_number == 15 {
            if !replaced && !new_payload.is_empty() {
                write_len_delimited_field(&mut output, 15, new_payload);
            }
            replaced = true;
        } else {
            output.extend_from_slice(&envelope_wire[tlv.full_range.clone()]);
        }
    }
    if !replaced && !new_payload.is_empty() {
        write_len_delimited_field(&mut output, 15, new_payload);
    }
    Ok(output)
}

fn rewrite_schema_identity_bytes(
    schema_bytes: &[u8],
    new_major: u32,
) -> Result<Vec<u8>, CompatibilityError> {
    let inner_tlvs = scan_tlvs(schema_bytes)?;
    let mut out = Vec::with_capacity(schema_bytes.len() + 6);
    let mut emitted_major = false;

    for tlv in &inner_tlvs {
        if tlv.field_number == 2 {
            if !emitted_major && new_major != 0 {
                write_varint_field(&mut out, 2, u64::from(new_major));
                emitted_major = true;
            }
        } else {
            if !emitted_major && tlv.field_number > 2 && new_major != 0 {
                write_varint_field(&mut out, 2, u64::from(new_major));
                emitted_major = true;
            }
            out.extend_from_slice(&schema_bytes[tlv.full_range.clone()]);
        }
    }
    if !emitted_major && new_major != 0 {
        write_varint_field(&mut out, 2, u64::from(new_major));
    }
    Ok(out)
}

fn read_varint(buf: &[u8], start: usize) -> Result<(u64, usize), CompatibilityError> {
    let mut value = 0_u64;
    let mut shift = 0_u32;
    let mut pos = start;
    loop {
        let Some(&byte) = buf.get(pos) else {
            return Err(CompatibilityError::MalformedProtobuf(
                "unexpected EOF while decoding protobuf varint".to_owned(),
            ));
        };
        pos += 1;
        if shift == 63 && byte > 1 {
            return Err(CompatibilityError::MalformedProtobuf(
                "protobuf varint overflowed 64 bits".to_owned(),
            ));
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, pos));
        }
        shift += 7;
        if shift >= 64 {
            return Err(CompatibilityError::MalformedProtobuf(
                "overlong protobuf varint".to_owned(),
            ));
        }
    }
}

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value.to_le_bytes()[0] & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value.to_le_bytes()[0]);
}

fn write_varint_field(out: &mut Vec<u8>, field_number: u32, value: u64) {
    let tag = (u64::from(field_number) << 3) | u64::from(WIRE_TYPE_VARINT);
    write_varint(out, tag);
    write_varint(out, value);
}

fn write_len_delimited_field(out: &mut Vec<u8>, field_number: u32, payload: &[u8]) {
    let tag = (u64::from(field_number) << 3) | u64::from(WIRE_TYPE_LEN);
    write_varint(out, tag);
    write_varint(out, payload.len() as u64);
    out.extend_from_slice(payload);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tlv_scanner_fails_closed_on_truncated_or_invalid_wire_frames() {
        // Truncated varint (continuation bit set at EOF)
        assert!(matches!(
            scan_tlvs(&[0x08, 0x80]),
            Err(CompatibilityError::MalformedProtobuf(_))
        ));
        // Field number 0 is illegal in Protobuf
        assert!(matches!(
            scan_tlvs(&[0x00, 0x01]),
            Err(CompatibilityError::MalformedProtobuf(_))
        ));
        // Deprecated start-group wire type 3 is rejected
        assert!(matches!(
            scan_tlvs(&[0x0b]),
            Err(CompatibilityError::MalformedProtobuf(_))
        ));
        // Truncated length-delimited slice (claims 10 bytes, only 2 present)
        assert!(matches!(
            scan_tlvs(&[0x0a, 0x0a, 0x01, 0x02]),
            Err(CompatibilityError::MalformedProtobuf(_))
        ));
    }
}
