//! W36-P8 & C-SCHED-002 scheduler scale probe and bounded-select equivalence
//! verification (`plan_nodes_state_order` covering index + zero Temp B-Tree
//! merged cursor path).
//!
//! ```sh
//! cargo test -p nlos-plan --test scheduler_scale_probe -- --nocapture
//! cargo test --release -p nlos-plan --test scheduler_scale_probe -- --include-ignored --nocapture
//! ```

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nlos_plan::{
    ACTIVE_BAND_STATES, AWAITING_MATERIALIZATION_STATES, AdmissionConsult, AdmissionConsultOutcome,
    ApplyPlanRevisionRequest, BOUNDED_AWAITING_CURSOR_SQL, BOUNDED_BAND_SEATS_SQL,
    MaterializationAdmission, MaterializationAdmissionVerdict, MaterializationRequest,
    MaterializationResolution, MaterializationScheduler, NodePinRequest, NodeResidencyTier,
    NodeTransitionRequest, PlanNodeDeclaration, PlanNodeKind, PlanNodeState, PlanStoreError,
    ResidencyTransitionRequest, SelectionKind, SelectionSkipReason, SqlitePlanAuthority,
};
use nlos_types::{IdempotencyKey, TaskNodeId, TaskPlanId};
use rusqlite::{Connection, params};

const SMOKE_COUNT: u64 = 48;
const SCALE_COUNT: u64 = 10_000;
/// Select p95 must stay well under this ceiling (debug/test, single
/// platform). The probe records wall time; the only assertion is the
/// absolute ceiling plus "every ready node is selected".
const SELECT_CEILING: Duration = Duration::from_secs(5);
const SCALE_WARMUP_ROUNDS: usize = 2;
const SCALE_SAMPLE_ROUNDS: usize = 25;

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Root(std::path::PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "nlos-plan-sched-scale-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create fixture directory");
        Self(path)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn node(index: u64) -> PlanNodeDeclaration {
    let mut key = [0_u8; 16];
    key[8..].copy_from_slice(&index.to_be_bytes());
    PlanNodeDeclaration {
        node_key: key,
        kind: PlanNodeKind::AgentRole,
        binding_digest: [0x11; 32],
        dependency_keys: Vec::new(),
        input_selectors_digest: [0x22; 32],
        output_contract_digest: [0x33; 32],
        policy_digest: [0x44; 32],
        resource_ceiling_digest: [0x55; 32],
        conditions: None,
    }
}

fn tagged_node(tag: u8, payload: u8, deps: &[u8]) -> PlanNodeDeclaration {
    PlanNodeDeclaration {
        node_key: [tag; 16],
        kind: PlanNodeKind::AgentRole,
        binding_digest: [payload; 32],
        dependency_keys: deps.iter().map(|dep| [*dep; 16]).collect(),
        input_selectors_digest: [payload; 32],
        output_contract_digest: [payload; 32],
        policy_digest: [payload; 32],
        resource_ceiling_digest: [payload; 32],
        conditions: None,
    }
}

fn indexed_node_with_dep(index: u64, dep: Option<u64>) -> PlanNodeDeclaration {
    let mut declaration = node(index);
    if let Some(dep_idx) = dep {
        let mut dep_key = [0_u8; 16];
        dep_key[8..].copy_from_slice(&dep_idx.to_be_bytes());
        declaration.dependency_keys = vec![dep_key];
    }
    declaration
}

fn idempotency_key_u64(domain: u64, seq: u64) -> IdempotencyKey {
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&domain.to_be_bytes());
    bytes[8..].copy_from_slice(&seq.to_be_bytes());
    IdempotencyKey::from_bytes(bytes)
}

struct AdmitAll;

impl AdmissionConsult for AdmitAll {
    type Error = Infallible;

    fn consult_materialization(
        &self,
        other_declared_task_nodes: u64,
    ) -> Result<AdmissionConsultOutcome, Infallible> {
        Ok(AdmissionConsultOutcome::Admitted(
            MaterializationAdmission {
                profile_id: "sched-scale-admit".to_string(),
                projected_task_nodes: other_declared_task_nodes + 1,
                projected_active_working_set: 1,
            },
        ))
    }
}

fn assert_durability_and_v9(db_path: &Path) {
    let raw = Connection::open(db_path).expect("open raw connection");
    let journal_mode: String = raw
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .expect("read journal_mode");
    let synchronous: i64 = raw
        .pragma_query_value(None, "synchronous", |row| row.get(0))
        .expect("read synchronous");
    let user_version: i64 = raw
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read user_version");
    let integrity: String = raw
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity_check");
    assert!(
        journal_mode.eq_ignore_ascii_case("wal"),
        "expected WAL journal_mode, got {journal_mode}"
    );
    assert_eq!(synchronous, 2, "expected synchronous=FULL (2)");
    assert_eq!(user_version, 9, "SCHEMA_VERSION must remain 9");
    assert_eq!(integrity, "ok", "integrity_check must pass");
}

