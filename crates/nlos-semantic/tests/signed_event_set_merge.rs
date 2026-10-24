//! `DIST-EVENT-001` signed Semantic event set merge primitive tests.
//!
//! Each test exports already-verified `(SemanticEventRecord, AdmissionReceipt)`
//! pairs from real two-domain `SemanticAuthority` stores, then merges the two
//! sets with `merge_signed_event_sets`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_capability::{
    CapabilityAuthority, CapabilityRights, CapabilityTarget, IssueRootCapabilityRequest,
    SignedIssueRootCapabilityRequest, issue_root_command_message,
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_process::{
    CreateIsolationDomainRequest, ProcessAuthority, RegisterDelegatedProcessRequest,
};
use nlos_semantic::{
    AdmissionReceipt, AppendAssertionRequest, AppendDecision, AppendSpecRequest,
    AppendTypedEventRequest, AssertionMode, CriterionAggregation, CriterionEffect,
    CrossDomainAuditRecord, CrossDomainEventItem, EvaluatorKind, ImmutableEvaluatorReference,
    ImmutableEvaluatorReferenceKind, IntentConstraints, IntentCriterion, IntentCriticality,
    IntentSettlement, IntentSpecBody, JudgmentRelation, MergedSignedEvent, SemanticAuthority,
    SemanticPayloadIdentity, SettlementMode, SettlementTimeoutAction, SignedEventSetConflict,
    SignedEventSetItem, SignedEventSetOrigin, SignedEventSetRejection, StoreSigner,
    StoreSignerError, TaintFlags, UnsignedAssertionEvent, UnsignedJudgmentEvent, UnsignedSpecEvent,
    admission_receipt_core_digest, content_digest, encode_intent_spec_body,
    encode_unsigned_assertion_event, encode_unsigned_judgment_event, encode_unsigned_spec_event,
    intent_spec_body_digest, merge_cross_domain_event_sets, merge_signed_event_sets,
    semantic_event_id, verify_and_merge_cross_domain_events,
};
use nlos_types::{
    Generation, IdempotencyKey, NamespaceId, ReceiptId, SemanticEventId, TaskAttemptId, TaskId,
};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Root(PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-semantic-merge-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
struct TestSigner {
    key: SigningKey,
    binding: nlos_identity::IdentityBinding,
}

impl StoreSigner for TestSigner {
    fn principal_id(&self) -> nlos_types::PrincipalId {
        self.binding.principal_id
    }

    fn control_domain_id(&self) -> nlos_types::ControlDomainId {
        self.binding.control_domain_id
    }

    fn key_id(&self) -> nlos_types::KeyId {
        self.binding.key_id
    }

    fn sign(&self, message_digest: &[u8; 32]) -> Result<[u8; 64], StoreSignerError> {
        Ok(self.key.sign(message_digest).to_bytes())
    }
}

/// One admission domain: a full authority stack rooted at one store. Two
/// domains share the same issuer identity (same `issuer_seed`) while signing
/// admissions with different store keys (`store_seed`).
struct Domain {
    identity: IdentityAuthority,
    capability: CapabilityAuthority,
    process: ProcessAuthority,
    semantic: SemanticAuthority,
    issuer_key: SigningKey,
    issuer: nlos_identity::IdentityBinding,
    process_binding: nlos_process::ProcessBindingRecord,
    capability_record: nlos_capability::CapabilityRecord,
    store_signer: TestSigner,
}

fn bootstrap(
    identity: &IdentityAuthority,
    seed: u8,
) -> (SigningKey, nlos_identity::IdentityBinding) {
    let key = SigningKey::from_bytes(&[seed; 32]);
    let binding = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms: 10_000,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
            created_at_ms: 0,
        })
        .unwrap()
        .binding();
    (key, binding)
}

fn domain(root: &Root, issuer_seed: u8, store_seed: u8) -> Domain {
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (issuer_key, issuer) = bootstrap(&identity, issuer_seed);
    let (store_key, store_binding) = bootstrap(&identity, store_seed);
    let process = ProcessAuthority::open(root.path()).unwrap();
    let authority_domain = process
        .create_isolation_domain(CreateIsolationDomainRequest {
            policy_digest: [issuer_seed.wrapping_add(4); 32],
            idempotency_key: IdempotencyKey::from_bytes([issuer_seed.wrapping_add(5); 16]),
            created_at_ms: 0,
        })
        .unwrap()
        .record()
        .clone();
    let process_binding = process
        .register_delegated_process(RegisterDelegatedProcessRequest {
            task_id: TaskId::from_bytes([issuer_seed.wrapping_add(6); 16]),
            task_attempt_id: TaskAttemptId::from_bytes([issuer_seed.wrapping_add(7); 16]),
            attempt_generation: Generation::INITIAL,
            isolation_domain_id: authority_domain.isolation_domain_id,
            isolation_domain_generation: authority_domain.generation,
            isolation_domain_fencing_token: authority_domain.fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([issuer_seed.wrapping_add(8); 16]),
            created_at_ms: 0,
        })
        .unwrap()
        .record()
        .clone();
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let command = IssueRootCapabilityRequest {
        issuer_key_id: issuer.key_id,
        holder_key_id: issuer.key_id,
        target: CapabilityTarget::Namespace(NamespaceId::from_bytes([0x44; 16])),
        rights: CapabilityRights::SEMANTIC_APPEND,
        purpose_digest: Some([0x77; 32]),
        valid_from_ms: 0,
        valid_until_ms: 9_000,
        delegation_depth_remaining: 0,
        call_limit: None,
        idempotency_key: IdempotencyKey::from_bytes([issuer_seed.wrapping_add(9); 16]),
        issued_at_ms: 0,
    };
    let capability_record = capability
        .issue_root_signed(
            &identity,
            SignedIssueRootCapabilityRequest {
                command,
                signer: issuer.principal_id,
                signature: issuer_key
                    .sign(&issue_root_command_message(command))
                    .to_bytes(),
            },
        )
        .unwrap()
        .record();
    let semantic = SemanticAuthority::open(root.path()).unwrap();
    Domain {
        identity,
        capability,
        process,
        semantic,
        issuer_key,
        issuer,
        process_binding,
        capability_record,
        store_signer: TestSigner {
            key: store_key,
            binding: store_binding,
        },
    }
}

