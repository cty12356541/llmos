use std::error::Error;
use std::fmt;
use std::io;

use cap_std::fs::Dir;
use nlos_identity::{Ed25519Signature, IdentityAuthorityError};
use nlos_schema::{
    CommonSemanticsError, CompatibilityError,
    sabi::v1::{MintReferenceRequest, ReadReferenceRequest, ReleaseReferenceRequest},
};
use nlos_types::{Generation, IdempotencyKey, NamespaceId, PrincipalId, ReceiptId, WorkspaceId};
use sha2::{Digest, Sha256};

use crate::service_call::{AuthorizationError, RuntimeCaller, ServiceTime};
use crate::{COMMAND_MESSAGE_DOMAIN, CapabilityAuthorityError, CapabilityHandle};

pub(crate) const MAX_BLOCK: u64 = 65_408;
pub(crate) const MAX_BLOCKS: u64 = 4_096;
pub(crate) const RATE_WINDOW_MS: u64 = 60_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkspaceBinding {
    pub workspace: WorkspaceId,
    pub namespace: NamespaceId,
    pub revision: Generation,
}

/// Directory supplied by trusted embedding code that manages the root,
/// its ancestors, and mounts on a local filesystem.
pub struct AuthorizedWorkspace {
    pub(super) binding: WorkspaceBinding,
    pub(super) directory: Dir,
}

impl AuthorizedWorkspace {
    /// The host vouches for the managed local-filesystem profile and the
    /// authorization of this directory object. Windows directory handles
    /// must not allow `FILE_SHARE_DELETE`.
    #[must_use]
    pub const fn managed_local(binding: WorkspaceBinding, directory: Dir) -> Self {
        Self { binding, directory }
    }
}

pub trait WorkspaceResolver: Send + Sync {
    /// Resolves a workspace only after checking the current workspace
    /// binding and caller's access. Unsupported filesystem profiles fail closed.
    ///
    /// # Errors
    ///
    /// Returns a denial or an operational failure.
    fn resolve(
        &self,
        caller: &RuntimeCaller,
        workspace: WorkspaceBinding,
    ) -> Result<AuthorizedWorkspace, BlobError>;
}

pub type BlobClock = dyn Fn() -> Result<ServiceTime, AuthorizationError> + Send + Sync;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlobCommandScope {
    pub workspace: WorkspaceBinding,
    pub source: CapabilityHandle,
    pub issued_at_ms: u64,
}

#[derive(Clone, Copy)]
pub struct SignedMintCommand {
    pub scope: BlobCommandScope,
    pub signer: PrincipalId,
    pub signature: Ed25519Signature,
}

#[derive(Clone, Copy)]
pub struct SignedReleaseCommand {
    pub scope: BlobCommandScope,
    pub signer: PrincipalId,
    pub signature: Ed25519Signature,
}

#[derive(Clone, Copy)]
pub enum BlobCommandProof<'a> {
    Mint(&'a SignedMintCommand),
    Release(&'a SignedReleaseCommand),
}

#[derive(Eq, PartialEq)]
pub struct BlobReply {
    pub payload: Vec<u8>,
    pub receipt_id: ReceiptId,
}

impl fmt::Debug for BlobReply {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BlobReply")
            .field("payload_bytes", &self.payload.len())
            .field("receipt_id", &self.receipt_id)
            .finish()
    }
}

/// Limits may be tightened below the defaults. TTL is requested on the wire;
/// hosts can use `DEFAULT_BLOB_TTL_MS` for their normal request duration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlobConfig {
    pub file_bytes: u64,
    pub manifest_blocks: u64,
    pub active_per_principal: usize,
    pub active_global: usize,
    pub reads_per_principal: usize,
    pub reads_global: usize,
    pub mints_per_principal: usize,
    pub mints_global: usize,
    pub ttl_max_ms: u64,
    pub max_reads: u64,
    pub audit_bytes: u64,
    pub audit_rows: u64,
}

pub const DEFAULT_BLOB_TTL_MS: u64 = 300_000;

