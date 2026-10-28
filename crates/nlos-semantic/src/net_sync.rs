//! Cross-socket signed semantic vector checkpoint delta pull and 3-part
//! provenance merge over `nlos-net` authenticated mTLS (`[SEM-CHECKPOINT-001]`,
//! `[DIST-EVENT-001]`, `[DIST-AUDIT-001]`, `[DIST-NAME-001]`,
//! `[TCB-ENDPOINT-001]`).
//!
//! # Distributed Invariants Enforced
//!
//! 1. **Nominal `CellIdentity` & 5-Axis mTLS Exporter Fence Pinning
//!    (`[TCB-ENDPOINT-001]`, `[DIST-NAME-001]`)**:
//!    Every RPC is addressed nominally by [`CellIdentity`] over
//!    [`CellRpcTransport`] (never raw `SocketAddr`), and the server handler
//!    rejects any request whose payload `caller_cell` disagrees with the TLS 1.3
//!    exporter-bound `request.peer_session.peer_claims.cell_identity`.
//! 2. **Signed Vector Checkpoint Causal Diff with Zero Cross-Cell Scalar
//!    `log_seq` (`[SEM-CHECKPOINT-001]`, `[CONS-SCOPE-001]`)**:
//!    Peers exchange [`SignedSemanticVectorCheckpoint`]s, cryptographically
//!    verify both signatures against [`IdentityAuthority`], compute per-domain
//!    causal relations via [`diff_vector_checkpoints`], and paginate missing
//!    [`CrossDomainEventItem`] batches strictly by
//!    `(authority_id, SemanticEventId)` — never assuming or transmitting a
//!    cross-Cell global scalar `log_seq` cursor.
//! 3. **Three-Part Provenance Verification & Atomic Fail-Closed SQLite Merge
//!    (`[DIST-AUDIT-001]`, `[DIST-EVENT-001]`)**:
//!    Every pulled [`CrossDomainEventItem`] must carry (a) a valid original
//!    producer signature, (b) a valid source-store [`AdmissionReceipt`]
//!    signature and binding, and (c) a non-zero `trust_snapshot_digest`.
//!    Any tampered checkpoint signature, forged `caller_cell`, missing/invalid
//!    provenance element, or conflicting event payload under the same
//!    [`SemanticEventId`] fails closed before any SQLite row is committed.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use nlos_capability::CapabilityTarget;
use nlos_cell::{CellEpoch, CellIdentity, RemoteCallContract, RemoteCallOutcome};
use nlos_identity::{
    IdentityAuthority, VerifySemanticAuthoritySignatureRequest, VerifySemanticSignatureRequest,
};
use nlos_net::{
    CellRpcTransport, IncomingRpcRequest, RpcHandlerReply, RpcRequestHandler, RpcWireStatus,
};
use nlos_types::{
    ControlDomainId, IdempotencyKey, KeyId, NamespaceId, PrincipalId, ReceiptId, SemanticEventId,
    TaskId,
};
use sha2::{Digest, Sha256};

use crate::{
    AdmissionDurability, AdmissionReceipt, CrossDomainConvergenceReport, CrossDomainEventItem,
    DomainSemanticWaterMark, SemanticAuthority, SemanticAuthorityError, SemanticEventRecord,
    SemanticPayloadIdentity, SemanticVectorCheckpointError, SignedSemanticVectorCheckpoint,
    StoreSigner, TaintFlags, VectorCheckpointDiff, VectorCheckpointRelation,
    admission_receipt_core_digest, admission_receipt_signature_message, diff_vector_checkpoints,
    semantic_event_id, verify_signed_vector_checkpoint,
};

/// Wire frame magic for `nlos-semantic` cross-socket checkpoint sync (`NLOSSY01`).
pub const SEMANTIC_NET_SYNC_WIRE_MAGIC: &[u8; 8] = b"NLOSSY01";

/// Domain-separation prefix for `NLOSSY01` wire frame `SHA-256` checksums.
pub const SEMANTIC_NET_SYNC_WIRE_CHECKSUM_DOMAIN: &[u8] = b"nlos/semantic-net-sync-wire/v1";

/// Maximum number of [`CrossDomainEventItem`]s returned in a single bounded pull RPC.
pub const MAX_SEMANTIC_SYNC_BATCH_EVENTS: usize = 128;

/// Default bounded batch size used by [`SemanticNetSyncConfig`].
pub const DEFAULT_SEMANTIC_SYNC_BATCH_EVENTS: usize = 32;

/// Default RPC deadline in milliseconds for read-only checkpoint/delta sync calls.
pub const DEFAULT_SEMANTIC_SYNC_DEADLINE_MS: u64 = 5_000;

const REQ_TAG_EXCHANGE_CHECKPOINT: u8 = 1;
const REQ_TAG_PULL_DOMAIN_EVENTS: u8 = 2;

const RESP_TAG_EXCHANGE_CHECKPOINT: u8 = 1;
const RESP_TAG_PULL_DOMAIN_EVENTS: u8 = 2;
const RESP_TAG_REJECTION: u8 = 255;

/// Deterministic pagination cursor over `(authority_id, SemanticEventId)` with
/// zero cross-Cell scalar `log_seq` assumption (`[SEM-CHECKPOINT-001]`).
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DomainEventPullCursor {
    /// 16-byte origin `ControlDomainId` (`authority_id`).
    pub authority_id: [u8; 16],
    /// Last delivered [`SemanticEventId`] within `authority_id`.
    pub last_event_id: SemanticEventId,
}

impl DomainEventPullCursor {
    #[must_use]
    const fn sort_key(self) -> ([u8; 16], [u8; 32]) {
        (self.authority_id, self.last_event_id.into_bytes())
    }
}

/// Wire-level protocol rejection code returned by [`SemanticNetSyncServer`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticNetRejectionCode {
    /// Malformed or checksum-failing `NLOSSY01` frame.
    MalformedWireFrame = 1,
    /// Claimed `caller_cell` did not match TLS 1.3 exporter `peer_claims.cell_identity`.
    CallerCellIdentityMismatch = 2,
    /// Caller's [`SignedSemanticVectorCheckpoint`] failed cryptographic or digest verification.
    InvalidCallerCheckpoint = 3,
    /// Invalid `max_batch_events` (`0`) or empty `authority_ids` set.
    InvalidPullParameters = 4,
    /// Local authority store or identity verification error on responder.
    ResponderAuthorityFailure = 5,
}

impl SemanticNetRejectionCode {
    const fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::MalformedWireFrame),
            2 => Some(Self::CallerCellIdentityMismatch),
            3 => Some(Self::InvalidCallerCheckpoint),
            4 => Some(Self::InvalidPullParameters),
            5 => Some(Self::ResponderAuthorityFailure),
            _ => None,
        }
    }
}

/// Request frames spoken over `CellRpcTransport` by `nlos-semantic::net_sync`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SemanticNetSyncRequest {
    /// Exchanges signed vector checkpoints (`[SEM-CHECKPOINT-001]`).
    ExchangeVectorCheckpoint {
        /// Nominal caller `CellIdentity` (must match `peer_claims.cell_identity`).
        caller_cell: CellIdentity,
        /// Caller's signed vector checkpoint.
        caller_checkpoint: SignedSemanticVectorCheckpoint,
    },
    /// Pulls a bounded page of missing [`CrossDomainEventItem`]s for selected
    /// `authority_ids` without any cross-Cell scalar `log_seq`.
    PullDomainEventsBatch {
        /// Nominal caller `CellIdentity` (must match `peer_claims.cell_identity`).
        caller_cell: CellIdentity,
        /// Origin `ControlDomainId`s (`authority_id`) with `RemoteAhead` or `Concurrent` state.
        authority_ids: BTreeSet<[u8; 16]>,
        /// Event IDs already committed locally in `authority_ids` (skipped from delta).
        known_event_ids: BTreeSet<SemanticEventId>,
        /// Exclusive `(authority_id, last_event_id)` pagination cursor.
        cursor: Option<DomainEventPullCursor>,
        /// Maximum items to return in this page (`1..=MAX_SEMANTIC_SYNC_BATCH_EVENTS`).
        max_batch_events: u32,
    },
}

