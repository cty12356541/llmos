use minicbor::data::Type;
use minicbor::{Decoder, Encoder};
use nlos_types::NamespaceId;
use sha2::{Digest, Sha256};

use crate::{
    CriterionAggregation, CriterionEffect, EvaluatorKind, ImmutableEvaluatorReference,
    ImmutableEvaluatorReferenceKind, IntentConstraints, IntentCriterion, IntentCriticality,
    IntentSettlement, IntentSpecBody, MAX_CANONICAL_EVENT_BYTES, MAX_SPEC_CAPABILITY_REFS,
    MAX_SPEC_CRITERIA, MAX_SPEC_EXTENSION_BYTES, MAX_SPEC_EXTENSIONS, SemanticAuthorityError,
    SettlementMode, SettlementTimeoutAction, SpecExtension,
};

const SPEC_SCHEMA_NAME: &str = "llmos.intent-spec";
const SPEC_SCHEMA_MAJOR: u32 = 1;
const SPEC_SCHEMA_MINOR: u32 = 0;

/// Encodes the supported v1 `IntentSpecBody` profile as deterministic CBOR.
///
/// The Stage B profile represents `ResourceVector`, `ArtifactSelector`, authority,
/// independence, and risk policies by immutable digests. Unknown critical
/// extensions are rejected; bounded noncritical extensions round-trip.
///
/// # Errors
///
/// Rejects invalid bounds, ordering, settlement semantics, unsupported
/// critical extensions, or encoding failures.
pub fn encode_intent_spec_body(body: &IntentSpecBody) -> Result<Vec<u8>, SemanticAuthorityError> {
    validate_body(body)?;
    let mut criteria = body
        .acceptance
        .iter()
        .map(|criterion| {
            let bytes = encode_criterion(criterion)?;
            Ok((criterion_id_from_bytes(&bytes), bytes))
        })
        .collect::<Result<Vec<_>, SemanticAuthorityError>>()?;
    criteria.sort_unstable_by_key(|entry| entry.0);
    if criteria.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(SemanticAuthorityError::InvalidSpecBody(
            "duplicate criterion identity",
        ));
    }

    let mut encoder = Encoder::new(Vec::new());
    encoder
        .map(10)
        .and_then(|e| e.u8(0))
        .and_then(|e| e.str(SPEC_SCHEMA_NAME))
        .and_then(|e| e.u8(1))
        .and_then(|e| e.u32(SPEC_SCHEMA_MAJOR))
        .and_then(|e| e.u8(2))
        .and_then(|e| e.u32(SPEC_SCHEMA_MINOR))
        .and_then(|e| e.u8(3))
        .and_then(|e| e.bytes(&body.goal_digest))
        .and_then(|e| e.u8(4))
        .and_then(|e| e.array(criteria.len() as u64))
        .map_err(encoding_error)?;
    for (id, bytes) in &criteria {
        encoder
            .array(2)
            .and_then(|e| e.bytes(id))
            .and_then(|e| e.bytes(bytes))
            .map_err(encoding_error)?;
    }
    encoder.u8(5).map_err(encoding_error)?;
    encode_constraints(&mut encoder, &body.constraints)?;
    encoder
        .u8(6)
        .and_then(|e| e.u8(criticality_code(body.criticality)))
        .and_then(|e| e.u8(7))
        .map_err(encoding_error)?;
    encode_settlement(&mut encoder, body.settlement)?;
    encoder.u8(8).map_err(encoding_error)?;
    encode_extensions(&mut encoder, &body.critical_extensions)?;
    encoder.u8(9).map_err(encoding_error)?;
    encode_extensions(&mut encoder, &body.noncritical_extensions)?;
    let bytes = encoder.into_writer();
    if bytes.len() > MAX_CANONICAL_EVENT_BYTES {
        return Err(SemanticAuthorityError::CanonicalTooLarge);
    }
    Ok(bytes)
}

