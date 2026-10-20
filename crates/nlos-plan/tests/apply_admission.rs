//! W36-P8 apply-time declared-population admission consult battery
//! (W31-G §8.2.4: 声明面 apply 时 `TaskNode` 维 admission consult 仍缺 —
//! only the materialization half was wired by W31-A).
//!
//! [`SqlitePlanAuthority::apply_plan_revision`] is the production
//! declaration face and must consult or typed-deny (W31-G §8.2.4): a
//! missing consult is [`PlanStoreError::DeclarationConsultUnavailable`],
//! never a silent admit. [`SqlitePlanAuthority::apply_plan_revision_with_admission`]
//! is the consult-bearing production path. The consult-free bypass is
//! explicitly named `apply_plan_revision_ungated` and is test/fixture
//! only. The projection is the store-wide persisted `plan_nodes` count
//! plus the keys this revision declares that carry no row yet. A denial
//! is the typed [`nlos_plan::PlanStoreError::DeclarationAdmissionDenied`]
//! with zero durable effects; a failed consult fails closed; idempotent
//! replays bypass the consult (the registration-gate discipline).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_plan::{
    ApplyPlanRevisionRequest, DeclarationAdmissionConsult, DeclarationAdmissionOutcome,
    MaterializationAdmission, MaterializationAdmissionVerdict, MaterializationRequest,
    MaterializationResolution, NodeTransitionRequest, PlanNodeDeclaration, PlanNodeKind,
    PlanNodeState, PlanRevisionDecision, PlanStoreError, SqlitePlanAuthority,
};
use nlos_task::{ScaleProfile, SqliteTaskAuthority, TaskStoreError};
use nlos_types::IdempotencyKey;

/// Tier whose declared-TaskNode dimension is exactly two: the smallest
/// honest population bound (the working-set dimension stays open so it
/// never muddies the declaration verdict).
static NODE_CAP_TWO: ScaleProfile = ScaleProfile {
    profile_id: "task-apply-node-cap-two",
    max_task_nodes: 2,
    max_task_registrations: 64,
    max_active_working_set: 64,
    reclaim_threshold_ratio: None,
};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Root(std::path::PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "nlos-plan-apply-admission-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create fixture directory");
        Self(path)
    }

    fn plan(&self) -> SqlitePlanAuthority {
        SqlitePlanAuthority::open(self.0.join("plan.sqlite3")).expect("open plan authority")
    }

    fn task(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open_with_scale_profile(self.0.join("task.sqlite3"), &NODE_CAP_TWO)
            .expect("open task authority")
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn node(key: u8, payload: u8) -> PlanNodeDeclaration {
    PlanNodeDeclaration {
        node_key: [key; 16],
        kind: PlanNodeKind::AgentRole,
        binding_digest: [payload; 32],
        dependency_keys: Vec::new(),
        input_selectors_digest: [payload; 32],
        output_contract_digest: [payload; 32],
        policy_digest: [payload; 32],
        resource_ceiling_digest: [payload; 32],
        conditions: None,
    }
}

fn request(
    plan_id: Option<nlos_types::TaskPlanId>,
    nodes: Vec<PlanNodeDeclaration>,
    key: u8,
    applied_at_ms: u64,
) -> ApplyPlanRevisionRequest {
    ApplyPlanRevisionRequest {
        plan_id,
        nodes,
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        applied_at_ms,
    }
}

fn plan_node_rows(db_path: &std::path::Path) -> i64 {
    let raw = rusqlite::Connection::open(db_path).expect("open raw connection");
    raw.query_row("SELECT COUNT(*) FROM plan_nodes", [], |row| row.get(0))
        .expect("count plan_nodes")
}

/// The production-shaped consult wiring: the Task authority's
/// `answer_plan_declaration` mapped onto the plan-side consult outcome
/// (the 1:1 assembler mapping, carried inline exactly like the G3 and
/// scheduler batteries).
struct TaskDeclarationConsult<'a>(&'a SqliteTaskAuthority);

