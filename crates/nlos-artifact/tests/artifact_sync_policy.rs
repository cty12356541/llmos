mod support;

use nlos_artifact::{
    ARTIFACT_TRANSFER_PACKAGE_DOMAIN, ArtifactBlobPayload, ArtifactConflictStrategy,
    ArtifactDeletionSemantics, ArtifactError, ArtifactLocalSyncState, ArtifactMergeStrategy,
    ArtifactRemoteSyncCandidate, ArtifactReplicaAuthority, ArtifactReplicaSyncOutcome,
    ArtifactStore, ArtifactSyncDecision, ArtifactSyncError, ArtifactSyncPolicy,
    ArtifactSyncPolicyError, ArtifactSyncPolicyRegistry, CollectOrphanBlobsDecision,
    CollectOrphanBlobsRequest, ContentDigest, CreateArtifactSpec, CrossStoreArtifactSyncReport,
    ExportArtifactTransferRequest, HeadState, ImportArtifactTransferRequest,
    PolicyRegistrationDecision, ProvenanceSourceKind, PublishStagedRevisionRequest,
    ReplicaSyncRetentionSet, StageRevisionRequest, StagedRevisionState, SyncArtifactReplicaRequest,
    collect_orphan_blobs_with_sync_roots, compute_conflict_digest, evaluate_artifact_sync,
    export_artifact_transfer_package, export_replica_revision_bundle,
    import_artifact_transfer_package, read_retained_sync_blob, sync_artifact_replica_head,
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

fn import_request_for<'a>(
    package: &'a nlos_artifact::ArtifactTransferPackage,
    expected_local_state: Option<&'a ArtifactLocalSyncState>,
    seed: u8,
    synced_at_ms: u64,
) -> ImportArtifactTransferRequest<'a> {
    ImportArtifactTransferRequest {
        package,
        expected_local_state,
        task_id: TaskId::from_bytes([0x60_u8.wrapping_add(seed); 16]),
        permit_id: CommitPermitId::from_bytes([0x70_u8.wrapping_add(seed); 16]),
        write_set_root: ContentDigest::from_bytes([0x80_u8.wrapping_add(seed); 32]),
        idempotency_key: IdempotencyKey::from_bytes([0x90_u8.wrapping_add(seed); 16]),
        synced_at_ms,
    }
}

fn write_unreferenced_orphan_blob(dir: &TestStoreDir, payload: &[u8]) -> ContentDigest {
    let orphan_digest = ContentDigest::of_bytes(payload);
    let blob_path = dir.artifact_blob(orphan_digest);
    std::fs::create_dir_all(blob_path.parent().expect("shard dir")).expect("create shard dir");
    std::fs::write(&blob_path, payload).expect("write orphan blob");
    orphan_digest
}

fn open_replica_pair(prefix: &str) -> (TestStoreDir, ArtifactStore, TestStoreDir, ArtifactStore) {
    let origin_dir = TestStoreDir::new(&format!("{prefix}-origin"));
    let target_dir = TestStoreDir::new(&format!("{prefix}-target"));
    let origin_store = ArtifactStore::open(origin_dir.root()).expect("open origin store");
    let target_store = ArtifactStore::open(target_dir.root()).expect("open target store");
    (origin_dir, origin_store, target_dir, target_store)
}

fn peer_registry(
    content_type: &str,
    conflict_strategy: ArtifactConflictStrategy,
    deletion_semantics: ArtifactDeletionSemantics,
) -> ArtifactSyncPolicyRegistry {
    let mut registry = ArtifactSyncPolicyRegistry::new();
    registry
        .register(ArtifactSyncPolicy {
            content_type: content_type.to_string(),
            merge_strategy: ArtifactMergeStrategy::FastForwardLineageOnly,
            conflict_strategy,
            replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
            deletion_semantics,
        })
        .expect("register peer policy");
    registry
}

fn gc_req(tag: u8, collected_at_ms: u64) -> CollectOrphanBlobsRequest {
    CollectOrphanBlobsRequest {
        idempotency_key: IdempotencyKey::from_bytes([tag; 16]),
        collected_at_ms,
    }
}

