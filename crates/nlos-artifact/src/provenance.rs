//! Per-revision provenance minimal prefix (B-ARTIFACT-006): one immutable
//! receipt row per committed revision binding a **source triple** that is
//! either caller-asserted opaque (direct [`crate::ArtifactStore::put_revision`])
//! or owner-derived (staged [`crate::ArtifactStore::publish_staged_revision`]).
//!
//! # Semantics
//!
//! The authority stores the triple durably but, for caller-asserted rows,
//! does not verify its semantics — matching the stage-B placeholder
//! discipline of resource effect-closed proof digests. Owner-derived rows
//! bind to the immutable publication receipt and copy its task/permit/
//! write-set triple exactly; readback cross-checks that binding.
//!
//! # Fail-closed read gate
//!
//! [`crate::ArtifactStore::get_revision`] refuses bytes when no provenance
//! receipt exists for the requested revision
//! ([`crate::ArtifactError::ProvenanceIncomplete`]). The metadata/audit
//! plane (`inspect_revision`, `list_revisions`, `inspect_provenance`,
//! `recover`, package verification) stays un-gated so operators can see
//! revisions that lack provenance and decide how to repair them.
//!
//! # Scope
//!
//! No lineage chains, no attestation verification, no `TaskWriteSet`
//! consumer wiring, and no backfill of pre-v7 revisions — those remain
//! unreadable until a dedicated repair slice (if ever) records provenance.

use nlos_types::{ArtifactId, CommitPermitId, ReceiptId, TaskId};
use rusqlite::{Row, Transaction, params};
use sha2::{Digest, Sha256};

use crate::ArtifactError;
use crate::model::{
    ArtifactPublicationReceipt, ArtifactProvenanceReceipt, ProvenanceSourceKind,
    ProvenanceSourceTriple,
};
use crate::query::SqlRead;
use crate::store::{ArtifactStore, encode_u64};

impl ArtifactStore {
    /// Reads the immutable provenance receipt of one revision. Never gated
    /// by retention or provenance completeness (audit plane).
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactError::RevisionNotFound`] when the artifact or
    /// revision row is absent, [`ArtifactError::ProvenanceIncomplete`] when
    /// the revision exists but has no receipt, or a storage/corruption error.
    pub fn inspect_provenance(
        &self,
        artifact_id: ArtifactId,
        revision: u64,
    ) -> Result<ArtifactProvenanceReceipt, ArtifactError> {
        let connection = self.lock_connection()?;
        crate::query::load_artifact_optional(&*connection, artifact_id)?
            .ok_or(ArtifactError::ArtifactNotFound(artifact_id))?;
        crate::query::load_revision_optional(&*connection, artifact_id, revision)?.ok_or(
            ArtifactError::RevisionNotFound {
                artifact_id,
                revision,
            },
        )?;
        load_provenance_optional(&*connection, artifact_id, revision)?
            .ok_or(ArtifactError::ProvenanceIncomplete {
                artifact_id,
                revision,
            })
    }

    /// Reads one immutable provenance receipt by identity.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactError::ProvenanceReceiptNotFound`] or a storage
    /// error.
    pub fn inspect_provenance_receipt(
        &self,
        receipt_id: ReceiptId,
    ) -> Result<ArtifactProvenanceReceipt, ArtifactError> {
        let connection = self.lock_connection()?;
        load_provenance_by_receipt(&*connection, receipt_id)?
            .ok_or(ArtifactError::ProvenanceReceiptNotFound(receipt_id))
    }
}

/// Fail-closed gate for byte reads: a committed revision without a durable
/// provenance receipt is treated as incomplete provenance.
pub(crate) fn ensure_provenance_recorded(
    source: &impl SqlRead,
    artifact_id: ArtifactId,
    revision: u64,
) -> Result<(), ArtifactError> {
    if load_provenance_optional(source, artifact_id, revision)?.is_some() {
        Ok(())
    } else {
        Err(ArtifactError::ProvenanceIncomplete {
            artifact_id,
            revision,
        })
    }
}

