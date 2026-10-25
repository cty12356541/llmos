//! Production signing key custody vault, key-epoch lifecycle, and durable sign
//! audit ledger (`#17 C-CUSTODY-GATEWAY`, ADR-0017 Constraint 4 & ADR-0022).
//!
//! Private signing seeds never cross the vault boundary as plaintext values:
//! seeds are wrapped under a caller-supplied 32-byte KEK before reaching
//! `SQLite` (`WAL` + `FULL`, `STRICT` tables, immutability triggers), and every
//! custody signature atomically advances a strictly monotonic `sign_seq` audit
//! counter before returning a [`CustodySignAuditReceipt`].
//!
//! Because `nlos-capability` keeps its runtime dependency graph minimal
//! (`sha2` + `rusqlite` + `nlos-identity` + `nlos-types`), this module embeds a
//! self-contained, `#![forbid(unsafe_code)]` RFC 8032 `Ed25519` engine over
//! `sha2::Sha512` that is verified byte-for-byte against `ed25519-dalek` in
//! unit tests and against [`IdentityAuthority`] strict verification at runtime.

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

pub use nlos_identity::{
    BootstrapDecision, BootstrapPrincipalRequest, CustodyProfile, Ed25519PublicKey,
    Ed25519Signature, IdentityAuthority, IdentityAuthorityError, IdentityBinding, KeyPurpose,
    KeyRevocationDecision, KeyRevocationReceipt, KeyRotationDecision, KeyRotationReceipt,
    RegisterCustodyBindingRequest, RevokeKeyRequest, RotateKeyRequest,
    VerifiedSemanticAuthoritySigner, VerifySemanticAuthoritySignatureRequest,
};
use nlos_types::{ControlDomainId, IdempotencyKey, KeyId, PrincipalId, ReceiptId};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::{
    CapabilityAuthority, CapabilityAuthorityError, CapabilityIssueDecision,
    IssueRootCapabilityRequest, SignedIssueRootCapabilityRequest, issue_root_command_message,
};

const CUSTODY_SCHEMA_VERSION: i64 = 1;
const CUSTODY_DB_FILENAME: &str = "signing-key-custody.db";
const VAULT_META_SINGLETON_ID: i64 = 1;

const DOMAIN_KEK_COMMITMENT: &[u8] = b"nlos/custody/kek-commitment/v1";
const DOMAIN_SEED_KEYSTREAM: &[u8] = b"nlos/custody/seed-keystream/v1";
const DOMAIN_SEED_AUTH_TAG: &[u8] = b"nlos/custody/seed-auth-tag/v1";
const DOMAIN_SIGNING_DIGEST: &[u8] = b"nlos/custody/signing-digest/v1";
const DOMAIN_AUDIT_RECEIPT_ID: &[u8] = b"nlos/custody/sign-audit-receipt/v1";
const DOMAIN_ROTATION_RECEIPT_ID: &[u8] = b"nlos/custody/rotation-receipt/v1";
const DOMAIN_REVOCATION_TOMBSTONE_ID: &[u8] = b"nlos/custody/revocation-tombstone/v1";

/// Computes a canonical domain-separated `SHA-256` digest with explicit
/// big-endian length framing over each input slice.
#[must_use]
pub fn custody_domain_sha256(domain: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    let domain_len = u64::try_from(domain.len()).unwrap_or(u64::MAX);
    hasher.update(domain_len.to_be_bytes());
    hasher.update(domain);
    for part in parts {
        let part_len = u64::try_from(part.len()).unwrap_or(u64::MAX);
        hasher.update(part_len.to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// Computes the canonical 32-byte signing digest bound to `(domain_tag, payload_digest)`.
#[must_use]
pub fn compute_custody_signing_digest(domain_tag: &[u8], payload_digest: [u8; 32]) -> [u8; 32] {
    let domain_tag_digest = custody_domain_sha256(b"nlos/custody/domain-tag/v1", &[domain_tag]);
    custody_domain_sha256(
        DOMAIN_SIGNING_DIGEST,
        &[domain_tag_digest.as_slice(), payload_digest.as_slice()],
    )
}

/// Derives the RFC 8032 `Ed25519` public key from a 32-byte private seed.
#[must_use]
pub fn derive_ed25519_public_key_from_seed(seed: &[u8; 32]) -> Ed25519PublicKey {
    ed25519_rfc8032::derive_public_key(seed)
}

/// Verifies an RFC 8032 `Ed25519` signature under strict canonical rules
/// (canonical `S < L`, canonical `y < p`, non-negative zero sign, and non-weak
/// public key).
#[must_use]
pub fn verify_ed25519_signature_strict(
    public_key: &Ed25519PublicKey,
    message: &[u8],
    signature: &Ed25519Signature,
) -> bool {
    ed25519_rfc8032::verify_strict(public_key, message, signature)
}

/// Custody backing tier for a managed signing key slot (ADR-0022 & `#17`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CustodyTier {
    /// ADR-0022 operator/daemon local sealed key profile (`0600` / KEK-wrapped).
    OperatorFileSealed,
    /// OS keychain / DPAPI / platform secret-store sealed key profile.
    PlatformKeychainSealed,
    /// Hardware HSM / TPM / PKCS#11 attested non-exportable key profile.
    HardwareHsmAttested,
}

impl CustodyTier {
    #[must_use]
    pub const fn code(self) -> i64 {
        match self {
            Self::OperatorFileSealed => 1,
            Self::PlatformKeychainSealed => 2,
            Self::HardwareHsmAttested => 3,
        }
    }

    const fn from_code(code: i64) -> Result<Self, CustodyVaultError> {
        match code {
            1 => Ok(Self::OperatorFileSealed),
            2 => Ok(Self::PlatformKeychainSealed),
            3 => Ok(Self::HardwareHsmAttested),
            _ => Err(CustodyVaultError::CorruptRecord(
                "unknown custody_tier code",
            )),
        }
    }
}

/// Explicit key lifecycle state in [`SigningKeyCustodyVault`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CustodyKeyState {
    /// Eligible for new signing within `[valid_from_ms, valid_until_ms]` and
    /// live verification.
    Active,
    /// Superseded by key-epoch rotation: strictly barred from new signing,
    /// while still permitted to verify historical signatures issued before
    /// `rotated_at_ms`.
    RotatedVerifyOnly,
    /// Compromised tombstone: barred from new signing, and any signature at or
    /// after `revoked_from_ms` (plus all live operational verifications) fails
    /// closed.
    CompromisedRevoked,
    /// Past `valid_until_ms`: barred from new signing and live verification.
    Expired,
}

impl CustodyKeyState {
    #[must_use]
    pub const fn code(self) -> i64 {
        match self {
            Self::Active => 1,
            Self::RotatedVerifyOnly => 2,
            Self::CompromisedRevoked => 3,
            Self::Expired => 4,
        }
    }

    const fn from_code(code: i64) -> Result<Self, CustodyVaultError> {
        match code {
            1 => Ok(Self::Active),
            2 => Ok(Self::RotatedVerifyOnly),
            3 => Ok(Self::CompromisedRevoked),
            4 => Ok(Self::Expired),
            _ => Err(CustodyVaultError::CorruptRecord("unknown key_state code")),
        }
    }
}

/// Public metadata of a managed `(key_id, rotation_epoch)` slot. Private seed
/// material is never exposed here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CustodyKeySlot {
    pub key_id: KeyId,
    pub principal_id: PrincipalId,
    pub control_domain_id: ControlDomainId,
    pub authority_id: [u8; 16],
    pub key_purpose: KeyPurpose,
    pub public_key: Ed25519PublicKey,
    pub custody_tier: CustodyTier,
    pub rotation_epoch: u64,
    pub predecessor_key_id: Option<KeyId>,
    pub successor_key_id: Option<KeyId>,
    pub valid_from_ms: u64,
    pub valid_until_ms: u64,
    pub state: CustodyKeyState,
    pub provisioned_at_ms: u64,
    pub rotated_at_ms: Option<u64>,
    pub revoked_from_ms: Option<u64>,
    pub revocation_reason_digest: Option<[u8; 32]>,
}

/// Provisioning input for a new [`CustodyKeySlot`]. The 32-byte `seed` is
/// wrapped immediately under the vault KEK and zeroized before returning.
#[derive(Clone, Copy)]
pub struct ProvisionCustodyKeyRequest {
    pub key_id: KeyId,
    pub principal_id: PrincipalId,
    pub control_domain_id: ControlDomainId,
    pub authority_id: [u8; 16],
    pub key_purpose: KeyPurpose,
    pub custody_tier: CustodyTier,
    pub rotation_epoch: u64,
    pub valid_from_ms: u64,
    pub valid_until_ms: u64,
    pub seed: [u8; 32],
    pub provisioned_at_ms: u64,
}

impl fmt::Debug for ProvisionCustodyKeyRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProvisionCustodyKeyRequest")
            .field("key_id", &self.key_id)
            .field("principal_id", &self.principal_id)
            .field("control_domain_id", &self.control_domain_id)
            .field("authority_id", &self.authority_id)
            .field("key_purpose", &self.key_purpose)
            .field("custody_tier", &self.custody_tier)
            .field("rotation_epoch", &self.rotation_epoch)
            .field("valid_from_ms", &self.valid_from_ms)
            .field("valid_until_ms", &self.valid_until_ms)
            .field("seed", &"<redacted-32-byte-seed>")
            .field("provisioned_at_ms", &self.provisioned_at_ms)
            .finish()
    }
}

/// Input to bootstrap a new principal in [`IdentityAuthority`] directly from a
/// custody-sealed key seed without exposing the private key (ADR-0022).
#[derive(Clone, Copy)]
pub struct BootstrapCustodyPrincipalRequest {
    pub authority_id: [u8; 16],
    pub principal_profile_digest: [u8; 32],
    pub control_domain_policy_digest: [u8; 32],
    pub key_purpose: KeyPurpose,
    pub custody_tier: CustodyTier,
    pub valid_from_ms: u64,
    pub valid_until_ms: u64,
    pub seed: [u8; 32],
    pub bootstrap_idempotency_key: IdempotencyKey,
    pub custody_binding_idempotency_key: IdempotencyKey,
    pub provisioned_at_ms: u64,
}

impl fmt::Debug for BootstrapCustodyPrincipalRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BootstrapCustodyPrincipalRequest")
            .field("authority_id", &self.authority_id)
            .field("principal_profile_digest", &self.principal_profile_digest)
            .field(
                "control_domain_policy_digest",
                &self.control_domain_policy_digest,
            )
            .field("key_purpose", &self.key_purpose)
            .field("custody_tier", &self.custody_tier)
            .field("valid_from_ms", &self.valid_from_ms)
            .field("valid_until_ms", &self.valid_until_ms)
            .field("seed", &"<redacted-32-byte-seed>")
            .field("provisioned_at_ms", &self.provisioned_at_ms)
            .finish_non_exhaustive()
    }
}

/// Request to sign an exact 32-byte message digest inside the custody vault.
///
/// If `expected_rotation_epoch` is `Some(epoch)`, signing requires that exact
/// epoch to be `Active`; if `None`, signing resolves the currently `Active`
/// epoch of `key_id` (or fails closed if the latest epoch of `key_id` is
/// `RotatedVerifyOnly`, `CompromisedRevoked`, or `Expired`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CustodySignRequest<'a> {
    pub key_id: KeyId,
    pub expected_rotation_epoch: Option<u64>,
    pub domain_tag: &'a [u8],
    pub message_digest: [u8; 32],
    pub signed_at_ms: u64,
}

/// Immutable audit receipt emitted for every custody signing operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CustodySignAuditReceipt {
    pub receipt_id: ReceiptId,
    pub sign_seq: u64,
    pub key_id: KeyId,
    pub principal_id: PrincipalId,
    pub control_domain_id: ControlDomainId,
    pub authority_id: [u8; 16],
    pub custody_tier: CustodyTier,
    pub rotation_epoch: u64,
    pub domain_tag_digest: [u8; 32],
    pub message_digest: [u8; 32],
    pub signature: Ed25519Signature,
    pub signed_at_ms: u64,
}

/// Request to rotate an `Active(E)` custody key slot to `RotatedVerifyOnly(E)`
/// while atomically activating `(new_key_id, E + 1)` as `Active(E + 1)`.
///
/// `new_key_id` may equal `current_key_id` (matching [`IdentityAuthority::rotate_key`]
/// generation rotation on the same [`KeyId`]) or introduce a new [`KeyId`].
#[derive(Clone, Copy)]
pub struct RotateCustodyKeyEpochRequest {
    pub current_key_id: KeyId,
    pub expected_rotation_epoch: u64,
    pub new_key_id: KeyId,
    pub new_custody_tier: CustodyTier,
    pub new_valid_from_ms: u64,
    pub new_valid_until_ms: u64,
    pub new_seed: [u8; 32],
    pub rotated_at_ms: u64,
}

impl fmt::Debug for RotateCustodyKeyEpochRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RotateCustodyKeyEpochRequest")
            .field("current_key_id", &self.current_key_id)
            .field("expected_rotation_epoch", &self.expected_rotation_epoch)
            .field("new_key_id", &self.new_key_id)
            .field("new_custody_tier", &self.new_custody_tier)
            .field("new_valid_from_ms", &self.new_valid_from_ms)
            .field("new_valid_until_ms", &self.new_valid_until_ms)
            .field("new_seed", &"<redacted-32-byte-seed>")
            .field("rotated_at_ms", &self.rotated_at_ms)
            .finish()
    }
}

/// Immutable rotation receipt binding the verify-only predecessor slot and the
/// newly activated successor slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CustodyRotationReceipt {
    pub receipt_id: ReceiptId,
    pub previous_slot: CustodyKeySlot,
    pub active_slot: CustodyKeySlot,
    pub rotated_at_ms: u64,
}

/// Request to permanently mark a `(key_id, rotation_epoch)` slot
/// `CompromisedRevoked` from `revoked_from_ms` onward.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RevokeCompromisedKeyRequest {
    pub key_id: KeyId,
    pub rotation_epoch: u64,
    pub revoked_from_ms: u64,
    pub reason_digest: [u8; 32],
    pub recorded_at_ms: u64,
}

