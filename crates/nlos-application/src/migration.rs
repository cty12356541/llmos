//! PKG migration runner (W29-E, B1-3): the `[PKG-UPDATE-001]` staged
//! same-major migration between package revisions — durable drill state,
//! a typed post-migration health-check verdict, the single-transaction
//! atomic switch, and the PKG-level failed-health rollback.
//!
//! The runner extends the update path's precedent instead of replacing
//! it: `update_application` remains the one-step content-advance channel
//! (compatibility window + one CAS); the migration runner is the
//! multi-step drill the spec mandates for revisions that carry data or
//! schema consequences —
//!
//! ```text
//! begin (pending)  →  step 1..N (running)  →  health verdict (terminal)
//!      ├─ passed  → activate: ONE transaction = generation CAS +1,
//!      │            installation receipt at the new generation,
//!      │            migration row → done          (the atomic switch)
//!      └─ failed  → rollback: ONE transaction = migration row → failed
//!                   + PKG rollback receipt; the application row is
//!                   never touched (the prior revision stays active)
//! ```
//!
//! Durability discipline (mirroring every prior authority surface):
//!
//! - **One durable row per drill**, keyed by the command's idempotency
//!   key; replay returns the recorded state and never re-validates. A
//!   partial unique index enforces at most one *live* drill per
//!   application; terminal drills accumulate as history.
//! - **Steps are durable facts** in strict order (`1 + completed`), so a
//!   crash between steps converges on restart by replaying the recorded
//!   prefix and continuing at the next index — the staged-migration
//!   precedent of the artifact authority's schema migrations.
//! - **The health verdict is terminal once recorded** (DDL lattice
//!   `unchecked → passed|failed`): the durable verdict is the authority
//!   and replay never re-consults the probe. The authority itself
//!   re-reads the artifact receipt and re-checks the recorded target
//!   binding before consulting the probe (the honest "manifest verify
//!   re-run" face available from durable state; config-shape validity
//!   and richer checks live in caller-supplied
//!   [`MigrationHealthProbe`] implementations).
//! - **The atomic switch is one `Immediate` transaction**: the
//!   application-row CAS (`generation = from + 1`, manifest digest →
//!   target, `status = installed`, frozen baseline predicate), the
//!   immutable installation receipt at the new generation (same table
//!   and generation-bounds guard as install/update, seven-equation
//!   digest binding against the verified receipt), and the migration
//!   row's `done` transition with the activated installation reference
//!   (DDL: `done` requires a passed verdict *and* the recorded
//!   activation) live and die together.
//! - **The PKG rollback is distinct from the lifecycle rollback**: it
//!   never touches the application row (the failed drill never activated
//!   anything, so the prior revision is *already* the active one — the
//!   receipt records that fact durably and replayably), whereas
//!   `rollback_application` forward-rolls a disabled/uninstalled
//!   application's previous content onto a fresh generation. Different
//!   tables, different gates, different semantics.
//!
//! Honest scope: the drill's steps are caller-driven records (this
//! authority owns lifecycle facts, not user data — step *execution*
//! happens outside, step *completion* is a durable mark); there is no
//! abandon command for a live-but-superseded drill, no `SNAPSHOT_ASSISTED_
//! ROLLBACK`/`IRREVERSIBLE` declaration surface, and no Trusted-UI
//! confirmation wiring (all deferred follow-up lanes).

use nlos_artifact::{ArtifactStore, ContentDigest, PackageVerificationReceipt};
use nlos_types::{ApplicationId, Generation, IdempotencyKey, InstallationId, PackageId, ReceiptId};
use rusqlite::{Connection, TransactionBehavior, params};

use crate::{
    ApplicationAuthority, ApplicationAuthorityError, ApplicationStatus, CompatibilityWindow,
    InstallationReceipt, binding_error, decode_u64, derive_installation_id, encode_u64,
    insert_receipt, load_application_by_package, load_installation_receipt_at_generation,
    readback_verified_receipt_by_id,
};

/// Durable state of one staged package migration (schema v7 `state`
/// column). The lattice is `Pending → Running → Done | Failed` with
/// terminal `Done`/`Failed`; only the first recorded step leaves
/// `Pending`, and only activation/rollback terminate a live drill.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationState {
    /// The drill was durably accepted; no step has been recorded yet.
    Pending,
    /// At least one step is recorded; the drill is resumable.
    Running,
    /// Terminal: the atomic switch committed (activation recorded).
    Done,
    /// Terminal: the health check failed and the PKG rollback committed.
    Failed,
}

impl MigrationState {
    pub(crate) const fn encode(self) -> i64 {
        match self {
            Self::Pending => 1,
            Self::Running => 2,
            Self::Done => 3,
            Self::Failed => 4,
        }
    }

    pub(crate) fn decode(value: i64) -> Result<Self, ApplicationAuthorityError> {
        match value {
            1 => Ok(Self::Pending),
            2 => Ok(Self::Running),
            3 => Ok(Self::Done),
            4 => Ok(Self::Failed),
            _ => Err(ApplicationAuthorityError::CorruptRecord(
                "unknown application migration state",
            )),
        }
    }

    const fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

/// Durable health-check verdict of one drill (schema v7 `health_state`
/// column). `Unchecked → Passed | Failed` is a terminal transition: the
/// first recorded verdict is the authority and is never re-recorded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationHealthState {
    /// No verdict yet (the `health_checked_at_ms` column is NULL).
    Unchecked,
    /// The probe pronounced the migrated target revision healthy.
    Passed,
    /// The probe refused; the drill must take the PKG rollback path.
    Failed,
}

impl MigrationHealthState {
    pub(crate) const fn encode(self) -> i64 {
        match self {
            Self::Unchecked => 0,
            Self::Passed => 1,
            Self::Failed => 2,
        }
    }

    pub(crate) fn decode(value: i64) -> Result<Self, ApplicationAuthorityError> {
        match value {
            0 => Ok(Self::Unchecked),
            1 => Ok(Self::Passed),
            2 => Ok(Self::Failed),
            _ => Err(ApplicationAuthorityError::CorruptRecord(
                "unknown application migration health state",
            )),
        }
    }
}

impl CompatibilityWindow {
    pub(crate) const fn encode(self) -> i64 {
        match self {
            Self::SameMajor => 1,
            Self::SameMinor => 2,
        }
    }

    pub(crate) fn decode(value: i64) -> Result<Self, ApplicationAuthorityError> {
        match value {
            1 => Ok(Self::SameMajor),
            2 => Ok(Self::SameMinor),
            _ => Err(ApplicationAuthorityError::CorruptRecord(
                "unknown application migration compatibility window",
            )),
        }
    }
}

