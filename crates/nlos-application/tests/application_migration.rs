//! B1-3 (W29-E) `[PKG-UPDATE-001]` migration-runner matrix: same-major
//! staged migration between package revisions with durable drill state
//! (`pending/running/done/failed`), a typed post-migration health-check
//! verdict, the single-transaction atomic switch, and the PKG-level
//! failed-health rollback path (distinct from the lifecycle
//! `rollback_application` gate).
//!
//! Crash model (documented harness constraint, lighter than the kill-9
//! matrix by design): every "crash" point reopens the authority from the
//! same durable root — the process-restart convergence model the update
//! replay tests use. A kill-9 fault-matrix extension over the five
//! migration entries is a deferred follow-up lane.
//!
//! Matrix:
//! - happy path: same-major drill `begin → steps → health(pass) →
//!   activate` lands one installation receipt at generation+1 with the
//!   target digest (the atomic switch), dense generation history;
//! - begin idempotency: same-key replay returns the recorded view, shape
//!   mismatch conflicts, a second live drill is refused (one live
//!   migration per application);
//! - typed refusals with zero durable state (unknown receipt, identity
//!   mismatch, disabled application, unchanged manifest, both
//!   compatibility windows, precedes-verification, zero-step drill);
//! - step gates: strict order, completion, replay and timestamp conflict,
//!   typed temporal bindings;
//! - mid-migration crash convergence: reopening between every command
//!   converges byte-equal onto the no-crash control run, and the health
//!   probe is never re-consulted once its verdict is durable;
//! - health-check failure: the application never leaves the prior
//!   revision, the PKG rollback receipt is durable and replayable, and a
//!   later migration can still succeed;
//! - W28-B interplay: a task-templated package (manifest `tasks`
//!   segment) migrates identically (the runner consumes the same
//!   verified-receipt shape, segment-agnostic);
//! - interplay with the direct update channel: a drill superseded by a
//!   direct `update_application` refuses activation typed
//!   (`MigrationBaselineMoved`) and never moves the application silently;
//!   after a successful migration the direct channel continues densely.

use std::cell::Cell;

use nlos_application::{
    ActivateMigrationDecision, ActivatePackageMigrationRequest, ApplicationAuthority,
    ApplicationAuthorityError, ApplicationStatus, CompatibilityWindow, MigrateApplicationRequest,
    MigrateDecision, MigrationHealthDecision, MigrationHealthProbe, MigrationHealthState,
    MigrationRollbackReceipt, MigrationState, MigrationView, RecordMigrationStepDecision,
    RecordMigrationStepRequest, RollbackApplicationRequest, RollbackMigrationDecision,
    RollbackPackageMigrationRequest, derive_installation_id, pack_package_version,
};
use nlos_types::{Generation, IdempotencyKey, PackageId, ReceiptId};

mod support;

use support::{TestStack, disabled, installed, open_authority, updated};

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn label(name: &str) -> String {
    format!("migration-{name}")
}

fn begin_request(
    package_id: PackageId,
    receipt_id: ReceiptId,
    key_seed: u8,
    steps: u64,
    at_ms: u64,
) -> MigrateApplicationRequest {
    MigrateApplicationRequest {
        package_id,
        package_verification_receipt_id: receipt_id,
        idempotency_key: key(key_seed),
        compatibility_window: CompatibilityWindow::SameMajor,
        declared_step_count: steps,
        requested_at_ms: at_ms,
    }
}

/// A probe whose verdict is fixed, whose consultations are counted (a
/// durable verdict must never re-consult the probe), and which captures
/// the scalar slice of the health context the assertions need.
struct ScriptedProbe {
    verdict: bool,
    calls: Cell<u32>,
    seen_from_generation: Cell<u64>,
    seen_completed_steps: Cell<u64>,
    seen_declared_steps: Cell<u64>,
    seen_package: Cell<Option<PackageId>>,
}

impl ScriptedProbe {
    const fn with_verdict(verdict: bool) -> Self {
        Self {
            verdict,
            calls: Cell::new(0),
            seen_from_generation: Cell::new(0),
            seen_completed_steps: Cell::new(0),
            seen_declared_steps: Cell::new(0),
            seen_package: Cell::new(None),
        }
    }
}

impl MigrationHealthProbe for ScriptedProbe {
    fn target_revision_healthy(
        &self,
        context: &nlos_application::MigrationHealthContext<'_>,
    ) -> bool {
        self.calls.set(self.calls.get() + 1);
        self.seen_from_generation.set(context.from_generation.get());
        self.seen_completed_steps.set(context.completed_step_count);
        self.seen_declared_steps.set(context.declared_step_count);
        self.seen_package.set(Some(context.package_id));
        self.verdict
    }
}

struct Drill {
    stack: TestStack,
    authority: ApplicationAuthority,
    package_id: PackageId,
    target: nlos_artifact::PackageVerificationReceipt,
}

impl Drill {
    /// Reopens the authority from the same durable root (the
    /// process-restart crash model; the artifact store models a separate
    /// surviving authority).
    fn crash_and_recover(&mut self) {
        self.authority = ApplicationAuthority::open(self.stack.root.root())
            .expect("reopen application authority");
    }

