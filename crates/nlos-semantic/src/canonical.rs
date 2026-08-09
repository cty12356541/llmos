use minicbor::data::Type;
use minicbor::{Decoder, Encoder};
use nlos_capability::CapabilityTarget;
use nlos_types::{
    ControlDomainId, Generation, KeyId, NamespaceId, PrincipalId, ProcessId, ReceiptId,
    SemanticEventId, TaskId,
};
use sha2::{Digest, Sha256};

use crate::{
    AssertionMode, LocalProcessRef, MAX_CANONICAL_EVENT_BYTES, MAX_LINEAGE_ITEMS, MAX_NONCE_BYTES,
    MIN_NONCE_BYTES, SemanticAuthorityError, UnsignedAssertionEvent,
};

const FIELD_COUNT: u64 = 17;
const SCHEMA_NAME: &str = "llmos.semantic-event";
const SCHEMA_MAJOR: u32 = 1;
const SCHEMA_MINOR: u32 = 0;
const EVENT_TYPE_ASSERTION: u8 = 1;

/// Encodes the v1 Assertion unsigned envelope as deterministic CBOR.
///
/// # Errors
///
/// Rejects invalid bounds, ordering, payload semantics, or encoding failure.
pub fn encode_unsigned_assertion_event(
    event: &UnsignedAssertionEvent,
) -> Result<Vec<u8>, SemanticAuthorityError> {
    validate_event(event)?;
    let mut encoder = Encoder::new(Vec::new());
    encoder
        .map(FIELD_COUNT)
        .and_then(|e| e.u8(0))
        .and_then(|e| e.str(SCHEMA_NAME))
        .and_then(|e| e.u8(1))
        .and_then(|e| e.u32(SCHEMA_MAJOR))
        .and_then(|e| e.u8(2))
        .and_then(|e| e.u32(SCHEMA_MINOR))
        .and_then(|e| e.u8(3))
        .and_then(|e| e.u8(EVENT_TYPE_ASSERTION))
        .and_then(|e| e.u8(4))
        .and_then(|e| e.u8(scope_kind(event.scope)))
        .and_then(|e| e.u8(5))
        .and_then(|e| e.bytes(&scope_bytes(event.scope)))
        .and_then(|e| e.u8(6))
        .and_then(|e| e.bytes(event.issuer.as_bytes()))
        .and_then(|e| e.u8(7))
        .and_then(|e| e.array(3))
        .and_then(|e| e.u8(1))
        .and_then(|e| e.bytes(event.issuer_execution.process_id.as_bytes()))
        .and_then(|e| e.u64(event.issuer_execution.generation.get()))
        .and_then(|e| e.u8(8))
        .and_then(|e| e.bytes(event.control_domain.as_bytes()))
        .and_then(|e| e.u8(9))
        .and_then(|e| e.u64(event.issued_at_unix_ns))
        .and_then(|e| e.u8(10))
        .and_then(|e| e.bytes(&event.nonce))
        .and_then(|e| e.u8(11))
        .and_then(|e| e.array(event.declared_parents.len() as u64))
        .map_err(encoding_error)?;
    for parent in &event.declared_parents {
        encoder.bytes(parent.as_bytes()).map_err(encoding_error)?;
    }
    encoder
        .u8(12)
        .and_then(|e| e.null())
        .map_err(encoding_error)?;
    encoder.u8(13).map_err(encoding_error)?;
    encode_optional_u64(&mut encoder, event.valid_until_ms)?;
    encoder.u8(14).map_err(encoding_error)?;
    encode_optional_digest(&mut encoder, event.purpose_digest)?;
    encoder
        .u8(15)
        .and_then(|e| e.array(4))
        .and_then(|e| e.bytes(&event.content_digest))
        .and_then(|e| e.u8(event.assertion_mode.encode()))
        .map_err(encoding_error)?;
    encode_optional_receipt(&mut encoder, event.execution_evidence_receipt_id)?;
    encode_optional_u16(&mut encoder, event.confidence_bp)?;
    encoder
        .u8(16)
        .and_then(|e| e.bytes(event.key_id.as_bytes()))
        .map_err(encoding_error)?;
    let bytes = encoder.into_writer();
    if bytes.len() > MAX_CANONICAL_EVENT_BYTES {
        return Err(SemanticAuthorityError::CanonicalTooLarge);
    }
    Ok(bytes)
}