/// Immutable tombstone recorded when a custody key slot is revoked due to
/// compromise.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CustodyRevocationTombstone {
    pub tombstone_id: ReceiptId,
    pub key_id: KeyId,
    pub principal_id: PrincipalId,
    pub authority_id: [u8; 16],
    pub rotation_epoch: u64,
    pub revoked_from_ms: u64,
    pub reason_digest: [u8; 32],
    pub recorded_at_ms: u64,
}

/// Verified signer metadata returned by custody verification checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerifiedCustodySignature {
    pub key_id: KeyId,
    pub principal_id: PrincipalId,
    pub control_domain_id: ControlDomainId,
    pub authority_id: [u8; 16],
    pub custody_tier: CustodyTier,
    pub rotation_epoch: u64,
    pub key_state_at_check: CustodyKeyState,
}

/// Typed fail-closed errors of [`SqliteCustodyStore`] and
/// [`SigningKeyCustodyVault`].
#[derive(Debug)]
pub enum CustodyVaultError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    Identity(IdentityAuthorityError),
    Capability(CapabilityAuthorityError),
    DurabilityUnavailable {
        journal_mode: String,
        synchronous: i64,
    },
    SchemaVersionUnsupported(i64),
    WrappingKeyMismatch,
    SealedMaterialIntegrityFailure(KeyId),
    InvalidValidityWindow {
        valid_from_ms: u64,
        valid_until_ms: u64,
    },
    InvalidRotationEpoch(u64),
    KeySlotAlreadyExists {
        key_id: KeyId,
        rotation_epoch: u64,
    },
    KeySlotNotFound {
        key_id: KeyId,
        rotation_epoch: Option<u64>,
    },
    ActiveEpochConflict {
        principal_id: PrincipalId,
        authority_id: [u8; 16],
        existing_key_id: KeyId,
        existing_epoch: u64,
    },
    RotationEpochFenceConflict {
        expected: u64,
        actual: u64,
    },
    KeyStateNotSignable {
        key_id: KeyId,
        rotation_epoch: u64,
        state: CustodyKeyState,
    },
    KeyCompromised {
        key_id: KeyId,
        rotation_epoch: u64,
        revoked_from_ms: u64,
    },
    KeyNotYetValid {
        key_id: KeyId,
        signed_at_ms: u64,
        valid_from_ms: u64,
    },
    KeyExpired {
        key_id: KeyId,
        at_ms: u64,
        valid_until_ms: u64,
    },
    KeyRotatedVerifyOnly {
        key_id: KeyId,
        rotation_epoch: u64,
    },
    HistoricalTimestampAfterRotation {
        key_id: KeyId,
        signed_at_ms: u64,
        rotated_at_ms: u64,
    },
    SignerMetadataMismatch(KeyId),
    InvalidSignature,
    InvalidRevocationWindow {
        revoked_from_ms: u64,
        recorded_at_ms: u64,
    },
    CorruptRecord(&'static str),
    LockPoisoned,
}

impl fmt::Display for CustodyVaultError {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite custody vault failure: {error}"),
            Self::Io(error) => write!(formatter, "custody vault I/O failure: {error}"),
            Self::Identity(error) => write!(formatter, "identity authority failure: {error}"),
            Self::Capability(error) => write!(formatter, "capability authority failure: {error}"),
            Self::DurabilityUnavailable {
                journal_mode,
                synchronous,
            } => write!(
                formatter,
                "WAL/FULL durability unavailable for custody store: journal_mode={journal_mode}, synchronous={synchronous}"
            ),
            Self::SchemaVersionUnsupported(version) => {
                write!(formatter, "unsupported custody schema version {version}")
            }
            Self::WrappingKeyMismatch => {
                formatter.write_str("vault wrapping KEK does not match durable KEK commitment")
            }
            Self::SealedMaterialIntegrityFailure(key_id) => write!(
                formatter,
                "sealed key material authentication tag failed for {key_id:?}"
            ),
            Self::InvalidValidityWindow {
                valid_from_ms,
                valid_until_ms,
            } => write!(
                formatter,
                "invalid key validity window [{valid_from_ms}, {valid_until_ms}]"
            ),
            Self::InvalidRotationEpoch(epoch) => {
                write!(formatter, "rotation epoch must be >= 1, got {epoch}")
            }
            Self::KeySlotAlreadyExists {
                key_id,
                rotation_epoch,
            } => write!(
                formatter,
                "custody key slot ({key_id:?}, epoch {rotation_epoch}) already exists"
            ),
            Self::KeySlotNotFound {
                key_id,
                rotation_epoch,
            } => write!(
                formatter,
                "custody key slot ({key_id:?}, epoch {rotation_epoch:?}) does not exist"
            ),
            Self::ActiveEpochConflict {
                principal_id,
                existing_key_id,
                existing_epoch,
                ..
            } => write!(
                formatter,
                "principal {principal_id:?} already has active key slot ({existing_key_id:?}, epoch {existing_epoch})"
            ),
            Self::RotationEpochFenceConflict { expected, actual } => write!(
                formatter,
                "rotation epoch fence conflict: expected {expected}, actual {actual}"
            ),
            Self::KeyStateNotSignable {
                key_id,
                rotation_epoch,
                state,
            } => write!(
                formatter,
                "custody key slot ({key_id:?}, epoch {rotation_epoch}) in state {state:?} cannot sign new digests"
            ),
            Self::KeyCompromised {
                key_id,
                rotation_epoch,
                revoked_from_ms,
            } => write!(
                formatter,
                "custody key slot ({key_id:?}, epoch {rotation_epoch}) is compromised/revoked from {revoked_from_ms} ms"
            ),
            Self::KeyNotYetValid {
                key_id,
                signed_at_ms,
                valid_from_ms,
            } => write!(
                formatter,
                "custody key slot {key_id:?} not yet valid at {signed_at_ms} ms (valid_from={valid_from_ms})"
            ),
            Self::KeyExpired {
                key_id,
                at_ms,
                valid_until_ms,
            } => write!(
                formatter,
                "custody key slot {key_id:?} expired at {at_ms} ms (valid_until={valid_until_ms})"
            ),
            Self::KeyRotatedVerifyOnly {
                key_id,
                rotation_epoch,
            } => write!(
                formatter,
                "custody key slot ({key_id:?}, epoch {rotation_epoch}) is rotated verify-only"
            ),
            Self::HistoricalTimestampAfterRotation {
                key_id,
                signed_at_ms,
                rotated_at_ms,
            } => write!(
                formatter,
                "historical signature timestamp {signed_at_ms} ms exceeds rotation timestamp {rotated_at_ms} ms for {key_id:?}"
            ),
            Self::SignerMetadataMismatch(key_id) => write!(
                formatter,
                "signer binding metadata disagrees with custody slot {key_id:?}"
            ),
            Self::InvalidSignature => formatter.write_str("Ed25519 signature verification failed"),
            Self::InvalidRevocationWindow {
                revoked_from_ms,
                recorded_at_ms,
            } => write!(
                formatter,
                "revoked_from_ms ({revoked_from_ms}) cannot exceed recorded_at_ms ({recorded_at_ms})"
            ),
            Self::CorruptRecord(reason) => {
                write!(formatter, "corrupt custody vault record: {reason}")
            }
            Self::LockPoisoned => formatter.write_str("custody store mutex is poisoned"),
        }
    }
}