fn explain_details(raw: &Connection, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Vec<String> {
    let explain_sql = format!("EXPLAIN QUERY PLAN {sql}");
    let mut statement = raw
        .prepare(&explain_sql)
        .expect("prepare EXPLAIN QUERY PLAN");
    let rows = statement
        .query_map(params, |row| row.get::<_, String>(3))
        .expect("execute EXPLAIN QUERY PLAN");
    rows.collect::<Result<Vec<_>, _>>()
        .expect("collect EXPLAIN QUERY PLAN details")
}

fn run_scheduler_scale_cell(count: u64, print: bool) {
    let root = Root::new(&format!("n{count}"));
    let authority = SqlitePlanAuthority::open(root.0.join("plan.sqlite3")).expect("open plan");
    let apply_started = Instant::now();
    let plan_id = authority
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: None,
            nodes: (0..count).map(node).collect(),
            idempotency_key: IdempotencyKey::from_bytes([0x51; 16]),
            applied_at_ms: 1_000,
        })
        .expect("apply independent nodes")
        .receipt()
        .plan_id;
    let apply_elapsed = apply_started.elapsed();

    let mut scheduler = MaterializationScheduler::new(count);
    let select_started = Instant::now();
    let report = scheduler
        .select(&authority, plan_id)
        .expect("select at scale");
    let select_elapsed = select_started.elapsed();
    assert_eq!(report.selections.len() as u64, count);
    assert_eq!(report.skips, [] as [nlos_plan::SkipEntry; 0]);
    assert!(
        select_elapsed < SELECT_CEILING,
        "select {count} nodes took {select_elapsed:?} (>= {SELECT_CEILING:?})"
    );

    let drive_started = Instant::now();
    let summary = scheduler
        .drive(&authority, &report, &AdmitAll, 2_000)
        .expect("drive at scale");
    let drive_elapsed = drive_started.elapsed();
    assert_eq!(summary.selected, count);
    assert_eq!(summary.approved, count);
    assert_eq!(summary.rejected, 0);
    assert_eq!(summary.skipped, 0);

    if print {
        eprintln!(
            "W36-P8 scheduler scale probe (single platform): \
             nodes={count} apply_total={apply_elapsed:?} \
             select_total={select_elapsed:?} drive_total={drive_elapsed:?} \
             selected={} approved={}",
            summary.selected, summary.approved
        );
    }
}

#[test]
fn scheduler_scale_smoke_selects_and_drives_ready_fifo() {
    run_scheduler_scale_cell(SMOKE_COUNT, false);
}

#[test]
#[ignore = "explicit W36-P8 scheduler 10K scale probe (B-PLAN-001 §11.5)"]
fn ten_thousand_nodes_scheduler_select_and_drive() {
    run_scheduler_scale_cell(SCALE_COUNT, true);
}

/// Verifies `EXPLAIN QUERY PLAN` for the production bounded-select SQL
/// queries: every awaiting state cursor walks `plan_nodes_state_order`
/// via `SEARCH plan_nodes USING INDEX plan_nodes_state_order` with zero
/// `SCAN plan_nodes` and zero `TEMP B-TREE FOR ORDER BY`, and the active
/// band count uses the covering index.
#[test]
fn explain_query_plan_uses_covering_index_without_temp_btree() {
    let root = Root::new("eqp");
    let db_path = root.0.join("plan.sqlite3");
    let authority = SqlitePlanAuthority::open(&db_path).expect("open plan");
    let plan_id = authority
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: None,
            nodes: (0..128_u64).map(node).collect(),
            idempotency_key: idempotency_key_u64(1, 1),
            applied_at_ms: 1_000,
        })
        .expect("apply revision")
        .receipt()
        .plan_id;
    drop(authority);

    assert_durability_and_v9(&db_path);
    let raw = Connection::open(&db_path).expect("open raw");
    let plan_bytes = plan_id.as_bytes().as_slice();

    for state_code in AWAITING_MATERIALIZATION_STATES {
        let details = explain_details(
            &raw,
            BOUNDED_AWAITING_CURSOR_SQL,
            &[&plan_bytes, &state_code],
        );
        assert!(
            details
                .iter()
                .any(|d| d.contains("USING INDEX plan_nodes_state_order")),
            "state={state_code}: expected USING INDEX plan_nodes_state_order, got {details:?}"
        );
        assert!(
            !details
                .iter()
                .any(|d| d.contains("SCAN plan_nodes") || d.contains("SCAN TABLE plan_nodes")),
            "state={state_code}: unexpected full table scan in {details:?}"
        );
        assert!(
            !details.iter().any(|d| d.contains("TEMP B-TREE")),
            "state={state_code}: unexpected TEMP B-TREE in {details:?}"
        );
        if state_code == AWAITING_MATERIALIZATION_STATES[0] {
            eprintln!(
                "EQP cursor (node_state={state_code}): {}",
                details.join(" | ")
            );
        }
    }

    let band_details = explain_details(&raw, BOUNDED_BAND_SEATS_SQL, &[&plan_bytes]);
    assert!(
        band_details
            .iter()
            .any(|d| d.contains("plan_nodes_state_order") && d.contains("USING")),
        "expected plan_nodes_state_order index on band seats query, got {band_details:?}"
    );
    assert!(
        !band_details
            .iter()
            .any(|d| d.contains("SCAN plan_nodes") || d.contains("SCAN TABLE plan_nodes")),
        "unexpected full table scan in band query: {band_details:?}"
    );
    assert!(
        !band_details.iter().any(|d| d.contains("TEMP B-TREE")),
        "unexpected TEMP B-TREE in band query: {band_details:?}"
    );
    eprintln!("EQP band_seats: {}", band_details.join(" | "));

    // Contrast check: confirms why a single IN-list query is NOT used directly,
    // and captures the legacy select() full-plan query plan.
    let in_list_sql = "SELECT task_node_id, first_declared_at_ms, declared_revision
        FROM plan_nodes
        WHERE plan_id = ?1 AND node_state IN (1, 2, 3, 4, 5, 10)
        ORDER BY first_declared_at_ms, task_node_id";
    let in_details = explain_details(&raw, in_list_sql, &[&plan_bytes]);
    eprintln!("EQP single-IN contrast: {}", in_details.join(" | "));
    assert!(
        in_details.iter().any(|d| d.contains("TEMP B-TREE")),
        "expected IN-list ORDER BY contrast to need TEMP B-TREE, got {in_details:?}"
    );

    let legacy_sql = "SELECT plan_id, task_node_id, node_key, node_kind, declared_revision,
            node_digest, node_state, transition_count,
            residency_tier, residency_transition_count,
            first_declared_at_ms, updated_at_ms
        FROM plan_nodes WHERE plan_id = ?1
        ORDER BY first_declared_at_ms, task_node_id";
    let legacy_details = explain_details(&raw, legacy_sql, &[&plan_bytes]);
    eprintln!("EQP legacy select: {}", legacy_details.join(" | "));
}