    fn begin(&self, key_seed: u8, steps: u64, at_ms: u64) -> MigrationView {
        match self
            .authority
            .migrate_application(
                &self.stack.artifacts,
                begin_request(
                    self.package_id,
                    self.target.receipt_id,
                    key_seed,
                    steps,
                    at_ms,
                ),
            )
            .expect("begin must succeed")
        {
            MigrateDecision::Started(view) => view,
            MigrateDecision::Replayed(view) => {
                panic!("fresh key cannot replay a migration, got {view:?}")
            }
        }
    }

    fn begin_replayed(&self, key_seed: u8, steps: u64, at_ms: u64) -> MigrationView {
        match self
            .authority
            .migrate_application(
                &self.stack.artifacts,
                begin_request(
                    self.package_id,
                    self.target.receipt_id,
                    key_seed,
                    steps,
                    at_ms,
                ),
            )
            .expect("begin replay must succeed")
        {
            MigrateDecision::Replayed(view) => view,
            MigrateDecision::Started(view) => {
                panic!("recorded migration cannot restart, got {view:?}")
            }
        }
    }

    fn step(&self, key_seed: u8, index: u64, at_ms: u64) -> nlos_application::MigrationStepRecord {
        match self
            .authority
            .record_migration_step(RecordMigrationStepRequest {
                idempotency_key: key(key_seed),
                step_index: index,
                completed_at_ms: at_ms,
            })
            .expect("step must succeed")
        {
            RecordMigrationStepDecision::Recorded(record) => record,
            RecordMigrationStepDecision::Replayed(record) => {
                panic!("unrecorded step cannot replay, got {record:?}")
            }
        }
    }

    fn step_replayed(
        &self,
        key_seed: u8,
        index: u64,
        at_ms: u64,
    ) -> nlos_application::MigrationStepRecord {
        match self
            .authority
            .record_migration_step(RecordMigrationStepRequest {
                idempotency_key: key(key_seed),
                step_index: index,
                completed_at_ms: at_ms,
            })
            .expect("step replay must succeed")
        {
            RecordMigrationStepDecision::Replayed(record) => record,
            RecordMigrationStepDecision::Recorded(record) => {
                panic!("recorded step cannot re-record, got {record:?}")
            }
        }
    }

    fn run_all_steps(&self, key_seed: u8, declared: u64, first_ms: u64) {
        for index in 1..=declared {
            self.step(key_seed, index, first_ms + (index - 1) * 100);
        }
    }

    fn health(
        &self,
        key_seed: u8,
        probe: &impl MigrationHealthProbe,
        at_ms: u64,
    ) -> nlos_application::MigrationHealthReport {
        match self
            .authority
            .run_migration_health_check(&self.stack.artifacts, key(key_seed), probe, at_ms)
            .expect("health check must succeed")
        {
            MigrationHealthDecision::Recorded(report) => report,
            MigrationHealthDecision::Replayed(report) => {
                panic!("unchecked migration cannot replay a verdict, got {report:?}")
            }
        }
    }

    fn health_replayed(
        &self,
        key_seed: u8,
        probe: &impl MigrationHealthProbe,
        at_ms: u64,
    ) -> nlos_application::MigrationHealthReport {
        match self
            .authority
            .run_migration_health_check(&self.stack.artifacts, key(key_seed), probe, at_ms)
            .expect("health replay must succeed")
        {
            MigrationHealthDecision::Replayed(report) => report,
            MigrationHealthDecision::Recorded(report) => {
                panic!("a recorded verdict cannot re-record, got {report:?}")
            }
        }
    }

    fn activate(&self, key_seed: u8, at_ms: u64) -> nlos_application::InstallationReceipt {
        match self
            .authority
            .activate_package_migration(
                &self.stack.artifacts,
                ActivatePackageMigrationRequest {
                    idempotency_key: key(key_seed),
                    activated_at_ms: at_ms,
                },
            )
            .expect("activation must succeed")
        {
            ActivateMigrationDecision::Activated(receipt) => receipt,
            ActivateMigrationDecision::Replayed(receipt) => {
                panic!("unactivated migration cannot replay, got {receipt:?}")
            }
        }
    }

    fn activate_replayed(&self, key_seed: u8, at_ms: u64) -> nlos_application::InstallationReceipt {
        match self
            .authority
            .activate_package_migration(
                &self.stack.artifacts,
                ActivatePackageMigrationRequest {
                    idempotency_key: key(key_seed),
                    activated_at_ms: at_ms,
                },
            )
            .expect("activation replay must succeed")
        {
            ActivateMigrationDecision::Replayed(receipt) => receipt,
            ActivateMigrationDecision::Activated(receipt) => {
                panic!("a done migration cannot activate twice, got {receipt:?}")
            }
        }
    }

    fn application(&self) -> nlos_application::ApplicationView {
        self.authority
            .inspect_application(self.package_id)
            .expect("inspect application")
            .expect("installed application")
    }

    fn migration(&self, key_seed: u8) -> MigrationView {
        self.authority
            .inspect_package_migration(key(key_seed))
            .expect("inspect migration")
            .expect("recorded migration")
    }
}

fn rolled_back(
    authority: &ApplicationAuthority,
    key_seed: u8,
    at_ms: u64,
) -> MigrationRollbackReceipt {
    match authority
        .rollback_package_migration(RollbackPackageMigrationRequest {
            idempotency_key: key(key_seed),
            rolled_back_at_ms: at_ms,
        })
        .expect("migration rollback must succeed")
    {
        RollbackMigrationDecision::RolledBack(receipt) => receipt,
        RollbackMigrationDecision::Replayed(receipt) => {
            panic!("unrolled migration cannot replay, got {receipt:?}")
        }
    }
}