impl Error for CustodyVaultError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Identity(error) => Some(error),
            Self::Capability(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for CustodyVaultError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<IdentityAuthorityError> for CustodyVaultError {
    fn from(error: IdentityAuthorityError) -> Self {
        Self::Identity(error)
    }
}

impl From<CapabilityAuthorityError> for CustodyVaultError {
    fn from(error: CapabilityAuthorityError) -> Self {
        Self::Capability(error)
    }
}

/// Durable `SQLite` (`WAL` + `FULL`) backing store for [`SigningKeyCustodyVault`].
///
/// Opens `<root>/signing-key-custody.db`, keeping `CapabilityAuthority`'s
/// `capability-authority.db` (`SCHEMA_VERSION = 2`) completely untouched.
pub struct SqliteCustodyStore {
    connection: Mutex<Connection>,
}

impl SqliteCustodyStore {
    /// Opens `<root>/signing-key-custody.db` with `WAL` + `FULL` durability and
    /// `STRICT` schema enforcement.
    ///
    /// # Errors
    ///
    /// Fails closed if the directory cannot be created, `WAL`/`FULL` pragmas
    /// are unavailable, or `user_version` is unsupported.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, CustodyVaultError> {
        if !root.as_ref().to_string_lossy().starts_with("file:") {
            std::fs::create_dir_all(root.as_ref()).map_err(CustodyVaultError::Io)?;
        }
        let mut connection = Connection::open(root.as_ref().join(CUSTODY_DB_FILENAME))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let journal_mode: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let synchronous: i64 =
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 {
            return Err(CustodyVaultError::DurabilityUnavailable {
                journal_mode,
                synchronous,
            });
        }
        init_or_validate_schema(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Opens an ephemeral in-memory `SQLite` custody store for isolated tests.
    ///
    /// # Errors
    ///
    /// Returns [`CustodyVaultError::Sqlite`] if schema initialization fails.
    pub fn open_in_memory() -> Result<Self, CustodyVaultError> {
        let mut connection = Connection::open_in_memory()?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        init_or_validate_schema(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, CustodyVaultError> {
        self.connection
            .lock()
            .map_err(|_| CustodyVaultError::LockPoisoned)
    }
}

/// Production-grade signing key custody vault enforcing non-exportable KEK-sealed
/// seeds, key-epoch rotation (`Active -> RotatedVerifyOnly`), compromise
/// tombstones (`CompromisedRevoked`), and monotonic `CustodySignAuditReceipt`
/// persistence.
pub struct SigningKeyCustodyVault {
    store: SqliteCustodyStore,
    wrapping_kek: [u8; 32],
}

impl fmt::Debug for SigningKeyCustodyVault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SigningKeyCustodyVault")
            .field("wrapping_kek", &"<redacted-32-byte-kek>")
            .finish_non_exhaustive()
    }
}

impl SigningKeyCustodyVault {
    /// Opens `<root>/signing-key-custody.db` (`WAL` + `FULL`) and binds or
    /// verifies `wrapping_kek` against the durable KEK commitment.
    ///
    /// # Errors
    ///
    /// Fails closed on `SQLite`/durability errors or if `wrapping_kek` does not
    /// match the durable vault commitment.
    pub fn open_durable(
        root: impl AsRef<Path>,
        wrapping_kek: [u8; 32],
    ) -> Result<Self, CustodyVaultError> {
        let store = SqliteCustodyStore::open(root)?;
        Self::with_store(store, wrapping_kek)
    }

    /// Creates an in-memory custody vault bound to `wrapping_kek`.
    ///
    /// # Errors
    ///
    /// Fails if in-memory `SQLite` initialization fails.
    pub fn new_in_memory(wrapping_kek: [u8; 32]) -> Result<Self, CustodyVaultError> {
        let store = SqliteCustodyStore::open_in_memory()?;
        Self::with_store(store, wrapping_kek)
    }

    /// Binds `wrapping_kek` to an opened [`SqliteCustodyStore`], initializing
    /// or verifying the durable KEK commitment in `custody_vault_meta`.
    ///
    /// # Errors
    ///
    /// Returns [`CustodyVaultError::WrappingKeyMismatch`] if `store` was
    /// previously initialized with a different `wrapping_kek`.
    pub fn with_store(
        store: SqliteCustodyStore,
        wrapping_kek: [u8; 32],
    ) -> Result<Self, CustodyVaultError> {
        {
            let mut conn = store.lock()?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<(Vec<u8>, Vec<u8>)> = tx
                .query_row(
                    "SELECT vault_salt, kek_commitment FROM custody_vault_meta WHERE singleton_id = ?1",
                    params![VAULT_META_SINGLETON_ID],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((salt_vec, commitment_vec)) = existing {
                let salt = decode_array::<32>(salt_vec, "vault_salt")?;
                let stored_commitment = decode_array::<32>(commitment_vec, "kek_commitment")?;
                let expected = compute_kek_commitment(&salt, &wrapping_kek);
                if stored_commitment != expected {
                    return Err(CustodyVaultError::WrappingKeyMismatch);
                }
            } else {
                let salt = custody_domain_sha256(
                    b"nlos/custody/default-vault-salt/v1",
                    &[wrapping_kek.as_slice()],
                );
                let commitment = compute_kek_commitment(&salt, &wrapping_kek);
                tx.execute(
                    "INSERT INTO custody_vault_meta (
                        singleton_id, vault_salt, kek_commitment, last_sign_seq
                     ) VALUES (?1, ?2, ?3, 0)",
                    params![
                        VAULT_META_SINGLETON_ID,
                        salt.as_slice(),
                        commitment.as_slice(),
                    ],
                )?;
            }
            tx.commit()?;
        }
        Ok(Self {
            store,
            wrapping_kek,
        })
    }

    /// Provisions a new `Active` key slot, sealing the 32-byte `Ed25519` seed
    /// under `wrapping_kek` and returning only the public [`CustodyKeySlot`].
    ///
    /// # Errors
    ///
    /// Fails closed on invalid validity bounds, zero epoch, duplicate
    /// `(key_id, rotation_epoch)`, or an already-active key slot for the same
    /// `(principal_id, authority_id, key_purpose)` lineage.
    pub fn provision_key_slot(
        &self,
        mut request: ProvisionCustodyKeyRequest,
    ) -> Result<CustodyKeySlot, CustodyVaultError> {
        let result = self.provision_key_slot_inner(&request);
        zeroize_bytes(&mut request.seed);
        result
    }

    fn provision_key_slot_inner(
        &self,
        request: &ProvisionCustodyKeyRequest,
    ) -> Result<CustodyKeySlot, CustodyVaultError> {
        if request.rotation_epoch == 0 {
            return Err(CustodyVaultError::InvalidRotationEpoch(0));
        }
        if request.valid_until_ms <= request.valid_from_ms {
            return Err(CustodyVaultError::InvalidValidityWindow {
                valid_from_ms: request.valid_from_ms,
                valid_until_ms: request.valid_until_ms,
            });
        }
        let public_key = ed25519_rfc8032::derive_public_key(&request.seed);
        let wrapped = wrap_seed(
            &self.wrapping_kek,
            request.key_id,
            request.custody_tier,
            request.rotation_epoch,
            &public_key,
            &request.seed,
        );
        let mut conn = self.store.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if load_slot_epoch_optional(&tx, request.key_id, request.rotation_epoch)?.is_some() {
            return Err(CustodyVaultError::KeySlotAlreadyExists {
                key_id: request.key_id,
                rotation_epoch: request.rotation_epoch,
            });
        }
        if let Some(existing_active) = find_active_slot_for_lineage(
            &tx,
            request.principal_id,
            request.authority_id,
            request.key_purpose,
        )? {
            return Err(CustodyVaultError::ActiveEpochConflict {
                principal_id: request.principal_id,
                authority_id: request.authority_id,
                existing_key_id: existing_active.key_id,
                existing_epoch: existing_active.rotation_epoch,
            });
        }
        let slot = CustodyKeySlot {
            key_id: request.key_id,
            principal_id: request.principal_id,
            control_domain_id: request.control_domain_id,
            authority_id: request.authority_id,
            key_purpose: request.key_purpose,
            public_key,
            custody_tier: request.custody_tier,
            rotation_epoch: request.rotation_epoch,
            predecessor_key_id: None,
            successor_key_id: None,
            valid_from_ms: request.valid_from_ms,
            valid_until_ms: request.valid_until_ms,
            state: CustodyKeyState::Active,
            provisioned_at_ms: request.provisioned_at_ms,
            rotated_at_ms: None,
            revoked_from_ms: None,
            revocation_reason_digest: None,
        };
        insert_custody_slot(&tx, &slot, &wrapped)?;
        tx.commit()?;
        Ok(slot)
    }

    /// Bootstraps a new principal in [`IdentityAuthority`] AND provisions the
    /// corresponding `(key_id, epoch = 1)` custody slot inside this vault so
    /// no raw private key ever leaves custody (ADR-0022).
    ///
    /// # Errors
    ///
    /// Fails closed if either [`IdentityAuthority`] bootstrap/custody binding
    /// or custody slot provisioning fails.
    pub fn bootstrap_principal_under_custody(
        &self,
        identity: &IdentityAuthority,
        mut request: BootstrapCustodyPrincipalRequest,
    ) -> Result<(CustodyKeySlot, IdentityBinding), CustodyVaultError> {
        let public_key = ed25519_rfc8032::derive_public_key(&request.seed);
        let bootstrap_result = identity.bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: request.principal_profile_digest,
            control_domain_policy_digest: request.control_domain_policy_digest,
            public_key,
            key_purpose: request.key_purpose,
            key_valid_from_ms: request.valid_from_ms,
            key_valid_until_ms: request.valid_until_ms,
            idempotency_key: request.bootstrap_idempotency_key,
            created_at_ms: request.provisioned_at_ms,
        });
        let binding = match bootstrap_result {
            Ok(decision) => decision.binding(),
            Err(err) => {
                zeroize_bytes(&mut request.seed);
                return Err(CustodyVaultError::Identity(err));
            }
        };
        if let Err(err) = identity.register_custody_binding(RegisterCustodyBindingRequest {
            key_id: binding.key_id,
            expected_key_generation: binding.key_generation,
            custody_profile: CustodyProfile::TrustedLocalSoftware,
            idempotency_key: request.custody_binding_idempotency_key,
            registered_at_ms: request.provisioned_at_ms,
        }) {
            zeroize_bytes(&mut request.seed);
            return Err(CustodyVaultError::Identity(err));
        }
        let seed_copy = request.seed;
        zeroize_bytes(&mut request.seed);
        let slot = self.provision_key_slot(ProvisionCustodyKeyRequest {
            key_id: binding.key_id,
            principal_id: binding.principal_id,
            control_domain_id: binding.control_domain_id,
            authority_id: request.authority_id,
            key_purpose: request.key_purpose,
            custody_tier: request.custody_tier,
            rotation_epoch: binding.key_generation.get(),
            valid_from_ms: request.valid_from_ms,
            valid_until_ms: request.valid_until_ms,
            seed: seed_copy,
            provisioned_at_ms: request.provisioned_at_ms,
        })?;
        Ok((slot, binding))
    }

    /// Issues a signed root capability under [`CapabilityAuthority`] using an
    /// operator key held inside this custody vault (ADR-0022), returning both
    /// the [`CapabilityIssueDecision`] and the durable [`CustodySignAuditReceipt`].
    ///
    /// # Errors
    ///
    /// Fails closed if the custody key slot is not `Active` / within validity
    /// or if [`CapabilityAuthority::issue_root_signed`] rejects the command.
    pub fn issue_root_capability_under_custody(
        &self,
        capability_authority: &CapabilityAuthority,
        identity_authority: &IdentityAuthority,
        command: IssueRootCapabilityRequest,
    ) -> Result<(CapabilityIssueDecision, CustodySignAuditReceipt), CustodyVaultError> {
        let slot = self.inspect_latest_key_slot(command.issuer_key_id)?;
        let command_digest = issue_root_command_message(command);
        let audit = self.sign_digest_under_custody(CustodySignRequest {
            key_id: command.issuer_key_id,
            expected_rotation_epoch: None,
            domain_tag: b"nlos/capability/command/issue-root/v1",
            message_digest: command_digest,
            signed_at_ms: command.issued_at_ms,
        })?;
        let decision = capability_authority.issue_root_signed(
            identity_authority,
            SignedIssueRootCapabilityRequest {
                command,
                signer: slot.principal_id,
                signature: audit.signature,
            },
        )?;
        Ok((decision, audit))
    }

    /// Signs `request.message_digest` inside the vault using the sealed key
    /// slot `request.key_id`, atomically incrementing `sign_seq` and persisting
    /// an immutable [`CustodySignAuditReceipt`].
    ///
    /// # Errors
    ///
    /// Fails closed when the key is missing, `RotatedVerifyOnly`,
    /// `CompromisedRevoked`, `Expired`, outside `[valid_from_ms, valid_until_ms]`,
    /// or when sealed seed authentication fails.
    #[allow(clippy::too_many_lines)] // Keeps key-state validation, unsealing, and audit seq CAS in one transaction.
    pub fn sign_digest_under_custody(
        &self,
        request: CustodySignRequest<'_>,
    ) -> Result<CustodySignAuditReceipt, CustodyVaultError> {
        let mut conn = self.store.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (slot, wrapped) = match request.expected_rotation_epoch {
            Some(epoch) => load_slot_epoch_with_material(&tx, request.key_id, epoch)?.ok_or(
                CustodyVaultError::KeySlotNotFound {
                    key_id: request.key_id,
                    rotation_epoch: Some(epoch),
                },
            )?,
            None => resolve_signing_slot_with_material(&tx, request.key_id)?.ok_or(
                CustodyVaultError::KeySlotNotFound {
                    key_id: request.key_id,
                    rotation_epoch: None,
                },
            )?,
        };

        match slot.state {
            CustodyKeyState::Active => {}
            CustodyKeyState::CompromisedRevoked => {
                return Err(CustodyVaultError::KeyCompromised {
                    key_id: slot.key_id,
                    rotation_epoch: slot.rotation_epoch,
                    revoked_from_ms: slot.revoked_from_ms.unwrap_or(0),
                });
            }
            CustodyKeyState::RotatedVerifyOnly | CustodyKeyState::Expired => {
                return Err(CustodyVaultError::KeyStateNotSignable {
                    key_id: slot.key_id,
                    rotation_epoch: slot.rotation_epoch,
                    state: slot.state,
                });
            }
        }

        if request.signed_at_ms < slot.valid_from_ms {
            return Err(CustodyVaultError::KeyNotYetValid {
                key_id: slot.key_id,
                signed_at_ms: request.signed_at_ms,
                valid_from_ms: slot.valid_from_ms,
            });
        }
        if request.signed_at_ms > slot.valid_until_ms {
            return Err(CustodyVaultError::KeyExpired {
                key_id: slot.key_id,
                at_ms: request.signed_at_ms,
                valid_until_ms: slot.valid_until_ms,
            });
        }

        let mut seed = unwrap_seed(&self.wrapping_kek, &slot, &wrapped)?;
        let derived_pub = ed25519_rfc8032::derive_public_key(&seed);
        if derived_pub != slot.public_key {
            zeroize_bytes(&mut seed);
            return Err(CustodyVaultError::SealedMaterialIntegrityFailure(
                slot.key_id,
            ));
        }
        let signature = ed25519_rfc8032::sign(&seed, &request.message_digest);
        zeroize_bytes(&mut seed);

        let last_seq_i64: i64 = tx.query_row(
            "SELECT last_sign_seq FROM custody_vault_meta WHERE singleton_id = ?1",
            params![VAULT_META_SINGLETON_ID],
            |row| row.get(0),
        )?;
        let last_seq = decode_u64(last_seq_i64)?;
        let next_seq = last_seq
            .checked_add(1)
            .ok_or(CustodyVaultError::CorruptRecord("sign_seq overflow"))?;
        tx.execute(
            "UPDATE custody_vault_meta SET last_sign_seq = ?1 WHERE singleton_id = ?2 AND last_sign_seq = ?3",
            params![encode_u64(next_seq)?, VAULT_META_SINGLETON_ID, last_seq_i64],
        )?;

        let domain_tag_digest =
            custody_domain_sha256(b"nlos/custody/domain-tag/v1", &[request.domain_tag]);
        let receipt_id = ReceiptId::from_bytes(derive_id16(
            DOMAIN_AUDIT_RECEIPT_ID,
            &[
                &next_seq.to_be_bytes(),
                slot.key_id.as_bytes(),
                &slot.rotation_epoch.to_be_bytes(),
                domain_tag_digest.as_slice(),
                request.message_digest.as_slice(),
            ],
        ));
        let receipt = CustodySignAuditReceipt {
            receipt_id,
            sign_seq: next_seq,
            key_id: slot.key_id,
            principal_id: slot.principal_id,
            control_domain_id: slot.control_domain_id,
            authority_id: slot.authority_id,
            custody_tier: slot.custody_tier,
            rotation_epoch: slot.rotation_epoch,
            domain_tag_digest,
            message_digest: request.message_digest,
            signature,
            signed_at_ms: request.signed_at_ms,
        };
        tx.execute(
            "INSERT INTO custody_sign_audit_log (
                sign_seq, receipt_id, key_id, principal_id, control_domain_id,
                authority_id, custody_tier, rotation_epoch, domain_tag_digest,
                message_digest, signature, signed_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                encode_u64(receipt.sign_seq)?,
                receipt.receipt_id.as_bytes().as_slice(),
                receipt.key_id.as_bytes().as_slice(),
                receipt.principal_id.as_bytes().as_slice(),
                receipt.control_domain_id.as_bytes().as_slice(),
                receipt.authority_id.as_slice(),
                receipt.custody_tier.code(),
                encode_u64(receipt.rotation_epoch)?,
                receipt.domain_tag_digest.as_slice(),
                receipt.message_digest.as_slice(),
                receipt.signature.as_slice(),
                encode_u64(receipt.signed_at_ms)?,
            ],
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    /// Atomically transitions `(current_key_id, expected_rotation_epoch)` from
    /// `Active(E)` to `RotatedVerifyOnly(E)` and provisions
    /// `(new_key_id, E + 1)` as `Active(E + 1)`.
    ///
    /// # Errors
    ///
    /// Fails closed if `(current_key_id, expected_rotation_epoch)` is absent,
    /// not `Active`, if `(new_key_id, E + 1)` already exists, or if validity
    /// bounds are invalid.
    pub fn rotate_key_epoch(
        &self,
        mut request: RotateCustodyKeyEpochRequest,
    ) -> Result<CustodyRotationReceipt, CustodyVaultError> {
        let result = self.rotate_key_epoch_inner(&request);
        zeroize_bytes(&mut request.new_seed);
        result
    }

    /// Rotates a custody key epoch AND advances the corresponding key
    /// generation in [`IdentityAuthority`] in lockstep.
    ///
    /// # Errors
    ///
    /// Fails closed if either custody rotation or [`IdentityAuthority::rotate_key`]
    /// fails.
    pub fn rotate_key_epoch_with_identity(
        &self,
        identity: &IdentityAuthority,
        current_binding: &IdentityBinding,
        request: RotateCustodyKeyEpochRequest,
        identity_idempotency_key: IdempotencyKey,
        custody_binding_idempotency_key: IdempotencyKey,
    ) -> Result<(CustodyRotationReceipt, KeyRotationReceipt), CustodyVaultError> {
        if request.new_key_id != request.current_key_id
            || current_binding.key_id != request.current_key_id
        {
            return Err(CustodyVaultError::SignerMetadataMismatch(
                request.current_key_id,
            ));
        }
        let rotation = self.rotate_key_epoch(request)?;
        let id_receipt = identity
            .rotate_key(RotateKeyRequest {
                key_id: rotation.active_slot.key_id,
                expected_key_generation: current_binding.key_generation,
                expected_identity_snapshot_id: current_binding.identity_snapshot_id,
                new_public_key: rotation.active_slot.public_key,
                new_valid_from_ms: rotation.active_slot.valid_from_ms,
                new_valid_until_ms: rotation.active_slot.valid_until_ms,
                idempotency_key: identity_idempotency_key,
                rotated_at_ms: rotation.rotated_at_ms,
            })?
            .receipt();
        identity.register_custody_binding(RegisterCustodyBindingRequest {
            key_id: rotation.active_slot.key_id,
            expected_key_generation: id_receipt.resulting_key_generation,
            custody_profile: CustodyProfile::TrustedLocalSoftware,
            idempotency_key: custody_binding_idempotency_key,
            registered_at_ms: rotation.rotated_at_ms,
        })?;
        Ok((rotation, id_receipt))
    }

    #[allow(clippy::too_many_lines)]
    fn rotate_key_epoch_inner(
        &self,
        request: &RotateCustodyKeyEpochRequest,
    ) -> Result<CustodyRotationReceipt, CustodyVaultError> {
        if request.new_valid_until_ms <= request.new_valid_from_ms {
            return Err(CustodyVaultError::InvalidValidityWindow {
                valid_from_ms: request.new_valid_from_ms,
                valid_until_ms: request.new_valid_until_ms,
            });
        }
        let mut conn = self.store.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(current) =
            load_slot_epoch_optional(&tx, request.current_key_id, request.expected_rotation_epoch)?
        else {
            if let Some(latest) = load_latest_slot_optional(&tx, request.current_key_id)? {
                return Err(CustodyVaultError::RotationEpochFenceConflict {
                    expected: request.expected_rotation_epoch,
                    actual: latest.rotation_epoch,
                });
            }
            return Err(CustodyVaultError::KeySlotNotFound {
                key_id: request.current_key_id,
                rotation_epoch: Some(request.expected_rotation_epoch),
            });
        };
        if current.state == CustodyKeyState::CompromisedRevoked {
            return Err(CustodyVaultError::KeyCompromised {
                key_id: current.key_id,
                rotation_epoch: current.rotation_epoch,
                revoked_from_ms: current.revoked_from_ms.unwrap_or(0),
            });
        }
        if current.state != CustodyKeyState::Active {
            return Err(CustodyVaultError::KeyStateNotSignable {
                key_id: current.key_id,
                rotation_epoch: current.rotation_epoch,
                state: current.state,
            });
        }
        if request.rotated_at_ms < current.valid_from_ms {
            return Err(CustodyVaultError::InvalidValidityWindow {
                valid_from_ms: current.valid_from_ms,
                valid_until_ms: request.rotated_at_ms,
            });
        }

        let next_epoch = current
            .rotation_epoch
            .checked_add(1)
            .ok_or(CustodyVaultError::CorruptRecord("rotation_epoch overflow"))?;
        if load_slot_epoch_optional(&tx, request.new_key_id, next_epoch)?.is_some() {
            return Err(CustodyVaultError::KeySlotAlreadyExists {
                key_id: request.new_key_id,
                rotation_epoch: next_epoch,
            });
        }

        let new_public_key = ed25519_rfc8032::derive_public_key(&request.new_seed);
        let wrapped = wrap_seed(
            &self.wrapping_kek,
            request.new_key_id,
            request.new_custody_tier,
            next_epoch,
            &new_public_key,
            &request.new_seed,
        );

        let changed = tx.execute(
            "UPDATE custody_key_slots
             SET key_state = ?1, rotated_at_ms = ?2, successor_key_id = ?3
             WHERE key_id = ?4 AND rotation_epoch = ?5 AND key_state = ?6",
            params![
                CustodyKeyState::RotatedVerifyOnly.code(),
                encode_u64(request.rotated_at_ms)?,
                request.new_key_id.as_bytes().as_slice(),
                current.key_id.as_bytes().as_slice(),
                encode_u64(current.rotation_epoch)?,
                CustodyKeyState::Active.code(),
            ],
        )?;
        if changed != 1 {
            return Err(CustodyVaultError::CorruptRecord(
                "rotation compare-and-swap failed",
            ));
        }

        let active_slot = CustodyKeySlot {
            key_id: request.new_key_id,
            principal_id: current.principal_id,
            control_domain_id: current.control_domain_id,
            authority_id: current.authority_id,
            key_purpose: current.key_purpose,
            public_key: new_public_key,
            custody_tier: request.new_custody_tier,
            rotation_epoch: next_epoch,
            predecessor_key_id: Some(current.key_id),
            successor_key_id: None,
            valid_from_ms: request.new_valid_from_ms,
            valid_until_ms: request.new_valid_until_ms,
            state: CustodyKeyState::Active,
            provisioned_at_ms: request.rotated_at_ms,
            rotated_at_ms: None,
            revoked_from_ms: None,
            revocation_reason_digest: None,
        };
        insert_custody_slot(&tx, &active_slot, &wrapped)?;

        let previous_slot = CustodyKeySlot {
            state: CustodyKeyState::RotatedVerifyOnly,
            rotated_at_ms: Some(request.rotated_at_ms),
            successor_key_id: Some(request.new_key_id),
            ..current
        };
        let receipt_id = ReceiptId::from_bytes(derive_id16(
            DOMAIN_ROTATION_RECEIPT_ID,
            &[
                current.key_id.as_bytes(),
                request.new_key_id.as_bytes(),
                &next_epoch.to_be_bytes(),
                &request.rotated_at_ms.to_be_bytes(),
            ],
        ));
        tx.execute(
            "INSERT INTO custody_rotation_receipts (
                receipt_id, previous_key_id, previous_epoch,
                active_key_id, active_epoch, rotated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                receipt_id.as_bytes().as_slice(),
                current.key_id.as_bytes().as_slice(),
                encode_u64(current.rotation_epoch)?,
                active_slot.key_id.as_bytes().as_slice(),
                encode_u64(active_slot.rotation_epoch)?,
                encode_u64(request.rotated_at_ms)?,
            ],
        )?;
        tx.commit()?;
        Ok(CustodyRotationReceipt {
            receipt_id,
            previous_slot,
            active_slot,
            rotated_at_ms: request.rotated_at_ms,
        })
    }

    /// Permanently marks `(request.key_id, request.rotation_epoch)` as
    /// `CompromisedRevoked` from `request.revoked_from_ms` onward and writes an
    /// immutable [`CustodyRevocationTombstone`].
    ///
    /// # Errors
    ///
    /// Fails closed if the slot is absent, `revoked_from_ms > recorded_at_ms`,
    /// or a conflicting compromise tombstone already exists.
    pub fn revoke_compromised_key(
        &self,
        request: RevokeCompromisedKeyRequest,
    ) -> Result<CustodyRevocationTombstone, CustodyVaultError> {
        if request.revoked_from_ms > request.recorded_at_ms {
            return Err(CustodyVaultError::InvalidRevocationWindow {
                revoked_from_ms: request.revoked_from_ms,
                recorded_at_ms: request.recorded_at_ms,
            });
        }
        let mut conn = self.store.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = load_slot_epoch_optional(&tx, request.key_id, request.rotation_epoch)?
            .ok_or(CustodyVaultError::KeySlotNotFound {
                key_id: request.key_id,
                rotation_epoch: Some(request.rotation_epoch),
            })?;

        if current.state == CustodyKeyState::CompromisedRevoked {
            let existing = load_tombstone_optional(&tx, request.key_id, request.rotation_epoch)?
                .ok_or(CustodyVaultError::CorruptRecord(
                    "compromised slot missing tombstone",
                ))?;
            if existing.revoked_from_ms == request.revoked_from_ms
                && existing.reason_digest == request.reason_digest
            {
                tx.commit()?;
                return Ok(existing);
            }
            return Err(CustodyVaultError::KeyCompromised {
                key_id: current.key_id,
                rotation_epoch: current.rotation_epoch,
                revoked_from_ms: existing.revoked_from_ms,
            });
        }

        let tombstone_id = ReceiptId::from_bytes(derive_id16(
            DOMAIN_REVOCATION_TOMBSTONE_ID,
            &[
                current.key_id.as_bytes(),
                &current.rotation_epoch.to_be_bytes(),
                &request.revoked_from_ms.to_be_bytes(),
                request.reason_digest.as_slice(),
            ],
        ));
        tx.execute(
            "UPDATE custody_key_slots
             SET key_state = ?1, revoked_from_ms = ?2, revocation_reason_digest = ?3
             WHERE key_id = ?4 AND rotation_epoch = ?5",
            params![
                CustodyKeyState::CompromisedRevoked.code(),
                encode_u64(request.revoked_from_ms)?,
                request.reason_digest.as_slice(),
                current.key_id.as_bytes().as_slice(),
                encode_u64(current.rotation_epoch)?,
            ],
        )?;
        tx.execute(
            "INSERT INTO custody_revocation_tombstones (
                tombstone_id, key_id, rotation_epoch, principal_id, authority_id,
                revoked_from_ms, reason_digest, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                tombstone_id.as_bytes().as_slice(),
                current.key_id.as_bytes().as_slice(),
                encode_u64(current.rotation_epoch)?,
                current.principal_id.as_bytes().as_slice(),
                current.authority_id.as_slice(),
                encode_u64(request.revoked_from_ms)?,
                request.reason_digest.as_slice(),
                encode_u64(request.recorded_at_ms)?,
            ],
        )?;
        tx.commit()?;
        Ok(CustodyRevocationTombstone {
            tombstone_id,
            key_id: current.key_id,
            principal_id: current.principal_id,
            authority_id: current.authority_id,
            rotation_epoch: current.rotation_epoch,
            revoked_from_ms: request.revoked_from_ms,
            reason_digest: request.reason_digest,
            recorded_at_ms: request.recorded_at_ms,
        })
    }

    /// Permanently marks a custody key slot `CompromisedRevoked` AND revokes
    /// the corresponding active binding in [`IdentityAuthority`].
    ///
    /// # Errors
    ///
    /// Fails closed if either custody tombstone recording or
    /// [`IdentityAuthority::revoke_key`] fails.
    pub fn revoke_compromised_key_with_identity(
        &self,
        identity: &IdentityAuthority,
        current_binding: &IdentityBinding,
        request: RevokeCompromisedKeyRequest,
        identity_idempotency_key: IdempotencyKey,
    ) -> Result<(CustodyRevocationTombstone, KeyRevocationReceipt), CustodyVaultError> {
        if current_binding.key_id != request.key_id {
            return Err(CustodyVaultError::SignerMetadataMismatch(request.key_id));
        }
        let tombstone = self.revoke_compromised_key(request)?;
        let id_receipt = identity
            .revoke_key(RevokeKeyRequest {
                key_id: request.key_id,
                expected_key_generation: current_binding.key_generation,
                expected_identity_snapshot_id: current_binding.identity_snapshot_id,
                idempotency_key: identity_idempotency_key,
                revoked_at_ms: request.recorded_at_ms,
            })?
            .receipt();
        Ok((tombstone, id_receipt))
    }

    /// Transitions any `Active` key slots whose `valid_until_ms < now_ms` into
    /// [`CustodyKeyState::Expired`], returning the count transitioned.
    ///
    /// # Errors
    ///
    /// Fails on `SQLite` error.
    pub fn expire_keys_at(&self, now_ms: u64) -> Result<usize, CustodyVaultError> {
        let mut conn = self.store.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE custody_key_slots
             SET key_state = ?1
             WHERE key_state = ?2 AND valid_until_ms < ?3",
            params![
                CustodyKeyState::Expired.code(),
                CustodyKeyState::Active.code(),
                encode_u64(now_ms)?,
            ],
        )?;
        tx.commit()?;
        Ok(changed)
    }

    /// Verifies a live operational signature requiring `(key_id, expected_rotation_epoch)`
    /// to be currently `Active`, uncompromised, within validity at
    /// `verified_at_ms`, and cryptographically valid under strict `Ed25519`.
    ///
    /// # Errors
    ///
    /// Fails closed on compromised, rotated-verify-only, expired, not-yet-valid,
    /// epoch-mismatched, or invalid signatures.
    pub fn verify_active_epoch_signature(
        &self,
        key_id: KeyId,
        expected_rotation_epoch: u64,
        message_digest: &[u8; 32],
        signature: &Ed25519Signature,
        verified_at_ms: u64,
    ) -> Result<VerifiedCustodySignature, CustodyVaultError> {
        let slot = match self.inspect_key_slot_epoch(key_id, expected_rotation_epoch) {
            Ok(slot) => slot,
            Err(CustodyVaultError::KeySlotNotFound { .. }) => {
                if let Ok(latest) = self.inspect_latest_key_slot(key_id) {
                    return Err(CustodyVaultError::RotationEpochFenceConflict {
                        expected: expected_rotation_epoch,
                        actual: latest.rotation_epoch,
                    });
                }
                return Err(CustodyVaultError::KeySlotNotFound {
                    key_id,
                    rotation_epoch: Some(expected_rotation_epoch),
                });
            }
            Err(other) => return Err(other),
        };
        match slot.state {
            CustodyKeyState::CompromisedRevoked => {
                return Err(CustodyVaultError::KeyCompromised {
                    key_id,
                    rotation_epoch: slot.rotation_epoch,
                    revoked_from_ms: slot.revoked_from_ms.unwrap_or(0),
                });
            }
            CustodyKeyState::RotatedVerifyOnly => {
                return Err(CustodyVaultError::KeyRotatedVerifyOnly {
                    key_id,
                    rotation_epoch: slot.rotation_epoch,
                });
            }
            CustodyKeyState::Expired => {
                return Err(CustodyVaultError::KeyExpired {
                    key_id,
                    at_ms: verified_at_ms,
                    valid_until_ms: slot.valid_until_ms,
                });
            }
            CustodyKeyState::Active => {}
        }
        if verified_at_ms < slot.valid_from_ms {
            return Err(CustodyVaultError::KeyNotYetValid {
                key_id,
                signed_at_ms: verified_at_ms,
                valid_from_ms: slot.valid_from_ms,
            });
        }
        if verified_at_ms > slot.valid_until_ms {
            return Err(CustodyVaultError::KeyExpired {
                key_id,
                at_ms: verified_at_ms,
                valid_until_ms: slot.valid_until_ms,
            });
        }
        if !ed25519_rfc8032::verify_strict(&slot.public_key, message_digest, signature) {
            return Err(CustodyVaultError::InvalidSignature);
        }
        Ok(VerifiedCustodySignature {
            key_id: slot.key_id,
            principal_id: slot.principal_id,
            control_domain_id: slot.control_domain_id,
            authority_id: slot.authority_id,
            custody_tier: slot.custody_tier,
            rotation_epoch: slot.rotation_epoch,
            key_state_at_check: slot.state,
        })
    }

    /// Verifies a historical signature or audit receipt: `RotatedVerifyOnly(E)`
    /// slots remain valid for signatures issued at or before `rotated_at_ms`
    /// within `[valid_from_ms, valid_until_ms]`, while `CompromisedRevoked`
    /// slots flag any signature with `signed_at_ms >= revoked_from_ms` fail-closed.
    ///
    /// # Errors
    ///
    /// Fails closed on post-compromise signatures, post-rotation backdating,
    /// out-of-window timestamps, epoch mismatch, or invalid `Ed25519` proofs.
    pub fn verify_historical_signature(
        &self,
        key_id: KeyId,
        expected_rotation_epoch: u64,
        message_digest: &[u8; 32],
        signature: &Ed25519Signature,
        signed_at_ms: u64,
    ) -> Result<VerifiedCustodySignature, CustodyVaultError> {
        let slot = self.inspect_key_slot_epoch(key_id, expected_rotation_epoch)?;
        if slot.state == CustodyKeyState::CompromisedRevoked {
            let revoked_from_ms = slot.revoked_from_ms.unwrap_or(0);
            if signed_at_ms >= revoked_from_ms {
                return Err(CustodyVaultError::KeyCompromised {
                    key_id,
                    rotation_epoch: slot.rotation_epoch,
                    revoked_from_ms,
                });
            }
        }
        if signed_at_ms < slot.valid_from_ms {
            return Err(CustodyVaultError::KeyNotYetValid {
                key_id,
                signed_at_ms,
                valid_from_ms: slot.valid_from_ms,
            });
        }
        if signed_at_ms > slot.valid_until_ms {
            return Err(CustodyVaultError::KeyExpired {
                key_id,
                at_ms: signed_at_ms,
                valid_until_ms: slot.valid_until_ms,
            });
        }
        if let Some(rotated_at_ms) = slot.rotated_at_ms
            && signed_at_ms > rotated_at_ms
        {
            return Err(CustodyVaultError::HistoricalTimestampAfterRotation {
                key_id,
                signed_at_ms,
                rotated_at_ms,
            });
        }
        if !ed25519_rfc8032::verify_strict(&slot.public_key, message_digest, signature) {
            return Err(CustodyVaultError::InvalidSignature);
        }
        Ok(VerifiedCustodySignature {
            key_id: slot.key_id,
            principal_id: slot.principal_id,
            control_domain_id: slot.control_domain_id,
            authority_id: slot.authority_id,
            custody_tier: slot.custody_tier,
            rotation_epoch: slot.rotation_epoch,
            key_state_at_check: slot.state,
        })
    }

    /// Reads the public metadata of an exact `(key_id, rotation_epoch)` slot.
    ///
    /// # Errors
    ///
    /// Returns [`CustodyVaultError::KeySlotNotFound`] when absent.
    pub fn inspect_key_slot_epoch(
        &self,
        key_id: KeyId,
        rotation_epoch: u64,
    ) -> Result<CustodyKeySlot, CustodyVaultError> {
        let conn = self.store.lock()?;
        load_slot_epoch_optional(&conn, key_id, rotation_epoch)?.ok_or(
            CustodyVaultError::KeySlotNotFound {
                key_id,
                rotation_epoch: Some(rotation_epoch),
            },
        )
    }

    /// Reads the highest-epoch [`CustodyKeySlot`] recorded for `key_id`.
    ///
    /// # Errors
    ///
    /// Returns [`CustodyVaultError::KeySlotNotFound`] when `key_id` is absent.
    pub fn inspect_latest_key_slot(
        &self,
        key_id: KeyId,
    ) -> Result<CustodyKeySlot, CustodyVaultError> {
        let conn = self.store.lock()?;
        load_latest_slot_optional(&conn, key_id)?.ok_or(CustodyVaultError::KeySlotNotFound {
            key_id,
            rotation_epoch: None,
        })
    }

    /// Reads the compromise tombstone of `(key_id, rotation_epoch)`, if one exists.
    ///
    /// # Errors
    ///
    /// Returns storage or corrupt-record errors.
    pub fn inspect_revocation_tombstone(
        &self,
        key_id: KeyId,
        rotation_epoch: u64,
    ) -> Result<Option<CustodyRevocationTombstone>, CustodyVaultError> {
        let conn = self.store.lock()?;
        load_tombstone_optional(&conn, key_id, rotation_epoch)
    }

    /// Reads the current vault-wide monotonic `last_sign_seq` (`0` before any
    /// signing operation).
    ///
    /// # Errors
    ///
    /// Returns storage or corrupt-record errors.
    pub fn current_sign_seq(&self) -> Result<u64, CustodyVaultError> {
        let conn = self.store.lock()?;
        let seq: i64 = conn.query_row(
            "SELECT last_sign_seq FROM custody_vault_meta WHERE singleton_id = ?1",
            params![VAULT_META_SINGLETON_ID],
            |row| row.get(0),
        )?;
        decode_u64(seq)
    }

    /// Reads a durable [`CustodySignAuditReceipt`] by its monotonic `sign_seq`.
    ///
    /// # Errors
    ///
    /// Returns storage or corrupt-record errors.
    pub fn inspect_audit_receipt_by_seq(
        &self,
        sign_seq: u64,
    ) -> Result<Option<CustodySignAuditReceipt>, CustodyVaultError> {
        let conn = self.store.lock()?;
        load_audit_receipt_optional(&conn, sign_seq)
    }

    /// Lists all durable [`CustodySignAuditReceipt`] rows in strictly ascending
    /// `sign_seq` order.
    ///
    /// # Errors
    ///
    /// Returns storage or corrupt-record errors.
    pub fn list_audit_receipts(&self) -> Result<Vec<CustodySignAuditReceipt>, CustodyVaultError> {
        let conn = self.store.lock()?;
        let mut stmt = conn.prepare(
            "SELECT sign_seq, receipt_id, key_id, principal_id, control_domain_id,
                    authority_id, custody_tier, rotation_epoch, domain_tag_digest,
                    message_digest, signature, signed_at_ms
             FROM custody_sign_audit_log
             ORDER BY sign_seq ASC",
        )?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(decode_audit_row(row)?);
        }
        Ok(out)
    }
}