impl Default for BlobConfig {
    fn default() -> Self {
        Self {
            file_bytes: 64 * 1024 * 1024,
            manifest_blocks: MAX_BLOCKS,
            active_per_principal: 16,
            active_global: 256,
            reads_per_principal: 4,
            reads_global: 16,
            mints_per_principal: 16,
            mints_global: 64,
            ttl_max_ms: 3_600_000,
            max_reads: 4_096,
            audit_bytes: 512 * 1024 * 1024,
            audit_rows: 2_000_000,
        }
    }
}

impl BlobConfig {
    pub(super) fn validate(self) -> Result<Self, BlobError> {
        let max = Self::default();
        let limits = [
            (self.file_bytes, max.file_bytes),
            (self.manifest_blocks, max.manifest_blocks),
            (self.ttl_max_ms, max.ttl_max_ms),
            (self.max_reads, max.max_reads),
            (self.audit_bytes, max.audit_bytes),
            (self.audit_rows, max.audit_rows),
        ];
        let concurrency = [
            (self.active_per_principal, max.active_per_principal),
            (self.active_global, max.active_global),
            (self.reads_per_principal, max.reads_per_principal),
            (self.reads_global, max.reads_global),
            (self.mints_per_principal, max.mints_per_principal),
            (self.mints_global, max.mints_global),
        ];
        if limits
            .into_iter()
            .any(|(value, max)| !(1..=max).contains(&value))
            || concurrency
                .into_iter()
                .any(|(value, max)| !(1..=max).contains(&value))
        {
            return Err(BlobError::InvalidConfig);
        }
        Ok(self)
    }
}

#[derive(Debug)]
pub enum BlobError {
    Authorization(AuthorizationError),
    Authority(CapabilityAuthorityError),
    Identity(IdentityAuthorityError),
    Wire(CompatibilityError),
    Common(CommonSemanticsError),
    Io(io::Error),
    InvalidConfig,
    InvalidRequest,
    UnsupportedBinding,
    BindingMismatch,
    MissingSignature,
    NotFound,
    Expired,
    Released,
    Exhausted,
    EndOfFile,
    InvalidRange,
    ContentChanged,
    IdempotencyConflict,
    QuotaExceeded,
    RateLimited,
    AuditFull,
    InstanceActive,
    Busy,
    CorruptRecord,
    Poisoned,
}

impl fmt::Display for BlobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Authorization(_) => "Blob service authorization failed",
            Self::Authority(_) => "Blob capability authority failed",
            Self::Identity(_) => "Blob identity authority failed",
            Self::Wire(_) | Self::Common(_) | Self::InvalidRequest => {
                "invalid Blob service request"
            }
            Self::Io(_) => "Blob storage I/O failed",
            Self::InvalidConfig => "invalid Blob service limits",
            Self::UnsupportedBinding => "unsupported Blob request context",
            Self::BindingMismatch => "Blob request binding mismatch",
            Self::MissingSignature => "required Blob command signature missing",
            Self::NotFound => "Blob reference unavailable",
            Self::Expired => "Blob reference expired",
            Self::Released => "Blob reference released",
            Self::Exhausted => "Blob reference read limit exhausted",
            Self::EndOfFile => "Blob reference end of file",
            Self::InvalidRange => "invalid Blob block range",
            Self::ContentChanged => "Blob file differs from its mint-time commitment",
            Self::IdempotencyConflict => "Blob idempotency key digest conflict",
            Self::QuotaExceeded => "Blob concurrency or file quota exceeded",
            Self::RateLimited => "Blob mint rate limit exceeded",
            Self::AuditFull => "Blob audit admission limit exceeded",
            Self::InstanceActive => "a Blob service is active for this authority database",
            Self::Busy => "Blob authority temporarily busy",
            Self::CorruptRecord => "invalid durable Blob record",
            Self::Poisoned => "Blob service state unavailable",
        })
    }
}

impl Error for BlobError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Authorization(err) => Some(err),
            Self::Authority(err) => Some(err),
            Self::Identity(err) => Some(err),
            Self::Wire(err) => Some(err),
            Self::Common(err) => Some(err),
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