pub(crate) fn insert_caller_asserted_provenance(
    transaction: &Transaction<'_>,
    artifact_id: ArtifactId,
    revision: u64,
    triple: ProvenanceSourceTriple,
    created_at_ms: u64,
) -> Result<ArtifactProvenanceReceipt, ArtifactError> {
    let receipt = ArtifactProvenanceReceipt {
        receipt_id: derive_provenance_receipt_id(artifact_id, revision),
        artifact_id,
        revision,
        source_kind: ProvenanceSourceKind::CallerAssertedOpaque,
        source_triple: triple,
        publication_receipt_id: None,
        created_at_ms,
    };
    insert_provenance_receipt(transaction, &receipt)?;
    Ok(receipt)
}

pub(crate) fn insert_owner_derived_provenance(
    transaction: &Transaction<'_>,
    publication: &ArtifactPublicationReceipt,
) -> Result<ArtifactProvenanceReceipt, ArtifactError> {
    let receipt = ArtifactProvenanceReceipt {
        receipt_id: derive_provenance_receipt_id(publication.artifact_id, publication.revision),
        artifact_id: publication.artifact_id,
        revision: publication.revision,
        source_kind: ProvenanceSourceKind::OwnerDerived,
        source_triple: ProvenanceSourceTriple {
            source_a: publication.task_id.into_bytes(),
            source_b: publication.permit_id.into_bytes(),
            source_digest: publication.write_set_root,
        },
        publication_receipt_id: Some(publication.receipt_id),
        created_at_ms: publication.created_at_ms,
    };
    insert_provenance_receipt(transaction, &receipt)?;
    Ok(receipt)
}

pub(crate) fn derive_provenance_receipt_id(
    artifact_id: ArtifactId,
    revision: u64,
) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/artifact-provenance-receipt/v1");
    hasher.update(artifact_id.as_bytes());
    hasher.update(revision.to_be_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    ReceiptId::from_bytes(bytes)
}

fn insert_provenance_receipt(
    transaction: &Transaction<'_>,
    receipt: &ArtifactProvenanceReceipt,
) -> Result<(), ArtifactError> {
    let source_kind = match receipt.source_kind {
        ProvenanceSourceKind::CallerAssertedOpaque => 0_i64,
        ProvenanceSourceKind::OwnerDerived => 1_i64,
    };
    transaction.execute(
        "INSERT INTO artifact_provenance_receipts (
            receipt_id, artifact_id, revision, source_kind,
            source_a, source_b, source_digest,
            publication_receipt_id, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            receipt.receipt_id.as_bytes().as_slice(),
            receipt.artifact_id.as_bytes().as_slice(),
            encode_u64(receipt.revision)?,
            source_kind,
            receipt.source_triple.source_a.as_slice(),
            receipt.source_triple.source_b.as_slice(),
            receipt.source_triple.source_digest.as_bytes().as_slice(),
            receipt
                .publication_receipt_id
                .map(ReceiptId::into_bytes)
                .as_ref()
                .map(<[u8; 16]>::as_slice),
            encode_u64(receipt.created_at_ms)?,
        ],
    )?;
    Ok(())
}

pub(crate) fn load_provenance_optional(
    source: &impl SqlRead,
    artifact_id: ArtifactId,
    revision: u64,
) -> Result<Option<ArtifactProvenanceReceipt>, ArtifactError> {
    let mut statement = source.prepare_statement(
        "SELECT receipt_id, artifact_id, revision, source_kind,
                source_a, source_b, source_digest, publication_receipt_id,
                created_at_ms
         FROM artifact_provenance_receipts
         WHERE artifact_id = ?1 AND revision = ?2",
    )?;
    let mut rows = statement.query(params![
        artifact_id.as_bytes().as_slice(),
        encode_u64(revision)?,
    ])?;
    rows.next()?.map(|row| decode_provenance_row(source, row)).transpose()
}

