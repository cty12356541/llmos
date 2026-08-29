//! Durable local Capability issue, attenuation, and revocation authority.
//!
//! Capability IDs are authority records, not caller assertions. Every use is
//! checked against the current generation and the complete ancestor chain.

mod model;
mod schema;

use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_identity::{
    Ed25519Signature, IdentityAuthority, IdentityAuthorityError, IdentityBinding,
    VerifiedCapabilityCommandSigner, VerifyCapabilityCommandSignatureRequest,
};
use nlos_types::{CapabilityId, Generation, IdempotencyKey, KeyId, PrincipalId, ReceiptId};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

pub use model::{
    AuthorizeSemanticRequest, CapabilityHandle, CapabilityIssueDecision, CapabilityIssueReceipt,
    CapabilityRecord, CapabilityRevocationDecision, CapabilityRevocationReceipt, CapabilityRights,
    CapabilityTarget, DelegateCapabilityRequest, IssueRootCapabilityRequest,
    RevokeCapabilityRequest, SemanticAuthorization, SignedDelegateCapabilityRequest,
    SignedIssueRootCapabilityRequest, SignedRevokeCapabilityRequest,
};

const SCHEMA_VERSION: i64 = 1;

/// Domain separator for signed Capability command messages. Every field is
/// fixed-width big-endian with presence tags, so the framing stays canonical
/// without variable-length length prefixes (mirrors the identity message
/// style).
const COMMAND_MESSAGE_DOMAIN: &[u8] = b"nlos/capability/command/v1";
const COMMAND_KIND_ISSUE_ROOT: u8 = 1;
const COMMAND_KIND_DELEGATE: u8 = 2;
const COMMAND_KIND_REVOKE: u8 = 3;

#[derive(Debug)]
pub enum CapabilityAuthorityError {
    Sqlite(rusqlite::Error),
    Identity(IdentityAuthorityError),
    Io(std::io::Error),
    DurabilityUnavailable {
        journal_mode: String,
        synchronous: i64,
    },
    SchemaVersionUnsupported(i64),
    CapabilityNotFound(CapabilityId),
    IdempotencyConflict,
    InvalidValidity,
    InvalidCallLimit,
    IdentityKeyInactive(KeyId),
    SignatureInvalid,
    PrincipalUnknown(PrincipalId),
    KeyRevoked,
    GenerationFenceConflict,
    CapabilityRevoked,
    CapabilityNotYetValid,
    CapabilityExpired,
    AncestorRevokedOrFenced,
    DelegatorMismatch,
    RevokerUnauthorized,
    DelegationNotAllowed,
    RightsAmplification,
    ScopeAmplification,
    PurposeAmplification,
    ValidityAmplification,
    CallLimitAmplification,
    DelegationDepthAmplification,
    HolderMismatch,
    TargetMismatch,
    RequiredRightMissing,
    PurposeMismatch,
    GenerationExhausted,
    CorruptRecord(&'static str),
    LockPoisoned,
}

impl fmt::Display for CapabilityAuthorityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => {
                write!(formatter, "SQLite capability authority failure: {error}")
            }
            Self::Identity(error) => write!(formatter, "identity authority failure: {error}"),
            Self::Io(error) => write!(formatter, "capability authority I/O failure: {error}"),
            Self::DurabilityUnavailable {
                journal_mode,
                synchronous,
            } => write!(
                formatter,
                "WAL/FULL durability unavailable: journal_mode={journal_mode}, synchronous={synchronous}"
            ),
            Self::SchemaVersionUnsupported(version) => write!(
                formatter,
                "unsupported capability authority schema version {version}"
            ),
            Self::CapabilityNotFound(id) => write!(formatter, "capability {id:?} does not exist"),
            Self::IdempotencyConflict => {
                formatter.write_str("idempotency key was rebound to different capability input")
            }
            Self::InvalidValidity => formatter.write_str("invalid capability validity interval"),
            Self::InvalidCallLimit => formatter.write_str("call limit must be non-zero"),
            Self::IdentityKeyInactive(id) => write!(formatter, "identity key {id:?} is inactive"),
            Self::SignatureInvalid => {
                formatter.write_str("capability command signature is invalid")
            }
            Self::PrincipalUnknown(id) => write!(
                formatter,
                "capability command signer principal {id:?} does not exist"
            ),
            Self::KeyRevoked => formatter.write_str("capability command signer key is revoked"),
            Self::GenerationFenceConflict => formatter.write_str("capability generation is stale"),
            Self::CapabilityRevoked => formatter.write_str("capability is revoked"),
            Self::CapabilityNotYetValid => formatter.write_str("capability is not yet valid"),
            Self::CapabilityExpired => formatter.write_str("capability has expired"),
            Self::AncestorRevokedOrFenced => {
                formatter.write_str("an ancestor capability was revoked or generation-fenced")
            }
            Self::DelegatorMismatch => {
                formatter.write_str("delegator is not the capability holder")
            }
            Self::RevokerUnauthorized => {
                formatter.write_str("revoker is neither capability issuer nor holder")
            }
            Self::DelegationNotAllowed => {
                formatter.write_str("capability does not permit further delegation")
            }
            Self::RightsAmplification => formatter.write_str("delegation amplifies rights"),
            Self::ScopeAmplification => formatter.write_str("delegation changes target scope"),
            Self::PurposeAmplification => formatter.write_str("delegation broadens purpose"),
            Self::ValidityAmplification => formatter.write_str("delegation broadens validity"),
            Self::CallLimitAmplification => formatter.write_str("delegation broadens call limit"),
            Self::DelegationDepthAmplification => {
                formatter.write_str("delegation does not reduce remaining depth")
            }
            Self::HolderMismatch => formatter.write_str("authenticated signer is not the holder"),
            Self::TargetMismatch => formatter.write_str("capability target does not match"),
            Self::RequiredRightMissing => {
                formatter.write_str("required capability right is absent")
            }
            Self::PurposeMismatch => formatter.write_str("capability purpose does not match"),
            Self::GenerationExhausted => formatter.write_str("generation space exhausted"),
            Self::CorruptRecord(reason) => write!(formatter, "corrupt durable record: {reason}"),
            Self::LockPoisoned => formatter.write_str("capability authority lock is poisoned"),
        }
    }
}

