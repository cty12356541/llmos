//! Deterministic `minicbor` codecs for legacy Stage B `cancel` and
//! `query_isolation` driver wire payloads, complementing
//! `nlos_driver_mock::codec::{RegisterOperationWire, DispatchOperationWire, CompleteOperationWire}`.

use minicbor::{Decoder, Encoder};
use nlos_driver_mock::codec::{
    CodecError, MAX_DRIVER_MOCK_PAYLOAD_BYTES, PAYLOAD_SCHEMA_MAJOR, PAYLOAD_SCHEMA_NAME,
    state_from_wire_code,
};
use nlos_schema::MAX_PROVIDER_PROFILE_BYTES;

const ID_BYTES: usize = 16;
const DIGEST_BYTES: usize = 32;
const QUERY_ISOLATION_TAG: &str = "query_isolation";

/// Legacy Stage B `minicbor` cancellation request payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CancelOperationWire {
    /// Target 128-bit `OperationId`.
    pub operation_id: [u8; ID_BYTES],
    /// Monotonic non-zero `Operation` generation.
    pub operation_generation: u64,
    /// Compare-and-set `cancel_epoch` fence.
    pub expected_cancel_epoch: u64,
    /// Pre-derived 128-bit receipt ID if cancelled before external effect.
    pub no_effect_receipt_id: [u8; ID_BYTES],
}

/// Legacy Stage B `minicbor` cancellation acknowledgment payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CancelOperationResultWire {
    /// Whether this acknowledgment replayed an earlier durable decision.
    pub replayed: bool,
    /// Whether the operation had already reached a terminal state.
    pub already_terminal: bool,
    /// Target 128-bit `OperationId`.
    pub operation_id: [u8; ID_BYTES],
    /// Monotonic non-zero `Operation` generation.
    pub operation_generation: u64,
    /// `nlos_driver_mock::codec` wire state code (`1..=8`).
    pub state_code: u8,
    /// Resulting durable `cancel_epoch`.
    pub cancel_epoch: u64,
    /// Optional 128-bit terminal receipt ID (`[0u8; 16]` when non-terminal).
    pub terminal_receipt_id: [u8; ID_BYTES],
}

/// Legacy Stage B `minicbor` isolation query marker payload.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct QueryIsolationWire;

/// Legacy Stage B `minicbor` isolation report payload.
// Mirrors the four orthogonal [TASK-PROVIDER-001] isolation axes.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IsolationReportWire {
    /// Per-tenant isolation guarantee.
    pub tenant_isolated: bool,
    /// Ephemeral per-session state guarantee.
    pub session_ephemeral: bool,
    /// Partitioned KV/prompt cache guarantee.
    pub cache_partitioned: bool,
    /// Zero post-call data retention guarantee.
    pub zero_data_retention: bool,
    /// 32-byte provider attestation digest (`[0u8; 32]` when unverified).
    pub attestation_digest: [u8; DIGEST_BYTES],
    /// Bounded provider profile identifier.
    pub provider_profile: String,
}

/// Encodes a [`CancelOperationWire`] into deterministic Stage B `minicbor`.
///
/// # Errors
///
/// Fails closed if `operation_generation == 0` or encoding fails.
pub fn encode_cancel_request(value: &CancelOperationWire) -> Result<Vec<u8>, CodecError> {
    if value.operation_generation == 0 {
        return Err(CodecError::ZeroGeneration("operation"));
    }
    let mut encoder = Encoder::new(Vec::new());
    encoder
        .map(6)
        .and_then(|e| e.u8(0))
        .and_then(|e| e.str(PAYLOAD_SCHEMA_NAME))
        .and_then(|e| e.u8(1))
        .and_then(|e| e.u32(PAYLOAD_SCHEMA_MAJOR))
        .and_then(|e| e.u8(2))
        .and_then(|e| e.bytes(&value.operation_id))
        .and_then(|e| e.u8(3))
        .and_then(|e| e.u64(value.operation_generation))
        .and_then(|e| e.u8(4))
        .and_then(|e| e.u64(value.expected_cancel_epoch))
        .and_then(|e| e.u8(5))
        .and_then(|e| e.bytes(&value.no_effect_receipt_id))
        .map_err(|err| CodecError::Encoding(err.to_string()))?;
    Ok(encoder.into_writer())
}

