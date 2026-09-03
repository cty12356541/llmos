//! Single-writer `SQLite` implementation of the durable task authority.
//!
//! The process-local mutex is an admission gate only; `BEGIN IMMEDIATE`
//! remains the storage-level writer fence, identical to `nlos-store`. Every
//! linearized decision (permit CAS, cancellation, finalize) commits its
//! state transition, epoch advance, and receipt in one transaction, so a
//! crash cannot split a decision from its durable record.

use std::fmt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_operation::OperationHandle;
use nlos_types::{
    ArtifactId, CancellationScopeId, ChannelId, CommitPermitId, Generation, IdempotencyKey,
    OperationId, ProcessId, ReceiptId, TaskAttemptId, TaskAuthorityAssignmentId, TaskId,
    TaskSnapshotId,
};
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior, params};

use crate::lease::{
    AuthorityAssignmentRecord, AuthorityAssignmentState, AuthorityLeasePermitRequest,
    AuthorityLeaseRecord, AuthorityLeaseTakeoverFenceRecord, AuthorityLeaseTakeoverFenceRequest,
    AuthoritySuccessorRegistryReopenRecord, AuthoritySuccessorRegistryReopenRequest,
    AuthorityTakeoverBarrierCoverage, AuthorityTakeoverBarrierCoverageState,
    AuthorityTakeoverBarrierReceiptRecord, AuthorityTakeoverBarrierReceiptRequest,
    AuthorityTakeoverBarrierSigner, AuthorityTakeoverCompletionRecord,
    AuthorityTakeoverFenceMemberRecord, AuthorityTakeoverReceiptRecord,
    AuthorityTakeoverReceiptState, BarrierObservationSignature, CompleteAuthorityTakeoverRequest,
    barrier_observation_signature_message, derive_assignment_id,
    derive_takeover_barrier_receipt_id, derive_takeover_fence_receipt_id,
    derive_takeover_receipt_id, insert_assignment, insert_takeover_barrier_receipt,
    insert_takeover_fence_member, insert_takeover_fence_receipt, insert_takeover_receipt,
    load_assignment_by_id, load_current_assignment, load_takeover_barrier_receipt_by_participant,
    load_takeover_barrier_receipts, load_takeover_fence_members, load_takeover_fence_receipt,
    load_takeover_receipt, load_takeover_receipt_by_id, mark_assignment_fenced,
    mark_assignment_takeover_pending, refresh_active_assignment,
    validate_authority_lease_binding_in_transaction,
};
use crate::migrations::{
    migrate_v1, migrate_v2, migrate_v3, migrate_v4, migrate_v5, migrate_v6, migrate_v7, migrate_v8,
    migrate_v9, migrate_v10, migrate_v11, migrate_v12, migrate_v13, migrate_v14, migrate_v15,
    migrate_v16, migrate_v17, migrate_v18, migrate_v19, migrate_v20, migrate_v21, migrate_v22,
    migrate_v23, migrate_v24, migrate_v25, migrate_v26, migrate_v27, migrate_v28, migrate_v29,
    migrate_v30, migrate_v31, migrate_v32, migrate_v33, migrate_v34, migrate_v35, migrate_v36,
    migrate_v37, migrate_v38, migrate_v39, migrate_v40, migrate_v41,
};
use crate::model::{derive_closure_receipt_id, derive_permit_id, empty_effect_history_root};
use crate::{
    AttemptHandle, AttemptRecord, AttemptRegistrationDecision, AttemptSpec, AttemptState,
    CancelDecision, CancelRequest, ClosedAttempt, PermitConflict, PermitDecision, PermitRecord,
    PermitRequest, PermitState, PlannedEffect, ReceiptOutcome, SnapshotBundle, SnapshotConsistency,
    TaskReceiptRecord, TaskRecord, TaskRegistrationDecision, TaskSnapshotReceiptRecord,
    TaskSnapshotReceiptSpec, TaskSpec, TaskState, TaskStoreError, TaskWriteSetArtifactRead,
    TaskWriteSetArtifactWrite, TaskWriteSetDecision, TaskWriteSetEffectEndpoint,
    TaskWriteSetEffectEndpointKind, TaskWriteSetEffectEndpointRequest, TaskWriteSetRecord,
    TaskWriteSetRequest, TaskWriteSetSemanticAppend, TaskWriteSetSemanticRequiredDurability,
    TaskWriteSetSemanticTarget,
};

const SCHEMA_VERSION: i64 = 41;

/// A single-writer `SQLite` task authority.
///
/// All mutating APIs are linearized through one `BEGIN IMMEDIATE`
/// transaction per call, which serializes cancel, permit issuance, and
/// finalize on the same control/cancel/permit epochs
/// (`[TASK-CANCEL-003]`).
pub struct SqliteTaskAuthority {
    connection: Mutex<Connection>,
}

pub(crate) struct StoredTask {
    pub(crate) record: TaskRecord,
    revision: i64,
}

struct StoredCancel {
    idempotency_key: IdempotencyKey,
    cancel_epoch_after: u64,
}

/// Borrowed owner-authority bundle for the struct-based seal and permit
/// entries. Every field mirrors the exact `Option<&_>` authority slot the
/// ladder constructors thread into `seal_task_write_set_inner` /
/// `request_commit_permit_inner`, so one value can name any combination
/// of authorities — including kinds no single ladder constructor carries
/// together (for example Operation and Channel endpoints in one write
/// set). The default value is the authority-free (all-`None`) bundle.
#[derive(Clone, Copy, Default)]
pub struct Authorities<'a> {
    /// Artifact head owner; required by every seal entry, optional for
    /// permit revalidation.
    pub artifact: Option<&'a nlos_artifact::ArtifactStore>,
    /// Process/AgentInstance/IsolationDomain binding owner.
    pub process: Option<&'a nlos_process::ProcessAuthority>,
    /// Semantic admission/append owner (seal only; permit-side Semantic
    /// revalidation stays on the dedicated Semantic-aware finalize path).
    pub semantic: Option<&'a nlos_semantic::SemanticAuthority>,
    /// Resource reservation/ledger owner.
    pub resource: Option<&'a nlos_resource::ResourceAuthority>,
    /// Operation endpoint proof owner.
    pub operation: Option<&'a nlos_store::SqliteOperationStore>,
    /// Channel endpoint proof owner.
    pub channel: Option<&'a nlos_channel::ChannelAuthority>,
}

// The owner stores are opaque SQLite handles without `Debug`, so the
// bundle prints per-field presence instead of handle contents.
impl fmt::Debug for Authorities<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Authorities")
            .field("artifact", &self.artifact.is_some())
            .field("process", &self.process.is_some())
            .field("semantic", &self.semantic.is_some())
            .field("resource", &self.resource.is_some())
            .field("operation", &self.operation.is_some())
            .field("channel", &self.channel.is_some())
            .finish()
    }
}

