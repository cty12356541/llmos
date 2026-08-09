//! Durable local Principal/ControlDomain/key authority.
//!
//! This Stage-B reference slice owns identity assignment, versioned identity
//! snapshots, public signing-key validity/revocation, and strict Ed25519
//! verification for Semantic admission. Private key custody and general
//! Capability authorization remain separate authorities.

mod model;
mod schema;

use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use ed25519_dalek::{Signature, VerifyingKey};
use nlos_types::{
    ControlDomainId, Generation, IdempotencyKey, IdentitySnapshotId, KeyId, PrincipalId, ReceiptId,
    SemanticEventId,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

pub use model::{
    BootstrapDecision, BootstrapPrincipalRequest, Ed25519PublicKey, Ed25519Signature,
    IdentityBinding, KeyPurpose, KeyRevocationDecision, KeyRevocationReceipt, RevokeKeyRequest,
    VerifiedSemanticSigner, VerifySemanticSignatureRequest,
};

const SCHEMA_VERSION: i64 = 1;
const CHANGE_BOOTSTRAP: i64 = 1;
const CHANGE_KEY_REVOCATION: i64 = 2;

#[derive(Debug)]
pub enum IdentityAuthorityError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    DurabilityUnavailable {
        journal_mode: String,
        synchronous: i64,
    },
    SchemaVersionUnsupported(i64),
    InvalidKeyValidity,
    InvalidPublicKey,
    PrincipalNotFound(PrincipalId),
    ControlDomainNotFound(ControlDomainId),
    IdentitySnapshotNotFound(IdentitySnapshotId),
    KeyNotFound(KeyId),
    IdempotencyConflict,
    IdentitySnapshotFenceConflict,
    KeyGenerationFenceConflict,
    KeyRevoked,
    KeyNotYetValid,
    KeyExpired,
    KeyPurposeMismatch,
    SignerBindingMismatch,
    InvalidSignature,
    GenerationExhausted,
    CorruptRecord(&'static str),
    LockPoisoned,
}

impl fmt::Display for IdentityAuthorityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite identity authority failure: {error}"),
            Self::Io(error) => write!(formatter, "identity authority I/O failure: {error}"),
            Self::DurabilityUnavailable {
                journal_mode,
                synchronous,
            } => write!(
                formatter,
                "WAL/FULL durability unavailable: journal_mode={journal_mode}, synchronous={synchronous}"
            ),
            Self::SchemaVersionUnsupported(version) => {
                write!(
                    formatter,
                    "unsupported identity authority schema version {version}"
                )
            }
            Self::InvalidKeyValidity => formatter.write_str("invalid key validity interval"),
            Self::InvalidPublicKey => formatter.write_str("invalid Ed25519 public key"),
            Self::PrincipalNotFound(id) => write!(formatter, "principal {id:?} does not exist"),
            Self::ControlDomainNotFound(id) => {
                write!(formatter, "control domain {id:?} does not exist")
            }
            Self::IdentitySnapshotNotFound(id) => {
                write!(formatter, "identity snapshot {id:?} does not exist")
            }
            Self::KeyNotFound(id) => write!(formatter, "key {id:?} does not exist"),
            Self::IdempotencyConflict => {
                formatter.write_str("idempotency key was rebound to different identity input")
            }
            Self::IdentitySnapshotFenceConflict => {
                formatter.write_str("identity snapshot fence is stale")
            }
            Self::KeyGenerationFenceConflict => {
                formatter.write_str("key generation fence is stale")
            }
            Self::KeyRevoked => formatter.write_str("signing key is revoked"),
            Self::KeyNotYetValid => formatter.write_str("signing key is not yet valid"),
            Self::KeyExpired => formatter.write_str("signing key has expired"),
            Self::KeyPurposeMismatch => {
                formatter.write_str("key is not authorized for semantic event signing")
            }
            Self::SignerBindingMismatch => {
                formatter.write_str("principal, control domain, and key binding do not match")
            }
            Self::InvalidSignature => formatter.write_str("semantic event signature is invalid"),
            Self::GenerationExhausted => formatter.write_str("generation space exhausted"),
            Self::CorruptRecord(reason) => write!(formatter, "corrupt durable record: {reason}"),
            Self::LockPoisoned => formatter.write_str("identity authority writer lock is poisoned"),
        }
    }
}

