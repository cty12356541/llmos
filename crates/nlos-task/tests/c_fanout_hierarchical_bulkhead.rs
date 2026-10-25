//! Integration conformance suite for `C-FANOUT`:
//! - `[DIST-TASK-003]` Hierarchical subgroups, arrival-order-independent
//!   `DeterministicLocalReducer`, binary SHA-256 `MerkleResultRoot` +
//!   `MerkleInclusionProof`, and `O(B * depth)` `IncrementalSubgroupCheckpoint`
//! - `[DIST-FANOUT-001]` Six-axis spawn/fanout quota reservation,
//!   `WaitingResourceBackpressure`, and deterministic slot-release drain
//! - `[DIST-BULKHEAD-001]` Multi-scope bulkhead isolation (`Tenant`,
//!   `Workspace`, `TaskGroup`, `Provider`) + untouchable
//!   `SafetyTcbSystemReserve`

use nlos_task::{
    BulkheadAdmissionDecision, BulkheadController, BulkheadExecutionOutcome, BulkheadHealthState,
    BulkheadScopeId, BulkheadScopeKind, BulkheadSpec, BulkheadTripReason,
    DeterministicLocalReducer, FailureBudget, FanoutActualUsage, FanoutAdmissionDecision,
    FanoutAxis, FanoutError, FanoutGroupLifecycle, FanoutHardRefusalReason, FanoutLeafId,
    FanoutLeafMember, FanoutProviderId, FanoutQuotaContract, FanoutReservationRequest,
    FanoutTenantId, FanoutUsageLedger, FanoutWaitTicketId, HierarchicalFanoutTree,
    IncrementalSubgroupCheckpoint, LeafCandidateReceipt, LocalReducerKind, MerkleProofStep,
    MerkleSiblingPosition, SafetyTcbSystemReserve, SubgroupEdge, WorkloadBulkheadRoute,
    verify_merkle_proof,
};
use nlos_types::{
    AgentInstanceId, ControlDomainId, IdempotencyKey, ReceiptId, ReservationId, ResourceGroupId,
    TaskAttemptId, TaskGroupId, WorkspaceId,
};

fn id16(tag: u8, index: u32) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0] = tag;
    out[12..16].copy_from_slice(&index.to_be_bytes());
    out
}

fn digest32(tag: u8, index: u32) -> [u8; 32] {
    let mut out = [tag; 32];
    out[28..32].copy_from_slice(&index.to_be_bytes());
    out
}

fn make_leaf_member(index: u32) -> FanoutLeafMember {
    let leaf_id = if index.is_multiple_of(2) {
        FanoutLeafId::TaskAttempt(TaskAttemptId::from_bytes(id16(0xA1, index)))
    } else {
        FanoutLeafId::AgentInstance(AgentInstanceId::from_bytes(id16(0xA2, index)))
    };
    let domain = ControlDomainId::from_bytes(id16(0xD0, index % 16));
    FanoutLeafMember {
        leaf_id,
        control_domain_id: domain,
    }
}

fn make_candidate(member: FanoutLeafMember, variant: u8, seq: u32) -> LeafCandidateReceipt {
    LeafCandidateReceipt {
        leaf_id: member.leaf_id,
        receipt_id: ReceiptId::from_bytes(id16(0xB0 ^ variant, seq)),
        control_domain_id: member.control_domain_id,
        result_digest: digest32(0xC0 ^ variant, seq),
        provenance_digest: digest32(0xE0 ^ variant, seq),
    }
}

// ============================================================================
// [DIST-TASK-003] Tests 1–5
// ============================================================================

#[test]
fn hierarchical_fanout_tree_partitions_1200_leaves_with_bounded_degree_and_no_all_to_all() {
    let root_id = TaskGroupId::from_bytes(id16(0x01, 0));
    let leaves: Vec<FanoutLeafMember> = (0..1_200).map(make_leaf_member).collect();

    let tree = HierarchicalFanoutTree::build_balanced(root_id, 10, 3, &leaves)
        .expect("1,200 leaves fit inside B=10, max_depth=3");

    let metrics = tree.complexity_metrics();
    assert_eq!(metrics.total_leaves, 1_200);
    assert_eq!(metrics.observed_max_depth, 3);
    assert!(metrics.root_coordinator_degree <= 10);
    assert!(metrics.max_local_subgroup_degree <= 10);

    // 120 leaf subgroups + 12 mid subgroups + 2 top subgroups + 1 root = 135 subgroups.
    assert_eq!(metrics.subgroup_count, 135);
    assert_eq!(metrics.hierarchical_edge_count, 134 + 1_200);
    assert_eq!(metrics.avoided_all_to_all_edges, 1_200 * 1_199 / 2);
    assert!(metrics.hierarchical_edge_count * 500 < metrics.avoided_all_to_all_edges);

    // Reversed leaf registration order produces the exact same canonical hierarchy.
    let mut reversed_leaves = leaves.clone();
    reversed_leaves.reverse();
    let reversed_tree = HierarchicalFanoutTree::build_balanced(root_id, 10, 3, &reversed_leaves)
        .expect("reversed leaves fit");
    assert_eq!(
        tree.complexity_metrics(),
        reversed_tree.complexity_metrics()
    );
    assert_eq!(
        tree.subgroup(root_id),
        reversed_tree.subgroup(root_id),
        "root subgroup topology must be arrival-order independent"
    );
}

fn assert_incremental_tree_bounds(root: TaskGroupId, g1: TaskGroupId, g2: TaskGroupId) {
    let mut tree = HierarchicalFanoutTree::new(root, 2, 1).expect("valid empty tree");
    tree.register_subgroup(root, g1)
        .expect("depth 1 subgroup ok");
    assert_eq!(
        tree.register_subgroup(g1, g2),
        Err(FanoutError::MaxDepthExceeded {
            requested_depth: 2,
            max_depth: 1,
        })
    );
    let leaf0 = make_leaf_member(0);
    let leaf1 = make_leaf_member(1);
    tree.register_leaf(root, leaf0)
        .expect("second child under root (fanout 2/2) ok");
    assert_eq!(
        tree.register_leaf(root, leaf1),
        Err(FanoutError::BranchingFactorExceeded {
            subgroup_id: root,
            attempted_degree: 3,
            max_branching_factor: 2,
        })
    );
    assert_eq!(
        tree.register_leaf(g1, leaf0),
        Err(FanoutError::DuplicateLeafMember {
            leaf_id: leaf0.leaf_id,
        })
    );
}