fn rollback_replayed(
    authority: &ApplicationAuthority,
    key_seed: u8,
    at_ms: u64,
) -> MigrationRollbackReceipt {
    match authority
        .rollback_package_migration(RollbackPackageMigrationRequest {
            idempotency_key: key(key_seed),
            rolled_back_at_ms: at_ms,
        })
        .expect("migration rollback replay must succeed")
    {
        RollbackMigrationDecision::Replayed(receipt) => receipt,
        RollbackMigrationDecision::RolledBack(receipt) => {
            panic!("a failed migration cannot roll back twice, got {receipt:?}")
        }
    }
}

/// Installs `1.0.0` at generation 1 and verifies a `1.2.0` same-major
/// target, leaving the drill one `migrate_application` away.
fn drill_fixture(name: &str, seed: u8, package_seed: u8) -> Drill {
    let stack = TestStack::new(&label(name), seed);
    let from = stack.verify_package(
        package_seed,
        pack_package_version(1, 0, 0),
        key(0xF0),
        6_000,
    );
    let target = stack.verify_package(
        package_seed,
        pack_package_version(1, 2, 0),
        key(0xF1),
        6_050,
    );
    let authority = open_authority(stack.root.root());
    installed(&authority, &stack.artifacts, from.receipt_id, 0x01, 6_100);
    Drill {
        stack,
        authority,
        package_id: from.package_id,
        target,
    }
}

/// 同 major 迁移演练全链：pending→running→(steps)→health pass→原子切换
/// 单事务落 gen+1 installation receipt，代际历史稠密。
#[test]
fn migration_happy_path_same_major_activates_atomically() {
    let drill = drill_fixture("happy", 0x51, 0x41);
    let probe = ScriptedProbe::with_verdict(true);

    let view = drill.begin(0x11, 2, 6_200);
    assert_eq!(view.state, MigrationState::Pending);
    assert_eq!(view.health, MigrationHealthState::Unchecked);
    assert_eq!(view.declared_step_count, 2);
    assert_eq!(view.completed_step_count, 0);
    assert_eq!(view.from_generation, Generation::INITIAL);
    assert_eq!(
        view.from_manifest_digest,
        drill.application().package_manifest_digest
    );
    assert_eq!(view.from_package_version, pack_package_version(1, 0, 0));
    assert_eq!(view.target_manifest_digest, drill.target.manifest_digest);
    assert_eq!(view.target_package_version, drill.target.package_version);
    assert_eq!(view.activated_installation_id, None);

    let step_one = drill.step(0x11, 1, 6_300);
    assert_eq!(step_one.step_index, 1);
    assert_eq!(step_one.completed_at_ms, 6_300);
    let running = drill.migration(0x11);
    assert_eq!(running.state, MigrationState::Running);
    assert_eq!(running.completed_step_count, 1);

    drill.step(0x11, 2, 6_400);

    let report = drill.health(0x11, &probe, 6_500);
    assert!(report.passed);
    assert_eq!(probe.calls.get(), 1, "the probe runs exactly once");
    assert_eq!(probe.seen_from_generation.get(), 1);
    assert_eq!(probe.seen_completed_steps.get(), 2);
    assert_eq!(probe.seen_declared_steps.get(), 2);
    assert_eq!(probe.seen_package.get(), Some(drill.package_id));

    let receipt = drill.activate(0x11, 6_600);
    assert_eq!(receipt.installation_generation.get(), 2);
    assert_eq!(
        receipt.package_manifest_digest,
        drill.target.manifest_digest
    );
    assert_eq!(receipt.package_version, drill.target.package_version);
    assert_eq!(receipt.entry_count, drill.target.entry_count);
    assert_eq!(
        receipt.package_verification_receipt_id,
        drill.target.receipt_id
    );
    assert_eq!(receipt.installer_principal, drill.target.signer);
    assert_eq!(receipt.installed_at_ms, 6_600);
    assert_eq!(
        receipt.installation_id,
        derive_installation_id(
            key(0x11),
            receipt.application_id,
            receipt.installation_generation
        ),
        "the migration key drives the deterministic installation identity"
    );

    let app = drill.application();
    assert_eq!(app.status, ApplicationStatus::Installed);
    assert_eq!(app.current_installation_generation.get(), 2);
    assert_eq!(app.package_manifest_digest, drill.target.manifest_digest);
    assert_eq!(app.updated_at_ms, 6_600);

    let done = drill.migration(0x11);
    assert_eq!(done.state, MigrationState::Done);
    assert_eq!(done.health, MigrationHealthState::Passed);
    assert_eq!(done.completed_step_count, 2);
    assert_eq!(
        done.activated_installation_id,
        Some(receipt.installation_id)
    );

    let installations = drill
        .authority
        .list_installations(receipt.application_id)
        .expect("list installations");
    assert_eq!(installations.len(), 2);
    assert_eq!(installations[1], receipt);
    assert_eq!(
        installations[0].installation_generation,
        Generation::INITIAL,
        "generation history stays dense across install and migration"
    );

    assert_eq!(
        drill
            .authority
            .list_migration_steps(key(0x11))
            .expect("list steps")
            .iter()
            .map(|step| (step.step_index, step.completed_at_ms))
            .collect::<Vec<_>>(),
        vec![(1, 6_300), (2, 6_400)]
    );
}

