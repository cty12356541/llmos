//! Cross-Cell Process migration state machine and session migratability policy
//! (`v0.5 §26.3`, `[DIST-MIGRATE-001]` and `[DIST-MIGRATE-002]`).
//!
//! - `[DIST-MIGRATE-001]`: Process migration requires a strict 8-step sequence:
//!   `Initiated` -> `Quiesced` (`quiesce/cancel`) -> `Checkpointed` ->
//!   `SourceFenced` -> `CapabilitiesRebound` -> `LeasesRegranted`
//!   (`Lease close/regrant`) -> `TargetRestored` (`target_generation = source_generation + 1`) ->
//!   `Completed` (`ProcessMigrationReceipt`).
//! - `[DIST-MIGRATE-002]`: Any `pinned/non-migratable Driver session` attached
//!   to a Process MUST either fail closed (`PinnedSessionPolicy::RefuseMigration`)
//!   or explicitly downgrade the migration mode to `restart-from-artifact`
//!   (`MigrationMode::DowngradedRestartFromArtifact`), never pretending a
//!   transparent checkpoint/restore migration took place.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use nlos_types::{IsolationDomainId, ProcessId};
use sha2::{Digest, Sha256};

use crate::model::{FencingToken, ProcessBindingRecord};

/// Domain separation tag for computing [`ProcessMigrationReceipt::receipt_digest`].
pub const PROCESS_MIGRATION_RECEIPT_DOMAIN: &[u8] = b"llmos/process-migration-receipt/v1";

type MigratableSessionDigests = Vec<([u8; 16], [u8; 32])>;

/// Describes a Driver session attached to a Process at migration admission time
/// (`[DIST-MIGRATE-002]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DriverSessionAttachment {
    /// A driver session whose state can be cleanly serialized into a process
    /// checkpoint and transparently restored on the target Cell.
    Migratable {
        session_id: [u8; 16],
        state_digest: [u8; 32],
    },
    /// A hardware-pinned or host-bound driver session that cannot be transparently
    /// migrated across Cells (`[DIST-MIGRATE-002]`).
    PinnedNonMigratable {
        session_id: [u8; 16],
        pin_reason: String,
        artifact_restart_image_digest: Option<[u8; 32]>,
    },
}

impl DriverSessionAttachment {
    /// Returns the 16-byte session identifier.
    #[must_use]
    pub const fn session_id(&self) -> [u8; 16] {
        match self {
            Self::Migratable { session_id, .. } | Self::PinnedNonMigratable { session_id, .. } => {
                *session_id
            }
        }
    }
}

/// Admission policy governing how `PinnedNonMigratable` driver sessions are
/// handled when a Process migration is requested (`[DIST-MIGRATE-002]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PinnedSessionPolicy {
    /// Immediately refuse migration fail-closed if any attached driver session
    /// is `PinnedNonMigratable`.
    RefuseMigration,
    /// Explicitly downgrade the migration to `restart-from-artifact` if all
    /// attached `PinnedNonMigratable` sessions declare a valid artifact restart
    /// image digest. Never pretends transparent checkpoint migration.
    DowngradeToRestartFromArtifact,
}

/// Resolved execution mode of a Process migration (`[DIST-MIGRATE-002]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MigrationMode {
    /// All attached driver sessions are migratable; full state checkpoint and
    /// transparent restore is permitted.
    TransparentCheckpointRestore,
    /// Migration was explicitly downgraded to `restart-from-artifact` because
    /// one or more pinned non-migratable driver sessions were attached.
    DowngradedRestartFromArtifact {
        artifact_image_digest: [u8; 32],
        dropped_pinned_sessions: Vec<[u8; 16]>,
    },
}

/// Ordered state of a Process migration coordinator (`[DIST-MIGRATE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum ProcessMigrationStep {
    Initiated = 1,
    Quiesced = 2,
    Checkpointed = 3,
    SourceFenced = 4,
    CapabilitiesRebound = 5,
    LeasesRegranted = 6,
    TargetRestored = 7,
    Completed = 8,
    Aborted = 9,
    Quarantined = 10,
}

/// Outcome of submitting a step proof to [`ProcessMigrationCoordinator`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationStepDecision {
    /// The proof advanced the migration state machine to the next step.
    Advanced(ProcessMigrationStep),
    /// The exact same proof was already recorded for this step (idempotent replay).
    Replayed(ProcessMigrationStep),
}

/// Input request to initiate a Process migration (`[DIST-MIGRATE-001/002]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitiateProcessMigrationRequest {
    pub migration_id: [u8; 16],
    pub process_id: ProcessId,
    pub source_cell_id: [u8; 16],
    pub target_cell_id: [u8; 16],
    pub source_generation: u64,
    pub target_generation: u64,
    pub attached_sessions: Vec<DriverSessionAttachment>,
    pub pinned_session_policy: PinnedSessionPolicy,
    pub initiated_at_ms: u64,
}

/// Immutable migration plan admitted at step 1 (`Initiated`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessMigrationPlan {
    pub migration_id: [u8; 16],
    pub process_id: ProcessId,
    pub source_cell_id: [u8; 16],
    pub target_cell_id: [u8; 16],
    pub source_generation: u64,
    pub target_generation: u64,
    pub migration_mode: MigrationMode,
    pub migratable_sessions: MigratableSessionDigests,
    pub initiated_at_ms: u64,
}

/// Step 2 proof: active fibers and in-flight operations on the source Process
/// have been quiesced or cancelled (`quiesce/cancel`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuiesceProof {
    pub process_id: ProcessId,
    pub source_cell_id: [u8; 16],
    pub source_generation: u64,
    pub quiesced_fibers: u32,
    pub cancelled_inflight_ops: u32,
    pub inflight_effects_drained: bool,
    pub quiesced_at_ms: u64,
}

/// Payload descriptor for step 3 (`Checkpointed`), discriminated by [`MigrationMode`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CheckpointPayloadKind {
    /// Full state checkpoint used only in [`MigrationMode::TransparentCheckpointRestore`].
    FullStateCheckpoint {
        state_checkpoint_digest: [u8; 32],
        migrated_session_digests: MigratableSessionDigests,
    },
    /// Artifact restart image used in [`MigrationMode::DowngradedRestartFromArtifact`].
    ArtifactRestartImage { artifact_image_digest: [u8; 32] },
}

impl CheckpointPayloadKind {
    /// Returns the primary 32-byte checkpoint or artifact image digest.
    #[must_use]
    pub const fn primary_digest(&self) -> [u8; 32] {
        match self {
            Self::FullStateCheckpoint {
                state_checkpoint_digest,
                ..
            } => *state_checkpoint_digest,
            Self::ArtifactRestartImage {
                artifact_image_digest,
            } => *artifact_image_digest,
        }
    }
}