#[test]
fn hierarchical_fanout_tree_rejects_cycles_multiple_parents_depth_and_branching_overflow_fail_closed()
 {
    let root = TaskGroupId::from_bytes(id16(0x10, 0));
    let g1 = TaskGroupId::from_bytes(id16(0x10, 1));
    let g2 = TaskGroupId::from_bytes(id16(0x10, 2));
    let g3 = TaskGroupId::from_bytes(id16(0x10, 3));

    // 1. 3-node cycle g1 -> g2 -> g3 -> g1.
    let cycle_edges = [
        SubgroupEdge {
            parent_group_id: root,
            child_group_id: g1,
        },
        SubgroupEdge {
            parent_group_id: g1,
            child_group_id: g2,
        },
        SubgroupEdge {
            parent_group_id: g2,
            child_group_id: g3,
        },
        SubgroupEdge {
            parent_group_id: g3,
            child_group_id: g1,
        },
    ];
    assert!(matches!(
        HierarchicalFanoutTree::from_topology(root, 4, 4, &cycle_edges, &[]),
        Err(FanoutError::CycleDetected { .. })
    ));

    // 2. Back-edge to root.
    let root_cycle = [
        SubgroupEdge {
            parent_group_id: root,
            child_group_id: g1,
        },
        SubgroupEdge {
            parent_group_id: g1,
            child_group_id: root,
        },
    ];
    assert_eq!(
        HierarchicalFanoutTree::from_topology(root, 4, 4, &root_cycle, &[])
            .expect_err("edge to root must fail"),
        FanoutError::CycleDetected { subgroup_id: root }
    );

    // 3. Diamond / multiple parents: root -> g1 -> g3 and root -> g2 -> g3.
    let diamond = [
        SubgroupEdge {
            parent_group_id: root,
            child_group_id: g1,
        },
        SubgroupEdge {
            parent_group_id: root,
            child_group_id: g2,
        },
        SubgroupEdge {
            parent_group_id: g1,
            child_group_id: g3,
        },
        SubgroupEdge {
            parent_group_id: g2,
            child_group_id: g3,
        },
    ];
    assert_eq!(
        HierarchicalFanoutTree::from_topology(root, 4, 4, &diamond, &[])
            .expect_err("diamond DAG must fail strict tree rule"),
        FanoutError::MultipleParents { subgroup_id: g3 }
    );

    // 4. Disconnected subgroup island: g2 -> g3 without link from root.
    let disconnected = [
        SubgroupEdge {
            parent_group_id: root,
            child_group_id: g1,
        },
        SubgroupEdge {
            parent_group_id: g2,
            child_group_id: g3,
        },
    ];
    assert!(matches!(
        HierarchicalFanoutTree::from_topology(root, 4, 4, &disconnected, &[]),
        Err(FanoutError::DisconnectedSubgroup { .. })
    ));

    // 5. Depth & branching factor overflow + duplicate leaf rejection.
    assert_incremental_tree_bounds(root, g1, g2);
}

#[test]
fn deterministic_local_reducer_is_arrival_order_independent_and_preserves_divergent_candidates() {
    let reducer_digest = digest32(0x77, 1);
    let members: Vec<FanoutLeafMember> = (0..64).map(make_leaf_member).collect();

    let mut all_receipts: Vec<LeafCandidateReceipt> = members
        .iter()
        .enumerate()
        .map(|(i, &m)| make_candidate(m, 0, u32::try_from(i).unwrap_or(0)))
        .collect();

    // Inject conflicting candidate receipts for leaves 5, 19, and 42 (`[TASK-GROUP-003]`).
    all_receipts.push(make_candidate(members[5], 1, 500));
    all_receipts.push(make_candidate(members[5], 2, 501));
    all_receipts.push(make_candidate(members[19], 1, 1900));
    all_receipts.push(make_candidate(members[42], 1, 4200));

    let mut forward_reducer =
        DeterministicLocalReducer::new(reducer_digest, LocalReducerKind::UnanimousConsensusCheck)
            .expect("valid reducer");
    for &receipt in &all_receipts {
        assert!(forward_reducer.record_candidate(receipt));
    }
    // Idempotent duplicate replay must return false and leave digests unchanged.
    assert!(!forward_reducer.record_candidate(all_receipts[0]));

    let mut reverse_reducer =
        DeterministicLocalReducer::new(reducer_digest, LocalReducerKind::UnanimousConsensusCheck)
            .expect("valid reducer");
    for &receipt in all_receipts.iter().rev() {
        let _ = reverse_reducer.record_candidate(receipt);
    }

    // Stride-permuted arrival order (coprime step 19 mod 68; gcd(19, 68) = 1).
    let mut stride_reducer =
        DeterministicLocalReducer::new(reducer_digest, LocalReducerKind::UnanimousConsensusCheck)
            .expect("valid reducer");
    let len = all_receipts.len();
    for step in 0..len {
        let idx = (step * 19 + 11) % len;
        let _ = stride_reducer.record_candidate(all_receipts[idx]);
    }

    let summary_fwd = forward_reducer.reduce();
    let summary_rev = reverse_reducer.reduce();
    let summary_stride = stride_reducer.reduce();

    assert_eq!(summary_fwd, summary_rev);
    assert_eq!(summary_fwd, summary_stride);
    assert_eq!(summary_fwd.reported_leaf_count, 64);
    assert!(summary_fwd.has_conflict());
    assert_eq!(summary_fwd.divergent_leaves.len(), 3);

    // Leaf 5 must retain all 3 competing candidates in canonical order (no LWW).
    let leaf5_record = summary_fwd
        .divergent_leaves
        .iter()
        .find(|d| d.leaf_id == members[5].leaf_id)
        .expect("leaf 5 divergence preserved");
    assert_eq!(leaf5_record.candidates.len(), 3);

    // Reducer digest mismatch fails closed.
    let wrong_digest = digest32(0x88, 2);
    assert_eq!(
        forward_reducer.verify_reducer_binding(&wrong_digest),
        Err(FanoutError::ReducerDigestMismatch {
            expected: reducer_digest,
            supplied: wrong_digest,
        })
    );
    forward_reducer
        .verify_reducer_binding(&reducer_digest)
        .expect("matching digest passes");
}

