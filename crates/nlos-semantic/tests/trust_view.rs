#![allow(deprecated)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::Signer;
use nlos_capability::{
    CapabilityAuthority, CapabilityRights, CapabilityTarget, IssueRootCapabilityRequest,
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_process::{
    CreateIsolationDomainRequest, ProcessAuthority, RegisterDelegatedProcessRequest,
};
use nlos_semantic::{
    AppendAssertionRequest, AppendDecision, AppendTypedEventRequest, AssertionMode, EvaluatorKind,
    EventVerificationTarget, ImmutableEvaluatorReference, ImmutableEvaluatorReferenceKind,
    IssueDeclassificationDecision, IssueDeclassificationReceiptRequest, JudgmentRelation,
    LocalProcessRef, SemanticAuthority, SemanticAuthorityError, StoreSigner, StoreSignerError,
    TaintFlags, TrustViewJudgmentRole, TrustViewVerificationStatus, UnsignedAssertionEvent,
    UnsignedJudgmentEvent, UnsignedVerificationEvent, VerificationOutcome, VerificationTarget,
    content_digest, declassification_issue_authorization_id, encode_unsigned_assertion_event,
    encode_unsigned_judgment_event, encode_unsigned_verification_event, semantic_event_id,
};
use nlos_types::{
    Generation, IdempotencyKey, NamespaceId, PrincipalId, ReceiptId, SemanticEventId,
    TaskAttemptId, TaskId,
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
            "nlos-semantic-trust-view-{label}-{}-{nonce}-{}",
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
    key: ed25519_dalek::SigningKey,
    binding: nlos_identity::IdentityBinding,
}

impl StoreSigner for TestSigner {
    fn principal_id(&self) -> PrincipalId {
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
    issuer_key: ed25519_dalek::SigningKey,
    issuer: nlos_identity::IdentityBinding,
    process_binding: nlos_process::ProcessBindingRecord,
    adjudicate_capability: nlos_capability::CapabilityRecord,
    store_signer: TestSigner,
    scope: CapabilityTarget,
    purpose_digest: Option<[u8; 32]>,
}

fn bootstrap(
    identity: &IdentityAuthority,
    seed: u8,
) -> (ed25519_dalek::SigningKey, nlos_identity::IdentityBinding) {
    let key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
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
    let scope = CapabilityTarget::Namespace(NamespaceId::from_bytes([0x44; 16]));
    let purpose_digest = Some([0x77; 32]);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let adjudicate_capability = capability
        .issue_root(
            &identity,
            IssueRootCapabilityRequest {
                issuer_key_id: issuer.key_id,
                holder_key_id: issuer.key_id,
                target: scope,
                rights: CapabilityRights::SEMANTIC_ADJUDICATE,
                purpose_digest,
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
        adjudicate_capability,
        store_signer: TestSigner {
            key: store_key,
            binding: store_binding,
        },
        scope,
        purpose_digest,
    }
}

fn append_capability(fixture: &Fixture, seed: u8) -> nlos_capability::CapabilityRecord {
    fixture
        .capability
        .issue_root(
            &fixture.identity,
            IssueRootCapabilityRequest {
                issuer_key_id: fixture.issuer.key_id,
                holder_key_id: fixture.issuer.key_id,
                target: fixture.scope,
                rights: CapabilityRights::SEMANTIC_APPEND,
                purpose_digest: fixture.purpose_digest,
                valid_from_ms: 0,
                valid_until_ms: 9_000,
                delegation_depth_remaining: 0,
                call_limit: None,
                idempotency_key: IdempotencyKey::from_bytes([seed; 16]),
                issued_at_ms: 0,
            },
        )
        .unwrap()
        .record()
}

fn append_assertion(
    fixture: &Fixture,
    append_cap: &nlos_capability::CapabilityRecord,
    seed: u8,
    parents: Vec<SemanticEventId>,
    captured: Vec<SemanticEventId>,
    ingress_taint: TaintFlags,
    declassification_receipt_id: Option<ReceiptId>,
) -> SemanticEventId {
    let media_type = "text/plain".to_owned();
    let content_bytes = format!("assertion-{seed}").into_bytes();
    let event = UnsignedAssertionEvent {
        scope: fixture.scope,
        issuer: fixture.issuer.principal_id,
        issuer_execution: LocalProcessRef {
            process_id: fixture.process_binding.process_id,
            generation: fixture.process_binding.process_generation,
        },
        control_domain: fixture.issuer.control_domain_id,
        issued_at_unix_ns: 1_000_000_000 + u64::from(seed),
        nonce: vec![seed; 16],
        declared_parents: parents,
        declassification_receipt_id,
        valid_until_ms: Some(8_000),
        purpose_digest: fixture.purpose_digest,
        content_digest: content_digest(&media_type, &content_bytes).unwrap(),
        assertion_mode: AssertionMode::Inference,
        execution_evidence_receipt_id: None,
        confidence_bp: Some(8_000),
        key_id: fixture.issuer.key_id,
    };
    let canonical = encode_unsigned_assertion_event(&event).unwrap();
    let event_id = semantic_event_id(&canonical);
    let request = AppendAssertionRequest {
        canonical_unsigned_event: canonical,
        claimed_event_id: event_id,
        signature: fixture
            .issuer_key
            .sign(&nlos_identity::semantic_signature_message(event_id))
            .to_bytes(),
        capability: append_cap.handle,
        content_media_type: media_type,
        content_bytes,
        captured_inputs: captured,
        ingress_taint,
        authz_policy_digest: [0x99; 32],
        admission_limit_ms: Some(8_500),
        admitted_at_ms: 2_000,
    };
    match fixture.semantic.append_assertion(
        &fixture.identity,
        &fixture.capability,
        &fixture.process,
        &fixture.store_signer,
        &request,
    ) {
        Ok(AppendDecision::Admitted(_) | AppendDecision::Replayed(_)) => event_id,
        Err(error) => panic!("assertion admission failed: {error}"),
    }
}

fn issue_receipt(
    fixture: &Fixture,
    source_events: Vec<SemanticEventId>,
    removed_labels: TaintFlags,
    nonce: Vec<u8>,
) -> IssueDeclassificationDecision {
    let request = IssueDeclassificationReceiptRequest {
        holder: fixture.issuer.principal_id,
        scope: fixture.scope,
        source_events,
        removed_labels,
        purpose_digest: fixture.purpose_digest,
        expires_at_ms: 8_000,
        nonce,
        issued_at_ms: 1_500,
        capability: fixture.adjudicate_capability.handle,
        adjudicator_key_id: fixture.issuer.key_id,
        adjudicator_signature: [0_u8; 64],
    };
    let authorization_id = declassification_issue_authorization_id(&request);
    let request = IssueDeclassificationReceiptRequest {
        adjudicator_signature: fixture
            .issuer_key
            .sign(&nlos_identity::semantic_signature_message(authorization_id))
            .to_bytes(),
        ..request
    };
    fixture
        .semantic
        .issue_declassification_receipt(
            &fixture.identity,
            &fixture.capability,
            &fixture.store_signer,
            &request,
        )
        .unwrap()
}

fn append_judgment(
    fixture: &Fixture,
    append_cap: &nlos_capability::CapabilityRecord,
    seed: u8,
    relation: JudgmentRelation,
    source: SemanticEventId,
    target: SemanticEventId,
) -> SemanticEventId {
    let (source, target) = if relation.is_symmetric() && target.as_bytes() < source.as_bytes() {
        (target, source)
    } else {
        (source, target)
    };
    let event = UnsignedJudgmentEvent {
        scope: fixture.scope,
        issuer: fixture.issuer.principal_id,
        issuer_execution: LocalProcessRef {
            process_id: fixture.process_binding.process_id,
            generation: fixture.process_binding.process_generation,
        },
        control_domain: fixture.issuer.control_domain_id,
        issued_at_unix_ns: 3_000_000_000 + u64::from(seed),
        nonce: vec![seed.wrapping_add(100); 16],
        declared_parents: Vec::new(),
        valid_until_ms: None,
        purpose_digest: fixture.purpose_digest,
        key_id: fixture.issuer.key_id,
        relation,
        source,
        target,
        context_digest: Some([0xcc; 32]),
        evaluator_evidence_receipt_id: ReceiptId::from_bytes([seed.wrapping_add(0x40); 16]),
        confidence_bp: Some(9_000),
    };
    let canonical = encode_unsigned_judgment_event(&event).unwrap();
    let event_id = semantic_event_id(&canonical);
    let request = AppendTypedEventRequest {
        canonical_unsigned_event: canonical,
        claimed_event_id: event_id,
        signature: fixture
            .issuer_key
            .sign(&nlos_identity::semantic_signature_message(event_id))
            .to_bytes(),
        capability: append_cap.handle,
        captured_inputs: Vec::new(),
        ingress_taint: TaintFlags::default(),
        authz_policy_digest: [0x99; 32],
        admission_limit_ms: Some(8_500),
        admitted_at_ms: 2_500,
    };
    match fixture.semantic.append_judgment(
        &fixture.identity,
        &fixture.capability,
        &fixture.process,
        &fixture.store_signer,
        &request,
    ) {
        Ok(AppendDecision::Admitted(_) | AppendDecision::Replayed(_)) => event_id,
        Err(error) => panic!("judgment admission failed: {error}"),
    }
}

fn append_verification(
    fixture: &Fixture,
    append_cap: &nlos_capability::CapabilityRecord,
    seed: u8,
    target: SemanticEventId,
    outcome: VerificationOutcome,
) -> SemanticEventId {
    let event = UnsignedVerificationEvent {
        scope: fixture.scope,
        issuer: fixture.issuer.principal_id,
        issuer_execution: LocalProcessRef {
            process_id: fixture.process_binding.process_id,
            generation: fixture.process_binding.process_generation,
        },
        control_domain: fixture.issuer.control_domain_id,
        issued_at_unix_ns: 5_000_000_000 + u64::from(seed),
        nonce: vec![seed.wrapping_add(120); 16],
        declared_parents: Vec::new(),
        valid_until_ms: None,
        purpose_digest: fixture.purpose_digest,
        key_id: fixture.issuer.key_id,
        target: VerificationTarget::Event(EventVerificationTarget { event_id: target }),
        outcome,
        evaluator_kind: EvaluatorKind::DeterministicTool,
        procedure_ref: ImmutableEvaluatorReference {
            kind: ImmutableEvaluatorReferenceKind::Artifact,
            digest: [0xda; 32],
        },
        evaluator_evidence_receipt_id: ReceiptId::from_bytes([seed.wrapping_add(0x60); 16]),
        evidence: Vec::new(),
    };
    let canonical = encode_unsigned_verification_event(&event).unwrap();
    let event_id = semantic_event_id(&canonical);
    let request = AppendTypedEventRequest {
        canonical_unsigned_event: canonical,
        claimed_event_id: event_id,
        signature: fixture
            .issuer_key
            .sign(&nlos_identity::semantic_signature_message(event_id))
            .to_bytes(),
        capability: append_cap.handle,
        captured_inputs: Vec::new(),
        ingress_taint: TaintFlags::default(),
        authz_policy_digest: [0x99; 32],
        admission_limit_ms: Some(8_500),
        admitted_at_ms: 2_600,
    };
    match fixture.semantic.append_verification(
        &fixture.identity,
        &fixture.capability,
        &fixture.process,
        &fixture.store_signer,
        &request,
    ) {
        Ok(AppendDecision::Admitted(_) | AppendDecision::Replayed(_)) => event_id,
        Err(error) => panic!("verification admission failed: {error}"),
    }
}

#[test]
fn trust_view_inherits_union_taint_without_declassification() {
    let root = Root::new("union-taint");
    let fixture = fixture(&root, 10);
    let append_cap = append_capability(&fixture, 0xa1);
    let parent = append_assertion(
        &fixture,
        &append_cap,
        1,
        Vec::new(),
        Vec::new(),
        TaintFlags::PRIVATE,
        None,
    );
    let child = append_assertion(
        &fixture,
        &append_cap,
        2,
        vec![parent],
        vec![parent],
        TaintFlags::UNTRUSTED_INGRESS,
        None,
    );
    let view = fixture.semantic.inspect_trust_view(child).unwrap();
    assert_eq!(
        view.effective_taint,
        TaintFlags::PRIVATE.union(TaintFlags::UNTRUSTED_INGRESS)
    );
    assert!(view.declassification_receipt_id.is_none());
    assert_eq!(
        view.verification_status,
        TrustViewVerificationStatus::Unverified
    );
    assert!(view.judgment_facts.is_empty());
    assert!(view.retraction.is_none());
}

#[test]
fn trust_view_reflects_declassification_and_judgment_facts() {
    let root = Root::new("declass-judge");
    let fixture = fixture(&root, 20);
    let append_cap = append_capability(&fixture, 0xb1);
    let left = append_assertion(
        &fixture,
        &append_cap,
        1,
        Vec::new(),
        Vec::new(),
        TaintFlags::PRIVATE.union(TaintFlags::UNTRUSTED_INGRESS),
        None,
    );
    let declass = issue_receipt(&fixture, vec![left], TaintFlags::PRIVATE, vec![0xde; 16]);
    let right = append_assertion(
        &fixture,
        &append_cap,
        2,
        vec![left],
        vec![left],
        TaintFlags::default(),
        Some(declass.receipt().receipt_id),
    );
    append_judgment(
        &fixture,
        &append_cap,
        3,
        JudgmentRelation::Entails,
        left,
        right,
    );

    let child_view = fixture.semantic.inspect_trust_view(right).unwrap();
    assert_eq!(child_view.effective_taint, TaintFlags::UNTRUSTED_INGRESS);
    assert_eq!(
        child_view.declassification_receipt_id,
        Some(declass.receipt().receipt_id)
    );
    assert_eq!(
        child_view.verification_status,
        TrustViewVerificationStatus::Unverified
    );

    let left_view = fixture.semantic.inspect_trust_view(left).unwrap();
    assert_eq!(
        left_view.effective_taint,
        TaintFlags::PRIVATE.union(TaintFlags::UNTRUSTED_INGRESS)
    );
    assert_eq!(left_view.judgment_facts.len(), 1);
    assert_eq!(
        left_view.judgment_facts[0].role,
        TrustViewJudgmentRole::Source
    );
    assert_eq!(
        left_view.judgment_facts[0].relation,
        JudgmentRelation::Entails
    );
    assert_eq!(left_view.judgment_facts[0].counterpart_event_id, right);

    let right_view = fixture.semantic.inspect_trust_view(right).unwrap();
    assert_eq!(right_view.judgment_facts.len(), 1);
    assert_eq!(
        right_view.judgment_facts[0].role,
        TrustViewJudgmentRole::Target
    );
}

#[test]
fn trust_view_collects_verification_status_and_rejects_unknown_event() {
    let root = Root::new("verify");
    let fixture = fixture(&root, 30);
    let append_cap = append_capability(&fixture, 0xc1);
    let target = append_assertion(
        &fixture,
        &append_cap,
        1,
        Vec::new(),
        Vec::new(),
        TaintFlags::PRIVATE,
        None,
    );
    append_verification(&fixture, &append_cap, 2, target, VerificationOutcome::Pass);

    let view = fixture.semantic.inspect_trust_view(target).unwrap();
    assert_eq!(view.verification_status, TrustViewVerificationStatus::Pass);
    assert_eq!(view.verification_facts.len(), 1);
    assert_eq!(
        view.verification_facts[0].outcome,
        VerificationOutcome::Pass
    );

    let missing = SemanticEventId::from_bytes([0xee; 32]);
    assert!(matches!(
        fixture.semantic.inspect_trust_view(missing),
        Err(SemanticAuthorityError::EventNotFound(_))
    ));
}

#[test]
fn trust_view_replay_is_stable_after_restart() {
    let root = Root::new("restart");
    let fixture = fixture(&root, 40);
    let append_cap = append_capability(&fixture, 0xd1);
    let event_id = append_assertion(
        &fixture,
        &append_cap,
        1,
        Vec::new(),
        Vec::new(),
        TaintFlags::PROVENANCE_INCOMPLETE,
        None,
    );
    let before = fixture.semantic.inspect_trust_view(event_id).unwrap();

    let reopened = SemanticAuthority::open(root.path()).unwrap();
    let after = reopened.inspect_trust_view(event_id).unwrap();
    assert_eq!(before, after);
}
