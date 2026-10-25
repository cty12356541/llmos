//! Signed Semantic event set merge and cross-domain audit preservation
//! primitives — the library carrier of `[DIST-EVENT-001]` and `[DIST-AUDIT-001]`
//! (v0.5 §26.2):
//!
//! * `[DIST-EVENT-001]`: Semantic Event sync only merges signed event sets plus
//!   admission information and MUST NOT create semantic judgments through CRDT
//!   behavior.
//! * `[DIST-AUDIT-001]`: Cross-domain accepted events MUST simultaneously
//!   preserve the source producer signature, the receiving-domain
//!   `AdmissionReceipt`, and the applicable trust snapshot digest.
//!
//! Scope of this slice:
//!
//! * [`merge_signed_event_sets`] merges two sets of already-verified
//!   `(SemanticEventRecord, AdmissionReceipt)` pairs exported from
//!   [`crate::SemanticAuthority::export_signed_event_set`].
//! * [`merge_cross_domain_event_sets`] and
//!   [`verify_and_merge_cross_domain_events`] merge cross-domain
//!   [`CrossDomainEventItem`]s exported from
//!   [`crate::SemanticAuthority::export_cross_domain_event_set`], enforcing
//!   `[DIST-AUDIT-001]` by preserving each receiving domain's
//!   [`CrossDomainAuditRecord`] (`producer_key_id`, `producer_signature`,
//!   `admission`, `trust_snapshot_digest`) and rejecting missing/zero trust
//!   snapshots, invalid/zero producer signatures, or conflicting producer
//!   signatures across domains fail-closed.
//! * [`SemanticVectorCheckpoint`], [`SignedSemanticVectorCheckpoint`],
//!   [`DomainSemanticWaterMark`] ([`CellSemanticCursor`]),
//!   [`verify_signed_vector_checkpoint`], and [`diff_vector_checkpoints`]
//!   provide `[SEM-CHECKPOINT-001]` signed vector checkpoints and causal
//!   comparison (`Equal`, `LocalAhead`, `RemoteAhead`, `Concurrent`) without
//!   assuming a cross-Cell global scalar `log_seq`.
//! * The merge result is the set union keyed by `SemanticEventId`: one signed
//!   event admitted in two domains merges into one entry carrying both
//!   admission observations, and neither observation is privileged.
//! * Explicitly absent (per spec and slice boundary): CRDT semantics,
//!   contradiction/retraction resolution, validity/taint filtering, network
//!   transport, and cross-process orchestration.
//!
//! Merged entries deliberately expose no single merged `log_seq`: store-local
//! scalar sequence numbers are incomparable across stores
//! (`[SEM-CHECKPOINT-001]`, v0.5 line 2808). Every admission observation keeps
//! its own local `log_seq` and store identity triple, and the merged output
//! ordering is `event_id` byte order only — it carries no causal or semantic
//! meaning.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use nlos_capability::CapabilityTarget;
use nlos_identity::{
    IdentityAuthority, VerifySemanticAuthoritySignatureRequest, VerifySemanticSignatureRequest,
};
use nlos_types::{ControlDomainId, KeyId, PrincipalId, ReceiptId, SemanticEventId};
use sha2::{Digest, Sha256};

use crate::canonical::semantic_event_id;
use crate::model::{
    AdmissionReceipt, DurabilityReceipt, SemanticEventRecord, SemanticPayloadIdentity,
};

/// Domain-separation tag for [`SemanticVectorCheckpoint`] digests
/// (`[SEM-CHECKPOINT-001]`, v0.5 line 2808).
pub const SEMANTIC_VECTOR_CHECKPOINT_DOMAIN: &[u8] = b"llmos/semantic-vector-checkpoint/v1";

/// Domain-separation tag for [`SignedSemanticVectorCheckpoint`] authority
/// signatures (`[SEM-CHECKPOINT-001]`).
pub const SEMANTIC_VECTOR_CHECKPOINT_SIGNATURE_DOMAIN: &[u8] =
    b"llmos/semantic-vector-checkpoint-signature/v1";

/// Domain-separation tag for `[DIST-AUDIT-001]` cross-domain provenance
/// commitments bound into receiving-store durability receipts on convergence.
pub const CROSS_DOMAIN_AUDIT_COMMITMENT_DOMAIN: &[u8] = b"llmos/cross-domain-audit-commitment/v1";

/// Which input set one item or conflict occurrence came from. The labels are
/// positional only; they carry no authority or ordering semantics.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SignedEventSetOrigin {
    First,
    Second,
}

impl SignedEventSetOrigin {
    const fn rank(self) -> u8 {
        match self {
            Self::First => 0,
            Self::Second => 1,
        }
    }
}

/// One element of a to-be-merged signed event set: a locally verified event
/// record plus the admission observation that admitted it in the exporting
/// domain. Both halves are expected to come from the same authority readback
/// (`SemanticAuthority::inspect_event` / `inspect_admission_receipt`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignedEventSetItem {
    pub event: SemanticEventRecord,
    pub admission: AdmissionReceipt,
}

/// One merged signed event: the canonical event identity plus every
/// non-conflicting admission observation for it, one per admitting store.
///
/// There is no merged `log_seq` field: each admission observation keeps its
/// own store-local `log_seq` and store identity triple
/// (`[SEM-CHECKPOINT-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergedSignedEvent {
    pub event_id: SemanticEventId,
    pub canonical_unsigned_event: Vec<u8>,
    pub scope: CapabilityTarget,
    pub issuer: PrincipalId,
    pub control_domain: ControlDomainId,
    pub key_id: KeyId,
    pub payload_identity: SemanticPayloadIdentity,
    /// All surviving admission observations, sorted by `receipt_id` byte
    /// order. No observation is privileged over another.
    pub admissions: Vec<AdmissionReceipt>,
}

/// Deterministic per-item integrity rejection. Each item fails at most one
/// check, in the variant order listed here.
///
/// These are intrinsic identity-derivation checks (event id covers the
/// canonical bytes, receipt id covers its signed core digest, and the
/// receipt binds to exactly its own event and log position), not
/// re-verification of signatures.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SignedEventSetRejection {
    /// The claimed `event_id` does not equal the digest of the item's
    /// canonical unsigned bytes, so the signed identity does not cover the
    /// carried content.
    EventIdMismatch {
        origin: SignedEventSetOrigin,
        claimed_event_id: SemanticEventId,
        derived_event_id: SemanticEventId,
    },
    /// The admission receipt names a different event than the record it was
    /// paired with.
    AdmissionEventBinding {
        origin: SignedEventSetOrigin,
        event_id: SemanticEventId,
        admission_event_id: SemanticEventId,
    },
    /// The receipt's local log position disagrees with the record's position
    /// in the same store.
    AdmissionLogSeqBinding {
        origin: SignedEventSetOrigin,
        event_id: SemanticEventId,
        event_log_seq: u64,
        admission_log_seq: u64,
    },
    /// The receipt id does not equal the first 16 bytes of the receipt's core
    /// digest, so the receipt identity does not cover its own content.
    AdmissionReceiptIdDerivation {
        origin: SignedEventSetOrigin,
        event_id: SemanticEventId,
        claimed_receipt_id: ReceiptId,
        derived_receipt_id: ReceiptId,
    },
    /// A cross-domain observation has an all-zero `trust_snapshot_digest`,
    /// violating `[DIST-AUDIT-001]`.
    MissingTrustSnapshot {
        origin: SignedEventSetOrigin,
        event_id: SemanticEventId,
        receipt_id: ReceiptId,
    },
    /// A cross-domain observation has an all-zero or cryptographically invalid
    /// source producer signature (`[DIST-AUDIT-001]`).
    InvalidProducerSignature {
        origin: SignedEventSetOrigin,
        event_id: SemanticEventId,
        key_id: KeyId,
    },
    /// Cross-domain observations for the same `event_id` disagree on the
    /// source producer signature; neither signature is privileged so the event
    /// is rejected fail-closed (`[DIST-EVENT-001]`, `[DIST-AUDIT-001]`).
    ProducerSignatureConflict { event_id: SemanticEventId },
    /// Every admission observation for this event was excluded by a conflict,
    /// so no verified signed event plus admission pair survives to merge.
    EventWithoutValidAdmission { event_id: SemanticEventId },
}