#[test]
fn merkle_result_root_and_inclusion_proof_verify_and_reject_tampering_across_all_fields() {
    let reducer_digest = digest32(0x55, 9);
    let mut reducer =
        DeterministicLocalReducer::new(reducer_digest, LocalReducerKind::CanonicalSortedFold)
            .expect("valid reducer");

    // Use 13 leaves (odd count across multiple binary levels) plus a divergent leaf.
    let members: Vec<FanoutLeafMember> = (0..13).map(make_leaf_member).collect();
    for (idx, &m) in members.iter().enumerate() {
        reducer.record_candidate(make_candidate(m, 0, u32::try_from(idx).unwrap_or(0)));
    }
    reducer.record_candidate(make_candidate(members[7], 1, 777));

    let summary = reducer.reduce();
    assert_eq!(summary.internal_nodes_hashed, 12);

    for &member in &members {
        let proof = reducer.prove_leaf(member.leaf_id).expect("leaf reported");
        assert!(verify_merkle_proof(
            &summary.merkle_result_root,
            &reducer_digest,
            &proof
        ));
    }

    let base_proof = reducer
        .prove_leaf(members[7].leaf_id)
        .expect("divergent leaf proof");
    assert_eq!(base_proof.leaf_entry.candidates.len(), 2);

    // 1. Tamper result_digest.
    let mut bad_result = base_proof.clone();
    bad_result.leaf_entry.candidates[0].result_digest[0] ^= 0x01;
    assert!(!verify_merkle_proof(
        &summary.merkle_result_root,
        &reducer_digest,
        &bad_result
    ));

    // 2. Tamper provenance_digest.
    let mut bad_prov = base_proof.clone();
    bad_prov.leaf_entry.candidates[1].provenance_digest[31] ^= 0x80;
    assert!(!verify_merkle_proof(
        &summary.merkle_result_root,
        &reducer_digest,
        &bad_prov
    ));

    // 3. Non-canonical candidate ordering inside proof leaf.
    let mut unsorted = base_proof.clone();
    unsorted.leaf_entry.candidates.swap(0, 1);
    assert!(!verify_merkle_proof(
        &summary.merkle_result_root,
        &reducer_digest,
        &unsorted
    ));

    // 4. Empty candidate list in forged proof.
    let mut empty_candidates = base_proof.clone();
    empty_candidates.leaf_entry.candidates.clear();
    assert!(!verify_merkle_proof(
        &summary.merkle_result_root,
        &reducer_digest,
        &empty_candidates
    ));

    // 5. Wrong reducer_digest or zero reducer_digest.
    assert!(!verify_merkle_proof(
        &summary.merkle_result_root,
        &digest32(0x55, 10),
        &base_proof
    ));
    assert!(!verify_merkle_proof(
        &summary.merkle_result_root,
        &[0u8; 32],
        &base_proof
    ));

    // 6. Flip sibling position Left <-> Right.
    let mut flipped_pos = base_proof.clone();
    flipped_pos.steps[0].position = match flipped_pos.steps[0].position {
        MerkleSiblingPosition::Left => MerkleSiblingPosition::Right,
        MerkleSiblingPosition::Right => MerkleSiblingPosition::Left,
    };
    assert!(!verify_merkle_proof(
        &summary.merkle_result_root,
        &reducer_digest,
        &flipped_pos
    ));

    // 7. Extra trailing step or missing step.
    let mut extra_step = base_proof.clone();
    extra_step.steps.push(MerkleProofStep {
        sibling_hash: [0x11; 32],
        position: MerkleSiblingPosition::Right,
    });
    assert!(!verify_merkle_proof(
        &summary.merkle_result_root,
        &reducer_digest,
        &extra_step
    ));

    let mut missing_step = base_proof;
    let _ = missing_step.steps.pop();
    assert!(!verify_merkle_proof(
        &summary.merkle_result_root,
        &reducer_digest,
        &missing_step
    ));
}

#[test]
fn incremental_subgroup_checkpoint_recomputes_only_dirty_path_sublinear_in_1000_leaf_tree() {
    let root_id = TaskGroupId::from_bytes(id16(0x20, 0));
    let reducer_digest = digest32(0x99, 42);
    let leaves: Vec<FanoutLeafMember> = (0..1_000).map(make_leaf_member).collect();

    // 1,000 leaves with B=10 -> 1 root (d=0) + 10 mid (d=1) + 100 leaf-groups (d=2) = 111 subgroups.
    let tree_a =
        HierarchicalFanoutTree::build_balanced(root_id, 10, 3, &leaves).expect("valid 1,000 tree");
    let tree_b = tree_a.clone();
    assert_eq!(tree_a.subgroup_count(), 111);

    let mut ckpt_fwd = IncrementalSubgroupCheckpoint::new(
        tree_a,
        reducer_digest,
        LocalReducerKind::CanonicalSortedFold,
    )
    .expect("valid checkpoint engine");
    let mut ckpt_rev = IncrementalSubgroupCheckpoint::new(
        tree_b,
        reducer_digest,
        LocalReducerKind::CanonicalSortedFold,
    )
    .expect("valid checkpoint engine");

    // Report first 999 leaves in forward order on ckpt_fwd, reverse order on ckpt_rev.
    for (idx, &m) in leaves[..999].iter().enumerate() {
        assert!(
            ckpt_fwd
                .record_leaf_candidate(make_candidate(m, 0, u32::try_from(idx).unwrap_or(0)))
                .expect("registered leaf")
        );
    }
    for (idx, &m) in leaves[..999].iter().enumerate().rev() {
        assert!(
            ckpt_rev
                .record_leaf_candidate(make_candidate(m, 0, u32::try_from(idx).unwrap_or(0)))
                .expect("registered leaf")
        );
    }

    let report_999_fwd = ckpt_fwd.checkpoint();
    let report_999_rev = ckpt_rev.checkpoint();
    assert_eq!(
        report_999_fwd.root_merkle_result,
        report_999_rev.root_merkle_result
    );
    assert_eq!(
        report_999_fwd.root_checkpoint_digest,
        report_999_rev.root_checkpoint_digest
    );
    assert_eq!(report_999_fwd.completed_leaves, 999);

    // Now update ONLY the 1,000th leaf (`leaves[999]`) and run an incremental checkpoint!
    assert!(
        ckpt_fwd
            .record_leaf_candidate(make_candidate(leaves[999], 0, 999))
            .expect("1000th leaf")
    );
    let incremental_report = ckpt_fwd.checkpoint();

    // Only 3 subgroups along leaf -> d=2 -> d=1 -> d=0 root are dirty; 108 subgroups are reused!
    assert_eq!(incremental_report.dirty_subgroups_recomputed, 3);
    assert_eq!(incremental_report.cached_subgroups_reused, 108);
    assert_eq!(incremental_report.leaf_hashes_computed, 1);
    // Each of the 3 dirty subgroups has 10 items -> 9 binary internal hashes each = 27.
    assert_eq!(incremental_report.internal_merkle_nodes_hashed, 27);
    assert_eq!(incremental_report.total_hashed_nodes, 28);
    assert!(incremental_report.total_hashed_nodes * 30 < incremental_report.total_leaves);

    // Compare against clean O(N) full-tree reference recomputation:
    // Full tree hashes 1,000 leaves + 111 * 9 (999) internal binary nodes = 1,999 hashes!
    let (full_merkle, full_digest, full_hashed_nodes) = ckpt_fwd.recompute_full_reference();
    assert_eq!(full_hashed_nodes, 1_999);
    assert_eq!(incremental_report.root_merkle_result, full_merkle);
    assert_eq!(incremental_report.root_checkpoint_digest, full_digest);

    // Verify end-to-end Merkle inclusion proofs from multiple leaves up to the 1,000-leaf root!
    for sample_idx in [0usize, 333, 777, 999] {
        let proof = ckpt_fwd
            .prove_leaf_to_root(leaves[sample_idx].leaf_id)
            .expect("proof up to root");
        assert!(verify_merkle_proof(
            &incremental_report.root_merkle_result,
            &reducer_digest,
            &proof
        ));
    }
}

