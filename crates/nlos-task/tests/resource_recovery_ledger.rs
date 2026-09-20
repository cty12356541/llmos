#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
//! W28-C-2 ledger acceptance: Resource coordinator recovery failures
//! append to a durable CAS-guarded ledger row (total-failure count
//! compare-and-swap, exponential capped backoff, escalation after the
//! fixed consecutive threshold pinned at eight) and read back
//! consistently through `inspect_resource_recovery` — the v42 Semantic
//! ledger mirrored statement-for-statement for the third domain
//! (`SEM-RECOV-001..007`, ADR-0017 decision L-B). Task 2 adds the due
//! scan (state/time filters plus the ledger-less rescan), CAS resume from
//! `Escalated`, and the mirrored summary counts; Task 3 adds the alert
//! surface (escalated listing, idempotent acknowledgement with immutable
//! receipts), proves a successful converge resolves the ledger inside the
//! terminal transaction while a lost ledger row never blocks convergence,
//! and pins the not-due boundary (zero ledger failure records).
//!
//! The fixture mirrors `resource_prepare_finalize.rs`: seal a write set
//! with two Resource reservations, draw a plain permit, prepare the
//! finalize envelope — the plan sits in `Planned`, the only pre-terminal
//! state the recovery ledger accepts.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_resource::{
    AccountRecord, CreateAccountRequest, CreateQuoteRequest, DriverRecord, QuoteRecord,
    RegisterDriverRequest, ReservationRecord, ReserveRequest, ResourceAuthority, ResourceDemand,
};
use nlos_task::{
    AttemptSpec, PrepareResourceFinalizeRequest, ResourceCommitPlanState, ResourceConvergeDecision,
    ResourceRecoveryAlertAcknowledgeDecision, ResourceRecoveryAlertAcknowledgeRequest,
    ResourceRecoveryFailureRequest, ResourceRecoveryFailureSource, ResourceRecoveryRecord,
    ResourceRecoveryResumeRequest, ResourceRecoveryState, ResourceRecoverySummary, SnapshotBundle,
    SnapshotConsistency, SqliteTaskAuthority, TaskSnapshotReceiptSpec, TaskSpec, TaskStoreError,
    TaskWriteSetRequest, TaskWriteSetResourceReservationRequest, empty_effect_history_root,
};
use nlos_types::{
    CallId, CancellationScopeId, Generation, IdempotencyKey, OperationId, PrincipalId, ReceiptId,
    TaskAttemptId, TaskId, TaskSnapshotId,
};
use rusqlite::Connection;

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    task_path: PathBuf,
    resource_root: PathBuf,
    artifact_root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "nlos-task-resource-recovery-ledger-{}-{suffix}",
            std::process::id()
        ));
        Self {
            task_path: base.with_extension("sqlite3"),
            resource_root: base.with_extension("resource"),
            artifact_root: base.with_extension("artifact"),
        }
    }

    fn open_task(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.task_path).expect("open task authority")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = self.task_path.as_os_str().to_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir_all(&self.resource_root);
        let _ = std::fs::remove_dir_all(&self.artifact_root);
    }
}

fn task_id() -> TaskId {
    TaskId::from_bytes([0x11; 16])
}

fn attempt_spec() -> AttemptSpec {
    AttemptSpec {
        task_id: task_id(),
        attempt_id: TaskAttemptId::from_bytes([0x21; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0x22; 16]),
            snapshot_digest: [0x23; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x24; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x25; 16]),
        registered_at_ms: 1_010,
    }
}

struct OwnerFixture {
    authority: ResourceAuthority,
    driver: DriverRecord,
    account: AccountRecord,
}

impl OwnerFixture {
    fn new(root: &Path, seed: u8) -> Self {
        let authority = ResourceAuthority::open(root).expect("open resource authority");
        let driver = authority
            .register_driver(RegisterDriverRequest {
                profile_digest: [seed; 32],
                idempotency_key: IdempotencyKey::from_bytes([seed ^ 0x01; 16]),
                created_at_ms: 1_000,
            })
            .expect("register driver")
            .record();
        let account = authority
            .create_account(CreateAccountRequest {
                initial_credit: 1_000,
                idempotency_key: IdempotencyKey::from_bytes([seed ^ 0x02; 16]),
                created_at_ms: 1_000,
            })
            .expect("create account");
        Self {
            authority,
            driver,
            account,
        }
    }

