//! Deterministic CBOR payload codec for the mock driver's typed IPC face.
//!
//! The payloads live inside the shared SABI `Envelope.payload` bytes. They
//! are deliberately a crate-local deterministic `minicbor` map encoding
//! (mirroring the `nlos-canonical` discipline: exact field count, ascending
//! keys, fixed-width identifiers, fail-closed schema identity) rather than a
//! frozen `nlos-schema` protobuf: the mock driver plane is not yet a frozen
//! schema channel, and promoting it is a follow-up decision, not a side
//! effect of this crate.

use std::error::Error;
use std::fmt;

use minicbor::{Decoder, Encoder};
use nlos_operation::{CompletionOutcome, OperationState};
use nlos_types::ReceiptId;

/// Schema identity carried by every mock driver payload.
pub const PAYLOAD_SCHEMA_NAME: &str = "nlos.driver-mock.payload";
pub const PAYLOAD_SCHEMA_MAJOR: u32 = 1;
/// Upper bound for one encoded payload (well below the envelope bound).
pub const MAX_DRIVER_MOCK_PAYLOAD_BYTES: usize = 4096;

/// Wire codes for provider terminal outcomes. `0` stays unspecified so a
/// decoding peer fails closed on an absent outcome.
pub const OUTCOME_CODE_COMPLETED: u8 = 1;
pub const OUTCOME_CODE_FAILED: u8 = 2;
pub const OUTCOME_CODE_PARTIAL_EFFECT: u8 = 3;
pub const OUTCOME_CODE_EFFECT_UNKNOWN: u8 = 4;
pub const OUTCOME_CODE_CANCELLED_BEFORE_EFFECT: u8 = 5;

/// Wire codes for `OperationState`. `0` stays unspecified.
pub const STATE_CODE_REGISTERED: u8 = 1;
pub const STATE_CODE_DISPATCHED: u8 = 2;
pub const STATE_CODE_CANCEL_REQUESTED: u8 = 3;
pub const STATE_CODE_COMPLETED: u8 = 4;
pub const STATE_CODE_FAILED: u8 = 5;
pub const STATE_CODE_CANCELLED_BEFORE_EFFECT: u8 = 6;
pub const STATE_CODE_PARTIAL_EFFECT: u8 = 7;
pub const STATE_CODE_EFFECT_UNKNOWN: u8 = 8;

const ID_BYTES: usize = 16;
const SEED_BYTES: usize = 32;

#[derive(Debug)]
pub enum CodecError {
    /// The infallible vector writer failed; mirrors `nlos-canonical`.
    Encoding(String),
    Malformed(String),
    WrongFieldCount {
        actual: u64,
        expected: u64,
    },
    WrongSchemaName,
    UnsupportedMajor {
        actual: u32,
        supported: u32,
    },
    UnknownFieldKey(u64),
    KeyOutOfOrder,
    TrailingBytes,
    InvalidLength {
        field: &'static str,
        actual: usize,
    },
    ZeroGeneration(&'static str),
    UnknownOutcomeCode(u8),
    UnknownStateCode(u8),
}

impl fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encoding(message) => write!(formatter, "payload encoding failed: {message}"),
            Self::Malformed(message) => write!(formatter, "malformed payload: {message}"),
            Self::WrongFieldCount { actual, expected } => write!(
                formatter,
                "payload has {actual} fields; exactly {expected} are required"
            ),
            Self::WrongSchemaName => {
                write!(
                    formatter,
                    "payload schema name is not {PAYLOAD_SCHEMA_NAME}"
                )
            }
            Self::UnsupportedMajor { actual, supported } => write!(
                formatter,
                "payload schema major {actual} is unsupported; {supported} is required"
            ),
            Self::UnknownFieldKey(key) => write!(formatter, "unknown payload field key {key}"),
            Self::KeyOutOfOrder => formatter.write_str("payload field keys are out of order"),
            Self::TrailingBytes => formatter.write_str("payload has trailing bytes"),
            Self::InvalidLength { field, actual } => write!(
                formatter,
                "payload field {field} has {actual} bytes; a fixed width is required"
            ),
            Self::ZeroGeneration(field) => {
                write!(
                    formatter,
                    "payload field {field} generation must be non-zero"
                )
            }
            Self::UnknownOutcomeCode(code) => {
                write!(formatter, "unknown payload outcome code {code}")
            }
            Self::UnknownStateCode(code) => write!(formatter, "unknown payload state code {code}"),
        }
    }
}

impl Error for CodecError {}