fn single_authority_registry(content_type: &str) -> ArtifactSyncPolicyRegistry {
    let mut registry = ArtifactSyncPolicyRegistry::new();
    registry
        .register(ArtifactSyncPolicy {
            content_type: content_type.to_string(),
            merge_strategy: ArtifactMergeStrategy::AuthoritativeReplicaWins,
            conflict_strategy: ArtifactConflictStrategy::DeferToAuthoritativeReplica,
            replica_authority: ArtifactReplicaAuthority::SingleAuthoritativeCell {
                authoritative_cell_id: AUTH_CELL,
            },
            deletion_semantics: ArtifactDeletionSemantics::Forbidden,
        })
        .expect("register single-authoritative policy");
    registry
}

fn seed_divergent_pair(
    origin_store: &ArtifactStore,
    target_store: &ArtifactStore,
    spec: &CreateArtifactSpec,
    base_bytes: &[u8],
    origin_v2: &[u8],
    target_v2: &[u8],
) {
    origin_store
        .create_artifact(spec.clone())
        .expect("create origin");
    target_store
        .create_artifact(spec.clone())
        .expect("create target");
    origin_store
        .put_revision(put(spec.artifact_id, 0, base_bytes))
        .expect("origin v1");
    target_store
        .put_revision(put(spec.artifact_id, 0, base_bytes))
        .expect("target v1");
    origin_store
        .put_revision(put(spec.artifact_id, 1, origin_v2))
        .expect("origin v2");
    target_store
        .put_revision(put(spec.artifact_id, 1, target_v2))
        .expect("target v2");
}

#[test]
fn transfer_package_multi_revision_export_import_and_idempotent_reimport() {
    assert_eq!(
        ARTIFACT_TRANSFER_PACKAGE_DOMAIN,
        b"llmos/artifact-transfer-package/v1"
    );
    let (_origin_dir, origin_store, _target_dir, target_store) = open_replica_pair("xfer");
    let registry = peer_registry(
        "text/markdown",
        ArtifactConflictStrategy::QuarantineDivergentBranch,
        ArtifactDeletionSemantics::TombstoneRevisionRequired,
    );

    let spec = artifact_spec_with_content_type(20, "text/markdown");
    origin_store
        .create_artifact(spec.clone())
        .expect("create on origin");
    let (v1, v2, v3) = (bytes(0x11, 200), bytes(0x22, 240), bytes(0x33, 280));
    for (idx, payload) in [&v1, &v2, &v3].into_iter().enumerate() {
        origin_store
            .put_revision(put(spec.artifact_id, idx as u64, payload))
            .expect("put revision");
    }

    let pkg_v3 = export_artifact_transfer_package(
        &origin_store,
        ExportArtifactTransferRequest::head(spec.artifact_id, CELL_LEFT, 7, READ_NOW_MS),
    )
    .expect("export v1..=v3 package");
    assert_eq!(pkg_v3.revisions.len(), 3);
    assert_eq!(pkg_v3.blobs.len(), 3);
    assert_eq!(pkg_v3.producer_epoch, 7);
    assert_eq!(pkg_v3.package_digest, pkg_v3.compute_digest());

    let mut retention = ReplicaSyncRetentionSet::new();
    let report = import_artifact_transfer_package(
        &target_store,
        &registry,
        &mut retention,
        import_request_for(&pkg_v3, None, 1, 6_000),
    )
    .expect("import v1..=v3 into empty target store");

    let CrossStoreArtifactSyncReport::ImportedFastForward {
        artifact_id,
        prior_local_head,
        new_head,
        imported_revisions,
        materialized_blob_digests,
        publication_receipts,
    } = report
    else {
        panic!("expected ImportedFastForward, got {report:?}");
    };
    assert_eq!(artifact_id, spec.artifact_id);
    assert_eq!(prior_local_head, None);
    assert_eq!(new_head, pkg_v3.bundle.remote_head);
    assert_eq!(imported_revisions, vec![1, 2, 3]);
    assert_eq!(materialized_blob_digests.len(), 3);
    assert_eq!(publication_receipts.len(), 3);

    for (rev, expected) in [(1, &v1), (2, &v2), (3, &v3)] {
        assert_eq!(
            &target_store
                .get_revision(spec.artifact_id, rev, READ_NOW_MS)
                .expect("read synced revision"),
            expected
        );
    }
    assert_eq!(
        target_store
            .inspect_provenance(spec.artifact_id, 3)
            .expect("provenance v3")
            .source_kind,
        ProvenanceSourceKind::OwnerDerived
    );

    let replay = target_store
        .import_artifact_transfer_package(
            &registry,
            &mut retention,
            import_request_for(&pkg_v3, None, 2, 6_100),
        )
        .expect("idempotent re-import");
    assert_eq!(
        replay,
        CrossStoreArtifactSyncReport::NoOpConverged {
            artifact_id: spec.artifact_id,
            head: pkg_v3.bundle.remote_head,
        }
    );
}