fn request_with_nonce(domain: &Domain, seed: u8, nonce: Vec<u8>) -> AppendAssertionRequest {
    let media_type = "text/plain".to_owned();
    let content_bytes = format!("assertion-{seed}").into_bytes();
    let event = UnsignedAssertionEvent {
        scope: domain.capability_record.target,
        issuer: domain.issuer.principal_id,
        issuer_execution: nlos_semantic::LocalProcessRef {
            process_id: domain.process_binding.process_id,
            generation: domain.process_binding.process_generation,
        },
        control_domain: domain.issuer.control_domain_id,
        issued_at_unix_ns: 1_000_000_000 + u64::from(seed),
        nonce,
        declared_parents: Vec::new(),
        declassification_receipt_id: None,
        valid_until_ms: Some(8_000),
        purpose_digest: domain.capability_record.purpose_digest,
        content_digest: content_digest(&media_type, &content_bytes).unwrap(),
        assertion_mode: AssertionMode::Inference,
        execution_evidence_receipt_id: None,
        confidence_bp: Some(8_000),
        key_id: domain.issuer.key_id,
    };
    let canonical_unsigned_event = encode_unsigned_assertion_event(&event).unwrap();
    let claimed_event_id = semantic_event_id(&canonical_unsigned_event);
    let signature = domain
        .issuer_key
        .sign(&nlos_identity::semantic_signature_message(claimed_event_id))
        .to_bytes();
    AppendAssertionRequest {
        canonical_unsigned_event,
        claimed_event_id,
        signature,
        capability: domain.capability_record.handle,
        content_media_type: media_type,
        content_bytes,
        captured_inputs: Vec::new(),
        ingress_taint: TaintFlags::default(),
        authz_policy_digest: [0x99; 32],
        admission_limit_ms: Some(8_500),
        admitted_at_ms: 2_000,
    }
}

fn request(domain: &Domain, seed: u8) -> AppendAssertionRequest {
    request_with_nonce(domain, seed, vec![seed; 16])
}

fn append(domain: &Domain, request: &AppendAssertionRequest) -> AppendDecision {
    domain
        .semantic
        .append_assertion(
            &domain.identity,
            &domain.capability,
            &domain.process,
            &domain.store_signer,
            request,
        )
        .unwrap()
}

fn export_item(domain: &Domain, event_id: SemanticEventId) -> SignedEventSetItem {
    SignedEventSetItem {
        event: domain.semantic.inspect_event(event_id).unwrap(),
        admission: domain.semantic.inspect_admission_receipt(event_id).unwrap(),
    }
}

/// Applies a mutation and re-derives the receipt id from the mutated core
/// digest, mirroring the authority's deterministic receipt-id derivation.
fn reidentified_receipt(
    receipt: &AdmissionReceipt,
    mutate: impl FnOnce(&mut AdmissionReceipt),
) -> AdmissionReceipt {
    let mut mutated = receipt.clone();
    mutate(&mut mutated);
    let mut id_bytes = [0_u8; 16];
    id_bytes.copy_from_slice(&admission_receipt_core_digest(&mutated)[..16]);
    mutated.receipt_id = ReceiptId::from_bytes(id_bytes);
    mutated
}

/// Two-domain twin: one issuer identity admitted in two independent stores.
fn twin_domains() -> (Root, Domain, Domain) {
    let root_a = Root::new("a");
    let root_b = Root::new("b");
    let domain_a = domain(&root_a, 7, 27);
    let domain_b = domain(&root_b, 7, 40);
    // Content-derived identity: the same issuer seed registers the same
    // principal/key ids in both domains.
    assert_eq!(domain_a.issuer.key_id, domain_b.issuer.key_id);
    assert_eq!(domain_a.issuer.principal_id, domain_b.issuer.principal_id);
    assert_ne!(
        domain_a.store_signer.key_id(),
        domain_b.store_signer.key_id()
    );
    (root_a, domain_a, domain_b)
}