/// Step 3 proof: source Process state checkpoint or downgraded artifact image
/// digest has been persisted (`checkpoint`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessCheckpointProof {
    pub process_id: ProcessId,
    pub source_generation: u64,
    pub payload: CheckpointPayloadKind,
    pub checkpointed_at_ms: u64,
}

/// Step 4 proof: source Cell has fenced the old Process generation and
/// unregistered it from the source supervisor (`源 fencing`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceProcessFenceProof {
    pub process_id: ProcessId,
    pub source_cell_id: [u8; 16],
    pub source_cell_epoch: u64,
    pub fenced_generation: u64,
    pub source_fencing_token: FencingToken,
    pub supervisor_unregistered: bool,
    pub fenced_at_ms: u64,
}

/// One capability handle rebound from `(source_cell_id, source_generation)` to
/// `(target_cell_id, target_generation)`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReboundCapabilityEntry {
    pub capability_id: [u8; 16],
    pub source_cell_id: [u8; 16],
    pub source_generation: u64,
    pub target_cell_id: [u8; 16],
    pub target_generation: u64,
    pub attenuation_digest: [u8; 32],
}

/// Step 5 proof: capabilities held by the Process have been rebound to the
/// target Cell and target generation (`Capability rebind`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityRebindProof {
    pub process_id: ProcessId,
    pub rebound_capabilities: Vec<ReboundCapabilityEntry>,
    pub rebound_at_ms: u64,
}

/// One lease closed/fenced on the source Cell and re-granted on the target Cell
/// (`[LEASE-MIGRATE-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigratedLeaseEntry {
    pub source_lease_id: [u8; 16],
    pub source_cell_id: [u8; 16],
    pub source_closed_or_fenced: bool,
    pub source_close_receipt_digest: [u8; 32],
    pub target_lease_id: [u8; 16],
    pub target_cell_id: [u8; 16],
    pub target_grant_epoch: u64,
}

/// Step 6 proof: all source Cell leases were closed or fenced before target Cell
/// leases were granted (`Lease close/regrant`, `[LEASE-MIGRATE-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeaseCloseRegrantProof {
    pub process_id: ProcessId,
    pub source_generation: u64,
    pub target_generation: u64,
    pub migrated_leases: Vec<MigratedLeaseEntry>,
    pub regranted_at_ms: u64,
}

/// Step 7 proof: target Cell restored the Process at `target_generation = source_generation + 1`
/// (`目标 restore` + `新 generation`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetRestoreProof {
    pub process_id: ProcessId,
    pub target_cell_id: [u8; 16],
    pub restored_generation: u64,
    pub target_fencing_token: FencingToken,
    pub target_isolation_domain_id: IsolationDomainId,
    pub restored_image_or_checkpoint_digest: [u8; 32],
    pub restored_at_ms: u64,
}

impl TargetRestoreProof {
    /// Constructs a [`TargetRestoreProof`] directly from an authoritative
    /// [`ProcessBindingRecord`] produced by [`crate::ProcessAuthority::restore_process`].
    #[must_use]
    pub fn from_restored_binding(
        record: &ProcessBindingRecord,
        target_cell_id: [u8; 16],
        restored_image_or_checkpoint_digest: [u8; 32],
    ) -> Self {
        Self {
            process_id: record.process_id,
            target_cell_id,
            restored_generation: record.process_generation.get(),
            target_fencing_token: record.process_fencing_token,
            target_isolation_domain_id: record.isolation_domain_id,
            restored_image_or_checkpoint_digest,
            restored_at_ms: record.created_at_ms,
        }
    }
}

/// Step 8 immutable receipt sealing all proofs of a completed Process migration
/// (`[DIST-MIGRATE-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessMigrationReceipt {
    pub migration_id: [u8; 16],
    pub process_id: ProcessId,
    pub source_cell_id: [u8; 16],
    pub target_cell_id: [u8; 16],
    pub source_generation: u64,
    pub target_generation: u64,
    pub migration_mode: MigrationMode,
    pub quiesce_proof: QuiesceProof,
    pub checkpoint_proof: ProcessCheckpointProof,
    pub source_fence_proof: SourceProcessFenceProof,
    pub capability_rebind_proof: CapabilityRebindProof,
    pub lease_regrant_proof: LeaseCloseRegrantProof,
    pub target_restore_proof: TargetRestoreProof,
    pub completed_at_ms: u64,
    pub receipt_digest: [u8; 32],
}

impl ProcessMigrationReceipt {
    /// Recomputes and verifies the domain-separated SHA-256 `receipt_digest`.
    #[must_use]
    pub fn verify_digest(&self) -> bool {
        let expected = compute_receipt_digest(
            self.migration_id,
            self.process_id,
            self.source_cell_id,
            self.target_cell_id,
            self.source_generation,
            self.target_generation,
            &self.migration_mode,
            &self.quiesce_proof,
            &self.checkpoint_proof,
            &self.source_fence_proof,
            &self.capability_rebind_proof,
            &self.lease_regrant_proof,
            &self.target_restore_proof,
            self.completed_at_ms,
        );
        self.receipt_digest == expected
    }
}

/// Outcome of aborting a migration prior to target restoration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MigrationAbortOutcome {
    /// Migration was aborted before `SourceFenced`; the source Process generation
    /// was never fenced on the source Cell.
    AbortedPreFence {
        migration_id: [u8; 16],
        process_id: ProcessId,
        aborted_at_step: ProcessMigrationStep,
        reason: String,
        aborted_at_ms: u64,
    },
    /// Migration failed after `SourceFenced` but before `TargetRestored`;
    /// because the source generation is already fenced and target has not
    /// restored, the migration enters `Quarantined` for explicit recovery.
    QuarantinedPostFence {
        migration_id: [u8; 16],
        process_id: ProcessId,
        quarantined_at_step: ProcessMigrationStep,
        source_fencing_token: FencingToken,
        reason: String,
        quarantined_at_ms: u64,
    },
}