/// Response frames returned by [`SemanticNetSyncServer`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SemanticNetSyncResponse {
    /// Responder's verified [`SignedSemanticVectorCheckpoint`].
    ExchangeVectorCheckpoint {
        /// Nominal responder `CellIdentity`.
        responder_cell: CellIdentity,
        /// Responder's signed vector checkpoint.
        responder_checkpoint: SignedSemanticVectorCheckpoint,
    },
    /// Bounded page of [`CrossDomainEventItem`]s in deterministic
    /// `(authority_id, SemanticEventId)` order.
    PullDomainEventsBatch {
        /// Nominal responder `CellIdentity`.
        responder_cell: CellIdentity,
        /// Exported 3-part provenance event items in this batch.
        items: Vec<CrossDomainEventItem>,
        /// Cursor of the last item in `items` when `has_more` is `true`.
        next_cursor: Option<DomainEventPullCursor>,
        /// `true` if additional matching items remain after `next_cursor`.
        has_more: bool,
    },
    /// Typed protocol-level fail-closed rejection.
    Rejected {
        /// Structured rejection reason code.
        code: SemanticNetRejectionCode,
        /// Diagnostic description.
        detail: String,
    },
}

/// Errors surfaced by cross-socket semantic checkpoint exchange and delta merge.
#[derive(Debug)]
pub enum SemanticNetSyncError {
    /// Local [`SemanticAuthority`] or merge verification failed fail-closed.
    Authority(SemanticAuthorityError),
    /// Remote checkpoint failed [`verify_signed_vector_checkpoint`].
    InvalidRemoteCheckpoint(SemanticVectorCheckpointError),
    /// Transport call failed to return a completed reply (`RemoteCallOutcome != Ok`).
    TransportFailure(String),
    /// Remote peer returned a protocol rejection frame.
    RemoteRejected {
        /// Rejection reason code.
        code: SemanticNetRejectionCode,
        /// Diagnostic detail from responder.
        detail: String,
    },
    /// Claimed `caller_cell` disagreed with 5-axis mTLS `peer_claims.cell_identity`.
    CallerCellIdentityMismatch {
        /// Identity claimed inside the RPC payload.
        claimed: CellIdentity,
        /// Identity authenticated by the TLS 1.3 Exporter `CellFence` handshake.
        authenticated: CellIdentity,
    },
    /// Responder payload `responder_cell` disagreed with nominal `target_cell`.
    ResponderCellIdentityMismatch {
        /// Expected target `CellIdentity`.
        expected: CellIdentity,
        /// Actual `responder_cell` reported in reply payload.
        actual: CellIdentity,
    },
    /// Wire frame failed magic, bounds, or `SHA-256` checksum validation.
    WireCodec(&'static str),
    /// Remote pagination violated batch-size or monotonic cursor invariants.
    PaginationProtocolViolation(&'static str),
    /// A pulled [`CrossDomainEventItem`] failed 3-part `[DIST-AUDIT-001]`
    /// cryptographic provenance verification before merge.
    InvalidProvenanceReceipt {
        /// Offending event identifier.
        event_id: SemanticEventId,
        /// Specific failed provenance axis.
        reason: &'static str,
    },
    /// Configuration error (e.g., `max_batch_events == 0` or zero trust digest).
    InvalidConfig(&'static str),
}

impl fmt::Display for SemanticNetSyncError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authority(err) => write!(formatter, "semantic authority failure: {err}"),
            Self::InvalidRemoteCheckpoint(err) => {
                write!(formatter, "remote vector checkpoint invalid: {err}")
            }
            Self::TransportFailure(detail) => {
                write!(formatter, "mTLS semantic sync transport failure: {detail}")
            }
            Self::RemoteRejected { code, detail } => {
                write!(
                    formatter,
                    "remote semantic sync peer rejected request ({code:?}): {detail}"
                )
            }
            Self::CallerCellIdentityMismatch {
                claimed,
                authenticated,
            } => write!(
                formatter,
                "caller CellIdentity {claimed:?} mismatches mTLS authenticated peer {authenticated:?}"
            ),
            Self::ResponderCellIdentityMismatch { expected, actual } => write!(
                formatter,
                "responder CellIdentity {actual:?} mismatches target {expected:?}"
            ),
            Self::WireCodec(reason) => write!(formatter, "semantic sync wire error: {reason}"),
            Self::PaginationProtocolViolation(reason) => {
                write!(formatter, "semantic sync pagination violation: {reason}")
            }
            Self::InvalidProvenanceReceipt { event_id, reason } => write!(
                formatter,
                "cross-domain provenance verification failed for {event_id:?}: {reason}"
            ),
            Self::InvalidConfig(reason) => {
                write!(formatter, "invalid semantic sync config: {reason}")
            }
        }
    }
}

impl Error for SemanticNetSyncError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Authority(err) => Some(err),
            Self::InvalidRemoteCheckpoint(err) => Some(err),
            _ => None,
        }
    }
}

impl From<SemanticAuthorityError> for SemanticNetSyncError {
    fn from(value: SemanticAuthorityError) -> Self {
        Self::Authority(value)
    }
}

impl From<SemanticVectorCheckpointError> for SemanticNetSyncError {
    fn from(value: SemanticVectorCheckpointError) -> Self {
        Self::InvalidRemoteCheckpoint(value)
    }
}

/// Pluggable local authority provider for [`SemanticNetSyncServer`] and
/// [`SemanticNetSyncClient`].
///
/// Allows standalone `SemanticAuthority` instances as well as composite hosts
/// (such as `nlos-slice-k::NetCellHost`) to serve and consume cross-socket
/// semantic checkpoint sync over the exact same durable SQLite authority.
pub trait SemanticSyncProvider: Send + Sync {
    /// Nominal [`CellIdentity`] of this local endpoint.
    fn local_cell_identity(&self) -> CellIdentity;

    /// Current authoritative [`CellEpoch`] of this local endpoint.
    fn local_cell_epoch(&self) -> CellEpoch {
        CellEpoch::INITIAL
    }

    /// Exports and signs this store's current [`SignedSemanticVectorCheckpoint`].
    ///
    /// # Errors
    ///
    /// Fails closed on zero trust digest, signer binding mismatch, or SQLite error.
    fn export_local_checkpoint(
        &self,
        signed_at_ms: u64,
    ) -> Result<SignedSemanticVectorCheckpoint, SemanticNetSyncError>;

    /// Cryptographically verifies a peer's [`SignedSemanticVectorCheckpoint`].
    ///
    /// # Errors
    ///
    /// Fails closed if the digest, Merkle root, non-zero trust digest, or
    /// `IdentityAuthority` Ed25519 signature verification fails.
    fn verify_remote_checkpoint(
        &self,
        checkpoint: &SignedSemanticVectorCheckpoint,
    ) -> Result<(), SemanticNetSyncError>;

    /// Exports all committed local [`CrossDomainEventItem`]s with 3-part
    /// provenance bindings.
    ///
    /// # Errors
    ///
    /// Returns a fail-closed authority error on corruption or missing trust digest.
    fn export_local_cross_domain_events(
        &self,
    ) -> Result<Vec<CrossDomainEventItem>, SemanticNetSyncError>;

    /// Verifies all 3 provenance elements (`[DIST-AUDIT-001]`) and atomically
    /// merges `remote_items` into the local store (`[DIST-EVENT-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed with zero SQLite writes if any item is invalid or conflicts.
    fn import_remote_cross_domain_events(
        &self,
        remote_items: &[CrossDomainEventItem],
    ) -> Result<CrossDomainConvergenceReport, SemanticNetSyncError>;
}