#[test]
fn transfer_package_tampered_blob_and_package_digest_fail_closed_with_zero_writes() {
    let (_origin_dir, origin_store, _target_dir, target_store) = open_replica_pair("tamper");
    let registry = peer_registry(
        "text/markdown",
        ArtifactConflictStrategy::FailClosedConflict,
        ArtifactDeletionSemantics::TombstoneRevisionRequired,
    );

    let spec = artifact_spec_with_content_type(21, "text/markdown");
    origin_store
        .create_artifact(spec.clone())
        .expect("create on origin");
    origin_store
        .put_revision(put(spec.artifact_id, 0, &bytes(0x41, 128)))
        .expect("put v1");
    origin_store
        .put_revision(put(spec.artifact_id, 1, &bytes(0x42, 128)))
        .expect("put v2");

    let valid_pkg = origin_store
        .export_artifact_transfer_package(ExportArtifactTransferRequest::head(
            spec.artifact_id,
            CELL_LEFT,
            1,
            READ_NOW_MS,
        ))
        .expect("export valid package");

    let mut retention = ReplicaSyncRetentionSet::new();

    let mut tampered_blob_pkg = valid_pkg.clone();
    tampered_blob_pkg.blobs[0].bytes[0] ^= 0xff;
    assert!(matches!(
        import_artifact_transfer_package(
            &target_store,
            &registry,
            &mut retention,
            import_request_for(&tampered_blob_pkg, None, 3, 6_000),
        ),
        Err(ArtifactSyncError::TamperedBlobDigest { .. })
    ));

    let mut tampered_tip_pkg = valid_pkg.clone();
    tampered_tip_pkg.bundle.bytes[0] ^= 0xaa;
    assert!(matches!(
        import_artifact_transfer_package(
            &target_store,
            &registry,
            &mut retention,
            import_request_for(&tampered_tip_pkg, None, 4, 6_000),
        ),
        Err(ArtifactSyncError::TamperedBlobDigest { .. })
    ));

    let mut swapped_blob_pkg = valid_pkg.clone();
    swapped_blob_pkg.blobs[0] = ArtifactBlobPayload::new(b"evil replacement payload".to_vec());
    assert!(matches!(
        import_artifact_transfer_package(
            &target_store,
            &registry,
            &mut retention,
            import_request_for(&swapped_blob_pkg, None, 5, 6_000),
        ),
        Err(ArtifactSyncError::TamperedPackageDigest { .. })
    ));

    let mut tampered_digest_pkg = valid_pkg;
    tampered_digest_pkg.package_digest[31] ^= 0x01;
    assert!(matches!(
        import_artifact_transfer_package(
            &target_store,
            &registry,
            &mut retention,
            import_request_for(&tampered_digest_pkg, None, 6, 6_000),
        ),
        Err(ArtifactSyncError::TamperedPackageDigest { .. })
    ));

    assert!(matches!(
        target_store.inspect_artifact(spec.artifact_id),
        Err(ArtifactError::ArtifactNotFound(_))
    ));
    let recovery = target_store.recover().expect("recover clean target store");
    assert_eq!(recovery.missing_blobs.len(), 0);
    assert_eq!(recovery.orphan_blobs.len(), 0);
    assert_eq!(recovery.removed_tmp_files, 0);
    assert!(retention.is_empty());
}