fn advance_node(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    edges: &[(PlanNodeState, PlanNodeState)],
    revision: u64,
    key_seq: &mut u64,
    at_ms: u64,
) {
    for &(from, to) in edges {
        *key_seq += 1;
        authority
            .record_node_transition(NodeTransitionRequest {
                plan_id,
                node_id,
                from_state: from,
                to_state: to,
                expected_declared_revision: revision,
                idempotency_key: idempotency_key_u64(0xAA, *key_seq),
                transitioned_at_ms: at_ms,
            })
            .expect("legal node transition");
    }
}

fn gate_approve_node(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    key_seq: &mut u64,
    at_ms: u64,
) {
    *key_seq += 1;
    let key = idempotency_key_u64(0xBB, *key_seq);
    authority
        .request_materialization(MaterializationRequest {
            plan_id,
            node_id,
            idempotency_key: key,
            requested_at_ms: at_ms,
        })
        .expect("request materialization");
    authority
        .resolve_materialization(MaterializationResolution {
            request_key: key,
            verdict: MaterializationAdmissionVerdict::Approved(MaterializationAdmission {
                profile_id: "diff-equiv".to_string(),
                projected_task_nodes: 32,
                projected_active_working_set: 1,
            }),
            resolved_at_ms: at_ms + 1,
        })
        .expect("approve materialization");
}

fn pin_node_hot(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    revision: u64,
    key_seq: &mut u64,
    at_ms: u64,
) {
    for (from, to) in [
        (NodeResidencyTier::MetadataOnly, NodeResidencyTier::Cold),
        (NodeResidencyTier::Cold, NodeResidencyTier::Warm),
        (NodeResidencyTier::Warm, NodeResidencyTier::Hot),
    ] {
        *key_seq += 1;
        authority
            .record_residency_transition(ResidencyTransitionRequest {
                plan_id,
                node_id,
                from_tier: from,
                to_tier: to,
                expected_declared_revision: revision,
                idempotency_key: idempotency_key_u64(0xCC, *key_seq),
                transitioned_at_ms: at_ms,
            })
            .expect("raise residency tier");
    }
    *key_seq += 1;
    authority
        .record_node_pin(NodePinRequest {
            plan_id,
            node_id,
            expected_declared_revision: revision,
            idempotency_key: idempotency_key_u64(0xDD, *key_seq),
            transitioned_at_ms: at_ms + 1,
        })
        .expect("pin node");
}

fn gate_and_advance(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    edges: &[(PlanNodeState, PlanNodeState)],
    key_seq: &mut u64,
    at_ms: u64,
) {
    gate_approve_node(authority, plan_id, node_id, key_seq, at_ms);
    advance_node(authority, plan_id, node_id, edges, 1, key_seq, at_ms + 5);
}

fn rev1_declarations() -> Vec<PlanNodeDeclaration> {
    vec![
        tagged_node(0x01, 1, &[]),     // -> Completed (11)
        tagged_node(0x02, 2, &[]),     // -> Declared (1), ready
        tagged_node(0x03, 3, &[0x01]), // dep 0x01 Completed -> Eligible (3), ready
        tagged_node(0x04, 4, &[0x02]), // dep 0x02 Declared -> BlockedDependency (2)
        tagged_node(0x05, 5, &[]),     // -> WaitingAuthorization (4), ready
        tagged_node(0x06, 6, &[]),     // -> WaitingResource (5) + PENDING round
        tagged_node(0x07, 7, &[]),     // -> Materializing (6) + PINNED
        tagged_node(0x08, 8, &[]),     // -> Active (7)
        tagged_node(0x09, 9, &[]),     // -> Checkpointed (8) + PINNED
        tagged_node(0x0a, 10, &[]),    // -> Evicted (9)
        tagged_node(0x0b, 11, &[]),    // -> Rehydrating (10) (awaiting + band seat)
        tagged_node(0x0c, 12, &[]),    // -> Rehydrating (10) + PENDING round
        tagged_node(0x0d, 13, &[]),    // -> Failed (12)
        tagged_node(0x0e, 14, &[]),    // -> Cancelled (13)
        tagged_node(0x0f, 15, &[]),    // -> PENDING round then Cancelled (13)
    ]
}