/// Ready-to-use [`SemanticSyncProvider`] backed directly by
/// [`SemanticAuthority`], [`IdentityAuthority`], and a [`StoreSigner`].
pub struct AuthoritySemanticSyncProvider<S> {
    local_cell: CellIdentity,
    semantic: Arc<SemanticAuthority>,
    identity: Arc<IdentityAuthority>,
    store_signer: S,
    trust_snapshot_digest: [u8; 32],
}

impl<S> AuthoritySemanticSyncProvider<S>
where
    S: StoreSigner + Send + Sync,
{
    /// Constructs a provider bound to `local_cell` and a non-zero
    /// `trust_snapshot_digest`.
    ///
    /// # Errors
    ///
    /// Fails closed with [`SemanticVectorCheckpointError::MissingTrustSnapshot`]
    /// if `trust_snapshot_digest` is all zeros.
    pub fn new(
        local_cell: CellIdentity,
        semantic: Arc<SemanticAuthority>,
        identity: Arc<IdentityAuthority>,
        store_signer: S,
        trust_snapshot_digest: [u8; 32],
    ) -> Result<Self, SemanticNetSyncError> {
        if trust_snapshot_digest == [0_u8; 32] {
            return Err(SemanticNetSyncError::InvalidRemoteCheckpoint(
                SemanticVectorCheckpointError::MissingTrustSnapshot,
            ));
        }
        Ok(Self {
            local_cell,
            semantic,
            identity,
            store_signer,
            trust_snapshot_digest,
        })
    }

    /// Borrows the underlying [`SemanticAuthority`].
    #[must_use]
    pub fn semantic(&self) -> &Arc<SemanticAuthority> {
        &self.semantic
    }

    /// Borrows the underlying [`IdentityAuthority`].
    #[must_use]
    pub fn identity(&self) -> &Arc<IdentityAuthority> {
        &self.identity
    }

    /// Configured non-zero `trust_snapshot_digest`.
    #[must_use]
    pub const fn trust_snapshot_digest(&self) -> [u8; 32] {
        self.trust_snapshot_digest
    }
}

impl<S> SemanticSyncProvider for AuthoritySemanticSyncProvider<S>
where
    S: StoreSigner + Send + Sync,
{
    fn local_cell_identity(&self) -> CellIdentity {
        self.local_cell
    }

    fn export_local_checkpoint(
        &self,
        signed_at_ms: u64,
    ) -> Result<SignedSemanticVectorCheckpoint, SemanticNetSyncError> {
        Ok(self.semantic.export_signed_vector_checkpoint(
            &self.identity,
            &self.store_signer,
            self.trust_snapshot_digest,
            signed_at_ms,
        )?)
    }

    fn verify_remote_checkpoint(
        &self,
        checkpoint: &SignedSemanticVectorCheckpoint,
    ) -> Result<(), SemanticNetSyncError> {
        verify_signed_vector_checkpoint(&self.identity, checkpoint)?;
        Ok(())
    }

    fn export_local_cross_domain_events(
        &self,
    ) -> Result<Vec<CrossDomainEventItem>, SemanticNetSyncError> {
        Ok(self
            .semantic
            .export_cross_domain_event_set(self.trust_snapshot_digest)?)
    }

    fn import_remote_cross_domain_events(
        &self,
        remote_items: &[CrossDomainEventItem],
    ) -> Result<CrossDomainConvergenceReport, SemanticNetSyncError> {
        for item in remote_items {
            verify_cross_domain_item_provenance(&self.identity, item)?;
        }
        Ok(self.semantic.import_cross_domain_event_set(
            &self.identity,
            &self.store_signer,
            self.trust_snapshot_digest,
            remote_items,
        )?)
    }
}

/// Cryptographically verifies all three `[DIST-AUDIT-001]` provenance elements
/// of an incoming [`CrossDomainEventItem`] against `identity`:
///
/// 1. Non-zero `trust_snapshot_digest` (`!= [0; 32]`).
/// 2. Original producer Ed25519 signature over canonical `event_id`.
/// 3. Source-store [`AdmissionReceipt`] core derivation and Ed25519
///    `store_signature`.
///
/// # Errors
///
/// Fails closed before any SQLite transaction is opened if any provenance axis
/// is missing, mismatched, or fails signature verification.
pub fn verify_cross_domain_item_provenance(
    identity: &IdentityAuthority,
    item: &CrossDomainEventItem,
) -> Result<(), SemanticNetSyncError> {
    if item.trust_snapshot_digest == [0_u8; 32] {
        return Err(SemanticNetSyncError::Authority(
            SemanticAuthorityError::VectorCheckpoint(
                SemanticVectorCheckpointError::MissingTrustSnapshot,
            ),
        ));
    }
    let event_id = item.event.event_id;
    if semantic_event_id(&item.event.canonical_unsigned_event) != event_id {
        return Err(SemanticNetSyncError::InvalidProvenanceReceipt {
            event_id,
            reason: "canonical_unsigned_event hash mismatches claimed event_id",
        });
    }
    if item.admission.event_id != event_id || item.admission.log_seq != item.event.log_seq {
        return Err(SemanticNetSyncError::InvalidProvenanceReceipt {
            event_id,
            reason: "AdmissionReceipt (event_id, log_seq) binding mismatch",
        });
    }
    identity
        .verify_semantic_signature(VerifySemanticSignatureRequest {
            event_id,
            issuer: item.event.issuer,
            control_domain_id: item.event.control_domain,
            key_id: item.event.key_id,
            signature: item.producer_signature,
            admitted_at_ms: item.admission.admitted_at_ms,
        })
        .map_err(|_| SemanticNetSyncError::InvalidProvenanceReceipt {
            event_id,
            reason: "original producer Ed25519 signature rejected",
        })?;

    let core_digest = admission_receipt_core_digest(&item.admission);
    if item.admission.receipt_id.as_bytes() != &core_digest[..16] {
        return Err(SemanticNetSyncError::InvalidProvenanceReceipt {
            event_id,
            reason: "AdmissionReceipt receipt_id does not derive from signed core",
        });
    }
    let receipt_message =
        admission_receipt_signature_message(item.admission.receipt_id, core_digest);
    let verified_store = identity
        .verify_semantic_authority_signature(VerifySemanticAuthoritySignatureRequest {
            message_digest: receipt_message,
            issuer: item.admission.store_principal,
            control_domain_id: item.admission.store_control_domain,
            key_id: item.admission.store_key_id,
            signature: item.admission.store_signature,
            verified_at_ms: item.admission.admitted_at_ms,
        })
        .map_err(|_| SemanticNetSyncError::InvalidProvenanceReceipt {
            event_id,
            reason: "source store AdmissionReceipt Ed25519 signature rejected",
        })?;
    if verified_store.principal_id() != item.admission.store_principal
        || verified_store.control_domain_id() != item.admission.store_control_domain
        || verified_store.key_id() != item.admission.store_key_id
    {
        return Err(SemanticNetSyncError::InvalidProvenanceReceipt {
            event_id,
            reason: "source store AdmissionReceipt signer binding mismatch",
        });
    }
    Ok(())
}

/// Configuration for [`SemanticNetSyncClient`] delta pulls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticNetSyncConfig {
    /// Maximum [`CrossDomainEventItem`]s requested per RPC batch (`1..=128`).
    pub max_batch_events: usize,
    /// Per-RPC deadline in milliseconds.
    pub rpc_deadline_ms: u64,
    /// Logical timestamp recorded when signing local vector checkpoints.
    pub signed_at_ms: u64,
    /// Base [`IdempotencyKey`] mixed with step/page index for deterministic RPC keys.
    pub base_idempotency_key: IdempotencyKey,
}

impl Default for SemanticNetSyncConfig {
    fn default() -> Self {
        Self {
            max_batch_events: DEFAULT_SEMANTIC_SYNC_BATCH_EVENTS,
            rpc_deadline_ms: DEFAULT_SEMANTIC_SYNC_DEADLINE_MS,
            signed_at_ms: 1_000,
            base_idempotency_key: IdempotencyKey::from_bytes([0x5E; 16]),
        }
    }
}

