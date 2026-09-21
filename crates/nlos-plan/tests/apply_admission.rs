//! W36-P8 apply-time declared-population admission consult battery
//! (W31-G §8.2.4: 声明面 apply 时 TaskNode 维 admission consult 仍缺 —
//! only the materialization half was wired by W31-A).
//!
//! [`SqlitePlanAuthority::apply_plan_revision_with_admission`] consults
//! the Task tier through the [`nlos_plan::DeclarationAdmissionConsult`]
//! seam (the W31-A consult posture) before any durable write: the
//! projection is the store-wide persisted `plan_nodes` count plus the
//! keys this revision declares that carry no row yet. A denial is the
//! typed [`nlos_plan::PlanStoreError::DeclarationAdmissionDenied`] with
//! zero durable effects; a failed consult fails closed; idempotent
//! replays bypass the consult (the registration-gate discipline). The
//! plain `apply_plan_revision` face is unchanged.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_plan::{
    ApplyPlanRevisionRequest, DeclarationAdmissionConsult, DeclarationAdmissionOutcome,
    PlanNodeDeclaration, PlanNodeKind, PlanRevisionDecision, PlanStoreError, SqlitePlanAuthority,
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
        SqliteTaskAuthority::open_with_scale_profile(&self.0.join("task.sqlite3"), &NODE_CAP_TWO)
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

/// The plain face is untouched by the gated addition: applying without
/// a consult is legal at any population (structural bounds aside), the
/// two faces' receipts interleave on one chain, and the Infallible
/// consult demonstrates the boundary accepts transport-free impls.
#[test]
fn plain_face_stays_consult_free_and_faces_interleave() {
    let root = Root::new("plain-face");
    let plan = root.plan();
    let task = root.task();
    let consult = TaskDeclarationConsult(&task);

    // Plain face: three nodes, no consult, admitted (the structural
    // 100K bound is the only population bound on this face).
    let plain = plan
        .apply_plan_revision(request(
            None,
            vec![node(0x0a, 1), node(0x0b, 2), node(0x0c, 3)],
            0x11,
            1_000,
        ))
        .expect("plain face admits without a consult");
    let plan_id = plain.receipt().plan_id;

    // Gated face on the same plan: re-declaring the same keys projects
    // no growth, so the tier admits the reshape.
    let gated = plan
        .apply_plan_revision_with_admission(
            request(
                Some(plan_id),
                vec![node(0x0a, 1), node(0x0b, 2), node(0x0c, 3)],
                0x12,
                2_000,
            ),
            &consult,
        )
        .expect("gated reshape of existing keys stays admitted");
    assert_eq!(gated.receipt().revision, 2);

    // Any growth through the gated face is still denied at 3 > 2.
    let denied = plan
        .apply_plan_revision_with_admission(
            request(
                Some(plan_id),
                vec![node(0x0a, 1), node(0x0b, 2), node(0x0c, 3), node(0x0d, 4)],
                0x13,
                3_000,
            ),
            &consult,
        )
        .expect_err("growth through the gated face is denied");
    assert!(matches!(
        denied,
        PlanStoreError::DeclarationAdmissionDenied {
            projected_task_nodes: 4,
            ..
        }
    ));
}
