//! Append-only local Semantic assertion admission authority.
//!
//! This slice validates deterministic CBOR/EventId, issuer signature and
//! execution fence, Capability scope/right, lineage, content digest, and a
//! signed durable `AdmissionReceipt` in one `SQLite` transaction.

mod canonical;
mod model;
mod schema;
mod spec;

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use minicbor::Encoder;
use nlos_capability::{
    AuthorizeSemanticRequest, CapabilityAuthority, CapabilityAuthorityError, CapabilityRights,
    CapabilityTarget,
};
use nlos_identity::{
    IdentityAuthority, IdentityAuthorityError, VerifySemanticAuthoritySignatureRequest,
    VerifySemanticSignatureRequest,
};
use nlos_process::{ProcessAuthority, ProcessAuthorityError};
use nlos_types::{ReceiptId, SemanticEventId};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

pub use canonical::{
    decode_unsigned_assertion_event, decode_unsigned_spec_event, encode_unsigned_assertion_event,
    encode_unsigned_spec_event, semantic_event_id,
};
pub use model::{
    AcknowledgeOutboxRequest, AdmissionDurability, AdmissionReceipt, AppendAssertionRequest,
    AppendDecision, AppendSpecRequest, AssertionMode, CriterionAggregation, CriterionEffect,
    DurabilityReceipt, EvaluatorKind, ImmutableEvaluatorReference, ImmutableEvaluatorReferenceKind,
    IntentConstraints, IntentCriterion, IntentCriticality, IntentSettlement, IntentSpecBody,
    LocalProcessRef, MAX_CANONICAL_EVENT_BYTES, MAX_CONTENT_BYTES, MAX_LINEAGE_ITEMS,
    MAX_NONCE_BYTES, MAX_SPEC_CAPABILITY_REFS, MAX_SPEC_CRITERIA, MAX_SPEC_EXTENSION_BYTES,
    MAX_SPEC_EXTENSIONS, MIN_NONCE_BYTES, OutboxAckDecision, SemanticAdmissionEndpointProof,
    SemanticEventRecord, SemanticOutboxRecord, SemanticPayloadIdentity, SettlementMode,
    SettlementTimeoutAction, SpecExtension, StoreSigner, StoreSignerError, TaintFlags,
    UnsignedAssertionEvent, UnsignedSpecEvent,
};
pub use spec::{
    criterion_id, decode_intent_spec_body, encode_intent_spec_body, hard_criteria_digest,
    intent_spec_body_digest,
};

const SCHEMA_VERSION: i64 = 3;
const EDGE_DECLARED: i64 = 1;
const EDGE_CAPTURED: i64 = 2;

#[derive(Debug)]
pub enum SemanticAuthorityError {
    Sqlite(rusqlite::Error),
    Identity(IdentityAuthorityError),
    Capability(CapabilityAuthorityError),
    Process(ProcessAuthorityError),
    Io(std::io::Error),
    DurabilityUnavailable {
        journal_mode: String,
        synchronous: i64,
    },
    SchemaVersionUnsupported(i64),
    CanonicalTooLarge,
    CanonicalMismatch,
    CanonicalEncoding(String),
    CanonicalDecoding(String),
    MalformedCanonical(&'static str),
    UnsupportedSchema,
    UnsupportedEventType,
    InvalidNonce,
    InvalidTarget,
    InvalidIssuerExecution,
    InvalidAssertionPayload,
    InvalidSpecBody(&'static str),
    UnsupportedCriticalSpecExtension,
    SpecBodyDigestMismatch,
    SpecBodyDigestCollision,
    MissingExecutionEvidence,
    EventIdMismatch,
    EventIdCollision,
    EventReplayConflict,
    ContentTooLarge,
    InvalidMediaType,
    ContentDigestMismatch,
    ContentDigestCollision,
    InvalidLineage,
    DanglingLineage(SemanticEventId),
    EventExpired,
    StoreSigningFailed(String),
    StoreSignerBindingMismatch,
    EventNotFound(SemanticEventId),
    OutboxAckBindingMismatch,
    OutboxAckNotMonotonic {
        previous: u64,
        reported: u64,
    },
    OutboxAckBeforeAdmission,
    CorruptRecord(&'static str),
    LockPoisoned,
}

impl fmt::Display for SemanticAuthorityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite semantic authority failure: {error}"),
            Self::Identity(error) => write!(formatter, "identity validation failed: {error}"),
            Self::Capability(error) => write!(formatter, "capability validation failed: {error}"),
            Self::Process(error) => {
                write!(formatter, "issuer execution validation failed: {error}")
            }
            Self::Io(error) => write!(formatter, "semantic authority I/O failure: {error}"),
            Self::DurabilityUnavailable {
                journal_mode,
                synchronous,
            } => write!(
                formatter,
                "WAL/FULL durability unavailable: journal_mode={journal_mode}, synchronous={synchronous}"
            ),
            Self::SchemaVersionUnsupported(version) => write!(
                formatter,
                "unsupported semantic authority schema version {version}"
            ),
            Self::CanonicalTooLarge => formatter.write_str("canonical event exceeds bound"),
            Self::CanonicalMismatch => formatter.write_str("event bytes are not canonical CBOR"),
            Self::CanonicalEncoding(error) => {
                write!(formatter, "canonical encoding failed: {error}")
            }
            Self::CanonicalDecoding(error) => {
                write!(formatter, "canonical decoding failed: {error}")
            }
            Self::MalformedCanonical(field) => write!(formatter, "malformed canonical {field}"),
            Self::UnsupportedSchema => formatter.write_str("unsupported semantic schema"),
            Self::UnsupportedEventType => formatter.write_str("unsupported semantic event type"),
            Self::InvalidNonce => formatter.write_str("event nonce must contain 16..=32 bytes"),
            Self::InvalidTarget => formatter.write_str("invalid semantic target"),
            Self::InvalidIssuerExecution => {
                formatter.write_str("invalid or stale issuer execution")
            }
            Self::InvalidAssertionPayload => formatter.write_str("invalid assertion payload"),
            Self::InvalidSpecBody(reason) => write!(formatter, "invalid IntentSpec body: {reason}"),
            Self::UnsupportedCriticalSpecExtension => {
                formatter.write_str("unsupported critical IntentSpec extension")
            }
            Self::SpecBodyDigestMismatch => {
                formatter.write_str("SpecBodyDigest does not match canonical IntentSpec body")
            }
            Self::SpecBodyDigestCollision => {
                formatter.write_str("SpecBodyDigest is bound to different canonical bytes")
            }
            Self::MissingExecutionEvidence => {
                formatter.write_str("FACT_FROM_TOOL requires execution evidence")
            }
            Self::EventIdMismatch => {
                formatter.write_str("claimed EventId does not match canonical bytes")
            }
            Self::EventIdCollision => {
                formatter.write_str("EventId is bound to different canonical bytes")
            }
            Self::EventReplayConflict => {
                formatter.write_str("event replay bytes do not match committed event")
            }
            Self::ContentTooLarge => formatter.write_str("content exceeds bound"),
            Self::InvalidMediaType => formatter.write_str("invalid content media type"),
            Self::ContentDigestMismatch => {
                formatter.write_str("content digest does not match event payload")
            }
            Self::ContentDigestCollision => {
                formatter.write_str("content digest is bound to different bytes")
            }
            Self::InvalidLineage => {
                formatter.write_str("lineage must be bounded, sorted, and unique")
            }
            Self::DanglingLineage(id) => write!(formatter, "lineage event {id:?} is not committed"),
            Self::EventExpired => formatter.write_str("event is expired at admission"),
            Self::StoreSigningFailed(error) => write!(formatter, "store signing failed: {error}"),
            Self::StoreSignerBindingMismatch => {
                formatter.write_str("store signer identity does not match verified key binding")
            }
            Self::EventNotFound(id) => write!(formatter, "semantic event {id:?} does not exist"),
            Self::OutboxAckBindingMismatch => {
                formatter.write_str("outbox acknowledgement binding does not match owner record")
            }
            Self::OutboxAckNotMonotonic { previous, reported } => write!(
                formatter,
                "outbox acknowledgement timestamp regressed: previous={previous}, reported={reported}"
            ),
            Self::OutboxAckBeforeAdmission => {
                formatter.write_str("outbox acknowledgement precedes admission")
            }
            Self::CorruptRecord(reason) => write!(formatter, "corrupt durable record: {reason}"),
            Self::LockPoisoned => formatter.write_str("semantic authority lock is poisoned"),
        }
    }
}