/// begin 幂等：同 key 同形状 replay 返回当前视图；任何字段变化是
/// `IdempotencyConflict`；同一应用同时只允许一条在途迁移。
#[test]
fn migration_begin_replays_conflicts_and_enforces_one_live_drill() {
    let drill = drill_fixture("begin-replay", 0x52, 0x41);
    let first = drill.begin(0x21, 2, 6_200);
    assert_eq!(drill.begin_replayed(0x21, 2, 6_200), first);

    // A *known* sibling receipt (the FINALIZED gate readback precedes
    // replay, so an unknown receipt id would be refused as
    // `PackageVerificationReceiptNotFound`, never reaching the shape
    // comparison — the install/update channel semantics).
    let sibling = drill
        .stack
        .verify_package(0x41, pack_package_version(1, 4, 0), key(0xED), 6_060);
    for mutated in [
        begin_request(drill.package_id, drill.target.receipt_id, 0x21, 3, 6_200),
        {
            let mut request =
                begin_request(drill.package_id, drill.target.receipt_id, 0x21, 2, 6_200);
            request.compatibility_window = CompatibilityWindow::SameMinor;
            request
        },
        begin_request(drill.package_id, drill.target.receipt_id, 0x21, 2, 6_250),
        begin_request(drill.package_id, sibling.receipt_id, 0x21, 2, 6_200),
    ] {
        assert!(matches!(
            drill
                .authority
                .migrate_application(&drill.stack.artifacts, mutated),
            Err(ApplicationAuthorityError::IdempotencyConflict)
        ));
    }
    assert_eq!(
        drill
            .authority
            .list_package_migrations(drill.package_id)
            .expect("list migrations")
            .len(),
        1,
        "conflicts never write a second drill"
    );

    assert!(matches!(
        drill
            .authority
            .migrate_application(
                &drill.stack.artifacts,
                begin_request(drill.package_id, drill.target.receipt_id, 0x22, 2, 6_200)
            ),
        Err(ApplicationAuthorityError::MigrationAlreadyLive {
            live_idempotency_key,
            ..
        }) if live_idempotency_key == key(0x21)
    ));
}

/// 拒绝全表：每个 typed 拒绝都零 durable 副作用（迁移清单为空）。
#[test]
fn migration_refusals_are_typed_with_zero_durable_state() {
    let drill = drill_fixture("refusals", 0x53, 0x41);
    let authority = &drill.authority;
    let artifacts = &drill.stack.artifacts;
    let request = |receipt_id: ReceiptId, at_ms: u64, steps: u64| {
        begin_request(drill.package_id, receipt_id, 0x31, steps, at_ms)
    };

    assert!(matches!(
        authority.migrate_application(
            artifacts,
            request(ReceiptId::from_bytes([0xEE; 16]), 6_200, 2)
        ),
        Err(ApplicationAuthorityError::PackageVerificationReceiptNotFound(_))
    ));

    let foreign = drill
        .stack
        .verify_package(0x49, pack_package_version(1, 2, 0), key(0xE3), 6_050);
    assert!(matches!(
        authority.migrate_application(artifacts, request(foreign.receipt_id, 6_200, 2)),
        Err(ApplicationAuthorityError::PackageIdentityMismatch { .. })
    ));

    assert!(matches!(
        authority.migrate_application(artifacts, request(drill.target.receipt_id, 6_049, 2)),
        Err(ApplicationAuthorityError::MigrationPrecedesVerification {
            verified_at_ms: 6_050,
            requested_at_ms: 6_049,
        })
    ));

    assert!(matches!(
        authority.migrate_application(artifacts, request(drill.target.receipt_id, 6_200, 0)),
        Err(ApplicationAuthorityError::MigrationStepCountZero)
    ));

    // Same manifest content: re-verify the installed 1.0.0 under a fresh
    // artifact key — a distinct receipt with an unchanged digest.
    let unchanged =
        drill
            .stack
            .verify_package(0x41, pack_package_version(1, 0, 0), key(0xE4), 6_060);
    assert_ne!(unchanged.receipt_id, drill.target.receipt_id);
    assert!(matches!(
        authority.migrate_application(artifacts, request(unchanged.receipt_id, 6_200, 2)),
        Err(ApplicationAuthorityError::UpdateManifestUnchanged { .. })
    ));

    let cross_major =
        drill
            .stack
            .verify_package(0x41, pack_package_version(2, 0, 0), key(0xE5), 6_060);
    assert!(matches!(
        authority.migrate_application(artifacts, request(cross_major.receipt_id, 6_200, 2)),
        Err(ApplicationAuthorityError::UpdateCompatibilityViolation { .. })
    ));

    let cross_minor =
        drill
            .stack
            .verify_package(0x41, pack_package_version(1, 3, 0), key(0xE6), 6_060);
    let mut same_minor = request(cross_minor.receipt_id, 6_200, 2);
    same_minor.compatibility_window = CompatibilityWindow::SameMinor;
    assert!(matches!(
        authority.migrate_application(artifacts, same_minor),
        Err(ApplicationAuthorityError::UpdateCompatibilityViolation { .. })
    ));

    assert!(
        drill
            .authority
            .list_package_migrations(drill.package_id)
            .expect("list migrations")
            .is_empty(),
        "every typed refusal left zero durable migration state"
    );

    // A disabled application refuses a fresh drill with zero state.
    let second = drill_fixture("refusals-disabled", 0x54, 0x43);
    disabled(&second.authority, second.package_id, 0x05, 6_150);
    assert!(matches!(
        second.authority.migrate_application(
            &second.stack.artifacts,
            begin_request(second.package_id, second.target.receipt_id, 0x33, 2, 6_200)
        ),
        Err(ApplicationAuthorityError::ApplicationDisabled { .. })
    ));
    assert!(
        second
            .authority
            .list_package_migrations(second.package_id)
            .expect("list migrations")
            .is_empty()
    );
}