impl Error for CapabilityAuthorityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Identity(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for CapabilityAuthorityError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<IdentityAuthorityError> for CapabilityAuthorityError {
    fn from(error: IdentityAuthorityError) -> Self {
        Self::Identity(error)
    }
}

pub struct CapabilityAuthority {
    connection: Mutex<Connection>,
}

impl CapabilityAuthority {
    /// Opens `<root>/capability-authority.db` with WAL/FULL durability.
    ///
    /// # Errors
    ///
    /// Fails when storage, durability configuration, or schema validation
    /// cannot be established.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, CapabilityAuthorityError> {
        std::fs::create_dir_all(root.as_ref()).map_err(CapabilityAuthorityError::Io)?;
        let mut connection = Connection::open(root.as_ref().join("capability-authority.db"))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let journal_mode: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let synchronous: i64 =
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 {
            return Err(CapabilityAuthorityError::DurabilityUnavailable {
                journal_mode,
                synchronous,
            });
        }
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => schema::migrate_v1(&mut connection)?,
            SCHEMA_VERSION => {}
            other => return Err(CapabilityAuthorityError::SchemaVersionUnsupported(other)),
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Issues a root capability through the deprecated unsigned trusted
    /// authority API after resolving both issuer and holder from the Identity
    /// authority.
    ///
    /// # Deprecated
    ///
    /// Unsigned capability commands carry no proof of the acting principal;
    /// use [`CapabilityAuthority::issue_root_signed`] (ADR-0010). Removal is
    /// a future breaking change.
    ///
    /// # Errors
    ///
    /// Fails on inactive identities, invalid bounds, idempotency conflict, or
    /// storage failure.
    #[deprecated(
        since = "0.1.0",
        note = "unsigned TCB entry; use `issue_root_signed` (ADR-0010)"
    )]
    pub fn issue_root(
        &self,
        identity: &IdentityAuthority,
        request: IssueRootCapabilityRequest,
    ) -> Result<CapabilityIssueDecision, CapabilityAuthorityError> {
        self.issue_root_impl(identity, request, None)
    }

    /// Issues a root capability through the signature-gated authority API:
    /// the acting issuer principal must present an Ed25519 signature over
    /// [`issue_root_command_message`] under its current Identity key binding.
    /// The gate order is input bounds, idempotent replay (a replayed decision
    /// is the durable authority and never re-verifies the signature), command
    /// signature verification (typed fail-closed before any durable write),
    /// then the existing identity resolution and issuance transaction. The
    /// durable `capability_heads` issuer columns record the verified signer.
    ///
    /// # Errors
    ///
    /// Fails on invalid bounds, unknown signers, invalid or revoked signer
    /// signatures, inactive identities, idempotency conflict, or storage
    /// failure.
    pub fn issue_root_signed(
        &self,
        identity: &IdentityAuthority,
        request: SignedIssueRootCapabilityRequest,
    ) -> Result<CapabilityIssueDecision, CapabilityAuthorityError> {
        let proof = CommandSignatureProof {
            signer: request.signer,
            signature: request.signature,
        };
        self.issue_root_impl(identity, request.command, Some(proof))
    }

    /// Shared issuance body; `proof` is `Some` exactly for the signed entry.
    fn issue_root_impl(
        &self,
        identity: &IdentityAuthority,
        request: IssueRootCapabilityRequest,
        proof: Option<CommandSignatureProof>,
    ) -> Result<CapabilityIssueDecision, CapabilityAuthorityError> {
        validate_bounds(
            request.valid_from_ms,
            request.valid_until_ms,
            request.call_limit,
        )?;
        let request_digest = root_request_digest(request);
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(replay) =
            load_issue_replay(&transaction, request.idempotency_key, request_digest)?
        {
            transaction.commit()?;
            return Ok(replay);
        }
        let verified = verify_command_signature(
            identity,
            proof,
            &issue_root_command_message(request),
            request.issued_at_ms,
        )?;
        let issuer = active_identity(identity, request.issuer_key_id, request.issued_at_ms)?;
        require_signed_by(verified, &issuer)?;
        let holder = active_identity(identity, request.holder_key_id, request.issued_at_ms)?;
        let record = new_record(
            request_digest,
            issuer,
            holder,
            request.target,
            request.rights,
            request.purpose_digest,
            request.valid_from_ms,
            request.valid_until_ms,
            request.delegation_depth_remaining,
            request.call_limit,
            None,
        );
        let receipt = new_issue_receipt(record, request_digest, request.issued_at_ms);
        insert_capability(
            &transaction,
            record,
            receipt,
            request.idempotency_key,
            request_digest,
        )?;
        transaction.commit()?;
        Ok(CapabilityIssueDecision::Issued(record, receipt))
    }

    /// Delegates an active capability through the deprecated unsigned trusted
    /// authority API while mechanically enforcing monotonic attenuation and
    /// binding an immutable delegation Receipt.
    ///
    /// # Deprecated
    ///
    /// Unsigned capability commands carry no proof of the acting principal;
    /// use [`CapabilityAuthority::delegate_signed`] (ADR-0010). Removal is a
    /// future breaking change.
    ///
    /// # Errors
    ///
    /// Fails on stale/revoked ancestry, unauthenticated identities, any scope,
    /// rights, purpose, validity, call-limit, or depth amplification, or storage
    /// failure.
    #[deprecated(
        since = "0.1.0",
        note = "unsigned TCB entry; use `delegate_signed` (ADR-0010)"
    )]
    pub fn delegate(
        &self,
        identity: &IdentityAuthority,
        request: DelegateCapabilityRequest,
    ) -> Result<CapabilityIssueDecision, CapabilityAuthorityError> {
        self.delegate_impl(identity, request, None)
    }

    /// Delegates an active capability through the signature-gated authority
    /// API: the acting delegator principal must present an Ed25519 signature
    /// over [`delegate_command_message`] under its current Identity key
    /// binding, and the signature is bound to the delegator principal before
    /// attenuation is evaluated. The gate order is input bounds, idempotent
    /// replay (never re-verifies the signature), ancestry structure, command
    /// signature verification (typed fail-closed before any durable write),
    /// then the existing identity and attenuation gates. The durable child
    /// record's issuer columns record the verified signer.
    ///
    /// # Errors
    ///
    /// Fails on unknown signers, invalid or revoked signer signatures,
    /// stale/revoked ancestry, unauthenticated identities, any scope, rights,
    /// purpose, validity, call-limit, or depth amplification, or storage
    /// failure.
    pub fn delegate_signed(
        &self,
        identity: &IdentityAuthority,
        request: SignedDelegateCapabilityRequest,
    ) -> Result<CapabilityIssueDecision, CapabilityAuthorityError> {
        let proof = CommandSignatureProof {
            signer: request.signer,
            signature: request.signature,
        };
        self.delegate_impl(identity, request.command, Some(proof))
    }

    /// Shared delegation body; `proof` is `Some` exactly for the signed entry.
    ///
    /// # Errors
    ///
    /// Fails on invalid bounds, stale/revoked ancestry, signature, identity,
    /// or amplification failures, or storage failure.
    #[allow(clippy::too_many_lines)] // Keeps the attenuation decision in one linear audit path.
    fn delegate_impl(
        &self,
        identity: &IdentityAuthority,
        request: DelegateCapabilityRequest,
        proof: Option<CommandSignatureProof>,
    ) -> Result<CapabilityIssueDecision, CapabilityAuthorityError> {
        validate_bounds(
            request.valid_from_ms,
            request.valid_until_ms,
            request.call_limit,
        )?;
        let request_digest = delegate_request_digest(request);
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(replay) =
            load_issue_replay(&transaction, request.idempotency_key, request_digest)?
        {
            transaction.commit()?;
            return Ok(replay);
        }
        let parent = load_exact_current(&transaction, request.parent)?;
        validate_active_chain(&transaction, parent, request.delegated_at_ms)?;
        let verified = verify_command_signature(
            identity,
            proof,
            &delegate_command_message(request),
            request.delegated_at_ms,
        )?;
        let delegator =
            active_identity(identity, request.delegator_key_id, request.delegated_at_ms)?;
        require_signed_by(verified, &delegator)?;
        if delegator.principal_id != parent.holder
            || delegator.control_domain_id != parent.holder_control_domain
        {
            return Err(CapabilityAuthorityError::DelegatorMismatch);
        }
        let recipient =
            active_identity(identity, request.recipient_key_id, request.delegated_at_ms)?;
        if !parent.rights.contains(CapabilityRights::DELEGATE) {
            return Err(CapabilityAuthorityError::DelegationNotAllowed);
        }
        if !request.rights.is_subset_of(parent.rights) {
            return Err(CapabilityAuthorityError::RightsAmplification);
        }
        if request.target != parent.target {
            return Err(CapabilityAuthorityError::ScopeAmplification);
        }
        if !purpose_is_attenuated(parent.purpose_digest, request.purpose_digest) {
            return Err(CapabilityAuthorityError::PurposeAmplification);
        }
        if request.valid_from_ms < parent.valid_from_ms
            || request.valid_until_ms > parent.valid_until_ms
        {
            return Err(CapabilityAuthorityError::ValidityAmplification);
        }
        if !call_limit_is_attenuated(parent.call_limit, request.call_limit) {
            return Err(CapabilityAuthorityError::CallLimitAmplification);
        }
        if request.delegation_depth_remaining >= parent.delegation_depth_remaining {
            return Err(CapabilityAuthorityError::DelegationDepthAmplification);
        }
        let record = new_record(
            request_digest,
            delegator,
            recipient,
            request.target,
            request.rights,
            request.purpose_digest,
            request.valid_from_ms,
            request.valid_until_ms,
            request.delegation_depth_remaining,
            request.call_limit,
            Some(request.parent),
        );
        let receipt = new_issue_receipt(record, request_digest, request.delegated_at_ms);
        insert_capability(
            &transaction,
            record,
            receipt,
            request.idempotency_key,
            request_digest,
        )?;
        transaction.commit()?;
        Ok(CapabilityIssueDecision::Issued(record, receipt))
    }

    /// Revokes a capability by advancing its generation through the
    /// deprecated unsigned trusted authority API. Issuer or holder may
    /// revoke; descendants are invalidated by their stored parent generation.
    ///
    /// # Deprecated
    ///
    /// Unsigned capability commands carry no proof of the acting principal;
    /// use [`CapabilityAuthority::revoke_signed`] (ADR-0010). Removal is a
    /// future breaking change.
    ///
    /// # Errors
    ///
    /// Fails on stale generation, inactive/unauthorized revoker, idempotency
    /// conflict, generation exhaustion, or storage failure.
    #[deprecated(
        since = "0.1.0",
        note = "unsigned TCB entry; use `revoke_signed` (ADR-0010)"
    )]
    pub fn revoke(
        &self,
        identity: &IdentityAuthority,
        request: RevokeCapabilityRequest,
    ) -> Result<CapabilityRevocationDecision, CapabilityAuthorityError> {
        self.revoke_impl(identity, request, None)
    }

    /// Revokes a capability through the signature-gated authority API: the
    /// acting revoker principal must present an Ed25519 signature over
    /// [`revoke_command_message`] under its current Identity key binding. The
    /// gate order is idempotent replay (never re-verifies the signature),
    /// target structure and revocation state, command signature verification
    /// (typed fail-closed before any durable write), then the existing
    /// authorization and generation-advance transaction. The durable
    /// revocation receipt records the verified signer as `revoker`.
    ///
    /// # Errors
    ///
    /// Fails on stale generation, unknown signers, invalid or revoked signer
    /// signatures, unauthorized revokers, idempotency conflict, generation
    /// exhaustion, or storage failure.
    pub fn revoke_signed(
        &self,
        identity: &IdentityAuthority,
        request: SignedRevokeCapabilityRequest,
    ) -> Result<CapabilityRevocationDecision, CapabilityAuthorityError> {
        let proof = CommandSignatureProof {
            signer: request.signer,
            signature: request.signature,
        };
        self.revoke_impl(identity, request.command, Some(proof))
    }

    /// Shared revocation body; `proof` is `Some` exactly for the signed entry.
    fn revoke_impl(
        &self,
        identity: &IdentityAuthority,
        request: RevokeCapabilityRequest,
        proof: Option<CommandSignatureProof>,
    ) -> Result<CapabilityRevocationDecision, CapabilityAuthorityError> {
        let request_digest = revoke_request_digest(request);
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(replay) =
            load_revocation_replay(&transaction, request.idempotency_key, request_digest)?
        {
            transaction.commit()?;
            return Ok(CapabilityRevocationDecision::Replayed(replay));
        }
        let record = load_exact_current(&transaction, request.handle)?;
        if record.revoked_at_ms.is_some() {
            return Err(CapabilityAuthorityError::CapabilityRevoked);
        }
        let verified = verify_command_signature(
            identity,
            proof,
            &revoke_command_message(request),
            request.revoked_at_ms,
        )?;
        let revoker = active_identity(identity, request.revoker_key_id, request.revoked_at_ms)?;
        require_signed_by(verified, &revoker)?;
        if revoker.principal_id != record.issuer && revoker.principal_id != record.holder {
            return Err(CapabilityAuthorityError::RevokerUnauthorized);
        }
        let next_generation = record
            .handle
            .generation
            .checked_next()
            .ok_or(CapabilityAuthorityError::GenerationExhausted)?;
        transaction.execute(
            "INSERT INTO capability_versions (capability_id, generation, revoked_at_ms)
             VALUES (?1, ?2, ?3)",
            params![
                record.handle.capability_id.as_bytes().as_slice(),
                encode_generation(next_generation)?,
                encode_u64(request.revoked_at_ms)?,
            ],
        )?;
        let changed = transaction.execute(
            "UPDATE capability_heads SET current_generation=?1
             WHERE capability_id=?2 AND current_generation=?3",
            params![
                encode_generation(next_generation)?,
                record.handle.capability_id.as_bytes().as_slice(),
                encode_generation(request.handle.generation)?,
            ],
        )?;
        if changed != 1 {
            return Err(CapabilityAuthorityError::GenerationFenceConflict);
        }
        let receipt_id = ReceiptId::from_bytes(derive_id(
            b"nlos/capability-revocation-receipt/id/v1",
            &[&request_digest],
        ));
        let receipt = CapabilityRevocationReceipt {
            receipt_id,
            capability_id: request.handle.capability_id,
            prior_generation: request.handle.generation,
            resulting_generation: next_generation,
            revoker: revoker.principal_id,
            revoked_at_ms: request.revoked_at_ms,
        };
        transaction.execute(
            "INSERT INTO capability_revocation_receipts (
                idempotency_key, request_digest, receipt_id, capability_id,
                prior_generation, resulting_generation, revoker_principal_id, revoked_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                request.idempotency_key.as_bytes().as_slice(),
                request_digest.as_slice(),
                receipt_id.as_bytes().as_slice(),
                request.handle.capability_id.as_bytes().as_slice(),
                encode_generation(request.handle.generation)?,
                encode_generation(next_generation)?,
                revoker.principal_id.as_bytes().as_slice(),
                encode_u64(request.revoked_at_ms)?,
            ],
        )?;
        transaction.commit()?;
        Ok(CapabilityRevocationDecision::Revoked(receipt))
    }

    /// Authorizes one Semantic operation using an unforgeable signer proof
    /// returned by `IdentityAuthority::verify_semantic_signature`.
    ///
    /// # Errors
    ///
    /// Fails on stale/revoked ancestry, holder/target/purpose mismatch, missing
    /// rights, invalid time, or storage failure.
    pub fn authorize_semantic(
        &self,
        request: AuthorizeSemanticRequest,
    ) -> Result<SemanticAuthorization, CapabilityAuthorityError> {
        let connection = self.lock()?;
        let record = load_exact_current(&connection, request.handle)?;
        validate_active_chain(&connection, record, request.admitted_at_ms)?;
        if request.signer.principal_id() != record.holder
            || request.signer.control_domain_id() != record.holder_control_domain
        {
            return Err(CapabilityAuthorityError::HolderMismatch);
        }
        if request.target != record.target {
            return Err(CapabilityAuthorityError::TargetMismatch);
        }
        if !record.rights.contains(request.required_right) {
            return Err(CapabilityAuthorityError::RequiredRightMissing);
        }
        if record.purpose_digest != request.purpose_digest {
            return Err(CapabilityAuthorityError::PurposeMismatch);
        }
        Ok(SemanticAuthorization {
            capability_id: record.handle.capability_id,
            generation: record.handle.generation,
            holder: record.holder,
            target: record.target,
            granted_rights: record.rights,
            purpose_digest: record.purpose_digest,
        })
    }

    /// Reads an exact current-generation capability after validating its
    /// ancestor chain at `at_ms`.
    ///
    /// # Errors
    ///
    /// Fails for unknown, stale, revoked, expired, or corrupt records.
    pub fn inspect_active(
        &self,
        handle: CapabilityHandle,
        at_ms: u64,
    ) -> Result<CapabilityRecord, CapabilityAuthorityError> {
        let connection = self.lock()?;
        let record = load_exact_current(&connection, handle)?;
        validate_active_chain(&connection, record, at_ms)?;
        Ok(record)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, CapabilityAuthorityError> {
        self.connection
            .lock()
            .map_err(|_| CapabilityAuthorityError::LockPoisoned)
    }
}