/// One provider registration request as carried on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisterOperationWire {
    pub operation_id: [u8; ID_BYTES],
    pub operation_generation: u64,
    pub owner_fiber_id: [u8; ID_BYTES],
    pub owner_fiber_generation: u64,
    pub cancellation_scope_id: [u8; ID_BYTES],
    pub cancellation_generation: u64,
}

/// One provider dispatch request as carried on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DispatchOperationWire {
    pub operation_id: [u8; ID_BYTES],
    pub operation_generation: u64,
    pub callback_id: [u8; ID_BYTES],
}

/// One provider completion request as carried on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompleteOperationWire {
    pub operation_id: [u8; ID_BYTES],
    pub operation_generation: u64,
    pub callback_id: [u8; ID_BYTES],
    pub seed: [u8; SEED_BYTES],
}

/// Registration result as carried on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisterOperationResultWire {
    pub replayed: bool,
    pub operation_id: [u8; ID_BYTES],
    pub operation_generation: u64,
    pub admission_receipt_id: [u8; ID_BYTES],
}

/// Dispatch result as carried on the wire: the durable receipts plus the
/// fenced one-shot callback ticket.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DispatchOperationResultWire {
    pub replayed: bool,
    pub preparation_receipt_id: [u8; ID_BYTES],
    pub activation_receipt_id: [u8; ID_BYTES],
    pub callback_id: [u8; ID_BYTES],
    pub operation_id: [u8; ID_BYTES],
    pub operation_generation: u64,
    pub owner_fiber_id: [u8; ID_BYTES],
    pub owner_fiber_generation: u64,
    pub cancel_epoch: u64,
}

/// Completion result as carried on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompleteOperationResultWire {
    pub replayed: bool,
    pub outcome_code: u8,
    pub receipt_id: [u8; ID_BYTES],
    pub state_code: u8,
}

/// The terminal outcome class without its receipt identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalOutcomeKind {
    Completed,
    Failed,
    PartialEffect,
    EffectUnknown,
    CancelledBeforeEffect,
}

/// Encodes the wire outcome code for one terminal outcome.
#[must_use]
pub fn outcome_wire_code(outcome: CompletionOutcome) -> u8 {
    match outcome {
        CompletionOutcome::Completed { .. } => OUTCOME_CODE_COMPLETED,
        CompletionOutcome::Failed { .. } => OUTCOME_CODE_FAILED,
        CompletionOutcome::PartialEffect { .. } => OUTCOME_CODE_PARTIAL_EFFECT,
        CompletionOutcome::EffectUnknown { .. } => OUTCOME_CODE_EFFECT_UNKNOWN,
        CompletionOutcome::CancelledBeforeEffect { .. } => OUTCOME_CODE_CANCELLED_BEFORE_EFFECT,
    }
}

/// Decodes the wire outcome code into its terminal class.
///
/// # Errors
///
/// Returns [`CodecError::UnknownOutcomeCode`] for any unspecified code.
pub fn outcome_from_wire_code(code: u8) -> Result<TerminalOutcomeKind, CodecError> {
    match code {
        OUTCOME_CODE_COMPLETED => Ok(TerminalOutcomeKind::Completed),
        OUTCOME_CODE_FAILED => Ok(TerminalOutcomeKind::Failed),
        OUTCOME_CODE_PARTIAL_EFFECT => Ok(TerminalOutcomeKind::PartialEffect),
        OUTCOME_CODE_EFFECT_UNKNOWN => Ok(TerminalOutcomeKind::EffectUnknown),
        OUTCOME_CODE_CANCELLED_BEFORE_EFFECT => Ok(TerminalOutcomeKind::CancelledBeforeEffect),
        other => Err(CodecError::UnknownOutcomeCode(other)),
    }
}

/// Encodes the wire state code for one operation state.
#[must_use]
pub fn state_wire_code(state: OperationState) -> u8 {
    match state {
        OperationState::Registered => STATE_CODE_REGISTERED,
        OperationState::Dispatched => STATE_CODE_DISPATCHED,
        OperationState::CancelRequested => STATE_CODE_CANCEL_REQUESTED,
        OperationState::Completed { .. } => STATE_CODE_COMPLETED,
        OperationState::Failed { .. } => STATE_CODE_FAILED,
        OperationState::CancelledBeforeEffect { .. } => STATE_CODE_CANCELLED_BEFORE_EFFECT,
        OperationState::PartialEffect { .. } => STATE_CODE_PARTIAL_EFFECT,
        OperationState::EffectUnknown { .. } => STATE_CODE_EFFECT_UNKNOWN,
    }
}