/// Decodes a [`CancelOperationWire`] from deterministic Stage B `minicbor`.
///
/// # Errors
///
/// Fails closed on oversized payloads, wrong schema/major, out-of-order keys,
/// invalid identifier widths, zero generation, or trailing bytes.
pub fn decode_cancel_request(wire: &[u8]) -> Result<CancelOperationWire, CodecError> {
    let mut decoder = open_map(wire, 6, 5)?;
    expect_key(&mut decoder, 2)?;
    let operation_id = read_fixed::<ID_BYTES>(&mut decoder, "operation_id")?;
    expect_key(&mut decoder, 3)?;
    let operation_generation = read_nonzero_u64(&mut decoder, "operation")?;
    expect_key(&mut decoder, 4)?;
    let expected_cancel_epoch = decoder
        .u64()
        .map_err(|err| CodecError::Malformed(err.to_string()))?;
    expect_key(&mut decoder, 5)?;
    let no_effect_receipt_id = read_fixed::<ID_BYTES>(&mut decoder, "no_effect_receipt_id")?;
    finish_decoder(&decoder, wire.len())?;
    Ok(CancelOperationWire {
        operation_id,
        operation_generation,
        expected_cancel_epoch,
        no_effect_receipt_id,
    })
}

/// Encodes a [`CancelOperationResultWire`] into deterministic Stage B `minicbor`.
///
/// # Errors
///
/// Fails closed on zero `operation_generation`, invalid `state_code`, or encoding error.
pub fn encode_cancel_result(value: &CancelOperationResultWire) -> Result<Vec<u8>, CodecError> {
    if value.operation_generation == 0 {
        return Err(CodecError::ZeroGeneration("operation"));
    }
    let _ = state_from_wire_code(value.state_code)?;
    let mut encoder = Encoder::new(Vec::new());
    encoder
        .map(9)
        .and_then(|e| e.u8(0))
        .and_then(|e| e.str(PAYLOAD_SCHEMA_NAME))
        .and_then(|e| e.u8(1))
        .and_then(|e| e.u32(PAYLOAD_SCHEMA_MAJOR))
        .and_then(|e| e.u8(2))
        .and_then(|e| e.bool(value.replayed))
        .and_then(|e| e.u8(3))
        .and_then(|e| e.bool(value.already_terminal))
        .and_then(|e| e.u8(4))
        .and_then(|e| e.bytes(&value.operation_id))
        .and_then(|e| e.u8(5))
        .and_then(|e| e.u64(value.operation_generation))
        .and_then(|e| e.u8(6))
        .and_then(|e| e.u8(value.state_code))
        .and_then(|e| e.u8(7))
        .and_then(|e| e.u64(value.cancel_epoch))
        .and_then(|e| e.u8(8))
        .and_then(|e| e.bytes(&value.terminal_receipt_id))
        .map_err(|err| CodecError::Encoding(err.to_string()))?;
    Ok(encoder.into_writer())
}

/// Decodes a [`CancelOperationResultWire`] from deterministic Stage B `minicbor`.
///
/// # Errors
///
/// Fails closed on malformed map structure, invalid state code, zero generation, or trailing bytes.
pub fn decode_cancel_result(wire: &[u8]) -> Result<CancelOperationResultWire, CodecError> {
    let mut decoder = open_map(wire, 9, 8)?;
    expect_key(&mut decoder, 2)?;
    let replayed = decoder
        .bool()
        .map_err(|err| CodecError::Malformed(err.to_string()))?;
    expect_key(&mut decoder, 3)?;
    let already_terminal = decoder
        .bool()
        .map_err(|err| CodecError::Malformed(err.to_string()))?;
    expect_key(&mut decoder, 4)?;
    let operation_id = read_fixed::<ID_BYTES>(&mut decoder, "operation_id")?;
    expect_key(&mut decoder, 5)?;
    let operation_generation = read_nonzero_u64(&mut decoder, "operation")?;
    expect_key(&mut decoder, 6)?;
    let state_code = decoder
        .u8()
        .map_err(|err| CodecError::Malformed(err.to_string()))?;
    let _ = state_from_wire_code(state_code)?;
    expect_key(&mut decoder, 7)?;
    let cancel_epoch = decoder
        .u64()
        .map_err(|err| CodecError::Malformed(err.to_string()))?;
    expect_key(&mut decoder, 8)?;
    let terminal_receipt_id = read_fixed::<ID_BYTES>(&mut decoder, "terminal_receipt_id")?;
    finish_decoder(&decoder, wire.len())?;
    Ok(CancelOperationResultWire {
        replayed,
        already_terminal,
        operation_id,
        operation_generation,
        state_code,
        cancel_epoch,
        terminal_receipt_id,
    })
}