fn active_identity(
    authority: &IdentityAuthority,
    key_id: KeyId,
    at_ms: u64,
) -> Result<IdentityBinding, CapabilityAuthorityError> {
    let binding = authority.inspect_current_binding(key_id)?;
    if binding.key_revoked_at_ms.is_some()
        || at_ms < binding.key_valid_from_ms
        || at_ms > binding.key_valid_until_ms
    {
        return Err(CapabilityAuthorityError::IdentityKeyInactive(key_id));
    }
    Ok(binding)
}

/// The acting principal's signature over one Capability command message.
struct CommandSignatureProof {
    signer: PrincipalId,
    signature: Ed25519Signature,
}

/// Verifies the command signature for signed entries; unsigned entries pass
/// `None` and skip straight to the trusted TCB gates.
fn verify_command_signature(
    identity: &IdentityAuthority,
    proof: Option<CommandSignatureProof>,
    message: &[u8; 32],
    at_ms: u64,
) -> Result<Option<VerifiedCapabilityCommandSigner>, CapabilityAuthorityError> {
    let Some(proof) = proof else {
        return Ok(None);
    };
    identity
        .verify_capability_command_signature(VerifyCapabilityCommandSignatureRequest {
            message_digest: *message,
            principal: proof.signer,
            signature: proof.signature,
            verified_at_ms: at_ms,
        })
        .map(Some)
        .map_err(command_signature_error)
}