/// Read-only view of one durable migration row (the drill's current
/// state, its frozen baseline and target binding, and its progress).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationView {
    pub idempotency_key: IdempotencyKey,
    pub application_id: ApplicationId,
    pub package_id: PackageId,
    /// The installation generation the drill started from (frozen).
    pub from_generation: Generation,
    /// The manifest digest the application must still be at for the
    /// drill to activate or roll back (frozen baseline).
    pub from_manifest_digest: ContentDigest,
    pub from_package_version: u64,
    /// The artifact-authority verified receipt the drill targets.
    pub package_verification_receipt_id: ReceiptId,
    pub target_manifest_digest: ContentDigest,
    pub target_package_version: u64,
    pub compatibility_window: CompatibilityWindow,
    pub declared_step_count: u64,
    pub completed_step_count: u64,
    pub state: MigrationState,
    pub health: MigrationHealthState,
    pub health_checked_at_ms: Option<u64>,
    /// Set only in the `Done` state (the atomic switch's installation).
    pub activated_installation_id: Option<InstallationId>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Durable completion mark of one drill step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationStepRecord {
    pub idempotency_key: IdempotencyKey,
    pub step_index: u64,
    pub completed_at_ms: u64,
}

/// Durable typed health-check verdict: pass/fail as recorded, replayable
/// byte-equal, never re-recorded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationHealthReport {
    pub idempotency_key: IdempotencyKey,
    pub passed: bool,
    pub checked_at_ms: u64,
}

/// Immutable durable proof that one migration failed its health check
/// and the *prior* revision remained active — the PKG-level rollback
/// fact (distinct from the lifecycle [`crate::RollbackReceipt`]: the
/// application row is never touched, because a failed drill never
/// activated anything to undo).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationRollbackReceipt {
    /// The migration drill this receipt terminated (one rollback per
    /// drill; the drill's own key addresses it).
    pub idempotency_key: IdempotencyKey,
    pub application_id: ApplicationId,
    /// The manifest digest of the abandoned target revision.
    pub abandoned_target_manifest_digest: ContentDigest,
    /// The generation that remained active throughout (the frozen
    /// baseline).
    pub retained_generation: Generation,
    /// The manifest digest that remained active throughout.
    pub retained_manifest_digest: ContentDigest,
    pub rolled_back_at_ms: u64,
}

/// Request to begin one staged migration drill (authority-first: the
/// target revision is named by its artifact-authority verification
/// receipt id, exactly like install/update).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MigrateApplicationRequest {
    /// The package identity whose installed application is migrated.
    pub package_id: PackageId,
    /// Receipt id of the newly verified signed target package.
    pub package_verification_receipt_id: ReceiptId,
    /// Caller-supplied exactly-once key for the drill.
    pub idempotency_key: IdempotencyKey,
    /// Explicit compatibility window validated pre-mutation against the
    /// current installation's version (the update-channel semantics).
    pub compatibility_window: CompatibilityWindow,
    /// The declared number of drill steps (≥ 1); each step's completion
    /// is durably recorded before activation is possible.
    pub declared_step_count: u64,
    pub requested_at_ms: u64,
}

/// Outcome of one [`ApplicationAuthority::migrate_application`] call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MigrateDecision {
    /// First execution of this key: the drill committed durably in the
    /// `Pending` state with the declared step count.
    Started(MigrationView),
    /// Durable replay: this key already began; the recorded drill's
    /// *current* state is returned unchanged (no second row, no restart).
    Replayed(MigrationView),
}

/// Request to durably record one drill step's completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordMigrationStepRequest {
    /// The drill being advanced.
    pub idempotency_key: IdempotencyKey,
    /// The step being recorded; must be exactly `1 + completed`.
    pub step_index: u64,
    pub completed_at_ms: u64,
}

/// Outcome of one [`ApplicationAuthority::record_migration_step`] call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordMigrationStepDecision {
    /// The step's completion committed (the first step also flips the
    /// drill from `Pending` to `Running`).
    Recorded(MigrationStepRecord),
    /// Durable replay: this step is already recorded byte-equal.
    Replayed(MigrationStepRecord),
}

/// The honest post-migration verification hook: a caller-supplied probe
/// consulted exactly once per drill, after every step is complete and
/// after the authority's own binding re-verification, with the writer
/// transaction open (the [`crate::ActiveTaskActivityProbe`] precedent).
/// The first verdict is durable and terminal — replay never re-consults
/// the probe, so probes may be as heavyweight as they need to be
/// (config-shape validation, manifest re-verification, smoke launches).
pub trait MigrationHealthProbe {
    /// `true` = the migrated target revision is healthy and may be
    /// activated; `false` = the drill must take the rollback path.
    fn target_revision_healthy(&self, context: &MigrationHealthContext<'_>) -> bool;
}

/// Everything the authority can honestly show a probe about the drill.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationHealthContext<'a> {
    pub application_id: ApplicationId,
    pub package_id: PackageId,
    pub from_generation: Generation,
    pub from_manifest_digest: ContentDigest,
    pub from_package_version: u64,
    /// The artifact-authority verified receipt of the target revision
    /// (re-read at health-check time; the recorded binding was just
    /// re-verified against it).
    pub target: &'a PackageVerificationReceipt,
    pub completed_step_count: u64,
    pub declared_step_count: u64,
}

/// Outcome of one [`ApplicationAuthority::run_migration_health_check`]
/// call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MigrationHealthDecision {
    /// The probe's verdict committed durably (terminal from here on).
    Recorded(MigrationHealthReport),
    /// Durable replay: a verdict already exists; it is returned
    /// unchanged and the probe was not consulted.
    Replayed(MigrationHealthReport),
}

/// Request to activate one health-passed drill — the atomic switch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActivatePackageMigrationRequest {
    /// The drill to activate.
    pub idempotency_key: IdempotencyKey,
    pub activated_at_ms: u64,
}

/// Outcome of one [`ApplicationAuthority::activate_package_migration`]
/// call; the committed fact is an immutable [`InstallationReceipt`] at
/// `from_generation + 1` (the same carrier install/update use).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActivateMigrationDecision {
    /// First activation: generation CAS, receipt insert, and the
    /// migration row's `done` transition committed in one transaction.
    Activated(InstallationReceipt),
    /// Durable replay: the drill is already done; the recorded
    /// installation receipt is returned unchanged.
    Replayed(InstallationReceipt),
}

/// Request to take the PKG rollback path for one health-failed drill.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RollbackPackageMigrationRequest {
    /// The health-failed drill to terminate.
    pub idempotency_key: IdempotencyKey,
    pub rolled_back_at_ms: u64,
}