impl DeclarationAdmissionConsult for TaskDeclarationConsult<'_> {
    type Error = TaskStoreError;

    fn consult_plan_declaration(
        &self,
        projected_task_nodes: u64,
    ) -> Result<DeclarationAdmissionOutcome, TaskStoreError> {
        match self.0.answer_plan_declaration(projected_task_nodes) {
            Ok(()) => Ok(DeclarationAdmissionOutcome::Admits),
            Err(TaskStoreError::TaskNodeAdmissionDenied {
                profile_id,
                max_task_nodes,
                ..
            }) => Ok(DeclarationAdmissionOutcome::Denied {
                profile_id: profile_id.to_string(),
                max_task_nodes,
            }),
            Err(other) => Err(other),
        }
    }
}

/// A consult that denies every projection (tier-independent), counting
/// its calls so the replay test can prove the gate is bypassed.
struct DenyAfterFirst {
    calls: AtomicU64,
}

impl DenyAfterFirst {
    fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
        }
    }

    fn calls(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }
}

#[derive(Debug)]
struct ConsultBroken;

impl DeclarationAdmissionConsult for DenyAfterFirst {
    type Error = ConsultBroken;

    fn consult_plan_declaration(
        &self,
        _projected_task_nodes: u64,
    ) -> Result<DeclarationAdmissionOutcome, ConsultBroken> {
        if self.calls.fetch_add(1, Ordering::SeqCst) > 0 {
            return Ok(DeclarationAdmissionOutcome::Denied {
                profile_id: "task-apply-flip".to_string(),
                max_task_nodes: 0,
            });
        }
        Ok(DeclarationAdmissionOutcome::Admits)
    }
}

struct AlwaysBroken;

impl DeclarationAdmissionConsult for AlwaysBroken {
    type Error = ConsultBroken;

    fn consult_plan_declaration(
        &self,
        _projected_task_nodes: u64,
    ) -> Result<DeclarationAdmissionOutcome, ConsultBroken> {
        Err(ConsultBroken)
    }
}

/// The falsification face: a revision whose declared population exceeds
/// the Task tier's declared-TaskNode dimension is refused typed before
/// any write — no plan, no nodes, no receipt, and a later in-tier
/// revision applies cleanly to the untouched database.
#[test]
fn gated_apply_denies_population_over_tier_with_zero_durable_effects() {
    let root = Root::new("deny-over-tier");
    let plan = root.plan();
    let task = root.task();
    let consult = TaskDeclarationConsult(&task);

    let denied = plan
        .apply_plan_revision_with_admission(
            request(
                None,
                vec![node(0x0a, 1), node(0x0b, 2), node(0x0c, 3)],
                0x11,
                1_000,
            ),
            &consult,
        )
        .expect_err("three declared nodes exceed the two-node tier");
    assert!(matches!(
        denied,
        PlanStoreError::DeclarationAdmissionDenied {
            ref profile_id,
            projected_task_nodes: 3,
            max_task_nodes: 2,
        } if profile_id == "task-apply-node-cap-two"
    ));

    let db_path = root.0.join("plan.sqlite3");
    assert_eq!(
        plan_node_rows(&db_path),
        0,
        "no node row survives the denial"
    );
    let raw = rusqlite::Connection::open(&db_path).expect("raw open");
    let plans: i64 = raw
        .query_row("SELECT COUNT(*) FROM plans", [], |row| row.get(0))
        .expect("count plans");
    let revisions: i64 = raw
        .query_row("SELECT COUNT(*) FROM plan_revisions", [], |row| row.get(0))
        .expect("count revisions");
    assert_eq!(plans, 0, "denied apply leaves no plan head");
    assert_eq!(revisions, 0, "denied apply leaves no receipt");
    assert_eq!(
        raw.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .expect("integrity"),
        "ok"
    );

    // The untouched database still accepts an in-tier revision under a
    // fresh key.
    let admitted = plan
        .apply_plan_revision_with_admission(
            request(None, vec![node(0x0a, 1), node(0x0b, 2)], 0x12, 2_000),
            &consult,
        )
        .expect("in-tier revision applies after the denial");
    assert_eq!(admitted.receipt().revision, 1);
}

