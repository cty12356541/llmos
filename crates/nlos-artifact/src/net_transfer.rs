//! Cross-socket chunked resumable content-addressed [`ArtifactTransferPackage`]
//! streaming over `nlos-net` (`[DIST-ART-001]`, `[DIST-NAME-001]`,
//! `[TCB-ENDPOINT-001]`, `[DIST-FAIL-001]`, `ADR-0026` §3.1, §6.3).
//!
//! # Protocol & Visibility Invariants
//!
//! 1. **Session & Manifest Registration (`ControlRpc`, `NLOSAT01`)**:
//!    Validates 5-axis mTLS `CellFence` identity pinning
//!    (`caller_cell == peer_claims.cell_identity`), verifies the
//!    `[DIST-ART-001]` 4-axis policy in [`ArtifactSyncPolicyRegistry`], and
//!    verifies `package_manifest_digest` (`ARTIFACT_TRANSFER_PACKAGE_DOMAIN`)
//!    before creating `<root>/artifacts/tmp/net-stage/<pkg_hex>/manifest.bin`.
//! 2. **Bounded-Window Chunk Streaming (`BulkStream`, `NLOSAB01`)**:
//!    Streams content-addressed blobs in bounded `<= 64 KiB` framed chunks
//!    carrying `chunk_index`, `offset`, `chunk_sha256`, `total_blob_sha256`,
//!    and `package_manifest_digest` over [`BulkStreamTransport`] (honoring the
//!    64 MiB per-stream credit window so `ControlRpc` is never starved). Each
//!    chunk is verified against `chunk_sha256` and fsync-staged into
//!    `<root>/artifacts/tmp/net-stage/<pkg_hex>/<blob_hex>.part` +
//!    `<blob_hex>.cursor` as it arrives.
//! 3. **Crash / Disconnect Resumable Cursor**:
//!    If a TCP/mTLS stream drops mid-blob (`AfterFirstPayloadByte` ->
//!    [`RemoteCallOutcome::DeadlineExceededUncertain`]), already-verified
//!    chunks remain durably staged under `artifacts/tmp/net-stage/`. After
//!    authoritative cursor reconciliation (`QueryTransferCursor`), resume
//!    transmits ONLY unverified chunks from `cursor.next_offset` without
//!    re-sending prior verified chunks.
//! 4. **Strict Canonical Visibility Gate**:
//!    Canonical [`ArtifactStore`] (`metadata.db` and `artifacts/blobs/`) is
//!    **never** mutated during manifest registration or chunk streaming. Only
//!    `FinalizeStagedTransfer` — after verifying 100% of declared chunks,
//!    re-hashing every full blob against `total_blob_sha256`, re-verifying
//!    `package_manifest_digest`, and evaluating [`ArtifactSyncPolicyRegistry`]
//!    — invokes [`import_artifact_transfer_package`].

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use nlos_cell::{CellEpoch, CellIdentity, RemoteCallContract, RemoteCallOutcome};
use nlos_net::{
    BulkHandlerCompletion, BulkStreamHandler, BulkStreamReader, BulkStreamTransport,
    BulkTransferReceipt, CellRpcTransport, IncomingBulkMeta, IncomingRpcRequest,
    MAX_BULK_CHUNK_BYTES, PendingReconciliationTracker, RpcHandlerReply, RpcRequestHandler,
    RpcWireStatus, TransportDispatchRefusal,
};
use nlos_types::{
    ApplicationId, ArtifactId, CommitPermitId, IdempotencyKey, ReceiptId, SchedulerDomainId, TaskId,
};
use sha2::{Digest, Sha256};

use crate::model::{ArtifactPublicationReceipt, ContentDigest, HeadState, StagingId, hex_encode};
use crate::store::ArtifactStore;
use crate::sync::{
    ArtifactBlobPayload, ArtifactConflictReceipt, ArtifactConflictStrategy, ArtifactLocalSyncState,
    ArtifactSyncError, ArtifactSyncPolicyRegistry, ArtifactTombstoneMetadata,
    ArtifactTransferPackage, CrossStoreArtifactSyncReport, ImportArtifactTransferRequest,
    ReplicaRevisionBundle, ReplicaSyncRetentionSet, TransferRevisionEntry,
    compute_transfer_package_digest, import_artifact_transfer_package,
};

/// Wire magic identifying `nlos-artifact` control-plane RPC envelopes (`8` bytes).
pub const ARTIFACT_NET_RPC_MAGIC: [u8; 8] = *b"NLOSAT01";

/// Wire magic identifying `nlos-artifact` bulk-stream chunk frames (`8` bytes).
pub const ARTIFACT_NET_BULK_MAGIC: [u8; 8] = *b"NLOSAB01";

/// Disk cursor file magic inside `<root>/artifacts/tmp/net-stage/<pkg_hex>/`.
const STAGE_CURSOR_MAGIC: [u8; 8] = *b"NLOSAC01";

/// Disk manifest file magic inside `<root>/artifacts/tmp/net-stage/<pkg_hex>/`.
const STAGE_MANIFEST_MAGIC: [u8; 8] = *b"NLOSAM01";

/// Fixed header byte size of [`ArtifactBulkChunkFrame`] (`144` bytes).
pub const ARTIFACT_NET_CHUNK_HEADER_BYTES: usize = 8 + 16 + 32 + 32 + 8 + 4 + 8 + 32 + 4;

/// Maximum payload bytes inside a single [`ArtifactBulkChunkFrame`] so the
/// encoded wire chunk never exceeds [`MAX_BULK_CHUNK_BYTES`] (`64 KiB`).
pub const MAX_NET_ARTIFACT_CHUNK_PAYLOAD_BYTES: usize =
    MAX_BULK_CHUNK_BYTES - ARTIFACT_NET_CHUNK_HEADER_BYTES;

/// Default payload byte slice per chunk (`60 KiB`, yielding `61_584`-byte wire
/// chunks comfortably within the `64 KiB` `BulkStreamTransport` limit).
pub const DEFAULT_NET_ARTIFACT_CHUNK_PAYLOAD_BYTES: u32 = 60 * 1024;

const TAG_REQ_BEGIN_SESSION: u8 = 0x01;
const TAG_REQ_QUERY_CURSOR: u8 = 0x02;
const TAG_REQ_FINALIZE_TRANSFER: u8 = 0x03;

const TAG_RESP_SESSION_CURSOR: u8 = 0x11;
const TAG_RESP_CURSOR_STATE: u8 = 0x12;
const TAG_RESP_FINALIZED_REPORT: u8 = 0x13;
const TAG_RESP_REJECTED: u8 = 0x7f;

/// Declared content-addressed blob descriptor inside an [`ArtifactTransferManifest`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactBlobDescriptor {
    pub blob_digest: ContentDigest,
    pub size_bytes: u64,
}

/// Lightweight manifest header for an [`ArtifactTransferPackage`], verifiable
/// against `package_manifest_digest` (`ARTIFACT_TRANSFER_PACKAGE_DOMAIN`) prior
/// to streaming multi-megabyte blob payloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactTransferManifest {
    pub artifact_id: ArtifactId,
    pub content_type: String,
    pub application_id: Option<ApplicationId>,
    pub owner: Option<String>,
    pub artifact_created_at_ms: u64,
    pub origin_cell_id: [u8; 16],
    pub producer_epoch: u64,
    pub remote_head: HeadState,
    pub parent_head: Option<HeadState>,
    pub ancestor_lineage: Vec<HeadState>,
    pub is_tombstone: bool,
    pub tombstone: Option<ArtifactTombstoneMetadata>,
    pub tip_size_bytes: u64,
    pub revisions: Vec<TransferRevisionEntry>,
    pub blobs: Vec<ArtifactBlobDescriptor>,
    pub package_manifest_digest: [u8; 32],
}

impl ArtifactTransferManifest {
    /// Extracts the verifiable manifest header from an [`ArtifactTransferPackage`].
    #[must_use]
    pub fn from_package(package: &ArtifactTransferPackage) -> Self {
        let blobs = package
            .blobs
            .iter()
            .map(|b| ArtifactBlobDescriptor {
                blob_digest: b.blob_digest,
                size_bytes: u64::try_from(b.bytes.len()).unwrap_or(u64::MAX),
            })
            .collect();
        Self {
            artifact_id: package.bundle.artifact_id,
            content_type: package.bundle.content_type.clone(),
            application_id: package.application_id,
            owner: package.owner.clone(),
            artifact_created_at_ms: package.artifact_created_at_ms,
            origin_cell_id: package.bundle.origin_cell_id,
            producer_epoch: package.producer_epoch,
            remote_head: package.bundle.remote_head,
            parent_head: package.bundle.parent_head,
            ancestor_lineage: package.bundle.ancestor_lineage.clone(),
            is_tombstone: package.bundle.is_tombstone,
            tombstone: package.tombstone,
            tip_size_bytes: u64::try_from(package.bundle.bytes.len()).unwrap_or(u64::MAX),
            revisions: package.revisions.clone(),
            blobs,
            package_manifest_digest: package.package_digest,
        }
    }

    /// Recomputes the exact `ARTIFACT_TRANSFER_PACKAGE_DOMAIN` `SHA-256` digest
    /// from manifest metadata and declared blob lengths/digests.
    #[must_use]
    pub fn compute_manifest_digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(crate::sync::ARTIFACT_TRANSFER_PACKAGE_DOMAIN);
        hasher.update(self.artifact_id.as_bytes());
        hash_len_prefixed(&mut hasher, self.content_type.as_bytes());
        match self.application_id {
            Some(app_id) => {
                hasher.update([1_u8]);
                hasher.update(app_id.as_bytes());
            }
            None => hasher.update([0_u8]),
        }
        match &self.owner {
            Some(owner_str) => {
                hasher.update([1_u8]);
                hash_len_prefixed(&mut hasher, owner_str.as_bytes());
            }
            None => hasher.update([0_u8]),
        }
        hasher.update(self.artifact_created_at_ms.to_be_bytes());
        hasher.update(self.origin_cell_id);
        hasher.update(self.producer_epoch.to_be_bytes());
        hasher.update(self.remote_head.revision.to_be_bytes());
        hasher.update(self.remote_head.digest.as_bytes());
        match self.parent_head {
            Some(parent) => {
                hasher.update([1_u8]);
                hasher.update(parent.revision.to_be_bytes());
                hasher.update(parent.digest.as_bytes());
            }
            None => hasher.update([0_u8]),
        }
        hash_usize_as_u64(&mut hasher, self.ancestor_lineage.len());
        for ancestor in &self.ancestor_lineage {
            hasher.update(ancestor.revision.to_be_bytes());
            hasher.update(ancestor.digest.as_bytes());
        }
        hasher.update([u8::from(self.is_tombstone)]);
        match self.tombstone {
            Some(meta) => {
                hasher.update([1_u8]);
                hasher.update(meta.tombstone_revision.to_be_bytes());
                hasher.update(meta.deleted_at_ms.to_be_bytes());
                match meta.retention_until_ms {
                    Some(until_ms) => {
                        hasher.update([1_u8]);
                        hasher.update(until_ms.to_be_bytes());
                    }
                    None => hasher.update([0_u8]),
                }
            }
            None => hasher.update([0_u8]),
        }
        hasher.update(self.tip_size_bytes.to_be_bytes());
        hasher.update(self.remote_head.digest.as_bytes());
        hash_usize_as_u64(&mut hasher, self.revisions.len());
        for rev in &self.revisions {
            hasher.update(rev.revision.to_be_bytes());
            hasher.update(rev.digest.as_bytes());
            hasher.update(rev.size_bytes.to_be_bytes());
            hasher.update(rev.created_at_ms.to_be_bytes());
        }
        hash_usize_as_u64(&mut hasher, self.blobs.len());
        for blob in &self.blobs {
            hasher.update(blob.blob_digest.as_bytes());
            hasher.update(blob.size_bytes.to_be_bytes());
        }
        hasher.finalize().into()
    }
}

fn hash_usize_as_u64(hasher: &mut Sha256, len: usize) {
    let val = u64::try_from(len).unwrap_or(u64::MAX);
    hasher.update(val.to_be_bytes());
}

fn hash_len_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    hash_usize_as_u64(hasher, bytes.len());
    hasher.update(bytes);
}

/// Durable verified chunk cursor for a single content-addressed blob in
/// `<root>/artifacts/tmp/net-stage/<pkg_hex>/`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlobChunkCursor {
    pub blob_digest: ContentDigest,
    pub total_bytes: u64,
    pub verified_chunks: u32,
    pub next_offset: u64,
}

impl BlobChunkCursor {
    /// Returns `true` when 100% of declared bytes for this blob are staged.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.next_offset == self.total_bytes
    }
}

/// Session-wide resumable transfer cursor across all blobs declared in an
/// [`ArtifactTransferManifest`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactTransferSessionCursor {
    pub package_manifest_digest: [u8; 32],
    pub chunk_payload_bytes: u32,
    pub blob_cursors: Vec<BlobChunkCursor>,
}

impl ArtifactTransferSessionCursor {
    /// Returns `true` when every declared blob has `next_offset == total_bytes`.
    #[must_use]
    pub fn all_blobs_complete(&self) -> bool {
        self.blob_cursors.iter().all(BlobChunkCursor::is_complete)
    }