// ============================================================================
// [DIST-FANOUT-001] Tests 6–8
// ============================================================================

fn sample_quota_contract() -> FanoutQuotaContract {
    FanoutQuotaContract {
        max_group_depth: 4,
        max_total_children: 10,
        max_active_agents: 3,
        max_message_count: 100,
        max_message_bytes: 64_000,
        max_context_read_bytes: 1_000_000,
        max_write_bytes: 256_000,
        max_external_effects: 5,
        max_backpressure_queue_len: 4,
        base_retry_after_ms: 50,
    }
}

fn base_reservation(seq: u32) -> FanoutReservationRequest {
    FanoutReservationRequest {
        reservation_id: ReservationId::from_bytes(id16(0x30, seq)),
        idempotency_key: IdempotencyKey::from_bytes(id16(0x31, seq)),
        requested_depth: 2,
        child_spawns: 1,
        active_agents: 1,
        reserved_messages: 10,
        reserved_message_bytes: 4_000,
        reserved_context_read_bytes: 100_000,
        reserved_write_bytes: 16_000,
        reserved_external_effects: 1,
    }
}

#[test]
fn fanout_admission_enforces_all_six_normative_quota_dimensions_and_multi_axis_saturation() {
    let group_id = TaskGroupId::from_bytes(id16(0x40, 1));
    let mut ledger = FanoutUsageLedger::new(group_id, sample_quota_contract());

    // Dimension 1: max_group_depth -> HardRefused (structural depth bound).
    let mut too_deep = base_reservation(1);
    too_deep.requested_depth = 5;
    assert_eq!(
        ledger.request_admission(too_deep).expect("evaluated"),
        FanoutAdmissionDecision::HardRefused {
            reason: FanoutHardRefusalReason::MaxGroupDepthExceeded {
                requested_depth: 5,
                max_group_depth: 4,
            },
        }
    );

    // Dimension 2a: max_total_children -> WaitingResourceBackpressure(TotalChildren).
    let mut children_overflow = base_reservation(2);
    children_overflow.child_spawns = 11;
    assert_eq!(
        ledger
            .request_admission(children_overflow)
            .expect("evaluated"),
        FanoutAdmissionDecision::WaitingResourceBackpressure {
            wait_ticket_id: FanoutWaitTicketId(1),
            saturated_axes: vec![FanoutAxis::TotalChildren],
            retry_after_hint_ms: Some(50),
        }
    );

    // Fresh ledger to isolate each remaining axis cleanly.
    let mut ledger = FanoutUsageLedger::new(group_id, sample_quota_contract());

    // Dimension 2b: max_active_agents.
    let mut active_overflow = base_reservation(3);
    active_overflow.active_agents = 4;
    assert_eq!(
        ledger
            .request_admission(active_overflow)
            .expect("evaluated"),
        FanoutAdmissionDecision::WaitingResourceBackpressure {
            wait_ticket_id: FanoutWaitTicketId(1),
            saturated_axes: vec![FanoutAxis::ActiveAgents],
            retry_after_hint_ms: Some(50),
        }
    );

    // Dimension 3 (message count + message bytes), Dimension 4 (context_read_bytes),
    // Dimension 5 (write_bytes), and Dimension 6 (external_effects) simultaneously!
    let mut ledger = FanoutUsageLedger::new(group_id, sample_quota_contract());
    let mut multi_overflow = base_reservation(4);
    multi_overflow.reserved_messages = 101;
    multi_overflow.reserved_message_bytes = 64_001;
    multi_overflow.reserved_context_read_bytes = 1_000_001;
    multi_overflow.reserved_write_bytes = 256_001;
    multi_overflow.reserved_external_effects = 6;

    assert_eq!(
        ledger.request_admission(multi_overflow).expect("evaluated"),
        FanoutAdmissionDecision::WaitingResourceBackpressure {
            wait_ticket_id: FanoutWaitTicketId(1),
            saturated_axes: vec![
                FanoutAxis::MessageCount,
                FanoutAxis::MessageBytes,
                FanoutAxis::ContextReadBytes,
                FanoutAxis::WriteBytes,
                FanoutAxis::ExternalEffects,
            ],
            retry_after_hint_ms: Some(50),
        }
    );
}