impl SignedEventSetRejection {
    fn order_key(&self) -> (SemanticEventId, u8, u8) {
        match *self {
            Self::EventIdMismatch {
                origin,
                claimed_event_id,
                ..
            } => (claimed_event_id, origin.rank(), 0),
            Self::AdmissionEventBinding {
                origin, event_id, ..
            } => (event_id, origin.rank(), 1),
            Self::AdmissionLogSeqBinding {
                origin, event_id, ..
            } => (event_id, origin.rank(), 2),
            Self::AdmissionReceiptIdDerivation {
                origin, event_id, ..
            } => (event_id, origin.rank(), 3),
            Self::MissingTrustSnapshot {
                origin, event_id, ..
            } => (event_id, origin.rank(), 4),
            Self::InvalidProducerSignature {
                origin, event_id, ..
            } => (event_id, origin.rank(), 5),
            Self::ProducerSignatureConflict { event_id } => (event_id, 2, 6),
            Self::EventWithoutValidAdmission { event_id } => (event_id, 2, 7),
        }
    }
}

/// One implicated admission observation inside a conflict.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AdmissionConflictOccurrence {
    pub origin: SignedEventSetOrigin,
    pub receipt_id: ReceiptId,
}

/// Cross-item admission-side or producer-signature conflict. The event content
/// itself is never in conflict — equal `SemanticEventId`s cover equal canonical
/// bytes by construction — only admission observations or cross-domain audit
/// envelopes can disagree, and this layer never picks a winner: every
/// implicated observation is excluded and reported.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SignedEventSetConflict {
    /// The same receipt id appears with diverging receipt content, so at
    /// least one observation under that id is corrupt. All observations
    /// carrying the id are implicated.
    AdmissionReceiptContent {
        receipt_id: ReceiptId,
        occurrences: Vec<AdmissionConflictOccurrence>,
    },
    /// The same store identity triple carries more than one distinct receipt
    /// id for one event, violating the one-receipt-per-store-and-event
    /// invariant. All of that store's observations for the event are
    /// implicated.
    DuplicateStoreAdmission {
        event_id: SemanticEventId,
        occurrences: Vec<AdmissionConflictOccurrence>,
    },
    /// Cross-domain observations for the same `event_id` present diverging
    /// source producer signatures (`[DIST-AUDIT-001]`). All observations for
    /// the event are implicated and excluded.
    ProducerSignatureMismatch {
        event_id: SemanticEventId,
        occurrences: Vec<AdmissionConflictOccurrence>,
    },
    /// Cross-domain observations sharing the same `receipt_id` present
    /// diverging `trust_snapshot_digest` values (`[DIST-AUDIT-001]`). All
    /// observations carrying that receipt id are implicated and excluded.
    TrustSnapshotConflict {
        receipt_id: ReceiptId,
        occurrences: Vec<AdmissionConflictOccurrence>,
    },
}

impl SignedEventSetConflict {
    fn order_key(&self) -> (u8, [u8; 32]) {
        match self {
            Self::AdmissionReceiptContent { receipt_id, .. } => {
                let mut key = [0_u8; 32];
                key[..16].copy_from_slice(receipt_id.as_bytes());
                (0, key)
            }
            Self::DuplicateStoreAdmission { event_id, .. } => (1, event_id.into_bytes()),
            Self::ProducerSignatureMismatch { event_id, .. } => (2, event_id.into_bytes()),
            Self::TrustSnapshotConflict { receipt_id, .. } => {
                let mut key = [0_u8; 32];
                key[..16].copy_from_slice(receipt_id.as_bytes());
                (3, key)
            }
        }
    }
}

/// One cross-domain event observation carrying the complete `[DIST-AUDIT-001]`
/// provenance triple alongside the canonical [`SemanticEventRecord`]:
///
/// 1. `producer_signature`: the source producer's 64-byte Ed25519 signature
///    bound to `event.key_id`;
/// 2. `admission`: the receiving domain's signed [`AdmissionReceipt`];
/// 3. `trust_snapshot_digest`: the non-zero 32-byte digest of the applicable
///    trust snapshot at cross-domain acceptance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CrossDomainEventItem {
    pub event: SemanticEventRecord,
    pub producer_signature: [u8; 64],
    pub admission: AdmissionReceipt,
    pub trust_snapshot_digest: [u8; 32],
}

/// Durable cross-domain audit anchor preserved for each surviving
/// receiving-domain admission observation (`[DIST-AUDIT-001]`, v0.5 §26.2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CrossDomainAuditRecord {
    pub producer_key_id: KeyId,
    pub producer_signature: [u8; 64],
    pub admission: AdmissionReceipt,
    pub trust_snapshot_digest: [u8; 32],
}

/// One merged cross-domain signed event (`[DIST-EVENT-001]` +
/// `[DIST-AUDIT-001]`).
///
/// Preserves the canonical event identity, the agreed/verified source
/// `producer_signature`, all surviving receiving-domain `AdmissionReceipt`s,
/// and the corresponding [`CrossDomainAuditRecord`] envelopes binding producer
/// signature + receiving `AdmissionReceipt` + `trust_snapshot_digest`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergedCrossDomainEvent {
    pub event_id: SemanticEventId,
    pub canonical_unsigned_event: Vec<u8>,
    pub scope: CapabilityTarget,
    pub issuer: PrincipalId,
    pub control_domain: ControlDomainId,
    pub key_id: KeyId,
    pub payload_identity: SemanticPayloadIdentity,
    pub producer_signature: [u8; 64],
    /// All surviving admission observations, sorted by `receipt_id` byte order.
    pub admissions: Vec<AdmissionReceipt>,
    /// Complete `[DIST-AUDIT-001]` provenance records, sorted by
    /// `admission.receipt_id` byte order.
    pub audit_records: Vec<CrossDomainAuditRecord>,
}

/// Deterministic result of merging two cross-domain signed event sets
/// (`[DIST-EVENT-001]` + `[DIST-AUDIT-001]`).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CrossDomainEventSetMergeReport {
    pub merged: Vec<MergedCrossDomainEvent>,
    pub rejections: Vec<SignedEventSetRejection>,
    pub conflicts: Vec<SignedEventSetConflict>,
}

/// Deterministic result of merging two signed event sets.
///
/// `merged` is sorted by `event_id` byte order, `rejections` by
/// `(event_id, origin, check order)`, and `conflicts` by conflict kind then
/// id bytes. The orderings are presentation-only; none is causal.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SignedEventSetMergeReport {
    /// Signed events that merged, each carrying all surviving admission
    /// observations.
    pub merged: Vec<MergedSignedEvent>,
    /// Items rejected by intrinsic integrity checks, or events left without
    /// any valid admission observation.
    pub rejections: Vec<SignedEventSetRejection>,
    /// Admission observations excluded because they conflict with each other.
    pub conflicts: Vec<SignedEventSetConflict>,
}