#[derive(Clone, Copy)]
struct WrappedSeedMaterial {
    nonce: [u8; 16],
    ciphertext: [u8; 32],
    auth_tag: [u8; 32],
}

fn compute_kek_commitment(salt: &[u8; 32], wrapping_kek: &[u8; 32]) -> [u8; 32] {
    custody_domain_sha256(
        DOMAIN_KEK_COMMITMENT,
        &[salt.as_slice(), wrapping_kek.as_slice()],
    )
}

fn wrap_seed(
    wrapping_kek: &[u8; 32],
    key_id: KeyId,
    custody_tier: CustodyTier,
    rotation_epoch: u64,
    public_key: &Ed25519PublicKey,
    seed: &[u8; 32],
) -> WrappedSeedMaterial {
    let nonce = derive_id16(
        b"nlos/custody/seed-nonce/v1",
        &[
            key_id.as_bytes(),
            &rotation_epoch.to_be_bytes(),
            public_key.as_slice(),
        ],
    );
    let keystream = custody_domain_sha256(
        DOMAIN_SEED_KEYSTREAM,
        &[
            wrapping_kek.as_slice(),
            key_id.as_bytes(),
            nonce.as_slice(),
            &rotation_epoch.to_be_bytes(),
        ],
    );
    let mut ciphertext = [0u8; 32];
    for idx in 0..32 {
        ciphertext[idx] = seed[idx] ^ keystream[idx];
    }
    let auth_tag = compute_seed_auth_tag(
        wrapping_kek,
        key_id,
        custody_tier,
        rotation_epoch,
        public_key,
        &nonce,
        &ciphertext,
    );
    WrappedSeedMaterial {
        nonce,
        ciphertext,
        auth_tag,
    }
}