fn build_rev1_thirteen_state_topology(
    authority: &SqlitePlanAuthority,
) -> (TaskPlanId, Vec<PlanNodeDeclaration>, u64) {
    let rev1_nodes = rev1_declarations();
    let plan_id = authority
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: None,
            nodes: rev1_nodes.clone(),
            idempotency_key: idempotency_key_u64(1, 1),
            applied_at_ms: 1_000,
        })
        .expect("apply revision 1")
        .receipt()
        .plan_id;

    let by_key: HashMap<[u8; 16], TaskNodeId> = authority
        .list_plan_nodes(plan_id)
        .expect("list rev1 nodes")
        .into_iter()
        .map(|record| (record.node_key, record.node_id))
        .collect();
    let id = |tag: u8| by_key[&[tag; 16]];
    let mut seq = 0_u64;

    gate_and_advance(
        authority,
        plan_id,
        id(0x01),
        &[
            (PlanNodeState::Materializing, PlanNodeState::Active),
            (PlanNodeState::Active, PlanNodeState::Completed),
        ],
        &mut seq,
        1_100,
    );
    gate_approve_node(authority, plan_id, id(0x07), &mut seq, 1_120);
    pin_node_hot(authority, plan_id, id(0x07), 1, &mut seq, 1_125);
    gate_and_advance(
        authority,
        plan_id,
        id(0x08),
        &[(PlanNodeState::Materializing, PlanNodeState::Active)],
        &mut seq,
        1_130,
    );
    gate_and_advance(
        authority,
        plan_id,
        id(0x09),
        &[
            (PlanNodeState::Materializing, PlanNodeState::Active),
            (PlanNodeState::Active, PlanNodeState::Checkpointed),
        ],
        &mut seq,
        1_140,
    );
    pin_node_hot(authority, plan_id, id(0x09), 1, &mut seq, 1_148);
    gate_and_advance(
        authority,
        plan_id,
        id(0x0a),
        &[
            (PlanNodeState::Materializing, PlanNodeState::Active),
            (PlanNodeState::Active, PlanNodeState::Checkpointed),
            (PlanNodeState::Checkpointed, PlanNodeState::Evicted),
        ],
        &mut seq,
        1_150,
    );
    for (tag, at_ms) in [(0x0b_u8, 1_160_u64), (0x0c, 1_170)] {
        gate_and_advance(
            authority,
            plan_id,
            id(tag),
            &[
                (PlanNodeState::Materializing, PlanNodeState::Active),
                (PlanNodeState::Active, PlanNodeState::Checkpointed),
                (PlanNodeState::Checkpointed, PlanNodeState::Evicted),
                (PlanNodeState::Evicted, PlanNodeState::Rehydrating),
            ],
            &mut seq,
            at_ms,
        );
    }
    gate_and_advance(
        authority,
        plan_id,
        id(0x0d),
        &[(PlanNodeState::Materializing, PlanNodeState::Failed)],
        &mut seq,
        1_180,
    );
    (plan_id, rev1_nodes, seq)
}

fn extend_rev2_thirteen_state_topology(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    mut rev2_nodes: Vec<PlanNodeDeclaration>,
    mut seq: u64,
) {
    rev2_nodes.extend([
        tagged_node(0x10, 16, &[0x01]), // Declared at t=2_000, ready + PINNED
        tagged_node(0x11, 17, &[]),     // WaitingResource + PENDING at t=2_000
        tagged_node(0x12, 18, &[0x08]), // depends on Active 0x08 -> not ready
        tagged_node(0x13, 19, &[0x0b]), // depends on Rehydrating 0x0b -> not ready
        tagged_node(0x14, 20, &[]),     // Declared at t=2_000, ready
    ]);
    authority
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: Some(plan_id),
            nodes: rev2_nodes,
            idempotency_key: idempotency_key_u64(1, 2),
            applied_at_ms: 2_000,
        })
        .expect("apply revision 2");

    let by_key: HashMap<[u8; 16], TaskNodeId> = authority
        .list_plan_nodes(plan_id)
        .expect("list rev2 nodes")
        .into_iter()
        .map(|record| (record.node_key, record.node_id))
        .collect();
    let id = |tag: u8| by_key[&[tag; 16]];

    advance_node(
        authority,
        plan_id,
        id(0x03),
        &[(PlanNodeState::Declared, PlanNodeState::Eligible)],
        2,
        &mut seq,
        2_100,
    );
    let blocked_err = authority
        .request_materialization(MaterializationRequest {
            plan_id,
            node_id: id(0x04),
            idempotency_key: idempotency_key_u64(0xEE, 1),
            requested_at_ms: 2_110,
        })
        .expect_err("0x04 depends on Declared 0x02");
    assert!(matches!(
        blocked_err,
        PlanStoreError::DependenciesNotReady { .. }
    ));

    advance_node(
        authority,
        plan_id,
        id(0x05),
        &[
            (PlanNodeState::Declared, PlanNodeState::Eligible),
            (PlanNodeState::Eligible, PlanNodeState::WaitingAuthorization),
        ],
        2,
        &mut seq,
        2_120,
    );
    for (tag, at_ms) in [
        (0x06_u8, 2_130_u64),
        (0x0c, 2_135),
        (0x0f, 2_145),
        (0x11, 2_170),
    ] {
        seq += 1;
        authority
            .request_materialization(MaterializationRequest {
                plan_id,
                node_id: id(tag),
                idempotency_key: idempotency_key_u64(0xBB, seq),
                requested_at_ms: at_ms,
            })
            .expect("open pending round");
    }
    advance_node(
        authority,
        plan_id,
        id(0x0e),
        &[(PlanNodeState::Declared, PlanNodeState::Cancelled)],
        2,
        &mut seq,
        2_140,
    );
    advance_node(
        authority,
        plan_id,
        id(0x0f),
        &[(PlanNodeState::WaitingResource, PlanNodeState::Cancelled)],
        2,
        &mut seq,
        2_150,
    );
    pin_node_hot(authority, plan_id, id(0x10), 2, &mut seq, 2_160);
}