impl Error for SemanticAuthorityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Identity(error) => Some(error),
            Self::Capability(error) => Some(error),
            Self::Process(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for SemanticAuthorityError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<IdentityAuthorityError> for SemanticAuthorityError {
    fn from(error: IdentityAuthorityError) -> Self {
        Self::Identity(error)
    }
}

impl From<CapabilityAuthorityError> for SemanticAuthorityError {
    fn from(error: CapabilityAuthorityError) -> Self {
        Self::Capability(error)
    }
}

impl From<ProcessAuthorityError> for SemanticAuthorityError {
    fn from(error: ProcessAuthorityError) -> Self {
        Self::Process(error)
    }
}

pub struct SemanticAuthority {
    connection: Mutex<Connection>,
}

impl SemanticAuthority {
    /// Opens `<root>/semantic-authority.db` with WAL/FULL durability.
    ///
    /// # Errors
    ///
    /// Fails when storage, durability configuration, or schema validation
    /// cannot be established.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, SemanticAuthorityError> {
        std::fs::create_dir_all(root.as_ref()).map_err(SemanticAuthorityError::Io)?;
        let mut connection = Connection::open(root.as_ref().join("semantic-authority.db"))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let journal_mode: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let synchronous: i64 =
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 {
            return Err(SemanticAuthorityError::DurabilityUnavailable {
                journal_mode,
                synchronous,
            });
        }
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => {
                schema::migrate_v2(&mut connection)?;
                schema::migrate_v3(&mut connection)?;
            }
            1 => {
                schema::migrate_v1_to_v2(&mut connection)?;
                schema::migrate_v3(&mut connection)?;
            }
            2 => schema::migrate_v3(&mut connection)?,
            SCHEMA_VERSION => {}
            other => return Err(SemanticAuthorityError::SchemaVersionUnsupported(other)),
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Atomically admits one canonical Assertion event after validating all
    /// available Identity, Process, Capability, content, and lineage facts.
    ///
    /// # Errors
    ///
    /// Returns typed fail-closed errors for canonical/identity/authz/lineage
    /// failures or when no durable transaction and signed Receipt can be
    /// committed.
    #[allow(clippy::too_many_arguments)] // Cross-authority references are explicit admission inputs.
    #[allow(clippy::too_many_lines)] // Keeps the six admission gates in their transaction order.
    pub fn append_assertion(
        &self,
        identity: &IdentityAuthority,
        capability: &CapabilityAuthority,
        process: &ProcessAuthority,
        store_signer: &impl StoreSigner,
        request: &AppendAssertionRequest,
    ) -> Result<AppendDecision, SemanticAuthorityError> {
        validate_append_request(request)?;
        let event = decode_unsigned_assertion_event(&request.canonical_unsigned_event)?;
        let computed_event_id = semantic_event_id(&request.canonical_unsigned_event);
        if computed_event_id != request.claimed_event_id {
            return Err(SemanticAuthorityError::EventIdMismatch);
        }
        if content_digest(&request.content_media_type, &request.content_bytes)?
            != event.content_digest
        {
            return Err(SemanticAuthorityError::ContentDigestMismatch);
        }

        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(replay) = load_replay(&transaction, request)? {
            transaction.commit()?;
            return Ok(AppendDecision::Replayed(replay));
        }

        let signer = identity.verify_semantic_signature(VerifySemanticSignatureRequest {
            event_id: request.claimed_event_id,
            issuer: event.issuer,
            control_domain_id: event.control_domain,
            key_id: event.key_id,
            signature: request.signature,
            admitted_at_ms: request.admitted_at_ms,
        })?;
        let execution =
            process.inspect_active_process_binding(event.issuer_execution.process_id)?;
        if execution.process_generation != event.issuer_execution.generation {
            return Err(SemanticAuthorityError::InvalidIssuerExecution);
        }
        capability.authorize_semantic(AuthorizeSemanticRequest {
            handle: request.capability,
            signer,
            target: event.scope,
            required_right: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: event.purpose_digest,
            admitted_at_ms: request.admitted_at_ms,
        })?;
        let capability_record =
            capability.inspect_active(request.capability, request.admitted_at_ms)?;
        let key_binding = identity.inspect_current_binding(event.key_id)?;
        let effective_valid_until_ms = effective_valid_until(
            event.valid_until_ms,
            key_binding.key_valid_until_ms,
            capability_record.valid_until_ms,
            request.admission_limit_ms,
        );
        if effective_valid_until_ms < request.admitted_at_ms {
            return Err(SemanticAuthorityError::EventExpired);
        }

        validate_lineage(
            &transaction,
            request.claimed_event_id,
            &event.declared_parents,
            &request.captured_inputs,
        )?;
        let effective_taint = derive_effective_taint(
            &transaction,
            request.ingress_taint,
            &event.declared_parents,
            &request.captured_inputs,
        )?;
        insert_or_validate_content(
            &transaction,
            event.content_digest,
            &request.content_media_type,
            &request.content_bytes,
        )?;
        insert_event(&transaction, request, &event)?;
        transaction.execute(
            "INSERT INTO event_signatures (event_id, key_id, signature) VALUES (?1, ?2, ?3)",
            params![
                request.claimed_event_id.as_bytes().as_slice(),
                event.key_id.as_bytes().as_slice(),
                request.signature.as_slice(),
            ],
        )?;
        transaction.execute(
            "INSERT INTO event_log (event_id) VALUES (?1)",
            [request.claimed_event_id.as_bytes().as_slice()],
        )?;
        let log_seq = u64::try_from(transaction.last_insert_rowid())
            .map_err(|_| SemanticAuthorityError::CorruptRecord("negative log sequence"))?;
        insert_lineage_edges(
            &transaction,
            request.claimed_event_id,
            &event.declared_parents,
            EDGE_DECLARED,
        )?;
        insert_lineage_edges(
            &transaction,
            request.claimed_event_id,
            &request.captured_inputs,
            EDGE_CAPTURED,
        )?;

        let receipt_core_digest = build_admission_receipt_core_digest(
            request.claimed_event_id,
            log_seq,
            request.admitted_at_ms,
            Some(effective_valid_until_ms),
            &request.captured_inputs,
            effective_taint,
            request.authz_policy_digest,
            store_signer.principal_id(),
            store_signer.control_domain_id(),
            store_signer.key_id(),
        );
        let mut receipt_id_bytes = [0_u8; 16];
        receipt_id_bytes.copy_from_slice(&receipt_core_digest[..16]);
        let receipt_id = ReceiptId::from_bytes(receipt_id_bytes);
        let receipt_message = admission_receipt_signature_message(receipt_id, receipt_core_digest);
        let store_signature = store_signer.sign(&receipt_message).map_err(|error| {
            SemanticAuthorityError::StoreSigningFailed(error.message().to_owned())
        })?;
        let verified_store = identity.verify_semantic_authority_signature(
            VerifySemanticAuthoritySignatureRequest {
                message_digest: receipt_message,
                issuer: store_signer.principal_id(),
                control_domain_id: store_signer.control_domain_id(),
                key_id: store_signer.key_id(),
                signature: store_signature,
                verified_at_ms: request.admitted_at_ms,
            },
        )?;
        if verified_store.principal_id() != store_signer.principal_id()
            || verified_store.control_domain_id() != store_signer.control_domain_id()
            || verified_store.key_id() != store_signer.key_id()
        {
            return Err(SemanticAuthorityError::StoreSignerBindingMismatch);
        }
        let receipt = AdmissionReceipt {
            receipt_id,
            event_id: request.claimed_event_id,
            log_seq,
            admitted_at_ms: request.admitted_at_ms,
            effective_valid_until_ms: Some(effective_valid_until_ms),
            captured_inputs: request.captured_inputs.clone(),
            effective_taint,
            authz_policy_digest: request.authz_policy_digest,
            durability: AdmissionDurability::Durable,
            store_principal: store_signer.principal_id(),
            store_control_domain: store_signer.control_domain_id(),
            store_key_id: store_signer.key_id(),
            store_signature,
        };
        insert_admission_receipt(&transaction, &receipt)?;
        transaction.execute(
            "INSERT INTO semantic_outbox (log_seq, event_id, receipt_id, acknowledged_at_ms)
             VALUES (?1, ?2, ?3, NULL)",
            params![
                encode_u64(log_seq)?,
                receipt.event_id.as_bytes().as_slice(),
                receipt.receipt_id.as_bytes().as_slice(),
            ],
        )?;
        transaction.commit()?;
        Ok(AppendDecision::Admitted(receipt))
    }

    /// Atomically admits one canonical SPEC event containing a complete
    /// canonical `IntentSpecBody`.
    ///
    /// # Errors
    ///
    /// Returns typed fail-closed errors for body/canonical/identity/authz/
    /// lineage failures or when the signed durable Receipt cannot commit.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_lines)]
    pub fn append_spec(
        &self,
        identity: &IdentityAuthority,
        capability: &CapabilityAuthority,
        process: &ProcessAuthority,
        store_signer: &impl StoreSigner,
        request: &AppendSpecRequest,
    ) -> Result<AppendDecision, SemanticAuthorityError> {
        canonical::validate_sorted_unique(&request.captured_inputs)?;
        let event = decode_unsigned_spec_event(&request.canonical_unsigned_event)?;
        let computed_event_id = semantic_event_id(&request.canonical_unsigned_event);
        if computed_event_id != request.claimed_event_id {
            return Err(SemanticAuthorityError::EventIdMismatch);
        }

        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(replay) = load_spec_replay(&transaction, request)? {
            transaction.commit()?;
            return Ok(AppendDecision::Replayed(replay));
        }

        let signer = identity.verify_semantic_signature(VerifySemanticSignatureRequest {
            event_id: request.claimed_event_id,
            issuer: event.issuer,
            control_domain_id: event.control_domain,
            key_id: event.key_id,
            signature: request.signature,
            admitted_at_ms: request.admitted_at_ms,
        })?;
        let execution =
            process.inspect_active_process_binding(event.issuer_execution.process_id)?;
        if execution.process_generation != event.issuer_execution.generation {
            return Err(SemanticAuthorityError::InvalidIssuerExecution);
        }
        capability.authorize_semantic(AuthorizeSemanticRequest {
            handle: request.capability,
            signer,
            target: event.scope,
            required_right: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: event.purpose_digest,
            admitted_at_ms: request.admitted_at_ms,
        })?;
        let capability_record =
            capability.inspect_active(request.capability, request.admitted_at_ms)?;
        let key_binding = identity.inspect_current_binding(event.key_id)?;
        let effective_valid_until_ms = effective_valid_until(
            event.valid_until_ms,
            key_binding.key_valid_until_ms,
            capability_record.valid_until_ms,
            request.admission_limit_ms,
        );
        if effective_valid_until_ms < request.admitted_at_ms {
            return Err(SemanticAuthorityError::EventExpired);
        }

        validate_lineage(
            &transaction,
            request.claimed_event_id,
            &event.declared_parents,
            &request.captured_inputs,
        )?;
        let effective_taint = derive_effective_taint(
            &transaction,
            request.ingress_taint,
            &event.declared_parents,
            &request.captured_inputs,
        )?;
        insert_or_validate_spec_body(
            &transaction,
            event.spec_body_digest,
            &event.canonical_spec_body,
        )?;
        insert_spec_event(&transaction, request, &event)?;
        transaction.execute(
            "INSERT INTO event_signatures (event_id, key_id, signature) VALUES (?1, ?2, ?3)",
            params![
                request.claimed_event_id.as_bytes().as_slice(),
                event.key_id.as_bytes().as_slice(),
                request.signature.as_slice(),
            ],
        )?;
        transaction.execute(
            "INSERT INTO event_log (event_id) VALUES (?1)",
            [request.claimed_event_id.as_bytes().as_slice()],
        )?;
        let log_seq = u64::try_from(transaction.last_insert_rowid())
            .map_err(|_| SemanticAuthorityError::CorruptRecord("negative log sequence"))?;
        insert_lineage_edges(
            &transaction,
            request.claimed_event_id,
            &event.declared_parents,
            EDGE_DECLARED,
        )?;
        insert_lineage_edges(
            &transaction,
            request.claimed_event_id,
            &request.captured_inputs,
            EDGE_CAPTURED,
        )?;

        let receipt_core_digest = build_admission_receipt_core_digest(
            request.claimed_event_id,
            log_seq,
            request.admitted_at_ms,
            Some(effective_valid_until_ms),
            &request.captured_inputs,
            effective_taint,
            request.authz_policy_digest,
            store_signer.principal_id(),
            store_signer.control_domain_id(),
            store_signer.key_id(),
        );
        let mut receipt_id_bytes = [0_u8; 16];
        receipt_id_bytes.copy_from_slice(&receipt_core_digest[..16]);
        let receipt_id = ReceiptId::from_bytes(receipt_id_bytes);
        let receipt_message = admission_receipt_signature_message(receipt_id, receipt_core_digest);
        let store_signature = store_signer.sign(&receipt_message).map_err(|error| {
            SemanticAuthorityError::StoreSigningFailed(error.message().to_owned())
        })?;
        let verified_store = identity.verify_semantic_authority_signature(
            VerifySemanticAuthoritySignatureRequest {
                message_digest: receipt_message,
                issuer: store_signer.principal_id(),
                control_domain_id: store_signer.control_domain_id(),
                key_id: store_signer.key_id(),
                signature: store_signature,
                verified_at_ms: request.admitted_at_ms,
            },
        )?;
        if verified_store.principal_id() != store_signer.principal_id()
            || verified_store.control_domain_id() != store_signer.control_domain_id()
            || verified_store.key_id() != store_signer.key_id()
        {
            return Err(SemanticAuthorityError::StoreSignerBindingMismatch);
        }
        let receipt = AdmissionReceipt {
            receipt_id,
            event_id: request.claimed_event_id,
            log_seq,
            admitted_at_ms: request.admitted_at_ms,
            effective_valid_until_ms: Some(effective_valid_until_ms),
            captured_inputs: request.captured_inputs.clone(),
            effective_taint,
            authz_policy_digest: request.authz_policy_digest,
            durability: AdmissionDurability::Durable,
            store_principal: store_signer.principal_id(),
            store_control_domain: store_signer.control_domain_id(),
            store_key_id: store_signer.key_id(),
            store_signature,
        };
        insert_admission_receipt(&transaction, &receipt)?;
        transaction.execute(
            "INSERT INTO semantic_outbox (log_seq, event_id, receipt_id, acknowledged_at_ms)
             VALUES (?1, ?2, ?3, NULL)",
            params![
                encode_u64(log_seq)?,
                receipt.event_id.as_bytes().as_slice(),
                receipt.receipt_id.as_bytes().as_slice(),
            ],
        )?;
        transaction.commit()?;
        Ok(AppendDecision::Admitted(receipt))
    }

    /// Reads a committed immutable event and its authoritative log sequence.
    ///
    /// # Errors
    ///
    /// Fails when the event is unknown, corrupt, or storage cannot be read.
    pub fn inspect_event(
        &self,
        event_id: SemanticEventId,
    ) -> Result<SemanticEventRecord, SemanticAuthorityError> {
        let connection = self.lock()?;
        load_event_record(&connection, event_id)?
            .ok_or(SemanticAuthorityError::EventNotFound(event_id))
    }

    /// Reads the immutable `AdmissionReceipt` for one admitted event.
    ///
    /// The receipt is the authority's observation fact. A receipt with
    /// `durability = Durable` is the direct durable-admission path; no
    /// outbox acknowledgement is inferred as a stronger publication proof.
    ///
    /// # Errors
    ///
    /// Returns `EventNotFound` when the event is absent, or a storage/corrupt
    /// record error when the event and receipt cannot be read consistently.
    pub fn inspect_admission_receipt(
        &self,
        event_id: SemanticEventId,
    ) -> Result<AdmissionReceipt, SemanticAuthorityError> {
        let connection = self.lock()?;
        load_event_record(&connection, event_id)?
            .ok_or(SemanticAuthorityError::EventNotFound(event_id))?;
        load_receipt(&connection, event_id)
    }

    /// Reads one immutable owner-issued durability proof for an admitted
    /// event. A missing proof is not synthesized from an outbox row.
    ///
    /// # Errors
    ///
    /// Returns `EventNotFound` when the event is absent, or a corrupt/storage
    /// error when the requested receipt is absent or malformed.
    pub fn inspect_durability_receipt(
        &self,
        event_id: SemanticEventId,
        receipt_id: ReceiptId,
    ) -> Result<DurabilityReceipt, SemanticAuthorityError> {
        let connection = self.lock()?;
        load_event_record(&connection, event_id)?
            .ok_or(SemanticAuthorityError::EventNotFound(event_id))?;
        load_durability_receipt(&connection, event_id, receipt_id)
    }

    /// Reads the owner-consistent transport status for one admission outbox item.
    ///
    /// The returned acknowledgement is only an outbox transport observation;
    /// this method never treats it as a Semantic checkpoint or publication
    /// proof.
    ///
    /// # Errors
    ///
    /// Returns `EventNotFound` for an unknown event, or a corrupt/storage error
    /// when the event, admission receipt, and outbox row do not agree.
    pub fn inspect_outbox(
        &self,
        event_id: SemanticEventId,
    ) -> Result<SemanticOutboxRecord, SemanticAuthorityError> {
        let connection = self.lock()?;
        let event = load_event_record(&connection, event_id)?
            .ok_or(SemanticAuthorityError::EventNotFound(event_id))?;
        let admission = load_receipt(&connection, event_id)?;
        let outbox = load_outbox(&connection, event_id)?.ok_or(
            SemanticAuthorityError::CorruptRecord("admitted event has no outbox row"),
        )?;
        if outbox.event_id != event_id
            || outbox.log_seq != event.log_seq
            || outbox.receipt_id != admission.receipt_id
            || outbox.log_seq != admission.log_seq
        {
            return Err(SemanticAuthorityError::CorruptRecord(
                "outbox row disagrees with event or admission receipt",
            ));
        }
        Ok(outbox)
    }

    /// Records an owner-bound, monotonic transport acknowledgement for one
    /// admission outbox item.
    ///
    /// The event/log/receipt triple is re-read from this authority in the
    /// same transaction. A later timestamp advances the transport
    /// high-water; the same timestamp replays; an older timestamp or any
    /// identity mismatch fails closed. This method never creates a
    /// checkpoint/publication receipt and never changes the event log.
    ///
    /// # Errors
    /// Returns `EventNotFound` for an unknown event, typed binding/time
    /// conflicts, or a corrupt/storage failure when the owner rows disagree.
    pub fn acknowledge_outbox(
        &self,
        request: AcknowledgeOutboxRequest,
    ) -> Result<OutboxAckDecision, SemanticAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let event = load_event_record(&transaction, request.event_id)?
            .ok_or(SemanticAuthorityError::EventNotFound(request.event_id))?;
        let admission = load_receipt(&transaction, request.event_id)?;
        let outbox = load_outbox(&transaction, request.event_id)?.ok_or(
            SemanticAuthorityError::CorruptRecord("admitted event has no outbox row"),
        )?;
        if outbox.event_id != request.event_id
            || outbox.log_seq != event.log_seq
            || outbox.log_seq != admission.log_seq
            || outbox.receipt_id != admission.receipt_id
            || request.log_seq != outbox.log_seq
            || request.receipt_id != outbox.receipt_id
        {
            return Err(SemanticAuthorityError::OutboxAckBindingMismatch);
        }
        if request.acknowledged_at_ms < admission.admitted_at_ms {
            return Err(SemanticAuthorityError::OutboxAckBeforeAdmission);
        }
        if let Some(previous) = outbox.acknowledged_at_ms {
            if request.acknowledged_at_ms < previous {
                return Err(SemanticAuthorityError::OutboxAckNotMonotonic {
                    previous,
                    reported: request.acknowledged_at_ms,
                });
            }
            if request.acknowledged_at_ms == previous {
                transaction.commit()?;
                return Ok(OutboxAckDecision::Replayed(outbox));
            }
        }
        let changed = transaction.execute(
            "UPDATE semantic_outbox
             SET acknowledged_at_ms=?1
             WHERE event_id=?2 AND log_seq=?3 AND receipt_id=?4
               AND (acknowledged_at_ms IS NULL OR acknowledged_at_ms=?5)",
            params![
                encode_u64(request.acknowledged_at_ms)?,
                request.event_id.as_bytes().as_slice(),
                encode_u64(request.log_seq)?,
                request.receipt_id.as_bytes().as_slice(),
                outbox.acknowledged_at_ms.map(encode_u64).transpose()?,
            ],
        )?;
        if changed != 1 {
            return Err(SemanticAuthorityError::CorruptRecord(
                "outbox acknowledgement compare-and-swap failed",
            ));
        }
        transaction.commit()?;
        Ok(OutboxAckDecision::Recorded(SemanticOutboxRecord {
            acknowledged_at_ms: Some(request.acknowledged_at_ms),
            ..outbox
        }))
    }

    /// Reads the durable authority-issued Semantic admission endpoint proof.
    ///
    /// # Errors
    ///
    /// Returns corruption or storage errors. Registration consumers must
    /// compare every transported field with this authority readback.
    pub fn inspect_admission_endpoint_proof(
        &self,
    ) -> Result<SemanticAdmissionEndpointProof, SemanticAuthorityError> {
        let connection = self.lock()?;
        schema::load_semantic_admission_endpoint_proof(&connection)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, SemanticAuthorityError> {
        self.connection
            .lock()
            .map_err(|_| SemanticAuthorityError::LockPoisoned)
    }
}

/// Computes the v0.5 content identity over deterministic CBOR
/// `[media_type, exact_bytes]`.
///
/// # Errors
///
/// Rejects invalid media types, oversized content, or an unexpected encoder
/// failure.
pub fn content_digest(
    media_type: &str,
    exact_bytes: &[u8],
) -> Result<[u8; 32], SemanticAuthorityError> {
    validate_content(media_type, exact_bytes)?;
    let mut encoder = Encoder::new(Vec::new());
    encoder
        .array(2)
        .and_then(|e| e.str(media_type))
        .and_then(|e| e.bytes(exact_bytes))
        .map_err(|error| SemanticAuthorityError::CanonicalEncoding(error.to_string()))?;
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/content/v1");
    hasher.update(encoder.into_writer());
    Ok(hasher.finalize().into())
}

/// Computes the digest actually signed by the Semantic store authority.
#[must_use]
pub fn admission_receipt_signature_message(
    receipt_id: ReceiptId,
    receipt_core_digest: [u8; 32],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/admission-receipt-signature/v1");
    hasher.update(receipt_id.as_bytes());
    hasher.update(receipt_core_digest);
    hasher.finalize().into()
}

/// Recomputes the immutable `AdmissionReceipt` core digest, excluding its own
/// derived ID and store signature.
#[must_use]
pub fn admission_receipt_core_digest(receipt: &AdmissionReceipt) -> [u8; 32] {
    build_admission_receipt_core_digest(
        receipt.event_id,
        receipt.log_seq,
        receipt.admitted_at_ms,
        receipt.effective_valid_until_ms,
        &receipt.captured_inputs,
        receipt.effective_taint,
        receipt.authz_policy_digest,
        receipt.store_principal,
        receipt.store_control_domain,
        receipt.store_key_id,
    )
}

fn validate_append_request(request: &AppendAssertionRequest) -> Result<(), SemanticAuthorityError> {
    validate_content(&request.content_media_type, &request.content_bytes)?;
    canonical::validate_sorted_unique(&request.captured_inputs)?;
    Ok(())
}

fn validate_content(media_type: &str, exact_bytes: &[u8]) -> Result<(), SemanticAuthorityError> {
    if media_type.is_empty()
        || media_type.len() > 128
        || !media_type.is_ascii()
        || media_type.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(SemanticAuthorityError::InvalidMediaType);
    }
    if exact_bytes.len() > MAX_CONTENT_BYTES {
        return Err(SemanticAuthorityError::ContentTooLarge);
    }
    Ok(())
}