fn unwrap_seed(
    wrapping_kek: &[u8; 32],
    slot: &CustodyKeySlot,
    wrapped: &WrappedSeedMaterial,
) -> Result<[u8; 32], CustodyVaultError> {
    let expected_tag = compute_seed_auth_tag(
        wrapping_kek,
        slot.key_id,
        slot.custody_tier,
        slot.rotation_epoch,
        &slot.public_key,
        &wrapped.nonce,
        &wrapped.ciphertext,
    );
    if constant_time_eq_32(&expected_tag, &wrapped.auth_tag) != 1 {
        return Err(CustodyVaultError::SealedMaterialIntegrityFailure(
            slot.key_id,
        ));
    }
    let keystream = custody_domain_sha256(
        DOMAIN_SEED_KEYSTREAM,
        &[
            wrapping_kek.as_slice(),
            slot.key_id.as_bytes(),
            wrapped.nonce.as_slice(),
            &slot.rotation_epoch.to_be_bytes(),
        ],
    );
    let mut seed = [0u8; 32];
    for idx in 0..32 {
        seed[idx] = wrapped.ciphertext[idx] ^ keystream[idx];
    }
    Ok(seed)
}

fn compute_seed_auth_tag(
    wrapping_kek: &[u8; 32],
    key_id: KeyId,
    custody_tier: CustodyTier,
    rotation_epoch: u64,
    public_key: &Ed25519PublicKey,
    nonce: &[u8; 16],
    ciphertext: &[u8; 32],
) -> [u8; 32] {
    custody_domain_sha256(
        DOMAIN_SEED_AUTH_TAG,
        &[
            wrapping_kek.as_slice(),
            key_id.as_bytes(),
            &custody_tier.code().to_be_bytes(),
            &rotation_epoch.to_be_bytes(),
            public_key.as_slice(),
            nonce.as_slice(),
            ciphertext.as_slice(),
        ],
    )
}

fn constant_time_eq_32(lhs: &[u8; 32], rhs: &[u8; 32]) -> u8 {
    let mut diff = 0u8;
    for idx in 0..32 {
        diff |= lhs[idx] ^ rhs[idx];
    }
    u8::from(diff == 0)
}

fn zeroize_bytes(buf: &mut [u8; 32]) {
    for byte in buf.iter_mut() {
        *byte = 0;
    }
    std::hint::black_box(buf);
}

fn init_or_validate_schema(connection: &mut Connection) -> Result<(), CustodyVaultError> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        0 => {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(CUSTODY_SCHEMA_V1)?;
            tx.pragma_update(None, "user_version", CUSTODY_SCHEMA_VERSION)?;
            tx.commit()?;
            Ok(())
        }
        CUSTODY_SCHEMA_VERSION => Ok(()),
        other => Err(CustodyVaultError::SchemaVersionUnsupported(other)),
    }
}

const CUSTODY_SCHEMA_V1: &str = "
CREATE TABLE custody_vault_meta (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
    vault_salt BLOB NOT NULL CHECK (length(vault_salt) = 32),
    kek_commitment BLOB NOT NULL CHECK (length(kek_commitment) = 32),
    last_sign_seq INTEGER NOT NULL CHECK (last_sign_seq >= 0)
) STRICT;

CREATE TRIGGER custody_vault_meta_monotonic_seq
BEFORE UPDATE ON custody_vault_meta
BEGIN
    SELECT CASE
        WHEN NEW.vault_salt != OLD.vault_salt OR NEW.kek_commitment != OLD.kek_commitment
            THEN RAISE(ABORT, 'custody_vault_meta KEK commitment is immutable')
        WHEN NEW.last_sign_seq < OLD.last_sign_seq
            THEN RAISE(ABORT, 'custody_vault_meta last_sign_seq must be monotonic')
    END;
END;

CREATE TRIGGER custody_vault_meta_no_delete
BEFORE DELETE ON custody_vault_meta
BEGIN
    SELECT RAISE(ABORT, 'custody_vault_meta cannot be deleted');
END;

CREATE TABLE custody_key_slots (
    key_id BLOB NOT NULL CHECK (length(key_id) = 16),
    rotation_epoch INTEGER NOT NULL CHECK (rotation_epoch >= 1),
    principal_id BLOB NOT NULL CHECK (length(principal_id) = 16),
    control_domain_id BLOB NOT NULL CHECK (length(control_domain_id) = 16),
    authority_id BLOB NOT NULL CHECK (length(authority_id) = 16),
    key_purpose INTEGER NOT NULL CHECK (key_purpose IN (1, 2)),
    public_key BLOB NOT NULL CHECK (length(public_key) = 32),
    custody_tier INTEGER NOT NULL CHECK (custody_tier IN (1, 2, 3)),
    predecessor_key_id BLOB CHECK (predecessor_key_id IS NULL OR length(predecessor_key_id) = 16),
    successor_key_id BLOB CHECK (successor_key_id IS NULL OR length(successor_key_id) = 16),
    valid_from_ms INTEGER NOT NULL CHECK (valid_from_ms >= 0),
    valid_until_ms INTEGER NOT NULL CHECK (valid_until_ms > valid_from_ms),
    key_state INTEGER NOT NULL CHECK (key_state IN (1, 2, 3, 4)),
    provisioned_at_ms INTEGER NOT NULL CHECK (provisioned_at_ms >= 0),
    rotated_at_ms INTEGER CHECK (rotated_at_ms IS NULL OR rotated_at_ms >= 0),
    revoked_from_ms INTEGER CHECK (revoked_from_ms IS NULL OR revoked_from_ms >= 0),
    revocation_reason_digest BLOB CHECK (revocation_reason_digest IS NULL OR length(revocation_reason_digest) = 32),
    wrap_nonce BLOB NOT NULL CHECK (length(wrap_nonce) = 16),
    wrapped_seed BLOB NOT NULL CHECK (length(wrapped_seed) = 32),
    wrap_auth_tag BLOB NOT NULL CHECK (length(wrap_auth_tag) = 32),
    PRIMARY KEY (key_id, rotation_epoch)
) STRICT;