/// Merges two sets of already-verified signed Semantic events with their
/// admission information (`[DIST-EVENT-001]`).
///
/// The merge is a pure set union keyed by `SemanticEventId`: the same signed
/// event admitted in both sets merges once with both admission observations,
/// identical observations deduplicate idempotently, and no semantic judgment
/// (contradiction resolution, retraction application, validity/taint
/// filtering, content-level dedup) is performed. Items that fail intrinsic
/// identity checks are rejected, conflicting admission observations are
/// excluded and reported, and an event whose admissions all conflict is not
/// merged.
///
/// This function performs no I/O, holds no state, and verifies no
/// signatures; inputs are trusted to have passed admission verification
/// upstream. It never panics and always returns a full report.
#[must_use]
pub fn merge_signed_event_sets(
    first: &[SignedEventSetItem],
    second: &[SignedEventSetItem],
) -> SignedEventSetMergeReport {
    let mut rejections: Vec<SignedEventSetRejection> = Vec::new();
    let mut conflicts: Vec<SignedEventSetConflict> = Vec::new();
    let mut merged: Vec<MergedSignedEvent> = Vec::new();

    // Phase 1: per-item intrinsic integrity validation.
    let mut accepted: Vec<(SignedEventSetOrigin, &SignedEventSetItem)> = Vec::new();
    for (origin, set) in [
        (SignedEventSetOrigin::First, first),
        (SignedEventSetOrigin::Second, second),
    ] {
        for item in set {
            match validate_item(origin, item) {
                Ok(()) => accepted.push((origin, item)),
                Err(rejection) => rejections.push(rejection),
            }
        }
    }

    // Phase 2: group accepted items by event id.
    let mut groups: BTreeMap<SemanticEventId, Vec<(SignedEventSetOrigin, &SignedEventSetItem)>> =
        BTreeMap::new();
    for (origin, item) in accepted {
        groups
            .entry(item.event.event_id)
            .or_default()
            .push((origin, item));
    }

    for (event_id, items) in groups {
        merge_event_group(
            event_id,
            &items,
            &mut merged,
            &mut rejections,
            &mut conflicts,
        );
    }

    rejections.sort_by_key(SignedEventSetRejection::order_key);
    conflicts.sort_by_key(SignedEventSetConflict::order_key);
    SignedEventSetMergeReport {
        merged,
        rejections,
        conflicts,
    }
}

/// Merges two sets of cross-domain signed Semantic events while enforcing
/// `[DIST-EVENT-001]` and `[DIST-AUDIT-001]` provenance preservation.
///
/// Every surviving observation must carry a non-zero `producer_signature` and
/// a non-zero `trust_snapshot_digest`, and all observations for the same
/// `event_id` across domains must agree on the source `producer_signature`.
/// Diverging producer signatures for the same `event_id` fail closed with
/// [`SignedEventSetConflict::ProducerSignatureMismatch`] and
/// [`SignedEventSetRejection::ProducerSignatureConflict`].
#[must_use]
pub fn merge_cross_domain_event_sets(
    first: &[CrossDomainEventItem],
    second: &[CrossDomainEventItem],
) -> CrossDomainEventSetMergeReport {
    merge_cross_domain_inner(None, first, second)
}

/// Verifies source producer signatures against `identity` and merges two sets
/// of cross-domain signed Semantic events under `[DIST-EVENT-001]` and
/// `[DIST-AUDIT-001]`.
///
/// In addition to all intrinsic identity, admission-binding, non-zero
/// `trust_snapshot_digest`, and cross-domain conflict checks of
/// [`merge_cross_domain_event_sets`], each item's `producer_signature` is
/// cryptographically verified against `identity` at the observation's
/// `admitted_at_ms`. Any item whose producer signature fails verification is
/// rejected fail-closed with
/// [`SignedEventSetRejection::InvalidProducerSignature`].
#[must_use]
pub fn verify_and_merge_cross_domain_events(
    identity: &IdentityAuthority,
    first: &[CrossDomainEventItem],
    second: &[CrossDomainEventItem],
) -> CrossDomainEventSetMergeReport {
    merge_cross_domain_inner(Some(identity), first, second)
}

fn merge_cross_domain_inner(
    identity: Option<&IdentityAuthority>,
    first: &[CrossDomainEventItem],
    second: &[CrossDomainEventItem],
) -> CrossDomainEventSetMergeReport {
    let mut rejections: Vec<SignedEventSetRejection> = Vec::new();
    let mut conflicts: Vec<SignedEventSetConflict> = Vec::new();
    let mut merged: Vec<MergedCrossDomainEvent> = Vec::new();

    // Phase 1: per-item intrinsic + DIST-AUDIT-001 validation.
    let mut accepted: Vec<(SignedEventSetOrigin, &CrossDomainEventItem)> = Vec::new();
    for (origin, set) in [
        (SignedEventSetOrigin::First, first),
        (SignedEventSetOrigin::Second, second),
    ] {
        for item in set {
            match validate_cross_domain_item(origin, item, identity) {
                Ok(()) => accepted.push((origin, item)),
                Err(rejection) => rejections.push(rejection),
            }
        }
    }

    // Phase 2: group accepted items by event id.
    let mut groups: BTreeMap<SemanticEventId, Vec<(SignedEventSetOrigin, &CrossDomainEventItem)>> =
        BTreeMap::new();
    for (origin, item) in accepted {
        groups
            .entry(item.event.event_id)
            .or_default()
            .push((origin, item));
    }

    for (event_id, items) in groups {
        merge_cross_domain_event_group(
            event_id,
            &items,
            &mut merged,
            &mut rejections,
            &mut conflicts,
        );
    }

    rejections.sort_by_key(SignedEventSetRejection::order_key);
    conflicts.sort_by_key(SignedEventSetConflict::order_key);
    CrossDomainEventSetMergeReport {
        merged,
        rejections,
        conflicts,
    }
}

fn validate_item(
    origin: SignedEventSetOrigin,
    item: &SignedEventSetItem,
) -> Result<(), SignedEventSetRejection> {
    validate_event_and_admission(origin, &item.event, &item.admission)
}

fn validate_event_and_admission(
    origin: SignedEventSetOrigin,
    event: &SemanticEventRecord,
    admission: &AdmissionReceipt,
) -> Result<(), SignedEventSetRejection> {
    let derived_event_id = semantic_event_id(&event.canonical_unsigned_event);
    if derived_event_id != event.event_id {
        return Err(SignedEventSetRejection::EventIdMismatch {
            origin,
            claimed_event_id: event.event_id,
            derived_event_id,
        });
    }
    if admission.event_id != event.event_id {
        return Err(SignedEventSetRejection::AdmissionEventBinding {
            origin,
            event_id: event.event_id,
            admission_event_id: admission.event_id,
        });
    }
    if admission.log_seq != event.log_seq {
        return Err(SignedEventSetRejection::AdmissionLogSeqBinding {
            origin,
            event_id: event.event_id,
            event_log_seq: event.log_seq,
            admission_log_seq: admission.log_seq,
        });
    }
    let derived_receipt_id = derive_receipt_id(admission);
    if derived_receipt_id != admission.receipt_id {
        return Err(SignedEventSetRejection::AdmissionReceiptIdDerivation {
            origin,
            event_id: event.event_id,
            claimed_receipt_id: admission.receipt_id,
            derived_receipt_id,
        });
    }
    Ok(())
}