impl SqliteTaskAuthority {
    /// Opens or creates a task authority database and validates its schema.
    ///
    /// Equivalent to [`SqliteTaskAuthority::open_with_vfs`] with `None`,
    /// i.e. the process-default `SQLite` VFS.
    ///
    /// # Errors
    ///
    /// Returns an error when the database cannot be opened, when WAL/FULL
    /// durability cannot be established (verified by reading the pragmas
    /// back; a silent fallback is rejected with
    /// [`TaskStoreError::DurabilityUnavailable`]), or when the stored schema
    /// version cannot be migrated or validated.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, TaskStoreError> {
        Self::open_with_vfs(path, None)
    }

    /// Opens or creates a task authority database through a named
    /// `SQLite` VFS.
    ///
    /// `vfs = None` uses the process-default VFS; `Some(name)` selects a VFS
    /// previously registered under that name (e.g. a fault-injection shim
    /// registered by tests). The open flags are identical to
    /// [`Connection::open`] regardless of the chosen VFS.
    ///
    /// # Errors
    ///
    /// Returns an error when the named VFS does not exist, when the database
    /// cannot be opened, when WAL/FULL durability cannot be established
    /// (verified by reading the pragmas back; a silent fallback is rejected
    /// with [`TaskStoreError::DurabilityUnavailable`]), or when the stored
    /// schema version cannot be migrated or validated.
    #[allow(clippy::too_many_lines)] // Explicit linear migration chain is easier to audit.
    pub fn open_with_vfs(
        path: impl AsRef<Path>,
        vfs: Option<&str>,
    ) -> Result<Self, TaskStoreError> {
        let mut connection = match vfs {
            None => Connection::open(path)?,
            Some(name) => Connection::open_with_flags_and_vfs(path, OpenFlags::default(), name)?,
        };
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;

        // `pragma_update` discards the result row of `journal_mode`, so a
        // failed WAL transition would silently fall back (e.g. to `delete`).
        // Read both durability pragmas back and fail closed.
        let journal_mode: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let synchronous: i64 =
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 {
            return Err(TaskStoreError::DurabilityUnavailable {
                journal_mode,
                synchronous,
            });
        }

        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => {
                migrate_v1(&mut connection)?;
                migrate_v2(&mut connection)?;
                migrate_v3(&mut connection)?;
                migrate_v4(&mut connection)?;
                migrate_v5(&mut connection)?;
                migrate_v6(&mut connection)?;
                migrate_v7(&mut connection)?;
                migrate_v8(&mut connection)?;
                migrate_v9(&mut connection)?;
                migrate_v10(&mut connection)?;
                migrate_v11(&mut connection)?;
                migrate_v12(&mut connection)?;
            }
            1 => {
                migrate_v2(&mut connection)?;
                migrate_v3(&mut connection)?;
                migrate_v4(&mut connection)?;
                migrate_v5(&mut connection)?;
                migrate_v6(&mut connection)?;
                migrate_v7(&mut connection)?;
                migrate_v8(&mut connection)?;
                migrate_v9(&mut connection)?;
                migrate_v10(&mut connection)?;
                migrate_v11(&mut connection)?;
                migrate_v12(&mut connection)?;
            }
            2 => {
                migrate_v3(&mut connection)?;
                migrate_v4(&mut connection)?;
                migrate_v5(&mut connection)?;
                migrate_v6(&mut connection)?;
                migrate_v7(&mut connection)?;
                migrate_v8(&mut connection)?;
                migrate_v9(&mut connection)?;
                migrate_v10(&mut connection)?;
                migrate_v11(&mut connection)?;
                migrate_v12(&mut connection)?;
            }
            3 => {
                migrate_v4(&mut connection)?;
                migrate_v5(&mut connection)?;
                migrate_v6(&mut connection)?;
                migrate_v7(&mut connection)?;
                migrate_v8(&mut connection)?;
                migrate_v9(&mut connection)?;
                migrate_v10(&mut connection)?;
                migrate_v11(&mut connection)?;
                migrate_v12(&mut connection)?;
            }
            4 => {
                migrate_v5(&mut connection)?;
                migrate_v6(&mut connection)?;
                migrate_v7(&mut connection)?;
                migrate_v8(&mut connection)?;
                migrate_v9(&mut connection)?;
                migrate_v10(&mut connection)?;
                migrate_v11(&mut connection)?;
                migrate_v12(&mut connection)?;
            }
            5 => {
                migrate_v6(&mut connection)?;
                migrate_v7(&mut connection)?;
                migrate_v8(&mut connection)?;
                migrate_v9(&mut connection)?;
                migrate_v10(&mut connection)?;
                migrate_v11(&mut connection)?;
                migrate_v12(&mut connection)?;
            }
            6 => {
                migrate_v7(&mut connection)?;
                migrate_v8(&mut connection)?;
                migrate_v9(&mut connection)?;
                migrate_v10(&mut connection)?;
                migrate_v11(&mut connection)?;
                migrate_v12(&mut connection)?;
            }
            7 => {
                migrate_v8(&mut connection)?;
                migrate_v9(&mut connection)?;
                migrate_v10(&mut connection)?;
                migrate_v11(&mut connection)?;
                migrate_v12(&mut connection)?;
            }
            8 => {
                migrate_v9(&mut connection)?;
                migrate_v10(&mut connection)?;
                migrate_v11(&mut connection)?;
                migrate_v12(&mut connection)?;
            }
            9 => {
                migrate_v10(&mut connection)?;
                migrate_v11(&mut connection)?;
                migrate_v12(&mut connection)?;
            }
            10 => {
                migrate_v11(&mut connection)?;
                migrate_v12(&mut connection)?;
            }
            11 => migrate_v12(&mut connection)?,
            12..=SCHEMA_VERSION => {}
            other => return Err(TaskStoreError::UnsupportedSchema(other)),
        }
        if version < SCHEMA_VERSION {
            migrate_v13(&mut connection)?;
            migrate_v14(&mut connection)?;
            migrate_v15(&mut connection)?;
            migrate_v16(&mut connection)?;
            migrate_v17(&mut connection)?;
            migrate_v18(&mut connection)?;
            migrate_v19(&mut connection)?;
            migrate_v20(&mut connection)?;
            migrate_v21(&mut connection)?;
            migrate_v22(&mut connection)?;
            migrate_v23(&mut connection)?;
            migrate_v24(&mut connection)?;
            migrate_v25(&mut connection)?;
            migrate_v26(&mut connection)?;
            migrate_v27(&mut connection)?;
            migrate_v28(&mut connection)?;
            migrate_v29(&mut connection)?;
            migrate_v30(&mut connection)?;
            migrate_v31(&mut connection)?;
            migrate_v32(&mut connection)?;
            migrate_v33(&mut connection)?;
            migrate_v34(&mut connection)?;
            migrate_v35(&mut connection)?;
            migrate_v36(&mut connection)?;
            migrate_v37(&mut connection)?;
            migrate_v38(&mut connection)?;
            migrate_v39(&mut connection)?;
            migrate_v40(&mut connection)?;
            migrate_v41(&mut connection)?;
        }

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Registers a Task idempotently.
    ///
    /// The initial `TaskHead` is `commit_seq = 0`, the domain-separated
    /// empty effect-history root (`[TASK-EFFECT-ID-001]`), and
    /// `retry_fence_epoch = 0`. Repeating the exact specification returns
    /// `Existing`; reusing the task ID with a different generation is
    /// rejected fail-closed.
    ///
    /// # Errors
    ///
    /// Returns a storage error or `DuplicateTask` for conflicting reuse.
    pub fn register_task(
        &self,
        spec: TaskSpec,
    ) -> Result<TaskRegistrationDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_task_optional(&transaction, spec.task_id)? {
            if existing.record.task_generation == spec.task_generation {
                transaction.commit()?;
                return Ok(TaskRegistrationDecision::Existing(spec.task_id));
            }
            return Err(TaskStoreError::DuplicateTask);
        }
        let record = TaskRecord {
            task_id: spec.task_id,
            task_generation: spec.task_generation,
            head_commit_seq: 0,
            head_effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
            control_epoch: 1,
            cancel_epoch: 0,
            permit_epoch: 0,
            state: TaskState::Active,
            active_permit: None,
            created_at_ms: spec.registered_at_ms,
            updated_at_ms: spec.registered_at_ms,
        };
        insert_task(&transaction, &record)?;
        crate::participant::initialize_registry(&transaction, &record, spec.registered_at_ms)?;
        transaction.commit()?;
        Ok(TaskRegistrationDecision::Created(spec.task_id))
    }

    /// Persists an immutable `TaskSnapshotReceipt` and its ordered signed
    /// authority-checkpoint receipt set. The snapshot must bind the current
    /// Task head/history/fence exactly at registration time.
    ///
    /// # Errors
    ///
    /// Returns a stale/incomplete/conflicting receipt or storage error.
    #[allow(clippy::needless_pass_by_value)]
    pub fn register_snapshot_receipt(
        &self,
        spec: TaskSnapshotReceiptSpec,
    ) -> Result<TaskSnapshotReceiptRecord, TaskStoreError> {
        if spec.built_at_ms < 0 {
            return Err(TaskStoreError::InvalidSnapshotReceipt {
                reason: "built_at_ms must be non-negative",
            });
        }
        if spec.per_authority_checkpoint_receipts.is_empty() {
            return Err(TaskStoreError::InvalidSnapshotReceipt {
                reason: "at least one authority checkpoint receipt is required",
            });
        }
        if spec.per_authority_checkpoint_receipts.len() > 64 {
            return Err(TaskStoreError::InvalidSnapshotReceipt {
                reason: "authority checkpoint receipt count exceeds 64",
            });
        }
        let mut deduplicated = spec.per_authority_checkpoint_receipts.clone();
        deduplicated.sort_unstable();
        deduplicated.dedup();
        if deduplicated.len() != spec.per_authority_checkpoint_receipts.len() {
            return Err(TaskStoreError::InvalidSnapshotReceipt {
                reason: "authority checkpoint receipt IDs must be unique",
            });
        }

        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = load_task(&transaction, spec.task_id)?;
        if let Some(existing) =
            load_snapshot_receipt_optional(&transaction, spec.task_id, spec.receipt_id)?
        {
            if existing == spec {
                transaction.commit()?;
                return Ok(existing);
            }
            return Err(TaskStoreError::InvalidSnapshotReceipt {
                reason: "receipt ID was rebound to different bytes",
            });
        }
        if load_snapshot_receipt_by_snapshot_optional(
            &transaction,
            spec.task_id,
            spec.snapshot.snapshot_id,
        )?
        .is_some()
        {
            return Err(TaskStoreError::InvalidSnapshotReceipt {
                reason: "snapshot already has a different receipt",
            });
        }
        if validate_head_binding(&task.record, &spec.snapshot).is_some() {
            return Err(TaskStoreError::InvalidSnapshotReceipt {
                reason: "snapshot does not bind the current Task head/history/fence",
            });
        }
        insert_snapshot_if_absent(&transaction, spec.task_id, &spec.snapshot, spec.built_at_ms)?;
        insert_snapshot_receipt(&transaction, &spec)?;
        transaction.commit()?;
        Ok(spec)
    }

    /// Reads one immutable durable `TaskSnapshotReceipt`.
    ///
    /// # Errors
    ///
    /// Returns `SnapshotReceiptNotFound` or a storage error.
    pub fn inspect_snapshot_receipt(
        &self,
        task_id: TaskId,
        receipt_id: ReceiptId,
    ) -> Result<TaskSnapshotReceiptRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_snapshot_receipt(&*connection, task_id, receipt_id)
    }

    /// Seals an authority-verified snapshot/read-set `TaskWriteSet` slice.
    /// Artifact heads are read directly from the owning `ArtifactAuthority`;
    /// Task snapshot, group, and participant facts are read in the same
    /// `TaskAuthority` transaction that persists the immutable seal.
    ///
    /// # Errors
    ///
    /// Returns owner readback, snapshot binding, duplicate-read,
    /// idempotency, participant, or storage errors. No `TaskWriteSet` row is
    /// written when an owner read does not match the requested revision.
    #[allow(clippy::needless_pass_by_value)]
    #[allow(clippy::too_many_lines)]
    #[deprecated(
        since = "0.1.0",
        note = "use seal_task_write_set_with_authorities_struct with Authorities { artifact: Some(..), .. }"
    )]
    pub fn seal_task_write_set(
        &self,
        artifact_authority: &nlos_artifact::ArtifactStore,
        request: TaskWriteSetRequest,
    ) -> Result<TaskWriteSetDecision, TaskStoreError> {
        self.seal_task_write_set_inner(artifact_authority, None, None, None, None, None, request)
    }

    /// Seals the snapshot/read-set slice and an owner-verified current
    /// Process/AgentInstance/IsolationDomain binding.
    ///
    /// The Process endpoint must already be present in the OPEN Task
    /// participant registry; this method never expands the registry behind a
    /// seal. Use [`Self::register_process_binding_participant`] first.
    ///
    /// # Errors
    ///
    /// Returns typed Process owner, binding, registry, snapshot, read-set,
    /// idempotency, or storage errors.
    #[allow(clippy::needless_pass_by_value)]
    #[allow(clippy::too_many_lines)]
    #[deprecated(
        since = "0.1.0",
        note = "use seal_task_write_set_with_authorities_struct with Authorities { artifact: Some(..), process: Some(..), .. }"
    )]
    pub fn seal_task_write_set_with_process_authority(
        &self,
        artifact_authority: &nlos_artifact::ArtifactStore,
        process_authority: &nlos_process::ProcessAuthority,
        request: TaskWriteSetRequest,
    ) -> Result<TaskWriteSetDecision, TaskStoreError> {
        self.seal_task_write_set_inner(
            artifact_authority,
            Some(process_authority),
            None,
            None,
            None,
            None,
            request,
        )
    }

    /// Seals a write set after `SemanticAuthority` event readback.
    ///
    /// # Errors
    ///
    /// Returns typed Semantic owner, endpoint, read-set, snapshot, or storage
    /// errors.
    #[allow(clippy::needless_pass_by_value)]
    #[allow(clippy::too_many_lines)]
    #[deprecated(
        since = "0.1.0",
        note = "use seal_task_write_set_with_authorities_struct with Authorities { artifact: Some(..), semantic: Some(..), .. }"
    )]
    pub fn seal_task_write_set_with_semantic_authority(
        &self,
        artifact_authority: &nlos_artifact::ArtifactStore,
        semantic_authority: &nlos_semantic::SemanticAuthority,
        request: TaskWriteSetRequest,
    ) -> Result<TaskWriteSetDecision, TaskStoreError> {
        self.seal_task_write_set_inner(
            artifact_authority,
            None,
            Some(semantic_authority),
            None,
            None,
            None,
            request,
        )
    }

    /// Seals a write set after `ResourceAuthority` Reservation readback.
    ///
    /// # Errors
    ///
    /// Returns typed Resource owner, endpoint, reservation, snapshot, or
    /// storage errors.
    #[allow(clippy::needless_pass_by_value)]
    #[allow(clippy::too_many_lines)]
    #[deprecated(
        since = "0.1.0",
        note = "use seal_task_write_set_with_authorities_struct with Authorities { artifact: Some(..), resource: Some(..), .. }"
    )]
    pub fn seal_task_write_set_with_resource_authority(
        &self,
        artifact_authority: &nlos_artifact::ArtifactStore,
        resource_authority: &nlos_resource::ResourceAuthority,
        request: TaskWriteSetRequest,
    ) -> Result<TaskWriteSetDecision, TaskStoreError> {
        self.seal_task_write_set_inner(
            artifact_authority,
            None,
            None,
            Some(resource_authority),
            None,
            None,
            request,
        )
    }

    /// Seals a write set after direct readback from Process, Semantic, and
    /// Resource owner authorities. All endpoint participants must have been
    /// registered in the same OPEN Task registry before this call.
    ///
    /// # Errors
    ///
    /// Returns typed owner-read, endpoint-registration, snapshot, read-set,
    /// idempotency, or storage errors.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_lines)]
    #[deprecated(
        since = "0.1.0",
        note = "use seal_task_write_set_with_authorities_struct with Authorities { artifact/process/semantic/resource: Some(..) }"
    )]
    pub fn seal_task_write_set_with_authorities(
        &self,
        artifact_authority: &nlos_artifact::ArtifactStore,
        process_authority: &nlos_process::ProcessAuthority,
        semantic_authority: &nlos_semantic::SemanticAuthority,
        resource_authority: &nlos_resource::ResourceAuthority,
        request: TaskWriteSetRequest,
    ) -> Result<TaskWriteSetDecision, TaskStoreError> {
        self.seal_task_write_set_inner(
            artifact_authority,
            Some(process_authority),
            Some(semantic_authority),
            Some(resource_authority),
            None,
            None,
            request,
        )
    }

    /// Seals a write set after the owning Operation authority has returned an
    /// exact `OperationId + Generation` endpoint proof. The Operation proof
    /// is persisted only when the endpoint is already present in the OPEN
    /// participant registry; this method does not expand that registry.
    ///
    /// # Errors
    ///
    /// Returns typed Operation owner, endpoint-registration, snapshot,
    /// read-set, idempotency, or storage errors.
    #[allow(clippy::needless_pass_by_value)]
    #[allow(clippy::too_many_lines)]
    #[deprecated(
        since = "0.1.0",
        note = "use seal_task_write_set_with_authorities_struct with Authorities { artifact: Some(..), operation: Some(..), .. }"
    )]
    pub fn seal_task_write_set_with_operation_authority(
        &self,
        artifact_authority: &nlos_artifact::ArtifactStore,
        operation_authority: &nlos_store::SqliteOperationStore,
        request: TaskWriteSetRequest,
    ) -> Result<TaskWriteSetDecision, TaskStoreError> {
        self.seal_task_write_set_inner(
            artifact_authority,
            None,
            None,
            None,
            Some(operation_authority),
            None,
            request,
        )
    }

    /// Seals a write set after the owning Channel authority has returned the
    /// current-generation `ChannelEndpointProof`. The Channel proof is
    /// persisted only when the endpoint is already present in the OPEN
    /// participant registry; this method does not expand that registry.
    ///
    /// # Errors
    ///
    /// Returns typed Channel owner, endpoint-registration, snapshot,
    /// read-set, idempotency, or storage errors.
    #[allow(clippy::needless_pass_by_value)]
    #[allow(clippy::too_many_lines)]
    #[deprecated(
        since = "0.1.0",
        note = "use seal_task_write_set_with_authorities_struct with Authorities { artifact: Some(..), channel: Some(..), .. }"
    )]
    pub fn seal_task_write_set_with_channel_authority(
        &self,
        artifact_authority: &nlos_artifact::ArtifactStore,
        channel_authority: &nlos_channel::ChannelAuthority,
        request: TaskWriteSetRequest,
    ) -> Result<TaskWriteSetDecision, TaskStoreError> {
        self.seal_task_write_set_inner(
            artifact_authority,
            None,
            None,
            None,
            None,
            Some(channel_authority),
            request,
        )
    }

    /// Seals a write set after direct readback from Process, Semantic,
    /// Resource, and Operation owner authorities. This is the combined entry
    /// point for a request carrying more than one non-Artifact endpoint kind.
    ///
    /// # Errors
    ///
    /// Returns typed owner-proof, endpoint-registration, snapshot, read-set,
    /// idempotency, or storage errors.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_lines)]
    #[deprecated(
        since = "0.1.0",
        note = "use seal_task_write_set_with_authorities_struct with Authorities { artifact/process/semantic/resource/operation: Some(..) }"
    )]
    pub fn seal_task_write_set_with_authorities_and_operation_authority(
        &self,
        artifact_authority: &nlos_artifact::ArtifactStore,
        process_authority: &nlos_process::ProcessAuthority,
        semantic_authority: &nlos_semantic::SemanticAuthority,
        resource_authority: &nlos_resource::ResourceAuthority,
        operation_authority: &nlos_store::SqliteOperationStore,
        request: TaskWriteSetRequest,
    ) -> Result<TaskWriteSetDecision, TaskStoreError> {
        self.seal_task_write_set_inner(
            artifact_authority,
            Some(process_authority),
            Some(semantic_authority),
            Some(resource_authority),
            Some(operation_authority),
            None,
            request,
        )
    }

    /// Seals a write set with the owner authorities named by one
    /// [`Authorities`] bundle. This is the combined entry point for a
    /// request whose effect endpoints span authority kinds that no single
    /// ladder constructor carries together (for example an
    /// `OperationBinding` and a `ChannelTopicBinding` in the same write
    /// set); the bundle is destructured into the exact inner authority
    /// slots used by the ladder constructors, with no separate sealing
    /// path.
    ///
    /// `authorities.artifact` is required — like every seal entry — and a
    /// missing Artifact authority fails closed before any owner read.
    ///
    /// # Errors
    ///
    /// Returns typed owner-proof, endpoint-registration, snapshot,
    /// read-set, idempotency, or storage errors.
    #[allow(clippy::needless_pass_by_value)]
    pub fn seal_task_write_set_with_authorities_struct(
        &self,
        authorities: Authorities<'_>,
        request: TaskWriteSetRequest,
    ) -> Result<TaskWriteSetDecision, TaskStoreError> {
        let artifact_authority =
            authorities
                .artifact
                .ok_or(TaskStoreError::TaskWriteSetConflict {
                    reason: "struct-based seal requires the Artifact authority",
                })?;
        self.seal_task_write_set_inner(
            artifact_authority,
            authorities.process,
            authorities.semantic,
            authorities.resource,
            authorities.operation,
            authorities.channel,
            request,
        )
    }

    #[allow(clippy::needless_pass_by_value)]
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_lines)]
    fn seal_task_write_set_inner(
        &self,
        artifact_authority: &nlos_artifact::ArtifactStore,
        process_authority: Option<&nlos_process::ProcessAuthority>,
        semantic_authority: Option<&nlos_semantic::SemanticAuthority>,
        resource_authority: Option<&nlos_resource::ResourceAuthority>,
        operation_authority: Option<&nlos_store::SqliteOperationStore>,
        channel_authority: Option<&nlos_channel::ChannelAuthority>,
        request: TaskWriteSetRequest,
    ) -> Result<TaskWriteSetDecision, TaskStoreError> {
        if request.sealed_at_ms < 0 {
            return Err(TaskStoreError::TaskWriteSetConflict {
                reason: "sealed_at_ms must be non-negative",
            });
        }
        let mut artifact_reads = request.artifact_reads.clone();
        artifact_reads.sort_unstable_by_key(|read| read.artifact_id.into_bytes());
        if artifact_reads
            .windows(2)
            .any(|pair| pair[0].artifact_id == pair[1].artifact_id)
        {
            return Err(TaskStoreError::TaskWriteSetConflict {
                reason: "Artifact read set contains duplicate artifact IDs",
            });
        }
        for read in &artifact_reads {
            let now_ms = u64::try_from(request.sealed_at_ms).map_err(|_| {
                TaskStoreError::TaskWriteSetConflict {
                    reason: "sealed_at_ms must be non-negative",
                }
            })?;
            let head = artifact_authority
                .resolve_head(read.artifact_id, now_ms)
                .map_err(TaskStoreError::ArtifactParticipantAuthority)?;
            let (current_revision, current_digest) = head.map_or((0, None), |head| {
                (head.revision, Some(head.digest.into_bytes()))
            });
            if current_revision != read.expected_head_revision
                || current_digest != read.expected_head_digest
            {
                return Err(TaskStoreError::TaskWriteSetReadConflict);
            }
        }

        let mut artifact_writes = request.artifact_writes.clone();
        artifact_writes.sort_unstable_by_key(|write| {
            (write.artifact_id.into_bytes(), write.proposed_revision)
        });
        if artifact_writes
            .windows(2)
            .any(|pair| pair[0].artifact_id == pair[1].artifact_id)
        {
            return Err(TaskStoreError::TaskWriteSetConflict {
                reason: "Artifact write set contains duplicate artifact slots",
            });
        }
        let mut artifact_write_participants = Vec::with_capacity(artifact_writes.len());
        for write in &artifact_writes {
            let now_ms = u64::try_from(request.sealed_at_ms).map_err(|_| {
                TaskStoreError::TaskWriteSetConflict {
                    reason: "sealed_at_ms must be non-negative",
                }
            })?;
            let head = artifact_authority
                .resolve_head(write.artifact_id, now_ms)
                .map_err(TaskStoreError::ArtifactParticipantAuthority)?;
            let current_revision = head.map_or(0, |head| head.revision);
            let expected_target = write
                .expected_head_revision
                .checked_add(1)
                .ok_or(TaskStoreError::EpochExhausted)?;
            if current_revision != write.expected_head_revision
                || write.proposed_revision != expected_target
            {
                return Err(TaskStoreError::TaskWriteSetConflict {
                    reason: "Artifact write declaration disagrees with current head",
                });
            }
            let proof = artifact_authority
                .inspect_head_endpoint_proof(write.artifact_id)
                .map_err(TaskStoreError::ArtifactParticipantAuthority)?;
            artifact_write_participants.push(crate::ParticipantRecord {
                participant_type: crate::ParticipantType::ArtifactHead,
                participant_id: proof.participant_id,
                participant_generation: proof.participant_generation,
                admission_receipt_id: proof.admission_receipt_id,
            });
        }

        let process_binding = match (request.process_binding, process_authority) {
            (None, _) => None,
            (Some(_), None) => {
                return Err(TaskStoreError::TaskWriteSetConflict {
                    reason: "Process binding requires ProcessAuthority readback",
                });
            }
            (Some(expected), Some(authority)) => {
                let active = nlos_process::ActiveProcessBinding {
                    process_id: expected.process_id,
                    process_generation: expected.process_generation,
                    process_fencing_token: expected.process_fencing_token,
                    agent_instance_id: expected.agent_instance_id,
                    agent_instance_generation: expected.agent_instance_generation,
                    isolation_domain_id: expected.isolation_domain_id,
                    isolation_domain_generation: expected.isolation_domain_generation,
                    isolation_domain_fencing_token: expected.isolation_domain_fencing_token,
                };
                let owner_record = authority
                    .verify_active_process_binding(&active)
                    .map_err(TaskStoreError::ProcessParticipantAuthority)?;
                if owner_record.task_id != request.task_id
                    || owner_record.task_attempt_id != request.attempt_id
                    || owner_record.attempt_generation != request.attempt_generation
                {
                    return Err(TaskStoreError::TaskWriteSetConflict {
                        reason: "Process binding is for a different TaskAttempt",
                    });
                }
                let proof = authority
                    .inspect_binding_endpoint_proof(expected.process_id)
                    .map_err(TaskStoreError::ProcessParticipantAuthority)?;
                Some(crate::TaskWriteSetProcessBinding {
                    process_id: owner_record.process_id,
                    process_generation: owner_record.process_generation,
                    process_fencing_token: owner_record.process_fencing_token,
                    agent_instance_id: owner_record.agent_instance_id,
                    agent_instance_generation: owner_record.agent_instance_generation,
                    isolation_domain_id: owner_record.isolation_domain_id,
                    isolation_domain_generation: owner_record.isolation_domain_generation,
                    isolation_domain_fencing_token: owner_record.isolation_domain_fencing_token,
                    participant_id: proof.participant_id,
                    participant_generation: proof.participant_generation,
                    admission_receipt_id: proof.admission_receipt_id,
                })
            }
        };

        let mut semantic_reads = request.semantic_reads.clone();
        semantic_reads.sort_unstable_by_key(|read| read.event_id);
        if semantic_reads
            .windows(2)
            .any(|pair| pair[0].event_id == pair[1].event_id)
        {
            return Err(TaskStoreError::TaskWriteSetSemanticReadConflict);
        }
        let semantic_endpoint = if semantic_reads.is_empty() && request.semantic_appends.is_empty()
        {
            None
        } else {
            let authority = semantic_authority.ok_or(TaskStoreError::TaskWriteSetConflict {
                reason: "Semantic reads/appends require SemanticAuthority readback",
            })?;
            let proof = authority
                .inspect_admission_endpoint_proof()
                .map_err(TaskStoreError::SemanticParticipantAuthority)?;
            for read in &semantic_reads {
                let event = authority
                    .inspect_event(read.event_id)
                    .map_err(TaskStoreError::SemanticParticipantAuthority)?;
                if event.log_seq != read.expected_log_seq
                    || crate::model::semantic_canonical_digest(&event.canonical_unsigned_event)
                        != read.expected_canonical_digest
                {
                    return Err(TaskStoreError::TaskWriteSetSemanticReadConflict);
                }
            }
            Some(crate::ParticipantRecord {
                participant_type: crate::ParticipantType::SemanticAdmission,
                participant_id: proof.participant_id,
                participant_generation: proof.participant_generation,
                admission_receipt_id: proof.admission_receipt_id,
            })
        };

        let mut semantic_appends = request.semantic_appends.clone();
        semantic_appends.sort_unstable_by_key(|append| append.event_id);
        if semantic_appends
            .windows(2)
            .any(|pair| pair[0].event_id == pair[1].event_id)
        {
            return Err(TaskStoreError::TaskWriteSetConflict {
                reason: "Semantic append set contains duplicate event IDs",
            });
        }
        let mut semantic_append_records = Vec::with_capacity(semantic_appends.len());
        if !semantic_appends.is_empty() {
            let authority = semantic_authority.ok_or(TaskStoreError::TaskWriteSetConflict {
                reason: "Semantic appends require SemanticAuthority readback",
            })?;
            for append in semantic_appends {
                let event = authority
                    .inspect_event(append.event_id)
                    .map_err(TaskStoreError::SemanticParticipantAuthority)?;
                if event.scope_kind() != append.target.kind()
                    || event.scope_id() != append.target.id()
                {
                    return Err(TaskStoreError::TaskWriteSetConflict {
                        reason: "Semantic append target scope differs from admitted event",
                    });
                }
                let receipt = authority
                    .inspect_admission_receipt(append.event_id)
                    .map_err(TaskStoreError::SemanticParticipantAuthority)?;
                if receipt.event_id != append.event_id
                    || receipt.log_seq != event.log_seq
                    || !matches!(
                        (append.required_durability, receipt.durability),
                        (
                            TaskWriteSetSemanticRequiredDurability::Durable,
                            nlos_semantic::AdmissionDurability::Durable
                        )
                    )
                {
                    return Err(TaskStoreError::TaskWriteSetConflict {
                        reason: "Semantic append lacks the required durable AdmissionReceipt",
                    });
                }
                if receipt.authz_policy_digest != append.expected_admission_policy_digest {
                    return Err(TaskStoreError::TaskWriteSetConflict {
                        reason: "Semantic append admission policy differs from owner receipt",
                    });
                }
                if let Some(durability_receipt_id) = append.durability_receipt_id {
                    let durability_receipt = authority
                        .inspect_durability_receipt(append.event_id, durability_receipt_id)
                        .map_err(TaskStoreError::SemanticParticipantAuthority)?;
                    if durability_receipt.event_id != append.event_id
                        || durability_receipt.receipt_id != durability_receipt_id
                    {
                        return Err(TaskStoreError::TaskWriteSetConflict {
                            reason: "Semantic durability receipt does not match admitted event",
                        });
                    }
                }
                semantic_append_records.push(TaskWriteSetSemanticAppend {
                    event_id: append.event_id,
                    target: append.target,
                    required_durability: append.required_durability,
                    admission_receipt_id: receipt.receipt_id,
                    admission_policy_digest: Some(append.expected_admission_policy_digest),
                    durability_receipt_id: append.durability_receipt_id,
                });
            }
        }

        let mut resource_requests = request.resource_reservations.clone();
        resource_requests.sort_unstable_by_key(|reservation| reservation.reservation_id);
        if resource_requests
            .windows(2)
            .any(|pair| pair[0].reservation_id == pair[1].reservation_id)
        {
            return Err(TaskStoreError::TaskWriteSetResourceReservationConflict);
        }
        let mut resource_reservations = Vec::with_capacity(resource_requests.len());
        let mut resource_participants = Vec::new();
        if !resource_requests.is_empty() {
            let authority = resource_authority.ok_or(TaskStoreError::TaskWriteSetConflict {
                reason: "Resource reservations require ResourceAuthority readback",
            })?;
            for request in resource_requests {
                let reservation = authority
                    .inspect_permit_binding(request.reservation_id)
                    .map_err(TaskStoreError::ResourceParticipantAuthority)?;
                if reservation.call_id != request.expected_call_id
                    || reservation.operation_id != request.expected_operation_id
                    || reservation.quote_id != request.expected_quote_id
                {
                    return Err(TaskStoreError::TaskWriteSetResourceReservationConflict);
                }
                let driver_proof = authority
                    .inspect_driver_gateway_endpoint_proof(reservation.driver_id)
                    .map_err(TaskStoreError::ResourceParticipantAuthority)?;
                if driver_proof.participant_generation != reservation.driver_generation {
                    return Err(TaskStoreError::TaskWriteSetResourceReservationConflict);
                }
                let account_proof = authority
                    .inspect_resource_ledger_endpoint_proof(reservation.account_id)
                    .map_err(TaskStoreError::ResourceParticipantAuthority)?;
                resource_participants.push(crate::ParticipantRecord {
                    participant_type: crate::ParticipantType::DriverGateway,
                    participant_id: driver_proof.participant_id,
                    participant_generation: driver_proof.participant_generation,
                    admission_receipt_id: driver_proof.admission_receipt_id,
                });
                resource_participants.push(crate::ParticipantRecord {
                    participant_type: crate::ParticipantType::ResourceLedger,
                    participant_id: account_proof.participant_id,
                    participant_generation: account_proof.participant_generation,
                    admission_receipt_id: account_proof.admission_receipt_id,
                });
                resource_reservations.push(crate::TaskWriteSetResourceReservation {
                    reservation_id: reservation.reservation_id,
                    account_id: reservation.account_id,
                    quote_id: reservation.quote_id,
                    call_id: reservation.call_id,
                    operation_id: reservation.operation_id,
                    driver_id: reservation.driver_id,
                    device_id: reservation.device_id,
                    driver_generation: reservation.driver_generation,
                    driver_fencing_token: reservation.driver_fencing_token,
                    upper_bound: reservation.upper_bound,
                });
            }
        }

        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = load_task(&transaction, request.task_id)?;
        let planned_effects = request.planned_effects.clone();
        let effect_set_root = if planned_effects.is_empty() {
            [0; 32]
        } else {
            crate::effect::validate_planned_effects(
                task.record.task_id,
                task.record.task_generation,
                &planned_effects,
            )?
        };
        let attempt = load_attempt(&transaction, request.task_id, request.attempt_id)?;
        if attempt.attempt_generation != request.attempt_generation {
            return Err(TaskStoreError::InvalidGeneration);
        }
        let (effect_endpoints, effect_endpoint_participants) = resolve_effect_endpoints(
            artifact_authority,
            process_authority,
            semantic_authority,
            resource_authority,
            operation_authority,
            channel_authority,
            &request.effect_endpoints,
            planned_effects.len(),
            &task.record,
            request.attempt_id,
            request.attempt_generation,
        )?;
        let effect_endpoint_set_root = crate::model::effect_endpoint_set_root(&effect_endpoints);
        let snapshot_receipt_id =
            attempt
                .snapshot_receipt_id
                .ok_or(TaskStoreError::TaskWriteSetConflict {
                    reason: "TaskWriteSet requires a receipted snapshot-bound attempt",
                })?;
        let snapshot_receipt =
            load_snapshot_receipt(&transaction, request.task_id, snapshot_receipt_id)?;
        if snapshot_receipt.snapshot != attempt.snapshot
            || snapshot_receipt.achieved_consistency == SnapshotConsistency::MixedNonSettleable
        {
            return Err(TaskStoreError::TaskWriteSetConflict {
                reason: "attempt snapshot receipt does not authorize this write set",
            });
        }
        if validate_head_binding(&task.record, &attempt.snapshot).is_some() {
            return Err(TaskStoreError::StaleTaskHead);
        }
        let group_binding = crate::group::current_commit_binding(&transaction, request.attempt_id)?;
        let registry = crate::participant::initialize_registry(
            &transaction,
            &task.record,
            request.sealed_at_ms,
        )?;
        if registry.state != crate::ParticipantRegistryState::Open {
            return Err(TaskStoreError::ParticipantRegistryFrozen {
                state: registry.state,
            });
        }
        for participant in artifact_write_participants {
            if !crate::participant::has_participant(&registry, participant) {
                return Err(TaskStoreError::TaskWriteSetConflict {
                    reason: "Artifact write endpoint is not registered in participant registry",
                });
            }
        }
        if let Some(binding) = process_binding {
            let participant = crate::ParticipantRecord {
                participant_type: crate::ParticipantType::ProcessBinding,
                participant_id: binding.participant_id,
                participant_generation: binding.participant_generation,
                admission_receipt_id: binding.admission_receipt_id,
            };
            if !crate::participant::has_participant(&registry, participant) {
                return Err(TaskStoreError::TaskWriteSetConflict {
                    reason: "Process endpoint is not registered in participant registry",
                });
            }
        }
        if let Some(participant) = semantic_endpoint
            && !crate::participant::has_participant(&registry, participant)
        {
            return Err(TaskStoreError::TaskWriteSetConflict {
                reason: "Semantic endpoint is not registered in participant registry",
            });
        }
        for participant in resource_participants {
            if !crate::participant::has_participant(&registry, participant) {
                return Err(TaskStoreError::TaskWriteSetConflict {
                    reason: "Resource endpoint is not registered in participant registry",
                });
            }
        }
        for participant in effect_endpoint_participants {
            if !crate::participant::has_participant(&registry, participant) {
                return Err(TaskStoreError::TaskWriteSetConflict {
                    reason: "planned effect endpoint is not registered in participant registry",
                });
            }
        }
        let participant_registry_binding = crate::ParticipantRegistryBinding {
            generation: registry.generation,
            root: registry.root,
        };
        let artifact_read_set_root = crate::model::artifact_read_set_root(&artifact_reads);
        let artifact_write_set_root = crate::model::artifact_write_set_root(&artifact_writes);
        let semantic_read_set_root = crate::model::semantic_read_set_root(&semantic_reads);
        let semantic_append_set_root =
            crate::model::semantic_append_set_root(&semantic_append_records);
        let resource_reservation_set_root =
            crate::model::resource_reservation_set_root(&resource_reservations);
        let mut record = TaskWriteSetRecord {
            task_id: request.task_id,
            attempt_id: request.attempt_id,
            attempt_generation: request.attempt_generation,
            idempotency_key: request.idempotency_key,
            snapshot_id: attempt.snapshot.snapshot_id,
            snapshot_receipt_id,
            expected_head_commit_seq: attempt.snapshot.expected_head_commit_seq,
            effect_history_root: attempt.snapshot.effect_history_root,
            retry_fence_epoch: attempt.snapshot.retry_fence_epoch,
            group_binding,
            participant_registry_binding,
            artifact_reads,
            artifact_writes,
            process_binding,
            semantic_reads,
            semantic_appends: semantic_append_records,
            resource_reservations,
            planned_effects,
            effect_endpoints,
            artifact_read_set_root,
            semantic_read_set_root,
            resource_reservation_set_root,
            effect_set_root,
            effect_endpoint_set_root,
            semantic_append_set_root,
            artifact_write_set_root,
            write_set_root: [0; 32],
            sealed_at_ms: request.sealed_at_ms,
        };
        record.write_set_root = crate::model::task_write_set_root(&record);
        if let Some(existing) =
            load_write_set_by_key(&transaction, request.task_id, request.idempotency_key)?
        {
            if existing == record {
                transaction.commit()?;
                return Ok(TaskWriteSetDecision::Replayed(existing));
            }
            return Err(TaskStoreError::TaskWriteSetConflict {
                reason: "idempotency key was rebound to different TaskWriteSet bytes",
            });
        }
        if load_write_set_by_root(&transaction, request.task_id, record.write_set_root)?.is_some() {
            return Err(TaskStoreError::TaskWriteSetConflict {
                reason: "write_set_root is already bound to another seal",
            });
        }
        insert_write_set(&transaction, &record)?;
        transaction.commit()?;
        Ok(TaskWriteSetDecision::Sealed(record))
    }

    /// Reads one durable `TaskWriteSet` seal by its idempotency key.
    ///
    /// # Errors
    ///
    /// Returns a typed not-found or storage error when the seal is absent or
    /// cannot be decoded.
    pub fn inspect_task_write_set(
        &self,
        task_id: TaskId,
        idempotency_key: IdempotencyKey,
    ) -> Result<TaskWriteSetRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_write_set_by_key(&*connection, task_id, idempotency_key)?
            .ok_or(TaskStoreError::TaskWriteSetNotFound)
    }

    /// Registers one `TaskAttempt` idempotently.
    ///
    /// Each attempt carries an independent ID/generation and its own
    /// cancellation scope (`[TASK-ATTEMPT-001]`), and binds an immutable
    /// frozen-input snapshot bundle (`[TASK-SNAPSHOT-001]`). Several
    /// attempts MAY bind the same snapshot. Replaying the same idempotency
    /// key with the same bytes returns the original handle; different bytes
    /// under the same key or ID fail closed. A cancelled task admits no new
    /// attempts.
    ///
    /// # Errors
    ///
    /// Returns a validation, conflict, or storage error.
    pub fn register_attempt(
        &self,
        spec: AttemptSpec,
    ) -> Result<AttemptRegistrationDecision, TaskStoreError> {
        self.register_attempt_bound(spec, None)
    }

    /// Registers an attempt bound to an immutable durable
    /// `TaskSnapshotReceipt`. Legacy unreceipted attempts remain readable,
    /// but replay cannot switch between the two registration paths.
    ///
    /// # Errors
    ///
    /// Returns a validation, receipt-binding, conflict, or storage error.
    pub fn register_attempt_with_snapshot_receipt(
        &self,
        spec: AttemptSpec,
        snapshot_receipt_id: ReceiptId,
    ) -> Result<AttemptRegistrationDecision, TaskStoreError> {
        self.register_attempt_bound(spec, Some(snapshot_receipt_id))
    }

    fn register_attempt_bound(
        &self,
        spec: AttemptSpec,
        snapshot_receipt_id: Option<ReceiptId>,
    ) -> Result<AttemptRegistrationDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = load_task(&transaction, spec.task_id)?;
        if let Some(existing) =
            load_attempt_by_key(&transaction, spec.task_id, spec.idempotency_key)?
        {
            if attempt_matches_spec(&existing, &spec)
                && existing.snapshot_receipt_id == snapshot_receipt_id
            {
                transaction.commit()?;
                return Ok(AttemptRegistrationDecision::Existing(handle_of(&existing)));
            }
            return Err(TaskStoreError::IdempotencyConflict);
        }
        if load_attempt_global(&transaction, spec.attempt_id)?.is_some() {
            return Err(TaskStoreError::DuplicateAttempt);
        }
        if task.record.state == TaskState::Cancelled {
            return Err(TaskStoreError::TaskCancelled);
        }
        if let Some(receipt_id) = snapshot_receipt_id {
            let receipt = load_snapshot_receipt(&transaction, spec.task_id, receipt_id)?;
            if receipt.snapshot != spec.snapshot {
                return Err(TaskStoreError::InvalidSnapshotReceipt {
                    reason: "attempt snapshot differs from snapshot receipt",
                });
            }
            if receipt.achieved_consistency == crate::SnapshotConsistency::MixedNonSettleable {
                return Err(TaskStoreError::InvalidSnapshotReceipt {
                    reason: "MIXED_NON_SETTLEABLE snapshot cannot authorize an attempt",
                });
            }
        }
        insert_snapshot_if_absent(
            &transaction,
            spec.task_id,
            &spec.snapshot,
            spec.registered_at_ms,
        )?;
        let record = AttemptRecord {
            task_id: spec.task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            snapshot: spec.snapshot,
            snapshot_receipt_id,
            cancellation_scope_id: spec.cancellation_scope_id,
            cancellation_generation: spec.cancellation_generation,
            state: AttemptState::Created,
            receipt_id: None,
            created_at_ms: spec.registered_at_ms,
            updated_at_ms: spec.registered_at_ms,
        };
        insert_attempt(&transaction, &record, spec.idempotency_key)?;
        transaction.commit()?;
        Ok(AttemptRegistrationDecision::Created(handle_of(&record)))
    }

    /// Runs the linearizable `CommitPermit` CAS (`[TASK-COMMIT-001]`).
    ///
    /// A new permit is issued only when no permit is outstanding. The
    /// attempt's snapshot binding must match the current `TaskHead`
    /// bit-for-bit (commit seq, effect-history root, retry-fence epoch);
    /// otherwise the attempt is durably `Conflicted`. If another attempt
    /// holds the outstanding permit, the requester is durably `Superseded`
    /// with the winner's identity. If cancellation committed first
    /// (`[TASK-CANCEL-003]`), no permit is issued: the attempt closes
    /// pre-permit with a `CANCELLED_BEFORE_EFFECT` closure receipt and the
    /// `TaskHead` stays unchanged. Same key + same bytes replays the
    /// original permit; same key + different bytes fails closed.
    ///
    /// # Errors
    ///
    /// Returns a not-found, generation, idempotency-conflict, or storage
    /// error.
    // By-value request mirrors every other mutating API here; the lint only
    // fires because `planned_effects: Vec<_>` ended `Copy`.
    #[allow(clippy::needless_pass_by_value)]
    #[deprecated(
        since = "0.1.0",
        note = "use request_commit_permit_with_authorities_struct with Authorities::default() for the legacy unbound behavior"
    )]
    pub fn request_commit_permit(
        &self,
        request: PermitRequest,
    ) -> Result<PermitDecision, TaskStoreError> {
        self.request_commit_permit_inner(None, None, None, None, None, request, None)
    }

    /// Runs the `CommitPermit` CAS with an immutable binding to a live
    /// `TaskAuthority` lease. The lease is checked in the same `SQLite`
    /// transaction that freezes the permit; legacy callers remain unbound.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::request_commit_permit`], plus a
    /// typed lease conflict when the supplied holder, term, epoch, token, or
    /// expiry is not the current durable lease.
    #[allow(clippy::needless_pass_by_value)]
    pub fn request_commit_permit_with_authority_lease(
        &self,
        request: AuthorityLeasePermitRequest,
    ) -> Result<PermitDecision, TaskStoreError> {
        self.request_commit_permit_inner(
            None,
            None,
            None,
            None,
            None,
            request.permit,
            Some(request.lease),
        )
    }

    /// Runs the `CommitPermit` CAS after re-reading every Artifact head named
    /// by an authority-sealed `TaskWriteSet` from its owning
    /// [`nlos_artifact::ArtifactStore`]. Each declared write must still point
    /// at the same current head revision and the immediately following target
    /// revision before the permit can freeze the participant set.
    ///
    /// This is an opt-in strengthening of the legacy permit entry point: it
    /// does not stage or publish bytes, and it does not invent an Artifact
    /// publication receipt. A permit replay returns the existing durable
    /// decision without repeating owner reads.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::request_commit_permit`], plus a
    /// typed Artifact owner or write-binding conflict when a sealed Artifact
    /// head has advanced or disappeared.
    #[allow(clippy::needless_pass_by_value)]
    #[deprecated(
        since = "0.1.0",
        note = "use request_commit_permit_with_authorities_struct with Authorities { artifact: Some(..), .. }"
    )]
    pub fn request_commit_permit_with_artifact_authority(
        &self,
        artifact_authority: &nlos_artifact::ArtifactStore,
        request: PermitRequest,
    ) -> Result<PermitDecision, TaskStoreError> {
        self.request_commit_permit_inner(
            Some(artifact_authority),
            None,
            None,
            None,
            None,
            request,
            None,
        )
    }

    /// Runs the `CommitPermit` CAS after re-reading an optional Process /
    /// `AgentInstance` / `IsolationDomain` binding from its owning
    /// [`nlos_process::ProcessAuthority`]. The complete owner binding and its
    /// endpoint proof must still match the sealed `TaskWriteSet` before the
    /// participant registry can be frozen.
    ///
    /// This is an opt-in strengthening of the legacy permit entry point: it
    /// does not spawn, rotate, or otherwise mutate Process authority state. A
    /// permit replay returns the existing durable decision without repeating
    /// owner reads.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::request_commit_permit`], plus a
    /// typed Process owner or write-binding conflict when the sealed binding
    /// is no longer current.
    #[allow(clippy::needless_pass_by_value)]
    #[deprecated(
        since = "0.1.0",
        note = "use request_commit_permit_with_authorities_struct with Authorities { process: Some(..), .. }"
    )]
    pub fn request_commit_permit_with_process_authority(
        &self,
        process_authority: &nlos_process::ProcessAuthority,
        request: PermitRequest,
    ) -> Result<PermitDecision, TaskStoreError> {
        self.request_commit_permit_inner(
            None,
            Some(process_authority),
            None,
            None,
            None,
            request,
            None,
        )
    }

    /// Runs the `CommitPermit` CAS after re-reading every Resource reservation
    /// named by an authority-sealed `TaskWriteSet` from its owning
    /// [`nlos_resource::ResourceAuthority`]. The owner must still report the
    /// same RESERVED call/operation/quote, Driver generation/fence, device and
    /// upper-bound bytes before the permit can freeze the participant set.
    ///
    /// This is an opt-in strengthening of the legacy permit entry point: it
    /// does not activate or consume a reservation, and it does not invent a
    /// Resource publication/finalization receipt. A permit replay returns the
    /// existing durable decision without repeating owner reads.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::request_commit_permit`], plus a
    /// typed Resource owner or reservation-binding conflict when a sealed
    /// reservation has disappeared, activated, rotated, or changed.
    #[allow(clippy::needless_pass_by_value)]
    #[deprecated(
        since = "0.1.0",
        note = "use request_commit_permit_with_authorities_struct with Authorities { resource: Some(..), .. }"
    )]
    pub fn request_commit_permit_with_resource_authority(
        &self,
        resource_authority: &nlos_resource::ResourceAuthority,
        request: PermitRequest,
    ) -> Result<PermitDecision, TaskStoreError> {
        self.request_commit_permit_inner(
            None,
            None,
            Some(resource_authority),
            None,
            None,
            request,
            None,
        )
    }

    /// Runs the `CommitPermit` CAS after re-reading every Operation endpoint
    /// named by a sealed `TaskWriteSet` from its owning Operation authority.
    /// A stale or changed owner proof blocks participant-registry freezing.
    ///
    /// This is an opt-in strengthening of the legacy permit entry point: it
    /// does not dispatch or transition the Operation itself.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::request_commit_permit`], plus a
    /// typed Operation owner or endpoint-proof conflict.
    #[allow(clippy::needless_pass_by_value)]
    #[deprecated(
        since = "0.1.0",
        note = "use request_commit_permit_with_authorities_struct with Authorities { operation: Some(..), .. }"
    )]
    pub fn request_commit_permit_with_operation_authority(
        &self,
        operation_authority: &nlos_store::SqliteOperationStore,
        request: PermitRequest,
    ) -> Result<PermitDecision, TaskStoreError> {
        self.request_commit_permit_inner(
            None,
            None,
            None,
            Some(operation_authority),
            None,
            request,
            None,
        )
    }

    /// Runs the `CommitPermit` CAS after re-reading every Channel endpoint
    /// named by a sealed `TaskWriteSet` from its owning Channel authority.
    /// The proof is re-read at the CURRENT generation, so a rotation between
    /// seal and permit naturally breaks the byte comparison and blocks
    /// participant-registry freezing (stale fence).
    ///
    /// This is an opt-in strengthening of the legacy permit entry point: it
    /// does not dispatch or transition the Channel itself.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::request_commit_permit`], plus a
    /// typed Channel owner or endpoint-proof conflict.
    #[allow(clippy::needless_pass_by_value)]
    #[deprecated(
        since = "0.1.0",
        note = "use request_commit_permit_with_authorities_struct with Authorities { channel: Some(..), .. }"
    )]
    pub fn request_commit_permit_with_channel_authority(
        &self,
        channel_authority: &nlos_channel::ChannelAuthority,
        request: PermitRequest,
    ) -> Result<PermitDecision, TaskStoreError> {
        self.request_commit_permit_inner(
            None,
            None,
            None,
            None,
            Some(channel_authority),
            request,
            None,
        )
    }

    /// Runs the `CommitPermit` CAS with Artifact, Process, Resource, and
    /// Operation owner revalidation enabled together. Semantic append
    /// revalidation remains on the dedicated Semantic-aware finalize path.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::request_commit_permit`], plus typed
    /// Artifact, Process, Resource, or Operation owner-proof conflicts.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::needless_pass_by_value)]
    #[deprecated(
        since = "0.1.0",
        note = "use request_commit_permit_with_authorities_struct with Authorities { artifact/process/resource/operation: Some(..) }"
    )]
    pub fn request_commit_permit_with_authorities_and_operation_authority(
        &self,
        artifact_authority: &nlos_artifact::ArtifactStore,
        process_authority: &nlos_process::ProcessAuthority,
        resource_authority: &nlos_resource::ResourceAuthority,
        operation_authority: &nlos_store::SqliteOperationStore,
        request: PermitRequest,
    ) -> Result<PermitDecision, TaskStoreError> {
        self.request_commit_permit_inner(
            Some(artifact_authority),
            Some(process_authority),
            Some(resource_authority),
            Some(operation_authority),
            None,
            request,
            None,
        )
    }

    /// Runs the `CommitPermit` CAS with the owner revalidations named by
    /// one [`Authorities`] bundle (Artifact, Process, Resource, Operation,
    /// and Channel together). The bundle is destructured into the exact
    /// inner authority slots used by the ladder permit entries; in
    /// particular `authorities.semantic` is not consumed here — Semantic
    /// append revalidation remains on the dedicated Semantic-aware
    /// finalize path — and the authority-lease binding stays on
    /// [`Self::request_commit_permit_with_authority_lease`].
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::request_commit_permit`], plus
    /// typed Artifact, Process, Resource, Operation, or Channel
    /// owner-proof conflicts.
    #[allow(clippy::needless_pass_by_value)]
    pub fn request_commit_permit_with_authorities_struct(
        &self,
        authorities: Authorities<'_>,
        request: PermitRequest,
    ) -> Result<PermitDecision, TaskStoreError> {
        self.request_commit_permit_inner(
            authorities.artifact,
            authorities.process,
            authorities.resource,
            authorities.operation,
            authorities.channel,
            request,
            None,
        )
    }

    #[allow(clippy::needless_pass_by_value)]
    #[allow(clippy::too_many_arguments)]
    fn request_commit_permit_inner(
        &self,
        artifact_authority: Option<&nlos_artifact::ArtifactStore>,
        process_authority: Option<&nlos_process::ProcessAuthority>,
        resource_authority: Option<&nlos_resource::ResourceAuthority>,
        operation_authority: Option<&nlos_store::SqliteOperationStore>,
        channel_authority: Option<&nlos_channel::ChannelAuthority>,
        request: PermitRequest,
        authority_lease: Option<AuthorityLeaseRecord>,
    ) -> Result<PermitDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = load_task(&transaction, request.task_id)?;
        if let Some(existing) =
            load_permit_by_key(&transaction, request.task_id, request.idempotency_key)?
        {
            let decision = replay_permit(&transaction, existing, &request, authority_lease)?;
            transaction.commit()?;
            return Ok(decision);
        }
        let attempt = load_attempt(&transaction, request.task_id, request.attempt_id)?;
        if attempt.attempt_generation != request.attempt_generation {
            return Err(TaskStoreError::InvalidGeneration);
        }
        if task.record.cancel_epoch > 0 {
            let decision = close_attempt_for_cancel(
                &transaction,
                &task.record,
                &attempt,
                request.requested_at_ms,
            )?;
            transaction.commit()?;
            return Ok(decision);
        }
        let decision = compete_for_permit(
            &transaction,
            &task,
            &attempt,
            &request,
            artifact_authority,
            process_authority,
            resource_authority,
            operation_authority,
            channel_authority,
            authority_lease,
        )?;
        transaction.commit()?;
        Ok(decision)
    }

    /// Commits a Task cancellation (`[TASK-CANCEL-002]`).
    ///
    /// The first cancellation atomically increments `cancel_epoch` before
    /// anything else, blocking all later permit issuance. Every open
    /// pre-permit attempt closes with a `CANCELLED_BEFORE_EFFECT` closure
    /// receipt and the `TaskHead` stays unchanged. An already-issued permit
    /// is NOT cleared (`[TASK-COMMIT-003]`); its holder may still finalize
    /// (permit-first linearization; effect-level fencing is deferred to the
    /// `EffectPermit` slice). Replaying the same key returns the original
    /// decision without re-incrementing; a different key after cancellation
    /// observes `AlreadyCancelled`.
    ///
    /// # Errors
    ///
    /// Returns a not-found, epoch-exhaustion, or storage error.
    pub fn cancel_task(&self, request: CancelRequest) -> Result<CancelDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = load_task(&transaction, request.task_id)?;
        if let Some(cancel) = load_cancel(&transaction, request.task_id)? {
            let decision = if cancel.idempotency_key == request.idempotency_key {
                CancelDecision::Replayed {
                    cancel_epoch: cancel.cancel_epoch_after,
                }
            } else {
                CancelDecision::AlreadyCancelled {
                    cancel_epoch: task.record.cancel_epoch,
                }
            };
            transaction.commit()?;
            return Ok(decision);
        }
        let cancel_epoch = task
            .record
            .cancel_epoch
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?;
        let control_epoch = task
            .record
            .control_epoch
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?;
        let mut closed_attempts = Vec::new();
        for attempt in list_open_attempts(&transaction, request.task_id)? {
            let receipt_id =
                derive_closure_receipt_id(request.task_id, attempt.attempt_id, cancel_epoch);
            insert_receipt(
                &transaction,
                &closure_receipt(&task.record, &attempt, receipt_id, request.requested_at_ms),
            )?;
            set_attempt_state(
                &transaction,
                &attempt,
                AttemptState::Cancelled,
                Some(receipt_id),
                request.requested_at_ms,
            )?;
            closed_attempts.push(ClosedAttempt {
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                receipt_id,
            });
        }
        update_task(&transaction, &task, request.requested_at_ms, |record| {
            record.cancel_epoch = cancel_epoch;
            record.control_epoch = control_epoch;
            record.state = TaskState::Cancelled;
        })?;
        insert_cancel(
            &transaction,
            request.task_id,
            request.idempotency_key,
            cancel_epoch,
            request.requested_at_ms,
        )?;
        transaction.commit()?;
        Ok(CancelDecision::Applied {
            cancel_epoch,
            closed_attempts,
        })
    }

    /// Reads the durable head/control view of a Task, including the
    /// currently outstanding permit (if any).
    ///
    /// # Errors
    ///
    /// Returns `TaskNotFound` or a storage error.
    pub fn inspect_task(&self, task_id: TaskId) -> Result<TaskRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        let mut stored = load_task(&*connection, task_id)?;
        stored.record.active_permit =
            load_outstanding_permit(&*connection, task_id)?.map(|permit| permit.permit_id);
        Ok(stored.record)
    }

    /// Reads the current durable participant registry for a Task.
    ///
    /// # Errors
    ///
    /// Returns `TaskNotFound`, `ParticipantRegistryNotFound`, corruption, or
    /// storage errors.
    pub fn inspect_participant_registry(
        &self,
        task_id: TaskId,
    ) -> Result<crate::ParticipantRegistryRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        let task = load_task(&*connection, task_id)?;
        crate::participant::inspect_registry(&connection, &task.record)
    }

    /// Prepares the local participant registry for a lease takeover.
    ///
    /// The current registry generation/root is CAS-frozen as
    /// `FROZEN_FOR_TAKEOVER` under a newly validated live lease, and the
    /// Task control epoch advances in the same transaction. New permit,
    /// effect, adoption, and reconcile mutations are then rejected until a
    /// future Assignment/TakeoverReceipt path installs a successor registry.
    /// Repeating the exact fence after the state is already frozen is a
    /// read-only replay. This API is intentionally only a local pre-gate; it
    /// does not create a cross-authority barrier receipt or activate a new
    /// assignment.
    ///
    /// # Errors
    ///
    /// Returns a task, lease, registry binding, CAS, or storage error.
    #[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
    pub fn prepare_authority_takeover_fence(
        &self,
        request: AuthorityLeaseTakeoverFenceRequest,
    ) -> Result<crate::ParticipantRegistryRecord, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = load_task(&transaction, request.task_id)?;
        validate_authority_lease_binding_in_transaction(
            &transaction,
            request.lease.binding(),
            request.requested_at_ms,
        )?;
        let assignment = load_current_assignment(&transaction, request.task_id)?;
        if let Some(assignment) = assignment.as_ref() {
            if assignment.task_generation != task.record.task_generation {
                return Err(TaskStoreError::CorruptRecord(
                    "assignment task generation mismatch",
                ));
            }
            if assignment.participant_registry_binding != request.expected_registry_binding {
                return Err(TaskStoreError::AuthorityLeaseBindingMismatch);
            }
            if assignment.state == AuthorityAssignmentState::Fenced {
                return Err(TaskStoreError::AuthorityLeaseFenced);
            }
        }
        let before = crate::participant::inspect_registry(&transaction, &task.record)?;
        let registry = crate::participant::freeze_for_takeover(
            &transaction,
            &task.record,
            request.expected_registry_binding,
            request.requested_at_ms,
        )?;
        let (fence_members, outstanding_operation_participant_root, exact_fence_set_root) =
            match load_outstanding_operation_participants(&transaction, request.task_id)? {
                Some(outstanding_participants) => {
                    let fence_members = crate::participant::takeover_fence_members(
                        &registry,
                        &outstanding_participants,
                    )?;
                    let (outstanding_root, exact_root) = crate::participant::takeover_fence_roots(
                        &registry,
                        &outstanding_participants,
                    )?;
                    (
                        Some(fence_members),
                        Some(outstanding_root),
                        Some(exact_root),
                    )
                }
                None => (None, None, None),
            };
        let control_epoch = if before.state == crate::ParticipantRegistryState::FrozenForTakeover {
            task.record.control_epoch
        } else {
            let next = task
                .record
                .control_epoch
                .checked_add(1)
                .ok_or(TaskStoreError::EpochExhausted)?;
            update_task(
                &transaction,
                &task,
                request.requested_at_ms,
                |task_record| {
                    task_record.control_epoch = next;
                },
            )?;
            next
        };
        let mut record = AuthorityLeaseTakeoverFenceRecord {
            receipt_id: derive_takeover_fence_receipt_id(
                request.task_id,
                task.record.task_generation,
                request.expected_registry_binding,
                request.lease.binding(),
            ),
            task_id: request.task_id,
            task_generation: task.record.task_generation,
            frozen_registry_binding: request.expected_registry_binding,
            authority_lease_binding: request.lease.binding(),
            control_epoch,
            exact_fence_set_root,
            outstanding_operation_participant_root,
            created_at_ms: request.requested_at_ms,
        };
        if let Some(existing) = load_takeover_fence_receipt(
            &transaction,
            request.task_id,
            request.expected_registry_binding,
        )? {
            if existing.authority_lease_binding != record.authority_lease_binding
                || existing.control_epoch != record.control_epoch
            {
                return Err(TaskStoreError::AuthorityLeaseBindingMismatch);
            }
            if existing
                .exact_fence_set_root
                .is_some_and(|root| Some(root) != record.exact_fence_set_root)
                || existing
                    .outstanding_operation_participant_root
                    .is_some_and(|root| Some(root) != record.outstanding_operation_participant_root)
            {
                return Err(TaskStoreError::CorruptRecord(
                    "takeover fence root changed during replay",
                ));
            }
            record = existing;
        } else {
            insert_takeover_fence_receipt(&transaction, &record)?;
        }
        if let Some(fence_members) = fence_members.as_deref() {
            persist_takeover_fence_members(&transaction, &record, fence_members)?;
        }
        if let Some(assignment) = assignment {
            persist_takeover_pending_receipt(
                &transaction,
                &task.record,
                assignment,
                &record,
                request.requested_at_ms,
            )?;
        }
        transaction.commit()?;
        Ok(registry)
    }

    /// Reads the immutable local takeover-fence receipt for one frozen
    /// registry generation/root.
    ///
    /// The receipt is only a local pre-gate observation. When the durable
    /// write set exposes a complete participant mapping, its roots cover the
    /// frozen registry union with locally durable outstanding-operation
    /// participants; otherwise the roots remain `None`. Remote endpoint
    /// barrier receipts and successor assignment activation remain outside
    /// this API.
    ///
    /// # Errors
    ///
    /// Returns `ReceiptNotFound` or a storage/corruption error.
    pub fn inspect_authority_takeover_fence_receipt(
        &self,
        task_id: TaskId,
        registry_binding: crate::ParticipantRegistryBinding,
    ) -> Result<AuthorityLeaseTakeoverFenceRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_takeover_fence_receipt(&*connection, task_id, registry_binding)?
            .ok_or(TaskStoreError::ReceiptNotFound)
    }

    /// Reads the durable exact local fence-set member manifest for one frozen
    /// registry. An empty list means the fence roots were intentionally left
    /// unknown in this schema version, not that the set is empty.
    ///
    /// # Errors
    ///
    /// Returns `ReceiptNotFound` or a storage/corruption error.
    pub fn inspect_authority_takeover_fence_members(
        &self,
        task_id: TaskId,
        registry_binding: crate::ParticipantRegistryBinding,
    ) -> Result<Vec<AuthorityTakeoverFenceMemberRecord>, TaskStoreError> {
        let connection = self.lock_connection()?;
        let fence = load_takeover_fence_receipt(&*connection, task_id, registry_binding)?
            .ok_or(TaskStoreError::ReceiptNotFound)?;
        let members = load_takeover_fence_members(&*connection, fence.receipt_id)?;
        match fence.exact_fence_set_root {
            Some(_) if members.is_empty() => Err(TaskStoreError::CorruptRecord(
                "takeover fence member manifest missing",
            )),
            Some(root) => {
                validate_takeover_fence_manifest(
                    &members,
                    fence.receipt_id,
                    fence.task_id,
                    fence.task_generation,
                    root,
                )?;
                Ok(members)
            }
            None if !members.is_empty() => Err(TaskStoreError::CorruptRecord(
                "takeover fence member manifest unexpected",
            )),
            None => Ok(members),
        }
    }

    /// Reads the pending-or-complete local prefix of a
    /// `TaskAuthorityTakeoverReceipt`.
    ///
    /// The returned record proves that the old local assignment and the
    /// local frozen fence were durably linked. While the takeover is
    /// unresolved, `new_assignment_id` is `None` and `barrier_state` is
    /// `Pending`; after [`SqliteTaskAuthority::complete_authority_takeover`]
    /// the receipt carries `Complete` and the activated successor
    /// assignment identity.
    ///
    /// # Errors
    ///
    /// Returns `ReceiptNotFound` or a storage/corruption error.
    pub fn inspect_authority_takeover_receipt(
        &self,
        task_id: TaskId,
        fence_receipt_id: ReceiptId,
    ) -> Result<AuthorityTakeoverReceiptRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_takeover_receipt(&*connection, task_id, fence_receipt_id)?
            .ok_or(TaskStoreError::ReceiptNotFound)
    }

    /// Records one endpoint's immutable takeover-barrier observation.
    ///
    /// The endpoint must already be present in the frozen registry and the
    /// pending takeover receipt must carry an exact local fence-set root.
    /// This method stores the supplied remote receipt identity/digest for
    /// replay and audit only; it does not verify remote signatures, mark the
    /// parent receipt complete, or activate a successor assignment.
    ///
    /// # Errors
    ///
    /// Returns `ReceiptNotFound`, a participant/root binding error, a
    /// conflicting replay, or a storage/corruption error.
    #[allow(clippy::needless_pass_by_value)]
    pub fn record_authority_takeover_barrier_receipt(
        &self,
        request: AuthorityTakeoverBarrierReceiptRequest,
    ) -> Result<AuthorityTakeoverBarrierReceiptRecord, TaskStoreError> {
        if request.observed_at_ms < 0 {
            return Err(TaskStoreError::CorruptRecord("takeover barrier timestamp"));
        }
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let core = validate_barrier_observation(
            &transaction,
            request.takeover_receipt_id,
            &request.participant,
        )?;
        let record = AuthorityTakeoverBarrierReceiptRecord {
            receipt_id: derive_takeover_barrier_receipt_id(
                request.takeover_receipt_id,
                request.participant,
                request.remote_receipt_id,
                request.barrier_digest,
                core.fence_set_root,
            ),
            takeover_receipt_id: request.takeover_receipt_id,
            task_id: core.takeover.task_id,
            task_generation: core.takeover.task_generation,
            participant: request.participant,
            remote_receipt_id: request.remote_receipt_id,
            barrier_digest: Some(request.barrier_digest),
            fence_set_root: core.fence_set_root,
            state: crate::lease::AuthorityTakeoverBarrierReceiptState::Observed,
            observed_at_ms: request.observed_at_ms,
            signer: None,
        };
        if let Some(existing) = load_takeover_barrier_receipt_by_participant(
            &transaction,
            request.takeover_receipt_id,
            request.participant,
        )? {
            if existing != record {
                return Err(TaskStoreError::CorruptRecord(
                    "takeover barrier receipt changed during replay",
                ));
            }
            transaction.commit()?;
            return Ok(existing);
        }
        insert_takeover_barrier_receipt(&transaction, &record)?;
        transaction.commit()?;
        Ok(record)
    }

    /// Records one endpoint's immutable takeover-barrier observation with an
    /// NLOS principal Ed25519 signature verified by the `nlos-identity` key
    /// authority.
    ///
    /// Verification covers the principal/control-domain/key binding, the
    /// `BarrierObservationSigning` key purpose, key validity and revocation
    /// state at `observed_at_ms`, and a strict Ed25519 signature over the
    /// domain-separated observation message digest (see
    /// [`barrier_observation_signature_message`]). The durable signer
    /// columns are filled from the verified authority proof, never from
    /// caller assertions. Unsigned observations remain recordable for
    /// same-trust-domain local use; coverage semantics are unchanged and
    /// this method does not mark the parent takeover receipt complete or
    /// activate a successor assignment. Replaying the exact signed
    /// observation returns the stored record; mixing signed and unsigned
    /// forms of the same observation fails closed.
    ///
    /// # Errors
    ///
    /// Returns `ReceiptNotFound`, a participant/root binding error, the
    /// wrapped [`nlos_identity::IdentityAuthorityError`] from signature
    /// verification, a conflicting replay, or a storage/corruption error.
    #[allow(clippy::needless_pass_by_value)]
    pub fn record_authority_takeover_barrier_receipt_signed(
        &self,
        identity: &nlos_identity::IdentityAuthority,
        request: AuthorityTakeoverBarrierReceiptRequest,
        signature: BarrierObservationSignature,
    ) -> Result<AuthorityTakeoverBarrierReceiptRecord, TaskStoreError> {
        if request.observed_at_ms < 0 {
            return Err(TaskStoreError::CorruptRecord("takeover barrier timestamp"));
        }
        let verified_at_ms = u64::try_from(request.observed_at_ms)
            .map_err(|_| TaskStoreError::CorruptRecord("takeover barrier timestamp"))?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let core = validate_barrier_observation(
            &transaction,
            request.takeover_receipt_id,
            &request.participant,
        )?;
        let message_digest = barrier_observation_signature_message(
            request.takeover_receipt_id,
            &request.participant,
            request.remote_receipt_id,
            request.barrier_digest,
            core.fence_set_root,
        );
        let proof = identity
            .verify_barrier_observation_signature(
                nlos_identity::VerifyBarrierObservationSignatureRequest {
                    message_digest,
                    issuer: signature.issuer,
                    control_domain_id: signature.control_domain_id,
                    key_id: signature.key_id,
                    signature: signature.signature,
                    verified_at_ms,
                },
            )
            .map_err(TaskStoreError::BarrierSignerIdentityAuthority)?;
        let record = AuthorityTakeoverBarrierReceiptRecord {
            receipt_id: derive_takeover_barrier_receipt_id(
                request.takeover_receipt_id,
                request.participant,
                request.remote_receipt_id,
                request.barrier_digest,
                core.fence_set_root,
            ),
            takeover_receipt_id: request.takeover_receipt_id,
            task_id: core.takeover.task_id,
            task_generation: core.takeover.task_generation,
            participant: request.participant,
            remote_receipt_id: request.remote_receipt_id,
            barrier_digest: Some(request.barrier_digest),
            fence_set_root: core.fence_set_root,
            state: crate::lease::AuthorityTakeoverBarrierReceiptState::Observed,
            observed_at_ms: request.observed_at_ms,
            signer: Some(AuthorityTakeoverBarrierSigner {
                principal_id: proof.principal_id(),
                control_domain_id: proof.control_domain_id(),
                key_id: proof.key_id(),
                key_generation: proof.key_generation(),
                signature: signature.signature,
            }),
        };
        if let Some(existing) = load_takeover_barrier_receipt_by_participant(
            &transaction,
            request.takeover_receipt_id,
            request.participant,
        )? {
            if existing != record {
                return Err(TaskStoreError::CorruptRecord(
                    "takeover barrier receipt changed during replay",
                ));
            }
            transaction.commit()?;
            return Ok(existing);
        }
        insert_takeover_barrier_receipt(&transaction, &record)?;
        transaction.commit()?;
        Ok(record)
    }

    /// Completes a pending takeover receipt and activates the new-term
    /// successor assignment in one `BEGIN IMMEDIATE` transaction.
    ///
    /// What this proves: the full exact-fence manifest is covered by
    /// immutable local observations (coverage recomputed inline against the
    /// frozen member manifest), every observation carries a
    /// `nlos-identity`-verified principal signer (the v36 gate; any unsigned
    /// observation fails closed with
    /// [`TaskStoreError::BarrierObservationUnsigned`]), and the successor
    /// lease binding is still the live durable lease at completion time.
    /// On success the receipt moves `Pending → Complete` with the successor
    /// assignment identity filled (the only transition the schema-v37
    /// narrowed trigger permits), the old assignment is CAS-fenced
    /// (`TakeoverPending → Fenced`), and the successor assignment row
    /// becomes `Active`.
    ///
    /// What this does NOT prove: remote barrier truth beyond signature
    /// validity — a signature proves the signer endorsed the observation
    /// material, not that the remote barrier physically completed; registry
    /// re-opening (the registry stays `FrozenForTakeover` — successor-term
    /// registry generation and cross-term permit issuance are the next
    /// slice); and any IPC peer authentication.
    ///
    /// Replaying the completion of an already-complete receipt re-reads the
    /// durable state and returns the byte-equal record without further
    /// mutation; a completed receipt whose successor assignment is missing
    /// or not `Active` is corruption.
    ///
    /// # Errors
    ///
    /// Returns `ReceiptNotFound`, `AuthorityLeaseBindingMismatch` when the
    /// request lease differs from the receipt's immutable successor-term
    /// binding, `AuthorityLeaseExpired`/`AuthorityLeaseFenced` when that
    /// lease is no longer live at `completed_at_ms`,
    /// `BarrierObservationUnsigned` when any observation lacks a signer,
    /// `CorruptRecord` for partial coverage, a missing fence manifest, an
    /// incomplete exact-fence root, or binding drift, or a storage error.
    #[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
    pub fn complete_authority_takeover(
        &self,
        request: CompleteAuthorityTakeoverRequest,
    ) -> Result<AuthorityTakeoverCompletionRecord, TaskStoreError> {
        if request.completed_at_ms < 0 {
            return Err(TaskStoreError::CorruptRecord(
                "takeover completion timestamp",
            ));
        }
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let receipt = load_takeover_receipt_by_id(&transaction, request.takeover_receipt_id)?
            .ok_or(TaskStoreError::ReceiptNotFound)?;
        if request.lease.binding() != receipt.new_authority_lease_binding {
            return Err(TaskStoreError::AuthorityLeaseBindingMismatch);
        }
        let successor_assignment_id = derive_assignment_id(
            receipt.task_id,
            receipt.task_generation,
            receipt.new_authority_lease_binding.authority_id,
            receipt.new_authority_lease_binding.term,
            receipt.frozen_registry_binding,
        );
        if receipt.barrier_state == AuthorityTakeoverReceiptState::Complete {
            return complete_takeover_replay(&transaction, &receipt, successor_assignment_id);
        }
        if receipt.new_assignment_id.is_some() {
            return Err(TaskStoreError::CorruptRecord(
                "takeover receipt is not pending",
            ));
        }
        validate_authority_lease_binding_in_transaction(
            &transaction,
            receipt.new_authority_lease_binding,
            request.completed_at_ms,
        )?;
        let task = load_task(&transaction, receipt.task_id)?;
        if task.record.task_generation != receipt.task_generation {
            return Err(TaskStoreError::CorruptRecord(
                "takeover completion task generation",
            ));
        }
        let registry = crate::participant::inspect_registry(&transaction, &task.record)?;
        if registry.generation != receipt.frozen_registry_binding.generation
            || registry.root != receipt.frozen_registry_binding.root
            || registry.state != crate::ParticipantRegistryState::FrozenForTakeover
        {
            return Err(TaskStoreError::ParticipantRegistryBindingMismatch);
        }
        validate_takeover_completion_coverage(&transaction, &receipt)?;
        let completion = AuthorityTakeoverCompletionRecord {
            takeover_receipt_id: receipt.receipt_id,
            task_id: receipt.task_id,
            old_assignment_id: receipt.old_assignment_id,
            new_assignment_id: successor_assignment_id,
            barrier_state: AuthorityTakeoverReceiptState::Complete,
            completed_at_ms: request.completed_at_ms,
        };
        let changed = transaction.execute(
            "UPDATE task_authority_takeover_receipts
             SET barrier_state = ?1, new_assignment_id = ?2
             WHERE receipt_id = ?3
               AND barrier_state = ?4 AND new_assignment_id IS NULL",
            params![
                AuthorityTakeoverReceiptState::Complete.code(),
                successor_assignment_id.as_bytes().as_slice(),
                receipt.receipt_id.as_bytes().as_slice(),
                AuthorityTakeoverReceiptState::Pending.code(),
            ],
        )?;
        if changed != 1 {
            return Err(TaskStoreError::CorruptRecord(
                "takeover receipt is not pending",
            ));
        }
        // A CAS miss here means the old assignment already left
        // `TakeoverPending` (for example another receipt completed first),
        // matching the fenced outcome of the neighbouring CAS helpers.
        let changed = transaction.execute(
            "UPDATE task_authority_assignments
             SET assignment_state = ?1, updated_at_ms = ?2
             WHERE assignment_id = ?3 AND assignment_state = ?4",
            params![
                AuthorityAssignmentState::Fenced.code(),
                request.completed_at_ms,
                receipt.old_assignment_id.as_bytes().as_slice(),
                AuthorityAssignmentState::TakeoverPending.code(),
            ],
        )?;
        if changed != 1 {
            return Err(TaskStoreError::AuthorityLeaseFenced);
        }
        insert_assignment(
            &transaction,
            &AuthorityAssignmentRecord {
                assignment_id: successor_assignment_id,
                task_id: receipt.task_id,
                task_generation: receipt.task_generation,
                authority_lease_binding: receipt.new_authority_lease_binding,
                control_epoch: receipt.new_control_epoch,
                participant_registry_binding: receipt.frozen_registry_binding,
                state: AuthorityAssignmentState::Active,
                created_at_ms: request.completed_at_ms,
                updated_at_ms: request.completed_at_ms,
            },
        )?;
        transaction.commit()?;
        Ok(completion)
    }

    /// Reopens the successor-term participant registry after a completed
    /// takeover and rotates the active assignment to the new registry
    /// generation in one `BEGIN IMMEDIATE` transaction.
    ///
    /// The frozen participant tuples are copied into a successor registry
    /// generation with a new root. The assignment created by takeover
    /// completion is fenced, and a new active assignment bound to that root
    /// is installed. This is the local hand-off required before a new
    /// lease-bound `CommitPermit` can be issued; it does not re-attest remote
    /// endpoints or adopt an old quarantined permit.
    ///
    /// Replaying after the successor registry has already been created is
    /// read-only and returns the same identity projection, even when a later
    /// permit has moved the registry from `Open` to `FrozenForPermit`.
    ///
    /// # Errors
    ///
    /// Returns `ReceiptNotFound`, a completion/lease/assignment/registry
    /// binding error, a timestamp or epoch error, or a storage error.
    #[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
    pub fn reopen_successor_registry(
        &self,
        request: AuthoritySuccessorRegistryReopenRequest,
    ) -> Result<AuthoritySuccessorRegistryReopenRecord, TaskStoreError> {
        if request.reopened_at_ms < 0 {
            return Err(TaskStoreError::CorruptRecord(
                "successor registry timestamp",
            ));
        }
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let takeover = load_takeover_receipt_by_id(&transaction, request.takeover_receipt_id)?
            .ok_or(TaskStoreError::ReceiptNotFound)?;
        if request.lease.binding() != takeover.new_authority_lease_binding {
            return Err(TaskStoreError::AuthorityLeaseBindingMismatch);
        }
        if takeover.barrier_state != AuthorityTakeoverReceiptState::Complete {
            return Err(TaskStoreError::CorruptRecord(
                "takeover receipt is not complete",
            ));
        }
        let old_registry_binding = takeover.frozen_registry_binding;
        let fenced_assignment_id =
            takeover
                .new_assignment_id
                .ok_or(TaskStoreError::CorruptRecord(
                    "completed takeover lacks successor assignment",
                ))?;
        let task = load_task(&transaction, takeover.task_id)?;
        if task.record.task_generation != takeover.task_generation {
            return Err(TaskStoreError::CorruptRecord(
                "successor registry task generation",
            ));
        }
        let registry = crate::participant::inspect_registry(&transaction, &task.record)?;

        // A previously committed reopen has enough durable identity to replay
        // without revalidating the now possibly expired lease. Exact lease
        // bytes were already checked above, which prevents another term from
        // manufacturing a replay for this takeover receipt.
        if registry.generation > old_registry_binding.generation {
            if registry.prior_root != old_registry_binding.root
                || !matches!(
                    registry.state,
                    crate::ParticipantRegistryState::Open
                        | crate::ParticipantRegistryState::FrozenForPermit
                )
            {
                return Err(TaskStoreError::CorruptRecord(
                    "successor registry replay binding",
                ));
            }
            let old_assignment =
                load_assignment_by_id(&transaction, takeover.task_id, fenced_assignment_id)?
                    .ok_or(TaskStoreError::CorruptRecord(
                        "successor registry replay old assignment",
                    ))?;
            if old_assignment.state != AuthorityAssignmentState::Fenced
                || old_assignment.authority_lease_binding != takeover.new_authority_lease_binding
                || old_assignment.participant_registry_binding != old_registry_binding
            {
                return Err(TaskStoreError::CorruptRecord(
                    "successor registry replay old assignment",
                ));
            }
            let active = load_current_assignment(&transaction, takeover.task_id)?.ok_or(
                TaskStoreError::CorruptRecord("successor registry replay active assignment"),
            )?;
            if active.state != AuthorityAssignmentState::Active
                || active.authority_lease_binding != takeover.new_authority_lease_binding
                || active.participant_registry_binding
                    != (crate::ParticipantRegistryBinding {
                        generation: registry.generation,
                        root: registry.root,
                    })
            {
                return Err(TaskStoreError::CorruptRecord(
                    "successor registry replay active assignment",
                ));
            }
            let record = AuthoritySuccessorRegistryReopenRecord {
                takeover_receipt_id: takeover.receipt_id,
                task_id: takeover.task_id,
                old_registry_binding,
                successor_registry_binding: crate::ParticipantRegistryBinding {
                    generation: registry.generation,
                    root: registry.root,
                },
                fenced_assignment_id,
                active_assignment_id: active.assignment_id,
            };
            transaction.commit()?;
            return Ok(record);
        }

        if registry.generation != old_registry_binding.generation
            || registry.root != old_registry_binding.root
            || registry.state != crate::ParticipantRegistryState::FrozenForTakeover
        {
            return Err(TaskStoreError::ParticipantRegistryBindingMismatch);
        }
        let old_assignment =
            load_assignment_by_id(&transaction, takeover.task_id, fenced_assignment_id)?.ok_or(
                TaskStoreError::CorruptRecord("successor registry assignment missing"),
            )?;
        if old_assignment.state != AuthorityAssignmentState::Active
            || old_assignment.authority_lease_binding != takeover.new_authority_lease_binding
            || old_assignment.participant_registry_binding != old_registry_binding
        {
            return Err(TaskStoreError::AuthorityLeaseFenced);
        }
        if request.reopened_at_ms < old_assignment.updated_at_ms
            || request.reopened_at_ms < task.record.updated_at_ms
        {
            return Err(TaskStoreError::CorruptRecord(
                "successor registry timestamp",
            ));
        }
        validate_authority_lease_binding_in_transaction(
            &transaction,
            takeover.new_authority_lease_binding,
            request.reopened_at_ms,
        )?;
        let next_control_epoch = task
            .record
            .control_epoch
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?;
        let successor_registry = crate::participant::reopen_after_takeover(
            &transaction,
            &task.record,
            old_registry_binding,
            request.reopened_at_ms,
        )?;
        let successor_registry_binding = crate::ParticipantRegistryBinding {
            generation: successor_registry.generation,
            root: successor_registry.root,
        };
        let active_assignment_id = derive_assignment_id(
            task.record.task_id,
            task.record.task_generation,
            request.lease.authority_id,
            request.lease.term,
            successor_registry_binding,
        );
        if active_assignment_id == fenced_assignment_id {
            return Err(TaskStoreError::CorruptRecord(
                "successor assignment identity did not advance",
            ));
        }
        mark_assignment_fenced(&transaction, &old_assignment, request.reopened_at_ms)?;
        insert_assignment(
            &transaction,
            &AuthorityAssignmentRecord {
                assignment_id: active_assignment_id,
                task_id: task.record.task_id,
                task_generation: task.record.task_generation,
                authority_lease_binding: takeover.new_authority_lease_binding,
                control_epoch: next_control_epoch,
                participant_registry_binding: successor_registry_binding,
                state: AuthorityAssignmentState::Active,
                created_at_ms: request.reopened_at_ms,
                updated_at_ms: request.reopened_at_ms,
            },
        )?;
        update_task(&transaction, &task, request.reopened_at_ms, |record| {
            record.control_epoch = next_control_epoch;
        })?;
        let result = AuthoritySuccessorRegistryReopenRecord {
            takeover_receipt_id: takeover.receipt_id,
            task_id: takeover.task_id,
            old_registry_binding,
            successor_registry_binding,
            fenced_assignment_id,
            active_assignment_id,
        };
        transaction.commit()?;
        Ok(result)
    }

    /// Reads all immutable local endpoint-barrier observations for a pending
    /// takeover receipt. An empty list is valid and does not imply completion.
    ///
    /// # Errors
    ///
    /// Returns `ReceiptNotFound` or a storage/corruption error.
    pub fn inspect_authority_takeover_barrier_receipts(
        &self,
        takeover_receipt_id: ReceiptId,
    ) -> Result<Vec<AuthorityTakeoverBarrierReceiptRecord>, TaskStoreError> {
        let connection = self.lock_connection()?;
        if load_takeover_receipt_by_id(&*connection, takeover_receipt_id)?.is_none() {
            return Err(TaskStoreError::ReceiptNotFound);
        }
        load_takeover_barrier_receipts(&*connection, takeover_receipt_id)
    }

    /// Computes a read-only local coverage view for a pending takeover.
    ///
    /// `LocallyCovered` means every canonical fence member has one immutable
    /// `Observed` row with the same exact root. It is deliberately not a
    /// remote barrier proof and does not mutate the parent receipt or the
    /// assignment.
    ///
    /// # Errors
    ///
    /// Returns `ReceiptNotFound` or a storage/corruption error.
    pub fn inspect_authority_takeover_barrier_coverage(
        &self,
        takeover_receipt_id: ReceiptId,
    ) -> Result<AuthorityTakeoverBarrierCoverage, TaskStoreError> {
        let connection = self.lock_connection()?;
        let takeover = load_takeover_receipt_by_id(&*connection, takeover_receipt_id)?
            .ok_or(TaskStoreError::ReceiptNotFound)?;
        if takeover.barrier_state != AuthorityTakeoverReceiptState::Pending
            || takeover.new_assignment_id.is_some()
        {
            return Err(TaskStoreError::CorruptRecord(
                "takeover receipt is not pending",
            ));
        }
        let members = load_takeover_fence_members(&*connection, takeover.fence_receipt_id)?;
        let observations = load_takeover_barrier_receipts(&*connection, takeover_receipt_id)?;
        let Some(fence_set_root) = takeover.exact_fence_set_root else {
            return Ok(AuthorityTakeoverBarrierCoverage {
                takeover_receipt_id,
                fence_set_root: None,
                state: AuthorityTakeoverBarrierCoverageState::ManifestUnavailable,
                expected_member_count: 0,
                observed_member_count: observations.len(),
                missing_participants: Vec::new(),
            });
        };
        if members.is_empty() {
            return Ok(AuthorityTakeoverBarrierCoverage {
                takeover_receipt_id,
                fence_set_root: Some(fence_set_root),
                state: AuthorityTakeoverBarrierCoverageState::ManifestUnavailable,
                expected_member_count: 0,
                observed_member_count: observations.len(),
                missing_participants: Vec::new(),
            });
        }
        let expected = validate_takeover_fence_manifest(
            &members,
            takeover.fence_receipt_id,
            takeover.task_id,
            takeover.task_generation,
            fence_set_root,
        )?;
        for observation in &observations {
            if observation.takeover_receipt_id != takeover_receipt_id
                || observation.task_id != takeover.task_id
                || observation.task_generation != takeover.task_generation
                || observation.fence_set_root != fence_set_root
                || observation.state != crate::lease::AuthorityTakeoverBarrierReceiptState::Observed
                || !expected.contains(&observation.participant)
            {
                return Err(TaskStoreError::CorruptRecord(
                    "takeover barrier observation binding",
                ));
            }
        }
        let missing_participants = expected
            .iter()
            .copied()
            .filter(|participant| {
                !observations
                    .iter()
                    .any(|observation| observation.participant == *participant)
            })
            .collect::<Vec<_>>();
        let state = if missing_participants.is_empty() {
            AuthorityTakeoverBarrierCoverageState::LocallyCovered
        } else {
            AuthorityTakeoverBarrierCoverageState::Partial
        };
        Ok(AuthorityTakeoverBarrierCoverage {
            takeover_receipt_id,
            fence_set_root: Some(fence_set_root),
            state,
            expected_member_count: expected.len(),
            observed_member_count: observations.len(),
            missing_participants,
        })
    }

    /// Reads the latest durable local `TaskAuthority` assignment, if one has
    /// been established by a lease-bound permit path.
    ///
    /// # Errors
    ///
    /// Returns `ReceiptNotFound` when no assignment exists, or a storage /
    /// corruption error.
    pub fn inspect_authority_assignment(
        &self,
        task_id: TaskId,
    ) -> Result<AuthorityAssignmentRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_current_assignment(&*connection, task_id)?.ok_or(TaskStoreError::ReceiptNotFound)
    }

    /// Registers an Artifact head after direct proof readback from its owner.
    ///
    /// The caller supplies only the stable Artifact identity and the Task
    /// registry position it observed; it cannot inject participant identity,
    /// generation, or Receipt bytes.
    ///
    /// # Errors
    ///
    /// Returns typed Artifact proof, task, registry CAS/freeze/bound, or
    /// storage errors. No Task mutation occurs when proof readback fails.
    pub fn register_artifact_head_participant(
        &self,
        artifact_authority: &nlos_artifact::ArtifactStore,
        task_id: TaskId,
        expected: crate::ParticipantRegistryBinding,
        artifact_id: nlos_types::ArtifactId,
        registered_at_ms: i64,
    ) -> Result<crate::ParticipantRegistrationDecision, TaskStoreError> {
        let proof = artifact_authority
            .inspect_head_endpoint_proof(artifact_id)
            .map_err(TaskStoreError::ArtifactParticipantAuthority)?;
        let participant = crate::ParticipantRecord {
            participant_type: crate::ParticipantType::ArtifactHead,
            participant_id: proof.participant_id,
            participant_generation: proof.participant_generation,
            admission_receipt_id: proof.admission_receipt_id,
        };
        self.register_verified_participant(task_id, expected, participant, registered_at_ms)
    }

    /// Registers the owning Semantic admission endpoint after direct proof
    /// readback. No caller-supplied endpoint tuple is accepted.
    ///
    /// # Errors
    ///
    /// Returns typed Semantic proof, task, registry CAS/freeze/bound, or
    /// storage errors. No Task mutation occurs when proof readback fails.
    pub fn register_semantic_admission_participant(
        &self,
        semantic_authority: &nlos_semantic::SemanticAuthority,
        task_id: TaskId,
        expected: crate::ParticipantRegistryBinding,
        registered_at_ms: i64,
    ) -> Result<crate::ParticipantRegistrationDecision, TaskStoreError> {
        let proof = semantic_authority
            .inspect_admission_endpoint_proof()
            .map_err(TaskStoreError::SemanticParticipantAuthority)?;
        let participant = crate::ParticipantRecord {
            participant_type: crate::ParticipantType::SemanticAdmission,
            participant_id: proof.participant_id,
            participant_generation: proof.participant_generation,
            admission_receipt_id: proof.admission_receipt_id,
        };
        self.register_verified_participant(task_id, expected, participant, registered_at_ms)
    }

    /// Registers the current Process binding after direct `ProcessAuthority`
    /// endpoint-proof readback.
    ///
    /// # Errors
    ///
    /// Returns typed Process proof, task, registry CAS/freeze, bound, or
    /// storage errors. No Task mutation occurs when proof readback fails.
    #[allow(clippy::too_many_arguments)]
    pub fn register_process_binding_participant(
        &self,
        process_authority: &nlos_process::ProcessAuthority,
        task_id: TaskId,
        expected: crate::ParticipantRegistryBinding,
        attempt_id: nlos_types::TaskAttemptId,
        expected_attempt_generation: nlos_types::Generation,
        process_id: nlos_types::ProcessId,
        expected_process_generation: nlos_types::Generation,
        registered_at_ms: i64,
    ) -> Result<crate::ParticipantRegistrationDecision, TaskStoreError> {
        let owner = process_authority
            .inspect_active_process_binding(process_id)
            .map_err(TaskStoreError::ProcessParticipantAuthority)?;
        if owner.task_id != task_id
            || owner.task_attempt_id != attempt_id
            || owner.attempt_generation != expected_attempt_generation
        {
            return Err(TaskStoreError::TaskWriteSetConflict {
                reason: "Process endpoint is for a different TaskAttempt",
            });
        }
        let proof = process_authority
            .inspect_binding_endpoint_proof(process_id)
            .map_err(TaskStoreError::ProcessParticipantAuthority)?;
        if proof.participant_generation != expected_process_generation {
            return Err(TaskStoreError::ParticipantEndpointGenerationMismatch {
                expected: expected_process_generation.get(),
                current: proof.participant_generation.get(),
            });
        }
        let participant = crate::ParticipantRecord {
            participant_type: crate::ParticipantType::ProcessBinding,
            participant_id: proof.participant_id,
            participant_generation: proof.participant_generation,
            admission_receipt_id: proof.admission_receipt_id,
        };
        self.register_verified_participant(task_id, expected, participant, registered_at_ms)
    }

    /// Registers the current Operation endpoint after exact owner proof
    /// readback. The caller cannot provide the Operation participant tuple;
    /// it is derived by `SqliteOperationStore` from the durable registration
    /// row and generation.
    ///
    /// # Errors
    ///
    /// Returns typed Operation proof, generation, task, registry CAS/freeze,
    /// bound, or storage errors. No Task mutation occurs on proof mismatch.
    pub fn register_operation_binding_participant(
        &self,
        operation_authority: &nlos_store::SqliteOperationStore,
        task_id: TaskId,
        expected: crate::ParticipantRegistryBinding,
        operation_id: OperationId,
        expected_operation_generation: Generation,
        registered_at_ms: i64,
    ) -> Result<crate::ParticipantRegistrationDecision, TaskStoreError> {
        let proof = operation_authority
            .inspect_endpoint_proof(OperationHandle {
                operation_id,
                generation: expected_operation_generation,
            })
            .map_err(TaskStoreError::OperationParticipantAuthority)?;
        if proof.operation.operation_id != operation_id
            || proof.operation.generation != expected_operation_generation
            || proof.participant_generation != expected_operation_generation
        {
            return Err(TaskStoreError::ParticipantEndpointGenerationMismatch {
                expected: expected_operation_generation.get(),
                current: proof.participant_generation.get(),
            });
        }
        let participant = crate::ParticipantRecord {
            participant_type: crate::ParticipantType::OperationBinding,
            participant_id: proof.participant_id,
            participant_generation: proof.participant_generation,
            admission_receipt_id: proof.admission_receipt_id,
        };
        self.register_verified_participant(task_id, expected, participant, registered_at_ms)
    }

    /// Registers the current Channel endpoint after exact owner proof
    /// readback. The caller cannot provide the Channel participant tuple; it
    /// is derived by `ChannelAuthority` from the durable current generation.
    ///
    /// # Errors
    ///
    /// Returns typed Channel proof, generation, task, registry CAS/freeze,
    /// bound, or storage errors. No Task mutation occurs on proof mismatch.
    pub fn register_channel_participant(
        &self,
        channel_authority: &nlos_channel::ChannelAuthority,
        task_id: TaskId,
        expected: crate::ParticipantRegistryBinding,
        channel_id: nlos_types::ChannelId,
        expected_channel_generation: Generation,
        registered_at_ms: i64,
    ) -> Result<crate::ParticipantRegistrationDecision, TaskStoreError> {
        let proof = channel_authority
            .inspect_endpoint_proof(channel_id)
            .map_err(TaskStoreError::ChannelParticipantAuthority)?;
        if proof.participant_generation != expected_channel_generation {
            return Err(TaskStoreError::ParticipantEndpointGenerationMismatch {
                expected: expected_channel_generation.get(),
                current: proof.participant_generation.get(),
            });
        }
        let participant = crate::ParticipantRecord {
            participant_type: crate::ParticipantType::ChannelTopic,
            participant_id: proof.participant_id,
            participant_generation: proof.participant_generation,
            admission_receipt_id: proof.admission_receipt_id,
        };
        self.register_verified_participant(task_id, expected, participant, registered_at_ms)
    }

    /// Registers the current Driver gateway generation after direct
    /// `ResourceAuthority` readback and an exact planned-generation check.
    ///
    /// # Errors
    ///
    /// Returns typed Resource proof, generation, task, registry CAS/freeze,
    /// bound, or storage errors. No Task mutation occurs on proof mismatch.
    pub fn register_driver_gateway_participant(
        &self,
        resource_authority: &nlos_resource::ResourceAuthority,
        task_id: TaskId,
        expected: crate::ParticipantRegistryBinding,
        driver_id: nlos_types::DriverId,
        expected_driver_generation: nlos_types::Generation,
        registered_at_ms: i64,
    ) -> Result<crate::ParticipantRegistrationDecision, TaskStoreError> {
        let proof = resource_authority
            .inspect_driver_gateway_endpoint_proof(driver_id)
            .map_err(TaskStoreError::ResourceParticipantAuthority)?;
        if proof.participant_generation != expected_driver_generation {
            return Err(TaskStoreError::ParticipantEndpointGenerationMismatch {
                expected: expected_driver_generation.get(),
                current: proof.participant_generation.get(),
            });
        }
        let participant = crate::ParticipantRecord {
            participant_type: crate::ParticipantType::DriverGateway,
            participant_id: proof.participant_id,
            participant_generation: proof.participant_generation,
            admission_receipt_id: proof.admission_receipt_id,
        };
        self.register_verified_participant(task_id, expected, participant, registered_at_ms)
    }

    /// Registers a Resource/Ledger account endpoint after direct owner
    /// readback and an exact planned-generation check.
    ///
    /// # Errors
    ///
    /// Returns typed Resource proof, generation, task, registry CAS/freeze,
    /// bound, or storage errors. No Task mutation occurs on proof mismatch.
    pub fn register_resource_ledger_participant(
        &self,
        resource_authority: &nlos_resource::ResourceAuthority,
        task_id: TaskId,
        expected: crate::ParticipantRegistryBinding,
        account_id: nlos_types::ResourceAccountId,
        expected_account_generation: nlos_types::Generation,
        registered_at_ms: i64,
    ) -> Result<crate::ParticipantRegistrationDecision, TaskStoreError> {
        let proof = resource_authority
            .inspect_resource_ledger_endpoint_proof(account_id)
            .map_err(TaskStoreError::ResourceParticipantAuthority)?;
        if proof.participant_generation != expected_account_generation {
            return Err(TaskStoreError::ParticipantEndpointGenerationMismatch {
                expected: expected_account_generation.get(),
                current: proof.participant_generation.get(),
            });
        }
        let participant = crate::ParticipantRecord {
            participant_type: crate::ParticipantType::ResourceLedger,
            participant_id: proof.participant_id,
            participant_generation: proof.participant_generation,
            admission_receipt_id: proof.admission_receipt_id,
        };
        self.register_verified_participant(task_id, expected, participant, registered_at_ms)
    }

    fn register_verified_participant(
        &self,
        task_id: TaskId,
        expected: crate::ParticipantRegistryBinding,
        participant: crate::ParticipantRecord,
        registered_at_ms: i64,
    ) -> Result<crate::ParticipantRegistrationDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = load_task(&transaction, task_id)?;
        let decision = crate::participant::register_verified_participant(
            &transaction,
            &task.record,
            expected,
            participant,
            registered_at_ms,
        )?;
        transaction.commit()?;
        Ok(decision)
    }

    /// Reads the durable view of one `TaskAttempt`.
    ///
    /// # Errors
    ///
    /// Returns `AttemptNotFound` or a storage error.
    pub fn inspect_attempt(
        &self,
        task_id: TaskId,
        attempt_id: TaskAttemptId,
    ) -> Result<AttemptRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_attempt(&*connection, task_id, attempt_id)
    }

    /// Reads the durable view of one `CommitPermit`.
    ///
    /// # Errors
    ///
    /// Returns `PermitNotFound` or a storage error.
    pub fn inspect_permit(
        &self,
        task_id: TaskId,
        permit_id: CommitPermitId,
    ) -> Result<PermitRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_permit_by_id(&*connection, task_id, permit_id)
    }

    /// Reads one durable task receipt (commit or closure).
    ///
    /// # Errors
    ///
    /// Returns `ReceiptNotFound` or a storage error.
    pub fn inspect_receipt(
        &self,
        task_id: TaskId,
        receipt_id: ReceiptId,
    ) -> Result<TaskReceiptRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_receipt(&*connection, task_id, receipt_id)
    }

    pub(crate) fn lock_connection(&self) -> Result<MutexGuard<'_, Connection>, TaskStoreError> {
        self.connection
            .lock()
            .map_err(|_| TaskStoreError::LockPoisoned)
    }
}

fn validate_takeover_fence_manifest(
    members: &[AuthorityTakeoverFenceMemberRecord],
    fence_receipt_id: ReceiptId,
    task_id: TaskId,
    task_generation: Generation,
    expected_root: [u8; 32],
) -> Result<Vec<crate::ParticipantRecord>, TaskStoreError> {
    if members.iter().any(|member| {
        member.fence_receipt_id != fence_receipt_id
            || member.task_id != task_id
            || member.task_generation != task_generation
    }) {
        return Err(TaskStoreError::CorruptRecord(
            "takeover fence member binding",
        ));
    }
    let participants = members
        .iter()
        .map(|member| member.participant)
        .collect::<Vec<_>>();
    let actual_root = crate::participant::takeover_fence_set_root(&participants)?;
    if actual_root != expected_root {
        return Err(TaskStoreError::CorruptRecord("takeover fence member root"));
    }
    Ok(participants)
}

/// Shared takeover-barrier validation core: pending receipt state, exact
/// fence-set root resolution, frozen registry binding, and participant
/// membership in the canonical manifest. Both the unsigned and signed
/// observation paths must pass this core before any durable write.
struct BarrierObservationCore {
    takeover: AuthorityTakeoverReceiptRecord,
    fence_set_root: [u8; 32],
}

fn validate_barrier_observation(
    transaction: &Transaction<'_>,
    takeover_receipt_id: ReceiptId,
    participant: &crate::ParticipantRecord,
) -> Result<BarrierObservationCore, TaskStoreError> {
    let takeover = load_takeover_receipt_by_id(transaction, takeover_receipt_id)?
        .ok_or(TaskStoreError::ReceiptNotFound)?;
    if takeover.barrier_state != AuthorityTakeoverReceiptState::Pending
        || takeover.new_assignment_id.is_some()
    {
        return Err(TaskStoreError::CorruptRecord(
            "takeover receipt is not pending",
        ));
    }
    let fence_set_root = takeover
        .exact_fence_set_root
        .ok_or(TaskStoreError::CorruptRecord(
            "takeover fence set root is incomplete",
        ))?;
    let task = load_task(transaction, takeover.task_id)?;
    if task.record.task_generation != takeover.task_generation {
        return Err(TaskStoreError::CorruptRecord(
            "takeover barrier task generation",
        ));
    }
    let registry = crate::participant::inspect_registry(transaction, &task.record)?;
    if registry.generation != takeover.frozen_registry_binding.generation
        || registry.root != takeover.frozen_registry_binding.root
    {
        return Err(TaskStoreError::ParticipantRegistryBindingMismatch);
    }
    if registry.state != crate::ParticipantRegistryState::FrozenForTakeover {
        return Err(TaskStoreError::ParticipantRegistryBindingMismatch);
    }
    let fence_members = load_takeover_fence_members(transaction, takeover.fence_receipt_id)?;
    let manifest_participants = if fence_members.is_empty() {
        Vec::new()
    } else {
        validate_takeover_fence_manifest(
            &fence_members,
            takeover.fence_receipt_id,
            takeover.task_id,
            takeover.task_generation,
            fence_set_root,
        )?
    };
    if fence_members.is_empty() || !manifest_participants.contains(participant) {
        return Err(TaskStoreError::ParticipantRegistryBindingMismatch);
    }
    Ok(BarrierObservationCore {
        takeover,
        fence_set_root,
    })
}

/// Replay short-circuit for an already-completed takeover receipt: the
/// decision is durable, so the successor assignment is only re-read and
/// the byte-equal completion record returned. `completed_at_ms` reads back
/// from the successor assignment's `created_at_ms` so the record is
/// restart-stable regardless of the replay request's timestamp.
fn complete_takeover_replay(
    transaction: &Transaction<'_>,
    receipt: &AuthorityTakeoverReceiptRecord,
    successor_assignment_id: TaskAuthorityAssignmentId,
) -> Result<AuthorityTakeoverCompletionRecord, TaskStoreError> {
    let new_assignment_id = receipt
        .new_assignment_id
        .ok_or(TaskStoreError::CorruptRecord(
            "takeover completion assignment",
        ))?;
    if new_assignment_id != successor_assignment_id {
        return Err(TaskStoreError::CorruptRecord(
            "takeover completion assignment",
        ));
    }
    let original_assignment =
        load_assignment_by_id(transaction, receipt.task_id, new_assignment_id)?.ok_or(
            TaskStoreError::CorruptRecord("takeover completion assignment"),
        )?;
    let assignment_is_active = original_assignment.state == AuthorityAssignmentState::Active;
    let assignment_was_rotated = if original_assignment.state == AuthorityAssignmentState::Fenced {
        let task = load_task(transaction, receipt.task_id)?;
        let registry = crate::participant::inspect_registry(transaction, &task.record)?;
        let current = load_current_assignment(transaction, receipt.task_id)?.ok_or(
            TaskStoreError::CorruptRecord("takeover completion successor assignment"),
        )?;
        registry.prior_root == receipt.frozen_registry_binding.root
            && registry.generation > receipt.frozen_registry_binding.generation
            && matches!(
                registry.state,
                crate::ParticipantRegistryState::Open
                    | crate::ParticipantRegistryState::FrozenForPermit
            )
            && current.state == AuthorityAssignmentState::Active
            && current.authority_lease_binding == receipt.new_authority_lease_binding
            && current.participant_registry_binding.generation == registry.generation
            && current.participant_registry_binding.root == registry.root
    } else {
        false
    };
    if (!assignment_is_active && !assignment_was_rotated)
        || original_assignment.authority_lease_binding != receipt.new_authority_lease_binding
        || original_assignment.participant_registry_binding != receipt.frozen_registry_binding
    {
        return Err(TaskStoreError::CorruptRecord(
            "takeover completion assignment",
        ));
    }
    Ok(AuthorityTakeoverCompletionRecord {
        takeover_receipt_id: receipt.receipt_id,
        task_id: receipt.task_id,
        old_assignment_id: receipt.old_assignment_id,
        new_assignment_id,
        barrier_state: AuthorityTakeoverReceiptState::Complete,
        completed_at_ms: original_assignment.created_at_ms,
    })
}

/// Inline barrier-coverage recompute for the completion transaction.
/// Mirrors the per-observation binding checks of
/// `inspect_authority_takeover_barrier_coverage` (which stays read-only and
/// Pending-only) and adds the completion gates the coverage view does not
/// carry: a resolvable exact-fence root, a non-empty validated manifest,
/// every observation principal-signed (v36 signer columns), and zero
/// missing manifest members.
fn validate_takeover_completion_coverage(
    transaction: &Transaction<'_>,
    receipt: &AuthorityTakeoverReceiptRecord,
) -> Result<(), TaskStoreError> {
    let fence_set_root = receipt
        .exact_fence_set_root
        .ok_or(TaskStoreError::CorruptRecord(
            "takeover fence set root is incomplete",
        ))?;
    let members = load_takeover_fence_members(transaction, receipt.fence_receipt_id)?;
    if members.is_empty() {
        return Err(TaskStoreError::CorruptRecord(
            "takeover fence member manifest missing",
        ));
    }
    let expected = validate_takeover_fence_manifest(
        &members,
        receipt.fence_receipt_id,
        receipt.task_id,
        receipt.task_generation,
        fence_set_root,
    )?;
    let observations = load_takeover_barrier_receipts(transaction, receipt.receipt_id)?;
    for observation in &observations {
        if observation.takeover_receipt_id != receipt.receipt_id
            || observation.task_id != receipt.task_id
            || observation.task_generation != receipt.task_generation
            || observation.fence_set_root != fence_set_root
            || observation.state != crate::lease::AuthorityTakeoverBarrierReceiptState::Observed
            || !expected.contains(&observation.participant)
        {
            return Err(TaskStoreError::CorruptRecord(
                "takeover barrier observation binding",
            ));
        }
        if observation.signer.is_none() {
            return Err(TaskStoreError::BarrierObservationUnsigned);
        }
    }
    let fully_covered = expected.iter().all(|participant| {
        observations
            .iter()
            .any(|observation| observation.participant == *participant)
    });
    if !fully_covered {
        return Err(TaskStoreError::CorruptRecord(
            "takeover barrier coverage is partial",
        ));
    }
    Ok(())
}

fn replay_permit(
    transaction: &Transaction<'_>,
    existing: PermitRecord,
    request: &PermitRequest,
    authority_lease: Option<AuthorityLeaseRecord>,
) -> Result<PermitDecision, TaskStoreError> {
    let stored_root = crate::effect::stored_effect_set_root(transaction, existing.permit_id)?
        .unwrap_or_else(crate::effect::empty_effect_set_root);
    let same_bytes = existing.attempt_id == request.attempt_id
        && existing.attempt_generation == request.attempt_generation
        && existing.write_set_root == request.write_set_root
        && existing.valid_until_ms == request.valid_until_ms
        && stored_root == crate::effect::effect_set_root_of(&request.planned_effects)
        && existing.authority_lease_binding == authority_lease.map(AuthorityLeaseRecord::binding);
    if same_bytes {
        Ok(PermitDecision::Replayed(Box::new(existing)))
    } else {
        Err(TaskStoreError::IdempotencyConflict)
    }
}

fn close_attempt_for_cancel(
    transaction: &Transaction<'_>,
    task: &TaskRecord,
    attempt: &AttemptRecord,
    now_ms: i64,
) -> Result<PermitDecision, TaskStoreError> {
    if attempt.state == AttemptState::Cancelled {
        let receipt_id = attempt.receipt_id.ok_or(TaskStoreError::CorruptRecord(
            "cancelled attempt lacks closure receipt",
        ))?;
        return Ok(PermitDecision::CancelledBeforeEffect { receipt_id });
    }
    if !attempt.state.is_open_candidate() {
        return Err(TaskStoreError::InvalidAttemptState {
            state: attempt.state,
        });
    }
    let receipt_id = derive_closure_receipt_id(task.task_id, attempt.attempt_id, task.cancel_epoch);
    insert_receipt(
        transaction,
        &closure_receipt(task, attempt, receipt_id, now_ms),
    )?;
    set_attempt_state(
        transaction,
        attempt,
        AttemptState::Cancelled,
        Some(receipt_id),
        now_ms,
    )?;
    Ok(PermitDecision::CancelledBeforeEffect { receipt_id })
}

/// Reads every owner proof named by the planned effect endpoint request. The
/// returned participant tuples are checked against the same OPEN registry
/// later in the seal transaction; this helper never admits a participant by
/// itself.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn resolve_effect_endpoints(
    artifact_authority: &nlos_artifact::ArtifactStore,
    process_authority: Option<&nlos_process::ProcessAuthority>,
    semantic_authority: Option<&nlos_semantic::SemanticAuthority>,
    resource_authority: Option<&nlos_resource::ResourceAuthority>,
    operation_authority: Option<&nlos_store::SqliteOperationStore>,
    channel_authority: Option<&nlos_channel::ChannelAuthority>,
    requests: &[TaskWriteSetEffectEndpointRequest],
    effect_count: usize,
    task: &TaskRecord,
    attempt_id: TaskAttemptId,
    attempt_generation: Generation,
) -> Result<
    (
        Vec<TaskWriteSetEffectEndpoint>,
        Vec<crate::ParticipantRecord>,
    ),
    TaskStoreError,
> {
    let effect_count = u64::try_from(effect_count).map_err(|_| TaskStoreError::EpochExhausted)?;
    let mut seen = std::collections::BTreeSet::new();
    let mut endpoints = Vec::with_capacity(requests.len());
    let mut participants = Vec::with_capacity(requests.len());
    for request in requests {
        if request.effect_seq() >= effect_count {
            return Err(TaskStoreError::TaskWriteSetConflict {
                reason: "planned effect endpoint references an unknown effect sequence",
            });
        }
        let (kind, object_id) = match *request {
            TaskWriteSetEffectEndpointRequest::ArtifactHead { artifact_id, .. } => (
                TaskWriteSetEffectEndpointKind::ArtifactHead,
                artifact_id.into_bytes(),
            ),
            TaskWriteSetEffectEndpointRequest::SemanticAdmission { .. } => {
                (TaskWriteSetEffectEndpointKind::SemanticAdmission, [0; 16])
            }
            TaskWriteSetEffectEndpointRequest::ProcessBinding { process_id, .. } => (
                TaskWriteSetEffectEndpointKind::ProcessBinding,
                process_id.into_bytes(),
            ),
            TaskWriteSetEffectEndpointRequest::DriverGateway { driver_id, .. } => (
                TaskWriteSetEffectEndpointKind::DriverGateway,
                driver_id.into_bytes(),
            ),
            TaskWriteSetEffectEndpointRequest::ResourceLedger { account_id, .. } => (
                TaskWriteSetEffectEndpointKind::ResourceLedger,
                account_id.into_bytes(),
            ),
            TaskWriteSetEffectEndpointRequest::OperationBinding { operation_id, .. } => (
                TaskWriteSetEffectEndpointKind::OperationBinding,
                operation_id.into_bytes(),
            ),
            TaskWriteSetEffectEndpointRequest::ChannelTopicBinding { channel_id, .. } => (
                TaskWriteSetEffectEndpointKind::ChannelTopicBinding,
                channel_id.into_bytes(),
            ),
        };
        let key = (request.effect_seq(), kind.code(), object_id);
        if !seen.insert(key) {
            return Err(TaskStoreError::TaskWriteSetConflict {
                reason: "planned effect endpoint is duplicated",
            });
        }

        let (participant_id, participant_generation, admission_receipt_id) = match *request {
            TaskWriteSetEffectEndpointRequest::ArtifactHead { artifact_id, .. } => {
                let proof = artifact_authority
                    .inspect_head_endpoint_proof(artifact_id)
                    .map_err(TaskStoreError::ArtifactParticipantAuthority)?;
                (
                    proof.participant_id,
                    proof.participant_generation,
                    proof.admission_receipt_id,
                )
            }
            TaskWriteSetEffectEndpointRequest::SemanticAdmission { .. } => {
                let authority = semantic_authority.ok_or(TaskStoreError::TaskWriteSetConflict {
                    reason: "Semantic effect endpoint requires SemanticAuthority readback",
                })?;
                let proof = authority
                    .inspect_admission_endpoint_proof()
                    .map_err(TaskStoreError::SemanticParticipantAuthority)?;
                (
                    proof.participant_id,
                    proof.participant_generation,
                    proof.admission_receipt_id,
                )
            }
            TaskWriteSetEffectEndpointRequest::ProcessBinding {
                process_id,
                expected_process_generation,
                ..
            } => {
                let authority = process_authority.ok_or(TaskStoreError::TaskWriteSetConflict {
                    reason: "Process effect endpoint requires ProcessAuthority readback",
                })?;
                let owner = authority
                    .inspect_active_process_binding(process_id)
                    .map_err(TaskStoreError::ProcessParticipantAuthority)?;
                if owner.task_id != task.task_id
                    || owner.task_attempt_id != attempt_id
                    || owner.attempt_generation != attempt_generation
                    || owner.process_generation != expected_process_generation
                {
                    return Err(TaskStoreError::TaskWriteSetConflict {
                        reason: "Process effect endpoint is not bound to this TaskAttempt/generation",
                    });
                }
                let proof = authority
                    .inspect_binding_endpoint_proof(process_id)
                    .map_err(TaskStoreError::ProcessParticipantAuthority)?;
                (
                    proof.participant_id,
                    proof.participant_generation,
                    proof.admission_receipt_id,
                )
            }
            TaskWriteSetEffectEndpointRequest::DriverGateway {
                driver_id,
                expected_driver_generation,
                ..
            } => {
                let authority = resource_authority.ok_or(TaskStoreError::TaskWriteSetConflict {
                    reason: "Driver effect endpoint requires ResourceAuthority readback",
                })?;
                let proof = authority
                    .inspect_driver_gateway_endpoint_proof(driver_id)
                    .map_err(TaskStoreError::ResourceParticipantAuthority)?;
                if proof.participant_generation != expected_driver_generation {
                    return Err(TaskStoreError::ParticipantEndpointGenerationMismatch {
                        expected: expected_driver_generation.get(),
                        current: proof.participant_generation.get(),
                    });
                }
                (
                    proof.participant_id,
                    proof.participant_generation,
                    proof.admission_receipt_id,
                )
            }
            TaskWriteSetEffectEndpointRequest::ResourceLedger {
                account_id,
                expected_account_generation,
                ..
            } => {
                let authority = resource_authority.ok_or(TaskStoreError::TaskWriteSetConflict {
                    reason: "Resource effect endpoint requires ResourceAuthority readback",
                })?;
                let proof = authority
                    .inspect_resource_ledger_endpoint_proof(account_id)
                    .map_err(TaskStoreError::ResourceParticipantAuthority)?;
                if proof.participant_generation != expected_account_generation {
                    return Err(TaskStoreError::ParticipantEndpointGenerationMismatch {
                        expected: expected_account_generation.get(),
                        current: proof.participant_generation.get(),
                    });
                }
                (
                    proof.participant_id,
                    proof.participant_generation,
                    proof.admission_receipt_id,
                )
            }
            TaskWriteSetEffectEndpointRequest::OperationBinding {
                operation_id,
                expected_operation_generation,
                ..
            } => {
                let authority =
                    operation_authority.ok_or(TaskStoreError::TaskWriteSetConflict {
                        reason: "Operation effect endpoint requires OperationAuthority readback",
                    })?;
                let proof = authority
                    .inspect_endpoint_proof(OperationHandle {
                        operation_id,
                        generation: expected_operation_generation,
                    })
                    .map_err(TaskStoreError::OperationParticipantAuthority)?;
                if proof.operation.operation_id != operation_id
                    || proof.operation.generation != expected_operation_generation
                    || proof.participant_generation != expected_operation_generation
                {
                    return Err(TaskStoreError::ParticipantEndpointGenerationMismatch {
                        expected: expected_operation_generation.get(),
                        current: proof.participant_generation.get(),
                    });
                }
                (
                    proof.participant_id,
                    proof.participant_generation,
                    proof.admission_receipt_id,
                )
            }
            TaskWriteSetEffectEndpointRequest::ChannelTopicBinding {
                channel_id,
                expected_channel_generation,
                ..
            } => {
                let authority = channel_authority.ok_or(TaskStoreError::TaskWriteSetConflict {
                    reason: "Channel effect endpoint requires ChannelAuthority readback",
                })?;
                let proof = authority
                    .inspect_endpoint_proof(channel_id)
                    .map_err(TaskStoreError::ChannelParticipantAuthority)?;
                if proof.channel_id != channel_id
                    || proof.participant_generation != expected_channel_generation
                {
                    return Err(TaskStoreError::ParticipantEndpointGenerationMismatch {
                        expected: expected_channel_generation.get(),
                        current: proof.participant_generation.get(),
                    });
                }
                (
                    proof.participant_id,
                    proof.participant_generation,
                    proof.admission_receipt_id,
                )
            }
        };
        endpoints.push(TaskWriteSetEffectEndpoint {
            effect_seq: request.effect_seq(),
            kind,
            object_id,
            participant_id,
            participant_generation,
            admission_receipt_id,
        });
        participants.push(crate::ParticipantRecord {
            participant_type: match kind {
                TaskWriteSetEffectEndpointKind::ArtifactHead => {
                    crate::ParticipantType::ArtifactHead
                }
                TaskWriteSetEffectEndpointKind::SemanticAdmission => {
                    crate::ParticipantType::SemanticAdmission
                }
                TaskWriteSetEffectEndpointKind::ProcessBinding => {
                    crate::ParticipantType::ProcessBinding
                }
                TaskWriteSetEffectEndpointKind::DriverGateway => {
                    crate::ParticipantType::DriverGateway
                }
                TaskWriteSetEffectEndpointKind::ResourceLedger => {
                    crate::ParticipantType::ResourceLedger
                }
                TaskWriteSetEffectEndpointKind::OperationBinding => {
                    crate::ParticipantType::OperationBinding
                }
                TaskWriteSetEffectEndpointKind::ChannelTopicBinding => {
                    crate::ParticipantType::ChannelTopic
                }
            },
            participant_id,
            participant_generation,
            admission_receipt_id,
        });
    }
    endpoints.sort_unstable_by_key(|endpoint| {
        (
            endpoint.effect_seq,
            endpoint.kind.code(),
            endpoint.object_id,
        )
    });
    Ok((endpoints, participants))
}

#[allow(clippy::too_many_arguments)]
fn compete_for_permit(
    transaction: &Transaction<'_>,
    task: &StoredTask,
    attempt: &AttemptRecord,
    request: &PermitRequest,
    artifact_authority: Option<&nlos_artifact::ArtifactStore>,
    process_authority: Option<&nlos_process::ProcessAuthority>,
    resource_authority: Option<&nlos_resource::ResourceAuthority>,
    operation_authority: Option<&nlos_store::SqliteOperationStore>,
    channel_authority: Option<&nlos_channel::ChannelAuthority>,
    authority_lease: Option<AuthorityLeaseRecord>,
) -> Result<PermitDecision, TaskStoreError> {
    if !attempt.state.is_open_candidate() {
        return Err(TaskStoreError::InvalidAttemptState {
            state: attempt.state,
        });
    }
    if let Some(reason) = validate_head_binding(&task.record, &attempt.snapshot) {
        set_attempt_state(
            transaction,
            attempt,
            AttemptState::Conflicted,
            None,
            request.requested_at_ms,
        )?;
        return Ok(PermitDecision::Conflicted { reason });
    }
    // `[TASK-COMMIT-003]` / `[TASK-EFFECT-003]`: a quarantine tombstone
    // blocks new winner issuance until every unknown slot is reconciled.
    if let Some(tombstone) = load_quarantined_permit(transaction, task.record.task_id)? {
        set_attempt_state(
            transaction,
            attempt,
            AttemptState::Superseded,
            None,
            request.requested_at_ms,
        )?;
        return Ok(PermitDecision::Quarantined {
            quarantine_receipt_id: crate::model::derive_quarantine_receipt_id(tombstone.permit_id),
        });
    }
    if let Some(active) = load_active_permit(transaction, task.record.task_id)? {
        // The conceptual CREATED → READY_TO_COMMIT walk collapses into this
        // atomic CAS: the durable outcomes are only COMMIT_PERMITTED,
        // SUPERSEDED, or CONFLICTED.
        if active.attempt_id == attempt.attempt_id {
            set_attempt_state(
                transaction,
                attempt,
                AttemptState::Conflicted,
                None,
                request.requested_at_ms,
            )?;
            return Ok(PermitDecision::Conflicted {
                reason: PermitConflict::AttemptAlreadyHoldsPermit {
                    permit_id: active.permit_id,
                },
            });
        }
        set_attempt_state(
            transaction,
            attempt,
            AttemptState::Superseded,
            None,
            request.requested_at_ms,
        )?;
        return Ok(PermitDecision::Superseded {
            winner: Box::new(active),
        });
    }
    let record = issue_permit(
        transaction,
        task,
        attempt,
        request,
        artifact_authority,
        process_authority,
        resource_authority,
        operation_authority,
        channel_authority,
        authority_lease,
    )?;
    Ok(PermitDecision::Issued(Box::new(record)))
}

fn validate_artifact_write_bindings(
    artifact_authority: &nlos_artifact::ArtifactStore,
    record: &TaskWriteSetRecord,
) -> Result<(), TaskStoreError> {
    let now_ms =
        u64::try_from(record.sealed_at_ms).map_err(|_| TaskStoreError::TaskWriteSetConflict {
            reason: "sealed_at_ms must be non-negative",
        })?;
    for expected in &record.artifact_writes {
        let actual_revision = artifact_authority
            .resolve_head(expected.artifact_id, now_ms)
            .map_err(TaskStoreError::ArtifactParticipantAuthority)?
            .map_or(0, |head| head.revision);
        let expected_target = expected
            .expected_head_revision
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?;
        if actual_revision != expected.expected_head_revision
            || expected.proposed_revision != expected_target
        {
            return Err(TaskStoreError::TaskWriteSetConflict {
                reason: "Artifact write owner head differs before permit freeze",
            });
        }
    }
    Ok(())
}

fn validate_process_binding(
    process_authority: &nlos_process::ProcessAuthority,
    task_id: nlos_types::TaskId,
    attempt_id: nlos_types::TaskAttemptId,
    attempt_generation: nlos_types::Generation,
    record: &TaskWriteSetRecord,
) -> Result<(), TaskStoreError> {
    let Some(expected) = record.process_binding else {
        return Ok(());
    };
    let active = nlos_process::ActiveProcessBinding {
        process_id: expected.process_id,
        process_generation: expected.process_generation,
        process_fencing_token: expected.process_fencing_token,
        agent_instance_id: expected.agent_instance_id,
        agent_instance_generation: expected.agent_instance_generation,
        isolation_domain_id: expected.isolation_domain_id,
        isolation_domain_generation: expected.isolation_domain_generation,
        isolation_domain_fencing_token: expected.isolation_domain_fencing_token,
    };
    let owner = process_authority
        .verify_active_process_binding(&active)
        .map_err(TaskStoreError::ProcessParticipantAuthority)?;
    if owner.task_id != task_id
        || owner.task_attempt_id != attempt_id
        || owner.attempt_generation != attempt_generation
    {
        return Err(TaskStoreError::TaskWriteSetConflict {
            reason: "Process binding owner belongs to a different TaskAttempt",
        });
    }
    let proof = process_authority
        .inspect_binding_endpoint_proof(expected.process_id)
        .map_err(TaskStoreError::ProcessParticipantAuthority)?;
    if proof.participant_id != expected.participant_id
        || proof.participant_generation != expected.participant_generation
        || proof.admission_receipt_id != expected.admission_receipt_id
    {
        return Err(TaskStoreError::TaskWriteSetConflict {
            reason: "Process binding endpoint proof differs before permit freeze",
        });
    }
    Ok(())
}

fn validate_resource_reservation_bindings(
    resource_authority: &nlos_resource::ResourceAuthority,
    record: &TaskWriteSetRecord,
) -> Result<(), TaskStoreError> {
    for expected in &record.resource_reservations {
        let actual = resource_authority
            .inspect_permit_binding(expected.reservation_id)
            .map_err(TaskStoreError::ResourceParticipantAuthority)?;
        if actual.reservation_id != expected.reservation_id
            || actual.account_id != expected.account_id
            || actual.quote_id != expected.quote_id
            || actual.call_id != expected.call_id
            || actual.operation_id != expected.operation_id
            || actual.driver_id != expected.driver_id
            || actual.device_id != expected.device_id
            || actual.driver_generation != expected.driver_generation
            || actual.driver_fencing_token != expected.driver_fencing_token
            || actual.upper_bound != expected.upper_bound
        {
            return Err(TaskStoreError::TaskWriteSetResourceReservationConflict);
        }
    }
    Ok(())
}

fn validate_operation_endpoint_bindings(
    operation_authority: Option<&nlos_store::SqliteOperationStore>,
    record: &TaskWriteSetRecord,
) -> Result<(), TaskStoreError> {
    let has_operation_endpoint = record
        .effect_endpoints
        .iter()
        .any(|endpoint| endpoint.kind == TaskWriteSetEffectEndpointKind::OperationBinding);
    if !has_operation_endpoint {
        return Ok(());
    }
    let authority = operation_authority.ok_or(TaskStoreError::TaskWriteSetConflict {
        reason: "Operation effect endpoint requires OperationAuthority readback before permit freeze",
    })?;
    for expected in record
        .effect_endpoints
        .iter()
        .filter(|endpoint| endpoint.kind == TaskWriteSetEffectEndpointKind::OperationBinding)
    {
        let proof = authority
            .inspect_endpoint_proof(OperationHandle {
                operation_id: OperationId::from_bytes(expected.object_id),
                generation: expected.participant_generation,
            })
            .map_err(TaskStoreError::OperationParticipantAuthority)?;
        if proof.operation.operation_id != OperationId::from_bytes(expected.object_id)
            || proof.operation.generation != expected.participant_generation
            || proof.participant_id != expected.participant_id
            || proof.participant_generation != expected.participant_generation
            || proof.admission_receipt_id != expected.admission_receipt_id
        {
            return Err(TaskStoreError::TaskWriteSetConflict {
                reason: "Operation endpoint proof differs before permit freeze",
            });
        }
    }
    Ok(())
}

fn validate_channel_endpoint_bindings(
    channel_authority: Option<&nlos_channel::ChannelAuthority>,
    record: &TaskWriteSetRecord,
) -> Result<(), TaskStoreError> {
    let has_channel_endpoint = record
        .effect_endpoints
        .iter()
        .any(|endpoint| endpoint.kind == TaskWriteSetEffectEndpointKind::ChannelTopicBinding);
    if !has_channel_endpoint {
        return Ok(());
    }
    let authority = channel_authority.ok_or(TaskStoreError::TaskWriteSetConflict {
        reason: "Channel effect endpoint requires ChannelAuthority readback before permit freeze",
    })?;
    for expected in record
        .effect_endpoints
        .iter()
        .filter(|endpoint| endpoint.kind == TaskWriteSetEffectEndpointKind::ChannelTopicBinding)
    {
        let channel_id = ChannelId::from_bytes(expected.object_id);
        let proof = authority
            .inspect_endpoint_proof(channel_id)
            .map_err(TaskStoreError::ChannelParticipantAuthority)?;
        if proof.channel_id != channel_id
            || proof.participant_id != expected.participant_id
            || proof.participant_generation != expected.participant_generation
            || proof.admission_receipt_id != expected.admission_receipt_id
        {
            return Err(TaskStoreError::TaskWriteSetConflict {
                reason: "Channel endpoint proof differs before permit freeze",
            });
        }
    }
    Ok(())
}

fn validate_head_binding(task: &TaskRecord, snapshot: &SnapshotBundle) -> Option<PermitConflict> {
    if snapshot.expected_head_commit_seq != task.head_commit_seq {
        return Some(PermitConflict::StaleTaskHead {
            expected: snapshot.expected_head_commit_seq,
            current: task.head_commit_seq,
        });
    }
    if snapshot.effect_history_root != task.head_effect_history_root {
        return Some(PermitConflict::StaleEffectHistoryRoot);
    }
    if snapshot.retry_fence_epoch != task.retry_fence_epoch {
        return Some(PermitConflict::StaleRetryFenceEpoch);
    }
    None
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn issue_permit(
    transaction: &Transaction<'_>,
    task: &StoredTask,
    attempt: &AttemptRecord,
    request: &PermitRequest,
    artifact_authority: Option<&nlos_artifact::ArtifactStore>,
    process_authority: Option<&nlos_process::ProcessAuthority>,
    resource_authority: Option<&nlos_resource::ResourceAuthority>,
    operation_authority: Option<&nlos_store::SqliteOperationStore>,
    channel_authority: Option<&nlos_channel::ChannelAuthority>,
    authority_lease: Option<AuthorityLeaseRecord>,
) -> Result<PermitRecord, TaskStoreError> {
    if let Some(lease) = authority_lease {
        validate_authority_lease_binding_in_transaction(
            transaction,
            lease.binding(),
            request.requested_at_ms,
        )?;
    }
    let effect_set_root = crate::effect::validate_planned_effects(
        task.record.task_id,
        task.record.task_generation,
        &request.planned_effects,
    )?;
    let permit_epoch = task
        .record
        .permit_epoch
        .checked_add(1)
        .ok_or(TaskStoreError::EpochExhausted)?;
    let control_epoch = task
        .record
        .control_epoch
        .checked_add(1)
        .ok_or(TaskStoreError::EpochExhausted)?;
    let sealed_write_set =
        load_write_set_by_root(transaction, request.task_id, request.write_set_root)?;
    if let Some(record) = &sealed_write_set {
        if record.attempt_id != attempt.attempt_id
            || record.attempt_generation != attempt.attempt_generation
            || record.snapshot_id != attempt.snapshot.snapshot_id
            || record.expected_head_commit_seq != attempt.snapshot.expected_head_commit_seq
            || record.effect_history_root != attempt.snapshot.effect_history_root
            || record.retry_fence_epoch != attempt.snapshot.retry_fence_epoch
        {
            return Err(TaskStoreError::TaskWriteSetConflict {
                reason: "sealed TaskWriteSet no longer matches attempt snapshot",
            });
        }
        if record.artifact_read_set_root
            != crate::model::artifact_read_set_root(&record.artifact_reads)
            || record.semantic_append_set_root
                != crate::model::semantic_append_set_root(&record.semantic_appends)
            || record.semantic_read_set_root
                != crate::model::semantic_read_set_root(&record.semantic_reads)
            || record.resource_reservation_set_root
                != crate::model::resource_reservation_set_root(&record.resource_reservations)
            || record.effect_set_root
                != if record.planned_effects.is_empty() {
                    [0; 32]
                } else {
                    crate::effect::effect_set_root_of(&record.planned_effects)
                }
            || (!record.planned_effects.is_empty()
                && crate::effect::validate_planned_effects(
                    record.task_id,
                    task.record.task_generation,
                    &record.planned_effects,
                )? != record.effect_set_root)
            || record.effect_endpoint_set_root
                != crate::model::effect_endpoint_set_root(&record.effect_endpoints)
            || record.artifact_write_set_root
                != crate::model::artifact_write_set_root(&record.artifact_writes)
            || record.write_set_root != crate::model::task_write_set_root(record)
        {
            return Err(TaskStoreError::CorruptRecord(
                "TaskWriteSet canonical root mismatch",
            ));
        }
        if record.planned_effects != request.planned_effects
            || record.effect_set_root
                != if request.planned_effects.is_empty() {
                    [0; 32]
                } else {
                    crate::effect::effect_set_root_of(&request.planned_effects)
                }
        {
            return Err(TaskStoreError::TaskWriteSetConflict {
                reason: "permit planned effects differ from sealed TaskWriteSet",
            });
        }
        if let Some(resource_authority) = resource_authority {
            validate_resource_reservation_bindings(resource_authority, record)?;
        }
        if let Some(artifact_authority) = artifact_authority {
            validate_artifact_write_bindings(artifact_authority, record)?;
        }
        if let Some(process_authority) = process_authority {
            validate_process_binding(
                process_authority,
                task.record.task_id,
                attempt.attempt_id,
                attempt.attempt_generation,
                record,
            )?;
        }
        validate_operation_endpoint_bindings(operation_authority, record)?;
        validate_channel_endpoint_bindings(channel_authority, record)?;
        let current_group = crate::group::current_commit_binding(transaction, attempt.attempt_id)?;
        if record.group_binding != current_group {
            return Err(TaskStoreError::MembershipConflict);
        }
        let registry = crate::participant::initialize_registry(
            transaction,
            &task.record,
            request.requested_at_ms,
        )?;
        if registry.state != crate::ParticipantRegistryState::Open
            || record.participant_registry_binding.generation != registry.generation
            || record.participant_registry_binding.root != registry.root
        {
            return Err(TaskStoreError::ParticipantRegistryBindingMismatch);
        }
        for endpoint in &record.effect_endpoints {
            let participant = crate::ParticipantRecord {
                participant_type: match endpoint.kind {
                    TaskWriteSetEffectEndpointKind::ArtifactHead => {
                        crate::ParticipantType::ArtifactHead
                    }
                    TaskWriteSetEffectEndpointKind::SemanticAdmission => {
                        crate::ParticipantType::SemanticAdmission
                    }
                    TaskWriteSetEffectEndpointKind::ProcessBinding => {
                        crate::ParticipantType::ProcessBinding
                    }
                    TaskWriteSetEffectEndpointKind::DriverGateway => {
                        crate::ParticipantType::DriverGateway
                    }
                    TaskWriteSetEffectEndpointKind::ResourceLedger => {
                        crate::ParticipantType::ResourceLedger
                    }
                    TaskWriteSetEffectEndpointKind::OperationBinding => {
                        crate::ParticipantType::OperationBinding
                    }
                    TaskWriteSetEffectEndpointKind::ChannelTopicBinding => {
                        crate::ParticipantType::ChannelTopic
                    }
                },
                participant_id: endpoint.participant_id,
                participant_generation: endpoint.participant_generation,
                admission_receipt_id: endpoint.admission_receipt_id,
            };
            if !crate::participant::has_participant(&registry, participant) {
                return Err(TaskStoreError::ParticipantRegistryBindingMismatch);
            }
        }
    }
    let participant_registry_binding =
        crate::participant::freeze_for_permit(transaction, &task.record, request.requested_at_ms)?;
    if sealed_write_set
        .as_ref()
        .is_some_and(|record| record.participant_registry_binding != participant_registry_binding)
    {
        return Err(TaskStoreError::ParticipantRegistryBindingMismatch);
    }
    if let Some(lease) = authority_lease {
        ensure_active_assignment(
            transaction,
            &task.record,
            lease,
            participant_registry_binding,
            control_epoch,
            request.requested_at_ms,
        )?;
    }
    let record = PermitRecord {
        permit_id: derive_permit_id(request.task_id, request.idempotency_key),
        task_id: request.task_id,
        idempotency_key: request.idempotency_key,
        attempt_id: attempt.attempt_id,
        attempt_generation: attempt.attempt_generation,
        expected_head_commit_seq: attempt.snapshot.expected_head_commit_seq,
        expected_effect_history_root: attempt.snapshot.effect_history_root,
        expected_retry_fence_epoch: attempt.snapshot.retry_fence_epoch,
        write_set_root: request.write_set_root,
        group_binding: crate::group::current_commit_binding(transaction, attempt.attempt_id)?,
        participant_registry_binding: Some(participant_registry_binding),
        authority_lease_binding: authority_lease.map(AuthorityLeaseRecord::binding),
        permit_epoch,
        control_epoch,
        cancel_epoch: task.record.cancel_epoch,
        valid_until_ms: request.valid_until_ms,
        state: PermitState::Issued,
        created_at_ms: request.requested_at_ms,
        updated_at_ms: request.requested_at_ms,
    };
    insert_permit(transaction, &record)?;
    crate::effect::insert_effect_set(
        transaction,
        &record,
        &request.planned_effects,
        effect_set_root,
        request.requested_at_ms,
    )?;
    set_attempt_state(
        transaction,
        attempt,
        AttemptState::CommitPermitted,
        None,
        request.requested_at_ms,
    )?;
    update_task(transaction, task, request.requested_at_ms, |task_record| {
        task_record.permit_epoch = permit_epoch;
        task_record.control_epoch = control_epoch;
    })?;
    Ok(record)
}

fn ensure_active_assignment(
    transaction: &Transaction<'_>,
    task: &TaskRecord,
    lease: AuthorityLeaseRecord,
    registry_binding: crate::ParticipantRegistryBinding,
    control_epoch: u64,
    now_ms: i64,
) -> Result<AuthorityAssignmentRecord, TaskStoreError> {
    let assignment_id = derive_assignment_id(
        task.task_id,
        task.task_generation,
        lease.authority_id,
        lease.term,
        registry_binding,
    );
    if let Some(existing) = load_current_assignment(transaction, task.task_id)? {
        if existing.state != AuthorityAssignmentState::Active {
            return Err(TaskStoreError::AuthorityLeaseFenced);
        }
        if existing.assignment_id == assignment_id {
            return refresh_active_assignment(
                transaction,
                &existing,
                lease.binding(),
                control_epoch,
                now_ms,
            );
        }
        if existing.authority_lease_binding.term > lease.term {
            return Err(TaskStoreError::AuthorityLeaseFenced);
        }
    }
    let record = AuthorityAssignmentRecord {
        assignment_id,
        task_id: task.task_id,
        task_generation: task.task_generation,
        authority_lease_binding: lease.binding(),
        control_epoch,
        participant_registry_binding: registry_binding,
        state: AuthorityAssignmentState::Active,
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    };
    insert_assignment(transaction, &record)?;
    Ok(record)
}

fn persist_takeover_pending_receipt(
    transaction: &Transaction<'_>,
    task: &TaskRecord,
    assignment: AuthorityAssignmentRecord,
    fence: &AuthorityLeaseTakeoverFenceRecord,
    now_ms: i64,
) -> Result<(), TaskStoreError> {
    if assignment.task_id != task.task_id
        || assignment.task_generation != task.task_generation
        || assignment.participant_registry_binding != fence.frozen_registry_binding
    {
        return Err(TaskStoreError::CorruptRecord(
            "takeover assignment binding mismatch",
        ));
    }
    let current = load_current_assignment(transaction, task.task_id)?.ok_or(
        TaskStoreError::CorruptRecord("takeover assignment disappeared"),
    )?;
    if current.assignment_id != assignment.assignment_id {
        return Err(TaskStoreError::CorruptRecord(
            "takeover assignment changed during fence",
        ));
    }
    let pending = match current.state {
        AuthorityAssignmentState::Active => {
            if fence.authority_lease_binding.term <= current.authority_lease_binding.term {
                return Err(TaskStoreError::AuthorityLeaseFenced);
            }
            mark_assignment_takeover_pending(transaction, &current, now_ms)?
        }
        AuthorityAssignmentState::TakeoverPending => current,
        AuthorityAssignmentState::Fenced => return Err(TaskStoreError::AuthorityLeaseFenced),
    };
    let expected = AuthorityTakeoverReceiptRecord {
        receipt_id: derive_takeover_receipt_id(
            task.task_id,
            task.task_generation,
            pending.assignment_id,
            fence.receipt_id,
            fence.authority_lease_binding,
            fence.control_epoch,
        ),
        task_id: task.task_id,
        task_generation: task.task_generation,
        old_assignment_id: pending.assignment_id,
        new_assignment_id: None,
        fence_receipt_id: fence.receipt_id,
        frozen_old_authority_term: pending.authority_lease_binding.term,
        frozen_old_control_epoch: pending.control_epoch,
        new_authority_lease_binding: fence.authority_lease_binding,
        new_control_epoch: fence.control_epoch,
        frozen_registry_binding: fence.frozen_registry_binding,
        exact_fence_set_root: fence.exact_fence_set_root,
        outstanding_operation_participant_root: fence.outstanding_operation_participant_root,
        barrier_state: AuthorityTakeoverReceiptState::Pending,
        created_at_ms: fence.created_at_ms,
    };
    if let Some(existing) = load_takeover_receipt(transaction, task.task_id, fence.receipt_id)? {
        if existing != expected {
            return Err(TaskStoreError::CorruptRecord(
                "takeover receipt changed during replay",
            ));
        }
        return Ok(());
    }
    if current.state == AuthorityAssignmentState::TakeoverPending {
        return Err(TaskStoreError::CorruptRecord(
            "pending assignment missing takeover receipt",
        ));
    }
    insert_takeover_receipt(transaction, &expected)
}

fn persist_takeover_fence_members(
    transaction: &Transaction<'_>,
    fence: &AuthorityLeaseTakeoverFenceRecord,
    participants: &[crate::ParticipantRecord],
) -> Result<(), TaskStoreError> {
    let expected = participants
        .iter()
        .copied()
        .map(|participant| AuthorityTakeoverFenceMemberRecord {
            fence_receipt_id: fence.receipt_id,
            task_id: fence.task_id,
            task_generation: fence.task_generation,
            participant,
        })
        .collect::<Vec<_>>();
    let existing = load_takeover_fence_members(transaction, fence.receipt_id)?;
    if existing.is_empty() {
        for member in &expected {
            insert_takeover_fence_member(transaction, member)?;
        }
        return Ok(());
    }
    if existing != expected {
        return Err(TaskStoreError::CorruptRecord(
            "takeover fence member manifest changed during replay",
        ));
    }
    Ok(())
}

pub(crate) fn closure_receipt(
    task: &TaskRecord,
    attempt: &AttemptRecord,
    receipt_id: ReceiptId,
    now_ms: i64,
) -> TaskReceiptRecord {
    TaskReceiptRecord {
        receipt_id,
        task_id: task.task_id,
        permit_id: None,
        attempt_id: attempt.attempt_id,
        attempt_generation: attempt.attempt_generation,
        group_binding: None,
        participant_registry_binding: None,
        outcome: ReceiptOutcome::CancelledBeforeEffect,
        prior_head_commit_seq: task.head_commit_seq,
        prior_effect_history_root: task.head_effect_history_root,
        prior_retry_fence_epoch: task.retry_fence_epoch,
        new_head_commit_seq: task.head_commit_seq,
        new_effect_history_root: task.head_effect_history_root,
        new_retry_fence_epoch: task.retry_fence_epoch,
        created_at_ms: now_ms,
    }
}

pub(crate) fn handle_of(record: &AttemptRecord) -> AttemptHandle {
    AttemptHandle {
        attempt_id: record.attempt_id,
        attempt_generation: record.attempt_generation,
        snapshot_id: record.snapshot.snapshot_id,
    }
}

pub(crate) fn attempt_matches_spec(record: &AttemptRecord, spec: &AttemptSpec) -> bool {
    record.task_id == spec.task_id
        && record.attempt_id == spec.attempt_id
        && record.attempt_generation == spec.attempt_generation
        && record.snapshot == spec.snapshot
        && record.cancellation_scope_id == spec.cancellation_scope_id
        && record.cancellation_generation == spec.cancellation_generation
}

fn insert_snapshot_receipt(
    transaction: &Transaction<'_>,
    record: &TaskSnapshotReceiptRecord,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_snapshot_receipts (
            receipt_id, task_id, snapshot_id, builder_id,
            builder_version_digest, dependency_closure_root,
            semantic_resolver_digest, canonical_iteration_digest,
            achieved_consistency, built_at_ms, authority_id, key_id, signature
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            record.receipt_id.as_bytes().as_slice(),
            record.task_id.as_bytes().as_slice(),
            record.snapshot.snapshot_id.as_bytes().as_slice(),
            record.builder_id.as_slice(),
            record.builder_version_digest.as_slice(),
            record.dependency_closure_root.as_slice(),
            record.semantic_resolver_digest.as_slice(),
            record.canonical_iteration_digest.as_slice(),
            record.achieved_consistency.code(),
            record.built_at_ms,
            record.authority_id.as_slice(),
            record.key_id.as_slice(),
            record.signature.as_slice(),
        ],
    )?;
    for (sequence, checkpoint) in record.per_authority_checkpoint_receipts.iter().enumerate() {
        transaction.execute(
            "INSERT INTO task_snapshot_checkpoint_receipts (
                snapshot_receipt_id, checkpoint_seq, checkpoint_receipt_id
             ) VALUES (?1, ?2, ?3)",
            params![
                record.receipt_id.as_bytes().as_slice(),
                i64::try_from(sequence).map_err(|_| TaskStoreError::InvalidSnapshotReceipt {
                    reason: "checkpoint sequence exceeds SQLite integer",
                })?,
                checkpoint.as_bytes().as_slice(),
            ],
        )?;
    }
    Ok(())
}

fn load_snapshot_receipt(
    source: &impl SqlRead,
    task_id: TaskId,
    receipt_id: ReceiptId,
) -> Result<TaskSnapshotReceiptRecord, TaskStoreError> {
    load_snapshot_receipt_optional(source, task_id, receipt_id)?
        .ok_or(TaskStoreError::SnapshotReceiptNotFound)
}

fn load_snapshot_receipt_optional(
    source: &impl SqlRead,
    task_id: TaskId,
    receipt_id: ReceiptId,
) -> Result<Option<TaskSnapshotReceiptRecord>, TaskStoreError> {
    load_snapshot_receipt_matching(source, task_id, *receipt_id.as_bytes(), false)
}

fn load_snapshot_receipt_by_snapshot_optional(
    source: &impl SqlRead,
    task_id: TaskId,
    snapshot_id: TaskSnapshotId,
) -> Result<Option<TaskSnapshotReceiptRecord>, TaskStoreError> {
    load_snapshot_receipt_matching(source, task_id, *snapshot_id.as_bytes(), true)
}

fn load_snapshot_receipt_matching(
    source: &impl SqlRead,
    task_id: TaskId,
    key: [u8; 16],
    by_snapshot: bool,
) -> Result<Option<TaskSnapshotReceiptRecord>, TaskStoreError> {
    let predicate = if by_snapshot {
        "r.task_id = ?1 AND r.snapshot_id = ?2"
    } else {
        "r.task_id = ?1 AND r.receipt_id = ?2"
    };
    let sql = format!(
        "SELECT r.receipt_id, r.task_id,
                s.snapshot_id, s.snapshot_digest, s.expected_head_commit_seq,
                s.effect_history_root, s.retry_fence_epoch,
                r.builder_id, r.builder_version_digest,
                r.dependency_closure_root, r.semantic_resolver_digest,
                r.canonical_iteration_digest, r.achieved_consistency,
                r.built_at_ms, r.authority_id, r.key_id, r.signature
         FROM task_snapshot_receipts r
         JOIN task_snapshots s
           ON s.task_id = r.task_id AND s.snapshot_id = r.snapshot_id
         WHERE {predicate}"
    );
    let header = {
        let mut statement = source.prepare_statement(&sql)?;
        let mut rows = statement.query(params![task_id.as_bytes().as_slice(), key.as_slice()])?;
        rows.next()?
            .map(decode_snapshot_receipt_header)
            .transpose()?
    };
    let Some(mut record) = header else {
        return Ok(None);
    };
    record.per_authority_checkpoint_receipts =
        load_snapshot_checkpoints(source, record.receipt_id)?;
    Ok(Some(record))
}

fn decode_snapshot_receipt_header(
    row: &rusqlite::Row<'_>,
) -> Result<TaskSnapshotReceiptRecord, TaskStoreError> {
    Ok(TaskSnapshotReceiptRecord {
        receipt_id: ReceiptId::from_bytes(blob16(row, 0)?),
        task_id: TaskId::from_bytes(blob16(row, 1)?),
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes(blob16(row, 2)?),
            snapshot_digest: blob32(row, 3)?,
            expected_head_commit_seq: u64_from_blob(row, 4)?,
            effect_history_root: blob32(row, 5)?,
            retry_fence_epoch: u64_from_blob(row, 6)?,
        },
        builder_id: blob16(row, 7)?,
        builder_version_digest: blob32(row, 8)?,
        per_authority_checkpoint_receipts: Vec::new(),
        dependency_closure_root: blob32(row, 9)?,
        semantic_resolver_digest: blob32(row, 10)?,
        canonical_iteration_digest: blob32(row, 11)?,
        achieved_consistency: crate::SnapshotConsistency::from_code(row.get(12)?)?,
        built_at_ms: row.get(13)?,
        authority_id: blob16(row, 14)?,
        key_id: blob16(row, 15)?,
        signature: blob64(row, 16)?,
    })
}

fn load_snapshot_checkpoints(
    source: &impl SqlRead,
    receipt_id: ReceiptId,
) -> Result<Vec<ReceiptId>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT checkpoint_receipt_id
         FROM task_snapshot_checkpoint_receipts
         WHERE snapshot_receipt_id = ?1
         ORDER BY checkpoint_seq",
    )?;
    let mut rows = statement.query([receipt_id.as_bytes().as_slice()])?;
    let mut checkpoints = Vec::new();
    while let Some(row) = rows.next()? {
        checkpoints.push(ReceiptId::from_bytes(blob16(row, 0)?));
    }
    if checkpoints.is_empty() {
        return Err(TaskStoreError::CorruptRecord(
            "snapshot receipt has no authority checkpoint receipts",
        ));
    }
    Ok(checkpoints)
}

pub(crate) trait SqlRead {
    fn prepare_statement(&self, sql: &str) -> Result<rusqlite::Statement<'_>, rusqlite::Error>;
}

impl SqlRead for Connection {
    fn prepare_statement(&self, sql: &str) -> Result<rusqlite::Statement<'_>, rusqlite::Error> {
        self.prepare(sql)
    }
}

impl SqlRead for Transaction<'_> {
    fn prepare_statement(&self, sql: &str) -> Result<rusqlite::Statement<'_>, rusqlite::Error> {
        self.prepare(sql)
    }
}

fn insert_task(transaction: &Transaction<'_>, record: &TaskRecord) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO tasks (
            task_id, task_generation, head_commit_seq, head_effect_history_root,
            retry_fence_epoch, control_epoch, cancel_epoch, permit_epoch,
            task_state, revision, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10, ?11)",
        params![
            record.task_id.as_bytes().as_slice(),
            encode_u64(record.task_generation.get()).as_slice(),
            encode_u64(record.head_commit_seq).as_slice(),
            record.head_effect_history_root.as_slice(),
            encode_u64(record.retry_fence_epoch).as_slice(),
            encode_u64(record.control_epoch).as_slice(),
            encode_u64(record.cancel_epoch).as_slice(),
            encode_u64(record.permit_epoch).as_slice(),
            record.state.code(),
            record.created_at_ms,
            record.updated_at_ms,
        ],
    )?;
    Ok(())
}

pub(crate) fn update_task(
    transaction: &Transaction<'_>,
    task: &StoredTask,
    now_ms: i64,
    mutate: impl FnOnce(&mut TaskRecord),
) -> Result<(), TaskStoreError> {
    let mut record = task.record.clone();
    mutate(&mut record);
    let changed = transaction.execute(
        "UPDATE tasks SET
            head_commit_seq = ?1, head_effect_history_root = ?2,
            retry_fence_epoch = ?3, control_epoch = ?4, cancel_epoch = ?5,
            permit_epoch = ?6, task_state = ?7, updated_at_ms = ?8,
            revision = revision + 1
         WHERE task_id = ?9 AND revision = ?10",
        params![
            encode_u64(record.head_commit_seq).as_slice(),
            record.head_effect_history_root.as_slice(),
            encode_u64(record.retry_fence_epoch).as_slice(),
            encode_u64(record.control_epoch).as_slice(),
            encode_u64(record.cancel_epoch).as_slice(),
            encode_u64(record.permit_epoch).as_slice(),
            record.state.code(),
            now_ms,
            record.task_id.as_bytes().as_slice(),
            task.revision,
        ],
    )?;
    if changed != 1 {
        return Err(TaskStoreError::CorruptRecord(
            "task revision compare-and-swap failed",
        ));
    }
    Ok(())
}

const TASK_COLUMNS: &str = "task_id, task_generation, head_commit_seq, head_effect_history_root,
     retry_fence_epoch, control_epoch, cancel_epoch, permit_epoch,
     task_state, revision, created_at_ms, updated_at_ms";

pub(crate) fn load_task(
    source: &impl SqlRead,
    task_id: TaskId,
) -> Result<StoredTask, TaskStoreError> {
    load_task_optional(source, task_id)?.ok_or(TaskStoreError::TaskNotFound)
}

fn load_task_optional(
    source: &impl SqlRead,
    task_id: TaskId,
) -> Result<Option<StoredTask>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {TASK_COLUMNS} FROM tasks WHERE task_id = ?1"
    ))?;
    let mut rows = statement.query([task_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_task_row).transpose()
}

fn decode_task_row(row: &rusqlite::Row<'_>) -> Result<StoredTask, TaskStoreError> {
    let revision: i64 = row.get(9)?;
    if revision < 0 {
        return Err(TaskStoreError::CorruptRecord("negative task revision"));
    }
    Ok(StoredTask {
        record: TaskRecord {
            task_id: TaskId::from_bytes(blob16(row, 0)?),
            task_generation: generation_from_blob(row, 1)?,
            head_commit_seq: u64_from_blob(row, 2)?,
            head_effect_history_root: blob32(row, 3)?,
            retry_fence_epoch: u64_from_blob(row, 4)?,
            control_epoch: u64_from_blob(row, 5)?,
            cancel_epoch: u64_from_blob(row, 6)?,
            permit_epoch: u64_from_blob(row, 7)?,
            state: TaskState::from_code(row.get(8)?)?,
            active_permit: None,
            created_at_ms: row.get(10)?,
            updated_at_ms: row.get(11)?,
        },
        revision,
    })
}

pub(crate) fn insert_snapshot_if_absent(
    transaction: &Transaction<'_>,
    task_id: TaskId,
    snapshot: &SnapshotBundle,
    now_ms: i64,
) -> Result<(), TaskStoreError> {
    if let Some(existing) = load_snapshot_optional(transaction, task_id, snapshot.snapshot_id)? {
        if existing == *snapshot {
            return Ok(());
        }
        return Err(TaskStoreError::SnapshotConflict);
    }
    transaction.execute(
        "INSERT INTO task_snapshots (
            task_id, snapshot_id, snapshot_digest, expected_head_commit_seq,
            effect_history_root, retry_fence_epoch, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            task_id.as_bytes().as_slice(),
            snapshot.snapshot_id.as_bytes().as_slice(),
            snapshot.snapshot_digest.as_slice(),
            encode_u64(snapshot.expected_head_commit_seq).as_slice(),
            snapshot.effect_history_root.as_slice(),
            encode_u64(snapshot.retry_fence_epoch).as_slice(),
            now_ms,
        ],
    )?;
    Ok(())
}

fn load_snapshot_optional(
    source: &impl SqlRead,
    task_id: TaskId,
    snapshot_id: TaskSnapshotId,
) -> Result<Option<SnapshotBundle>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT snapshot_id, snapshot_digest, expected_head_commit_seq,
                effect_history_root, retry_fence_epoch
         FROM task_snapshots WHERE task_id = ?1 AND snapshot_id = ?2",
    )?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        snapshot_id.as_bytes().as_slice(),
    ])?;
    rows.next()?
        .map(|row| {
            Ok(SnapshotBundle {
                snapshot_id: TaskSnapshotId::from_bytes(blob16(row, 0)?),
                snapshot_digest: blob32(row, 1)?,
                expected_head_commit_seq: u64_from_blob(row, 2)?,
                effect_history_root: blob32(row, 3)?,
                retry_fence_epoch: u64_from_blob(row, 4)?,
            })
        })
        .transpose()
}

pub(crate) fn insert_attempt(
    transaction: &Transaction<'_>,
    record: &AttemptRecord,
    idempotency_key: IdempotencyKey,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_attempts (
            attempt_id, task_id, attempt_generation, snapshot_id,
            snapshot_receipt_id,
            cancellation_scope_id, cancellation_generation, idempotency_key,
            attempt_state, receipt_id, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, ?11)",
        params![
            record.attempt_id.as_bytes().as_slice(),
            record.task_id.as_bytes().as_slice(),
            encode_u64(record.attempt_generation.get()).as_slice(),
            record.snapshot.snapshot_id.as_bytes().as_slice(),
            record.snapshot_receipt_id.map(ReceiptId::into_bytes),
            record.cancellation_scope_id.as_bytes().as_slice(),
            encode_u64(record.cancellation_generation.get()).as_slice(),
            idempotency_key.as_bytes().as_slice(),
            record.state.code(),
            record.created_at_ms,
            record.updated_at_ms,
        ],
    )?;
    Ok(())
}

// Attempt rows always join their immutable snapshot row so the digest
// bundle an attempt is bound to can never drift from the snapshot table.
const ATTEMPT_SELECT: &str = "SELECT a.attempt_id, a.task_id, a.attempt_generation, a.snapshot_id,
            a.snapshot_receipt_id,
            a.cancellation_scope_id, a.cancellation_generation,
            a.attempt_state, a.receipt_id, a.created_at_ms, a.updated_at_ms,
            s.snapshot_digest, s.expected_head_commit_seq,
            s.effect_history_root, s.retry_fence_epoch
     FROM task_attempts a
     JOIN task_snapshots s
       ON s.task_id = a.task_id AND s.snapshot_id = a.snapshot_id";

pub(crate) fn load_attempt(
    source: &impl SqlRead,
    task_id: TaskId,
    attempt_id: TaskAttemptId,
) -> Result<AttemptRecord, TaskStoreError> {
    load_attempt_optional(source, task_id, attempt_id)?.ok_or(TaskStoreError::AttemptNotFound)
}

fn load_attempt_optional(
    source: &impl SqlRead,
    task_id: TaskId,
    attempt_id: TaskAttemptId,
) -> Result<Option<AttemptRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "{ATTEMPT_SELECT} WHERE a.task_id = ?1 AND a.attempt_id = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        attempt_id.as_bytes().as_slice(),
    ])?;
    rows.next()?.map(decode_attempt_row).transpose()
}

pub(crate) fn load_attempt_global(
    source: &impl SqlRead,
    attempt_id: TaskAttemptId,
) -> Result<Option<AttemptRecord>, TaskStoreError> {
    let mut statement =
        source.prepare_statement(&format!("{ATTEMPT_SELECT} WHERE a.attempt_id = ?1"))?;
    let mut rows = statement.query([attempt_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_attempt_row).transpose()
}

pub(crate) fn load_attempt_by_key(
    source: &impl SqlRead,
    task_id: TaskId,
    idempotency_key: IdempotencyKey,
) -> Result<Option<AttemptRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "{ATTEMPT_SELECT} WHERE a.task_id = ?1 AND a.idempotency_key = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        idempotency_key.as_bytes().as_slice(),
    ])?;
    rows.next()?.map(decode_attempt_row).transpose()
}

pub(crate) fn list_open_attempts(
    source: &impl SqlRead,
    task_id: TaskId,
) -> Result<Vec<AttemptRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "{ATTEMPT_SELECT}
         WHERE a.task_id = ?1 AND a.attempt_state IN (?2, ?3)
         ORDER BY a.created_at_ms, a.attempt_id"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        AttemptState::Created.code(),
        AttemptState::ReadyToCommit.code(),
    ])?;
    let mut attempts = Vec::new();
    while let Some(row) = rows.next()? {
        attempts.push(decode_attempt_row(row)?);
    }
    Ok(attempts)
}

fn decode_attempt_row(row: &rusqlite::Row<'_>) -> Result<AttemptRecord, TaskStoreError> {
    Ok(AttemptRecord {
        attempt_id: TaskAttemptId::from_bytes(blob16(row, 0)?),
        task_id: TaskId::from_bytes(blob16(row, 1)?),
        attempt_generation: generation_from_blob(row, 2)?,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes(blob16(row, 3)?),
            snapshot_digest: blob32(row, 11)?,
            expected_head_commit_seq: u64_from_blob(row, 12)?,
            effect_history_root: blob32(row, 13)?,
            retry_fence_epoch: u64_from_blob(row, 14)?,
        },
        snapshot_receipt_id: optional_blob16(row, 4)?.map(ReceiptId::from_bytes),
        cancellation_scope_id: CancellationScopeId::from_bytes(blob16(row, 5)?),
        cancellation_generation: generation_from_blob(row, 6)?,
        state: AttemptState::from_code(row.get(7)?)?,
        receipt_id: optional_blob16(row, 8)?.map(ReceiptId::from_bytes),
        created_at_ms: row.get(9)?,
        updated_at_ms: row.get(10)?,
    })
}

pub(crate) fn set_attempt_state(
    transaction: &Transaction<'_>,
    attempt: &AttemptRecord,
    state: AttemptState,
    receipt_id: Option<ReceiptId>,
    now_ms: i64,
) -> Result<(), TaskStoreError> {
    let changed = transaction.execute(
        "UPDATE task_attempts
         SET attempt_state = ?1, receipt_id = ?2, updated_at_ms = ?3
         WHERE attempt_id = ?4 AND attempt_state = ?5",
        params![
            state.code(),
            receipt_id
                .map(ReceiptId::into_bytes)
                .as_ref()
                .map(<[u8; 16]>::as_slice),
            now_ms,
            attempt.attempt_id.as_bytes().as_slice(),
            attempt.state.code(),
        ],
    )?;
    if changed != 1 {
        return Err(TaskStoreError::CorruptRecord(
            "attempt state compare-and-swap failed",
        ));
    }
    Ok(())
}

fn insert_permit(
    transaction: &Transaction<'_>,
    record: &PermitRecord,
) -> Result<(), TaskStoreError> {
    let group_id = record
        .group_binding
        .map(|binding| binding.group_id.into_bytes());
    let membership_generation = record
        .group_binding
        .map(|binding| encode_u64(binding.membership_generation));
    let membership_root = record.group_binding.map(|binding| binding.membership_root);
    let group_policy_digest = record
        .group_binding
        .map(|binding| binding.group_policy_digest);
    let participant_registry_generation = record
        .participant_registry_binding
        .map(|binding| encode_u64(binding.generation));
    let participant_registry_root = record
        .participant_registry_binding
        .map(|binding| binding.root);
    let authority_lease_authority_id = record
        .authority_lease_binding
        .map(|binding| binding.authority_id.into_bytes());
    let authority_lease_holder_id = record
        .authority_lease_binding
        .map(|binding| binding.holder_id.into_bytes());
    let authority_lease_term = record
        .authority_lease_binding
        .map(|binding| encode_u64(binding.term));
    let authority_lease_epoch = record
        .authority_lease_binding
        .map(|binding| encode_u64(binding.lease_epoch));
    let authority_lease_fencing_token = record
        .authority_lease_binding
        .map(|binding| binding.fencing_token);
    let authority_lease_expires_at_ms = record
        .authority_lease_binding
        .map(|binding| binding.expires_at_ms);
    transaction.execute(
        "INSERT INTO commit_permits (
            permit_id, task_id, idempotency_key, attempt_id, attempt_generation,
            expected_head_commit_seq, expected_effect_history_root,
            expected_retry_fence_epoch, write_set_root, permit_epoch,
            control_epoch, cancel_epoch, valid_until_ms, permit_state,
            created_at_ms, updated_at_ms, group_id, membership_generation,
            membership_root, group_policy_digest, participant_registry_generation,
            participant_registry_root, authority_lease_authority_id,
            authority_lease_holder_id, authority_lease_term, authority_lease_epoch,
            authority_lease_fencing_token, authority_lease_expires_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                   ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28)",
        params![
            record.permit_id.as_bytes().as_slice(),
            record.task_id.as_bytes().as_slice(),
            record.idempotency_key.as_bytes().as_slice(),
            record.attempt_id.as_bytes().as_slice(),
            encode_u64(record.attempt_generation.get()).as_slice(),
            encode_u64(record.expected_head_commit_seq).as_slice(),
            record.expected_effect_history_root.as_slice(),
            encode_u64(record.expected_retry_fence_epoch).as_slice(),
            record.write_set_root.as_slice(),
            encode_u64(record.permit_epoch).as_slice(),
            encode_u64(record.control_epoch).as_slice(),
            encode_u64(record.cancel_epoch).as_slice(),
            record.valid_until_ms,
            record.state.code(),
            record.created_at_ms,
            record.updated_at_ms,
            group_id.as_ref().map(<[u8; 16]>::as_slice),
            membership_generation.as_ref().map(<[u8; 8]>::as_slice),
            membership_root.as_ref().map(<[u8; 32]>::as_slice),
            group_policy_digest.as_ref().map(<[u8; 32]>::as_slice),
            participant_registry_generation
                .as_ref()
                .map(<[u8; 8]>::as_slice),
            participant_registry_root.as_ref().map(<[u8; 32]>::as_slice),
            authority_lease_authority_id
                .as_ref()
                .map(<[u8; 16]>::as_slice),
            authority_lease_holder_id.as_ref().map(<[u8; 16]>::as_slice),
            authority_lease_term.as_ref().map(<[u8; 8]>::as_slice),
            authority_lease_epoch.as_ref().map(<[u8; 8]>::as_slice),
            authority_lease_fencing_token
                .as_ref()
                .map(<[u8; 32]>::as_slice),
            authority_lease_expires_at_ms,
        ],
    )?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn insert_write_set(
    transaction: &Transaction<'_>,
    record: &TaskWriteSetRecord,
) -> Result<(), TaskStoreError> {
    let group_id = record
        .group_binding
        .map(|binding| binding.group_id.into_bytes());
    let membership_generation = record
        .group_binding
        .map(|binding| encode_u64(binding.membership_generation));
    let membership_root = record.group_binding.map(|binding| binding.membership_root);
    let group_policy_digest = record
        .group_binding
        .map(|binding| binding.group_policy_digest);
    transaction.execute(
        "INSERT INTO task_write_sets (
            task_id, attempt_id, attempt_generation, idempotency_key,
            snapshot_id, snapshot_receipt_id, expected_head_commit_seq,
            effect_history_root, retry_fence_epoch, group_id,
            membership_generation, membership_root, group_policy_digest,
            participant_registry_generation, participant_registry_root,
            artifact_read_set_root, semantic_read_set_root,
            semantic_append_set_root, artifact_write_set_root,
            resource_reservation_set_root, effect_set_root,
            effect_endpoint_set_root,
            write_set_root, sealed_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                   ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)",
        params![
            record.task_id.as_bytes().as_slice(),
            record.attempt_id.as_bytes().as_slice(),
            encode_u64(record.attempt_generation.get()).as_slice(),
            record.idempotency_key.as_bytes().as_slice(),
            record.snapshot_id.as_bytes().as_slice(),
            record.snapshot_receipt_id.as_bytes().as_slice(),
            encode_u64(record.expected_head_commit_seq).as_slice(),
            record.effect_history_root.as_slice(),
            encode_u64(record.retry_fence_epoch).as_slice(),
            group_id.as_ref().map(<[u8; 16]>::as_slice),
            membership_generation.as_ref().map(<[u8; 8]>::as_slice),
            membership_root.as_ref().map(<[u8; 32]>::as_slice),
            group_policy_digest.as_ref().map(<[u8; 32]>::as_slice),
            encode_u64(record.participant_registry_binding.generation).as_slice(),
            record.participant_registry_binding.root.as_slice(),
            record.artifact_read_set_root.as_slice(),
            record.semantic_read_set_root.as_slice(),
            record.semantic_append_set_root.as_slice(),
            record.artifact_write_set_root.as_slice(),
            record.resource_reservation_set_root.as_slice(),
            record.effect_set_root.as_slice(),
            record.effect_endpoint_set_root.as_slice(),
            record.write_set_root.as_slice(),
            record.sealed_at_ms,
        ],
    )?;
    for (sequence, read) in record.artifact_reads.iter().enumerate() {
        transaction.execute(
            "INSERT INTO task_write_set_artifact_reads (
                task_id, idempotency_key, read_seq, artifact_id,
                expected_head_revision, expected_head_digest
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.task_id.as_bytes().as_slice(),
                record.idempotency_key.as_bytes().as_slice(),
                i64::try_from(sequence).map_err(|_| TaskStoreError::TaskWriteSetConflict {
                    reason: "Artifact read set exceeds SQLite sequence range",
                })?,
                read.artifact_id.as_bytes().as_slice(),
                encode_u64(read.expected_head_revision).as_slice(),
                read.expected_head_digest.as_ref().map(<[u8; 32]>::as_slice),
            ],
        )?;
    }
    for (sequence, write) in record.artifact_writes.iter().enumerate() {
        transaction.execute(
            "INSERT INTO task_write_set_artifact_writes (
                task_id, idempotency_key, write_seq, artifact_id,
                expected_head_revision, proposed_revision, content_digest,
                size_bytes
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                record.task_id.as_bytes().as_slice(),
                record.idempotency_key.as_bytes().as_slice(),
                i64::try_from(sequence).map_err(|_| TaskStoreError::TaskWriteSetConflict {
                    reason: "Artifact write set exceeds SQLite sequence range",
                })?,
                write.artifact_id.as_bytes().as_slice(),
                encode_u64(write.expected_head_revision).as_slice(),
                encode_u64(write.proposed_revision).as_slice(),
                write.content_digest.as_slice(),
                encode_u64(write.size_bytes).as_slice(),
            ],
        )?;
    }
    for (sequence, append) in record.semantic_appends.iter().enumerate() {
        transaction.execute(
            "INSERT INTO task_write_set_semantic_appends (
                task_id, idempotency_key, append_seq, event_id,
                target_scope_kind, target_scope_id, required_durability,
                admission_receipt_id, durability_receipt_id,
                admission_policy_digest
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                record.task_id.as_bytes().as_slice(),
                record.idempotency_key.as_bytes().as_slice(),
                i64::try_from(sequence).map_err(|_| TaskStoreError::TaskWriteSetConflict {
                    reason: "Semantic append set exceeds SQLite sequence range",
                })?,
                append.event_id.as_bytes().as_slice(),
                i64::from(append.target.kind()),
                append.target.id().as_slice(),
                i64::from(TaskWriteSetSemanticRequiredDurability::code()),
                append.admission_receipt_id.as_bytes().as_slice(),
                append
                    .durability_receipt_id
                    .map(|receipt_id| receipt_id.as_bytes().to_vec()),
                append.admission_policy_digest.map(|digest| digest.to_vec()),
            ],
        )?;
    }
    if let Some(binding) = record.process_binding {
        transaction.execute(
            "INSERT INTO task_write_set_process_bindings (
                task_id, idempotency_key, process_id, process_generation,
                process_fencing_token, agent_instance_id, agent_instance_generation,
                isolation_domain_id, isolation_domain_generation,
                isolation_domain_fencing_token, participant_id,
                participant_generation, admission_receipt_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                record.task_id.as_bytes().as_slice(),
                record.idempotency_key.as_bytes().as_slice(),
                binding.process_id.as_bytes().as_slice(),
                encode_u64(binding.process_generation.get()).as_slice(),
                binding.process_fencing_token.as_slice(),
                binding.agent_instance_id.as_bytes().as_slice(),
                encode_u64(binding.agent_instance_generation.get()).as_slice(),
                binding.isolation_domain_id.as_bytes().as_slice(),
                encode_u64(binding.isolation_domain_generation.get()).as_slice(),
                binding.isolation_domain_fencing_token.as_slice(),
                binding.participant_id.as_bytes().as_slice(),
                encode_u64(binding.participant_generation.get()).as_slice(),
                binding.admission_receipt_id.as_bytes().as_slice(),
            ],
        )?;
    }
    for (sequence, read) in record.semantic_reads.iter().enumerate() {
        transaction.execute(
            "INSERT INTO task_write_set_semantic_reads (
                task_id, idempotency_key, read_seq, event_id,
                expected_log_seq, expected_canonical_digest
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.task_id.as_bytes().as_slice(),
                record.idempotency_key.as_bytes().as_slice(),
                i64::try_from(sequence).map_err(|_| TaskStoreError::TaskWriteSetConflict {
                    reason: "Semantic read set exceeds SQLite sequence range",
                })?,
                read.event_id.as_bytes().as_slice(),
                encode_u64(read.expected_log_seq).as_slice(),
                read.expected_canonical_digest.as_slice(),
            ],
        )?;
    }
    for (sequence, reservation) in record.resource_reservations.iter().enumerate() {
        transaction.execute(
            "INSERT INTO task_write_set_resource_reservations (
                task_id, idempotency_key, read_seq, reservation_id, account_id,
                quote_id, call_id, operation_id, driver_id, device_id,
                driver_generation, driver_fencing_token, upper_bound
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                record.task_id.as_bytes().as_slice(),
                record.idempotency_key.as_bytes().as_slice(),
                i64::try_from(sequence).map_err(|_| TaskStoreError::TaskWriteSetConflict {
                    reason: "Resource Reservation set exceeds SQLite sequence range",
                })?,
                reservation.reservation_id.as_bytes().as_slice(),
                reservation.account_id.as_bytes().as_slice(),
                reservation.quote_id.as_bytes().as_slice(),
                reservation.call_id.as_bytes().as_slice(),
                reservation.operation_id.as_bytes().as_slice(),
                reservation.driver_id.as_bytes().as_slice(),
                reservation.device_id.as_bytes().as_slice(),
                encode_u64(reservation.driver_generation.get()).as_slice(),
                reservation.driver_fencing_token.as_slice(),
                encode_u64(reservation.upper_bound).as_slice(),
            ],
        )?;
    }
    for (sequence, planned) in record.planned_effects.iter().enumerate() {
        let descriptor = &planned.descriptor;
        transaction.execute(
            "INSERT INTO task_write_set_planned_effects (
                task_id, idempotency_key, effect_seq, intent_spec_id,
                stable_action_slot, target_authority_object_id, effect_class,
                idempotency_scope, logical_effect_id,
                idempotency_identity_digest, required, required_condition_digest,
                success_criteria_digest, action_proposal_digest
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                record.task_id.as_bytes().as_slice(),
                record.idempotency_key.as_bytes().as_slice(),
                i64::try_from(sequence).map_err(|_| TaskStoreError::TaskWriteSetConflict {
                    reason: "planned effect set exceeds SQLite sequence range",
                })?,
                descriptor.intent_spec_id.as_slice(),
                encode_u64(descriptor.stable_action_slot).as_slice(),
                descriptor.target_authority_object_id.as_slice(),
                i64::from(descriptor.effect_class),
                i64::from(descriptor.idempotency_scope),
                descriptor.logical_effect_id().as_slice(),
                descriptor.idempotency_identity_digest().as_slice(),
                i64::from(planned.required),
                planned
                    .required_condition_digest
                    .as_ref()
                    .map(<[u8; 32]>::as_slice),
                planned.success_criteria_digest.as_slice(),
                planned.action_proposal_digest.as_slice(),
            ],
        )?;
    }
    for (sequence, endpoint) in record.effect_endpoints.iter().enumerate() {
        transaction.execute(
            "INSERT INTO task_write_set_effect_endpoints (
                task_id, idempotency_key, endpoint_seq, effect_seq,
                endpoint_kind, object_id, participant_id,
                participant_generation, admission_receipt_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                record.task_id.as_bytes().as_slice(),
                record.idempotency_key.as_bytes().as_slice(),
                i64::try_from(sequence).map_err(|_| TaskStoreError::TaskWriteSetConflict {
                    reason: "effect endpoint set exceeds SQLite sequence range",
                })?,
                i64::try_from(endpoint.effect_seq).map_err(|_| {
                    TaskStoreError::TaskWriteSetConflict {
                        reason: "effect endpoint effect sequence exceeds SQLite range",
                    }
                })?,
                i64::from(endpoint.kind.code()),
                endpoint.object_id.as_slice(),
                endpoint.participant_id.as_bytes().as_slice(),
                encode_u64(endpoint.participant_generation.get()).as_slice(),
                endpoint.admission_receipt_id.as_bytes().as_slice(),
            ],
        )?;
    }
    Ok(())
}

const WRITE_SET_COLUMNS: &str = "task_id, attempt_id, attempt_generation,
     idempotency_key, snapshot_id, snapshot_receipt_id,
     expected_head_commit_seq, effect_history_root, retry_fence_epoch,
     group_id, membership_generation, membership_root, group_policy_digest,
     participant_registry_generation, participant_registry_root,
     artifact_read_set_root, semantic_read_set_root, semantic_append_set_root,
     artifact_write_set_root,
     resource_reservation_set_root, effect_set_root, effect_endpoint_set_root,
     write_set_root, sealed_at_ms";

fn load_write_set_by_key(
    source: &impl SqlRead,
    task_id: TaskId,
    idempotency_key: IdempotencyKey,
) -> Result<Option<TaskWriteSetRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {WRITE_SET_COLUMNS} FROM task_write_sets
         WHERE task_id = ?1 AND idempotency_key = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        idempotency_key.as_bytes().as_slice(),
    ])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let mut record = decode_write_set_row(row)?;
    record.artifact_reads = load_write_set_reads(source, task_id, idempotency_key)?;
    record.artifact_writes = load_write_set_artifact_writes(source, task_id, idempotency_key)?;
    record.process_binding = load_write_set_process_binding(source, task_id, idempotency_key)?;
    record.semantic_reads = load_write_set_semantic_reads(source, task_id, idempotency_key)?;
    record.semantic_appends = load_write_set_semantic_appends(source, task_id, idempotency_key)?;
    record.resource_reservations =
        load_write_set_resource_reservations(source, task_id, idempotency_key)?;
    record.planned_effects = load_write_set_planned_effects(source, task_id, idempotency_key)?;
    record.effect_endpoints = load_write_set_effect_endpoints(source, task_id, idempotency_key)?;
    validate_artifact_write_rows(&record)?;
    validate_semantic_append_rows(&record)?;
    validate_effect_endpoint_rows(&record)?;
    Ok(Some(record))
}

pub(crate) fn load_write_set_by_root(
    source: &impl SqlRead,
    task_id: TaskId,
    write_set_root: [u8; 32],
) -> Result<Option<TaskWriteSetRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {WRITE_SET_COLUMNS} FROM task_write_sets
         WHERE task_id = ?1 AND write_set_root = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        write_set_root.as_slice(),
    ])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let mut record = decode_write_set_row(row)?;
    record.artifact_reads = load_write_set_reads(source, task_id, record.idempotency_key)?;
    record.artifact_writes =
        load_write_set_artifact_writes(source, task_id, record.idempotency_key)?;
    record.process_binding =
        load_write_set_process_binding(source, task_id, record.idempotency_key)?;
    record.semantic_reads = load_write_set_semantic_reads(source, task_id, record.idempotency_key)?;
    record.semantic_appends =
        load_write_set_semantic_appends(source, task_id, record.idempotency_key)?;
    record.resource_reservations =
        load_write_set_resource_reservations(source, task_id, record.idempotency_key)?;
    record.planned_effects =
        load_write_set_planned_effects(source, task_id, record.idempotency_key)?;
    record.effect_endpoints =
        load_write_set_effect_endpoints(source, task_id, record.idempotency_key)?;
    validate_artifact_write_rows(&record)?;
    validate_semantic_append_rows(&record)?;
    validate_effect_endpoint_rows(&record)?;
    Ok(Some(record))
}

fn validate_effect_endpoint_rows(record: &TaskWriteSetRecord) -> Result<(), TaskStoreError> {
    if record.effect_endpoints.is_empty() {
        if record.effect_endpoint_set_root != [0; 32] {
            return Err(TaskStoreError::CorruptRecord(
                "empty effect endpoint set has a non-zero root",
            ));
        }
        return Ok(());
    }
    let effect_count = u64::try_from(record.planned_effects.len())
        .map_err(|_| TaskStoreError::CorruptRecord("planned effect count"))?;
    let mut seen = std::collections::BTreeSet::new();
    for endpoint in &record.effect_endpoints {
        if endpoint.effect_seq >= effect_count
            || !seen.insert((
                endpoint.effect_seq,
                endpoint.kind.code(),
                endpoint.object_id,
            ))
        {
            return Err(TaskStoreError::CorruptRecord(
                "effect endpoint sequence or uniqueness",
            ));
        }
    }
    if record.effect_endpoint_set_root
        != crate::model::effect_endpoint_set_root(&record.effect_endpoints)
    {
        return Err(TaskStoreError::CorruptRecord(
            "effect endpoint root mismatch",
        ));
    }
    Ok(())
}

fn validate_artifact_write_rows(record: &TaskWriteSetRecord) -> Result<(), TaskStoreError> {
    if record.artifact_writes.is_empty() {
        if record.artifact_write_set_root != [0; 32] {
            return Err(TaskStoreError::CorruptRecord(
                "empty Artifact write set has a non-zero root",
            ));
        }
        return Ok(());
    }
    let mut seen = std::collections::BTreeSet::new();
    for write in &record.artifact_writes {
        let expected_target = write
            .expected_head_revision
            .checked_add(1)
            .ok_or(TaskStoreError::CorruptRecord("Artifact write revision"))?;
        if write.proposed_revision != expected_target || !seen.insert(write.artifact_id) {
            return Err(TaskStoreError::CorruptRecord(
                "Artifact write revision or uniqueness",
            ));
        }
    }
    if record.artifact_write_set_root
        != crate::model::artifact_write_set_root(&record.artifact_writes)
    {
        return Err(TaskStoreError::CorruptRecord(
            "Artifact write root mismatch",
        ));
    }
    Ok(())
}

fn validate_semantic_append_rows(record: &TaskWriteSetRecord) -> Result<(), TaskStoreError> {
    if record.semantic_appends.is_empty() {
        if record.semantic_append_set_root != [0; 32] {
            return Err(TaskStoreError::CorruptRecord(
                "empty Semantic append set has a non-zero root",
            ));
        }
        return Ok(());
    }
    let mut seen = std::collections::BTreeSet::new();
    let has_admission_policy_digests = record
        .semantic_appends
        .iter()
        .any(|append| append.admission_policy_digest.is_some());
    for append in &record.semantic_appends {
        if append.required_durability != TaskWriteSetSemanticRequiredDurability::Durable
            || !seen.insert(append.event_id)
            || (has_admission_policy_digests && append.admission_policy_digest.is_none())
        {
            return Err(TaskStoreError::CorruptRecord(
                "Semantic append durability, policy declaration, or uniqueness",
            ));
        }
    }
    if record.semantic_append_set_root
        != crate::model::semantic_append_set_root(&record.semantic_appends)
    {
        return Err(TaskStoreError::CorruptRecord(
            "Semantic append root mismatch",
        ));
    }
    Ok(())
}

fn load_write_set_artifact_writes(
    source: &impl SqlRead,
    task_id: TaskId,
    idempotency_key: IdempotencyKey,
) -> Result<Vec<TaskWriteSetArtifactWrite>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT artifact_id, expected_head_revision, proposed_revision,
                content_digest, size_bytes
         FROM task_write_set_artifact_writes
         WHERE task_id = ?1 AND idempotency_key = ?2 ORDER BY write_seq",
    )?;
    let rows = statement.query_map(
        params![
            task_id.as_bytes().as_slice(),
            idempotency_key.as_bytes().as_slice()
        ],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, Vec<u8>>(4)?,
            ))
        },
    )?;
    rows.map(|row| {
        let (artifact_id, expected_head_revision, proposed_revision, content_digest, size_bytes) =
            row?;
        Ok(TaskWriteSetArtifactWrite {
            artifact_id: ArtifactId::from_bytes(
                artifact_id
                    .try_into()
                    .map_err(|_| TaskStoreError::CorruptRecord("Artifact write artifact id"))?,
            ),
            expected_head_revision: u64_from_bytes(expected_head_revision)?,
            proposed_revision: u64_from_bytes(proposed_revision)?,
            content_digest: content_digest
                .try_into()
                .map_err(|_| TaskStoreError::CorruptRecord("Artifact write content digest"))?,
            size_bytes: u64_from_bytes(size_bytes)?,
        })
    })
    .collect()
}

fn load_write_set_semantic_appends(
    source: &impl SqlRead,
    task_id: TaskId,
    idempotency_key: IdempotencyKey,
) -> Result<Vec<TaskWriteSetSemanticAppend>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT event_id, target_scope_kind, target_scope_id,
                required_durability, admission_receipt_id, durability_receipt_id,
                admission_policy_digest
         FROM task_write_set_semantic_appends
         WHERE task_id = ?1 AND idempotency_key = ?2 ORDER BY append_seq",
    )?;
    let rows = statement.query_map(
        params![
            task_id.as_bytes().as_slice(),
            idempotency_key.as_bytes().as_slice()
        ],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, Option<Vec<u8>>>(5)?,
                row.get::<_, Option<Vec<u8>>>(6)?,
            ))
        },
    )?;
    rows.map(|row| {
        let (
            event_id,
            target_scope_kind,
            target_scope_id,
            required_durability,
            receipt_id,
            durability_receipt_id,
            admission_policy_digest,
        ) = row?;
        let target_scope_id = target_scope_id
            .try_into()
            .map_err(|_| TaskStoreError::CorruptRecord("Semantic append target scope"))?;
        let target = match target_scope_kind {
            1 => TaskWriteSetSemanticTarget::Namespace(nlos_types::NamespaceId::from_bytes(
                target_scope_id,
            )),
            2 => TaskWriteSetSemanticTarget::Task(TaskId::from_bytes(target_scope_id)),
            _ => return Err(TaskStoreError::CorruptRecord("Semantic append target kind")),
        };
        if required_durability != i64::from(TaskWriteSetSemanticRequiredDurability::code()) {
            return Err(TaskStoreError::CorruptRecord("Semantic append durability"));
        }
        let durability_receipt_id = durability_receipt_id
            .map(|receipt_id| {
                receipt_id
                    .try_into()
                    .map(ReceiptId::from_bytes)
                    .map_err(|_| TaskStoreError::CorruptRecord("Semantic durability receipt id"))
            })
            .transpose()?;
        let admission_policy_digest = admission_policy_digest
            .map(|digest| {
                digest
                    .try_into()
                    .map_err(|_| TaskStoreError::CorruptRecord("Semantic admission policy digest"))
            })
            .transpose()?;
        Ok(TaskWriteSetSemanticAppend {
            event_id: nlos_types::SemanticEventId::from_bytes(
                event_id
                    .try_into()
                    .map_err(|_| TaskStoreError::CorruptRecord("Semantic append event id"))?,
            ),
            target,
            required_durability: TaskWriteSetSemanticRequiredDurability::Durable,
            admission_receipt_id: ReceiptId::from_bytes(
                receipt_id
                    .try_into()
                    .map_err(|_| TaskStoreError::CorruptRecord("Semantic append receipt id"))?,
            ),
            admission_policy_digest,
            durability_receipt_id,
        })
    })
    .collect()
}

fn load_write_set_planned_effects(
    source: &impl SqlRead,
    task_id: TaskId,
    idempotency_key: IdempotencyKey,
) -> Result<Vec<PlannedEffect>, TaskStoreError> {
    let task_generation = source
        .prepare_statement("SELECT task_generation FROM tasks WHERE task_id = ?1")?
        .query_row([task_id.as_bytes().as_slice()], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .map_err(TaskStoreError::Sqlite)
        .and_then(generation_from_u64_bytes)?;
    let mut statement = source.prepare_statement(
        "SELECT effect_seq, intent_spec_id, stable_action_slot,
                target_authority_object_id, effect_class, idempotency_scope,
                logical_effect_id, idempotency_identity_digest, required,
                required_condition_digest, success_criteria_digest,
                action_proposal_digest
         FROM task_write_set_planned_effects
         WHERE task_id = ?1 AND idempotency_key = ?2 ORDER BY effect_seq",
    )?;
    let rows = statement.query_map(
        params![
            task_id.as_bytes().as_slice(),
            idempotency_key.as_bytes().as_slice()
        ],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Vec<u8>>(6)?,
                row.get::<_, Vec<u8>>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, Option<Vec<u8>>>(9)?,
                row.get::<_, Vec<u8>>(10)?,
                row.get::<_, Vec<u8>>(11)?,
            ))
        },
    )?;
    let mut planned = Vec::new();
    for row in rows {
        let (
            effect_seq,
            intent_spec_id,
            stable_action_slot,
            target_authority_object_id,
            effect_class,
            idempotency_scope,
            logical_effect_id,
            idempotency_identity_digest,
            required,
            required_condition_digest,
            success_criteria_digest,
            action_proposal_digest,
        ) = row?;
        let effect_seq = u64::try_from(effect_seq)
            .map_err(|_| TaskStoreError::CorruptRecord("planned effect sequence"))?;
        if effect_seq != planned.len() as u64 {
            return Err(TaskStoreError::CorruptRecord("planned effect sequence"));
        }
        if !(0..=i64::from(u32::MAX)).contains(&effect_class)
            || !(0..=i64::from(u32::MAX)).contains(&idempotency_scope)
            || !matches!(required, 0 | 1)
        {
            return Err(TaskStoreError::CorruptRecord("planned effect scalar"));
        }
        let descriptor = crate::LogicalEffectDescriptor {
            task_id,
            task_generation,
            intent_spec_id: blob32_vec(intent_spec_id)?,
            stable_action_slot: u64_from_bytes(stable_action_slot)?,
            target_authority_object_id: blob32_vec(target_authority_object_id)?,
            effect_class: u32::try_from(effect_class)
                .map_err(|_| TaskStoreError::CorruptRecord("planned effect class"))?,
            idempotency_scope: u32::try_from(idempotency_scope)
                .map_err(|_| TaskStoreError::CorruptRecord("planned effect scope"))?,
        };
        if descriptor.logical_effect_id() != blob32_vec(logical_effect_id)?
            || descriptor.idempotency_identity_digest() != blob32_vec(idempotency_identity_digest)?
        {
            return Err(TaskStoreError::CorruptRecord("planned effect identity"));
        }
        planned.push(PlannedEffect {
            descriptor,
            required: required == 1,
            required_condition_digest: required_condition_digest.map(blob32_vec).transpose()?,
            success_criteria_digest: blob32_vec(success_criteria_digest)?,
            action_proposal_digest: blob32_vec(action_proposal_digest)?,
        });
    }
    Ok(planned)
}

fn load_write_set_effect_endpoints(
    source: &impl SqlRead,
    task_id: TaskId,
    idempotency_key: IdempotencyKey,
) -> Result<Vec<TaskWriteSetEffectEndpoint>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT effect_seq, endpoint_kind, object_id, participant_id,
                participant_generation, admission_receipt_id
         FROM task_write_set_effect_endpoints
         WHERE task_id = ?1 AND idempotency_key = ?2 ORDER BY endpoint_seq",
    )?;
    let rows = statement.query_map(
        params![
            task_id.as_bytes().as_slice(),
            idempotency_key.as_bytes().as_slice()
        ],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, Vec<u8>>(5)?,
            ))
        },
    )?;
    let mut endpoints = Vec::new();
    for row in rows {
        let (
            effect_seq,
            endpoint_kind,
            object_id,
            participant_id,
            participant_generation,
            admission_receipt_id,
        ) = row?;
        let effect_seq = u64::try_from(effect_seq)
            .map_err(|_| TaskStoreError::CorruptRecord("effect endpoint sequence"))?;
        endpoints.push(TaskWriteSetEffectEndpoint {
            effect_seq,
            kind: TaskWriteSetEffectEndpointKind::from_code(endpoint_kind)?,
            object_id: object_id
                .try_into()
                .map_err(|_| TaskStoreError::CorruptRecord("effect endpoint object id"))?,
            participant_id: nlos_types::TaskParticipantId::from_bytes(
                participant_id
                    .try_into()
                    .map_err(|_| TaskStoreError::CorruptRecord("effect endpoint participant id"))?,
            ),
            participant_generation: generation_from_u64_bytes(participant_generation)?,
            admission_receipt_id: ReceiptId::from_bytes(
                admission_receipt_id
                    .try_into()
                    .map_err(|_| TaskStoreError::CorruptRecord("effect endpoint receipt id"))?,
            ),
        });
    }
    if endpoints
        .windows(2)
        .any(|pair| pair[0].effect_seq > pair[1].effect_seq)
    {
        return Err(TaskStoreError::CorruptRecord(
            "effect endpoint sequence ordering",
        ));
    }
    Ok(endpoints)
}

fn load_write_set_semantic_reads(
    source: &impl SqlRead,
    task_id: TaskId,
    idempotency_key: IdempotencyKey,
) -> Result<Vec<crate::TaskWriteSetSemanticRead>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT event_id, expected_log_seq, expected_canonical_digest
         FROM task_write_set_semantic_reads
         WHERE task_id = ?1 AND idempotency_key = ?2 ORDER BY read_seq",
    )?;
    let rows = statement.query_map(
        params![
            task_id.as_bytes().as_slice(),
            idempotency_key.as_bytes().as_slice()
        ],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        },
    )?;
    rows.map(|row| {
        let (event_id, log_seq, digest) = row?;
        Ok(crate::TaskWriteSetSemanticRead {
            event_id: nlos_types::SemanticEventId::from_bytes(blob32_vec(event_id)?),
            expected_log_seq: u64_from_bytes(log_seq)?,
            expected_canonical_digest: digest
                .try_into()
                .map_err(|_| TaskStoreError::CorruptRecord("semantic read digest"))?,
        })
    })
    .collect()
}

fn load_write_set_resource_reservations(
    source: &impl SqlRead,
    task_id: TaskId,
    idempotency_key: IdempotencyKey,
) -> Result<Vec<crate::TaskWriteSetResourceReservation>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT reservation_id, account_id, quote_id, call_id, operation_id,
                driver_id, device_id, driver_generation, driver_fencing_token,
                upper_bound
         FROM task_write_set_resource_reservations
         WHERE task_id = ?1 AND idempotency_key = ?2 ORDER BY read_seq",
    )?;
    let rows = statement.query_map(
        params![
            task_id.as_bytes().as_slice(),
            idempotency_key.as_bytes().as_slice()
        ],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, Vec<u8>>(5)?,
                row.get::<_, Vec<u8>>(6)?,
                row.get::<_, Vec<u8>>(7)?,
                row.get::<_, Vec<u8>>(8)?,
                row.get::<_, Vec<u8>>(9)?,
            ))
        },
    )?;
    rows.map(|row| {
        let (
            reservation_id,
            account_id,
            quote_id,
            call_id,
            operation_id,
            driver_id,
            device_id,
            driver_generation,
            driver_fencing_token,
            upper_bound,
        ) = row?;
        Ok(crate::TaskWriteSetResourceReservation {
            reservation_id: nlos_types::ReservationId::from_bytes(blob16_vec(reservation_id)?),
            account_id: nlos_types::ResourceAccountId::from_bytes(blob16_vec(account_id)?),
            quote_id: nlos_types::QuoteId::from_bytes(blob16_vec(quote_id)?),
            call_id: nlos_types::CallId::from_bytes(blob16_vec(call_id)?),
            operation_id: nlos_types::OperationId::from_bytes(blob16_vec(operation_id)?),
            driver_id: nlos_types::DriverId::from_bytes(blob16_vec(driver_id)?),
            device_id: nlos_types::DeviceId::from_bytes(blob16_vec(device_id)?),
            driver_generation: generation_from_u64_bytes(driver_generation)?,
            driver_fencing_token: driver_fencing_token
                .try_into()
                .map_err(|_| TaskStoreError::CorruptRecord("reservation driver fence"))?,
            upper_bound: u64_from_bytes(upper_bound)?,
        })
    })
    .collect()
}

fn blob16_vec(bytes: Vec<u8>) -> Result<[u8; 16], TaskStoreError> {
    bytes
        .try_into()
        .map_err(|_| TaskStoreError::CorruptRecord("16-byte write-set identity"))
}

fn blob32_vec(bytes: Vec<u8>) -> Result<[u8; 32], TaskStoreError> {
    bytes
        .try_into()
        .map_err(|_| TaskStoreError::CorruptRecord("32-byte write-set identity"))
}

fn generation_from_u64_bytes(bytes: Vec<u8>) -> Result<Generation, TaskStoreError> {
    let value = u64_from_bytes(bytes)?;
    std::num::NonZeroU64::new(value)
        .map(Generation::new)
        .ok_or(TaskStoreError::CorruptRecord("zero reservation generation"))
}

fn load_write_set_process_binding(
    source: &impl SqlRead,
    task_id: TaskId,
    idempotency_key: IdempotencyKey,
) -> Result<Option<crate::TaskWriteSetProcessBinding>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT process_id, process_generation, process_fencing_token,
                agent_instance_id, agent_instance_generation, isolation_domain_id,
                isolation_domain_generation, isolation_domain_fencing_token,
                participant_id, participant_generation, admission_receipt_id
         FROM task_write_set_process_bindings
         WHERE task_id = ?1 AND idempotency_key = ?2",
    )?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        idempotency_key.as_bytes().as_slice(),
    ])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(crate::TaskWriteSetProcessBinding {
        process_id: ProcessId::from_bytes(blob16(row, 0)?),
        process_generation: generation_from_blob(row, 1)?,
        process_fencing_token: blob32(row, 2)?,
        agent_instance_id: nlos_types::AgentInstanceId::from_bytes(blob16(row, 3)?),
        agent_instance_generation: generation_from_blob(row, 4)?,
        isolation_domain_id: nlos_types::IsolationDomainId::from_bytes(blob16(row, 5)?),
        isolation_domain_generation: generation_from_blob(row, 6)?,
        isolation_domain_fencing_token: blob32(row, 7)?,
        participant_id: nlos_types::TaskParticipantId::from_bytes(blob16(row, 8)?),
        participant_generation: generation_from_blob(row, 9)?,
        admission_receipt_id: ReceiptId::from_bytes(blob16(row, 10)?),
    }))
}

fn load_write_set_reads(
    source: &impl SqlRead,
    task_id: TaskId,
    idempotency_key: IdempotencyKey,
) -> Result<Vec<TaskWriteSetArtifactRead>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT artifact_id, expected_head_revision, expected_head_digest
         FROM task_write_set_artifact_reads
         WHERE task_id = ?1 AND idempotency_key = ?2 ORDER BY read_seq",
    )?;
    let rows = statement.query_map(
        params![
            task_id.as_bytes().as_slice(),
            idempotency_key.as_bytes().as_slice()
        ],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Option<Vec<u8>>>(2)?,
            ))
        },
    )?;
    rows.map(|row| {
        let (artifact_id, revision, digest) = row?;
        Ok(TaskWriteSetArtifactRead {
            artifact_id: ArtifactId::from_bytes(
                artifact_id
                    .try_into()
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
            ),
            expected_head_revision: u64_from_bytes(revision)?,
            expected_head_digest: digest
                .map(|value| {
                    value
                        .try_into()
                        .map_err(|_| TaskStoreError::CorruptRecord("write set digest"))
                })
                .transpose()?,
        })
    })
    .collect()
}

fn decode_write_set_row(row: &rusqlite::Row<'_>) -> Result<TaskWriteSetRecord, TaskStoreError> {
    let participant_generation = u64_from_blob(row, 13)?;
    let participant_root = blob32(row, 14)?;
    Ok(TaskWriteSetRecord {
        task_id: TaskId::from_bytes(blob16(row, 0)?),
        attempt_id: TaskAttemptId::from_bytes(blob16(row, 1)?),
        attempt_generation: generation_from_blob(row, 2)?,
        idempotency_key: IdempotencyKey::from_bytes(blob16(row, 3)?),
        snapshot_id: TaskSnapshotId::from_bytes(blob16(row, 4)?),
        snapshot_receipt_id: ReceiptId::from_bytes(blob16(row, 5)?),
        expected_head_commit_seq: u64_from_blob(row, 6)?,
        effect_history_root: blob32(row, 7)?,
        retry_fence_epoch: u64_from_blob(row, 8)?,
        group_binding: decode_group_binding(row, 9)?,
        participant_registry_binding: crate::ParticipantRegistryBinding {
            generation: participant_generation,
            root: participant_root,
        },
        artifact_reads: Vec::new(),
        process_binding: None,
        semantic_reads: Vec::new(),
        semantic_appends: Vec::new(),
        resource_reservations: Vec::new(),
        artifact_writes: Vec::new(),
        planned_effects: Vec::new(),
        effect_endpoints: Vec::new(),
        artifact_read_set_root: blob32(row, 15)?,
        semantic_read_set_root: blob32(row, 16)?,
        semantic_append_set_root: blob32(row, 17)?,
        artifact_write_set_root: blob32(row, 18)?,
        resource_reservation_set_root: blob32(row, 19)?,
        effect_set_root: blob32(row, 20)?,
        effect_endpoint_set_root: blob32(row, 21)?,
        write_set_root: blob32(row, 22)?,
        sealed_at_ms: row.get(23)?,
    })
}

const PERMIT_COLUMNS: &str = "permit_id, task_id, idempotency_key, attempt_id, attempt_generation,
     expected_head_commit_seq, expected_effect_history_root,
     expected_retry_fence_epoch, write_set_root, permit_epoch,
     control_epoch, cancel_epoch, valid_until_ms, permit_state,
     created_at_ms, updated_at_ms, group_id, membership_generation,
     membership_root, group_policy_digest, participant_registry_generation,
     participant_registry_root, authority_lease_authority_id,
     authority_lease_holder_id, authority_lease_term, authority_lease_epoch,
     authority_lease_fencing_token, authority_lease_expires_at_ms";

fn load_permit_by_key(
    source: &impl SqlRead,
    task_id: TaskId,
    idempotency_key: IdempotencyKey,
) -> Result<Option<PermitRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {PERMIT_COLUMNS} FROM commit_permits
         WHERE task_id = ?1 AND idempotency_key = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        idempotency_key.as_bytes().as_slice(),
    ])?;
    rows.next()?.map(decode_permit_row).transpose()
}

pub(crate) fn load_permit_by_id(
    source: &impl SqlRead,
    task_id: TaskId,
    permit_id: CommitPermitId,
) -> Result<PermitRecord, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {PERMIT_COLUMNS} FROM commit_permits
         WHERE task_id = ?1 AND permit_id = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        permit_id.as_bytes().as_slice(),
    ])?;
    rows.next()?
        .map(decode_permit_row)
        .transpose()?
        .ok_or(TaskStoreError::PermitNotFound)
}

fn load_active_permit(
    source: &impl SqlRead,
    task_id: TaskId,
) -> Result<Option<PermitRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {PERMIT_COLUMNS} FROM commit_permits
         WHERE task_id = ?1 AND permit_state = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        PermitState::Issued.code(),
    ])?;
    rows.next()?.map(decode_permit_row).transpose()
}

/// The outstanding permit for head reporting: `Issued` or the
/// non-reusable `Quarantined` tombstone (`[TASK-EFFECT-003]`).
fn load_outstanding_permit(
    source: &impl SqlRead,
    task_id: TaskId,
) -> Result<Option<PermitRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {PERMIT_COLUMNS} FROM commit_permits
         WHERE task_id = ?1 AND permit_state IN (?2, ?3)"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        PermitState::Issued.code(),
        PermitState::Quarantined.code(),
    ])?;
    rows.next()?.map(decode_permit_row).transpose()
}

fn load_outstanding_operation_participants(
    source: &impl SqlRead,
    task_id: TaskId,
) -> Result<Option<Vec<crate::ParticipantRecord>>, TaskStoreError> {
    let Some(permit) = load_outstanding_permit(source, task_id)? else {
        return Ok(Some(Vec::new()));
    };
    let Some(write_set) = load_write_set_by_root(source, task_id, permit.write_set_root)? else {
        return Ok(None);
    };
    if !write_set.artifact_reads.is_empty()
        || !write_set.artifact_writes.is_empty()
        || !write_set.semantic_reads.is_empty()
        || !write_set.semantic_appends.is_empty()
        || !write_set.resource_reservations.is_empty()
        || !write_set.planned_effects.is_empty() && write_set.effect_endpoints.is_empty()
    {
        return Ok(None);
    }
    let mut participants = write_set
        .process_binding
        .iter()
        .map(|binding| crate::ParticipantRecord {
            participant_type: crate::ParticipantType::ProcessBinding,
            participant_id: binding.participant_id,
            participant_generation: binding.participant_generation,
            admission_receipt_id: binding.admission_receipt_id,
        })
        .collect::<Vec<_>>();
    participants.extend(
        write_set
            .effect_endpoints
            .iter()
            .map(effect_endpoint_participant),
    );
    Ok(Some(participants))
}

fn effect_endpoint_participant(endpoint: &TaskWriteSetEffectEndpoint) -> crate::ParticipantRecord {
    let participant_type = match endpoint.kind {
        TaskWriteSetEffectEndpointKind::ArtifactHead => crate::ParticipantType::ArtifactHead,
        TaskWriteSetEffectEndpointKind::SemanticAdmission => {
            crate::ParticipantType::SemanticAdmission
        }
        TaskWriteSetEffectEndpointKind::ProcessBinding => crate::ParticipantType::ProcessBinding,
        TaskWriteSetEffectEndpointKind::DriverGateway => crate::ParticipantType::DriverGateway,
        TaskWriteSetEffectEndpointKind::ResourceLedger => crate::ParticipantType::ResourceLedger,
        TaskWriteSetEffectEndpointKind::OperationBinding => {
            crate::ParticipantType::OperationBinding
        }
        TaskWriteSetEffectEndpointKind::ChannelTopicBinding => crate::ParticipantType::ChannelTopic,
    };
    crate::ParticipantRecord {
        participant_type,
        participant_id: endpoint.participant_id,
        participant_generation: endpoint.participant_generation,
        admission_receipt_id: endpoint.admission_receipt_id,
    }
}

/// The quarantine tombstone blocking new winner issuance, if any
/// (`[TASK-COMMIT-003]`).
pub(crate) fn load_quarantined_permit(
    source: &impl SqlRead,
    task_id: TaskId,
) -> Result<Option<PermitRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {PERMIT_COLUMNS} FROM commit_permits
         WHERE task_id = ?1 AND permit_state = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        PermitState::Quarantined.code(),
    ])?;
    rows.next()?.map(decode_permit_row).transpose()
}

fn decode_permit_row(row: &rusqlite::Row<'_>) -> Result<PermitRecord, TaskStoreError> {
    let group_binding = decode_group_binding(row, 16)?;
    let participant_generation = optional_blob::<8>(row, 20)?;
    let participant_root = optional_blob::<32>(row, 21)?;
    let participant_registry_binding = match (participant_generation, participant_root) {
        (Some(generation), Some(root)) => Some(crate::ParticipantRegistryBinding {
            generation: u64::from_be_bytes(generation),
            root,
        }),
        (None, None) => None,
        _ => {
            return Err(TaskStoreError::CorruptRecord(
                "partial participant registry permit binding",
            ));
        }
    };
    let authority_lease_authority_id = optional_blob::<16>(row, 22)?;
    let authority_lease_holder_id = optional_blob::<16>(row, 23)?;
    let authority_lease_term = optional_blob::<8>(row, 24)?;
    let authority_lease_epoch = optional_blob::<8>(row, 25)?;
    let authority_lease_fencing_token = optional_blob::<32>(row, 26)?;
    let authority_lease_expires_at_ms: Option<i64> = row.get(27)?;
    let authority_lease_binding = match (
        authority_lease_authority_id,
        authority_lease_holder_id,
        authority_lease_term,
        authority_lease_epoch,
        authority_lease_fencing_token,
        authority_lease_expires_at_ms,
    ) {
        (
            Some(authority_id),
            Some(holder_id),
            Some(term),
            Some(lease_epoch),
            Some(fencing_token),
            Some(expires_at_ms),
        ) => Some(crate::lease::AuthorityLeaseBinding {
            authority_id: nlos_types::TaskParticipantId::from_bytes(authority_id),
            holder_id: ProcessId::from_bytes(holder_id),
            term: u64::from_be_bytes(term),
            lease_epoch: u64::from_be_bytes(lease_epoch),
            fencing_token,
            expires_at_ms,
        }),
        (None, None, None, None, None, None) => None,
        _ => {
            return Err(TaskStoreError::CorruptRecord(
                "partial authority lease permit binding",
            ));
        }
    };
    Ok(PermitRecord {
        permit_id: CommitPermitId::from_bytes(blob16(row, 0)?),
        task_id: TaskId::from_bytes(blob16(row, 1)?),
        idempotency_key: IdempotencyKey::from_bytes(blob16(row, 2)?),
        attempt_id: TaskAttemptId::from_bytes(blob16(row, 3)?),
        attempt_generation: generation_from_blob(row, 4)?,
        expected_head_commit_seq: u64_from_blob(row, 5)?,
        expected_effect_history_root: blob32(row, 6)?,
        expected_retry_fence_epoch: u64_from_blob(row, 7)?,
        write_set_root: blob32(row, 8)?,
        group_binding,
        participant_registry_binding,
        authority_lease_binding,
        permit_epoch: u64_from_blob(row, 9)?,
        control_epoch: u64_from_blob(row, 10)?,
        cancel_epoch: u64_from_blob(row, 11)?,
        valid_until_ms: row.get(12)?,
        state: PermitState::from_code(row.get(13)?)?,
        created_at_ms: row.get(14)?,
        updated_at_ms: row.get(15)?,
    })
}

pub(crate) fn close_permit(
    transaction: &Transaction<'_>,
    permit: &PermitRecord,
    now_ms: i64,
) -> Result<(), TaskStoreError> {
    let changed = transaction.execute(
        "UPDATE commit_permits
         SET permit_state = ?1, updated_at_ms = ?2
         WHERE permit_id = ?3 AND permit_state = ?4",
        params![
            PermitState::Closed.code(),
            now_ms,
            permit.permit_id.as_bytes().as_slice(),
            PermitState::Issued.code(),
        ],
    )?;
    if changed != 1 {
        return Err(TaskStoreError::CorruptRecord(
            "permit close compare-and-swap failed",
        ));
    }
    Ok(())
}

fn insert_cancel(
    transaction: &Transaction<'_>,
    task_id: TaskId,
    idempotency_key: IdempotencyKey,
    cancel_epoch_after: u64,
    now_ms: i64,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_cancels (task_id, idempotency_key, cancel_epoch_after, created_at_ms)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            task_id.as_bytes().as_slice(),
            idempotency_key.as_bytes().as_slice(),
            encode_u64(cancel_epoch_after).as_slice(),
            now_ms,
        ],
    )?;
    Ok(())
}

fn load_cancel(
    source: &impl SqlRead,
    task_id: TaskId,
) -> Result<Option<StoredCancel>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT idempotency_key, cancel_epoch_after FROM task_cancels WHERE task_id = ?1",
    )?;
    let mut rows = statement.query([task_id.as_bytes().as_slice()])?;
    rows.next()?
        .map(|row| {
            Ok(StoredCancel {
                idempotency_key: IdempotencyKey::from_bytes(blob16(row, 0)?),
                cancel_epoch_after: u64_from_blob(row, 1)?,
            })
        })
        .transpose()
}

pub(crate) fn insert_receipt(
    transaction: &Transaction<'_>,
    record: &TaskReceiptRecord,
) -> Result<(), TaskStoreError> {
    if !record.outcome.is_producible() {
        return Err(TaskStoreError::CorruptRecord(
            "reserved receipt outcome is not producible in this slice",
        ));
    }
    if record.permit_id.is_some() && record.participant_registry_binding.is_none() {
        return Err(TaskStoreError::ParticipantRegistryBindingMissing);
    }
    let group_id = record
        .group_binding
        .map(|binding| binding.group_id.into_bytes());
    let membership_generation = record
        .group_binding
        .map(|binding| encode_u64(binding.membership_generation));
    let membership_root = record.group_binding.map(|binding| binding.membership_root);
    let group_policy_digest = record
        .group_binding
        .map(|binding| binding.group_policy_digest);
    let participant_generation = record
        .participant_registry_binding
        .map(|binding| encode_u64(binding.generation));
    let participant_root = record
        .participant_registry_binding
        .map(|binding| binding.root);
    transaction.execute(
        "INSERT INTO task_receipts (
            receipt_id, task_id, permit_id, attempt_id, attempt_generation,
            outcome, prior_head_commit_seq, prior_effect_history_root,
            prior_retry_fence_epoch, new_head_commit_seq,
            new_effect_history_root, new_retry_fence_epoch, created_at_ms,
            group_id, membership_generation, membership_root, group_policy_digest,
            participant_registry_generation, participant_registry_root
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                   ?14, ?15, ?16, ?17, ?18, ?19)",
        params![
            record.receipt_id.as_bytes().as_slice(),
            record.task_id.as_bytes().as_slice(),
            record
                .permit_id
                .map(CommitPermitId::into_bytes)
                .as_ref()
                .map(<[u8; 16]>::as_slice),
            record.attempt_id.as_bytes().as_slice(),
            encode_u64(record.attempt_generation.get()).as_slice(),
            record.outcome.code(),
            encode_u64(record.prior_head_commit_seq).as_slice(),
            record.prior_effect_history_root.as_slice(),
            encode_u64(record.prior_retry_fence_epoch).as_slice(),
            encode_u64(record.new_head_commit_seq).as_slice(),
            record.new_effect_history_root.as_slice(),
            encode_u64(record.new_retry_fence_epoch).as_slice(),
            record.created_at_ms,
            group_id.as_ref().map(<[u8; 16]>::as_slice),
            membership_generation.as_ref().map(<[u8; 8]>::as_slice),
            membership_root.as_ref().map(<[u8; 32]>::as_slice),
            group_policy_digest.as_ref().map(<[u8; 32]>::as_slice),
            participant_generation.as_ref().map(<[u8; 8]>::as_slice),
            participant_root.as_ref().map(<[u8; 32]>::as_slice),
        ],
    )?;
    Ok(())
}

const RECEIPT_COLUMNS: &str = "receipt_id, task_id, permit_id, attempt_id, attempt_generation,
     outcome, prior_head_commit_seq, prior_effect_history_root,
     prior_retry_fence_epoch, new_head_commit_seq,
     new_effect_history_root, new_retry_fence_epoch, created_at_ms,
     group_id, membership_generation, membership_root, group_policy_digest,
     participant_registry_generation, participant_registry_root";

pub(crate) fn load_receipt(
    source: &impl SqlRead,
    task_id: TaskId,
    receipt_id: ReceiptId,
) -> Result<TaskReceiptRecord, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {RECEIPT_COLUMNS} FROM task_receipts
         WHERE task_id = ?1 AND receipt_id = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        receipt_id.as_bytes().as_slice(),
    ])?;
    rows.next()?
        .map(decode_receipt_row)
        .transpose()?
        .ok_or(TaskStoreError::ReceiptNotFound)
}

pub(crate) fn load_receipt_by_permit(
    source: &impl SqlRead,
    task_id: TaskId,
    permit_id: CommitPermitId,
) -> Result<Option<TaskReceiptRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {RECEIPT_COLUMNS} FROM task_receipts
         WHERE task_id = ?1 AND permit_id = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        permit_id.as_bytes().as_slice(),
    ])?;
    rows.next()?.map(decode_receipt_row).transpose()
}

fn decode_receipt_row(row: &rusqlite::Row<'_>) -> Result<TaskReceiptRecord, TaskStoreError> {
    Ok(TaskReceiptRecord {
        receipt_id: ReceiptId::from_bytes(blob16(row, 0)?),
        task_id: TaskId::from_bytes(blob16(row, 1)?),
        permit_id: optional_blob16(row, 2)?.map(CommitPermitId::from_bytes),
        attempt_id: TaskAttemptId::from_bytes(blob16(row, 3)?),
        attempt_generation: generation_from_blob(row, 4)?,
        group_binding: decode_group_binding(row, 13)?,
        participant_registry_binding: decode_participant_binding(row, 17)?,
        outcome: ReceiptOutcome::from_code(row.get(5)?)?,
        prior_head_commit_seq: u64_from_blob(row, 6)?,
        prior_effect_history_root: blob32(row, 7)?,
        prior_retry_fence_epoch: u64_from_blob(row, 8)?,
        new_head_commit_seq: u64_from_blob(row, 9)?,
        new_effect_history_root: blob32(row, 10)?,
        new_retry_fence_epoch: u64_from_blob(row, 11)?,
        created_at_ms: row.get(12)?,
    })
}

fn decode_group_binding(
    row: &rusqlite::Row<'_>,
    first_index: usize,
) -> Result<Option<crate::TaskGroupCommitBinding>, TaskStoreError> {
    let group_id = optional_blob::<16>(row, first_index)?;
    let generation = optional_blob::<8>(row, first_index + 1)?;
    let root = optional_blob::<32>(row, first_index + 2)?;
    let policy = optional_blob::<32>(row, first_index + 3)?;
    match (group_id, generation, root, policy) {
        (None, None, None, None) => Ok(None),
        (Some(group_id), Some(generation), Some(root), Some(policy)) => {
            Ok(Some(crate::TaskGroupCommitBinding {
                group_id: crate::TaskGroupId::from_bytes(group_id),
                membership_generation: u64::from_be_bytes(generation),
                membership_root: root,
                group_policy_digest: policy,
            }))
        }
        _ => Err(TaskStoreError::CorruptRecord(
            "partial task group commit binding",
        )),
    }
}

pub(crate) fn decode_participant_binding(
    row: &rusqlite::Row<'_>,
    first_index: usize,
) -> Result<Option<crate::ParticipantRegistryBinding>, TaskStoreError> {
    let generation = optional_blob::<8>(row, first_index)?;
    let root = optional_blob::<32>(row, first_index + 1)?;
    match (generation, root) {
        (None, None) => Ok(None),
        (Some(generation), Some(root)) => Ok(Some(crate::ParticipantRegistryBinding {
            generation: u64::from_be_bytes(generation),
            root,
        })),
        _ => Err(TaskStoreError::CorruptRecord(
            "partial participant registry binding",
        )),
    }
}

pub(crate) const fn encode_u64(value: u64) -> [u8; 8] {
    value.to_be_bytes()
}

pub(crate) fn generation_from_blob(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<Generation, TaskStoreError> {
    let value = u64_from_blob(row, index)?;
    let non_zero =
        std::num::NonZeroU64::new(value).ok_or(TaskStoreError::CorruptRecord("zero generation"))?;
    Ok(Generation::new(non_zero))
}

pub(crate) fn u64_from_blob(row: &rusqlite::Row<'_>, index: usize) -> Result<u64, TaskStoreError> {
    Ok(u64::from_be_bytes(blob8(row, index)?))
}

fn u64_from_bytes(bytes: Vec<u8>) -> Result<u64, TaskStoreError> {
    Ok(u64::from_be_bytes(bytes.try_into().map_err(|_| {
        TaskStoreError::CorruptRecord("expected 8-byte integer")
    })?))
}

pub(crate) fn blob16(row: &rusqlite::Row<'_>, index: usize) -> Result<[u8; 16], TaskStoreError> {
    let value: Vec<u8> = row.get(index)?;
    value
        .try_into()
        .map_err(|_| TaskStoreError::CorruptRecord("expected 16-byte blob"))
}

pub(crate) fn blob32(row: &rusqlite::Row<'_>, index: usize) -> Result<[u8; 32], TaskStoreError> {
    let value: Vec<u8> = row.get(index)?;
    value
        .try_into()
        .map_err(|_| TaskStoreError::CorruptRecord("expected 32-byte blob"))
}

fn blob64(row: &rusqlite::Row<'_>, index: usize) -> Result<[u8; 64], TaskStoreError> {
    let value: Vec<u8> = row.get(index)?;
    value
        .try_into()
        .map_err(|_| TaskStoreError::CorruptRecord("expected 64-byte blob"))
}

pub(crate) fn optional_blob16(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<Option<[u8; 16]>, TaskStoreError> {
    let value: Option<Vec<u8>> = row.get(index)?;
    value
        .map(|bytes| {
            bytes
                .try_into()
                .map_err(|_| TaskStoreError::CorruptRecord("expected optional 16-byte blob"))
        })
        .transpose()
}

pub(crate) fn optional_blob<const N: usize>(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<Option<[u8; N]>, TaskStoreError> {
    let value: Option<Vec<u8>> = row.get(index)?;
    value
        .map(|bytes| {
            bytes
                .try_into()
                .map_err(|_| TaskStoreError::CorruptRecord("unexpected optional blob width"))
        })
        .transpose()
}

fn blob8(row: &rusqlite::Row<'_>, index: usize) -> Result<[u8; 8], TaskStoreError> {
    let value: Vec<u8> = row.get(index)?;
    value
        .try_into()
        .map_err(|_| TaskStoreError::CorruptRecord("expected 8-byte blob"))
}
