//! W26-001 ledger acceptance: Semantic commit recovery failures append to a
//! durable CAS-guarded ledger row (total-failure count compare-and-swap,
//! exponential capped backoff, escalation after the fixed consecutive
//! threshold) and read back consistently through `inspect_semantic_recovery`.
//! Task 3 extends the same lane: due scanning (state/time filters plus the
//! ledger-less rescan), CAS resume from `Escalated`, and the mirrored
//! summary counts. Task 4 adds the alert surface (escalated listing,
//! idempotent acknowledgement with immutable receipts) and proves that a
//! successful Semantic finalize resolves the ledger while a lost ledger row
//! never blocks convergence to the unique terminal state.
//!
//! The store/plan fixture mirrors the construction in
//! `nlos-commit-coordinator`'s `semantic_pending_restart_scan.rs` (via
//! `semantic_convergence.rs`): seed a Semantic authority, seal a write set
//! with one durable semantic append, draw a commit permit, and plan the
//! semantic commit — the plan sits in `Planned` (non-Finalized), the only
//! plan state the recovery ledger refuses on the `Finalized` terminal.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_artifact::ArtifactStore;
use nlos_capability::CapabilityTarget;
use nlos_semantic::{PublishSemanticPublicationRequest, SemanticAuthority};
use nlos_task::{
    AttemptSpec, EffectPermitDecision, EffectPermitRequest, FinalizeSemanticCommitRequest,
    LogicalEffectDescriptor, NestedSemanticPublicationReceipt, NoEffectReason, NoEffectRequest,
    ParticipantRegistryBinding, PermitDecision, PermitRequest, PlanSemanticCommitRequest,
    PlannedEffect, PrepareSemanticFinalizeRequest, RecordSemanticPublicationsRequest,
    SemanticCommitPlanState, SemanticFinalizeDecision, SemanticRecoveryAlertAcknowledgeDecision,
    SemanticRecoveryAlertAcknowledgeRequest, SemanticRecoveryFailureRequest,
    SemanticRecoveryFailureSource, SemanticRecoveryRecord, SemanticRecoveryResumeRequest,
    SemanticRecoveryState, SemanticRecoverySummary, SemanticTaskCommitReceipt, SnapshotBundle,
    SnapshotConsistency, SqliteTaskAuthority, TaskSnapshotReceiptSpec, TaskSpec, TaskStoreError,
    TaskWriteSetEffectEndpointRequest, TaskWriteSetRequest, TaskWriteSetSemanticAppendRequest,
    TaskWriteSetSemanticRequiredDurability, TaskWriteSetSemanticTarget, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, NamespaceId, PrincipalId, ReceiptId,
    SemanticEventId, TaskAttemptId, TaskId, TaskSnapshotId,
};
use rusqlite::Connection;

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    task_path: PathBuf,
    semantic_root: PathBuf,
    artifact_root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "nlos-task-semantic-recovery-ledger-{}-{suffix}",
            std::process::id()
        ));
        Self {
            task_path: base.with_extension("sqlite3"),
            semantic_root: base.with_extension("semantic"),
            artifact_root: base.with_extension("artifact"),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = self.task_path.as_os_str().to_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir_all(&self.semantic_root);
        let _ = std::fs::remove_dir_all(&self.artifact_root);
    }
}

fn seed_semantic_authority(
    root: &Path,
) -> (SemanticAuthority, SemanticEventId, ReceiptId, ReceiptId) {
    let semantic = SemanticAuthority::open(root).expect("open Semantic authority");
    let event_id = SemanticEventId::from_bytes([0x90; 32]);
    let admission_receipt_id = ReceiptId::from_bytes([0xa0; 16]);
    let durability_receipt_id = ReceiptId::from_bytes([0xb0; 16]);
    let target = NamespaceId::from_bytes([0xc0; 16]);
    let raw = Connection::open(root.join("semantic-authority.db")).expect("open raw Semantic db");
    raw.execute(
        "INSERT INTO content_objects (content_digest, media_type, exact_bytes)
         VALUES (?1, ?2, ?3)",
        rusqlite::params![[0xd0u8; 32].as_slice(), "text/plain", b"semantic"],
    )
    .expect("insert content");
    raw.execute(
        "INSERT INTO semantic_events (
            event_id, canonical_unsigned_event, event_type, scope_kind, scope_id,
            issuer_principal_id, issuer_process_id, issuer_process_generation,
            control_domain_id, issued_at_unix_ns, valid_until_ms, purpose_digest,
            key_id, content_digest
         ) VALUES (?1, ?2, 1, 1, ?3, ?4, ?5, 1, ?6, 1, NULL, NULL, ?7, ?8)",
        rusqlite::params![
            event_id.as_bytes().as_slice(),
            [0xe1u8, 0xe2, 0xe3].as_slice(),
            target.as_bytes().as_slice(),
            [0xe4u8; 16].as_slice(),
            [0xe5u8; 16].as_slice(),
            [0xe6u8; 16].as_slice(),
            [0xe7u8; 16].as_slice(),
            [0xd0u8; 32].as_slice(),
        ],
    )
    .expect("insert event");
    raw.execute(
        "INSERT INTO event_log (event_id) VALUES (?1)",
        [event_id.as_bytes().as_slice()],
    )
    .expect("insert event log");
    raw.execute(
        "INSERT INTO admission_receipts (
            receipt_id, event_id, log_seq, admitted_at_ms, effective_valid_until_ms,
            effective_taint, authz_policy_digest, durability, store_principal_id,
            store_control_domain_id, store_key_id, store_signature
         ) VALUES (?1, ?2, 1, 100, NULL, 0, ?3, 2, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            admission_receipt_id.as_bytes().as_slice(),
            event_id.as_bytes().as_slice(),
            [0xe8u8; 32].as_slice(),
            [0xe9u8; 16].as_slice(),
            [0xeau8; 16].as_slice(),
            [0xebu8; 16].as_slice(),
            [0xecu8; 64].as_slice(),
        ],
    )
    .expect("insert admission");
    raw.execute(
        "INSERT INTO durability_receipts (
            receipt_id, event_id, durable_checkpoint_id, durable_at_ms, store_signature
         ) VALUES (?1, ?2, ?3, 110, ?4)",
        rusqlite::params![
            durability_receipt_id.as_bytes().as_slice(),
            event_id.as_bytes().as_slice(),
            [0xedu8; 32].as_slice(),
            [0xeeu8; 64].as_slice(),
        ],
    )
    .expect("insert durability");
    drop(raw);
    (
        semantic,
        event_id,
        admission_receipt_id,
        durability_receipt_id,
    )
}