/// Signed entries must prove that the declared behavior key is the current
/// key of the verifying principal, so a signature by any other principal is
/// rejected even when it verifies cryptographically.
fn require_signed_by(
    verified: Option<VerifiedCapabilityCommandSigner>,
    binding: &IdentityBinding,
) -> Result<(), CapabilityAuthorityError> {
    match verified {
        None => Ok(()),
        Some(signer)
            if signer.principal_id() == binding.principal_id
                && signer.key_id() == binding.key_id =>
        {
            Ok(())
        }
        Some(_) => Err(CapabilityAuthorityError::SignatureInvalid),
    }
}

/// Lifts the identity verification failure into the typed command-signature
/// surface (`SignatureInvalid`/`PrincipalUnknown`/`KeyRevoked`); all other
/// identity failures keep their typed wrapping.
fn command_signature_error(error: IdentityAuthorityError) -> CapabilityAuthorityError {
    match error {
        IdentityAuthorityError::InvalidSignature => CapabilityAuthorityError::SignatureInvalid,
        IdentityAuthorityError::PrincipalNotFound(id) => {
            CapabilityAuthorityError::PrincipalUnknown(id)
        }
        IdentityAuthorityError::KeyRevoked => CapabilityAuthorityError::KeyRevoked,
        other => CapabilityAuthorityError::Identity(other),
    }
}