#[test]
fn two_domains_same_event_merges_with_both_admissions() {
    let (_root_a, domain_a, domain_b) = twin_domains();
    let request = request(&domain_a, 7);
    let event_id = request.claimed_event_id;
    append(&domain_a, &request);
    let mut request_b = request.clone();
    request_b.capability = domain_b.capability_record.handle;
    append(&domain_b, &request_b);

    let item_a = export_item(&domain_a, event_id);
    let item_b = export_item(&domain_b, event_id);
    assert_eq!(
        item_a.event.canonical_unsigned_event,
        request.canonical_unsigned_event
    );
    assert_ne!(item_a.admission.receipt_id, item_b.admission.receipt_id);

    let report =
        merge_signed_event_sets(std::slice::from_ref(&item_a), std::slice::from_ref(&item_b));
    assert!(report.rejections.is_empty(), "{:?}", report.rejections);
    assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
    assert_eq!(report.merged.len(), 1);
    let merged = &report.merged[0];
    assert_eq!(merged.event_id, event_id);
    assert_eq!(
        merged.canonical_unsigned_event,
        request.canonical_unsigned_event
    );
    assert_eq!(merged.scope, domain_a.capability_record.target);
    assert_eq!(merged.issuer, domain_a.issuer.principal_id);
    assert_eq!(merged.control_domain, domain_a.issuer.control_domain_id);
    assert_eq!(merged.key_id, domain_a.issuer.key_id);
    assert_eq!(
        merged.payload_identity,
        SemanticPayloadIdentity::AssertionContent(
            content_digest(&request.content_media_type, &request.content_bytes).unwrap()
        )
    );
    assert_eq!(merged.admissions.len(), 2);
    assert!(merged.admissions[0].receipt_id < merged.admissions[1].receipt_id);
    assert!(
        merged
            .admissions
            .iter()
            .any(|receipt| receipt.receipt_id == item_a.admission.receipt_id)
    );
    assert!(
        merged
            .admissions
            .iter()
            .any(|receipt| receipt.receipt_id == item_b.admission.receipt_id)
    );
}

#[test]
fn disjoint_events_union_sorted_by_event_id() {
    let (_root_a, domain_a, domain_b) = twin_domains();
    let request_a = request(&domain_a, 7);
    let request_b = {
        let mut other = request(&domain_b, 9);
        other.capability = domain_b.capability_record.handle;
        other
    };
    append(&domain_a, &request_a);
    append(&domain_b, &request_b);
    let item_a = export_item(&domain_a, request_a.claimed_event_id);
    let item_b = export_item(&domain_b, request_b.claimed_event_id);

    let report = merge_signed_event_sets(&[item_a], &[item_b]);
    assert_eq!(
        report.rejections,
        [] as [nlos_semantic::SignedEventSetRejection; 0]
    );
    assert_eq!(
        report.conflicts,
        [] as [nlos_semantic::SignedEventSetConflict; 0]
    );
    assert_eq!(report.merged.len(), 2);
    assert!(report.merged[0].event_id < report.merged[1].event_id);
    assert_eq!(report.merged[0].admissions.len(), 1);
    assert_eq!(report.merged[1].admissions.len(), 1);
}

#[test]
fn identical_items_across_sets_dedup_idempotently() {
    let (_root_a, domain_a, _domain_b) = twin_domains();
    let request = request(&domain_a, 7);
    append(&domain_a, &request);
    let item = export_item(&domain_a, request.claimed_event_id);

    let report = merge_signed_event_sets(std::slice::from_ref(&item), std::slice::from_ref(&item));
    assert_eq!(
        report.rejections,
        [] as [nlos_semantic::SignedEventSetRejection; 0]
    );
    assert_eq!(
        report.conflicts,
        [] as [nlos_semantic::SignedEventSetConflict; 0]
    );
    assert_eq!(report.merged.len(), 1);
    assert_eq!(report.merged[0].admissions.len(), 1);
    assert_eq!(
        report.merged[0].admissions[0].receipt_id,
        item.admission.receipt_id
    );
}

#[test]
fn rejects_event_id_canonical_mismatch() {
    let root = Root::new("single");
    let domain_a = domain(&root, 7, 27);
    let request = request(&domain_a, 7);
    append(&domain_a, &request);
    let mut tampered = export_item(&domain_a, request.claimed_event_id);
    tampered.event.canonical_unsigned_event[0] ^= 0xFF;

    let report = merge_signed_event_sets(&[tampered], &[]);
    assert_eq!(report.merged, [] as [nlos_semantic::MergedSignedEvent; 0]);
    assert_eq!(
        report.conflicts,
        [] as [nlos_semantic::SignedEventSetConflict; 0]
    );
    assert_eq!(report.rejections.len(), 1);
    match &report.rejections[0] {
        SignedEventSetRejection::EventIdMismatch {
            origin,
            claimed_event_id,
            derived_event_id,
        } => {
            assert_eq!(*origin, SignedEventSetOrigin::First);
            assert_eq!(*claimed_event_id, request.claimed_event_id);
            assert_ne!(*derived_event_id, request.claimed_event_id);
        }
        other => panic!("unexpected rejection: {other:?}"),
    }
}

