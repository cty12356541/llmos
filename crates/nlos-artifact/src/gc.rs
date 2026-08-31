//! Explicit conservative garbage collection of orphan artifact blobs
//! (B-ARTIFACT minimal GC prefix).
//!
//! [`ArtifactStore::collect_orphan_blobs`] is the only code path in this
//! crate that deletes a blob under `artifacts/blobs/`. It removes a blob
//! only when it is a **provable orphan**: its digest is named by no
//! durable row —
//!
//! - no committed `artifact_revisions` row,
//! - no `artifact_staged_revisions` row in *any* state. An unreleased
//!   stage keeps its blob for a future publish, and a published stage's
//!   digest is also covered by its revision row; both states are counted,
//!   so the judgement is deliberately over-inclusive,
//! - no artifact `head_digest` (implied by the revision rows already;
//!   counted anyway as free conservatism).
//!
//! The fail-safe direction is fixed: anything not provably orphaned is
//! retained. The cache domain (`cache/blobs/`) is a separate retention
//! domain and is never scanned or touched; foreign files reported by
//! `recover` are never touched.
//!
//! # Atomicity and crash windows
//!
//! One run holds the process-local writer mutex and one
//! `BEGIN IMMEDIATE` transaction from the reference scan to the receipt
//! commit, so the committed reference set cannot change under it
//! (single-writer discipline, as for every other mutating API). Blob
//! files cannot join a `SQLite` transaction, so the order is fixed:
//!
//! 1. compute the orphan set inside the open transaction,
//! 2. remove each orphan file (`blob::remove_blob` fsyncs the shard
//!    directory, so removals are durable),
//! 3. insert the immutable [`GcReceipt`] and commit the transaction.
//!
//! A crash before step 3 leaves the removals durable with **no receipt**;
//! that state is consistent by construction — the removed digests were
//! provable orphans, `recover` no longer lists them, and the next
//! explicit run recomputes the diff from scratch (idempotent). A crash
//! after step 3 leaves a receipt whose digest list is exactly the removed
//! set. A committed receipt therefore never lies, and an absent receipt
//! never hides committed state.
//!
//! Replay follows the crate's immutable-receipt precedent (package
//! verification, ADR-0010): an existing receipt for the caller's
//! idempotency key is the durable authority and replays **without
//! re-running** the scan or any deletion.
//!
//! # Scope and honesty boundaries
//!
//! Explicit invocation only: no automatic trigger, schedule, or open-time
//! sweep. No retention/TTL policy. No cross-artifact or external
//! reference tracking — a blob referenced only from outside this store is
//! by construction an orphan *to this store*. Presence, not integrity,
//! decides candidacy; full-blob re-hashing remains an audit concern.

use std::collections::HashSet;

use nlos_types::{IdempotencyKey, ReceiptId};
use rusqlite::{Row, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::ArtifactError;
use crate::blob;
use crate::model::ContentDigest;
use crate::query::{SqlRead, load_all_revision_digests};
use crate::store::{ArtifactStore, encode_u64};

/// Request for one explicit [`ArtifactStore::collect_orphan_blobs`] run.
#[derive(Clone, Copy, Debug)]
pub struct CollectOrphanBlobsRequest {
    /// Caller-supplied exactly-once key for the GC receipt. A repeated
    /// key replays the stored receipt without re-running the collection.
    pub idempotency_key: IdempotencyKey,
    /// Caller-supplied run timestamp (milliseconds since Unix epoch).
    pub collected_at_ms: u64,
}

/// Immutable record of one GC run: the orphan digests whose blob files
/// the run removed from `artifacts/blobs/`, sorted ascending.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GcReceipt {
    pub receipt_id: ReceiptId,
    pub collected_digests: Vec<ContentDigest>,
    pub collected_count: u64,
    /// How many digest-addressed blob files existed in the artifacts
    /// domain at scan time (referenced + collected).
    pub scanned_blob_count: u64,
    pub created_at_ms: u64,
}

/// Outcome of [`ArtifactStore::collect_orphan_blobs`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CollectOrphanBlobsDecision {
    /// The run scanned, removed the listed orphans, and committed its
    /// receipt.
    Collected(GcReceipt),
    /// The same idempotency key had already completed; the durable
    /// receipt is replayed unchanged without re-running.
    Replayed(GcReceipt),
}