#[test]
fn transfer_package_quarantine_conflict_preserves_both_blobs_across_sync_aware_gc() {
    let (_origin_dir, origin_store, target_dir, target_store) = open_replica_pair("quar");
    let registry = peer_registry(
        "application/vnd.llmos.note+json",
        ArtifactConflictStrategy::QuarantineDivergentBranch,
        ArtifactDeletionSemantics::TombstoneRevisionRequired,
    );

    let spec = artifact_spec_with_content_type(22, "application/vnd.llmos.note+json");
    let (origin_v2, target_v2) = (bytes(0xa2, 160), bytes(0xb2, 160));
    seed_divergent_pair(
        &origin_store,
        &target_store,
        &spec,
        &bytes(0x10, 128),
        &origin_v2,
        &target_v2,
    );
    let target_v2_head = target_store
        .resolve_head(spec.artifact_id, READ_NOW_MS)
        .expect("resolve target")
        .expect("target v2 head");

    let pkg_from_origin = origin_store
        .export_artifact_transfer_package(ExportArtifactTransferRequest::head(
            spec.artifact_id,
            CELL_LEFT,
            2,
            READ_NOW_MS,
        ))
        .expect("export origin v2");

    let mut retention = ReplicaSyncRetentionSet::new();
    let report = target_store
        .import_artifact_transfer_package(
            &registry,
            &mut retention,
            import_request_for(&pkg_from_origin, None, 10, 7_000),
        )
        .expect("quarantine conflict isolated");

    let CrossStoreArtifactSyncReport::ConflictIsolated {
        conflict_receipt,
        preserved_both_blobs,
        ..
    } = report
    else {
        panic!("expected ConflictIsolated, got {report:?}");
    };
    assert!(preserved_both_blobs);
    assert_eq!(conflict_receipt.local_head, target_v2_head);
    assert_eq!(
        conflict_receipt.remote_head,
        pkg_from_origin.bundle.remote_head
    );

    let stray_orphan = write_unreferenced_orphan_blob(&target_dir, b"crashed uncommitted residue");
    let gc_during_conflict =
        collect_orphan_blobs_with_sync_roots(&target_store, gc_req(0xd1, 8_000), &retention)
            .expect("sync-aware GC while conflict active");
    assert!(matches!(
        gc_during_conflict,
        CollectOrphanBlobsDecision::Collected(_)
    ));
    assert_eq!(
        gc_during_conflict.receipt().collected_digests,
        vec![stray_orphan]
    );
    assert!(target_dir.artifact_blob(target_v2_head.digest).is_file());
    let remote_digest = pkg_from_origin.bundle.remote_head.digest;
    assert!(target_dir.artifact_blob(remote_digest).is_file());
    assert_eq!(
        read_retained_sync_blob(&target_store, &retention, remote_digest, 8_000)
            .expect("read quarantined remote blob"),
        origin_v2
    );

    retention
        .resolve_conflict(conflict_receipt.conflict_digest)
        .expect("resolve conflict");
    let gc_after_resolution = target_store
        .collect_orphan_blobs_with_sync_roots(gc_req(0xd2, 9_000), &retention)
        .expect("sync-aware GC after conflict resolution");
    assert_eq!(
        gc_after_resolution.receipt().collected_digests,
        vec![remote_digest]
    );
    assert!(target_dir.artifact_blob(target_v2_head.digest).is_file());
}