/// Outcome report of a completed cross-socket checkpoint exchange + delta pull +
/// 3-part provenance merge (`[SEM-CHECKPOINT-001]`, `[DIST-EVENT-001]`,
/// `[DIST-AUDIT-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticNetSyncReport {
    /// Local signed checkpoint before pulling from the remote peer.
    pub pre_sync_local_checkpoint: SignedSemanticVectorCheckpoint,
    /// Verified signed checkpoint received from the remote peer.
    pub remote_checkpoint: SignedSemanticVectorCheckpoint,
    /// Causal vector diff computed before pulling missing domains.
    pub pre_sync_diff: VectorCheckpointDiff,
    /// Number of bounded `PullDomainEventsBatch` RPCs executed.
    pub batches_pulled: usize,
    /// Total missing [`CrossDomainEventItem`]s transferred across all batches.
    pub pulled_event_count: usize,
    /// Atomic convergence report (`None` when `pre_sync_diff` had no remote delta,
    /// or `Some` after `import_cross_domain_event_set`).
    pub convergence: Option<CrossDomainConvergenceReport>,
    /// Local signed checkpoint exported after atomic convergence.
    pub post_sync_local_checkpoint: SignedSemanticVectorCheckpoint,
    /// Causal vector diff between `post_sync_local_checkpoint` and `remote_checkpoint`.
    pub post_sync_diff: VectorCheckpointDiff,
}

/// Authenticated mTLS RPC responder for cross-socket semantic vector checkpoint
/// exchange and bounded delta pulls (`[TCB-ENDPOINT-001]`, `[SEM-CHECKPOINT-001]`).
pub struct SemanticNetSyncServer<P> {
    provider: Arc<P>,
    checkpoint_signed_at_ms: u64,
}

impl<P> Clone for SemanticNetSyncServer<P> {
    fn clone(&self) -> Self {
        Self {
            provider: Arc::clone(&self.provider),
            checkpoint_signed_at_ms: self.checkpoint_signed_at_ms,
        }
    }
}

