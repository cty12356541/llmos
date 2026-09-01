//! B-SEMANTIC-006: admission of the §17.2/§17.3/§17.4 typed events
//! (Judgment/Verification/Retraction): durable rows, idempotent replay,
//! domain-separated signatures, and the append-only retraction ledger.
#![allow(deprecated)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_capability::{
    CapabilityAuthority, CapabilityHandle, CapabilityRights, CapabilityTarget,
    IssueRootCapabilityRequest,
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_process::{
    CreateIsolationDomainRequest, ProcessAuthority, RegisterDelegatedProcessRequest,
};
use nlos_semantic::{
    AppendAssertionRequest, AppendDecision, AppendSpecRequest, AppendTypedEventRequest,
    AssertionMode, CriterionAggregation, CriterionEffect, CriterionVerificationTarget,
    EvaluatorKind, EventVerificationTarget, ImmutableEvaluatorReference,
    ImmutableEvaluatorReferenceKind, IntentConstraints, IntentCriterion, IntentCriticality,
    IntentSettlement, JudgmentRelation, LocalProcessRef, RetractionMode, RetractionRecord,
    SemanticAuthority, SemanticAuthorityError, SemanticPayloadIdentity, SettlementMode,
    SettlementTimeoutAction, StoreSigner, StoreSignerError, TaintFlags, UnsignedAssertionEvent,
    UnsignedJudgmentEvent, UnsignedRetractionEvent, UnsignedSpecEvent, UnsignedVerificationEvent,
    VerificationOutcome, VerificationTarget, content_digest, criterion_id,
    decode_unsigned_judgment_event, decode_unsigned_retraction_event, encode_intent_spec_body,
    encode_unsigned_assertion_event, encode_unsigned_judgment_event,
    encode_unsigned_retraction_event, encode_unsigned_spec_event,
    encode_unsigned_verification_event, hard_criteria_digest, intent_spec_body_digest,
    semantic_event_id,
};
use nlos_types::{
    Generation, IdempotencyKey, KeyId, NamespaceId, PrincipalId, ReceiptId, SemanticEventId,
    TaskAttemptId, TaskId,
};

static NEXT: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EventLane {
    Judgment,
    Verification,
    Retraction,
}