#[test]
fn rejects_admission_event_binding_mismatch() {
    let root = Root::new("single");
    let domain_a = domain(&root, 7, 27);
    let first = request(&domain_a, 7);
    let second = request(&domain_a, 9);
    append(&domain_a, &first);
    append(&domain_a, &second);
    let mut mismatched = export_item(&domain_a, first.claimed_event_id);
    mismatched.admission.event_id = second.claimed_event_id;

    let report = merge_signed_event_sets(&[mismatched], &[]);
    assert_eq!(report.merged, [] as [nlos_semantic::MergedSignedEvent; 0]);
    assert_eq!(
        report.conflicts,
        [] as [nlos_semantic::SignedEventSetConflict; 0]
    );
    assert_eq!(report.rejections.len(), 1);
    match &report.rejections[0] {
        SignedEventSetRejection::AdmissionEventBinding {
            origin,
            event_id,
            admission_event_id,
        } => {
            assert_eq!(*origin, SignedEventSetOrigin::First);
            assert_eq!(*event_id, first.claimed_event_id);
            assert_eq!(*admission_event_id, second.claimed_event_id);
        }
        other => panic!("unexpected rejection: {other:?}"),
    }
}

#[test]
fn rejects_admission_log_seq_binding_mismatch() {
    let root = Root::new("single");
    let domain_a = domain(&root, 7, 27);
    let request = request(&domain_a, 7);
    append(&domain_a, &request);
    let item = export_item(&domain_a, request.claimed_event_id);
    let event_log_seq = item.event.log_seq;
    // Mutate the log position and re-derive the receipt id so only the
    // event/receipt log-seq binding check fails.
    let shifted = reidentified_receipt(&item.admission, |receipt| {
        receipt.log_seq = event_log_seq + 1;
    });
    let mismatched = SignedEventSetItem {
        event: item.event,
        admission: shifted,
    };

    let report = merge_signed_event_sets(&[mismatched], &[]);
    assert_eq!(report.merged, [] as [nlos_semantic::MergedSignedEvent; 0]);
    assert_eq!(
        report.conflicts,
        [] as [nlos_semantic::SignedEventSetConflict; 0]
    );
    assert_eq!(report.rejections.len(), 1);
    match &report.rejections[0] {
        SignedEventSetRejection::AdmissionLogSeqBinding {
            origin,
            event_id,
            event_log_seq: observed_event_log_seq,
            admission_log_seq,
        } => {
            assert_eq!(*origin, SignedEventSetOrigin::First);
            assert_eq!(*event_id, request.claimed_event_id);
            assert_eq!(*observed_event_log_seq, event_log_seq);
            assert_eq!(*admission_log_seq, event_log_seq + 1);
        }
        other => panic!("unexpected rejection: {other:?}"),
    }
}

#[test]
fn rejects_admission_receipt_id_derivation_mismatch() {
    let root = Root::new("single");
    let domain_a = domain(&root, 7, 27);
    let request = request(&domain_a, 7);
    append(&domain_a, &request);
    let mut mismatched = export_item(&domain_a, request.claimed_event_id);
    mismatched.admission.receipt_id = ReceiptId::from_bytes([0xC3; 16]);

    let report = merge_signed_event_sets(&[mismatched], &[]);
    assert_eq!(report.merged, [] as [nlos_semantic::MergedSignedEvent; 0]);
    assert_eq!(
        report.conflicts,
        [] as [nlos_semantic::SignedEventSetConflict; 0]
    );
    assert_eq!(report.rejections.len(), 1);
    match &report.rejections[0] {
        SignedEventSetRejection::AdmissionReceiptIdDerivation {
            origin,
            event_id,
            claimed_receipt_id,
            derived_receipt_id,
        } => {
            assert_eq!(*origin, SignedEventSetOrigin::First);
            assert_eq!(*event_id, request.claimed_event_id);
            assert_eq!(*claimed_receipt_id, ReceiptId::from_bytes([0xC3; 16]));
            assert_ne!(*derived_receipt_id, ReceiptId::from_bytes([0xC3; 16]));
        }
        other => panic!("unexpected rejection: {other:?}"),
    }
}

#[test]
fn receipt_content_conflict_excludes_event_without_other_admission() {
    let (_root_a, domain_a, domain_b) = twin_domains();
    let request = request(&domain_a, 7);
    let event_id = request.claimed_event_id;
    append(&domain_a, &request);
    let mut request_b = request.clone();
    request_b.capability = domain_b.capability_record.handle;
    append(&domain_b, &request_b);
    let item_a = export_item(&domain_a, event_id);
    let item_b = export_item(&domain_b, event_id);
    // Same receipt id, diverging store signature: both observations are
    // implicated and the event keeps no valid admission.
    let mut forged_b = item_b.clone();
    forged_b.admission = item_a.admission.clone();
    forged_b.admission.store_signature[0] ^= 0xFF;

    let report = merge_signed_event_sets(
        std::slice::from_ref(&item_a),
        std::slice::from_ref(&forged_b),
    );
    assert_eq!(report.merged, [] as [nlos_semantic::MergedSignedEvent; 0]);
    assert_eq!(report.conflicts.len(), 1);
    match &report.conflicts[0] {
        SignedEventSetConflict::AdmissionReceiptContent {
            receipt_id,
            occurrences,
        } => {
            assert_eq!(*receipt_id, item_a.admission.receipt_id);
            assert_eq!(occurrences.len(), 2);
        }
        other => {
            panic!("unexpected conflict: {other:?}")
        }
    }
    assert_eq!(report.rejections.len(), 1);
    match &report.rejections[0] {
        SignedEventSetRejection::EventWithoutValidAdmission { event_id: id } => {
            assert_eq!(*id, event_id);
        }
        other => panic!("unexpected rejection: {other:?}"),
    }
}

