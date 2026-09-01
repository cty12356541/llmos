//! Canonical CBOR envelopes for the §17.2/§17.3/§17.4 typed Semantic events.
//!
//! Judgment, Verification, and Retraction reuse the 17-field deterministic
//! Assertion/Spec envelope; their complete payload lives inside field 15, so
//! the `EventId` covers every payload fact.

use minicbor::{Decoder, Encoder};
use nlos_capability::CapabilityTarget;
use nlos_types::{ControlDomainId, KeyId, PrincipalId, ProcessId, ReceiptId, SemanticEventId};

use crate::{
    CriterionVerificationTarget, EvaluatorKind, EventVerificationTarget,
    ImmutableEvaluatorReference, ImmutableEvaluatorReferenceKind, JudgmentRelation,
    LocalProcessRef, MAX_CANONICAL_EVENT_BYTES, MAX_LINEAGE_ITEMS, RetractionMode,
    SemanticAuthorityError, UnsignedJudgmentEvent, UnsignedRetractionEvent,
    UnsignedVerificationEvent, VerificationOutcome, VerificationTarget,
    canonical::{
        SCHEMA_MAJOR, SCHEMA_MINOR, SCHEMA_NAME, decode_error, decode_event_ids, decode_generation,
        decode_optional_digest, decode_optional_u16, decode_optional_u64, decode_scope,
        encode_optional_digest, encode_optional_u16, encode_optional_u64, encoding_error,
        expect_key, fixed_bytes, scope_bytes, scope_kind, validate_sorted_unique,
    },
};

pub(crate) const EVENT_TYPE_JUDGMENT: u8 = 2;
pub(crate) const EVENT_TYPE_VERIFICATION: u8 = 3;
pub(crate) const EVENT_TYPE_RETRACTION: u8 = 4;

const VERIFICATION_TARGET_KIND_EVENT: u8 = 1;
const VERIFICATION_TARGET_KIND_CRITERION: u8 = 2;

/// One decoded typed event with its envelope facts exposed for admission.
pub(crate) enum TypedEvent {
    Judgment(Box<UnsignedJudgmentEvent>),
    Verification(Box<UnsignedVerificationEvent>),
    Retraction(Box<UnsignedRetractionEvent>),
}

impl TypedEvent {
    /// Decodes any typed event by its field-3 event type discriminator.
    ///
    /// # Errors
    ///
    /// Rejects unknown event types and per-type canonical violations.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, SemanticAuthorityError> {
        let event_type = probe_event_type(bytes)?;
        match event_type {
            EVENT_TYPE_JUDGMENT => Ok(Self::Judgment(Box::new(decode_unsigned_judgment_event(
                bytes,
            )?))),
            EVENT_TYPE_VERIFICATION => Ok(Self::Verification(Box::new(
                decode_unsigned_verification_event(bytes)?,
            ))),
            EVENT_TYPE_RETRACTION => Ok(Self::Retraction(Box::new(
                decode_unsigned_retraction_event(bytes)?,
            ))),
            _ => Err(SemanticAuthorityError::UnsupportedEventType),
        }
    }

    pub(crate) fn declared_parents(&self) -> &[SemanticEventId] {
        match self {
            Self::Judgment(event) => &event.declared_parents,
            Self::Verification(event) => &event.declared_parents,
            Self::Retraction(event) => &event.declared_parents,
        }
    }

    pub(crate) const fn scope(&self) -> CapabilityTarget {
        match self {
            Self::Judgment(event) => event.scope,
            Self::Verification(event) => event.scope,
            Self::Retraction(event) => event.scope,
        }
    }

    pub(crate) const fn issuer(&self) -> PrincipalId {
        match self {
            Self::Judgment(event) => event.issuer,
            Self::Verification(event) => event.issuer,
            Self::Retraction(event) => event.issuer,
        }
    }

    pub(crate) const fn issuer_execution(&self) -> &LocalProcessRef {
        match self {
            Self::Judgment(event) => &event.issuer_execution,
            Self::Verification(event) => &event.issuer_execution,
            Self::Retraction(event) => &event.issuer_execution,
        }
    }

    pub(crate) const fn control_domain(&self) -> ControlDomainId {
        match self {
            Self::Judgment(event) => event.control_domain,
            Self::Verification(event) => event.control_domain,
            Self::Retraction(event) => event.control_domain,
        }
    }

    pub(crate) const fn issued_at_unix_ns(&self) -> u64 {
        match self {
            Self::Judgment(event) => event.issued_at_unix_ns,
            Self::Verification(event) => event.issued_at_unix_ns,
            Self::Retraction(event) => event.issued_at_unix_ns,
        }
    }

    pub(crate) const fn valid_until_ms(&self) -> Option<u64> {
        match self {
            Self::Judgment(event) => event.valid_until_ms,
            Self::Verification(event) => event.valid_until_ms,
            Self::Retraction(event) => event.valid_until_ms,
        }
    }

    pub(crate) const fn purpose_digest(&self) -> Option<[u8; 32]> {
        match self {
            Self::Judgment(event) => event.purpose_digest,
            Self::Verification(event) => event.purpose_digest,
            Self::Retraction(event) => event.purpose_digest,
        }
    }

    pub(crate) const fn key_id(&self) -> KeyId {
        match self {
            Self::Judgment(event) => event.key_id,
            Self::Verification(event) => event.key_id,
            Self::Retraction(event) => event.key_id,
        }
    }
}