/// Positive face: an in-tier revision applies through the gated face
/// with the same receipt/chain semantics as the plain face, and an
/// idempotent replay answers from the durable receipt without
/// consulting again — even when the consult would now deny.
#[test]
fn gated_apply_admits_within_tier_and_replay_bypasses_the_gate() {
    let root = Root::new("admit-replay");
    let plan = root.plan();

    let counting = DenyAfterFirst::new();
    let applied = plan
        .apply_plan_revision_with_admission(
            request(None, vec![node(0x0a, 1), node(0x0b, 2)], 0x11, 1_000),
            &counting,
        )
        .expect("first consult admits");
    assert_eq!(counting.calls(), 1);
    let applied_receipt = applied.receipt();
    assert_eq!(applied_receipt.revision, 1);
    assert_eq!(applied_receipt.declared_node_count, 2);

    let replay = plan
        .apply_plan_revision_with_admission(
            request(None, vec![node(0x0a, 1), node(0x0b, 2)], 0x11, 1_000),
            &counting,
        )
        .expect("replay answers from the durable receipt");
    assert!(
        matches!(replay, PlanRevisionDecision::Replayed(ref receipt)
            if *receipt == applied_receipt),
        "replay is byte-equal and consult-free"
    );
    assert_eq!(
        counting.calls(),
        1,
        "the replay never consulted: the receipt is the authority"
    );

    let chain = plan
        .verify_revision_chain(applied_receipt.plan_id)
        .expect("chain verifies");
    assert_eq!(chain.revision_count, 1);
}

/// The dimension is store-wide lifetime rows: growth via new keys is
/// denied once the projection exceeds the tier, re-declaring existing
/// keys stays admitted, and the plan head never advances on a denial.
#[test]
fn gated_apply_accumulates_store_wide_and_denies_only_growth() {
    let root = Root::new("accumulate");
    let plan = root.plan();
    let task = root.task();
    let consult = TaskDeclarationConsult(&task);

    let first = plan
        .apply_plan_revision_with_admission(
            request(None, vec![node(0x0a, 1), node(0x0b, 2)], 0x11, 1_000),
            &consult,
        )
        .expect("revision 1 fills the tier exactly");
    let plan_id = first.receipt().plan_id;

    let denied = plan
        .apply_plan_revision_with_admission(
            request(
                Some(plan_id),
                vec![node(0x0a, 1), node(0x0b, 2), node(0x0c, 3)],
                0x12,
                2_000,
            ),
            &consult,
        )
        .expect_err("one new key over a full tier is growth");
    assert!(matches!(
        denied,
        PlanStoreError::DeclarationAdmissionDenied {
            projected_task_nodes: 3,
            max_task_nodes: 2,
            ..
        }
    ));
    assert_eq!(
        plan.inspect_plan(plan_id)
            .expect("inspect head")
            .expect("plan exists")
            .current_revision,
        1,
        "the head never advanced past the denial"
    );

    // Re-declaring the same total set adds no row: projection stays at
    // the store-wide count and the reshape applies.
    let reshape = plan
        .apply_plan_revision_with_admission(
            request(
                Some(plan_id),
                vec![node(0x0a, 9), node(0x0b, 9)],
                0x13,
                3_000,
            ),
            &consult,
        )
        .expect("reshape without growth applies");
    assert_eq!(reshape.receipt().revision, 2);
    assert_eq!(
        plan.inspect_declared_task_node_count()
            .expect("store-wide count"),
        2
    );
    assert_eq!(
        plan.verify_revision_chain(plan_id)
            .expect("chain verifies")
            .revision_count,
        2
    );
}

/// Cross-plan accumulation: the dimension counts every plan's rows, so
/// a second plan's first declaration is denied once the combined
/// projection exceeds the tier.
#[test]
fn gated_apply_denies_cross_plan_accumulation() {
    let root = Root::new("cross-plan");
    let plan = root.plan();
    let task = root.task();
    let consult = TaskDeclarationConsult(&task);

    plan.apply_plan_revision_with_admission(
        request(None, vec![node(0x0a, 1), node(0x0b, 2)], 0x11, 1_000),
        &consult,
    )
    .expect("plan one fills the tier");

    let denied = plan
        .apply_plan_revision_with_admission(
            request(None, vec![node(0x0a, 3)], 0x21, 2_000),
            &consult,
        )
        .expect_err("a second plan's new node grows the store-wide count");
    assert!(matches!(
        denied,
        PlanStoreError::DeclarationAdmissionDenied {
            projected_task_nodes: 3,
            max_task_nodes: 2,
            ..
        }
    ));
    assert_eq!(
        plan.inspect_declared_task_node_count()
            .expect("store-wide count"),
        2,
        "the denied plan contributed no row"
    );
}