impl<P> SemanticNetSyncServer<P>
where
    P: SemanticSyncProvider + 'static,
{
    /// Creates a new responder backed by `provider`.
    #[must_use]
    pub const fn new(provider: Arc<P>, checkpoint_signed_at_ms: u64) -> Self {
        Self {
            provider,
            checkpoint_signed_at_ms,
        }
    }

    /// Returns `true` if `payload` begins with [`SEMANTIC_NET_SYNC_WIRE_MAGIC`].
    #[must_use]
    pub fn is_semantic_sync_frame(payload: &[u8]) -> bool {
        payload.starts_with(SEMANTIC_NET_SYNC_WIRE_MAGIC)
    }

    /// Synchronously processes one authenticated mTLS [`IncomingRpcRequest`].
    ///
    /// Directly callable by composite dispatchers (such as `NetCellHost`)
    /// without holding any lock across `.await`.
    #[must_use]
    pub fn handle_incoming_rpc(&self, request: &IncomingRpcRequest) -> RpcHandlerReply {
        let observed_epoch = self.provider.local_cell_epoch();
        let authenticated_peer = request.peer_session.peer_claims.cell_identity;
        let Some(decoded) = decode_sync_request(&request.payload) else {
            return encode_rejection_reply(
                observed_epoch,
                SemanticNetRejectionCode::MalformedWireFrame,
                "invalid or corrupted NLOSSY01 request frame",
            );
        };

        match decoded {
            SemanticNetSyncRequest::ExchangeVectorCheckpoint {
                caller_cell,
                caller_checkpoint,
            } => self.handle_exchange_checkpoint(
                observed_epoch,
                authenticated_peer,
                caller_cell,
                &caller_checkpoint,
            ),
            SemanticNetSyncRequest::PullDomainEventsBatch {
                caller_cell,
                authority_ids,
                known_event_ids,
                cursor,
                max_batch_events,
            } => self.handle_pull_domain_events(
                observed_epoch,
                authenticated_peer,
                caller_cell,
                &authority_ids,
                &known_event_ids,
                cursor,
                max_batch_events,
            ),
        }
    }

    fn handle_exchange_checkpoint(
        &self,
        observed_epoch: CellEpoch,
        authenticated_peer: CellIdentity,
        caller_cell: CellIdentity,
        caller_checkpoint: &SignedSemanticVectorCheckpoint,
    ) -> RpcHandlerReply {
        if caller_cell != authenticated_peer {
            return encode_rejection_reply(
                observed_epoch,
                SemanticNetRejectionCode::CallerCellIdentityMismatch,
                "claimed caller_cell mismatches TLS 1.3 exporter peer_claims.cell_identity",
            );
        }
        if let Err(err) = self.provider.verify_remote_checkpoint(caller_checkpoint) {
            return encode_rejection_reply(
                observed_epoch,
                SemanticNetRejectionCode::InvalidCallerCheckpoint,
                &err.to_string(),
            );
        }
        match self
            .provider
            .export_local_checkpoint(self.checkpoint_signed_at_ms)
        {
            Ok(responder_checkpoint) => RpcHandlerReply {
                observed_epoch,
                idempotency_replayed: false,
                status: RpcWireStatus::Ok,
                payload: encode_sync_response(&SemanticNetSyncResponse::ExchangeVectorCheckpoint {
                    responder_cell: self.provider.local_cell_identity(),
                    responder_checkpoint,
                }),
            },
            Err(err) => encode_rejection_reply(
                observed_epoch,
                SemanticNetRejectionCode::ResponderAuthorityFailure,
                &err.to_string(),
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_pull_domain_events(
        &self,
        observed_epoch: CellEpoch,
        authenticated_peer: CellIdentity,
        caller_cell: CellIdentity,
        authority_ids: &BTreeSet<[u8; 16]>,
        known_event_ids: &BTreeSet<SemanticEventId>,
        cursor: Option<DomainEventPullCursor>,
        max_batch_events: u32,
    ) -> RpcHandlerReply {
        if caller_cell != authenticated_peer {
            return encode_rejection_reply(
                observed_epoch,
                SemanticNetRejectionCode::CallerCellIdentityMismatch,
                "claimed caller_cell mismatches TLS 1.3 exporter peer_claims.cell_identity",
            );
        }
        if max_batch_events == 0 || authority_ids.is_empty() {
            return encode_rejection_reply(
                observed_epoch,
                SemanticNetRejectionCode::InvalidPullParameters,
                "max_batch_events must be >= 1 and authority_ids must be non-empty",
            );
        }
        let batch_limit = usize::try_from(max_batch_events)
            .unwrap_or(MAX_SEMANTIC_SYNC_BATCH_EVENTS)
            .clamp(1, MAX_SEMANTIC_SYNC_BATCH_EVENTS);

        let all_items = match self.provider.export_local_cross_domain_events() {
            Ok(items) => items,
            Err(err) => {
                return encode_rejection_reply(
                    observed_epoch,
                    SemanticNetRejectionCode::ResponderAuthorityFailure,
                    &err.to_string(),
                );
            }
        };

        let mut matching: Vec<CrossDomainEventItem> = all_items
            .into_iter()
            .filter(|item| {
                let domain_bytes = item.event.control_domain.into_bytes();
                authority_ids.contains(&domain_bytes)
                    && !known_event_ids.contains(&item.event.event_id)
            })
            .collect();

        // Deterministic canonical sort by `(authority_id, SemanticEventId)` —
        // zero cross-Cell scalar `log_seq` coupling (`[SEM-CHECKPOINT-001]`).
        matching.sort_by_key(|item| {
            (
                item.event.control_domain.into_bytes(),
                item.event.event_id.into_bytes(),
            )
        });

        if let Some(after) = cursor {
            let after_key = after.sort_key();
            matching.retain(|item| {
                (
                    item.event.control_domain.into_bytes(),
                    item.event.event_id.into_bytes(),
                ) > after_key
            });
        }

        let has_more = matching.len() > batch_limit;
        matching.truncate(batch_limit);
        let next_cursor = if has_more {
            matching.last().map(|last| DomainEventPullCursor {
                authority_id: last.event.control_domain.into_bytes(),
                last_event_id: last.event.event_id,
            })
        } else {
            None
        };

        RpcHandlerReply {
            observed_epoch,
            idempotency_replayed: false,
            status: RpcWireStatus::Ok,
            payload: encode_sync_response(&SemanticNetSyncResponse::PullDomainEventsBatch {
                responder_cell: self.provider.local_cell_identity(),
                items: matching,
                next_cursor,
                has_more,
            }),
        }
    }
}

impl<P> RpcRequestHandler for SemanticNetSyncServer<P>
where
    P: SemanticSyncProvider + 'static,
{
    fn handle_rpc(
        &self,
        request: IncomingRpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcHandlerReply> + Send + 'static>> {
        let this = self.clone();
        Box::pin(async move { this.handle_incoming_rpc(&request) })
    }
}

/// Client driver for nominal `CellIdentity` checkpoint exchange, bounded delta
/// pulls, and atomic 3-part provenance merge over [`CellRpcTransport`].
pub struct SemanticNetSyncClient<'a, P> {
    transport: &'a dyn CellRpcTransport,
    provider: &'a P,
}

impl<'a, P> SemanticNetSyncClient<'a, P>
where
    P: SemanticSyncProvider,
{
    /// Creates a new client driver bound to `transport` and local `provider`.
    #[must_use]
    pub fn new(transport: &'a dyn CellRpcTransport, provider: &'a P) -> Self {
        Self {
            transport,
            provider,
        }
    }

    /// Sends an explicit [`SemanticNetSyncRequest`] to `target_cell` and decodes
    /// the structured response.
    ///
    /// # Errors
    ///
    /// Returns [`SemanticNetSyncError::TransportFailure`],
    /// [`SemanticNetSyncError::RemoteRejected`], or [`SemanticNetSyncError::WireCodec`].
    pub async fn send_raw_request(
        &self,
        target_cell: CellIdentity,
        contract: RemoteCallContract,
        request: &SemanticNetSyncRequest,
    ) -> Result<SemanticNetSyncResponse, SemanticNetSyncError> {
        let wire = encode_sync_request(request);
        let outcome = self
            .transport
            .call_cell_rpc(target_cell, &contract, wire)
            .await
            .map_err(|refusal| SemanticNetSyncError::TransportFailure(format!("{refusal}")))?;

        let reply_bytes = match outcome {
            RemoteCallOutcome::Succeeded { value, .. } => value.payload,
            other => {
                return Err(SemanticNetSyncError::TransportFailure(format!("{other:?}")));
            }
        };

        let decoded = decode_sync_response(&reply_bytes).ok_or(SemanticNetSyncError::WireCodec(
            "malformed NLOSSY01 response",
        ))?;
        if let SemanticNetSyncResponse::Rejected { code, detail } = decoded {
            return Err(SemanticNetSyncError::RemoteRejected { code, detail });
        }
        Ok(decoded)
    }

    /// Exchanges [`SignedSemanticVectorCheckpoint`]s with `target_cell`,
    /// verifies the remote signature via [`IdentityAuthority`], and computes
    /// [`diff_vector_checkpoints`].
    ///
    /// # Errors
    ///
    /// Fails closed if local export, mTLS transport, responder identity check,
    /// or remote checkpoint signature verification fails.
    pub async fn exchange_checkpoints(
        &self,
        target_cell: CellIdentity,
        config: SemanticNetSyncConfig,
    ) -> Result<
        (
            SignedSemanticVectorCheckpoint,
            SignedSemanticVectorCheckpoint,
            VectorCheckpointDiff,
        ),
        SemanticNetSyncError,
    > {
        let local_checkpoint = self.provider.export_local_checkpoint(config.signed_at_ms)?;
        let contract = build_readonly_rpc_contract(
            derive_sync_rpc_key(config.base_idempotency_key, target_cell, 0),
            config.rpc_deadline_ms,
        );
        let request = SemanticNetSyncRequest::ExchangeVectorCheckpoint {
            caller_cell: self.provider.local_cell_identity(),
            caller_checkpoint: local_checkpoint.clone(),
        };
        let response = self
            .send_raw_request(target_cell, contract, &request)
            .await?;
        let SemanticNetSyncResponse::ExchangeVectorCheckpoint {
            responder_cell,
            responder_checkpoint,
        } = response
        else {
            return Err(SemanticNetSyncError::WireCodec(
                "unexpected response variant for ExchangeVectorCheckpoint",
            ));
        };

        if responder_cell != target_cell {
            return Err(SemanticNetSyncError::ResponderCellIdentityMismatch {
                expected: target_cell,
                actual: responder_cell,
            });
        }

        // Fail-closed cryptographic & structural checkpoint verification (`[SEM-CHECKPOINT-001]`).
        self.provider
            .verify_remote_checkpoint(&responder_checkpoint)?;

        let diff = diff_vector_checkpoints(
            &local_checkpoint.checkpoint(),
            &responder_checkpoint.checkpoint(),
        );
        Ok((local_checkpoint, responder_checkpoint, diff))
    }

    /// Executes end-to-end signed vector checkpoint exchange, bounded multi-batch
    /// missing event delta pulls (`RemoteAhead` / `Concurrent` domains), 3-part
    /// provenance verification (`[DIST-AUDIT-001]`), and single-transaction
    /// atomic merge (`[DIST-EVENT-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed with zero local SQLite mutations if any checkpoint signature,
    /// `caller_cell` / `responder_cell` pin, pagination invariant, 3-part
    /// provenance element, or event-set conflict check fails.
    pub async fn pull_and_merge_from_peer(
        &self,
        target_cell: CellIdentity,
        config: SemanticNetSyncConfig,
    ) -> Result<SemanticNetSyncReport, SemanticNetSyncError> {
        if config.max_batch_events == 0 {
            return Err(SemanticNetSyncError::InvalidConfig(
                "max_batch_events must be >= 1",
            ));
        }
        let batch_size = config
            .max_batch_events
            .clamp(1, MAX_SEMANTIC_SYNC_BATCH_EVENTS);

        let (pre_sync_local_checkpoint, remote_checkpoint, pre_sync_diff) =
            self.exchange_checkpoints(target_cell, config).await?;

        let pull_domains: BTreeSet<[u8; 16]> = pre_sync_diff
            .domain_diffs
            .iter()
            .filter(|d| {
                matches!(
                    d.relation,
                    VectorCheckpointRelation::RemoteAhead | VectorCheckpointRelation::Concurrent
                )
            })
            .map(|d| d.authority_id)
            .collect();

        if pull_domains.is_empty() {
            return Ok(SemanticNetSyncReport {
                post_sync_local_checkpoint: pre_sync_local_checkpoint.clone(),
                post_sync_diff: pre_sync_diff.clone(),
                pre_sync_local_checkpoint,
                remote_checkpoint,
                pre_sync_diff,
                batches_pulled: 0,
                pulled_event_count: 0,
                convergence: None,
            });
        }

        let local_items = self.provider.export_local_cross_domain_events()?;
        let known_event_ids: BTreeSet<SemanticEventId> = local_items
            .iter()
            .filter(|item| pull_domains.contains(&item.event.control_domain.into_bytes()))
            .map(|item| item.event.event_id)
            .collect();

        let (batches_pulled, pulled_items) = self
            .pull_missing_batches(
                target_cell,
                config,
                batch_size,
                &pull_domains,
                &known_event_ids,
            )
            .await?;

        let pulled_event_count = pulled_items.len();

        // Pre-check non-zero trust_snapshot_digest & basic 3-part provenance envelope
        // before handing the full accumulated delta to `import_remote_cross_domain_events`
        // (which verifies producer signature + source AdmissionReceipt + conflicts in
        // one atomic fail-closed SQLite transaction).
        for item in &pulled_items {
            if item.trust_snapshot_digest == [0_u8; 32] {
                return Err(SemanticNetSyncError::Authority(
                    SemanticAuthorityError::VectorCheckpoint(
                        SemanticVectorCheckpointError::MissingTrustSnapshot,
                    ),
                ));
            }
        }

        let convergence = self
            .provider
            .import_remote_cross_domain_events(&pulled_items)?;

        let post_sync_local_checkpoint =
            self.provider.export_local_checkpoint(config.signed_at_ms)?;
        let post_sync_diff = diff_vector_checkpoints(
            &post_sync_local_checkpoint.checkpoint(),
            &remote_checkpoint.checkpoint(),
        );

        Ok(SemanticNetSyncReport {
            pre_sync_local_checkpoint,
            remote_checkpoint,
            pre_sync_diff,
            batches_pulled,
            pulled_event_count,
            convergence: Some(convergence),
            post_sync_local_checkpoint,
            post_sync_diff,
        })
    }

    async fn pull_missing_batches(
        &self,
        target_cell: CellIdentity,
        config: SemanticNetSyncConfig,
        batch_size: usize,
        pull_domains: &BTreeSet<[u8; 16]>,
        known_event_ids: &BTreeSet<SemanticEventId>,
    ) -> Result<(usize, Vec<CrossDomainEventItem>), SemanticNetSyncError> {
        let mut cursor: Option<DomainEventPullCursor> = None;
        let mut batches_pulled = 0_usize;
        let mut pulled_items: Vec<CrossDomainEventItem> = Vec::new();
        let batch_size_u32 = u32::try_from(batch_size).unwrap_or(128);

        loop {
            let step = u64::try_from(batches_pulled).unwrap_or(u64::MAX) + 1;
            let contract = build_readonly_rpc_contract(
                derive_sync_rpc_key(config.base_idempotency_key, target_cell, step),
                config.rpc_deadline_ms,
            );
            let req = SemanticNetSyncRequest::PullDomainEventsBatch {
                caller_cell: self.provider.local_cell_identity(),
                authority_ids: pull_domains.clone(),
                known_event_ids: known_event_ids.clone(),
                cursor,
                max_batch_events: batch_size_u32,
            };
            let resp = self.send_raw_request(target_cell, contract, &req).await?;
            let SemanticNetSyncResponse::PullDomainEventsBatch {
                responder_cell,
                items,
                next_cursor,
                has_more,
            } = resp
            else {
                return Err(SemanticNetSyncError::WireCodec(
                    "unexpected response variant for PullDomainEventsBatch",
                ));
            };

            if responder_cell != target_cell {
                return Err(SemanticNetSyncError::ResponderCellIdentityMismatch {
                    expected: target_cell,
                    actual: responder_cell,
                });
            }
            if items.len() > batch_size {
                return Err(SemanticNetSyncError::PaginationProtocolViolation(
                    "responder returned more items than max_batch_events",
                ));
            }
            batches_pulled += 1;

            let mut prev_key = cursor.map(DomainEventPullCursor::sort_key);
            for item in &items {
                let key = (
                    item.event.control_domain.into_bytes(),
                    item.event.event_id.into_bytes(),
                );
                if let Some(prev) = prev_key
                    && key <= prev
                {
                    return Err(SemanticNetSyncError::PaginationProtocolViolation(
                        "responder returned non-monotonic (authority_id, event_id) batch order",
                    ));
                }
                prev_key = Some(key);
            }

            pulled_items.extend(items);

            if !has_more {
                break;
            }
            let Some(adv_cursor) = next_cursor else {
                return Err(SemanticNetSyncError::PaginationProtocolViolation(
                    "has_more=true without next_cursor",
                ));
            };
            if let Some(prior) = cursor
                && adv_cursor.sort_key() <= prior.sort_key()
            {
                return Err(SemanticNetSyncError::PaginationProtocolViolation(
                    "next_cursor did not advance monotonically",
                ));
            }
            cursor = Some(adv_cursor);
        }

        Ok((batches_pulled, pulled_items))
    }
}

const fn build_readonly_rpc_contract(
    idempotency_key: IdempotencyKey,
    deadline_ms: u64,
) -> RemoteCallContract {
    RemoteCallContract::new(
        idempotency_key.into_bytes(),
        idempotency_key,
        deadline_ms,
        1,
        0,
        false,
    )
}

fn encode_rejection_reply(
    observed_epoch: CellEpoch,
    code: SemanticNetRejectionCode,
    detail: &str,
) -> RpcHandlerReply {
    RpcHandlerReply {
        observed_epoch,
        idempotency_replayed: false,
        status: RpcWireStatus::Ok,
        payload: encode_sync_response(&SemanticNetSyncResponse::Rejected {
            code,
            detail: detail.to_owned(),
        }),
    }
}

fn derive_sync_rpc_key(
    base: IdempotencyKey,
    target_cell: CellIdentity,
    step: u64,
) -> IdempotencyKey {
    let mut hasher = Sha256::new();
    hasher.update(b"nlos/semantic-net-sync/rpc-idempotency/v1");
    hasher.update(base.as_bytes());
    hasher.update(target_cell.as_bytes());
    hasher.update(step.to_be_bytes());
    let digest = hasher.finalize();
    let mut out = [0_u8; 16];
    out.copy_from_slice(&digest[..16]);
    IdempotencyKey::from_bytes(out)
}

// =============================================================================
// Deterministic Checksummed Wire Framing (`NLOSSY01`)
// =============================================================================

/// Encodes a [`SemanticNetSyncRequest`] into a checksummed `NLOSSY01` frame.
#[must_use]
pub fn encode_sync_request(request: &SemanticNetSyncRequest) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(SEMANTIC_NET_SYNC_WIRE_MAGIC);
    match request {
        SemanticNetSyncRequest::ExchangeVectorCheckpoint {
            caller_cell,
            caller_checkpoint,
        } => {
            out.push(REQ_TAG_EXCHANGE_CHECKPOINT);
            out.extend_from_slice(caller_cell.as_bytes());
            put_signed_checkpoint(&mut out, caller_checkpoint);
        }
        SemanticNetSyncRequest::PullDomainEventsBatch {
            caller_cell,
            authority_ids,
            known_event_ids,
            cursor,
            max_batch_events,
        } => {
            out.push(REQ_TAG_PULL_DOMAIN_EVENTS);
            out.extend_from_slice(caller_cell.as_bytes());
            put_len(&mut out, authority_ids.len());
            for auth in authority_ids {
                out.extend_from_slice(auth);
            }
            put_len(&mut out, known_event_ids.len());
            for ev_id in known_event_ids {
                out.extend_from_slice(ev_id.as_bytes());
            }
            put_cursor(&mut out, *cursor);
            out.extend_from_slice(&max_batch_events.to_be_bytes());
        }
    }
    append_frame_checksum(&mut out);
    out
}

/// Decodes a [`SemanticNetSyncRequest`] from a checksummed `NLOSSY01` frame.
#[must_use]
pub fn decode_sync_request(bytes: &[u8]) -> Option<SemanticNetSyncRequest> {
    let body = verify_and_strip_frame_checksum(bytes)?;
    let mut reader = WireReader::new(body);
    if reader.take_fixed::<8>()? != *SEMANTIC_NET_SYNC_WIRE_MAGIC {
        return None;
    }
    let req = match reader.take_u8()? {
        REQ_TAG_EXCHANGE_CHECKPOINT => {
            let caller_cell = take_cell_identity(&mut reader)?;
            let caller_checkpoint = take_signed_checkpoint(&mut reader)?;
            SemanticNetSyncRequest::ExchangeVectorCheckpoint {
                caller_cell,
                caller_checkpoint,
            }
        }
        REQ_TAG_PULL_DOMAIN_EVENTS => {
            let caller_cell = take_cell_identity(&mut reader)?;
            let auth_count = reader.take_len()?;
            let mut authority_ids = BTreeSet::new();
            for _ in 0..auth_count {
                authority_ids.insert(reader.take_fixed::<16>()?);
            }
            let known_count = reader.take_len()?;
            let mut known_event_ids = BTreeSet::new();
            for _ in 0..known_count {
                known_event_ids.insert(SemanticEventId::from_bytes(reader.take_fixed::<32>()?));
            }
            let cursor = take_cursor(&mut reader)?;
            let max_batch_events = reader.take_u32()?;
            SemanticNetSyncRequest::PullDomainEventsBatch {
                caller_cell,
                authority_ids,
                known_event_ids,
                cursor,
                max_batch_events,
            }
        }
        _ => return None,
    };
    if !reader.is_empty() {
        return None;
    }
    Some(req)
}

/// Encodes a [`SemanticNetSyncResponse`] into a checksummed `NLOSSY01` frame.
#[must_use]
pub fn encode_sync_response(response: &SemanticNetSyncResponse) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(SEMANTIC_NET_SYNC_WIRE_MAGIC);
    match response {
        SemanticNetSyncResponse::ExchangeVectorCheckpoint {
            responder_cell,
            responder_checkpoint,
        } => {
            out.push(RESP_TAG_EXCHANGE_CHECKPOINT);
            out.extend_from_slice(responder_cell.as_bytes());
            put_signed_checkpoint(&mut out, responder_checkpoint);
        }
        SemanticNetSyncResponse::PullDomainEventsBatch {
            responder_cell,
            items,
            next_cursor,
            has_more,
        } => {
            out.push(RESP_TAG_PULL_DOMAIN_EVENTS);
            out.extend_from_slice(responder_cell.as_bytes());
            put_cross_domain_items(&mut out, items);
            put_cursor(&mut out, *next_cursor);
            out.push(u8::from(*has_more));
        }
        SemanticNetSyncResponse::Rejected { code, detail } => {
            out.push(RESP_TAG_REJECTION);
            out.push(*code as u8);
            put_bytes(&mut out, detail.as_bytes());
        }
    }
    append_frame_checksum(&mut out);
    out
}