/// Builds a rich multi-revision DAG spanning all 13 `PlanNodeState`s,
/// pending gate rounds (early, late, `Rehydrating`, and cancelled-while-pending),
/// DAG blocks, and `PINNED` residency overlays, then asserts `select_bounded`
/// matches `select` across all window settings.
#[test]
fn differential_equivalence_across_complex_plan_topologies() {
    let root = Root::new("diff-equiv");
    let db_path = root.0.join("plan.sqlite3");
    let authority = SqlitePlanAuthority::open(&db_path).expect("open plan");
    let (plan_id, rev1_nodes, seq) = build_rev1_thirteen_state_topology(&authority);
    extend_rev2_thirteen_state_topology(&authority, plan_id, rev1_nodes, seq);

    let states_present: HashSet<PlanNodeState> = authority
        .list_plan_nodes(plan_id)
        .expect("list final topology")
        .into_iter()
        .map(|record| record.state)
        .collect();
    assert_eq!(
        states_present.len(),
        13,
        "all 13 states must be represented"
    );

    // Band seats: 0x07, 0x08, 0x09, 0x0b, 0x0c => 5.
    // Pending seats: 0x06, 0x0c, 0x0f, 0x11 => 4. Total seats_in_use = 9.
    for window in [0_u64, 1, 5, 8, 9, 10, 11, 12, 13, 15, 64] {
        let scheduler = MaterializationScheduler::new(window);
        let full = scheduler.select(&authority, plan_id).expect("full select");
        let bounded = scheduler
            .select_bounded(&authority, plan_id)
            .expect("bounded select");
        assert_eq!(bounded.plan_id, full.plan_id, "window={window}");
        assert_eq!(bounded.window, full.window, "window={window}");
        assert_eq!(bounded.seats_in_use, 9, "window={window}");
        assert_eq!(bounded.seats_in_use, full.seats_in_use, "window={window}");
        assert_eq!(
            bounded.selections, full.selections,
            "selections diverged at window={window}"
        );
        for skip in &bounded.skips {
            if matches!(
                skip.reason,
                SelectionSkipReason::DependenciesNotReady { .. }
            ) {
                assert!(
                    full.skips.contains(skip),
                    "unexpected DependenciesNotReady skip at window={window}: {skip:?}"
                );
            }
        }
    }

    let unknown = TaskPlanId::from_bytes([0xfe; 16]);
    assert!(matches!(
        MaterializationScheduler::new(4).select_bounded(&authority, unknown),
        Err(PlanStoreError::PlanNotFound(missing)) if missing == unknown
    ));

    let mut bounded_sched = MaterializationScheduler::new(11);
    let summary = bounded_sched
        .run_pass_bounded(&authority, plan_id, &AdmitAll, 3_000)
        .expect("run_pass_bounded");
    assert_eq!(summary.selected, 5, "3 adoptable pending + 2 new");
    assert_eq!(summary.approved, 5);
    assert_eq!(summary.rejected, 0);
}

