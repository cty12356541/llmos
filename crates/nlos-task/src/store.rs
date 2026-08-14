//! Single-writer `SQLite` implementation of the durable task authority.
//!
//! The process-local mutex is an admission gate only; `BEGIN IMMEDIATE`
//! remains the storage-level writer fence, identical to `nlos-store`. Every
//! linearized decision (permit CAS, cancellation, finalize) commits its
//! state transition, epoch advance, and receipt in one transaction, so a
//! crash cannot split a decision from its durable record.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_types::{
    ArtifactId, CancellationScopeId, CommitPermitId, Generation, IdempotencyKey, ProcessId,
    ReceiptId, TaskAttemptId, TaskId, TaskSnapshotId,
};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
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
    TaskWriteSetRequest,
};

const SCHEMA_VERSION: i64 = 20;

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
    pub fn seal_task_write_set(
        &self,
        artifact_authority: &nlos_artifact::ArtifactStore,
        request: TaskWriteSetRequest,
    ) -> Result<TaskWriteSetDecision, TaskStoreError> {
        self.seal_task_write_set_inner(artifact_authority, None, None, None, request)
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
            request,
        )
    }

    #[allow(clippy::needless_pass_by_value)]
    #[allow(clippy::too_many_lines)]
    fn seal_task_write_set_inner(
        &self,
        artifact_authority: &nlos_artifact::ArtifactStore,
        process_authority: Option<&nlos_process::ProcessAuthority>,
        semantic_authority: Option<&nlos_semantic::SemanticAuthority>,
        resource_authority: Option<&nlos_resource::ResourceAuthority>,
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
            let head = artifact_authority
                .resolve_head(read.artifact_id)
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
            let head = artifact_authority
                .resolve_head(write.artifact_id)
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
        let semantic_endpoint = if semantic_reads.is_empty() {
            None
        } else {
            let authority = semantic_authority.ok_or(TaskStoreError::TaskWriteSetConflict {
                reason: "Semantic reads require SemanticAuthority readback",
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
            resource_reservations,
            planned_effects,
            effect_endpoints,
            artifact_read_set_root,
            semantic_read_set_root,
            resource_reservation_set_root,
            effect_set_root,
            effect_endpoint_set_root,
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
    pub fn request_commit_permit(
        &self,
        request: PermitRequest,
    ) -> Result<PermitDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = load_task(&transaction, request.task_id)?;
        if let Some(existing) =
            load_permit_by_key(&transaction, request.task_id, request.idempotency_key)?
        {
            let decision = replay_permit(&transaction, existing, &request)?;
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
        let decision = compete_for_permit(&transaction, &task, &attempt, &request)?;
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

fn replay_permit(
    transaction: &Transaction<'_>,
    existing: PermitRecord,
    request: &PermitRequest,
) -> Result<PermitDecision, TaskStoreError> {
    let stored_root = crate::effect::stored_effect_set_root(transaction, existing.permit_id)?
        .unwrap_or_else(crate::effect::empty_effect_set_root);
    let same_bytes = existing.attempt_id == request.attempt_id
        && existing.attempt_generation == request.attempt_generation
        && existing.write_set_root == request.write_set_root
        && existing.valid_until_ms == request.valid_until_ms
        && stored_root == crate::effect::effect_set_root_of(&request.planned_effects);
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

fn compete_for_permit(
    transaction: &Transaction<'_>,
    task: &StoredTask,
    attempt: &AttemptRecord,
    request: &PermitRequest,
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
    let record = issue_permit(transaction, task, attempt, request)?;
    Ok(PermitDecision::Issued(Box::new(record)))
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

#[allow(clippy::too_many_lines)]
fn issue_permit(
    transaction: &Transaction<'_>,
    task: &StoredTask,
    attempt: &AttemptRecord,
    request: &PermitRequest,
) -> Result<PermitRecord, TaskStoreError> {
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

fn migrate_v1(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V1_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v1 → v2 is purely additive (new effect-plane tables + `user_version`),
/// committed in one transaction: a failure anywhere rolls back to a
/// complete v1 database, never a half-migrated one.
fn migrate_v2(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(crate::effect::SCHEMA_V2_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v2 → v3 is purely additive (effect history + quarantine/adoption/
/// reconcile receipts + monotonic sequences + `user_version`), committed
/// in one transaction: a failure anywhere rolls back to a complete v2
/// database, never a half-migrated one.
fn migrate_v3(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(crate::effect::SCHEMA_V3_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v3 → v4 is purely additive (`TaskGroup` plane: groups, members,
/// admission/removal receipts, group cancels, attempt group bindings +
/// `user_version`), committed in one transaction: a failure anywhere
/// rolls back to a complete v3 database, never a half-migrated one.
fn migrate_v4(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(crate::group::SCHEMA_V4_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v4 → v5 is purely additive: nullable group-binding columns are added to
/// permits and receipts. Existing ungrouped/v1-v4 rows decode as `None`;
/// new grouped permits persist all four fields and copy them verbatim into
/// their terminal receipt.
fn migrate_v5(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V5_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v5 → v6 adds the immutable Artifact publication plan bound to one
/// outstanding permit. It records intent only; publication authorization
/// and receipt consumption are later state transitions.
fn migrate_v6(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(crate::commit::SCHEMA_V6_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v6 → v7 adds immutable nested Artifact publication receipt evidence.
fn migrate_v7(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(crate::commit::SCHEMA_V7_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v7 → v8 adds the mutable per-plan recovery scheduling and escalation
/// ledger. Canonical plan/publication/Task receipts remain immutable.
fn migrate_v8(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(crate::recovery::SCHEMA_V8_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v8 → v9 adds immutable acknowledgements for individual durable
/// escalation instances. Recovery scheduling remains in the mutable ledger.
fn migrate_v9(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(crate::recovery::SCHEMA_V9_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v9 → v10 adds immutable snapshot receipts, their ordered authority
/// checkpoint receipt set, and an optional binding on attempts. Existing
/// attempts remain explicitly legacy/unreceipted rather than receiving
/// invented proof during migration.
fn migrate_v10(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V10_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v10 → v11 adds authority-assigned `TaskStore` participant identity,
/// versioned participant registries/receipts, and permit-time registry
/// generation/root bindings. Existing permits remain explicitly unbound.
fn migrate_v11(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name IN (
            'task_authority_identity', 'task_participant_registries',
            'task_participants', 'task_participant_registry_receipts'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='trigger' AND (
            name LIKE 'task_participant_registry%'
            OR name LIKE 'task_participants_%'
            OR name LIKE 'task_authority_identity_%'
         )",
        [],
        |row| row.get(0),
    )?;
    let mut has_generation = false;
    let mut has_root = false;
    {
        let mut statement = connection.prepare("PRAGMA table_info(commit_permits)")?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        for column in columns {
            match column?.as_str() {
                "participant_registry_generation" => has_generation = true,
                "participant_registry_root" => has_root = true,
                _ => {}
            }
        }
    }
    if table_count == 4 && trigger_count == 8 && has_generation && has_root {
        connection.pragma_update(None, "user_version", 11)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 || has_generation || has_root {
        return Err(TaskStoreError::CorruptRecord(
            "partial participant registry schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(crate::participant::SCHEMA_V11_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v11 → v12 copies the frozen participant registry binding into every new
/// `EffectPermit` and permit-backed Task receipt. Existing rows remain
/// explicitly unbound rather than receiving invented authority evidence.
fn migrate_v12(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let mut present = 0usize;
    for (table, expected) in [
        (
            "effect_permits",
            [
                "participant_registry_generation",
                "participant_registry_root",
            ],
        ),
        (
            "task_receipts",
            [
                "participant_registry_generation",
                "participant_registry_root",
            ],
        ),
    ] {
        let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        for column in columns {
            if expected.contains(&column?.as_str()) {
                present += 1;
            }
        }
    }
    let trigger_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='trigger' AND name='effect_permit_participant_binding_immutable'
         )",
        [],
        |row| row.get(0),
    )?;
    if present == 4 && trigger_present {
        connection.pragma_update(None, "user_version", 12)?;
        return Ok(());
    }
    if present != 0 || trigger_present {
        return Err(TaskStoreError::CorruptRecord(
            "partial participant binding propagation schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V12_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v12 → v13 adds the immutable, authority-derived snapshot/read-set
/// `TaskWriteSet` seal. Existing tasks receive no invented write-set rows.
fn migrate_v13(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN
            ('task_write_sets', 'task_write_set_artifact_reads')",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_write_set_is_immutable', 'task_write_set_is_immutable_delete',
             'task_write_set_artifact_read_is_immutable',
             'task_write_set_artifact_read_is_immutable_delete')",
        [],
        |row| row.get(0),
    )?;
    if table_count == 2 && trigger_count == 4 {
        connection.pragma_update(None, "user_version", 13)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 {
        return Err(TaskStoreError::CorruptRecord("partial TaskWriteSet schema"));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V13_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v13 → v14 adds the immutable owner-verified Process binding child for a
/// `TaskWriteSet`. Existing seals remain valid and explicitly have no Process
/// binding; no caller-supplied execution identity is invented during upgrade.
fn migrate_v14(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='table' AND name='task_write_set_process_bindings'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_write_set_process_binding_is_immutable',
             'task_write_set_process_binding_is_immutable_delete')",
        [],
        |row| row.get(0),
    )?;
    if table_present && trigger_count == 2 {
        connection.pragma_update(None, "user_version", 14)?;
        return Ok(());
    }
    if table_present || trigger_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial TaskWriteSet Process binding schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V14_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v14 → v15 widens the participant type check to include the Process
/// binding endpoint. The immutable participant rows are copied byte-for-byte;
/// no registry generation or root is rewritten.
fn migrate_v15(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_sql: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_master
             WHERE type='table' AND name='task_participants'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(table_sql) = table_sql else {
        return Err(TaskStoreError::CorruptRecord(
            "missing participant table during v15 migration",
        ));
    };
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_participants_immutable_update',
             'task_participants_immutable_delete')",
        [],
        |row| row.get(0),
    )?;
    if table_sql.contains("BETWEEN 1 AND 7") && trigger_count == 2 {
        connection.pragma_update(None, "user_version", 15)?;
        return Ok(());
    }
    if table_sql.contains("BETWEEN 1 AND 7") {
        return Err(TaskStoreError::CorruptRecord(
            "partial Process participant type migration",
        ));
    }
    if !table_sql.contains("BETWEEN 1 AND 6") {
        return Err(TaskStoreError::CorruptRecord(
            "unexpected participant type constraint",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V15_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v15 → v16 adds immutable Semantic read and Resource Reservation children
/// for the authority-verified `TaskWriteSet` slice.
fn migrate_v16(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN
            ('task_write_set_semantic_reads', 'task_write_set_resource_reservations')",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_write_set_semantic_read_is_immutable',
             'task_write_set_semantic_read_is_immutable_delete',
             'task_write_set_resource_reservation_is_immutable',
             'task_write_set_resource_reservation_is_immutable_delete')",
        [],
        |row| row.get(0),
    )?;
    let root_column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('task_write_sets')
         WHERE name IN ('semantic_read_set_root', 'resource_reservation_set_root')",
        [],
        |row| row.get(0),
    )?;
    if table_count == 2 && trigger_count == 4 && root_column_count == 2 {
        connection.pragma_update(None, "user_version", 16)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 || root_column_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial Semantic/Resource TaskWriteSet schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V16_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v16 → v17 adds the immutable planned-effect declaration to each verified
/// `TaskWriteSet`. Existing rows keep a zero effect root and no invented
/// planned slots, preserving their v1/v2 write-set roots.
fn migrate_v17(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='table' AND name='task_write_set_planned_effects'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_write_set_planned_effect_is_immutable',
             'task_write_set_planned_effect_is_immutable_delete')",
        [],
        |row| row.get(0),
    )?;
    let root_column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('task_write_sets')
         WHERE name = 'effect_set_root'",
        [],
        |row| row.get(0),
    )?;
    if table_present && trigger_count == 2 && root_column_count == 1 {
        connection.pragma_update(None, "user_version", 17)?;
        return Ok(());
    }
    if table_present || trigger_count != 0 || root_column_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial planned-effect TaskWriteSet schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V17_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v17 → v18 adds immutable owner endpoint proofs for planned effect slots.
/// Existing rows retain a zero endpoint root and no invented endpoint facts.
fn migrate_v18(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='table' AND name='task_write_set_effect_endpoints'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_write_set_effect_endpoint_is_immutable',
             'task_write_set_effect_endpoint_is_immutable_delete')",
        [],
        |row| row.get(0),
    )?;
    let root_column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('task_write_sets')
         WHERE name = 'effect_endpoint_set_root'",
        [],
        |row| row.get(0),
    )?;
    if table_present && trigger_count == 2 && root_column_count == 1 {
        connection.pragma_update(None, "user_version", 18)?;
        return Ok(());
    }
    if table_present || trigger_count != 0 || root_column_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial effect-endpoint TaskWriteSet schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V18_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v18 → v19 adds the authority-checked proposed Artifact write declaration.
/// It is an intent root only; publication still requires a later Artifact
/// staging/publication receipt path.
fn migrate_v19(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='table' AND name='task_write_set_artifact_writes'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_write_set_artifact_write_is_immutable',
             'task_write_set_artifact_write_is_immutable_delete')",
        [],
        |row| row.get(0),
    )?;
    let root_column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('task_write_sets')
         WHERE name = 'artifact_write_set_root'",
        [],
        |row| row.get(0),
    )?;
    if table_present && trigger_count == 2 && root_column_count == 1 {
        connection.pragma_update(None, "user_version", 19)?;
        return Ok(());
    }
    if table_present || trigger_count != 0 || root_column_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial Artifact-write TaskWriteSet schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V19_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v19 → v20 removes the historical equality check between the permit-bound
/// `write_set_root` and the canonical Artifact publication-plan root. A
/// sealed `TaskWriteSet` may now carry proposed Artifact writes whose staging
/// identity is chosen after permit issuance, so the two roots are durable but
/// distinct commitments.
fn migrate_v20(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_sql: String = connection.query_row(
        "SELECT COALESCE(
            (SELECT sql FROM sqlite_master
             WHERE type='table' AND name='task_artifact_commit_plans'), '')",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_artifact_commit_plan_identity_immutable',
             'task_artifact_commit_plan_no_delete')",
        [],
        |row| row.get(0),
    )?;
    let column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('task_artifact_commit_plans')",
        [],
        |row| row.get(0),
    )?;
    if table_sql.is_empty() || trigger_count != 2 || column_count != 13 {
        return Err(TaskStoreError::CorruptRecord(
            "partial Artifact commit-plan schema",
        ));
    }
    let normalized_sql: String = table_sql
        .to_ascii_lowercase()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if !normalized_sql.contains("check(artifact_plan_root=write_set_root)") {
        connection.pragma_update(None, "user_version", 20)?;
        return Ok(());
    }

    connection.pragma_update(None, "foreign_keys", "OFF")?;
    let migration = (|| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(SCHEMA_V20_SQL)?;
        transaction.commit()?;
        Ok::<(), TaskStoreError>(())
    })();
    let restore = connection.pragma_update(None, "foreign_keys", "ON");
    if let Err(error) = migration {
        let _ = restore;
        return Err(error);
    }
    restore?;
    Ok(())
}

const SCHEMA_V13_SQL: &str = "CREATE TABLE task_write_sets (
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
        attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        snapshot_id BLOB NOT NULL CHECK(length(snapshot_id) = 16),
        snapshot_receipt_id BLOB NOT NULL CHECK(length(snapshot_receipt_id) = 16),
        expected_head_commit_seq BLOB NOT NULL CHECK(length(expected_head_commit_seq) = 8),
        effect_history_root BLOB NOT NULL CHECK(length(effect_history_root) = 32),
        retry_fence_epoch BLOB NOT NULL CHECK(length(retry_fence_epoch) = 8),
        group_id BLOB CHECK(group_id IS NULL OR length(group_id) = 16),
        membership_generation BLOB CHECK(membership_generation IS NULL OR length(membership_generation) = 8),
        membership_root BLOB CHECK(membership_root IS NULL OR length(membership_root) = 32),
        group_policy_digest BLOB CHECK(group_policy_digest IS NULL OR length(group_policy_digest) = 32),
        participant_registry_generation BLOB NOT NULL CHECK(length(participant_registry_generation) = 8),
        participant_registry_root BLOB NOT NULL CHECK(length(participant_registry_root) = 32),
        artifact_read_set_root BLOB NOT NULL CHECK(length(artifact_read_set_root) = 32),
        write_set_root BLOB NOT NULL CHECK(length(write_set_root) = 32),
        sealed_at_ms INTEGER NOT NULL CHECK(sealed_at_ms >= 0),
        PRIMARY KEY(task_id, idempotency_key),
        UNIQUE(task_id, write_set_root),
        FOREIGN KEY(task_id) REFERENCES tasks(task_id)
    ) STRICT;
    CREATE TABLE task_write_set_artifact_reads (
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        read_seq INTEGER NOT NULL CHECK(read_seq >= 0),
        artifact_id BLOB NOT NULL CHECK(length(artifact_id) = 16),
        expected_head_revision BLOB NOT NULL CHECK(length(expected_head_revision) = 8),
        expected_head_digest BLOB CHECK(expected_head_digest IS NULL OR length(expected_head_digest) = 32),
        PRIMARY KEY(task_id, idempotency_key, read_seq),
        UNIQUE(task_id, idempotency_key, artifact_id),
        FOREIGN KEY(task_id, idempotency_key)
            REFERENCES task_write_sets(task_id, idempotency_key)
    ) STRICT;
    CREATE TRIGGER task_write_set_is_immutable
    BEFORE UPDATE ON task_write_sets
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet is immutable'); END;
    CREATE TRIGGER task_write_set_is_immutable_delete
    BEFORE DELETE ON task_write_sets
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet is immutable'); END;
    CREATE TRIGGER task_write_set_artifact_read_is_immutable
    BEFORE UPDATE ON task_write_set_artifact_reads
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet artifact read is immutable'); END;
    CREATE TRIGGER task_write_set_artifact_read_is_immutable_delete
    BEFORE DELETE ON task_write_set_artifact_reads
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet artifact read is immutable'); END;
    PRAGMA user_version = 13;";

const SCHEMA_V14_SQL: &str = "CREATE TABLE task_write_set_process_bindings (
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        process_id BLOB NOT NULL CHECK(length(process_id) = 16),
        process_generation BLOB NOT NULL CHECK(length(process_generation) = 8),
        process_fencing_token BLOB NOT NULL CHECK(length(process_fencing_token) = 32),
        agent_instance_id BLOB NOT NULL CHECK(length(agent_instance_id) = 16),
        agent_instance_generation BLOB NOT NULL CHECK(length(agent_instance_generation) = 8),
        isolation_domain_id BLOB NOT NULL CHECK(length(isolation_domain_id) = 16),
        isolation_domain_generation BLOB NOT NULL CHECK(length(isolation_domain_generation) = 8),
        isolation_domain_fencing_token BLOB NOT NULL CHECK(length(isolation_domain_fencing_token) = 32),
        participant_id BLOB NOT NULL CHECK(length(participant_id) = 16),
        participant_generation BLOB NOT NULL CHECK(length(participant_generation) = 8),
        admission_receipt_id BLOB NOT NULL CHECK(length(admission_receipt_id) = 16),
        PRIMARY KEY(task_id, idempotency_key),
        FOREIGN KEY(task_id, idempotency_key)
            REFERENCES task_write_sets(task_id, idempotency_key)
    ) STRICT;
    CREATE TRIGGER task_write_set_process_binding_is_immutable
    BEFORE UPDATE ON task_write_set_process_bindings
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet Process binding is immutable'); END;
    CREATE TRIGGER task_write_set_process_binding_is_immutable_delete
    BEFORE DELETE ON task_write_set_process_bindings
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet Process binding is immutable'); END;
    PRAGMA user_version = 14;";

const SCHEMA_V15_SQL: &str = "DROP TRIGGER task_participants_immutable_update;
    DROP TRIGGER task_participants_immutable_delete;
    CREATE TABLE task_participants_v15 (
        registry_id BLOB NOT NULL CHECK(length(registry_id) = 16),
        participant_seq INTEGER NOT NULL CHECK(participant_seq >= 0),
        participant_type INTEGER NOT NULL CHECK(participant_type BETWEEN 1 AND 7),
        participant_id BLOB NOT NULL CHECK(length(participant_id) = 16),
        participant_generation BLOB NOT NULL CHECK(length(participant_generation) = 8),
        admission_receipt_id BLOB NOT NULL CHECK(length(admission_receipt_id) = 16),
        PRIMARY KEY(registry_id, participant_seq),
        UNIQUE(registry_id, participant_type, participant_id),
        FOREIGN KEY(registry_id) REFERENCES task_participant_registries(registry_id)
    ) STRICT;
    INSERT INTO task_participants_v15
        (registry_id, participant_seq, participant_type, participant_id,
         participant_generation, admission_receipt_id)
        SELECT registry_id, participant_seq, participant_type, participant_id,
               participant_generation, admission_receipt_id
        FROM task_participants;
    DROP TABLE task_participants;
    ALTER TABLE task_participants_v15 RENAME TO task_participants;
    CREATE TRIGGER task_participants_immutable_update BEFORE UPDATE ON task_participants
    BEGIN SELECT RAISE(ABORT, 'task participant is immutable'); END;
    CREATE TRIGGER task_participants_immutable_delete BEFORE DELETE ON task_participants
    BEGIN SELECT RAISE(ABORT, 'task participant is immutable'); END;
    PRAGMA user_version = 15;";

const SCHEMA_V16_SQL: &str = "ALTER TABLE task_write_sets
        ADD COLUMN semantic_read_set_root BLOB NOT NULL DEFAULT X'0000000000000000000000000000000000000000000000000000000000000000'
        CHECK(length(semantic_read_set_root) = 32);
    ALTER TABLE task_write_sets
        ADD COLUMN resource_reservation_set_root BLOB NOT NULL DEFAULT X'0000000000000000000000000000000000000000000000000000000000000000'
        CHECK(length(resource_reservation_set_root) = 32);
    CREATE TABLE task_write_set_semantic_reads (
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        read_seq INTEGER NOT NULL CHECK(read_seq >= 0),
        event_id BLOB NOT NULL CHECK(length(event_id) = 32),
        expected_log_seq BLOB NOT NULL CHECK(length(expected_log_seq) = 8),
        expected_canonical_digest BLOB NOT NULL CHECK(length(expected_canonical_digest) = 32),
        PRIMARY KEY(task_id, idempotency_key, read_seq),
        UNIQUE(task_id, idempotency_key, event_id),
        FOREIGN KEY(task_id, idempotency_key)
            REFERENCES task_write_sets(task_id, idempotency_key)
    ) STRICT;
    CREATE TABLE task_write_set_resource_reservations (
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        read_seq INTEGER NOT NULL CHECK(read_seq >= 0),
        reservation_id BLOB NOT NULL CHECK(length(reservation_id) = 16),
        account_id BLOB NOT NULL CHECK(length(account_id) = 16),
        quote_id BLOB NOT NULL CHECK(length(quote_id) = 16),
        call_id BLOB NOT NULL CHECK(length(call_id) = 16),
        operation_id BLOB NOT NULL CHECK(length(operation_id) = 16),
        driver_id BLOB NOT NULL CHECK(length(driver_id) = 16),
        device_id BLOB NOT NULL CHECK(length(device_id) = 16),
        driver_generation BLOB NOT NULL CHECK(length(driver_generation) = 8),
        driver_fencing_token BLOB NOT NULL CHECK(length(driver_fencing_token) = 32),
        upper_bound BLOB NOT NULL CHECK(length(upper_bound) = 8),
        PRIMARY KEY(task_id, idempotency_key, read_seq),
        UNIQUE(task_id, idempotency_key, reservation_id),
        FOREIGN KEY(task_id, idempotency_key)
            REFERENCES task_write_sets(task_id, idempotency_key)
    ) STRICT;
    CREATE TRIGGER task_write_set_semantic_read_is_immutable
    BEFORE UPDATE ON task_write_set_semantic_reads
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet Semantic read is immutable'); END;
    CREATE TRIGGER task_write_set_semantic_read_is_immutable_delete
    BEFORE DELETE ON task_write_set_semantic_reads
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet Semantic read is immutable'); END;
    CREATE TRIGGER task_write_set_resource_reservation_is_immutable
    BEFORE UPDATE ON task_write_set_resource_reservations
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet Resource Reservation is immutable'); END;
    CREATE TRIGGER task_write_set_resource_reservation_is_immutable_delete
    BEFORE DELETE ON task_write_set_resource_reservations
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet Resource Reservation is immutable'); END;
    PRAGMA user_version = 16;";

const SCHEMA_V17_SQL: &str = "ALTER TABLE task_write_sets
        ADD COLUMN effect_set_root BLOB NOT NULL DEFAULT X'0000000000000000000000000000000000000000000000000000000000000000'
        CHECK(length(effect_set_root) = 32);
    CREATE TABLE task_write_set_planned_effects (
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        effect_seq INTEGER NOT NULL CHECK(effect_seq >= 0),
        intent_spec_id BLOB NOT NULL CHECK(length(intent_spec_id) = 32),
        stable_action_slot BLOB NOT NULL CHECK(length(stable_action_slot) = 8),
        target_authority_object_id BLOB NOT NULL CHECK(length(target_authority_object_id) = 32),
        effect_class INTEGER NOT NULL CHECK(effect_class BETWEEN 0 AND 4294967295),
        idempotency_scope INTEGER NOT NULL CHECK(idempotency_scope BETWEEN 0 AND 4294967295),
        logical_effect_id BLOB NOT NULL CHECK(length(logical_effect_id) = 32),
        idempotency_identity_digest BLOB NOT NULL CHECK(length(idempotency_identity_digest) = 32),
        required INTEGER NOT NULL CHECK(required IN (0, 1)),
        required_condition_digest BLOB CHECK(required_condition_digest IS NULL OR length(required_condition_digest) = 32),
        success_criteria_digest BLOB NOT NULL CHECK(length(success_criteria_digest) = 32),
        action_proposal_digest BLOB NOT NULL CHECK(length(action_proposal_digest) = 32),
        PRIMARY KEY(task_id, idempotency_key, effect_seq),
        UNIQUE(task_id, idempotency_key, logical_effect_id),
        FOREIGN KEY(task_id, idempotency_key)
            REFERENCES task_write_sets(task_id, idempotency_key)
    ) STRICT;
    CREATE TRIGGER task_write_set_planned_effect_is_immutable
    BEFORE UPDATE ON task_write_set_planned_effects
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet planned effect is immutable'); END;
    CREATE TRIGGER task_write_set_planned_effect_is_immutable_delete
    BEFORE DELETE ON task_write_set_planned_effects
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet planned effect is immutable'); END;
    PRAGMA user_version = 17;";

const SCHEMA_V18_SQL: &str = "ALTER TABLE task_write_sets
        ADD COLUMN effect_endpoint_set_root BLOB NOT NULL DEFAULT X'0000000000000000000000000000000000000000000000000000000000000000'
        CHECK(length(effect_endpoint_set_root) = 32);
    CREATE TABLE task_write_set_effect_endpoints (
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        endpoint_seq INTEGER NOT NULL CHECK(endpoint_seq >= 0),
        effect_seq INTEGER NOT NULL CHECK(effect_seq >= 0),
        endpoint_kind INTEGER NOT NULL CHECK(endpoint_kind BETWEEN 1 AND 5),
        object_id BLOB NOT NULL CHECK(length(object_id) = 16),
        participant_id BLOB NOT NULL CHECK(length(participant_id) = 16),
        participant_generation BLOB NOT NULL CHECK(length(participant_generation) = 8),
        admission_receipt_id BLOB NOT NULL CHECK(length(admission_receipt_id) = 16),
        PRIMARY KEY(task_id, idempotency_key, endpoint_seq),
        UNIQUE(task_id, idempotency_key, effect_seq, endpoint_kind, object_id),
        FOREIGN KEY(task_id, idempotency_key)
            REFERENCES task_write_sets(task_id, idempotency_key)
    ) STRICT;
    CREATE TRIGGER task_write_set_effect_endpoint_is_immutable
    BEFORE UPDATE ON task_write_set_effect_endpoints
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet effect endpoint is immutable'); END;
    CREATE TRIGGER task_write_set_effect_endpoint_is_immutable_delete
    BEFORE DELETE ON task_write_set_effect_endpoints
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet effect endpoint is immutable'); END;
    PRAGMA user_version = 18;";

const SCHEMA_V19_SQL: &str = "ALTER TABLE task_write_sets
        ADD COLUMN artifact_write_set_root BLOB NOT NULL DEFAULT X'0000000000000000000000000000000000000000000000000000000000000000'
        CHECK(length(artifact_write_set_root) = 32);
    CREATE TABLE task_write_set_artifact_writes (
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        write_seq INTEGER NOT NULL CHECK(write_seq >= 0),
        artifact_id BLOB NOT NULL CHECK(length(artifact_id) = 16),
        expected_head_revision BLOB NOT NULL CHECK(length(expected_head_revision) = 8),
        proposed_revision BLOB NOT NULL CHECK(length(proposed_revision) = 8),
        content_digest BLOB NOT NULL CHECK(length(content_digest) = 32),
        size_bytes BLOB NOT NULL CHECK(length(size_bytes) = 8),
        PRIMARY KEY(task_id, idempotency_key, write_seq),
        UNIQUE(task_id, idempotency_key, artifact_id),
        FOREIGN KEY(task_id, idempotency_key)
            REFERENCES task_write_sets(task_id, idempotency_key)
    ) STRICT;
    CREATE TRIGGER task_write_set_artifact_write_is_immutable
    BEFORE UPDATE ON task_write_set_artifact_writes
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet Artifact write is immutable'); END;
    CREATE TRIGGER task_write_set_artifact_write_is_immutable_delete
    BEFORE DELETE ON task_write_set_artifact_writes
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet Artifact write is immutable'); END;
    PRAGMA user_version = 19;";

const SCHEMA_V20_SQL: &str = "DROP TRIGGER IF EXISTS task_artifact_commit_plan_identity_immutable;
    DROP TRIGGER IF EXISTS task_artifact_commit_plan_no_delete;
    CREATE TABLE task_artifact_commit_plans_v20 (
        plan_id BLOB PRIMARY KEY NOT NULL CHECK(length(plan_id) = 16),
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        permit_id BLOB NOT NULL UNIQUE CHECK(length(permit_id) = 16),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
        attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
        write_set_root BLOB NOT NULL CHECK(length(write_set_root) = 32),
        artifact_plan_root BLOB NOT NULL CHECK(length(artifact_plan_root) = 32),
        expected_artifact_count BLOB NOT NULL CHECK(length(expected_artifact_count) = 8),
        plan_state INTEGER NOT NULL CHECK(plan_state IN (0, 1, 2, 3)),
        task_receipt_id BLOB CHECK(task_receipt_id IS NULL OR length(task_receipt_id) = 16),
        created_at_ms INTEGER NOT NULL,
        updated_at_ms INTEGER NOT NULL,
        UNIQUE(task_id, idempotency_key),
        FOREIGN KEY(task_id) REFERENCES tasks(task_id),
        FOREIGN KEY(permit_id) REFERENCES commit_permits(permit_id),
        CHECK((plan_state = 3) = (task_receipt_id IS NOT NULL))
     ) STRICT;
    INSERT INTO task_artifact_commit_plans_v20 (
        plan_id, task_id, permit_id, idempotency_key, attempt_id,
        attempt_generation, write_set_root, artifact_plan_root,
        expected_artifact_count, plan_state, task_receipt_id,
        created_at_ms, updated_at_ms
    ) SELECT plan_id, task_id, permit_id, idempotency_key, attempt_id,
        attempt_generation, write_set_root, artifact_plan_root,
        expected_artifact_count, plan_state, task_receipt_id,
        created_at_ms, updated_at_ms
      FROM task_artifact_commit_plans;
    DROP TABLE task_artifact_commit_plans;
    ALTER TABLE task_artifact_commit_plans_v20 RENAME TO task_artifact_commit_plans;
    CREATE TRIGGER task_artifact_commit_plan_identity_immutable
    BEFORE UPDATE ON task_artifact_commit_plans
    WHEN OLD.plan_id IS NOT NEW.plan_id
      OR OLD.task_id IS NOT NEW.task_id
      OR OLD.permit_id IS NOT NEW.permit_id
      OR OLD.idempotency_key IS NOT NEW.idempotency_key
      OR OLD.attempt_id IS NOT NEW.attempt_id
      OR OLD.attempt_generation IS NOT NEW.attempt_generation
      OR OLD.write_set_root IS NOT NEW.write_set_root
      OR OLD.artifact_plan_root IS NOT NEW.artifact_plan_root
      OR OLD.expected_artifact_count IS NOT NEW.expected_artifact_count
      OR OLD.created_at_ms IS NOT NEW.created_at_ms
    BEGIN
        SELECT RAISE(ABORT, 'artifact commit plan identity is immutable');
    END;
    CREATE TRIGGER task_artifact_commit_plan_no_delete
    BEFORE DELETE ON task_artifact_commit_plans
    BEGIN
        SELECT RAISE(ABORT, 'artifact commit plan is durable evidence');
    END;
    PRAGMA user_version = 20;";

const SCHEMA_V12_SQL: &str = "ALTER TABLE effect_permits
        ADD COLUMN participant_registry_generation BLOB
        CHECK(participant_registry_generation IS NULL OR length(participant_registry_generation) = 8);
    ALTER TABLE effect_permits
        ADD COLUMN participant_registry_root BLOB
        CHECK(participant_registry_root IS NULL OR length(participant_registry_root) = 32);
    ALTER TABLE task_receipts
        ADD COLUMN participant_registry_generation BLOB
        CHECK(participant_registry_generation IS NULL OR length(participant_registry_generation) = 8);
    ALTER TABLE task_receipts
        ADD COLUMN participant_registry_root BLOB
        CHECK(participant_registry_root IS NULL OR length(participant_registry_root) = 32);
    CREATE TRIGGER effect_permit_participant_binding_immutable
    BEFORE UPDATE ON effect_permits
    WHEN NEW.participant_registry_generation IS NOT OLD.participant_registry_generation
      OR NEW.participant_registry_root IS NOT OLD.participant_registry_root
    BEGIN SELECT RAISE(ABORT, 'effect permit participant binding is immutable'); END;
    PRAGMA user_version = 12;";

const SCHEMA_V10_SQL: &str = "CREATE TABLE task_snapshot_receipts (
        receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        snapshot_id BLOB NOT NULL CHECK(length(snapshot_id) = 16),
        builder_id BLOB NOT NULL CHECK(length(builder_id) = 16),
        builder_version_digest BLOB NOT NULL CHECK(length(builder_version_digest) = 32),
        dependency_closure_root BLOB NOT NULL CHECK(length(dependency_closure_root) = 32),
        semantic_resolver_digest BLOB NOT NULL CHECK(length(semantic_resolver_digest) = 32),
        canonical_iteration_digest BLOB NOT NULL CHECK(length(canonical_iteration_digest) = 32),
        achieved_consistency INTEGER NOT NULL CHECK(achieved_consistency BETWEEN 0 AND 3),
        durability INTEGER NOT NULL DEFAULT 1 CHECK(durability = 1),
        built_at_ms INTEGER NOT NULL CHECK(built_at_ms >= 0),
        authority_id BLOB NOT NULL CHECK(length(authority_id) = 16),
        key_id BLOB NOT NULL CHECK(length(key_id) = 16),
        signature BLOB NOT NULL CHECK(length(signature) = 64),
        UNIQUE(task_id, snapshot_id),
        FOREIGN KEY(task_id, snapshot_id) REFERENCES task_snapshots(task_id, snapshot_id)
    ) STRICT;

    CREATE TABLE task_snapshot_checkpoint_receipts (
        snapshot_receipt_id BLOB NOT NULL CHECK(length(snapshot_receipt_id) = 16),
        checkpoint_seq INTEGER NOT NULL CHECK(checkpoint_seq >= 0),
        checkpoint_receipt_id BLOB NOT NULL CHECK(length(checkpoint_receipt_id) = 16),
        PRIMARY KEY(snapshot_receipt_id, checkpoint_seq),
        UNIQUE(snapshot_receipt_id, checkpoint_receipt_id),
        FOREIGN KEY(snapshot_receipt_id) REFERENCES task_snapshot_receipts(receipt_id)
    ) STRICT;

    CREATE TRIGGER task_snapshot_receipt_is_immutable
    BEFORE UPDATE ON task_snapshot_receipts
    BEGIN SELECT RAISE(ABORT, 'task snapshot receipt is immutable'); END;

    CREATE TRIGGER task_snapshot_checkpoint_receipt_is_immutable
    BEFORE UPDATE ON task_snapshot_checkpoint_receipts
    BEGIN SELECT RAISE(ABORT, 'task snapshot checkpoint receipt is immutable'); END;

    ALTER TABLE task_attempts ADD COLUMN snapshot_receipt_id BLOB
        CHECK(snapshot_receipt_id IS NULL OR length(snapshot_receipt_id) = 16)
        REFERENCES task_snapshot_receipts(receipt_id);

    PRAGMA user_version = 10;";

const SCHEMA_V5_SQL: &str =
    "ALTER TABLE commit_permits ADD COLUMN group_id BLOB CHECK(group_id IS NULL OR length(group_id) = 16);
     ALTER TABLE commit_permits ADD COLUMN membership_generation BLOB CHECK(membership_generation IS NULL OR length(membership_generation) = 8);
     ALTER TABLE commit_permits ADD COLUMN membership_root BLOB CHECK(membership_root IS NULL OR length(membership_root) = 32);
     ALTER TABLE commit_permits ADD COLUMN group_policy_digest BLOB CHECK(group_policy_digest IS NULL OR length(group_policy_digest) = 32);
     ALTER TABLE task_receipts ADD COLUMN group_id BLOB CHECK(group_id IS NULL OR length(group_id) = 16);
     ALTER TABLE task_receipts ADD COLUMN membership_generation BLOB CHECK(membership_generation IS NULL OR length(membership_generation) = 8);
     ALTER TABLE task_receipts ADD COLUMN membership_root BLOB CHECK(membership_root IS NULL OR length(membership_root) = 32);
     ALTER TABLE task_receipts ADD COLUMN group_policy_digest BLOB CHECK(group_policy_digest IS NULL OR length(group_policy_digest) = 32);
     PRAGMA user_version = 5;";

const SCHEMA_V1_SQL: &str =
    "CREATE TABLE tasks (
            task_id BLOB PRIMARY KEY NOT NULL CHECK(length(task_id) = 16),
            task_generation BLOB NOT NULL CHECK(length(task_generation) = 8),
            head_commit_seq BLOB NOT NULL CHECK(length(head_commit_seq) = 8),
            head_effect_history_root BLOB NOT NULL CHECK(length(head_effect_history_root) = 32),
            retry_fence_epoch BLOB NOT NULL CHECK(length(retry_fence_epoch) = 8),
            control_epoch BLOB NOT NULL CHECK(length(control_epoch) = 8),
            cancel_epoch BLOB NOT NULL CHECK(length(cancel_epoch) = 8),
            permit_epoch BLOB NOT NULL CHECK(length(permit_epoch) = 8),
            task_state INTEGER NOT NULL,
            revision INTEGER NOT NULL DEFAULT 0,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL
        ) STRICT;

        CREATE TABLE task_snapshots (
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            snapshot_id BLOB NOT NULL CHECK(length(snapshot_id) = 16),
            snapshot_digest BLOB NOT NULL CHECK(length(snapshot_digest) = 32),
            expected_head_commit_seq BLOB NOT NULL CHECK(length(expected_head_commit_seq) = 8),
            effect_history_root BLOB NOT NULL CHECK(length(effect_history_root) = 32),
            retry_fence_epoch BLOB NOT NULL CHECK(length(retry_fence_epoch) = 8),
            created_at_ms INTEGER NOT NULL,
            PRIMARY KEY(task_id, snapshot_id),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TRIGGER task_snapshot_is_immutable
        BEFORE UPDATE ON task_snapshots
        BEGIN
            SELECT RAISE(ABORT, 'task snapshot is immutable');
        END;

        CREATE TABLE task_attempts (
            attempt_id BLOB PRIMARY KEY NOT NULL CHECK(length(attempt_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
            snapshot_id BLOB NOT NULL CHECK(length(snapshot_id) = 16),
            cancellation_scope_id BLOB NOT NULL CHECK(length(cancellation_scope_id) = 16),
            cancellation_generation BLOB NOT NULL CHECK(length(cancellation_generation) = 8),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            attempt_state INTEGER NOT NULL,
            receipt_id BLOB CHECK(receipt_id IS NULL OR length(receipt_id) = 16),
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            UNIQUE(task_id, idempotency_key),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TABLE commit_permits (
            permit_id BLOB PRIMARY KEY NOT NULL CHECK(length(permit_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
            attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
            expected_head_commit_seq BLOB NOT NULL CHECK(length(expected_head_commit_seq) = 8),
            expected_effect_history_root BLOB NOT NULL CHECK(length(expected_effect_history_root) = 32),
            expected_retry_fence_epoch BLOB NOT NULL CHECK(length(expected_retry_fence_epoch) = 8),
            write_set_root BLOB NOT NULL CHECK(length(write_set_root) = 32),
            permit_epoch BLOB NOT NULL CHECK(length(permit_epoch) = 8),
            control_epoch BLOB NOT NULL CHECK(length(control_epoch) = 8),
            cancel_epoch BLOB NOT NULL CHECK(length(cancel_epoch) = 8),
            valid_until_ms INTEGER NOT NULL,
            permit_state INTEGER NOT NULL,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            UNIQUE(task_id, idempotency_key),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        -- Defense in depth: the single-writer transaction already
        -- serializes issuance; this index makes a second outstanding
        -- permit per task unrepresentable on disk.
        CREATE UNIQUE INDEX commit_permits_single_active
            ON commit_permits(task_id) WHERE permit_state = 0;

        CREATE TABLE task_cancels (
            task_id BLOB PRIMARY KEY NOT NULL CHECK(length(task_id) = 16),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            cancel_epoch_after BLOB NOT NULL CHECK(length(cancel_epoch_after) = 8),
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TABLE task_receipts (
            receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            permit_id BLOB CHECK(permit_id IS NULL OR length(permit_id) = 16),
            attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
            attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
            outcome INTEGER NOT NULL,
            prior_head_commit_seq BLOB NOT NULL CHECK(length(prior_head_commit_seq) = 8),
            prior_effect_history_root BLOB NOT NULL CHECK(length(prior_effect_history_root) = 32),
            prior_retry_fence_epoch BLOB NOT NULL CHECK(length(prior_retry_fence_epoch) = 8),
            new_head_commit_seq BLOB NOT NULL CHECK(length(new_head_commit_seq) = 8),
            new_effect_history_root BLOB NOT NULL CHECK(length(new_effect_history_root) = 32),
            new_retry_fence_epoch BLOB NOT NULL CHECK(length(new_retry_fence_epoch) = 8),
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE INDEX task_receipts_by_permit
            ON task_receipts(task_id, permit_id);

        CREATE TRIGGER task_receipt_is_immutable
        BEFORE UPDATE ON task_receipts
        BEGIN
            SELECT RAISE(ABORT, 'task receipt is immutable');
        END;

        PRAGMA user_version = 1;";

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
    transaction.execute(
        "INSERT INTO commit_permits (
            permit_id, task_id, idempotency_key, attempt_id, attempt_generation,
            expected_head_commit_seq, expected_effect_history_root,
            expected_retry_fence_epoch, write_set_root, permit_epoch,
            control_epoch, cancel_epoch, valid_until_ms, permit_state,
            created_at_ms, updated_at_ms, group_id, membership_generation,
            membership_root, group_policy_digest, participant_registry_generation,
            participant_registry_root
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                   ?17, ?18, ?19, ?20, ?21, ?22)",
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
            artifact_write_set_root,
            resource_reservation_set_root, effect_set_root,
            effect_endpoint_set_root,
            write_set_root, sealed_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                   ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
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
     artifact_read_set_root, semantic_read_set_root, artifact_write_set_root,
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
    record.resource_reservations =
        load_write_set_resource_reservations(source, task_id, idempotency_key)?;
    record.planned_effects = load_write_set_planned_effects(source, task_id, idempotency_key)?;
    record.effect_endpoints = load_write_set_effect_endpoints(source, task_id, idempotency_key)?;
    validate_artifact_write_rows(&record)?;
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
    record.resource_reservations =
        load_write_set_resource_reservations(source, task_id, record.idempotency_key)?;
    record.planned_effects =
        load_write_set_planned_effects(source, task_id, record.idempotency_key)?;
    record.effect_endpoints =
        load_write_set_effect_endpoints(source, task_id, record.idempotency_key)?;
    validate_artifact_write_rows(&record)?;
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
        resource_reservations: Vec::new(),
        artifact_writes: Vec::new(),
        planned_effects: Vec::new(),
        effect_endpoints: Vec::new(),
        artifact_read_set_root: blob32(row, 15)?,
        semantic_read_set_root: blob32(row, 16)?,
        artifact_write_set_root: blob32(row, 17)?,
        resource_reservation_set_root: blob32(row, 18)?,
        effect_set_root: blob32(row, 19)?,
        effect_endpoint_set_root: blob32(row, 20)?,
        write_set_root: blob32(row, 21)?,
        sealed_at_ms: row.get(22)?,
    })
}

const PERMIT_COLUMNS: &str = "permit_id, task_id, idempotency_key, attempt_id, attempt_generation,
     expected_head_commit_seq, expected_effect_history_root,
     expected_retry_fence_epoch, write_set_root, permit_epoch,
     control_epoch, cancel_epoch, valid_until_ms, permit_state,
     created_at_ms, updated_at_ms, group_id, membership_generation,
     membership_root, group_policy_digest, participant_registry_generation,
     participant_registry_root";

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

fn optional_blob<const N: usize>(
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