impl Error for IdentityAuthorityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for IdentityAuthorityError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

pub struct IdentityAuthority {
    connection: Mutex<Connection>,
}

impl IdentityAuthority {
    /// Opens `<root>/identity-authority.db` with WAL/FULL durability.
    ///
    /// # Errors
    ///
    /// Fails when storage, durability configuration, or schema validation
    /// cannot be established.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, IdentityAuthorityError> {
        std::fs::create_dir_all(root.as_ref()).map_err(IdentityAuthorityError::Io)?;
        let mut connection = Connection::open(root.as_ref().join("identity-authority.db"))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let journal_mode: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let synchronous: i64 =
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 {
            return Err(IdentityAuthorityError::DurabilityUnavailable {
                journal_mode,
                synchronous,
            });
        }
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => schema::migrate_v1(&mut connection)?,
            SCHEMA_VERSION => {}
            other => return Err(IdentityAuthorityError::SchemaVersionUnsupported(other)),
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Atomically creates an authority-assigned Principal, single-member
    /// `ControlDomain`, initial identity snapshot, and semantic signing key.
    /// This method is a trusted local-bootstrap boundary, not an untrusted IPC
    /// registration endpoint.
    ///
    /// # Errors
    ///
    /// Fails for malformed keys/validity, idempotency rebinding, or storage
    /// failure.
    #[allow(clippy::too_many_lines)] // Keep the atomic bootstrap write set visible in one transaction.
    pub fn bootstrap_principal(
        &self,
        request: BootstrapPrincipalRequest,
    ) -> Result<BootstrapDecision, IdentityAuthorityError> {
        validate_bootstrap_request(request)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) =
            load_binding_by_bootstrap_key(&transaction, request.idempotency_key)?
        {
            if !bootstrap_matches(existing, request, &transaction)? {
                return Err(IdentityAuthorityError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(BootstrapDecision::Replayed(existing));
        }

        let principal_id = PrincipalId::from_bytes(derive_id(
            b"nlos/principal/id/v1",
            &[
                request.idempotency_key.as_bytes(),
                &request.principal_profile_digest,
                &request.public_key,
            ],
        ));
        let control_domain_id = ControlDomainId::from_bytes(derive_id(
            b"nlos/control-domain/id/v1",
            &[
                principal_id.as_bytes(),
                &request.control_domain_policy_digest,
            ],
        ));
        let key_id = KeyId::from_bytes(derive_id(
            b"nlos/key/id/v1",
            &[
                principal_id.as_bytes(),
                &[request.key_purpose as u8],
                &request.public_key,
            ],
        ));
        let snapshot_generation = Generation::INITIAL;
        let identity_snapshot_id = IdentitySnapshotId::from_bytes(derive_id(
            b"nlos/identity-snapshot/id/v1",
            &[
                control_domain_id.as_bytes(),
                &snapshot_generation.get().to_be_bytes(),
                principal_id.as_bytes(),
                key_id.as_bytes(),
            ],
        ));
        let binding = IdentityBinding {
            principal_id,
            control_domain_id,
            identity_snapshot_id,
            snapshot_generation,
            key_id,
            key_generation: Generation::INITIAL,
            key_purpose: request.key_purpose,
            public_key: request.public_key,
            key_valid_from_ms: request.key_valid_from_ms,
            key_valid_until_ms: request.key_valid_until_ms,
            key_revoked_at_ms: None,
        };

        transaction.execute(
            "INSERT INTO principals (
                principal_id, bootstrap_idempotency_key, profile_digest, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                principal_id.as_bytes().as_slice(),
                request.idempotency_key.as_bytes().as_slice(),
                request.principal_profile_digest.as_slice(),
                encode_u64(request.created_at_ms)?,
            ],
        )?;
        transaction.execute(
            "INSERT INTO control_domains (
                control_domain_id, current_snapshot_id, current_generation,
                policy_digest, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, 1, ?3, ?4, ?4)",
            params![
                control_domain_id.as_bytes().as_slice(),
                identity_snapshot_id.as_bytes().as_slice(),
                request.control_domain_policy_digest.as_slice(),
                encode_u64(request.created_at_ms)?,
            ],
        )?;
        insert_snapshot(
            &transaction,
            identity_snapshot_id,
            control_domain_id,
            snapshot_generation,
            None,
            request.control_domain_policy_digest,
            request.created_at_ms,
            CHANGE_BOOTSTRAP,
        )?;
        transaction.execute(
            "INSERT INTO snapshot_principals (identity_snapshot_id, principal_id) VALUES (?1, ?2)",
            params![
                identity_snapshot_id.as_bytes().as_slice(),
                principal_id.as_bytes().as_slice(),
            ],
        )?;
        transaction.execute(
            "INSERT INTO key_heads (
                key_id, principal_id, control_domain_id, current_generation, created_at_ms
             ) VALUES (?1, ?2, ?3, 1, ?4)",
            params![
                key_id.as_bytes().as_slice(),
                principal_id.as_bytes().as_slice(),
                control_domain_id.as_bytes().as_slice(),
                encode_u64(request.created_at_ms)?,
            ],
        )?;
        insert_key_version(&transaction, &binding)?;
        transaction.execute(
            "INSERT INTO snapshot_key_bindings (
                identity_snapshot_id, key_id, key_generation
             ) VALUES (?1, ?2, 1)",
            params![
                identity_snapshot_id.as_bytes().as_slice(),
                key_id.as_bytes().as_slice(),
            ],
        )?;
        transaction.commit()?;
        Ok(BootstrapDecision::Created(binding))
    }

    /// Revokes a key with both key-generation and identity-snapshot CAS. The
    /// old immutable snapshot remains queryable; the new current snapshot
    /// binds the revoked key generation.
    ///
    /// # Errors
    ///
    /// Fails on stale fences, idempotency rebinding, repeated revocation,
    /// generation exhaustion, or storage failure.
    #[allow(clippy::too_many_lines)] // Keep both fencing CAS operations in one auditable transaction.
    pub fn revoke_key(
        &self,
        request: RevokeKeyRequest,
    ) -> Result<KeyRevocationDecision, IdentityAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_revocation_by_key(&transaction, request.idempotency_key)? {
            if existing.key_id != request.key_id
                || existing.expected_key_generation != request.expected_key_generation
                || existing.expected_snapshot_id != request.expected_identity_snapshot_id
                || existing.receipt.revoked_at_ms != request.revoked_at_ms
            {
                return Err(IdentityAuthorityError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(KeyRevocationDecision::Replayed(existing.receipt));
        }

        let binding = load_current_binding(&transaction, request.key_id)?
            .ok_or(IdentityAuthorityError::KeyNotFound(request.key_id))?;
        if binding.key_generation != request.expected_key_generation {
            return Err(IdentityAuthorityError::KeyGenerationFenceConflict);
        }
        if binding.identity_snapshot_id != request.expected_identity_snapshot_id {
            return Err(IdentityAuthorityError::IdentitySnapshotFenceConflict);
        }
        if binding.key_revoked_at_ms.is_some() {
            return Err(IdentityAuthorityError::KeyRevoked);
        }
        if request.revoked_at_ms < binding.key_valid_from_ms {
            return Err(IdentityAuthorityError::InvalidKeyValidity);
        }
        let next_key_generation = binding
            .key_generation
            .checked_next()
            .ok_or(IdentityAuthorityError::GenerationExhausted)?;
        let next_snapshot_generation = binding
            .snapshot_generation
            .checked_next()
            .ok_or(IdentityAuthorityError::GenerationExhausted)?;
        let next_snapshot_id = IdentitySnapshotId::from_bytes(derive_id(
            b"nlos/identity-snapshot/id/v1",
            &[
                binding.control_domain_id.as_bytes(),
                &next_snapshot_generation.get().to_be_bytes(),
                binding.identity_snapshot_id.as_bytes(),
                request.idempotency_key.as_bytes(),
            ],
        ));
        let receipt_id = ReceiptId::from_bytes(derive_id(
            b"nlos/key-revocation-receipt/id/v1",
            &[
                request.key_id.as_bytes(),
                &next_key_generation.get().to_be_bytes(),
                next_snapshot_id.as_bytes(),
                request.idempotency_key.as_bytes(),
            ],
        ));
        let policy_digest = load_domain_policy(&transaction, binding.control_domain_id)?;
        let revoked_binding = IdentityBinding {
            identity_snapshot_id: next_snapshot_id,
            snapshot_generation: next_snapshot_generation,
            key_generation: next_key_generation,
            key_revoked_at_ms: Some(request.revoked_at_ms),
            ..binding
        };
        insert_key_version(&transaction, &revoked_binding)?;
        insert_snapshot(
            &transaction,
            next_snapshot_id,
            binding.control_domain_id,
            next_snapshot_generation,
            Some(binding.identity_snapshot_id),
            policy_digest,
            request.revoked_at_ms,
            CHANGE_KEY_REVOCATION,
        )?;
        transaction.execute(
            "INSERT INTO snapshot_principals (identity_snapshot_id, principal_id) VALUES (?1, ?2)",
            params![
                next_snapshot_id.as_bytes().as_slice(),
                binding.principal_id.as_bytes().as_slice(),
            ],
        )?;
        transaction.execute(
            "INSERT INTO snapshot_key_bindings (
                identity_snapshot_id, key_id, key_generation
             ) VALUES (?1, ?2, ?3)",
            params![
                next_snapshot_id.as_bytes().as_slice(),
                request.key_id.as_bytes().as_slice(),
                encode_generation(next_key_generation)?,
            ],
        )?;
        let key_changed = transaction.execute(
            "UPDATE key_heads SET current_generation=?1
             WHERE key_id=?2 AND current_generation=?3",
            params![
                encode_generation(next_key_generation)?,
                request.key_id.as_bytes().as_slice(),
                encode_generation(request.expected_key_generation)?,
            ],
        )?;
        if key_changed != 1 {
            return Err(IdentityAuthorityError::KeyGenerationFenceConflict);
        }
        let snapshot_changed = transaction.execute(
            "UPDATE control_domains
             SET current_snapshot_id=?1, current_generation=?2, updated_at_ms=?3
             WHERE control_domain_id=?4 AND current_snapshot_id=?5 AND current_generation=?6",
            params![
                next_snapshot_id.as_bytes().as_slice(),
                encode_generation(next_snapshot_generation)?,
                encode_u64(request.revoked_at_ms)?,
                binding.control_domain_id.as_bytes().as_slice(),
                request.expected_identity_snapshot_id.as_bytes().as_slice(),
                encode_generation(binding.snapshot_generation)?,
            ],
        )?;
        if snapshot_changed != 1 {
            return Err(IdentityAuthorityError::IdentitySnapshotFenceConflict);
        }
        let receipt = KeyRevocationReceipt {
            receipt_id,
            key_id: request.key_id,
            resulting_key_generation: next_key_generation,
            identity_snapshot_id: next_snapshot_id,
            snapshot_generation: next_snapshot_generation,
            revoked_at_ms: request.revoked_at_ms,
        };
        transaction.execute(
            "INSERT INTO key_revocations (
                idempotency_key, receipt_id, key_id, expected_key_generation,
                expected_snapshot_id, resulting_key_generation,
                resulting_snapshot_id, resulting_snapshot_generation, revoked_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                request.idempotency_key.as_bytes().as_slice(),
                receipt_id.as_bytes().as_slice(),
                request.key_id.as_bytes().as_slice(),
                encode_generation(request.expected_key_generation)?,
                request.expected_identity_snapshot_id.as_bytes().as_slice(),
                encode_generation(next_key_generation)?,
                next_snapshot_id.as_bytes().as_slice(),
                encode_generation(next_snapshot_generation)?,
                encode_u64(request.revoked_at_ms)?,
            ],
        )?;
        transaction.commit()?;
        Ok(KeyRevocationDecision::Revoked(receipt))
    }

    /// Validates current Principal/ControlDomain/key binding, purpose,
    /// validity, revocation state, and the v0.5 semantic signature preimage.
    ///
    /// # Errors
    ///
    /// Returns a typed fail-closed error for unknown/mismatched identities,
    /// invalid key state, malformed keys, or signature failure.
    pub fn verify_semantic_signature(
        &self,
        request: VerifySemanticSignatureRequest,
    ) -> Result<VerifiedSemanticSigner, IdentityAuthorityError> {
        let connection = self.lock()?;
        let binding = load_current_binding(&connection, request.key_id)?
            .ok_or(IdentityAuthorityError::KeyNotFound(request.key_id))?;
        if binding.principal_id != request.issuer
            || binding.control_domain_id != request.control_domain_id
        {
            return Err(IdentityAuthorityError::SignerBindingMismatch);
        }
        if binding.key_purpose != KeyPurpose::SemanticEventSigning {
            return Err(IdentityAuthorityError::KeyPurposeMismatch);
        }
        if binding.key_revoked_at_ms.is_some() {
            return Err(IdentityAuthorityError::KeyRevoked);
        }
        if request.admitted_at_ms < binding.key_valid_from_ms {
            return Err(IdentityAuthorityError::KeyNotYetValid);
        }
        if request.admitted_at_ms > binding.key_valid_until_ms {
            return Err(IdentityAuthorityError::KeyExpired);
        }
        let verifying_key = VerifyingKey::from_bytes(&binding.public_key)
            .map_err(|_| IdentityAuthorityError::InvalidPublicKey)?;
        let signature = Signature::from_bytes(&request.signature);
        verifying_key
            .verify_strict(&semantic_signature_message(request.event_id), &signature)
            .map_err(|_| IdentityAuthorityError::InvalidSignature)?;
        Ok(VerifiedSemanticSigner {
            principal_id: binding.principal_id,
            control_domain_id: binding.control_domain_id,
            identity_snapshot_id: binding.identity_snapshot_id,
            snapshot_generation: binding.snapshot_generation,
            key_id: binding.key_id,
            key_generation: binding.key_generation,
        })
    }

    /// Returns the current durable binding for a key.
    ///
    /// # Errors
    ///
    /// Fails when the key does not exist, storage is corrupt, or `SQLite` fails.
    pub fn inspect_current_binding(
        &self,
        key_id: KeyId,
    ) -> Result<IdentityBinding, IdentityAuthorityError> {
        let connection = self.lock()?;
        load_current_binding(&connection, key_id)?
            .ok_or(IdentityAuthorityError::KeyNotFound(key_id))
    }

    /// Reads the immutable Principal/key state recorded by an exact identity
    /// snapshot, even after the current snapshot advances.
    ///
    /// # Errors
    ///
    /// Fails when the snapshot/key binding does not exist, storage is corrupt,
    /// or `SQLite` fails.
    pub fn inspect_binding_at_snapshot(
        &self,
        identity_snapshot_id: IdentitySnapshotId,
        key_id: KeyId,
    ) -> Result<IdentityBinding, IdentityAuthorityError> {
        let connection = self.lock()?;
        load_binding_at_snapshot(&connection, identity_snapshot_id, key_id)?.ok_or(
            IdentityAuthorityError::IdentitySnapshotNotFound(identity_snapshot_id),
        )
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, IdentityAuthorityError> {
        self.connection
            .lock()
            .map_err(|_| IdentityAuthorityError::LockPoisoned)
    }
}