/// Builds a task authority holding one non-Finalized semantic commit plan,
/// reusing the `semantic_pending_restart_scan.rs` construction: seeded
/// Semantic authority, sealed write set with one durable semantic append,
/// commit permit, then `plan_semantic_commit`. With `with_effect` the sealed
/// write set additionally declares one non-required Semantic-admission
/// effect slot — the shape the persisted mixed-finalize envelope requires.
/// The fixture is returned so callers that drive the owner authority keep
/// the temp directories alive for the whole test.
// The reference fixture drives the deprecated unbound seal/permit entry
// points; mirroring it verbatim keeps this ledger fixture reviewable
// against its source.
#[allow(deprecated)]
#[allow(clippy::too_many_lines)] // One verbatim fixture construction, mirroring its source.
fn build_semantic_store_with_pending_plan(
    with_effect: bool,
) -> (
    Fixture,
    SqliteTaskAuthority,
    SemanticAuthority,
    nlos_task::SemanticCommitPlanId,
) {
    let fixture = Fixture::new();
    let (semantic, event_id, admission_receipt_id, durability_receipt_id) =
        seed_semantic_authority(&fixture.semantic_root);
    let artifact = ArtifactStore::open(&fixture.artifact_root).expect("open Artifact authority");
    let task_id = TaskId::from_bytes([0x10; 16]);
    let attempt_id = TaskAttemptId::from_bytes([0x11; 16]);
    let target = NamespaceId::from_bytes([0xc0; 16]);
    let attempt = AttemptSpec {
        task_id,
        attempt_id,
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0x12; 16]),
            snapshot_digest: [0x13; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x14; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x15; 16]),
        registered_at_ms: 10,
    };
    let task = SqliteTaskAuthority::open(&fixture.task_path).expect("open Task authority");
    task.register_task(TaskSpec {
        task_id,
        task_generation: Generation::INITIAL,
        registered_at_ms: 1,
    })
    .unwrap();
    task.register_snapshot_receipt(TaskSnapshotReceiptSpec {
        task_id,
        snapshot: attempt.snapshot,
        receipt_id: ReceiptId::from_bytes([0x16; 16]),
        builder_id: [0x17; 16],
        builder_version_digest: [0x18; 32],
        per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes([0x19; 16])],
        dependency_closure_root: [0x1a; 32],
        semantic_resolver_digest: [0x1b; 32],
        canonical_iteration_digest: [0x1c; 32],
        achieved_consistency: SnapshotConsistency::Causal,
        built_at_ms: 2,
        authority_id: [0x1d; 16],
        key_id: [0x1e; 16],
        signature: [0x1f; 64],
    })
    .unwrap();
    task.register_attempt_with_snapshot_receipt(attempt, ReceiptId::from_bytes([0x16; 16]))
        .unwrap();
    let registry = task.inspect_participant_registry(task_id).unwrap();
    task.register_semantic_admission_participant(
        &semantic,
        task_id,
        ParticipantRegistryBinding {
            generation: registry.generation,
            root: registry.root,
        },
        3,
    )
    .unwrap();
    let planned_effects = if with_effect {
        vec![mixed_effect(task_id)]
    } else {
        Vec::new()
    };
    let effect_endpoints = if with_effect {
        vec![TaskWriteSetEffectEndpointRequest::SemanticAdmission { effect_seq: 0 }]
    } else {
        Vec::new()
    };
    let write_set = task
        .seal_task_write_set_with_semantic_authority(
            &artifact,
            &semantic,
            TaskWriteSetRequest {
                task_id,
                attempt_id,
                attempt_generation: Generation::INITIAL,
                artifact_reads: Vec::new(),
                artifact_writes: Vec::new(),
                process_binding: None,
                semantic_reads: Vec::new(),
                semantic_appends: vec![TaskWriteSetSemanticAppendRequest {
                    event_id,
                    target: TaskWriteSetSemanticTarget::Namespace(target),
                    required_durability: TaskWriteSetSemanticRequiredDurability::Durable,
                    expected_admission_policy_digest: [0xe8; 32],
                    durability_receipt_id: Some(durability_receipt_id),
                }],
                resource_reservations: Vec::new(),
                planned_effects,
                effect_endpoints,
                idempotency_key: IdempotencyKey::from_bytes([0x20; 16]),
                sealed_at_ms: 4,
            },
        )
        .unwrap()
        .record()
        .clone();
    assert_eq!(
        write_set.semantic_appends[0].admission_receipt_id,
        admission_receipt_id
    );
    let permit = match task
        .request_commit_permit(PermitRequest {
            task_id,
            attempt_id,
            attempt_generation: Generation::INITIAL,
            write_set_root: write_set.write_set_root,
            planned_effects: if with_effect {
                vec![mixed_effect(task_id)]
            } else {
                Vec::new()
            },
            idempotency_key: IdempotencyKey::from_bytes([0x21; 16]),
            valid_until_ms: 1_000,
            requested_at_ms: 5,
        })
        .unwrap()
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    let plan = task
        .plan_semantic_commit(PlanSemanticCommitRequest {
            task_id,
            attempt_id,
            attempt_generation: Generation::INITIAL,
            permit_id: permit.permit_id,
            idempotency_key: IdempotencyKey::from_bytes([0x22; 16]),
            planned_at_ms: 6,
        })
        .unwrap()
        .record()
        .plan_id;
    (fixture, task, semantic, plan)
}