struct Root(PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-semantic-typed-{label}-{}-{nonce}-{}",
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

struct Issuer {
    key: SigningKey,
    binding: nlos_identity::IdentityBinding,
    process: nlos_process::ProcessBindingRecord,
    capability: CapabilityHandle,
}

impl Issuer {
    fn execution(&self) -> LocalProcessRef {
        LocalProcessRef {
            process_id: self.process.process_id,
            generation: self.process.process_generation,
        }
    }

    fn signed_request(&self, canonical: Vec<u8>) -> AppendTypedEventRequest {
        let event_id = semantic_event_id(&canonical);
        AppendTypedEventRequest {
            canonical_unsigned_event: canonical,
            claimed_event_id: event_id,
            signature: self
                .key
                .sign(&nlos_identity::semantic_signature_message(event_id))
                .to_bytes(),
            capability: self.capability,
            captured_inputs: Vec::new(),
            ingress_taint: TaintFlags::default(),
            authz_policy_digest: [0x33; 32],
            admission_limit_ms: Some(8_500),
            admitted_at_ms: 2_000,
        }
    }
}

struct Fixture {
    identity: IdentityAuthority,
    capability: CapabilityAuthority,
    process: ProcessAuthority,
    semantic: SemanticAuthority,
    domain: nlos_process::IsolationDomainRecord,
    issuer: Issuer,
    capability_target: CapabilityTarget,
    purpose_digest: Option<[u8; 32]>,
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
    let (issuer_key, issuer_binding) = bootstrap(&identity, seed);
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
    let issuer_process = process
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
    let capability_target = CapabilityTarget::Namespace(NamespaceId::from_bytes([0x44; 16]));
    let purpose_digest = Some([0x77; 32]);
    let capability_record = capability
        .issue_root(
            &identity,
            IssueRootCapabilityRequest {
                issuer_key_id: issuer_binding.key_id,
                holder_key_id: issuer_binding.key_id,
                target: capability_target,
                rights: CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::SEMANTIC_RETRACT),
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
        issuer: Issuer {
            binding: issuer_binding,
            capability: capability_record.handle,
            key: issuer_key,
            process: issuer_process,
        },
        capability_target,
        domain,
        identity,
        capability,
        process,
        purpose_digest,
        semantic,
        store_signer: TestSigner {
            binding: store_binding,
            key: store_key,
        },
    }
}

fn secondary_issuer(fixture: &Fixture, seed: u8, rights: CapabilityRights) -> Issuer {
    let (key, binding) = bootstrap(&fixture.identity, seed);
    let process = fixture
        .process
        .register_delegated_process(RegisterDelegatedProcessRequest {
            task_id: TaskId::from_bytes([seed.wrapping_add(6); 16]),
            task_attempt_id: TaskAttemptId::from_bytes([seed.wrapping_add(7); 16]),
            attempt_generation: Generation::INITIAL,
            isolation_domain_id: fixture.domain.isolation_domain_id,
            isolation_domain_generation: fixture.domain.generation,
            isolation_domain_fencing_token: fixture.domain.fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(30); 16]),
            created_at_ms: 0,
        })
        .unwrap()
        .record()
        .clone();
    let record = fixture
        .capability
        .issue_root(
            &fixture.identity,
            IssueRootCapabilityRequest {
                issuer_key_id: fixture.issuer.binding.key_id,
                holder_key_id: binding.key_id,
                target: fixture.capability_target,
                rights,
                purpose_digest: fixture.purpose_digest,
                valid_from_ms: 0,
                valid_until_ms: 9_000,
                delegation_depth_remaining: 0,
                call_limit: None,
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(31); 16]),
                issued_at_ms: 0,
            },
        )
        .unwrap()
        .record();
    Issuer {
        binding,
        capability: record.handle,
        key,
        process,
    }
}

fn append_assertion(fixture: &Fixture, seed: u8) -> SemanticEventId {
    let media_type = "text/plain".to_owned();
    let content_bytes = format!("assertion-{seed}").into_bytes();
    let event = UnsignedAssertionEvent {
        scope: fixture.capability_target,
        issuer: fixture.issuer.binding.principal_id,
        issuer_execution: fixture.issuer.execution(),
        control_domain: fixture.issuer.binding.control_domain_id,
        issued_at_unix_ns: 1_000_000_000 + u64::from(seed),
        nonce: vec![seed; 16],
        declared_parents: Vec::new(),
        valid_until_ms: Some(8_000),
        purpose_digest: fixture.purpose_digest,
        content_digest: content_digest(&media_type, &content_bytes).unwrap(),
        assertion_mode: AssertionMode::Inference,
        execution_evidence_receipt_id: None,
        confidence_bp: Some(8_000),
        key_id: fixture.issuer.binding.key_id,
    };
    let canonical = encode_unsigned_assertion_event(&event).unwrap();
    let event_id = semantic_event_id(&canonical);
    let request = AppendAssertionRequest {
        canonical_unsigned_event: canonical,
        claimed_event_id: event_id,
        signature: fixture
            .issuer
            .key
            .sign(&nlos_identity::semantic_signature_message(event_id))
            .to_bytes(),
        capability: fixture.issuer.capability,
        content_media_type: media_type,
        content_bytes,
        captured_inputs: Vec::new(),
        ingress_taint: TaintFlags::default(),
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
        Err(error) => panic!("assertion seed {seed} admission failed: {error}"),
    }
}

