mod support;

use nlos_artifact::{
    ArtifactConflictStrategy, ArtifactDeletionSemantics, ArtifactLocalSyncState,
    ArtifactMergeStrategy, ArtifactRemoteSyncCandidate, ArtifactReplicaAuthority,
    ArtifactReplicaSyncOutcome, ArtifactStore, ArtifactSyncDecision, ArtifactSyncError,
    ArtifactSyncPolicy, ArtifactSyncPolicyError, ArtifactSyncPolicyRegistry, ContentDigest,
    CreateArtifactSpec, HeadState, PolicyRegistrationDecision, ProvenanceSourceKind,
    PublishStagedRevisionRequest, StageRevisionRequest, StagedRevisionState,
    SyncArtifactReplicaRequest, compute_conflict_digest, evaluate_artifact_sync,
    export_replica_revision_bundle, sync_artifact_replica_head,
};
use nlos_types::{ApplicationId, CommitPermitId, IdempotencyKey, TaskId};

use support::{READ_NOW_MS, TestStoreDir, artifact_id, bytes, put};

const CELL_LEFT: [u8; 16] = [0x11; 16];
const CELL_RIGHT: [u8; 16] = [0x22; 16];
const AUTH_CELL: [u8; 16] = [0xaa; 16];

fn digest(seed: u8) -> ContentDigest {
    ContentDigest::of_bytes(&[seed; 32])
}

fn head(revision: u64, seed: u8) -> HeadState {
    HeadState {
        revision,
        digest: digest(seed),
    }
}

fn artifact_spec_with_content_type(seed: u8, content_type: &str) -> CreateArtifactSpec {
    CreateArtifactSpec {
        artifact_id: artifact_id(seed),
        idempotency_key: IdempotencyKey::from_bytes([0xa0_u8.wrapping_add(seed); 16]),
        content_type: content_type.to_string(),
        application_id: Some(ApplicationId::from_bytes([0xb0_u8.wrapping_add(seed); 16])),
        owner: Some(format!("user-{seed}")),
        created_at_ms: 1_000 + u64::from(seed),
    }
}

#[test]
fn undeclared_content_type_and_invalid_policy_combinations_fail_closed() {
    let mut registry = ArtifactSyncPolicyRegistry::new();
    assert!(registry.is_empty());

    let empty_ct = ArtifactSyncPolicy {
        content_type: String::new(),
        merge_strategy: ArtifactMergeStrategy::ImmutableContentAddressed,
        conflict_strategy: ArtifactConflictStrategy::FailClosedConflict,
        replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
        deletion_semantics: ArtifactDeletionSemantics::Forbidden,
    };
    assert_eq!(
        registry.register(empty_ct),
        Err(ArtifactSyncPolicyError::InvalidContentType)
    );

    let defer_with_symmetric = ArtifactSyncPolicy {
        content_type: "text/markdown".to_string(),
        merge_strategy: ArtifactMergeStrategy::FastForwardLineageOnly,
        conflict_strategy: ArtifactConflictStrategy::DeferToAuthoritativeReplica,
        replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
        deletion_semantics: ArtifactDeletionSemantics::TombstoneRevisionRequired,
    };
    assert_eq!(
        registry.register(defer_with_symmetric),
        Err(
            ArtifactSyncPolicyError::IncompatibleAuthorityForConflictStrategy {
                conflict_strategy: ArtifactConflictStrategy::DeferToAuthoritativeReplica,
                replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
            }
        )
    );

    let auth_wins_with_symmetric = ArtifactSyncPolicy {
        content_type: "application/sqlite3".to_string(),
        merge_strategy: ArtifactMergeStrategy::AuthoritativeReplicaWins,
        conflict_strategy: ArtifactConflictStrategy::FailClosedConflict,
        replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
        deletion_semantics: ArtifactDeletionSemantics::Forbidden,
    };
    assert_eq!(
        registry.register(auth_wins_with_symmetric),
        Err(
            ArtifactSyncPolicyError::IncompatibleAuthorityForMergeStrategy {
                merge_strategy: ArtifactMergeStrategy::AuthoritativeReplicaWins,
                replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
            }
        )
    );

    let zero_cell = ArtifactSyncPolicy {
        content_type: "application/sqlite3".to_string(),
        merge_strategy: ArtifactMergeStrategy::AuthoritativeReplicaWins,
        conflict_strategy: ArtifactConflictStrategy::DeferToAuthoritativeReplica,
        replica_authority: ArtifactReplicaAuthority::SingleAuthoritativeCell {
            authoritative_cell_id: [0_u8; 16],
        },
        deletion_semantics: ArtifactDeletionSemantics::Forbidden,
    };
    assert_eq!(
        registry.register(zero_cell),
        Err(ArtifactSyncPolicyError::ZeroAuthoritativeCellId)
    );

    let pkg_policy = ArtifactSyncPolicy {
        content_type: "application/vnd.llmos.package+cbor".to_string(),
        merge_strategy: ArtifactMergeStrategy::ImmutableContentAddressed,
        conflict_strategy: ArtifactConflictStrategy::FailClosedConflict,
        replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
        deletion_semantics: ArtifactDeletionSemantics::Forbidden,
    };
    let first = registry.register(pkg_policy.clone()).expect("register");
    assert!(matches!(first, PolicyRegistrationDecision::Registered(_)));
    let replay = registry.register(pkg_policy.clone()).expect("replay");
    assert!(matches!(replay, PolicyRegistrationDecision::Replayed(_)));
    assert_eq!(registry.len(), 1);

    let conflicting = ArtifactSyncPolicy {
        deletion_semantics: ArtifactDeletionSemantics::TombstoneRevisionRequired,
        ..pkg_policy
    };
    assert_eq!(
        registry.register(conflicting),
        Err(ArtifactSyncPolicyError::ConflictingPolicyRegistration {
            content_type: "application/vnd.llmos.package+cbor".to_string(),
        })
    );

    let local = ArtifactLocalSyncState {
        artifact_id: artifact_id(1),
        content_type: "application/x-unregistered".to_string(),
        local_head: None,
    };
    let remote =
        ArtifactRemoteSyncCandidate::revision_update(CELL_LEFT, head(1, 0x10), None, vec![]);
    assert!(matches!(
        evaluate_artifact_sync(&registry, &local, &remote),
        Err(ArtifactSyncError::Policy(
            ArtifactSyncPolicyError::UnregisteredContentType { content_type }
        )) if content_type == "application/x-unregistered"
    ));
}

