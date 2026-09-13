//! Read-side unification of the five durable Task commit receipt flavors.
//!
//! The Task authority persists five distinct terminal receipt shapes — the
//! plain permit receipt, the Artifact-aware, Semantic-aware, Resource-aware,
//! and combined Semantic + Resource finalizes. Recovery and inspection
//! surfaces need one nominal type to carry any of them without touching the
//! persisted facts. [`TaskCommitReceipt`] is that pure aggregation: it owns
//! no storage, mutates nothing, and adds only a deterministic digest plus a
//! human-readable [`fmt::Display`].

use std::fmt;

use sha2::{Digest, Sha256};

use crate::commit::{ArtifactTaskCommitReceipt, NestedArtifactPublicationReceipt};
use crate::group::TaskGroupCommitBinding;
use crate::model::{ReceiptOutcome, TaskReceiptRecord, TaskWriteSetSemanticTarget};
use crate::participant::ParticipantRegistryBinding;
use crate::resource_commit::{
    NestedResourceCostReceipt, ResourceTaskCommitReceipt, SemanticResourceTaskCommitReceipt,
};
use crate::semantic_commit::{NestedSemanticPublicationReceipt, SemanticTaskCommitReceipt};

/// Domain-separation prefix of the unified commit-receipt digest. The `/v1`
/// suffix freezes the formula version; any change to the encoding below
/// requires a new domain string.
const DIGEST_DOMAIN: &[u8] = b"llmos/task-commit-receipt/v1";

/// Variant discriminants of the digest encoding.
const PLAIN_DISCRIMINANT: u8 = 1;
const ARTIFACT_DISCRIMINANT: u8 = 2;
const SEMANTIC_DISCRIMINANT: u8 = 3;
const RESOURCE_DISCRIMINANT: u8 = 4;
const SEMANTIC_RESOURCE_DISCRIMINANT: u8 = 5;

/// Option tags of the digest encoding: `0` encodes `None`, `1` prefixes a
/// present value.
const NONE_TAG: u8 = 0;
const SOME_TAG: u8 = 1;