/// Outcome of one [`ApplicationAuthority::rollback_package_migration`]
/// call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RollbackMigrationDecision {
    /// First rollback: the migration row's `failed` transition and the
    /// immutable PKG rollback receipt committed in one transaction; the
    /// application row is untouched (the prior revision stayed active).
    RolledBack(MigrationRollbackReceipt),
    /// Durable replay: this drill already rolled back; the recorded
    /// receipt is returned unchanged.
    Replayed(MigrationRollbackReceipt),
}

impl ApplicationAuthority {
    /// Begins one staged same-major migration drill: reads the artifact
    /// authority's verified-package receipt by id (FINALIZED gate), then
    /// in one `Immediate` transaction validates the update-channel
    /// preconditions and commits the durable drill row in the `Pending`
    /// state.
    ///
    /// Fail-closed order (mirroring `update_application`):
    ///
    /// 1. **Replay** inside the transaction: the durable drill row under
    ///    this key is the authority; the same key with a different
    ///    request shape is a typed
    ///    [`ApplicationAuthorityError::IdempotencyConflict`].
    /// 2. **Preconditions**: temporal binding
    ///    (`requested_at_ms >= verified_at_ms`), package identity match,
    ///    application exists / `installed`, target manifest changed, the
    ///    declared compatibility window against the current
    ///    installation's version, and no other live drill for this
    ///    application.
    /// 3. **Commit**: one row, frozen baseline (`from_*` copied from the
    ///    current installation receipt) and frozen target (`target_*`
    ///    copied from the verified receipt); zero steps, `Unchecked`
    ///    health. A refusal leaves zero durable state.
    ///
    /// # Errors
    ///
    /// Fails closed (zero durable state) for an unknown verification
    /// receipt, an idempotency conflict, any update-channel precondition,
    /// a declared window violation, a zero-step drill, or a second live
    /// drill for the application.
    pub fn migrate_application(
        &self,
        artifacts: &ArtifactStore,
        request: MigrateApplicationRequest,
    ) -> Result<MigrateDecision, ApplicationAuthorityError> {
        let verified =
            readback_verified_receipt_by_id(artifacts, request.package_verification_receipt_id)?;

        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(existing) = load_migration_view_by_key(&transaction, request.idempotency_key)? {
            if existing.application_id != crate::derive_application_id(request.package_id)
                || existing.package_verification_receipt_id
                    != request.package_verification_receipt_id
                || existing.compatibility_window != request.compatibility_window
                || existing.declared_step_count != request.declared_step_count
                || existing.created_at_ms != request.requested_at_ms
            {
                return Err(ApplicationAuthorityError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(MigrateDecision::Replayed(existing));
        }

        if request.requested_at_ms < verified.verified_at_ms {
            return Err(ApplicationAuthorityError::MigrationPrecedesVerification {
                verified_at_ms: verified.verified_at_ms,
                requested_at_ms: request.requested_at_ms,
            });
        }

        if verified.package_id != request.package_id {
            return Err(ApplicationAuthorityError::PackageIdentityMismatch {
                expected: request.package_id,
                actual: verified.package_id,
            });
        }

        if request.declared_step_count == 0 {
            return Err(ApplicationAuthorityError::MigrationStepCountZero);
        }

        let application = load_application_by_package(&transaction, request.package_id)?.ok_or(
            ApplicationAuthorityError::ApplicationNotFound {
                package_id: request.package_id,
            },
        )?;
        match application.status {
            ApplicationStatus::Uninstalled => {
                return Err(ApplicationAuthorityError::ApplicationUninstalled {
                    application_id: application.application_id,
                });
            }
            ApplicationStatus::Disabled => {
                return Err(ApplicationAuthorityError::ApplicationDisabled {
                    application_id: application.application_id,
                });
            }
            ApplicationStatus::Installed => {}
        }

        if verified.manifest_digest == application.package_manifest_digest {
            return Err(ApplicationAuthorityError::UpdateManifestUnchanged {
                package_id: request.package_id,
                manifest_digest: verified.manifest_digest,
            });
        }

        let current = load_installation_receipt_at_generation(
            &transaction,
            application.application_id,
            application.current_installation_generation,
        )?
        .ok_or(ApplicationAuthorityError::CorruptRecord(
            "current installation receipt is missing",
        ))?;
        request.compatibility_window.validate(
            request.package_id,
            current.package_version,
            verified.package_version,
        )?;

        if let Some(live) =
            load_live_migration_for_application(&transaction, application.application_id)?
        {
            return Err(ApplicationAuthorityError::MigrationAlreadyLive {
                application_id: application.application_id,
                live_idempotency_key: live.idempotency_key,
            });
        }

        insert_migration_row(&transaction, &request, &current, &verified)?;
        let view = load_migration_view_by_key(&transaction, request.idempotency_key)?.ok_or(
            ApplicationAuthorityError::CorruptRecord("committed migration row is missing"),
        )?;
        transaction.commit()?;
        Ok(MigrateDecision::Started(view))
    }

    /// Durably records one drill step's completion. Steps are strictly
    /// ordered — only `1 + completed` is acceptable — and immutable;
    /// replaying a recorded step (byte-equal timestamp) is a durable
    /// no-op, a different timestamp under the same index is a typed
    /// [`ApplicationAuthorityError::IdempotencyConflict`]. The first
    /// recorded step flips the drill `Pending → Running` in the same
    /// transaction.
    ///
    /// # Errors
    ///
    /// Fails closed (zero durable state) for an unknown drill, a
    /// terminal drill, an out-of-order or beyond-declared index, a
    /// timestamp preceding the drill's last update, or a conflict.
    pub fn record_migration_step(
        &self,
        request: RecordMigrationStepRequest,
    ) -> Result<RecordMigrationStepDecision, ApplicationAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(existing) =
            load_migration_step(&transaction, request.idempotency_key, request.step_index)?
        {
            if existing.completed_at_ms != request.completed_at_ms {
                return Err(ApplicationAuthorityError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(RecordMigrationStepDecision::Replayed(existing));
        }

        let migration = load_migration_row(&transaction, request.idempotency_key)?.ok_or(
            ApplicationAuthorityError::MigrationNotFound {
                idempotency_key: request.idempotency_key,
            },
        )?;
        if migration.state.is_terminal() {
            return Err(ApplicationAuthorityError::MigrationTerminal {
                application_id: migration.application_id,
                state: migration.state,
            });
        }

        if migration.completed_step_count == migration.declared_step_count {
            return Err(ApplicationAuthorityError::MigrationDrillComplete {
                idempotency_key: request.idempotency_key,
                declared_step_count: migration.declared_step_count,
            });
        }
        let expected = migration.completed_step_count + 1;
        if request.step_index != expected {
            return Err(ApplicationAuthorityError::MigrationStepOutOfOrder {
                requested_step_index: request.step_index,
                expected_step_index: expected,
                declared_step_count: migration.declared_step_count,
            });
        }

        if request.completed_at_ms < migration.updated_at_ms {
            return Err(ApplicationAuthorityError::MigrationStepPrecedesLastUpdate {
                last_updated_at_ms: migration.updated_at_ms,
                completed_at_ms: request.completed_at_ms,
            });
        }

        let next_state = if migration.state == MigrationState::Pending {
            MigrationState::Running
        } else {
            migration.state
        };
        transaction.execute(
            "UPDATE application_migrations
             SET state = ?1, updated_at_ms = ?2
             WHERE idempotency_key = ?3 AND state IN (1, 2)",
            params![
                next_state.encode(),
                encode_u64(request.completed_at_ms)?,
                request.idempotency_key.as_bytes().as_slice(),
            ],
        )?;
        transaction.execute(
            "INSERT INTO application_migration_steps (
                idempotency_key, step_index, completed_at_ms
             ) VALUES (?1, ?2, ?3)",
            params![
                request.idempotency_key.as_bytes().as_slice(),
                encode_u64(request.step_index)?,
                encode_u64(request.completed_at_ms)?,
            ],
        )?;
        transaction.commit()?;

        Ok(RecordMigrationStepDecision::Recorded(MigrationStepRecord {
            idempotency_key: request.idempotency_key,
            step_index: request.step_index,
            completed_at_ms: request.completed_at_ms,
        }))
    }

    /// Runs the drill's post-migration health check and records the
    /// typed verdict durably. Preconditions: the drill is `Running` and
    /// every declared step is complete. The authority first re-reads the
    /// target's verified receipt and re-verifies the recorded target
    /// binding (the honest manifest-verify re-run available from durable
    /// state; drift is fail-closed corruption), then consults the probe
    /// exactly once — with the writer transaction open, like the
    /// activity-gate precedents. The first verdict is terminal: replay
    /// returns it without re-consulting the probe.
    ///
    /// # Errors
    ///
    /// Fails closed (zero durable state) for an unknown drill, a
    /// terminal or pending drill, an incomplete drill, a timestamp
    /// preceding the drill's last update, a binding drift (corrupt
    /// durable state), or a storage failure.
    pub fn run_migration_health_check(
        &self,
        artifacts: &ArtifactStore,
        idempotency_key: IdempotencyKey,
        probe: &impl MigrationHealthProbe,
        checked_at_ms: u64,
    ) -> Result<MigrationHealthDecision, ApplicationAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let migration = load_migration_row(&transaction, idempotency_key)?
            .ok_or(ApplicationAuthorityError::MigrationNotFound { idempotency_key })?;
        if migration.state.is_terminal() {
            return Err(ApplicationAuthorityError::MigrationTerminal {
                application_id: migration.application_id,
                state: migration.state,
            });
        }
        if migration.health != MigrationHealthState::Unchecked {
            let report = MigrationHealthReport {
                idempotency_key,
                passed: migration.health == MigrationHealthState::Passed,
                checked_at_ms: migration.health_checked_at_ms.ok_or(
                    ApplicationAuthorityError::CorruptRecord(
                        "recorded migration verdict is missing its timestamp",
                    ),
                )?,
            };
            transaction.commit()?;
            return Ok(MigrationHealthDecision::Replayed(report));
        }
        if migration.state == MigrationState::Pending {
            return Err(ApplicationAuthorityError::MigrationNotRunning {
                application_id: migration.application_id,
                state: migration.state,
            });
        }
        if migration.completed_step_count != migration.declared_step_count {
            return Err(ApplicationAuthorityError::MigrationStepsIncomplete {
                completed_step_count: migration.completed_step_count,
                declared_step_count: migration.declared_step_count,
            });
        }
        if checked_at_ms < migration.updated_at_ms {
            return Err(ApplicationAuthorityError::HealthCheckPrecedesLastUpdate {
                last_updated_at_ms: migration.updated_at_ms,
                checked_at_ms,
            });
        }

        // The honest built-in re-verification: the FINALIZED gate re-read
        // plus the frozen target binding, before the probe is consulted.
        let verified =
            readback_verified_receipt_by_id(artifacts, migration.package_verification_receipt_id)?;
        if verified.receipt_id != migration.package_verification_receipt_id
            || verified.manifest_digest != migration.target_manifest_digest
            || verified.package_version != migration.target_package_version
            || verified.package_id != migration.package_id
        {
            return Err(ApplicationAuthorityError::CorruptRecord(
                "migration target binding mismatch at health check",
            ));
        }

        let context = MigrationHealthContext {
            application_id: migration.application_id,
            package_id: migration.package_id,
            from_generation: migration.from_generation,
            from_manifest_digest: migration.from_manifest_digest,
            from_package_version: migration.from_package_version,
            target: &verified,
            completed_step_count: migration.completed_step_count,
            declared_step_count: migration.declared_step_count,
        };
        let passed = probe.target_revision_healthy(&context);
        let health = if passed {
            MigrationHealthState::Passed
        } else {
            MigrationHealthState::Failed
        };

        let changed = transaction.execute(
            "UPDATE application_migrations
             SET health_state = ?1, health_checked_at_ms = ?2, updated_at_ms = ?2
             WHERE idempotency_key = ?3 AND state = 2 AND health_state = 0",
            params![
                health.encode(),
                encode_u64(checked_at_ms)?,
                idempotency_key.as_bytes().as_slice(),
            ],
        )?;
        if changed != 1 {
            return Err(ApplicationAuthorityError::CorruptRecord(
                "migration health verdict CAS lost",
            ));
        }
        transaction.commit()?;

        Ok(MigrationHealthDecision::Recorded(MigrationHealthReport {
            idempotency_key,
            passed,
            checked_at_ms,
        }))
    }

    /// Activates one health-passed drill — the atomic switch. Requires
    /// the drill `Running`, every step complete, a `Passed` verdict, and
    /// the application still `installed` at the frozen baseline. The
    /// switch is one `Immediate` transaction: the application-row CAS
    /// (`from_generation → from_generation + 1`, manifest digest →
    /// target), the immutable installation receipt at the new
    /// generation (seven-equation digest binding against the re-read
    /// verified receipt), and the migration row's `Done` transition with
    /// the activated installation reference. A crash before the commit
    /// leaves the application wholly on the prior revision and the drill
    /// resumable; a replay after the commit returns the recorded
    /// installation receipt unchanged.
    ///
    /// # Errors
    ///
    /// Fails closed (the application row is never touched) for an
    /// unknown drill, a terminal/pending drill, an unchecked or failed
    /// verdict, an incomplete drill, a timestamp preceding the health
    /// check, a moved baseline (the direct update channel advanced the
    /// generation), a non-installed application, or a lost CAS.
    #[allow(clippy::too_many_lines)]
    pub fn activate_package_migration(
        &self,
        artifacts: &ArtifactStore,
        request: ActivatePackageMigrationRequest,
    ) -> Result<ActivateMigrationDecision, ApplicationAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let migration = load_migration_row(&transaction, request.idempotency_key)?.ok_or(
            ApplicationAuthorityError::MigrationNotFound {
                idempotency_key: request.idempotency_key,
            },
        )?;
        if migration.state == MigrationState::Done {
            let installation_id = migration.activated_installation_id.ok_or(
                ApplicationAuthorityError::CorruptRecord(
                    "done migration is missing its activated installation",
                ),
            )?;
            let receipt = crate::load_receipt_optional(&transaction, installation_id)?.ok_or(
                ApplicationAuthorityError::CorruptRecord(
                    "activated installation receipt is missing",
                ),
            )?;
            transaction.commit()?;
            return Ok(ActivateMigrationDecision::Replayed(receipt));
        }
        if migration.state.is_terminal() {
            return Err(ApplicationAuthorityError::MigrationTerminal {
                application_id: migration.application_id,
                state: migration.state,
            });
        }
        if migration.state == MigrationState::Pending {
            return Err(ApplicationAuthorityError::MigrationNotRunning {
                application_id: migration.application_id,
                state: migration.state,
            });
        }
        match migration.health {
            MigrationHealthState::Unchecked => {
                return Err(ApplicationAuthorityError::MigrationHealthUnchecked {
                    idempotency_key: request.idempotency_key,
                });
            }
            MigrationHealthState::Failed => {
                return Err(ApplicationAuthorityError::MigrationHealthFailed {
                    idempotency_key: request.idempotency_key,
                });
            }
            MigrationHealthState::Passed => {}
        }
        if migration.completed_step_count != migration.declared_step_count {
            return Err(ApplicationAuthorityError::MigrationStepsIncomplete {
                completed_step_count: migration.completed_step_count,
                declared_step_count: migration.declared_step_count,
            });
        }
        let health_checked_at_ms =
            migration
                .health_checked_at_ms
                .ok_or(ApplicationAuthorityError::CorruptRecord(
                    "recorded migration verdict is missing its timestamp",
                ))?;
        if request.activated_at_ms < health_checked_at_ms {
            return Err(ApplicationAuthorityError::ActivationPrecedesHealthCheck {
                health_checked_at_ms,
                activated_at_ms: request.activated_at_ms,
            });
        }

        let verified =
            readback_verified_receipt_by_id(artifacts, migration.package_verification_receipt_id)?;
        if verified.receipt_id != migration.package_verification_receipt_id
            || verified.manifest_digest != migration.target_manifest_digest
            || verified.package_version != migration.target_package_version
            || verified.package_id != migration.package_id
        {
            return Err(ApplicationAuthorityError::CorruptRecord(
                "migration target binding mismatch at activation",
            ));
        }

        let application = load_application_by_package(&transaction, migration.package_id)?.ok_or(
            ApplicationAuthorityError::ApplicationNotFound {
                package_id: migration.package_id,
            },
        )?;
        match application.status {
            ApplicationStatus::Uninstalled => {
                return Err(ApplicationAuthorityError::ApplicationUninstalled {
                    application_id: application.application_id,
                });
            }
            ApplicationStatus::Disabled => {
                return Err(ApplicationAuthorityError::ApplicationDisabled {
                    application_id: application.application_id,
                });
            }
            ApplicationStatus::Installed => {}
        }
        if application.current_installation_generation != migration.from_generation
            || application.package_manifest_digest != migration.from_manifest_digest
        {
            return Err(ApplicationAuthorityError::MigrationBaselineMoved {
                application_id: application.application_id,
                from_generation: migration.from_generation,
                current_generation: application.current_installation_generation,
            });
        }

        let next = migration.from_generation.checked_next().ok_or(
            ApplicationAuthorityError::CorruptRecord("installation generation space is exhausted"),
        )?;
        let receipt = InstallationReceipt {
            installation_id: derive_installation_id(
                request.idempotency_key,
                application.application_id,
                next,
            ),
            application_id: application.application_id,
            installation_generation: next,
            package_id: verified.package_id,
            package_manifest_digest: verified.manifest_digest,
            package_version: verified.package_version,
            entry_count: verified.entry_count,
            package_verification_receipt_id: verified.receipt_id,
            installer_principal: verified.signer,
            idempotency_key: request.idempotency_key,
            installed_at_ms: request.activated_at_ms,
        };
        if let Some(error) = binding_error(&receipt, &verified) {
            return Err(error);
        }

        let changed = transaction.execute(
            "UPDATE applications
             SET current_installation_generation = ?1,
                 package_manifest_digest = ?2,
                 updated_at_ms = ?3
             WHERE application_id = ?4
               AND current_installation_generation = ?5
               AND package_manifest_digest = ?6
               AND status = ?7",
            params![
                crate::encode_generation(next)?,
                verified.manifest_digest.as_bytes().as_slice(),
                encode_u64(request.activated_at_ms)?,
                application.application_id.as_bytes().as_slice(),
                crate::encode_generation(migration.from_generation)?,
                migration.from_manifest_digest.as_bytes().as_slice(),
                ApplicationStatus::Installed.encode(),
            ],
        )?;
        if changed != 1 {
            return Err(ApplicationAuthorityError::MigrationBaselineMoved {
                application_id: application.application_id,
                from_generation: migration.from_generation,
                current_generation: application.current_installation_generation,
            });
        }

        insert_receipt(&transaction, &receipt)?;
        let changed = transaction.execute(
            "UPDATE application_migrations
             SET state = ?1, activated_installation_id = ?2, updated_at_ms = ?3
             WHERE idempotency_key = ?4 AND state = 2",
            params![
                MigrationState::Done.encode(),
                receipt.installation_id.as_bytes().as_slice(),
                encode_u64(request.activated_at_ms)?,
                request.idempotency_key.as_bytes().as_slice(),
            ],
        )?;
        if changed != 1 {
            return Err(ApplicationAuthorityError::CorruptRecord(
                "migration activation state CAS lost",
            ));
        }
        transaction.commit()?;
        Ok(ActivateMigrationDecision::Activated(receipt))
    }