#[test]
fn immutable_content_addressed_happy_path_replay_and_corrupt_digest_mismatch() {
    let mut registry = ArtifactSyncPolicyRegistry::new();
    registry
        .register(ArtifactSyncPolicy {
            content_type: "application/vnd.llmos.package+cbor".to_string(),
            merge_strategy: ArtifactMergeStrategy::ImmutableContentAddressed,
            conflict_strategy: ArtifactConflictStrategy::FailClosedConflict,
            replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
            deletion_semantics: ArtifactDeletionSemantics::Forbidden,
        })
        .expect("register package policy");

    let rev1 = head(1, 0x01);
    let rev2 = head(2, 0x02);
    let rev3 = head(3, 0x03);
    let corrupt_rev1 = head(1, 0x99);

    let pkg_empty = ArtifactLocalSyncState {
        artifact_id: artifact_id(2),
        content_type: "application/vnd.llmos.package+cbor".to_string(),
        local_head: None,
    };
    let r1_cand = ArtifactRemoteSyncCandidate::revision_update(CELL_LEFT, rev1, None, vec![]);
    assert_eq!(
        evaluate_artifact_sync(&registry, &pkg_empty, &r1_cand).expect("accept rev 1"),
        ArtifactSyncDecision::AcceptRemoteHead {
            artifact_id: artifact_id(2),
            prior_local_head: None,
            new_head: rev1,
            origin_cell_id: CELL_LEFT,
            is_tombstone: false,
        }
    );

    let pkg_at_r1 = ArtifactLocalSyncState {
        local_head: Some(rev1),
        ..pkg_empty
    };
    assert_eq!(
        evaluate_artifact_sync(&registry, &pkg_at_r1, &r1_cand).expect("idempotent replay"),
        ArtifactSyncDecision::NoOpAlreadySynced {
            artifact_id: artifact_id(2),
            head: rev1,
        }
    );

    let corrupt_cand =
        ArtifactRemoteSyncCandidate::revision_update(CELL_LEFT, corrupt_rev1, None, vec![]);
    assert!(matches!(
        evaluate_artifact_sync(&registry, &pkg_at_r1, &corrupt_cand),
        Err(ArtifactSyncError::CorruptRevisionDigestMismatch {
            artifact_id: id,
            revision: 1,
            local_digest,
            remote_digest,
        }) if id == artifact_id(2)
            && local_digest == rev1.digest
            && remote_digest == corrupt_rev1.digest
    ));

    let r2_cand =
        ArtifactRemoteSyncCandidate::revision_update(CELL_LEFT, rev2, Some(rev1), vec![rev1]);
    assert_eq!(
        evaluate_artifact_sync(&registry, &pkg_at_r1, &r2_cand).expect("accept rev 2"),
        ArtifactSyncDecision::AcceptRemoteHead {
            artifact_id: artifact_id(2),
            prior_local_head: Some(rev1),
            new_head: rev2,
            origin_cell_id: CELL_LEFT,
            is_tombstone: false,
        }
    );

    let r3_gap =
        ArtifactRemoteSyncCandidate::revision_update(CELL_LEFT, rev3, Some(rev2), vec![rev1, rev2]);
    assert!(matches!(
        evaluate_artifact_sync(&registry, &pkg_at_r1, &r3_gap),
        Err(ArtifactSyncError::NonContiguousRevision {
            expected_revision: 2,
            remote_revision: 3,
        })
    ));
}

