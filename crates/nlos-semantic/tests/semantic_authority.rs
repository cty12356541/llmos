#![allow(deprecated)] // Deprecated unsigned Capability entries; the signed-entry migration is an ADR-0010 follow-up.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey, Verifier};
use nlos_capability::{
    CapabilityAuthority, CapabilityRights, CapabilityTarget, IssueRootCapabilityRequest,
    RevokeCapabilityRequest,
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_process::{
    CreateIsolationDomainRequest, ProcessAuthority, RegisterDelegatedProcessRequest,
};
use nlos_semantic::{
    AcknowledgeOutboxRequest, AdmissionReceipt, AppendAssertionRequest, AppendDecision,
    AppendSpecRequest, AssertionMode, CriterionAggregation, CriterionEffect, EvaluatorKind,
    ImmutableEvaluatorReference, ImmutableEvaluatorReferenceKind, IntentConstraints,
    IntentCriterion, IntentCriticality, IntentSettlement, IntentSpecBody, LocalProcessRef,
    OutboxAckDecision, PublishSemanticPublicationRequest, SemanticAuthority,
    SemanticAuthorityError, SemanticPayloadIdentity, SemanticPublicationDecision, SettlementMode,
    SettlementTimeoutAction, StoreSigner, StoreSignerError, TaintFlags, UnsignedAssertionEvent,
    UnsignedSpecEvent, admission_receipt_core_digest, admission_receipt_signature_message,
    content_digest, decode_unsigned_assertion_event, decode_unsigned_spec_event,
    encode_intent_spec_body, encode_unsigned_assertion_event, encode_unsigned_spec_event,
    hard_criteria_digest, intent_spec_body_digest, semantic_event_id,
};
use nlos_types::{
    CommitPermitId, Generation, IdempotencyKey, NamespaceId, ReceiptId, SemanticEventId,
    TaskAttemptId, TaskId,
};
use rusqlite::Connection;

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Root(PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-semantic-{label}-{}-{nonce}-{}",
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

struct Fixture {
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

fn fixture(root: &Root, seed: u8) -> Fixture {
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (issuer_key, issuer) = bootstrap(&identity, seed);
    let (store_key, store_binding) = bootstrap(&identity, seed.wrapping_add(20));
    let process = ProcessAuthority::open(root.path()).unwrap();
    let domain = process
        .create_isolation_domain(CreateIsolationDomainRequest {
            policy_digest: [seed.wrapping_add(4); 32],
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(5); 16]),
            created_at_ms: 0,
        })
        .unwrap()
        .record()
        .clone();
    let process_binding = process
        .register_delegated_process(RegisterDelegatedProcessRequest {
            task_id: TaskId::from_bytes([seed.wrapping_add(6); 16]),
            task_attempt_id: TaskAttemptId::from_bytes([seed.wrapping_add(7); 16]),
            attempt_generation: Generation::INITIAL,
            isolation_domain_id: domain.isolation_domain_id,
            isolation_domain_generation: domain.generation,
            isolation_domain_fencing_token: domain.fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(8); 16]),
            created_at_ms: 0,
        })
        .unwrap()
        .record()
        .clone();
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let capability_record = capability
        .issue_root(
            &identity,
            IssueRootCapabilityRequest {
                issuer_key_id: issuer.key_id,
                holder_key_id: issuer.key_id,
                target: CapabilityTarget::Namespace(NamespaceId::from_bytes([0x44; 16])),
                rights: CapabilityRights::SEMANTIC_APPEND,
                purpose_digest: Some([0x77; 32]),
                valid_from_ms: 0,
                valid_until_ms: 9_000,
                delegation_depth_remaining: 0,
                call_limit: None,
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(9); 16]),
                issued_at_ms: 0,
            },
        )
        .unwrap()
        .record();
    let semantic = SemanticAuthority::open(root.path()).unwrap();
    Fixture {
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

fn request(
    fixture: &Fixture,
    seed: u8,
    parents: Vec<SemanticEventId>,
    captured_inputs: Vec<SemanticEventId>,
    ingress_taint: TaintFlags,
) -> AppendAssertionRequest {
    let media_type = "text/plain".to_owned();
    let content_bytes = format!("assertion-{seed}").into_bytes();
    let event = UnsignedAssertionEvent {
        scope: fixture.capability_record.target,
        issuer: fixture.issuer.principal_id,
        issuer_execution: LocalProcessRef {
            process_id: fixture.process_binding.process_id,
            generation: fixture.process_binding.process_generation,
        },
        control_domain: fixture.issuer.control_domain_id,
        issued_at_unix_ns: 1_000_000_000 + u64::from(seed),
        nonce: vec![seed; 16],
        declared_parents: parents,
        valid_until_ms: Some(8_000),
        purpose_digest: fixture.capability_record.purpose_digest,
        content_digest: content_digest(&media_type, &content_bytes).unwrap(),
        assertion_mode: AssertionMode::Inference,
        execution_evidence_receipt_id: None,
        confidence_bp: Some(8_000),
        key_id: fixture.issuer.key_id,
    };
    let canonical_unsigned_event = encode_unsigned_assertion_event(&event).unwrap();
    let claimed_event_id = semantic_event_id(&canonical_unsigned_event);
    let signature = fixture
        .issuer_key
        .sign(&nlos_identity::semantic_signature_message(claimed_event_id))
        .to_bytes();
    AppendAssertionRequest {
        canonical_unsigned_event,
        claimed_event_id,
        signature,
        capability: fixture.capability_record.handle,
        content_media_type: media_type,
        content_bytes,
        captured_inputs,
        ingress_taint,
        authz_policy_digest: [0x99; 32],
        admission_limit_ms: Some(8_500),
        admitted_at_ms: 2_000,
    }
}

fn append(fixture: &Fixture, request: &AppendAssertionRequest) -> AppendDecision {
    fixture
        .semantic
        .append_assertion(
            &fixture.identity,
            &fixture.capability,
            &fixture.process,
            &fixture.store_signer,
            request,
        )
        .unwrap()
}

fn spec_body() -> IntentSpecBody {
    let criterion = IntentCriterion {
        description_digest: [0xa1; 32],
        effect: CriterionEffect::Hard,
        evaluator_kind: EvaluatorKind::DeterministicTool,
        evaluator_ref: ImmutableEvaluatorReference {
            kind: ImmutableEvaluatorReferenceKind::Artifact,
            digest: [0xa2; 32],
        },
        target_selector_digest: [0xa3; 32],
        timeout_ms: Some(5_000),
        independence_policy_digest: None,
        authority_policy_digest: None,
        risk_policy_digest: None,
        aggregation: CriterionAggregation {
            pass_quorum: 1,
            fail_quorum: 1,
            veto_on_authorized_fail: true,
        },
    };
    let mut body = IntentSpecBody {
        goal_digest: [0xa0; 32],
        acceptance: vec![criterion],
        constraints: IntentConstraints {
            resource_vector_digest: [0xa4; 32],
            deadline_ms: Some(8_000),
            namespace_root: NamespaceId::from_bytes([0x44; 16]),
            allowed_capability_digests: vec![[0xa5; 32]],
            forbidden_capability_digests: vec![[0xa6; 32]],
        },
        criticality: IntentCriticality::Standard,
        settlement: IntentSettlement {
            mode: SettlementMode::Automatic,
            hard_criteria_digest: None,
            on_timeout: SettlementTimeoutAction::Dispute,
            challenge_window_ms: Some(1_000),
        },
        critical_extensions: Vec::new(),
        noncritical_extensions: Vec::new(),
    };
    body.settlement.hard_criteria_digest = hard_criteria_digest(&body).unwrap();
    body
}

fn spec_request(fixture: &Fixture, seed: u8, parents: Vec<SemanticEventId>) -> AppendSpecRequest {
    let body = spec_body();
    let canonical_spec_body = encode_intent_spec_body(&body).unwrap();
    let event = UnsignedSpecEvent {
        scope: fixture.capability_record.target,
        issuer: fixture.issuer.principal_id,
        issuer_execution: LocalProcessRef {
            process_id: fixture.process_binding.process_id,
            generation: fixture.process_binding.process_generation,
        },
        control_domain: fixture.issuer.control_domain_id,
        issued_at_unix_ns: 2_000_000_000 + u64::from(seed),
        nonce: vec![seed; 16],
        declared_parents: parents,
        valid_until_ms: Some(8_000),
        purpose_digest: fixture.capability_record.purpose_digest,
        spec_body_digest: intent_spec_body_digest(&body).unwrap(),
        canonical_spec_body,
        key_id: fixture.issuer.key_id,
    };
    let canonical_unsigned_event = encode_unsigned_spec_event(&event).unwrap();
    let claimed_event_id = semantic_event_id(&canonical_unsigned_event);
    AppendSpecRequest {
        canonical_unsigned_event,
        claimed_event_id,
        signature: fixture
            .issuer_key
            .sign(&nlos_identity::semantic_signature_message(claimed_event_id))
            .to_bytes(),
        capability: fixture.capability_record.handle,
        captured_inputs: Vec::new(),
        ingress_taint: TaintFlags::default(),
        authz_policy_digest: [0xb0; 32],
        admission_limit_ms: Some(8_500),
        admitted_at_ms: 2_000,
    }
}

#[test]
fn canonical_event_round_trips_and_rejects_noncanonical_or_invalid_payloads() {
    let root = Root::new("canonical");
    let fixture = fixture(&root, 10);
    let request = request(&fixture, 1, Vec::new(), Vec::new(), TaintFlags::default());
    let decoded = decode_unsigned_assertion_event(&request.canonical_unsigned_event).unwrap();
    assert_eq!(
        encode_unsigned_assertion_event(&decoded).unwrap(),
        request.canonical_unsigned_event
    );
    assert_eq!(
        semantic_event_id(&request.canonical_unsigned_event),
        request.claimed_event_id
    );
    let mut trailing = request.canonical_unsigned_event.clone();
    trailing.push(0);
    assert!(matches!(
        decode_unsigned_assertion_event(&trailing),
        Err(SemanticAuthorityError::CanonicalMismatch)
    ));
    let mut bad = decoded;
    bad.nonce.clear();
    assert!(matches!(
        encode_unsigned_assertion_event(&bad),
        Err(SemanticAuthorityError::InvalidNonce)
    ));
    bad.nonce = vec![1; 16];
    bad.assertion_mode = AssertionMode::FactFromTool;
    assert!(matches!(
        encode_unsigned_assertion_event(&bad),
        Err(SemanticAuthorityError::MissingExecutionEvidence)
    ));
}

#[test]
fn admission_is_durable_signed_atomic_and_exactly_replayable() {
    let root = Root::new("admit");
    let (request, receipt) = {
        let fixture = fixture(&root, 30);
        let request = request(&fixture, 2, Vec::new(), Vec::new(), TaintFlags::PRIVATE);
        let first = append(&fixture, &request);
        assert!(matches!(first, AppendDecision::Admitted(_)));
        let replay = append(&fixture, &request);
        assert!(matches!(replay, AppendDecision::Replayed(_)));
        assert_eq!(first.receipt(), replay.receipt());
        let receipt = first.receipt().clone();
        verify_store_receipt(&fixture.store_signer, &receipt);
        let outbox = fixture
            .semantic
            .inspect_outbox(request.claimed_event_id)
            .unwrap();
        assert_eq!(outbox.log_seq, receipt.log_seq);
        assert_eq!(outbox.event_id, request.claimed_event_id);
        assert_eq!(outbox.receipt_id, receipt.receipt_id);
        assert_eq!(outbox.acknowledged_at_ms, None);
        (request, receipt)
    };

    let fixture = fixture(&root, 30);
    let replay = append(&fixture, &request);
    assert_eq!(replay.receipt(), &receipt);
    let record = fixture
        .semantic
        .inspect_event(request.claimed_event_id)
        .unwrap();
    assert_eq!(record.event_id, request.claimed_event_id);
    assert_eq!(record.log_seq, 1);
    assert_eq!(receipt.effective_valid_until_ms, Some(8_000));
    assert_eq!(receipt.effective_taint, TaintFlags::PRIVATE);
    assert_eq!(
        fixture
            .semantic
            .inspect_outbox(request.claimed_event_id)
            .unwrap()
            .receipt_id,
        receipt.receipt_id
    );
}

#[test]
#[allow(clippy::too_many_lines)] // Keep the owner binding and monotonic ACK matrix together.
fn outbox_ack_is_owner_bound_monotonic_and_not_publication_proof() {
    let root = Root::new("outbox-ack");
    let (request, receipt) = {
        let fixture = fixture(&root, 75);
        let request = request(&fixture, 2, Vec::new(), Vec::new(), TaintFlags::PRIVATE);
        let receipt = append(&fixture, &request).receipt().clone();
        (request, receipt)
    };

    let authority_fixture = fixture(&root, 75);
    assert!(matches!(
        authority_fixture
            .semantic
            .acknowledge_outbox(AcknowledgeOutboxRequest {
                event_id: request.claimed_event_id,
                log_seq: receipt.log_seq,
                receipt_id: receipt.receipt_id,
                acknowledged_at_ms: receipt.admitted_at_ms - 1,
            }),
        Err(SemanticAuthorityError::OutboxAckBeforeAdmission)
    ));
    let ack = AcknowledgeOutboxRequest {
        event_id: request.claimed_event_id,
        log_seq: receipt.log_seq,
        receipt_id: receipt.receipt_id,
        acknowledged_at_ms: receipt.admitted_at_ms + 100,
    };
    let first = authority_fixture.semantic.acknowledge_outbox(ack).unwrap();
    assert!(matches!(first, OutboxAckDecision::Recorded(_)));
    assert_eq!(first.record().acknowledged_at_ms, Some(2_100));
    let replay = authority_fixture.semantic.acknowledge_outbox(ack).unwrap();
    assert!(matches!(replay, OutboxAckDecision::Replayed(_)));
    assert_eq!(replay.record(), first.record());
    assert!(matches!(
        authority_fixture
            .semantic
            .acknowledge_outbox(AcknowledgeOutboxRequest {
                acknowledged_at_ms: 2_099,
                ..ack
            }),
        Err(SemanticAuthorityError::OutboxAckNotMonotonic {
            previous: 2_100,
            reported: 2_099
        })
    ));
    let later = authority_fixture
        .semantic
        .acknowledge_outbox(AcknowledgeOutboxRequest {
            acknowledged_at_ms: 2_200,
            ..ack
        })
        .unwrap();
    assert!(matches!(later, OutboxAckDecision::Recorded(_)));
    assert_eq!(later.record().acknowledged_at_ms, Some(2_200));
    assert_eq!(
        authority_fixture
            .semantic
            .inspect_outbox(request.claimed_event_id)
            .unwrap()
            .acknowledged_at_ms,
        Some(2_200)
    );
    assert!(matches!(
        authority_fixture
            .semantic
            .acknowledge_outbox(AcknowledgeOutboxRequest {
                log_seq: receipt.log_seq + 1,
                ..ack
            }),
        Err(SemanticAuthorityError::OutboxAckBindingMismatch)
    ));

    let reopened = fixture(&root, 75);
    let recovered = reopened
        .semantic
        .inspect_outbox(request.claimed_event_id)
        .unwrap();
    assert_eq!(recovered.receipt_id, receipt.receipt_id);
    assert_eq!(recovered.acknowledged_at_ms, Some(2_200));
    assert!(matches!(
        reopened
            .semantic
            .acknowledge_outbox(AcknowledgeOutboxRequest {
                acknowledged_at_ms: 2_200,
                ..ack
            }),
        Ok(OutboxAckDecision::Replayed(_))
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn semantic_publication_receipt_is_owner_derived_durable_and_replayable() {
    let root = Root::new("semantic-publication");
    let (request, admission) = {
        let fixture = fixture(&root, 151);
        let request = request(&fixture, 6, Vec::new(), Vec::new(), TaintFlags::default());
        let admission = append(&fixture, &request).receipt().clone();
        let publication = fixture
            .semantic
            .publish_semantic_publication(PublishSemanticPublicationRequest {
                task_id: TaskId::from_bytes([0x11; 16]),
                permit_id: CommitPermitId::from_bytes([0x22; 16]),
                write_set_root: [0x33; 32],
                event_id: request.claimed_event_id,
                target: fixture.capability_record.target,
                admission_receipt_id: admission.receipt_id,
                durability_receipt_id: None,
                published_at_ms: 2_500,
            })
            .unwrap();
        assert!(matches!(
            publication,
            SemanticPublicationDecision::Published(_)
        ));
        let receipt = publication.receipt();
        assert_eq!(receipt.event_id, request.claimed_event_id);
        assert_eq!(receipt.admission_receipt_id, admission.receipt_id);
        assert_eq!(receipt.log_seq, admission.log_seq);
        assert_ne!(receipt.semantic_checkpoint_after, [0; 32]);
        (request, admission)
    };

    let authority_fixture = fixture(&root, 151);
    let publication_request = PublishSemanticPublicationRequest {
        task_id: TaskId::from_bytes([0x11; 16]),
        permit_id: CommitPermitId::from_bytes([0x22; 16]),
        write_set_root: [0x33; 32],
        event_id: request.claimed_event_id,
        target: authority_fixture.capability_record.target,
        admission_receipt_id: admission.receipt_id,
        durability_receipt_id: None,
        published_at_ms: 9_999,
    };
    let replay = authority_fixture
        .semantic
        .publish_semantic_publication(publication_request)
        .unwrap();
    assert!(matches!(replay, SemanticPublicationDecision::Replayed(_)));
    assert_eq!(replay.receipt().created_at_ms, 2_500);
    let receipt = replay.receipt();
    assert_eq!(
        authority_fixture
            .semantic
            .inspect_publication_receipt(receipt.receipt_id)
            .unwrap(),
        receipt
    );

    assert!(matches!(
        authority_fixture.semantic.publish_semantic_publication(
            PublishSemanticPublicationRequest {
                target: CapabilityTarget::Task(TaskId::from_bytes([0x44; 16])),
                ..publication_request
            }
        ),
        Err(SemanticAuthorityError::SemanticPublicationTargetMismatch)
    ));
    assert!(matches!(
        authority_fixture.semantic.publish_semantic_publication(
            PublishSemanticPublicationRequest {
                admission_receipt_id: ReceiptId::from_bytes([0x55; 16]),
                ..publication_request
            }
        ),
        Err(SemanticAuthorityError::SemanticPublicationAdmissionBindingMismatch)
    ));
    drop(authority_fixture);

    let reopened = fixture(&root, 151);
    assert_eq!(
        reopened
            .semantic
            .inspect_publication_receipt(receipt.receipt_id)
            .unwrap(),
        receipt
    );
    let raw = Connection::open(root.path().join("semantic-authority.db")).unwrap();
    assert!(
        raw.execute(
            "UPDATE semantic_publication_receipts SET created_at_ms=0",
            [],
        )
        .is_err()
    );
    assert!(
        raw.execute("DELETE FROM semantic_publication_receipts", [])
            .is_err()
    );
}

fn verify_store_receipt(signer: &TestSigner, receipt: &AdmissionReceipt) {
    let core = admission_receipt_core_digest(receipt);
    let message = admission_receipt_signature_message(receipt.receipt_id, core);
    let signature = ed25519_dalek::Signature::from_bytes(&receipt.store_signature);
    signer
        .key
        .verifying_key()
        .verify(&message, &signature)
        .unwrap();
}

#[test]
fn identity_capability_and_process_failures_leave_no_partial_event() {
    let root = Root::new("authz");
    let fixture = fixture(&root, 50);
    let base = request(&fixture, 3, Vec::new(), Vec::new(), TaintFlags::default());

    let mut bad_signature = base.clone();
    bad_signature.signature[0] ^= 1;
    assert!(matches!(
        fixture.semantic.append_assertion(
            &fixture.identity,
            &fixture.capability,
            &fixture.process,
            &fixture.store_signer,
            &bad_signature,
        ),
        Err(SemanticAuthorityError::Identity(_))
    ));

    let wrong_store_signer = TestSigner {
        key: SigningKey::from_bytes(&[0xfe; 32]),
        binding: fixture.store_signer.binding,
    };
    assert!(matches!(
        fixture.semantic.append_assertion(
            &fixture.identity,
            &fixture.capability,
            &fixture.process,
            &wrong_store_signer,
            &base,
        ),
        Err(SemanticAuthorityError::Identity(_))
    ));

    let mut stale_process_event =
        decode_unsigned_assertion_event(&base.canonical_unsigned_event).unwrap();
    stale_process_event.issuer_execution.generation =
        Generation::new(std::num::NonZeroU64::new(2).unwrap());
    let stale_canonical = encode_unsigned_assertion_event(&stale_process_event).unwrap();
    let stale_id = semantic_event_id(&stale_canonical);
    let stale = AppendAssertionRequest {
        canonical_unsigned_event: stale_canonical,
        claimed_event_id: stale_id,
        signature: fixture
            .issuer_key
            .sign(&nlos_identity::semantic_signature_message(stale_id))
            .to_bytes(),
        ..base.clone()
    };
    assert!(matches!(
        fixture.semantic.append_assertion(
            &fixture.identity,
            &fixture.capability,
            &fixture.process,
            &fixture.store_signer,
            &stale,
        ),
        Err(SemanticAuthorityError::InvalidIssuerExecution)
    ));
    assert!(matches!(
        fixture.semantic.inspect_event(base.claimed_event_id),
        Err(SemanticAuthorityError::EventNotFound(_))
    ));
}

#[test]
fn lineage_requires_committed_ancestors_and_inherits_taint() {
    let root = Root::new("lineage");
    let fixture = fixture(&root, 70);
    let dangling_id = SemanticEventId::from_bytes([0xee; 32]);
    let dangling = request(
        &fixture,
        4,
        vec![dangling_id],
        Vec::new(),
        TaintFlags::default(),
    );
    assert!(matches!(
        fixture.semantic.append_assertion(
            &fixture.identity,
            &fixture.capability,
            &fixture.process,
            &fixture.store_signer,
            &dangling,
        ),
        Err(SemanticAuthorityError::DanglingLineage(id)) if id == dangling_id
    ));

    let parent = request(&fixture, 5, Vec::new(), Vec::new(), TaintFlags::PRIVATE);
    let parent_receipt = append(&fixture, &parent).receipt().clone();
    let child = request(
        &fixture,
        6,
        vec![parent.claimed_event_id],
        vec![parent.claimed_event_id],
        TaintFlags::UNTRUSTED_INGRESS,
    );
    let child_receipt = append(&fixture, &child).receipt().clone();
    assert_eq!(child_receipt.log_seq, parent_receipt.log_seq + 1);
    assert_eq!(
        child_receipt.effective_taint,
        TaintFlags::PRIVATE.union(TaintFlags::UNTRUSTED_INGRESS)
    );
    assert_eq!(child_receipt.captured_inputs, vec![parent.claimed_event_id]);
}

#[test]
fn committed_event_replays_after_capability_revoke_but_new_event_is_fenced() {
    let root = Root::new("revoked-replay");
    let fixture = fixture(&root, 90);
    let original = request(&fixture, 7, Vec::new(), Vec::new(), TaintFlags::default());
    let receipt = append(&fixture, &original).receipt().clone();
    fixture
        .capability
        .revoke(
            &fixture.identity,
            RevokeCapabilityRequest {
                handle: fixture.capability_record.handle,
                revoker_key_id: fixture.issuer.key_id,
                idempotency_key: IdempotencyKey::from_bytes([0xd0; 16]),
                revoked_at_ms: 3_000,
            },
        )
        .unwrap();
    assert_eq!(append(&fixture, &original).receipt(), &receipt);
    let mut next = request(&fixture, 8, Vec::new(), Vec::new(), TaintFlags::default());
    next.admitted_at_ms = 3_100;
    assert!(matches!(
        fixture.semantic.append_assertion(
            &fixture.identity,
            &fixture.capability,
            &fixture.process,
            &fixture.store_signer,
            &next,
        ),
        Err(SemanticAuthorityError::Capability(_))
    ));
}

#[test]
fn spec_event_canonical_body_digest_and_type_are_strict() {
    let root = Root::new("spec-canonical");
    let fixture = fixture(&root, 100);
    let request = spec_request(&fixture, 10, Vec::new());
    let event = decode_unsigned_spec_event(&request.canonical_unsigned_event).unwrap();
    assert_eq!(
        encode_unsigned_spec_event(&event).unwrap(),
        request.canonical_unsigned_event
    );

    let mut mismatch = event;
    mismatch.spec_body_digest[0] ^= 1;
    assert!(matches!(
        encode_unsigned_spec_event(&mismatch),
        Err(SemanticAuthorityError::SpecBodyDigestMismatch)
    ));
    assert!(matches!(
        decode_unsigned_assertion_event(&request.canonical_unsigned_event),
        Err(SemanticAuthorityError::UnsupportedEventType)
    ));
}

#[test]
fn spec_event_admission_is_atomic_durable_and_replayable_after_restart() {
    let root = Root::new("spec-admit");
    let (request, receipt) = {
        let fixture = fixture(&root, 120);
        let parent = request(&fixture, 11, Vec::new(), Vec::new(), TaintFlags::PRIVATE);
        append(&fixture, &parent);
        let request = spec_request(&fixture, 12, vec![parent.claimed_event_id]);
        let first = fixture
            .semantic
            .append_spec(
                &fixture.identity,
                &fixture.capability,
                &fixture.process,
                &fixture.store_signer,
                &request,
            )
            .unwrap();
        assert!(matches!(first, AppendDecision::Admitted(_)));
        assert_eq!(first.receipt().effective_taint, TaintFlags::PRIVATE);
        verify_store_receipt(&fixture.store_signer, first.receipt());
        (request, first.receipt().clone())
    };

    let fixture = fixture(&root, 120);
    let replay = fixture
        .semantic
        .append_spec(
            &fixture.identity,
            &fixture.capability,
            &fixture.process,
            &fixture.store_signer,
            &request,
        )
        .unwrap();
    assert!(matches!(replay, AppendDecision::Replayed(_)));
    assert_eq!(replay.receipt(), &receipt);
    let record = fixture
        .semantic
        .inspect_event(request.claimed_event_id)
        .unwrap();
    assert_eq!(
        record.payload_identity,
        SemanticPayloadIdentity::IntentSpecBody(
            decode_unsigned_spec_event(&request.canonical_unsigned_event)
                .unwrap()
                .spec_body_digest
        )
    );

    let raw = Connection::open(root.path().join("semantic-authority.db")).unwrap();
    assert_eq!(
        raw.query_row("SELECT COUNT(*) FROM spec_bodies", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(raw.execute("DELETE FROM spec_bodies", []).is_err());
}

#[test]
fn real_v1_store_migrates_without_losing_assertion_or_receipt() {
    let root = Root::new("v1-migration");
    let (event_id, receipt_id) = {
        let fixture = fixture(&root, 125);
        let request = request(&fixture, 13, Vec::new(), Vec::new(), TaintFlags::default());
        let receipt = append(&fixture, &request).receipt().clone();
        (request.claimed_event_id, receipt.receipt_id)
    };

    let raw = Connection::open(root.path().join("semantic-authority.db")).unwrap();
    raw.pragma_update(None, "foreign_keys", "OFF").unwrap();
    raw.pragma_update(None, "legacy_alter_table", "ON").unwrap();
    raw.execute_batch(
        "DROP TRIGGER semantic_events_immutable_update;
         DROP TRIGGER semantic_events_immutable_delete;
         DROP TRIGGER spec_bodies_immutable_update;
         DROP TRIGGER spec_bodies_immutable_delete;
         ALTER TABLE semantic_events RENAME TO semantic_events_v2;
         CREATE TABLE semantic_events (
            event_id BLOB PRIMARY KEY NOT NULL CHECK(length(event_id) = 32),
            canonical_unsigned_event BLOB NOT NULL CHECK(length(canonical_unsigned_event) <= 65536),
            event_type INTEGER NOT NULL CHECK(event_type = 1),
            scope_kind INTEGER NOT NULL CHECK(scope_kind IN (1, 2)),
            scope_id BLOB NOT NULL CHECK(length(scope_id) = 16),
            issuer_principal_id BLOB NOT NULL CHECK(length(issuer_principal_id) = 16),
            issuer_process_id BLOB NOT NULL CHECK(length(issuer_process_id) = 16),
            issuer_process_generation INTEGER NOT NULL CHECK(issuer_process_generation >= 1),
            control_domain_id BLOB NOT NULL CHECK(length(control_domain_id) = 16),
            issued_at_unix_ns INTEGER NOT NULL CHECK(issued_at_unix_ns >= 0),
            valid_until_ms INTEGER CHECK(valid_until_ms IS NULL OR valid_until_ms >= 0),
            purpose_digest BLOB CHECK(purpose_digest IS NULL OR length(purpose_digest) = 32),
            key_id BLOB NOT NULL CHECK(length(key_id) = 16),
            content_digest BLOB NOT NULL CHECK(length(content_digest) = 32),
            FOREIGN KEY(content_digest) REFERENCES content_objects(content_digest)
         ) STRICT;
         INSERT INTO semantic_events
         SELECT event_id, canonical_unsigned_event, event_type, scope_kind, scope_id,
                issuer_principal_id, issuer_process_id, issuer_process_generation,
                control_domain_id, issued_at_unix_ns, valid_until_ms, purpose_digest,
                key_id, content_digest
         FROM semantic_events_v2;
         DROP TABLE semantic_events_v2;
         DROP TABLE spec_bodies;
         CREATE TRIGGER semantic_events_immutable_update BEFORE UPDATE ON semantic_events
         BEGIN SELECT RAISE(ABORT, 'semantic event is immutable'); END;
         CREATE TRIGGER semantic_events_immutable_delete BEFORE DELETE ON semantic_events
         BEGIN SELECT RAISE(ABORT, 'semantic event is immutable'); END;
         PRAGMA user_version = 1;",
    )
    .unwrap();
    drop(raw);

    let migrated = SemanticAuthority::open(root.path()).unwrap();
    assert_eq!(migrated.inspect_event(event_id).unwrap().log_seq, 1);
    let raw = Connection::open(root.path().join("semantic-authority.db")).unwrap();
    assert_eq!(
        raw.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        4
    );
    assert_eq!(
        raw.query_row(
            "SELECT receipt_id FROM admission_receipts WHERE event_id=?1",
            [event_id.as_bytes().as_slice()],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .unwrap(),
        receipt_id.as_bytes()
    );
    assert!(
        raw.query_row("PRAGMA foreign_key_check", [], |row| row
            .get::<_, String>(0))
            .is_err()
    );
}

#[test]
fn semantic_admission_endpoint_proof_is_authority_assigned_durable_and_immutable() {
    let root = Root::new("endpoint-proof");
    let proof = {
        let authority = SemanticAuthority::open(root.path()).unwrap();
        let proof = authority.inspect_admission_endpoint_proof().unwrap();
        assert_eq!(
            proof.participant_generation,
            nlos_types::Generation::INITIAL
        );
        proof
    };
    let authority = SemanticAuthority::open(root.path()).unwrap();
    assert_eq!(authority.inspect_admission_endpoint_proof().unwrap(), proof);
    drop(authority);
    let raw = Connection::open(root.path().join("semantic-authority.db")).unwrap();
    assert!(
        raw.execute(
            "UPDATE semantic_admission_endpoint_proof SET participant_id=zeroblob(16)",
            [],
        )
        .is_err()
    );
    assert!(
        raw.execute("DELETE FROM semantic_admission_endpoint_proof", [])
            .is_err()
    );
}

#[test]
fn semantic_authority_tables_are_append_only_at_storage_layer() {
    let root = Root::new("immutable");
    let fixture = fixture(&root, 110);
    let request = request(&fixture, 9, Vec::new(), Vec::new(), TaintFlags::default());
    let receipt = append(&fixture, &request).receipt().clone();
    drop(fixture);

    let raw = Connection::open(root.path().join("semantic-authority.db")).unwrap();
    assert!(
        raw.execute("UPDATE semantic_events SET event_type=1", [])
            .is_err()
    );
    assert!(raw.execute("DELETE FROM admission_receipts", []).is_err());
    assert!(raw.execute("DELETE FROM event_signatures", []).is_err());
    assert_eq!(
        raw.query_row("SELECT COUNT(*) FROM semantic_outbox", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_ne!(receipt.receipt_id, ReceiptId::from_bytes([0; 16]));
}
