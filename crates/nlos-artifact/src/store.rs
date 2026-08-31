//! `ArtifactStore`: open/durability gating, schema migrations, artifact creation,
//! and the two-phase revision commit.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};

use crate::ArtifactError;
use crate::blob::{self, DomainPaths};
use crate::model::{
    ArtifactHeadEndpointProof, ArtifactRecord, ContentDigest, CreateArtifactDecision,
    CreateArtifactSpec, MAX_TEXT_COMPONENT_BYTES, PutRevisionDecision, PutRevisionRequest,
    RevisionRecord,
};
use crate::query::{load_artifact_optional, load_revision_optional};

const SCHEMA_VERSION: i64 = 5;

/// Filesystem layout under the store root.
#[derive(Clone, Debug)]
pub(crate) struct StorePaths {
    pub(crate) artifacts: DomainPaths,
    pub(crate) cache: DomainPaths,
}

/// Single-writer content-addressed artifact store.
///
/// The mutex is a process-local admission gate; `SQLite` `BEGIN IMMEDIATE`
/// remains the storage-level writer fence, and the blob commit protocol
/// (see `blob.rs`) is the storage-level durability fence for bytes.
pub struct ArtifactStore {
    connection: Mutex<Connection>,
    paths: StorePaths,
}

impl ArtifactStore {
    /// Opens or creates a store rooted at `root_path` and validates its
    /// schema and durability pragmas.
    ///
    /// Equivalent to [`ArtifactStore::open_with_vfs`] with `None`, i.e. the
    /// process-default `SQLite` VFS.
    ///
    /// Recovery is **not** run implicitly; call [`ArtifactStore::recover`]
    /// explicitly (e.g. right after open) when reconciliation is wanted.
    ///
    /// # Errors
    ///
    /// Returns an error when the directories or database cannot be created,
    /// when WAL/FULL durability cannot be established (verified by reading
    /// the pragmas back; silent fallback is rejected with
    /// [`ArtifactError::DurabilityUnavailable`]), or when the stored schema
    /// `user_version` is unknown ([`ArtifactError::SchemaVersionUnsupported`]).
    pub fn open(root_path: impl AsRef<Path>) -> Result<Self, ArtifactError> {
        Self::open_with_vfs(root_path, None)
    }

    /// Opens or creates a store through a named `SQLite` VFS (e.g. the
    /// fault-injection shim registered by tests). See
    /// [`ArtifactStore::open`] for the full contract.
    ///
    /// # Errors
    ///
    /// Same as [`ArtifactStore::open`], plus an error when the named VFS
    /// does not exist.
    pub fn open_with_vfs(
        root_path: impl AsRef<Path>,
        vfs: Option<&str>,
    ) -> Result<Self, ArtifactError> {
        let root = root_path.as_ref();
        let paths = StorePaths {
            artifacts: DomainPaths {
                blobs: root.join("artifacts").join("blobs"),
                tmp: root.join("artifacts").join("tmp"),
            },
            cache: DomainPaths {
                blobs: root.join("cache").join("blobs"),
                tmp: root.join("cache").join("tmp"),
            },
        };
        // tmp and blobs of one domain are siblings under the same root, so
        // the rename-based commit protocol stays on one device; a root that
        // violates this surfaces as `CrossDeviceRename` on the first put.
        for directory in [
            &paths.artifacts.blobs,
            &paths.artifacts.tmp,
            &paths.cache.blobs,
            &paths.cache.tmp,
        ] {
            std::fs::create_dir_all(directory).map_err(ArtifactError::Io)?;
        }

        let database = root.join("metadata.db");
        let mut connection = match vfs {
            None => Connection::open(database)?,
            Some(name) => {
                Connection::open_with_flags_and_vfs(database, OpenFlags::default(), name)?
            }
        };
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;

        // `pragma_update` discards the `journal_mode` result row, so a failed
        // WAL transition would silently fall back. Read both durability
        // pragmas back and fail closed (nlos-store authority pattern).
        let journal_mode: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let synchronous: i64 =
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 {
            return Err(ArtifactError::DurabilityUnavailable {
                journal_mode,
                synchronous,
            });
        }

        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => {
                crate::schema::migrate_v1(&mut connection)?;
                crate::schema::migrate_v2(&mut connection)?;
                crate::schema::migrate_v3(&mut connection)?;
                crate::schema::migrate_v4(&mut connection)?;
                crate::schema::migrate_v5(&mut connection)?;
            }
            1 => {
                crate::schema::migrate_v2(&mut connection)?;
                crate::schema::migrate_v3(&mut connection)?;
                crate::schema::migrate_v4(&mut connection)?;
                crate::schema::migrate_v5(&mut connection)?;
            }
            2 => {
                crate::schema::migrate_v3(&mut connection)?;
                crate::schema::migrate_v4(&mut connection)?;
                crate::schema::migrate_v5(&mut connection)?;
            }
            3 => {
                crate::schema::migrate_v4(&mut connection)?;
                crate::schema::migrate_v5(&mut connection)?;
            }
            4 => crate::schema::migrate_v5(&mut connection)?,
            SCHEMA_VERSION => {}
            other => return Err(ArtifactError::SchemaVersionUnsupported(other)),
        }