#[test]
fn fast_forward_lineage_happy_path_replay_and_corrupt_digest_mismatch() {
    let mut registry = ArtifactSyncPolicyRegistry::new();
    registry
        .register(ArtifactSyncPolicy {
            content_type: "text/markdown".to_string(),
            merge_strategy: ArtifactMergeStrategy::FastForwardLineageOnly,
            conflict_strategy: ArtifactConflictStrategy::FailClosedConflict,
            replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
            deletion_semantics: ArtifactDeletionSemantics::TombstoneRevisionRequired,
        })
        .expect("register markdown policy");

    let rev1 = head(1, 0x01);
    let rev2 = head(2, 0x02);
    let rev3 = head(3, 0x03);
    let corrupt_rev1 = head(1, 0x99);

    let md_at_r1 = ArtifactLocalSyncState {
        artifact_id: artifact_id(3),
        content_type: "text/markdown".to_string(),
        local_head: Some(rev1),
    };
    let r1_cand = ArtifactRemoteSyncCandidate::revision_update(CELL_LEFT, rev1, None, vec![]);
    assert_eq!(
        evaluate_artifact_sync(&registry, &md_at_r1, &r1_cand).expect("md replay"),
        ArtifactSyncDecision::NoOpAlreadySynced {
            artifact_id: artifact_id(3),
            head: rev1,
        }
    );

    let corrupt_cand =
        ArtifactRemoteSyncCandidate::revision_update(CELL_LEFT, corrupt_rev1, None, vec![]);
    assert!(matches!(
        evaluate_artifact_sync(&registry, &md_at_r1, &corrupt_cand),
        Err(ArtifactSyncError::CorruptRevisionDigestMismatch {
            artifact_id: id,
            revision: 1,
            ..
        }) if id == artifact_id(3)
    ));

    let r3_fast_forward =
        ArtifactRemoteSyncCandidate::revision_update(CELL_LEFT, rev3, Some(rev2), vec![rev1, rev2]);
    assert_eq!(
        evaluate_artifact_sync(&registry, &md_at_r1, &r3_fast_forward)
            .expect("fast-forward rev 1->3"),
        ArtifactSyncDecision::AcceptRemoteHead {
            artifact_id: artifact_id(3),
            prior_local_head: Some(rev1),
            new_head: rev3,
            origin_cell_id: CELL_LEFT,
            is_tombstone: false,
        }
    );
}