fn validate_bounds(
    valid_from_ms: u64,
    valid_until_ms: u64,
    call_limit: Option<u64>,
) -> Result<(), CapabilityAuthorityError> {
    if valid_from_ms > valid_until_ms {
        return Err(CapabilityAuthorityError::InvalidValidity);
    }
    if call_limit == Some(0) {
        return Err(CapabilityAuthorityError::InvalidCallLimit);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // Constructs the immutable authority record in one place.
fn new_record(
    request_digest: [u8; 32],
    issuer: IdentityBinding,
    holder: IdentityBinding,
    target: CapabilityTarget,
    rights: CapabilityRights,
    purpose_digest: Option<[u8; 32]>,
    valid_from_ms: u64,
    valid_until_ms: u64,
    delegation_depth_remaining: u8,
    call_limit: Option<u64>,
    parent: Option<CapabilityHandle>,
) -> CapabilityRecord {
    CapabilityRecord {
        handle: CapabilityHandle {
            capability_id: CapabilityId::from_bytes(derive_id(
                b"nlos/capability/id/v1",
                &[&request_digest],
            )),
            generation: Generation::INITIAL,
        },
        issuer: issuer.principal_id,
        issuer_control_domain: issuer.control_domain_id,
        holder: holder.principal_id,
        holder_control_domain: holder.control_domain_id,
        target,
        rights,
        purpose_digest,
        valid_from_ms,
        valid_until_ms,
        delegation_depth_remaining,
        call_limit,
        parent,
        revoked_at_ms: None,
    }
}

fn new_issue_receipt(
    record: CapabilityRecord,
    request_digest: [u8; 32],
    issued_at_ms: u64,
) -> CapabilityIssueReceipt {
    CapabilityIssueReceipt {
        receipt_id: ReceiptId::from_bytes(derive_id(
            b"nlos/capability-issue-receipt/id/v1",
            &[record.handle.capability_id.as_bytes(), &request_digest],
        )),
        capability_id: record.handle.capability_id,
        generation: record.handle.generation,
        parent: record.parent,
        issued_at_ms,
    }
}

fn insert_capability(
    transaction: &Transaction<'_>,
    record: CapabilityRecord,
    receipt: CapabilityIssueReceipt,
    idempotency_key: IdempotencyKey,
    request_digest: [u8; 32],
) -> Result<(), CapabilityAuthorityError> {
    let (parent_id, parent_generation) = encode_parent(record.parent)?;
    transaction.execute(
        "INSERT INTO capability_heads (
            capability_id, current_generation, issuer_principal_id,
            issuer_control_domain_id, holder_principal_id, holder_control_domain_id,
            target_kind, target_id, rights, purpose_digest, valid_from_ms, valid_until_ms,
            delegation_depth_remaining, call_limit, parent_capability_id,
            parent_generation, created_at_ms
         ) VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            record.handle.capability_id.as_bytes().as_slice(),
            record.issuer.as_bytes().as_slice(),
            record.issuer_control_domain.as_bytes().as_slice(),
            record.holder.as_bytes().as_slice(),
            record.holder_control_domain.as_bytes().as_slice(),
            record.target.kind(),
            record.target.bytes().as_slice(),
            encode_u64(record.rights.bits())?,
            record.purpose_digest,
            encode_u64(record.valid_from_ms)?,
            encode_u64(record.valid_until_ms)?,
            i64::from(record.delegation_depth_remaining),
            record.call_limit.map(encode_u64).transpose()?,
            parent_id,
            parent_generation,
            encode_u64(receipt.issued_at_ms)?,
        ],
    )?;
    transaction.execute(
        "INSERT INTO capability_versions (capability_id, generation, revoked_at_ms)
         VALUES (?1, 1, NULL)",
        [record.handle.capability_id.as_bytes().as_slice()],
    )?;
    transaction.execute(
        "INSERT INTO capability_issue_receipts (
            idempotency_key, request_digest, receipt_id, capability_id, generation,
            parent_capability_id, parent_generation, issued_at_ms
         ) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7)",
        params![
            idempotency_key.as_bytes().as_slice(),
            request_digest.as_slice(),
            receipt.receipt_id.as_bytes().as_slice(),
            record.handle.capability_id.as_bytes().as_slice(),
            parent_id,
            parent_generation,
            encode_u64(receipt.issued_at_ms)?,
        ],
    )?;
    Ok(())
}