#[test]
fn fanout_backpressure_blocks_runaway_recursion_and_slot_settlement_drains_waiters_deterministically()
 {
    let group_id = TaskGroupId::from_bytes(id16(0x41, 1));
    let mut ledger = FanoutUsageLedger::new(group_id, sample_quota_contract());

    // Admit 3 active reservations (saturating max_active_agents = 3).
    for seq in 1..=3 {
        let req = base_reservation(seq);
        let decision = ledger.request_admission(req).expect("admit 1..=3");
        assert!(matches!(decision, FanoutAdmissionDecision::Admitted { .. }));
    }
    assert_eq!(ledger.active_agents(), 3);
    assert_eq!(ledger.headroom().remaining_active_agents, 0);

    // Next 4 spawn requests hit WaitingResourceBackpressure (queue cap = 4).
    for (offset, seq) in (4..=7).enumerate() {
        let req = base_reservation(seq);
        let decision = ledger.request_admission(req).expect("backpressure 4..=7");
        let expected_ticket = u64::try_from(offset + 1).unwrap_or(1);
        assert_eq!(
            decision,
            FanoutAdmissionDecision::WaitingResourceBackpressure {
                wait_ticket_id: FanoutWaitTicketId(expected_ticket),
                saturated_axes: vec![FanoutAxis::ActiveAgents],
                retry_after_hint_ms: Some(50 * expected_ticket),
            }
        );
    }

    // 8th request exceeds max_backpressure_queue_len (4) -> HardRefused(BackpressureQueueFull).
    assert_eq!(
        ledger
            .request_admission(base_reservation(8))
            .expect("queue shed"),
        FanoutAdmissionDecision::HardRefused {
            reason: FanoutHardRefusalReason::BackpressureQueueFull {
                max_backpressure_queue_len: 4,
            },
        }
    );

    // Attempting to settle with actual usage exceeding reservation fails closed without mutation.
    let res1_id = base_reservation(1).reservation_id;
    assert_eq!(
        ledger.settle_reservation(
            res1_id,
            FanoutActualUsage {
                actual_child_spawns: 1,
                actual_messages: 5,
                actual_message_bytes: 1_000,
                actual_context_read_bytes: 50_000,
                actual_write_bytes: 8_000,
                actual_external_effects: 2, // exceeds reserved 1!
            }
        ),
        Err(FanoutError::ActualUsageExceedsReservation {
            reservation_id: res1_id,
            axis: FanoutAxis::ExternalEffects,
        })
    );

    // Valid settlement of reservation #1 refunds unused headroom and promotes FIFO waiter #4!
    let settle1 = ledger
        .settle_reservation(
            res1_id,
            FanoutActualUsage {
                actual_child_spawns: 1,
                actual_messages: 4,
                actual_message_bytes: 1_000,
                actual_context_read_bytes: 20_000,
                actual_write_bytes: 4_000,
                actual_external_effects: 0,
            },
        )
        .expect("settle res1");
    assert_eq!(
        settle1.unblocked_reservations,
        vec![(FanoutWaitTicketId(1), base_reservation(4).reservation_id)]
    );
    assert_eq!(ledger.active_agents(), 3);
    assert_eq!(ledger.waiting_backpressure_count(), 3);

    // Cancelling reservation #2 refunds 100% of its budget and promotes FIFO waiter #5!
    let res2_id = base_reservation(2).reservation_id;
    let cancel2 = ledger.cancel_reservation(res2_id).expect("cancel res2");
    assert_eq!(
        cancel2.unblocked_reservations,
        vec![(FanoutWaitTicketId(2), base_reservation(5).reservation_id)]
    );
    assert_eq!(ledger.waiting_backpressure_count(), 2);

    // Cancelling the group flushes remaining waiters and refuses further admissions.
    let dropped = ledger.set_lifecycle(FanoutGroupLifecycle::Cancelled);
    assert_eq!(dropped, 2);
    assert_eq!(ledger.waiting_backpressure_count(), 0);
    assert_eq!(
        ledger
            .request_admission(base_reservation(99))
            .expect("cancelled refusal"),
        FanoutAdmissionDecision::HardRefused {
            reason: FanoutHardRefusalReason::GroupCancelled,
        }
    );
}

#[test]
fn fanout_admission_idempotent_replay_and_quota_extension_drain() {
    let group_id = TaskGroupId::from_bytes(id16(0x42, 1));
    let mut contract = sample_quota_contract();
    contract.max_external_effects = 1;
    let mut ledger = FanoutUsageLedger::new(group_id, contract);

    let req1 = base_reservation(1);
    let first = ledger.request_admission(req1).expect("admit req1");
    let replay = ledger.request_admission(req1).expect("idempotent replay");
    assert_eq!(first, replay);

    // Conflicting reuse of reservation_id #1 fails closed.
    let mut conflicting = req1;
    conflicting.reserved_write_bytes += 1;
    assert_eq!(
        ledger.request_admission(conflicting),
        Err(FanoutError::ConflictingReservationRequest {
            reservation_id: req1.reservation_id,
        })
    );

    // req2 blocks on ExternalEffects (max_external_effects = 1).
    let req2 = base_reservation(2);
    let wait_decision = ledger.request_admission(req2).expect("req2 backpressured");
    assert_eq!(
        wait_decision,
        FanoutAdmissionDecision::WaitingResourceBackpressure {
            wait_ticket_id: FanoutWaitTicketId(1),
            saturated_axes: vec![FanoutAxis::ExternalEffects],
            retry_after_hint_ms: Some(50),
        }
    );
    // Replaying queued req2 returns the same wait ticket.
    assert_eq!(
        ledger.request_admission(req2).expect("queued replay"),
        wait_decision
    );

    // Extending quota contract (`max_external_effects: 1 -> 3`) unblocks req2 immediately!
    contract.max_external_effects = 3;
    let ext_report = ledger.extend_quota_contract(contract);
    assert_eq!(
        ext_report.unblocked_reservations,
        vec![(FanoutWaitTicketId(1), req2.reservation_id)]
    );
    assert_eq!(ledger.active_reservation_count(), 2);
}

// ============================================================================
// [DIST-BULKHEAD-001] Tests 9–12
// ============================================================================