/// 步骤门：严格顺序、完成后拒新步、replay、时间戳冲突与 typed 时序绑定；
/// 未完成演练的 health/activation 是 typed 拒绝。
#[test]
fn migration_step_gates_replay_and_ordering_are_typed() {
    let drill = drill_fixture("steps", 0x55, 0x41);
    let authority = &drill.authority;
    drill.begin(0x41, 2, 6_200);

    assert!(matches!(
        authority.record_migration_step(RecordMigrationStepRequest {
            idempotency_key: key(0x41),
            step_index: 2,
            completed_at_ms: 6_300,
        }),
        Err(ApplicationAuthorityError::MigrationStepOutOfOrder {
            requested_step_index: 2,
            expected_step_index: 1,
            declared_step_count: 2,
        })
    ));

    drill.step(0x41, 1, 6_300);
    let replay = drill.step_replayed(0x41, 1, 6_300);
    assert_eq!(replay.step_index, 1);
    assert_eq!(replay.completed_at_ms, 6_300);

    assert!(matches!(
        authority.record_migration_step(RecordMigrationStepRequest {
            idempotency_key: key(0x41),
            step_index: 1,
            completed_at_ms: 6_350,
        }),
        Err(ApplicationAuthorityError::IdempotencyConflict)
    ));

    assert!(matches!(
        authority.record_migration_step(RecordMigrationStepRequest {
            idempotency_key: key(0x41),
            step_index: 2,
            completed_at_ms: 6_290,
        }),
        Err(ApplicationAuthorityError::MigrationStepPrecedesLastUpdate {
            last_updated_at_ms: 6_300,
            completed_at_ms: 6_290,
        })
    ));

    drill.step(0x41, 2, 6_400);
    assert!(matches!(
        authority.record_migration_step(RecordMigrationStepRequest {
            idempotency_key: key(0x41),
            step_index: 3,
            completed_at_ms: 6_500,
        }),
        Err(ApplicationAuthorityError::MigrationDrillComplete {
            declared_step_count: 2,
            ..
        })
    ));

    let probe = ScriptedProbe::with_verdict(true);
    assert!(matches!(
        authority.run_migration_health_check(&drill.stack.artifacts, key(0x99), &probe, 6_500),
        Err(ApplicationAuthorityError::MigrationNotFound { .. })
    ));
    assert!(matches!(
        authority.activate_package_migration(
            &drill.stack.artifacts,
            ActivatePackageMigrationRequest {
                idempotency_key: key(0x41),
                activated_at_ms: 6_600,
            }
        ),
        Err(ApplicationAuthorityError::MigrationHealthUnchecked { .. })
    ));
    assert_eq!(probe.calls.get(), 0, "no probe consultation on refusal");

    let report = drill.health(0x41, &probe, 6_500);
    assert!(report.passed);
    assert!(matches!(
        authority.activate_package_migration(
            &drill.stack.artifacts,
            ActivatePackageMigrationRequest {
                idempotency_key: key(0x41),
                activated_at_ms: 6_450,
            }
        ),
        Err(ApplicationAuthorityError::ActivationPrecedesHealthCheck {
            health_checked_at_ms: 6_500,
            activated_at_ms: 6_450,
        })
    ));

    // An incomplete drill cannot run its health check.
    let other = drill_fixture("steps-incomplete", 0x56, 0x45);
    other.begin(0x42, 2, 6_200);
    other.step(0x42, 1, 6_300);
    assert!(matches!(
        other.authority.run_migration_health_check(
            &other.stack.artifacts,
            key(0x42),
            &probe,
            6_500
        ),
        Err(ApplicationAuthorityError::MigrationStepsIncomplete {
            completed_step_count: 1,
            declared_step_count: 2,
        })
    ));
    assert_eq!(probe.calls.get(), 1, "only the completed drill consulted");
}