fn load_issue_replay(
    transaction: &Transaction<'_>,
    idempotency_key: IdempotencyKey,
    request_digest: [u8; 32],
) -> Result<Option<CapabilityIssueDecision>, CapabilityAuthorityError> {
    let row = transaction
        .query_row(
            "SELECT request_digest, receipt_id, capability_id, generation,
                    parent_capability_id, parent_generation, issued_at_ms
             FROM capability_issue_receipts WHERE idempotency_key=?1",
            [idempotency_key.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .optional()?;
    let Some(row) = row else {
        return Ok(None);
    };
    if decode_array::<32>(row.0, "issue request digest")? != request_digest {
        return Err(CapabilityAuthorityError::IdempotencyConflict);
    }
    let capability_id = decode_id(row.2, CapabilityId::from_bytes, "capability id")?;
    let generation = decode_generation(row.3)?;
    let record = load_record_generation(transaction, capability_id, generation)?.ok_or(
        CapabilityAuthorityError::CorruptRecord("issued capability missing"),
    )?;
    let receipt = CapabilityIssueReceipt {
        receipt_id: decode_id(row.1, ReceiptId::from_bytes, "receipt id")?,
        capability_id,
        generation,
        parent: decode_parent(row.4, row.5)?,
        issued_at_ms: decode_u64(row.6)?,
    };
    Ok(Some(CapabilityIssueDecision::Replayed(record, receipt)))
}

fn load_revocation_replay(
    transaction: &Transaction<'_>,
    idempotency_key: IdempotencyKey,
    request_digest: [u8; 32],
) -> Result<Option<CapabilityRevocationReceipt>, CapabilityAuthorityError> {
    let row = transaction
        .query_row(
            "SELECT request_digest, receipt_id, capability_id, prior_generation,
                    resulting_generation, revoker_principal_id, revoked_at_ms
             FROM capability_revocation_receipts WHERE idempotency_key=?1",
            [idempotency_key.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .optional()?;
    let Some(row) = row else {
        return Ok(None);
    };
    if decode_array::<32>(row.0, "revocation request digest")? != request_digest {
        return Err(CapabilityAuthorityError::IdempotencyConflict);
    }
    Ok(Some(CapabilityRevocationReceipt {
        receipt_id: decode_id(row.1, ReceiptId::from_bytes, "receipt id")?,
        capability_id: decode_id(row.2, CapabilityId::from_bytes, "capability id")?,
        prior_generation: decode_generation(row.3)?,
        resulting_generation: decode_generation(row.4)?,
        revoker: decode_id(row.5, PrincipalId::from_bytes, "revoker principal")?,
        revoked_at_ms: decode_u64(row.6)?,
    }))
}

fn load_exact_current(
    connection: &Connection,
    handle: CapabilityHandle,
) -> Result<CapabilityRecord, CapabilityAuthorityError> {
    let current = load_current_record(connection, handle.capability_id)?.ok_or(
        CapabilityAuthorityError::CapabilityNotFound(handle.capability_id),
    )?;
    if current.handle.generation != handle.generation {
        return Err(CapabilityAuthorityError::GenerationFenceConflict);
    }
    Ok(current)
}

fn load_current_record(
    connection: &Connection,
    capability_id: CapabilityId,
) -> Result<Option<CapabilityRecord>, CapabilityAuthorityError> {
    let generation = connection
        .query_row(
            "SELECT current_generation FROM capability_heads WHERE capability_id=?1",
            [capability_id.as_bytes().as_slice()],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .map(decode_generation)
        .transpose()?;
    match generation {
        Some(value) => load_record_generation(connection, capability_id, value),
        None => Ok(None),
    }
}

fn load_record_generation(
    connection: &Connection,
    capability_id: CapabilityId,
    generation: Generation,
) -> Result<Option<CapabilityRecord>, CapabilityAuthorityError> {
    connection
        .query_row(
            "SELECT h.issuer_principal_id, h.issuer_control_domain_id,
                    h.holder_principal_id, h.holder_control_domain_id,
                    h.target_kind, h.target_id, h.rights, h.purpose_digest,
                    h.valid_from_ms, h.valid_until_ms, h.delegation_depth_remaining,
                    h.call_limit, h.parent_capability_id, h.parent_generation,
                    v.revoked_at_ms
             FROM capability_heads h JOIN capability_versions v
               ON v.capability_id=h.capability_id AND v.generation=?2
             WHERE h.capability_id=?1",
            params![
                capability_id.as_bytes().as_slice(),
                encode_generation(generation)?,
            ],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, Option<Vec<u8>>>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, Option<i64>>(11)?,
                    row.get::<_, Option<Vec<u8>>>(12)?,
                    row.get::<_, Option<i64>>(13)?,
                    row.get::<_, Option<i64>>(14)?,
                ))
            },
        )
        .optional()?
        .map(|row| {
            let target_bytes = decode_array::<16>(row.5, "target id")?;
            Ok(CapabilityRecord {
                handle: CapabilityHandle {
                    capability_id,
                    generation,
                },
                issuer: decode_id(row.0, PrincipalId::from_bytes, "issuer")?,
                issuer_control_domain: decode_id(
                    row.1,
                    nlos_types::ControlDomainId::from_bytes,
                    "issuer domain",
                )?,
                holder: decode_id(row.2, PrincipalId::from_bytes, "holder")?,
                holder_control_domain: decode_id(
                    row.3,
                    nlos_types::ControlDomainId::from_bytes,
                    "holder domain",
                )?,
                target: CapabilityTarget::decode(row.4, target_bytes)
                    .ok_or(CapabilityAuthorityError::CorruptRecord("target kind"))?,
                rights: CapabilityRights::from_bits(decode_u64(row.6)?)
                    .ok_or(CapabilityAuthorityError::CorruptRecord("rights"))?,
                purpose_digest: row
                    .7
                    .map(|bytes| decode_array(bytes, "purpose digest"))
                    .transpose()?,
                valid_from_ms: decode_u64(row.8)?,
                valid_until_ms: decode_u64(row.9)?,
                delegation_depth_remaining: u8::try_from(row.10)
                    .map_err(|_| CapabilityAuthorityError::CorruptRecord("delegation depth"))?,
                call_limit: row.11.map(decode_u64).transpose()?,
                parent: decode_parent(row.12, row.13)?,
                revoked_at_ms: row.14.map(decode_u64).transpose()?,
            })
        })
        .transpose()
}

fn validate_active_chain(
    connection: &Connection,
    mut record: CapabilityRecord,
    at_ms: u64,
) -> Result<(), CapabilityAuthorityError> {
    for depth in 0..=u8::MAX {
        if record.revoked_at_ms.is_some() {
            return if depth == 0 {
                Err(CapabilityAuthorityError::CapabilityRevoked)
            } else {
                Err(CapabilityAuthorityError::AncestorRevokedOrFenced)
            };
        }
        if at_ms < record.valid_from_ms {
            return Err(CapabilityAuthorityError::CapabilityNotYetValid);
        }
        if at_ms > record.valid_until_ms {
            return Err(CapabilityAuthorityError::CapabilityExpired);
        }
        let Some(parent_handle) = record.parent else {
            return Ok(());
        };
        let Some(parent) = load_current_record(connection, parent_handle.capability_id)? else {
            return Err(CapabilityAuthorityError::AncestorRevokedOrFenced);
        };
        if parent.handle.generation != parent_handle.generation {
            return Err(CapabilityAuthorityError::AncestorRevokedOrFenced);
        }
        record = parent;
    }
    Err(CapabilityAuthorityError::CorruptRecord(
        "capability ancestor chain exceeds 256",
    ))
}

fn purpose_is_attenuated(parent: Option<[u8; 32]>, child: Option<[u8; 32]>) -> bool {
    match (parent, child) {
        (None, _) => true,
        (Some(expected), Some(actual)) => expected == actual,
        (Some(_), None) => false,
    }
}

fn call_limit_is_attenuated(parent: Option<u64>, child: Option<u64>) -> bool {
    match (parent, child) {
        (None, _) => true,
        (Some(maximum), Some(actual)) => actual <= maximum,
        (Some(_), None) => false,
    }
}

fn root_request_digest(request: IssueRootCapabilityRequest) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"nlos/capability-root-request/v1");
    hash_issue_fields(
        &mut hasher,
        request.issuer_key_id,
        request.holder_key_id,
        request.target,
        request.rights,
        request.purpose_digest,
        request.valid_from_ms,
        request.valid_until_ms,
        request.delegation_depth_remaining,
        request.call_limit,
        request.idempotency_key,
        request.issued_at_ms,
    );
    hasher.finalize().into()
}

