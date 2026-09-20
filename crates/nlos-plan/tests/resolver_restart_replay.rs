//! Restart replay + idempotency convergence for the Dependency Resolver:
//! a crash between any two durable effects leaves the authority at a
//! complete prefix, key replay after restart converges to the same
//! receipt without double commits, and the pinned-view fence survives
//! restarts and post-crash reshapes (house pattern of
//! `restart_replay.rs` rounds).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_plan::{
    ApplyPlanRevisionRequest, PlanNodeDeclaration, PlanNodeKind, PlanResolutionDecision,
    PlanRevisionSelector, PlanStoreError, ResolvePlanRequest, SqlitePlanAuthority,
};
use nlos_types::{IdempotencyKey, TaskPlanId};
use rusqlite::Connection;

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Root(std::path::PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-plan-resolver-replay-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn node(key: u8, payload: u8, dependencies: &[u8]) -> PlanNodeDeclaration {
    PlanNodeDeclaration {
        node_key: [key; 16],
        kind: PlanNodeKind::AgentRole,
        binding_digest: [payload; 32],
        dependency_keys: dependencies.iter().map(|key| [*key; 16]).collect(),
        input_selectors_digest: [payload; 32],
        output_contract_digest: [payload; 32],
        policy_digest: [payload; 32],
        resource_ceiling_digest: [payload; 32],
    }
}

fn revision_request(
    plan_id: Option<TaskPlanId>,
    nodes: Vec<PlanNodeDeclaration>,
    key: u8,
) -> ApplyPlanRevisionRequest {
    ApplyPlanRevisionRequest {
        plan_id,
        nodes,
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        applied_at_ms: 1_000,
    }
}

fn resolve_request(selector: PlanRevisionSelector, key: u8) -> ResolvePlanRequest {
    ResolvePlanRequest {
        selector,
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        resolved_at_ms: 3_000,
    }
}

fn raw_count(path: &std::path::Path, table: &str) -> i64 {
    let connection = Connection::open(path).expect("raw reader");
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count rows")
}

fn assert_integrity(path: &std::path::Path) {
    let connection = Connection::open(path).expect("integrity reader");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity check");
    assert_eq!(result, "ok");
}

/// Restart between every resolver effect: resolve (key A) → crash → replay
/// A byte-equal; second key at the same head → second receipt, same
/// content; revision 2 applied → crash → replay A still answers from the
/// original pinned receipt (no float), a fresh resolution pins the new
/// head. Nothing double-commits; the chain and integrity stay intact.
#[test]
fn restart_between_resolver_effects_replays_once_and_converges() {
    let root = Root::new("rounds");
    let db_path = root.0.join("plan.sqlite3");
    std::fs::create_dir_all(&root.0).expect("create db directory");
    let mut authority = SqlitePlanAuthority::open(&db_path).expect("open");
    let plan_id = authority
        .apply_plan_revision(revision_request(
            None,
            vec![node(0x0a, 0x11, &[]), node(0x0b, 0x22, &[0x0a])],
            0x01,
        ))
        .expect("revision 1")
        .receipt()
        .plan_id;

    // Effect 1: resolution of revision 1. Crash. Replay converges.
    let original = authority
        .resolve_plan(resolve_request(
            PlanRevisionSelector::Current(plan_id),
            0x51,
        ))
        .expect("resolve revision 1")
        .handle();
    assert_eq!(original.revision, 1);
    drop(authority);
    authority = SqlitePlanAuthority::open(&db_path).expect("reopen after resolve");

    let replay = authority
        .resolve_plan(resolve_request(
            PlanRevisionSelector::Current(plan_id),
            0x51,
        ))
        .expect("replay after restart");
    assert!(matches!(replay, PlanResolutionDecision::Replayed(_)));
    assert_eq!(replay.handle(), original);
    assert_eq!(raw_count(&db_path, "plan_resolution_receipts"), 1);

    // Effect 2: an independent key at the same head. Crash. Still exactly
    // two receipts, same pinned content.
    let second = authority
        .resolve_plan(resolve_request(
            PlanRevisionSelector::At {
                plan_id,
                revision: 1,
            },
            0x52,
        ))
        .expect("second resolution")
        .handle();
    drop(authority);
    authority = SqlitePlanAuthority::open(&db_path).expect("reopen after second resolve");
    assert_eq!(raw_count(&db_path, "plan_resolution_receipts"), 2);
    assert_eq!(second.resolution_digest, original.resolution_digest);
    assert_ne!(second.resolution_id, original.resolution_id);

    // Effect 3: revision 2 (node b reshaped, node c added). Crash. The
    // old key's replay answers from the original receipt pinned to
    // revision 1 — never the new head; a fresh resolution pins revision 2.
    authority
        .apply_plan_revision(revision_request(
            Some(plan_id),
            vec![
                node(0x0a, 0x11, &[]),
                node(0x0b, 0x99, &[0x0a]),
                node(0x0c, 0x33, &[0x0b]),
            ],
            0x02,
        ))
        .expect("revision 2");
    drop(authority);
    authority = SqlitePlanAuthority::open(&db_path).expect("reopen after revision 2");

    let stale_retry = authority
        .resolve_plan(resolve_request(
            PlanRevisionSelector::Current(plan_id),
            0x51,
        ))
        .expect("crash-retry of the original key");
    assert!(matches!(stale_retry, PlanResolutionDecision::Replayed(_)));
    assert_eq!(stale_retry.handle(), original);

    let fresh = authority
        .resolve_plan(resolve_request(
            PlanRevisionSelector::Current(plan_id),
            0x53,
        ))
        .expect("fresh resolution pins revision 2")
        .handle();
    assert_eq!(fresh.revision, 2);
    assert_eq!(fresh.resolved_order.len(), 3);
    assert_eq!(raw_count(&db_path, "plan_resolution_receipts"), 3);
    assert_eq!(
        authority
            .verify_revision_chain(plan_id)
            .expect("chain")
            .revision_count,
        2
    );
    assert_integrity(&db_path);
}

/// The pinned-view fence survives restarts: a resolution taken at
/// revision 1 keeps observing revision-1 shapes after a restart, a
/// post-crash revision 2 reshape, and a second restart.
#[test]
fn restart_preserves_pinned_view_across_post_crash_reshape() {
    let root = Root::new("pinned");
    let db_path = root.0.join("plan.sqlite3");
    std::fs::create_dir_all(&root.0).expect("create db directory");
    let mut authority = SqlitePlanAuthority::open(&db_path).expect("open");
    let plan_id = authority
        .apply_plan_revision(revision_request(
            None,
            vec![node(0x0a, 0x11, &[]), node(0x0b, 0x22, &[0x0a])],
            0x01,
        ))
        .expect("revision 1")
        .receipt()
        .plan_id;
    let handle = authority
        .resolve_plan(resolve_request(
            PlanRevisionSelector::Current(plan_id),
            0x61,
        ))
        .expect("resolve revision 1")
        .handle();
    let pinned_before = authority
        .inspect_resolved_nodes(handle.resolution_id)
        .expect("pinned view before restart");
    drop(authority);

    // Restart, then reshape the unresolved node b in revision 2.
    authority = SqlitePlanAuthority::open(&db_path).expect("reopen");
    authority
        .apply_plan_revision(revision_request(
            Some(plan_id),
            vec![node(0x0a, 0x11, &[]), node(0x0b, 0x99, &[0x0a])],
            0x02,
        ))
        .expect("revision 2 reshapes node b");
    drop(authority);
    authority = SqlitePlanAuthority::open(&db_path).expect("second reopen");

    let pinned_after = authority
        .inspect_resolved_nodes(handle.resolution_id)
        .expect("pinned view after restarts and reshape");
    assert_eq!(
        pinned_after, pinned_before,
        "the pinned view is durable: it survives restarts and does not drift to revision 2"
    );
    assert!(matches!(
        authority.inspect_resolved_nodes(nlos_types::ReceiptId::from_bytes([0xee; 16])),
        Err(PlanStoreError::ResolutionNotFound(_))
    ));
    assert_integrity(&db_path);
}