/// One non-required Semantic-admission effect declaration, mirroring
/// `semantic_commit.rs`'s `mixed_effect` fixture shape.
fn mixed_effect(task_id: TaskId) -> PlannedEffect {
    PlannedEffect {
        descriptor: LogicalEffectDescriptor {
            task_id,
            task_generation: Generation::INITIAL,
            intent_spec_id: [0x73; 32],
            stable_action_slot: 1,
            target_authority_object_id: [0x74; 32],
            effect_class: 1,
            idempotency_scope: 1,
        },
        required: false,
        required_condition_digest: None,
        success_criteria_digest: [0x75; 32],
        action_proposal_digest: [0x76; 32],
    }
}

fn semantic_store_with_pending_plan() -> (SqliteTaskAuthority, nlos_task::SemanticCommitPlanId) {
    let (_fixture, task, _semantic, plan_id) = build_semantic_store_with_pending_plan(false);
    (task, plan_id)
}

#[test]
fn record_failure_roundtrips_and_cas_rejects_stale_expected() {
    let (authority, plan_id) = semantic_store_with_pending_plan();
    let first = authority
        .record_semantic_recovery_failure(SemanticRecoveryFailureRequest {
            plan_id,
            expected_total_failures: 0,
            source: SemanticRecoveryFailureSource::Coordinator,
            observed_at_ms: 1_000,
            base_delay_ms: 100,
            max_delay_ms: 5_000,
        })
        .unwrap();
    assert_eq!(first.total_failures, 1);
    assert_eq!(first.state, SemanticRecoveryState::Retrying);
    assert_eq!(first.next_retry_at_ms, Some(1_000 + 100));
    // 过期 expected 被 CAS 拒绝
    let err = authority
        .record_semantic_recovery_failure(SemanticRecoveryFailureRequest {
            plan_id,
            expected_total_failures: 0,
            source: SemanticRecoveryFailureSource::Coordinator,
            observed_at_ms: 1_100,
            base_delay_ms: 100,
            max_delay_ms: 5_000,
        })
        .unwrap_err();
    assert!(matches!(
        err,
        TaskStoreError::SemanticRecoveryCasMismatch {
            expected: 0,
            current: 1,
        }
    ));
    // 回读一致
    let read = authority
        .inspect_semantic_recovery(plan_id)
        .unwrap()
        .unwrap();
    assert_eq!(read.total_failures, 1);
}

/// Records one Semantic recovery failure with the shared test delays
/// (base 100 ms, capped 5 000 ms) and returns the durable record.
fn record_failure(
    authority: &SqliteTaskAuthority,
    plan_id: nlos_task::SemanticCommitPlanId,
    expected_total_failures: u64,
    observed_at_ms: i64,
) -> SemanticRecoveryRecord {
    authority
        .record_semantic_recovery_failure(SemanticRecoveryFailureRequest {
            plan_id,
            expected_total_failures,
            source: SemanticRecoveryFailureSource::Coordinator,
            observed_at_ms,
            base_delay_ms: 100,
            max_delay_ms: 5_000,
        })
        .expect("record semantic recovery failure")
}