    fn quote(&self, seed: u8, upper_bound: u64) -> QuoteRecord {
        self.authority
            .create_quote(CreateQuoteRequest {
                driver_id: self.driver.driver_id,
                driver_generation: self.driver.generation,
                driver_fencing_token: self.driver.fencing_token,
                operation_proposal_digest: [seed; 32],
                pricing_version: [seed ^ 0x11; 32],
                upper_bound,
                demand_capacity: ResourceDemand::default(),
                valid_until_ms: 9_000,
                idempotency_key: IdempotencyKey::from_bytes([seed ^ 0x12; 16]),
                created_at_ms: 1_000,
            })
            .expect("create quote")
            .record()
    }

    fn reserve(
        &self,
        quote: &QuoteRecord,
        call_id: CallId,
        operation_id: OperationId,
        key: IdempotencyKey,
    ) -> ReservationRecord {
        self.authority
            .reserve(ReserveRequest {
                account_id: self.account.account_id,
                quote_id: quote.quote_id,
                call_id,
                operation_id,
                idempotency_key: key,
                demand: ResourceDemand::default(),
                reserved_at_ms: 1_100,
            })
            .expect("reserve")
            .record()
    }

    fn settle(&self, reservation: &ReservationRecord, final_usage: u64, seed: u8) {
        let activation = self
            .authority
            .activate(nlos_resource::ActivateReservationRequest {
                reservation_id: reservation.reservation_id,
                call_id: reservation.call_id,
                operation_id: reservation.operation_id,
                driver_id: reservation.driver_id,
                driver_generation: reservation.driver_generation,
                driver_fencing_token: reservation.driver_fencing_token,
                activation_token: reservation.activation_token,
                activated_at_ms: 1_400,
            })
            .expect("activate")
            .receipt();
        self.authority
            .finalize_reservation(nlos_resource::FinalizeReservationRequest {
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                activation_receipt_id: activation.receipt_id,
                effect_closed_proof_digest: [seed ^ 0x21; 32],
                final_seq: 0,
                final_usage,
                finalized_at_ms: 1_600,
            })
            .expect("owner finalize");
    }
}