    /// Sum of verified chunks across all declared blobs.
    #[must_use]
    pub fn total_verified_chunks(&self) -> u32 {
        self.blob_cursors
            .iter()
            .fold(0_u32, |acc, c| acc.saturating_add(c.verified_chunks))
    }

    /// Sum of verified staged bytes across all declared blobs.
    #[must_use]
    pub fn total_verified_bytes(&self) -> u64 {
        self.blob_cursors
            .iter()
            .fold(0_u64, |acc, c| acc.saturating_add(c.next_offset))
    }

    /// Looks up the cursor for `blob_digest`.
    #[must_use]
    pub fn cursor_for(&self, blob_digest: ContentDigest) -> Option<BlobChunkCursor> {
        self.blob_cursors
            .iter()
            .find(|c| c.blob_digest == blob_digest)
            .copied()
    }
}

/// Authoritative cursor state returned by `QueryTransferCursor` during
/// post-disconnect reconciliation (`[DIST-FAIL-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactTransferCursorState {
    /// Session has verified chunks staged in `artifacts/tmp/net-stage/`.
    Staged(ArtifactTransferSessionCursor),
    /// Package has already been finalized into the target store.
    AlreadyFinalized(CrossStoreArtifactSyncReport),
    /// No active staging session or finalized receipt exists for this digest.
    NotStaged,
}

/// Self-verifying `<= 64 KiB` chunk frame carried inside a `BulkStreamTransport`
/// chunk (`NLOSAB01`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactBulkChunkFrame {
    pub caller_cell: CellIdentity,
    pub package_manifest_digest: [u8; 32],
    pub total_blob_sha256: ContentDigest,
    pub total_blob_bytes: u64,
    pub chunk_index: u32,
    pub offset: u64,
    pub chunk_sha256: [u8; 32],
    pub payload: Vec<u8>,
}

impl ArtifactBulkChunkFrame {
    /// Encodes and validates a chunk frame (`<= MAX_BULK_CHUNK_BYTES`).
    ///
    /// # Errors
    ///
    /// Fails closed if `payload` is empty, exceeds
    /// [`MAX_NET_ARTIFACT_CHUNK_PAYLOAD_BYTES`], or mismatches `chunk_sha256`.
    pub fn encode(&self) -> Result<Vec<u8>, ArtifactNetTransferError> {
        if self.payload.is_empty() {
            return Err(ArtifactNetTransferError::InvalidChunkSize(0));
        }
        if self.payload.len() > MAX_NET_ARTIFACT_CHUNK_PAYLOAD_BYTES {
            return Err(ArtifactNetTransferError::ChunkPayloadExceedsMax {
                len: self.payload.len(),
                max: MAX_NET_ARTIFACT_CHUNK_PAYLOAD_BYTES,
            });
        }
        let actual_sha: [u8; 32] = Sha256::digest(&self.payload).into();
        if actual_sha != self.chunk_sha256 {
            return Err(ArtifactNetTransferError::LocalChunkDigestMismatch);
        }
        let payload_len = u32::try_from(self.payload.len())
            .map_err(|_| ArtifactNetTransferError::WireCodec("chunk payload u32 overflow"))?;
        let mut out = Vec::with_capacity(ARTIFACT_NET_CHUNK_HEADER_BYTES + self.payload.len());
        out.extend_from_slice(&ARTIFACT_NET_BULK_MAGIC);
        out.extend_from_slice(self.caller_cell.as_bytes());
        out.extend_from_slice(&self.package_manifest_digest);
        out.extend_from_slice(self.total_blob_sha256.as_bytes());
        out.extend_from_slice(&self.total_blob_bytes.to_be_bytes());
        out.extend_from_slice(&self.chunk_index.to_be_bytes());
        out.extend_from_slice(&self.offset.to_be_bytes());
        out.extend_from_slice(&self.chunk_sha256);
        out.extend_from_slice(&payload_len.to_be_bytes());
        out.extend_from_slice(&self.payload);
        Ok(out)
    }

    /// Decodes a wire chunk and cryptographically verifies `SHA-256(payload) == chunk_sha256`.
    ///
    /// # Errors
    ///
    /// Fails closed on magic mismatch, truncated/trailing bytes, empty payload,
    /// or `chunk_sha256` tamper.
    pub fn decode_and_verify(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.len() <= ARTIFACT_NET_CHUNK_HEADER_BYTES || bytes.len() > MAX_BULK_CHUNK_BYTES {
            return Err("artifact bulk chunk frame length out of bounds");
        }
        if bytes[..8] != ARTIFACT_NET_BULK_MAGIC {
            return Err("artifact bulk chunk frame magic mismatch");
        }
        let mut cell_bytes = [0_u8; 16];
        cell_bytes.copy_from_slice(&bytes[8..24]);
        let mut pkg_digest = [0_u8; 32];
        pkg_digest.copy_from_slice(&bytes[24..56]);
        let mut blob_digest = [0_u8; 32];
        blob_digest.copy_from_slice(&bytes[56..88]);
        let mut total_bytes_arr = [0_u8; 8];
        total_bytes_arr.copy_from_slice(&bytes[88..96]);
        let mut idx_arr = [0_u8; 4];
        idx_arr.copy_from_slice(&bytes[96..100]);
        let mut off_arr = [0_u8; 8];
        off_arr.copy_from_slice(&bytes[100..108]);
        let mut chunk_sha = [0_u8; 32];
        chunk_sha.copy_from_slice(&bytes[108..140]);
        let mut len_arr = [0_u8; 4];
        len_arr.copy_from_slice(&bytes[140..144]);
        let payload_len = usize::try_from(u32::from_be_bytes(len_arr))
            .map_err(|_| "artifact bulk chunk payload_len overflow")?;
        if payload_len == 0 || ARTIFACT_NET_CHUNK_HEADER_BYTES + payload_len != bytes.len() {
            return Err("artifact bulk chunk payload_len mismatch");
        }
        let payload = bytes[144..].to_vec();
        let computed: [u8; 32] = Sha256::digest(&payload).into();
        if computed != chunk_sha {
            return Err("artifact bulk chunk sha256 mismatch");
        }
        Ok(Self {
            caller_cell: CellIdentity::from_domain(SchedulerDomainId::from_bytes(cell_bytes)),
            package_manifest_digest: pkg_digest,
            total_blob_sha256: ContentDigest::from_bytes(blob_digest),
            total_blob_bytes: u64::from_be_bytes(total_bytes_arr),
            chunk_index: u32::from_be_bytes(idx_arr),
            offset: u64::from_be_bytes(off_arr),
            chunk_sha256: chunk_sha,
            payload,
        })
    }
}

/// Slices the unverified suffix of `blob` (`cursor.next_offset..`) into encoded
/// [`ArtifactBulkChunkFrame`]s of at most `chunk_payload_bytes`.
///
/// # Errors
///
/// Fails closed if `chunk_payload_bytes` is out of range, `blob` mismatches its
/// declared digest, or `cursor` exceeds `blob.bytes.len()`.
pub fn slice_remaining_blob_into_framed_chunks(
    caller_cell: CellIdentity,
    package_manifest_digest: [u8; 32],
    blob: &ArtifactBlobPayload,
    cursor: &BlobChunkCursor,
    chunk_payload_bytes: u32,
) -> Result<Vec<Vec<u8>>, ArtifactNetTransferError> {
    let chunk_size = usize::try_from(chunk_payload_bytes).unwrap_or(0);
    if chunk_size == 0 || chunk_size > MAX_NET_ARTIFACT_CHUNK_PAYLOAD_BYTES {
        return Err(ArtifactNetTransferError::InvalidChunkSize(
            chunk_payload_bytes,
        ));
    }
    blob.verify_digest()
        .map_err(|_| ArtifactNetTransferError::LocalBlobDigestMismatch(blob.blob_digest))?;
    let total_blob_bytes = u64::try_from(blob.bytes.len()).unwrap_or(u64::MAX);
    if cursor.blob_digest != blob.blob_digest || cursor.total_bytes != total_blob_bytes {
        return Err(ArtifactNetTransferError::CursorDescriptorMismatch(
            blob.blob_digest,
        ));
    }
    if cursor.next_offset > total_blob_bytes {
        return Err(ArtifactNetTransferError::CursorDescriptorMismatch(
            blob.blob_digest,
        ));
    }
    let start = usize::try_from(cursor.next_offset)
        .map_err(|_| ArtifactNetTransferError::WireCodec("cursor offset usize overflow"))?;
    if start == blob.bytes.len() {
        return Ok(Vec::new());
    }

    let mut frames = Vec::new();
    let mut chunk_index = cursor.verified_chunks;
    let mut offset = cursor.next_offset;
    for slice in blob.bytes[start..].chunks(chunk_size) {
        let chunk_sha256: [u8; 32] = Sha256::digest(slice).into();
        let frame = ArtifactBulkChunkFrame {
            caller_cell,
            package_manifest_digest,
            total_blob_sha256: blob.blob_digest,
            total_blob_bytes,
            chunk_index,
            offset,
            chunk_sha256,
            payload: slice.to_vec(),
        };
        frames.push(frame.encode()?);
        chunk_index = chunk_index
            .checked_add(1)
            .ok_or(ArtifactNetTransferError::WireCodec("chunk_index overflow"))?;
        offset = offset.saturating_add(u64::try_from(slice.len()).unwrap_or(0));
    }
    Ok(frames)
}

/// Request parameters for finalizing a fully staged [`ArtifactTransferPackage`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinalizeNetArtifactTransferRequest {
    pub package_manifest_digest: [u8; 32],
    pub expected_local_state: Option<ArtifactLocalSyncState>,
    pub task_id: TaskId,
    pub permit_id: CommitPermitId,
    pub write_set_root: ContentDigest,
    pub synced_at_ms: u64,
}

/// Structured server-side rejection surfaced over `NLOSAT01` control RPCs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactNetTransferRejection {
    /// Malformed or checksum-invalid `NLOSAT01` frame.
    MalformedWireFrame(String),
    /// Declared `caller_cell` did not match mTLS 5-axis `peer_claims.cell_identity`.
    PeerCellIdentityMismatch {
        declared: CellIdentity,
        authenticated_peer: CellIdentity,
    },
    /// Invalid chunk payload size (`0` or `> MAX_NET_ARTIFACT_CHUNK_PAYLOAD_BYTES`).
    InvalidChunkPayloadBytes(u32),
    /// Manifest `package_manifest_digest` did not match recomputed digest.
    PackageDigestMismatch {
        declared: [u8; 32],
        computed: [u8; 32],
    },
    /// Policy registry or sync-engine refusal (`[DIST-ART-001]`).
    PolicyOrSyncRejected(String),
    /// `FinalizeStagedTransfer` called when no staging session exists.
    SessionNotStaged([u8; 32]),
    /// `FinalizeStagedTransfer` called before 100% of declared chunks arrived.
    IncompleteStagedBlobs {
        package_manifest_digest: [u8; 32],
        incomplete_blobs: u32,
    },
    /// Full staged blob `SHA-256` failed verification during finalization.
    StagedBlobDigestMismatch {
        declared: ContentDigest,
        computed: ContentDigest,
    },
    /// Staging filesystem I/O failure.
    StagingIoError(String),
}

impl fmt::Display for ArtifactNetTransferRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl Error for ArtifactNetTransferRejection {}

/// Client-side error type for [`ArtifactNetTransferClient`].
#[derive(Debug)]
pub enum ArtifactNetTransferError {
    /// Pre-dispatch refusal from `nlos-net` (`SingleHostProfile`, `BlindRetryBlocked`, etc.).
    DispatchRefusal(TransportDispatchRefusal),
    /// Wire encoding/decoding failure.
    WireCodec(&'static str),
    /// Invalid chunk payload size (`0` or `> MAX_NET_ARTIFACT_CHUNK_PAYLOAD_BYTES`).
    InvalidChunkSize(u32),
    /// Chunk payload exceeded [`MAX_NET_ARTIFACT_CHUNK_PAYLOAD_BYTES`].
    ChunkPayloadExceedsMax { len: usize, max: usize },
    /// Local chunk digest mismatch before dispatch.
    LocalChunkDigestMismatch,
    /// Local `ArtifactBlobPayload` failed `verify_digest()` before dispatch.
    LocalBlobDigestMismatch(ContentDigest),
    /// Local `ArtifactTransferPackage` failed `compute_digest()` before dispatch.
    LocalPackageDigestMismatch,
    /// Server cursor descriptor did not match local blob digest/length.
    CursorDescriptorMismatch(ContentDigest),
    /// Server returned a structured fail-closed [`ArtifactNetTransferRejection`].
    RemoteRejected(ArtifactNetTransferRejection),
    /// Transport returned an incomplete/uncertain [`RemoteCallOutcome`].
    IncompleteTransportOutcome(&'static str),
}

impl fmt::Display for ArtifactNetTransferError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DispatchRefusal(e) => write!(formatter, "dispatch refusal: {e}"),
            Self::WireCodec(msg) => write!(formatter, "wire codec error: {msg}"),
            Self::InvalidChunkSize(sz) => write!(formatter, "invalid chunk size: {sz}"),
            Self::ChunkPayloadExceedsMax { len, max } => {
                write!(formatter, "chunk payload {len} exceeds max {max}")
            }
            Self::LocalChunkDigestMismatch => formatter.write_str("local chunk sha256 mismatch"),
            Self::LocalBlobDigestMismatch(d) => {
                write!(formatter, "local blob digest mismatch: {d}")
            }
            Self::LocalPackageDigestMismatch => {
                formatter.write_str("local package manifest digest mismatch")
            }
            Self::CursorDescriptorMismatch(d) => {
                write!(formatter, "cursor descriptor mismatch for blob {d}")
            }
            Self::RemoteRejected(r) => write!(formatter, "remote artifact transfer rejected: {r}"),
            Self::IncompleteTransportOutcome(msg) => {
                write!(formatter, "incomplete transport outcome: {msg}")
            }
        }
    }
}

