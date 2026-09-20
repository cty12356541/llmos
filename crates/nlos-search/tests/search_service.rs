//! B-SEARCH-001 integration tests: the search service is a read-only query
//! face over the Semantic authority (W33-E gate: read-only queries, zero
//! Semantic-authority writes; the derived in-memory index is rebuildable and
//! never canonical).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::Signer;
use nlos_capability::{
    CapabilityAuthority, CapabilityRights, CapabilityTarget, IssueRootCapabilityRequest,
    SignedIssueRootCapabilityRequest, issue_root_command_message,
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_process::{
    CreateIsolationDomainRequest, ProcessAuthority, RegisterDelegatedProcessRequest,
};
use nlos_search::{
    AssertionSelector, RetractionFilter, SearchError, SearchService, VerificationFilter,
};
use nlos_semantic::{
    AppendAssertionRequest, AppendDecision, AppendTypedEventRequest, AssertionMode, EvaluatorKind,
    EventVerificationTarget, ImmutableEvaluatorReference, ImmutableEvaluatorReferenceKind,
    JudgmentRelation, LocalProcessRef, RetractionMode, SemanticAuthority, SemanticAuthorityError,
    SemanticPayloadIdentity, StoreSigner, StoreSignerError, TaintFlags,
    TrustViewVerificationStatus, UnsignedAssertionEvent, UnsignedJudgmentEvent,
    UnsignedRetractionEvent, UnsignedVerificationEvent, VerificationOutcome, VerificationTarget,
    content_digest, encode_unsigned_assertion_event, encode_unsigned_judgment_event,
    encode_unsigned_retraction_event, encode_unsigned_verification_event, semantic_event_id,
};
use nlos_types::{
    Generation, IdempotencyKey, NamespaceId, PrincipalId, ReceiptId, SemanticEventId,
    TaskAttemptId, TaskId,
};
use rusqlite::{Connection, OpenFlags};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Root(PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-search-{label}-{}-{nonce}-{}",
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
    semantic: Arc<SemanticAuthority>,
    issuer_key: ed25519_dalek::SigningKey,
    issuer: nlos_identity::IdentityBinding,
    process_binding: nlos_process::ProcessBindingRecord,
    scope: CapabilityTarget,
    purpose_digest: Option<[u8; 32]>,
    store_signer: TestSigner,
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
    let semantic = Arc::new(SemanticAuthority::open(root.path()).unwrap());
    Fixture {
        identity,
        capability,
        process,
        semantic,
        issuer_key,
        issuer,
        process_binding,
        scope,
        purpose_digest,
        store_signer: TestSigner {
            key: store_key,
            binding: store_binding,
        },
    }
}

fn capability_with_rights(
    fixture: &Fixture,
    seed: u8,
    rights: CapabilityRights,
) -> nlos_capability::CapabilityRecord {
    let command = IssueRootCapabilityRequest {
        issuer_key_id: fixture.issuer.key_id,
        holder_key_id: fixture.issuer.key_id,
        target: fixture.scope,
        rights,
        purpose_digest: fixture.purpose_digest,
        valid_from_ms: 0,
        valid_until_ms: 9_000,
        delegation_depth_remaining: 0,
        call_limit: None,
        idempotency_key: IdempotencyKey::from_bytes([seed; 16]),
        issued_at_ms: 0,
    };
    fixture
        .capability
        .issue_root_signed(
            &fixture.identity,
            SignedIssueRootCapabilityRequest {
                command,
                signer: fixture.issuer.principal_id,
                signature: fixture
                    .issuer_key
                    .sign(&issue_root_command_message(command))
                    .to_bytes(),
            },
        )
        .unwrap()
        .record()
}

fn append_capability(fixture: &Fixture, seed: u8) -> nlos_capability::CapabilityRecord {
    capability_with_rights(fixture, seed, CapabilityRights::SEMANTIC_APPEND)
}

#[allow(clippy::too_many_arguments)]
fn append_assertion(
    fixture: &Fixture,
    append_cap: &nlos_capability::CapabilityRecord,
    seed: u8,
    mode: AssertionMode,
    media_type: &str,
    content: &str,
) -> SemanticEventId {
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
        declared_parents: Vec::new(),
        declassification_receipt_id: None,
        valid_until_ms: Some(8_000),
        purpose_digest: fixture.purpose_digest,
        content_digest: content_digest(media_type, content.as_bytes()).unwrap(),
        assertion_mode: mode,
        execution_evidence_receipt_id: (mode == AssertionMode::FactFromTool)
            .then(|| ReceiptId::from_bytes([seed.wrapping_add(0x30); 16])),
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
        content_media_type: media_type.to_owned(),
        content_bytes: content.as_bytes().to_vec(),
        captured_inputs: Vec::new(),
        ingress_taint: TaintFlags::default(),
        authz_policy_digest: [0x99; 32],
        admission_limit_ms: Some(8_500),
        admitted_at_ms: 2_000 + u64::from(seed),
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

fn append_retraction(
    fixture: &Fixture,
    retract_cap: &nlos_capability::CapabilityRecord,
    seed: u8,
    target: SemanticEventId,
) -> SemanticEventId {
    let event = UnsignedRetractionEvent {
        scope: fixture.scope,
        issuer: fixture.issuer.principal_id,
        issuer_execution: LocalProcessRef {
            process_id: fixture.process_binding.process_id,
            generation: fixture.process_binding.process_generation,
        },
        control_domain: fixture.issuer.control_domain_id,
        issued_at_unix_ns: 6_000_000_000 + u64::from(seed),
        nonce: vec![seed.wrapping_add(140); 16],
        declared_parents: Vec::new(),
        valid_until_ms: None,
        purpose_digest: fixture.purpose_digest,
        key_id: fixture.issuer.key_id,
        target_event_id: target,
        mode: RetractionMode::Withdraw,
        reason_digest: Some([0xdb; 32]),
        authority_evidence_receipt_id: ReceiptId::from_bytes([seed.wrapping_add(0x70); 16]),
    };
    let canonical = encode_unsigned_retraction_event(&event).unwrap();
    let event_id = semantic_event_id(&canonical);
    let request = AppendTypedEventRequest {
        canonical_unsigned_event: canonical,
        claimed_event_id: event_id,
        signature: fixture
            .issuer_key
            .sign(&nlos_identity::semantic_signature_message(event_id))
            .to_bytes(),
        capability: retract_cap.handle,
        captured_inputs: Vec::new(),
        ingress_taint: TaintFlags::default(),
        authz_policy_digest: [0x99; 32],
        admission_limit_ms: Some(8_500),
        admitted_at_ms: 2_700,
    };
    match fixture.semantic.append_retraction(
        &fixture.identity,
        &fixture.capability,
        &fixture.process,
        &fixture.store_signer,
        &request,
    ) {
        Ok(AppendDecision::Admitted(_) | AppendDecision::Replayed(_)) => event_id,
        Err(error) => panic!("retraction admission failed: {error}"),
    }
}

/// The canonical workload: five assertions in admission order
/// (seeds 1..=5), verifications and one retraction, plus one judgment
/// that must never surface in assertion queries.
///
/// - assertion 1: Inference, verified Pass.
/// - assertion 2: Speculation, verified Fail then Pass (latest wins).
/// - assertion 3: Directive, verified Inconclusive.
/// - assertion 4: Inference, unverified.
/// - assertion 5: `FactFromTool`, unverified, later withdrawn.
struct Workload {
    root: Root,
    fixture: Fixture,
    search: SearchService,
    events: [SemanticEventId; 5],
}

fn workload(label: &str) -> Workload {
    let root = Root::new(label);
    let fixture = fixture(&root, 10);
    let append_cap = append_capability(&fixture, 0xa1);
    let one = append_assertion(
        &fixture,
        &append_cap,
        1,
        AssertionMode::Inference,
        "text/plain",
        "one",
    );
    let two = append_assertion(
        &fixture,
        &append_cap,
        2,
        AssertionMode::Speculation,
        "application/json",
        "two",
    );
    let three = append_assertion(
        &fixture,
        &append_cap,
        3,
        AssertionMode::Directive,
        "text/plain",
        "three",
    );
    let four = append_assertion(
        &fixture,
        &append_cap,
        4,
        AssertionMode::Inference,
        "text/plain",
        "four",
    );
    let five = append_assertion(
        &fixture,
        &append_cap,
        5,
        AssertionMode::FactFromTool,
        "text/plain",
        "five",
    );
    append_judgment(
        &fixture,
        &append_cap,
        6,
        JudgmentRelation::Entails,
        one,
        two,
    );
    append_verification(&fixture, &append_cap, 7, one, VerificationOutcome::Pass);
    append_verification(&fixture, &append_cap, 8, two, VerificationOutcome::Fail);
    append_verification(&fixture, &append_cap, 9, two, VerificationOutcome::Pass);
    append_verification(
        &fixture,
        &append_cap,
        10,
        three,
        VerificationOutcome::Inconclusive,
    );
    let retract_cap = capability_with_rights(
        &fixture,
        0xb2,
        CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::SEMANTIC_RETRACT),
    );
    append_retraction(&fixture, &retract_cap, 11, five);
    let search =
        SearchService::open(root.path(), Arc::clone(&fixture.semantic)).expect("open search face");
    Workload {
        root,
        fixture,
        search,
        events: [one, two, three, four, five],
    }
}

fn all_selector() -> AssertionSelector {
    AssertionSelector::new(64)
}

fn mode_selector(mode: AssertionMode) -> AssertionSelector {
    AssertionSelector {
        assertion_mode: Some(mode),
        ..all_selector()
    }
}

fn retraction_selector(filter: RetractionFilter) -> AssertionSelector {
    AssertionSelector {
        retraction: filter,
        ..all_selector()
    }
}

fn verification_selector(status: TrustViewVerificationStatus) -> AssertionSelector {
    AssertionSelector {
        verification: VerificationFilter::Status(status),
        ..all_selector()
    }
}

fn hit_ids(hits: &[nlos_search::AssertionHit]) -> Vec<SemanticEventId> {
    hits.iter().map(|hit| hit.event_id).collect()
}

#[test]
fn selector_queries_match_authority_readback() {
    let workload = workload("readback");
    let expected_order = workload.events.to_vec();

    // Full scan: five assertions in admission (log_seq) order; the
    // judgment/verification/retraction events never surface.
    let hits = workload.search.search_assertions(&all_selector()).unwrap();
    assert_eq!(hit_ids(&hits), expected_order);
    for hit in &hits {
        let record = workload
            .fixture
            .semantic
            .inspect_event(hit.event_id)
            .unwrap();
        assert_eq!(record.scope, workload.fixture.scope);
        assert_eq!(record.issuer, workload.fixture.issuer.principal_id);
        assert_eq!(record.log_seq, hit.log_seq);
        assert_eq!(
            record.payload_identity,
            SemanticPayloadIdentity::AssertionContent(hit.content_digest)
        );
        let admission = workload
            .fixture
            .semantic
            .inspect_admission_receipt(hit.event_id)
            .unwrap();
        assert_eq!(admission.admitted_at_ms, hit.admitted_at_ms);
    }

    // Predicate shape: assertion mode.
    let inferences = workload
        .search
        .search_assertions(&mode_selector(AssertionMode::Inference))
        .unwrap();
    assert_eq!(
        hit_ids(&inferences),
        vec![workload.events[0], workload.events[3]]
    );
    let directives = workload
        .search
        .search_assertions(&mode_selector(AssertionMode::Directive))
        .unwrap();
    assert_eq!(hit_ids(&directives), vec![workload.events[2]]);

    // Predicate shape: exact content digest.
    let digest = nlos_semantic::content_digest("application/json", b"two").unwrap();
    let by_digest = workload
        .search
        .search_assertions(&AssertionSelector {
            content_digest: Some(digest),
            ..all_selector()
        })
        .unwrap();
    assert_eq!(hit_ids(&by_digest), vec![workload.events[1]]);
    assert_eq!(by_digest[0].content_media_type, "application/json");

    // Predicate shape: scope and issuer.
    let by_scope = workload
        .search
        .search_assertions(&AssertionSelector {
            scope: Some(workload.fixture.scope),
            ..all_selector()
        })
        .unwrap();
    assert_eq!(hit_ids(&by_scope), expected_order);
    let by_issuer = workload
        .search
        .search_assertions(&AssertionSelector {
            issuer: Some(workload.fixture.issuer.principal_id),
            ..all_selector()
        })
        .unwrap();
    assert_eq!(hit_ids(&by_issuer), expected_order);
    let unknown_issuer = workload
        .search
        .search_assertions(&AssertionSelector {
            issuer: Some(PrincipalId::from_bytes([0xfe; 16])),
            ..all_selector()
        })
        .unwrap();
    assert!(unknown_issuer.is_empty());
    let unknown_scope = workload
        .search
        .search_assertions(&AssertionSelector {
            scope: Some(CapabilityTarget::Task(TaskId::from_bytes([0xfd; 16]))),
            ..all_selector()
        })
        .unwrap();
    assert!(unknown_scope.is_empty());

    // Retraction facet straight from the authority's durable rows.
    let only_retracted = workload
        .search
        .search_assertions(&retraction_selector(RetractionFilter::OnlyRetracted))
        .unwrap();
    assert_eq!(hit_ids(&only_retracted), vec![workload.events[4]]);
    let exclude_retracted = workload
        .search
        .search_assertions(&retraction_selector(RetractionFilter::ExcludeRetracted))
        .unwrap();
    assert_eq!(hit_ids(&exclude_retracted), expected_order[..4].to_vec());

    // Limit applies after the filters, preserving log_seq order.
    let limited = workload
        .search
        .search_assertions(&AssertionSelector::new(2))
        .unwrap();
    assert_eq!(hit_ids(&limited), expected_order[..2].to_vec());
}

#[test]
fn verification_joined_search_matches_authority_trust_views() {
    let workload = workload("verification");

    let pass = workload
        .search
        .search_assertions(&verification_selector(TrustViewVerificationStatus::Pass))
        .unwrap();
    assert_eq!(
        hit_ids(&pass),
        vec![workload.events[0], workload.events[1]],
        "latest verification outcome wins: two is Pass after the Fail"
    );
    let inconclusive = workload
        .search
        .search_assertions(&verification_selector(
            TrustViewVerificationStatus::Inconclusive,
        ))
        .unwrap();
    assert_eq!(hit_ids(&inconclusive), vec![workload.events[2]]);
    let unverified = workload
        .search
        .search_assertions(&verification_selector(
            TrustViewVerificationStatus::Unverified,
        ))
        .unwrap();
    assert_eq!(
        hit_ids(&unverified),
        vec![workload.events[3], workload.events[4]]
    );
    let fail = workload
        .search
        .search_assertions(&verification_selector(TrustViewVerificationStatus::Fail))
        .unwrap();
    assert!(fail.is_empty());

    // Limit applies after the verification join, never before it: a small
    // limit over Unverified (4, 5 in admission order) still surfaces the
    // first unverified hit even though earlier hits failed the filter.
    let limited_unverified = workload
        .search
        .search_assertions(&AssertionSelector {
            verification: VerificationFilter::Status(TrustViewVerificationStatus::Unverified),
            limit: 1,
            ..all_selector()
        })
        .unwrap();
    assert_eq!(hit_ids(&limited_unverified), vec![workload.events[3]]);
    let limited_pass = workload
        .search
        .search_assertions(&AssertionSelector {
            verification: VerificationFilter::Status(TrustViewVerificationStatus::Pass),
            limit: 1,
            ..all_selector()
        })
        .unwrap();
    assert_eq!(
        hit_ids(&limited_pass),
        vec![workload.events[0]],
        "the Pass hit at log_seq 2 must survive limit=1, not be shadowed by the earlier hit"
    );

    // The verification join reads exactly the authority's TrustView: every
    // full-scan hit's live trust view matches the joined filter's outcome,
    // and the passthrough returns the authority snapshot verbatim.
    for hit in workload.search.search_assertions(&all_selector()).unwrap() {
        let view = workload
            .fixture
            .semantic
            .inspect_trust_view(hit.event_id)
            .unwrap();
        assert_eq!(workload.search.trust_view(hit.event_id).unwrap(), view);
    }
    let one_view = workload.search.trust_view(workload.events[0]).unwrap();
    assert_eq!(
        one_view.verification_status,
        TrustViewVerificationStatus::Pass
    );
    assert!(!one_view.retracted);
}

#[test]
fn index_rebuild_is_deterministic_and_matches_live_scan() {
    let workload = workload("index");

    let first = workload.search.build_index().unwrap();
    let second = workload.search.build_index().unwrap();
    assert_eq!(
        first.entries(),
        second.entries(),
        "rebuild is deterministic"
    );
    assert_eq!(first.assertion_count(), 5);

    // Structural selectors: the pure index and the live scan agree.
    for selector in [
        all_selector(),
        mode_selector(AssertionMode::Inference),
        retraction_selector(RetractionFilter::OnlyRetracted),
        retraction_selector(RetractionFilter::ExcludeRetracted),
        AssertionSelector::new(3),
    ] {
        let scanned = workload.search.search_assertions(&selector).unwrap();
        let indexed = first.query(&selector).unwrap();
        assert_eq!(
            hit_ids(&scanned),
            hit_ids(&indexed),
            "selector {selector:?}"
        );
        assert_eq!(scanned, indexed);
    }
}

#[test]
fn stale_index_converges_on_rebuild_and_never_overrides_the_authority() {
    let root = Root::new("stale-index");
    let fixture = fixture(&root, 30);
    let append_cap = append_capability(&fixture, 0xc1);
    let retract_cap = capability_with_rights(
        &fixture,
        0xc2,
        CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::SEMANTIC_RETRACT),
    );
    let target = append_assertion(
        &fixture,
        &append_cap,
        1,
        AssertionMode::Inference,
        "text/plain",
        "t",
    );
    let search =
        SearchService::open(root.path(), Arc::clone(&fixture.semantic)).expect("open search face");

    // Index built before the authority retracts: it still reports the
    // pre-retraction state — it is a snapshot, never canonical.
    let stale = search.build_index().unwrap();
    assert!(!stale.entries()[0].retracted);

    append_retraction(&fixture, &retract_cap, 2, target);

    let only_retracted = retraction_selector(RetractionFilter::OnlyRetracted);
    let live = search.search_assertions(&only_retracted).unwrap();
    assert_eq!(
        hit_ids(&live),
        vec![target],
        "live scan sees the authority fact"
    );
    let from_stale = stale.query(&only_retracted).unwrap();
    assert!(
        from_stale.is_empty(),
        "the snapshot under-reports; it must not fabricate or veto authority facts"
    );

    // Rebuilding the index converges to the authority state.
    let rebuilt = search.build_index().unwrap();
    let converged = rebuilt.query(&only_retracted).unwrap();
    assert_eq!(hit_ids(&converged), vec![target]);
}