/// Decodes the wire state code back into an `OperationState`; a terminal
/// code yields the state with a zero receipt (the receipt travels as its own
/// payload field).
///
/// # Errors
///
/// Returns [`CodecError::UnknownStateCode`] for any unspecified code.
pub fn state_from_wire_code(code: u8) -> Result<OperationState, CodecError> {
    let zero = || ReceiptId::from_bytes([0; ID_BYTES]);
    match code {
        STATE_CODE_REGISTERED => Ok(OperationState::Registered),
        STATE_CODE_DISPATCHED => Ok(OperationState::Dispatched),
        STATE_CODE_CANCEL_REQUESTED => Ok(OperationState::CancelRequested),
        STATE_CODE_COMPLETED => Ok(OperationState::Completed { receipt_id: zero() }),
        STATE_CODE_FAILED => Ok(OperationState::Failed { receipt_id: zero() }),
        STATE_CODE_CANCELLED_BEFORE_EFFECT => {
            Ok(OperationState::CancelledBeforeEffect { receipt_id: zero() })
        }
        STATE_CODE_PARTIAL_EFFECT => Ok(OperationState::PartialEffect { receipt_id: zero() }),
        STATE_CODE_EFFECT_UNKNOWN => Ok(OperationState::EffectUnknown { receipt_id: zero() }),
        other => Err(CodecError::UnknownStateCode(other)),
    }
}

/// # Errors
///
/// Returns a [`CodecError`] when the deterministic encoding cannot be
/// produced (the vector writer is infallible, so this is defensive only).
pub fn encode_register_request(value: &RegisterOperationWire) -> Result<Vec<u8>, CodecError> {
    let mut encoder = Encoder::new(Vec::new());
    encoder
        .map(8)
        .and_then(|e| e.u8(0))
        .and_then(|e| e.str(PAYLOAD_SCHEMA_NAME))
        .and_then(|e| e.u8(1))
        .and_then(|e| e.u32(PAYLOAD_SCHEMA_MAJOR))
        .and_then(|e| e.u8(2))
        .and_then(|e| e.bytes(&value.operation_id))
        .and_then(|e| e.u8(3))
        .and_then(|e| e.u64(value.operation_generation))
        .and_then(|e| e.u8(4))
        .and_then(|e| e.bytes(&value.owner_fiber_id))
        .and_then(|e| e.u8(5))
        .and_then(|e| e.u64(value.owner_fiber_generation))
        .and_then(|e| e.u8(6))
        .and_then(|e| e.bytes(&value.cancellation_scope_id))
        .and_then(|e| e.u8(7))
        .and_then(|e| e.u64(value.cancellation_generation))
        .map_err(encode_error)?;
    finish(encoder)
}

/// # Errors
///
/// Fails closed on any field-count, schema, key-order, width, or trailing
/// byte deviation.
pub fn decode_register_request(bytes: &[u8]) -> Result<RegisterOperationWire, CodecError> {
    let mut decoder = Decoder::new(bytes);
    expect_field_count(&mut decoder, 8)?;
    expect_schema(&mut decoder)?;
    let operation_id = fixed_id(&mut decoder, 2, "operation_id")?;
    let operation_generation = generation_field(&mut decoder, 3, "operation")?;
    let owner_fiber_id = fixed_id(&mut decoder, 4, "owner_fiber_id")?;
    let owner_fiber_generation = generation_field(&mut decoder, 5, "owner_fiber")?;
    let cancellation_scope_id = fixed_id(&mut decoder, 6, "cancellation_scope_id")?;
    let cancellation_generation = generation_field(&mut decoder, 7, "cancellation")?;
    expect_end(&mut decoder, bytes.len())?;
    Ok(RegisterOperationWire {
        operation_id,
        operation_generation,
        owner_fiber_id,
        owner_fiber_generation,
        cancellation_scope_id,
        cancellation_generation,
    })
}