/// Decodes a [`SemanticNetSyncResponse`] from a checksummed `NLOSSY01` frame.
#[must_use]
pub fn decode_sync_response(bytes: &[u8]) -> Option<SemanticNetSyncResponse> {
    let body = verify_and_strip_frame_checksum(bytes)?;
    let mut reader = WireReader::new(body);
    if reader.take_fixed::<8>()? != *SEMANTIC_NET_SYNC_WIRE_MAGIC {
        return None;
    }
    let resp = match reader.take_u8()? {
        RESP_TAG_EXCHANGE_CHECKPOINT => {
            let responder_cell = take_cell_identity(&mut reader)?;
            let responder_checkpoint = take_signed_checkpoint(&mut reader)?;
            SemanticNetSyncResponse::ExchangeVectorCheckpoint {
                responder_cell,
                responder_checkpoint,
            }
        }
        RESP_TAG_PULL_DOMAIN_EVENTS => {
            let responder_cell = take_cell_identity(&mut reader)?;
            let items = take_cross_domain_items(&mut reader)?;
            let next_cursor = take_cursor(&mut reader)?;
            let has_more = match reader.take_u8()? {
                0 => false,
                1 => true,
                _ => return None,
            };
            SemanticNetSyncResponse::PullDomainEventsBatch {
                responder_cell,
                items,
                next_cursor,
                has_more,
            }
        }
        RESP_TAG_REJECTION => {
            let code = SemanticNetRejectionCode::from_u8(reader.take_u8()?)?;
            let detail = String::from_utf8(reader.take_vec()?).ok()?;
            SemanticNetSyncResponse::Rejected { code, detail }
        }
        _ => return None,
    };
    if !reader.is_empty() {
        return None;
    }
    Some(resp)
}