        Ok(Self {
            connection: Mutex::new(connection),
            paths,
        })
    }

    /// Creates an artifact idempotently by caller key.
    ///
    /// Repeating the exact specification under the same idempotency key
    /// returns `Existing` with the stored record (the stored
    /// `created_at_ms` wins; the replay's timestamp is ignored). Reusing the
    /// key — or the artifact identity — with a different specification is
    /// rejected with [`ArtifactError::IdempotencyConflict`].
    ///
    /// # Errors
    ///
    /// Returns a validation, conflict, or storage error.
    pub fn create_artifact(
        &self,
        spec: CreateArtifactSpec,
    ) -> Result<CreateArtifactDecision, ArtifactError> {
        validate_text_component("content_type", &spec.content_type)?;
        if let Some(owner) = &spec.owner {
            validate_text_component("owner", owner)?;
        }
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(existing) =
            crate::query::load_artifact_by_key(&transaction, spec.idempotency_key)?
        {
            let same = existing.artifact_id == spec.artifact_id
                && existing.content_type == spec.content_type
                && existing.application_id == spec.application_id
                && existing.owner == spec.owner;
            if !same {
                return Err(ArtifactError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(CreateArtifactDecision::Existing(existing));
        }
        if load_artifact_optional(&transaction, spec.artifact_id)?.is_some() {
            return Err(ArtifactError::IdempotencyConflict);
        }

        let record = ArtifactRecord {
            artifact_id: spec.artifact_id,
            content_type: spec.content_type,
            application_id: spec.application_id,
            owner: spec.owner,
            head_revision: 0,
            head_digest: None,
            created_at_ms: spec.created_at_ms,
        };
        crate::query::insert_artifact(&transaction, &record, spec.idempotency_key)?;
        crate::schema::insert_artifact_head_endpoint_proof(&transaction, record.artifact_id)?;
        transaction.commit()?;
        Ok(CreateArtifactDecision::Created(record))
    }

    /// Reads the durable authority-issued endpoint proof for an Artifact head.
    ///
    /// # Errors
    ///
    /// Returns `ArtifactNotFound`, corruption, or storage errors. Registration
    /// consumers must compare every field with this readback.
    pub fn inspect_head_endpoint_proof(
        &self,
        artifact_id: nlos_types::ArtifactId,
    ) -> Result<ArtifactHeadEndpointProof, ArtifactError> {
        let connection = self.lock_connection()?;
        crate::schema::load_artifact_head_endpoint_proof(&connection, artifact_id)
    }

    /// Appends one immutable revision and advances the head, crash-safely.
    ///
    /// Phase 1 commits the blob bytes durably under their digest; phase 2
    /// inserts the immutable revision row and compare-and-swaps the head in
    /// one `BEGIN IMMEDIATE` transaction. The new revision number is derived
    /// as `expected_head_revision + 1` (authority-issued, deterministic).
    ///
    /// Decision order inside the transaction: an exact re-put (same derived
    /// revision, same digest) replays as [`PutRevisionDecision::Replayed`];
    /// a stale or future head expectation fails with
    /// [`ArtifactError::HeadConflict`]; an occupied slot under a matching
    /// head fails closed with [`ArtifactError::RevisionConflict`].
    ///
    /// # Errors
    ///
    /// Returns a blob-commit error (no metadata is committed in that case),
    /// a typed conflict, or a storage error.
    pub fn put_revision(
        &self,
        request: PutRevisionRequest<'_>,
    ) -> Result<PutRevisionDecision, ArtifactError> {
        let digest = ContentDigest::of_bytes(request.bytes);
        // Phase 1: durable blob BEFORE any metadata referencing it exists.
        blob::commit_blob(&self.paths.artifacts, digest, request.bytes)?;

        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let artifact = load_artifact_optional(&transaction, request.artifact_id)?
            .ok_or(ArtifactError::ArtifactNotFound(request.artifact_id))?;
        let head = artifact.head_revision;
        let target =
            request
                .expected_head_revision
                .checked_add(1)
                .ok_or(ArtifactError::HeadConflict {
                    expected: request.expected_head_revision,
                    current: head,
                })?;

        let slot = load_revision_optional(&transaction, request.artifact_id, target)?;
        if let Some(slot) = &slot
            && slot.digest == digest
        {
            transaction.commit()?;
            return Ok(PutRevisionDecision::Replayed(slot.clone()));
        }
        if head != request.expected_head_revision {
            return Err(ArtifactError::HeadConflict {
                expected: request.expected_head_revision,
                current: head,
            });
        }
        if slot.is_some() {
            return Err(ArtifactError::RevisionConflict {
                artifact_id: request.artifact_id,
                revision: target,
            });
        }

        let record = RevisionRecord {
            artifact_id: request.artifact_id,
            revision: target,
            digest,
            size_bytes: u64::try_from(request.bytes.len())
                .map_err(|_| ArtifactError::InvalidSpec("blob length exceeds u64"))?,
            created_at_ms: request.created_at_ms,
        };
        transaction.execute(
            "INSERT INTO artifact_revisions (
                artifact_id, revision, digest, size_bytes, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.artifact_id.as_bytes().as_slice(),
                encode_u64(record.revision)?,
                record.digest.as_bytes().as_slice(),
                encode_u64(record.size_bytes)?,
                encode_u64(record.created_at_ms)?,
            ],
        )?;
        let changed = transaction.execute(
            "UPDATE artifacts SET head_revision = ?1, head_digest = ?2
             WHERE artifact_id = ?3 AND head_revision = ?4",
            params![
                encode_u64(target)?,
                digest.as_bytes().as_slice(),
                request.artifact_id.as_bytes().as_slice(),
                encode_u64(request.expected_head_revision)?,
            ],
        )?;
        if changed != 1 {
            return Err(ArtifactError::CorruptRecord(
                "head compare-and-swap failed under BEGIN IMMEDIATE",
            ));
        }
        transaction.commit()?;
        Ok(PutRevisionDecision::Committed(record))
    }

    pub(crate) fn lock_connection(&self) -> Result<MutexGuard<'_, Connection>, ArtifactError> {
        self.connection
            .lock()
            .map_err(|_| ArtifactError::LockPoisoned)
    }

    pub(crate) fn paths(&self) -> &StorePaths {
        &self.paths
    }
}

/// Validates a caller-supplied bounded string (content type, owner, cache
/// key): non-empty, bounded, no NUL.
pub(crate) fn validate_text_component(
    field: &'static str,
    value: &str,
) -> Result<(), ArtifactError> {
    let valid =
        !value.is_empty() && value.len() <= MAX_TEXT_COMPONENT_BYTES && !value.contains('\0');
    if valid {
        Ok(())
    } else {
        Err(ArtifactError::InvalidSpec(field))
    }
}

pub(crate) fn encode_u64(value: u64) -> Result<i64, ArtifactError> {
    i64::try_from(value).map_err(|_| ArtifactError::InvalidSpec("value exceeds SQLite INTEGER"))
}