/// Computes the exact digest signed by a v0.5 semantic event issuer.
#[must_use]
pub fn semantic_signature_message(event_id: SemanticEventId) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/semantic-signature/v1");
    hasher.update(event_id.as_bytes());
    hasher.finalize().into()
}

fn validate_bootstrap_request(
    request: BootstrapPrincipalRequest,
) -> Result<(), IdentityAuthorityError> {
    if request.key_valid_from_ms > request.key_valid_until_ms {
        return Err(IdentityAuthorityError::InvalidKeyValidity);
    }
    VerifyingKey::from_bytes(&request.public_key)
        .map_err(|_| IdentityAuthorityError::InvalidPublicKey)?;
    Ok(())
}

fn bootstrap_matches(
    binding: IdentityBinding,
    request: BootstrapPrincipalRequest,
    transaction: &Transaction<'_>,
) -> Result<bool, IdentityAuthorityError> {
    let (profile, policy, created): (Vec<u8>, Vec<u8>, i64) = transaction.query_row(
        "SELECT p.profile_digest, d.policy_digest, p.created_at_ms
         FROM principals p
         JOIN control_domains d ON d.control_domain_id=?1
         WHERE p.principal_id=?2",
        params![
            binding.control_domain_id.as_bytes().as_slice(),
            binding.principal_id.as_bytes().as_slice(),
        ],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok(profile.as_slice() == request.principal_profile_digest
        && policy.as_slice() == request.control_domain_policy_digest
        && decode_u64(created)? == request.created_at_ms
        && binding.public_key == request.public_key
        && binding.key_purpose == request.key_purpose
        && binding.key_valid_from_ms == request.key_valid_from_ms
        && binding.key_valid_until_ms == request.key_valid_until_ms)
}

#[allow(clippy::too_many_arguments)] // Mirrors the immutable snapshot row without a mutable builder.
fn insert_snapshot(
    transaction: &Transaction<'_>,
    snapshot_id: IdentitySnapshotId,
    domain_id: ControlDomainId,
    generation: Generation,
    prior_snapshot_id: Option<IdentitySnapshotId>,
    policy_digest: [u8; 32],
    effective_at_ms: u64,
    change_kind: i64,
) -> Result<(), IdentityAuthorityError> {
    transaction.execute(
        "INSERT INTO identity_snapshots (
            identity_snapshot_id, control_domain_id, generation, prior_snapshot_id,
            policy_digest, effective_at_ms, change_kind
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            snapshot_id.as_bytes().as_slice(),
            domain_id.as_bytes().as_slice(),
            encode_generation(generation)?,
            prior_snapshot_id.map(IdentitySnapshotId::into_bytes),
            policy_digest.as_slice(),
            encode_u64(effective_at_ms)?,
            change_kind,
        ],
    )?;
    Ok(())
}