/// Fail-closed errors returned by the Process migration state machine
/// (`[DIST-MIGRATE-001]` and `[DIST-MIGRATE-002]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessMigrationError {
    /// An identity or digest field was all-zero or malformed.
    InvalidIdentity(&'static str),
    /// Source and target Cell IDs are identical.
    SameSourceAndTargetCell { cell_id: [u8; 16] },
    /// `target_generation` is not `source_generation + 1` or `source_generation == 0`.
    InvalidTargetGeneration {
        source_generation: u64,
        target_generation: u64,
    },
    /// Duplicate driver `session_id` in the attached session list.
    DuplicateDriverSession { session_id: [u8; 16] },
    /// `[DIST-MIGRATE-002]`: A `PinnedNonMigratable` driver session was attached
    /// under [`PinnedSessionPolicy::RefuseMigration`].
    PinnedSessionNonMigratable {
        session_id: [u8; 16],
        pin_reason: String,
    },
    /// `[DIST-MIGRATE-002]`: Downgrade to `restart-from-artifact` was requested,
    /// but a pinned session had no `artifact_restart_image_digest`.
    MissingArtifactRestartDigest {
        session_id: [u8; 16],
        pin_reason: String,
    },
    /// Multiple pinned sessions declared conflicting artifact restart image digests.
    ConflictingArtifactRestartDigests {
        first_digest: [u8; 32],
        conflicting_digest: [u8; 32],
    },
    /// Attempted to execute a migration step out of the strict 8-step order.
    InvalidStepOrder {
        current_step: ProcessMigrationStep,
        attempted_step: ProcessMigrationStep,
    },
    /// Re-submitted a proof for an already-completed step with different content.
    ConflictingStepProof { step: ProcessMigrationStep },
    /// A step proof referenced the wrong `process_id`, `cell_id`, or `generation`.
    BindingMismatch {
        step: ProcessMigrationStep,
        field: &'static str,
    },
    /// `QuiesceProof` reported that in-flight effects were not drained.
    SourceNotQuiesced,
    /// `ProcessCheckpointProof` payload does not match the admitted [`MigrationMode`]
    /// or migratable session set (`[DIST-MIGRATE-002]`).
    CheckpointModeMismatch { mode: MigrationMode },
    /// `SourceProcessFenceProof` is missing supervisor unregistration or valid fence.
    SourceFenceIncomplete,
    /// `[LEASE-MIGRATE-001]`: Attempted to regrant a lease on the target Cell
    /// before closing/fencing the source Cell lease.
    SourceLeaseNotClosed { source_lease_id: [u8; 16] },
    /// `TargetRestoreProof` digest does not match the checkpointed state or
    /// artifact image digest.
    RestoreDigestMismatch {
        expected_digest: [u8; 32],
        actual_digest: [u8; 32],
    },
    /// Target restore reused the old source fencing token instead of minting a
    /// fresh generation fence.
    DuplicateOrStaleTargetFence,
    /// Attempted to abort a migration that has already restored on the target Cell.
    CannotAbortAfterTargetRestore { current_step: ProcessMigrationStep },
}

impl fmt::Display for ProcessMigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentity(field) => {
                write!(formatter, "invalid or all-zero migration field: {field}")
            }
            Self::SameSourceAndTargetCell { cell_id } => {
                write!(
                    formatter,
                    "source and target cell_id are identical: {cell_id:?}"
                )
            }
            Self::InvalidTargetGeneration {
                source_generation,
                target_generation,
            } => write!(
                formatter,
                "invalid migration generation transition: {source_generation} -> {target_generation}"
            ),
            Self::DuplicateDriverSession { session_id } => {
                write!(formatter, "duplicate driver session_id: {session_id:?}")
            }
            Self::PinnedSessionNonMigratable {
                session_id,
                pin_reason,
            } => write!(
                formatter,
                "pinned driver session {session_id:?} is non-migratable ({pin_reason})"
            ),
            Self::MissingArtifactRestartDigest {
                session_id,
                pin_reason,
            } => write!(
                formatter,
                "pinned driver session {session_id:?} ({pin_reason}) has no artifact restart image digest"
            ),
            Self::ConflictingArtifactRestartDigests {
                first_digest,
                conflicting_digest,
            } => write!(
                formatter,
                "conflicting artifact restart digests: {first_digest:?} vs {conflicting_digest:?}"
            ),
            Self::InvalidStepOrder {
                current_step,
                attempted_step,
            } => write!(
                formatter,
                "invalid migration step order: cannot execute {attempted_step:?} from {current_step:?}"
            ),
            Self::ConflictingStepProof { step } => {
                write!(
                    formatter,
                    "conflicting proof replay for migration step {step:?}"
                )
            }
            Self::BindingMismatch { step, field } => {
                write!(
                    formatter,
                    "binding mismatch at step {step:?} on field {field}"
                )
            }
            Self::SourceNotQuiesced => {
                formatter.write_str("source process in-flight effects are not drained")
            }
            Self::CheckpointModeMismatch { mode } => {
                write!(
                    formatter,
                    "checkpoint proof does not match migration mode {mode:?}"
                )
            }
            Self::SourceFenceIncomplete => formatter.write_str(
                "source process fence proof is incomplete (supervisor must be unregistered and token non-zero)",
            ),
            Self::SourceLeaseNotClosed { source_lease_id } => write!(
                formatter,
                "source lease {source_lease_id:?} was not closed or fenced before target regrant"
            ),
            Self::RestoreDigestMismatch {
                expected_digest,
                actual_digest,
            } => write!(
                formatter,
                "target restore digest {actual_digest:?} disagrees with checkpoint digest {expected_digest:?}"
            ),
            Self::DuplicateOrStaleTargetFence => {
                formatter.write_str("target restore fencing token must differ from source fence")
            }
            Self::CannotAbortAfterTargetRestore { current_step } => write!(
                formatter,
                "cannot abort migration at step {current_step:?} after target restore"
            ),
        }
    }
}

impl Error for ProcessMigrationError {}

/// Stateful 8-step coordinator enforcing `[DIST-MIGRATE-001]` and `[DIST-MIGRATE-002]`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessMigrationCoordinator {
    plan: ProcessMigrationPlan,
    current_step: ProcessMigrationStep,
    quiesce_proof: Option<QuiesceProof>,
    checkpoint_proof: Option<ProcessCheckpointProof>,
    source_fence_proof: Option<SourceProcessFenceProof>,
    capability_rebind_proof: Option<CapabilityRebindProof>,
    lease_regrant_proof: Option<LeaseCloseRegrantProof>,
    target_restore_proof: Option<TargetRestoreProof>,
    receipt: Option<ProcessMigrationReceipt>,
    abort_outcome: Option<MigrationAbortOutcome>,
}

