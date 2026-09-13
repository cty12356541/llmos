//! Read-side unification tests for the five Task commit receipt flavors
//! (`[W26-003]`).
//!
//! The unified [`nlos_task::TaskCommitReceipt`] enum is a pure read-side
//! aggregation: it never alters persisted facts and only adds a deterministic
//! digest plus a human-readable `Display`.
//!
//! Brief assertion (c) — "digest survives serialization round-trip" — is
//! **skipped and registered**: none of the five wrapped receipt types
//! (`TaskReceiptRecord`, `ArtifactTaskCommitReceipt`,
//! `SemanticTaskCommitReceipt`, `ResourceTaskCommitReceipt`,
//! `SemanticResourceTaskCommitReceipt`) implements `Serialize`/`Deserialize`
//! (the crate has no serde dependency at all), so no serialized rebuild path
//! exists to test. Clone-based rebuild equality is asserted instead in the
//! determinism test.

use nlos_task::{
    ArtifactTaskCommitReceipt, NestedArtifactPublicationReceipt, NestedResourceCostReceipt,
    NestedSemanticPublicationReceipt, ReceiptOutcome, ResourceTaskCommitReceipt,
    SemanticResourceTaskCommitReceipt, SemanticTaskCommitReceipt, TaskCommitReceipt,
    TaskReceiptRecord, TaskWriteSetSemanticTarget,
};
use nlos_types::{
    ArtifactId, CallId, CommitPermitId, Generation, NamespaceId, OperationId, QuoteId, ReceiptId,
    ReservationId, ResourceAccountId, SemanticEventId, TaskAttemptId, TaskId,
};

fn plain_task_receipt(seed: u8) -> TaskReceiptRecord {
    TaskReceiptRecord {
        receipt_id: ReceiptId::from_bytes([seed; 16]),
        task_id: TaskId::from_bytes([0x01; 16]),
        permit_id: Some(CommitPermitId::from_bytes([0x02; 16])),
        attempt_id: TaskAttemptId::from_bytes([0x03; 16]),
        attempt_generation: Generation::INITIAL,
        group_binding: None,
        participant_registry_binding: None,
        outcome: ReceiptOutcome::Committed,
        prior_head_commit_seq: 7,
        prior_effect_history_root: [0x04; 32],
        prior_retry_fence_epoch: 3,
        new_head_commit_seq: 8,
        new_effect_history_root: [0x05; 32],
        new_retry_fence_epoch: 4,
        created_at_ms: 1_000,
    }
}

fn artifact_publication(
    seed: u8,
    task_id: TaskId,
    permit_id: CommitPermitId,
) -> NestedArtifactPublicationReceipt {
    NestedArtifactPublicationReceipt {
        receipt_id: ReceiptId::from_bytes([0xa0 + seed; 16]),
        staging_id: [0xa1 + seed; 16],
        artifact_id: ArtifactId::from_bytes([0xa2 + seed; 16]),
        revision: 3,
        digest: [0xa3 + seed; 32],
        size_bytes: 128,
        task_id,
        permit_id,
        write_set_root: [0xa4 + seed; 32],
        prior_head_revision: 2,
        prior_head_digest: Some([0xa5 + seed; 32]),
        new_head_revision: 3,
        new_head_digest: [0xa6 + seed; 32],
        created_at_ms: 5_000,
    }
}

fn semantic_publication(
    seed: u8,
    task_id: TaskId,
    permit_id: CommitPermitId,
) -> NestedSemanticPublicationReceipt {
    NestedSemanticPublicationReceipt {
        receipt_id: ReceiptId::from_bytes([0xb0 + seed; 16]),
        task_id,
        permit_id,
        write_set_root: [0xb1 + seed; 32],
        event_id: SemanticEventId::from_bytes([0xb2 + seed; 32]),
        target: TaskWriteSetSemanticTarget::Namespace(NamespaceId::from_bytes([0xb3; 16])),
        log_seq: 4,
        admission_receipt_id: ReceiptId::from_bytes([0xb4 + seed; 16]),
        durability_receipt_id: Some(ReceiptId::from_bytes([0xb5 + seed; 16])),
        semantic_checkpoint_after: [0xb6 + seed; 32],
        created_at_ms: 6_000,
    }
}