/// Builds the pending-plan fixture: sealed two-reservation write set,
/// plain permit, prepared envelope, plan `Planned`, owner unsettled.
#[allow(clippy::too_many_lines)] // One fixture builder mirrors the sibling bridge fixtures.
fn build_pending_plan() -> (
    Fixture,
    SqliteTaskAuthority,
    OwnerFixture,
    Vec<ReservationRecord>,
    nlos_task::ResourceCommitPlanId,
) {
    let fixture = Fixture::new();
    let owner = OwnerFixture::new(&fixture.resource_root, 0xa1);
    let quote = owner.quote(0xa2, 60);
    let reservation_one = owner.reserve(
        &quote,
        CallId::from_bytes([0xa5; 16]),
        OperationId::from_bytes([0xa6; 16]),
        IdempotencyKey::from_bytes([0xa7; 16]),
    );
    let quote_two = owner.quote(0xa3, 25);
    let reservation_two = owner.reserve(
        &quote_two,
        CallId::from_bytes([0xa9; 16]),
        OperationId::from_bytes([0xaa; 16]),
        IdempotencyKey::from_bytes([0xab; 16]),
    );
    let reservations = vec![reservation_one, reservation_two];
    let authority = fixture.open_task();
    authority
        .register_task(TaskSpec {
            application_id: None,
            plan_revision: None,
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .expect("register task");
    let spec = attempt_spec();
    authority
        .register_snapshot_receipt(TaskSnapshotReceiptSpec {
            task_id: task_id(),
            snapshot: spec.snapshot,
            receipt_id: ReceiptId::from_bytes([0x31; 16]),
            builder_id: [0x32; 16],
            builder_version_digest: [0x33; 32],
            per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes([0x34; 16])],
            dependency_closure_root: [0x35; 32],
            semantic_resolver_digest: [0x36; 32],
            canonical_iteration_digest: [0x37; 32],
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: 1_005,
            authority_id: [0x38; 16],
            key_id: [0x39; 16],
            signature: [0x3a; 64],
        })
        .expect("snapshot receipt");
    authority
        .register_attempt_with_snapshot_receipt(spec, ReceiptId::from_bytes([0x31; 16]))
        .expect("register attempt");
    let registry = authority
        .inspect_participant_registry(task_id())
        .expect("registry");
    let first_binding = nlos_task::ParticipantRegistryBinding {
        generation: registry.generation,
        root: registry.root,
    };
    let driver_registration = authority
        .register_driver_gateway_participant(
            &owner.authority,
            task_id(),
            first_binding,
            owner.driver.driver_id,
            owner.driver.generation,
            1_150,
        )
        .expect("driver participant");
    let second_binding = nlos_task::ParticipantRegistryBinding {
        generation: driver_registration.registry().generation,
        root: driver_registration.registry().root,
    };
    authority
        .register_resource_ledger_participant(
            &owner.authority,
            task_id(),
            second_binding,
            owner.account.account_id,
            Generation::INITIAL,
            1_160,
        )
        .expect("ledger participant");
    let artifact = nlos_artifact::ArtifactStore::open(&fixture.artifact_root).expect("artifact");
    let write_set = authority
        .seal_task_write_set_with_resource_authority(
            &artifact,
            &owner.authority,
            TaskWriteSetRequest {
                task_id: task_id(),
                attempt_id: spec.attempt_id,
                attempt_generation: spec.attempt_generation,
                artifact_reads: Vec::new(),
                artifact_writes: Vec::new(),
                process_binding: None,
                semantic_reads: Vec::new(),
                semantic_appends: Vec::new(),
                resource_reservations: reservations
                    .iter()
                    .map(|reservation| TaskWriteSetResourceReservationRequest {
                        reservation_id: reservation.reservation_id,
                        expected_call_id: reservation.call_id,
                        expected_operation_id: reservation.operation_id,
                        expected_quote_id: reservation.quote_id,
                    })
                    .collect(),
                planned_effects: Vec::new(),
                effect_endpoints: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([0x41; 16]),
                sealed_at_ms: 1_200,
            },
        )
        .expect("seal write set")
        .record()
        .clone();
    let decision = authority
        .request_commit_permit_with_resource_authority(
            &owner.authority,
            nlos_task::PermitRequest {
                task_id: task_id(),
                attempt_id: spec.attempt_id,
                attempt_generation: spec.attempt_generation,
                write_set_root: write_set.write_set_root,
                planned_effects: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([0x42; 16]),
                valid_until_ms: 9_000,
                requested_at_ms: 1_300,
            },
        )
        .expect("permit");
    let permit = match decision {
        nlos_task::PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    let plan_id = authority
        .prepare_resource_finalize(PrepareResourceFinalizeRequest {
            task_id: task_id(),
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: permit.permit_id,
            idempotency_key: IdempotencyKey::from_bytes([0x43; 16]),
            required_satisfaction: Vec::new(),
            fenced_participant_digest: [0x45; 32],
            prepared_at_ms: 1_450,
        })
        .expect("prepare envelope")
        .record()
        .plan_id;
    (fixture, authority, owner, reservations, plan_id)
}

/// Records one Resource recovery failure with the shared test delays
/// (base 100 ms, capped 5 000 ms) and returns the durable record.
fn record_failure(
    authority: &SqliteTaskAuthority,
    plan_id: nlos_task::ResourceCommitPlanId,
    expected_total_failures: u64,
    observed_at_ms: i64,
) -> ResourceRecoveryRecord {
    authority
        .record_resource_recovery_failure(ResourceRecoveryFailureRequest {
            plan_id,
            expected_total_failures,
            source: ResourceRecoveryFailureSource::Coordinator,
            observed_at_ms,
            base_delay_ms: 100,
            max_delay_ms: 5_000,
        })
        .expect("record resource recovery failure")
}

#[test]
fn record_failure_roundtrips_and_cas_rejects_stale_expected() {
    let (_fixture, authority, _owner, _reservations, plan_id) = build_pending_plan();
    let first = record_failure(&authority, plan_id, 0, 1_000);
    assert_eq!(first.plan_id, plan_id);
    assert_eq!(first.state, ResourceRecoveryState::Retrying);
    assert_eq!(first.consecutive_failures, 1);
    assert_eq!(first.total_failures, 1);
    assert_eq!(
        first.last_source,
        ResourceRecoveryFailureSource::Coordinator
    );
    assert_eq!(first.first_failed_at_ms, 1_000);
    assert_eq!(first.next_retry_at_ms, Some(1_100));
    assert_eq!(
        authority.inspect_resource_recovery(plan_id).unwrap(),
        Some(first)
    );
    // 过期 expected 被拒(并发双写至多一次自增)
    assert!(matches!(
        authority.record_resource_recovery_failure(ResourceRecoveryFailureRequest {
            plan_id,
            expected_total_failures: 0,
            source: ResourceRecoveryFailureSource::TaskAuthority,
            observed_at_ms: 1_200,
            base_delay_ms: 100,
            max_delay_ms: 5_000,
        }),
        Err(TaskStoreError::ResourceRecoveryCasMismatch {
            expected: 0,
            current: 1
        })
    ));
    // 按重读值续记 total=2 同行诚实推进
    let second = record_failure(&authority, plan_id, 1, 1_200);
    assert_eq!(second.total_failures, 2);
    assert_eq!(second.consecutive_failures, 2);
    assert_eq!(second.next_retry_at_ms, Some(1_400));
    // 终态 plan 拒绝记录(Finalized ⇒ Resolved 语义)
    // (由 converge 驱动,见 alert_lifecycle 测试)
}

#[test]
fn due_scan_filters_state_and_time_and_resume_requeues() {
    let (_fixture, authority, _owner, _reservations, plan_id) = build_pending_plan();
    // 无台账行的 incomplete plan 立即可扫:台账丢失重扫 durable plan 重建
    // 调度(SEM-RECOV-005 镜像,LEFT JOIN 的“无台账即到期”)
    let ledgerless = authority.list_due_resource_commit_plans(10, 0).unwrap();
    assert_eq!(ledgerless.len(), 1);
    assert_eq!(ledgerless[0].plan_id, plan_id);
    let mut expected = 0u64;
    for t in [1_000i64, 2_000, 3_000] {
        let rec = record_failure(&authority, plan_id, expected, t);
        expected = rec.total_failures;
    }
    // 第三次失败 consecutive=3 → 退避 100 * 2^2 = 400 → next_retry = 3_400
    assert!(
        authority
            .list_due_resource_commit_plans(10, 3_000)
            .unwrap()
            .is_empty()
    );
    let due = authority.list_due_resource_commit_plans(10, 3_500).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].plan_id, plan_id);
    assert!(
        authority
            .list_due_resource_commit_plans(0, 3_500)
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        authority.list_due_resource_commit_plans(10, -1),
        Err(TaskStoreError::InvalidResourceRecoveryPolicy { .. })
    ));
    for t in (4_000i64..=8_000).step_by(1_000) {
        let rec = record_failure(&authority, plan_id, expected, t);
        expected = rec.total_failures;
    }
    assert_eq!(expected, 8);
    // Escalated 不进扫描
    assert!(
        authority
            .list_due_resource_commit_plans(10, i64::MAX)
            .unwrap()
            .is_empty()
    );
    let resumed = authority
        .resume_resource_recovery(ResourceRecoveryResumeRequest {
            plan_id,
            expected_total_failures: expected,
            resumed_at_ms: 10_000,
        })
        .unwrap();
    assert_eq!(resumed.state, ResourceRecoveryState::Retrying);
    assert_eq!(resumed.consecutive_failures, 0);
    assert_eq!(resumed.total_failures, 8);
    assert_eq!(resumed.next_retry_at_ms, Some(10_000));
    let due = authority
        .list_due_resource_commit_plans(10, 10_000)
        .unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].plan_id, plan_id);
}