/// Unified read-side view over every durable Task commit receipt flavor.
///
/// This enum aggregates the five persisted terminal receipt shapes without
/// altering them: each variant embeds the exact record returned by the
/// corresponding finalize/replay path. It exists so recovery-plane readers
/// can hold "the commit receipt of an attempt" as one value.
///
/// # Deterministic digest formula
///
/// [`TaskCommitReceipt::commit_receipt_digest`] is
/// `SHA-256(DIGEST_DOMAIN || encoding)` where `DIGEST_DOMAIN` is the ASCII
/// string `llmos/task-commit-receipt/v1` and `encoding` is the following
/// canonical field-by-field big-endian fixed-width byte concatenation. The
/// hash is the workspace `sha2` dependency (already in this crate's tree);
/// no other digest primitive is used.
///
/// 1. Variant discriminant, one byte: `Plain` = 1, `Artifact` = 2,
///    `Semantic` = 3, `Resource` = 4, `SemanticResource` = 5.
/// 2. The base [`TaskReceiptRecord`] encoding (every variant):
///    1. `receipt_id`: 16 bytes;
///    2. `task_id`: 16 bytes;
///    3. `permit_id`: 1-byte option tag, then 16 bytes when present;
///    4. `attempt_id`: 16 bytes;
///    5. `attempt_generation`: `u64` big-endian;
///    6. `group_binding`: 1-byte option tag, then
///       `group_id` (16) || `membership_generation` (`u64` BE) ||
///       `membership_root` (32) || `group_policy_digest` (32) when present;
///    7. `participant_registry_binding`: 1-byte option tag, then
///       `generation` (`u64` BE) || `root` (32) when present;
///    8. `outcome` discriminant, one byte: `Committed` = 0,
///       `FailedBeforeEffect` = 1, `CancelledBeforeEffect` = 2,
///       `Partial` = 3, `PartialEffect` = 4, `FailedAfterEffect` = 5;
///    9. `prior_head_commit_seq`: `u64` big-endian;
///    10. `prior_effect_history_root`: 32 bytes;
///    11. `prior_retry_fence_epoch`: `u64` big-endian;
///    12. `new_head_commit_seq`: `u64` big-endian;
///    13. `new_effect_history_root`: 32 bytes;
///    14. `new_retry_fence_epoch`: `u64` big-endian;
///    15. `created_at_ms`: `i64` big-endian (two's complement).
/// 3. `Artifact` variant: the artifact publication list —
///    `u64` big-endian entry count, then per entry in vector order:
///    `receipt_id` (16) || `staging_id` (16) || `artifact_id` (16) ||
///    `revision` (`u64` BE) || `digest` (32) || `size_bytes` (`u64` BE) ||
///    `task_id` (16) || `permit_id` (16) || `write_set_root` (32) ||
///    `prior_head_revision` (`u64` BE) || `prior_head_digest`
///    (1-byte option tag + 32 when present) || `new_head_revision`
///    (`u64` BE) || `new_head_digest` (32) || `created_at_ms` (`i64` BE).
/// 4. `Semantic` variant: the semantic publication list —
///    `u64` big-endian entry count, then per entry in vector order:
///    `receipt_id` (16) || `task_id` (16) || `permit_id` (16) ||
///    `write_set_root` (32) || `event_id` (32) || `target`
///    (1-byte kind tag: `Namespace` = 1, `Task` = 2, then the 16-byte id) ||
///    `log_seq` (`u64` BE) || `admission_receipt_id` (16) ||
///    `durability_receipt_id` (1-byte option tag + 16 when present) ||
///    `semantic_checkpoint_after` (32) || `created_at_ms` (`u64` BE).
/// 5. `Resource` variant: the resource cost list —
///    `u64` big-endian entry count, then per entry in vector order:
///    `reservation_id` (16) || `account_id` (16) || `quote_id` (16) ||
///    `call_id` (16) || `operation_id` (16) || `upper_bound` (`u64` BE) ||
///    the activation receipt (`receipt_id` 16 || `reservation_id` 16 ||
///    `operation_id` 16 || `activated_at_ms` `u64` BE) ||
///    the consumption list (`u64` BE count, then per entry: `receipt_id` 16
///    || `reservation_id` 16 || `operation_id` 16 ||
///    `activation_receipt_id` 16 || `sequence` `u64` BE ||
///    `cumulative_usage` `u64` BE || `consumed_at_ms` `u64` BE) ||
///    the finalization receipt (`receipt_id` 16 || `reservation_id` 16 ||
///    `operation_id` 16 || `activation_receipt_id` 16 ||
///    `effect_closed_proof_digest` 32 || `high_water_seq` `u64` BE ||
///    `final_seq` `u64` BE || `high_water` `u64` BE || `final_usage` `u64`
///    BE || `refund_credit` `u64` BE || `finalized_at_ms` `u64` BE).
/// 6. `SemanticResource` variant: the semantic publication list encoding of
///    step 4 immediately followed by the resource cost list encoding of
///    step 5.
///
/// Vector order is part of the encoding: the nested lists are persisted in
/// a defined order (publication record order / consumption sequence), and
/// the digest is a function of the record value as stored.
///
/// **Any extension of the variant set or of a nested record's field set
/// must update this formula and its documentation in the same change.**
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskCommitReceipt {
    /// Plain permit-bound terminal receipt (`[TASK-COMMIT-001]` finalize).
    Plain(TaskReceiptRecord),
    /// Terminal receipt nesting Artifact publication evidence.
    Artifact(ArtifactTaskCommitReceipt),
    /// Terminal receipt nesting Semantic publication evidence.
    Semantic(SemanticTaskCommitReceipt),
    /// Terminal receipt nesting Resource cost evidence.
    Resource(ResourceTaskCommitReceipt),
    /// Terminal receipt nesting both Semantic and Resource evidence.
    SemanticResource(SemanticResourceTaskCommitReceipt),
}

impl TaskCommitReceipt {
    /// Deterministic 32-byte summary of this receipt (see the type-level
    /// digest formula documentation for the exact canonical encoding).
    ///
    /// The digest depends only on the receipt value: two equal receipt
    /// values always produce equal digests, and distinct variants or
    /// distinct encoded fields produce distinct digests up to SHA-256
    /// collision resistance.
    #[must_use]
    pub fn commit_receipt_digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(DIGEST_DOMAIN);
        match self {
            Self::Plain(receipt) => {
                hasher.update([PLAIN_DISCRIMINANT]);
                encode_task_receipt(&mut hasher, receipt);
            }
            Self::Artifact(receipt) => {
                hasher.update([ARTIFACT_DISCRIMINANT]);
                encode_task_receipt(&mut hasher, &receipt.task_receipt);
                encode_artifact_publications(&mut hasher, &receipt.artifact_publications);
            }
            Self::Semantic(receipt) => {
                hasher.update([SEMANTIC_DISCRIMINANT]);
                encode_task_receipt(&mut hasher, &receipt.task_receipt);
                encode_semantic_publications(&mut hasher, &receipt.semantic_publications);
            }
            Self::Resource(receipt) => {
                hasher.update([RESOURCE_DISCRIMINANT]);
                encode_task_receipt(&mut hasher, &receipt.task_receipt);
                encode_resource_cost_receipts(&mut hasher, &receipt.resource_cost_receipts);
            }
            Self::SemanticResource(receipt) => {
                hasher.update([SEMANTIC_RESOURCE_DISCRIMINANT]);
                encode_task_receipt(&mut hasher, &receipt.task_receipt);
                encode_semantic_publications(&mut hasher, &receipt.semantic_publications);
                encode_resource_cost_receipts(&mut hasher, &receipt.resource_cost_receipts);
            }
        }
        hasher.finalize().into()
    }
}