impl ProcessMigrationCoordinator {
    /// Step 1 (`Initiated`): Validates migration bindings and session migratability
    /// policy (`[DIST-MIGRATE-002]`), admitting a new [`ProcessMigrationCoordinator`].
    ///
    /// # Errors
    ///
    /// Fails closed if any identity is all-zero, `source_cell_id == target_cell_id`,
    /// `target_generation != source_generation + 1`, or a `PinnedNonMigratable`
    /// session violates the configured [`PinnedSessionPolicy`].
    pub fn initiate(
        request: InitiateProcessMigrationRequest,
    ) -> Result<Self, ProcessMigrationError> {
        if request.migration_id == [0u8; 16] {
            return Err(ProcessMigrationError::InvalidIdentity("migration_id"));
        }
        if *request.process_id.as_bytes() == [0u8; 16] {
            return Err(ProcessMigrationError::InvalidIdentity("process_id"));
        }
        if request.source_cell_id == [0u8; 16] {
            return Err(ProcessMigrationError::InvalidIdentity("source_cell_id"));
        }
        if request.target_cell_id == [0u8; 16] {
            return Err(ProcessMigrationError::InvalidIdentity("target_cell_id"));
        }
        if request.source_cell_id == request.target_cell_id {
            return Err(ProcessMigrationError::SameSourceAndTargetCell {
                cell_id: request.source_cell_id,
            });
        }
        if request.source_generation == 0
            || request.source_generation.checked_add(1) != Some(request.target_generation)
        {
            return Err(ProcessMigrationError::InvalidTargetGeneration {
                source_generation: request.source_generation,
                target_generation: request.target_generation,
            });
        }

        let (migration_mode, migratable_sessions) =
            resolve_session_policy(request.attached_sessions, request.pinned_session_policy)?;

        let plan = ProcessMigrationPlan {
            migration_id: request.migration_id,
            process_id: request.process_id,
            source_cell_id: request.source_cell_id,
            target_cell_id: request.target_cell_id,
            source_generation: request.source_generation,
            target_generation: request.target_generation,
            migration_mode,
            migratable_sessions,
            initiated_at_ms: request.initiated_at_ms,
        };

        Ok(Self {
            plan,
            current_step: ProcessMigrationStep::Initiated,
            quiesce_proof: None,
            checkpoint_proof: None,
            source_fence_proof: None,
            capability_rebind_proof: None,
            lease_regrant_proof: None,
            target_restore_proof: None,
            receipt: None,
            abort_outcome: None,
        })
    }

    /// Returns the immutable [`ProcessMigrationPlan`] admitted at step 1.
    #[must_use]
    pub const fn plan(&self) -> &ProcessMigrationPlan {
        &self.plan
    }

    /// Returns the current [`ProcessMigrationStep`] of the coordinator.
    #[must_use]
    pub const fn current_step(&self) -> ProcessMigrationStep {
        self.current_step
    }

    /// Returns the resolved [`MigrationMode`].
    #[must_use]
    pub const fn migration_mode(&self) -> &MigrationMode {
        &self.plan.migration_mode
    }

    /// Returns the sealed [`ProcessMigrationReceipt`] if the migration has completed.
    #[must_use]
    pub const fn receipt(&self) -> Option<&ProcessMigrationReceipt> {
        self.receipt.as_ref()
    }

    /// Returns the [`MigrationAbortOutcome`] if the migration was aborted or quarantined.
    #[must_use]
    pub const fn abort_outcome(&self) -> Option<&MigrationAbortOutcome> {
        self.abort_outcome.as_ref()
    }

    /// Step 2 (`Quiesced`): Records proof that active fibers and in-flight
    /// operations on the source Process have been quiesced or cancelled.
    ///
    /// # Errors
    ///
    /// Fails closed on step ordering violations, conflicting proof replay,
    /// binding mismatches, or undrained in-flight effects.
    pub fn record_quiesce(
        &mut self,
        proof: QuiesceProof,
    ) -> Result<MigrationStepDecision, ProcessMigrationError> {
        if let Some(existing) = &self.quiesce_proof {
            if *existing == proof {
                return Ok(MigrationStepDecision::Replayed(
                    ProcessMigrationStep::Quiesced,
                ));
            }
            return Err(ProcessMigrationError::ConflictingStepProof {
                step: ProcessMigrationStep::Quiesced,
            });
        }
        if self.current_step != ProcessMigrationStep::Initiated {
            return Err(ProcessMigrationError::InvalidStepOrder {
                current_step: self.current_step,
                attempted_step: ProcessMigrationStep::Quiesced,
            });
        }
        if proof.process_id != self.plan.process_id {
            return Err(ProcessMigrationError::BindingMismatch {
                step: ProcessMigrationStep::Quiesced,
                field: "process_id",
            });
        }
        if proof.source_cell_id != self.plan.source_cell_id {
            return Err(ProcessMigrationError::BindingMismatch {
                step: ProcessMigrationStep::Quiesced,
                field: "source_cell_id",
            });
        }
        if proof.source_generation != self.plan.source_generation {
            return Err(ProcessMigrationError::BindingMismatch {
                step: ProcessMigrationStep::Quiesced,
                field: "source_generation",
            });
        }
        if !proof.inflight_effects_drained {
            return Err(ProcessMigrationError::SourceNotQuiesced);
        }

        self.quiesce_proof = Some(proof);
        self.current_step = ProcessMigrationStep::Quiesced;
        Ok(MigrationStepDecision::Advanced(
            ProcessMigrationStep::Quiesced,
        ))
    }

    /// Step 3 (`Checkpointed`): Records the state checkpoint or downgraded
    /// artifact restart image digest matching the admitted [`MigrationMode`].
    ///
    /// # Errors
    ///
    /// Fails closed if invoked before `Quiesced`, if the payload kind disagrees
    /// with [`MigrationMode`], or if any digest is invalid.
    pub fn record_checkpoint(
        &mut self,
        proof: ProcessCheckpointProof,
    ) -> Result<MigrationStepDecision, ProcessMigrationError> {
        if let Some(existing) = &self.checkpoint_proof {
            if *existing == proof {
                return Ok(MigrationStepDecision::Replayed(
                    ProcessMigrationStep::Checkpointed,
                ));
            }
            return Err(ProcessMigrationError::ConflictingStepProof {
                step: ProcessMigrationStep::Checkpointed,
            });
        }
        if self.current_step != ProcessMigrationStep::Quiesced {
            return Err(ProcessMigrationError::InvalidStepOrder {
                current_step: self.current_step,
                attempted_step: ProcessMigrationStep::Checkpointed,
            });
        }
        if proof.process_id != self.plan.process_id {
            return Err(ProcessMigrationError::BindingMismatch {
                step: ProcessMigrationStep::Checkpointed,
                field: "process_id",
            });
        }
        if proof.source_generation != self.plan.source_generation {
            return Err(ProcessMigrationError::BindingMismatch {
                step: ProcessMigrationStep::Checkpointed,
                field: "source_generation",
            });
        }

        validate_checkpoint_payload(&self.plan, &proof.payload)?;

        self.checkpoint_proof = Some(proof);
        self.current_step = ProcessMigrationStep::Checkpointed;
        Ok(MigrationStepDecision::Advanced(
            ProcessMigrationStep::Checkpointed,
        ))
    }