/// A failed consult fails closed (ADR-0013: cannot verify ⇒ do not
/// commit): the typed `DeclarationConsultUnavailable`, zero durable
/// rows, and the same key applies once the consult works again.
#[test]
fn failed_consult_fails_closed_with_zero_durable_effects() {
    let root = Root::new("consult-failure");
    let plan = root.plan();

    let unavailable = plan
        .apply_plan_revision_with_admission(
            request(None, vec![node(0x0a, 1)], 0x11, 1_000),
            &AlwaysBroken,
        )
        .expect_err("a broken consult must fail the apply");
    assert!(matches!(
        unavailable,
        PlanStoreError::DeclarationConsultUnavailable
    ));
    assert_eq!(plan_node_rows(&root.0.join("plan.sqlite3")), 0);

    let recovered = DenyAfterFirst::new();
    plan.apply_plan_revision_with_admission(
        request(None, vec![node(0x0a, 1)], 0x11, 1_000),
        &recovered,
    )
    .expect("the same key applies once the consult answers");
    assert_eq!(recovered.calls(), 1);
}

/// Default apply without a consult is a typed refusal: zero durable
/// rows (W31-G §8.2.4 — cannot silent pass).
#[test]
fn default_apply_without_consult_fails_closed() {
    let root = Root::new("default-deny");
    let plan = root.plan();
    let denied = plan
        .apply_plan_revision(request(
            None,
            vec![node(0x0a, 1), node(0x0b, 2), node(0x0c, 3)],
            0x11,
            1_000,
        ))
        .expect_err("default apply without consult must fail closed");
    assert!(matches!(
        denied,
        PlanStoreError::DeclarationConsultUnavailable
    ));
    assert_eq!(plan_node_rows(&root.0.join("plan.sqlite3")), 0);
}

/// Production consult path still admits a no-growth reshape and denies
/// growth past the Task tier (the former "faces interleave" coverage,
/// now started from the gated face rather than a silent ungated apply).
#[test]
fn gated_apply_admits_reshape_and_denies_growth() {
    let root = Root::new("gated-reshape");
    let plan = root.plan();
    let task = root.task();
    let consult = TaskDeclarationConsult(&task);

    let first = plan
        .apply_plan_revision_with_admission(
            request(None, vec![node(0x0a, 1), node(0x0b, 2)], 0x11, 1_000),
            &consult,
        )
        .expect("first revision at the cap is admitted");
    let plan_id = first.receipt().plan_id;

    let reshaped = plan
        .apply_plan_revision_with_admission(
            request(
                Some(plan_id),
                vec![node(0x0a, 1), node(0x0b, 2)],
                0x12,
                2_000,
            ),
            &consult,
        )
        .expect("gated reshape of existing keys stays admitted");
    assert_eq!(reshaped.receipt().revision, 2);

    let denied = plan
        .apply_plan_revision_with_admission(
            request(
                Some(plan_id),
                vec![node(0x0a, 1), node(0x0b, 2), node(0x0c, 3)],
                0x13,
                3_000,
            ),
            &consult,
        )
        .expect_err("growth through the gated face is denied");
    assert!(matches!(
        denied,
        PlanStoreError::DeclarationAdmissionDenied {
            projected_task_nodes: 3,
            ..
        }
    ));
}

/// A consult that parks until released, reporting each projection it
/// was consulted with: the concurrency-test seam for the consult's
/// lock-freedom and the drift CAS.
struct ParkingConsult {
    entered: std::sync::mpsc::Sender<u64>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    calls: AtomicU64,
}

impl DeclarationAdmissionConsult for ParkingConsult {
    type Error = ConsultBroken;