#[test]
fn transfer_package_fail_closed_conflict_receipt_also_guards_competing_blobs_until_resolved() {
    let (_left_dir, left_store, _right_dir, right_store) = open_replica_pair("fc");
    let registry = peer_registry(
        "text/markdown",
        ArtifactConflictStrategy::FailClosedConflict,
        ArtifactDeletionSemantics::TombstoneRevisionRequired,
    );

    let spec = artifact_spec_with_content_type(23, "text/markdown");
    let (left_v2, right_v2) = (bytes(0xc1, 144), bytes(0xc2, 144));
    seed_divergent_pair(
        &left_store,
        &right_store,
        &spec,
        &bytes(0x20, 96),
        &left_v2,
        &right_v2,
    );

    let pkg_left = left_store
        .export_artifact_transfer_package(ExportArtifactTransferRequest::head(
            spec.artifact_id,
            CELL_LEFT,
            3,
            READ_NOW_MS,
        ))
        .expect("export left");
    let pkg_right = right_store
        .export_artifact_transfer_package(ExportArtifactTransferRequest::head(
            spec.artifact_id,
            CELL_RIGHT,
            3,
            READ_NOW_MS,
        ))
        .expect("export right");

    let mut left_retention = ReplicaSyncRetentionSet::new();
    let mut right_retention = ReplicaSyncRetentionSet::new();
    let rep_on_left = left_store
        .import_artifact_transfer_package(
            &registry,
            &mut left_retention,
            import_request_for(&pkg_right, None, 11, 7_500),
        )
        .expect("left isolates right conflict");
    let rep_on_right = right_store
        .import_artifact_transfer_package(
            &registry,
            &mut right_retention,
            import_request_for(&pkg_left, None, 12, 7_500),
        )
        .expect("right isolates left conflict");

    let (
        CrossStoreArtifactSyncReport::ConflictIsolated {
            conflict_receipt: rec_left,
            preserved_both_blobs: kept_left,
            ..
        },
        CrossStoreArtifactSyncReport::ConflictIsolated {
            conflict_receipt: rec_right,
            preserved_both_blobs: kept_right,
            ..
        },
    ) = (rep_on_left, rep_on_right)
    else {
        panic!("expected bidirectional ConflictIsolated");
    };
    assert!(kept_left && kept_right);
    assert_eq!(rec_left.conflict_digest, rec_right.conflict_digest);

    let gc_left = left_store
        .collect_orphan_blobs_with_sync_roots(
            CollectOrphanBlobsRequest {
                idempotency_key: IdempotencyKey::from_bytes([0xd3; 16]),
                collected_at_ms: 8_500,
            },
            &left_retention,
        )
        .expect("GC on left");
    assert_eq!(gc_left.receipt().collected_digests.len(), 0);
    assert_eq!(
        left_store
            .read_retained_sync_blob(&left_retention, pkg_right.bundle.remote_head.digest, 8_500)
            .expect("left reads pinned right blob"),
        right_v2
    );
}

#[test]
fn transfer_package_tombstone_with_retention_propagates_and_enforces_expiry_window() {
    let (_origin_dir, origin_store, _target_dir, target_store) = open_replica_pair("tomb");
    let registry = peer_registry(
        "application/vnd.llmos.ephemeral+bin",
        ArtifactConflictStrategy::QuarantineDivergentBranch,
        ArtifactDeletionSemantics::GcAfterRetentionBarrier,
    );

    let mut spec = artifact_spec_with_content_type(24, "application/vnd.llmos.ephemeral+bin");
    spec.created_at_ms = 1_000;
    origin_store
        .create_artifact(spec.clone())
        .expect("create on origin");
    let v1_bytes = bytes(0x55, 192);
    origin_store
        .put_revision(put(spec.artifact_id, 0, &v1_bytes))
        .expect("put v1 on origin");

    let mut retention = ReplicaSyncRetentionSet::new();
    let pkg_v1 = origin_store
        .export_artifact_transfer_package(ExportArtifactTransferRequest::head(
            spec.artifact_id,
            CELL_LEFT,
            1,
            3_000,
        ))
        .expect("export v1");
    target_store
        .import_artifact_transfer_package(
            &registry,
            &mut retention,
            import_request_for(&pkg_v1, None, 20, 3_500),
        )
        .expect("import v1 on target");

    origin_store
        .put_revision(put(spec.artifact_id, 1, b"llmos/tombstone/v1:deleted"))
        .expect("put tombstone v2 on origin");
    let pkg_tomb = origin_store
        .export_artifact_transfer_package(ExportArtifactTransferRequest::tombstone_head(
            spec.artifact_id,
            2,
            CELL_LEFT,
            2,
            4_000,
            Some(10_000),
            4_500,
        ))
        .expect("export tombstone package");

    let tomb_report = target_store
        .import_artifact_transfer_package(
            &registry,
            &mut retention,
            import_request_for(&pkg_tomb, None, 21, 5_000),
        )
        .expect("import tombstone on target");
    assert!(matches!(
        tomb_report,
        CrossStoreArtifactSyncReport::TombstoneApplied {
            artifact_id,
            retention_until_ms: Some(10_000),
            ..
        } if artifact_id == spec.artifact_id
    ));

    assert_eq!(
        target_store
            .get_revision(spec.artifact_id, 1, 6_000)
            .expect("v1 readable inside retention window"),
        v1_bytes
    );
    assert!(matches!(
        retention.evaluate_tombstone_reclamation(&registry, spec.artifact_id, CELL_LEFT, 6_000, 0),
        Err(ArtifactSyncError::RetentionBarrierUnsatisfied {
            retention_expired: false,
            active_ref_count: 0,
            ..
        })
    ));

    assert!(matches!(
        target_store.get_revision(spec.artifact_id, 1, 10_001),
        Err(ArtifactError::RetentionExpired {
            artifact_id,
            expires_at_ms: 10_000,
        }) if artifact_id == spec.artifact_id
    ));
    assert_eq!(
        retention
            .evaluate_tombstone_reclamation(&registry, spec.artifact_id, CELL_LEFT, 10_001, 0)
            .expect("reclaim eligible after retention expiry"),
        ArtifactSyncDecision::ReclaimEligible {
            artifact_id: spec.artifact_id,
        }
    );
}