#[test]
fn due_scan_filters_state_and_time_and_resume_requeues() {
    let (authority, plan_id) = semantic_store_with_pending_plan();
    // 无台账行的 incomplete plan 立即可扫:台账丢失重扫 durable plan 重建
    // 调度(spec SEM-RECOV-005,镜像 artifact 版 LEFT JOIN 的"无台账即到期")
    let ledgerless = authority.list_due_semantic_commit_plans(10, 0).unwrap();
    assert_eq!(ledgerless.len(), 1);
    assert_eq!(ledgerless[0].plan_id, plan_id);
    // 三次失败推高 next_retry
    let mut expected = 0u64;
    for t in [1_000i64, 2_000, 3_000] {
        let rec = record_failure(&authority, plan_id, expected, t);
        expected = rec.total_failures;
    }
    // 第三次失败 consecutive=3 → 退避 100 * 2^2 = 400 → next_retry = 3_400
    // 未到期:不返回
    assert!(
        authority
            .list_due_semantic_commit_plans(10, 3_000)
            .unwrap()
            .is_empty()
    );
    // 到期:返回该 plan
    let due = authority.list_due_semantic_commit_plans(10, 3_500).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].plan_id, plan_id);
    // limit=0 返回空(镜像 list_incomplete 的守卫)
    assert!(
        authority
            .list_due_semantic_commit_plans(0, 3_500)
            .unwrap()
            .is_empty()
    );
    // 负扫描时间戳被拒(镜像 artifact 守卫)
    assert!(matches!(
        authority.list_due_semantic_commit_plans(10, -1),
        Err(TaskStoreError::InvalidSemanticRecoveryPolicy { .. })
    ));
    // 连续失败达到固定阈值 8 后 Escalated:不进扫描
    for t in (4_000i64..=8_000).step_by(1_000) {
        let rec = record_failure(&authority, plan_id, expected, t);
        expected = rec.total_failures;
    }
    assert_eq!(expected, 8);
    assert!(
        authority
            .list_due_semantic_commit_plans(10, i64::MAX)
            .unwrap()
            .is_empty()
    );
    // resume(Escalated→Retrying CAS)后重回调度,总失败史保留
    let resumed = authority
        .resume_semantic_recovery(SemanticRecoveryResumeRequest {
            plan_id,
            expected_total_failures: expected,
            resumed_at_ms: 10_000,
        })
        .unwrap();
    assert_eq!(resumed.state, SemanticRecoveryState::Retrying);
    assert_eq!(resumed.consecutive_failures, 0);
    assert_eq!(resumed.total_failures, 8);
    assert_eq!(resumed.next_retry_at_ms, Some(10_000));
    assert_eq!(resumed.escalated_at_ms, None);
    let requed = authority
        .list_due_semantic_commit_plans(10, 10_000)
        .unwrap();
    assert_eq!(requed.len(), 1);
    assert_eq!(requed[0].plan_id, plan_id);
}

#[test]
fn escalation_threshold_pinned_at_eight_and_backoff_saturates() {
    let (authority, plan_id) = semantic_store_with_pending_plan();
    let mut expected = 0u64;
    let mut records = Vec::new();
    for t in (1_000i64..=8_000).step_by(1_000) {
        let rec = record_failure(&authority, plan_id, expected, t);
        expected = rec.total_failures;
        records.push(rec);
    }
    // 指数封顶退避是请求与连续计数的纯函数(无 jitter):
    // consecutive=k → 100 * 2^(k-1),封顶 5 000
    assert_eq!(records[0].next_retry_at_ms, Some(1_000 + 100)); // 2^0
    assert_eq!(records[2].next_retry_at_ms, Some(3_000 + 400)); // 2^2
    assert_eq!(records[5].next_retry_at_ms, Some(6_000 + 3_200)); // 2^5
    assert_eq!(records[6].next_retry_at_ms, Some(7_000 + 5_000)); // 2^6=6400 → 封顶
    // 第 7 次失败仍未升级
    assert_eq!(records[6].state, SemanticRecoveryState::Retrying);
    assert!(records[6].next_retry_at_ms.is_some());
    // 第 8 次(阈值常量 = 8)进 Escalated,记录形状钉死
    let escalated = records[7];
    assert_eq!(escalated.consecutive_failures, 8);
    assert_eq!(escalated.total_failures, 8);
    assert_eq!(escalated.state, SemanticRecoveryState::Escalated);
    assert_eq!(escalated.next_retry_at_ms, None); // Escalated 后不再调度
    assert_eq!(escalated.escalated_at_ms, Some(8_000)); // 触发失败时刻置时
    assert_eq!(escalated.resolved_at_ms, None);
    // 台账读回一致
    let read = authority
        .inspect_semantic_recovery(plan_id)
        .unwrap()
        .unwrap();
    assert_eq!(read, escalated);
}

#[test]
fn resume_rejects_missing_ledger_stale_cas_and_non_escalated_state() {
    let (authority, plan_id) = semantic_store_with_pending_plan();
    // 负 resume 时间戳被拒
    assert!(matches!(
        authority.resume_semantic_recovery(SemanticRecoveryResumeRequest {
            plan_id,
            expected_total_failures: 0,
            resumed_at_ms: -1,
        }),
        Err(TaskStoreError::InvalidSemanticRecoveryPolicy { .. })
    ));
    // 无台账行:无可恢复的调度(镜像 artifact 的 ledger-not-found 语义;
    // 复用既有 SemanticCommitPlanNotFound 变体)
    assert!(matches!(
        authority.resume_semantic_recovery(SemanticRecoveryResumeRequest {
            plan_id,
            expected_total_failures: 0,
            resumed_at_ms: 1_000,
        }),
        Err(TaskStoreError::SemanticCommitPlanNotFound)
    ));
    let first = record_failure(&authority, plan_id, 0, 1_000);
    assert_eq!(first.total_failures, 1);
    // 过期 CAS 先于状态校验被拒
    assert!(matches!(
        authority.resume_semantic_recovery(SemanticRecoveryResumeRequest {
            plan_id,
            expected_total_failures: 0,
            resumed_at_ms: 2_000,
        }),
        Err(TaskStoreError::SemanticRecoveryCasMismatch {
            expected: 0,
            current: 1
        })
    ));
    // 非 Escalated 状态拒绝 resume
    assert!(matches!(
        authority.resume_semantic_recovery(SemanticRecoveryResumeRequest {
            plan_id,
            expected_total_failures: 1,
            resumed_at_ms: 2_000,
        }),
        Err(TaskStoreError::InvalidSemanticRecoveryState {
            state: SemanticRecoveryState::Retrying
        })
    ));
    // 升级后:resume 时间戳回退被拒
    let mut expected = first.total_failures;
    for t in (2_000i64..=8_000).step_by(1_000) {
        expected = record_failure(&authority, plan_id, expected, t).total_failures;
    }
    assert_eq!(expected, 8);
    assert!(matches!(
        authority.resume_semantic_recovery(SemanticRecoveryResumeRequest {
            plan_id,
            expected_total_failures: 8,
            resumed_at_ms: 7_999,
        }),
        Err(TaskStoreError::InvalidSemanticRecoveryPolicy { .. })
    ));
}