struct MultiScopeTestHarness {
    controller: BulkheadController,
    tenant_a: FanoutTenantId,
    workspace_a: WorkspaceId,
    group_storm: TaskGroupId,
    group_sibling: TaskGroupId,
    provider_a: FanoutProviderId,
    provider_b: FanoutProviderId,
    tcb_rg: ResourceGroupId,
}

fn setup_bulkhead_harness() -> MultiScopeTestHarness {
    let tcb_rg = ResourceGroupId::from_bytes(id16(0xF0, 0));
    let mut controller = BulkheadController::new(
        20,
        SafetyTcbSystemReserve {
            resource_group_id: tcb_rg,
            reserved_tcb_slots: 4,
        },
    )
    .expect("valid controller");

    let tenant_a = FanoutTenantId::from_bytes(id16(0x50, 1));
    let workspace_a = WorkspaceId::from_bytes(id16(0x51, 1));
    let group_storm = TaskGroupId::from_bytes(id16(0x52, 1));
    let group_sibling = TaskGroupId::from_bytes(id16(0x52, 2));
    let provider_a = FanoutProviderId::from_bytes(id16(0x53, 1));
    let provider_b = FanoutProviderId::from_bytes(id16(0x53, 2));

    let default_budget = FailureBudget {
        max_consecutive_failures: 3,
        max_total_failures: 5,
    };

    controller
        .register_bulkhead(BulkheadSpec {
            scope_id: BulkheadScopeId::Tenant(tenant_a),
            resource_group_id: ResourceGroupId::from_bytes(id16(0x60, 1)),
            max_concurrent: 12,
            max_queue_depth: 8,
            failure_budget: default_budget,
        })
        .expect("tenant_a");
    controller
        .register_bulkhead(BulkheadSpec {
            scope_id: BulkheadScopeId::Workspace(workspace_a),
            resource_group_id: ResourceGroupId::from_bytes(id16(0x61, 1)),
            max_concurrent: 12,
            max_queue_depth: 8,
            failure_budget: default_budget,
        })
        .expect("workspace_a");
    controller
        .register_bulkhead(BulkheadSpec {
            scope_id: BulkheadScopeId::TaskGroup(group_storm),
            resource_group_id: ResourceGroupId::from_bytes(id16(0x62, 1)),
            max_concurrent: 3,
            max_queue_depth: 2,
            failure_budget: FailureBudget {
                max_consecutive_failures: 2,
                max_total_failures: 3,
            },
        })
        .expect("group_storm");
    controller
        .register_bulkhead(BulkheadSpec {
            scope_id: BulkheadScopeId::TaskGroup(group_sibling),
            resource_group_id: ResourceGroupId::from_bytes(id16(0x62, 2)),
            max_concurrent: 4,
            max_queue_depth: 4,
            failure_budget: default_budget,
        })
        .expect("group_sibling");
    controller
        .register_bulkhead(BulkheadSpec {
            scope_id: BulkheadScopeId::Provider(provider_a),
            resource_group_id: ResourceGroupId::from_bytes(id16(0x63, 1)),
            max_concurrent: 2,
            max_queue_depth: 2,
            failure_budget: default_budget,
        })
        .expect("provider_a");
    controller
        .register_bulkhead(BulkheadSpec {
            scope_id: BulkheadScopeId::Provider(provider_b),
            resource_group_id: ResourceGroupId::from_bytes(id16(0x63, 2)),
            max_concurrent: 8,
            max_queue_depth: 8,
            failure_budget: default_budget,
        })
        .expect("provider_b");

    MultiScopeTestHarness {
        controller,
        tenant_a,
        workspace_a,
        group_storm,
        group_sibling,
        provider_a,
        provider_b,
        tcb_rg,
    }
}

#[test]
fn bulkhead_isolates_agent_fanout_storm_without_degrading_sibling_task_group_or_safety_tcb() {
    let mut h = setup_bulkhead_harness();
    let storm_route = WorkloadBulkheadRoute {
        tenant_id: h.tenant_a,
        workspace_id: h.workspace_a,
        task_group_id: h.group_storm,
        provider_id: Some(h.provider_b),
    };

    let mut acquired = 0usize;
    let mut queued = 0usize;
    let mut rejected = 0usize;

    for _ in 0..50 {
        match h
            .controller
            .acquire_workload(storm_route)
            .expect("registered")
        {
            BulkheadAdmissionDecision::Acquired { .. } => acquired += 1,
            BulkheadAdmissionDecision::Queued {
                bottleneck_scope, ..
            } => {
                assert_eq!(bottleneck_scope, BulkheadScopeId::TaskGroup(h.group_storm));
                assert_eq!(bottleneck_scope.kind(), BulkheadScopeKind::TaskGroup);
                queued += 1;
            }
            BulkheadAdmissionDecision::RejectedQueueFull { saturated_scope } => {
                assert_eq!(saturated_scope, BulkheadScopeId::TaskGroup(h.group_storm));
                rejected += 1;
            }
            other => panic!("unexpected decision during storm: {other:?}"),
        }
    }
    assert_eq!(acquired, 3);
    assert_eq!(queued, 2);
    assert_eq!(rejected, 45);

    // Sibling TaskGroup in the SAME Tenant & Workspace acquires all 4 slots with zero queueing!
    let sibling_route = WorkloadBulkheadRoute {
        tenant_id: h.tenant_a,
        workspace_id: h.workspace_a,
        task_group_id: h.group_sibling,
        provider_id: Some(h.provider_b),
    };
    for _ in 0..4 {
        let decision = h
            .controller
            .acquire_workload(sibling_route)
            .expect("sibling ok");
        assert!(matches!(
            decision,
            BulkheadAdmissionDecision::Acquired { .. }
        ));
    }

    // SafetyTcbSystemReserve remains 100% untouched (4/4 slots free).
    assert_eq!(h.controller.available_tcb_slots(), 4);
    for _ in 0..4 {
        let (_, rg) = h
            .controller
            .acquire_safety_tcb_slot()
            .expect("TCB slot guaranteed");
        assert_eq!(rg, h.tcb_rg);
    }
    assert_eq!(h.controller.available_tcb_slots(), 0);
}