impl fmt::Display for TaskCommitReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plain(receipt) => {
                write!(formatter, "Plain({:?})", receipt.receipt_id)
            }
            Self::Artifact(receipt) => write!(
                formatter,
                "Artifact({:?}, artifact_publications: {})",
                receipt.task_receipt.receipt_id,
                receipt.artifact_publications.len()
            ),
            Self::Semantic(receipt) => write!(
                formatter,
                "Semantic({:?}, semantic_publications: {})",
                receipt.task_receipt.receipt_id,
                receipt.semantic_publications.len()
            ),
            Self::Resource(receipt) => write!(
                formatter,
                "Resource({:?}, resource_cost_receipts: {})",
                receipt.task_receipt.receipt_id,
                receipt.resource_cost_receipts.len()
            ),
            Self::SemanticResource(receipt) => write!(
                formatter,
                "SemanticResource({:?}, semantic_publications: {}, resource_cost_receipts: {})",
                receipt.task_receipt.receipt_id,
                receipt.semantic_publications.len(),
                receipt.resource_cost_receipts.len()
            ),
        }
    }
}

/// Encodes the base [`TaskReceiptRecord`] (digest formula step 2).
fn encode_task_receipt(hasher: &mut Sha256, record: &TaskReceiptRecord) {
    hasher.update(record.receipt_id.as_bytes());
    hasher.update(record.task_id.as_bytes());
    match record.permit_id {
        None => hasher.update([NONE_TAG]),
        Some(permit_id) => {
            hasher.update([SOME_TAG]);
            hasher.update(permit_id.as_bytes());
        }
    }
    hasher.update(record.attempt_id.as_bytes());
    hasher.update(record.attempt_generation.get().to_be_bytes());
    match record.group_binding {
        None => hasher.update([NONE_TAG]),
        Some(binding) => {
            hasher.update([SOME_TAG]);
            encode_group_binding(hasher, &binding);
        }
    }
    match record.participant_registry_binding {
        None => hasher.update([NONE_TAG]),
        Some(binding) => {
            hasher.update([SOME_TAG]);
            encode_participant_registry_binding(hasher, &binding);
        }
    }
    hasher.update([outcome_discriminant(record.outcome)]);
    hasher.update(record.prior_head_commit_seq.to_be_bytes());
    hasher.update(record.prior_effect_history_root);
    hasher.update(record.prior_retry_fence_epoch.to_be_bytes());
    hasher.update(record.new_head_commit_seq.to_be_bytes());
    hasher.update(record.new_effect_history_root);
    hasher.update(record.new_retry_fence_epoch.to_be_bytes());
    hasher.update(record.created_at_ms.to_be_bytes());
}

/// Encodes a present [`TaskGroupCommitBinding`] (after its option tag).
fn encode_group_binding(hasher: &mut Sha256, binding: &TaskGroupCommitBinding) {
    hasher.update(binding.group_id.as_bytes());
    hasher.update(binding.membership_generation.to_be_bytes());
    hasher.update(binding.membership_root);
    hasher.update(binding.group_policy_digest);
}

/// Encodes a present [`ParticipantRegistryBinding`] (after its option tag).
fn encode_participant_registry_binding(hasher: &mut Sha256, binding: &ParticipantRegistryBinding) {
    hasher.update(binding.generation.to_be_bytes());
    hasher.update(binding.root);
}

/// Returns the digest discriminant of a [`ReceiptOutcome`] (formula step
/// 2.8; mirrors the persisted outcome codes).
const fn outcome_discriminant(outcome: ReceiptOutcome) -> u8 {
    match outcome {
        ReceiptOutcome::Committed => 0,
        ReceiptOutcome::FailedBeforeEffect => 1,
        ReceiptOutcome::CancelledBeforeEffect => 2,
        ReceiptOutcome::Partial => 3,
        ReceiptOutcome::PartialEffect => 4,
        ReceiptOutcome::FailedAfterEffect => 5,
    }
}