macro_rules! wrap_error {
    ($($source:ty => $variant:ident),+ $(,)?) => {$(
        impl From<$source> for BlobError {
            fn from(error: $source) -> Self {
                Self::$variant(error)
            }
        }
    )+};
}

wrap_error!(
    AuthorizationError => Authorization,
    CapabilityAuthorityError => Authority,
    IdentityAuthorityError => Identity,
    CompatibilityError => Wire,
    CommonSemanticsError => Common,
    io::Error => Io,
);

impl From<rusqlite::Error> for BlobError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Authority(CapabilityAuthorityError::Sqlite(error))
    }
}

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

fn scope_fields(hash: &mut Sha256, scope: BlobCommandScope) {
    hash.update(scope.workspace.workspace.as_bytes());
    hash.update(scope.workspace.namespace.as_bytes());
    hash.update(scope.workspace.revision.get().to_be_bytes());
    hash.update(scope.source.capability_id.as_bytes());
    hash.update(scope.source.generation.get().to_be_bytes());
    hash.update(scope.issued_at_ms.to_be_bytes());
}

fn actor_fields(hash: &mut Sha256, actor: &RuntimeCaller, key: IdempotencyKey) {
    hash.update(actor.principal.as_bytes());
    hash.update(actor.application.as_bytes());
    hash.update(actor.process.as_bytes());
    hash.update(actor.process_generation.get().to_be_bytes());
    hash.update(key.as_bytes());
}

fn mint_fields(hash: &mut Sha256, request: &MintReferenceRequest, scope: BlobCommandScope) {
    scope_fields(hash, scope);
    field(hash, request.path_handle.as_bytes());
    field(hash, &request.file_digest_sha256);
    field(hash, &request.recipient_principal_id);
    for value in [
        request.size_bytes,
        request.max_reads,
        request.max_chunk_bytes,
        request.valid_until_ms,
    ] {
        hash.update(value.to_be_bytes());
    }
}

fn release_fields(hash: &mut Sha256, request: &ReleaseReferenceRequest, scope: BlobCommandScope) {
    scope_fields(hash, scope);
    field(hash, &request.reference_id);
}

/// Computes the signed Capability command kind 4. The proof binds the process
/// incarnation. Session and transport deadline are checked by current policy.
/// An authorized retry in another process signs the same operation again.
#[must_use]
pub fn mint_command_message(
    actor: &RuntimeCaller,
    key: IdempotencyKey,
    request: &MintReferenceRequest,
    scope: BlobCommandScope,
) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(COMMAND_MESSAGE_DOMAIN);
    hash.update([4]);
    actor_fields(&mut hash, actor, key);
    mint_fields(&mut hash, request, scope);
    hash.finalize().into()
}

/// Computes the signed Capability command kind 5.
#[must_use]
pub fn release_command_message(
    actor: &RuntimeCaller,
    key: IdempotencyKey,
    request: &ReleaseReferenceRequest,
    scope: BlobCommandScope,
) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(COMMAND_MESSAGE_DOMAIN);
    hash.update([5]);
    actor_fields(&mut hash, actor, key);
    release_fields(&mut hash, request, scope);
    hash.finalize().into()
}

#[must_use]
pub fn mint_input_digest(request: &MintReferenceRequest, scope: BlobCommandScope) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"nlos/blob/mint/input/v1");
    mint_fields(&mut hash, request, scope);
    hash.finalize().into()
}

#[must_use]
pub fn read_input_digest(request: &ReadReferenceRequest) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"nlos/blob/read/input/v1");
    field(&mut hash, &request.reference_id);
    hash.update(request.offset.to_be_bytes());
    hash.update(request.length_bytes.to_be_bytes());
    hash.finalize().into()
}

#[must_use]
pub fn release_input_digest(
    request: &ReleaseReferenceRequest,
    scope: BlobCommandScope,
) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"nlos/blob/release/input/v1");
    release_fields(&mut hash, request, scope);
    hash.finalize().into()
}