impl Error for ArtifactNetTransferError {}

/// Telemetry summary of one end-to-end or resumed [`ArtifactNetTransferClient`] push.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactTransferExecutionSummary {
    pub package_manifest_digest: [u8; 32],
    pub resumed_from_verified_chunks: u32,
    pub resumed_from_offset_bytes: u64,
    pub chunks_transmitted: u32,
    pub payload_bytes_transmitted: u64,
}

/// Returns the staging directory `<root>/artifacts/tmp/net-stage/<pkg_hex>`
/// used by [`ArtifactNetTransferServer`] for `package_manifest_digest`.
#[must_use]
pub fn staged_transfer_dir_for_package(
    store: &ArtifactStore,
    package_manifest_digest: [u8; 32],
) -> PathBuf {
    store
        .paths()
        .artifacts
        .tmp
        .join("net-stage")
        .join(hex_encode(&package_manifest_digest))
}

/// Server endpoint handling `NLOSAT01` control RPCs and `NLOSAB01` bulk chunk
/// streams while keeping incomplete transfers strictly isolated in
/// `<root>/artifacts/tmp/net-stage/`.
pub struct ArtifactNetTransferServer {
    local_cell: CellIdentity,
    epoch_source: Arc<dyn Fn() -> CellEpoch + Send + Sync>,
    store: Arc<ArtifactStore>,
    policy_registry: Arc<Mutex<ArtifactSyncPolicyRegistry>>,
    retention_set: Arc<Mutex<ReplicaSyncRetentionSet>>,
    staging_lock: Arc<Mutex<()>>,
    finalized_by_key: Mutex<BTreeMap<IdempotencyKey, CrossStoreArtifactSyncReport>>,
    finalized_by_pkg: Mutex<BTreeMap<[u8; 32], CrossStoreArtifactSyncReport>>,
}

impl ArtifactNetTransferServer {
    /// Constructs a server with a fixed `observed_epoch` and fresh retention set.
    #[must_use]
    pub fn new(
        local_cell: CellIdentity,
        observed_epoch: CellEpoch,
        store: Arc<ArtifactStore>,
        policy_registry: ArtifactSyncPolicyRegistry,
    ) -> Self {
        Self::with_shared_state(
            local_cell,
            Arc::new(move || observed_epoch),
            store,
            Arc::new(Mutex::new(policy_registry)),
            Arc::new(Mutex::new(ReplicaSyncRetentionSet::new())),
        )
    }

    /// Constructs a server sharing live epoch, policy registry, and retention
    /// set handles (used by `NetCellHost`).
    #[must_use]
    pub fn with_shared_state(
        local_cell: CellIdentity,
        epoch_source: Arc<dyn Fn() -> CellEpoch + Send + Sync>,
        store: Arc<ArtifactStore>,
        policy_registry: Arc<Mutex<ArtifactSyncPolicyRegistry>>,
        retention_set: Arc<Mutex<ReplicaSyncRetentionSet>>,
    ) -> Self {
        Self {
            local_cell,
            epoch_source,
            store,
            policy_registry,
            retention_set,
            staging_lock: Arc::new(Mutex::new(())),
            finalized_by_key: Mutex::new(BTreeMap::new()),
            finalized_by_pkg: Mutex::new(BTreeMap::new()),
        }
    }

    /// Returns `true` if `payload` starts with [`ARTIFACT_NET_RPC_MAGIC`].
    #[must_use]
    pub fn is_artifact_rpc_frame(payload: &[u8]) -> bool {
        payload.len() >= 8 && payload[..8] == ARTIFACT_NET_RPC_MAGIC
    }

    /// Returns `true` if `chunk` starts with [`ARTIFACT_NET_BULK_MAGIC`].
    #[must_use]
    pub fn is_artifact_bulk_frame(chunk: &[u8]) -> bool {
        chunk.len() >= 8 && chunk[..8] == ARTIFACT_NET_BULK_MAGIC
    }

    /// Returns the local [`CellIdentity`].
    #[must_use]
    pub const fn local_cell(&self) -> CellIdentity {
        self.local_cell
    }

    /// Clones a snapshot of the server's [`ReplicaSyncRetentionSet`].
    #[must_use]
    pub fn snapshot_retention_set(&self) -> ReplicaSyncRetentionSet {
        self.retention_set
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Shared handle to the server's [`ReplicaSyncRetentionSet`].
    #[must_use]
    pub const fn retention_set(&self) -> &Arc<Mutex<ReplicaSyncRetentionSet>> {
        &self.retention_set
    }

    /// Shared handle to the server's [`ArtifactSyncPolicyRegistry`].
    #[must_use]
    pub const fn policy_registry(&self) -> &Arc<Mutex<ArtifactSyncPolicyRegistry>> {
        &self.policy_registry
    }

    /// Synchronously processes an incoming `NLOSAT01` control RPC request.
    #[must_use]
    pub fn handle_incoming_rpc(&self, request: &IncomingRpcRequest) -> RpcHandlerReply {
        let observed_epoch = (self.epoch_source)();
        let authenticated_peer = request.peer_session.peer_claims.cell_identity;
        let decoded = match decode_rpc_request(&request.payload) {
            Ok(req) => req,
            Err(reason) => {
                return rejected_reply(
                    observed_epoch,
                    &ArtifactNetTransferRejection::MalformedWireFrame(reason.to_string()),
                );
            }
        };

        let (reply_res, replayed) = match decoded {
            DecodedRpcReq::BeginSession {
                caller_cell,
                manifest,
                chunk_payload_bytes,
            } => (
                self.exec_begin_session(
                    authenticated_peer,
                    caller_cell,
                    &manifest,
                    chunk_payload_bytes,
                )
                .map(|c| encode_session_cursor_resp(&c)),
                false,
            ),
            DecodedRpcReq::QueryCursor {
                caller_cell,
                package_manifest_digest,
            } => (
                self.exec_query_cursor(authenticated_peer, caller_cell, package_manifest_digest)
                    .map(|state| encode_cursor_state_resp(&state)),
                false,
            ),
            DecodedRpcReq::Finalize {
                caller_cell,
                finalize_req,
            } => match self.exec_finalize(
                authenticated_peer,
                caller_cell,
                request.idempotency_key,
                &finalize_req,
            ) {
                Ok((report, was_replayed)) => (Ok(encode_finalized_resp(&report)), was_replayed),
                Err(rej) => (Err(rej), false),
            },
        };

        match reply_res {
            Ok(payload) => RpcHandlerReply {
                observed_epoch,
                idempotency_replayed: replayed,
                status: RpcWireStatus::Ok,
                payload,
            },
            Err(rej) => rejected_reply(observed_epoch, &rej),
        }
    }

    /// Drains and verifies `<= 64 KiB` chunks from `reader`, persisting each
    /// verified chunk into `<root>/artifacts/tmp/net-stage/<pkg_hex>/` as it
    /// arrives so mid-stream disconnects preserve durable chunk progress.
    ///
    /// # Errors
    ///
    /// Fails closed on peer disconnect, `NLOSAB01` framing/digest error,
    /// `caller_cell` mismatch, unregistered session, out-of-order offset, or
    /// staging I/O failure.
    pub async fn handle_incoming_bulk(
        &self,
        meta: &IncomingBulkMeta,
        mut reader: BulkStreamReader,
    ) -> Result<BulkHandlerCompletion, &'static str> {
        let authenticated_peer = meta.peer_session.peer_claims.cell_identity;
        let mut all_replayed = true;
        let mut saw_any = false;
        while let Some(raw_chunk) = reader.next_chunk().await? {
            saw_any = true;
            let frame = ArtifactBulkChunkFrame::decode_and_verify(&raw_chunk)?;
            if frame.caller_cell != authenticated_peer {
                return Err("artifact bulk chunk caller_cell mismatch with mTLS peer");
            }
            let was_replay = self.stage_verified_chunk(&frame)?;
            if !was_replay {
                all_replayed = false;
            }
        }
        Ok(BulkHandlerCompletion {
            observed_epoch: (self.epoch_source)(),
            idempotency_replayed: saw_any && all_replayed,
        })
    }

    fn exec_begin_session(
        &self,
        authenticated_peer: CellIdentity,
        caller_cell: CellIdentity,
        manifest: &ArtifactTransferManifest,
        chunk_payload_bytes: u32,
    ) -> Result<ArtifactTransferSessionCursor, ArtifactNetTransferRejection> {
        verify_peer_cell(caller_cell, authenticated_peer)?;
        let chunk_sz = usize::try_from(chunk_payload_bytes).unwrap_or(0);
        if chunk_sz == 0 || chunk_sz > MAX_NET_ARTIFACT_CHUNK_PAYLOAD_BYTES {
            return Err(ArtifactNetTransferRejection::InvalidChunkPayloadBytes(
                chunk_payload_bytes,
            ));
        }
        if manifest.origin_cell_id == [0_u8; 16] {
            return Err(ArtifactNetTransferRejection::PolicyOrSyncRejected(
                ArtifactSyncError::ZeroOriginCellId.to_string(),
            ));
        }
        {
            let reg = self
                .policy_registry
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            reg.resolve(&manifest.content_type)
                .map_err(|e| ArtifactNetTransferRejection::PolicyOrSyncRejected(e.to_string()))?;
        }
        let computed = manifest.compute_manifest_digest();
        if computed != manifest.package_manifest_digest {
            return Err(ArtifactNetTransferRejection::PackageDigestMismatch {
                declared: manifest.package_manifest_digest,
                computed,
            });
        }

        let _guard = self
            .staging_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let stage_dir =
            staged_transfer_dir_for_package(&self.store, manifest.package_manifest_digest);
        fs::create_dir_all(&stage_dir)
            .map_err(|e| ArtifactNetTransferRejection::StagingIoError(e.to_string()))?;

        let effective_chunk_bytes = if stage_dir.join("manifest.bin").is_file() {
            let (_, existing_chunk_bytes) = read_stage_manifest(&stage_dir)
                .map_err(|e| ArtifactNetTransferRejection::StagingIoError(e.to_string()))?;
            existing_chunk_bytes
        } else {
            write_stage_manifest(&stage_dir, manifest, chunk_payload_bytes)
                .map_err(|e| ArtifactNetTransferRejection::StagingIoError(e.to_string()))?;
            chunk_payload_bytes
        };

        load_or_init_session_cursor(&stage_dir, manifest, effective_chunk_bytes)
            .map_err(|e| ArtifactNetTransferRejection::StagingIoError(e.to_string()))
    }