#[test]
fn divergent_branches_emit_order_invariant_fail_closed_conflict_receipts() {
    let mut registry = ArtifactSyncPolicyRegistry::new();
    registry
        .register(ArtifactSyncPolicy {
            content_type: "text/markdown".to_string(),
            merge_strategy: ArtifactMergeStrategy::FastForwardLineageOnly,
            conflict_strategy: ArtifactConflictStrategy::FailClosedConflict,
            replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
            deletion_semantics: ArtifactDeletionSemantics::TombstoneRevisionRequired,
        })
        .expect("register fail-closed policy");

    let common_r1 = head(1, 0x01);
    let left_branch_rev2 = head(2, 0xa2);
    let right_branch_rev2 = head(2, 0xb2);
    let right_branch_rev3 = head(3, 0xb3);

    let state_left = ArtifactLocalSyncState {
        artifact_id: artifact_id(4),
        content_type: "text/markdown".to_string(),
        local_head: Some(left_branch_rev2),
    };
    let cand_from_right = ArtifactRemoteSyncCandidate::revision_update(
        CELL_RIGHT,
        right_branch_rev3,
        Some(right_branch_rev2),
        vec![common_r1, right_branch_rev2],
    );

    let state_right = ArtifactLocalSyncState {
        artifact_id: artifact_id(4),
        content_type: "text/markdown".to_string(),
        local_head: Some(right_branch_rev3),
    };
    let cand_from_left = ArtifactRemoteSyncCandidate::revision_update(
        CELL_LEFT,
        left_branch_rev2,
        Some(common_r1),
        vec![common_r1],
    );

    let decision_left_sees_right = evaluate_artifact_sync(&registry, &state_left, &cand_from_right)
        .expect("left evaluates right under FailClosedConflict");
    let decision_right_sees_left = evaluate_artifact_sync(&registry, &state_right, &cand_from_left)
        .expect("right evaluates left under FailClosedConflict");

    let (
        ArtifactSyncDecision::RefuseConflict(receipt_lr),
        ArtifactSyncDecision::RefuseConflict(receipt_rl),
    ) = (decision_left_sees_right, decision_right_sees_left)
    else {
        panic!("expected RefuseConflict decisions in both directions");
    };

    assert_eq!(receipt_lr.conflict_digest, receipt_rl.conflict_digest);
    assert_eq!(
        receipt_lr.conflict_digest,
        compute_conflict_digest(
            artifact_id(4),
            "text/markdown",
            left_branch_rev2,
            right_branch_rev3
        )
    );
    assert_eq!(
        receipt_lr.conflict_digest,
        compute_conflict_digest(
            artifact_id(4),
            "text/markdown",
            right_branch_rev3,
            left_branch_rev2
        )
    );
}

#[test]
fn divergent_branches_emit_order_invariant_quarantine_conflict_receipts() {
    let mut registry = ArtifactSyncPolicyRegistry::new();
    registry
        .register(ArtifactSyncPolicy {
            content_type: "application/vnd.llmos.note+json".to_string(),
            merge_strategy: ArtifactMergeStrategy::FastForwardLineageOnly,
            conflict_strategy: ArtifactConflictStrategy::QuarantineDivergentBranch,
            replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
            deletion_semantics: ArtifactDeletionSemantics::TombstoneRevisionRequired,
        })
        .expect("register quarantine policy");

    let common_r1 = head(1, 0x01);
    let left_branch_rev2 = head(2, 0xa2);
    let right_branch_rev2 = head(2, 0xb2);

    let q_state_left = ArtifactLocalSyncState {
        artifact_id: artifact_id(5),
        content_type: "application/vnd.llmos.note+json".to_string(),
        local_head: Some(left_branch_rev2),
    };
    let q_cand_right = ArtifactRemoteSyncCandidate::revision_update(
        CELL_RIGHT,
        right_branch_rev2,
        Some(common_r1),
        vec![common_r1],
    );
    let q_state_right = ArtifactLocalSyncState {
        artifact_id: artifact_id(5),
        content_type: "application/vnd.llmos.note+json".to_string(),
        local_head: Some(right_branch_rev2),
    };
    let q_cand_left = ArtifactRemoteSyncCandidate::revision_update(
        CELL_LEFT,
        left_branch_rev2,
        Some(common_r1),
        vec![common_r1],
    );

    let q_lr = evaluate_artifact_sync(&registry, &q_state_left, &q_cand_right)
        .expect("quarantine left sees right");
    let q_rl = evaluate_artifact_sync(&registry, &q_state_right, &q_cand_left)
        .expect("quarantine right sees left");
    let (
        ArtifactSyncDecision::QuarantineConflict(q_receipt_lr),
        ArtifactSyncDecision::QuarantineConflict(q_receipt_rl),
    ) = (q_lr, q_rl)
    else {
        panic!("expected QuarantineConflict decisions in both directions");
    };
    assert_eq!(q_receipt_lr.conflict_digest, q_receipt_rl.conflict_digest);
}