#[test]
fn no_authority_writes_across_a_full_query_workload() {
    let workload = workload("no-write");
    let authority_db = workload.root.path().join("semantic-authority.db");
    let before = logical_dump(&authority_db);

    // The full query workload: live scans, index builds and queries, trust
    // views and verification-joined searches.
    let selectors = [
        all_selector(),
        mode_selector(AssertionMode::Inference),
        mode_selector(AssertionMode::FactFromTool),
        retraction_selector(RetractionFilter::Any),
        retraction_selector(RetractionFilter::OnlyRetracted),
        retraction_selector(RetractionFilter::ExcludeRetracted),
        verification_selector(TrustViewVerificationStatus::Pass),
        verification_selector(TrustViewVerificationStatus::Unverified),
        AssertionSelector::new(1),
    ];
    for selector in &selectors {
        let _ = workload.search.search_assertions(selector).unwrap();
    }
    let index = workload.search.build_index().unwrap();
    let rebuilt = workload.search.build_index().unwrap();
    for selector in &selectors {
        if matches!(selector.verification, VerificationFilter::Status(_)) {
            let _ = index.query(selector).unwrap_err();
            continue;
        }
        let _ = index.query(selector).unwrap();
    }
    let _ = rebuilt.query(&all_selector()).unwrap();
    for event in workload.events {
        let _ = workload.search.trust_view(event).unwrap();
    }

    let after = logical_dump(&authority_db);
    assert_eq!(
        before, after,
        "a full read-only query workload must not mutate a single authority row"
    );
}