    fn consult_plan_declaration(
        &self,
        projected_task_nodes: u64,
    ) -> Result<DeclarationAdmissionOutcome, ConsultBroken> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered
            .send(projected_task_nodes)
            .expect("signal consult entered");
        self.release
            .lock()
            .expect("release lock")
            .recv()
            .expect("release consult");
        Ok(DeclarationAdmissionOutcome::Admits)
    }
}

/// The admission consult runs outside the writer critical section
/// (deep-audit/29 #3): a parked consult does not hold the plan
/// authority's writer — another thread's revision applies and commits
/// while the consult is in flight — and the parked apply still lands
/// (its drifted snapshot restarts the consult round, see the next
/// test).
#[test]
fn slow_consult_does_not_hold_the_plan_writer() {
    let root = Root::new("slow-consult");
    let plan = root.plan();
    let (entered, entered_rx) = std::sync::mpsc::channel::<u64>();
    let (release_tx, release) = std::sync::mpsc::channel::<()>();
    let consult = ParkingConsult {
        entered,
        release: std::sync::Mutex::new(release),
        calls: AtomicU64::new(0),
    };

    std::thread::scope(|scope| {
        let applier = scope.spawn(|| {
            plan.apply_plan_revision_with_admission(
                request(None, vec![node(0x0a, 1)], 0x11, 1_000),
                &consult,
            )
        });
        let first_projection = entered_rx.recv().expect("consult entered");
        assert_eq!(first_projection, 1);

        // The writer is free: another plan's revision applies and
        // commits while the consult is parked.
        let (writer_done, writer_rx) = std::sync::mpsc::channel::<()>();
        let writer_done_tx = writer_done.clone();
        let plan_ref = &plan;
        scope.spawn(move || {
            plan_ref
                .apply_plan_revision_ungated(request(None, vec![node(0x2a, 2)], 0x21, 1_000))
                .expect("the parked consult must not hold the writer");
            writer_done_tx.send(()).expect("signal writer done");
        });
        writer_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the writer completes while the consult is parked");

        // Release; the drifted snapshot restarts the consult round once
        // (the concurrent revision grew the store-wide population).
        release_tx.send(()).expect("first release");
        let second_projection = entered_rx.recv().expect("second consult round");
        assert_eq!(second_projection, 2);
        release_tx.send(()).expect("second release");

        let decision = applier
            .join()
            .expect("applier thread")
            .expect("gated apply lands after the consult answers");
        assert!(matches!(decision, PlanRevisionDecision::Applied(_)));
    });
    assert_eq!(plan_node_rows(&root.0.join("plan.sqlite3")), 2);
}

/// The consult-window drift is closed by the CAS re-verification: the
/// apply re-consults with the fresh projection and only commits against
/// the number the Task tier actually admitted — growth-only is
/// preserved without holding the writer across the external callback
/// (deep-audit/29 #3).
#[test]
fn drifted_population_snapshot_is_reconsulted_with_fresh_projection() {
    let root = Root::new("drift-cas");
    let plan = root.plan();
    let (entered, entered_rx) = std::sync::mpsc::channel::<u64>();
    let (release_tx, release) = std::sync::mpsc::channel::<()>();
    let consult = ParkingConsult {
        entered,
        release: std::sync::Mutex::new(release),
        calls: AtomicU64::new(0),
    };

    std::thread::scope(|scope| {
        let applier = scope.spawn(|| {
            plan.apply_plan_revision_with_admission(
                request(None, vec![node(0x0a, 1)], 0x11, 1_000),
                &consult,
            )
        });

        // Round 1 consults the stale snapshot (1 node); a concurrent
        // writer grows the store to 2 behind the consult's back.
        assert_eq!(entered_rx.recv().expect("round 1"), 1);
        plan.apply_plan_revision_ungated(request(None, vec![node(0x2a, 2)], 0x21, 1_000))
            .expect("concurrent growth during the consult window");
        release_tx.send(()).expect("release round 1");

        // Round 2 consults the fresh projection and commits against it.
        assert_eq!(entered_rx.recv().expect("round 2"), 2);
        release_tx.send(()).expect("release round 2");

        let decision = applier
            .join()
            .expect("applier thread")
            .expect("apply commits after the drift restart");
        assert!(matches!(decision, PlanRevisionDecision::Applied(_)));
    });
    assert_eq!(consult.calls.load(Ordering::SeqCst), 2);
    assert_eq!(plan_node_rows(&root.0.join("plan.sqlite3")), 2);
}