/// Differential equivalence on a plan with zero ready nodes (100 completed +
/// 50 blocked dependents + 0 pending) and a zero-ready fast exit.
#[test]
fn differential_equivalence_all_blocked_and_completed() {
    let root = Root::new("all-blocked");
    let authority = SqlitePlanAuthority::open(root.0.join("plan.sqlite3")).expect("open plan");
    let mut declarations = Vec::with_capacity(151);
    declarations.push(node(0));
    for i in 1..=100_u64 {
        declarations.push(node(i));
    }
    for i in 101..=150_u64 {
        declarations.push(indexed_node_with_dep(i, Some(0)));
    }
    let plan_id = authority
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: None,
            nodes: declarations,
            idempotency_key: idempotency_key_u64(2, 1),
            applied_at_ms: 1_000,
        })
        .expect("apply revision")
        .receipt()
        .plan_id;

    let mut seq = 0_u64;
    let nodes = authority.list_plan_nodes(plan_id).expect("list nodes");
    let id_0 = nodes
        .iter()
        .find(|n| n.node_key == node(0).node_key)
        .expect("node 0")
        .node_id;
    gate_approve_node(&authority, plan_id, id_0, &mut seq, 1_100);
    advance_node(
        &authority,
        plan_id,
        id_0,
        &[(PlanNodeState::Materializing, PlanNodeState::Active)],
        1,
        &mut seq,
        1_110,
    );

    let db_path = root.0.join("plan.sqlite3");
    drop(authority);
    {
        let raw = Connection::open(&db_path).expect("raw setup");
        let tx = raw.unchecked_transaction().expect("tx");
        for i in 1..=100_u64 {
            let mut key = [0_u8; 16];
            key[8..].copy_from_slice(&i.to_be_bytes());
            tx.execute(
                "UPDATE plan_nodes SET node_state = 11, transition_count = 3
                 WHERE plan_id = ?1 AND node_key = ?2",
                params![plan_id.as_bytes().as_slice(), key.as_slice()],
            )
            .expect("mark completed");
        }
        tx.commit().expect("commit");
    }

    let authority = SqlitePlanAuthority::open(&db_path).expect("reopen");
    for window in [0_u64, 1, 10] {
        let sched = MaterializationScheduler::new(window);
        let full = sched.select(&authority, plan_id).expect("select");
        let bounded = sched
            .select_bounded(&authority, plan_id)
            .expect("select_bounded");
        assert_eq!(bounded.selections, []);
        assert_eq!(bounded.selections, full.selections);
        assert_eq!(bounded.seats_in_use, full.seats_in_use);
    }
}

/// Fast non-ignored scale regression: 5,000 completed tail nodes + 32
/// awaiting nodes (with `PINNED` overlay and `window = 8`). Verifies
/// `select_bounded` picks exact 8 items, stops early without enumerating
/// the 5,000-node tail, and matches `select().selections`.
#[test]
fn fast_scale_regression_five_thousand_tail_bounded_early_stop() {
    let root = Root::new("tail-5k");
    let db_path = root.0.join("plan.sqlite3");
    let authority = SqlitePlanAuthority::open(&db_path).expect("open plan");
    let total = 5_032_u64;
    let plan_id = authority
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: None,
            nodes: (0..total).map(node).collect(),
            idempotency_key: idempotency_key_u64(3, 1),
            applied_at_ms: 1_000,
        })
        .expect("apply 5032 nodes")
        .receipt()
        .plan_id;

    let mut seq = 0_u64;
    // Pin two awaiting nodes via the public API.
    {
        let raw = Connection::open(&db_path).expect("raw lookup");
        for target in [5_000_u64, 5_005] {
            let target_key = node(target).node_key;
            let node_bytes: Vec<u8> = raw
                .query_row(
                    "SELECT task_node_id FROM plan_nodes WHERE plan_id = ?1 AND node_key = ?2",
                    params![plan_id.as_bytes().as_slice(), target_key.as_slice()],
                    |row| row.get(0),
                )
                .expect("find target id");
            let node_id = TaskNodeId::from_bytes(node_bytes.try_into().expect("16 bytes"));
            pin_node_hot(&authority, plan_id, node_id, 1, &mut seq, 1_500);
        }
    }
    drop(authority);

    // Move first 5,000 nodes to Completed (11) in one durable transaction.
    {
        let raw = Connection::open(&db_path).expect("raw setup");
        let mut cutoff = [0_u8; 16];
        cutoff[8..].copy_from_slice(&5_000_u64.to_be_bytes());
        raw.execute(
            "UPDATE plan_nodes SET node_state = 11, transition_count = 3
             WHERE plan_id = ?1 AND node_key < ?2",
            params![plan_id.as_bytes().as_slice(), cutoff.as_slice()],
        )
        .expect("complete 5000 tail nodes");
    }

    let authority = SqlitePlanAuthority::open(&db_path).expect("reopen");
    assert_durability_and_v9(&db_path);

    let mut scheduler = MaterializationScheduler::new(8);
    let full = scheduler.select(&authority, plan_id).expect("full select");
    let bounded = scheduler
        .select_bounded(&authority, plan_id)
        .expect("bounded select");
    assert_eq!(bounded.seats_in_use, 0);
    assert_eq!(bounded.selections.len(), 8);
    assert_eq!(bounded.selections, full.selections);
    assert!(
        bounded.skips.is_empty(),
        "early stop after filling window=8 must not emit tail skips, got {:?}",
        bounded.skips
    );
    assert!(
        full.skips.len() == 5_024,
        "full select enumerates all 5,000 completed + 24 exhausted skips"
    );

    let summary = scheduler
        .run_pass_bounded(&authority, plan_id, &AdmitAll, 2_000)
        .expect("run_pass_bounded");
    assert_eq!(summary.selected, 8);
    assert_eq!(summary.approved, 8);
    assert_eq!(summary.skipped, 0);

    // Next pass with window=16 sees 8 band seats in use and selects the next 8.
    scheduler.set_window(16);
    let next_full = scheduler.select(&authority, plan_id).expect("next full");
    let next_bounded = scheduler
        .select_bounded(&authority, plan_id)
        .expect("next bounded");
    assert_eq!(next_bounded.seats_in_use, 8);
    assert_eq!(next_bounded.selections.len(), 8);
    assert_eq!(next_bounded.selections, next_full.selections);
    assert!(
        next_bounded
            .selections
            .iter()
            .all(|entry| matches!(entry.kind, SelectionKind::NewGateRound))
    );
}