    /// Step 4 (`SourceFenced`): Records proof that the source Cell has fenced
    /// `source_generation` and unregistered the Process from the source supervisor.
    ///
    /// # Errors
    ///
    /// Fails closed if invoked before `Checkpointed`, if bindings mismatch, or
    /// if the source supervisor was not unregistered.
    pub fn record_source_fence(
        &mut self,
        proof: SourceProcessFenceProof,
    ) -> Result<MigrationStepDecision, ProcessMigrationError> {
        if let Some(existing) = &self.source_fence_proof {
            if *existing == proof {
                return Ok(MigrationStepDecision::Replayed(
                    ProcessMigrationStep::SourceFenced,
                ));
            }
            return Err(ProcessMigrationError::ConflictingStepProof {
                step: ProcessMigrationStep::SourceFenced,
            });
        }
        if self.current_step != ProcessMigrationStep::Checkpointed {
            return Err(ProcessMigrationError::InvalidStepOrder {
                current_step: self.current_step,
                attempted_step: ProcessMigrationStep::SourceFenced,
            });
        }
        if proof.process_id != self.plan.process_id {
            return Err(ProcessMigrationError::BindingMismatch {
                step: ProcessMigrationStep::SourceFenced,
                field: "process_id",
            });
        }
        if proof.source_cell_id != self.plan.source_cell_id {
            return Err(ProcessMigrationError::BindingMismatch {
                step: ProcessMigrationStep::SourceFenced,
                field: "source_cell_id",
            });
        }
        if proof.fenced_generation != self.plan.source_generation {
            return Err(ProcessMigrationError::BindingMismatch {
                step: ProcessMigrationStep::SourceFenced,
                field: "fenced_generation",
            });
        }
        if proof.source_cell_epoch == 0
            || proof.source_fencing_token == [0u8; 32]
            || !proof.supervisor_unregistered
        {
            return Err(ProcessMigrationError::SourceFenceIncomplete);
        }

        self.source_fence_proof = Some(proof);
        self.current_step = ProcessMigrationStep::SourceFenced;
        Ok(MigrationStepDecision::Advanced(
            ProcessMigrationStep::SourceFenced,
        ))
    }

    /// Step 5 (`CapabilitiesRebound`): Records proof that capabilities held by
    /// the Process have been rebound to `(target_cell_id, target_generation)`.
    ///
    /// # Errors
    ///
    /// Fails closed if invoked before `SourceFenced` or if any capability entry
    /// binds to the wrong Cell or generation.
    pub fn record_capability_rebind(
        &mut self,
        proof: CapabilityRebindProof,
    ) -> Result<MigrationStepDecision, ProcessMigrationError> {
        if let Some(existing) = &self.capability_rebind_proof {
            if *existing == proof {
                return Ok(MigrationStepDecision::Replayed(
                    ProcessMigrationStep::CapabilitiesRebound,
                ));
            }
            return Err(ProcessMigrationError::ConflictingStepProof {
                step: ProcessMigrationStep::CapabilitiesRebound,
            });
        }
        if self.current_step != ProcessMigrationStep::SourceFenced {
            return Err(ProcessMigrationError::InvalidStepOrder {
                current_step: self.current_step,
                attempted_step: ProcessMigrationStep::CapabilitiesRebound,
            });
        }
        if proof.process_id != self.plan.process_id {
            return Err(ProcessMigrationError::BindingMismatch {
                step: ProcessMigrationStep::CapabilitiesRebound,
                field: "process_id",
            });
        }
        for entry in &proof.rebound_capabilities {
            if entry.capability_id == [0u8; 16] {
                return Err(ProcessMigrationError::InvalidIdentity("capability_id"));
            }
            if entry.attenuation_digest == [0u8; 32] {
                return Err(ProcessMigrationError::InvalidIdentity("attenuation_digest"));
            }
            if entry.source_cell_id != self.plan.source_cell_id {
                return Err(ProcessMigrationError::BindingMismatch {
                    step: ProcessMigrationStep::CapabilitiesRebound,
                    field: "source_cell_id",
                });
            }
            if entry.source_generation != self.plan.source_generation {
                return Err(ProcessMigrationError::BindingMismatch {
                    step: ProcessMigrationStep::CapabilitiesRebound,
                    field: "source_generation",
                });
            }
            if entry.target_cell_id != self.plan.target_cell_id {
                return Err(ProcessMigrationError::BindingMismatch {
                    step: ProcessMigrationStep::CapabilitiesRebound,
                    field: "target_cell_id",
                });
            }
            if entry.target_generation != self.plan.target_generation {
                return Err(ProcessMigrationError::BindingMismatch {
                    step: ProcessMigrationStep::CapabilitiesRebound,
                    field: "target_generation",
                });
            }
        }

        self.capability_rebind_proof = Some(proof);
        self.current_step = ProcessMigrationStep::CapabilitiesRebound;
        Ok(MigrationStepDecision::Advanced(
            ProcessMigrationStep::CapabilitiesRebound,
        ))
    }