fn insert_key_version(
    transaction: &Transaction<'_>,
    binding: &IdentityBinding,
) -> Result<(), IdentityAuthorityError> {
    transaction.execute(
        "INSERT INTO key_versions (
            key_id, generation, purpose, algorithm, public_key,
            valid_from_ms, valid_until_ms, revoked_at_ms
         ) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7)",
        params![
            binding.key_id.as_bytes().as_slice(),
            encode_generation(binding.key_generation)?,
            binding.key_purpose.encode(),
            binding.public_key.as_slice(),
            encode_u64(binding.key_valid_from_ms)?,
            encode_u64(binding.key_valid_until_ms)?,
            binding.key_revoked_at_ms.map(encode_u64).transpose()?,
        ],
    )?;
    Ok(())
}

fn load_binding_by_bootstrap_key(
    transaction: &Transaction<'_>,
    idempotency_key: IdempotencyKey,
) -> Result<Option<IdentityBinding>, IdentityAuthorityError> {
    let key_id = transaction
        .query_row(
            "SELECT kh.key_id
             FROM principals p JOIN key_heads kh ON kh.principal_id=p.principal_id
             WHERE p.bootstrap_idempotency_key=?1",
            [idempotency_key.as_bytes().as_slice()],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?
        .map(|bytes| decode_id::<16, _>(bytes, KeyId::from_bytes, "key id"))
        .transpose()?;
    match key_id {
        Some(id) => load_current_binding(transaction, id),
        None => Ok(None),
    }
}

fn load_current_binding(
    connection: &Connection,
    key_id: KeyId,
) -> Result<Option<IdentityBinding>, IdentityAuthorityError> {
    connection
        .query_row(
            "SELECT kh.principal_id, kh.control_domain_id,
                    d.current_snapshot_id, d.current_generation,
                    kh.key_id, kv.generation, kv.purpose, kv.public_key,
                    kv.valid_from_ms, kv.valid_until_ms, kv.revoked_at_ms
             FROM key_heads kh
             JOIN key_versions kv
               ON kv.key_id=kh.key_id AND kv.generation=kh.current_generation
             JOIN control_domains d ON d.control_domain_id=kh.control_domain_id
             JOIN snapshot_principals sp
               ON sp.identity_snapshot_id=d.current_snapshot_id
              AND sp.principal_id=kh.principal_id
             JOIN snapshot_key_bindings skb
               ON skb.identity_snapshot_id=d.current_snapshot_id
              AND skb.key_id=kh.key_id AND skb.key_generation=kh.current_generation
             WHERE kh.key_id=?1",
            [key_id.as_bytes().as_slice()],
            |row| {
                let principal: Vec<u8> = row.get(0)?;
                let domain: Vec<u8> = row.get(1)?;
                let snapshot: Vec<u8> = row.get(2)?;
                let snapshot_generation: i64 = row.get(3)?;
                let key: Vec<u8> = row.get(4)?;
                let key_generation: i64 = row.get(5)?;
                let purpose: i64 = row.get(6)?;
                let public_key: Vec<u8> = row.get(7)?;
                let valid_from: i64 = row.get(8)?;
                let valid_until: i64 = row.get(9)?;
                let revoked_at: Option<i64> = row.get(10)?;
                Ok((
                    principal,
                    domain,
                    snapshot,
                    snapshot_generation,
                    key,
                    key_generation,
                    purpose,
                    public_key,
                    valid_from,
                    valid_until,
                    revoked_at,
                ))
            },
        )
        .optional()?
        .map(decode_binding)
        .transpose()
}

fn load_binding_at_snapshot(
    connection: &Connection,
    identity_snapshot_id: IdentitySnapshotId,
    key_id: KeyId,
) -> Result<Option<IdentityBinding>, IdentityAuthorityError> {
    connection
        .query_row(
            "SELECT kh.principal_id, kh.control_domain_id,
                    s.identity_snapshot_id, s.generation,
                    kh.key_id, kv.generation, kv.purpose, kv.public_key,
                    kv.valid_from_ms, kv.valid_until_ms, kv.revoked_at_ms
             FROM identity_snapshots s
             JOIN snapshot_key_bindings skb
               ON skb.identity_snapshot_id=s.identity_snapshot_id
             JOIN key_heads kh ON kh.key_id=skb.key_id
             JOIN key_versions kv
               ON kv.key_id=skb.key_id AND kv.generation=skb.key_generation
             JOIN snapshot_principals sp
               ON sp.identity_snapshot_id=s.identity_snapshot_id
              AND sp.principal_id=kh.principal_id
             WHERE s.identity_snapshot_id=?1 AND skb.key_id=?2",
            params![
                identity_snapshot_id.as_bytes().as_slice(),
                key_id.as_bytes().as_slice(),
            ],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, Vec<u8>>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, Option<i64>>(10)?,
                ))
            },
        )
        .optional()?
        .map(decode_binding)
        .transpose()
}