    fn exec_query_cursor(
        &self,
        authenticated_peer: CellIdentity,
        caller_cell: CellIdentity,
        package_manifest_digest: [u8; 32],
    ) -> Result<ArtifactTransferCursorState, ArtifactNetTransferRejection> {
        verify_peer_cell(caller_cell, authenticated_peer)?;
        let _guard = self
            .staging_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(report) = self
            .finalized_by_pkg
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&package_manifest_digest)
            .cloned()
        {
            return Ok(ArtifactTransferCursorState::AlreadyFinalized(report));
        }
        let stage_dir = staged_transfer_dir_for_package(&self.store, package_manifest_digest);
        if !stage_dir.join("manifest.bin").is_file() {
            return Ok(ArtifactTransferCursorState::NotStaged);
        }
        let (manifest, chunk_payload_bytes) = read_stage_manifest(&stage_dir)
            .map_err(|e| ArtifactNetTransferRejection::StagingIoError(e.to_string()))?;
        let cursor = load_or_init_session_cursor(&stage_dir, &manifest, chunk_payload_bytes)
            .map_err(|e| ArtifactNetTransferRejection::StagingIoError(e.to_string()))?;
        Ok(ArtifactTransferCursorState::Staged(cursor))
    }

    fn stage_verified_chunk(&self, frame: &ArtifactBulkChunkFrame) -> Result<bool, &'static str> {
        let _guard = self
            .staging_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let stage_dir = staged_transfer_dir_for_package(&self.store, frame.package_manifest_digest);
        if !stage_dir.join("manifest.bin").is_file() {
            return Err("artifact bulk stream session manifest not staged");
        }
        let (manifest, _) =
            read_stage_manifest(&stage_dir).map_err(|_| "corrupt staged manifest.bin")?;
        let descriptor = manifest
            .blobs
            .iter()
            .find(|b| b.blob_digest == frame.total_blob_sha256)
            .ok_or("artifact bulk chunk blob_digest not declared in manifest")?;
        if descriptor.size_bytes != frame.total_blob_bytes {
            return Err("artifact bulk chunk total_blob_bytes mismatch");
        }
        let payload_len = u64::try_from(frame.payload.len()).unwrap_or(0);
        let end_offset = frame
            .offset
            .checked_add(payload_len)
            .ok_or("artifact bulk chunk offset overflow")?;
        if end_offset > descriptor.size_bytes {
            return Err("artifact bulk chunk exceeds declared blob size");
        }

        let cursor = read_or_init_blob_cursor(&stage_dir, *descriptor)
            .map_err(|_| "failed to read staged blob cursor")?;
        let part_path = blob_part_path(&stage_dir, frame.total_blob_sha256);

        if end_offset <= cursor.next_offset {
            verify_existing_part_slice(&part_path, frame.offset, &frame.payload)?;
            return Ok(true);
        }
        if frame.offset != cursor.next_offset || frame.chunk_index != cursor.verified_chunks {
            return Err("artifact bulk chunk out-of-order offset or chunk_index");
        }

        write_chunk_to_part_and_advance_cursor(
            &stage_dir,
            &part_path,
            frame.total_blob_sha256,
            descriptor.size_bytes,
            cursor.verified_chunks + 1,
            frame.offset,
            &frame.payload,
        )?;
        Ok(false)
    }

    fn exec_finalize(
        &self,
        authenticated_peer: CellIdentity,
        caller_cell: CellIdentity,
        idempotency_key: IdempotencyKey,
        req: &FinalizeNetArtifactTransferRequest,
    ) -> Result<(CrossStoreArtifactSyncReport, bool), ArtifactNetTransferRejection> {
        verify_peer_cell(caller_cell, authenticated_peer)?;
        let _guard = self
            .staging_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);

        if let Some(cached) = self
            .finalized_by_key
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&idempotency_key)
            .cloned()
        {
            return Ok((cached, true));
        }

        let stage_dir = staged_transfer_dir_for_package(&self.store, req.package_manifest_digest);
        if !stage_dir.join("manifest.bin").is_file() {
            return Err(ArtifactNetTransferRejection::SessionNotStaged(
                req.package_manifest_digest,
            ));
        }

        let package = assemble_and_verify_staged_package(&stage_dir, req.package_manifest_digest)?;
        let report = {
            let registry = self
                .policy_registry
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let mut retention = self
                .retention_set
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            import_artifact_transfer_package(
                &self.store,
                &registry,
                &mut retention,
                ImportArtifactTransferRequest {
                    package: &package,
                    expected_local_state: req.expected_local_state.as_ref(),
                    task_id: req.task_id,
                    permit_id: req.permit_id,
                    write_set_root: req.write_set_root,
                    idempotency_key,
                    synced_at_ms: req.synced_at_ms,
                },
            )
            .map_err(|e| ArtifactNetTransferRejection::PolicyOrSyncRejected(e.to_string()))?
        };

        let _ = fs::remove_dir_all(&stage_dir);
        self.finalized_by_key
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(idempotency_key, report.clone());
        self.finalized_by_pkg
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(req.package_manifest_digest, report.clone());
        Ok((report, false))
    }
}

impl RpcRequestHandler for ArtifactNetTransferServer {
    fn handle_rpc(
        &self,
        request: IncomingRpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcHandlerReply> + Send + 'static>> {
        let reply = self.handle_incoming_rpc(&request);
        Box::pin(async move { reply })
    }
}

impl BulkStreamHandler for ArtifactNetTransferServer {
    fn handle_bulk_stream(
        &self,
        meta: IncomingBulkMeta,
        reader: BulkStreamReader,
    ) -> Pin<Box<dyn Future<Output = Result<BulkHandlerCompletion, &'static str>> + Send + 'static>>
    {
        // Clone shared handles into an owned helper so the returned future is `'static`.
        let runner = BulkRunnerHandles {
            epoch_source: Arc::clone(&self.epoch_source),
            store: Arc::clone(&self.store),
            staging_lock: Arc::clone(&self.staging_lock),
        };
        Box::pin(async move { runner.run(&meta, reader).await })
    }
}

struct BulkRunnerHandles {
    epoch_source: Arc<dyn Fn() -> CellEpoch + Send + Sync>,
    store: Arc<ArtifactStore>,
    staging_lock: Arc<Mutex<()>>,
}

impl BulkRunnerHandles {
    async fn run(
        &self,
        meta: &IncomingBulkMeta,
        mut reader: BulkStreamReader,
    ) -> Result<BulkHandlerCompletion, &'static str> {
        let authenticated_peer = meta.peer_session.peer_claims.cell_identity;
        let mut all_replayed = true;
        let mut saw_any = false;
        while let Some(raw_chunk) = reader.next_chunk().await? {
            saw_any = true;
            let frame = ArtifactBulkChunkFrame::decode_and_verify(&raw_chunk)?;
            if frame.caller_cell != authenticated_peer {
                return Err("artifact bulk chunk caller_cell mismatch with mTLS peer");
            }
            let was_replay = {
                let _guard = self
                    .staging_lock
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                stage_chunk_on_disk(&self.store, &frame)?
            };
            if !was_replay {
                all_replayed = false;
            }
        }
        Ok(BulkHandlerCompletion {
            observed_epoch: (self.epoch_source)(),
            idempotency_replayed: saw_any && all_replayed,
        })
    }
}

fn stage_chunk_on_disk(
    store: &ArtifactStore,
    frame: &ArtifactBulkChunkFrame,
) -> Result<bool, &'static str> {
    let stage_dir = staged_transfer_dir_for_package(store, frame.package_manifest_digest);
    if !stage_dir.join("manifest.bin").is_file() {
        return Err("artifact bulk stream session manifest not staged");
    }
    let (manifest, _) =
        read_stage_manifest(&stage_dir).map_err(|_| "corrupt staged manifest.bin")?;
    let descriptor = manifest
        .blobs
        .iter()
        .find(|b| b.blob_digest == frame.total_blob_sha256)
        .ok_or("artifact bulk chunk blob_digest not declared in manifest")?;
    if descriptor.size_bytes != frame.total_blob_bytes {
        return Err("artifact bulk chunk total_blob_bytes mismatch");
    }
    let payload_len = u64::try_from(frame.payload.len()).unwrap_or(0);
    let end_offset = frame
        .offset
        .checked_add(payload_len)
        .ok_or("artifact bulk chunk offset overflow")?;
    if end_offset > descriptor.size_bytes {
        return Err("artifact bulk chunk exceeds declared blob size");
    }
    let cursor = read_or_init_blob_cursor(&stage_dir, *descriptor)
        .map_err(|_| "failed to read staged blob cursor")?;
    let part_path = blob_part_path(&stage_dir, frame.total_blob_sha256);
    if end_offset <= cursor.next_offset {
        verify_existing_part_slice(&part_path, frame.offset, &frame.payload)?;
        return Ok(true);
    }
    if frame.offset != cursor.next_offset || frame.chunk_index != cursor.verified_chunks {
        return Err("artifact bulk chunk out-of-order offset or chunk_index");
    }
    write_chunk_to_part_and_advance_cursor(
        &stage_dir,
        &part_path,
        frame.total_blob_sha256,
        descriptor.size_bytes,
        cursor.verified_chunks + 1,
        frame.offset,
        &frame.payload,
    )?;
    Ok(false)
}

fn verify_peer_cell(
    declared: CellIdentity,
    authenticated_peer: CellIdentity,
) -> Result<(), ArtifactNetTransferRejection> {
    if declared != authenticated_peer {
        return Err(ArtifactNetTransferRejection::PeerCellIdentityMismatch {
            declared,
            authenticated_peer,
        });
    }
    Ok(())
}

fn rejected_reply(
    observed_epoch: CellEpoch,
    rejection: &ArtifactNetTransferRejection,
) -> RpcHandlerReply {
    RpcHandlerReply {
        observed_epoch,
        idempotency_replayed: false,
        status: RpcWireStatus::Ok,
        payload: encode_rejected_resp(rejection),
    }
}

fn blob_part_path(stage_dir: &Path, digest: ContentDigest) -> PathBuf {
    stage_dir.join(format!("{}.part", digest.to_hex()))
}

fn blob_cursor_path(stage_dir: &Path, digest: ContentDigest) -> PathBuf {
    stage_dir.join(format!("{}.cursor", digest.to_hex()))
}

fn load_or_init_session_cursor(
    stage_dir: &Path,
    manifest: &ArtifactTransferManifest,
    chunk_payload_bytes: u32,
) -> Result<ArtifactTransferSessionCursor, std::io::Error> {
    let mut blob_cursors = Vec::with_capacity(manifest.blobs.len());
    for descriptor in &manifest.blobs {
        let cursor = read_or_init_blob_cursor(stage_dir, *descriptor)?;
        blob_cursors.push(cursor);
    }
    Ok(ArtifactTransferSessionCursor {
        package_manifest_digest: manifest.package_manifest_digest,
        chunk_payload_bytes,
        blob_cursors,
    })
}

fn read_or_init_blob_cursor(
    stage_dir: &Path,
    descriptor: ArtifactBlobDescriptor,
) -> Result<BlobChunkCursor, std::io::Error> {
    if descriptor.size_bytes == 0 {
        let empty_path = blob_part_path(stage_dir, descriptor.blob_digest);
        if !empty_path.is_file() {
            fs::write(&empty_path, [])?;
        }
        return Ok(BlobChunkCursor {
            blob_digest: descriptor.blob_digest,
            total_bytes: 0,
            verified_chunks: 0,
            next_offset: 0,
        });
    }

    let cursor_path = blob_cursor_path(stage_dir, descriptor.blob_digest);
    let part_path = blob_part_path(stage_dir, descriptor.blob_digest);
    if !cursor_path.is_file() {
        return Ok(BlobChunkCursor {
            blob_digest: descriptor.blob_digest,
            total_bytes: descriptor.size_bytes,
            verified_chunks: 0,
            next_offset: 0,
        });
    }
    let bytes = fs::read(&cursor_path)?;
    if bytes.len() != 8 + 32 + 8 + 4 + 8 + 32 || bytes[..8] != STAGE_CURSOR_MAGIC {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "corrupt blob cursor header",
        ));
    }
    let body = &bytes[..60];
    let expected_sum: [u8; 32] = Sha256::digest(body).into();
    if expected_sum != bytes[60..92] {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "corrupt blob cursor sha256",
        ));
    }
    let mut d_bytes = [0_u8; 32];
    d_bytes.copy_from_slice(&bytes[8..40]);
    let mut total_arr = [0_u8; 8];
    total_arr.copy_from_slice(&bytes[40..48]);
    let mut chunks_arr = [0_u8; 4];
    chunks_arr.copy_from_slice(&bytes[48..52]);
    let mut off_arr = [0_u8; 8];
    off_arr.copy_from_slice(&bytes[52..60]);
    let next_offset = u64::from_be_bytes(off_arr);
    if ContentDigest::from_bytes(d_bytes) != descriptor.blob_digest
        || u64::from_be_bytes(total_arr) != descriptor.size_bytes
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "blob cursor descriptor mismatch",
        ));
    }
    let file_len = fs::metadata(&part_path).map_or(0, |m| m.len());
    if file_len < next_offset {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "staged .part shorter than recorded cursor offset",
        ));
    }
    Ok(BlobChunkCursor {
        blob_digest: descriptor.blob_digest,
        total_bytes: descriptor.size_bytes,
        verified_chunks: u32::from_be_bytes(chunks_arr),
        next_offset,
    })
}

fn verify_existing_part_slice(
    part_path: &Path,
    offset: u64,
    expected_payload: &[u8],
) -> Result<(), &'static str> {
    let mut file = OpenOptions::new()
        .read(true)
        .open(part_path)
        .map_err(|_| "failed to open staged .part for replay check")?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|_| "failed to seek staged .part for replay check")?;
    let mut existing = vec![0_u8; expected_payload.len()];
    file.read_exact(&mut existing)
        .map_err(|_| "failed to read staged .part for replay check")?;
    if existing != expected_payload {
        return Err("replayed chunk bytes differ from staged .part bytes");
    }
    Ok(())
}

fn write_chunk_to_part_and_advance_cursor(
    stage_dir: &Path,
    part_path: &Path,
    blob_digest: ContentDigest,
    total_bytes: u64,
    new_verified_chunks: u32,
    offset: u64,
    payload: &[u8],
) -> Result<(), &'static str> {
    let new_offset = offset + u64::try_from(payload.len()).unwrap_or(0);
    {
        let mut part = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(part_path)
            .map_err(|_| "failed to open staged .part file")?;
        part.seek(SeekFrom::Start(offset))
            .map_err(|_| "failed to seek staged .part file")?;
        part.write_all(payload)
            .map_err(|_| "failed to write staged .part chunk")?;
        part.set_len(new_offset)
            .map_err(|_| "failed to truncate uncommitted tail on .part file")?;
        part.sync_data()
            .map_err(|_| "failed to fsync staged .part chunk")?;
    }

    let mut body = Vec::with_capacity(92);
    body.extend_from_slice(&STAGE_CURSOR_MAGIC);
    body.extend_from_slice(blob_digest.as_bytes());
    body.extend_from_slice(&total_bytes.to_be_bytes());
    body.extend_from_slice(&new_verified_chunks.to_be_bytes());
    body.extend_from_slice(&new_offset.to_be_bytes());
    let sum: [u8; 32] = Sha256::digest(&body).into();
    body.extend_from_slice(&sum);

    let tmp_cursor = stage_dir.join(format!("{}.cursor.tmp", blob_digest.to_hex()));
    let final_cursor = blob_cursor_path(stage_dir, blob_digest);
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_cursor)
            .map_err(|_| "failed to create .cursor.tmp")?;
        f.write_all(&body)
            .map_err(|_| "failed to write .cursor.tmp")?;
        f.sync_data().map_err(|_| "failed to fsync .cursor.tmp")?;
    }
    fs::rename(&tmp_cursor, &final_cursor).map_err(|_| "failed to rename .cursor.tmp")?;
    Ok(())
}

