//! Structured G3 gate condition tests (W36-P7, schema v6): the
//! Namespace/ResourceContract/fanout conditions upgrade from opaque
//! digests to typed validated forms — declaration validation, canonical
//! (order-free) identity participation in the node digest, bit-compat
//! of the legacy digest-only form, the frozen-shape fence over
//! conditions rewrites, and fail-closed decode of tampered bodies.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_plan::{
    ApplyPlanRevisionRequest, FanoutCondition, NamespaceCondition, NodeConditions,
    PlanNodeDeclaration, PlanNodeKind, PlanStoreError, ResourceContractCondition,
    SqlitePlanAuthority,
};
use nlos_types::{IdempotencyKey, NamespaceId, TaskNodeId, TaskPlanId};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Root(std::path::PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-plan-conditions-{label}-{}-{nonce}-{}",
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

fn conditions(namespaces: &[u8], fanout: u64) -> NodeConditions {
    NodeConditions {
        namespace: NamespaceCondition {
            namespaces: namespaces
                .iter()
                .map(|byte| NamespaceId::from_bytes([*byte; 16]))
                .collect(),
        },
        resource_contract: ResourceContractCondition {
            cpu_shares: 2,
            memory_mib: 128,
            io_weight: 0,
        },
        fanout: FanoutCondition {
            max_downstream_fanout: fanout,
        },
    }
}

fn node(key: u8, payload: u8, node_conditions: Option<NodeConditions>) -> PlanNodeDeclaration {
    PlanNodeDeclaration {
        node_key: [key; 16],
        kind: PlanNodeKind::AgentRole,
        binding_digest: [payload; 32],
        dependency_keys: Vec::new(),
        input_selectors_digest: [payload; 32],
        output_contract_digest: [payload; 32],
        policy_digest: [payload; 32],
        resource_ceiling_digest: [payload; 32],
        conditions: node_conditions,
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

fn apply_first(authority: &SqlitePlanAuthority, nodes: Vec<PlanNodeDeclaration>) -> TaskPlanId {
    authority
        .apply_plan_revision(revision_request(None, nodes, 0x01))
        .expect("revision 1")
        .receipt()
        .plan_id
}

fn node_id_of(authority: &SqlitePlanAuthority, plan_id: TaskPlanId, key: u8) -> TaskNodeId {
    authority
        .list_plan_nodes(plan_id)
        .expect("list nodes")
        .into_iter()
        .find(|record| record.node_key == [key; 16])
        .expect("declared node exists")
        .node_id
}

fn user_version(path: &std::path::Path) -> i64 {
    let connection = Connection::open(path).expect("raw reader");
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read user_version")
}

fn assert_integrity(path: &std::path::Path) {
    let connection = Connection::open(path).expect("integrity reader");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity check");
    assert_eq!(result, "ok");
}

/// The pre-v6 node-digest formula, replicated here to pin the
/// bit-compatibility contract: a `conditions: None` declaration must
/// hash identically to the W28-A formula (nothing is appended for
/// absence), so every stored v1..v5 row and digest-only re-declaration
/// keeps comparing bitwise.
fn pre_v6_node_digest(node: &PlanNodeDeclaration) -> [u8; 32] {
    let kind_byte = match node.kind {
        PlanNodeKind::AgentRole => 1_u8,
        PlanNodeKind::Executable => 2_u8,
    };
    let mut dependencies: Vec<&[u8; 16]> = node.dependency_keys.iter().collect();
    dependencies.sort_unstable();
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/plan/node-digest/v1");
    hasher.update(node.node_key);
    hasher.update([kind_byte]);
    hasher.update(node.binding_digest);
    hasher.update((dependencies.len() as u64).to_be_bytes());
    for dependency in dependencies {
        hasher.update(*dependency);
    }
    hasher.update(node.input_selectors_digest);
    hasher.update(node.output_contract_digest);
    hasher.update(node.policy_digest);
    hasher.update(node.resource_ceiling_digest);
    hasher.finalize().into()
}

/// Valid condition sets apply beside the digest-only form in one
/// revision, read back canonically (namespace order normalizes to
/// sorted), stay durable across restart, and leave the revision chain
/// verifiable.
#[test]
fn structured_conditions_apply_validate_and_read_back_canonically() {
    let root = Root::new("apply-readback");
    let db_path = root.0.join("plan.sqlite3");
    std::fs::create_dir_all(&root.0).expect("create db directory");
    let authority = SqlitePlanAuthority::open(&db_path).expect("open authority");
    assert_eq!(user_version(&db_path), 6);

    let unsorted = conditions(&[0x0c, 0x0a, 0x0b], 4);
    let plan_id = apply_first(
        &authority,
        vec![
            node(0x0a, 0x11, Some(unsorted.clone())),
            node(0x0b, 0x22, None),
        ],
    );
    let sorted_id = node_id_of(&authority, plan_id, 0x0a);
    let digest_only_id = node_id_of(&authority, plan_id, 0x0b);

    let read_back = authority
        .inspect_node_conditions(plan_id, 1, sorted_id)
        .expect("read structured conditions")
        .expect("conditions row present");
    assert_eq!(read_back.namespace.namespaces, {
        let mut sorted = unsorted.namespace.namespaces.clone();
        sorted.sort_unstable();
        sorted
    });
    assert_eq!(read_back.resource_contract, unsorted.resource_contract);
    assert_eq!(read_back.fanout, unsorted.fanout);
    assert_eq!(
        authority
            .inspect_node_conditions(plan_id, 1, digest_only_id)
            .expect("read digest-only node"),
        None
    );
    authority
        .verify_revision_chain(plan_id)
        .expect("chain still verifies with conditions folded in");
    drop(authority);

    let reopened = SqlitePlanAuthority::open(&db_path).expect("reopen");
    assert_eq!(
        reopened
            .inspect_node_conditions(plan_id, 1, sorted_id)
            .expect("read after restart")
            .expect("conditions durable"),
        read_back
    );
    assert_integrity(&db_path);
}

/// The legacy digest-only form stays bit-identical to the pre-v6
/// formula (replicated above), while a `Some(conditions)` declaration
/// extends the digest; the same node re-declared with the same
/// condition set in a different namespace order (and under another
/// plan identity — the digest excludes it) hashes identically.
#[test]
fn conditions_digest_fold_is_bit_compatible_and_order_free() {
    let root = Root::new("digest-fold");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");
    let legacy = node(0x0a, 0x11, None);
    let plan_id = apply_first(
        &authority,
        vec![
            legacy.clone(),
            node(0x0b, 0x11, Some(conditions(&[0x0a], 4))),
        ],
    );
    let legacy_digest = authority
        .list_plan_nodes(plan_id)
        .expect("list nodes")
        .into_iter()
        .find(|record| record.node_key == [0x0a; 16])
        .expect("node")
        .node_digest;
    let structured_digest = authority
        .list_plan_nodes(plan_id)
        .expect("list nodes")
        .into_iter()
        .find(|record| record.node_key == [0x0b; 16])
        .expect("node")
        .node_digest;

    assert_eq!(legacy_digest, pre_v6_node_digest(&legacy));
    assert_ne!(
        structured_digest,
        pre_v6_node_digest(&node(0x0b, 0x11, None))
    );

    // Same node key, same condition set, different declaration order,
    // different plan: the digest is order- and plan-independent.
    let other_plan = authority
        .apply_plan_revision(revision_request(
            None,
            vec![node(0x0b, 0x11, Some(conditions(&[0x0a, 0x0d], 4)))],
            0x11,
        ))
        .expect("declare the set in one order")
        .receipt()
        .plan_id;
    let one_order = authority
        .list_plan_nodes(other_plan)
        .expect("list nodes")
        .into_iter()
        .find(|record| record.node_key == [0x0b; 16])
        .expect("node")
        .node_digest;
    let third_plan = authority
        .apply_plan_revision(revision_request(
            None,
            vec![node(0x0b, 0x11, Some(conditions(&[0x0d, 0x0a], 4)))],
            0x12,
        ))
        .expect("declare the same set in another order")
        .receipt()
        .plan_id;
    let other_order = authority
        .list_plan_nodes(third_plan)
        .expect("list nodes")
        .into_iter()
        .find(|record| record.node_key == [0x0b; 16])
        .expect("node")
        .node_digest;
    assert_eq!(one_order, other_order);
    assert_ne!(one_order, structured_digest);

    // A different fanout bound changes the digest.
    let fourth_plan = authority
        .apply_plan_revision(revision_request(
            None,
            vec![node(0x0b, 0x11, Some(conditions(&[0x0a], 5)))],
            0x13,
        ))
        .expect("declare a different bound")
        .receipt()
        .plan_id;
    let other_bound = authority
        .list_plan_nodes(fourth_plan)
        .expect("list nodes")
        .into_iter()
        .find(|record| record.node_key == [0x0b; 16])
        .expect("node")
        .node_digest;
    assert_ne!(other_bound, structured_digest);
}

/// The invalid-form matrix: empty/duplicate/over-bound namespace sets,
/// all-zero resource contracts, and zero fanout bounds are each refused
/// typed at apply time, before anything durable is written.
#[test]
fn structured_conditions_invalid_forms_fail_typed() {
    let root = Root::new("invalid");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");

    let invalid_sets = [
        conditions(&[], 4),
        conditions(&[0x0a, 0x0a], 4),
        NodeConditions {
            namespace: NamespaceCondition {
                // 257 distinct ids: one over MAX_CONDITION_NAMESPACES.
                namespaces: (0..257_u128)
                    .map(|index| NamespaceId::from_bytes(index.to_be_bytes()))
                    .collect(),
            },
            resource_contract: ResourceContractCondition {
                cpu_shares: 1,
                memory_mib: 0,
                io_weight: 0,
            },
            fanout: FanoutCondition {
                max_downstream_fanout: 4,
            },
        },
        NodeConditions {
            namespace: conditions(&[0x0a], 4).namespace,
            resource_contract: ResourceContractCondition {
                cpu_shares: 0,
                memory_mib: 0,
                io_weight: 0,
            },
            fanout: conditions(&[0x0a], 4).fanout,
        },
        conditions(&[0x0a], 0),
    ];
    for invalid in invalid_sets {
        let result = authority.apply_plan_revision(revision_request(
            None,
            vec![node(0x0a, 0x11, Some(invalid))],
            0x02,
        ));
        assert!(
            matches!(result, Err(PlanStoreError::InvalidNodeConditions { .. })),
            "invalid condition set must fail typed"
        );
    }
    let connection = Connection::open(&root.0).expect("raw counter");
    let rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM plan_revision_nodes", [], |row| {
            row.get(0)
        })
        .expect("count shape rows");
    assert_eq!(rows, 0);
}

/// The G1 frozen-shape fence covers the structured conditions: an
/// execution-frozen node re-declaring the same condition set keeps its
/// original revision/digest (order-free); a different set is refused
/// typed.
#[test]
fn frozen_node_conditions_rewrite_is_refused_typed() {
    let root = Root::new("frozen");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");
    let plan_id = apply_first(
        &authority,
        vec![node(0x0a, 0x11, Some(conditions(&[0x0a, 0x0b], 4)))],
    );
    let node_id = node_id_of(&authority, plan_id, 0x0a);

    let raw = Connection::open(&root.0).expect("raw writer");
    // 6 = `PlanNodeState::Materializing` (the execution-freeze boundary;
    // the discriminant is crate-private, the wire value is schema-stable).
    raw.execute(
        "UPDATE plan_nodes SET node_state = 6 WHERE plan_id = ?1 AND task_node_id = ?2",
        rusqlite::params![plan_id.as_bytes().as_slice(), node_id.as_bytes().as_slice()],
    )
    .expect("freeze the node at the execution boundary");
    drop(raw);

    let original_digest = authority
        .list_plan_nodes(plan_id)
        .expect("list nodes")
        .into_iter()
        .find(|record| record.node_id == node_id)
        .expect("node")
        .node_digest;
    authority
        .apply_plan_revision(revision_request(
            Some(plan_id),
            vec![node(0x0a, 0x11, Some(conditions(&[0x0b, 0x0a], 4)))],
            0x03,
        ))
        .expect("bit-identical re-declaration (order-free) keeps the frozen shape");
    assert_eq!(
        authority
            .list_plan_nodes(plan_id)
            .expect("list nodes")
            .into_iter()
            .find(|record| record.node_id == node_id)
            .expect("node")
            .node_digest,
        original_digest
    );

    assert!(matches!(
        authority.apply_plan_revision(revision_request(
            Some(plan_id),
            vec![node(0x0a, 0x11, Some(conditions(&[0x0b, 0x0a], 5)))],
            0x04,
        )),
        Err(PlanStoreError::FrozenNodeShapeRewrite { .. })
    ));
}

/// The canonical conditions encoding, replicated here for tampering:
/// schema tag, sorted namespace set, three resource dimensions, fanout.
fn encode_conditions(conditions: &NodeConditions) -> Vec<u8> {
    let mut namespaces = conditions.namespace.namespaces.clone();
    namespaces.sort_unstable();
    let mut bytes = Vec::with_capacity(41 + namespaces.len() * 16);
    bytes.push(1_u8);
    bytes.extend_from_slice(&(namespaces.len() as u64).to_be_bytes());
    for namespace in namespaces {
        bytes.extend_from_slice(namespace.as_bytes());
    }
    bytes.extend_from_slice(&conditions.resource_contract.cpu_shares.to_be_bytes());
    bytes.extend_from_slice(&conditions.resource_contract.memory_mib.to_be_bytes());
    bytes.extend_from_slice(&conditions.resource_contract.io_weight.to_be_bytes());
    bytes.extend_from_slice(&conditions.fanout.max_downstream_fanout.to_be_bytes());
    bytes
}

fn injected_node_id(plan_id: TaskPlanId, node_key: [u8; 16]) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/plan/task-node-id/v1");
    hasher.update(plan_id.as_bytes());
    hasher.update(node_key);
    let digest: [u8; 32] = hasher.finalize().into();
    digest[..16].try_into().expect("16-byte node id")
}