#[test]
fn escalation_threshold_pinned_at_eight_and_backoff_saturates() {
    let (_fixture, authority, _owner, _reservations, plan_id) = build_pending_plan();
    let mut expected = 0u64;
    let mut record = None;
    for t in (1_000i64..=8_000).step_by(1_000) {
        record = Some(record_failure(&authority, plan_id, expected, t));
        expected = record.as_ref().unwrap().total_failures;
    }
    let escalated = record.expect("eight failures recorded");
    assert_eq!(escalated.state, ResourceRecoveryState::Escalated);
    assert_eq!(escalated.consecutive_failures, 8);
    assert_eq!(escalated.total_failures, 8);
    assert_eq!(escalated.escalated_at_ms, Some(8_000));
    assert_eq!(escalated.next_retry_at_ms, None);
    // 退避公式钉死:consecutive=7 的下一次退避 100*2^6=6400 超过上限
    // 5000 ⇒ 封顶(第 7 次失败 observed 7_000 → next_retry 12_000)。
    let (_f2, authority2, _o2, _r2, plan2) = build_pending_plan();
    let mut e2 = 0u64;
    let mut last = None;
    for t in (1_000i64..=7_000).step_by(1_000) {
        last = Some(record_failure(&authority2, plan2, e2, t));
        e2 = last.as_ref().unwrap().total_failures;
    }
    let seventh = last.expect("seven failures");
    assert_eq!(seventh.state, ResourceRecoveryState::Retrying);
    assert_eq!(seventh.consecutive_failures, 7);
    assert_eq!(seventh.next_retry_at_ms, Some(7_000 + 5_000));
}