/// 迁移中崩溃→重启收敛（进程重启模型）：每条命令之间重开 authority，
/// 收敛结果与无崩溃对照逐字节相等，且 health 结论落库后 probe 不再被咨询。
#[test]
fn migration_crash_between_steps_converges_on_restart() {
    // Control run: identical seeds, no reopen.
    let control = drill_fixture("crash-control", 0x57, 0x47);
    control.begin(0x51, 2, 6_200);
    control.run_all_steps(0x51, 2, 6_300);
    let control_probe = ScriptedProbe::with_verdict(true);
    control.health(0x51, &control_probe, 6_500);
    let control_receipt = control.activate(0x51, 6_600);
    let control_view = control.application();
    let control_migration = control.migration(0x51);

    // Crash run: reopen the authority after every command.
    let mut drill = drill_fixture("crash-run", 0x57, 0x47);
    drill.begin(0x51, 2, 6_200);
    drill.crash_and_recover();

    let replayed_begin = drill.begin_replayed(0x51, 2, 6_200);
    assert_eq!(replayed_begin.state, MigrationState::Pending);
    assert_eq!(replayed_begin.completed_step_count, 0);

    drill.step(0x51, 1, 6_300);
    drill.crash_and_recover();
    let replayed_step = drill.step_replayed(0x51, 1, 6_300);
    assert_eq!(replay_step_index(&replayed_step), 1);

    drill.step(0x51, 2, 6_400);
    drill.crash_and_recover();

    let probe = ScriptedProbe::with_verdict(true);
    drill.health(0x51, &probe, 6_500);
    drill.crash_and_recover();

    let replay_probe = ScriptedProbe::with_verdict(false);
    let replayed_health = drill.health_replayed(0x51, &replay_probe, 9_999);
    assert!(replayed_health.passed);
    assert_eq!(replayed_health.checked_at_ms, 6_500);
    assert_eq!(
        replay_probe.calls.get(),
        0,
        "the durable verdict is the authority; the probe is never re-consulted"
    );

    let receipt = drill.activate(0x51, 6_600);
    drill.crash_and_recover();
    let replayed_activation = drill.activate_replayed(0x51, 9_999);
    assert_eq!(replayed_activation, receipt);
    assert_eq!(receipt, control_receipt, "crash run converges byte-equal");
    assert_eq!(drill.application(), control_view);
    assert_eq!(drill.migration(0x51), control_migration);

    assert_eq!(control_probe.calls.get(), 1);
    assert_eq!(probe.calls.get(), 1);
}

fn replay_step_index(record: &nlos_application::MigrationStepRecord) -> u64 {
    record.step_index
}

/// 健康检查失败→PKG 级回滚：应用从未离开旧代际，回滚 receipt durable
/// 且可 replay；回滚后新迁移仍可成功；与生命周期 rollback 门不同表不同义。
#[test]
fn health_check_failure_rolls_back_to_prior_revision_durably() {
    let drill = drill_fixture("health-fail", 0x58, 0x48);
    let authority = &drill.authority;
    drill.begin(0x61, 2, 6_200);
    drill.run_all_steps(0x61, 2, 6_300);

    // Rollback before any verdict is a typed refusal.
    assert!(matches!(
        authority.rollback_package_migration(RollbackPackageMigrationRequest {
            idempotency_key: key(0x61),
            rolled_back_at_ms: 6_700,
        }),
        Err(ApplicationAuthorityError::MigrationRequiresFailedHealth {
            health: MigrationHealthState::Unchecked,
        })
    ));

    let probe = ScriptedProbe::with_verdict(false);
    let report = drill.health(0x61, &probe, 6_500);
    assert!(!report.passed);
    let failed = drill.migration(0x61);
    assert_eq!(failed.health, MigrationHealthState::Failed);
    assert_eq!(failed.state, MigrationState::Running);

    let before = drill.application();
    assert_eq!(before.current_installation_generation, Generation::INITIAL);
    assert_eq!(before.package_manifest_digest, failed.from_manifest_digest);

    assert!(matches!(
        authority.activate_package_migration(
            &drill.stack.artifacts,
            ActivatePackageMigrationRequest {
                idempotency_key: key(0x61),
                activated_at_ms: 6_600,
            }
        ),
        Err(ApplicationAuthorityError::MigrationHealthFailed { .. })
    ));

    assert!(matches!(
        authority.rollback_package_migration(RollbackPackageMigrationRequest {
            idempotency_key: key(0x61),
            rolled_back_at_ms: 6_450,
        }),
        Err(
            ApplicationAuthorityError::MigrationRollbackPrecedesHealthCheck {
                health_checked_at_ms: 6_500,
                rolled_back_at_ms: 6_450,
            }
        )
    ));

    let receipt = rolled_back(authority, 0x61, 6_700);
    assert_eq!(receipt.idempotency_key, key(0x61));
    assert_eq!(receipt.application_id, failed.application_id);
    assert_eq!(receipt.retained_generation, Generation::INITIAL);
    assert_eq!(
        receipt.retained_manifest_digest,
        failed.from_manifest_digest
    );
    assert_eq!(
        receipt.abandoned_target_manifest_digest,
        drill.target.manifest_digest
    );
    assert_eq!(receipt.rolled_back_at_ms, 6_700);

    let after = drill.application();
    assert_eq!(
        after, before,
        "the prior revision remains active; the PKG rollback never touches the application row"
    );

    let terminal = drill.migration(0x61);
    assert_eq!(terminal.state, MigrationState::Failed);
    assert_eq!(
        authority
            .inspect_migration_rollback_receipt(key(0x61))
            .expect("inspect migration rollback"),
        Some(receipt.clone())
    );

    assert_eq!(rollback_replayed(authority, 0x61, 6_700), receipt);
    assert!(matches!(
        authority.rollback_package_migration(RollbackPackageMigrationRequest {
            idempotency_key: key(0x61),
            rolled_back_at_ms: 6_800,
        }),
        Err(ApplicationAuthorityError::IdempotencyConflict)
    ));
}

