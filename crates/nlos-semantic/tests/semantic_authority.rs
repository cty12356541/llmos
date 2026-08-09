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
    AdmissionReceipt, AppendAssertionRequest, AppendDecision, AssertionMode, LocalProcessRef,
    SemanticAuthority, SemanticAuthorityError, StoreSigner, StoreSignerError, TaintFlags,
    UnsignedAssertionEvent, admission_receipt_core_digest, admission_receipt_signature_message,
    content_digest, decode_unsigned_assertion_event, encode_unsigned_assertion_event,
    semantic_event_id,
};
use nlos_types::{
    Generation, IdempotencyKey, NamespaceId, ReceiptId, SemanticEventId, TaskAttemptId, TaskId,
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