/// W57-A #8: the declared-TaskNode admission dimension excludes terminal
/// tombstone rows, so the population is reusable — a full tier whose
/// node fails does not hold its declaration seat forever. A growth
/// revision denied while the tier is full admits after one node reaches
/// a terminal state (`MATERIALIZING → FAILED`, the W57-A #4 edge), the
/// projection drops by exactly the tombstone, and the physical row
/// stays (`plan_nodes_no_delete` — excluded, never deleted).
#[test]
fn terminal_tombstone_frees_the_declared_population_seat_for_new_keys() {
    let root = Root::new("tombstone-seat");
    let plan = root.plan();
    let task = root.task();
    let consult = TaskDeclarationConsult(&task);

    let first = plan
        .apply_plan_revision_with_admission(
            request(None, vec![node(0x0a, 1), node(0x0b, 2)], 0x11, 1_000),
            &consult,
        )
        .expect("revision 1 fills the tier exactly");
    let plan_id = first.receipt().plan_id;
    let node_a = plan
        .list_plan_nodes(plan_id)
        .expect("list nodes")
        .into_iter()
        .find(|record| record.node_key == [0x0a; 16])
        .expect("node a")
        .node_id;

    // Baseline: growth past the full tier is denied while both nodes
    // are alive.
    let denied = plan
        .apply_plan_revision_with_admission(
            request(
                Some(plan_id),
                vec![node(0x0a, 1), node(0x0b, 2), node(0x0c, 3)],
                0x12,
                2_000,
            ),
            &consult,
        )
        .expect_err("growth over the full tier is denied");
    assert!(matches!(
        denied,
        PlanStoreError::DeclarationAdmissionDenied {
            projected_task_nodes: 3,
            max_task_nodes: 2,
            ..
        }
    ));

    // Node a fails: DECLARED → (gate) MATERIALIZING → FAILED.
    plan.request_materialization(MaterializationRequest {
        plan_id,
        node_id: node_a,
        idempotency_key: IdempotencyKey::from_bytes([0x31; 16]),
        requested_at_ms: 2_500,
    })
    .expect("gate request drives a to WAITING_RESOURCE");
    plan.resolve_materialization(MaterializationResolution {
        request_key: IdempotencyKey::from_bytes([0x31; 16]),
        verdict: MaterializationAdmissionVerdict::Approved(MaterializationAdmission {
            profile_id: "task-apply-node-cap-two".to_string(),
            projected_task_nodes: 2,
            projected_active_working_set: 2,
        }),
        resolved_at_ms: 2_501,
    })
    .expect("gate approval drives a to MATERIALIZING");
    plan.record_node_transition(NodeTransitionRequest {
        plan_id,
        node_id: node_a,
        from_state: PlanNodeState::Materializing,
        to_state: PlanNodeState::Failed,
        expected_declared_revision: 1,
        idempotency_key: IdempotencyKey::from_bytes([0x32; 16]),
        transitioned_at_ms: 2_600,
    })
    .expect("MATERIALIZING → FAILED (W57-A #4 edge)");
    assert_eq!(
        plan.inspect_declared_task_node_count()
            .expect("declared count"),
        1,
        "the FAILED tombstone left the declared population"
    );

    // The freed seat admits the previously denied growth: the frozen
    // node re-declares bit-identically and the new key counts.
    let admitted = plan
        .apply_plan_revision_with_admission(
            request(
                Some(plan_id),
                vec![node(0x0a, 1), node(0x0b, 2), node(0x0c, 3)],
                0x13,
                3_000,
            ),
            &consult,
        )
        .expect("the tombstone's seat admits the new key");
    assert_eq!(admitted.receipt().revision, 2);
    assert_eq!(
        plan.inspect_declared_task_node_count()
            .expect("declared count"),
        2,
        "only live nodes occupy the dimension"
    );
    assert_eq!(
        plan_node_rows(&root.0.join("plan.sqlite3")),
        3,
        "the tombstone row is durable metadata, never deleted"
    );
}