type BindingRow = (
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    i64,
    Vec<u8>,
    i64,
    i64,
    Vec<u8>,
    i64,
    i64,
    Option<i64>,
);

fn decode_binding(row: BindingRow) -> Result<IdentityBinding, IdentityAuthorityError> {
    Ok(IdentityBinding {
        principal_id: decode_id::<16, _>(row.0, PrincipalId::from_bytes, "principal id")?,
        control_domain_id: decode_id::<16, _>(row.1, ControlDomainId::from_bytes, "domain id")?,
        identity_snapshot_id: decode_id::<16, _>(
            row.2,
            IdentitySnapshotId::from_bytes,
            "snapshot id",
        )?,
        snapshot_generation: decode_generation(row.3)?,
        key_id: decode_id::<16, _>(row.4, KeyId::from_bytes, "key id")?,
        key_generation: decode_generation(row.5)?,
        key_purpose: KeyPurpose::decode(row.6)
            .ok_or(IdentityAuthorityError::CorruptRecord("key purpose"))?,
        public_key: decode_array::<32>(row.7, "public key")?,
        key_valid_from_ms: decode_u64(row.8)?,
        key_valid_until_ms: decode_u64(row.9)?,
        key_revoked_at_ms: row.10.map(decode_u64).transpose()?,
    })
}