#[test]
fn duplicate_store_admission_conflict_keeps_other_stores() {
    let (_root_a, domain_a, domain_b) = twin_domains();
    let request = request(&domain_a, 7);
    let event_id = request.claimed_event_id;
    append(&domain_a, &request);
    let mut request_b = request.clone();
    request_b.capability = domain_b.capability_record.handle;
    append(&domain_b, &request_b);
    let item_a = export_item(&domain_a, event_id);
    let item_b = export_item(&domain_b, event_id);
    // A second, self-consistent receipt from the SAME store: the one receipt
    // per store/event invariant makes both store-A observations suspect.
    let duplicate_a = reidentified_receipt(&item_a.admission, |receipt| {
        receipt.admitted_at_ms += 1;
    });
    let duplicated = SignedEventSetItem {
        event: item_a.event.clone(),
        admission: duplicate_a,
    };

    let report = merge_signed_event_sets(&[item_a, duplicated], std::slice::from_ref(&item_b));
    assert_eq!(report.merged.len(), 1);
    let merged: &MergedSignedEvent = &report.merged[0];
    assert_eq!(merged.event_id, event_id);
    assert_eq!(merged.admissions.len(), 1);
    assert_eq!(merged.admissions[0].receipt_id, item_b.admission.receipt_id);
    assert_eq!(
        report.rejections,
        [] as [nlos_semantic::SignedEventSetRejection; 0]
    );
    assert_eq!(report.conflicts.len(), 1);
    match &report.conflicts[0] {
        SignedEventSetConflict::DuplicateStoreAdmission {
            event_id: id,
            occurrences,
        } => {
            assert_eq!(*id, event_id);
            assert_eq!(occurrences.len(), 2);
        }
        other => {
            panic!("unexpected conflict: {other:?}")
        }
    }
}

#[test]
fn same_content_different_nonce_events_merge_separately() {
    let root = Root::new("single");
    let domain_a = domain(&root, 7, 27);
    let first = request_with_nonce(&domain_a, 7, vec![1; 16]);
    let second = request_with_nonce(&domain_a, 7, vec![2; 16]);
    assert_eq!(first.content_bytes, second.content_bytes);
    assert_ne!(first.claimed_event_id, second.claimed_event_id);
    append(&domain_a, &first);
    append(&domain_a, &second);
    let item_first = export_item(&domain_a, first.claimed_event_id);
    let item_second = export_item(&domain_a, second.claimed_event_id);

    let report = merge_signed_event_sets(&[item_first], &[item_second]);
    assert_eq!(
        report.rejections,
        [] as [nlos_semantic::SignedEventSetRejection; 0]
    );
    assert_eq!(
        report.conflicts,
        [] as [nlos_semantic::SignedEventSetConflict; 0]
    );
    // No content-level dedup: distinct signed events stay distinct
    // (`DIST-EVENT-001` forbids CRDT-style semantic collapse).
    assert_eq!(report.merged.len(), 2);
}

#[test]
fn empty_inputs_and_argument_order_independence() {
    let (_root_a, domain_a, domain_b) = twin_domains();
    let report = merge_signed_event_sets(&[], &[]);
    assert_eq!(report.merged, [] as [nlos_semantic::MergedSignedEvent; 0]);
    assert_eq!(
        report.rejections,
        [] as [nlos_semantic::SignedEventSetRejection; 0]
    );
    assert_eq!(
        report.conflicts,
        [] as [nlos_semantic::SignedEventSetConflict; 0]
    );

    let request = request(&domain_a, 7);
    append(&domain_a, &request);
    let mut request_b = request.clone();
    request_b.capability = domain_b.capability_record.handle;
    append(&domain_b, &request_b);
    let item_a = export_item(&domain_a, request.claimed_event_id);
    let item_b = export_item(&domain_b, request.claimed_event_id);

    let forward =
        merge_signed_event_sets(std::slice::from_ref(&item_a), std::slice::from_ref(&item_b));
    let backward =
        merge_signed_event_sets(std::slice::from_ref(&item_b), std::slice::from_ref(&item_a));
    assert_eq!(forward.merged, backward.merged);
    assert_eq!(forward.merged.len(), 1);
}