fn percentile_us(sorted_us: &[u128], pct: usize) -> u128 {
    let idx = (sorted_us.len() * pct).div_ceil(100).saturating_sub(1);
    sorted_us[idx.min(sorted_us.len() - 1)]
}

fn apply_bulk_fixture_states(
    db_path: &Path,
    plan_id: TaskPlanId,
    node_count: u64,
    terminal_count: u64,
    active_band: u64,
) {
    let raw = Connection::open(db_path).expect("open raw setup");
    raw.pragma_update(None, "journal_mode", "WAL").expect("wal");
    raw.pragma_update(None, "synchronous", "FULL")
        .expect("full");
    let tx = raw.unchecked_transaction().expect("begin tx");

    let mut terminal_end_key = [0_u8; 16];
    terminal_end_key[8..].copy_from_slice(&terminal_count.to_be_bytes());
    tx.execute(
        "UPDATE plan_nodes SET node_state = 11, transition_count = 3
         WHERE plan_id = ?1 AND node_key < ?2",
        params![plan_id.as_bytes().as_slice(), terminal_end_key.as_slice()],
    )
    .expect("mark terminal tail");

    let mut active_stmt = tx
        .prepare(
            "UPDATE plan_nodes SET node_state = ?3, transition_count = 2
             WHERE plan_id = ?1 AND node_key = ?2",
        )
        .expect("prepare active update");
    for offset in 0..active_band {
        let idx = terminal_count + offset;
        let state = ACTIVE_BAND_STATES[usize::try_from(offset % 4).expect("fits")];
        let mut key = [0_u8; 16];
        key[8..].copy_from_slice(&idx.to_be_bytes());
        active_stmt
            .execute(params![
                plan_id.as_bytes().as_slice(),
                key.as_slice(),
                state
            ])
            .expect("update active band state");
    }
    drop(active_stmt);

    let mut await_stmt = tx
        .prepare(
            "UPDATE plan_nodes SET node_state = ?3, transition_count = 1
             WHERE plan_id = ?1 AND node_key = ?2",
        )
        .expect("prepare await update");
    let awaiting_start = terminal_count + active_band;
    for idx in awaiting_start..node_count {
        let state = if idx.is_multiple_of(8) {
            2_i64
        } else {
            match idx % 3 {
                0 => 1_i64,
                1 => 3_i64,
                _ => 5_i64,
            }
        };
        let mut key = [0_u8; 16];
        key[8..].copy_from_slice(&idx.to_be_bytes());
        await_stmt
            .execute(params![
                plan_id.as_bytes().as_slice(),
                key.as_slice(),
                state
            ])
            .expect("update awaiting state");
    }
    drop(await_stmt);
    tx.commit().expect("commit fixture states");
}

fn pin_and_request_fixture_samples(
    db_path: &Path,
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_count: u64,
    terminal_count: u64,
    active_band: u64,
    ratio_pct: u64,
) -> u64 {
    let mut seq = 0_u64;
    let raw = Connection::open(db_path).expect("raw lookup");
    let pin_targets = (terminal_count..node_count)
        .step_by(usize::try_from((node_count / 8).max(1)).expect("fits"));
    for idx in pin_targets.take(8) {
        let mut key = [0_u8; 16];
        key[8..].copy_from_slice(&idx.to_be_bytes());
        let node_bytes: Vec<u8> = raw
            .query_row(
                "SELECT task_node_id FROM plan_nodes WHERE plan_id = ?1 AND node_key = ?2",
                params![plan_id.as_bytes().as_slice(), key.as_slice()],
                |row| row.get(0),
            )
            .expect("find pin target");
        let node_id = TaskNodeId::from_bytes(node_bytes.try_into().expect("16 bytes"));
        pin_node_hot(authority, plan_id, node_id, 1, &mut seq, 1_500);
    }

    let mut pending_added = 0_u64;
    let awaiting_start = terminal_count + active_band;
    for idx in awaiting_start..node_count {
        if idx.is_multiple_of(8) {
            continue;
        }
        let mut key = [0_u8; 16];
        key[8..].copy_from_slice(&idx.to_be_bytes());
        let node_bytes: Vec<u8> = raw
            .query_row(
                "SELECT task_node_id FROM plan_nodes WHERE plan_id = ?1 AND node_key = ?2",
                params![plan_id.as_bytes().as_slice(), key.as_slice()],
                |row| row.get(0),
            )
            .expect("find pending target");
        let node_id = TaskNodeId::from_bytes(node_bytes.try_into().expect("16 bytes"));
        seq += 1;
        authority
            .request_materialization(MaterializationRequest {
                plan_id,
                node_id,
                idempotency_key: idempotency_key_u64(0xF0 + ratio_pct, seq),
                requested_at_ms: 1_800,
            })
            .expect("create pending gate round");
        pending_added += 1;
        if pending_added >= 4 {
            break;
        }
    }
    pending_added
}