fn effective_valid_until(
    declared: Option<u64>,
    key: u64,
    capability: u64,
    admission_limit: Option<u64>,
) -> u64 {
    declared
        .into_iter()
        .chain([key, capability])
        .chain(admission_limit)
        .min()
        .expect("key and capability validity are always present")
}

fn validate_lineage(
    transaction: &Transaction<'_>,
    event_id: SemanticEventId,
    declared: &[SemanticEventId],
    captured: &[SemanticEventId],
) -> Result<(), SemanticAuthorityError> {
    for parent in declared.iter().chain(captured) {
        if *parent == event_id {
            return Err(SemanticAuthorityError::InvalidLineage);
        }
        let exists = transaction
            .query_row(
                "SELECT 1 FROM admission_receipts WHERE event_id=?1",
                [parent.as_bytes().as_slice()],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            return Err(SemanticAuthorityError::DanglingLineage(*parent));
        }
    }
    Ok(())
}

fn derive_effective_taint(
    transaction: &Transaction<'_>,
    mut taint: TaintFlags,
    declared: &[SemanticEventId],
    captured: &[SemanticEventId],
) -> Result<TaintFlags, SemanticAuthorityError> {
    for parent in declared.iter().chain(captured) {
        let bits: i64 = transaction.query_row(
            "SELECT effective_taint FROM admission_receipts WHERE event_id=?1",
            [parent.as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        let bits = u64::try_from(bits)
            .map_err(|_| SemanticAuthorityError::CorruptRecord("negative taint"))?;
        let inherited = TaintFlags::from_bits(bits)
            .ok_or(SemanticAuthorityError::CorruptRecord("unknown taint bits"))?;
        taint = taint.union(inherited);
    }
    Ok(taint)
}

fn insert_or_validate_content(
    transaction: &Transaction<'_>,
    digest: [u8; 32],
    media_type: &str,
    exact_bytes: &[u8],
) -> Result<(), SemanticAuthorityError> {
    let existing = transaction
        .query_row(
            "SELECT media_type, exact_bytes FROM content_objects WHERE content_digest=?1",
            [digest.as_slice()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    if let Some((existing_media_type, existing_bytes)) = existing {
        if existing_media_type != media_type || existing_bytes != exact_bytes {
            return Err(SemanticAuthorityError::ContentDigestCollision);
        }
        return Ok(());
    }
    transaction.execute(
        "INSERT INTO content_objects (content_digest, media_type, exact_bytes) VALUES (?1, ?2, ?3)",
        params![digest.as_slice(), media_type, exact_bytes],
    )?;
    Ok(())
}

fn insert_or_validate_spec_body(
    transaction: &Transaction<'_>,
    digest: [u8; 32],
    canonical_body: &[u8],
) -> Result<(), SemanticAuthorityError> {
    let existing = transaction
        .query_row(
            "SELECT canonical_spec_body FROM spec_bodies WHERE spec_body_digest=?1",
            [digest.as_slice()],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    if let Some(existing_body) = existing {
        if existing_body != canonical_body {
            return Err(SemanticAuthorityError::SpecBodyDigestCollision);
        }
        return Ok(());
    }
    transaction.execute(
        "INSERT INTO spec_bodies (spec_body_digest, canonical_spec_body) VALUES (?1, ?2)",
        params![digest.as_slice(), canonical_body],
    )?;
    Ok(())
}

fn insert_event(
    transaction: &Transaction<'_>,
    request: &AppendAssertionRequest,
    event: &UnsignedAssertionEvent,
) -> Result<(), SemanticAuthorityError> {
    let (scope_kind, scope_id) = encode_scope(event.scope);
    transaction.execute(
        "INSERT INTO semantic_events (
            event_id, canonical_unsigned_event, event_type, scope_kind, scope_id,
            issuer_principal_id, issuer_process_id, issuer_process_generation,
            control_domain_id, issued_at_unix_ns, valid_until_ms, purpose_digest,
            key_id, content_digest
         ) VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            request.claimed_event_id.as_bytes().as_slice(),
            request.canonical_unsigned_event.as_slice(),
            scope_kind,
            scope_id.as_slice(),
            event.issuer.as_bytes().as_slice(),
            event.issuer_execution.process_id.as_bytes().as_slice(),
            encode_u64(event.issuer_execution.generation.get())?,
            event.control_domain.as_bytes().as_slice(),
            encode_u64(event.issued_at_unix_ns)?,
            event.valid_until_ms.map(encode_u64).transpose()?,
            event.purpose_digest,
            event.key_id.as_bytes().as_slice(),
            event.content_digest.as_slice(),
        ],
    )?;
    Ok(())
}

fn insert_spec_event(
    transaction: &Transaction<'_>,
    request: &AppendSpecRequest,
    event: &UnsignedSpecEvent,
) -> Result<(), SemanticAuthorityError> {
    let (scope_kind, scope_id) = encode_scope(event.scope);
    transaction.execute(
        "INSERT INTO semantic_events (
            event_id, canonical_unsigned_event, event_type, scope_kind, scope_id,
            issuer_principal_id, issuer_process_id, issuer_process_generation,
            control_domain_id, issued_at_unix_ns, valid_until_ms, purpose_digest,
            key_id, content_digest, spec_body_digest
         ) VALUES (?1, ?2, 5, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL, ?13)",
        params![
            request.claimed_event_id.as_bytes().as_slice(),
            request.canonical_unsigned_event.as_slice(),
            scope_kind,
            scope_id.as_slice(),
            event.issuer.as_bytes().as_slice(),
            event.issuer_execution.process_id.as_bytes().as_slice(),
            encode_u64(event.issuer_execution.generation.get())?,
            event.control_domain.as_bytes().as_slice(),
            encode_u64(event.issued_at_unix_ns)?,
            event.valid_until_ms.map(encode_u64).transpose()?,
            event.purpose_digest,
            event.key_id.as_bytes().as_slice(),
            event.spec_body_digest.as_slice(),
        ],
    )?;
    Ok(())
}

fn insert_lineage_edges(
    transaction: &Transaction<'_>,
    child: SemanticEventId,
    parents: &[SemanticEventId],
    kind: i64,
) -> Result<(), SemanticAuthorityError> {
    for parent in parents {
        transaction.execute(
            "INSERT INTO lineage_edges (child_event_id, parent_event_id, edge_kind)
             VALUES (?1, ?2, ?3)",
            params![
                child.as_bytes().as_slice(),
                parent.as_bytes().as_slice(),
                kind,
            ],
        )?;
    }
    Ok(())
}

fn insert_admission_receipt(
    transaction: &Transaction<'_>,
    receipt: &AdmissionReceipt,
) -> Result<(), SemanticAuthorityError> {
    transaction.execute(
        "INSERT INTO admission_receipts (
            receipt_id, event_id, log_seq, admitted_at_ms, effective_valid_until_ms,
            effective_taint, authz_policy_digest, durability, store_principal_id,
            store_control_domain_id, store_key_id, store_signature
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 2, ?8, ?9, ?10, ?11)",
        params![
            receipt.receipt_id.as_bytes().as_slice(),
            receipt.event_id.as_bytes().as_slice(),
            encode_u64(receipt.log_seq)?,
            encode_u64(receipt.admitted_at_ms)?,
            receipt
                .effective_valid_until_ms
                .map(encode_u64)
                .transpose()?,
            encode_u64(receipt.effective_taint.bits())?,
            receipt.authz_policy_digest.as_slice(),
            receipt.store_principal.as_bytes().as_slice(),
            receipt.store_control_domain.as_bytes().as_slice(),
            receipt.store_key_id.as_bytes().as_slice(),
            receipt.store_signature.as_slice(),
        ],
    )?;
    Ok(())
}

fn load_replay(
    transaction: &Transaction<'_>,
    request: &AppendAssertionRequest,
) -> Result<Option<AdmissionReceipt>, SemanticAuthorityError> {
    if let Some(canonical) = load_existing_canonical(transaction, request.claimed_event_id)? {
        if canonical != request.canonical_unsigned_event {
            return Err(SemanticAuthorityError::EventIdCollision);
        }
    } else {
        return Ok(None);
    }
    let existing = transaction
        .query_row(
            "SELECT e.canonical_unsigned_event, s.signature, c.media_type, c.exact_bytes
             FROM semantic_events e
             JOIN event_signatures s ON s.event_id=e.event_id
             JOIN content_objects c ON c.content_digest=e.content_digest
             WHERE e.event_id=?1",
            [request.claimed_event_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((_canonical, signature, media_type, content)) = existing else {
        return Err(SemanticAuthorityError::CorruptRecord(
            "assertion payload row",
        ));
    };
    if signature.as_slice() != request.signature
        || media_type != request.content_media_type
        || content != request.content_bytes
    {
        return Err(SemanticAuthorityError::EventReplayConflict);
    }
    load_receipt(transaction, request.claimed_event_id).map(Some)
}

fn load_spec_replay(
    transaction: &Transaction<'_>,
    request: &AppendSpecRequest,
) -> Result<Option<AdmissionReceipt>, SemanticAuthorityError> {
    if let Some(canonical) = load_existing_canonical(transaction, request.claimed_event_id)? {
        if canonical != request.canonical_unsigned_event {
            return Err(SemanticAuthorityError::EventIdCollision);
        }
    } else {
        return Ok(None);
    }
    let existing = transaction
        .query_row(
            "SELECT e.canonical_unsigned_event, s.signature, b.canonical_spec_body
             FROM semantic_events e
             JOIN event_signatures s ON s.event_id=e.event_id
             JOIN spec_bodies b ON b.spec_body_digest=e.spec_body_digest
             WHERE e.event_id=?1",
            [request.claimed_event_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((canonical, signature, canonical_body)) = existing else {
        return Err(SemanticAuthorityError::CorruptRecord("SPEC payload row"));
    };
    let event = decode_unsigned_spec_event(&canonical)?;
    if signature.as_slice() != request.signature || canonical_body != event.canonical_spec_body {
        return Err(SemanticAuthorityError::EventReplayConflict);
    }
    load_receipt(transaction, request.claimed_event_id).map(Some)
}

fn load_existing_canonical(
    connection: &Connection,
    event_id: SemanticEventId,
) -> Result<Option<Vec<u8>>, SemanticAuthorityError> {
    connection
        .query_row(
            "SELECT canonical_unsigned_event FROM semantic_events WHERE event_id=?1",
            [event_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()
        .map_err(SemanticAuthorityError::Sqlite)
}

fn load_receipt(
    connection: &Connection,
    event_id: SemanticEventId,
) -> Result<AdmissionReceipt, SemanticAuthorityError> {
    let row = connection.query_row(
        "SELECT receipt_id, log_seq, admitted_at_ms, effective_valid_until_ms,
                effective_taint, authz_policy_digest, store_principal_id,
                store_control_domain_id, store_key_id, store_signature
         FROM admission_receipts WHERE event_id=?1",
        [event_id.as_bytes().as_slice()],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Vec<u8>>(5)?,
                row.get::<_, Vec<u8>>(6)?,
                row.get::<_, Vec<u8>>(7)?,
                row.get::<_, Vec<u8>>(8)?,
                row.get::<_, Vec<u8>>(9)?,
            ))
        },
    )?;
    let captured_inputs = load_edge_ids(connection, event_id, EDGE_CAPTURED)?;
    Ok(AdmissionReceipt {
        receipt_id: decode_id(row.0, ReceiptId::from_bytes, "receipt id")?,
        event_id,
        log_seq: decode_u64(row.1)?,
        admitted_at_ms: decode_u64(row.2)?,
        effective_valid_until_ms: row.3.map(decode_u64).transpose()?,
        captured_inputs,
        effective_taint: TaintFlags::from_bits(decode_u64(row.4)?)
            .ok_or(SemanticAuthorityError::CorruptRecord("taint bits"))?,
        authz_policy_digest: decode_array(row.5, "policy digest")?,
        durability: AdmissionDurability::Durable,
        store_principal: decode_id(
            row.6,
            nlos_types::PrincipalId::from_bytes,
            "store principal",
        )?,
        store_control_domain: decode_id(
            row.7,
            nlos_types::ControlDomainId::from_bytes,
            "store domain",
        )?,
        store_key_id: decode_id(row.8, nlos_types::KeyId::from_bytes, "store key")?,
        store_signature: decode_array(row.9, "store signature")?,
    })
}

fn load_durability_receipt(
    connection: &Connection,
    event_id: SemanticEventId,
    receipt_id: ReceiptId,
) -> Result<DurabilityReceipt, SemanticAuthorityError> {
    let row = connection.query_row(
        "SELECT event_id, durable_checkpoint_id, durable_at_ms, store_signature
         FROM durability_receipts WHERE receipt_id=?1 AND event_id=?2",
        params![
            receipt_id.as_bytes().as_slice(),
            event_id.as_bytes().as_slice()
        ],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        },
    )?;
    Ok(DurabilityReceipt {
        receipt_id,
        event_id: nlos_types::SemanticEventId::from_bytes(
            row.0
                .try_into()
                .map_err(|_| SemanticAuthorityError::CorruptRecord("durability event id"))?,
        ),
        durable_checkpoint_id: row
            .1
            .try_into()
            .map_err(|_| SemanticAuthorityError::CorruptRecord("durable checkpoint id"))?,
        durable_at_ms: decode_u64(row.2)?,
        store_signature: row
            .3
            .try_into()
            .map_err(|_| SemanticAuthorityError::CorruptRecord("durability store signature"))?,
    })
}

fn load_outbox(
    connection: &Connection,
    event_id: SemanticEventId,
) -> Result<Option<SemanticOutboxRecord>, SemanticAuthorityError> {
    connection
        .query_row(
            "SELECT log_seq, event_id, receipt_id, acknowledged_at_ms
             FROM semantic_outbox WHERE event_id=?1",
            [event_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .optional()?
        .map(|row| {
            Ok(SemanticOutboxRecord {
                log_seq: decode_u64(row.0)?,
                event_id: decode_id(row.1, SemanticEventId::from_bytes, "outbox event id")?,
                receipt_id: decode_id(row.2, ReceiptId::from_bytes, "outbox receipt id")?,
                acknowledged_at_ms: row.3.map(decode_u64).transpose()?,
            })
        })
        .transpose()
}

fn load_event_record(
    connection: &Connection,
    event_id: SemanticEventId,
) -> Result<Option<SemanticEventRecord>, SemanticAuthorityError> {
    connection
        .query_row(
            "SELECT e.canonical_unsigned_event, e.event_type, e.scope_kind, e.scope_id,
                    e.issuer_principal_id, e.control_domain_id, e.key_id,
                    e.content_digest, e.spec_body_digest, l.log_seq
             FROM semantic_events e JOIN event_log l ON l.event_id=e.event_id
             WHERE e.event_id=?1",
            [event_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, Vec<u8>>(6)?,
                    row.get::<_, Option<Vec<u8>>>(7)?,
                    row.get::<_, Option<Vec<u8>>>(8)?,
                    row.get::<_, i64>(9)?,
                ))
            },
        )
        .optional()?
        .map(|row| {
            Ok(SemanticEventRecord {
                event_id,
                canonical_unsigned_event: row.0,
                scope: decode_scope(row.2, decode_array(row.3, "scope id")?)?,
                issuer: decode_id(row.4, nlos_types::PrincipalId::from_bytes, "issuer")?,
                control_domain: decode_id(
                    row.5,
                    nlos_types::ControlDomainId::from_bytes,
                    "domain",
                )?,
                key_id: decode_id(row.6, nlos_types::KeyId::from_bytes, "key")?,
                payload_identity: decode_payload_identity(row.1, row.7, row.8)?,
                log_seq: decode_u64(row.9)?,
            })
        })
        .transpose()
}

fn load_edge_ids(
    connection: &Connection,
    child: SemanticEventId,
    kind: i64,
) -> Result<Vec<SemanticEventId>, SemanticAuthorityError> {
    let mut statement = connection.prepare(
        "SELECT parent_event_id FROM lineage_edges
         WHERE child_event_id=?1 AND edge_kind=?2 ORDER BY parent_event_id",
    )?;
    let rows = statement.query_map(params![child.as_bytes().as_slice(), kind], |row| {
        row.get::<_, Vec<u8>>(0)
    })?;
    rows.map(|row| {
        row.map_err(SemanticAuthorityError::Sqlite)
            .and_then(|bytes| decode_id(bytes, SemanticEventId::from_bytes, "event id"))
    })
    .collect()
}

#[allow(clippy::too_many_arguments)] // Fixed signed Receipt core field order.
fn build_admission_receipt_core_digest(
    event_id: SemanticEventId,
    log_seq: u64,
    admitted_at_ms: u64,
    effective_valid_until_ms: Option<u64>,
    captured_inputs: &[SemanticEventId],
    effective_taint: TaintFlags,
    authz_policy_digest: [u8; 32],
    store_principal: nlos_types::PrincipalId,
    store_control_domain: nlos_types::ControlDomainId,
    store_key_id: nlos_types::KeyId,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/admission-receipt/v1");
    hasher.update(event_id.as_bytes());
    hasher.update(log_seq.to_be_bytes());
    hasher.update(admitted_at_ms.to_be_bytes());
    match effective_valid_until_ms {
        Some(value) => {
            hasher.update([1]);
            hasher.update(value.to_be_bytes());
        }
        None => hasher.update([0]),
    }
    hasher.update((captured_inputs.len() as u64).to_be_bytes());
    for input in captured_inputs {
        hasher.update(input.as_bytes());
    }
    hasher.update(effective_taint.bits().to_be_bytes());
    hasher.update(authz_policy_digest);
    hasher.update([2]);
    hasher.update(store_principal.as_bytes());
    hasher.update(store_control_domain.as_bytes());
    hasher.update(store_key_id.as_bytes());
    hasher.finalize().into()
}

fn encode_scope(scope: CapabilityTarget) -> (i64, [u8; 16]) {
    match scope {
        CapabilityTarget::Namespace(id) => (1, id.into_bytes()),
        CapabilityTarget::Task(id) => (2, id.into_bytes()),
    }
}

fn decode_scope(kind: i64, bytes: [u8; 16]) -> Result<CapabilityTarget, SemanticAuthorityError> {
    match kind {
        1 => Ok(CapabilityTarget::Namespace(
            nlos_types::NamespaceId::from_bytes(bytes),
        )),
        2 => Ok(CapabilityTarget::Task(nlos_types::TaskId::from_bytes(
            bytes,
        ))),
        _ => Err(SemanticAuthorityError::CorruptRecord("scope kind")),
    }
}

fn decode_payload_identity(
    event_type: i64,
    content_digest: Option<Vec<u8>>,
    spec_body_digest: Option<Vec<u8>>,
) -> Result<SemanticPayloadIdentity, SemanticAuthorityError> {
    match (event_type, content_digest, spec_body_digest) {
        (1, Some(digest), None) => Ok(SemanticPayloadIdentity::AssertionContent(decode_array(
            digest,
            "content digest",
        )?)),
        (5, None, Some(digest)) => Ok(SemanticPayloadIdentity::IntentSpecBody(decode_array(
            digest,
            "spec body digest",
        )?)),
        _ => Err(SemanticAuthorityError::CorruptRecord(
            "event payload identity",
        )),
    }
}

fn encode_u64(value: u64) -> Result<i64, SemanticAuthorityError> {
    i64::try_from(value).map_err(|_| SemanticAuthorityError::CorruptRecord("u64 exceeds SQLite"))
}

fn decode_u64(value: i64) -> Result<u64, SemanticAuthorityError> {
    u64::try_from(value).map_err(|_| SemanticAuthorityError::CorruptRecord("negative integer"))
}

fn decode_array<const N: usize>(
    bytes: Vec<u8>,
    field: &'static str,
) -> Result<[u8; N], SemanticAuthorityError> {
    bytes
        .try_into()
        .map_err(|_| SemanticAuthorityError::CorruptRecord(field))
}

fn decode_id<const N: usize, T>(
    bytes: Vec<u8>,
    constructor: impl FnOnce([u8; N]) -> T,
    field: &'static str,
) -> Result<T, SemanticAuthorityError> {
    Ok(constructor(decode_array(bytes, field)?))
}