CREATE INDEX idx_custody_key_slots_lineage
    ON custody_key_slots (principal_id, authority_id, key_purpose, key_state);

CREATE TRIGGER custody_key_slots_no_delete
BEFORE DELETE ON custody_key_slots
BEGIN
    SELECT RAISE(ABORT, 'custody_key_slots rows are permanent audit records');
END;

CREATE TRIGGER custody_key_slots_compromised_terminal
BEFORE UPDATE ON custody_key_slots
WHEN OLD.key_state = 3
BEGIN
    SELECT RAISE(ABORT, 'CompromisedRevoked key slot state is terminal');
END;

CREATE TABLE custody_sign_audit_log (
    sign_seq INTEGER PRIMARY KEY CHECK (sign_seq >= 1),
    receipt_id BLOB NOT NULL UNIQUE CHECK (length(receipt_id) = 16),
    key_id BLOB NOT NULL CHECK (length(key_id) = 16),
    principal_id BLOB NOT NULL CHECK (length(principal_id) = 16),
    control_domain_id BLOB NOT NULL CHECK (length(control_domain_id) = 16),
    authority_id BLOB NOT NULL CHECK (length(authority_id) = 16),
    custody_tier INTEGER NOT NULL CHECK (custody_tier IN (1, 2, 3)),
    rotation_epoch INTEGER NOT NULL CHECK (rotation_epoch >= 1),
    domain_tag_digest BLOB NOT NULL CHECK (length(domain_tag_digest) = 32),
    message_digest BLOB NOT NULL CHECK (length(message_digest) = 32),
    signature BLOB NOT NULL CHECK (length(signature) = 64),
    signed_at_ms INTEGER NOT NULL CHECK (signed_at_ms >= 0),
    FOREIGN KEY (key_id, rotation_epoch) REFERENCES custody_key_slots(key_id, rotation_epoch)
) STRICT;

CREATE TRIGGER custody_sign_audit_log_no_update
BEFORE UPDATE ON custody_sign_audit_log
BEGIN
    SELECT RAISE(ABORT, 'custody_sign_audit_log is immutable');
END;

CREATE TRIGGER custody_sign_audit_log_no_delete
BEFORE DELETE ON custody_sign_audit_log
BEGIN
    SELECT RAISE(ABORT, 'custody_sign_audit_log is immutable');
END;

CREATE TABLE custody_rotation_receipts (
    receipt_id BLOB PRIMARY KEY CHECK (receipt_id IS NOT NULL AND length(receipt_id) = 16),
    previous_key_id BLOB NOT NULL CHECK (length(previous_key_id) = 16),
    previous_epoch INTEGER NOT NULL CHECK (previous_epoch >= 1),
    active_key_id BLOB NOT NULL CHECK (length(active_key_id) = 16),
    active_epoch INTEGER NOT NULL CHECK (active_epoch = previous_epoch + 1),
    rotated_at_ms INTEGER NOT NULL CHECK (rotated_at_ms >= 0),
    UNIQUE (previous_key_id, previous_epoch),
    UNIQUE (active_key_id, active_epoch),
    FOREIGN KEY (previous_key_id, previous_epoch) REFERENCES custody_key_slots(key_id, rotation_epoch),
    FOREIGN KEY (active_key_id, active_epoch) REFERENCES custody_key_slots(key_id, rotation_epoch)
) STRICT;

CREATE TRIGGER custody_rotation_receipts_no_update
BEFORE UPDATE ON custody_rotation_receipts
BEGIN
    SELECT RAISE(ABORT, 'custody_rotation_receipts is immutable');
END;

CREATE TRIGGER custody_rotation_receipts_no_delete
BEFORE DELETE ON custody_rotation_receipts
BEGIN
    SELECT RAISE(ABORT, 'custody_rotation_receipts is immutable');
END;

CREATE TABLE custody_revocation_tombstones (
    tombstone_id BLOB PRIMARY KEY CHECK (length(tombstone_id) = 16),
    key_id BLOB NOT NULL CHECK (length(key_id) = 16),
    rotation_epoch INTEGER NOT NULL CHECK (rotation_epoch >= 1),
    principal_id BLOB NOT NULL CHECK (length(principal_id) = 16),
    authority_id BLOB NOT NULL CHECK (length(authority_id) = 16),
    revoked_from_ms INTEGER NOT NULL CHECK (revoked_from_ms >= 0),
    reason_digest BLOB NOT NULL CHECK (length(reason_digest) = 32),
    recorded_at_ms INTEGER NOT NULL CHECK (recorded_at_ms >= revoked_from_ms),
    UNIQUE (key_id, rotation_epoch),
    FOREIGN KEY (key_id, rotation_epoch) REFERENCES custody_key_slots(key_id, rotation_epoch)
) STRICT;

CREATE TRIGGER custody_revocation_tombstones_no_update
BEFORE UPDATE ON custody_revocation_tombstones
BEGIN
    SELECT RAISE(ABORT, 'custody_revocation_tombstones is immutable');
END;

CREATE TRIGGER custody_revocation_tombstones_no_delete
BEFORE DELETE ON custody_revocation_tombstones
BEGIN
    SELECT RAISE(ABORT, 'custody_revocation_tombstones is immutable');
END;
";

fn insert_custody_slot(
    tx: &Transaction<'_>,
    slot: &CustodyKeySlot,
    wrapped: &WrappedSeedMaterial,
) -> Result<(), CustodyVaultError> {
    tx.execute(
        "INSERT INTO custody_key_slots (
            key_id, rotation_epoch, principal_id, control_domain_id, authority_id,
            key_purpose, public_key, custody_tier, predecessor_key_id,
            successor_key_id, valid_from_ms, valid_until_ms, key_state,
            provisioned_at_ms, rotated_at_ms, revoked_from_ms,
            revocation_reason_digest, wrap_nonce, wrapped_seed, wrap_auth_tag
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
        params![
            slot.key_id.as_bytes().as_slice(),
            encode_u64(slot.rotation_epoch)?,
            slot.principal_id.as_bytes().as_slice(),
            slot.control_domain_id.as_bytes().as_slice(),
            slot.authority_id.as_slice(),
            encode_key_purpose(slot.key_purpose),
            slot.public_key.as_slice(),
            slot.custody_tier.code(),
            slot.predecessor_key_id.map(KeyId::into_bytes),
            slot.successor_key_id.map(KeyId::into_bytes),
            encode_u64(slot.valid_from_ms)?,
            encode_u64(slot.valid_until_ms)?,
            slot.state.code(),
            encode_u64(slot.provisioned_at_ms)?,
            slot.rotated_at_ms.map(encode_u64).transpose()?,
            slot.revoked_from_ms.map(encode_u64).transpose()?,
            slot.revocation_reason_digest.map(|d| d.to_vec()),
            wrapped.nonce.as_slice(),
            wrapped.ciphertext.as_slice(),
            wrapped.auth_tag.as_slice(),
        ],
    )?;
    Ok(())
}

fn find_active_slot_for_lineage(
    conn: &Connection,
    principal_id: PrincipalId,
    authority_id: [u8; 16],
    key_purpose: KeyPurpose,
) -> Result<Option<CustodyKeySlot>, CustodyVaultError> {
    let found: Option<(Vec<u8>, i64)> = conn
        .query_row(
            "SELECT key_id, rotation_epoch FROM custody_key_slots
             WHERE principal_id = ?1 AND authority_id = ?2 AND key_purpose = ?3 AND key_state = ?4
             LIMIT 1",
            params![
                principal_id.as_bytes().as_slice(),
                authority_id.as_slice(),
                encode_key_purpose(key_purpose),
                CustodyKeyState::Active.code(),
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((bytes, epoch_i64)) = found else {
        return Ok(None);
    };
    let key_id = KeyId::from_bytes(decode_array::<16>(bytes, "key_id")?);
    let epoch = decode_u64(epoch_i64)?;
    load_slot_epoch_optional(conn, key_id, epoch)
}

fn resolve_signing_slot_with_material(
    conn: &Connection,
    key_id: KeyId,
) -> Result<Option<(CustodyKeySlot, WrappedSeedMaterial)>, CustodyVaultError> {
    let active_epoch: Option<i64> = conn
        .query_row(
            "SELECT rotation_epoch FROM custody_key_slots
             WHERE key_id = ?1 AND key_state = ?2
             ORDER BY rotation_epoch DESC LIMIT 1",
            params![key_id.as_bytes().as_slice(), CustodyKeyState::Active.code()],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(epoch_i64) = active_epoch {
        return load_slot_epoch_with_material(conn, key_id, decode_u64(epoch_i64)?);
    }
    let latest_epoch: Option<i64> = conn
        .query_row(
            "SELECT rotation_epoch FROM custody_key_slots
             WHERE key_id = ?1
             ORDER BY rotation_epoch DESC LIMIT 1",
            params![key_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(epoch_i64) = latest_epoch else {
        return Ok(None);
    };
    load_slot_epoch_with_material(conn, key_id, decode_u64(epoch_i64)?)
}

fn load_latest_slot_optional(
    conn: &Connection,
    key_id: KeyId,
) -> Result<Option<CustodyKeySlot>, CustodyVaultError> {
    let latest_epoch: Option<i64> = conn
        .query_row(
            "SELECT rotation_epoch FROM custody_key_slots
             WHERE key_id = ?1
             ORDER BY rotation_epoch DESC LIMIT 1",
            params![key_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(epoch_i64) = latest_epoch else {
        return Ok(None);
    };
    load_slot_epoch_optional(conn, key_id, decode_u64(epoch_i64)?)
}

fn load_slot_epoch_optional(
    conn: &Connection,
    key_id: KeyId,
    rotation_epoch: u64,
) -> Result<Option<CustodyKeySlot>, CustodyVaultError> {
    Ok(load_slot_epoch_with_material(conn, key_id, rotation_epoch)?.map(|(slot, _)| slot))
}

fn load_slot_epoch_with_material(
    conn: &Connection,
    key_id: KeyId,
    rotation_epoch: u64,
) -> Result<Option<(CustodyKeySlot, WrappedSeedMaterial)>, CustodyVaultError> {
    let row = conn
        .query_row(
            "SELECT key_id, principal_id, control_domain_id, authority_id, key_purpose,
                    public_key, custody_tier, rotation_epoch, predecessor_key_id,
                    successor_key_id, valid_from_ms, valid_until_ms, key_state,
                    provisioned_at_ms, rotated_at_ms, revoked_from_ms,
                    revocation_reason_digest, wrap_nonce, wrapped_seed, wrap_auth_tag
             FROM custody_key_slots
             WHERE key_id = ?1 AND rotation_epoch = ?2",
            params![key_id.as_bytes().as_slice(), encode_u64(rotation_epoch)?],
            |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Vec<u8>>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, Option<Vec<u8>>>(8)?,
                    r.get::<_, Option<Vec<u8>>>(9)?,
                    r.get::<_, i64>(10)?,
                    r.get::<_, i64>(11)?,
                    r.get::<_, i64>(12)?,
                    r.get::<_, i64>(13)?,
                    r.get::<_, Option<i64>>(14)?,
                    r.get::<_, Option<i64>>(15)?,
                    r.get::<_, Option<Vec<u8>>>(16)?,
                    r.get::<_, Vec<u8>>(17)?,
                    r.get::<_, Vec<u8>>(18)?,
                    r.get::<_, Vec<u8>>(19)?,
                ))
            },
        )
        .optional()?;
    let Some(raw) = row else {
        return Ok(None);
    };
    let predecessor_key_id = raw
        .8
        .map(|bytes| decode_array::<16>(bytes, "predecessor_key_id").map(KeyId::from_bytes))
        .transpose()?;
    let successor_key_id = raw
        .9
        .map(|bytes| decode_array::<16>(bytes, "successor_key_id").map(KeyId::from_bytes))
        .transpose()?;
    let revocation_reason_digest = raw
        .16
        .map(|bytes| decode_array::<32>(bytes, "revocation_reason_digest"))
        .transpose()?;
    let slot = CustodyKeySlot {
        key_id: KeyId::from_bytes(decode_array::<16>(raw.0, "key_id")?),
        principal_id: PrincipalId::from_bytes(decode_array::<16>(raw.1, "principal_id")?),
        control_domain_id: ControlDomainId::from_bytes(decode_array::<16>(
            raw.2,
            "control_domain_id",
        )?),
        authority_id: decode_array::<16>(raw.3, "authority_id")?,
        key_purpose: decode_key_purpose(raw.4)?,
        public_key: decode_array::<32>(raw.5, "public_key")?,
        custody_tier: CustodyTier::from_code(raw.6)?,
        rotation_epoch: decode_u64(raw.7)?,
        predecessor_key_id,
        successor_key_id,
        valid_from_ms: decode_u64(raw.10)?,
        valid_until_ms: decode_u64(raw.11)?,
        state: CustodyKeyState::from_code(raw.12)?,
        provisioned_at_ms: decode_u64(raw.13)?,
        rotated_at_ms: raw.14.map(decode_u64).transpose()?,
        revoked_from_ms: raw.15.map(decode_u64).transpose()?,
        revocation_reason_digest,
    };
    let wrapped = WrappedSeedMaterial {
        nonce: decode_array::<16>(raw.17, "wrap_nonce")?,
        ciphertext: decode_array::<32>(raw.18, "wrapped_seed")?,
        auth_tag: decode_array::<32>(raw.19, "wrap_auth_tag")?,
    };
    Ok(Some((slot, wrapped)))
}

fn load_tombstone_optional(
    conn: &Connection,
    key_id: KeyId,
    rotation_epoch: u64,
) -> Result<Option<CustodyRevocationTombstone>, CustodyVaultError> {
    let row = conn
        .query_row(
            "SELECT tombstone_id, key_id, principal_id, authority_id,
                    rotation_epoch, revoked_from_ms, reason_digest, recorded_at_ms
             FROM custody_revocation_tombstones
             WHERE key_id = ?1 AND rotation_epoch = ?2",
            params![key_id.as_bytes().as_slice(), encode_u64(rotation_epoch)?],
            |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Vec<u8>>(6)?,
                    r.get::<_, i64>(7)?,
                ))
            },
        )
        .optional()?;
    let Some(raw) = row else {
        return Ok(None);
    };
    Ok(Some(CustodyRevocationTombstone {
        tombstone_id: ReceiptId::from_bytes(decode_array::<16>(raw.0, "tombstone_id")?),
        key_id: KeyId::from_bytes(decode_array::<16>(raw.1, "key_id")?),
        principal_id: PrincipalId::from_bytes(decode_array::<16>(raw.2, "principal_id")?),
        authority_id: decode_array::<16>(raw.3, "authority_id")?,
        rotation_epoch: decode_u64(raw.4)?,
        revoked_from_ms: decode_u64(raw.5)?,
        reason_digest: decode_array::<32>(raw.6, "reason_digest")?,
        recorded_at_ms: decode_u64(raw.7)?,
    }))
}

fn load_audit_receipt_optional(
    conn: &Connection,
    sign_seq: u64,
) -> Result<Option<CustodySignAuditReceipt>, CustodyVaultError> {
    let mut stmt = conn.prepare(
        "SELECT sign_seq, receipt_id, key_id, principal_id, control_domain_id,
                authority_id, custody_tier, rotation_epoch, domain_tag_digest,
                message_digest, signature, signed_at_ms
         FROM custody_sign_audit_log
         WHERE sign_seq = ?1",
    )?;
    let mut rows = stmt.query(params![encode_u64(sign_seq)?])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(decode_audit_row(row)?))
}