impl CollectOrphanBlobsDecision {
    #[must_use]
    pub const fn receipt(&self) -> &GcReceipt {
        match self {
            Self::Collected(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

impl ArtifactStore {
    /// Collects orphan artifact blobs: every blob file under
    /// `artifacts/blobs/` whose digest no committed revision, no staged
    /// revision (any state), and no head references. Removals and the
    /// receipt follow the crash-window order documented in `gc`.
    ///
    /// The caller must observe the crate's single-writer discipline: no
    /// `put_revision`/`stage_revision` may be in flight on the same store
    /// while GC runs, since such a write commits its blob (phase 1)
    /// before its metadata (phase 2) and would be indistinguishable from
    /// a crash orphan mid-run.
    ///
    /// # Errors
    ///
    /// Returns a blob-removal or storage error. A failed run may already
    /// have removed (provably orphaned) files; see the module docs for
    /// why that state stays consistent and how a retry completes it.
    pub fn collect_orphan_blobs(
        &self,
        request: CollectOrphanBlobsRequest,
    ) -> Result<CollectOrphanBlobsDecision, ArtifactError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_receipt_by_key(&transaction, request.idempotency_key)? {
            transaction.commit()?;
            return Ok(CollectOrphanBlobsDecision::Replayed(existing));
        }

        let mut referenced: HashSet<ContentDigest> = load_all_revision_digests(&transaction)?
            .into_iter()
            .map(|(_, _, digest)| digest)
            .collect();
        referenced.extend(load_all_staged_digests_any_state(&transaction)?);
        referenced.extend(load_all_head_digests(&transaction)?);

        let scan = blob::scan_blobs(&self.paths().artifacts.blobs)?;
        let scanned_blob_count = u64::try_from(scan.present.len()).unwrap_or(u64::MAX);
        let mut orphans: Vec<ContentDigest> = scan
            .present
            .into_iter()
            .filter(|digest| !referenced.contains(digest))
            .collect();
        orphans.sort();

        // Removals are durable before the receipt records them (crash
        // windows in the module docs). A digest whose file a previous
        // uncommitted run already removed surfaces as `Ok(false)` and is
        // still legitimately collected: the final state matches the
        // receipt.
        for digest in &orphans {
            blob::remove_blob(&self.paths().artifacts, *digest)?;
        }

        let receipt = GcReceipt {
            receipt_id: derive_receipt_id(request.idempotency_key, &orphans),
            collected_count: u64::try_from(orphans.len()).unwrap_or(u64::MAX),
            collected_digests: orphans,
            scanned_blob_count,
            created_at_ms: request.collected_at_ms,
        };
        insert_receipt(&transaction, &receipt, request.idempotency_key)?;
        transaction.commit()?;
        Ok(CollectOrphanBlobsDecision::Collected(receipt))
    }

    /// Reads one immutable GC receipt.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactError::GcReceiptNotFound`] or a storage error.
    pub fn inspect_gc_receipt(&self, receipt_id: ReceiptId) -> Result<GcReceipt, ArtifactError> {
        let connection = self.lock_connection()?;
        load_receipt_optional(&*connection, receipt_id)?
            .ok_or(ArtifactError::GcReceiptNotFound(receipt_id))
    }
}

/// Every staged digest regardless of release state: the deliberately
/// over-inclusive staged reference rule of this module.
fn load_all_staged_digests_any_state(
    source: &impl SqlRead,
) -> Result<Vec<ContentDigest>, ArtifactError> {
    let mut statement = source.prepare_statement("SELECT digest FROM artifact_staged_revisions")?;
    let mut rows = statement.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(ContentDigest::from_bytes(blob32(row, 0)?));
    }
    Ok(out)
}

fn load_all_head_digests(source: &impl SqlRead) -> Result<Vec<ContentDigest>, ArtifactError> {
    let mut statement = source
        .prepare_statement("SELECT head_digest FROM artifacts WHERE head_digest IS NOT NULL")?;
    let mut rows = statement.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(ContentDigest::from_bytes(blob32(row, 0)?));
    }
    Ok(out)
}

fn derive_receipt_id(key: IdempotencyKey, collected: &[ContentDigest]) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/artifact-gc-receipt/v1");
    hasher.update(key.as_bytes());
    for digest in collected {
        hasher.update(digest.as_bytes());
    }
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    ReceiptId::from_bytes(bytes)
}