#[test]
fn summarize_counts_semantic_recovery_states() {
    let (authority, plan_id) = semantic_store_with_pending_plan();
    assert_eq!(
        authority.summarize_semantic_recovery().unwrap(),
        SemanticRecoverySummary::default()
    );
    // 一次失败 → retrying=1
    let first = record_failure(&authority, plan_id, 0, 1_000);
    assert_eq!(
        authority.summarize_semantic_recovery().unwrap(),
        SemanticRecoverySummary {
            retrying: 1,
            escalated: 0,
            unacknowledged_escalated: 0,
            resolved: 0,
        }
    );
    // 连续 8 次 → Escalated;semantic 告警确认面尚未开放(W26 后续任务),
    // 全部 escalated 均未确认
    let mut expected = first.total_failures;
    for t in (2_000i64..=8_000).step_by(1_000) {
        expected = record_failure(&authority, plan_id, expected, t).total_failures;
    }
    assert_eq!(expected, 8);
    assert_eq!(
        authority.summarize_semantic_recovery().unwrap(),
        SemanticRecoverySummary {
            retrying: 0,
            escalated: 1,
            unacknowledged_escalated: 1,
            resolved: 0,
        }
    );
    // resume 后重回 retrying;本用例不驱动 finalize,故 resolved 恒为 0
    // (resolve 路径由 Task 4 的 finalize 用例覆盖)
    authority
        .resume_semantic_recovery(SemanticRecoveryResumeRequest {
            plan_id,
            expected_total_failures: 8,
            resumed_at_ms: 9_000,
        })
        .unwrap();
    assert_eq!(
        authority.summarize_semantic_recovery().unwrap(),
        SemanticRecoverySummary {
            retrying: 1,
            escalated: 0,
            unacknowledged_escalated: 0,
            resolved: 0,
        }
    );
}

/// Drives the ledger to the fixed escalation threshold (eight consecutive
/// failures at the shared test delays) and returns the durable escalated
/// record.
fn escalate_plan(
    authority: &SqliteTaskAuthority,
    plan_id: nlos_task::SemanticCommitPlanId,
) -> SemanticRecoveryRecord {
    let mut expected = 0u64;
    let mut escalated = None;
    for t in (1_000i64..=8_000).step_by(1_000) {
        let record = record_failure(authority, plan_id, expected, t);
        expected = record.total_failures;
        escalated = Some(record);
    }
    escalated.expect("eight failures recorded")
}

/// Manually replays the coordinator's converge prefix from
/// `semantic_pending_restart_scan.rs` without the coordinator crate:
/// authorize the publication, let the owner authority publish, and consume
/// the nested receipt set until the plan is `Ready`.
fn converge_semantic_plan_to_ready(
    task: &SqliteTaskAuthority,
    semantic: &SemanticAuthority,
    plan_id: nlos_task::SemanticCommitPlanId,
    now_ms: i64,
) {
    task.authorize_semantic_publication(plan_id, now_ms)
        .expect("authorize semantic publication");
    let progress = task.inspect_semantic_commit_progress(plan_id).unwrap();
    assert_eq!(progress.plan.state, SemanticCommitPlanState::Publishing);
    let expectation = task
        .inspect_semantic_commit_expectations(plan_id)
        .unwrap()
        .into_iter()
        .next()
        .expect("fixture declares one Semantic publication");
    let owner = semantic
        .publish_semantic_publication(PublishSemanticPublicationRequest {
            task_id: progress.plan.task_id,
            permit_id: progress.plan.permit_id,
            write_set_root: progress.plan.write_set_root,
            event_id: expectation.event_id,
            target: match expectation.target {
                TaskWriteSetSemanticTarget::Namespace(namespace) => {
                    CapabilityTarget::Namespace(namespace)
                }
                TaskWriteSetSemanticTarget::Task(task) => CapabilityTarget::Task(task),
            },
            admission_receipt_id: expectation.admission_receipt_id,
            durability_receipt_id: expectation.durability_receipt_id,
            published_at_ms: u64::try_from(now_ms).expect("non-negative converge clock"),
        })
        .expect("owner publishes sealed expectation")
        .receipt();
    let nested = NestedSemanticPublicationReceipt {
        receipt_id: owner.receipt_id,
        task_id: owner.task_id,
        permit_id: owner.permit_id,
        write_set_root: owner.write_set_root,
        event_id: owner.event_id,
        target: expectation.target,
        log_seq: owner.log_seq,
        admission_receipt_id: owner.admission_receipt_id,
        durability_receipt_id: owner.durability_receipt_id,
        semantic_checkpoint_after: owner.semantic_checkpoint_after,
        created_at_ms: owner.created_at_ms,
    };
    let updated = task
        .record_semantic_publications(
            semantic,
            RecordSemanticPublicationsRequest {
                plan_id,
                receipts: vec![nested],
                observed_at_ms: now_ms,
            },
        )
        .expect("consume owner publication receipt");
    assert_eq!(updated.plan.state, SemanticCommitPlanState::Ready);
}