fn validate_cross_domain_item(
    origin: SignedEventSetOrigin,
    item: &CrossDomainEventItem,
    identity: Option<&IdentityAuthority>,
) -> Result<(), SignedEventSetRejection> {
    validate_event_and_admission(origin, &item.event, &item.admission)?;
    if item.trust_snapshot_digest == [0_u8; 32] {
        return Err(SignedEventSetRejection::MissingTrustSnapshot {
            origin,
            event_id: item.event.event_id,
            receipt_id: item.admission.receipt_id,
        });
    }
    if item.producer_signature == [0_u8; 64] {
        return Err(SignedEventSetRejection::InvalidProducerSignature {
            origin,
            event_id: item.event.event_id,
            key_id: item.event.key_id,
        });
    }
    if let Some(authority) = identity
        && authority
            .verify_semantic_signature(VerifySemanticSignatureRequest {
                event_id: item.event.event_id,
                issuer: item.event.issuer,
                control_domain_id: item.event.control_domain,
                key_id: item.event.key_id,
                signature: item.producer_signature,
                admitted_at_ms: item.admission.admitted_at_ms,
            })
            .is_err()
    {
        return Err(SignedEventSetRejection::InvalidProducerSignature {
            origin,
            event_id: item.event.event_id,
            key_id: item.event.key_id,
        });
    }
    Ok(())
}

/// Recomputes the deterministic admission receipt id: the first 16 bytes of
/// the receipt's core digest, mirroring the authority's issuance path.
fn derive_receipt_id(receipt: &AdmissionReceipt) -> ReceiptId {
    let mut id_bytes = [0_u8; 16];
    id_bytes.copy_from_slice(&crate::admission_receipt_core_digest(receipt)[..16]);
    ReceiptId::from_bytes(id_bytes)
}

type StoreTriple = (PrincipalId, ControlDomainId, KeyId);