fn judgment_request(
    fixture: &Fixture,
    seed: u8,
    relation: JudgmentRelation,
    source: SemanticEventId,
    target: SemanticEventId,
) -> AppendTypedEventRequest {
    let (source, target) = if relation.is_symmetric() && target.as_bytes() < source.as_bytes() {
        (target, source)
    } else {
        (source, target)
    };
    let event = UnsignedJudgmentEvent {
        scope: fixture.capability_target,
        issuer: fixture.issuer.binding.principal_id,
        issuer_execution: fixture.issuer.execution(),
        control_domain: fixture.issuer.binding.control_domain_id,
        issued_at_unix_ns: 3_000_000_000 + u64::from(seed),
        nonce: vec![seed.wrapping_add(100); 16],
        declared_parents: Vec::new(),
        valid_until_ms: None,
        purpose_digest: fixture.purpose_digest,
        key_id: fixture.issuer.binding.key_id,
        relation,
        source,
        target,
        context_digest: Some([0xcc; 32]),
        evaluator_evidence_receipt_id: ReceiptId::from_bytes([seed.wrapping_add(0x40); 16]),
        confidence_bp: Some(9_000),
    };
    fixture
        .issuer
        .signed_request(encode_unsigned_judgment_event(&event).unwrap())
}

fn spec_event(fixture: &Fixture) -> (SemanticEventId, Vec<[u8; 32]>) {
    let criterion = IntentCriterion {
        description_digest: [0xd1; 32],
        effect: CriterionEffect::Hard,
        evaluator_kind: EvaluatorKind::DeterministicTool,
        evaluator_ref: ImmutableEvaluatorReference {
            kind: ImmutableEvaluatorReferenceKind::Artifact,
            digest: [0xd2; 32],
        },
        target_selector_digest: [0xd3; 32],
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
    let mut body = nlos_semantic::IntentSpecBody {
        goal_digest: [0xd0; 32],
        acceptance: vec![criterion],
        constraints: IntentConstraints {
            resource_vector_digest: [0xd4; 32],
            deadline_ms: Some(8_000),
            namespace_root: NamespaceId::from_bytes([0x44; 16]),
            allowed_capability_digests: vec![[0xd5; 32]],
            forbidden_capability_digests: vec![[0xd6; 32]],
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
    let criterion_ids = body
        .acceptance
        .iter()
        .map(|c| criterion_id(c).unwrap())
        .collect::<Vec<_>>();
    let event = UnsignedSpecEvent {
        scope: fixture.capability_target,
        issuer: fixture.issuer.binding.principal_id,
        issuer_execution: fixture.issuer.execution(),
        control_domain: fixture.issuer.binding.control_domain_id,
        issued_at_unix_ns: 4_000_000_000,
        nonce: vec![0x55; 16],
        declared_parents: Vec::new(),
        valid_until_ms: Some(8_000),
        purpose_digest: fixture.purpose_digest,
        spec_body_digest: intent_spec_body_digest(&body).unwrap(),
        canonical_spec_body: encode_intent_spec_body(&body).unwrap(),
        key_id: fixture.issuer.binding.key_id,
    };
    let canonical = encode_unsigned_spec_event(&event).unwrap();
    let spec_id = semantic_event_id(&canonical);
    let request = AppendSpecRequest {
        canonical_unsigned_event: canonical,
        claimed_event_id: spec_id,
        signature: fixture
            .issuer
            .key
            .sign(&nlos_identity::semantic_signature_message(spec_id))
            .to_bytes(),
        capability: fixture.issuer.capability,
        captured_inputs: Vec::new(),
        ingress_taint: TaintFlags::default(),
        authz_policy_digest: [0xb0; 32],
        admission_limit_ms: Some(8_500),
        admitted_at_ms: 2_000,
    };
    match fixture.semantic.append_spec(
        &fixture.identity,
        &fixture.capability,
        &fixture.process,
        &fixture.store_signer,
        &request,
    ) {
        Ok(AppendDecision::Admitted(_) | AppendDecision::Replayed(_)) => (spec_id, criterion_ids),
        Err(error) => panic!("spec admission failed: {error}"),
    }
}

fn criterion_target(
    spec_id: SemanticEventId,
    criterion_id: [u8; 32],
    domain: nlos_types::ControlDomainId,
) -> VerificationTarget {
    VerificationTarget::Criterion(CriterionVerificationTarget {
        spec_id,
        criterion_id,
        artifact_set_digest: [0xd7; 32],
        procedure_digest: [0xd8; 32],
        evaluation_id: [0xd9; 32],
        producer_control_domains: vec![domain],
    })
}

fn verification_request(
    fixture: &Fixture,
    seed: u8,
    target: VerificationTarget,
    outcome: VerificationOutcome,
) -> AppendTypedEventRequest {
    let event = UnsignedVerificationEvent {
        scope: fixture.capability_target,
        issuer: fixture.issuer.binding.principal_id,
        issuer_execution: fixture.issuer.execution(),
        control_domain: fixture.issuer.binding.control_domain_id,
        issued_at_unix_ns: 5_000_000_000 + u64::from(seed),
        nonce: vec![seed.wrapping_add(120); 16],
        declared_parents: Vec::new(),
        valid_until_ms: None,
        purpose_digest: fixture.purpose_digest,
        key_id: fixture.issuer.binding.key_id,
        target,
        outcome,
        evaluator_kind: EvaluatorKind::DeterministicTool,
        procedure_ref: ImmutableEvaluatorReference {
            kind: ImmutableEvaluatorReferenceKind::Artifact,
            digest: [0xda; 32],
        },
        evaluator_evidence_receipt_id: ReceiptId::from_bytes([seed.wrapping_add(0x60); 16]),
        evidence: Vec::new(),
    };
    fixture
        .issuer
        .signed_request(encode_unsigned_verification_event(&event).unwrap())
}

fn retraction_request(
    fixture: &Fixture,
    seed: u8,
    issuer: &Issuer,
    target_event_id: SemanticEventId,
    mode: RetractionMode,
) -> AppendTypedEventRequest {
    let event = UnsignedRetractionEvent {
        scope: fixture.capability_target,
        issuer: issuer.binding.principal_id,
        issuer_execution: issuer.execution(),
        control_domain: issuer.binding.control_domain_id,
        issued_at_unix_ns: 6_000_000_000 + u64::from(seed),
        nonce: vec![seed.wrapping_add(140); 16],
        declared_parents: Vec::new(),
        valid_until_ms: None,
        purpose_digest: fixture.purpose_digest,
        key_id: issuer.binding.key_id,
        target_event_id,
        mode,
        reason_digest: Some([0xdb; 32]),
        authority_evidence_receipt_id: ReceiptId::from_bytes([seed.wrapping_add(0x70); 16]),
    };
    issuer.signed_request(encode_unsigned_retraction_event(&event).unwrap())
}

fn append(
    fixture: &Fixture,
    request: &AppendTypedEventRequest,
    lane: EventLane,
) -> Result<AppendDecision, SemanticAuthorityError> {
    match lane {
        EventLane::Judgment => fixture.semantic.append_judgment(
            &fixture.identity,
            &fixture.capability,
            &fixture.process,
            &fixture.store_signer,
            request,
        ),
        EventLane::Verification => fixture.semantic.append_verification(
            &fixture.identity,
            &fixture.capability,
            &fixture.process,
            &fixture.store_signer,
            request,
        ),
        EventLane::Retraction => fixture.semantic.append_retraction(
            &fixture.identity,
            &fixture.capability,
            &fixture.process,
            &fixture.store_signer,
            request,
        ),
    }
}

#[test]
fn judgment_admits_replays_across_restart_and_rejects_tamper_and_unknown_principal() {
    let root = Root::new("judgment");
    let fixture = fixture(&root, 1);
    let left = append_assertion(&fixture, 1);
    let right = append_assertion(&fixture, 2);

    let request = judgment_request(&fixture, 1, JudgmentRelation::Entails, left, right);
    let decision = append(&fixture, &request, EventLane::Judgment).unwrap();
    let receipt = match &decision {
        AppendDecision::Admitted(receipt) => receipt.clone(),
        other @ AppendDecision::Replayed(_) => panic!("expected admitted, got {other:?}"),
    };
    let record = fixture.semantic.inspect_event(receipt.event_id).unwrap();
    assert!(matches!(
        record.payload_identity,
        SemanticPayloadIdentity::Structural
    ));

    let reopened = SemanticAuthority::open(root.path()).unwrap();
    let replayed = reopened
        .append_judgment(
            &fixture.identity,
            &fixture.capability,
            &fixture.process,
            &fixture.store_signer,
            &request,
        )
        .unwrap();
    assert!(matches!(replayed, AppendDecision::Replayed(_)));
    assert_eq!(replayed.receipt().receipt_id, receipt.receipt_id);

    let mut tampered = judgment_request(&fixture, 2, JudgmentRelation::Entails, left, right);
    tampered.signature[0] ^= 0x01;
    assert!(matches!(
        append(&fixture, &tampered, EventLane::Judgment),
        Err(SemanticAuthorityError::Identity(_))
    ));

    let stranger_key = SigningKey::from_bytes(&[0xEE; 32]);
    let event = UnsignedJudgmentEvent {
        scope: fixture.capability_target,
        issuer: PrincipalId::from_bytes([0xEF; 16]),
        issuer_execution: fixture.issuer.execution(),
        control_domain: fixture.issuer.binding.control_domain_id,
        issued_at_unix_ns: 3_100_000_000,
        nonce: vec![0x77; 16],
        declared_parents: Vec::new(),
        valid_until_ms: None,
        purpose_digest: fixture.purpose_digest,
        key_id: KeyId::from_bytes([0xE1; 16]),
        relation: JudgmentRelation::Entails,
        source: left,
        target: right,
        context_digest: None,
        evaluator_evidence_receipt_id: ReceiptId::from_bytes([0xE2; 16]),
        confidence_bp: None,
    };
    let canonical = encode_unsigned_judgment_event(&event).unwrap();
    let claimed_event_id = semantic_event_id(&canonical);
    let stranger = AppendTypedEventRequest {
        canonical_unsigned_event: canonical,
        claimed_event_id,
        signature: stranger_key
            .sign(&nlos_identity::semantic_signature_message(claimed_event_id))
            .to_bytes(),
        capability: fixture.issuer.capability,
        captured_inputs: Vec::new(),
        ingress_taint: TaintFlags::default(),
        authz_policy_digest: [0x33; 32],
        admission_limit_ms: Some(8_500),
        admitted_at_ms: 2_000,
    };
    assert!(matches!(
        append(&fixture, &stranger, EventLane::Judgment),
        Err(SemanticAuthorityError::Identity(_))
    ));
}

#[test]
fn verification_admits_event_and_criterion_targets_and_rejects_bad_targets() {
    let root = Root::new("verification");
    let fixture = fixture(&root, 2);
    let observed = append_assertion(&fixture, 3);

    let event_target = VerificationTarget::Event(EventVerificationTarget { event_id: observed });
    let request = verification_request(&fixture, 4, event_target, VerificationOutcome::Pass);
    let decision = append(&fixture, &request, EventLane::Verification).unwrap();
    assert!(matches!(decision, AppendDecision::Admitted(_)));

    let replay = append(&fixture, &request, EventLane::Verification).unwrap();
    assert!(matches!(replay, AppendDecision::Replayed(_)));
    assert_eq!(replay.receipt().receipt_id, decision.receipt().receipt_id);

    let (spec_id, criterion_ids) = spec_event(&fixture);
    let target = criterion_target(
        spec_id,
        criterion_ids[0],
        fixture.issuer.binding.control_domain_id,
    );
    let request = verification_request(&fixture, 5, target, VerificationOutcome::Fail);
    match append(&fixture, &request, EventLane::Verification) {
        Ok(AppendDecision::Admitted(_)) => {}
        other => panic!("criterion verification expected admitted, got {other:?}"),
    }

    let unknown_spec = criterion_target(
        SemanticEventId::from_bytes([0x5A; 32]),
        [0x5B; 32],
        fixture.issuer.binding.control_domain_id,
    );
    let request = verification_request(&fixture, 6, unknown_spec, VerificationOutcome::Pass);
    assert!(matches!(
        append(&fixture, &request, EventLane::Verification),
        Err(SemanticAuthorityError::EventNotFound(_))
    ));

    let not_a_spec = criterion_target(
        observed,
        [0x5F; 32],
        fixture.issuer.binding.control_domain_id,
    );
    let request = verification_request(&fixture, 7, not_a_spec, VerificationOutcome::Pass);
    assert!(matches!(
        append(&fixture, &request, EventLane::Verification),
        Err(SemanticAuthorityError::InvalidVerificationTarget(_))
    ));

    let wrong_criterion = criterion_target(
        spec_id,
        [0x64; 32],
        fixture.issuer.binding.control_domain_id,
    );
    let request = verification_request(&fixture, 8, wrong_criterion, VerificationOutcome::Pass);
    assert!(matches!(
        append(&fixture, &request, EventLane::Verification),
        Err(SemanticAuthorityError::InvalidVerificationTarget(_))
    ));
}

#[test]
#[allow(clippy::too_many_lines)] // One matrix covers the full §17.4 disposition.
fn retraction_paths_cover_issuer_withdraw_and_all_negative_gates() {
    let root = Root::new("retraction");
    let fixture = fixture(&root, 3);
    let target = append_assertion(&fixture, 9);

    let stranger = secondary_issuer(&fixture, 50, CapabilityRights::SEMANTIC_RETRACT);
    let adjudicator = secondary_issuer(&fixture, 60, CapabilityRights::SEMANTIC_ADJUDICATE);
    let plain_issuer = secondary_issuer(&fixture, 70, CapabilityRights::SEMANTIC_APPEND);

    let missing = retraction_request(
        &fixture,
        10,
        &fixture.issuer,
        SemanticEventId::from_bytes([0x91; 32]),
        RetractionMode::Withdraw,
    );
    assert!(matches!(
        append(&fixture, &missing, EventLane::Retraction),
        Err(SemanticAuthorityError::EventNotFound(_))
    ));

    let third_party = retraction_request(&fixture, 11, &stranger, target, RetractionMode::Withdraw);
    assert!(matches!(
        append(&fixture, &third_party, EventLane::Retraction),
        Err(SemanticAuthorityError::RetractionSignerUnauthorized)
    ));

    let no_right = retraction_request(
        &fixture,
        12,
        &plain_issuer,
        target,
        RetractionMode::Invalidate,
    );
    assert!(matches!(
        append(&fixture, &no_right, EventLane::Retraction),
        Err(SemanticAuthorityError::Capability(_))
    ));

    let ttl_event = UnsignedRetractionEvent {
        scope: fixture.capability_target,
        issuer: fixture.issuer.binding.principal_id,
        issuer_execution: fixture.issuer.execution(),
        control_domain: fixture.issuer.binding.control_domain_id,
        issued_at_unix_ns: 6_200_000_000,
        nonce: vec![0x21; 16],
        declared_parents: Vec::new(),
        valid_until_ms: Some(8_000),
        purpose_digest: fixture.purpose_digest,
        key_id: fixture.issuer.binding.key_id,
        target_event_id: target,
        mode: RetractionMode::Withdraw,
        reason_digest: None,
        authority_evidence_receipt_id: ReceiptId::from_bytes([0x22; 16]),
    };
    assert!(matches!(
        encode_unsigned_retraction_event(&ttl_event),
        Err(SemanticAuthorityError::InvalidRetractionPayload(_))
    ));

    let request = retraction_request(
        &fixture,
        13,
        &fixture.issuer,
        target,
        RetractionMode::Withdraw,
    );
    let decision = append(&fixture, &request, EventLane::Retraction).unwrap();
    let receipt = match &decision {
        AppendDecision::Admitted(receipt) => receipt.clone(),
        other @ AppendDecision::Replayed(_) => panic!("expected admitted withdraw, got {other:?}"),
    };
    let retraction: RetractionRecord = fixture
        .semantic
        .inspect_event_retraction(target)
        .unwrap()
        .expect("durable retraction row");
    assert_eq!(retraction.retraction_event_id, receipt.event_id);
    assert_eq!(retraction.mode, RetractionMode::Withdraw);
    assert_eq!(retraction.retracted_by, fixture.issuer.binding.principal_id);
    assert!(fixture.semantic.inspect_event(target).is_ok());

    let again = retraction_request(
        &fixture,
        14,
        &fixture.issuer,
        target,
        RetractionMode::Withdraw,
    );
    assert!(matches!(
        append(&fixture, &again, EventLane::Retraction),
        Err(SemanticAuthorityError::EventAlreadyRetracted(_))
    ));

    let invalidate = retraction_request(
        &fixture,
        15,
        &adjudicator,
        target,
        RetractionMode::Invalidate,
    );
    assert!(matches!(
        append(&fixture, &invalidate, EventLane::Retraction),
        Err(SemanticAuthorityError::EventAlreadyRetracted(_))
    ));

    let reopened = SemanticAuthority::open(root.path()).unwrap();
    let replay = reopened
        .append_retraction(
            &fixture.identity,
            &fixture.capability,
            &fixture.process,
            &fixture.store_signer,
            &request,
        )
        .unwrap();
    assert!(matches!(replay, AppendDecision::Replayed(_)));

    let adjudicable = append_assertion(&fixture, 16);
    let invalidate = retraction_request(
        &fixture,
        17,
        &adjudicator,
        adjudicable,
        RetractionMode::Invalidate,
    );
    assert!(matches!(
        append(&fixture, &invalidate, EventLane::Retraction),
        Ok(AppendDecision::Admitted(_))
    ));
    let record = fixture
        .semantic
        .inspect_event_retraction(adjudicable)
        .unwrap()
        .unwrap();
    assert_eq!(record.mode, RetractionMode::Invalidate);
    assert_eq!(record.retracted_by, adjudicator.binding.principal_id);
}

#[test]
fn typed_canonical_bytes_round_trip_and_enforce_judgment_normalization() {
    let root = Root::new("canonical");
    let fixture = fixture(&root, 4);
    let left = append_assertion(&fixture, 21);
    let right = append_assertion(&fixture, 22);

    let (lo, hi) = if left.as_bytes() <= right.as_bytes() {
        (left, right)
    } else {
        (right, left)
    };
    let normalized = judgment_request(&fixture, 23, JudgmentRelation::Equivalent, lo, hi);
    let canonical = normalized.canonical_unsigned_event.clone();
    let decoded = decode_unsigned_judgment_event(&canonical).unwrap();
    assert_eq!(decoded.relation, JudgmentRelation::Equivalent);
    assert_eq!(decoded.source, lo);
    assert_eq!(decoded.target, hi);
    assert_eq!(encode_unsigned_judgment_event(&decoded).unwrap(), canonical);

    let mut reversed = decoded;
    reversed.source = hi;
    reversed.target = lo;
    match encode_unsigned_judgment_event(&reversed) {
        Err(SemanticAuthorityError::InvalidJudgmentPayload(_)) => {}
        other => panic!("reversed symmetric judgment expected rejection, got {other:?}"),
    }

    let directed = judgment_request(&fixture, 24, JudgmentRelation::Refines, hi, lo);
    let decoded = decode_unsigned_judgment_event(&directed.canonical_unsigned_event).unwrap();
    assert_eq!(decoded.source, hi);
    assert_eq!(decoded.target, lo);

    let withdrawal = retraction_request(
        &fixture,
        25,
        &fixture.issuer,
        left,
        RetractionMode::Withdraw,
    );
    let decoded = decode_unsigned_retraction_event(&withdrawal.canonical_unsigned_event).unwrap();
    assert_eq!(decoded.target_event_id, left);
}