fn assemble_and_verify_staged_package(
    stage_dir: &Path,
    expected_pkg_digest: [u8; 32],
) -> Result<ArtifactTransferPackage, ArtifactNetTransferRejection> {
    let (manifest, chunk_payload_bytes) = read_stage_manifest(stage_dir)
        .map_err(|e| ArtifactNetTransferRejection::StagingIoError(e.to_string()))?;
    let cursor = load_or_init_session_cursor(stage_dir, &manifest, chunk_payload_bytes)
        .map_err(|e| ArtifactNetTransferRejection::StagingIoError(e.to_string()))?;
    let incomplete_count = u32::try_from(
        cursor
            .blob_cursors
            .iter()
            .filter(|c| !c.is_complete())
            .count(),
    )
    .unwrap_or(u32::MAX);
    if incomplete_count > 0 {
        return Err(ArtifactNetTransferRejection::IncompleteStagedBlobs {
            package_manifest_digest: expected_pkg_digest,
            incomplete_blobs: incomplete_count,
        });
    }

    let mut blobs = Vec::with_capacity(manifest.blobs.len());
    let mut tip_bytes: Option<Vec<u8>> = None;
    for descriptor in &manifest.blobs {
        let part_path = blob_part_path(stage_dir, descriptor.blob_digest);
        let bytes = fs::read(&part_path)
            .map_err(|e| ArtifactNetTransferRejection::StagingIoError(e.to_string()))?;
        let computed = ContentDigest::of_bytes(&bytes);
        if computed != descriptor.blob_digest {
            return Err(ArtifactNetTransferRejection::StagedBlobDigestMismatch {
                declared: descriptor.blob_digest,
                computed,
            });
        }
        if descriptor.blob_digest == manifest.remote_head.digest {
            tip_bytes = Some(bytes.clone());
        }
        blobs.push(ArtifactBlobPayload {
            blob_digest: descriptor.blob_digest,
            bytes,
        });
    }

    let tip_bytes = tip_bytes.ok_or_else(|| {
        ArtifactNetTransferRejection::PolicyOrSyncRejected(
            "remote_head.digest missing from declared blobs".to_string(),
        )
    })?;
    let bundle = ReplicaRevisionBundle {
        artifact_id: manifest.artifact_id,
        content_type: manifest.content_type,
        origin_cell_id: manifest.origin_cell_id,
        remote_head: manifest.remote_head,
        parent_head: manifest.parent_head,
        ancestor_lineage: manifest.ancestor_lineage,
        is_tombstone: manifest.is_tombstone,
        bytes: tip_bytes,
    };
    let computed_pkg = compute_transfer_package_digest(
        &bundle,
        manifest.application_id,
        manifest.owner.as_deref(),
        manifest.artifact_created_at_ms,
        manifest.producer_epoch,
        manifest.tombstone,
        &manifest.revisions,
        &blobs,
    );
    if computed_pkg != expected_pkg_digest {
        return Err(ArtifactNetTransferRejection::PackageDigestMismatch {
            declared: expected_pkg_digest,
            computed: computed_pkg,
        });
    }
    Ok(ArtifactTransferPackage {
        bundle,
        application_id: manifest.application_id,
        owner: manifest.owner,
        artifact_created_at_ms: manifest.artifact_created_at_ms,
        producer_epoch: manifest.producer_epoch,
        tombstone: manifest.tombstone,
        revisions: manifest.revisions,
        blobs,
        package_digest: expected_pkg_digest,
    })
}

fn write_stage_manifest(
    stage_dir: &Path,
    manifest: &ArtifactTransferManifest,
    chunk_payload_bytes: u32,
) -> Result<(), std::io::Error> {
    let mut body = Vec::new();
    body.extend_from_slice(&STAGE_MANIFEST_MAGIC);
    body.extend_from_slice(&chunk_payload_bytes.to_be_bytes());
    encode_manifest(&mut body, manifest);
    let checksum: [u8; 32] = Sha256::digest(&body).into();
    body.extend_from_slice(&checksum);

    let tmp_path = stage_dir.join("manifest.bin.tmp");
    let final_path = stage_dir.join("manifest.bin");
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)?;
        f.write_all(&body)?;
        f.sync_data()?;
    }
    fs::rename(&tmp_path, &final_path)
}

fn read_stage_manifest(
    stage_dir: &Path,
) -> Result<(ArtifactTransferManifest, u32), std::io::Error> {
    let raw = fs::read(stage_dir.join("manifest.bin"))?;
    if raw.len() < 8 + 4 + 32 || raw[..8] != STAGE_MANIFEST_MAGIC {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid manifest.bin magic",
        ));
    }
    let body_end = raw.len() - 32;
    let sum: [u8; 32] = Sha256::digest(&raw[..body_end]).into();
    if sum != raw[body_end..] {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "manifest.bin checksum mismatch",
        ));
    }
    let mut r = WireReader::new(&raw[8..body_end]);
    let chunk_bytes = r.read_u32().map_err(io_invalid_data)?;
    let manifest = decode_manifest(&mut r).map_err(io_invalid_data)?;
    r.finish().map_err(io_invalid_data)?;
    Ok((manifest, chunk_bytes))
}

fn io_invalid_data(msg: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

/// Client orchestrator streaming content-addressed [`ArtifactTransferPackage`]s
/// in bounded `<= 64 KiB` chunks with crash/disconnect cursor reconciliation.
pub struct ArtifactNetTransferClient<'a, T> {
    local_cell: CellIdentity,
    transport: &'a T,
    chunk_payload_bytes: u32,
}

impl<'a, T> ArtifactNetTransferClient<'a, T>
where
    T: CellRpcTransport + BulkStreamTransport,
{
    /// Creates a client using [`DEFAULT_NET_ARTIFACT_CHUNK_PAYLOAD_BYTES`] (`60 KiB`).
    #[must_use]
    pub const fn new(local_cell: CellIdentity, transport: &'a T) -> Self {
        Self {
            local_cell,
            transport,
            chunk_payload_bytes: DEFAULT_NET_ARTIFACT_CHUNK_PAYLOAD_BYTES,
        }
    }

    /// Overrides the chunk payload slice size (`1..=MAX_NET_ARTIFACT_CHUNK_PAYLOAD_BYTES`).
    #[must_use]
    pub const fn with_chunk_payload_bytes(mut self, chunk_payload_bytes: u32) -> Self {
        self.chunk_payload_bytes = chunk_payload_bytes;
        self
    }

    /// Registers `manifest` on `target` (`side_effecting = false`) and returns
    /// the current [`ArtifactTransferSessionCursor`].
    ///
    /// # Errors
    ///
    /// Fails closed on pre-dispatch refusal or malformed response framing.
    pub async fn begin_transfer_session(
        &self,
        target: CellIdentity,
        contract: &RemoteCallContract,
        manifest: &ArtifactTransferManifest,
    ) -> Result<
        RemoteCallOutcome<Result<ArtifactTransferSessionCursor, ArtifactNetTransferRejection>>,
        ArtifactNetTransferError,
    > {
        let payload = encode_begin_session_req(self.local_cell, manifest, self.chunk_payload_bytes);
        let outcome = self
            .transport
            .call_cell_rpc(target, contract, payload)
            .await
            .map_err(ArtifactNetTransferError::DispatchRefusal)?;
        map_rpc_outcome(outcome, decode_session_cursor_resp)
    }

    /// Queries the authoritative staged cursor state on `target` (`side_effecting = false`)
    /// for post-disconnect reconciliation (`[DIST-FAIL-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed on pre-dispatch refusal or malformed response framing.
    pub async fn query_transfer_cursor(
        &self,
        target: CellIdentity,
        contract: &RemoteCallContract,
        package_manifest_digest: [u8; 32],
    ) -> Result<
        RemoteCallOutcome<Result<ArtifactTransferCursorState, ArtifactNetTransferRejection>>,
        ArtifactNetTransferError,
    > {
        let payload = encode_query_cursor_req(self.local_cell, package_manifest_digest);
        let outcome = self
            .transport
            .call_cell_rpc(target, contract, payload)
            .await
            .map_err(ArtifactNetTransferError::DispatchRefusal)?;
        map_rpc_outcome(outcome, decode_cursor_state_resp)
    }

    /// Streams ONLY the unverified suffix of `blob` (`cursor.next_offset..`)
    /// over `BulkStreamTransport` in `<= 64 KiB` framed chunks.
    ///
    /// # Errors
    ///
    /// Fails closed if local blob/chunk validation fails or transport refuses dispatch.
    pub async fn stream_remaining_blob_chunks(
        &self,
        target: CellIdentity,
        contract: &RemoteCallContract,
        package_manifest_digest: [u8; 32],
        blob: &ArtifactBlobPayload,
        cursor: &BlobChunkCursor,
        chunk_payload_bytes: u32,
    ) -> Result<RemoteCallOutcome<Option<BulkTransferReceipt>>, ArtifactNetTransferError> {
        let frames = slice_remaining_blob_into_framed_chunks(
            self.local_cell,
            package_manifest_digest,
            blob,
            cursor,
            chunk_payload_bytes,
        )?;
        if frames.is_empty() {
            return Ok(RemoteCallOutcome::Succeeded {
                value: None,
                observed_epoch: CellEpoch::INITIAL,
                idempotency_replayed: true,
            });
        }
        let outcome = self
            .transport
            .stream_bulk_chunks(target, contract, frames)
            .await
            .map_err(ArtifactNetTransferError::DispatchRefusal)?;
        Ok(match outcome {
            RemoteCallOutcome::Succeeded {
                value,
                observed_epoch,
                idempotency_replayed,
            } => RemoteCallOutcome::Succeeded {
                value: Some(value),
                observed_epoch,
                idempotency_replayed,
            },
            RemoteCallOutcome::Unavailable {
                deadline_tick,
                retry_directive,
                idempotency_key,
            } => RemoteCallOutcome::Unavailable {
                deadline_tick,
                retry_directive,
                idempotency_key,
            },
            RemoteCallOutcome::DeadlineExceededUncertain {
                deadline_tick,
                elapsed_ticks,
                side_effecting,
                retry_directive,
                idempotency_key,
            } => RemoteCallOutcome::DeadlineExceededUncertain {
                deadline_tick,
                elapsed_ticks,
                side_effecting,
                retry_directive,
                idempotency_key,
            },
            RemoteCallOutcome::Partial {
                completed_shards,
                failed_shards,
                partial_value: _,
                uncertainty_digest,
                retry_directive,
            } => RemoteCallOutcome::Partial {
                completed_shards,
                failed_shards,
                partial_value: None,
                uncertainty_digest,
                retry_directive,
            },
        })
    }

    /// Finalizes a fully staged transfer on `target` (`side_effecting = true`),
    /// running full blob `SHA-256`, `package_manifest_digest`, and
    /// `[DIST-ART-001]` policy checks before atomically updating `ArtifactStore`.
    ///
    /// # Errors
    ///
    /// Fails closed on pre-dispatch refusal or malformed response framing.
    pub async fn finalize_staged_transfer(
        &self,
        target: CellIdentity,
        contract: &RemoteCallContract,
        request: &FinalizeNetArtifactTransferRequest,
    ) -> Result<
        RemoteCallOutcome<Result<CrossStoreArtifactSyncReport, ArtifactNetTransferRejection>>,
        ArtifactNetTransferError,
    > {
        let payload = encode_finalize_req(self.local_cell, request);
        let outcome = self
            .transport
            .call_cell_rpc(target, contract, payload)
            .await
            .map_err(ArtifactNetTransferError::DispatchRefusal)?;
        map_rpc_outcome(outcome, decode_finalized_resp)
    }

    /// Executes an end-to-end (or post-reconciliation resumed) chunked transfer
    /// of `package` to `target`:
    /// `BeginTransferSession -> BulkStream remaining chunks -> FinalizeStagedTransfer`.
    ///
    /// # Errors
    ///
    /// Fails closed on local package digest mismatch, transport failure, or
    /// remote policy/integrity rejection.
    #[allow(clippy::too_many_arguments)]
    pub async fn transfer_package_resumable(
        &self,
        target: CellIdentity,
        package: &ArtifactTransferPackage,
        expected_local_state: Option<ArtifactLocalSyncState>,
        task_id: TaskId,
        permit_id: CommitPermitId,
        write_set_root: ContentDigest,
        synced_at_ms: u64,
        seed_tag: u8,
    ) -> Result<
        (
            ArtifactTransferExecutionSummary,
            CrossStoreArtifactSyncReport,
        ),
        ArtifactNetTransferError,
    > {
        if package.compute_digest() != package.package_digest {
            return Err(ArtifactNetTransferError::LocalPackageDigestMismatch);
        }
        let manifest = ArtifactTransferManifest::from_package(package);
        let begin_contract = derive_contract(package.package_digest, seed_tag, 0, false);
        let session_cursor = unwrap_rpc_outcome(
            self.begin_transfer_session(target, &begin_contract, &manifest)
                .await?,
        )?;

        let resumed_chunks = session_cursor.total_verified_chunks();
        let resumed_bytes = session_cursor.total_verified_bytes();
        let mut chunks_transmitted: u32 = 0;
        let mut payload_bytes_transmitted: u64 = 0;

        for (idx, blob) in package.blobs.iter().enumerate() {
            let cursor = session_cursor.cursor_for(blob.blob_digest).ok_or(
                ArtifactNetTransferError::CursorDescriptorMismatch(blob.blob_digest),
            )?;
            if cursor.is_complete() {
                continue;
            }
            let remaining_bytes = cursor.total_bytes.saturating_sub(cursor.next_offset);
            let step = u32::try_from(idx + 1).unwrap_or(u32::MAX);
            let bulk_contract = derive_contract(package.package_digest, seed_tag, step, true);
            let bulk_outcome = self
                .stream_remaining_blob_chunks(
                    target,
                    &bulk_contract,
                    package.package_digest,
                    blob,
                    &cursor,
                    session_cursor.chunk_payload_bytes,
                )
                .await?;
            match bulk_outcome {
                RemoteCallOutcome::Succeeded {
                    value: Some(receipt),
                    ..
                } => {
                    chunks_transmitted = chunks_transmitted.saturating_add(receipt.total_chunks);
                    payload_bytes_transmitted =
                        payload_bytes_transmitted.saturating_add(remaining_bytes);
                }
                RemoteCallOutcome::Succeeded { value: None, .. } => {}
                other => return Err(classify_incomplete_outcome(&other)),
            }
        }

        let finalize_contract = derive_contract(package.package_digest, seed_tag, u32::MAX, true);
        let finalize_req = FinalizeNetArtifactTransferRequest {
            package_manifest_digest: package.package_digest,
            expected_local_state,
            task_id,
            permit_id,
            write_set_root,
            synced_at_ms,
        };
        let report = unwrap_rpc_outcome(
            self.finalize_staged_transfer(target, &finalize_contract, &finalize_req)
                .await?,
        )?;
        Ok((
            ArtifactTransferExecutionSummary {
                package_manifest_digest: package.package_digest,
                resumed_from_verified_chunks: resumed_chunks,
                resumed_from_offset_bytes: resumed_bytes,
                chunks_transmitted,
                payload_bytes_transmitted,
            },
            report,
        ))
    }

    /// Reconciles an interrupted bulk-stream `IdempotencyKey` via
    /// `QueryTransferCursor` (`[DIST-FAIL-001]`), releases the client-side
    /// [`PendingReconciliationTracker`] fence, and returns the authoritative
    /// [`ArtifactTransferCursorState`].
    ///
    /// # Errors
    ///
    /// Fails closed if the cursor query RPC fails or is rejected by `target`.
    pub async fn reconcile_interrupted_stream(
        &self,
        target: CellIdentity,
        reconciliation_tracker: &PendingReconciliationTracker,
        interrupted_key: IdempotencyKey,
        query_contract: &RemoteCallContract,
        package_manifest_digest: [u8; 32],
    ) -> Result<ArtifactTransferCursorState, ArtifactNetTransferError> {
        let state = unwrap_rpc_outcome(
            self.query_transfer_cursor(target, query_contract, package_manifest_digest)
                .await?,
        )?;
        reconciliation_tracker.mark_reconciled(interrupted_key);
        Ok(state)
    }
}