fn merge_event_group(
    event_id: SemanticEventId,
    items: &[(SignedEventSetOrigin, &SignedEventSetItem)],
    merged: &mut Vec<MergedSignedEvent>,
    rejections: &mut Vec<SignedEventSetRejection>,
    conflicts: &mut Vec<SignedEventSetConflict>,
) {
    let Some((_, first)) = items.first() else {
        return;
    };
    // After Phase 1, equal ids cover equal canonical bytes by construction;
    // any residual divergence (a digest collision) rejects the whole group
    // instead of picking one content.
    if items.iter().any(|(_, item)| {
        item.event.canonical_unsigned_event != first.event.canonical_unsigned_event
    }) {
        for (origin, item) in items {
            rejections.push(SignedEventSetRejection::EventIdMismatch {
                origin: *origin,
                claimed_event_id: event_id,
                derived_event_id: semantic_event_id(&item.event.canonical_unsigned_event),
            });
        }
        return;
    }

    // Observation index: receipt id and store triple of every item.
    let mut by_receipt_id: BTreeMap<ReceiptId, Vec<(SignedEventSetOrigin, &AdmissionReceipt)>> =
        BTreeMap::new();
    let mut by_store: BTreeMap<StoreTriple, BTreeSet<ReceiptId>> = BTreeMap::new();
    for (origin, item) in items {
        by_receipt_id
            .entry(item.admission.receipt_id)
            .or_default()
            .push((*origin, &item.admission));
        by_store
            .entry(store_triple(&item.admission))
            .or_default()
            .insert(item.admission.receipt_id);
    }

    let mut implicated: BTreeSet<ReceiptId> = BTreeSet::new();

    // Same receipt id with diverging content implicates every observation
    // carrying that id.
    for (receipt_id, observations) in &by_receipt_id {
        let reference = observations[0].1;
        if observations
            .iter()
            .any(|(_, receipt)| *receipt != reference)
        {
            implicated.insert(*receipt_id);
            let occurrences = observations
                .iter()
                .map(|(origin, _)| AdmissionConflictOccurrence {
                    origin: *origin,
                    receipt_id: *receipt_id,
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            conflicts.push(SignedEventSetConflict::AdmissionReceiptContent {
                receipt_id: *receipt_id,
                occurrences,
            });
        }
    }

    // One store admits one event once: several distinct receipt ids under the
    // same store triple implicate all of that store's observations.
    for receipt_ids in by_store.values() {
        if receipt_ids.len() > 1 {
            let mut occurrences = BTreeSet::new();
            for receipt_id in receipt_ids {
                implicated.insert(*receipt_id);
                for (origin, _) in &by_receipt_id[receipt_id] {
                    occurrences.insert(AdmissionConflictOccurrence {
                        origin: *origin,
                        receipt_id: *receipt_id,
                    });
                }
            }
            conflicts.push(SignedEventSetConflict::DuplicateStoreAdmission {
                event_id,
                occurrences: occurrences.into_iter().collect(),
            });
        }
    }

    // Union the surviving admission observations, deduplicated by receipt id.
    let mut admissions: BTreeMap<ReceiptId, &AdmissionReceipt> = BTreeMap::new();
    for (_, item) in items {
        if !implicated.contains(&item.admission.receipt_id) {
            admissions.insert(item.admission.receipt_id, &item.admission);
        }
    }

    if admissions.is_empty() {
        rejections.push(SignedEventSetRejection::EventWithoutValidAdmission { event_id });
        return;
    }

    let event = &first.event;
    merged.push(MergedSignedEvent {
        event_id,
        canonical_unsigned_event: event.canonical_unsigned_event.clone(),
        scope: event.scope,
        issuer: event.issuer,
        control_domain: event.control_domain,
        key_id: event.key_id,
        payload_identity: event.payload_identity,
        admissions: admissions.into_values().cloned().collect(),
    });
}

fn merge_cross_domain_event_group(
    event_id: SemanticEventId,
    items: &[(SignedEventSetOrigin, &CrossDomainEventItem)],
    merged: &mut Vec<MergedCrossDomainEvent>,
    rejections: &mut Vec<SignedEventSetRejection>,
    conflicts: &mut Vec<SignedEventSetConflict>,
) {
    let Some((_, first)) = items.first() else {
        return;
    };
    if items.iter().any(|(_, item)| {
        item.event.canonical_unsigned_event != first.event.canonical_unsigned_event
    }) {
        for (origin, item) in items {
            rejections.push(SignedEventSetRejection::EventIdMismatch {
                origin: *origin,
                claimed_event_id: event_id,
                derived_event_id: semantic_event_id(&item.event.canonical_unsigned_event),
            });
        }
        return;
    }

    // [DIST-AUDIT-001] All cross-domain observations for the same event_id
    // must agree on the source producer signature. Any divergence fails closed.
    if items
        .iter()
        .any(|(_, item)| item.producer_signature != first.producer_signature)
    {
        let occurrences = items
            .iter()
            .map(|(origin, item)| AdmissionConflictOccurrence {
                origin: *origin,
                receipt_id: item.admission.receipt_id,
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        conflicts.push(SignedEventSetConflict::ProducerSignatureMismatch {
            event_id,
            occurrences,
        });
        rejections.push(SignedEventSetRejection::ProducerSignatureConflict { event_id });
        return;
    }

    let implicated = collect_cross_domain_implicated_receipts(event_id, items, conflicts);

    let mut surviving: BTreeMap<ReceiptId, &CrossDomainEventItem> = BTreeMap::new();
    for (_, item) in items {
        if !implicated.contains(&item.admission.receipt_id) {
            surviving.insert(item.admission.receipt_id, *item);
        }
    }

    if surviving.is_empty() {
        rejections.push(SignedEventSetRejection::EventWithoutValidAdmission { event_id });
        return;
    }

    let event = &first.event;
    let mut admissions = Vec::with_capacity(surviving.len());
    let mut audit_records = Vec::with_capacity(surviving.len());
    for item in surviving.into_values() {
        admissions.push(item.admission.clone());
        audit_records.push(CrossDomainAuditRecord {
            producer_key_id: event.key_id,
            producer_signature: first.producer_signature,
            admission: item.admission.clone(),
            trust_snapshot_digest: item.trust_snapshot_digest,
        });
    }

    merged.push(MergedCrossDomainEvent {
        event_id,
        canonical_unsigned_event: event.canonical_unsigned_event.clone(),
        scope: event.scope,
        issuer: event.issuer,
        control_domain: event.control_domain,
        key_id: event.key_id,
        payload_identity: event.payload_identity,
        producer_signature: first.producer_signature,
        admissions,
        audit_records,
    });
}

fn collect_cross_domain_implicated_receipts(
    event_id: SemanticEventId,
    items: &[(SignedEventSetOrigin, &CrossDomainEventItem)],
    conflicts: &mut Vec<SignedEventSetConflict>,
) -> BTreeSet<ReceiptId> {
    let mut by_receipt_id: BTreeMap<ReceiptId, Vec<(SignedEventSetOrigin, &CrossDomainEventItem)>> =
        BTreeMap::new();
    let mut by_store: BTreeMap<StoreTriple, BTreeSet<ReceiptId>> = BTreeMap::new();
    for (origin, item) in items {
        by_receipt_id
            .entry(item.admission.receipt_id)
            .or_default()
            .push((*origin, *item));
        by_store
            .entry(store_triple(&item.admission))
            .or_default()
            .insert(item.admission.receipt_id);
    }

    let mut implicated: BTreeSet<ReceiptId> = BTreeSet::new();

    for (receipt_id, observations) in &by_receipt_id {
        let reference = observations[0].1;
        if observations
            .iter()
            .any(|(_, item)| item.admission != reference.admission)
        {
            implicated.insert(*receipt_id);
            let occurrences = observations
                .iter()
                .map(|(origin, _)| AdmissionConflictOccurrence {
                    origin: *origin,
                    receipt_id: *receipt_id,
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            conflicts.push(SignedEventSetConflict::AdmissionReceiptContent {
                receipt_id: *receipt_id,
                occurrences,
            });
        } else if observations
            .iter()
            .any(|(_, item)| item.trust_snapshot_digest != reference.trust_snapshot_digest)
        {
            implicated.insert(*receipt_id);
            let occurrences = observations
                .iter()
                .map(|(origin, _)| AdmissionConflictOccurrence {
                    origin: *origin,
                    receipt_id: *receipt_id,
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            conflicts.push(SignedEventSetConflict::TrustSnapshotConflict {
                receipt_id: *receipt_id,
                occurrences,
            });
        }
    }

    for receipt_ids in by_store.values() {
        if receipt_ids.len() > 1 {
            let mut occurrences = BTreeSet::new();
            for receipt_id in receipt_ids {
                implicated.insert(*receipt_id);
                for (origin, _) in &by_receipt_id[receipt_id] {
                    occurrences.insert(AdmissionConflictOccurrence {
                        origin: *origin,
                        receipt_id: *receipt_id,
                    });
                }
            }
            conflicts.push(SignedEventSetConflict::DuplicateStoreAdmission {
                event_id,
                occurrences: occurrences.into_iter().collect(),
            });
        }
    }

    implicated
}

fn store_triple(receipt: &AdmissionReceipt) -> StoreTriple {
    (
        receipt.store_principal,
        receipt.store_control_domain,
        receipt.store_key_id,
    )
}

/// Per-origin authority/domain semantic watermark (`[SEM-CHECKPOINT-001]`,
/// v0.5 line 2808).
///
/// Ordered deterministically by `authority_id` (the 16-byte origin
/// `ControlDomainId` of the events in that domain) and never relies on a
/// store-local or cross-Cell scalar `log_seq`.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DomainSemanticWaterMark {
    pub authority_id: [u8; 16],
    pub event_count: u64,
    pub event_id_merkle_root: [u8; 32],
    pub latest_admission_receipt_digest: [u8; 32],
    pub trust_snapshot_digest: [u8; 32],
}

/// Per-Cell/origin semantic cursor alias (`[SEM-CHECKPOINT-001]`).
pub type CellSemanticCursor = DomainSemanticWaterMark;

/// Unsigned distributed Semantic vector checkpoint (`[SEM-CHECKPOINT-001]`,
/// v0.5 line 2808).
///
/// Summarizes the admitted Semantic event sets across origin authorities
/// without assuming a cross-Cell global scalar `log_seq`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticVectorCheckpoint {
    pub checkpoint_id: [u8; 16],
    pub domain_watermarks: Vec<DomainSemanticWaterMark>,
    pub covered_event_ids_root: [u8; 32],
    pub checkpoint_digest: [u8; 32],
}

/// Cryptographically signed distributed Semantic vector checkpoint
/// (`[SEM-CHECKPOINT-001]`, v0.5 line 2808).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignedSemanticVectorCheckpoint {
    pub checkpoint_id: [u8; 16],
    pub domain_watermarks: Vec<DomainSemanticWaterMark>,
    pub covered_event_ids_root: [u8; 32],
    pub checkpoint_digest: [u8; 32],
    pub signer_principal_id: PrincipalId,
    pub signer_authority_id: [u8; 16],
    pub signer_key_id: KeyId,
    pub signer_public_key: [u8; 32],
    pub signed_at_ms: u64,
    pub signature: [u8; 64],
}

impl SignedSemanticVectorCheckpoint {
    /// Extracts the unsigned [`SemanticVectorCheckpoint`] core for causal
    /// comparison via [`diff_vector_checkpoints`].
    #[must_use]
    pub fn checkpoint(&self) -> SemanticVectorCheckpoint {
        SemanticVectorCheckpoint {
            checkpoint_id: self.checkpoint_id,
            domain_watermarks: self.domain_watermarks.clone(),
            covered_event_ids_root: self.covered_event_ids_root,
            checkpoint_digest: self.checkpoint_digest,
        }
    }
}

/// Fail-closed validation and verification errors for
/// [`SemanticVectorCheckpoint`] and [`SignedSemanticVectorCheckpoint`]
/// (`[SEM-CHECKPOINT-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SemanticVectorCheckpointError {
    /// A checkpoint or watermark carries an all-zero `trust_snapshot_digest`.
    MissingTrustSnapshot,
    /// `domain_watermarks` are not strictly sorted and unique by
    /// `authority_id`.
    InvalidWatermarkOrder {
        previous_authority_id: [u8; 16],
        current_authority_id: [u8; 16],
    },
    /// A present domain watermark reports `event_count == 0`.
    EmptyDomainWatermark { authority_id: [u8; 16] },
    /// A present domain watermark carries a zero or empty `event_id_merkle_root`.
    ZeroWatermarkMerkleRoot { authority_id: [u8; 16] },
    /// A present domain watermark carries a zero
    /// `latest_admission_receipt_digest`.
    ZeroWatermarkAdmissionDigest { authority_id: [u8; 16] },
    /// `covered_event_ids_root` disagrees with `domain_watermarks`.
    CoveredEventIdsRootMismatch,
    /// `checkpoint_digest` does not match the domain-separated SHA-256 of the
    /// checkpoint fields.
    CheckpointDigestMismatch {
        claimed_digest: [u8; 32],
        derived_digest: [u8; 32],
    },
    /// `checkpoint_id` does not equal the first 16 bytes of `checkpoint_digest`.
    CheckpointIdMismatch {
        claimed_id: [u8; 16],
        derived_id: [u8; 16],
    },
    /// The signed checkpoint has zeroed signer authority, public key, or
    /// signature bytes.
    InvalidSignerMetadata,
    /// The signer authority or public key does not match the `IdentityAuthority`
    /// binding for `signer_key_id`.
    SignerBindingMismatch,
    /// The Ed25519 signature over the checkpoint failed `IdentityAuthority`
    /// verification.
    InvalidSignature,
}

impl fmt::Display for SemanticVectorCheckpointError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingTrustSnapshot => {
                formatter.write_str("vector checkpoint requires a non-zero trust_snapshot_digest")
            }
            Self::InvalidWatermarkOrder {
                previous_authority_id,
                current_authority_id,
            } => write!(
                formatter,
                "domain_watermarks must be strictly sorted by authority_id ({previous_authority_id:?} >= {current_authority_id:?})"
            ),
            Self::EmptyDomainWatermark { authority_id } => write!(
                formatter,
                "domain watermark {authority_id:?} has zero event_count"
            ),
            Self::ZeroWatermarkMerkleRoot { authority_id } => write!(
                formatter,
                "domain watermark {authority_id:?} has zero or empty event_id_merkle_root"
            ),
            Self::ZeroWatermarkAdmissionDigest { authority_id } => write!(
                formatter,
                "domain watermark {authority_id:?} has zero latest_admission_receipt_digest"
            ),
            Self::CoveredEventIdsRootMismatch => {
                formatter.write_str("covered_event_ids_root disagrees with domain_watermarks")
            }
            Self::CheckpointDigestMismatch { .. } => formatter
                .write_str("checkpoint_digest does not match domain-separated SHA-256 commitment"),
            Self::CheckpointIdMismatch { .. } => formatter
                .write_str("checkpoint_id does not match first 16 bytes of checkpoint_digest"),
            Self::InvalidSignerMetadata => {
                formatter.write_str("signed vector checkpoint has zeroed signer metadata")
            }
            Self::SignerBindingMismatch => formatter.write_str(
                "signed vector checkpoint signer metadata disagrees with identity binding",
            ),
            Self::InvalidSignature => {
                formatter.write_str("signed vector checkpoint signature verification failed")
            }
        }
    }
}

impl Error for SemanticVectorCheckpointError {}

/// Causal ordering relation between two vector checkpoints or two per-domain
/// watermarks (`[SEM-CHECKPOINT-001]`, `[CONS-SCOPE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorCheckpointRelation {
    /// Both checkpoints/watermarks cover the exact same event sets and
    /// admission watermarks.
    Equal,
    /// `local` has strictly advanced beyond `remote` in at least one origin
    /// domain and is neither behind nor divergent in any origin domain.
    LocalAhead,
    /// `remote` has strictly advanced beyond `local` in at least one origin
    /// domain and is neither behind nor divergent in any origin domain.
    RemoteAhead,
    /// `local` and `remote` have independently advanced in different origin
    /// domains, or observed divergent event sets within an origin domain.
    Concurrent,
}