fn insert_receipt(
    transaction: &rusqlite::Transaction<'_>,
    receipt: &GcReceipt,
    idempotency_key: IdempotencyKey,
) -> Result<(), ArtifactError> {
    let mut packed = Vec::with_capacity(receipt.collected_digests.len() * 32);
    for digest in &receipt.collected_digests {
        packed.extend_from_slice(digest.as_bytes());
    }
    transaction.execute(
        "INSERT INTO artifact_gc_receipts (
            receipt_id, idempotency_key, collected_digests,
            collected_count, scanned_blob_count, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            receipt.receipt_id.as_bytes().as_slice(),
            idempotency_key.as_bytes().as_slice(),
            packed,
            encode_u64(receipt.collected_count)?,
            encode_u64(receipt.scanned_blob_count)?,
            encode_u64(receipt.created_at_ms)?,
        ],
    )?;
    Ok(())
}

fn load_receipt_by_key(
    source: &impl SqlRead,
    idempotency_key: IdempotencyKey,
) -> Result<Option<GcReceipt>, ArtifactError> {
    let mut statement = source.prepare_statement(
        "SELECT receipt_id, collected_digests, collected_count,
                scanned_blob_count, created_at_ms
         FROM artifact_gc_receipts WHERE idempotency_key = ?1",
    )?;
    let mut rows = statement.query([idempotency_key.as_bytes().as_slice()])?;
    rows.next()?.map(decode_receipt_row).transpose()
}

fn load_receipt_optional(
    source: &impl SqlRead,
    receipt_id: ReceiptId,
) -> Result<Option<GcReceipt>, ArtifactError> {
    let mut statement = source.prepare_statement(
        "SELECT receipt_id, collected_digests, collected_count,
                scanned_blob_count, created_at_ms
         FROM artifact_gc_receipts WHERE receipt_id = ?1",
    )?;
    let mut rows = statement.query([receipt_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_receipt_row).transpose()
}

fn decode_receipt_row(row: &Row<'_>) -> Result<GcReceipt, ArtifactError> {
    let receipt_id = ReceiptId::from_bytes(blob16(row, 0)?);
    let packed: Vec<u8> = row.get(1)?;
    let collected_count = decode_u64(row, 2)?;
    let scanned_blob_count = decode_u64(row, 3)?;
    let created_at_ms = decode_u64(row, 4)?;
    if !packed.len().is_multiple_of(32) {
        return Err(ArtifactError::CorruptRecord("gc receipt digest packing"));
    }
    let collected_digests: Vec<ContentDigest> = packed
        .as_chunks::<32>()
        .0
        .iter()
        .map(|chunk| ContentDigest::from_bytes(*chunk))
        .collect();
    if u64::try_from(collected_digests.len()).unwrap_or(u64::MAX) != collected_count {
        return Err(ArtifactError::CorruptRecord(
            "gc receipt count disagrees with digest packing",
        ));
    }
    Ok(GcReceipt {
        receipt_id,
        collected_digests,
        collected_count,
        scanned_blob_count,
        created_at_ms,
    })
}

fn decode_u64(row: &Row<'_>, index: usize) -> Result<u64, ArtifactError> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| ArtifactError::CorruptRecord("negative u64 column"))
}

fn blob16(row: &Row<'_>, index: usize) -> Result<[u8; 16], ArtifactError> {
    blob_n(row, index)
}

fn blob32(row: &Row<'_>, index: usize) -> Result<[u8; 32], ArtifactError> {
    blob_n(row, index)
}

fn blob_n<const N: usize>(row: &Row<'_>, index: usize) -> Result<[u8; N], ArtifactError> {
    let bytes: Vec<u8> = row.get(index)?;
    <[u8; N]>::try_from(bytes.as_slice())
        .map_err(|_| ArtifactError::CorruptRecord("gc receipt blob length mismatch"))
}