/// Strictly decodes and re-encodes the supported v1 `IntentSpecBody` profile.
///
/// # Errors
///
/// Rejects malformed, indefinite, trailing, noncanonical, unsupported, or
/// semantically invalid body bytes.
pub fn decode_intent_spec_body(bytes: &[u8]) -> Result<IntentSpecBody, SemanticAuthorityError> {
    if bytes.len() > MAX_CANONICAL_EVENT_BYTES {
        return Err(SemanticAuthorityError::CanonicalTooLarge);
    }
    let mut decoder = Decoder::new(bytes);
    if decoder.map().map_err(decode_error)? != Some(10) {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    expect_key(&mut decoder, 0)?;
    if decoder.str().map_err(decode_error)? != SPEC_SCHEMA_NAME {
        return Err(SemanticAuthorityError::UnsupportedSchema);
    }
    expect_key(&mut decoder, 1)?;
    if decoder.u32().map_err(decode_error)? != SPEC_SCHEMA_MAJOR {
        return Err(SemanticAuthorityError::UnsupportedSchema);
    }
    expect_key(&mut decoder, 2)?;
    if decoder.u32().map_err(decode_error)? != SPEC_SCHEMA_MINOR {
        return Err(SemanticAuthorityError::UnsupportedSchema);
    }
    expect_key(&mut decoder, 3)?;
    let goal_digest = fixed_bytes(&mut decoder, "goal digest")?;
    expect_key(&mut decoder, 4)?;
    let count = definite_len(decoder.array().map_err(decode_error)?, "criteria")?;
    if count > MAX_SPEC_CRITERIA {
        return Err(SemanticAuthorityError::InvalidSpecBody("too many criteria"));
    }
    let mut acceptance = Vec::with_capacity(count);
    let mut previous_id = None;
    for _ in 0..count {
        if decoder.array().map_err(decode_error)? != Some(2) {
            return Err(SemanticAuthorityError::CanonicalMismatch);
        }
        let claimed_id: [u8; 32] = fixed_bytes(&mut decoder, "criterion id")?;
        if previous_id.is_some_and(|previous| previous >= claimed_id) {
            return Err(SemanticAuthorityError::CanonicalMismatch);
        }
        let criterion_bytes = decoder.bytes().map_err(decode_error)?;
        let criterion = decode_criterion(criterion_bytes)?;
        if criterion_id_from_bytes(criterion_bytes) != claimed_id {
            return Err(SemanticAuthorityError::InvalidSpecBody(
                "criterion identity mismatch",
            ));
        }
        previous_id = Some(claimed_id);
        acceptance.push(criterion);
    }
    expect_key(&mut decoder, 5)?;
    let constraints = decode_constraints(&mut decoder)?;
    expect_key(&mut decoder, 6)?;
    let criticality = decode_criticality(decoder.u8().map_err(decode_error)?)?;
    expect_key(&mut decoder, 7)?;
    let settlement = decode_settlement(&mut decoder)?;
    expect_key(&mut decoder, 8)?;
    let critical_extensions = decode_extensions(&mut decoder)?;
    expect_key(&mut decoder, 9)?;
    let noncritical_extensions = decode_extensions(&mut decoder)?;
    if decoder.position() != bytes.len() {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    let body = IntentSpecBody {
        goal_digest,
        acceptance,
        constraints,
        criticality,
        settlement,
        critical_extensions,
        noncritical_extensions,
    };
    validate_body(&body)?;
    if encode_intent_spec_body(&body)? != bytes {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    Ok(body)
}

/// Returns `H("llmos/criterion/v1" || canonical_criterion)`.
///
/// # Errors
///
/// Rejects a criterion that violates the bounded Stage B profile.
pub fn criterion_id(criterion: &IntentCriterion) -> Result<[u8; 32], SemanticAuthorityError> {
    Ok(criterion_id_from_bytes(&encode_criterion(criterion)?))
}

/// Returns `H("llmos/intent-spec-body/v1" || canonical_spec_body)`.
///
/// # Errors
///
/// Rejects a body that violates canonical or settlement invariants.
pub fn intent_spec_body_digest(body: &IntentSpecBody) -> Result<[u8; 32], SemanticAuthorityError> {
    let bytes = encode_intent_spec_body(body)?;
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/intent-spec-body/v1");
    hasher.update(bytes);
    Ok(hasher.finalize().into())
}

/// Returns the exact digest of the sorted, non-empty hard criterion set.
///
/// # Errors
///
/// Rejects a hard criterion that cannot be canonically encoded.
pub fn hard_criteria_digest(
    body: &IntentSpecBody,
) -> Result<Option<[u8; 32]>, SemanticAuthorityError> {
    let mut ids = body
        .acceptance
        .iter()
        .filter(|criterion| criterion.effect == CriterionEffect::Hard)
        .map(criterion_id)
        .collect::<Result<Vec<_>, _>>()?;
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(None);
    }
    let mut encoder = Encoder::new(Vec::new());
    encoder.array(ids.len() as u64).map_err(encoding_error)?;
    for id in ids {
        encoder.bytes(&id).map_err(encoding_error)?;
    }
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/hard-criteria/v1");
    hasher.update(encoder.into_writer());
    Ok(Some(hasher.finalize().into()))
}

fn validate_body(body: &IntentSpecBody) -> Result<(), SemanticAuthorityError> {
    if body.acceptance.len() > MAX_SPEC_CRITERIA {
        return Err(SemanticAuthorityError::InvalidSpecBody("too many criteria"));
    }
    if !body.critical_extensions.is_empty() {
        return Err(SemanticAuthorityError::UnsupportedCriticalSpecExtension);
    }
    validate_extensions(&body.noncritical_extensions)?;
    validate_digest_set(
        &body.constraints.allowed_capability_digests,
        "allowed capabilities",
    )?;
    validate_digest_set(
        &body.constraints.forbidden_capability_digests,
        "forbidden capabilities",
    )?;
    if body
        .constraints
        .allowed_capability_digests
        .iter()
        .any(|digest| {
            body.constraints
                .forbidden_capability_digests
                .contains(digest)
        })
    {
        return Err(SemanticAuthorityError::InvalidSpecBody(
            "capability cannot be both allowed and forbidden",
        ));
    }
    for criterion in &body.acceptance {
        validate_criterion(criterion)?;
    }
    let actual_hard_digest = hard_criteria_digest(body)?;
    match body.settlement.mode {
        SettlementMode::None => {
            if body.settlement.hard_criteria_digest.is_some()
                || body.settlement.challenge_window_ms.is_some()
            {
                return Err(SemanticAuthorityError::InvalidSpecBody(
                    "NONE settlement cannot carry hard digest or challenge window",
                ));
            }
        }
        SettlementMode::Automatic => {
            if actual_hard_digest.is_none()
                || body.settlement.hard_criteria_digest != actual_hard_digest
            {
                return Err(SemanticAuthorityError::InvalidSpecBody(
                    "AUTOMATIC settlement must bind the complete non-empty hard set",
                ));
            }
        }
        SettlementMode::Manual => {
            if body.settlement.hard_criteria_digest.is_some()
                && body.settlement.hard_criteria_digest != actual_hard_digest
            {
                return Err(SemanticAuthorityError::InvalidSpecBody(
                    "MANUAL hard digest does not match the complete hard set",
                ));
            }
        }
    }
    if body.settlement.challenge_window_ms == Some(0) {
        return Err(SemanticAuthorityError::InvalidSpecBody(
            "challenge window must be positive",
        ));
    }
    Ok(())
}

fn validate_criterion(criterion: &IntentCriterion) -> Result<(), SemanticAuthorityError> {
    if criterion.aggregation.pass_quorum == 0 || criterion.aggregation.fail_quorum == 0 {
        return Err(SemanticAuthorityError::InvalidSpecBody(
            "criterion quorums must be positive",
        ));
    }
    if criterion.timeout_ms == Some(0) {
        return Err(SemanticAuthorityError::InvalidSpecBody(
            "criterion timeout must be positive",
        ));
    }
    if criterion.effect == CriterionEffect::Hard
        && criterion.evaluator_kind != EvaluatorKind::DeterministicTool
        && (criterion.timeout_ms.is_none()
            || criterion.independence_policy_digest.is_none()
            || criterion.authority_policy_digest.is_none()
            || criterion.risk_policy_digest.is_none())
    {
        return Err(SemanticAuthorityError::InvalidSpecBody(
            "hard model/human criterion lacks authority, independence, timeout, or risk policy",
        ));
    }
    Ok(())
}

fn encode_criterion(criterion: &IntentCriterion) -> Result<Vec<u8>, SemanticAuthorityError> {
    validate_criterion(criterion)?;
    let mut encoder = Encoder::new(Vec::new());
    encoder
        .array(10)
        .and_then(|e| e.bytes(&criterion.description_digest))
        .and_then(|e| e.u8(effect_code(criterion.effect)))
        .and_then(|e| e.u8(evaluator_code(criterion.evaluator_kind)))
        .and_then(|e| e.array(2))
        .and_then(|e| e.u8(evaluator_reference_code(criterion.evaluator_ref.kind)))
        .and_then(|e| e.bytes(&criterion.evaluator_ref.digest))
        .and_then(|e| e.bytes(&criterion.target_selector_digest))
        .map_err(encoding_error)?;
    encode_optional_u64(&mut encoder, criterion.timeout_ms)?;
    encode_optional_digest(&mut encoder, criterion.independence_policy_digest)?;
    encode_optional_digest(&mut encoder, criterion.authority_policy_digest)?;
    encode_optional_digest(&mut encoder, criterion.risk_policy_digest)?;
    encoder
        .array(3)
        .and_then(|e| e.u16(criterion.aggregation.pass_quorum))
        .and_then(|e| e.u16(criterion.aggregation.fail_quorum))
        .and_then(|e| e.bool(criterion.aggregation.veto_on_authorized_fail))
        .map_err(encoding_error)?;
    Ok(encoder.into_writer())
}

fn decode_criterion(bytes: &[u8]) -> Result<IntentCriterion, SemanticAuthorityError> {
    let mut decoder = Decoder::new(bytes);
    if decoder.array().map_err(decode_error)? != Some(10) {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    let description_digest = fixed_bytes(&mut decoder, "criterion description")?;
    let effect = decode_effect(decoder.u8().map_err(decode_error)?)?;
    let evaluator_kind = decode_evaluator(decoder.u8().map_err(decode_error)?)?;
    if decoder.array().map_err(decode_error)? != Some(2) {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    let evaluator_ref = ImmutableEvaluatorReference {
        kind: decode_evaluator_reference(decoder.u8().map_err(decode_error)?)?,
        digest: fixed_bytes(&mut decoder, "evaluator reference")?,
    };
    let target_selector_digest = fixed_bytes(&mut decoder, "target selector")?;
    let timeout_ms = decode_optional_u64(&mut decoder)?;
    let independence_policy_digest = decode_optional_digest(&mut decoder)?;
    let authority_policy_digest = decode_optional_digest(&mut decoder)?;
    let risk_policy_digest = decode_optional_digest(&mut decoder)?;
    if decoder.array().map_err(decode_error)? != Some(3) {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    let aggregation = CriterionAggregation {
        pass_quorum: decoder.u16().map_err(decode_error)?,
        fail_quorum: decoder.u16().map_err(decode_error)?,
        veto_on_authorized_fail: decoder.bool().map_err(decode_error)?,
    };
    if decoder.position() != bytes.len() {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    let criterion = IntentCriterion {
        description_digest,
        effect,
        evaluator_kind,
        evaluator_ref,
        target_selector_digest,
        timeout_ms,
        independence_policy_digest,
        authority_policy_digest,
        risk_policy_digest,
        aggregation,
    };
    validate_criterion(&criterion)?;
    if encode_criterion(&criterion)? != bytes {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    Ok(criterion)
}

fn encode_constraints(
    encoder: &mut Encoder<Vec<u8>>,
    constraints: &IntentConstraints,
) -> Result<(), SemanticAuthorityError> {
    encoder
        .array(5)
        .and_then(|e| e.bytes(&constraints.resource_vector_digest))
        .map_err(encoding_error)?;
    encode_optional_u64(encoder, constraints.deadline_ms)?;
    encoder
        .bytes(constraints.namespace_root.as_bytes())
        .map_err(encoding_error)?;
    encode_digest_array(encoder, &constraints.allowed_capability_digests)?;
    encode_digest_array(encoder, &constraints.forbidden_capability_digests)
}

fn decode_constraints(
    decoder: &mut Decoder<'_>,
) -> Result<IntentConstraints, SemanticAuthorityError> {
    if decoder.array().map_err(decode_error)? != Some(5) {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    Ok(IntentConstraints {
        resource_vector_digest: fixed_bytes(decoder, "resource vector")?,
        deadline_ms: decode_optional_u64(decoder)?,
        namespace_root: NamespaceId::from_bytes(fixed_bytes(decoder, "namespace root")?),
        allowed_capability_digests: decode_digest_array(decoder)?,
        forbidden_capability_digests: decode_digest_array(decoder)?,
    })
}

fn encode_settlement(
    encoder: &mut Encoder<Vec<u8>>,
    settlement: IntentSettlement,
) -> Result<(), SemanticAuthorityError> {
    encoder
        .array(4)
        .and_then(|e| e.u8(settlement_mode_code(settlement.mode)))
        .map_err(encoding_error)?;
    encode_optional_digest(encoder, settlement.hard_criteria_digest)?;
    encoder
        .u8(timeout_action_code(settlement.on_timeout))
        .map_err(encoding_error)?;
    encode_optional_u64(encoder, settlement.challenge_window_ms)
}

fn decode_settlement(
    decoder: &mut Decoder<'_>,
) -> Result<IntentSettlement, SemanticAuthorityError> {
    if decoder.array().map_err(decode_error)? != Some(4) {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    Ok(IntentSettlement {
        mode: decode_settlement_mode(decoder.u8().map_err(decode_error)?)?,
        hard_criteria_digest: decode_optional_digest(decoder)?,
        on_timeout: decode_timeout_action(decoder.u8().map_err(decode_error)?)?,
        challenge_window_ms: decode_optional_u64(decoder)?,
    })
}

fn encode_extensions(
    encoder: &mut Encoder<Vec<u8>>,
    extensions: &[SpecExtension],
) -> Result<(), SemanticAuthorityError> {
    encoder
        .array(extensions.len() as u64)
        .map_err(encoding_error)?;
    for extension in extensions {
        encoder
            .array(2)
            .and_then(|e| e.u32(extension.id))
            .and_then(|e| e.bytes(&extension.value))
            .map_err(encoding_error)?;
    }
    Ok(())
}

fn decode_extensions(
    decoder: &mut Decoder<'_>,
) -> Result<Vec<SpecExtension>, SemanticAuthorityError> {
    let count = definite_len(decoder.array().map_err(decode_error)?, "extensions")?;
    if count > MAX_SPEC_EXTENSIONS {
        return Err(SemanticAuthorityError::InvalidSpecBody(
            "too many extensions",
        ));
    }
    let mut extensions = Vec::with_capacity(count);
    for _ in 0..count {
        if decoder.array().map_err(decode_error)? != Some(2) {
            return Err(SemanticAuthorityError::CanonicalMismatch);
        }
        extensions.push(SpecExtension {
            id: decoder.u32().map_err(decode_error)?,
            value: decoder.bytes().map_err(decode_error)?.to_vec(),
        });
    }
    validate_extensions(&extensions)?;
    Ok(extensions)
}

fn validate_extensions(extensions: &[SpecExtension]) -> Result<(), SemanticAuthorityError> {
    if extensions.len() > MAX_SPEC_EXTENSIONS
        || extensions
            .iter()
            .any(|extension| extension.value.len() > MAX_SPEC_EXTENSION_BYTES)
    {
        return Err(SemanticAuthorityError::InvalidSpecBody(
            "extension bound exceeded",
        ));
    }
    if extensions.windows(2).any(|pair| pair[0].id >= pair[1].id) {
        return Err(SemanticAuthorityError::InvalidSpecBody(
            "extensions must be sorted and unique",
        ));
    }
    Ok(())
}

fn validate_digest_set(
    values: &[[u8; 32]],
    name: &'static str,
) -> Result<(), SemanticAuthorityError> {
    if values.len() > MAX_SPEC_CAPABILITY_REFS || values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(SemanticAuthorityError::InvalidSpecBody(name));
    }
    Ok(())
}

fn encode_digest_array(
    encoder: &mut Encoder<Vec<u8>>,
    values: &[[u8; 32]],
) -> Result<(), SemanticAuthorityError> {
    encoder.array(values.len() as u64).map_err(encoding_error)?;
    for value in values {
        encoder.bytes(value).map_err(encoding_error)?;
    }
    Ok(())
}

fn decode_digest_array(decoder: &mut Decoder<'_>) -> Result<Vec<[u8; 32]>, SemanticAuthorityError> {
    let count = definite_len(decoder.array().map_err(decode_error)?, "capabilities")?;
    if count > MAX_SPEC_CAPABILITY_REFS {
        return Err(SemanticAuthorityError::InvalidSpecBody(
            "too many capability references",
        ));
    }
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(fixed_bytes(decoder, "capability digest")?);
    }
    validate_digest_set(&values, "capabilities must be sorted and unique")?;
    Ok(values)
}

fn criterion_id_from_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/criterion/v1");
    hasher.update(bytes);
    hasher.finalize().into()
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

fn expect_key(decoder: &mut Decoder<'_>, expected: u8) -> Result<(), SemanticAuthorityError> {
    if decoder.u8().map_err(decode_error)? != expected {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    Ok(())
}

fn definite_len(value: Option<u64>, field: &'static str) -> Result<usize, SemanticAuthorityError> {
    usize::try_from(value.ok_or(SemanticAuthorityError::CanonicalMismatch)?)
        .map_err(|_| SemanticAuthorityError::MalformedCanonical(field))
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

const fn effect_code(value: CriterionEffect) -> u8 {
    match value {
        CriterionEffect::Hard => 1,
        CriterionEffect::Soft => 2,
    }
}
fn decode_effect(value: u8) -> Result<CriterionEffect, SemanticAuthorityError> {
    match value {
        1 => Ok(CriterionEffect::Hard),
        2 => Ok(CriterionEffect::Soft),
        _ => Err(SemanticAuthorityError::InvalidSpecBody("criterion effect")),
    }
}
const fn evaluator_code(value: EvaluatorKind) -> u8 {
    match value {
        EvaluatorKind::Model => 1,
        EvaluatorKind::DeterministicTool => 2,
        EvaluatorKind::Human => 3,
    }
}
fn decode_evaluator(value: u8) -> Result<EvaluatorKind, SemanticAuthorityError> {
    match value {
        1 => Ok(EvaluatorKind::Model),
        2 => Ok(EvaluatorKind::DeterministicTool),
        3 => Ok(EvaluatorKind::Human),
        _ => Err(SemanticAuthorityError::InvalidSpecBody("evaluator kind")),
    }
}
const fn evaluator_reference_code(value: ImmutableEvaluatorReferenceKind) -> u8 {
    match value {
        ImmutableEvaluatorReferenceKind::Artifact => 1,
        ImmutableEvaluatorReferenceKind::AuthorityPolicy => 2,
    }
}
fn decode_evaluator_reference(
    value: u8,
) -> Result<ImmutableEvaluatorReferenceKind, SemanticAuthorityError> {
    match value {
        1 => Ok(ImmutableEvaluatorReferenceKind::Artifact),
        2 => Ok(ImmutableEvaluatorReferenceKind::AuthorityPolicy),
        _ => Err(SemanticAuthorityError::InvalidSpecBody(
            "evaluator reference kind",
        )),
    }
}
const fn criticality_code(value: IntentCriticality) -> u8 {
    match value {
        IntentCriticality::Low => 1,
        IntentCriticality::Standard => 2,
        IntentCriticality::High => 3,
        IntentCriticality::Critical => 4,
    }
}
fn decode_criticality(value: u8) -> Result<IntentCriticality, SemanticAuthorityError> {
    match value {
        1 => Ok(IntentCriticality::Low),
        2 => Ok(IntentCriticality::Standard),
        3 => Ok(IntentCriticality::High),
        4 => Ok(IntentCriticality::Critical),
        _ => Err(SemanticAuthorityError::InvalidSpecBody("criticality")),
    }
}
const fn settlement_mode_code(value: SettlementMode) -> u8 {
    match value {
        SettlementMode::None => 1,
        SettlementMode::Automatic => 2,
        SettlementMode::Manual => 3,
    }
}
fn decode_settlement_mode(value: u8) -> Result<SettlementMode, SemanticAuthorityError> {
    match value {
        1 => Ok(SettlementMode::None),
        2 => Ok(SettlementMode::Automatic),
        3 => Ok(SettlementMode::Manual),
        _ => Err(SemanticAuthorityError::InvalidSpecBody("settlement mode")),
    }
}
const fn timeout_action_code(value: SettlementTimeoutAction) -> u8 {
    match value {
        SettlementTimeoutAction::Refund => 1,
        SettlementTimeoutAction::Dispute => 2,
    }
}
fn decode_timeout_action(value: u8) -> Result<SettlementTimeoutAction, SemanticAuthorityError> {
    match value {
        1 => Ok(SettlementTimeoutAction::Refund),
        2 => Ok(SettlementTimeoutAction::Dispute),
        _ => Err(SemanticAuthorityError::InvalidSpecBody("timeout action")),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn encoding_error(
    error: minicbor::encode::Error<std::convert::Infallible>,
) -> SemanticAuthorityError {
    SemanticAuthorityError::CanonicalEncoding(error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn decode_error(error: minicbor::decode::Error) -> SemanticAuthorityError {
    SemanticAuthorityError::CanonicalDecoding(error.to_string())
}