/// # Errors
///
/// Returns a [`CodecError`] when the deterministic encoding cannot be
/// produced.
pub fn encode_register_result(value: &RegisterOperationResultWire) -> Result<Vec<u8>, CodecError> {
    let mut encoder = Encoder::new(Vec::new());
    encoder
        .map(6)
        .and_then(|e| e.u8(0))
        .and_then(|e| e.str(PAYLOAD_SCHEMA_NAME))
        .and_then(|e| e.u8(1))
        .and_then(|e| e.u32(PAYLOAD_SCHEMA_MAJOR))
        .and_then(|e| e.u8(2))
        .and_then(|e| e.bool(value.replayed))
        .and_then(|e| e.u8(3))
        .and_then(|e| e.bytes(&value.operation_id))
        .and_then(|e| e.u8(4))
        .and_then(|e| e.u64(value.operation_generation))
        .and_then(|e| e.u8(5))
        .and_then(|e| e.bytes(&value.admission_receipt_id))
        .map_err(encode_error)?;
    finish(encoder)
}

/// # Errors
///
/// Fails closed on any field-count, schema, key-order, width, or trailing
/// byte deviation.
pub fn decode_register_result(bytes: &[u8]) -> Result<RegisterOperationResultWire, CodecError> {
    let mut decoder = Decoder::new(bytes);
    expect_field_count(&mut decoder, 6)?;
    expect_schema(&mut decoder)?;
    let replayed = bool_field(&mut decoder, 2)?;
    let operation_id = fixed_id(&mut decoder, 3, "operation_id")?;
    let operation_generation = u64_field(&mut decoder, 4)?;
    let admission_receipt_id = fixed_id(&mut decoder, 5, "admission_receipt_id")?;
    expect_end(&mut decoder, bytes.len())?;
    Ok(RegisterOperationResultWire {
        replayed,
        operation_id,
        operation_generation,
        admission_receipt_id,
    })
}

/// # Errors
///
/// Returns a [`CodecError`] when the deterministic encoding cannot be
/// produced.
pub fn encode_dispatch_request(value: &DispatchOperationWire) -> Result<Vec<u8>, CodecError> {
    let mut encoder = Encoder::new(Vec::new());
    encoder
        .map(5)
        .and_then(|e| e.u8(0))
        .and_then(|e| e.str(PAYLOAD_SCHEMA_NAME))
        .and_then(|e| e.u8(1))
        .and_then(|e| e.u32(PAYLOAD_SCHEMA_MAJOR))
        .and_then(|e| e.u8(2))
        .and_then(|e| e.bytes(&value.operation_id))
        .and_then(|e| e.u8(3))
        .and_then(|e| e.u64(value.operation_generation))
        .and_then(|e| e.u8(4))
        .and_then(|e| e.bytes(&value.callback_id))
        .map_err(encode_error)?;
    finish(encoder)
}

/// # Errors
///
/// Fails closed on any field-count, schema, key-order, width, or trailing
/// byte deviation.
pub fn decode_dispatch_request(bytes: &[u8]) -> Result<DispatchOperationWire, CodecError> {
    let mut decoder = Decoder::new(bytes);
    expect_field_count(&mut decoder, 5)?;
    expect_schema(&mut decoder)?;
    let operation_id = fixed_id(&mut decoder, 2, "operation_id")?;
    let operation_generation = generation_field(&mut decoder, 3, "operation")?;
    let callback_id = fixed_id(&mut decoder, 4, "callback_id")?;
    expect_end(&mut decoder, bytes.len())?;
    Ok(DispatchOperationWire {
        operation_id,
        operation_generation,
        callback_id,
    })
}

/// # Errors
///
/// Returns a [`CodecError`] when the deterministic encoding cannot be
/// produced.
pub fn encode_dispatch_result(value: &DispatchOperationResultWire) -> Result<Vec<u8>, CodecError> {
    let mut encoder = Encoder::new(Vec::new());
    encoder
        .map(11)
        .and_then(|e| e.u8(0))
        .and_then(|e| e.str(PAYLOAD_SCHEMA_NAME))
        .and_then(|e| e.u8(1))
        .and_then(|e| e.u32(PAYLOAD_SCHEMA_MAJOR))
        .and_then(|e| e.u8(2))
        .and_then(|e| e.bool(value.replayed))
        .and_then(|e| e.u8(3))
        .and_then(|e| e.bytes(&value.preparation_receipt_id))
        .and_then(|e| e.u8(4))
        .and_then(|e| e.bytes(&value.activation_receipt_id))
        .and_then(|e| e.u8(5))
        .and_then(|e| e.bytes(&value.callback_id))
        .and_then(|e| e.u8(6))
        .and_then(|e| e.bytes(&value.operation_id))
        .and_then(|e| e.u8(7))
        .and_then(|e| e.u64(value.operation_generation))
        .and_then(|e| e.u8(8))
        .and_then(|e| e.bytes(&value.owner_fiber_id))
        .and_then(|e| e.u8(9))
        .and_then(|e| e.u64(value.owner_fiber_generation))
        .and_then(|e| e.u8(10))
        .and_then(|e| e.u64(value.cancel_epoch))
        .map_err(encode_error)?;
    finish(encoder)
}