#[test]
fn resume_rejects_missing_ledger_stale_cas_and_non_escalated_state() {
    let (_fixture, authority, _owner, _reservations, plan_id) = build_pending_plan();
    // 无台账行 ⇒ plan-not-found 语义(镜像 semantic:不新增 ledger-not-found 变体)
    assert!(matches!(
        authority.resume_resource_recovery(ResourceRecoveryResumeRequest {
            plan_id,
            expected_total_failures: 0,
            resumed_at_ms: 1_000,
        }),
        Err(TaskStoreError::ResourceCommitPlanNotFound)
    ));
    let first = record_failure(&authority, plan_id, 0, 1_000);
    // 非 Escalated 拒绝 resume
    assert!(matches!(
        authority.resume_resource_recovery(ResourceRecoveryResumeRequest {
            plan_id,
            expected_total_failures: 1,
            resumed_at_ms: 2_000,
        }),
        Err(TaskStoreError::InvalidResourceRecoveryState {
            state: ResourceRecoveryState::Retrying,
        })
    ));
    let _ = first;
    let missing_plan = nlos_task::ResourceCommitPlanId::from_bytes([0xee; 16]);
    assert!(matches!(
        authority.resume_resource_recovery(ResourceRecoveryResumeRequest {
            plan_id: missing_plan,
            expected_total_failures: 0,
            resumed_at_ms: 1_000,
        }),
        Err(TaskStoreError::ResourceCommitPlanNotFound)
    ));
}

#[test]
fn summarize_counts_resource_recovery_states() {
    let (_fixture, authority, _owner, _reservations, plan_id) = build_pending_plan();
    assert_eq!(
        authority.summarize_resource_recovery().unwrap(),
        ResourceRecoverySummary::default()
    );
    record_failure(&authority, plan_id, 0, 1_000);
    assert_eq!(
        authority.summarize_resource_recovery().unwrap(),
        ResourceRecoverySummary {
            retrying: 1,
            escalated: 0,
            unacknowledged_escalated: 0,
            resolved: 0,
        }
    );
    let mut expected = 1u64;
    for t in (2_000i64..=8_000).step_by(1_000) {
        let rec = record_failure(&authority, plan_id, expected, t);
        expected = rec.total_failures;
    }
    assert_eq!(
        authority.summarize_resource_recovery().unwrap(),
        ResourceRecoverySummary {
            retrying: 0,
            escalated: 1,
            unacknowledged_escalated: 1,
            resolved: 0,
        }
    );
    let acknowledged = authority
        .acknowledge_resource_recovery_alert(ResourceRecoveryAlertAcknowledgeRequest {
            plan_id,
            expected_total_failures: expected,
            principal_id: PrincipalId::from_bytes([0x77; 16]),
            idempotency_key: IdempotencyKey::from_bytes([0x78; 16]),
            acknowledged_at_ms: 9_000,
        })
        .expect("acknowledge");
    assert!(matches!(
        acknowledged,
        ResourceRecoveryAlertAcknowledgeDecision::Acknowledged(_)
    ));
    assert_eq!(
        authority.summarize_resource_recovery().unwrap(),
        ResourceRecoverySummary {
            retrying: 0,
            escalated: 1,
            unacknowledged_escalated: 0,
            resolved: 0,
        }
    );
}

