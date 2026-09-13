//! W26-001 ledger acceptance: Semantic commit recovery failures append to a
//! durable CAS-guarded ledger row (total-failure count compare-and-swap,
//! exponential capped backoff, escalation after the fixed consecutive
//! threshold) and read back consistently through `inspect_semantic_recovery`.
//! Task 3 extends the same lane: due scanning (state/time filters plus the
//! ledger-less rescan), CAS resume from `Escalated`, and the mirrored
//! summary counts.
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
use nlos_semantic::SemanticAuthority;
use nlos_task::{
    AttemptSpec, ParticipantRegistryBinding, PermitDecision, PermitRequest,
    PlanSemanticCommitRequest, SemanticRecoveryFailureRequest, SemanticRecoveryFailureSource,
    SemanticRecoveryRecord, SemanticRecoveryResumeRequest, SemanticRecoveryState,
    SemanticRecoverySummary, SnapshotBundle, SnapshotConsistency, SqliteTaskAuthority,
    TaskSnapshotReceiptSpec, TaskSpec, TaskStoreError, TaskWriteSetRequest,
    TaskWriteSetSemanticAppendRequest, TaskWriteSetSemanticRequiredDurability,
    TaskWriteSetSemanticTarget, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, NamespaceId, ReceiptId, SemanticEventId,
    TaskAttemptId, TaskId, TaskSnapshotId,
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
/// commit permit, then `plan_semantic_commit`.
// The reference fixture drives the deprecated unbound seal/permit entry
// points; mirroring it verbatim keeps this ledger fixture reviewable
// against its source.
#[allow(deprecated)]
fn semantic_store_with_pending_plan() -> (SqliteTaskAuthority, nlos_task::SemanticCommitPlanId) {
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
                planned_effects: Vec::new(),
                effect_endpoints: Vec::new(),
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
            planned_effects: Vec::new(),
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
    (task, plan)
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
    // resume 后重回 retrying;resolve 路径(SEM-RECOV-004 finalize 联动)在
    // 后续任务落地,当前 resolved 恒为 0
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