/// Strictly decodes and re-encodes the v1 Assertion unsigned envelope.
///
/// # Errors
///
/// Rejects malformed, indefinite, trailing, noncanonical, unsupported, or
/// semantically invalid event bytes.
pub fn decode_unsigned_assertion_event(
    bytes: &[u8],
) -> Result<UnsignedAssertionEvent, SemanticAuthorityError> {
    if bytes.len() > MAX_CANONICAL_EVENT_BYTES {
        return Err(SemanticAuthorityError::CanonicalTooLarge);
    }
    let event = decode_structural(bytes)?;
    if encode_unsigned_assertion_event(&event)? != bytes {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    Ok(event)
}

#[must_use]
pub fn semantic_event_id(canonical_unsigned_event: &[u8]) -> SemanticEventId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/semantic-event/v1");
    hasher.update(canonical_unsigned_event);
    SemanticEventId::from_bytes(hasher.finalize().into())
}

fn decode_structural(bytes: &[u8]) -> Result<UnsignedAssertionEvent, SemanticAuthorityError> {
    let mut decoder = Decoder::new(bytes);
    let count = decoder
        .map()
        .map_err(decode_error)?
        .ok_or(SemanticAuthorityError::CanonicalMismatch)?;
    if count != FIELD_COUNT {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    expect_key(&mut decoder, 0)?;
    if decoder.str().map_err(decode_error)? != SCHEMA_NAME {
        return Err(SemanticAuthorityError::UnsupportedSchema);
    }
    expect_key(&mut decoder, 1)?;
    if decoder.u32().map_err(decode_error)? != SCHEMA_MAJOR {
        return Err(SemanticAuthorityError::UnsupportedSchema);
    }
    expect_key(&mut decoder, 2)?;
    if decoder.u32().map_err(decode_error)? != SCHEMA_MINOR {
        return Err(SemanticAuthorityError::UnsupportedSchema);
    }
    expect_key(&mut decoder, 3)?;
    if decoder.u8().map_err(decode_error)? != EVENT_TYPE_ASSERTION {
        return Err(SemanticAuthorityError::UnsupportedEventType);
    }
    expect_key(&mut decoder, 4)?;
    let scope_kind = decoder.u8().map_err(decode_error)?;
    expect_key(&mut decoder, 5)?;
    let scope_id = fixed_bytes::<16>(&mut decoder, "scope id")?;
    let scope = decode_scope(scope_kind, scope_id)?;
    expect_key(&mut decoder, 6)?;
    let issuer = PrincipalId::from_bytes(fixed_bytes(&mut decoder, "issuer")?);
    expect_key(&mut decoder, 7)?;
    let execution_count = decoder
        .array()
        .map_err(decode_error)?
        .ok_or(SemanticAuthorityError::CanonicalMismatch)?;
    if execution_count != 3 || decoder.u8().map_err(decode_error)? != 1 {
        return Err(SemanticAuthorityError::InvalidIssuerExecution);
    }
    let process_id = ProcessId::from_bytes(fixed_bytes(&mut decoder, "process id")?);
    let generation = decode_generation(decoder.u64().map_err(decode_error)?)?;
    expect_key(&mut decoder, 8)?;
    let control_domain = ControlDomainId::from_bytes(fixed_bytes(&mut decoder, "domain")?);
    expect_key(&mut decoder, 9)?;
    let issued_at_unix_ns = decoder.u64().map_err(decode_error)?;
    expect_key(&mut decoder, 10)?;
    let nonce = decoder.bytes().map_err(decode_error)?.to_vec();
    expect_key(&mut decoder, 11)?;
    let declared_parents = decode_event_ids(&mut decoder)?;
    expect_key(&mut decoder, 12)?;
    decoder.null().map_err(decode_error)?;
    expect_key(&mut decoder, 13)?;
    let valid_until_ms = decode_optional_u64(&mut decoder)?;
    expect_key(&mut decoder, 14)?;
    let purpose_digest = decode_optional_digest(&mut decoder)?;
    expect_key(&mut decoder, 15)?;
    let payload_count = decoder
        .array()
        .map_err(decode_error)?
        .ok_or(SemanticAuthorityError::CanonicalMismatch)?;
    if payload_count != 4 {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    let content_digest = fixed_bytes(&mut decoder, "content digest")?;
    let assertion_mode = AssertionMode::decode(decoder.u8().map_err(decode_error)?)
        .ok_or(SemanticAuthorityError::InvalidAssertionPayload)?;
    let execution_evidence_receipt_id = decode_optional_receipt(&mut decoder)?;
    let confidence_bp = decode_optional_u16(&mut decoder)?;
    expect_key(&mut decoder, 16)?;
    let key_id = KeyId::from_bytes(fixed_bytes(&mut decoder, "key id")?);
    if decoder.position() != bytes.len() {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    let event = UnsignedAssertionEvent {
        scope,
        issuer,
        issuer_execution: LocalProcessRef {
            process_id,
            generation,
        },
        control_domain,
        issued_at_unix_ns,
        nonce,
        declared_parents,
        valid_until_ms,
        purpose_digest,
        content_digest,
        assertion_mode,
        execution_evidence_receipt_id,
        confidence_bp,
        key_id,
    };
    validate_event(&event)?;
    Ok(event)
}

fn validate_event(event: &UnsignedAssertionEvent) -> Result<(), SemanticAuthorityError> {
    if !(MIN_NONCE_BYTES..=MAX_NONCE_BYTES).contains(&event.nonce.len()) {
        return Err(SemanticAuthorityError::InvalidNonce);
    }
    validate_sorted_unique(&event.declared_parents)?;
    if event.confidence_bp.is_some_and(|value| value > 10_000) {
        return Err(SemanticAuthorityError::InvalidAssertionPayload);
    }
    if event.assertion_mode == AssertionMode::FactFromTool
        && event.execution_evidence_receipt_id.is_none()
    {
        return Err(SemanticAuthorityError::MissingExecutionEvidence);
    }
    Ok(())
}

pub(crate) fn validate_sorted_unique(
    values: &[SemanticEventId],
) -> Result<(), SemanticAuthorityError> {
    if values.len() > MAX_LINEAGE_ITEMS {
        return Err(SemanticAuthorityError::InvalidLineage);
    }
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(SemanticAuthorityError::InvalidLineage);
    }
    Ok(())
}

fn scope_kind(scope: CapabilityTarget) -> u8 {
    match scope {
        CapabilityTarget::Namespace(_) => 1,
        CapabilityTarget::Task(_) => 2,
    }
}

fn scope_bytes(scope: CapabilityTarget) -> [u8; 16] {
    match scope {
        CapabilityTarget::Namespace(id) => id.into_bytes(),
        CapabilityTarget::Task(id) => id.into_bytes(),
    }
}

fn decode_scope(kind: u8, bytes: [u8; 16]) -> Result<CapabilityTarget, SemanticAuthorityError> {
    match kind {
        1 => Ok(CapabilityTarget::Namespace(NamespaceId::from_bytes(bytes))),
        2 => Ok(CapabilityTarget::Task(TaskId::from_bytes(bytes))),
        _ => Err(SemanticAuthorityError::InvalidTarget),
    }
}

fn expect_key(decoder: &mut Decoder<'_>, expected: u8) -> Result<(), SemanticAuthorityError> {
    if decoder.u8().map_err(decode_error)? != expected {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    Ok(())
}

fn decode_event_ids(
    decoder: &mut Decoder<'_>,
) -> Result<Vec<SemanticEventId>, SemanticAuthorityError> {
    let count = decoder
        .array()
        .map_err(decode_error)?
        .ok_or(SemanticAuthorityError::CanonicalMismatch)?;
    let count = usize::try_from(count).map_err(|_| SemanticAuthorityError::InvalidLineage)?;
    if count > MAX_LINEAGE_ITEMS {
        return Err(SemanticAuthorityError::InvalidLineage);
    }
    let mut result = Vec::with_capacity(count);
    for _ in 0..count {
        result.push(SemanticEventId::from_bytes(fixed_bytes(
            decoder, "event id",
        )?));
    }
    validate_sorted_unique(&result)?;
    Ok(result)
}

fn encode_optional_u64(
    encoder: &mut Encoder<Vec<u8>>,
    value: Option<u64>,
) -> Result<(), SemanticAuthorityError> {
    match value {
        Some(value) => encoder.u64(value),
        None => encoder.null(),
    }
    .map_err(encoding_error)?;
    Ok(())
}

fn decode_optional_u64(decoder: &mut Decoder<'_>) -> Result<Option<u64>, SemanticAuthorityError> {
    if decoder.datatype().map_err(decode_error)? == Type::Null {
        decoder.null().map_err(decode_error)?;
        Ok(None)
    } else {
        Ok(Some(decoder.u64().map_err(decode_error)?))
    }
}

fn encode_optional_u16(
    encoder: &mut Encoder<Vec<u8>>,
    value: Option<u16>,
) -> Result<(), SemanticAuthorityError> {
    match value {
        Some(value) => encoder.u16(value),
        None => encoder.null(),
    }
    .map_err(encoding_error)?;
    Ok(())
}

fn decode_optional_u16(decoder: &mut Decoder<'_>) -> Result<Option<u16>, SemanticAuthorityError> {
    if decoder.datatype().map_err(decode_error)? == Type::Null {
        decoder.null().map_err(decode_error)?;
        Ok(None)
    } else {
        Ok(Some(decoder.u16().map_err(decode_error)?))
    }
}

fn encode_optional_digest(
    encoder: &mut Encoder<Vec<u8>>,
    value: Option<[u8; 32]>,
) -> Result<(), SemanticAuthorityError> {
    match value {
        Some(value) => encoder.bytes(&value),
        None => encoder.null(),
    }
    .map_err(encoding_error)?;
    Ok(())
}

fn decode_optional_digest(
    decoder: &mut Decoder<'_>,
) -> Result<Option<[u8; 32]>, SemanticAuthorityError> {
    if decoder.datatype().map_err(decode_error)? == Type::Null {
        decoder.null().map_err(decode_error)?;
        Ok(None)
    } else {
        Ok(Some(fixed_bytes(decoder, "optional digest")?))
    }
}

fn encode_optional_receipt(
    encoder: &mut Encoder<Vec<u8>>,
    value: Option<ReceiptId>,
) -> Result<(), SemanticAuthorityError> {
    match value {
        Some(value) => encoder.bytes(value.as_bytes()),
        None => encoder.null(),
    }
    .map_err(encoding_error)?;
    Ok(())
}

fn decode_optional_receipt(
    decoder: &mut Decoder<'_>,
) -> Result<Option<ReceiptId>, SemanticAuthorityError> {
    if decoder.datatype().map_err(decode_error)? == Type::Null {
        decoder.null().map_err(decode_error)?;
        Ok(None)
    } else {
        Ok(Some(ReceiptId::from_bytes(fixed_bytes(
            decoder,
            "receipt id",
        )?)))
    }
}

fn fixed_bytes<const N: usize>(
    decoder: &mut Decoder<'_>,
    field: &'static str,
) -> Result<[u8; N], SemanticAuthorityError> {
    decoder
        .bytes()
        .map_err(decode_error)?
        .try_into()
        .map_err(|_| SemanticAuthorityError::MalformedCanonical(field))
}

fn decode_generation(value: u64) -> Result<Generation, SemanticAuthorityError> {
    std::num::NonZeroU64::new(value)
        .map(Generation::new)
        .ok_or(SemanticAuthorityError::InvalidIssuerExecution)
}

#[allow(clippy::needless_pass_by_value)] // `map_err` supplies the owned encoder error.
fn encoding_error(
    error: minicbor::encode::Error<std::convert::Infallible>,
) -> SemanticAuthorityError {
    SemanticAuthorityError::CanonicalEncoding(error.to_string())
}

#[allow(clippy::needless_pass_by_value)] // `map_err` supplies the owned decoder error.
fn decode_error(error: minicbor::decode::Error) -> SemanticAuthorityError {
    SemanticAuthorityError::CanonicalDecoding(error.to_string())
}