    /// Step 6 (`LeasesRegranted`): Records proof that source Cell leases were
    /// closed or fenced before target Cell leases were granted (`[LEASE-MIGRATE-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed if invoked before `CapabilitiesRebound`, if bindings mismatch,
    /// or if any lease was not closed/fenced on the source Cell first.
    pub fn record_lease_close_regrant(
        &mut self,
        proof: LeaseCloseRegrantProof,
    ) -> Result<MigrationStepDecision, ProcessMigrationError> {
        if let Some(existing) = &self.lease_regrant_proof {
            if *existing == proof {
                return Ok(MigrationStepDecision::Replayed(
                    ProcessMigrationStep::LeasesRegranted,
                ));
            }
            return Err(ProcessMigrationError::ConflictingStepProof {
                step: ProcessMigrationStep::LeasesRegranted,
            });
        }
        if self.current_step != ProcessMigrationStep::CapabilitiesRebound {
            return Err(ProcessMigrationError::InvalidStepOrder {
                current_step: self.current_step,
                attempted_step: ProcessMigrationStep::LeasesRegranted,
            });
        }
        if proof.process_id != self.plan.process_id {
            return Err(ProcessMigrationError::BindingMismatch {
                step: ProcessMigrationStep::LeasesRegranted,
                field: "process_id",
            });
        }
        if proof.source_generation != self.plan.source_generation {
            return Err(ProcessMigrationError::BindingMismatch {
                step: ProcessMigrationStep::LeasesRegranted,
                field: "source_generation",
            });
        }
        if proof.target_generation != self.plan.target_generation {
            return Err(ProcessMigrationError::BindingMismatch {
                step: ProcessMigrationStep::LeasesRegranted,
                field: "target_generation",
            });
        }
        for lease in &proof.migrated_leases {
            if lease.source_lease_id == [0u8; 16] || lease.target_lease_id == [0u8; 16] {
                return Err(ProcessMigrationError::InvalidIdentity("lease_id"));
            }
            if lease.source_lease_id == lease.target_lease_id {
                return Err(ProcessMigrationError::InvalidIdentity(
                    "target_lease_id must be distinct from source_lease_id",
                ));
            }
            if lease.source_cell_id != self.plan.source_cell_id {
                return Err(ProcessMigrationError::BindingMismatch {
                    step: ProcessMigrationStep::LeasesRegranted,
                    field: "source_cell_id",
                });
            }
            if lease.target_cell_id != self.plan.target_cell_id {
                return Err(ProcessMigrationError::BindingMismatch {
                    step: ProcessMigrationStep::LeasesRegranted,
                    field: "target_cell_id",
                });
            }
            if !lease.source_closed_or_fenced
                || lease.source_close_receipt_digest == [0u8; 32]
                || lease.target_grant_epoch == 0
            {
                return Err(ProcessMigrationError::SourceLeaseNotClosed {
                    source_lease_id: lease.source_lease_id,
                });
            }
        }

        self.lease_regrant_proof = Some(proof);
        self.current_step = ProcessMigrationStep::LeasesRegranted;
        Ok(MigrationStepDecision::Advanced(
            ProcessMigrationStep::LeasesRegranted,
        ))
    }

    /// Step 7 (`TargetRestored`): Records proof that the Process was restored on
    /// `target_cell_id` at `target_generation = source_generation + 1`.
    ///
    /// # Errors
    ///
    /// Fails closed if invoked before `LeasesRegranted`, if the restored
    /// generation or Cell mismatches, if the restored digest disagrees with the
    /// checkpoint/artifact digest, or if the target fence is stale.
    pub fn record_target_restore(
        &mut self,
        proof: TargetRestoreProof,
    ) -> Result<MigrationStepDecision, ProcessMigrationError> {
        if let Some(existing) = &self.target_restore_proof {
            if *existing == proof {
                return Ok(MigrationStepDecision::Replayed(
                    ProcessMigrationStep::TargetRestored,
                ));
            }
            return Err(ProcessMigrationError::ConflictingStepProof {
                step: ProcessMigrationStep::TargetRestored,
            });
        }
        if self.current_step != ProcessMigrationStep::LeasesRegranted {
            return Err(ProcessMigrationError::InvalidStepOrder {
                current_step: self.current_step,
                attempted_step: ProcessMigrationStep::TargetRestored,
            });
        }
        if proof.process_id != self.plan.process_id {
            return Err(ProcessMigrationError::BindingMismatch {
                step: ProcessMigrationStep::TargetRestored,
                field: "process_id",
            });
        }
        if proof.target_cell_id != self.plan.target_cell_id {
            return Err(ProcessMigrationError::BindingMismatch {
                step: ProcessMigrationStep::TargetRestored,
                field: "target_cell_id",
            });
        }
        if proof.restored_generation != self.plan.target_generation {
            return Err(ProcessMigrationError::BindingMismatch {
                step: ProcessMigrationStep::TargetRestored,
                field: "restored_generation",
            });
        }
        if proof.target_fencing_token == [0u8; 32]
            || *proof.target_isolation_domain_id.as_bytes() == [0u8; 16]
        {
            return Err(ProcessMigrationError::InvalidIdentity(
                "target_fencing_token or target_isolation_domain_id",
            ));
        }

        let source_fence =
            self.source_fence_proof
                .as_ref()
                .ok_or(ProcessMigrationError::InvalidStepOrder {
                    current_step: self.current_step,
                    attempted_step: ProcessMigrationStep::TargetRestored,
                })?;
        if proof.target_fencing_token == source_fence.source_fencing_token {
            return Err(ProcessMigrationError::DuplicateOrStaleTargetFence);
        }

        let checkpoint =
            self.checkpoint_proof
                .as_ref()
                .ok_or(ProcessMigrationError::InvalidStepOrder {
                    current_step: self.current_step,
                    attempted_step: ProcessMigrationStep::TargetRestored,
                })?;
        let expected_digest = checkpoint.payload.primary_digest();
        if proof.restored_image_or_checkpoint_digest != expected_digest {
            return Err(ProcessMigrationError::RestoreDigestMismatch {
                expected_digest,
                actual_digest: proof.restored_image_or_checkpoint_digest,
            });
        }

        self.target_restore_proof = Some(proof);
        self.current_step = ProcessMigrationStep::TargetRestored;
        Ok(MigrationStepDecision::Advanced(
            ProcessMigrationStep::TargetRestored,
        ))
    }

    /// Step 8 (`Completed`): Seals the migration and emits an immutable,
    /// domain-separated [`ProcessMigrationReceipt`] (`[DIST-MIGRATE-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed if invoked before `TargetRestored` or if replayed with a
    /// conflicting completion timestamp.
    pub fn complete(
        &mut self,
        completed_at_ms: u64,
    ) -> Result<ProcessMigrationReceipt, ProcessMigrationError> {
        if let Some(existing) = &self.receipt {
            if existing.completed_at_ms == completed_at_ms {
                return Ok(existing.clone());
            }
            return Err(ProcessMigrationError::ConflictingStepProof {
                step: ProcessMigrationStep::Completed,
            });
        }
        if self.current_step != ProcessMigrationStep::TargetRestored {
            return Err(ProcessMigrationError::InvalidStepOrder {
                current_step: self.current_step,
                attempted_step: ProcessMigrationStep::Completed,
            });
        }

        let quiesce_proof = self.quiesce_proof.clone().ok_or({
            ProcessMigrationError::InvalidStepOrder {
                current_step: self.current_step,
                attempted_step: ProcessMigrationStep::Completed,
            }
        })?;
        let checkpoint_proof = self.checkpoint_proof.clone().ok_or({
            ProcessMigrationError::InvalidStepOrder {
                current_step: self.current_step,
                attempted_step: ProcessMigrationStep::Completed,
            }
        })?;
        let source_fence_proof = self.source_fence_proof.clone().ok_or({
            ProcessMigrationError::InvalidStepOrder {
                current_step: self.current_step,
                attempted_step: ProcessMigrationStep::Completed,
            }
        })?;
        let capability_rebind_proof = self.capability_rebind_proof.clone().ok_or({
            ProcessMigrationError::InvalidStepOrder {
                current_step: self.current_step,
                attempted_step: ProcessMigrationStep::Completed,
            }
        })?;
        let lease_regrant_proof = self.lease_regrant_proof.clone().ok_or({
            ProcessMigrationError::InvalidStepOrder {
                current_step: self.current_step,
                attempted_step: ProcessMigrationStep::Completed,
            }
        })?;
        let target_restore_proof = self.target_restore_proof.clone().ok_or({
            ProcessMigrationError::InvalidStepOrder {
                current_step: self.current_step,
                attempted_step: ProcessMigrationStep::Completed,
            }
        })?;

        let receipt_digest = compute_receipt_digest(
            self.plan.migration_id,
            self.plan.process_id,
            self.plan.source_cell_id,
            self.plan.target_cell_id,
            self.plan.source_generation,
            self.plan.target_generation,
            &self.plan.migration_mode,
            &quiesce_proof,
            &checkpoint_proof,
            &source_fence_proof,
            &capability_rebind_proof,
            &lease_regrant_proof,
            &target_restore_proof,
            completed_at_ms,
        );

        let receipt = ProcessMigrationReceipt {
            migration_id: self.plan.migration_id,
            process_id: self.plan.process_id,
            source_cell_id: self.plan.source_cell_id,
            target_cell_id: self.plan.target_cell_id,
            source_generation: self.plan.source_generation,
            target_generation: self.plan.target_generation,
            migration_mode: self.plan.migration_mode.clone(),
            quiesce_proof,
            checkpoint_proof,
            source_fence_proof,
            capability_rebind_proof,
            lease_regrant_proof,
            target_restore_proof,
            completed_at_ms,
            receipt_digest,
        };

        self.receipt = Some(receipt.clone());
        self.current_step = ProcessMigrationStep::Completed;
        Ok(receipt)
    }