#[test]
fn single_authoritative_cell_accepts_authoritative_and_rejects_non_authoritative_divergent_heads() {
    let mut registry = ArtifactSyncPolicyRegistry::new();
    registry
        .register(ArtifactSyncPolicy {
            content_type: "application/sqlite3".to_string(),
            merge_strategy: ArtifactMergeStrategy::AuthoritativeReplicaWins,
            conflict_strategy: ArtifactConflictStrategy::DeferToAuthoritativeReplica,
            replica_authority: ArtifactReplicaAuthority::SingleAuthoritativeCell {
                authoritative_cell_id: AUTH_CELL,
            },
            deletion_semantics: ArtifactDeletionSemantics::Forbidden,
        })
        .expect("register authoritative-wins policy");
    registry
        .register(ArtifactSyncPolicy {
            content_type: "application/vnd.llmos.config+cbor".to_string(),
            merge_strategy: ArtifactMergeStrategy::FastForwardLineageOnly,
            conflict_strategy: ArtifactConflictStrategy::DeferToAuthoritativeReplica,
            replica_authority: ArtifactReplicaAuthority::SingleAuthoritativeCell {
                authoritative_cell_id: AUTH_CELL,
            },
            deletion_semantics: ArtifactDeletionSemantics::TombstoneRevisionRequired,
        })
        .expect("register defer-to-authoritative policy");

    let local_r2 = head(2, 0x20);
    let divergent_r2 = head(2, 0x21);
    let parent_r1 = head(1, 0x10);

    for content_type in ["application/sqlite3", "application/vnd.llmos.config+cbor"] {
        let local = ArtifactLocalSyncState {
            artifact_id: artifact_id(6),
            content_type: content_type.to_string(),
            local_head: Some(local_r2),
        };

        let from_auth = ArtifactRemoteSyncCandidate::revision_update(
            AUTH_CELL,
            divergent_r2,
            Some(parent_r1),
            vec![parent_r1],
        );
        assert_eq!(
            evaluate_artifact_sync(&registry, &local, &from_auth).expect("authoritative cell wins"),
            ArtifactSyncDecision::AcceptRemoteHead {
                artifact_id: artifact_id(6),
                prior_local_head: Some(local_r2),
                new_head: divergent_r2,
                origin_cell_id: AUTH_CELL,
                is_tombstone: false,
            }
        );

        let from_non_auth = ArtifactRemoteSyncCandidate::revision_update(
            CELL_RIGHT,
            divergent_r2,
            Some(parent_r1),
            vec![parent_r1],
        );
        let err = evaluate_artifact_sync(&registry, &local, &from_non_auth)
            .expect_err("non-authoritative cell must be rejected");
        match err {
            ArtifactSyncError::NonAuthoritativeReplicaRejected {
                artifact_id: id,
                origin_cell_id,
                authoritative_cell_id,
                conflict_receipt: Some(receipt),
            } => {
                assert_eq!(id, artifact_id(6));
                assert_eq!(origin_cell_id, CELL_RIGHT);
                assert_eq!(authoritative_cell_id, AUTH_CELL);
                assert_eq!(
                    receipt.conflict_digest,
                    compute_conflict_digest(artifact_id(6), content_type, local_r2, divergent_r2)
                );
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }
}

#[test]
fn deletion_semantics_forbidden_and_tombstone_revision_required_enforced() {
    let mut registry = ArtifactSyncPolicyRegistry::new();
    registry
        .register(ArtifactSyncPolicy {
            content_type: "application/vnd.llmos.package+cbor".to_string(),
            merge_strategy: ArtifactMergeStrategy::ImmutableContentAddressed,
            conflict_strategy: ArtifactConflictStrategy::FailClosedConflict,
            replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
            deletion_semantics: ArtifactDeletionSemantics::Forbidden,
        })
        .expect("register forbidden deletion policy");
    registry
        .register(ArtifactSyncPolicy {
            content_type: "text/markdown".to_string(),
            merge_strategy: ArtifactMergeStrategy::FastForwardLineageOnly,
            conflict_strategy: ArtifactConflictStrategy::FailClosedConflict,
            replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
            deletion_semantics: ArtifactDeletionSemantics::TombstoneRevisionRequired,
        })
        .expect("register tombstone deletion policy");

    let rev1 = head(1, 0x01);
    let tombstone_rev2 = head(2, 0xfe);

    let pkg_local = ArtifactLocalSyncState {
        artifact_id: artifact_id(7),
        content_type: "application/vnd.llmos.package+cbor".to_string(),
        local_head: Some(rev1),
    };
    let silent = ArtifactRemoteSyncCandidate::silent_unversioned_deletion(CELL_LEFT);
    let tombstone = ArtifactRemoteSyncCandidate::tombstone_deletion(
        CELL_LEFT,
        tombstone_rev2,
        Some(rev1),
        vec![rev1],
    );
    let gc_ready = ArtifactRemoteSyncCandidate::physical_gc_reclamation(CELL_LEFT, true, 0);

    assert!(matches!(
        evaluate_artifact_sync(&registry, &pkg_local, &silent),
        Err(ArtifactSyncError::DeletionForbidden { .. })
    ));
    assert!(matches!(
        evaluate_artifact_sync(&registry, &pkg_local, &tombstone),
        Err(ArtifactSyncError::DeletionForbidden { .. })
    ));
    assert!(matches!(
        evaluate_artifact_sync(&registry, &pkg_local, &gc_ready),
        Err(ArtifactSyncError::DeletionForbidden { .. })
    ));

    let md_local = ArtifactLocalSyncState {
        artifact_id: artifact_id(8),
        content_type: "text/markdown".to_string(),
        local_head: Some(rev1),
    };
    assert!(matches!(
        evaluate_artifact_sync(&registry, &md_local, &silent),
        Err(ArtifactSyncError::TombstoneRevisionRequired { .. })
    ));
    assert!(matches!(
        evaluate_artifact_sync(&registry, &md_local, &gc_ready),
        Err(ArtifactSyncError::PhysicalGcNotPermitted { .. })
    ));
    assert_eq!(
        evaluate_artifact_sync(&registry, &md_local, &tombstone).expect("tombstone accepted"),
        ArtifactSyncDecision::AcceptRemoteHead {
            artifact_id: artifact_id(8),
            prior_local_head: Some(rev1),
            new_head: tombstone_rev2,
            origin_cell_id: CELL_LEFT,
            is_tombstone: true,
        }
    );

    let non_contiguous_tombstone = ArtifactRemoteSyncCandidate::tombstone_deletion(
        CELL_LEFT,
        head(3, 0xff),
        Some(tombstone_rev2),
        vec![rev1, tombstone_rev2],
    );
    assert!(matches!(
        evaluate_artifact_sync(&registry, &md_local, &non_contiguous_tombstone),
        Err(ArtifactSyncError::NonContiguousRevision {
            expected_revision: 2,
            remote_revision: 3,
        })
    ));
}

#[test]
fn deletion_semantics_gc_after_retention_barrier_enforced() {
    let mut registry = ArtifactSyncPolicyRegistry::new();
    registry
        .register(ArtifactSyncPolicy {
            content_type: "application/vnd.llmos.ephemeral+bin".to_string(),
            merge_strategy: ArtifactMergeStrategy::ImmutableContentAddressed,
            conflict_strategy: ArtifactConflictStrategy::FailClosedConflict,
            replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
            deletion_semantics: ArtifactDeletionSemantics::GcAfterRetentionBarrier,
        })
        .expect("register gc barrier deletion policy");

    let eph_local = ArtifactLocalSyncState {
        artifact_id: artifact_id(9),
        content_type: "application/vnd.llmos.ephemeral+bin".to_string(),
        local_head: Some(head(1, 0x01)),
    };
    let gc_not_expired = ArtifactRemoteSyncCandidate::physical_gc_reclamation(CELL_LEFT, false, 0);
    let gc_active_refs = ArtifactRemoteSyncCandidate::physical_gc_reclamation(CELL_LEFT, true, 2);
    let gc_ready = ArtifactRemoteSyncCandidate::physical_gc_reclamation(CELL_LEFT, true, 0);

    assert!(matches!(
        evaluate_artifact_sync(&registry, &eph_local, &gc_not_expired),
        Err(ArtifactSyncError::RetentionBarrierUnsatisfied {
            retention_expired: false,
            active_ref_count: 0,
            ..
        })
    ));
    assert!(matches!(
        evaluate_artifact_sync(&registry, &eph_local, &gc_active_refs),
        Err(ArtifactSyncError::RetentionBarrierUnsatisfied {
            retention_expired: true,
            active_ref_count: 2,
            ..
        })
    ));
    assert_eq!(
        evaluate_artifact_sync(&registry, &eph_local, &gc_ready).expect("reclaim eligible"),
        ArtifactSyncDecision::ReclaimEligible {
            artifact_id: artifact_id(9),
        }
    );
}

fn stage_and_publish_v1(store: &ArtifactStore, spec: &CreateArtifactSpec, payload: &[u8]) {
    let staged = store
        .stage_revision(StageRevisionRequest {
            artifact_id: spec.artifact_id,
            expected_head_revision: 0,
            bytes: payload,
            task_id: TaskId::from_bytes([0x11; 16]),
            permit_id: CommitPermitId::from_bytes([0x21; 16]),
            write_set_root: ContentDigest::from_bytes([0x31; 32]),
            idempotency_key: IdempotencyKey::from_bytes([0x41; 16]),
            created_at_ms: 2_000,
        })
        .expect("stage remote v1")
        .record()
        .clone();
    store
        .publish_staged_revision(PublishStagedRevisionRequest {
            staging_id: staged.staging_id,
            task_id: staged.task_id,
            permit_id: staged.permit_id,
            write_set_root: staged.write_set_root,
            published_at_ms: 2_100,
        })
        .expect("publish remote v1");
}

fn sync_request_for(
    bundle: &nlos_artifact::ReplicaRevisionBundle,
    seed: u8,
    synced_at_ms: u64,
) -> SyncArtifactReplicaRequest<'_> {
    SyncArtifactReplicaRequest {
        bundle,
        task_id: TaskId::from_bytes([0x60_u8.wrapping_add(seed); 16]),
        permit_id: CommitPermitId::from_bytes([0x70_u8.wrapping_add(seed); 16]),
        write_set_root: ContentDigest::from_bytes([0x80_u8.wrapping_add(seed); 32]),
        idempotency_key: IdempotencyKey::from_bytes([0x90_u8.wrapping_add(seed); 16]),
        synced_at_ms,
    }
}

#[test]
fn end_to_end_two_store_replica_sync_stages_and_publishes_verified_revisions() {
    let remote_dir = TestStoreDir::new("sync-remote");
    let local_dir = TestStoreDir::new("sync-local");
    let remote_store = ArtifactStore::open(remote_dir.root()).expect("open remote store");
    let local_store = ArtifactStore::open(local_dir.root()).expect("open local store");

    let mut registry = ArtifactSyncPolicyRegistry::new();
    registry
        .register(ArtifactSyncPolicy {
            content_type: "text/markdown".to_string(),
            merge_strategy: ArtifactMergeStrategy::FastForwardLineageOnly,
            conflict_strategy: ArtifactConflictStrategy::FailClosedConflict,
            replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
            deletion_semantics: ArtifactDeletionSemantics::TombstoneRevisionRequired,
        })
        .expect("register markdown policy");

    let spec = artifact_spec_with_content_type(10, "text/markdown");
    remote_store
        .create_artifact(spec.clone())
        .expect("create remote artifact");
    local_store
        .create_artifact(spec.clone())
        .expect("create local artifact");

    let v1_bytes = bytes(0x51, 320);
    stage_and_publish_v1(&remote_store, &spec, &v1_bytes);

    let bundle_v1 = export_replica_revision_bundle(
        &remote_store,
        spec.artifact_id,
        1,
        CELL_LEFT,
        false,
        READ_NOW_MS,
    )
    .expect("export remote v1");

    let mut tampered_bundle = bundle_v1.clone();
    tampered_bundle.bytes.push(0xff);
    assert!(matches!(
        sync_artifact_replica_head(
            &local_store,
            &registry,
            sync_request_for(&tampered_bundle, 1, 3_000)
        ),
        Err(ArtifactSyncError::RemotePayloadDigestMismatch { .. })
    ));
    assert_eq!(
        local_store
            .resolve_head(spec.artifact_id, READ_NOW_MS)
            .expect("head still empty"),
        None
    );

    let sync_req_v1 = sync_request_for(&bundle_v1, 1, 3_000);
    let outcome_v1 =
        sync_artifact_replica_head(&local_store, &registry, sync_req_v1).expect("sync v1");
    match &outcome_v1 {
        ArtifactReplicaSyncOutcome::Synced {
            publication_receipt,
            staged_record,
            is_tombstone,
        } => {
            assert!(!is_tombstone);
            assert_eq!(publication_receipt.new_head_revision, 1);
            assert_eq!(
                publication_receipt.new_head_digest,
                bundle_v1.remote_head.digest
            );
            assert_eq!(staged_record.state, StagedRevisionState::Published);
        }
        other => panic!("expected Synced outcome, got {other:?}"),
    }

    assert_eq!(
        local_store
            .resolve_head(spec.artifact_id, READ_NOW_MS)
            .expect("resolve local head"),
        Some(bundle_v1.remote_head)
    );
    assert_eq!(
        local_store
            .get_revision(spec.artifact_id, 1, READ_NOW_MS)
            .expect("read synced v1 bytes"),
        v1_bytes
    );
    let prov = local_store
        .inspect_provenance(spec.artifact_id, 1)
        .expect("inspect synced provenance");
    assert_eq!(prov.source_kind, ProvenanceSourceKind::OwnerDerived);

    let replay_v1 =
        sync_artifact_replica_head(&local_store, &registry, sync_req_v1).expect("replay v1");
    assert_eq!(
        replay_v1,
        ArtifactReplicaSyncOutcome::NoOpAlreadySynced {
            artifact_id: spec.artifact_id,
            head: bundle_v1.remote_head,
        }
    );
}

#[test]
fn end_to_end_two_store_divergent_heads_preserve_local_canonical_head() {
    let remote_dir = TestStoreDir::new("sync-div-remote");
    let local_dir = TestStoreDir::new("sync-div-local");
    let remote_store = ArtifactStore::open(remote_dir.root()).expect("open remote store");
    let local_store = ArtifactStore::open(local_dir.root()).expect("open local store");

    let mut registry = ArtifactSyncPolicyRegistry::new();
    registry
        .register(ArtifactSyncPolicy {
            content_type: "text/markdown".to_string(),
            merge_strategy: ArtifactMergeStrategy::FastForwardLineageOnly,
            conflict_strategy: ArtifactConflictStrategy::FailClosedConflict,
            replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
            deletion_semantics: ArtifactDeletionSemantics::TombstoneRevisionRequired,
        })
        .expect("register markdown policy");

    let spec = artifact_spec_with_content_type(11, "text/markdown");
    remote_store
        .create_artifact(spec.clone())
        .expect("create remote");
    local_store
        .create_artifact(spec.clone())
        .expect("create local");

    let base_bytes = bytes(0x61, 128);
    remote_store
        .put_revision(put(spec.artifact_id, 0, &base_bytes))
        .expect("remote put v1");
    local_store
        .put_revision(put(spec.artifact_id, 0, &base_bytes))
        .expect("local put v1");

    let local_v2_bytes = bytes(0x72, 128);
    local_store
        .put_revision(put(spec.artifact_id, 1, &local_v2_bytes))
        .expect("local put v2");
    let local_v2_head = local_store
        .resolve_head(spec.artifact_id, READ_NOW_MS)
        .expect("resolve local v2")
        .expect("local v2 head");

    let remote_v2_bytes = bytes(0x82, 128);
    remote_store
        .put_revision(put(spec.artifact_id, 1, &remote_v2_bytes))
        .expect("remote put v2");
    let bundle_remote_v2 = export_replica_revision_bundle(
        &remote_store,
        spec.artifact_id,
        2,
        CELL_LEFT,
        false,
        READ_NOW_MS,
    )
    .expect("export remote v2");

    let conflict_outcome = sync_artifact_replica_head(
        &local_store,
        &registry,
        SyncArtifactReplicaRequest {
            bundle: &bundle_remote_v2,
            task_id: TaskId::from_bytes([0x62; 16]),
            permit_id: CommitPermitId::from_bytes([0x72; 16]),
            write_set_root: ContentDigest::from_bytes([0x82; 32]),
            idempotency_key: IdempotencyKey::from_bytes([0x92; 16]),
            synced_at_ms: 4_000,
        },
    )
    .expect("conflict evaluation succeeds with RefusedConflict");

    match conflict_outcome {
        ArtifactReplicaSyncOutcome::RefusedConflict(receipt) => {
            assert_eq!(receipt.local_head, local_v2_head);
            assert_eq!(receipt.remote_head, bundle_remote_v2.remote_head);
            assert_eq!(
                receipt.conflict_digest,
                compute_conflict_digest(
                    spec.artifact_id,
                    "text/markdown",
                    local_v2_head,
                    bundle_remote_v2.remote_head,
                )
            );
        }
        other => panic!("expected RefusedConflict, got {other:?}"),
    }

    assert_eq!(
        local_store
            .resolve_head(spec.artifact_id, READ_NOW_MS)
            .expect("local head preserved"),
        Some(local_v2_head)
    );
    assert_eq!(
        local_store
            .get_revision(spec.artifact_id, 2, READ_NOW_MS)
            .expect("local v2 bytes preserved"),
        local_v2_bytes
    );
}