/// Per-origin-domain comparison between two [`SemanticVectorCheckpoint`]s.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DomainWatermarkDiff {
    pub authority_id: [u8; 16],
    pub relation: VectorCheckpointRelation,
    pub local_event_count: u64,
    pub remote_event_count: u64,
    pub local_merkle_root: Option<[u8; 32]>,
    pub remote_merkle_root: Option<[u8; 32]>,
}

/// Result of comparing two [`SemanticVectorCheckpoint`]s via
/// [`diff_vector_checkpoints`] without relying on any cross-Cell scalar
/// `log_seq` (`[SEM-CHECKPOINT-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorCheckpointDiff {
    pub relation: VectorCheckpointRelation,
    pub domain_diffs: Vec<DomainWatermarkDiff>,
    pub local_ahead_domains: Vec<[u8; 16]>,
    pub remote_ahead_domains: Vec<[u8; 16]>,
    pub concurrent_domains: Vec<[u8; 16]>,
}

/// Result of durably importing/converging verified cross-domain Semantic
/// events into a receiving [`crate::SemanticAuthority`] store
/// (`[DIST-EVENT-001]`, `[DIST-AUDIT-001]`, `[SEM-CHECKPOINT-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CrossDomainConvergenceReport {
    /// Complete verified cross-domain merge report over the receiving store's
    /// pre-existing events and the incoming remote event sets.
    pub merge_report: CrossDomainEventSetMergeReport,
    /// `SemanticEventId`s that were newly admitted and persisted into the
    /// receiving SQLite store during this convergence transaction, in
    /// deterministic topological import order.
    pub imported_event_ids: Vec<SemanticEventId>,
    /// `SemanticEventId`s that were already present in the receiving SQLite
    /// store and idempotently skipped.
    pub skipped_existing_event_ids: Vec<SemanticEventId>,
    /// Receiving-domain `AdmissionReceipt`s minted for the newly imported
    /// events.
    pub imported_admissions: Vec<AdmissionReceipt>,
    /// Receiving-domain `DurabilityReceipt`s minted for the newly imported
    /// events, binding each event to its `[DIST-AUDIT-001]` cross-domain
    /// provenance commitment digest.
    pub imported_durability_receipts: Vec<DurabilityReceipt>,
    /// Preserved `[DIST-AUDIT-001]` cross-domain audit records across all
    /// converged events.
    pub audit_records: Vec<CrossDomainAuditRecord>,
    /// Post-convergence [`SemanticVectorCheckpoint`] of the receiving store.
    pub vector_checkpoint: SemanticVectorCheckpoint,
}

/// Computes the deterministic binary Merkle root over a set of
/// `SemanticEventId`s sorted in canonical byte order (`[SEM-CHECKPOINT-001]`).
///
/// The result is completely independent of store-local `log_seq` ordering.
#[must_use]
pub fn compute_event_id_merkle_root(event_ids: &[SemanticEventId]) -> [u8; 32] {
    let unique_sorted: BTreeSet<SemanticEventId> = event_ids.iter().copied().collect();
    if unique_sorted.is_empty() {
        let mut hasher = Sha256::new();
        hasher.update(b"llmos/semantic-event-merkle-empty/v1");
        return hasher.finalize().into();
    }
    let mut layer: Vec<[u8; 32]> = unique_sorted
        .into_iter()
        .map(|id| {
            let mut hasher = Sha256::new();
            hasher.update(b"llmos/semantic-event-merkle-leaf/v1");
            hasher.update(id.as_bytes());
            hasher.finalize().into()
        })
        .collect();
    while layer.len() > 1 {
        let mut next = Vec::with_capacity(layer.len().div_ceil(2));
        for chunk in layer.chunks(2) {
            let mut hasher = Sha256::new();
            hasher.update(b"llmos/semantic-event-merkle-node/v1");
            hasher.update(chunk[0]);
            if chunk.len() == 2 {
                hasher.update([2_u8]);
                hasher.update(chunk[1]);
            } else {
                hasher.update([1_u8]);
            }
            next.push(hasher.finalize().into());
        }
        layer = next;
    }
    layer[0]
}

/// Computes the replica-invariant admission watermark digest for one origin
/// `authority_id` over its admitted events ordered by `(admitted_at_ms,
/// event_id)` — never relying on store-local `log_seq` (`[SEM-CHECKPOINT-001]`).
#[must_use]
pub fn compute_domain_admission_watermark_digest(
    authority_id: [u8; 16],
    admissions: &[&AdmissionReceipt],
) -> [u8; 32] {
    let mut ordered: Vec<&AdmissionReceipt> = admissions.to_vec();
    ordered.sort_by_key(|receipt| (receipt.admitted_at_ms, receipt.event_id));
    ordered.dedup_by_key(|receipt| receipt.event_id);

    let mut hasher = Sha256::new();
    hasher.update(b"llmos/semantic-watermark-admission/v1");
    hasher.update(authority_id);
    hasher.update((ordered.len() as u64).to_be_bytes());
    for admission in ordered {
        hasher.update(admission.event_id.as_bytes());
        hasher.update(admission.admitted_at_ms.to_be_bytes());
        match admission.effective_valid_until_ms {
            Some(valid_until) => {
                hasher.update([1_u8]);
                hasher.update(valid_until.to_be_bytes());
            }
            None => hasher.update([0_u8]),
        }
        hasher.update((admission.captured_inputs.len() as u64).to_be_bytes());
        for input in &admission.captured_inputs {
            hasher.update(input.as_bytes());
        }
        hasher.update(admission.effective_taint.bits().to_be_bytes());
        hasher.update(admission.authz_policy_digest);
        hasher.update([2_u8]);
    }
    hasher.finalize().into()
}