fn derive_contract(
    pkg_digest: [u8; 32],
    seed_tag: u8,
    step: u32,
    side_effecting: bool,
) -> RemoteCallContract {
    let mut call_hasher = Sha256::new();
    call_hasher.update(b"llmos/artifact-net-transfer/call/v1");
    call_hasher.update(pkg_digest);
    call_hasher.update([seed_tag]);
    call_hasher.update(step.to_be_bytes());
    let call_hash: [u8; 32] = call_hasher.finalize().into();
    let mut call_id = [0_u8; 16];
    call_id.copy_from_slice(&call_hash[..16]);

    let mut idem_hasher = Sha256::new();
    idem_hasher.update(b"llmos/artifact-net-transfer/idem/v1");
    idem_hasher.update(pkg_digest);
    idem_hasher.update([seed_tag]);
    idem_hasher.update(step.to_be_bytes());
    let idem_hash: [u8; 32] = idem_hasher.finalize().into();
    let mut idem_bytes = [0_u8; 16];
    idem_bytes.copy_from_slice(&idem_hash[..16]);

    RemoteCallContract::new(
        call_id,
        IdempotencyKey::from_bytes(idem_bytes),
        100,
        2,
        0,
        side_effecting,
    )
}

type RpcDecodeFn<R> = fn(&[u8]) -> Result<Result<R, ArtifactNetTransferRejection>, &'static str>;

fn map_rpc_outcome<R>(
    outcome: RemoteCallOutcome<nlos_net::RpcResponsePayload>,
    decode_fn: RpcDecodeFn<R>,
) -> Result<RemoteCallOutcome<Result<R, ArtifactNetTransferRejection>>, ArtifactNetTransferError> {
    match outcome {
        RemoteCallOutcome::Succeeded {
            value,
            observed_epoch,
            idempotency_replayed,
        } => {
            let parsed = decode_fn(&value.payload).map_err(ArtifactNetTransferError::WireCodec)?;
            Ok(RemoteCallOutcome::Succeeded {
                value: parsed,
                observed_epoch,
                idempotency_replayed,
            })
        }
        RemoteCallOutcome::Unavailable {
            deadline_tick,
            retry_directive,
            idempotency_key,
        } => Ok(RemoteCallOutcome::Unavailable {
            deadline_tick,
            retry_directive,
            idempotency_key,
        }),
        RemoteCallOutcome::DeadlineExceededUncertain {
            deadline_tick,
            elapsed_ticks,
            side_effecting,
            retry_directive,
            idempotency_key,
        } => Ok(RemoteCallOutcome::DeadlineExceededUncertain {
            deadline_tick,
            elapsed_ticks,
            side_effecting,
            retry_directive,
            idempotency_key,
        }),
        RemoteCallOutcome::Partial { .. } => Err(
            ArtifactNetTransferError::IncompleteTransportOutcome("unexpected partial rpc outcome"),
        ),
    }
}

fn unwrap_rpc_outcome<R>(
    outcome: RemoteCallOutcome<Result<R, ArtifactNetTransferRejection>>,
) -> Result<R, ArtifactNetTransferError> {
    match outcome {
        RemoteCallOutcome::Succeeded { value: Ok(v), .. } => Ok(v),
        RemoteCallOutcome::Succeeded {
            value: Err(rej), ..
        } => Err(ArtifactNetTransferError::RemoteRejected(rej)),
        other => Err(classify_incomplete_outcome(&other)),
    }
}

const fn classify_incomplete_outcome<T>(
    outcome: &RemoteCallOutcome<T>,
) -> ArtifactNetTransferError {
    match outcome {
        RemoteCallOutcome::Unavailable { .. } => {
            ArtifactNetTransferError::IncompleteTransportOutcome(
                "remote peer unavailable before payload write",
            )
        }
        RemoteCallOutcome::DeadlineExceededUncertain { .. } => {
            ArtifactNetTransferError::IncompleteTransportOutcome(
                "deadline exceeded or stream cut mid-flight; reconciliation required",
            )
        }
        RemoteCallOutcome::Partial { .. } | RemoteCallOutcome::Succeeded { .. } => {
            ArtifactNetTransferError::IncompleteTransportOutcome("unexpected remote outcome")
        }
    }
}

// ============================================================================
// Checksummed Binary Wire Codec (`NLOSAT01` + SHA-256)
// ============================================================================

enum DecodedRpcReq {
    BeginSession {
        caller_cell: CellIdentity,
        manifest: ArtifactTransferManifest,
        chunk_payload_bytes: u32,
    },
    QueryCursor {
        caller_cell: CellIdentity,
        package_manifest_digest: [u8; 32],
    },
    Finalize {
        caller_cell: CellIdentity,
        finalize_req: FinalizeNetArtifactTransferRequest,
    },
}

fn wrap_rpc_envelope(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(9 + body.len() + 32);
    frame.extend_from_slice(&ARTIFACT_NET_RPC_MAGIC);
    frame.push(tag);
    frame.extend_from_slice(body);
    let sum: [u8; 32] = Sha256::digest(&frame).into();
    frame.extend_from_slice(&sum);
    frame
}

fn unwrap_rpc_envelope(raw: &[u8]) -> Result<(u8, &[u8]), &'static str> {
    if raw.len() < 9 + 32 {
        return Err("NLOSAT01 frame shorter than minimum envelope");
    }
    if raw[..8] != ARTIFACT_NET_RPC_MAGIC {
        return Err("NLOSAT01 magic mismatch");
    }
    let body_end = raw.len() - 32;
    let sum: [u8; 32] = Sha256::digest(&raw[..body_end]).into();
    if sum != raw[body_end..] {
        return Err("NLOSAT01 frame sha256 checksum mismatch");
    }
    Ok((raw[8], &raw[9..body_end]))
}

fn encode_begin_session_req(
    caller_cell: CellIdentity,
    manifest: &ArtifactTransferManifest,
    chunk_payload_bytes: u32,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(caller_cell.as_bytes());
    body.extend_from_slice(&chunk_payload_bytes.to_be_bytes());
    encode_manifest(&mut body, manifest);
    wrap_rpc_envelope(TAG_REQ_BEGIN_SESSION, &body)
}

fn encode_query_cursor_req(
    caller_cell: CellIdentity,
    package_manifest_digest: [u8; 32],
) -> Vec<u8> {
    let mut body = Vec::with_capacity(48);
    body.extend_from_slice(caller_cell.as_bytes());
    body.extend_from_slice(&package_manifest_digest);
    wrap_rpc_envelope(TAG_REQ_QUERY_CURSOR, &body)
}

fn encode_finalize_req(
    caller_cell: CellIdentity,
    req: &FinalizeNetArtifactTransferRequest,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(caller_cell.as_bytes());
    body.extend_from_slice(&req.package_manifest_digest);
    match &req.expected_local_state {
        Some(st) => {
            body.push(1);
            body.extend_from_slice(st.artifact_id.as_bytes());
            encode_str(&mut body, &st.content_type);
            encode_opt_head(&mut body, st.local_head);
        }
        None => body.push(0),
    }
    body.extend_from_slice(req.task_id.as_bytes());
    body.extend_from_slice(req.permit_id.as_bytes());
    body.extend_from_slice(req.write_set_root.as_bytes());
    body.extend_from_slice(&req.synced_at_ms.to_be_bytes());
    wrap_rpc_envelope(TAG_REQ_FINALIZE_TRANSFER, &body)
}

fn decode_rpc_request(raw: &[u8]) -> Result<DecodedRpcReq, &'static str> {
    let (tag, body) = unwrap_rpc_envelope(raw)?;
    let mut r = WireReader::new(body);
    let decoded = match tag {
        TAG_REQ_BEGIN_SESSION => {
            let caller_cell =
                CellIdentity::from_domain(SchedulerDomainId::from_bytes(r.read_arr::<16>()?));
            let chunk_payload_bytes = r.read_u32()?;
            let manifest = decode_manifest(&mut r)?;
            DecodedRpcReq::BeginSession {
                caller_cell,
                manifest,
                chunk_payload_bytes,
            }
        }
        TAG_REQ_QUERY_CURSOR => {
            let caller_cell =
                CellIdentity::from_domain(SchedulerDomainId::from_bytes(r.read_arr::<16>()?));
            let package_manifest_digest = r.read_arr::<32>()?;
            DecodedRpcReq::QueryCursor {
                caller_cell,
                package_manifest_digest,
            }
        }
        TAG_REQ_FINALIZE_TRANSFER => {
            let caller_cell =
                CellIdentity::from_domain(SchedulerDomainId::from_bytes(r.read_arr::<16>()?));
            let package_manifest_digest = r.read_arr::<32>()?;
            let expected_local_state = match r.read_u8()? {
                0 => None,
                1 => Some(ArtifactLocalSyncState {
                    artifact_id: ArtifactId::from_bytes(r.read_arr::<16>()?),
                    content_type: r.read_str()?,
                    local_head: decode_opt_head(&mut r)?,
                }),
                _ => return Err("invalid expected_local_state tag"),
            };
            let task_id = TaskId::from_bytes(r.read_arr::<16>()?);
            let permit_id = CommitPermitId::from_bytes(r.read_arr::<16>()?);
            let write_set_root = ContentDigest::from_bytes(r.read_arr::<32>()?);
            let synced_at_ms = r.read_u64()?;
            DecodedRpcReq::Finalize {
                caller_cell,
                finalize_req: FinalizeNetArtifactTransferRequest {
                    package_manifest_digest,
                    expected_local_state,
                    task_id,
                    permit_id,
                    write_set_root,
                    synced_at_ms,
                },
            }
        }
        _ => return Err("unknown NLOSAT01 request tag"),
    };
    r.finish()?;
    Ok(decoded)
}