/// Tampered condition bodies fail closed on readback: a wrong schema
/// tag, a truncated body, or a non-canonical (unsorted) encoding is a
/// corrupt record, never reinterpreted.
#[test]
fn tampered_condition_bodies_fail_closed_on_decode() {
    let root = Root::new("tamper");
    let db_path = root.0.join("plan.sqlite3");
    std::fs::create_dir_all(&root.0).expect("create db directory");
    let authority = SqlitePlanAuthority::open(&db_path).expect("open authority");
    let plan_id = apply_first(&authority, vec![node(0x0a, 0x11, None)]);
    drop(authority);

    let base = conditions(&[0x0a, 0x0b], 4);
    let mut wrong_tag = encode_conditions(&base);
    wrong_tag[0] = 9;
    let truncated = encode_conditions(&base)[..20].to_vec();
    let mut unsorted = Vec::new();
    {
        let mut namespaces = base.namespace.namespaces.clone();
        namespaces.reverse();
        unsorted.push(1_u8);
        unsorted.extend_from_slice(&(namespaces.len() as u64).to_be_bytes());
        for namespace in namespaces {
            unsorted.extend_from_slice(namespace.as_bytes());
        }
        unsorted.extend_from_slice(&base.resource_contract.cpu_shares.to_be_bytes());
        unsorted.extend_from_slice(&base.resource_contract.memory_mib.to_be_bytes());
        unsorted.extend_from_slice(&base.resource_contract.io_weight.to_be_bytes());
        unsorted.extend_from_slice(&base.fanout.max_downstream_fanout.to_be_bytes());
    }

    let raw = Connection::open(&db_path).expect("raw writer");
    let cases: [(&str, [u8; 16], Vec<u8>); 3] = [
        ("wrong tag", [0xb1; 16], wrong_tag),
        ("truncated", [0xb2; 16], truncated),
        ("unsorted", [0xb3; 16], unsorted),
    ];
    for (label, injected_key, body) in &cases {
        let injected_id = injected_node_id(plan_id, *injected_key);
        raw.execute(
            "INSERT INTO plan_revision_nodes (
                plan_id, revision, task_node_id, node_key, node_kind, node_digest,
                conditions_body
              ) VALUES (?1, 1, ?2, ?3, 1, ?4, ?5)",
            rusqlite::params![
                plan_id.as_bytes().as_slice(),
                injected_id.as_slice(),
                injected_key.as_slice(),
                [0u8; 32].as_slice(),
                body.as_slice(),
            ],
        )
        .unwrap_or_else(|error| panic!("inject {label} shape row: {error}"));
    }
    drop(raw);

    let reopened = SqlitePlanAuthority::open(&db_path).expect("reopen");
    for (label, injected_key, _) in &cases {
        assert!(
            matches!(
                reopened.inspect_node_conditions(
                    plan_id,
                    1,
                    TaskNodeId::from_bytes(injected_node_id(plan_id, *injected_key))
                ),
                Err(PlanStoreError::CorruptRecord(_))
            ),
            "{label} body must fail closed (key {injected_key:02x?})"
        );
    }
    assert_integrity(&db_path);
}