/// Computes the domain-separated SHA-256 `checkpoint_digest`
/// (`"llmos/semantic-vector-checkpoint/v1"`) over `domain_watermarks` and
/// `covered_event_ids_root` (`[SEM-CHECKPOINT-001]`).
#[must_use]
pub fn compute_vector_checkpoint_digest(
    domain_watermarks: &[DomainSemanticWaterMark],
    covered_event_ids_root: [u8; 32],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(SEMANTIC_VECTOR_CHECKPOINT_DOMAIN);
    hasher.update((domain_watermarks.len() as u64).to_be_bytes());
    for watermark in domain_watermarks {
        hasher.update(watermark.authority_id);
        hasher.update(watermark.event_count.to_be_bytes());
        hasher.update(watermark.event_id_merkle_root);
        hasher.update(watermark.latest_admission_receipt_digest);
        hasher.update(watermark.trust_snapshot_digest);
    }
    hasher.update(covered_event_ids_root);
    hasher.finalize().into()
}

/// Computes the 32-byte message digest signed by a store authority over a
/// [`SignedSemanticVectorCheckpoint`].
#[must_use]
pub fn semantic_vector_checkpoint_signature_message(
    checkpoint_id: [u8; 16],
    checkpoint_digest: [u8; 32],
    signer_authority_id: [u8; 16],
    signer_public_key: [u8; 32],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(SEMANTIC_VECTOR_CHECKPOINT_SIGNATURE_DOMAIN);
    hasher.update(checkpoint_id);
    hasher.update(checkpoint_digest);
    hasher.update(signer_authority_id);
    hasher.update(signer_public_key);
    hasher.finalize().into()
}

/// Computes a deterministic `[DIST-AUDIT-001]` provenance commitment digest
/// over all surviving [`CrossDomainAuditRecord`]s for one `event_id`.
#[must_use]
pub fn compute_cross_domain_audit_commitment_digest(
    event_id: SemanticEventId,
    audit_records: &[CrossDomainAuditRecord],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(CROSS_DOMAIN_AUDIT_COMMITMENT_DOMAIN);
    hasher.update(event_id.as_bytes());
    hasher.update((audit_records.len() as u64).to_be_bytes());
    for record in audit_records {
        hasher.update(record.producer_key_id.as_bytes());
        hasher.update(record.producer_signature);
        hasher.update(record.admission.receipt_id.as_bytes());
        hasher.update(record.trust_snapshot_digest);
    }
    hasher.finalize().into()
}

/// Builds an unsigned [`SemanticVectorCheckpoint`] from a set of verified
/// [`SignedEventSetItem`]s and a non-zero `trust_snapshot_digest`
/// (`[SEM-CHECKPOINT-001]`).
///
/// # Errors
///
/// Returns [`SemanticVectorCheckpointError::MissingTrustSnapshot`] when
/// `trust_snapshot_digest` is all zeros.
pub fn build_vector_checkpoint(
    items: &[SignedEventSetItem],
    trust_snapshot_digest: [u8; 32],
) -> Result<SemanticVectorCheckpoint, SemanticVectorCheckpointError> {
    if trust_snapshot_digest == [0_u8; 32] {
        return Err(SemanticVectorCheckpointError::MissingTrustSnapshot);
    }

    let mut by_domain: BTreeMap<[u8; 16], BTreeMap<SemanticEventId, &SignedEventSetItem>> =
        BTreeMap::new();
    let mut all_event_ids: Vec<SemanticEventId> = Vec::with_capacity(items.len());
    for item in items {
        all_event_ids.push(item.event.event_id);
        by_domain
            .entry(item.event.control_domain.into_bytes())
            .or_default()
            .entry(item.event.event_id)
            .or_insert(item);
    }

    let mut domain_watermarks = Vec::with_capacity(by_domain.len());
    for (authority_id, domain_items) in by_domain {
        let domain_event_ids: Vec<SemanticEventId> = domain_items.keys().copied().collect();
        let domain_admissions: Vec<&AdmissionReceipt> =
            domain_items.values().map(|item| &item.admission).collect();
        domain_watermarks.push(DomainSemanticWaterMark {
            authority_id,
            event_count: domain_event_ids.len() as u64,
            event_id_merkle_root: compute_event_id_merkle_root(&domain_event_ids),
            latest_admission_receipt_digest: compute_domain_admission_watermark_digest(
                authority_id,
                &domain_admissions,
            ),
            trust_snapshot_digest,
        });
    }

    let covered_event_ids_root = compute_event_id_merkle_root(&all_event_ids);
    let checkpoint_digest =
        compute_vector_checkpoint_digest(&domain_watermarks, covered_event_ids_root);
    let mut checkpoint_id = [0_u8; 16];
    checkpoint_id.copy_from_slice(&checkpoint_digest[..16]);

    Ok(SemanticVectorCheckpoint {
        checkpoint_id,
        domain_watermarks,
        covered_event_ids_root,
        checkpoint_digest,
    })
}

/// Validates the structural, ordering, Merkle-root, and digest invariants of
/// an unsigned [`SemanticVectorCheckpoint`] (`[SEM-CHECKPOINT-001]`).
///
/// # Errors
///
/// Fails closed if any watermark has a zero trust snapshot, zero count, zero
/// Merkle root, zero admission digest, non-canonical `authority_id` order,
/// mismatched `covered_event_ids_root`, or mismatched `checkpoint_digest` /
/// `checkpoint_id`.
pub fn verify_vector_checkpoint(
    checkpoint: &SemanticVectorCheckpoint,
) -> Result<(), SemanticVectorCheckpointError> {
    let empty_root = compute_event_id_merkle_root(&[]);
    for pair in checkpoint.domain_watermarks.windows(2) {
        if pair[0].authority_id >= pair[1].authority_id {
            return Err(SemanticVectorCheckpointError::InvalidWatermarkOrder {
                previous_authority_id: pair[0].authority_id,
                current_authority_id: pair[1].authority_id,
            });
        }
    }
    for watermark in &checkpoint.domain_watermarks {
        if watermark.trust_snapshot_digest == [0_u8; 32] {
            return Err(SemanticVectorCheckpointError::MissingTrustSnapshot);
        }
        if watermark.event_count == 0 {
            return Err(SemanticVectorCheckpointError::EmptyDomainWatermark {
                authority_id: watermark.authority_id,
            });
        }
        if watermark.event_id_merkle_root == [0_u8; 32]
            || watermark.event_id_merkle_root == empty_root
        {
            return Err(SemanticVectorCheckpointError::ZeroWatermarkMerkleRoot {
                authority_id: watermark.authority_id,
            });
        }
        if watermark.latest_admission_receipt_digest == [0_u8; 32] {
            return Err(
                SemanticVectorCheckpointError::ZeroWatermarkAdmissionDigest {
                    authority_id: watermark.authority_id,
                },
            );
        }
    }
    if checkpoint.domain_watermarks.is_empty() {
        if checkpoint.covered_event_ids_root != empty_root {
            return Err(SemanticVectorCheckpointError::CoveredEventIdsRootMismatch);
        }
    } else {
        if checkpoint.covered_event_ids_root == [0_u8; 32]
            || checkpoint.covered_event_ids_root == empty_root
        {
            return Err(SemanticVectorCheckpointError::CoveredEventIdsRootMismatch);
        }
        if checkpoint.domain_watermarks.len() == 1
            && checkpoint.covered_event_ids_root
                != checkpoint.domain_watermarks[0].event_id_merkle_root
        {
            return Err(SemanticVectorCheckpointError::CoveredEventIdsRootMismatch);
        }
    }
    let derived_digest = compute_vector_checkpoint_digest(
        &checkpoint.domain_watermarks,
        checkpoint.covered_event_ids_root,
    );
    if checkpoint.checkpoint_digest != derived_digest {
        return Err(SemanticVectorCheckpointError::CheckpointDigestMismatch {
            claimed_digest: checkpoint.checkpoint_digest,
            derived_digest,
        });
    }
    let mut derived_id = [0_u8; 16];
    derived_id.copy_from_slice(&derived_digest[..16]);
    if checkpoint.checkpoint_id != derived_id {
        return Err(SemanticVectorCheckpointError::CheckpointIdMismatch {
            claimed_id: checkpoint.checkpoint_id,
            derived_id,
        });
    }
    Ok(())
}