/// # Errors
///
/// Fails closed on any field-count, schema, key-order, width, or trailing
/// byte deviation.
pub fn decode_dispatch_result(bytes: &[u8]) -> Result<DispatchOperationResultWire, CodecError> {
    let mut decoder = Decoder::new(bytes);
    expect_field_count(&mut decoder, 11)?;
    expect_schema(&mut decoder)?;
    let replayed = bool_field(&mut decoder, 2)?;
    let preparation_receipt_id = fixed_id(&mut decoder, 3, "preparation_receipt_id")?;
    let activation_receipt_id = fixed_id(&mut decoder, 4, "activation_receipt_id")?;
    let callback_id = fixed_id(&mut decoder, 5, "callback_id")?;
    let operation_id = fixed_id(&mut decoder, 6, "operation_id")?;
    let operation_generation = u64_field(&mut decoder, 7)?;
    let owner_fiber_id = fixed_id(&mut decoder, 8, "owner_fiber_id")?;
    let owner_fiber_generation = u64_field(&mut decoder, 9)?;
    let cancel_epoch = u64_field(&mut decoder, 10)?;
    expect_end(&mut decoder, bytes.len())?;
    Ok(DispatchOperationResultWire {
        replayed,
        preparation_receipt_id,
        activation_receipt_id,
        callback_id,
        operation_id,
        operation_generation,
        owner_fiber_id,
        owner_fiber_generation,
        cancel_epoch,
    })
}

/// # Errors
///
/// Returns a [`CodecError`] when the deterministic encoding cannot be
/// produced.
pub fn encode_complete_request(value: &CompleteOperationWire) -> Result<Vec<u8>, CodecError> {
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
        .and_then(|e| e.bytes(&value.callback_id))
        .and_then(|e| e.u8(5))
        .and_then(|e| e.bytes(&value.seed))
        .map_err(encode_error)?;
    finish(encoder)
}

/// # Errors
///
/// Fails closed on any field-count, schema, key-order, width, or trailing
/// byte deviation.
pub fn decode_complete_request(bytes: &[u8]) -> Result<CompleteOperationWire, CodecError> {
    let mut decoder = Decoder::new(bytes);
    expect_field_count(&mut decoder, 6)?;
    expect_schema(&mut decoder)?;
    let operation_id = fixed_id(&mut decoder, 2, "operation_id")?;
    let operation_generation = generation_field(&mut decoder, 3, "operation")?;
    let callback_id = fixed_id(&mut decoder, 4, "callback_id")?;
    let seed = fixed_seed(&mut decoder, 5)?;
    expect_end(&mut decoder, bytes.len())?;
    Ok(CompleteOperationWire {
        operation_id,
        operation_generation,
        callback_id,
        seed,
    })
}

/// # Errors
///
/// Returns a [`CodecError`] when the deterministic encoding cannot be
/// produced.
pub fn encode_complete_result(value: &CompleteOperationResultWire) -> Result<Vec<u8>, CodecError> {
    let mut encoder = Encoder::new(Vec::new());
    encoder
        .map(6)
        .and_then(|e| e.u8(0))
        .and_then(|e| e.str(PAYLOAD_SCHEMA_NAME))
        .and_then(|e| e.u8(1))
        .and_then(|e| e.u32(PAYLOAD_SCHEMA_MAJOR))
        .and_then(|e| e.u8(2))
        .and_then(|e| e.bool(value.replayed))
        .and_then(|e| e.u8(3))
        .and_then(|e| e.u8(value.outcome_code))
        .and_then(|e| e.u8(4))
        .and_then(|e| e.bytes(&value.receipt_id))
        .and_then(|e| e.u8(5))
        .and_then(|e| e.u8(value.state_code))
        .map_err(encode_error)?;
    finish(encoder)
}

/// # Errors
///
/// Fails closed on any field-count, schema, key-order, width, or trailing
/// byte deviation.
pub fn decode_complete_result(bytes: &[u8]) -> Result<CompleteOperationResultWire, CodecError> {
    let mut decoder = Decoder::new(bytes);
    expect_field_count(&mut decoder, 6)?;
    expect_schema(&mut decoder)?;
    let replayed = bool_field(&mut decoder, 2)?;
    let outcome_code = u8_field(&mut decoder, 3)?;
    let receipt_id = fixed_id(&mut decoder, 4, "receipt_id")?;
    let state_code = u8_field(&mut decoder, 5)?;
    expect_end(&mut decoder, bytes.len())?;
    Ok(CompleteOperationResultWire {
        replayed,
        outcome_code,
        receipt_id,
        state_code,
    })
}