    /// Takes the PKG rollback path for one health-failed drill: in one
    /// `Immediate` transaction flips the drill to `Failed` and commits
    /// the immutable migration-rollback receipt. The application row is
    /// deliberately never touched — a failed drill never activated
    /// anything, so the prior revision *is* still the active one; the
    /// receipt records that retained baseline (and the abandoned
    /// target) durably and replayably. This is the PKG-level rollback,
    /// distinct from the lifecycle [`ApplicationAuthority::
    /// rollback_application`] content forward roll.
    ///
    /// # Errors
    ///
    /// Fails closed (zero durable state) for an unknown drill, a
    /// terminal/pending drill, any verdict other than `Failed`, a
    /// timestamp preceding the failed verdict, a moved or non-installed
    /// baseline, or a lost CAS.
    pub fn rollback_package_migration(
        &self,
        request: RollbackPackageMigrationRequest,
    ) -> Result<RollbackMigrationDecision, ApplicationAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(existing) =
            load_migration_rollback_receipt_by_key(&transaction, request.idempotency_key)?
        {
            if existing.rolled_back_at_ms != request.rolled_back_at_ms {
                return Err(ApplicationAuthorityError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(RollbackMigrationDecision::Replayed(existing));
        }

        let migration = load_migration_row(&transaction, request.idempotency_key)?.ok_or(
            ApplicationAuthorityError::MigrationNotFound {
                idempotency_key: request.idempotency_key,
            },
        )?;
        if migration.state.is_terminal() {
            return Err(ApplicationAuthorityError::MigrationTerminal {
                application_id: migration.application_id,
                state: migration.state,
            });
        }
        if migration.state == MigrationState::Pending {
            return Err(ApplicationAuthorityError::MigrationNotRunning {
                application_id: migration.application_id,
                state: migration.state,
            });
        }
        if migration.health != MigrationHealthState::Failed {
            return Err(ApplicationAuthorityError::MigrationRequiresFailedHealth {
                health: migration.health,
            });
        }
        let health_checked_at_ms =
            migration
                .health_checked_at_ms
                .ok_or(ApplicationAuthorityError::CorruptRecord(
                    "recorded migration verdict is missing its timestamp",
                ))?;
        if request.rolled_back_at_ms < health_checked_at_ms {
            return Err(
                ApplicationAuthorityError::MigrationRollbackPrecedesHealthCheck {
                    health_checked_at_ms,
                    rolled_back_at_ms: request.rolled_back_at_ms,
                },
            );
        }

        let application = load_application_by_package(&transaction, migration.package_id)?.ok_or(
            ApplicationAuthorityError::ApplicationNotFound {
                package_id: migration.package_id,
            },
        )?;
        match application.status {
            ApplicationStatus::Uninstalled => {
                return Err(ApplicationAuthorityError::ApplicationUninstalled {
                    application_id: application.application_id,
                });
            }
            ApplicationStatus::Disabled => {
                return Err(ApplicationAuthorityError::ApplicationDisabled {
                    application_id: application.application_id,
                });
            }
            ApplicationStatus::Installed => {}
        }
        if application.current_installation_generation != migration.from_generation
            || application.package_manifest_digest != migration.from_manifest_digest
        {
            return Err(ApplicationAuthorityError::MigrationBaselineMoved {
                application_id: application.application_id,
                from_generation: migration.from_generation,
                current_generation: application.current_installation_generation,
            });
        }

        let receipt = MigrationRollbackReceipt {
            idempotency_key: request.idempotency_key,
            application_id: migration.application_id,
            abandoned_target_manifest_digest: migration.target_manifest_digest,
            retained_generation: migration.from_generation,
            retained_manifest_digest: migration.from_manifest_digest,
            rolled_back_at_ms: request.rolled_back_at_ms,
        };
        let changed = transaction.execute(
            "UPDATE application_migrations
             SET state = ?1, updated_at_ms = ?2
             WHERE idempotency_key = ?3 AND state = 2",
            params![
                MigrationState::Failed.encode(),
                encode_u64(request.rolled_back_at_ms)?,
                request.idempotency_key.as_bytes().as_slice(),
            ],
        )?;
        if changed != 1 {
            return Err(ApplicationAuthorityError::CorruptRecord(
                "migration rollback state CAS lost",
            ));
        }
        insert_migration_rollback_receipt(&transaction, &receipt)?;
        transaction.commit()?;
        Ok(RollbackMigrationDecision::RolledBack(receipt))
    }

    /// Reads one durable migration by drill key. `None` means no drill
    /// was ever recorded under this key.
    ///
    /// # Errors
    ///
    /// Fails closed on a storage error.
    pub fn inspect_package_migration(
        &self,
        idempotency_key: IdempotencyKey,
    ) -> Result<Option<MigrationView>, ApplicationAuthorityError> {
        let connection = self.lock()?;
        load_migration_view_by_key(&connection, idempotency_key)
    }

    /// Lists every durable migration of one application, oldest first.
    /// An unknown application lists as empty.
    ///
    /// # Errors
    ///
    /// Fails closed on a storage error.
    pub fn list_package_migrations(
        &self,
        package_id: PackageId,
    ) -> Result<Vec<MigrationView>, ApplicationAuthorityError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT idempotency_key FROM application_migrations
             WHERE application_id = ?1
             ORDER BY created_at_ms ASC, idempotency_key ASC",
        )?;
        let mut rows = statement.query([crate::derive_application_id(package_id)
            .as_bytes()
            .as_slice()])?;
        let mut keys = Vec::new();
        while let Some(row) = rows.next()? {
            keys.push(IdempotencyKey::from_bytes(crate::blob16(row, 0)?));
        }
        drop(rows);
        drop(statement);
        keys.into_iter()
            .map(|key| {
                load_migration_view_by_key(&connection, key)?.ok_or(
                    ApplicationAuthorityError::CorruptRecord("listed migration row is missing"),
                )
            })
            .collect()
    }

    /// Lists one drill's recorded step completions in order.
    ///
    /// # Errors
    ///
    /// Fails closed on a storage error.
    pub fn list_migration_steps(
        &self,
        idempotency_key: IdempotencyKey,
    ) -> Result<Vec<MigrationStepRecord>, ApplicationAuthorityError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT idempotency_key, step_index, completed_at_ms
             FROM application_migration_steps
             WHERE idempotency_key = ?1
             ORDER BY step_index ASC",
        )?;
        let mut rows = statement.query([idempotency_key.as_bytes().as_slice()])?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            records.push(MigrationStepRecord {
                idempotency_key: IdempotencyKey::from_bytes(crate::blob16(row, 0)?),
                step_index: decode_u64(row, 1)?,
                completed_at_ms: decode_u64(row, 2)?,
            });
        }
        Ok(records)
    }

    /// Reads one drill's immutable PKG rollback receipt. `None` means
    /// this drill never took the rollback path.
    ///
    /// # Errors
    ///
    /// Fails closed on a storage error.
    pub fn inspect_migration_rollback_receipt(
        &self,
        idempotency_key: IdempotencyKey,
    ) -> Result<Option<MigrationRollbackReceipt>, ApplicationAuthorityError> {
        let connection = self.lock()?;
        load_migration_rollback_receipt_by_key(&connection, idempotency_key)
    }
}