fn decode_audit_row(row: &rusqlite::Row<'_>) -> Result<CustodySignAuditReceipt, CustodyVaultError> {
    Ok(CustodySignAuditReceipt {
        sign_seq: decode_u64(row.get(0)?)?,
        receipt_id: ReceiptId::from_bytes(decode_array::<16>(row.get(1)?, "receipt_id")?),
        key_id: KeyId::from_bytes(decode_array::<16>(row.get(2)?, "key_id")?),
        principal_id: PrincipalId::from_bytes(decode_array::<16>(row.get(3)?, "principal_id")?),
        control_domain_id: ControlDomainId::from_bytes(decode_array::<16>(
            row.get(4)?,
            "control_domain_id",
        )?),
        authority_id: decode_array::<16>(row.get(5)?, "authority_id")?,
        custody_tier: CustodyTier::from_code(row.get(6)?)?,
        rotation_epoch: decode_u64(row.get(7)?)?,
        domain_tag_digest: decode_array::<32>(row.get(8)?, "domain_tag_digest")?,
        message_digest: decode_array::<32>(row.get(9)?, "message_digest")?,
        signature: decode_array::<64>(row.get(10)?, "signature")?,
        signed_at_ms: decode_u64(row.get(11)?)?,
    })
}

const fn encode_key_purpose(purpose: KeyPurpose) -> i64 {
    match purpose {
        KeyPurpose::SemanticSigning => 1,
        KeyPurpose::BarrierObservationSigning => 2,
    }
}

const fn decode_key_purpose(code: i64) -> Result<KeyPurpose, CustodyVaultError> {
    match code {
        1 => Ok(KeyPurpose::SemanticSigning),
        2 => Ok(KeyPurpose::BarrierObservationSigning),
        _ => Err(CustodyVaultError::CorruptRecord("unknown key_purpose code")),
    }
}

fn encode_u64(value: u64) -> Result<i64, CustodyVaultError> {
    i64::try_from(value).map_err(|_| CustodyVaultError::CorruptRecord("u64 exceeds SQLite i64"))
}

fn decode_u64(value: i64) -> Result<u64, CustodyVaultError> {
    u64::try_from(value).map_err(|_| CustodyVaultError::CorruptRecord("negative SQLite integer"))
}

fn decode_array<const N: usize>(
    bytes: Vec<u8>,
    field: &'static str,
) -> Result<[u8; N], CustodyVaultError> {
    bytes
        .try_into()
        .map_err(|_| CustodyVaultError::CorruptRecord(field))
}

fn derive_id16(domain: &[u8], parts: &[&[u8]]) -> [u8; 16] {
    let digest = custody_domain_sha256(domain, parts);
    digest[..16]
        .try_into()
        .expect("fixed 16-byte SHA-256 prefix")
}

/// Pure safe-Rust RFC 8032 `Ed25519` implementation over `sha2::Sha512`.
mod ed25519_rfc8032 {
    use sha2::{Digest, Sha512};

    const MASK51: u64 = (1u64 << 51) - 1;

    // p - 2 = 2^255 - 21 in little-endian u64 words.
    const EXP_P_MINUS_2: [u64; 4] = [
        0xffff_ffff_ffff_ffeb,
        0xffff_ffff_ffff_ffff,
        0xffff_ffff_ffff_ffff,
        0x7fff_ffff_ffff_ffff,
    ];

    // (p + 3) / 8 = 2^252 - 2 in little-endian u64 words.
    const EXP_P_PLUS_3_DIV_8: [u64; 4] = [
        0xffff_ffff_ffff_fffe,
        0xffff_ffff_ffff_ffff,
        0xffff_ffff_ffff_ffff,
        0x0fff_ffff_ffff_ffff,
    ];

    // (p - 1) / 4 = 2^253 - 5 in little-endian u64 words.
    const EXP_P_MINUS_1_DIV_4: [u64; 4] = [
        0xffff_ffff_ffff_fffb,
        0xffff_ffff_ffff_ffff,
        0xffff_ffff_ffff_ffff,
        0x1fff_ffff_ffff_ffff,
    ];

    // L = 2^252 + 27742317777372353535851937790883648493 in little-endian u64 words.
    const L_LOW_128: u128 = 27_742_317_777_372_353_535_851_937_790_883_648_493_u128;
    #[allow(clippy::cast_possible_truncation)]
    const ORDER_L: [u64; 4] = [
        L_LOW_128 as u64,
        (L_LOW_128 >> 64) as u64,
        0,
        0x1000_0000_0000_0000,
    ];

    #[derive(Clone, Copy)]
    struct Fe([u64; 5]);

    impl Fe {
        const ZERO: Self = Self([0, 0, 0, 0, 0]);
        const ONE: Self = Self([1, 0, 0, 0, 0]);

        const fn from_u64(value: u64) -> Self {
            Self([value & MASK51, value >> 51, 0, 0, 0])
        }

        fn carry(mut h: [u64; 5]) -> Self {
            let mut c = h[0] >> 51;
            h[0] &= MASK51;
            h[1] += c;
            c = h[1] >> 51;
            h[1] &= MASK51;
            h[2] += c;
            c = h[2] >> 51;
            h[2] &= MASK51;
            h[3] += c;
            c = h[3] >> 51;
            h[3] &= MASK51;
            h[4] += c;
            c = h[4] >> 51;
            h[4] &= MASK51;
            h[0] += c * 19;
            c = h[0] >> 51;
            h[0] &= MASK51;
            h[1] += c;
            Self(h)
        }

        fn add(self, rhs: Self) -> Self {
            Self::carry([
                self.0[0] + rhs.0[0],
                self.0[1] + rhs.0[1],
                self.0[2] + rhs.0[2],
                self.0[3] + rhs.0[3],
                self.0[4] + rhs.0[4],
            ])
        }

        fn sub(self, rhs: Self) -> Self {
            const FOUR_P0: u64 = 4 * (MASK51 - 18);
            const FOUR_P_REST: u64 = 4 * MASK51;
            Self::carry([
                self.0[0] + FOUR_P0 - rhs.0[0],
                self.0[1] + FOUR_P_REST - rhs.0[1],
                self.0[2] + FOUR_P_REST - rhs.0[2],
                self.0[3] + FOUR_P_REST - rhs.0[3],
                self.0[4] + FOUR_P_REST - rhs.0[4],
            ])
        }

        #[allow(clippy::cast_possible_truncation)]
        fn mul(self, rhs: Self) -> Self {
            let a0 = u128::from(self.0[0]);
            let a1 = u128::from(self.0[1]);
            let a2 = u128::from(self.0[2]);
            let a3 = u128::from(self.0[3]);
            let a4 = u128::from(self.0[4]);

            let b0 = u128::from(rhs.0[0]);
            let b1 = u128::from(rhs.0[1]);
            let b2 = u128::from(rhs.0[2]);
            let b3 = u128::from(rhs.0[3]);
            let b4 = u128::from(rhs.0[4]);

            let b1_19 = b1 * 19;
            let b2_19 = b2 * 19;
            let b3_19 = b3 * 19;
            let b4_19 = b4 * 19;

            let mut r0 = a0 * b0 + a1 * b4_19 + a2 * b3_19 + a3 * b2_19 + a4 * b1_19;
            let mut r1 = a0 * b1 + a1 * b0 + a2 * b4_19 + a3 * b3_19 + a4 * b2_19;
            let mut r2 = a0 * b2 + a1 * b1 + a2 * b0 + a3 * b4_19 + a4 * b3_19;
            let mut r3 = a0 * b3 + a1 * b2 + a2 * b1 + a3 * b0 + a4 * b4_19;
            let mut r4 = a0 * b4 + a1 * b3 + a2 * b2 + a3 * b1 + a4 * b0;

            let mask = u128::from(MASK51);
            let mut c = r0 >> 51;
            r0 &= mask;
            r1 += c;
            c = r1 >> 51;
            r1 &= mask;
            r2 += c;
            c = r2 >> 51;
            r2 &= mask;
            r3 += c;
            c = r3 >> 51;
            r3 &= mask;
            r4 += c;
            c = r4 >> 51;
            r4 &= mask;
            r0 += c * 19;
            c = r0 >> 51;
            r0 &= mask;
            r1 += c;

            Self::carry([r0 as u64, r1 as u64, r2 as u64, r3 as u64, r4 as u64])
        }

        fn square(self) -> Self {
            self.mul(self)
        }

        fn pow_u256(self, exp: &[u64; 4]) -> Self {
            let mut result = Self::ONE;
            for word_idx in (0..4).rev() {
                let word = exp[word_idx];
                for bit in (0..64).rev() {
                    result = result.square();
                    if ((word >> bit) & 1) == 1 {
                        result = result.mul(self);
                    }
                }
            }
            result
        }

        fn inv(self) -> Self {
            self.pow_u256(&EXP_P_MINUS_2)
        }

        fn to_bytes(self) -> [u8; 32] {
            let reduced = Self::carry(Self::carry(self.0).0);
            let mut h = reduced.0;

            let mut q = (h[0] + 19) >> 51;
            q = (h[1] + q) >> 51;
            q = (h[2] + q) >> 51;
            q = (h[3] + q) >> 51;
            q = (h[4] + q) >> 51;

            h[0] += 19 * q;
            let mut c = h[0] >> 51;
            h[0] &= MASK51;
            h[1] += c;
            c = h[1] >> 51;
            h[1] &= MASK51;
            h[2] += c;
            c = h[2] >> 51;
            h[2] &= MASK51;
            h[3] += c;
            c = h[3] >> 51;
            h[3] &= MASK51;
            h[4] = (h[4] + c) & MASK51;

            let w0 = h[0] | (h[1] << 51);
            let w1 = (h[1] >> 13) | (h[2] << 38);
            let w2 = (h[2] >> 26) | (h[3] << 25);
            let w3 = (h[3] >> 39) | (h[4] << 12);

            let mut out = [0u8; 32];
            out[0..8].copy_from_slice(&w0.to_le_bytes());
            out[8..16].copy_from_slice(&w1.to_le_bytes());
            out[16..24].copy_from_slice(&w2.to_le_bytes());
            out[24..32].copy_from_slice(&w3.to_le_bytes());
            out
        }

        fn from_bytes_canonical(bytes: &[u8; 32]) -> Option<Self> {
            if (bytes[31] & 0x80) != 0 {
                return None;
            }
            let w0 = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
            let w1 = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
            let w2 = u64::from_le_bytes(bytes[16..24].try_into().ok()?);
            let w3 = u64::from_le_bytes(bytes[24..32].try_into().ok()?);

            let h0 = w0 & MASK51;
            let h1 = ((w0 >> 51) | (w1 << 13)) & MASK51;
            let h2 = ((w1 >> 38) | (w2 << 26)) & MASK51;
            let h3 = ((w2 >> 25) | (w3 << 39)) & MASK51;
            let h4 = (w3 >> 12) & MASK51;

            let fe = Self([h0, h1, h2, h3, h4]);
            if &fe.to_bytes() != bytes {
                return None;
            }
            Some(fe)
        }
    }

    #[derive(Clone, Copy)]
    struct ExtPoint {
        x: Fe,
        y: Fe,
        z: Fe,
        t: Fe,
    }

    impl ExtPoint {
        const IDENTITY: Self = Self {
            x: Fe::ZERO,
            y: Fe::ONE,
            z: Fe::ONE,
            t: Fe::ZERO,
        };
    }

    fn curve_d() -> Fe {
        let num = Fe::ZERO.sub(Fe::from_u64(121_665));
        let den = Fe::from_u64(121_666).inv();
        num.mul(den)
    }

    fn sqrt_minus_one() -> Fe {
        Fe::from_u64(2).pow_u256(&EXP_P_MINUS_1_DIV_4)
    }

    #[allow(clippy::many_single_char_names)] // Matches RFC 8032 §5.1.4 variable naming verbatim.
    fn point_add(lhs: &ExtPoint, rhs: &ExtPoint) -> ExtPoint {
        let d2 = curve_d().add(curve_d());
        let a = lhs.y.sub(lhs.x).mul(rhs.y.sub(rhs.x));
        let b = lhs.y.add(lhs.x).mul(rhs.y.add(rhs.x));
        let c = lhs.t.mul(d2).mul(rhs.t);
        let z1_2 = lhs.z.add(lhs.z);
        let d = z1_2.mul(rhs.z);
        let e = b.sub(a);
        let f = d.sub(c);
        let g = d.add(c);
        let h = b.add(a);
        ExtPoint {
            x: e.mul(f),
            y: g.mul(h),
            t: e.mul(h),
            z: f.mul(g),
        }
    }

