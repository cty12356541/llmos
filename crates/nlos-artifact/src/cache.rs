//! Evictable derived-cache domain (`cache/`), separated from artifact本体
//! (`artifacts/`) per `[CTX-NOTDATA-001]`: cache eviction MUST NOT delete
//! artifact blobs, and this module has no code path that writes under
//! `artifacts/`.
//!
//! The cache is best-effort: entries may be evicted at any time, a missing
//! blob degrades to a cache miss (`Ok(None)`), and `recover` drops cache
//! rows whose blob vanished. Two cache keys may address identical content;
//! eviction removes the blob file only when no other entry references the
//! digest.

use rusqlite::{TransactionBehavior, params};

use crate::ArtifactError;
use crate::blob;
use crate::model::ContentDigest;
use crate::query::SqlRead;
use crate::store::{ArtifactStore, encode_u64, validate_text_component};

impl ArtifactStore {
    /// Stores `bytes` under `cache_key` in the evictable cache domain and
    /// returns the content digest. Re-putting an existing key replaces the
    /// entry (the cache is mutable and best-effort, unlike revisions).
    ///
    /// # Errors
    ///
    /// Returns a validation, blob-commit, or storage error.
    pub fn put_cache_blob(
        &self,
        cache_key: &str,
        bytes: &[u8],
        created_at_ms: u64,
    ) -> Result<ContentDigest, ArtifactError> {
        validate_text_component("cache_key", cache_key)?;
        let digest = ContentDigest::of_bytes(bytes);
        blob::commit_blob(&self.paths().cache, digest, bytes)?;

        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO cache_entries (cache_key, digest, size_bytes, created_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(cache_key) DO UPDATE SET
                digest = excluded.digest,
                size_bytes = excluded.size_bytes,
                created_at_ms = excluded.created_at_ms",
            params![
                cache_key,
                digest.as_bytes().as_slice(),
                encode_u64(
                    u64::try_from(bytes.len())
                        .map_err(|_| ArtifactError::InvalidSpec("blob length exceeds u64"))?
                )?,
                encode_u64(created_at_ms)?,
            ],
        )?;
        transaction.commit()?;
        Ok(digest)
    }

    /// Reads a cache entry, re-verifying its digest. A missing entry or a
    /// missing blob is a cache miss (`Ok(None)`); corrupted bytes are a
    /// typed [`ArtifactError::DigestMismatch`], never a silent wrong answer.
    ///
    /// # Errors
    ///
    /// Returns a validation, digest-mismatch, or storage error.
    pub fn get_cache_blob(&self, cache_key: &str) -> Result<Option<Vec<u8>>, ArtifactError> {
        validate_text_component("cache_key", cache_key)?;
        let connection = self.lock_connection()?;
        let Some(digest) = load_cache_digest(&*connection, cache_key)? else {
            return Ok(None);
        };
        blob::read_blob_verified(&self.paths().cache, digest)
    }

    /// Evicts one cache entry. Returns whether an entry existed. The blob
    /// file is removed only when no other cache entry references the same
    /// digest; artifact blobs under `artifacts/` are never touched.
    ///
    /// # Errors
    ///
    /// Returns a validation, I/O, or storage error.
    pub fn evict_cache_blob(&self, cache_key: &str) -> Result<bool, ArtifactError> {
        validate_text_component("cache_key", cache_key)?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(digest) = load_cache_digest(&*transaction, cache_key)? else {
            return Ok(false);
        };
        transaction.execute(
            "DELETE FROM cache_entries WHERE cache_key = ?1",
            [cache_key],
        )?;
        let remaining: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM cache_entries WHERE digest = ?1",
            [digest.as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        transaction.commit()?;
        if remaining == 0 {
            blob::remove_blob(&self.paths().cache, digest)?;
        }
        Ok(true)
    }
}

pub(crate) fn load_cache_digest(
    source: &impl SqlRead,
    cache_key: &str,
) -> Result<Option<ContentDigest>, ArtifactError> {
    let mut statement =
        source.prepare_statement("SELECT digest FROM cache_entries WHERE cache_key = ?1")?;
    let mut rows = statement.query([cache_key])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let bytes: Vec<u8> = row.get(0)?;
    let digest = <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| ArtifactError::CorruptRecord("cache digest length mismatch"))?;
    Ok(Some(ContentDigest::from_bytes(digest)))
}