/// Verifies a [`SignedSemanticVectorCheckpoint`] end-to-end against `identity`
/// (`[SEM-CHECKPOINT-001]`).
///
/// Validates all structural and digest invariants via
/// [`verify_vector_checkpoint`], verifies that `signer_principal_id`,
/// `signer_authority_id`, and `signer_public_key` match the active
/// `IdentityAuthority` binding for `signer_key_id`, and cryptographically
/// verifies `signature` over the domain-separated checkpoint signature
/// message.
///
/// # Errors
///
/// Returns a typed [`SemanticVectorCheckpointError`] on any watermark, root,
/// digest, signer-binding, or signature tampering.
pub fn verify_signed_vector_checkpoint(
    identity: &IdentityAuthority,
    checkpoint: &SignedSemanticVectorCheckpoint,
) -> Result<(), SemanticVectorCheckpointError> {
    verify_vector_checkpoint(&checkpoint.checkpoint())?;
    if checkpoint.signer_authority_id == [0_u8; 16]
        || checkpoint.signer_public_key == [0_u8; 32]
        || checkpoint.signature == [0_u8; 64]
    {
        return Err(SemanticVectorCheckpointError::InvalidSignerMetadata);
    }
    let binding = identity
        .inspect_current_binding(checkpoint.signer_key_id)
        .map_err(|_| SemanticVectorCheckpointError::SignerBindingMismatch)?;
    if binding.principal_id != checkpoint.signer_principal_id
        || binding.control_domain_id.into_bytes() != checkpoint.signer_authority_id
        || binding.public_key != checkpoint.signer_public_key
    {
        return Err(SemanticVectorCheckpointError::SignerBindingMismatch);
    }
    let message_digest = semantic_vector_checkpoint_signature_message(
        checkpoint.checkpoint_id,
        checkpoint.checkpoint_digest,
        checkpoint.signer_authority_id,
        checkpoint.signer_public_key,
    );
    let verified = identity
        .verify_semantic_authority_signature(VerifySemanticAuthoritySignatureRequest {
            message_digest,
            issuer: checkpoint.signer_principal_id,
            control_domain_id: ControlDomainId::from_bytes(checkpoint.signer_authority_id),
            key_id: checkpoint.signer_key_id,
            signature: checkpoint.signature,
            verified_at_ms: checkpoint.signed_at_ms,
        })
        .map_err(|_| SemanticVectorCheckpointError::InvalidSignature)?;
    if verified.principal_id() != checkpoint.signer_principal_id
        || verified.control_domain_id().into_bytes() != checkpoint.signer_authority_id
        || verified.key_id() != checkpoint.signer_key_id
    {
        return Err(SemanticVectorCheckpointError::SignerBindingMismatch);
    }
    Ok(())
}

/// Compares two [`SemanticVectorCheckpoint`]s causally across all origin
/// domains (`Equal`, `LocalAhead`, `RemoteAhead`, `Concurrent`) without
/// assuming any cross-Cell global scalar `log_seq` (`[SEM-CHECKPOINT-001]`,
/// `[CONS-SCOPE-001]`).
#[must_use]
pub fn diff_vector_checkpoints(
    local: &SemanticVectorCheckpoint,
    remote: &SemanticVectorCheckpoint,
) -> VectorCheckpointDiff {
    let local_map: BTreeMap<[u8; 16], &DomainSemanticWaterMark> = local
        .domain_watermarks
        .iter()
        .map(|wm| (wm.authority_id, wm))
        .collect();
    let remote_map: BTreeMap<[u8; 16], &DomainSemanticWaterMark> = remote
        .domain_watermarks
        .iter()
        .map(|wm| (wm.authority_id, wm))
        .collect();
    let all_authorities: BTreeSet<[u8; 16]> =
        local_map.keys().chain(remote_map.keys()).copied().collect();

    let mut domain_diffs = Vec::with_capacity(all_authorities.len());
    let mut local_ahead_domains = Vec::new();
    let mut remote_ahead_domains = Vec::new();
    let mut concurrent_domains = Vec::new();

    for authority_id in all_authorities {
        let local_wm = local_map.get(&authority_id).copied();
        let remote_wm = remote_map.get(&authority_id).copied();
        let (relation, local_count, remote_count, local_root, remote_root) =
            match (local_wm, remote_wm) {
                (Some(l), Some(r)) => {
                    let rel = if l.event_count == r.event_count
                        && l.event_id_merkle_root == r.event_id_merkle_root
                        && l.latest_admission_receipt_digest == r.latest_admission_receipt_digest
                    {
                        VectorCheckpointRelation::Equal
                    } else if l.event_count > r.event_count {
                        VectorCheckpointRelation::LocalAhead
                    } else if l.event_count < r.event_count {
                        VectorCheckpointRelation::RemoteAhead
                    } else {
                        VectorCheckpointRelation::Concurrent
                    };
                    (
                        rel,
                        l.event_count,
                        r.event_count,
                        Some(l.event_id_merkle_root),
                        Some(r.event_id_merkle_root),
                    )
                }
                (Some(l), None) => (
                    VectorCheckpointRelation::LocalAhead,
                    l.event_count,
                    0,
                    Some(l.event_id_merkle_root),
                    None,
                ),
                (None, Some(r)) => (
                    VectorCheckpointRelation::RemoteAhead,
                    0,
                    r.event_count,
                    None,
                    Some(r.event_id_merkle_root),
                ),
                (None, None) => unreachable!("authority_id came from union of keys"),
            };

        match relation {
            VectorCheckpointRelation::Equal => {}
            VectorCheckpointRelation::LocalAhead => local_ahead_domains.push(authority_id),
            VectorCheckpointRelation::RemoteAhead => remote_ahead_domains.push(authority_id),
            VectorCheckpointRelation::Concurrent => concurrent_domains.push(authority_id),
        }

        domain_diffs.push(DomainWatermarkDiff {
            authority_id,
            relation,
            local_event_count: local_count,
            remote_event_count: remote_count,
            local_merkle_root: local_root,
            remote_merkle_root: remote_root,
        });
    }

    let relation = if !concurrent_domains.is_empty()
        || (!local_ahead_domains.is_empty() && !remote_ahead_domains.is_empty())
    {
        VectorCheckpointRelation::Concurrent
    } else if !local_ahead_domains.is_empty() {
        VectorCheckpointRelation::LocalAhead
    } else if !remote_ahead_domains.is_empty() {
        VectorCheckpointRelation::RemoteAhead
    } else if local.covered_event_ids_root == remote.covered_event_ids_root {
        VectorCheckpointRelation::Equal
    } else {
        VectorCheckpointRelation::Concurrent
    };

    VectorCheckpointDiff {
        relation,
        domain_diffs,
        local_ahead_domains,
        remote_ahead_domains,
        concurrent_domains,
    }
}