/// Encodes a [`QueryIsolationWire`] into deterministic Stage B `minicbor`.
///
/// # Errors
///
/// Returns [`CodecError`] only if the vector encoder fails.
pub fn encode_query_isolation_request(_value: &QueryIsolationWire) -> Result<Vec<u8>, CodecError> {
    let mut encoder = Encoder::new(Vec::new());
    encoder
        .map(3)
        .and_then(|e| e.u8(0))
        .and_then(|e| e.str(PAYLOAD_SCHEMA_NAME))
        .and_then(|e| e.u8(1))
        .and_then(|e| e.u32(PAYLOAD_SCHEMA_MAJOR))
        .and_then(|e| e.u8(2))
        .and_then(|e| e.str(QUERY_ISOLATION_TAG))
        .map_err(|err| CodecError::Encoding(err.to_string()))?;
    Ok(encoder.into_writer())
}

/// Decodes a [`QueryIsolationWire`] from deterministic Stage B `minicbor`.
///
/// # Errors
///
/// Fails closed if the marker string or map header does not match `query_isolation`.
pub fn decode_query_isolation_request(wire: &[u8]) -> Result<QueryIsolationWire, CodecError> {
    let mut decoder = open_map(wire, 3, 2)?;
    expect_key(&mut decoder, 2)?;
    let tag = decoder
        .str()
        .map_err(|err| CodecError::Malformed(err.to_string()))?;
    if tag != QUERY_ISOLATION_TAG {
        return Err(CodecError::Malformed(
            "unexpected query_isolation marker".to_owned(),
        ));
    }
    finish_decoder(&decoder, wire.len())?;
    Ok(QueryIsolationWire)
}

/// Encodes an [`IsolationReportWire`] into deterministic Stage B `minicbor`.
///
/// # Errors
///
/// Fails closed if `provider_profile` is empty, exceeds [`MAX_PROVIDER_PROFILE_BYTES`],
/// or contains `'\0'`.
pub fn encode_isolation_report(value: &IsolationReportWire) -> Result<Vec<u8>, CodecError> {
    validate_profile(&value.provider_profile)?;
    let mut encoder = Encoder::new(Vec::new());
    encoder
        .map(8)
        .and_then(|e| e.u8(0))
        .and_then(|e| e.str(PAYLOAD_SCHEMA_NAME))
        .and_then(|e| e.u8(1))
        .and_then(|e| e.u32(PAYLOAD_SCHEMA_MAJOR))
        .and_then(|e| e.u8(2))
        .and_then(|e| e.bool(value.tenant_isolated))
        .and_then(|e| e.u8(3))
        .and_then(|e| e.bool(value.session_ephemeral))
        .and_then(|e| e.u8(4))
        .and_then(|e| e.bool(value.cache_partitioned))
        .and_then(|e| e.u8(5))
        .and_then(|e| e.bool(value.zero_data_retention))
        .and_then(|e| e.u8(6))
        .and_then(|e| e.bytes(&value.attestation_digest))
        .and_then(|e| e.u8(7))
        .and_then(|e| e.str(&value.provider_profile))
        .map_err(|err| CodecError::Encoding(err.to_string()))?;
    Ok(encoder.into_writer())
}