#[test]
fn bulkhead_contains_provider_stall_without_pinning_tenant_workspace_or_starving_healthy_provider()
{
    let mut h = setup_bulkhead_harness();
    let stalled_provider_route = WorkloadBulkheadRoute {
        tenant_id: h.tenant_a,
        workspace_id: h.workspace_a,
        task_group_id: h.group_sibling,
        provider_id: Some(h.provider_a), // max_concurrent = 2, max_queue_depth = 2
    };

    let d1 = h
        .controller
        .acquire_workload(stalled_provider_route)
        .expect("1");
    let d2 = h
        .controller
        .acquire_workload(stalled_provider_route)
        .expect("2");
    let BulkheadAdmissionDecision::Acquired {
        lease_id: stalled_lease_1,
        ..
    } = d1
    else {
        panic!("expected acquired");
    };
    assert!(matches!(d2, BulkheadAdmissionDecision::Acquired { .. }));

    // Next 2 requests queue specifically on Provider(provider_a) without pinning Tenant/TaskGroup!
    let q1 = h
        .controller
        .acquire_workload(stalled_provider_route)
        .expect("3");
    let q2 = h
        .controller
        .acquire_workload(stalled_provider_route)
        .expect("4");
    assert!(matches!(
        q1,
        BulkheadAdmissionDecision::Queued {
            bottleneck_scope,
            ..
        } if bottleneck_scope == BulkheadScopeId::Provider(h.provider_a)
    ));
    assert!(matches!(q2, BulkheadAdmissionDecision::Queued { .. }));

    // 5th request to stalled Provider A is shed cleanly.
    assert_eq!(
        h.controller
            .acquire_workload(stalled_provider_route)
            .expect("5"),
        BulkheadAdmissionDecision::RejectedQueueFull {
            saturated_scope: BulkheadScopeId::Provider(h.provider_a),
        }
    );

    // Tenant A / Group Sibling active concurrency is still only 2 (not 4!), so switching to
    // healthy Provider B immediately succeeds twice!
    let tenant_snap = h
        .controller
        .bulkhead_snapshot(BulkheadScopeId::Tenant(h.tenant_a))
        .expect("tenant snap");
    assert_eq!(tenant_snap.active_concurrent, 2);

    let healthy_provider_route = WorkloadBulkheadRoute {
        tenant_id: h.tenant_a,
        workspace_id: h.workspace_a,
        task_group_id: h.group_sibling,
        provider_id: Some(h.provider_b),
    };
    for _ in 0..2 {
        assert!(matches!(
            h.controller
                .acquire_workload(healthy_provider_route)
                .expect("healthy provider ok"),
            BulkheadAdmissionDecision::Acquired { .. }
        ));
    }

    // Completing stalled_lease_1 promotes the head-of-line waiter on Provider A once Group Sibling
    // also frees a slot; let's verify promotion when Provider A has capacity.
    // Note: group_sibling currently holds 4/4 active slots (2 on A + 2 on B), so releasing
    // stalled_lease_1 frees BOTH Provider A (1/2) AND group_sibling (3/4) -> promotes q1!
    let completion = h
        .controller
        .complete_workload_lease(stalled_lease_1, &BulkheadExecutionOutcome::Success, 1_000)
        .expect("complete stalled_lease_1");
    assert_eq!(completion.promoted_tickets.len(), 1);
    assert_eq!(completion.promoted_tickets[0].route, stalled_provider_route);
}

fn verify_sibling_isolation_and_monotonic_reset(h: &mut MultiScopeTestHarness) {
    // Tenant, Workspace, and sibling TaskGroup remain Healthy!
    let sibling_route = WorkloadBulkheadRoute {
        tenant_id: h.tenant_a,
        workspace_id: h.workspace_a,
        task_group_id: h.group_sibling,
        provider_id: Some(h.provider_b),
    };
    assert!(matches!(
        h.controller
            .acquire_workload(sibling_route)
            .expect("sibling unaffected"),
        BulkheadAdmissionDecision::Acquired { .. }
    ));

    // Administrative reset requires a strictly increasing reset_epoch.
    assert_eq!(
        h.controller
            .reset_poisoned_bulkhead(BulkheadScopeId::TaskGroup(h.group_storm), 0),
        Err(FanoutError::StaleBulkheadResetEpoch {
            scope_id: BulkheadScopeId::TaskGroup(h.group_storm),
            current_epoch: 0,
            supplied_epoch: 0,
        })
    );
    h.controller
        .reset_poisoned_bulkhead(BulkheadScopeId::TaskGroup(h.group_storm), 1)
        .expect("valid reset epoch 1");
    let snap = h
        .controller
        .bulkhead_snapshot(BulkheadScopeId::TaskGroup(h.group_storm))
        .expect("snap");
    assert_eq!(snap.health, BulkheadHealthState::Healthy);
    assert_eq!(snap.reset_epoch, 1);
}