#[test]
fn not_due_converge_records_no_failure_and_due_converge_resolves_ledger() {
    let (_fixture, authority, owner, reservations, plan_id) = build_pending_plan();
    // owner 未结算:NotDue 决策 ⇒ 零台账行(ADR-0017 决定 1 负向门)
    assert!(matches!(
        authority.converge_resource_commit_plan(&owner.authority, plan_id, 1_800),
        Ok(ResourceConvergeDecision::NotDue(_))
    ));
    assert_eq!(authority.inspect_resource_recovery(plan_id).unwrap(), None);
    // 预置一条 infra 失败行后 owner 结算 ⇒ 收敛把台账置 Resolved(同事务)
    let first = record_failure(&authority, plan_id, 0, 1_900);
    assert_eq!(first.state, ResourceRecoveryState::Retrying);
    for reservation in &reservations {
        owner.settle(reservation, 10, 0xac);
    }
    let decision = authority
        .converge_resource_commit_plan(&owner.authority, plan_id, 2_000)
        .expect("converge");
    assert!(matches!(decision, ResourceConvergeDecision::Finalized(_)));
    let resolved = authority
        .inspect_resource_recovery(plan_id)
        .unwrap()
        .expect("ledger row survives");
    assert_eq!(resolved.state, ResourceRecoveryState::Resolved);
    assert_eq!(resolved.total_failures, 1);
    assert_eq!(resolved.resolved_at_ms, Some(2_000));
    assert_eq!(
        authority
            .inspect_resource_commit_plan(plan_id)
            .unwrap()
            .state,
        ResourceCommitPlanState::Finalized
    );
    // 已 Finalized 的 plan 拒绝再记失败(镜像 SEM-RECOV-004 失败路径零台账写)
    assert!(matches!(
        authority.record_resource_recovery_failure(ResourceRecoveryFailureRequest {
            plan_id,
            expected_total_failures: 1,
            source: ResourceRecoveryFailureSource::Coordinator,
            observed_at_ms: 2_100,
            base_delay_ms: 100,
            max_delay_ms: 5_000,
        }),
        Err(TaskStoreError::InvalidResourceRecoveryState {
            state: ResourceRecoveryState::Resolved,
        })
    ));
    assert_eq!(
        authority.summarize_resource_recovery().unwrap(),
        ResourceRecoverySummary {
            retrying: 0,
            escalated: 0,
            unacknowledged_escalated: 0,
            resolved: 1,
        }
    );
}