    fn scalar_mul(point: &ExtPoint, scalar_le: &[u8; 32]) -> ExtPoint {
        let mut acc = ExtPoint::IDENTITY;
        let mut addend = *point;
        for &byte in scalar_le {
            for bit in 0..8 {
                if ((byte >> bit) & 1) == 1 {
                    acc = point_add(&acc, &addend);
                }
                addend = point_add(&addend, &addend);
            }
        }
        acc
    }

    fn encode_point(point: &ExtPoint) -> [u8; 32] {
        let z_inv = point.z.inv();
        let x = point.x.mul(z_inv);
        let y = point.y.mul(z_inv);
        let mut out = y.to_bytes();
        let x_sign = x.to_bytes()[0] & 1;
        out[31] |= x_sign << 7;
        out
    }

    fn decompress_point(bytes: &[u8; 32]) -> Option<ExtPoint> {
        let x_sign = (bytes[31] >> 7) & 1;
        let mut y_bytes = *bytes;
        y_bytes[31] &= 0x7f;
        let y = Fe::from_bytes_canonical(&y_bytes)?;

        let y2 = y.square();
        let u = y2.sub(Fe::ONE);
        let v = curve_d().mul(y2).add(Fe::ONE);
        let x2 = u.mul(v.inv());

        let mut x = x2.pow_u256(&EXP_P_PLUS_3_DIV_8);
        if x.square().sub(x2).to_bytes() != [0u8; 32] {
            x = x.mul(sqrt_minus_one());
            if x.square().sub(x2).to_bytes() != [0u8; 32] {
                return None;
            }
        }

        let x_bytes = x.to_bytes();
        if x_bytes == [0u8; 32] && x_sign == 1 {
            return None;
        }
        if (x_bytes[0] & 1) != x_sign {
            x = Fe::ZERO.sub(x);
        }

        Some(ExtPoint {
            x,
            y,
            z: Fe::ONE,
            t: x.mul(y),
        })
    }

    fn basepoint() -> ExtPoint {
        let y = Fe::from_u64(4).mul(Fe::from_u64(5).inv());
        let bytes = y.to_bytes();
        decompress_point(&bytes).expect("RFC 8032 Ed25519 basepoint is valid")
    }

    fn expand_seed(seed: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
        let digest: [u8; 64] = Sha512::digest(seed).into();
        let mut scalar = [0u8; 32];
        let mut prefix = [0u8; 32];
        scalar.copy_from_slice(&digest[0..32]);
        prefix.copy_from_slice(&digest[32..64]);
        scalar[0] &= 0xf8;
        scalar[31] &= 0x7f;
        scalar[31] |= 0x40;
        (scalar, prefix)
    }

    pub(super) fn derive_public_key(seed: &[u8; 32]) -> [u8; 32] {
        let (scalar, _) = expand_seed(seed);
        let base = basepoint();
        encode_point(&scalar_mul(&base, &scalar))
    }

    pub(super) fn sign(seed: &[u8; 32], message: &[u8]) -> [u8; 64] {
        let (scalar, prefix) = expand_seed(seed);
        let base = basepoint();
        let public_key = encode_point(&scalar_mul(&base, &scalar));

        let mut r_hasher = Sha512::new();
        r_hasher.update(prefix);
        r_hasher.update(message);
        let r_digest: [u8; 64] = r_hasher.finalize().into();
        let r_scalar = reduce_512_mod_l(&r_digest);

        let r_point = scalar_mul(&base, &r_scalar);
        let r_bytes = encode_point(&r_point);

        let mut k_hasher = Sha512::new();
        k_hasher.update(r_bytes);
        k_hasher.update(public_key);
        k_hasher.update(message);
        let k_digest: [u8; 64] = k_hasher.finalize().into();
        let k_scalar = reduce_512_mod_l(&k_digest);

        let s_bytes = mul_add_mod_l(&k_scalar, &scalar, &r_scalar);

        let mut signature = [0u8; 64];
        signature[0..32].copy_from_slice(&r_bytes);
        signature[32..64].copy_from_slice(&s_bytes);
        signature
    }

    pub(super) fn verify_strict(
        public_key: &[u8; 32],
        message: &[u8],
        signature: &[u8; 64],
    ) -> bool {
        let r_bytes: [u8; 32] = signature[0..32].try_into().expect("32-byte R");
        let s_bytes: [u8; 32] = signature[32..64].try_into().expect("32-byte S");
        if !is_canonical_scalar(&s_bytes) {
            return false;
        }
        let Some(a_point) = decompress_point(public_key) else {
            return false;
        };
        if is_small_order(&a_point) {
            return false;
        }
        let Some(r_point) = decompress_point(&r_bytes) else {
            return false;
        };

        let mut k_hasher = Sha512::new();
        k_hasher.update(r_bytes);
        k_hasher.update(public_key);
        k_hasher.update(message);
        let k_digest: [u8; 64] = k_hasher.finalize().into();
        let k_scalar = reduce_512_mod_l(&k_digest);

        let lhs = scalar_mul(&basepoint(), &s_bytes);
        let rhs = point_add(&r_point, &scalar_mul(&a_point, &k_scalar));
        encode_point(&lhs) == encode_point(&rhs)
    }

    fn is_small_order(point: &ExtPoint) -> bool {
        let p2 = point_add(point, point);
        let p4 = point_add(&p2, &p2);
        let p8 = point_add(&p4, &p4);
        encode_point(&p8) == encode_point(&ExtPoint::IDENTITY)
    }

    fn u256_from_le_bytes(bytes: &[u8; 32]) -> [u64; 4] {
        [
            u64::from_le_bytes(bytes[0..8].try_into().expect("word 0")),
            u64::from_le_bytes(bytes[8..16].try_into().expect("word 1")),
            u64::from_le_bytes(bytes[16..24].try_into().expect("word 2")),
            u64::from_le_bytes(bytes[24..32].try_into().expect("word 3")),
        ]
    }

    fn u256_to_le_bytes(words: &[u64; 4]) -> [u8; 32] {
        let mut out = [0u8; 32];
        out[0..8].copy_from_slice(&words[0].to_le_bytes());
        out[8..16].copy_from_slice(&words[1].to_le_bytes());
        out[16..24].copy_from_slice(&words[2].to_le_bytes());
        out[24..32].copy_from_slice(&words[3].to_le_bytes());
        out
    }

    fn u256_ge(lhs: &[u64; 4], rhs: &[u64; 4]) -> bool {
        for idx in (0..4).rev() {
            if lhs[idx] != rhs[idx] {
                return lhs[idx] > rhs[idx];
            }
        }
        true
    }

    fn u256_sub_assign(lhs: &mut [u64; 4], rhs: &[u64; 4]) {
        let mut borrow = 0u64;
        for idx in 0..4 {
            let (d1, b1) = lhs[idx].overflowing_sub(rhs[idx]);
            let (d2, b2) = d1.overflowing_sub(borrow);
            lhs[idx] = d2;
            borrow = u64::from(b1 || b2);
        }
    }

    fn is_canonical_scalar(bytes: &[u8; 32]) -> bool {
        let words = u256_from_le_bytes(bytes);
        !u256_ge(&words, &ORDER_L)
    }

    fn reduce_512_mod_l(bytes_le: &[u8; 64]) -> [u8; 32] {
        let mut rem = [0u64; 4];
        for bit_idx in (0..512).rev() {
            let bit = u64::from((bytes_le[bit_idx / 8] >> (bit_idx % 8)) & 1);
            let c0 = rem[0] >> 63;
            let c1 = rem[1] >> 63;
            let c2 = rem[2] >> 63;
            rem[0] = (rem[0] << 1) | bit;
            rem[1] = (rem[1] << 1) | c0;
            rem[2] = (rem[2] << 1) | c1;
            rem[3] = (rem[3] << 1) | c2;
            if u256_ge(&rem, &ORDER_L) {
                u256_sub_assign(&mut rem, &ORDER_L);
            }
        }
        u256_to_le_bytes(&rem)
    }

    #[allow(clippy::cast_possible_truncation)]
    fn mul_add_mod_l(a_bytes: &[u8; 32], b_bytes: &[u8; 32], add_bytes: &[u8; 32]) -> [u8; 32] {
        let a = u256_from_le_bytes(a_bytes);
        let b = u256_from_le_bytes(b_bytes);
        let c = u256_from_le_bytes(add_bytes);

        let mut acc = [0u64; 8];
        acc[0] = c[0];
        acc[1] = c[1];
        acc[2] = c[2];
        acc[3] = c[3];

        for i in 0..4 {
            let mut carry = 0u128;
            for j in 0..4 {
                let prod = u128::from(a[i]) * u128::from(b[j]) + u128::from(acc[i + j]) + carry;
                acc[i + j] = prod as u64;
                carry = prod >> 64;
            }
            let mut idx = i + 4;
            while carry > 0 && idx < 8 {
                let sum = u128::from(acc[idx]) + carry;
                acc[idx] = sum as u64;
                carry = sum >> 64;
                idx += 1;
            }
        }

        let mut raw = [0u8; 64];
        for (idx, word) in acc.iter().enumerate() {
            raw[idx * 8..(idx + 1) * 8].copy_from_slice(&word.to_le_bytes());
        }
        reduce_512_mod_l(&raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CapabilityRights, CapabilityTarget};
    use ed25519_dalek::{Signer, SigningKey};
    use nlos_types::NamespaceId;

    #[test]
    fn rfc8032_engine_matches_ed25519_dalek_byte_for_byte() {
        for seed_byte in [0x00u8, 0x01, 0x42, 0x7f, 0xa5, 0xff] {
            let mut seed = [seed_byte; 32];
            for (idx, item) in seed.iter_mut().enumerate() {
                *item ^= u8::try_from(idx * 7).expect("index fits u8");
            }
            let dalek = SigningKey::from_bytes(&seed);
            let expected_pub = dalek.verifying_key().to_bytes();
            let actual_pub = derive_ed25519_public_key_from_seed(&seed);
            assert_eq!(actual_pub, expected_pub);

            for msg in [
                b"".as_slice(),
                b"nlos-custody-vector-1".as_slice(),
                &[0xde; 32],
                &[0x5c; 128],
            ] {
                let expected_sig = dalek.sign(msg).to_bytes();
                let actual_sig = ed25519_rfc8032::sign(&seed, msg);
                assert_eq!(actual_sig, expected_sig);
                assert!(verify_ed25519_signature_strict(
                    &actual_pub,
                    msg,
                    &actual_sig
                ));

                let mut tampered = actual_sig;
                tampered[15] ^= 0x01;
                assert!(!verify_ed25519_signature_strict(
                    &actual_pub,
                    msg,
                    &tampered
                ));
            }
        }
    }

    #[test]
    fn custody_vault_issues_adr0022_root_capability_without_exporting_private_key() {
        let temp_dir =
            std::env::temp_dir().join(format!("nlos-custody-unit-{}-{}", std::process::id(), 101));
        let _ = std::fs::remove_dir_all(&temp_dir);

        let identity = IdentityAuthority::open(&temp_dir).expect("identity authority");
        let capability = CapabilityAuthority::open(&temp_dir).expect("capability authority");
        let vault = SigningKeyCustodyVault::open_durable(&temp_dir, [0x91; 32])
            .expect("open durable custody vault");

        let (op_slot, op_binding) = vault
            .bootstrap_principal_under_custody(
                &identity,
                BootstrapCustodyPrincipalRequest {
                    authority_id: [0x51; 16],
                    principal_profile_digest: [0x11; 32],
                    control_domain_policy_digest: [0xaa; 32],
                    key_purpose: KeyPurpose::SemanticSigning,
                    custody_tier: CustodyTier::OperatorFileSealed,
                    valid_from_ms: 1_000,
                    valid_until_ms: 10_000,
                    seed: [0x77; 32],
                    bootstrap_idempotency_key: IdempotencyKey::from_bytes([0x61; 16]),
                    custody_binding_idempotency_key: IdempotencyKey::from_bytes([0x62; 16]),
                    provisioned_at_ms: 1_000,
                },
            )
            .expect("operator bootstrap under custody");
        assert_eq!(op_slot.state, CustodyKeyState::Active);

        let (holder_slot, holder_binding) = vault
            .bootstrap_principal_under_custody(
                &identity,
                BootstrapCustodyPrincipalRequest {
                    authority_id: [0x52; 16],
                    principal_profile_digest: [0x22; 32],
                    control_domain_policy_digest: [0xbb; 32],
                    key_purpose: KeyPurpose::SemanticSigning,
                    custody_tier: CustodyTier::PlatformKeychainSealed,
                    valid_from_ms: 1_000,
                    valid_until_ms: 10_000,
                    seed: [0x88; 32],
                    bootstrap_idempotency_key: IdempotencyKey::from_bytes([0x63; 16]),
                    custody_binding_idempotency_key: IdempotencyKey::from_bytes([0x64; 16]),
                    provisioned_at_ms: 1_000,
                },
            )
            .expect("holder bootstrap under custody");

        let (issue_decision, audit_receipt) = vault
            .issue_root_capability_under_custody(
                &capability,
                &identity,
                IssueRootCapabilityRequest {
                    issuer_key_id: op_slot.key_id,
                    holder_key_id: holder_slot.key_id,
                    target: CapabilityTarget::Namespace(NamespaceId::from_bytes([0x99; 16])),
                    rights: CapabilityRights::SEMANTIC_APPEND,
                    purpose_digest: None,
                    valid_from_ms: 1_100,
                    valid_until_ms: 9_000,
                    delegation_depth_remaining: 0,
                    call_limit: Some(4),
                    idempotency_key: IdempotencyKey::from_bytes([0x65; 16]),
                    issued_at_ms: 1_200,
                },
            )
            .expect("issue root capability under custody");

        assert_eq!(audit_receipt.sign_seq, 1);
        assert_eq!(issue_decision.record().issuer, op_binding.principal_id);
        assert_eq!(issue_decision.record().holder, holder_binding.principal_id);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
