//! Recovery/reconcile between committed metadata and blob presence.
//!
//! `recover` is explicit (never run on open) and:
//!
//! - lists committed revisions whose blob is missing (`missing_blobs`;
//!   repairing them is a policy decision above this crate);
//! - removes orphan tmp files (by definition pre-rename, hence uncommitted);
//! - lists orphan blobs no committed revision or stage references
//!   (removable only by the explicit GC
//!   [`ArtifactStore::collect_orphan_blobs`]; `recover` itself never
//!   deletes an orphan blob);
//! - drops cache rows whose blob vanished (best-effort cache self-heal).

use std::collections::HashSet;

use rusqlite::TransactionBehavior;

use crate::ArtifactError;
use crate::blob;
use crate::model::{ContentDigest, MissingBlob, MissingStagedBlob, RecoveryReport};
use crate::publication::load_all_staged_digests;
use crate::query::load_all_revision_digests;
use crate::store::ArtifactStore;

impl ArtifactStore {
    /// Reconciles metadata against blob presence. See the module docs and
    /// [`RecoveryReport`] for the exact guarantees. Idempotent: a clean
    /// store recovers to an empty report.
    ///
    /// # Errors
    ///
    /// Returns an I/O or storage error; reconciliation findings themselves
    /// are data in the report, not errors.
    pub fn recover(&self) -> Result<RecoveryReport, ArtifactError> {
        let mut report = RecoveryReport::default();

        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Metadata -> blob direction: every committed revision must have
        // its blob; every cache row whose blob vanished is dropped.
        let revisions = load_all_revision_digests(&*transaction)?;
        let mut referenced: HashSet<ContentDigest> = HashSet::with_capacity(revisions.len());
        for (artifact_id, revision, digest) in revisions {
            referenced.insert(digest);
            let path = self.paths().artifacts.blob_path(digest);
            if !path.try_exists().map_err(ArtifactError::Io)? {
                report.missing_blobs.push(MissingBlob {
                    artifact_id,
                    revision,
                    digest,
                });
            }
        }

        // Staged blobs are durable authority state too. They must neither be
        // reported as orphans nor silently forgotten merely because they
        // have not advanced a canonical head yet.
        for (staging_id, artifact_id, target_revision, digest) in
            load_all_staged_digests(&*transaction)?
        {
            referenced.insert(digest);
            let path = self.paths().artifacts.blob_path(digest);
            if !path.try_exists().map_err(ArtifactError::Io)? {
                report.missing_staged_blobs.push(MissingStagedBlob {
                    staging_id,
                    artifact_id,
                    target_revision,
                    digest,
                });
            }
        }

        let cache_rows: Vec<(String, ContentDigest)> = {
            let mut statement =
                transaction.prepare("SELECT cache_key, digest FROM cache_entries")?;
            let rows = statement
                .query_map([], |row| {
                    let key: String = row.get(0)?;
                    let bytes: Vec<u8> = row.get(1)?;
                    Ok((key, bytes))
                })?
                .collect::<Result<Vec<_>, rusqlite::Error>>()?;
            rows.into_iter()
                .map(|(key, bytes)| {
                    let digest = <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
                        ArtifactError::CorruptRecord("cache digest length mismatch")
                    })?;
                    Ok((key, ContentDigest::from_bytes(digest)))
                })
                .collect::<Result<Vec<_>, ArtifactError>>()?
        };
        let mut cache_referenced: HashSet<ContentDigest> = HashSet::new();
        for (key, digest) in cache_rows {
            let path = self.paths().cache.blob_path(digest);
            if path.try_exists().map_err(ArtifactError::Io)? {
                cache_referenced.insert(digest);
            } else {
                transaction.execute("DELETE FROM cache_entries WHERE cache_key = ?1", [key])?;
                report.cache_rows_dropped += 1;
            }
        }
        transaction.commit()?;

        // Blob -> metadata direction: orphans are listed, never deleted.
        let artifact_scan = blob::scan_blobs(&self.paths().artifacts.blobs)?;
        report.orphan_blobs = artifact_scan
            .present
            .into_iter()
            .filter(|digest| !referenced.contains(digest))
            .collect();
        report.foreign_files.extend(artifact_scan.foreign);

        let cache_scan = blob::scan_blobs(&self.paths().cache.blobs)?;
        report.orphan_cache_blobs = cache_scan
            .present
            .into_iter()
            .filter(|digest| !cache_referenced.contains(digest))
            .collect();
        report.foreign_files.extend(cache_scan.foreign);

        // Tmp files are uncommitted by definition; removal is always safe.
        report.removed_tmp_files = blob::clean_tmp(&self.paths().artifacts.tmp)?
            + blob::clean_tmp(&self.paths().cache.tmp)?;
        Ok(report)
    }
}