fn spec_request(domain: &Domain, seed: u8) -> AppendSpecRequest {
    let body = IntentSpecBody {
        goal_digest: [seed.wrapping_add(0x10); 32],
        acceptance: vec![IntentCriterion {
            description_digest: [seed.wrapping_add(0x11); 32],
            effect: CriterionEffect::Hard,
            evaluator_kind: EvaluatorKind::DeterministicTool,
            evaluator_ref: ImmutableEvaluatorReference {
                kind: ImmutableEvaluatorReferenceKind::Artifact,
                digest: [seed.wrapping_add(0x12); 32],
            },
            target_selector_digest: [seed.wrapping_add(0x13); 32],
            timeout_ms: Some(5_000),
            independence_policy_digest: None,
            authority_policy_digest: None,
            risk_policy_digest: None,
            aggregation: CriterionAggregation {
                pass_quorum: 1,
                fail_quorum: 1,
                veto_on_authorized_fail: true,
            },
        }],
        constraints: IntentConstraints {
            resource_vector_digest: [seed.wrapping_add(0x14); 32],
            deadline_ms: Some(8_000),
            namespace_root: NamespaceId::from_bytes([0x44; 16]),
            allowed_capability_digests: Vec::new(),
            forbidden_capability_digests: Vec::new(),
        },
        criticality: IntentCriticality::Standard,
        settlement: IntentSettlement {
            mode: SettlementMode::None,
            hard_criteria_digest: None,
            on_timeout: SettlementTimeoutAction::Refund,
            challenge_window_ms: None,
        },
        critical_extensions: Vec::new(),
        noncritical_extensions: Vec::new(),
    };
    let canonical_spec_body = encode_intent_spec_body(&body).unwrap();
    let spec_body_digest = intent_spec_body_digest(&body).unwrap();
    let event = UnsignedSpecEvent {
        scope: domain.capability_record.target,
        issuer: domain.issuer.principal_id,
        issuer_execution: nlos_semantic::LocalProcessRef {
            process_id: domain.process_binding.process_id,
            generation: domain.process_binding.process_generation,
        },
        control_domain: domain.issuer.control_domain_id,
        issued_at_unix_ns: 2_000_000_000 + u64::from(seed),
        nonce: vec![seed.wrapping_add(0x20); 16],
        declared_parents: Vec::new(),
        valid_until_ms: Some(8_000),
        purpose_digest: domain.capability_record.purpose_digest,
        spec_body_digest,
        canonical_spec_body,
        key_id: domain.issuer.key_id,
    };
    let canonical_unsigned_event = encode_unsigned_spec_event(&event).unwrap();
    let claimed_event_id = semantic_event_id(&canonical_unsigned_event);
    let signature = domain
        .issuer_key
        .sign(&nlos_identity::semantic_signature_message(claimed_event_id))
        .to_bytes();
    AppendSpecRequest {
        canonical_unsigned_event,
        claimed_event_id,
        signature,
        capability: domain.capability_record.handle,
        captured_inputs: Vec::new(),
        ingress_taint: TaintFlags::default(),
        authz_policy_digest: [0x99; 32],
        admission_limit_ms: Some(8_500),
        admitted_at_ms: 2_100,
    }
}

fn judgment_request(
    domain: &Domain,
    seed: u8,
    source: SemanticEventId,
    target: SemanticEventId,
) -> AppendTypedEventRequest {
    let event = UnsignedJudgmentEvent {
        scope: domain.capability_record.target,
        issuer: domain.issuer.principal_id,
        issuer_execution: nlos_semantic::LocalProcessRef {
            process_id: domain.process_binding.process_id,
            generation: domain.process_binding.process_generation,
        },
        control_domain: domain.issuer.control_domain_id,
        issued_at_unix_ns: 3_000_000_000 + u64::from(seed),
        nonce: vec![seed.wrapping_add(0x30); 16],
        declared_parents: Vec::new(),
        valid_until_ms: Some(8_000),
        purpose_digest: domain.capability_record.purpose_digest,
        key_id: domain.issuer.key_id,
        relation: JudgmentRelation::Supports,
        source,
        target,
        context_digest: None,
        evaluator_evidence_receipt_id: ReceiptId::from_bytes([seed.wrapping_add(0x31); 16]),
        confidence_bp: Some(9_000),
    };
    let canonical_unsigned_event = encode_unsigned_judgment_event(&event).unwrap();
    let claimed_event_id = semantic_event_id(&canonical_unsigned_event);
    let signature = domain
        .issuer_key
        .sign(&nlos_identity::semantic_signature_message(claimed_event_id))
        .to_bytes();
    AppendTypedEventRequest {
        canonical_unsigned_event,
        claimed_event_id,
        signature,
        capability: domain.capability_record.handle,
        captured_inputs: Vec::new(),
        ingress_taint: TaintFlags::default(),
        authz_policy_digest: [0x99; 32],
        admission_limit_ms: Some(8_500),
        admitted_at_ms: 2_200,
    }
}

fn append_spec(domain: &Domain, request: &AppendSpecRequest) {
    let mut bound = request.clone();
    bound.capability = domain.capability_record.handle;
    domain
        .semantic
        .append_spec(
            &domain.identity,
            &domain.capability,
            &domain.process,
            &domain.store_signer,
            &bound,
        )
        .unwrap();
}

fn append_judgment(domain: &Domain, request: &AppendTypedEventRequest) {
    let mut bound = request.clone();
    bound.capability = domain.capability_record.handle;
    domain
        .semantic
        .append_judgment(
            &domain.identity,
            &domain.capability,
            &domain.process,
            &domain.store_signer,
            &bound,
        )
        .unwrap();
}