fn append_frame_checksum(buf: &mut Vec<u8>) {
    let mut hasher = Sha256::new();
    hasher.update(SEMANTIC_NET_SYNC_WIRE_CHECKSUM_DOMAIN);
    hasher.update(&buf[..]);
    let digest: [u8; 32] = hasher.finalize().into();
    buf.extend_from_slice(&digest);
}

fn verify_and_strip_frame_checksum(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() < 8 + 1 + 32 {
        return None;
    }
    let (body, expected_tag) = bytes.split_at(bytes.len() - 32);
    let mut hasher = Sha256::new();
    hasher.update(SEMANTIC_NET_SYNC_WIRE_CHECKSUM_DOMAIN);
    hasher.update(body);
    let actual: [u8; 32] = hasher.finalize().into();
    if actual.as_slice() != expected_tag {
        return None;
    }
    Some(body)
}

fn put_cursor(out: &mut Vec<u8>, cursor: Option<DomainEventPullCursor>) {
    match cursor {
        Some(c) => {
            out.push(1);
            out.extend_from_slice(&c.authority_id);
            out.extend_from_slice(c.last_event_id.as_bytes());
        }
        None => out.push(0),
    }
}

#[allow(clippy::option_option)]
fn take_cursor(reader: &mut WireReader<'_>) -> Option<Option<DomainEventPullCursor>> {
    match reader.take_u8()? {
        0 => Some(None),
        1 => Some(Some(DomainEventPullCursor {
            authority_id: reader.take_fixed::<16>()?,
            last_event_id: SemanticEventId::from_bytes(reader.take_fixed::<32>()?),
        })),
        _ => None,
    }
}

fn put_signed_checkpoint(out: &mut Vec<u8>, ckpt: &SignedSemanticVectorCheckpoint) {
    out.extend_from_slice(&ckpt.checkpoint_id);
    put_len(out, ckpt.domain_watermarks.len());
    for wm in &ckpt.domain_watermarks {
        out.extend_from_slice(&wm.authority_id);
        out.extend_from_slice(&wm.event_count.to_be_bytes());
        out.extend_from_slice(&wm.event_id_merkle_root);
        out.extend_from_slice(&wm.latest_admission_receipt_digest);
        out.extend_from_slice(&wm.trust_snapshot_digest);
    }
    out.extend_from_slice(&ckpt.covered_event_ids_root);
    out.extend_from_slice(&ckpt.checkpoint_digest);
    out.extend_from_slice(ckpt.signer_principal_id.as_bytes());
    out.extend_from_slice(&ckpt.signer_authority_id);
    out.extend_from_slice(ckpt.signer_key_id.as_bytes());
    out.extend_from_slice(&ckpt.signer_public_key);
    out.extend_from_slice(&ckpt.signed_at_ms.to_be_bytes());
    out.extend_from_slice(&ckpt.signature);
}

fn take_signed_checkpoint(reader: &mut WireReader<'_>) -> Option<SignedSemanticVectorCheckpoint> {
    let checkpoint_id = reader.take_fixed::<16>()?;
    let wm_count = reader.take_len()?;
    let mut domain_watermarks = Vec::with_capacity(wm_count);
    for _ in 0..wm_count {
        domain_watermarks.push(DomainSemanticWaterMark {
            authority_id: reader.take_fixed::<16>()?,
            event_count: reader.take_u64()?,
            event_id_merkle_root: reader.take_fixed::<32>()?,
            latest_admission_receipt_digest: reader.take_fixed::<32>()?,
            trust_snapshot_digest: reader.take_fixed::<32>()?,
        });
    }
    let covered_event_ids_root = reader.take_fixed::<32>()?;
    let checkpoint_digest = reader.take_fixed::<32>()?;
    let signer_principal_id = PrincipalId::from_bytes(reader.take_fixed::<16>()?);
    let signer_authority_id = reader.take_fixed::<16>()?;
    let signer_key_id = KeyId::from_bytes(reader.take_fixed::<16>()?);
    let signer_public_key = reader.take_fixed::<32>()?;
    let signed_at_ms = reader.take_u64()?;
    let signature = reader.take_fixed::<64>()?;
    Some(SignedSemanticVectorCheckpoint {
        checkpoint_id,
        domain_watermarks,
        covered_event_ids_root,
        checkpoint_digest,
        signer_principal_id,
        signer_authority_id,
        signer_key_id,
        signer_public_key,
        signed_at_ms,
        signature,
    })
}

