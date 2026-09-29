//! Signed Semantic event set merge primitive — the pure library carrier of
//! `[DIST-EVENT-001]` (v0.5 §26.2): Semantic Event sync only merges signed
//! event sets plus admission information and MUST NOT create semantic
//! judgments through CRDT behavior.
//!
//! Scope of this minimal slice:
//!
//! * Inputs are two sets of already-verified
//!   `(SemanticEventRecord, AdmissionReceipt)` pairs. Issuer-signature,
//!   capability, lineage, and store-signature verification happened at
//!   admission upstream; this layer performs no cryptographic verification
//!   and re-judges no semantics.
//! * The merge result is the set union keyed by `SemanticEventId`: one signed
//!   event admitted in two domains merges into one entry carrying both
//!   admission observations, and neither observation is privileged.
//! * Deterministic intrinsic-integrity checks reject corrupt items and report
//!   admission-side conflicts. Everything else is merged verbatim.
//! * Explicitly absent (per spec and slice boundary): CRDT semantics,
//!   contradiction/retraction resolution, validity/taint filtering, transport,
//!   storage, Cell indirection, and cross-process behavior.
//!
//! Merged entries deliberately expose no single merged `log_seq`: store-local
//! scalar sequence numbers are incomparable across stores
//! (`[SEM-CHECKPOINT-001]`, v0.5 line 2808). Every admission observation keeps
//! its own local `log_seq` and store identity triple, and the merged output
//! ordering is `event_id` byte order only — it carries no causal or semantic
//! meaning.

use std::collections::{BTreeMap, BTreeSet};

use nlos_capability::CapabilityTarget;
use nlos_types::{ControlDomainId, KeyId, PrincipalId, ReceiptId, SemanticEventId};

use crate::canonical::semantic_event_id;
use crate::model::{AdmissionReceipt, SemanticEventRecord, SemanticPayloadIdentity};

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
            Self::EventWithoutValidAdmission { event_id } => (event_id, 2, 4),
        }
    }
}

/// One implicated admission observation inside a conflict.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AdmissionConflictOccurrence {
    pub origin: SignedEventSetOrigin,
    pub receipt_id: ReceiptId,
}

/// Cross-item admission-side conflict. The event content itself is never in
/// conflict — equal `SemanticEventId`s cover equal canonical bytes by
/// construction — only admission observations can disagree, and this layer
/// never picks a winner: every implicated observation is excluded and
/// reported.
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
        }
    }
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

fn validate_item(
    origin: SignedEventSetOrigin,
    item: &SignedEventSetItem,
) -> Result<(), SignedEventSetRejection> {
    let event = &item.event;
    let admission = &item.admission;
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

fn store_triple(receipt: &AdmissionReceipt) -> StoreTriple {
    (
        receipt.store_principal,
        receipt.store_control_domain,
        receipt.store_key_id,
    )
}