fn load_domain_policy(
    transaction: &Transaction<'_>,
    domain_id: ControlDomainId,
) -> Result<[u8; 32], IdentityAuthorityError> {
    let bytes = transaction
        .query_row(
            "SELECT policy_digest FROM control_domains WHERE control_domain_id=?1",
            [domain_id.as_bytes().as_slice()],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?
        .ok_or(IdentityAuthorityError::ControlDomainNotFound(domain_id))?;
    decode_array(bytes, "domain policy")
}

struct StoredRevocation {
    expected_key_generation: Generation,
    expected_snapshot_id: IdentitySnapshotId,
    key_id: KeyId,
    receipt: KeyRevocationReceipt,
}

fn load_revocation_by_key(
    transaction: &Transaction<'_>,
    idempotency_key: IdempotencyKey,
) -> Result<Option<StoredRevocation>, IdentityAuthorityError> {
    transaction
        .query_row(
            "SELECT receipt_id, key_id, expected_key_generation, expected_snapshot_id,
                    resulting_key_generation, resulting_snapshot_id,
                    resulting_snapshot_generation, revoked_at_ms
             FROM key_revocations WHERE idempotency_key=?1",
            [idempotency_key.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )
        .optional()?
        .map(|row| {
            let key_id = decode_id::<16, _>(row.1, KeyId::from_bytes, "key id")?;
            let expected_snapshot_id =
                decode_id::<16, _>(row.3, IdentitySnapshotId::from_bytes, "snapshot id")?;
            Ok(StoredRevocation {
                expected_key_generation: decode_generation(row.2)?,
                expected_snapshot_id,
                key_id,
                receipt: KeyRevocationReceipt {
                    receipt_id: decode_id::<16, _>(row.0, ReceiptId::from_bytes, "receipt id")?,
                    key_id,
                    resulting_key_generation: decode_generation(row.4)?,
                    identity_snapshot_id: decode_id::<16, _>(
                        row.5,
                        IdentitySnapshotId::from_bytes,
                        "snapshot id",
                    )?,
                    snapshot_generation: decode_generation(row.6)?,
                    revoked_at_ms: decode_u64(row.7)?,
                },
            })
        })
        .transpose()
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

fn encode_generation(generation: Generation) -> Result<i64, IdentityAuthorityError> {
    encode_u64(generation.get())
}

fn decode_generation(value: i64) -> Result<Generation, IdentityAuthorityError> {
    let value = decode_u64(value)?;
    NonZeroU64::new(value)
        .map(Generation::new)
        .ok_or(IdentityAuthorityError::CorruptRecord("zero generation"))
}

fn encode_u64(value: u64) -> Result<i64, IdentityAuthorityError> {
    i64::try_from(value).map_err(|_| IdentityAuthorityError::CorruptRecord("u64 exceeds SQLite"))
}

fn decode_u64(value: i64) -> Result<u64, IdentityAuthorityError> {
    u64::try_from(value).map_err(|_| IdentityAuthorityError::CorruptRecord("negative integer"))
}

fn decode_array<const N: usize>(
    bytes: Vec<u8>,
    field: &'static str,
) -> Result<[u8; N], IdentityAuthorityError> {
    bytes
        .try_into()
        .map_err(|_| IdentityAuthorityError::CorruptRecord(field))
}

fn decode_id<const N: usize, T>(
    bytes: Vec<u8>,
    constructor: impl FnOnce([u8; N]) -> T,
    field: &'static str,
) -> Result<T, IdentityAuthorityError> {
    Ok(constructor(decode_array(bytes, field)?))
}