/// Reads only the field-3 discriminator; full strict validation happens in
/// the per-type decode.
fn probe_event_type(bytes: &[u8]) -> Result<u8, SemanticAuthorityError> {
    let mut probe = Decoder::new(bytes);
    if probe.map().map_err(decode_error)? != Some(17) {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    for _ in 0..3 {
        probe.u8().map_err(decode_error)?;
        probe.skip().map_err(decode_error)?;
    }
    if probe.u8().map_err(decode_error)? != 3 {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    probe.u8().map_err(decode_error)
}

/// Encodes the §17.2 Judgment unsigned envelope as deterministic CBOR.
///
/// # Errors
///
/// Rejects invalid bounds, unnormalized symmetric endpoints, or encoding
/// failure.
pub fn encode_unsigned_judgment_event(
    event: &UnsignedJudgmentEvent,
) -> Result<Vec<u8>, SemanticAuthorityError> {
    validate_judgment(event)?;
    let mut encoder = Encoder::new(Vec::new());
    encode_envelope_header(
        &mut encoder,
        EVENT_TYPE_JUDGMENT,
        event.scope,
        event.issuer,
        &event.issuer_execution,
        event.control_domain,
        event.issued_at_unix_ns,
        &event.nonce,
        &event.declared_parents,
    )?;
    encode_envelope_tail(&mut encoder, event.valid_until_ms, event.purpose_digest)?;
    encoder
        .array(6)
        .and_then(|e| e.u8(event.relation.encode()))
        .and_then(|e| e.bytes(event.source.as_bytes()))
        .and_then(|e| e.bytes(event.target.as_bytes()))
        .map_err(encoding_error)?;
    encode_optional_digest(&mut encoder, event.context_digest)?;
    encoder
        .bytes(event.evaluator_evidence_receipt_id.as_bytes())
        .map_err(encoding_error)?;
    encode_optional_u16(&mut encoder, event.confidence_bp)?;
    finish_envelope(encoder, event.key_id)
}

/// Strictly decodes and re-encodes the §17.2 Judgment unsigned envelope.
///
/// # Errors
///
/// Rejects malformed, noncanonical, or semantically invalid event bytes.
pub fn decode_unsigned_judgment_event(
    bytes: &[u8],
) -> Result<UnsignedJudgmentEvent, SemanticAuthorityError> {
    if bytes.len() > MAX_CANONICAL_EVENT_BYTES {
        return Err(SemanticAuthorityError::CanonicalTooLarge);
    }
    let mut decoder = Decoder::new(bytes);
    let prefix = decode_envelope_prefix(&mut decoder, EVENT_TYPE_JUDGMENT)?;
    if decoder.array().map_err(decode_error)? != Some(6) {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    let relation = JudgmentRelation::decode(decoder.u8().map_err(decode_error)?)
        .ok_or(SemanticAuthorityError::InvalidJudgmentPayload("relation"))?;
    let source = SemanticEventId::from_bytes(fixed_bytes(&mut decoder, "judgment source")?);
    let target = SemanticEventId::from_bytes(fixed_bytes(&mut decoder, "judgment target")?);
    let context_digest = decode_optional_digest(&mut decoder)?;
    let evaluator_evidence_receipt_id =
        ReceiptId::from_bytes(fixed_bytes(&mut decoder, "judgment evidence receipt")?);
    let confidence_bp = decode_optional_u16(&mut decoder)?;
    let key_id = decode_envelope_suffix(&mut decoder, bytes)?;
    let event = UnsignedJudgmentEvent {
        scope: prefix.scope,
        issuer: prefix.issuer,
        issuer_execution: prefix.issuer_execution,
        control_domain: prefix.control_domain,
        issued_at_unix_ns: prefix.issued_at_unix_ns,
        nonce: prefix.nonce,
        declared_parents: prefix.declared_parents,
        valid_until_ms: prefix.valid_until_ms,
        purpose_digest: prefix.purpose_digest,
        key_id,
        relation,
        source,
        target,
        context_digest,
        evaluator_evidence_receipt_id,
        confidence_bp,
    };
    validate_judgment(&event)?;
    if encode_unsigned_judgment_event(&event)? != bytes {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    Ok(event)
}

/// Encodes the §17.3 Verification unsigned envelope as deterministic CBOR.
///
/// # Errors
///
/// Rejects invalid bounds, unsorted evidence/domain sets, or encoding failure.
pub fn encode_unsigned_verification_event(
    event: &UnsignedVerificationEvent,
) -> Result<Vec<u8>, SemanticAuthorityError> {
    validate_verification(event)?;
    let mut encoder = Encoder::new(Vec::new());
    encode_envelope_header(
        &mut encoder,
        EVENT_TYPE_VERIFICATION,
        event.scope,
        event.issuer,
        &event.issuer_execution,
        event.control_domain,
        event.issued_at_unix_ns,
        &event.nonce,
        &event.declared_parents,
    )?;
    encode_envelope_tail(&mut encoder, event.valid_until_ms, event.purpose_digest)?;
    encoder.array(6).map_err(encoding_error)?;
    encode_verification_target(&mut encoder, &event.target)?;
    encoder
        .u8(event.outcome.encode())
        .and_then(|e| e.u8(event.evaluator_kind.encode()))
        .and_then(|e| e.array(2))
        .and_then(|e| e.u8(event.procedure_ref.kind.encode()))
        .and_then(|e| e.bytes(&event.procedure_ref.digest))
        .and_then(|e| e.bytes(event.evaluator_evidence_receipt_id.as_bytes()))
        .and_then(|e| e.array(event.evidence.len() as u64))
        .map_err(encoding_error)?;
    for evidence in &event.evidence {
        encoder.bytes(evidence.as_bytes()).map_err(encoding_error)?;
    }
    finish_envelope(encoder, event.key_id)
}

/// Strictly decodes and re-encodes the §17.3 Verification unsigned envelope.
///
/// # Errors
///
/// Rejects malformed, noncanonical, or semantically invalid event bytes.
pub fn decode_unsigned_verification_event(
    bytes: &[u8],
) -> Result<UnsignedVerificationEvent, SemanticAuthorityError> {
    if bytes.len() > MAX_CANONICAL_EVENT_BYTES {
        return Err(SemanticAuthorityError::CanonicalTooLarge);
    }
    let mut decoder = Decoder::new(bytes);
    let prefix = decode_envelope_prefix(&mut decoder, EVENT_TYPE_VERIFICATION)?;
    if decoder.array().map_err(decode_error)? != Some(6) {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    let target = decode_verification_target(&mut decoder)?;
    let outcome = VerificationOutcome::decode(decoder.u8().map_err(decode_error)?).ok_or(
        SemanticAuthorityError::InvalidVerificationPayload("outcome"),
    )?;
    let evaluator_kind = EvaluatorKind::decode(decoder.u8().map_err(decode_error)?).ok_or(
        SemanticAuthorityError::InvalidVerificationPayload("evaluator kind"),
    )?;
    if decoder.array().map_err(decode_error)? != Some(2) {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    let procedure_ref = ImmutableEvaluatorReference {
        kind: decode_evaluator_reference(decoder.u8().map_err(decode_error)?)?,
        digest: fixed_bytes(&mut decoder, "procedure reference")?,
    };
    let evaluator_evidence_receipt_id =
        ReceiptId::from_bytes(fixed_bytes(&mut decoder, "verification evidence receipt")?);
    let evidence = decode_event_ids(&mut decoder)?;
    let key_id = decode_envelope_suffix(&mut decoder, bytes)?;
    let event = UnsignedVerificationEvent {
        scope: prefix.scope,
        issuer: prefix.issuer,
        issuer_execution: prefix.issuer_execution,
        control_domain: prefix.control_domain,
        issued_at_unix_ns: prefix.issued_at_unix_ns,
        nonce: prefix.nonce,
        declared_parents: prefix.declared_parents,
        valid_until_ms: prefix.valid_until_ms,
        purpose_digest: prefix.purpose_digest,
        key_id,
        target,
        outcome,
        evaluator_kind,
        procedure_ref,
        evaluator_evidence_receipt_id,
        evidence,
    };
    validate_verification(&event)?;
    if encode_unsigned_verification_event(&event)? != bytes {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    Ok(event)
}

/// Encodes the §17.4 Retraction unsigned envelope as deterministic CBOR.
///
/// # Errors
///
/// Rejects invalid bounds or encoding failure.
pub fn encode_unsigned_retraction_event(
    event: &UnsignedRetractionEvent,
) -> Result<Vec<u8>, SemanticAuthorityError> {
    validate_retraction(event)?;
    let mut encoder = Encoder::new(Vec::new());
    encode_envelope_header(
        &mut encoder,
        EVENT_TYPE_RETRACTION,
        event.scope,
        event.issuer,
        &event.issuer_execution,
        event.control_domain,
        event.issued_at_unix_ns,
        &event.nonce,
        &event.declared_parents,
    )?;
    encode_envelope_tail(&mut encoder, event.valid_until_ms, event.purpose_digest)?;
    encoder
        .array(4)
        .and_then(|e| e.bytes(event.target_event_id.as_bytes()))
        .and_then(|e| e.u8(event.mode.encode()))
        .map_err(encoding_error)?;
    encode_optional_digest(&mut encoder, event.reason_digest)?;
    encoder
        .bytes(event.authority_evidence_receipt_id.as_bytes())
        .map_err(encoding_error)?;
    finish_envelope(encoder, event.key_id)
}

/// Strictly decodes and re-encodes the §17.4 Retraction unsigned envelope.
///
/// # Errors
///
/// Rejects malformed, noncanonical, or semantically invalid event bytes.
pub fn decode_unsigned_retraction_event(
    bytes: &[u8],
) -> Result<UnsignedRetractionEvent, SemanticAuthorityError> {
    if bytes.len() > MAX_CANONICAL_EVENT_BYTES {
        return Err(SemanticAuthorityError::CanonicalTooLarge);
    }
    let mut decoder = Decoder::new(bytes);
    let prefix = decode_envelope_prefix(&mut decoder, EVENT_TYPE_RETRACTION)?;
    if decoder.array().map_err(decode_error)? != Some(4) {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    let target_event_id =
        SemanticEventId::from_bytes(fixed_bytes(&mut decoder, "retraction target")?);
    let mode = RetractionMode::decode(decoder.u8().map_err(decode_error)?)
        .ok_or(SemanticAuthorityError::InvalidRetractionPayload("mode"))?;
    let reason_digest = decode_optional_digest(&mut decoder)?;
    let authority_evidence_receipt_id =
        ReceiptId::from_bytes(fixed_bytes(&mut decoder, "retraction authority evidence")?);
    let key_id = decode_envelope_suffix(&mut decoder, bytes)?;
    let event = UnsignedRetractionEvent {
        scope: prefix.scope,
        issuer: prefix.issuer,
        issuer_execution: prefix.issuer_execution,
        control_domain: prefix.control_domain,
        issued_at_unix_ns: prefix.issued_at_unix_ns,
        nonce: prefix.nonce,
        declared_parents: prefix.declared_parents,
        valid_until_ms: prefix.valid_until_ms,
        purpose_digest: prefix.purpose_digest,
        key_id,
        target_event_id,
        mode,
        reason_digest,
        authority_evidence_receipt_id,
    };
    validate_retraction(&event)?;
    if encode_unsigned_retraction_event(&event)? != bytes {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    Ok(event)
}

fn validate_judgment(event: &UnsignedJudgmentEvent) -> Result<(), SemanticAuthorityError> {
    validate_envelope_common(&event.nonce, &event.declared_parents)?;
    if event.confidence_bp.is_some_and(|value| value > 10_000) {
        return Err(SemanticAuthorityError::InvalidJudgmentPayload(
            "confidence_bp bound",
        ));
    }
    // [SEM-JUDGE-003]: symmetric relations must encode source <= target by
    // EventId byte order so one judgment has exactly one canonical encoding.
    if event.relation.is_symmetric() && event.source > event.target {
        return Err(SemanticAuthorityError::InvalidJudgmentPayload(
            "symmetric endpoints are not normalized",
        ));
    }
    Ok(())
}

fn validate_verification(event: &UnsignedVerificationEvent) -> Result<(), SemanticAuthorityError> {
    validate_envelope_common(&event.nonce, &event.declared_parents)?;
    validate_sorted_unique(&event.evidence)
        .map_err(|_| SemanticAuthorityError::InvalidVerificationPayload("evidence ordering"))?;
    validate_domain_set(&event.target)?;
    Ok(())
}

fn validate_retraction(event: &UnsignedRetractionEvent) -> Result<(), SemanticAuthorityError> {
    validate_envelope_common(&event.nonce, &event.declared_parents)?;
    // [SEM-TTL-003]: a retraction must never expire through a TTL.
    if event.valid_until_ms.is_some() {
        return Err(SemanticAuthorityError::InvalidRetractionPayload(
            "retraction must not declare valid_until",
        ));
    }
    Ok(())
}

fn validate_envelope_common(
    nonce: &[u8],
    declared_parents: &[SemanticEventId],
) -> Result<(), SemanticAuthorityError> {
    if !(crate::MIN_NONCE_BYTES..=crate::MAX_NONCE_BYTES).contains(&nonce.len()) {
        return Err(SemanticAuthorityError::InvalidNonce);
    }
    validate_sorted_unique(declared_parents)?;
    Ok(())
}

fn validate_domain_set(target: &VerificationTarget) -> Result<(), SemanticAuthorityError> {
    let crate::VerificationTarget::Criterion(criterion) = target else {
        return Ok(());
    };
    if criterion.producer_control_domains.len() > MAX_LINEAGE_ITEMS {
        return Err(SemanticAuthorityError::InvalidVerificationPayload(
            "producer control domain bound",
        ));
    }
    if criterion
        .producer_control_domains
        .windows(2)
        .any(|pair| pair[0].as_bytes() >= pair[1].as_bytes())
    {
        return Err(SemanticAuthorityError::InvalidVerificationPayload(
            "producer control domains must be sorted and unique",
        ));
    }
    Ok(())
}

fn encode_verification_target(
    encoder: &mut Encoder<Vec<u8>>,
    target: &VerificationTarget,
) -> Result<(), SemanticAuthorityError> {
    match target {
        VerificationTarget::Event(event_target) => {
            encoder
                .array(2)
                .and_then(|e| e.u8(VERIFICATION_TARGET_KIND_EVENT))
                .and_then(|e| e.bytes(event_target.event_id.as_bytes()))
                .map_err(encoding_error)?;
        }
        VerificationTarget::Criterion(criterion) => {
            encoder
                .array(2)
                .and_then(|e| e.u8(VERIFICATION_TARGET_KIND_CRITERION))
                .and_then(|e| e.array(6))
                .and_then(|e| e.bytes(criterion.spec_id.as_bytes()))
                .and_then(|e| e.bytes(&criterion.criterion_id))
                .and_then(|e| e.bytes(&criterion.artifact_set_digest))
                .and_then(|e| e.bytes(&criterion.procedure_digest))
                .and_then(|e| e.bytes(&criterion.evaluation_id))
                .and_then(|e| e.array(criterion.producer_control_domains.len() as u64))
                .map_err(encoding_error)?;
            for domain in &criterion.producer_control_domains {
                encoder.bytes(domain.as_bytes()).map_err(encoding_error)?;
            }
        }
    }
    Ok(())
}

fn decode_verification_target(
    decoder: &mut Decoder<'_>,
) -> Result<VerificationTarget, SemanticAuthorityError> {
    if decoder.array().map_err(decode_error)? != Some(2) {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    match decoder.u8().map_err(decode_error)? {
        VERIFICATION_TARGET_KIND_EVENT => Ok(VerificationTarget::Event(EventVerificationTarget {
            event_id: SemanticEventId::from_bytes(fixed_bytes(decoder, "verification target")?),
        })),
        VERIFICATION_TARGET_KIND_CRITERION => {
            if decoder.array().map_err(decode_error)? != Some(6) {
                return Err(SemanticAuthorityError::CanonicalMismatch);
            }
            Ok(VerificationTarget::Criterion(CriterionVerificationTarget {
                spec_id: SemanticEventId::from_bytes(fixed_bytes(decoder, "spec id")?),
                criterion_id: fixed_bytes(decoder, "criterion id")?,
                artifact_set_digest: fixed_bytes(decoder, "artifact set digest")?,
                procedure_digest: fixed_bytes(decoder, "procedure digest")?,
                evaluation_id: fixed_bytes(decoder, "evaluation id")?,
                producer_control_domains: decode_domain_ids(decoder)?,
            }))
        }
        _ => Err(SemanticAuthorityError::InvalidVerificationPayload(
            "target branch",
        )),
    }
}

fn decode_domain_ids(
    decoder: &mut Decoder<'_>,
) -> Result<Vec<ControlDomainId>, SemanticAuthorityError> {
    let count = decoder
        .array()
        .map_err(decode_error)?
        .ok_or(SemanticAuthorityError::CanonicalMismatch)?;
    let count = usize::try_from(count).map_err(|_| {
        SemanticAuthorityError::InvalidVerificationPayload("producer control domain bound")
    })?;
    if count > MAX_LINEAGE_ITEMS {
        return Err(SemanticAuthorityError::InvalidVerificationPayload(
            "producer control domain bound",
        ));
    }
    let mut domains = Vec::with_capacity(count);
    for _ in 0..count {
        domains.push(ControlDomainId::from_bytes(fixed_bytes(
            decoder,
            "producer control domain",
        )?));
    }
    Ok(domains)
}

const fn decode_evaluator_reference(
    value: u8,
) -> Result<ImmutableEvaluatorReferenceKind, SemanticAuthorityError> {
    match value {
        1 => Ok(ImmutableEvaluatorReferenceKind::Artifact),
        2 => Ok(ImmutableEvaluatorReferenceKind::AuthorityPolicy),
        _ => Err(SemanticAuthorityError::InvalidVerificationPayload(
            "procedure reference kind",
        )),
    }
}

#[allow(clippy::too_many_arguments)] // The shared envelope field order is fixed.
#[allow(clippy::too_many_lines)] // One linear pass over the 17 fixed envelope fields.
fn encode_envelope_header(
    encoder: &mut Encoder<Vec<u8>>,
    event_type: u8,
    scope: CapabilityTarget,
    issuer: PrincipalId,
    issuer_execution: &LocalProcessRef,
    control_domain: ControlDomainId,
    issued_at_unix_ns: u64,
    nonce: &[u8],
    declared_parents: &[SemanticEventId],
) -> Result<(), SemanticAuthorityError> {
    encoder
        .map(17)
        .and_then(|e| e.u8(0))
        .and_then(|e| e.str(SCHEMA_NAME))
        .and_then(|e| e.u8(1))
        .and_then(|e| e.u32(SCHEMA_MAJOR))
        .and_then(|e| e.u8(2))
        .and_then(|e| e.u32(SCHEMA_MINOR))
        .and_then(|e| e.u8(3))
        .and_then(|e| e.u8(event_type))
        .and_then(|e| e.u8(4))
        .and_then(|e| e.u8(scope_kind(scope)))
        .and_then(|e| e.u8(5))
        .and_then(|e| e.bytes(&scope_bytes(scope)))
        .and_then(|e| e.u8(6))
        .and_then(|e| e.bytes(issuer.as_bytes()))
        .and_then(|e| e.u8(7))
        .and_then(|e| e.array(3))
        .and_then(|e| e.u8(1))
        .and_then(|e| e.bytes(issuer_execution.process_id.as_bytes()))
        .and_then(|e| e.u64(issuer_execution.generation.get()))
        .and_then(|e| e.u8(8))
        .and_then(|e| e.bytes(control_domain.as_bytes()))
        .and_then(|e| e.u8(9))
        .and_then(|e| e.u64(issued_at_unix_ns))
        .and_then(|e| e.u8(10))
        .and_then(|e| e.bytes(nonce))
        .and_then(|e| e.u8(11))
        .and_then(|e| e.array(declared_parents.len() as u64))
        .map_err(encoding_error)?;
    for parent in declared_parents {
        encoder.bytes(parent.as_bytes()).map_err(encoding_error)?;
    }
    encoder
        .u8(12)
        .and_then(|e| e.null())
        .map_err(encoding_error)?;
    Ok(())
}

fn encode_envelope_tail(
    encoder: &mut Encoder<Vec<u8>>,
    valid_until_ms: Option<u64>,
    purpose_digest: Option<[u8; 32]>,
) -> Result<(), SemanticAuthorityError> {
    encoder.u8(13).map_err(encoding_error)?;
    encode_optional_u64(encoder, valid_until_ms)?;
    encoder.u8(14).map_err(encoding_error)?;
    encode_optional_digest(encoder, purpose_digest)?;
    encoder.u8(15).map_err(encoding_error)?;
    Ok(())
}

fn finish_envelope(
    mut encoder: Encoder<Vec<u8>>,
    key_id: KeyId,
) -> Result<Vec<u8>, SemanticAuthorityError> {
    encoder
        .u8(16)
        .and_then(|e| e.bytes(key_id.as_bytes()))
        .map_err(encoding_error)?;
    let bytes = encoder.into_writer();
    if bytes.len() > MAX_CANONICAL_EVENT_BYTES {
        return Err(SemanticAuthorityError::CanonicalTooLarge);
    }
    Ok(bytes)
}

struct DecodedEnvelopePrefix {
    scope: CapabilityTarget,
    issuer: PrincipalId,
    issuer_execution: LocalProcessRef,
    control_domain: ControlDomainId,
    issued_at_unix_ns: u64,
    nonce: Vec<u8>,
    declared_parents: Vec<SemanticEventId>,
    valid_until_ms: Option<u64>,
    purpose_digest: Option<[u8; 32]>,
}

/// Consumes envelope fields 0..=15 (including the payload key) and leaves the
/// decoder at the start of the field-15 payload value.
#[allow(clippy::too_many_lines)] // One linear pass over the fixed envelope fields.
fn decode_envelope_prefix(
    decoder: &mut Decoder<'_>,
    expected_event_type: u8,
) -> Result<DecodedEnvelopePrefix, SemanticAuthorityError> {
    if decoder.map().map_err(decode_error)? != Some(17) {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    expect_key(decoder, 0)?;
    if decoder.str().map_err(decode_error)? != SCHEMA_NAME {
        return Err(SemanticAuthorityError::UnsupportedSchema);
    }
    expect_key(decoder, 1)?;
    if decoder.u32().map_err(decode_error)? != SCHEMA_MAJOR {
        return Err(SemanticAuthorityError::UnsupportedSchema);
    }
    expect_key(decoder, 2)?;
    if decoder.u32().map_err(decode_error)? != SCHEMA_MINOR {
        return Err(SemanticAuthorityError::UnsupportedSchema);
    }
    expect_key(decoder, 3)?;
    if decoder.u8().map_err(decode_error)? != expected_event_type {
        return Err(SemanticAuthorityError::UnsupportedEventType);
    }
    expect_key(decoder, 4)?;
    let scope_kind = decoder.u8().map_err(decode_error)?;
    expect_key(decoder, 5)?;
    let scope = decode_scope(scope_kind, fixed_bytes(decoder, "scope id")?)?;
    expect_key(decoder, 6)?;
    let issuer = PrincipalId::from_bytes(fixed_bytes(decoder, "issuer")?);
    expect_key(decoder, 7)?;
    if decoder.array().map_err(decode_error)? != Some(3) || decoder.u8().map_err(decode_error)? != 1
    {
        return Err(SemanticAuthorityError::InvalidIssuerExecution);
    }
    let issuer_execution = LocalProcessRef {
        process_id: ProcessId::from_bytes(fixed_bytes(decoder, "process id")?),
        generation: decode_generation(decoder.u64().map_err(decode_error)?)?,
    };
    expect_key(decoder, 8)?;
    let control_domain = ControlDomainId::from_bytes(fixed_bytes(decoder, "domain")?);
    expect_key(decoder, 9)?;
    let issued_at_unix_ns = decoder.u64().map_err(decode_error)?;
    expect_key(decoder, 10)?;
    let nonce = decoder.bytes().map_err(decode_error)?.to_vec();
    expect_key(decoder, 11)?;
    let declared_parents = decode_event_ids(decoder)?;
    expect_key(decoder, 12)?;
    decoder.null().map_err(decode_error)?;
    expect_key(decoder, 13)?;
    let valid_until_ms = decode_optional_u64(decoder)?;
    expect_key(decoder, 14)?;
    let purpose_digest = decode_optional_digest(decoder)?;
    expect_key(decoder, 15)?;
    Ok(DecodedEnvelopePrefix {
        scope,
        issuer,
        issuer_execution,
        control_domain,
        issued_at_unix_ns,
        nonce,
        declared_parents,
        valid_until_ms,
        purpose_digest,
    })
}

/// Consumes the trailing field-16 key id and rejects trailing bytes.
fn decode_envelope_suffix(
    decoder: &mut Decoder<'_>,
    bytes: &[u8],
) -> Result<KeyId, SemanticAuthorityError> {
    expect_key(decoder, 16)?;
    let key_id = KeyId::from_bytes(fixed_bytes(decoder, "key id")?);
    if decoder.position() != bytes.len() {
        return Err(SemanticAuthorityError::CanonicalMismatch);
    }
    Ok(key_id)
}