fn load_provenance_by_receipt(
    source: &impl SqlRead,
    receipt_id: ReceiptId,
) -> Result<Option<ArtifactProvenanceReceipt>, ArtifactError> {
    let mut statement = source.prepare_statement(
        "SELECT receipt_id, artifact_id, revision, source_kind,
                source_a, source_b, source_digest, publication_receipt_id,
                created_at_ms
         FROM artifact_provenance_receipts WHERE receipt_id = ?1",
    )?;
    let mut rows = statement.query([receipt_id.as_bytes().as_slice()])?;
    rows.next()?.map(|row| decode_provenance_row(source, row)).transpose()
}

fn decode_provenance_row(
    source: &impl SqlRead,
    row: &Row<'_>,
) -> Result<ArtifactProvenanceReceipt, ArtifactError> {
    let source_kind_value: i64 = row.get(3)?;
    let source_kind = match source_kind_value {
        0 => ProvenanceSourceKind::CallerAssertedOpaque,
        1 => ProvenanceSourceKind::OwnerDerived,
        _ => return Err(ArtifactError::CorruptRecord("unknown provenance source kind")),
    };
    let publication_receipt_id = optional_blob16(row, 7)?.map(ReceiptId::from_bytes);
    if (source_kind == ProvenanceSourceKind::CallerAssertedOpaque)
        != publication_receipt_id.is_none()
    {
        return Err(ArtifactError::CorruptRecord(
            "provenance source kind and publication receipt disagree",
        ));
    }
    let receipt = ArtifactProvenanceReceipt {
        receipt_id: ReceiptId::from_bytes(blob16(row, 0)?),
        artifact_id: ArtifactId::from_bytes(blob16(row, 1)?),
        revision: decode_u64(row, 2)?,
        source_kind,
        source_triple: ProvenanceSourceTriple {
            source_a: blob16(row, 4)?,
            source_b: blob16(row, 5)?,
            source_digest: crate::model::ContentDigest::from_bytes(blob32(row, 6)?),
        },
        publication_receipt_id,
        created_at_ms: decode_u64(row, 8)?,
    };
    if source_kind == ProvenanceSourceKind::OwnerDerived {
        verify_owner_derived_binding(source, &receipt)?;
    }
    Ok(receipt)
}

fn verify_owner_derived_binding(
    source: &impl SqlRead,
    receipt: &ArtifactProvenanceReceipt,
) -> Result<(), ArtifactError> {
    let publication_id = receipt.publication_receipt_id.ok_or(ArtifactError::CorruptRecord(
        "owner-derived provenance lacks publication receipt id",
    ))?;
    let publication = crate::publication::load_receipt_optional(source, publication_id)?
        .ok_or(ArtifactError::CorruptRecord(
            "owner-derived provenance references missing publication receipt",
        ))?;
    if publication.artifact_id != receipt.artifact_id
        || publication.revision != receipt.revision
        || publication.receipt_id != publication_id
        || publication.task_id != TaskId::from_bytes(receipt.source_triple.source_a)
        || publication.permit_id != CommitPermitId::from_bytes(receipt.source_triple.source_b)
        || publication.write_set_root != receipt.source_triple.source_digest
    {
        return Err(ArtifactError::CorruptRecord(
            "owner-derived provenance triple disagrees with publication receipt",
        ));
    }
    Ok(())
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

fn optional_blob16(row: &Row<'_>, index: usize) -> Result<Option<[u8; 16]>, ArtifactError> {
    optional_blob_n(row, index)
}

fn blob_n<const N: usize>(row: &Row<'_>, index: usize) -> Result<[u8; N], ArtifactError> {
    let bytes: Vec<u8> = row.get(index)?;
    <[u8; N]>::try_from(bytes.as_slice())
        .map_err(|_| ArtifactError::CorruptRecord("blob column length mismatch"))
}

fn optional_blob_n<const N: usize>(
    row: &Row<'_>,
    index: usize,
) -> Result<Option<[u8; N]>, ArtifactError> {
    let bytes: Option<Vec<u8>> = row.get(index)?;
    bytes
        .map(|value| {
            <[u8; N]>::try_from(value.as_slice())
                .map_err(|_| ArtifactError::CorruptRecord("blob column length mismatch"))
        })
        .transpose()
}