/// Full logical dump of the authority database: the schema objects plus every
/// row of every table, serialized deterministically.
fn logical_dump(database: &Path) -> Vec<String> {
    let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open authority db read-only");
    let mut dump = Vec::new();
    let mut objects = connection
        .prepare(
            "SELECT type, name, tbl_name FROM sqlite_master
             WHERE name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .expect("schema objects");
    let schema_rows: Vec<(String, String, String)> = objects
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("schema rows")
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    for (kind, name, tbl) in schema_rows {
        dump.push(format!("schema|{kind}|{name}|{tbl}"));
    }
    drop(objects);
    let mut tables = connection
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .expect("tables");
    let names: Vec<String> = tables
        .query_map([], |row| row.get(0))
        .expect("table names")
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    for table in names {
        let mut statement = connection
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .expect("rows");
        let column_count = statement.column_count();
        let mut rows = statement.query([]).expect("row iterator");
        while let Some(row) = rows.next().unwrap() {
            let mut fields = Vec::with_capacity(column_count);
            for column in 0..column_count {
                let value = row.get_ref(column).expect("value");
                fields.push(format!("{value:?}"));
            }
            dump.push(format!("{table}|{}", fields.join("\u{1f}")));
        }
    }
    dump
}

#[test]
fn typed_failures_for_empty_state_and_invalid_inputs() {
    let root = Root::new("typed-empty");
    let fixture = fixture(&root, 40);
    let search =
        SearchService::open(root.path(), Arc::clone(&fixture.semantic)).expect("open search face");

    // Empty authority: typed-empty results, not errors.
    assert!(
        search
            .search_assertions(&all_selector())
            .unwrap()
            .is_empty()
    );
    let index = search.build_index().unwrap();
    assert_eq!(index.assertion_count(), 0);
    assert!(index.query(&all_selector()).unwrap().is_empty());
    assert!(
        search
            .search_assertions(&verification_selector(TrustViewVerificationStatus::Pass))
            .unwrap()
            .is_empty()
    );

    // Zero limit fails closed on both faces.
    assert!(matches!(
        search.search_assertions(&AssertionSelector::new(0)),
        Err(SearchError::InvalidLimit)
    ));
    assert!(matches!(
        index.query(&AssertionSelector::new(0)),
        Err(SearchError::InvalidLimit)
    ));

    // Unknown events propagate the authority's typed EventNotFound.
    let missing = SemanticEventId::from_bytes([0xee; 32]);
    assert!(matches!(
        search.trust_view(missing),
        Err(SearchError::Authority(
            SemanticAuthorityError::EventNotFound(_)
        ))
    ));

    // The pure index cannot join derived trust state: typed refusal.
    assert!(matches!(
        index.query(&verification_selector(TrustViewVerificationStatus::Pass)),
        Err(SearchError::VerificationJoinUnavailable)
    ));

    // A root without an authority database fails closed.
    let missing_root = Root::new("typed-missing");
    std::fs::create_dir_all(missing_root.path()).unwrap();
    assert!(SearchService::open(missing_root.path(), Arc::clone(&fixture.semantic)).is_err());
}

#[test]
fn schema_version_gate_fails_closed() {
    let root = Root::new("schema-gate");
    std::fs::create_dir_all(root.path()).unwrap();
    let database = root.path().join("semantic-authority.db");
    let connection = Connection::open(&database).unwrap();
    connection
        .pragma_update(None, "user_version", 9_999)
        .unwrap();
    drop(connection);
    let fixture_root = Root::new("schema-gate-authority");
    let fixture = fixture(&fixture_root, 50);
    let Err(error) = SearchService::open(root.path(), Arc::clone(&fixture.semantic)) else {
        panic!("unsupported schema version must fail closed");
    };
    assert!(matches!(
        error,
        SearchError::SchemaVersionUnsupported(9_999)
    ));
}