    /// Aborts a migration that has not yet reached `TargetRestored`.
    ///
    /// - If called before `SourceFenced` (`Initiated`, `Quiesced`, `Checkpointed`),
    ///   transitions to [`ProcessMigrationStep::Aborted`] (`AbortedPreFence`).
    /// - If called at or after `SourceFenced` (`SourceFenced`, `CapabilitiesRebound`,
    ///   `LeasesRegranted`), transitions to [`ProcessMigrationStep::Quarantined`]
    ///   (`QuarantinedPostFence`) because the source generation is already fenced.
    ///
    /// # Errors
    ///
    /// Fails closed if `TargetRestored` or `Completed` has already been reached,
    /// or if replayed with conflicting abort parameters.
    pub fn abort_before_target_restore(
        &mut self,
        reason: impl Into<String>,
        aborted_at_ms: u64,
    ) -> Result<MigrationAbortOutcome, ProcessMigrationError> {
        let reason = reason.into();
        if reason.trim().is_empty() {
            return Err(ProcessMigrationError::InvalidIdentity("abort_reason"));
        }

        if let Some(existing) = &self.abort_outcome {
            let matches = match existing {
                MigrationAbortOutcome::AbortedPreFence {
                    reason: r,
                    aborted_at_ms: ts,
                    ..
                }
                | MigrationAbortOutcome::QuarantinedPostFence {
                    reason: r,
                    quarantined_at_ms: ts,
                    ..
                } => *r == reason && *ts == aborted_at_ms,
            };
            if matches {
                return Ok(existing.clone());
            }
            return Err(ProcessMigrationError::ConflictingStepProof {
                step: self.current_step,
            });
        }

        match self.current_step {
            ProcessMigrationStep::Initiated
            | ProcessMigrationStep::Quiesced
            | ProcessMigrationStep::Checkpointed => {
                let outcome = MigrationAbortOutcome::AbortedPreFence {
                    migration_id: self.plan.migration_id,
                    process_id: self.plan.process_id,
                    aborted_at_step: self.current_step,
                    reason,
                    aborted_at_ms,
                };
                self.current_step = ProcessMigrationStep::Aborted;
                self.abort_outcome = Some(outcome.clone());
                Ok(outcome)
            }
            ProcessMigrationStep::SourceFenced
            | ProcessMigrationStep::CapabilitiesRebound
            | ProcessMigrationStep::LeasesRegranted => {
                let source_fencing_token = self
                    .source_fence_proof
                    .as_ref()
                    .map_or([0u8; 32], |proof| proof.source_fencing_token);
                let outcome = MigrationAbortOutcome::QuarantinedPostFence {
                    migration_id: self.plan.migration_id,
                    process_id: self.plan.process_id,
                    quarantined_at_step: self.current_step,
                    source_fencing_token,
                    reason,
                    quarantined_at_ms: aborted_at_ms,
                };
                self.current_step = ProcessMigrationStep::Quarantined;
                self.abort_outcome = Some(outcome.clone());
                Ok(outcome)
            }
            ProcessMigrationStep::TargetRestored
            | ProcessMigrationStep::Completed
            | ProcessMigrationStep::Aborted
            | ProcessMigrationStep::Quarantined => {
                Err(ProcessMigrationError::CannotAbortAfterTargetRestore {
                    current_step: self.current_step,
                })
            }
        }
    }
}

fn resolve_session_policy(
    attached_sessions: Vec<DriverSessionAttachment>,
    policy: PinnedSessionPolicy,
) -> Result<(MigrationMode, MigratableSessionDigests), ProcessMigrationError> {
    let mut seen_ids = BTreeSet::new();
    let mut migratable_sessions = Vec::new();
    let mut dropped_pinned_sessions = Vec::new();
    let mut resolved_artifact_digest: Option<[u8; 32]> = None;

    for session in attached_sessions {
        let session_id = session.session_id();
        if session_id == [0u8; 16] {
            return Err(ProcessMigrationError::InvalidIdentity("session_id"));
        }
        if !seen_ids.insert(session_id) {
            return Err(ProcessMigrationError::DuplicateDriverSession { session_id });
        }

        match session {
            DriverSessionAttachment::Migratable {
                session_id,
                state_digest,
            } => {
                if state_digest == [0u8; 32] {
                    return Err(ProcessMigrationError::InvalidIdentity("state_digest"));
                }
                migratable_sessions.push((session_id, state_digest));
            }
            DriverSessionAttachment::PinnedNonMigratable {
                session_id,
                pin_reason,
                artifact_restart_image_digest,
            } => {
                if pin_reason.trim().is_empty() {
                    return Err(ProcessMigrationError::InvalidIdentity("pin_reason"));
                }
                match policy {
                    PinnedSessionPolicy::RefuseMigration => {
                        return Err(ProcessMigrationError::PinnedSessionNonMigratable {
                            session_id,
                            pin_reason,
                        });
                    }
                    PinnedSessionPolicy::DowngradeToRestartFromArtifact => {
                        let Some(digest) = artifact_restart_image_digest else {
                            return Err(ProcessMigrationError::MissingArtifactRestartDigest {
                                session_id,
                                pin_reason,
                            });
                        };
                        if digest == [0u8; 32] {
                            return Err(ProcessMigrationError::InvalidIdentity(
                                "artifact_restart_image_digest",
                            ));
                        }
                        if let Some(first_digest) = resolved_artifact_digest {
                            if first_digest != digest {
                                return Err(
                                    ProcessMigrationError::ConflictingArtifactRestartDigests {
                                        first_digest,
                                        conflicting_digest: digest,
                                    },
                                );
                            }
                        } else {
                            resolved_artifact_digest = Some(digest);
                        }
                        dropped_pinned_sessions.push(session_id);
                    }
                }
            }
        }
    }

    if let Some(artifact_image_digest) = resolved_artifact_digest {
        Ok((
            MigrationMode::DowngradedRestartFromArtifact {
                artifact_image_digest,
                dropped_pinned_sessions,
            },
            migratable_sessions,
        ))
    } else {
        Ok((
            MigrationMode::TransparentCheckpointRestore,
            migratable_sessions,
        ))
    }
}