fn encode_session_cursor_resp(cursor: &ArtifactTransferSessionCursor) -> Vec<u8> {
    let mut body = Vec::new();
    encode_session_cursor(&mut body, cursor);
    wrap_rpc_envelope(TAG_RESP_SESSION_CURSOR, &body)
}

fn decode_session_cursor_resp(
    raw: &[u8],
) -> Result<Result<ArtifactTransferSessionCursor, ArtifactNetTransferRejection>, &'static str> {
    let (tag, body) = unwrap_rpc_envelope(raw)?;
    let mut r = WireReader::new(body);
    if tag == TAG_RESP_REJECTED {
        let rej = decode_rejection(&mut r)?;
        r.finish()?;
        return Ok(Err(rej));
    }
    if tag != TAG_RESP_SESSION_CURSOR {
        return Err("unexpected NLOSAT01 response tag for session cursor");
    }
    let cursor = decode_session_cursor(&mut r)?;
    r.finish()?;
    Ok(Ok(cursor))
}

fn encode_cursor_state_resp(state: &ArtifactTransferCursorState) -> Vec<u8> {
    let mut body = Vec::new();
    match state {
        ArtifactTransferCursorState::Staged(cursor) => {
            body.push(1);
            encode_session_cursor(&mut body, cursor);
        }
        ArtifactTransferCursorState::AlreadyFinalized(report) => {
            body.push(2);
            encode_sync_report(&mut body, report);
        }
        ArtifactTransferCursorState::NotStaged => body.push(3),
    }
    wrap_rpc_envelope(TAG_RESP_CURSOR_STATE, &body)
}

fn decode_cursor_state_resp(
    raw: &[u8],
) -> Result<Result<ArtifactTransferCursorState, ArtifactNetTransferRejection>, &'static str> {
    let (tag, body) = unwrap_rpc_envelope(raw)?;
    let mut r = WireReader::new(body);
    if tag == TAG_RESP_REJECTED {
        let rej = decode_rejection(&mut r)?;
        r.finish()?;
        return Ok(Err(rej));
    }
    if tag != TAG_RESP_CURSOR_STATE {
        return Err("unexpected NLOSAT01 response tag for cursor state");
    }
    let state = match r.read_u8()? {
        1 => ArtifactTransferCursorState::Staged(decode_session_cursor(&mut r)?),
        2 => ArtifactTransferCursorState::AlreadyFinalized(decode_sync_report(&mut r)?),
        3 => ArtifactTransferCursorState::NotStaged,
        _ => return Err("unknown cursor state tag"),
    };
    r.finish()?;
    Ok(Ok(state))
}

fn encode_finalized_resp(report: &CrossStoreArtifactSyncReport) -> Vec<u8> {
    let mut body = Vec::new();
    encode_sync_report(&mut body, report);
    wrap_rpc_envelope(TAG_RESP_FINALIZED_REPORT, &body)
}

fn decode_finalized_resp(
    raw: &[u8],
) -> Result<Result<CrossStoreArtifactSyncReport, ArtifactNetTransferRejection>, &'static str> {
    let (tag, body) = unwrap_rpc_envelope(raw)?;
    let mut r = WireReader::new(body);
    if tag == TAG_RESP_REJECTED {
        let rej = decode_rejection(&mut r)?;
        r.finish()?;
        return Ok(Err(rej));
    }
    if tag != TAG_RESP_FINALIZED_REPORT {
        return Err("unexpected NLOSAT01 response tag for finalized report");
    }
    let report = decode_sync_report(&mut r)?;
    r.finish()?;
    Ok(Ok(report))
}

fn encode_rejected_resp(rej: &ArtifactNetTransferRejection) -> Vec<u8> {
    let mut body = Vec::new();
    match rej {
        ArtifactNetTransferRejection::MalformedWireFrame(s) => {
            body.push(1);
            encode_str(&mut body, s);
        }
        ArtifactNetTransferRejection::PeerCellIdentityMismatch {
            declared,
            authenticated_peer,
        } => {
            body.push(2);
            body.extend_from_slice(declared.as_bytes());
            body.extend_from_slice(authenticated_peer.as_bytes());
        }
        ArtifactNetTransferRejection::InvalidChunkPayloadBytes(sz) => {
            body.push(3);
            body.extend_from_slice(&sz.to_be_bytes());
        }
        ArtifactNetTransferRejection::PackageDigestMismatch { declared, computed } => {
            body.push(4);
            body.extend_from_slice(declared);
            body.extend_from_slice(computed);
        }
        ArtifactNetTransferRejection::PolicyOrSyncRejected(s) => {
            body.push(5);
            encode_str(&mut body, s);
        }
        ArtifactNetTransferRejection::SessionNotStaged(d) => {
            body.push(6);
            body.extend_from_slice(d);
        }
        ArtifactNetTransferRejection::IncompleteStagedBlobs {
            package_manifest_digest,
            incomplete_blobs,
        } => {
            body.push(7);
            body.extend_from_slice(package_manifest_digest);
            body.extend_from_slice(&incomplete_blobs.to_be_bytes());
        }
        ArtifactNetTransferRejection::StagedBlobDigestMismatch { declared, computed } => {
            body.push(8);
            body.extend_from_slice(declared.as_bytes());
            body.extend_from_slice(computed.as_bytes());
        }
        ArtifactNetTransferRejection::StagingIoError(s) => {
            body.push(9);
            encode_str(&mut body, s);
        }
    }
    wrap_rpc_envelope(TAG_RESP_REJECTED, &body)
}

fn decode_rejection(r: &mut WireReader<'_>) -> Result<ArtifactNetTransferRejection, &'static str> {
    match r.read_u8()? {
        1 => Ok(ArtifactNetTransferRejection::MalformedWireFrame(
            r.read_str()?,
        )),
        2 => Ok(ArtifactNetTransferRejection::PeerCellIdentityMismatch {
            declared: CellIdentity::from_domain(SchedulerDomainId::from_bytes(r.read_arr::<16>()?)),
            authenticated_peer: CellIdentity::from_domain(SchedulerDomainId::from_bytes(
                r.read_arr::<16>()?,
            )),
        }),
        3 => Ok(ArtifactNetTransferRejection::InvalidChunkPayloadBytes(
            r.read_u32()?,
        )),
        4 => Ok(ArtifactNetTransferRejection::PackageDigestMismatch {
            declared: r.read_arr::<32>()?,
            computed: r.read_arr::<32>()?,
        }),
        5 => Ok(ArtifactNetTransferRejection::PolicyOrSyncRejected(
            r.read_str()?,
        )),
        6 => Ok(ArtifactNetTransferRejection::SessionNotStaged(
            r.read_arr::<32>()?,
        )),
        7 => Ok(ArtifactNetTransferRejection::IncompleteStagedBlobs {
            package_manifest_digest: r.read_arr::<32>()?,
            incomplete_blobs: r.read_u32()?,
        }),
        8 => Ok(ArtifactNetTransferRejection::StagedBlobDigestMismatch {
            declared: ContentDigest::from_bytes(r.read_arr::<32>()?),
            computed: ContentDigest::from_bytes(r.read_arr::<32>()?),
        }),
        9 => Ok(ArtifactNetTransferRejection::StagingIoError(r.read_str()?)),
        _ => Err("unknown ArtifactNetTransferRejection tag"),
    }
}

fn encode_manifest(out: &mut Vec<u8>, m: &ArtifactTransferManifest) {
    out.extend_from_slice(m.artifact_id.as_bytes());
    encode_str(out, &m.content_type);
    match m.application_id {
        Some(app) => {
            out.push(1);
            out.extend_from_slice(app.as_bytes());
        }
        None => out.push(0),
    }
    match &m.owner {
        Some(owner) => {
            out.push(1);
            encode_str(out, owner);
        }
        None => out.push(0),
    }
    out.extend_from_slice(&m.artifact_created_at_ms.to_be_bytes());
    out.extend_from_slice(&m.origin_cell_id);
    out.extend_from_slice(&m.producer_epoch.to_be_bytes());
    encode_head(out, m.remote_head);
    encode_opt_head(out, m.parent_head);
    encode_u32_len(out, m.ancestor_lineage.len());
    for anc in &m.ancestor_lineage {
        encode_head(out, *anc);
    }
    out.push(u8::from(m.is_tombstone));
    match m.tombstone {
        Some(ts) => {
            out.push(1);
            out.extend_from_slice(&ts.tombstone_revision.to_be_bytes());
            out.extend_from_slice(&ts.deleted_at_ms.to_be_bytes());
            encode_opt_u64(out, ts.retention_until_ms);
        }
        None => out.push(0),
    }
    out.extend_from_slice(&m.tip_size_bytes.to_be_bytes());
    encode_u32_len(out, m.revisions.len());
    for rev in &m.revisions {
        out.extend_from_slice(&rev.revision.to_be_bytes());
        out.extend_from_slice(rev.digest.as_bytes());
        out.extend_from_slice(&rev.size_bytes.to_be_bytes());
        out.extend_from_slice(&rev.created_at_ms.to_be_bytes());
    }
    encode_u32_len(out, m.blobs.len());
    for blob in &m.blobs {
        out.extend_from_slice(blob.blob_digest.as_bytes());
        out.extend_from_slice(&blob.size_bytes.to_be_bytes());
    }
    out.extend_from_slice(&m.package_manifest_digest);
}

fn decode_manifest(r: &mut WireReader<'_>) -> Result<ArtifactTransferManifest, &'static str> {
    let artifact_id = ArtifactId::from_bytes(r.read_arr::<16>()?);
    let content_type = r.read_str()?;
    let application_id = match r.read_u8()? {
        0 => None,
        1 => Some(ApplicationId::from_bytes(r.read_arr::<16>()?)),
        _ => return Err("invalid application_id tag"),
    };
    let owner = match r.read_u8()? {
        0 => None,
        1 => Some(r.read_str()?),
        _ => return Err("invalid owner tag"),
    };
    let artifact_created_at_ms = r.read_u64()?;
    let origin_cell_id = r.read_arr::<16>()?;
    let producer_epoch = r.read_u64()?;
    let remote_head = decode_head(r)?;
    let parent_head = decode_opt_head(r)?;
    let anc_len = r.read_len(4096)?;
    let mut ancestor_lineage = Vec::with_capacity(anc_len);
    for _ in 0..anc_len {
        ancestor_lineage.push(decode_head(r)?);
    }
    let is_tombstone = match r.read_u8()? {
        0 => false,
        1 => true,
        _ => return Err("invalid is_tombstone flag"),
    };
    let tombstone = match r.read_u8()? {
        0 => None,
        1 => Some(ArtifactTombstoneMetadata {
            tombstone_revision: r.read_u64()?,
            deleted_at_ms: r.read_u64()?,
            retention_until_ms: decode_opt_u64(r)?,
        }),
        _ => return Err("invalid tombstone tag"),
    };
    let tip_size_bytes = r.read_u64()?;
    let rev_len = r.read_len(4096)?;
    let mut revisions = Vec::with_capacity(rev_len);
    for _ in 0..rev_len {
        revisions.push(TransferRevisionEntry {
            revision: r.read_u64()?,
            digest: ContentDigest::from_bytes(r.read_arr::<32>()?),
            size_bytes: r.read_u64()?,
            created_at_ms: r.read_u64()?,
        });
    }
    let blob_len = r.read_len(4096)?;
    let mut blobs = Vec::with_capacity(blob_len);
    for _ in 0..blob_len {
        blobs.push(ArtifactBlobDescriptor {
            blob_digest: ContentDigest::from_bytes(r.read_arr::<32>()?),
            size_bytes: r.read_u64()?,
        });
    }
    let package_manifest_digest = r.read_arr::<32>()?;
    Ok(ArtifactTransferManifest {
        artifact_id,
        content_type,
        application_id,
        owner,
        artifact_created_at_ms,
        origin_cell_id,
        producer_epoch,
        remote_head,
        parent_head,
        ancestor_lineage,
        is_tombstone,
        tombstone,
        tip_size_bytes,
        revisions,
        blobs,
        package_manifest_digest,
    })
}

fn encode_session_cursor(out: &mut Vec<u8>, c: &ArtifactTransferSessionCursor) {
    out.extend_from_slice(&c.package_manifest_digest);
    out.extend_from_slice(&c.chunk_payload_bytes.to_be_bytes());
    encode_u32_len(out, c.blob_cursors.len());
    for bc in &c.blob_cursors {
        out.extend_from_slice(bc.blob_digest.as_bytes());
        out.extend_from_slice(&bc.total_bytes.to_be_bytes());
        out.extend_from_slice(&bc.verified_chunks.to_be_bytes());
        out.extend_from_slice(&bc.next_offset.to_be_bytes());
    }
}

fn decode_session_cursor(
    r: &mut WireReader<'_>,
) -> Result<ArtifactTransferSessionCursor, &'static str> {
    let package_manifest_digest = r.read_arr::<32>()?;
    let chunk_payload_bytes = r.read_u32()?;
    let count = r.read_len(4096)?;
    let mut blob_cursors = Vec::with_capacity(count);
    for _ in 0..count {
        blob_cursors.push(BlobChunkCursor {
            blob_digest: ContentDigest::from_bytes(r.read_arr::<32>()?),
            total_bytes: r.read_u64()?,
            verified_chunks: r.read_u32()?,
            next_offset: r.read_u64()?,
        });
    }
    Ok(ArtifactTransferSessionCursor {
        package_manifest_digest,
        chunk_payload_bytes,
        blob_cursors,
    })
}

fn encode_sync_report(out: &mut Vec<u8>, report: &CrossStoreArtifactSyncReport) {
    match report {
        CrossStoreArtifactSyncReport::ImportedFastForward {
            artifact_id,
            prior_local_head,
            new_head,
            imported_revisions,
            materialized_blob_digests,
            publication_receipts,
        } => {
            out.push(1);
            out.extend_from_slice(artifact_id.as_bytes());
            encode_opt_head(out, *prior_local_head);
            encode_head(out, *new_head);
            encode_u32_len(out, imported_revisions.len());
            for rev in imported_revisions {
                out.extend_from_slice(&rev.to_be_bytes());
            }
            encode_u32_len(out, materialized_blob_digests.len());
            for d in materialized_blob_digests {
                out.extend_from_slice(d.as_bytes());
            }
            encode_u32_len(out, publication_receipts.len());
            for rcpt in publication_receipts {
                encode_pub_receipt(out, rcpt);
            }
        }
        CrossStoreArtifactSyncReport::NoOpConverged { artifact_id, head } => {
            out.push(2);
            out.extend_from_slice(artifact_id.as_bytes());
            encode_head(out, *head);
        }
        CrossStoreArtifactSyncReport::ConflictIsolated {
            conflict_receipt,
            preserved_both_blobs,
            pinned_digests,
        } => {
            out.push(3);
            encode_conflict_receipt(out, conflict_receipt);
            out.push(u8::from(*preserved_both_blobs));
            encode_u32_len(out, pinned_digests.len());
            for d in pinned_digests {
                out.extend_from_slice(d.as_bytes());
            }
        }
        CrossStoreArtifactSyncReport::TombstoneApplied {
            artifact_id,
            prior_local_head,
            tombstone_head,
            retention_until_ms,
            publication_receipts,
        } => {
            out.push(4);
            out.extend_from_slice(artifact_id.as_bytes());
            encode_opt_head(out, *prior_local_head);
            encode_head(out, *tombstone_head);
            encode_opt_u64(out, *retention_until_ms);
            encode_u32_len(out, publication_receipts.len());
            for rcpt in publication_receipts {
                encode_pub_receipt(out, rcpt);
            }
        }
    }
}

fn decode_sync_report(
    r: &mut WireReader<'_>,
) -> Result<CrossStoreArtifactSyncReport, &'static str> {
    match r.read_u8()? {
        1 => {
            let artifact_id = ArtifactId::from_bytes(r.read_arr::<16>()?);
            let prior_local_head = decode_opt_head(r)?;
            let new_head = decode_head(r)?;
            let rev_count = r.read_len(4096)?;
            let mut imported_revisions = Vec::with_capacity(rev_count);
            for _ in 0..rev_count {
                imported_revisions.push(r.read_u64()?);
            }
            let dig_count = r.read_len(4096)?;
            let mut materialized_blob_digests = Vec::with_capacity(dig_count);
            for _ in 0..dig_count {
                materialized_blob_digests.push(ContentDigest::from_bytes(r.read_arr::<32>()?));
            }
            let rcpt_count = r.read_len(4096)?;
            let mut publication_receipts = Vec::with_capacity(rcpt_count);
            for _ in 0..rcpt_count {
                publication_receipts.push(decode_pub_receipt(r)?);
            }
            Ok(CrossStoreArtifactSyncReport::ImportedFastForward {
                artifact_id,
                prior_local_head,
                new_head,
                imported_revisions,
                materialized_blob_digests,
                publication_receipts,
            })
        }
        2 => Ok(CrossStoreArtifactSyncReport::NoOpConverged {
            artifact_id: ArtifactId::from_bytes(r.read_arr::<16>()?),
            head: decode_head(r)?,
        }),
        3 => {
            let conflict_receipt = decode_conflict_receipt(r)?;
            let preserved_both_blobs = match r.read_u8()? {
                0 => false,
                1 => true,
                _ => return Err("invalid preserved_both_blobs flag"),
            };
            let dig_count = r.read_len(4096)?;
            let mut pinned_digests = Vec::with_capacity(dig_count);
            for _ in 0..dig_count {
                pinned_digests.push(ContentDigest::from_bytes(r.read_arr::<32>()?));
            }
            Ok(CrossStoreArtifactSyncReport::ConflictIsolated {
                conflict_receipt,
                preserved_both_blobs,
                pinned_digests,
            })
        }
        4 => {
            let artifact_id = ArtifactId::from_bytes(r.read_arr::<16>()?);
            let prior_local_head = decode_opt_head(r)?;
            let tombstone_head = decode_head(r)?;
            let retention_until_ms = decode_opt_u64(r)?;
            let rcpt_count = r.read_len(4096)?;
            let mut publication_receipts = Vec::with_capacity(rcpt_count);
            for _ in 0..rcpt_count {
                publication_receipts.push(decode_pub_receipt(r)?);
            }
            Ok(CrossStoreArtifactSyncReport::TombstoneApplied {
                artifact_id,
                prior_local_head,
                tombstone_head,
                retention_until_ms,
                publication_receipts,
            })
        }
        _ => Err("unknown CrossStoreArtifactSyncReport tag"),
    }
}