#[test]
fn store_backed_export_signed_event_set_across_event_types_and_idempotent_merge() {
    let (_root_a, domain_a, domain_b) = twin_domains();

    let assertion = request(&domain_a, 7);
    let spec = spec_request(&domain_a, 8);
    let judgment = judgment_request(
        &domain_a,
        9,
        assertion.claimed_event_id,
        spec.claimed_event_id,
    );

    append(&domain_a, &assertion);
    append_spec(&domain_a, &spec);
    append_judgment(&domain_a, &judgment);

    // Domain B admits the extra assertion first so local log_seq values
    // intentionally diverge from Domain A (`[SEM-CHECKPOINT-001]`).
    let mut only_b = request(&domain_b, 11);
    only_b.capability = domain_b.capability_record.handle;
    append(&domain_b, &only_b);

    let mut assertion_b = assertion.clone();
    assertion_b.capability = domain_b.capability_record.handle;
    append(&domain_b, &assertion_b);
    append_spec(&domain_b, &spec);
    append_judgment(&domain_b, &judgment);

    let exported_a = domain_a.semantic.export_signed_event_set().unwrap();
    let exported_b = domain_b.semantic.export_signed_event_set().unwrap();
    assert_eq!(exported_a.len(), 3);
    assert_eq!(exported_b.len(), 4);
    assert_eq!(
        exported_a
            .iter()
            .map(|item| item.event.log_seq)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(
        exported_b
            .iter()
            .map(|item| item.event.log_seq)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );

    let report = merge_signed_event_sets(&exported_a, &exported_b);
    assert!(report.rejections.is_empty(), "{:?}", report.rejections);
    assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
    assert_eq!(report.merged.len(), 4);

    // The 3 shared events each carry 2 admissions (with distinct store-local
    // log_seq values); the B-only event carries 1 admission.
    for shared_id in [
        assertion.claimed_event_id,
        spec.claimed_event_id,
        judgment.claimed_event_id,
    ] {
        let entry = report
            .merged
            .iter()
            .find(|entry| entry.event_id == shared_id)
            .unwrap();
        assert_eq!(entry.admissions.len(), 2);
    }
    let only_b_entry = report
        .merged
        .iter()
        .find(|entry| entry.event_id == only_b.claimed_event_id)
        .unwrap();
    assert_eq!(only_b_entry.admissions.len(), 1);

    // Idempotent self-merge on store-exported slice.
    let self_report = merge_signed_event_sets(&exported_a, &exported_a);
    assert_eq!(
        self_report.rejections,
        [] as [nlos_semantic::SignedEventSetRejection; 0]
    );
    assert_eq!(
        self_report.conflicts,
        [] as [nlos_semantic::SignedEventSetConflict; 0]
    );
    assert_eq!(self_report.merged.len(), 3);
    assert!(
        self_report
            .merged
            .iter()
            .all(|entry| entry.admissions.len() == 1)
    );
}

#[test]
fn dist_audit_001_cross_domain_export_and_verified_merge_preserves_provenance() {
    let (_root_a, domain_a, domain_b) = twin_domains();
    let request = request(&domain_a, 7);
    let event_id = request.claimed_event_id;
    append(&domain_a, &request);
    let mut request_b = request.clone();
    request_b.capability = domain_b.capability_record.handle;
    append(&domain_b, &request_b);

    let sig_a = domain_a.semantic.inspect_event_signature(event_id).unwrap();
    let sig_b = domain_b.semantic.inspect_event_signature(event_id).unwrap();
    assert_eq!(sig_a, request.signature);
    assert_eq!(sig_b, request.signature);

    let snapshot_a = [0xA1; 32];
    let snapshot_b = [0xB2; 32];
    let cross_a = domain_a
        .semantic
        .export_cross_domain_event_set(snapshot_a)
        .unwrap();
    let cross_b = domain_b
        .semantic
        .export_cross_domain_event_set(snapshot_b)
        .unwrap();
    assert_eq!(cross_a.len(), 1);
    assert_eq!(cross_b.len(), 1);

    let report = verify_and_merge_cross_domain_events(&domain_a.identity, &cross_a, &cross_b);
    assert!(report.rejections.is_empty(), "{:?}", report.rejections);
    assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
    assert_eq!(report.merged.len(), 1);

    let merged = &report.merged[0];
    assert_eq!(merged.event_id, event_id);
    assert_eq!(merged.producer_signature, request.signature);
    assert_eq!(merged.admissions.len(), 2);
    assert_eq!(merged.audit_records.len(), 2);

    let expected_records: Vec<CrossDomainAuditRecord> = {
        let mut records = vec![
            CrossDomainAuditRecord {
                producer_key_id: domain_a.issuer.key_id,
                producer_signature: request.signature,
                admission: cross_a[0].admission.clone(),
                trust_snapshot_digest: snapshot_a,
            },
            CrossDomainAuditRecord {
                producer_key_id: domain_b.issuer.key_id,
                producer_signature: request.signature,
                admission: cross_b[0].admission.clone(),
                trust_snapshot_digest: snapshot_b,
            },
        ];
        records.sort_by_key(|record| record.admission.receipt_id);
        records
    };
    assert_eq!(merged.audit_records, expected_records);
}

#[test]
fn dist_audit_001_rejects_missing_or_zero_trust_snapshot_fail_closed() {
    let root = Root::new("zero-snapshot");
    let domain_a = domain(&root, 7, 27);
    let request = request(&domain_a, 7);
    append(&domain_a, &request);

    let items = domain_a
        .semantic
        .export_cross_domain_event_set([0_u8; 32])
        .unwrap();
    let report = verify_and_merge_cross_domain_events(&domain_a.identity, &items, &[]);
    assert_eq!(
        report.merged,
        [] as [nlos_semantic::MergedCrossDomainEvent; 0]
    );
    assert_eq!(
        report.conflicts,
        [] as [nlos_semantic::SignedEventSetConflict; 0]
    );
    assert_eq!(report.rejections.len(), 1);
    match &report.rejections[0] {
        SignedEventSetRejection::MissingTrustSnapshot {
            origin,
            event_id,
            receipt_id,
        } => {
            assert_eq!(*origin, SignedEventSetOrigin::First);
            assert_eq!(*event_id, request.claimed_event_id);
            assert_eq!(*receipt_id, items[0].admission.receipt_id);
        }
        other => panic!("unexpected rejection: {other:?}"),
    }
}

#[test]
fn dist_audit_001_rejects_invalid_or_zero_producer_signature_fail_closed() {
    let root = Root::new("bad-sig");
    let domain_a = domain(&root, 7, 27);
    let request = request(&domain_a, 7);
    append(&domain_a, &request);

    let mut forged = domain_a
        .semantic
        .export_cross_domain_event_set([0xCC; 32])
        .unwrap();
    forged[0].producer_signature[0] ^= 0xFF;

    let report = verify_and_merge_cross_domain_events(&domain_a.identity, &forged, &[]);
    assert_eq!(
        report.merged,
        [] as [nlos_semantic::MergedCrossDomainEvent; 0]
    );
    assert_eq!(
        report.conflicts,
        [] as [nlos_semantic::SignedEventSetConflict; 0]
    );
    assert_eq!(report.rejections.len(), 1);
    match &report.rejections[0] {
        SignedEventSetRejection::InvalidProducerSignature {
            origin,
            event_id,
            key_id,
        } => {
            assert_eq!(*origin, SignedEventSetOrigin::First);
            assert_eq!(*event_id, request.claimed_event_id);
            assert_eq!(*key_id, domain_a.issuer.key_id);
        }
        other => panic!("unexpected rejection: {other:?}"),
    }

    let mut zeroed = domain_a
        .semantic
        .export_cross_domain_event_set([0xCC; 32])
        .unwrap();
    zeroed[0].producer_signature = [0_u8; 64];
    let zero_report = merge_cross_domain_event_sets(&zeroed, &[]);
    assert_eq!(
        zero_report.merged,
        [] as [nlos_semantic::MergedCrossDomainEvent; 0]
    );
    assert_eq!(
        zero_report.rejections,
        vec![SignedEventSetRejection::InvalidProducerSignature {
            origin: SignedEventSetOrigin::First,
            event_id: request.claimed_event_id,
            key_id: domain_a.issuer.key_id,
        }]
    );
}

#[test]
fn dist_audit_001_rejects_conflicting_producer_signatures_across_domains_fail_closed() {
    let (_root_a, domain_a, domain_b) = twin_domains();
    let request = request(&domain_a, 7);
    let event_id = request.claimed_event_id;
    append(&domain_a, &request);
    let mut request_b = request.clone();
    request_b.capability = domain_b.capability_record.handle;
    append(&domain_b, &request_b);

    let cross_a = domain_a
        .semantic
        .export_cross_domain_event_set([0xA1; 32])
        .unwrap();
    let mut cross_b: Vec<CrossDomainEventItem> = domain_b
        .semantic
        .export_cross_domain_event_set([0xB2; 32])
        .unwrap();
    cross_b[0].producer_signature[0] ^= 0x55;

    let report = merge_cross_domain_event_sets(&cross_a, &cross_b);
    assert_eq!(
        report.merged,
        [] as [nlos_semantic::MergedCrossDomainEvent; 0]
    );
    assert_eq!(report.conflicts.len(), 1);
    match &report.conflicts[0] {
        SignedEventSetConflict::ProducerSignatureMismatch {
            event_id: conflict_id,
            occurrences,
        } => {
            assert_eq!(*conflict_id, event_id);
            assert_eq!(occurrences.len(), 2);
        }
        other => panic!("unexpected conflict: {other:?}"),
    }
    assert_eq!(
        report.rejections,
        vec![SignedEventSetRejection::ProducerSignatureConflict { event_id }]
    );
}

#[test]
fn dist_audit_001_rejects_conflicting_trust_snapshots_for_same_receipt_id() {
    let root = Root::new("snapshot-conflict");
    let domain_a = domain(&root, 7, 27);
    let request = request(&domain_a, 7);
    let event_id = request.claimed_event_id;
    append(&domain_a, &request);

    let cross_first = domain_a
        .semantic
        .export_cross_domain_event_set([0x11; 32])
        .unwrap();
    let cross_second = domain_a
        .semantic
        .export_cross_domain_event_set([0x22; 32])
        .unwrap();

    let report = merge_cross_domain_event_sets(&cross_first, &cross_second);
    assert_eq!(
        report.merged,
        [] as [nlos_semantic::MergedCrossDomainEvent; 0]
    );
    assert_eq!(report.conflicts.len(), 1);
    match &report.conflicts[0] {
        SignedEventSetConflict::TrustSnapshotConflict {
            receipt_id,
            occurrences,
        } => {
            assert_eq!(*receipt_id, cross_first[0].admission.receipt_id);
            assert_eq!(occurrences.len(), 2);
        }
        other => panic!("unexpected conflict: {other:?}"),
    }
    assert_eq!(
        report.rejections,
        vec![SignedEventSetRejection::EventWithoutValidAdmission { event_id }]
    );
}