/// Encodes the Artifact publication list (digest formula step 3).
fn encode_artifact_publications(
    hasher: &mut Sha256,
    publications: &[NestedArtifactPublicationReceipt],
) {
    hasher.update(
        u64::try_from(publications.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for publication in publications {
        hasher.update(publication.receipt_id.as_bytes());
        hasher.update(publication.staging_id);
        hasher.update(publication.artifact_id.as_bytes());
        hasher.update(publication.revision.to_be_bytes());
        hasher.update(publication.digest);
        hasher.update(publication.size_bytes.to_be_bytes());
        hasher.update(publication.task_id.as_bytes());
        hasher.update(publication.permit_id.as_bytes());
        hasher.update(publication.write_set_root);
        hasher.update(publication.prior_head_revision.to_be_bytes());
        match publication.prior_head_digest {
            None => hasher.update([NONE_TAG]),
            Some(digest) => {
                hasher.update([SOME_TAG]);
                hasher.update(digest);
            }
        }
        hasher.update(publication.new_head_revision.to_be_bytes());
        hasher.update(publication.new_head_digest);
        hasher.update(publication.created_at_ms.to_be_bytes());
    }
}

/// Encodes the Semantic publication list (digest formula step 4).
fn encode_semantic_publications(
    hasher: &mut Sha256,
    publications: &[NestedSemanticPublicationReceipt],
) {
    hasher.update(
        u64::try_from(publications.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for publication in publications {
        hasher.update(publication.receipt_id.as_bytes());
        hasher.update(publication.task_id.as_bytes());
        hasher.update(publication.permit_id.as_bytes());
        hasher.update(publication.write_set_root);
        hasher.update(publication.event_id.as_bytes());
        match publication.target {
            TaskWriteSetSemanticTarget::Namespace(id) => {
                hasher.update([1u8]);
                hasher.update(id.as_bytes());
            }
            TaskWriteSetSemanticTarget::Task(id) => {
                hasher.update([2u8]);
                hasher.update(id.as_bytes());
            }
        }
        hasher.update(publication.log_seq.to_be_bytes());
        hasher.update(publication.admission_receipt_id.as_bytes());
        match publication.durability_receipt_id {
            None => hasher.update([NONE_TAG]),
            Some(receipt_id) => {
                hasher.update([SOME_TAG]);
                hasher.update(receipt_id.as_bytes());
            }
        }
        hasher.update(publication.semantic_checkpoint_after);
        hasher.update(publication.created_at_ms.to_be_bytes());
    }
}

/// Encodes the Resource cost list (digest formula step 5).
fn encode_resource_cost_receipts(hasher: &mut Sha256, cost_receipts: &[NestedResourceCostReceipt]) {
    hasher.update(
        u64::try_from(cost_receipts.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for cost_receipt in cost_receipts {
        hasher.update(cost_receipt.reservation_id.as_bytes());
        hasher.update(cost_receipt.account_id.as_bytes());
        hasher.update(cost_receipt.quote_id.as_bytes());
        hasher.update(cost_receipt.call_id.as_bytes());
        hasher.update(cost_receipt.operation_id.as_bytes());
        hasher.update(cost_receipt.upper_bound.to_be_bytes());

        let activation = &cost_receipt.activation;
        hasher.update(activation.receipt_id.as_bytes());
        hasher.update(activation.reservation_id.as_bytes());
        hasher.update(activation.operation_id.as_bytes());
        hasher.update(activation.activated_at_ms.to_be_bytes());

        hasher.update(
            u64::try_from(cost_receipt.consumptions.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        for consumption in &cost_receipt.consumptions {
            hasher.update(consumption.receipt_id.as_bytes());
            hasher.update(consumption.reservation_id.as_bytes());
            hasher.update(consumption.operation_id.as_bytes());
            hasher.update(consumption.activation_receipt_id.as_bytes());
            hasher.update(consumption.sequence.to_be_bytes());
            hasher.update(consumption.cumulative_usage.to_be_bytes());
            hasher.update(consumption.consumed_at_ms.to_be_bytes());
        }

        let finalization = &cost_receipt.finalization;
        hasher.update(finalization.receipt_id.as_bytes());
        hasher.update(finalization.reservation_id.as_bytes());
        hasher.update(finalization.operation_id.as_bytes());
        hasher.update(finalization.activation_receipt_id.as_bytes());
        hasher.update(finalization.effect_closed_proof_digest);
        hasher.update(finalization.high_water_seq.to_be_bytes());
        hasher.update(finalization.final_seq.to_be_bytes());
        hasher.update(finalization.high_water.to_be_bytes());
        hasher.update(finalization.final_usage.to_be_bytes());
        hasher.update(finalization.refund_credit.to_be_bytes());
        hasher.update(finalization.finalized_at_ms.to_be_bytes());
    }
}