fn finish(encoder: Encoder<Vec<u8>>) -> Result<Vec<u8>, CodecError> {
    let bytes = encoder.into_writer();
    if bytes.len() > MAX_DRIVER_MOCK_PAYLOAD_BYTES {
        return Err(CodecError::InvalidLength {
            field: "payload",
            actual: bytes.len(),
        });
    }
    Ok(bytes)
}

fn expect_field_count(decoder: &mut Decoder<'_>, expected: u64) -> Result<(), CodecError> {
    let actual = decoder.map().map_err(decode_error)?.ok_or_else(|| {
        CodecError::Malformed("indefinite-length payload maps are not accepted".to_owned())
    })?;
    if actual != expected {
        return Err(CodecError::WrongFieldCount { actual, expected });
    }
    Ok(())
}

fn expect_schema(decoder: &mut Decoder<'_>) -> Result<(), CodecError> {
    expect_key(decoder, 0)?;
    let name = decoder.str().map_err(decode_error)?;
    if name != PAYLOAD_SCHEMA_NAME {
        return Err(CodecError::WrongSchemaName);
    }
    expect_key(decoder, 1)?;
    let major = decoder.u32().map_err(decode_error)?;
    if major != PAYLOAD_SCHEMA_MAJOR {
        return Err(CodecError::UnsupportedMajor {
            actual: major,
            supported: PAYLOAD_SCHEMA_MAJOR,
        });
    }
    Ok(())
}

/// Reads the next map key and requires it to equal `expected`. A larger key
/// is an unknown field; a smaller key is out of order.
fn expect_key(decoder: &mut Decoder<'_>, expected: u64) -> Result<(), CodecError> {
    let actual = decoder.u64().map_err(decode_error)?;
    if actual == expected {
        return Ok(());
    }
    if actual > expected {
        return Err(CodecError::UnknownFieldKey(actual));
    }
    Err(CodecError::KeyOutOfOrder)
}

fn fixed_id(
    decoder: &mut Decoder<'_>,
    key: u64,
    field: &'static str,
) -> Result<[u8; ID_BYTES], CodecError> {
    expect_key(decoder, key)?;
    let bytes = decoder.bytes().map_err(decode_error)?;
    let value: [u8; ID_BYTES] = bytes.try_into().map_err(|_| CodecError::InvalidLength {
        field,
        actual: bytes.len(),
    })?;
    Ok(value)
}

fn fixed_seed(decoder: &mut Decoder<'_>, key: u64) -> Result<[u8; SEED_BYTES], CodecError> {
    expect_key(decoder, key)?;
    let bytes = decoder.bytes().map_err(decode_error)?;
    bytes.try_into().map_err(|_| CodecError::InvalidLength {
        field: "seed",
        actual: bytes.len(),
    })
}

fn generation_field(
    decoder: &mut Decoder<'_>,
    key: u64,
    field: &'static str,
) -> Result<u64, CodecError> {
    let value = u64_field(decoder, key)?;
    if value == 0 {
        return Err(CodecError::ZeroGeneration(field));
    }
    Ok(value)
}

fn u64_field(decoder: &mut Decoder<'_>, key: u64) -> Result<u64, CodecError> {
    expect_key(decoder, key)?;
    decoder.u64().map_err(decode_error)
}

fn bool_field(decoder: &mut Decoder<'_>, key: u64) -> Result<bool, CodecError> {
    expect_key(decoder, key)?;
    decoder.bool().map_err(decode_error)
}

fn u8_field(decoder: &mut Decoder<'_>, key: u64) -> Result<u8, CodecError> {
    expect_key(decoder, key)?;
    decoder.u8().map_err(decode_error)
}

fn expect_end(decoder: &mut Decoder<'_>, total: usize) -> Result<(), CodecError> {
    if decoder.position() != total {
        return Err(CodecError::TrailingBytes);
    }
    Ok(())
}

fn encode_error(error: impl fmt::Display) -> CodecError {
    CodecError::Encoding(error.to_string())
}

fn decode_error(error: minicbor::decode::Error) -> CodecError {
    let message = error.to_string();
    drop(error);
    CodecError::Malformed(message)
}