/// Decodes an [`IsolationReportWire`] from deterministic Stage B `minicbor`.
///
/// # Errors
///
/// Fails closed on malformed map headers, invalid 32-byte digest width, invalid
/// `provider_profile`, or trailing bytes.
pub fn decode_isolation_report(wire: &[u8]) -> Result<IsolationReportWire, CodecError> {
    let mut decoder = open_map(wire, 8, 7)?;
    expect_key(&mut decoder, 2)?;
    let tenant_isolated = decoder
        .bool()
        .map_err(|err| CodecError::Malformed(err.to_string()))?;
    expect_key(&mut decoder, 3)?;
    let session_ephemeral = decoder
        .bool()
        .map_err(|err| CodecError::Malformed(err.to_string()))?;
    expect_key(&mut decoder, 4)?;
    let cache_partitioned = decoder
        .bool()
        .map_err(|err| CodecError::Malformed(err.to_string()))?;
    expect_key(&mut decoder, 5)?;
    let zero_data_retention = decoder
        .bool()
        .map_err(|err| CodecError::Malformed(err.to_string()))?;
    expect_key(&mut decoder, 6)?;
    let attestation_digest = read_fixed::<DIGEST_BYTES>(&mut decoder, "attestation_digest")?;
    expect_key(&mut decoder, 7)?;
    let provider_profile = decoder
        .str()
        .map_err(|err| CodecError::Malformed(err.to_string()))?
        .to_owned();
    validate_profile(&provider_profile)?;
    finish_decoder(&decoder, wire.len())?;
    Ok(IsolationReportWire {
        tenant_isolated,
        session_ephemeral,
        cache_partitioned,
        zero_data_retention,
        attestation_digest,
        provider_profile,
    })
}

fn validate_profile(profile: &str) -> Result<(), CodecError> {
    if profile.is_empty() || profile.len() > MAX_PROVIDER_PROFILE_BYTES || profile.contains('\0') {
        return Err(CodecError::Malformed(
            "invalid provider_profile in IsolationReportWire".to_owned(),
        ));
    }
    Ok(())
}

fn open_map(wire: &[u8], expected_len: u64, _max_key: u64) -> Result<Decoder<'_>, CodecError> {
    if wire.len() > MAX_DRIVER_MOCK_PAYLOAD_BYTES {
        return Err(CodecError::Malformed(
            "payload exceeds 4096-byte ceiling".to_owned(),
        ));
    }
    let mut decoder = Decoder::new(wire);
    let Some(actual) = decoder
        .map()
        .map_err(|err| CodecError::Malformed(err.to_string()))?
    else {
        return Err(CodecError::Malformed(
            "indefinite-length CBOR maps are forbidden".to_owned(),
        ));
    };
    if actual != expected_len {
        return Err(CodecError::WrongFieldCount {
            actual,
            expected: expected_len,
        });
    }
    expect_key(&mut decoder, 0)?;
    let schema = decoder
        .str()
        .map_err(|err| CodecError::Malformed(err.to_string()))?;
    if schema != PAYLOAD_SCHEMA_NAME {
        return Err(CodecError::WrongSchemaName);
    }
    expect_key(&mut decoder, 1)?;
    let major = decoder
        .u32()
        .map_err(|err| CodecError::Malformed(err.to_string()))?;
    if major != PAYLOAD_SCHEMA_MAJOR {
        return Err(CodecError::UnsupportedMajor {
            actual: major,
            supported: PAYLOAD_SCHEMA_MAJOR,
        });
    }
    Ok(decoder)
}

fn expect_key(decoder: &mut Decoder<'_>, expected: u64) -> Result<(), CodecError> {
    let key = decoder
        .u64()
        .map_err(|err| CodecError::Malformed(err.to_string()))?;
    if key < expected {
        return Err(CodecError::KeyOutOfOrder);
    }
    if key > expected {
        return Err(CodecError::UnknownFieldKey(key));
    }
    Ok(())
}

fn read_fixed<const N: usize>(
    decoder: &mut Decoder<'_>,
    field: &'static str,
) -> Result<[u8; N], CodecError> {
    let slice = decoder
        .bytes()
        .map_err(|err| CodecError::Malformed(err.to_string()))?;
    slice.try_into().map_err(|_| CodecError::InvalidLength {
        field,
        actual: slice.len(),
    })
}

fn read_nonzero_u64(decoder: &mut Decoder<'_>, field: &'static str) -> Result<u64, CodecError> {
    let value = decoder
        .u64()
        .map_err(|err| CodecError::Malformed(err.to_string()))?;
    if value == 0 {
        return Err(CodecError::ZeroGeneration(field));
    }
    Ok(value)
}

fn finish_decoder(decoder: &Decoder<'_>, total_len: usize) -> Result<(), CodecError> {
    if decoder.position() != total_len {
        return Err(CodecError::TrailingBytes);
    }
    Ok(())
}