fn encode_conflict_receipt(out: &mut Vec<u8>, c: &ArtifactConflictReceipt) {
    out.extend_from_slice(c.artifact_id.as_bytes());
    encode_str(out, &c.content_type);
    encode_head(out, c.local_head);
    encode_head(out, c.remote_head);
    out.extend_from_slice(&c.origin_cell_id);
    out.push(match c.strategy {
        ArtifactConflictStrategy::FailClosedConflict => 1,
        ArtifactConflictStrategy::QuarantineDivergentBranch => 2,
        ArtifactConflictStrategy::DeferToAuthoritativeReplica => 3,
    });
    out.extend_from_slice(c.conflict_digest.as_bytes());
}

fn decode_conflict_receipt(
    r: &mut WireReader<'_>,
) -> Result<ArtifactConflictReceipt, &'static str> {
    let artifact_id = ArtifactId::from_bytes(r.read_arr::<16>()?);
    let content_type = r.read_str()?;
    let local_head = decode_head(r)?;
    let remote_head = decode_head(r)?;
    let origin_cell_id = r.read_arr::<16>()?;
    let strategy = match r.read_u8()? {
        1 => ArtifactConflictStrategy::FailClosedConflict,
        2 => ArtifactConflictStrategy::QuarantineDivergentBranch,
        3 => ArtifactConflictStrategy::DeferToAuthoritativeReplica,
        _ => return Err("unknown ArtifactConflictStrategy tag"),
    };
    let conflict_digest = ContentDigest::from_bytes(r.read_arr::<32>()?);
    Ok(ArtifactConflictReceipt {
        artifact_id,
        content_type,
        local_head,
        remote_head,
        origin_cell_id,
        strategy,
        conflict_digest,
    })
}

fn encode_pub_receipt(out: &mut Vec<u8>, rcpt: &ArtifactPublicationReceipt) {
    out.extend_from_slice(rcpt.receipt_id.as_bytes());
    out.extend_from_slice(rcpt.staging_id.as_bytes());
    out.extend_from_slice(rcpt.artifact_id.as_bytes());
    out.extend_from_slice(&rcpt.revision.to_be_bytes());
    out.extend_from_slice(rcpt.digest.as_bytes());
    out.extend_from_slice(&rcpt.size_bytes.to_be_bytes());
    out.extend_from_slice(rcpt.task_id.as_bytes());
    out.extend_from_slice(rcpt.permit_id.as_bytes());
    out.extend_from_slice(rcpt.write_set_root.as_bytes());
    out.extend_from_slice(&rcpt.prior_head_revision.to_be_bytes());
    match rcpt.prior_head_digest {
        Some(d) => {
            out.push(1);
            out.extend_from_slice(d.as_bytes());
        }
        None => out.push(0),
    }
    out.extend_from_slice(&rcpt.new_head_revision.to_be_bytes());
    out.extend_from_slice(rcpt.new_head_digest.as_bytes());
    out.extend_from_slice(&rcpt.created_at_ms.to_be_bytes());
}

fn decode_pub_receipt(r: &mut WireReader<'_>) -> Result<ArtifactPublicationReceipt, &'static str> {
    let receipt_id = ReceiptId::from_bytes(r.read_arr::<16>()?);
    let staging_id = StagingId::from_bytes(r.read_arr::<16>()?);
    let artifact_id = ArtifactId::from_bytes(r.read_arr::<16>()?);
    let revision = r.read_u64()?;
    let digest = ContentDigest::from_bytes(r.read_arr::<32>()?);
    let size_bytes = r.read_u64()?;
    let task_id = TaskId::from_bytes(r.read_arr::<16>()?);
    let permit_id = CommitPermitId::from_bytes(r.read_arr::<16>()?);
    let write_set_root = ContentDigest::from_bytes(r.read_arr::<32>()?);
    let prior_head_revision = r.read_u64()?;
    let prior_head_digest = match r.read_u8()? {
        0 => None,
        1 => Some(ContentDigest::from_bytes(r.read_arr::<32>()?)),
        _ => return Err("invalid prior_head_digest tag"),
    };
    let new_head_revision = r.read_u64()?;
    let new_head_digest = ContentDigest::from_bytes(r.read_arr::<32>()?);
    let created_at_ms = r.read_u64()?;
    Ok(ArtifactPublicationReceipt {
        receipt_id,
        staging_id,
        artifact_id,
        revision,
        digest,
        size_bytes,
        task_id,
        permit_id,
        write_set_root,
        prior_head_revision,
        prior_head_digest,
        new_head_revision,
        new_head_digest,
        created_at_ms,
    })
}

fn encode_head(out: &mut Vec<u8>, h: HeadState) {
    out.extend_from_slice(&h.revision.to_be_bytes());
    out.extend_from_slice(h.digest.as_bytes());
}

fn decode_head(r: &mut WireReader<'_>) -> Result<HeadState, &'static str> {
    Ok(HeadState {
        revision: r.read_u64()?,
        digest: ContentDigest::from_bytes(r.read_arr::<32>()?),
    })
}

fn encode_opt_head(out: &mut Vec<u8>, h: Option<HeadState>) {
    match h {
        Some(head) => {
            out.push(1);
            encode_head(out, head);
        }
        None => out.push(0),
    }
}

fn decode_opt_head(r: &mut WireReader<'_>) -> Result<Option<HeadState>, &'static str> {
    match r.read_u8()? {
        0 => Ok(None),
        1 => Ok(Some(decode_head(r)?)),
        _ => Err("invalid optional HeadState tag"),
    }
}

fn encode_opt_u64(out: &mut Vec<u8>, val: Option<u64>) {
    match val {
        Some(v) => {
            out.push(1);
            out.extend_from_slice(&v.to_be_bytes());
        }
        None => out.push(0),
    }
}

fn decode_opt_u64(r: &mut WireReader<'_>) -> Result<Option<u64>, &'static str> {
    match r.read_u8()? {
        0 => Ok(None),
        1 => Ok(Some(r.read_u64()?)),
        _ => Err("invalid optional u64 tag"),
    }
}

fn encode_u32_len(out: &mut Vec<u8>, len: usize) {
    let val = u32::try_from(len).unwrap_or(u32::MAX);
    out.extend_from_slice(&val.to_be_bytes());
}

fn encode_str(out: &mut Vec<u8>, s: &str) {
    encode_u32_len(out, s.len());
    out.extend_from_slice(s.as_bytes());
}

struct WireReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> WireReader<'a> {
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn read_slice(&mut self, len: usize) -> Result<&'a [u8], &'static str> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or("NLOSAT01 wire reader overflow")?;
        if end > self.buf.len() {
            return Err("NLOSAT01 wire frame truncated");
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn read_arr<const N: usize>(&mut self) -> Result<[u8; N], &'static str> {
        let s = self.read_slice(N)?;
        let mut arr = [0_u8; N];
        arr.copy_from_slice(s);
        Ok(arr)
    }

    fn read_u8(&mut self) -> Result<u8, &'static str> {
        Ok(self.read_arr::<1>()?[0])
    }

    fn read_u32(&mut self) -> Result<u32, &'static str> {
        Ok(u32::from_be_bytes(self.read_arr::<4>()?))
    }

    fn read_u64(&mut self) -> Result<u64, &'static str> {
        Ok(u64::from_be_bytes(self.read_arr::<8>()?))
    }

    fn read_len(&mut self, max: usize) -> Result<usize, &'static str> {
        let count = usize::try_from(self.read_u32()?).map_err(|_| "NLOSAT01 length overflow")?;
        if count > max {
            return Err("NLOSAT01 collection count exceeds bound");
        }
        Ok(count)
    }

    fn read_str(&mut self) -> Result<String, &'static str> {
        let len = self.read_len(1024)?;
        let bytes = self.read_slice(len)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| "NLOSAT01 invalid utf8 string")
    }

    const fn finish(&self) -> Result<(), &'static str> {
        if self.pos != self.buf.len() {
            return Err("NLOSAT01 trailing bytes in frame");
        }
        Ok(())
    }
}