fn delegate_request_digest(request: DelegateCapabilityRequest) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"nlos/capability-delegate-request/v1");
    hash_delegate_fields(&mut hasher, request);
    hasher.finalize().into()
}

/// Computes the domain-separated digest verified by `issue_root_signed`:
/// command kind 1 of [`COMMAND_MESSAGE_DOMAIN`] over every semantic field of
/// the issuance command.
#[must_use]
pub fn issue_root_command_message(command: IssueRootCapabilityRequest) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(COMMAND_MESSAGE_DOMAIN);
    hasher.update([COMMAND_KIND_ISSUE_ROOT]);
    hash_issue_fields(
        &mut hasher,
        command.issuer_key_id,
        command.holder_key_id,
        command.target,
        command.rights,
        command.purpose_digest,
        command.valid_from_ms,
        command.valid_until_ms,
        command.delegation_depth_remaining,
        command.call_limit,
        command.idempotency_key,
        command.issued_at_ms,
    );
    hasher.finalize().into()
}

/// Computes the domain-separated digest verified by `delegate_signed`:
/// command kind 2 of [`COMMAND_MESSAGE_DOMAIN`] over every semantic field of
/// the delegation command, including the delegate target.
#[must_use]
pub fn delegate_command_message(command: DelegateCapabilityRequest) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(COMMAND_MESSAGE_DOMAIN);
    hasher.update([COMMAND_KIND_DELEGATE]);
    hash_delegate_fields(&mut hasher, command);
    hasher.finalize().into()
}

/// Computes the domain-separated digest verified by `revoke_signed`: command
/// kind 3 of [`COMMAND_MESSAGE_DOMAIN`] over every semantic field of the
/// revocation command.
#[must_use]
pub fn revoke_command_message(command: RevokeCapabilityRequest) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(COMMAND_MESSAGE_DOMAIN);
    hasher.update([COMMAND_KIND_REVOKE]);
    hash_revoke_fields(&mut hasher, command);
    hasher.finalize().into()
}