fn resource_cost_receipt(seed: u8) -> NestedResourceCostReceipt {
    let reservation_id = ReservationId::from_bytes([0xc0 + seed; 16]);
    let operation_id = OperationId::from_bytes([0xc1 + seed; 16]);
    let activation_receipt_id = ReceiptId::from_bytes([0xc2 + seed; 16]);
    NestedResourceCostReceipt {
        reservation_id,
        account_id: ResourceAccountId::from_bytes([0xc3 + seed; 16]),
        quote_id: QuoteId::from_bytes([0xc4 + seed; 16]),
        call_id: CallId::from_bytes([0xc5 + seed; 16]),
        operation_id,
        upper_bound: 1_000,
        activation: nlos_resource::ActivationReceipt {
            receipt_id: activation_receipt_id,
            reservation_id,
            operation_id,
            activated_at_ms: 1_400,
        },
        consumptions: vec![nlos_resource::ConsumptionReceipt {
            receipt_id: ReceiptId::from_bytes([0xc6 + seed; 16]),
            reservation_id,
            operation_id,
            activation_receipt_id,
            sequence: 1,
            cumulative_usage: 40,
            consumed_at_ms: 1_500,
        }],
        finalization: nlos_resource::FinalizationReceipt {
            receipt_id: ReceiptId::from_bytes([0xc7 + seed; 16]),
            reservation_id,
            operation_id,
            activation_receipt_id,
            effect_closed_proof_digest: [0xc8 + seed; 32],
            high_water_seq: 1,
            final_seq: 1,
            high_water: 40,
            final_usage: 40,
            refund_credit: 960,
            finalized_at_ms: 1_600,
        },
    }
}

/// One minimal receipt per enum variant, all sharing the same base
/// `TaskReceiptRecord` identity so pairwise digest distinctness is proven
/// against the variant discriminant plus the nested evidence, not against
/// diverging base identities.
fn sample_receipts() -> [TaskCommitReceipt; 5] {
    let base = plain_task_receipt(0x11);
    let task_id = base.task_id;
    let permit_id = base.permit_id.expect("permit id");
    [
        TaskCommitReceipt::Plain(base.clone()),
        TaskCommitReceipt::Artifact(ArtifactTaskCommitReceipt {
            task_receipt: base.clone(),
            artifact_publications: vec![artifact_publication(1, task_id, permit_id)],
        }),
        TaskCommitReceipt::Semantic(SemanticTaskCommitReceipt {
            task_receipt: base.clone(),
            semantic_publications: vec![semantic_publication(1, task_id, permit_id)],
        }),
        TaskCommitReceipt::Resource(ResourceTaskCommitReceipt {
            task_receipt: base.clone(),
            resource_cost_receipts: vec![resource_cost_receipt(1)],
        }),
        TaskCommitReceipt::SemanticResource(SemanticResourceTaskCommitReceipt {
            task_receipt: base,
            semantic_publications: vec![semantic_publication(2, task_id, permit_id)],
            resource_cost_receipts: vec![resource_cost_receipt(2)],
        }),
    ]
}

#[test]
fn digest_is_deterministic_across_equal_constructions() {
    let first = sample_receipts();
    let second = sample_receipts();
    for (index, (a, b)) in first.iter().zip(&second).enumerate() {
        assert_eq!(
            a.commit_receipt_digest(),
            b.commit_receipt_digest(),
            "variant {index}: digest must be equal for equal constructions"
        );
        assert_eq!(
            a.clone().commit_receipt_digest(),
            a.commit_receipt_digest(),
            "variant {index}: digest must survive a clone rebuild"
        );
    }
}

#[test]
fn variant_digests_are_pairwise_distinct() {
    let receipts = sample_receipts();
    for i in 0..receipts.len() {
        for j in (i + 1)..receipts.len() {
            assert_ne!(
                receipts[i].commit_receipt_digest(),
                receipts[j].commit_receipt_digest(),
                "variants {i} and {j} must not share a digest"
            );
        }
    }
}

#[test]
fn same_variant_digest_tracks_identity_fields() {
    let plain_a = TaskCommitReceipt::Plain(plain_task_receipt(0x21));
    let plain_b = TaskCommitReceipt::Plain(plain_task_receipt(0x22));
    assert_ne!(
        plain_a.commit_receipt_digest(),
        plain_b.commit_receipt_digest(),
        "differing receipt_id must change the digest"
    );

    let mut changed_outcome = plain_task_receipt(0x21);
    changed_outcome.outcome = ReceiptOutcome::FailedAfterEffect;
    assert_ne!(
        plain_a.commit_receipt_digest(),
        TaskCommitReceipt::Plain(changed_outcome).commit_receipt_digest(),
        "differing outcome must change the digest"
    );

    let mut empty_nested = plain_task_receipt(0x21);
    empty_nested.new_head_commit_seq += 1;
    assert_ne!(
        plain_a.commit_receipt_digest(),
        TaskCommitReceipt::Plain(empty_nested).commit_receipt_digest(),
        "differing new_head_commit_seq must change the digest"
    );
}

#[test]
fn display_contains_variant_name() {
    let receipts = sample_receipts();
    let expected_names = [
        "Plain",
        "Artifact",
        "Semantic",
        "Resource",
        "SemanticResource",
    ];
    for (receipt, name) in receipts.iter().zip(expected_names) {
        let rendered = receipt.to_string();
        assert!(
            rendered.contains(name),
            "Display {rendered:?} must contain variant name {name}"
        );
    }
}