#[test]
fn tombstone_retention_gc_guard_retains_superseded_sync_blobs_until_window_expires() {
    let (_origin_dir, origin_store, target_dir, target_store) = open_replica_pair("tomb-gc");
    let registry = peer_registry(
        "application/vnd.llmos.ephemeral+bin",
        ArtifactConflictStrategy::QuarantineDivergentBranch,
        ArtifactDeletionSemantics::GcAfterRetentionBarrier,
    );

    let spec = artifact_spec_with_content_type(25, "application/vnd.llmos.ephemeral+bin");
    let (remote_v2, local_v2) = (bytes(0xd4, 160), bytes(0xd5, 160));
    seed_divergent_pair(
        &origin_store,
        &target_store,
        &spec,
        &bytes(0x30, 128),
        &remote_v2,
        &local_v2,
    );

    let pkg_origin = origin_store
        .export_artifact_transfer_package(ExportArtifactTransferRequest::head(
            spec.artifact_id,
            CELL_LEFT,
            1,
            READ_NOW_MS,
        ))
        .expect("export origin v2");

    let mut retention = ReplicaSyncRetentionSet::new();
    let CrossStoreArtifactSyncReport::ConflictIsolated {
        conflict_receipt, ..
    } = target_store
        .import_artifact_transfer_package(
            &registry,
            &mut retention,
            import_request_for(&pkg_origin, None, 30, 5_000),
        )
        .expect("isolate conflict")
    else {
        panic!("expected ConflictIsolated");
    };

    retention
        .retire_conflict_into_tombstone_retention(
            conflict_receipt.conflict_digest,
            6_000,
            Some(10_000),
        )
        .expect("retire conflict into tombstone retention window");

    let gc_before_expiry = target_store
        .collect_orphan_blobs_with_sync_roots(
            CollectOrphanBlobsRequest {
                idempotency_key: IdempotencyKey::from_bytes([0xe1; 16]),
                collected_at_ms: 9_999,
            },
            &retention,
        )
        .expect("GC before tombstone retention expiry");
    assert_eq!(gc_before_expiry.receipt().collected_digests.len(), 0);
    assert_eq!(
        target_store
            .read_retained_sync_blob(&retention, pkg_origin.bundle.remote_head.digest, 9_999)
            .expect("retained before expiry"),
        remote_v2
    );

    let gc_after_expiry = target_store
        .collect_orphan_blobs_with_sync_roots(
            CollectOrphanBlobsRequest {
                idempotency_key: IdempotencyKey::from_bytes([0xe2; 16]),
                collected_at_ms: 10_001,
            },
            &retention,
        )
        .expect("GC after tombstone retention expiry");
    assert_eq!(
        gc_after_expiry.receipt().collected_digests,
        vec![pkg_origin.bundle.remote_head.digest]
    );
    assert!(
        !target_dir
            .artifact_blob(pkg_origin.bundle.remote_head.digest)
            .exists()
    );
    assert_eq!(retention.prune_expired_tombstones(10_001).len(), 1);
}