/// Drives the full `semantic_pending_restart_scan.rs` converge loop to the
/// terminal finalize (no persisted mixed envelope in this shape, so the
/// coordinator's `finalize_ready` lands on `finalize_semantic_commit`).
fn converge_semantic_plan(
    task: &SqliteTaskAuthority,
    semantic: &SemanticAuthority,
    plan_id: nlos_task::SemanticCommitPlanId,
    now_ms: i64,
) -> SemanticTaskCommitReceipt {
    converge_semantic_plan_to_ready(task, semantic, plan_id, now_ms);
    let decision = task
        .finalize_semantic_commit(FinalizeSemanticCommitRequest {
            plan_id,
            finalized_at_ms: now_ms,
        })
        .expect("terminal semantic finalize");
    match decision {
        SemanticFinalizeDecision::Committed(receipt)
        | SemanticFinalizeDecision::Replayed(receipt) => *receipt,
    }
}

#[test]
#[allow(clippy::too_many_lines)] // Lifecycle stays adjacent for audit, mirroring the artifact alert test.
fn alert_lifecycle_and_finalize_resolves_ledger() {
    let (fixture, authority, semantic, plan_id) = build_semantic_store_with_pending_plan(false);
    // 失败至升级阈值,产生告警
    let escalated = escalate_plan(&authority, plan_id);
    assert_eq!(escalated.state, SemanticRecoveryState::Escalated);
    let alerts = authority.list_semantic_recovery_alerts().unwrap();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].recovery, escalated);
    assert_eq!(alerts[0].acknowledgement, None);
    assert_eq!(
        authority
            .summarize_semantic_recovery()
            .unwrap()
            .unacknowledged_escalated,
        1
    );
    let request = SemanticRecoveryAlertAcknowledgeRequest {
        plan_id,
        expected_total_failures: escalated.total_failures,
        principal_id: PrincipalId::from_bytes([0x71; 16]),
        idempotency_key: IdempotencyKey::from_bytes([0x72; 16]),
        acknowledged_at_ms: 9_000,
    };
    let SemanticRecoveryAlertAcknowledgeDecision::Acknowledged(receipt) = authority
        .acknowledge_semantic_recovery_alert(request)
        .unwrap()
    else {
        panic!("first acknowledgement must create a receipt");
    };
    assert_eq!(receipt.plan_id, plan_id);
    assert_eq!(receipt.total_failures, 8);
    assert_eq!(receipt.acknowledged_at_ms, 9_000);
    // 确认不 resume:台账保持 Escalated,且告警已确认
    assert_eq!(
        authority
            .inspect_semantic_recovery(plan_id)
            .unwrap()
            .unwrap()
            .state,
        SemanticRecoveryState::Escalated
    );
    assert_eq!(
        authority
            .summarize_semantic_recovery()
            .unwrap()
            .unacknowledged_escalated,
        0
    );
    // 同 key 重放 Acknowledged→Replayed:同一 receipt,不双记
    assert_eq!(
        authority
            .acknowledge_semantic_recovery_alert(request)
            .unwrap(),
        SemanticRecoveryAlertAcknowledgeDecision::Replayed(receipt)
    );
    let alerts = authority.list_semantic_recovery_alerts().unwrap();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].acknowledgement, Some(receipt));
    // finalize 成功(手动 converge,镜像 semantic_pending_restart_scan 驱动)
    // → 同一事务内置 Resolved,总失败史保留
    let committed = converge_semantic_plan(&authority, &semantic, plan_id, 10_000);
    assert_eq!(committed.task_receipt.new_head_commit_seq, 1);
    let resolved = authority
        .inspect_semantic_recovery(plan_id)
        .unwrap()
        .unwrap();
    assert_eq!(resolved.state, SemanticRecoveryState::Resolved);
    assert_eq!(resolved.total_failures, 8);
    assert_eq!(resolved.consecutive_failures, 0);
    assert_eq!(resolved.resolved_at_ms, Some(10_000));
    assert_eq!(resolved.escalated_at_ms, None);
    assert_eq!(resolved.next_retry_at_ms, None);
    // Resolved 后:告警面清空、到期扫描不返回、汇总 resolved=1
    assert!(
        authority
            .list_semantic_recovery_alerts()
            .unwrap()
            .is_empty()
    );
    assert!(
        authority
            .list_due_semantic_commit_plans(10, i64::MAX)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        authority.summarize_semantic_recovery().unwrap(),
        SemanticRecoverySummary {
            retrying: 0,
            escalated: 0,
            unacknowledged_escalated: 0,
            resolved: 1,
        }
    );
    // 自愈:删台账行后重开,plan 终态不变,重扫不产生幻影收敛。
    // 已确认的告警 receipt 以外键钉住台账行(v42 完整性),模拟"已确认
    // 行丢失"的损坏路径需先关外键强制(镜像 migrate_v40 的 pragma 模式)
    drop(authority);
    let raw = Connection::open(&fixture.task_path).unwrap();
    raw.pragma_update(None, "foreign_keys", "OFF").unwrap();
    raw.execute(
        "DELETE FROM task_semantic_recovery WHERE plan_id = ?1",
        [plan_id.as_bytes().as_slice()],
    )
    .unwrap();
    drop(raw);
    let reopened = SqliteTaskAuthority::open(&fixture.task_path).unwrap();
    assert!(
        reopened
            .inspect_semantic_recovery(plan_id)
            .unwrap()
            .is_none()
    );
    // Finalized plan 不回到期扫描
    assert!(
        reopened
            .list_due_semantic_commit_plans(10, i64::MAX)
            .unwrap()
            .is_empty()
    );
    // 幂等重放 finalize:终态唯一,不复活台账行
    let replay = reopened
        .finalize_semantic_commit(FinalizeSemanticCommitRequest {
            plan_id,
            finalized_at_ms: 11_000,
        })
        .unwrap();
    assert!(matches!(replay, SemanticFinalizeDecision::Replayed(_)));
    assert_eq!(replay.receipt(), &committed);
    assert!(
        reopened
            .inspect_semantic_recovery(plan_id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn alert_acknowledgement_fences_and_receipts_are_immutable() {
    let (fixture, authority, _semantic, plan_id) = build_semantic_store_with_pending_plan(false);
    // 负确认时间戳被拒
    assert!(matches!(
        authority.acknowledge_semantic_recovery_alert(SemanticRecoveryAlertAcknowledgeRequest {
            plan_id,
            expected_total_failures: 0,
            principal_id: PrincipalId::from_bytes([0x81; 16]),
            idempotency_key: IdempotencyKey::from_bytes([0x82; 16]),
            acknowledged_at_ms: -1,
        }),
        Err(TaskStoreError::InvalidSemanticRecoveryPolicy { .. })
    ));
    // 无台账行:告警身份不存在复用既有 SemanticCommitPlanNotFound
    // (镜像 artifact 的 ArtifactRecoveryNotFound 语义,不新增变体)
    assert!(matches!(
        authority.acknowledge_semantic_recovery_alert(SemanticRecoveryAlertAcknowledgeRequest {
            plan_id,
            expected_total_failures: 0,
            principal_id: PrincipalId::from_bytes([0x81; 16]),
            idempotency_key: IdempotencyKey::from_bytes([0x82; 16]),
            acknowledged_at_ms: 1_000,
        }),
        Err(TaskStoreError::SemanticCommitPlanNotFound)
    ));
    let escalated = escalate_plan(&authority, plan_id);
    // 过期 CAS 先拒(镜像 artifact 告警确认的失败计数围栏)
    assert!(matches!(
        authority.acknowledge_semantic_recovery_alert(SemanticRecoveryAlertAcknowledgeRequest {
            plan_id,
            expected_total_failures: escalated.total_failures - 1,
            principal_id: PrincipalId::from_bytes([0x81; 16]),
            idempotency_key: IdempotencyKey::from_bytes([0x82; 16]),
            acknowledged_at_ms: 9_000,
        }),
        Err(TaskStoreError::SemanticRecoveryCasMismatch {
            expected: 7,
            current: 8
        })
    ));
    let request = SemanticRecoveryAlertAcknowledgeRequest {
        plan_id,
        expected_total_failures: escalated.total_failures,
        principal_id: PrincipalId::from_bytes([0x81; 16]),
        idempotency_key: IdempotencyKey::from_bytes([0x82; 16]),
        acknowledged_at_ms: 9_000,
    };
    let receipt = authority
        .acknowledge_semantic_recovery_alert(request)
        .unwrap()
        .receipt();
    // 同 key 换 principal → IdempotencyConflict(receipt 不可变,不可改写归属)
    assert!(matches!(
        authority.acknowledge_semantic_recovery_alert(SemanticRecoveryAlertAcknowledgeRequest {
            principal_id: PrincipalId::from_bytes([0x83; 16]),
            ..request
        }),
        Err(TaskStoreError::IdempotencyConflict)
    ));
    // resume(Escalated→Retrying)后:非 Escalated 状态拒绝确认
    authority
        .resume_semantic_recovery(SemanticRecoveryResumeRequest {
            plan_id,
            expected_total_failures: escalated.total_failures,
            resumed_at_ms: 9_500,
        })
        .unwrap();
    assert!(matches!(
        authority.acknowledge_semantic_recovery_alert(SemanticRecoveryAlertAcknowledgeRequest {
            plan_id,
            expected_total_failures: escalated.total_failures,
            principal_id: PrincipalId::from_bytes([0x84; 16]),
            idempotency_key: IdempotencyKey::from_bytes([0x85; 16]),
            acknowledged_at_ms: 9_600,
        }),
        Err(TaskStoreError::InvalidSemanticRecoveryState {
            state: SemanticRecoveryState::Retrying
        })
    ));
    // DDL 不可变触发器:UPDATE/DELETE 告警 receipt 直接 ABORT(v42)
    drop(authority);
    let raw = Connection::open(&fixture.task_path).unwrap();
    assert!(
        raw.execute(
            "UPDATE task_semantic_recovery_alert_receipts
             SET acknowledged_at_ms = acknowledged_at_ms + 1 WHERE receipt_id = ?1",
            [receipt.receipt_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM task_semantic_recovery_alert_receipts WHERE receipt_id = ?1",
            [receipt.receipt_id.as_bytes().as_slice()],
        )
        .is_err()
    );
}

#[test]
fn ledger_row_loss_before_finalize_still_converges() {
    let (fixture, authority, semantic, plan_id) = build_semantic_store_with_pending_plan(false);
    escalate_plan(&authority, plan_id);
    // 删台账行(台账损坏/丢失):plan 本身仍是 durable 事实
    drop(authority);
    let raw = Connection::open(&fixture.task_path).unwrap();
    raw.execute(
        "DELETE FROM task_semantic_recovery WHERE plan_id = ?1",
        [plan_id.as_bytes().as_slice()],
    )
    .unwrap();
    drop(raw);
    let reopened = SqliteTaskAuthority::open(&fixture.task_path).unwrap();
    assert!(
        reopened
            .inspect_semantic_recovery(plan_id)
            .unwrap()
            .is_none()
    );
    // 无台账行的 incomplete plan 立即可扫(SEM-RECOV-005 自愈重建调度)
    let due = reopened.list_due_semantic_commit_plans(10, 0).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].plan_id, plan_id);
    // converge 到唯一终态;台账无行可置,不产生幻影行
    let committed = converge_semantic_plan(&reopened, &semantic, plan_id, 20_000);
    assert_eq!(committed.task_receipt.new_head_commit_seq, 1);
    assert_eq!(
        reopened
            .inspect_semantic_commit_progress(plan_id)
            .unwrap()
            .plan
            .state,
        SemanticCommitPlanState::Finalized
    );
    assert!(
        reopened
            .inspect_semantic_recovery(plan_id)
            .unwrap()
            .is_none()
    );
    assert!(
        reopened
            .list_due_semantic_commit_plans(10, i64::MAX)
            .unwrap()
            .is_empty()
    );
    // 再次 finalize:幂等重放,终态不变
    let replay = reopened
        .finalize_semantic_commit(FinalizeSemanticCommitRequest {
            plan_id,
            finalized_at_ms: 21_000,
        })
        .unwrap();
    assert!(matches!(replay, SemanticFinalizeDecision::Replayed(_)));
    assert_eq!(replay.receipt(), &committed);
}

#[test]
fn persisted_envelope_finalize_resolves_ledger() {
    let (_fixture, authority, semantic, plan_id) = build_semantic_store_with_pending_plan(true);
    let escalated = escalate_plan(&authority, plan_id);
    assert_eq!(escalated.state, SemanticRecoveryState::Escalated);
    // 手动 converge 到 Ready,再走持久化 mixed envelope 的 v3 finalize
    // (reconcile.rs finalize_commit_v3_with_persisted_semantic_envelope)
    converge_semantic_plan_to_ready(&authority, &semantic, plan_id, 20_000);
    authority
        .prepare_semantic_finalize(PrepareSemanticFinalizeRequest {
            plan_id,
            required_satisfaction: Vec::new(),
            fenced_participant_digest: [0; 32],
            prepared_at_ms: 21_000,
        })
        .unwrap();
    let plan = authority.inspect_semantic_commit_plan(plan_id).unwrap();
    let permit = authority
        .inspect_permit(plan.task_id, plan.permit_id)
        .unwrap();
    let issued = match authority
        .request_effect_permit(EffectPermitRequest {
            task_id: plan.task_id,
            attempt_id: plan.attempt_id,
            attempt_generation: plan.attempt_generation,
            permit_id: plan.permit_id,
            permit_epoch: permit.permit_epoch,
            effect_seq: 0,
            idempotency_key: IdempotencyKey::from_bytes([0x77; 16]),
            valid_until_ms: 30_000,
            requested_at_ms: 21_000,
        })
        .unwrap()
    {
        EffectPermitDecision::Issued(issued) | EffectPermitDecision::Replayed(issued) => *issued,
    };
    authority
        .record_no_effect(NoEffectRequest {
            task_id: plan.task_id,
            attempt_id: plan.attempt_id,
            attempt_generation: plan.attempt_generation,
            permit_id: plan.permit_id,
            permit_epoch: permit.permit_epoch,
            effect_seq: 0,
            reason: NoEffectReason::NotSelected,
            dispatch_token: Some(issued.one_shot_dispatch_token),
            recorded_at_ms: 22_000,
        })
        .unwrap();
    let decision = authority
        .finalize_commit_v3_with_persisted_semantic_envelope(&semantic, plan_id, 23_000)
        .unwrap();
    assert!(matches!(decision, SemanticFinalizeDecision::Committed(_)));
    // 成功事务内置 Resolved(SEM-RECOV-004 的 mixed finalize 接线点)
    let resolved = authority
        .inspect_semantic_recovery(plan_id)
        .unwrap()
        .unwrap();
    assert_eq!(resolved.state, SemanticRecoveryState::Resolved);
    assert_eq!(resolved.total_failures, 8);
    assert_eq!(resolved.resolved_at_ms, Some(23_000));
    assert_eq!(resolved.escalated_at_ms, None);
    assert!(
        authority
            .list_semantic_recovery_alerts()
            .unwrap()
            .is_empty()
    );
}