/// 失败演练终态后：生命周期 rollback 门不被 PKG 回滚影响；新迁移可在
/// 保留基线上重新开始并成功激活，旧 PKG 回滚 receipt 留作 durable 历史。
#[test]
fn failed_drill_unblocks_fresh_migration_and_keeps_lifecycle_gate_distinct() {
    let drill = drill_fixture("fresh-after-fail", 0x5D, 0x4E);
    let authority = &drill.authority;
    let failed = drill.begin(0x64, 1, 6_200);
    drill.step(0x64, 1, 6_300);
    drill.health(0x64, &ScriptedProbe::with_verdict(false), 6_500);
    let rollback = rolled_back(authority, 0x64, 6_700);

    // Terminal migrations refuse fresh step/health commands typed.
    assert!(matches!(
        authority.record_migration_step(RecordMigrationStepRequest {
            idempotency_key: key(0x64),
            step_index: 2,
            completed_at_ms: 6_800,
        }),
        Err(ApplicationAuthorityError::MigrationTerminal {
            state: MigrationState::Failed,
            ..
        })
    ));
    assert!(matches!(
        authority.run_migration_health_check(
            &drill.stack.artifacts,
            key(0x64),
            &ScriptedProbe::with_verdict(true),
            6_800
        ),
        Err(ApplicationAuthorityError::MigrationTerminal {
            state: MigrationState::Failed,
            ..
        })
    ));

    // The PKG rollback is distinct from the lifecycle rollback gate: the
    // lifecycle channel still requires a disabled/uninstalled application.
    assert!(matches!(
        authority.rollback_application(RollbackApplicationRequest {
            package_id: drill.package_id,
            idempotency_key: key(0x65),
            rollback_at_ms: 6_800,
        }),
        Err(
            ApplicationAuthorityError::RollbackRequiresDisabledOrUninstalled {
                status: ApplicationStatus::Installed,
                ..
            }
        )
    ));

    // After the failed drill terminated, a fresh migration succeeds on
    // the retained baseline.
    let second = drill.begin(0x66, 1, 6_800);
    assert_eq!(second.from_generation, Generation::INITIAL);
    assert_eq!(second.from_manifest_digest, failed.from_manifest_digest);
    assert_eq!(
        second.from_manifest_digest,
        rollback.retained_manifest_digest
    );
    drill.step(0x66, 1, 6_850);
    drill.health(0x66, &ScriptedProbe::with_verdict(true), 6_900);
    let activated = drill.activate(0x66, 6_950);
    assert_eq!(activated.installation_generation.get(), 2);
    assert_eq!(
        activated.package_manifest_digest,
        drill.target.manifest_digest
    );
    assert_eq!(
        authority
            .inspect_migration_rollback_receipt(key(0x64))
            .expect("inspect migration rollback"),
        Some(rollback),
        "the earlier PKG rollback receipt survives as durable history"
    );
}

/// W28-B 交互：带 manifest `tasks` 模板段的包与无段包走同一迁移面
/// （runner 只消费 verified receipt 形状，段无关）。
#[test]
fn migration_with_templated_package_segment_drills_identically() {
    let stack = TestStack::new(&label("templated"), 0x59);
    let from = stack.verify_package(0x4A, pack_package_version(1, 0, 0), key(0xF0), 6_000);
    let (receipt, _signed) = stack.verify_templated_package(
        0x4A,
        pack_package_version(1, 1, 0),
        vec![support::task_template(
            [0x50; 16],
            nlos_artifact::PackageTaskKind::Executable,
            vec![],
        )],
        key(0xF1),
        6_050,
    );
    let authority = open_authority(stack.root.root());
    installed(&authority, &stack.artifacts, from.receipt_id, 0x01, 6_100);

    let mut request = begin_request(from.package_id, receipt.receipt_id, 0x71, 1, 6_200);
    request.compatibility_window = CompatibilityWindow::SameMajor;
    let view = match authority
        .migrate_application(&stack.artifacts, request)
        .expect("templated migration begins")
    {
        MigrateDecision::Started(view) => view,
        MigrateDecision::Replayed(view) => panic!("fresh key, got {view:?}"),
    };
    assert_eq!(view.target_manifest_digest, receipt.manifest_digest);

    match authority
        .record_migration_step(RecordMigrationStepRequest {
            idempotency_key: key(0x71),
            step_index: 1,
            completed_at_ms: 6_300,
        })
        .expect("templated step")
    {
        RecordMigrationStepDecision::Recorded(record) => assert_eq!(record.step_index, 1),
        RecordMigrationStepDecision::Replayed(record) => panic!("fresh, got {record:?}"),
    }

    let probe = ScriptedProbe::with_verdict(true);
    match authority
        .run_migration_health_check(&stack.artifacts, key(0x71), &probe, 6_500)
        .expect("templated health check")
    {
        MigrationHealthDecision::Recorded(report) => assert!(report.passed),
        MigrationHealthDecision::Replayed(report) => panic!("fresh, got {report:?}"),
    }

    let activated = match authority
        .activate_package_migration(
            &stack.artifacts,
            ActivatePackageMigrationRequest {
                idempotency_key: key(0x71),
                activated_at_ms: 6_600,
            },
        )
        .expect("templated activation")
    {
        ActivateMigrationDecision::Activated(activated) => activated,
        ActivateMigrationDecision::Replayed(activated) => panic!("fresh, got {activated:?}"),
    };
    assert_eq!(activated.package_manifest_digest, receipt.manifest_digest);
    assert_eq!(activated.package_version, receipt.package_version);
    assert_eq!(probe.seen_from_generation.get(), 1);
}