#[test]
fn transfer_package_single_authoritative_cell_and_expected_local_state_cas_enforced() {
    let (_auth_dir, auth_store, _rogue_dir, rogue_store) = open_replica_pair("auth-src");
    let follower_dir = TestStoreDir::new("follower-cell");
    let follower_store = ArtifactStore::open(follower_dir.root()).expect("open follower");
    let registry = single_authority_registry("application/sqlite3");

    let spec = artifact_spec_with_content_type(26, "application/sqlite3");
    for (store, tag) in [(&auth_store, 0x77_u8), (&rogue_store, 0x88_u8)] {
        store.create_artifact(spec.clone()).expect("create");
        store
            .put_revision(put(spec.artifact_id, 0, &bytes(tag, 128)))
            .expect("put v1");
    }

    let rogue_pkg = rogue_store
        .export_artifact_transfer_package(ExportArtifactTransferRequest::head(
            spec.artifact_id,
            CELL_RIGHT,
            1,
            READ_NOW_MS,
        ))
        .expect("export rogue pkg");
    let mut retention = ReplicaSyncRetentionSet::new();
    assert!(matches!(
        follower_store.import_artifact_transfer_package(
            &registry,
            &mut retention,
            import_request_for(&rogue_pkg, None, 40, 6_000),
        ),
        Err(ArtifactSyncError::NonAuthoritativeReplicaRejected { .. })
    ));
    assert!(matches!(
        follower_store.inspect_artifact(spec.artifact_id),
        Err(ArtifactError::ArtifactNotFound(_))
    ));

    let auth_pkg = auth_store
        .export_artifact_transfer_package(ExportArtifactTransferRequest::head(
            spec.artifact_id,
            AUTH_CELL,
            5,
            READ_NOW_MS,
        ))
        .expect("export auth pkg");
    let stale_local = ArtifactLocalSyncState {
        artifact_id: spec.artifact_id,
        content_type: "application/sqlite3".to_string(),
        local_head: Some(head(1, 0x99)),
    };
    assert!(matches!(
        follower_store.import_artifact_transfer_package(
            &registry,
            &mut retention,
            import_request_for(&auth_pkg, Some(&stale_local), 41, 6_100),
        ),
        Err(ArtifactSyncError::LocalHeadStateDrift {
            expected: Some(_),
            actual: None,
        })
    ));

    let empty_local = ArtifactLocalSyncState {
        artifact_id: spec.artifact_id,
        content_type: "application/sqlite3".to_string(),
        local_head: None,
    };
    assert!(matches!(
        follower_store
            .import_artifact_transfer_package(
                &registry,
                &mut retention,
                import_request_for(&auth_pkg, Some(&empty_local), 42, 6_200),
            )
            .expect("import from authoritative cell"),
        CrossStoreArtifactSyncReport::ImportedFastForward { .. }
    ));

    auth_store
        .put_revision(put(spec.artifact_id, 1, b"illegal tombstone"))
        .expect("auth v2");
    let forbidden_tomb = auth_store
        .export_artifact_transfer_package(ExportArtifactTransferRequest::tombstone_head(
            spec.artifact_id,
            2,
            AUTH_CELL,
            6,
            6_300,
            None,
            READ_NOW_MS,
        ))
        .expect("export tombstone");
    assert!(matches!(
        follower_store.import_artifact_transfer_package(
            &registry,
            &mut retention,
            import_request_for(&forbidden_tomb, None, 43, 6_400),
        ),
        Err(ArtifactSyncError::DeletionForbidden { .. })
    ));
}