/// Sets up a `node_count` plan with `ratio_pct`% awaiting/active nodes
/// (20% active band across states 6/7/8/10 + `PINNED` residency overlay;
/// 80% awaiting across states 1/2/3/5 with `PENDING` gate rounds and DAG
/// deps) and `(100 - ratio_pct)`% `Completed` tail, keeping `WAL + FULL`.
fn build_scale_comparison_fixture(
    db_path: &Path,
    node_count: u64,
    ratio_pct: u64,
) -> (SqlitePlanAuthority, TaskPlanId, u64) {
    let authority = SqlitePlanAuthority::open(db_path).expect("open plan");
    let non_terminal = (node_count * ratio_pct) / 100;
    let active_band = (non_terminal / 5).max(1);
    let awaiting_count = non_terminal.saturating_sub(active_band).max(1);
    let terminal_count = node_count.saturating_sub(active_band + awaiting_count);

    let first_active_idx = terminal_count;
    let declarations: Vec<PlanNodeDeclaration> = (0..node_count)
        .map(|idx| {
            if idx >= terminal_count + active_band && idx.is_multiple_of(8) {
                indexed_node_with_dep(idx, Some(first_active_idx))
            } else if idx > terminal_count && terminal_count > 0 && idx.is_multiple_of(5) {
                indexed_node_with_dep(idx, Some(0))
            } else {
                node(idx)
            }
        })
        .collect();

    let plan_id = authority
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: None,
            nodes: declarations,
            idempotency_key: idempotency_key_u64(0x50 + ratio_pct, node_count),
            applied_at_ms: 1_000,
        })
        .expect("apply scale revision")
        .receipt()
        .plan_id;
    drop(authority);

    apply_bulk_fixture_states(db_path, plan_id, node_count, terminal_count, active_band);
    let authority = SqlitePlanAuthority::open(db_path).expect("reopen authority");
    let pending_added = pin_and_request_fixture_samples(
        db_path,
        &authority,
        plan_id,
        node_count,
        terminal_count,
        active_band,
        ratio_pct,
    );

    let window = active_band + pending_added + 64;
    assert_durability_and_v9(db_path);
    (authority, plan_id, window)
}

fn measure_scale_tier(node_count: u64, tier_label: &str) {
    for ratio_pct in [1_u64, 5, 50] {
        let root = Root::new(&format!("cmp-{tier_label}-r{ratio_pct}"));
        let db_path = root.0.join("plan.sqlite3");
        let (authority, plan_id, window) =
            build_scale_comparison_fixture(&db_path, node_count, ratio_pct);
        let scheduler = MaterializationScheduler::new(window);

        // Differential equivalence check before timing.
        let full_report = scheduler.select(&authority, plan_id).expect("full select");
        let bounded_report = scheduler
            .select_bounded(&authority, plan_id)
            .expect("bounded select");
        assert_eq!(
            bounded_report.seats_in_use, full_report.seats_in_use,
            "{tier_label} {ratio_pct}% seats_in_use mismatch"
        );
        assert_eq!(
            bounded_report.selections, full_report.selections,
            "{tier_label} {ratio_pct}% selections mismatch"
        );
        let selected_count = bounded_report.selections.len();

        for _ in 0..SCALE_WARMUP_ROUNDS {
            let _ = scheduler.select(&authority, plan_id).expect("warmup full");
            let _ = scheduler
                .select_bounded(&authority, plan_id)
                .expect("warmup bounded");
        }

        let mut full_us = Vec::with_capacity(SCALE_SAMPLE_ROUNDS);
        let mut bounded_us = Vec::with_capacity(SCALE_SAMPLE_ROUNDS);
        for _ in 0..SCALE_SAMPLE_ROUNDS {
            let t0 = Instant::now();
            let f = scheduler.select(&authority, plan_id).expect("timed full");
            full_us.push(t0.elapsed().as_micros());
            assert_eq!(f.selections.len(), selected_count);

            let t1 = Instant::now();
            let b = scheduler
                .select_bounded(&authority, plan_id)
                .expect("timed bounded");
            bounded_us.push(t1.elapsed().as_micros());
            assert_eq!(b.selections.len(), selected_count);
        }

        full_us.sort_unstable();
        bounded_us.sort_unstable();
        let full_p50 = percentile_us(&full_us, 50);
        let full_p95 = percentile_us(&full_us, 95);
        let bounded_p50 = percentile_us(&bounded_us, 50);
        let bounded_p95 = percentile_us(&bounded_us, 95);
        #[allow(clippy::cast_precision_loss)]
        let speedup_p50 = (full_p50 as f64) / (bounded_p50.max(1) as f64);
        #[allow(clippy::cast_precision_loss)]
        let speedup_p95 = (full_p95 as f64) / (bounded_p95.max(1) as f64);

        assert_durability_and_v9(&db_path);
        eprintln!(
            "C-SCHED-002 scale probe [{tier_label} nodes={node_count} ratio={ratio_pct}% window={window} selected={selected_count}]: \
             select_p50={full_p50}us select_p95={full_p95}us | \
             select_bounded_p50={bounded_p50}us select_bounded_p95={bounded_p95}us | \
             speedup_p50={speedup_p50:.2}x speedup_p95={speedup_p95:.2}x"
        );
    }
}

#[test]
#[ignore = "explicit C-SCHED-002 10K select vs select_bounded comparison probe"]
fn scale_probe_10k_select_vs_bounded() {
    measure_scale_tier(10_000, "10K");
}

#[test]
#[ignore = "explicit C-SCHED-002 100K select vs select_bounded comparison probe"]
fn scale_probe_100k_select_vs_bounded() {
    measure_scale_tier(100_000, "100K");
}