#[test]
fn alert_acknowledgement_fences_and_receipts_are_immutable() {
    let (fixture, authority, _owner, _reservations, plan_id) = build_pending_plan();
    let mut expected = 0u64;
    for t in (1_000i64..=8_000).step_by(1_000) {
        let rec = record_failure(&authority, plan_id, expected, t);
        expected = rec.total_failures;
    }
    let alerts = authority.list_resource_recovery_alerts().unwrap();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].recovery.total_failures, 8);
    assert_eq!(alerts[0].acknowledgement, None);
    // 过期 CAS 拒绝
    assert!(matches!(
        authority.acknowledge_resource_recovery_alert(ResourceRecoveryAlertAcknowledgeRequest {
            plan_id,
            expected_total_failures: 7,
            principal_id: PrincipalId::from_bytes([0x77; 16]),
            idempotency_key: IdempotencyKey::from_bytes([0x78; 16]),
            acknowledged_at_ms: 9_000,
        }),
        Err(TaskStoreError::ResourceRecoveryCasMismatch {
            expected: 7,
            current: 8
        })
    ));
    let created = authority
        .acknowledge_resource_recovery_alert(ResourceRecoveryAlertAcknowledgeRequest {
            plan_id,
            expected_total_failures: 8,
            principal_id: PrincipalId::from_bytes([0x77; 16]),
            idempotency_key: IdempotencyKey::from_bytes([0x78; 16]),
            acknowledged_at_ms: 9_000,
        })
        .expect("acknowledge");
    let receipt = created.receipt();
    // 幂等重放(同 key)与 (plan, total) 槽位重放均返回原回执
    let replayed = authority
        .acknowledge_resource_recovery_alert(ResourceRecoveryAlertAcknowledgeRequest {
            plan_id,
            expected_total_failures: 8,
            principal_id: PrincipalId::from_bytes([0x77; 16]),
            idempotency_key: IdempotencyKey::from_bytes([0x78; 16]),
            acknowledged_at_ms: 9_500,
        })
        .expect("replay");
    assert!(matches!(
        replayed,
        ResourceRecoveryAlertAcknowledgeDecision::Replayed(r) if r == receipt
    ));
    // 不同 principal 同 key 冲突
    assert!(matches!(
        authority.acknowledge_resource_recovery_alert(ResourceRecoveryAlertAcknowledgeRequest {
            plan_id,
            expected_total_failures: 8,
            principal_id: PrincipalId::from_bytes([0x79; 16]),
            idempotency_key: IdempotencyKey::from_bytes([0x78; 16]),
            acknowledged_at_ms: 9_600,
        }),
        Err(TaskStoreError::IdempotencyConflict)
    ));
    let alerts = authority.list_resource_recovery_alerts().unwrap();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].acknowledgement, Some(receipt));
    // 回执行不可变(v43 触发器)
    let raw = Connection::open(&fixture.task_path).expect("raw reader");
    assert!(
        raw.execute(
            "UPDATE task_resource_recovery_alert_receipts SET acknowledged_at_ms = 0",
            []
        )
        .is_err()
    );
    assert!(
        raw.execute("DELETE FROM task_resource_recovery_alert_receipts", [])
            .is_err()
    );
}

#[test]
fn ledger_row_loss_before_finalize_still_converges() {
    let (fixture, authority, owner, reservations, plan_id) = build_pending_plan();
    let first = record_failure(&authority, plan_id, 0, 1_000);
    assert_eq!(first.total_failures, 1);
    drop(authority);
    // 静默丢行(F 矩阵 F4 同型):台账行消失 ⇒ 无台账即到期,重扫重建调度
    let raw = Connection::open(&fixture.task_path).expect("raw deleter");
    raw.execute("PRAGMA foreign_keys = OFF", []).unwrap();
    // alert 表无可变行,直接绕过 FK 删除 recovery 行
    raw.execute(
        "DELETE FROM task_resource_recovery WHERE plan_id = ?1",
        [plan_id.as_bytes().as_slice()],
    )
    .expect("delete ledger row");
    drop(raw);
    let reopened = fixture.open_task();
    assert_eq!(reopened.inspect_resource_recovery(plan_id).unwrap(), None);
    let due = reopened.list_due_resource_commit_plans(10, 0).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].plan_id, plan_id);
    for reservation in &reservations {
        owner.settle(reservation, 10, 0xac);
    }
    let decision = reopened
        .converge_resource_commit_plan(&owner.authority, plan_id, 2_000)
        .expect("converge after row loss");
    assert!(matches!(decision, ResourceConvergeDecision::Finalized(_)));
    assert_eq!(
        reopened
            .inspect_resource_commit_plan(plan_id)
            .unwrap()
            .state,
        ResourceCommitPlanState::Finalized
    );
    // 丢行不产生幻影收敛计数:resolve 是无操作,台账保持空
    assert_eq!(reopened.inspect_resource_recovery(plan_id).unwrap(), None);
}