/// 与直接 update 通道交互：在途迁移被直接 update 超越后，激活 typed
/// 拒绝（基线已移动），应用停留在 update 的代际；迁移成功后直接通道
/// 继续稠密推进。
#[test]
fn migration_interplays_with_direct_update_channel() {
    let drill = drill_fixture("superseded", 0x5A, 0x4B);
    let authority = &drill.authority;
    drill.begin(0x81, 2, 6_200);
    drill.step(0x81, 1, 6_300);

    // The direct update channel advances the generation past the drill's
    // frozen baseline.
    let direct = drill
        .stack
        .verify_package(0x4B, pack_package_version(1, 9, 0), key(0xE8), 6_050);
    let direct_receipt = updated(
        authority,
        &drill.stack.artifacts,
        drill.package_id,
        direct.receipt_id,
        0x02,
        6_350,
    );
    assert_eq!(direct_receipt.installation_generation.get(), 2);

    drill.step(0x81, 2, 6_400);
    drill.health(0x81, &ScriptedProbe::with_verdict(true), 6_500);
    assert!(matches!(
        authority.activate_package_migration(
            &drill.stack.artifacts,
            ActivatePackageMigrationRequest {
                idempotency_key: key(0x81),
                activated_at_ms: 6_600,
            }
        ),
        Err(ApplicationAuthorityError::MigrationBaselineMoved {
            from_generation,
            current_generation,
            ..
        }) if from_generation == Generation::INITIAL
            && current_generation.get() == 2
    ));
    let app = drill.application();
    assert_eq!(
        app.package_manifest_digest, direct_receipt.package_manifest_digest,
        "the superseded drill never moves the application silently"
    );

    // The stranded live migration blocks a fresh drill (documented
    // deferred minor: no abandon command in this slice).
    assert!(matches!(
        authority.migrate_application(
            &drill.stack.artifacts,
            begin_request(drill.package_id, drill.target.receipt_id, 0x82, 1, 6_200)
        ),
        Err(ApplicationAuthorityError::MigrationAlreadyLive { .. })
    ));
}

/// 迁移成功后直接通道与第二次迁移继续稠密推进代际。
#[test]
fn post_migration_generation_continues_dense() {
    let drill = drill_fixture("dense", 0x5B, 0x4C);
    let authority = &drill.authority;
    drill.begin(0x91, 1, 6_200);
    drill.step(0x91, 1, 6_300);
    drill.health(0x91, &ScriptedProbe::with_verdict(true), 6_500);
    let migration_receipt = drill.activate(0x91, 6_600);
    assert_eq!(migration_receipt.installation_generation.get(), 2);

    let patch = drill
        .stack
        .verify_package(0x4C, pack_package_version(1, 2, 6), key(0xE9), 6_050);
    let updated_receipt = updated(
        authority,
        &drill.stack.artifacts,
        drill.package_id,
        patch.receipt_id,
        0x03,
        6_700,
    );
    assert_eq!(updated_receipt.installation_generation.get(), 3);

    let later = drill
        .stack
        .verify_package(0x4C, pack_package_version(1, 4, 0), key(0xEA), 6_050);
    let view = match authority
        .migrate_application(
            &drill.stack.artifacts,
            begin_request(drill.package_id, later.receipt_id, 0x92, 1, 6_750),
        )
        .expect("SameMajor admits the minor-bump drill after the direct update")
    {
        MigrateDecision::Started(view) => view,
        MigrateDecision::Replayed(view) => panic!("fresh key, got {view:?}"),
    };
    assert_eq!(view.from_generation.get(), 3);
    drill.step(0x92, 1, 6_800);
    drill.health(0x92, &ScriptedProbe::with_verdict(true), 6_850);
    let second = drill.activate(0x92, 6_900);
    assert_eq!(second.installation_generation.get(), 4);

    let generations = authority
        .list_installations(second.application_id)
        .expect("list")
        .iter()
        .map(|receipt| receipt.installation_generation.get())
        .collect::<Vec<_>>();
    assert_eq!(generations, vec![1, 2, 3, 4]);
}

/// `SameMinor` 窗口下的迁移：minor 一致 patch 递增被接受，跨 minor 被
/// typed 拒绝（与 update 通道同一窗口语义）。
#[test]
fn migration_same_minor_window_admits_patch_and_refuses_cross_minor() {
    let drill = drill_fixture("same-minor", 0x5C, 0x4D);
    // 1.0.0 → 1.0.9: same major+minor.
    let patch = drill
        .stack
        .verify_package(0x4D, pack_package_version(1, 0, 9), key(0xEB), 6_050);
    let mut request = begin_request(drill.package_id, patch.receipt_id, 0xA1, 1, 6_200);
    request.compatibility_window = CompatibilityWindow::SameMinor;
    assert!(matches!(
        drill
            .authority
            .migrate_application(&drill.stack.artifacts, request),
        Ok(MigrateDecision::Started(_))
    ));
    drill.step(0xA1, 1, 6_300);
    drill.health(0xA1, &ScriptedProbe::with_verdict(true), 6_500);
    let receipt = drill.activate(0xA1, 6_600);
    assert_eq!(receipt.package_version, pack_package_version(1, 0, 9));

    // A later cross-minor target under SameMinor is refused with zero
    // durable state.
    let cross = drill
        .stack
        .verify_package(0x4D, pack_package_version(1, 1, 0), key(0xEC), 6_050);
    let mut refused = begin_request(drill.package_id, cross.receipt_id, 0xA2, 1, 6_700);
    refused.compatibility_window = CompatibilityWindow::SameMinor;
    assert!(matches!(
        drill
            .authority
            .migrate_application(&drill.stack.artifacts, refused),
        Err(ApplicationAuthorityError::UpdateCompatibilityViolation { .. })
    ));
}