fn hash_delegate_fields(hasher: &mut Sha256, request: DelegateCapabilityRequest) {
    hasher.update(request.parent.capability_id.as_bytes());
    hasher.update(request.parent.generation.get().to_be_bytes());
    hasher.update(request.delegator_key_id.as_bytes());
    hasher.update(request.recipient_key_id.as_bytes());
    hasher.update([request.target.kind()]);
    hasher.update(request.target.bytes());
    hasher.update(request.rights.bits().to_be_bytes());
    hash_optional_digest(hasher, request.purpose_digest);
    hasher.update(request.valid_from_ms.to_be_bytes());
    hasher.update(request.valid_until_ms.to_be_bytes());
    hasher.update([request.delegation_depth_remaining]);
    hash_optional_u64(hasher, request.call_limit);
    hasher.update(request.idempotency_key.as_bytes());
    hasher.update(request.delegated_at_ms.to_be_bytes());
}

#[allow(clippy::too_many_arguments)] // Fixed canonical field order for root issuance.
fn hash_issue_fields(
    hasher: &mut Sha256,
    issuer_key_id: KeyId,
    holder_key_id: KeyId,
    target: CapabilityTarget,
    rights: CapabilityRights,
    purpose_digest: Option<[u8; 32]>,
    valid_from_ms: u64,
    valid_until_ms: u64,
    delegation_depth_remaining: u8,
    call_limit: Option<u64>,
    idempotency_key: IdempotencyKey,
    issued_at_ms: u64,
) {
    hasher.update(issuer_key_id.as_bytes());
    hasher.update(holder_key_id.as_bytes());
    hasher.update([target.kind()]);
    hasher.update(target.bytes());
    hasher.update(rights.bits().to_be_bytes());
    hash_optional_digest(hasher, purpose_digest);
    hasher.update(valid_from_ms.to_be_bytes());
    hasher.update(valid_until_ms.to_be_bytes());
    hasher.update([delegation_depth_remaining]);
    hash_optional_u64(hasher, call_limit);
    hasher.update(idempotency_key.as_bytes());
    hasher.update(issued_at_ms.to_be_bytes());
}

fn revoke_request_digest(request: RevokeCapabilityRequest) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"nlos/capability-revoke-request/v1");
    hash_revoke_fields(&mut hasher, request);
    hasher.finalize().into()
}

fn hash_revoke_fields(hasher: &mut Sha256, request: RevokeCapabilityRequest) {
    hasher.update(request.handle.capability_id.as_bytes());
    hasher.update(request.handle.generation.get().to_be_bytes());
    hasher.update(request.revoker_key_id.as_bytes());
    hasher.update(request.idempotency_key.as_bytes());
    hasher.update(request.revoked_at_ms.to_be_bytes());
}

fn hash_optional_digest(hasher: &mut Sha256, value: Option<[u8; 32]>) {
    match value {
        Some(bytes) => {
            hasher.update([1]);
            hasher.update(bytes);
        }
        None => hasher.update([0]),
    }
}

fn hash_optional_u64(hasher: &mut Sha256, value: Option<u64>) {
    match value {
        Some(number) => {
            hasher.update([1]);
            hasher.update(number.to_be_bytes());
        }
        None => hasher.update([0]),
    }
}

fn derive_id(domain: &[u8], parts: &[&[u8]]) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    for part in parts {
        hasher.update(part);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    digest[..16]
        .try_into()
        .expect("SHA-256 prefix has fixed length")
}

fn encode_parent(
    parent: Option<CapabilityHandle>,
) -> Result<(Option<[u8; 16]>, Option<i64>), CapabilityAuthorityError> {
    match parent {
        Some(handle) => Ok((
            Some(handle.capability_id.into_bytes()),
            Some(encode_generation(handle.generation)?),
        )),
        None => Ok((None, None)),
    }
}

fn decode_parent(
    id: Option<Vec<u8>>,
    generation: Option<i64>,
) -> Result<Option<CapabilityHandle>, CapabilityAuthorityError> {
    match (id, generation) {
        (Some(bytes), Some(value)) => Ok(Some(CapabilityHandle {
            capability_id: decode_id(bytes, CapabilityId::from_bytes, "parent capability")?,
            generation: decode_generation(value)?,
        })),
        (None, None) => Ok(None),
        _ => Err(CapabilityAuthorityError::CorruptRecord("partial parent")),
    }
}

fn encode_generation(generation: Generation) -> Result<i64, CapabilityAuthorityError> {
    encode_u64(generation.get())
}

fn decode_generation(value: i64) -> Result<Generation, CapabilityAuthorityError> {
    let value = decode_u64(value)?;
    NonZeroU64::new(value)
        .map(Generation::new)
        .ok_or(CapabilityAuthorityError::CorruptRecord("zero generation"))
}

fn encode_u64(value: u64) -> Result<i64, CapabilityAuthorityError> {
    i64::try_from(value).map_err(|_| CapabilityAuthorityError::CorruptRecord("u64 exceeds SQLite"))
}

fn decode_u64(value: i64) -> Result<u64, CapabilityAuthorityError> {
    u64::try_from(value).map_err(|_| CapabilityAuthorityError::CorruptRecord("negative integer"))
}

fn decode_array<const N: usize>(
    bytes: Vec<u8>,
    field: &'static str,
) -> Result<[u8; N], CapabilityAuthorityError> {
    bytes
        .try_into()
        .map_err(|_| CapabilityAuthorityError::CorruptRecord(field))
}

fn decode_id<T>(
    bytes: Vec<u8>,
    constructor: impl FnOnce([u8; 16]) -> T,
    field: &'static str,
) -> Result<T, CapabilityAuthorityError> {
    Ok(constructor(decode_array(bytes, field)?))
}