fn validate_checkpoint_payload(
    plan: &ProcessMigrationPlan,
    payload: &CheckpointPayloadKind,
) -> Result<(), ProcessMigrationError> {
    match (&plan.migration_mode, payload) {
        (
            MigrationMode::TransparentCheckpointRestore,
            CheckpointPayloadKind::FullStateCheckpoint {
                state_checkpoint_digest,
                migrated_session_digests,
            },
        ) => {
            if *state_checkpoint_digest == [0u8; 32] {
                return Err(ProcessMigrationError::InvalidIdentity(
                    "state_checkpoint_digest",
                ));
            }
            if *migrated_session_digests != plan.migratable_sessions {
                return Err(ProcessMigrationError::CheckpointModeMismatch {
                    mode: plan.migration_mode.clone(),
                });
            }
            Ok(())
        }
        (
            MigrationMode::DowngradedRestartFromArtifact {
                artifact_image_digest: expected_digest,
                ..
            },
            CheckpointPayloadKind::ArtifactRestartImage {
                artifact_image_digest,
            },
        ) => {
            if artifact_image_digest != expected_digest {
                return Err(ProcessMigrationError::CheckpointModeMismatch {
                    mode: plan.migration_mode.clone(),
                });
            }
            Ok(())
        }
        _ => Err(ProcessMigrationError::CheckpointModeMismatch {
            mode: plan.migration_mode.clone(),
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn compute_receipt_digest(
    migration_id: [u8; 16],
    process_id: ProcessId,
    source_cell_id: [u8; 16],
    target_cell_id: [u8; 16],
    source_generation: u64,
    target_generation: u64,
    migration_mode: &MigrationMode,
    quiesce_proof: &QuiesceProof,
    checkpoint_proof: &ProcessCheckpointProof,
    source_fence_proof: &SourceProcessFenceProof,
    capability_rebind_proof: &CapabilityRebindProof,
    lease_regrant_proof: &LeaseCloseRegrantProof,
    target_restore_proof: &TargetRestoreProof,
    completed_at_ms: u64,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(PROCESS_MIGRATION_RECEIPT_DOMAIN);
    hasher.update([0u8]);
    hasher.update(migration_id);
    hasher.update(process_id.as_bytes());
    hasher.update(source_cell_id);
    hasher.update(target_cell_id);
    hasher.update(source_generation.to_be_bytes());
    hasher.update(target_generation.to_be_bytes());

    match migration_mode {
        MigrationMode::TransparentCheckpointRestore => {
            hasher.update([1u8]);
        }
        MigrationMode::DowngradedRestartFromArtifact {
            artifact_image_digest,
            dropped_pinned_sessions,
        } => {
            hasher.update([2u8]);
            hasher.update(artifact_image_digest);
            hasher.update((dropped_pinned_sessions.len() as u64).to_be_bytes());
            for session_id in dropped_pinned_sessions {
                hasher.update(session_id);
            }
        }
    }

    hasher.update(quiesce_proof.quiesced_fibers.to_be_bytes());
    hasher.update(quiesce_proof.cancelled_inflight_ops.to_be_bytes());
    hasher.update([u8::from(quiesce_proof.inflight_effects_drained)]);
    hasher.update(quiesce_proof.quiesced_at_ms.to_be_bytes());

    hasher.update(checkpoint_proof.payload.primary_digest());
    hasher.update(checkpoint_proof.checkpointed_at_ms.to_be_bytes());

    hasher.update(source_fence_proof.source_cell_epoch.to_be_bytes());
    hasher.update(source_fence_proof.fenced_generation.to_be_bytes());
    hasher.update(source_fence_proof.source_fencing_token);
    hasher.update([u8::from(source_fence_proof.supervisor_unregistered)]);
    hasher.update(source_fence_proof.fenced_at_ms.to_be_bytes());

    hasher.update((capability_rebind_proof.rebound_capabilities.len() as u64).to_be_bytes());
    for cap in &capability_rebind_proof.rebound_capabilities {
        hasher.update(cap.capability_id);
        hasher.update(cap.source_cell_id);
        hasher.update(cap.source_generation.to_be_bytes());
        hasher.update(cap.target_cell_id);
        hasher.update(cap.target_generation.to_be_bytes());
        hasher.update(cap.attenuation_digest);
    }
    hasher.update(capability_rebind_proof.rebound_at_ms.to_be_bytes());

    hasher.update((lease_regrant_proof.migrated_leases.len() as u64).to_be_bytes());
    for lease in &lease_regrant_proof.migrated_leases {
        hasher.update(lease.source_lease_id);
        hasher.update(lease.source_cell_id);
        hasher.update([u8::from(lease.source_closed_or_fenced)]);
        hasher.update(lease.source_close_receipt_digest);
        hasher.update(lease.target_lease_id);
        hasher.update(lease.target_cell_id);
        hasher.update(lease.target_grant_epoch.to_be_bytes());
    }
    hasher.update(lease_regrant_proof.regranted_at_ms.to_be_bytes());

    hasher.update(target_restore_proof.restored_generation.to_be_bytes());
    hasher.update(target_restore_proof.target_fencing_token);
    hasher.update(target_restore_proof.target_isolation_domain_id.as_bytes());
    hasher.update(target_restore_proof.restored_image_or_checkpoint_digest);
    hasher.update(target_restore_proof.restored_at_ms.to_be_bytes());

    hasher.update(completed_at_ms.to_be_bytes());

    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}