#[test]
fn bulkhead_failure_budget_poisons_faulty_task_group_and_evicts_waiters_while_preserving_peers_and_reset()
 {
    let mut h = setup_bulkhead_harness();
    let storm_route = WorkloadBulkheadRoute {
        tenant_id: h.tenant_a,
        workspace_id: h.workspace_a,
        task_group_id: h.group_storm, // max_consecutive_failures = 2, max_total_failures = 3
        provider_id: Some(h.provider_b),
    };

    // Acquire 3 active slots + 2 queued waiters on group_storm.
    let mut active_leases = Vec::new();
    for _ in 0..3 {
        if let BulkheadAdmissionDecision::Acquired { lease_id, .. } = h
            .controller
            .acquire_workload(storm_route)
            .expect("acquire storm")
        {
            active_leases.push(lease_id);
        }
    }
    for _ in 0..2 {
        assert!(matches!(
            h.controller.acquire_workload(storm_route).expect("queue"),
            BulkheadAdmissionDecision::Queued { .. }
        ));
    }

    // Fail #1 -> consecutive=1, total=1 (promotes 1st waiter).
    let r1 = h
        .controller
        .complete_workload_lease(
            active_leases[0],
            &BulkheadExecutionOutcome::Failure {
                attributed_scopes: vec![BulkheadScopeId::TaskGroup(h.group_storm)],
            },
            2_000,
        )
        .expect("fail 1");
    assert_eq!(r1.newly_poisoned_scopes, Vec::new());
    assert_eq!(r1.promoted_tickets.len(), 1);
    let promoted_lease = r1.promoted_tickets[0].lease_id;

    // Success #1 -> resets consecutive=0, keeps total=1 (promotes 2nd waiter).
    let s1 = h
        .controller
        .complete_workload_lease(active_leases[1], &BulkheadExecutionOutcome::Success, 2_010)
        .expect("success resets consecutive");
    assert_eq!(s1.promoted_tickets.len(), 1);

    // Enqueue one more waiter now that group_storm is back at 3/3 active.
    assert!(matches!(
        h.controller
            .acquire_workload(storm_route)
            .expect("queue waiter"),
        BulkheadAdmissionDecision::Queued { .. }
    ));

    // Fail #2 -> consecutive=1, total=2 (not yet poisoned).
    let r2 = h
        .controller
        .complete_workload_lease(
            active_leases[2],
            &BulkheadExecutionOutcome::Failure {
                attributed_scopes: vec![BulkheadScopeId::TaskGroup(h.group_storm)],
            },
            2_020,
        )
        .expect("fail 2");
    assert_eq!(r2.newly_poisoned_scopes, Vec::new());

    // Re-enqueue a waiter that should be evicted when group_storm poisons on Fail #3!
    let _ = h.controller.acquire_workload(storm_route);
    let _ = h.controller.acquire_workload(storm_route);

    // Fail #3 -> consecutive=2 (hits max_consecutive_failures=2 AND total=3) -> POISONED!
    let r3 = h
        .controller
        .complete_workload_lease(
            promoted_lease,
            &BulkheadExecutionOutcome::Failure {
                attributed_scopes: vec![BulkheadScopeId::TaskGroup(h.group_storm)],
            },
            2_030,
        )
        .expect("fail 3 trips circuit");
    assert_eq!(
        r3.newly_poisoned_scopes,
        vec![(
            BulkheadScopeId::TaskGroup(h.group_storm),
            BulkheadTripReason::ConsecutiveFailuresExceeded {
                observed: 2,
                limit: 2,
            },
        )]
    );
    assert!(r3.dropped_poisoned_waiters >= 1);

    // Subsequent group_storm requests fail closed immediately as RejectedPoisoned.
    assert_eq!(
        h.controller
            .acquire_workload(storm_route)
            .expect("poisoned check"),
        BulkheadAdmissionDecision::RejectedPoisoned {
            scope_id: BulkheadScopeId::TaskGroup(h.group_storm),
            reason: BulkheadTripReason::ConsecutiveFailuresExceeded {
                observed: 2,
                limit: 2,
            },
        }
    );

    verify_sibling_isolation_and_monotonic_reset(&mut h);
}

#[test]
fn safety_tcb_system_reserve_is_100_percent_untouchable_under_global_non_tcb_exhaustion() {
    let tcb_rg = ResourceGroupId::from_bytes(id16(0xFE, 0));
    // total_system_slots = 10, reserved_tcb_slots = 4 => non_tcb_ceiling = 6.
    let mut controller = BulkheadController::new(
        10,
        SafetyTcbSystemReserve {
            resource_group_id: tcb_rg,
            reserved_tcb_slots: 4,
        },
    )
    .expect("valid controller");
    assert_eq!(controller.non_tcb_system_ceiling(), 6);

    let budget = FailureBudget {
        max_consecutive_failures: 5,
        max_total_failures: 10,
    };

    // Register 2 large Tenants whose individual concurrency caps (10 each) exceed non_tcb_ceiling (6).
    let mut routes = Vec::new();
    for idx in 1..=2u32 {
        let tenant = FanoutTenantId::from_bytes(id16(0x70, idx));
        let ws = WorkspaceId::from_bytes(id16(0x71, idx));
        let tg = TaskGroupId::from_bytes(id16(0x72, idx));
        controller
            .register_bulkhead(BulkheadSpec {
                scope_id: BulkheadScopeId::Tenant(tenant),
                resource_group_id: ResourceGroupId::from_bytes(id16(0x80, idx)),
                max_concurrent: 10,
                max_queue_depth: 4,
                failure_budget: budget,
            })
            .expect("tenant");
        controller
            .register_bulkhead(BulkheadSpec {
                scope_id: BulkheadScopeId::Workspace(ws),
                resource_group_id: ResourceGroupId::from_bytes(id16(0x81, idx)),
                max_concurrent: 10,
                max_queue_depth: 4,
                failure_budget: budget,
            })
            .expect("ws");
        controller
            .register_bulkhead(BulkheadSpec {
                scope_id: BulkheadScopeId::TaskGroup(tg),
                resource_group_id: ResourceGroupId::from_bytes(id16(0x82, idx)),
                max_concurrent: 10,
                max_queue_depth: 4,
                failure_budget: budget,
            })
            .expect("tg");
        routes.push(WorkloadBulkheadRoute {
            tenant_id: tenant,
            workspace_id: ws,
            task_group_id: tg,
            provider_id: None,
        });
    }

    // Tenant 1 acquires 6 slots, hitting non_tcb_system_ceiling = 6.
    for _ in 0..6 {
        assert!(matches!(
            controller
                .acquire_workload(routes[0])
                .expect("acquire up to ceiling"),
            BulkheadAdmissionDecision::Acquired { .. }
        ));
    }
    assert_eq!(controller.active_non_tcb_slots(), 6);

    // Tenant 2 (with 0/10 local usage!) is blocked at the global non-TCB ceiling — NEVER allowed
    // to dip into the 4 reserved SafetyTcb slots!
    assert_eq!(
        controller
            .acquire_workload(routes[1])
            .expect("ceiling check"),
        BulkheadAdmissionDecision::RejectedNonTcbSystemCeilingExhausted { non_tcb_ceiling: 6 }
    );

    // All 4 SafetyTcb slots remain 100% available and acquirable!
    assert_eq!(controller.available_tcb_slots(), 4);
    let mut tcb_leases = Vec::new();
    for _ in 0..4 {
        let (lease_id, rg) = controller
            .acquire_safety_tcb_slot()
            .expect("TCB reserve intact");
        assert_eq!(rg, tcb_rg);
        tcb_leases.push(lease_id);
    }
    assert!(controller.acquire_safety_tcb_slot().is_none());

    for lease_id in tcb_leases {
        controller
            .release_safety_tcb_slot(lease_id)
            .expect("release TCB slot");
    }
    assert_eq!(controller.available_tcb_slots(), 4);
}