/// The raw migration row (progress counts resolved).
struct MigrationRow {
    application_id: ApplicationId,
    package_id: PackageId,
    from_generation: Generation,
    from_manifest_digest: ContentDigest,
    from_package_version: u64,
    package_verification_receipt_id: ReceiptId,
    target_manifest_digest: ContentDigest,
    target_package_version: u64,
    declared_step_count: u64,
    completed_step_count: u64,
    state: MigrationState,
    health: MigrationHealthState,
    health_checked_at_ms: Option<u64>,
    activated_installation_id: Option<InstallationId>,
    updated_at_ms: u64,
}

fn load_migration_row(
    source: &Connection,
    key: IdempotencyKey,
) -> Result<Option<MigrationRow>, ApplicationAuthorityError> {
    let mut statement = source.prepare(
        "SELECT application_id, package_id, from_generation,
                from_manifest_digest, from_package_version,
                package_verification_receipt_id, target_manifest_digest,
                target_package_version, step_count, state, health_state,
                health_checked_at_ms, activated_installation_id, updated_at_ms,
                (SELECT COUNT(*) FROM application_migration_steps
                 WHERE idempotency_key = application_migrations.idempotency_key)
         FROM application_migrations WHERE idempotency_key = ?1",
    )?;
    let mut rows = statement.query([key.as_bytes().as_slice()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(MigrationRow {
        application_id: ApplicationId::from_bytes(crate::blob16(row, 0)?),
        package_id: PackageId::from_bytes(crate::blob16(row, 1)?),
        from_generation: crate::decode_generation(row, 2)?,
        from_manifest_digest: ContentDigest::from_bytes(crate::blob32(row, 3)?),
        from_package_version: decode_u64(row, 4)?,
        package_verification_receipt_id: ReceiptId::from_bytes(crate::blob16(row, 5)?),
        target_manifest_digest: ContentDigest::from_bytes(crate::blob32(row, 6)?),
        target_package_version: decode_u64(row, 7)?,
        declared_step_count: decode_u64(row, 8)?,
        state: MigrationState::decode(row.get(9)?)?,
        health: MigrationHealthState::decode(row.get(10)?)?,
        health_checked_at_ms: decode_optional_u64(row, 11)?,
        activated_installation_id: decode_optional_installation_id(row, 12)?,
        updated_at_ms: decode_u64(row, 13)?,
        completed_step_count: decode_u64(row, 14)?,
    }))
}

fn load_migration_view_by_key(
    source: &Connection,
    key: IdempotencyKey,
) -> Result<Option<MigrationView>, ApplicationAuthorityError> {
    let mut statement = source.prepare(
        "SELECT application_id, package_id, from_generation,
                from_manifest_digest, from_package_version,
                package_verification_receipt_id, target_manifest_digest,
                target_package_version, compatibility_window, step_count,
                state, health_state, health_checked_at_ms,
                activated_installation_id, created_at_ms, updated_at_ms,
                (SELECT COUNT(*) FROM application_migration_steps
                 WHERE idempotency_key = application_migrations.idempotency_key)
         FROM application_migrations WHERE idempotency_key = ?1",
    )?;
    let mut rows = statement.query([key.as_bytes().as_slice()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(MigrationView {
        idempotency_key: key,
        application_id: ApplicationId::from_bytes(crate::blob16(row, 0)?),
        package_id: PackageId::from_bytes(crate::blob16(row, 1)?),
        from_generation: crate::decode_generation(row, 2)?,
        from_manifest_digest: ContentDigest::from_bytes(crate::blob32(row, 3)?),
        from_package_version: decode_u64(row, 4)?,
        package_verification_receipt_id: ReceiptId::from_bytes(crate::blob16(row, 5)?),
        target_manifest_digest: ContentDigest::from_bytes(crate::blob32(row, 6)?),
        target_package_version: decode_u64(row, 7)?,
        compatibility_window: CompatibilityWindow::decode(row.get(8)?)?,
        declared_step_count: decode_u64(row, 9)?,
        state: MigrationState::decode(row.get(10)?)?,
        health: MigrationHealthState::decode(row.get(11)?)?,
        health_checked_at_ms: decode_optional_u64(row, 12)?,
        activated_installation_id: decode_optional_installation_id(row, 13)?,
        created_at_ms: decode_u64(row, 14)?,
        updated_at_ms: decode_u64(row, 15)?,
        completed_step_count: decode_u64(row, 16)?,
    }))
}

fn load_live_migration_for_application(
    source: &Connection,
    application_id: ApplicationId,
) -> Result<Option<MigrationView>, ApplicationAuthorityError> {
    let mut statement = source.prepare(
        "SELECT idempotency_key FROM application_migrations
         WHERE application_id = ?1 AND state IN (1, 2)
         ORDER BY created_at_ms ASC LIMIT 1",
    )?;
    let mut rows = statement.query([application_id.as_bytes().as_slice()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let key = IdempotencyKey::from_bytes(crate::blob16(row, 0)?);
    load_migration_view_by_key(source, key)?.map(Some).ok_or(
        ApplicationAuthorityError::CorruptRecord("live migration row is missing"),
    )
}

fn load_migration_step(
    source: &Connection,
    key: IdempotencyKey,
    step_index: u64,
) -> Result<Option<MigrationStepRecord>, ApplicationAuthorityError> {
    let mut statement = source.prepare(
        "SELECT idempotency_key, step_index, completed_at_ms
         FROM application_migration_steps
         WHERE idempotency_key = ?1 AND step_index = ?2",
    )?;
    let mut rows = statement.query(params![key.as_bytes().as_slice(), encode_u64(step_index)?,])?;
    rows.next()?
        .map(|row| {
            Ok(MigrationStepRecord {
                idempotency_key: IdempotencyKey::from_bytes(crate::blob16(row, 0)?),
                step_index: decode_u64(row, 1)?,
                completed_at_ms: decode_u64(row, 2)?,
            })
        })
        .transpose()
}

fn insert_migration_row(
    transaction: &Connection,
    request: &MigrateApplicationRequest,
    current: &InstallationReceipt,
    verified: &PackageVerificationReceipt,
) -> Result<(), ApplicationAuthorityError> {
    transaction.execute(
        "INSERT INTO application_migrations (
            idempotency_key, application_id, package_id,
            from_generation, from_manifest_digest, from_package_version,
            package_verification_receipt_id, target_manifest_digest,
            target_package_version, compatibility_window, step_count,
            state, health_state, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1, 0, ?12, ?12)",
        params![
            request.idempotency_key.as_bytes().as_slice(),
            current.application_id.as_bytes().as_slice(),
            request.package_id.as_bytes().as_slice(),
            crate::encode_generation(current.installation_generation)?,
            current.package_manifest_digest.as_bytes().as_slice(),
            encode_u64(current.package_version)?,
            verified.receipt_id.as_bytes().as_slice(),
            verified.manifest_digest.as_bytes().as_slice(),
            encode_u64(verified.package_version)?,
            request.compatibility_window.encode(),
            encode_u64(request.declared_step_count)?,
            encode_u64(request.requested_at_ms)?,
        ],
    )?;
    Ok(())
}

fn insert_migration_rollback_receipt(
    transaction: &Connection,
    receipt: &MigrationRollbackReceipt,
) -> Result<(), ApplicationAuthorityError> {
    transaction.execute(
        "INSERT INTO application_migration_rollback_receipts (
            idempotency_key, application_id,
            abandoned_target_manifest_digest, retained_generation,
            retained_manifest_digest, rolled_back_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            receipt.idempotency_key.as_bytes().as_slice(),
            receipt.application_id.as_bytes().as_slice(),
            receipt
                .abandoned_target_manifest_digest
                .as_bytes()
                .as_slice(),
            crate::encode_generation(receipt.retained_generation)?,
            receipt.retained_manifest_digest.as_bytes().as_slice(),
            encode_u64(receipt.rolled_back_at_ms)?,
        ],
    )?;
    Ok(())
}

fn load_migration_rollback_receipt_by_key(
    source: &Connection,
    key: IdempotencyKey,
) -> Result<Option<MigrationRollbackReceipt>, ApplicationAuthorityError> {
    let mut statement = source.prepare(
        "SELECT idempotency_key, application_id,
                abandoned_target_manifest_digest, retained_generation,
                retained_manifest_digest, rolled_back_at_ms
         FROM application_migration_rollback_receipts
         WHERE idempotency_key = ?1",
    )?;
    let mut rows = statement.query([key.as_bytes().as_slice()])?;
    rows.next()?
        .map(|row| {
            Ok(MigrationRollbackReceipt {
                idempotency_key: IdempotencyKey::from_bytes(crate::blob16(row, 0)?),
                application_id: ApplicationId::from_bytes(crate::blob16(row, 1)?),
                abandoned_target_manifest_digest: ContentDigest::from_bytes(crate::blob32(row, 2)?),
                retained_generation: crate::decode_generation(row, 3)?,
                retained_manifest_digest: ContentDigest::from_bytes(crate::blob32(row, 4)?),
                rolled_back_at_ms: decode_u64(row, 5)?,
            })
        })
        .transpose()
}

fn decode_optional_u64(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<Option<u64>, ApplicationAuthorityError> {
    let value: Option<i64> = row.get(index)?;
    value
        .map(u64::try_from)
        .transpose()
        .map_err(|_| ApplicationAuthorityError::CorruptRecord("negative u64 column"))
}

fn decode_optional_installation_id(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<Option<InstallationId>, ApplicationAuthorityError> {
    let bytes: Option<Vec<u8>> = row.get(index)?;
    bytes
        .map(|bytes| <[u8; 16]>::try_from(bytes.as_slice()).map(InstallationId::from_bytes))
        .transpose()
        .map_err(|_| ApplicationAuthorityError::CorruptRecord("installation blob length mismatch"))
}

#[cfg(test)]
mod tests {
    use super::{CompatibilityWindow, MigrationHealthState, MigrationState};
    use crate::ApplicationAuthorityError;

    #[test]
    fn migration_state_encoding_round_trips_and_rejects_unknowns() {
        assert_eq!(MigrationState::Pending.encode(), 1);
        assert_eq!(MigrationState::Running.encode(), 2);
        assert_eq!(MigrationState::Done.encode(), 3);
        assert_eq!(MigrationState::Failed.encode(), 4);
        for state in [
            MigrationState::Pending,
            MigrationState::Running,
            MigrationState::Done,
            MigrationState::Failed,
        ] {
            assert_eq!(
                MigrationState::decode(state.encode()).expect("known state"),
                state
            );
        }
        assert!(matches!(
            MigrationState::decode(0),
            Err(ApplicationAuthorityError::CorruptRecord(_))
        ));
        assert!(matches!(
            MigrationState::decode(5),
            Err(ApplicationAuthorityError::CorruptRecord(_))
        ));
        assert!(MigrationState::Done.is_terminal());
        assert!(MigrationState::Failed.is_terminal());
        assert!(!MigrationState::Pending.is_terminal());
        assert!(!MigrationState::Running.is_terminal());
    }

    #[test]
    fn migration_health_encoding_round_trips_and_rejects_unknowns() {
        assert_eq!(MigrationHealthState::Unchecked.encode(), 0);
        assert_eq!(MigrationHealthState::Passed.encode(), 1);
        assert_eq!(MigrationHealthState::Failed.encode(), 2);
        for health in [
            MigrationHealthState::Unchecked,
            MigrationHealthState::Passed,
            MigrationHealthState::Failed,
        ] {
            assert_eq!(
                MigrationHealthState::decode(health.encode()).expect("known verdict"),
                health
            );
        }
        assert!(matches!(
            MigrationHealthState::decode(3),
            Err(ApplicationAuthorityError::CorruptRecord(_))
        ));
    }

    #[test]
    fn compatibility_window_storage_encoding_round_trips() {
        assert_eq!(CompatibilityWindow::SameMajor.encode(), 1);
        assert_eq!(CompatibilityWindow::SameMinor.encode(), 2);
        assert_eq!(
            CompatibilityWindow::decode(1).expect("same major"),
            CompatibilityWindow::SameMajor
        );
        assert_eq!(
            CompatibilityWindow::decode(2).expect("same minor"),
            CompatibilityWindow::SameMinor
        );
        assert!(matches!(
            CompatibilityWindow::decode(3),
            Err(ApplicationAuthorityError::CorruptRecord(_))
        ));
    }
}