fn put_cross_domain_items(out: &mut Vec<u8>, items: &[CrossDomainEventItem]) {
    put_len(out, items.len());
    for item in items {
        let ev = &item.event;
        out.extend_from_slice(ev.event_id.as_bytes());
        put_bytes(out, &ev.canonical_unsigned_event);
        match ev.scope {
            CapabilityTarget::Namespace(ns) => {
                out.push(1);
                out.extend_from_slice(ns.as_bytes());
            }
            CapabilityTarget::Task(tid) => {
                out.push(2);
                out.extend_from_slice(tid.as_bytes());
            }
        }
        out.extend_from_slice(ev.issuer.as_bytes());
        out.extend_from_slice(ev.control_domain.as_bytes());
        out.extend_from_slice(ev.key_id.as_bytes());
        match ev.payload_identity {
            SemanticPayloadIdentity::AssertionContent(d) => {
                out.push(1);
                out.extend_from_slice(&d);
            }
            SemanticPayloadIdentity::IntentSpecBody(d) => {
                out.push(2);
                out.extend_from_slice(&d);
            }
            SemanticPayloadIdentity::Structural => out.push(3),
        }
        out.extend_from_slice(&ev.log_seq.to_be_bytes());
        out.extend_from_slice(&item.producer_signature);

        let adm = &item.admission;
        out.extend_from_slice(adm.receipt_id.as_bytes());
        out.extend_from_slice(adm.event_id.as_bytes());
        out.extend_from_slice(&adm.log_seq.to_be_bytes());
        out.extend_from_slice(&adm.admitted_at_ms.to_be_bytes());
        put_opt_u64(out, adm.effective_valid_until_ms);
        put_len(out, adm.captured_inputs.len());
        for cap_id in &adm.captured_inputs {
            out.extend_from_slice(cap_id.as_bytes());
        }
        out.extend_from_slice(&adm.effective_taint.bits().to_be_bytes());
        out.extend_from_slice(&adm.authz_policy_digest);
        out.extend_from_slice(adm.store_principal.as_bytes());
        out.extend_from_slice(adm.store_control_domain.as_bytes());
        out.extend_from_slice(adm.store_key_id.as_bytes());
        out.extend_from_slice(&adm.store_signature);
        out.extend_from_slice(&item.trust_snapshot_digest);
    }
}

fn take_cross_domain_items(reader: &mut WireReader<'_>) -> Option<Vec<CrossDomainEventItem>> {
    let count = reader.take_len()?;
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        let event_id = SemanticEventId::from_bytes(reader.take_fixed::<32>()?);
        let canonical_unsigned_event = reader.take_vec()?;
        let scope = match reader.take_u8()? {
            1 => CapabilityTarget::Namespace(NamespaceId::from_bytes(reader.take_fixed::<16>()?)),
            2 => CapabilityTarget::Task(TaskId::from_bytes(reader.take_fixed::<16>()?)),
            _ => return None,
        };
        let issuer = PrincipalId::from_bytes(reader.take_fixed::<16>()?);
        let control_domain = ControlDomainId::from_bytes(reader.take_fixed::<16>()?);
        let key_id = KeyId::from_bytes(reader.take_fixed::<16>()?);
        let payload_identity = match reader.take_u8()? {
            1 => SemanticPayloadIdentity::AssertionContent(reader.take_fixed::<32>()?),
            2 => SemanticPayloadIdentity::IntentSpecBody(reader.take_fixed::<32>()?),
            3 => SemanticPayloadIdentity::Structural,
            _ => return None,
        };
        let log_seq = reader.take_u64()?;
        let producer_signature = reader.take_fixed::<64>()?;

        let receipt_id = ReceiptId::from_bytes(reader.take_fixed::<16>()?);
        let adm_event_id = SemanticEventId::from_bytes(reader.take_fixed::<32>()?);
        let adm_log_seq = reader.take_u64()?;
        let admitted_at_ms = reader.take_u64()?;
        let effective_valid_until_ms = reader.take_opt_u64()?;
        let cap_len = reader.take_len()?;
        let mut captured_inputs = Vec::with_capacity(cap_len);
        for _ in 0..cap_len {
            captured_inputs.push(SemanticEventId::from_bytes(reader.take_fixed::<32>()?));
        }
        let effective_taint = TaintFlags::from_bits(reader.take_u64()?)?;
        let authz_policy_digest = reader.take_fixed::<32>()?;
        let store_principal = PrincipalId::from_bytes(reader.take_fixed::<16>()?);
        let store_control_domain = ControlDomainId::from_bytes(reader.take_fixed::<16>()?);
        let store_key_id = KeyId::from_bytes(reader.take_fixed::<16>()?);
        let store_signature = reader.take_fixed::<64>()?;
        let trust_snapshot_digest = reader.take_fixed::<32>()?;

        items.push(CrossDomainEventItem {
            event: SemanticEventRecord {
                event_id,
                canonical_unsigned_event,
                scope,
                issuer,
                control_domain,
                key_id,
                payload_identity,
                log_seq,
            },
            producer_signature,
            admission: AdmissionReceipt {
                receipt_id,
                event_id: adm_event_id,
                log_seq: adm_log_seq,
                admitted_at_ms,
                effective_valid_until_ms,
                captured_inputs,
                effective_taint,
                authz_policy_digest,
                durability: AdmissionDurability::Durable,
                store_principal,
                store_control_domain,
                store_key_id,
                store_signature,
            },
            trust_snapshot_digest,
        });
    }
    Some(items)
}

fn take_cell_identity(reader: &mut WireReader<'_>) -> Option<CellIdentity> {
    let raw = reader.take_fixed::<16>()?;
    Some(CellIdentity::from_domain(
        nlos_types::SchedulerDomainId::from_bytes(raw),
    ))
}

fn put_len(out: &mut Vec<u8>, len: usize) {
    out.extend_from_slice(&u64::try_from(len).unwrap_or(u64::MAX).to_be_bytes());
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_len(out, bytes.len());
    out.extend_from_slice(bytes);
}

fn put_opt_u64(out: &mut Vec<u8>, value: Option<u64>) {
    match value {
        Some(v) => {
            out.push(1);
            out.extend_from_slice(&v.to_be_bytes());
        }
        None => out.push(0),
    }
}

struct WireReader<'a> {
    rest: &'a [u8],
}

impl<'a> WireReader<'a> {
    const fn new(rest: &'a [u8]) -> Self {
        Self { rest }
    }

    const fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }

    fn take_u8(&mut self) -> Option<u8> {
        let (&first, tail) = self.rest.split_first()?;
        self.rest = tail;
        Some(first)
    }

    fn take_fixed<const N: usize>(&mut self) -> Option<[u8; N]> {
        if self.rest.len() < N {
            return None;
        }
        let (head, tail) = self.rest.split_at(N);
        self.rest = tail;
        head.try_into().ok()
    }

    fn take_u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take_fixed::<4>()?))
    }

    fn take_u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take_fixed::<8>()?))
    }

    #[allow(clippy::option_option)]
    fn take_opt_u64(&mut self) -> Option<Option<u64>> {
        match self.take_u8()? {
            0 => Some(None),
            1 => Some(Some(self.take_u64()?)),
            _ => None,
        }
    }

    fn take_len(&mut self) -> Option<usize> {
        let raw = self.take_u64()?;
        let len = usize::try_from(raw).ok()?;
        if len > self.rest.len() {
            return None;
        }
        Some(len)
    }

    fn take_vec(&mut self) -> Option<Vec<u8>> {
        let len = self.take_len()?;
        let (head, tail) = self.rest.split_at(len);
        self.rest = tail;
        Some(head.to_vec())
    }
}
