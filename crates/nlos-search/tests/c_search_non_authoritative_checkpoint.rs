//! C-SEARCH-001 / W65 integration tests: non-authoritative consistency
//! declaration & use-restriction gate (`[CONS-DECL-001]`, `[CONS-CTRL-001]`),
//! signed vector checkpoint-bounded snapshot candidate discovery
//! (`[SEM-CHECKPOINT-001]`, `[TASK-SNAPSHOT-001]`), authority/checkpoint
//! candidate re-verification (`[TASK-SNAPSHOT-003]`, `[SEM-RETRACT-004]`),
//! cursor-paginated scans (`[PERF-CONTROL-001]`), multi-Cell federated
//! candidate aggregation (`[DIST-FAIL-001]`), and the strict `SQLite`
//! zero-write invariant.

use std::collections::BTreeSet;
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
    AssertionHit, AssertionSelector, CellSearchShardOutcome, ControlConsumerKind,
    FailedShardSummary, NonAuthoritativeCandidateHit, PaginatedAssertionSelector,
    RemoteSearchFailureReason, RetractionFilter, SearchAchievedConsistency, SearchError,
    SearchPage, SearchRequestedConsistency, SearchService, SearchUseRestrictions,
    VerificationFilter, aggregate_federated_candidates, enforce_not_control_authoritative,
    verify_candidate_against_checkpoint,
};
use nlos_semantic::{
    AppendAssertionRequest, AppendDecision, AppendTypedEventRequest, AssertionMode, EvaluatorKind,
    EventVerificationTarget, ImmutableEvaluatorReference, ImmutableEvaluatorReferenceKind,
    LocalProcessRef, RetractionMode, SemanticAuthority, SemanticVectorCheckpointError, StoreSigner,
    StoreSignerError, TaintFlags, TrustViewVerificationStatus, UnsignedAssertionEvent,
    UnsignedRetractionEvent, UnsignedVerificationEvent, VerificationOutcome, VerificationTarget,
    content_digest, encode_unsigned_assertion_event, encode_unsigned_retraction_event,
    encode_unsigned_verification_event, semantic_event_id,
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
            "nlos-c-search-{label}-{}-{nonce}-{}",
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

struct CellFixture {
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

impl CellFixture {
    fn issuer_domain_id(&self) -> [u8; 16] {
        self.issuer.control_domain_id.into_bytes()
    }

    fn store_verifying_key_bytes(&self) -> [u8; 32] {
        self.store_signer.key.verifying_key().to_bytes()
    }
}

fn bootstrap_principal(
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
            key_valid_until_ms: 100_000,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
            created_at_ms: 0,
        })
        .expect("bootstrap principal")
        .binding();
    (key, binding)
}

fn new_cell_fixture(root: &Root, seed: u8) -> CellFixture {
    let identity = IdentityAuthority::open(root.path()).expect("open identity");
    let (issuer_key, issuer) = bootstrap_principal(&identity, seed);
    let (store_key, store_binding) = bootstrap_principal(&identity, seed.wrapping_add(20));
    let process = ProcessAuthority::open(root.path()).expect("open process");
    let domain = process
        .create_isolation_domain(CreateIsolationDomainRequest {
            policy_digest: [seed.wrapping_add(4); 32],
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(5); 16]),
            created_at_ms: 0,
        })
        .expect("create domain")
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
        .expect("register process")
        .record()
        .clone();
    let scope = CapabilityTarget::Namespace(NamespaceId::from_bytes([0x44; 16]));
    let purpose_digest = Some([0x77; 32]);
    let capability = CapabilityAuthority::open(root.path()).expect("open capability");
    let semantic = Arc::new(SemanticAuthority::open(root.path()).expect("open semantic"));
    CellFixture {
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

fn issue_capability(
    fixture: &CellFixture,
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
        valid_until_ms: 90_000,
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
        .expect("issue root capability")
        .record()
}

fn append_assertion_at(
    fixture: &CellFixture,
    cap: &nlos_capability::CapabilityRecord,
    seed: u8,
    mode: AssertionMode,
    media_type: &str,
    content: &str,
    admitted_at_ms: u64,
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
        valid_until_ms: Some(85_000),
        purpose_digest: fixture.purpose_digest,
        content_digest: content_digest(media_type, content.as_bytes()).expect("content digest"),
        assertion_mode: mode,
        execution_evidence_receipt_id: (mode == AssertionMode::FactFromTool)
            .then(|| ReceiptId::from_bytes([seed.wrapping_add(0x30); 16])),
        confidence_bp: Some(8_000),
        key_id: fixture.issuer.key_id,
    };
    let canonical = encode_unsigned_assertion_event(&event).expect("encode assertion");
    let event_id = semantic_event_id(&canonical);
    let request = AppendAssertionRequest {
        canonical_unsigned_event: canonical,
        claimed_event_id: event_id,
        signature: fixture
            .issuer_key
            .sign(&nlos_identity::semantic_signature_message(event_id))
            .to_bytes(),
        capability: cap.handle,
        content_media_type: media_type.to_owned(),
        content_bytes: content.as_bytes().to_vec(),
        captured_inputs: Vec::new(),
        ingress_taint: TaintFlags::default(),
        authz_policy_digest: [0x99; 32],
        admission_limit_ms: Some(80_000),
        admitted_at_ms,
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

fn append_verification_pass(
    fixture: &CellFixture,
    cap: &nlos_capability::CapabilityRecord,
    seed: u8,
    target: SemanticEventId,
    admitted_at_ms: u64,
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
        outcome: VerificationOutcome::Pass,
        evaluator_kind: EvaluatorKind::DeterministicTool,
        procedure_ref: ImmutableEvaluatorReference {
            kind: ImmutableEvaluatorReferenceKind::Artifact,
            digest: [0xda; 32],
        },
        evaluator_evidence_receipt_id: ReceiptId::from_bytes([seed.wrapping_add(0x60); 16]),
        evidence: Vec::new(),
    };
    let canonical = encode_unsigned_verification_event(&event).expect("encode verification");
    let event_id = semantic_event_id(&canonical);
    let request = AppendTypedEventRequest {
        canonical_unsigned_event: canonical,
        claimed_event_id: event_id,
        signature: fixture
            .issuer_key
            .sign(&nlos_identity::semantic_signature_message(event_id))
            .to_bytes(),
        capability: cap.handle,
        captured_inputs: Vec::new(),
        ingress_taint: TaintFlags::default(),
        authz_policy_digest: [0x99; 32],
        admission_limit_ms: Some(80_000),
        admitted_at_ms,
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

fn append_retraction_withdraw(
    fixture: &CellFixture,
    retract_cap: &nlos_capability::CapabilityRecord,
    seed: u8,
    target: SemanticEventId,
    admitted_at_ms: u64,
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
    let canonical = encode_unsigned_retraction_event(&event).expect("encode retraction");
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
        admission_limit_ms: Some(80_000),
        admitted_at_ms,
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

#[test]
fn cons_ctrl_001_use_restrictions_reject_all_five_control_gates_and_allow_candidate_discovery() {
    let restrictions = SearchUseRestrictions::NON_AUTHORITATIVE;
    assert_eq!(restrictions, SearchUseRestrictions::default());
    assert!(restrictions.is_strictly_non_authoritative());
    assert!(restrictions.forbid_capability_decision);
    assert!(restrictions.forbid_resource_ledger_commit);
    assert!(restrictions.forbid_task_head_commit);
    assert!(restrictions.forbid_package_update);
    assert!(restrictions.forbid_irreversible_action);

    assert!(
        restrictions
            .enforce_not_control_authoritative(ControlConsumerKind::CandidateDiscovery)
            .is_ok()
    );
    assert!(enforce_not_control_authoritative(ControlConsumerKind::CandidateDiscovery).is_ok());

    let prohibited = [
        ControlConsumerKind::CapabilityDecision,
        ControlConsumerKind::ResourceAccountCommit,
        ControlConsumerKind::TaskHeadCommit,
        ControlConsumerKind::PackageUpdate,
        ControlConsumerKind::IrreversibleAction,
    ];
    for gate in prohibited {
        assert!(matches!(
            restrictions.enforce_not_control_authoritative(gate),
            Err(SearchError::ControlAuthoritativeUseForbidden(actual)) if actual == gate
        ));
        assert!(matches!(
            enforce_not_control_authoritative(gate),
            Err(SearchError::ControlAuthoritativeUseForbidden(actual)) if actual == gate
        ));
    }

    // Defense-in-depth: clearing any single `forbid_*` flag fails closed even
    // for `CandidateDiscovery`, preventing forged struct literals from bypassing
    // `[CONS-CTRL-001]`.
    let tampered = SearchUseRestrictions {
        forbid_task_head_commit: false,
        ..SearchUseRestrictions::NON_AUTHORITATIVE
    };
    assert!(!tampered.is_strictly_non_authoritative());
    assert!(matches!(
        tampered.enforce_not_control_authoritative(ControlConsumerKind::CandidateDiscovery),
        Err(SearchError::ControlAuthoritativeUseForbidden(
            ControlConsumerKind::CandidateDiscovery
        ))
    ));
}

#[test]
fn cons_decl_001_requested_vs_achieved_consistency_downgrade_gate() {
    let req_snapshot = SearchRequestedConsistency::CheckpointBoundedSnapshot;
    assert!(
        req_snapshot
            .verify_achieved(
                SearchAchievedConsistency::CheckpointBoundedSnapshot,
                false,
                &[]
            )
            .is_ok()
    );
    for weaker in [
        SearchAchievedConsistency::EventualLiveScan,
        SearchAchievedConsistency::DerivedInMemoryIndex,
        SearchAchievedConsistency::FederatedPartialEventual,
    ] {
        assert!(matches!(
            req_snapshot.verify_achieved(weaker, false, &[weaker]),
            Err(SearchError::ConsistencyUnsatisfied {
                requested: SearchRequestedConsistency::CheckpointBoundedSnapshot,
                achieved,
            }) if achieved == weaker
        ));
        assert!(matches!(
            req_snapshot.verify_achieved(weaker, true, &[]),
            Err(SearchError::ConsistencyUnsatisfied { .. })
        ));
        assert!(
            req_snapshot
                .verify_achieved(weaker, true, &[weaker])
                .is_ok()
        );
    }

    let req_eventual = SearchRequestedConsistency::EventualOrDerived;
    for satisfied in [
        SearchAchievedConsistency::EventualLiveScan,
        SearchAchievedConsistency::DerivedInMemoryIndex,
        SearchAchievedConsistency::CheckpointBoundedSnapshot,
    ] {
        assert!(req_eventual.verify_achieved(satisfied, false, &[]).is_ok());
    }
    assert!(matches!(
        req_eventual.verify_achieved(
            SearchAchievedConsistency::FederatedPartialEventual,
            false,
            &[],
        ),
        Err(SearchError::ConsistencyUnsatisfied { .. })
    ));
    assert!(
        req_eventual
            .verify_achieved(
                SearchAchievedConsistency::FederatedPartialEventual,
                true,
                &[SearchAchievedConsistency::FederatedPartialEventual],
            )
            .is_ok()
    );
}

#[test]
fn sem_checkpoint_001_search_candidates_at_checkpoint_excludes_post_checkpoint_admissions() {
    let root = Root::new("ckpt-snapshot");
    let fixture = new_cell_fixture(&root, 10);
    let append_cap = issue_capability(&fixture, 0xa1, CapabilityRights::SEMANTIC_APPEND);

    let a1 = append_assertion_at(
        &fixture,
        &append_cap,
        1,
        AssertionMode::Inference,
        "text/plain",
        "pre-1",
        1_000,
    );
    let a2 = append_assertion_at(
        &fixture,
        &append_cap,
        2,
        AssertionMode::FactFromTool,
        "application/json",
        "{\"k\":2}",
        1_100,
    );
    append_verification_pass(&fixture, &append_cap, 3, a1, 1_150);
    let a3 = append_assertion_at(
        &fixture,
        &append_cap,
        4,
        AssertionMode::Directive,
        "text/plain",
        "pre-3",
        1_200,
    );

    let checkpoint = fixture
        .semantic
        .export_signed_vector_checkpoint(
            &fixture.identity,
            &fixture.store_signer,
            [0x55; 32],
            1_300,
        )
        .expect("export signed vector checkpoint");

    // Admit two post-checkpoint assertions into the same live store.
    let a4 = append_assertion_at(
        &fixture,
        &append_cap,
        5,
        AssertionMode::Inference,
        "text/plain",
        "post-4",
        2_000,
    );
    let a5 = append_assertion_at(
        &fixture,
        &append_cap,
        6,
        AssertionMode::Inference,
        "text/plain",
        "post-5",
        2_100,
    );

    let search =
        SearchService::open(root.path(), Arc::clone(&fixture.semantic)).expect("open search");
    let live_hits = search
        .search_assertions(&AssertionSelector::new(10))
        .expect("live search");
    assert_eq!(
        live_hits.iter().map(|h| h.event_id).collect::<Vec<_>>(),
        vec![a1, a2, a3, a4, a5]
    );

    let vk_bytes = fixture.store_verifying_key_bytes();
    let page = search
        .search_candidates_at_checkpoint(
            fixture.issuer_domain_id(),
            &checkpoint,
            &vk_bytes,
            &AssertionSelector::new(10),
            None,
        )
        .expect("checkpoint search via key");
    assert!(page.exhausted);
    assert_eq!(page.next_after_log_seq, None);
    assert_eq!(
        page.achieved_consistency,
        SearchAchievedConsistency::CheckpointBoundedSnapshot
    );
    assert_eq!(
        page.use_restrictions,
        SearchUseRestrictions::NON_AUTHORITATIVE
    );
    assert!(
        page.enforce_not_control_authoritative(ControlConsumerKind::TaskHeadCommit)
            .is_err()
    );
    assert_eq!(
        page.items
            .iter()
            .map(|c| c.hit.event_id)
            .collect::<Vec<_>>(),
        vec![a1, a2, a3]
    );
}

#[test]
fn sem_checkpoint_001_checkpoint_search_paginates_within_signed_watermark_bound() {
    let root = Root::new("ckpt-paged");
    let fixture = new_cell_fixture(&root, 15);
    let append_cap = issue_capability(&fixture, 0xa8, CapabilityRights::SEMANTIC_APPEND);

    let a1 = append_assertion_at(
        &fixture,
        &append_cap,
        1,
        AssertionMode::Inference,
        "text/plain",
        "c1",
        1_000,
    );
    let a2 = append_assertion_at(
        &fixture,
        &append_cap,
        2,
        AssertionMode::Inference,
        "text/plain",
        "c2",
        1_100,
    );
    let a3 = append_assertion_at(
        &fixture,
        &append_cap,
        3,
        AssertionMode::Inference,
        "text/plain",
        "c3",
        1_200,
    );
    let checkpoint = fixture
        .semantic
        .export_signed_vector_checkpoint(
            &fixture.identity,
            &fixture.store_signer,
            [0x56; 32],
            1_300,
        )
        .expect("export checkpoint");
    append_assertion_at(
        &fixture,
        &append_cap,
        4,
        AssertionMode::Inference,
        "text/plain",
        "post-c4",
        2_000,
    );

    let search =
        SearchService::open(root.path(), Arc::clone(&fixture.semantic)).expect("open search");
    let p1 = search
        .search_candidates_at_checkpoint(
            fixture.issuer_domain_id(),
            &checkpoint,
            &fixture.identity,
            &AssertionSelector::new(2),
            None,
        )
        .expect("ckpt page 1");
    assert!(!p1.exhausted);
    assert!(p1.next_after_log_seq.is_some());
    assert_eq!(
        p1.items.iter().map(|c| c.hit.event_id).collect::<Vec<_>>(),
        vec![a1, a2]
    );

    let p2 = search
        .search_candidates_at_checkpoint(
            fixture.issuer_domain_id(),
            &checkpoint,
            &fixture.identity,
            &AssertionSelector::new(2),
            p1.next_after_log_seq,
        )
        .expect("ckpt page 2");
    assert!(p2.exhausted);
    assert_eq!(p2.next_after_log_seq, None);
    assert_eq!(
        p2.items.iter().map(|c| c.hit.event_id).collect::<Vec<_>>(),
        vec![a3]
    );
}

#[test]
fn sem_checkpoint_001_checkpoint_search_rejects_tampered_signature_digest_and_wrong_key() {
    let root = Root::new("ckpt-tamper");
    let fixture = new_cell_fixture(&root, 25);
    let append_cap = issue_capability(&fixture, 0xa2, CapabilityRights::SEMANTIC_APPEND);
    append_assertion_at(
        &fixture,
        &append_cap,
        1,
        AssertionMode::Inference,
        "text/plain",
        "alpha",
        1_000,
    );

    let checkpoint = fixture
        .semantic
        .export_signed_vector_checkpoint(
            &fixture.identity,
            &fixture.store_signer,
            [0x66; 32],
            1_500,
        )
        .expect("export signed checkpoint");
    let vk_bytes = fixture.store_verifying_key_bytes();
    let search =
        SearchService::open(root.path(), Arc::clone(&fixture.semantic)).expect("open search");

    let mut bad_sig = checkpoint.clone();
    bad_sig.signature[0] ^= 0xff;
    assert!(matches!(
        search.search_candidates_at_checkpoint(
            fixture.issuer_domain_id(),
            &bad_sig,
            &vk_bytes,
            &AssertionSelector::new(5),
            None,
        ),
        Err(SearchError::InvalidCheckpoint(
            SemanticVectorCheckpointError::InvalidSignature
        ))
    ));
    assert!(matches!(
        search.search_candidates_at_checkpoint(
            fixture.issuer_domain_id(),
            &bad_sig,
            &fixture.identity,
            &AssertionSelector::new(5),
            None,
        ),
        Err(SearchError::InvalidCheckpoint(
            SemanticVectorCheckpointError::InvalidSignature
        ))
    ));

    let mut bad_digest = checkpoint.clone();
    bad_digest.checkpoint_digest[31] ^= 0x01;
    assert!(matches!(
        search.search_candidates_at_checkpoint(
            fixture.issuer_domain_id(),
            &bad_digest,
            &vk_bytes,
            &AssertionSelector::new(5),
            None,
        ),
        Err(SearchError::InvalidCheckpoint(
            SemanticVectorCheckpointError::CheckpointDigestMismatch { .. }
        ))
    ));

    let wrong_key = ed25519_dalek::SigningKey::from_bytes(&[0xfe; 32])
        .verifying_key()
        .to_bytes();
    assert!(matches!(
        search.search_candidates_at_checkpoint(
            fixture.issuer_domain_id(),
            &checkpoint,
            &wrong_key,
            &AssertionSelector::new(5),
            None,
        ),
        Err(SearchError::InvalidCheckpoint(
            SemanticVectorCheckpointError::SignerBindingMismatch
        ))
    ));
}

#[test]
fn sem_checkpoint_001_checkpoint_search_enforces_domain_presence_and_merkle_root_match() {
    let root = Root::new("ckpt-domain-merkle");
    let fixture = new_cell_fixture(&root, 26);
    let append_cap = issue_capability(&fixture, 0xa9, CapabilityRights::SEMANTIC_APPEND);
    append_assertion_at(
        &fixture,
        &append_cap,
        1,
        AssertionMode::Inference,
        "text/plain",
        "alpha",
        1_000,
    );

    let checkpoint = fixture
        .semantic
        .export_signed_vector_checkpoint(
            &fixture.identity,
            &fixture.store_signer,
            [0x67; 32],
            1_500,
        )
        .expect("export signed checkpoint");
    let vk_bytes = fixture.store_verifying_key_bytes();
    let search =
        SearchService::open(root.path(), Arc::clone(&fixture.semantic)).expect("open search");

    let absent_domain = [0xee; 16];
    assert!(matches!(
        search.search_candidates_at_checkpoint(
            absent_domain,
            &checkpoint,
            &vk_bytes,
            &AssertionSelector::new(5),
            None,
        ),
        Err(SearchError::MissingDomainWatermark { domain_id }) if domain_id == absent_domain
    ));
    let empty_optional = search
        .search_candidates_at_checkpoint_with_options(
            absent_domain,
            &checkpoint,
            &vk_bytes,
            &AssertionSelector::new(5),
            None,
            false,
        )
        .expect("optional domain absent returns empty page");
    assert_eq!(empty_optional.items.len(), 0);
    assert!(empty_optional.exhausted);
    assert_eq!(empty_optional.next_after_log_seq, None);

    // Validly signed checkpoint from a divergent store (different Merkle root
    // for the same domain seed) fails closed with `CheckpointWatermarkMismatch`.
    let other_root = Root::new("ckpt-divergent");
    let other_fixture = new_cell_fixture(&other_root, 26);
    let other_cap = issue_capability(&other_fixture, 0xa9, CapabilityRights::SEMANTIC_APPEND);
    append_assertion_at(
        &other_fixture,
        &other_cap,
        1,
        AssertionMode::Inference,
        "text/plain",
        "divergent-payload",
        1_000,
    );
    let divergent_checkpoint = other_fixture
        .semantic
        .export_signed_vector_checkpoint(
            &other_fixture.identity,
            &other_fixture.store_signer,
            [0x67; 32],
            1_500,
        )
        .expect("export divergent checkpoint");
    assert!(matches!(
        search.search_candidates_at_checkpoint(
            fixture.issuer_domain_id(),
            &divergent_checkpoint,
            &other_fixture.store_verifying_key_bytes(),
            &AssertionSelector::new(5),
            None,
        ),
        Err(SearchError::CheckpointWatermarkMismatch { domain_id })
            if domain_id == fixture.issuer_domain_id()
    ));
}

#[test]
fn task_snapshot_003_verify_candidate_against_checkpoint_succeeds_for_valid_unretracted_fact() {
    let root = Root::new("verify-ok");
    let fixture = new_cell_fixture(&root, 40);
    let append_cap = issue_capability(&fixture, 0xa3, CapabilityRights::SEMANTIC_APPEND);

    let a1 = append_assertion_at(
        &fixture,
        &append_cap,
        1,
        AssertionMode::Inference,
        "text/plain",
        "fact-one",
        1_000,
    );
    let a2 = append_assertion_at(
        &fixture,
        &append_cap,
        2,
        AssertionMode::FactFromTool,
        "text/plain",
        "fact-two",
        1_100,
    );

    let checkpoint = fixture
        .semantic
        .export_signed_vector_checkpoint(
            &fixture.identity,
            &fixture.store_signer,
            [0x77; 32],
            1_200,
        )
        .expect("export signed checkpoint");

    let search =
        SearchService::open(root.path(), Arc::clone(&fixture.semantic)).expect("open search");
    let index = search.build_index().expect("build derived index");
    let derived_candidates = index
        .query_candidates(fixture.issuer_domain_id(), &AssertionSelector::new(10))
        .expect("query derived candidates");
    assert_eq!(derived_candidates.len(), 2);
    assert_eq!(
        derived_candidates[0].achieved_consistency,
        SearchAchievedConsistency::DerivedInMemoryIndex
    );

    let vk_bytes = fixture.store_verifying_key_bytes();
    let proof1 = verify_candidate_against_checkpoint(
        &search,
        &derived_candidates[0],
        &checkpoint,
        &vk_bytes,
    )
    .expect("re-verify candidate 1 via pinned key");
    assert_eq!(proof1.event_id, a1);
    assert_eq!(proof1.domain_id, fixture.issuer_domain_id());
    assert_eq!(proof1.local_log_seq, 1);
    assert_eq!(proof1.watermark_max_log_seq, 2);
    assert_eq!(proof1.checkpoint_id, checkpoint.checkpoint_id);
    assert_eq!(proof1.checkpoint_digest, checkpoint.checkpoint_digest);
    assert_eq!(proof1.admission_receipt.event_id, a1);

    let proof2 = search
        .verify_candidate_against_checkpoint(&derived_candidates[1], &checkpoint, &fixture.identity)
        .expect("re-verify candidate 2 via IdentityAuthority");
    assert_eq!(proof2.event_id, a2);
    assert_eq!(proof2.local_log_seq, 2);
}

#[test]
fn task_snapshot_003_verify_candidate_fails_closed_on_post_index_retraction_and_post_checkpoint_drift()
 {
    let root = Root::new("verify-reject");
    let fixture = new_cell_fixture(&root, 55);
    let cap = issue_capability(
        &fixture,
        0xa4,
        CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::SEMANTIC_RETRACT),
    );

    let a_valid = append_assertion_at(
        &fixture,
        &cap,
        1,
        AssertionMode::Inference,
        "text/plain",
        "stays-valid",
        1_000,
    );
    let a_retracted = append_assertion_at(
        &fixture,
        &cap,
        2,
        AssertionMode::Inference,
        "text/plain",
        "retracted-after-index",
        1_100,
    );
    let checkpoint = fixture
        .semantic
        .export_signed_vector_checkpoint(
            &fixture.identity,
            &fixture.store_signer,
            [0x88; 32],
            1_200,
        )
        .expect("export checkpoint");

    let search =
        SearchService::open(root.path(), Arc::clone(&fixture.semantic)).expect("open search");
    let indexed = search
        .build_index()
        .expect("build index")
        .query_candidates(fixture.issuer_domain_id(), &AssertionSelector::new(10))
        .expect("query stale index");
    assert_eq!(indexed.len(), 2);
    assert!(!indexed[1].hit.retracted);

    append_retraction_withdraw(&fixture, &cap, 3, a_retracted, 2_000);
    let a_post = append_assertion_at(
        &fixture,
        &cap,
        4,
        AssertionMode::Inference,
        "text/plain",
        "admitted-after-checkpoint",
        2_100,
    );
    let vk_bytes = fixture.store_verifying_key_bytes();

    assert_eq!(
        verify_candidate_against_checkpoint(&search, &indexed[0], &checkpoint, &vk_bytes)
            .expect("valid candidate passes")
            .event_id,
        a_valid
    );
    assert!(matches!(
        verify_candidate_against_checkpoint(&search, &indexed[1], &checkpoint, &vk_bytes),
        Err(SearchError::CandidateRetracted { event_id }) if event_id == a_retracted
    ));

    let post_hit = search
        .search_assertions(&AssertionSelector::new(10))
        .expect("live scan")
        .into_iter()
        .find(|h| h.event_id == a_post)
        .expect("find post-ckpt hit");
    let post_candidate = NonAuthoritativeCandidateHit::new(
        post_hit,
        fixture.issuer_domain_id(),
        SearchAchievedConsistency::EventualLiveScan,
    );
    assert!(matches!(
        verify_candidate_against_checkpoint(&search, &post_candidate, &checkpoint, &vk_bytes),
        Err(SearchError::CandidateOutsideCheckpoint {
            event_id,
            candidate_log_seq: 4,
            watermark_max_log_seq: 2,
        }) if event_id == a_post
    ));

    let mut forged = indexed[0].clone();
    forged.hit.content_digest = [0xde; 32];
    assert!(matches!(
        verify_candidate_against_checkpoint(&search, &forged, &checkpoint, &vk_bytes),
        Err(SearchError::CandidateAuthorityMismatch(_))
    ));
}

#[test]
fn perf_control_001_cursor_paginated_scan_has_zero_duplicates_and_zero_gaps_across_filters() {
    let root = Root::new("paginated");
    let fixture = new_cell_fixture(&root, 70);
    let cap = issue_capability(
        &fixture,
        0xa5,
        CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::SEMANTIC_RETRACT),
    );

    for idx in 0_u8..35 {
        let mode = match idx % 3 {
            0 => AssertionMode::Inference,
            1 => AssertionMode::FactFromTool,
            _ => AssertionMode::Directive,
        };
        let id = append_assertion_at(
            &fixture,
            &cap,
            idx.wrapping_add(1),
            mode,
            "text/plain",
            &format!("item-{idx}"),
            1_000 + u64::from(idx),
        );
        if idx % 7 == 0 {
            append_retraction_withdraw(
                &fixture,
                &cap,
                idx.wrapping_add(80),
                id,
                3_000 + u64::from(idx),
            );
        }
    }

    let search =
        SearchService::open(root.path(), Arc::clone(&fixture.semantic)).expect("open search");
    let full_expected = search
        .search_assertions(&AssertionSelector::new(100))
        .expect("baseline full scan");
    assert_eq!(full_expected.len(), 35);

    for page_size in [1_usize, 4, 5, 7, 50] {
        let collected = collect_all_pages(&search, AssertionSelector::new(page_size), None);
        assert_eq!(
            collected, full_expected,
            "unfiltered pagination mismatch for page_size={page_size}"
        );
    }

    let filtered_selector = AssertionSelector {
        assertion_mode: Some(AssertionMode::Inference),
        retraction: RetractionFilter::ExcludeRetracted,
        limit: 100,
        ..AssertionSelector::new(100)
    };
    let filtered_expected = search
        .search_assertions(&filtered_selector)
        .expect("filtered baseline");
    assert_ne!(filtered_expected.len(), 0);
    let filtered_paged = collect_all_pages(
        &search,
        AssertionSelector {
            limit: 3,
            ..filtered_selector
        },
        None,
    );
    assert_eq!(filtered_paged, filtered_expected);

    // At `log_seq = 17`, assertion #14 (`idx = 14`) is admitted, while its
    // retraction sits at `log_seq = 18`: bounding at 17 excludes post-cut
    // retraction #18 (`retracted == false`), while bounding at 18 includes it
    // (`retracted == true`) without leaking assertion #15 (`log_seq = 19`).
    let cutoff_before_retraction = full_expected[14].log_seq;
    let bounded_before = collect_all_pages(
        &search,
        AssertionSelector::new(4),
        Some(cutoff_before_retraction),
    );
    assert_eq!(bounded_before.len(), 15);
    assert!(!bounded_before[14].retracted);

    let bounded_after = collect_all_pages(
        &search,
        AssertionSelector::new(4),
        Some(cutoff_before_retraction + 1),
    );
    assert_eq!(bounded_after, full_expected[..15].to_vec());
}

#[test]
fn perf_control_001_paginated_scan_bounds_per_page_sql_window_without_scanning_tail() {
    let root = Root::new("paginated-window");
    let fixture = new_cell_fixture(&root, 75);
    let cap = issue_capability(&fixture, 0xa6, CapabilityRights::SEMANTIC_APPEND);

    let mut ids = Vec::with_capacity(80);
    for idx in 0_u8..80 {
        ids.push(append_assertion_at(
            &fixture,
            &cap,
            idx.wrapping_add(1),
            AssertionMode::Directive,
            "text/plain",
            &format!("w-{idx}"),
            1_000 + u64::from(idx),
        ));
    }
    append_verification_pass(&fixture, &cap, 200, ids[0], 4_000);
    append_verification_pass(&fixture, &cap, 201, ids[1], 4_001);

    let search =
        SearchService::open(root.path(), Arc::clone(&fixture.semantic)).expect("open search");
    let page1 = search
        .search_assertions_paginated(&PaginatedAssertionSelector::new(AssertionSelector {
            verification: VerificationFilter::Status(TrustViewVerificationStatus::Pass),
            limit: 2,
            ..AssertionSelector::new(2)
        }))
        .expect("bounded verified page 1");
    assert_eq!(page1.items.len(), 2);
    assert_eq!(page1.items[0].event_id, ids[0]);
    assert_eq!(page1.items[1].event_id, ids[1]);
    assert!(!page1.exhausted);
    assert_eq!(page1.next_after_log_seq, Some(page1.items[1].log_seq));
    assert_eq!(
        page1.achieved_consistency,
        SearchAchievedConsistency::EventualLiveScan
    );
}

fn collect_all_pages(
    search: &SearchService,
    selector: AssertionSelector,
    max_log_seq_inclusive: Option<u64>,
) -> Vec<AssertionHit> {
    let mut cursor: Option<u64> = None;
    let mut seen_ids: BTreeSet<SemanticEventId> = BTreeSet::new();
    let mut prev_seq = 0_u64;
    let mut out = Vec::new();
    loop {
        let page: SearchPage<AssertionHit> = search
            .search_assertions_paginated(&PaginatedAssertionSelector {
                selector,
                after_log_seq: cursor,
                max_log_seq_inclusive,
            })
            .expect("paginated query");
        assert_eq!(page.exhausted, page.next_after_log_seq.is_none());
        assert_eq!(
            page.use_restrictions,
            SearchUseRestrictions::NON_AUTHORITATIVE
        );
        assert!(page.items.len() <= selector.limit);
        for hit in page.items {
            assert!(
                hit.log_seq > prev_seq,
                "log_seq must advance strictly across pages"
            );
            assert!(
                seen_ids.insert(hit.event_id),
                "duplicate event_id across pages"
            );
            prev_seq = hit.log_seq;
            out.push(hit);
        }
        if page.exhausted {
            break;
        }
        assert!(page.next_after_log_seq.expect("cursor") >= prev_seq);
        cursor = page.next_after_log_seq;
    }
    out
}

struct TwoCellShards {
    shard_a: CellSearchShardOutcome,
    shard_b: CellSearchShardOutcome,
    expected_order: Vec<SemanticEventId>,
    domain_a: [u8; 16],
    domain_b: [u8; 16],
}

fn build_two_cell_federated_shards() -> TwoCellShards {
    let root_a = Root::new("fed-cell-a");
    let root_b = Root::new("fed-cell-b");
    let cell_a = new_cell_fixture(&root_a, 90);
    let cell_b = new_cell_fixture(&root_b, 120);
    let cap_a = issue_capability(
        &cell_a,
        0xb1,
        CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::SEMANTIC_RETRACT),
    );
    let cap_b = issue_capability(&cell_b, 0xb2, CapabilityRights::SEMANTIC_APPEND);

    let a_shared_1 = append_assertion_at(
        &cell_a,
        &cap_a,
        1,
        AssertionMode::Inference,
        "text/plain",
        "shared-1",
        1_100,
    );
    let a_shared_2 = append_assertion_at(
        &cell_a,
        &cap_a,
        2,
        AssertionMode::FactFromTool,
        "text/plain",
        "shared-2",
        1_200,
    );
    let a_only = append_assertion_at(
        &cell_a,
        &cap_a,
        3,
        AssertionMode::Directive,
        "text/plain",
        "cell-a-only",
        1_300,
    );
    let b_only = append_assertion_at(
        &cell_b,
        &cap_b,
        10,
        AssertionMode::Inference,
        "text/plain",
        "cell-b-only",
        1_050,
    );
    append_retraction_withdraw(&cell_a, &cap_a, 4, a_shared_2, 1_900);

    let hits_a = SearchService::open(root_a.path(), Arc::clone(&cell_a.semantic))
        .expect("open search A")
        .search_assertions(&AssertionSelector::new(20))
        .expect("hits A");
    let mut hits_b = SearchService::open(root_b.path(), Arc::clone(&cell_b.semantic))
        .expect("open search B")
        .search_assertions(&AssertionSelector::new(20))
        .expect("hits B");

    let mut rep_1 = hits_a
        .iter()
        .find(|h| h.event_id == a_shared_1)
        .expect("s1")
        .clone();
    rep_1.log_seq = 99;
    rep_1.admitted_at_ms = 3_000;
    let mut rep_2 = hits_a
        .iter()
        .find(|h| h.event_id == a_shared_2)
        .expect("s2")
        .clone();
    assert!(rep_2.retracted);
    rep_2.log_seq = 42;
    rep_2.admitted_at_ms = 2_500;
    rep_2.retracted = false;
    hits_b.push(rep_1);
    hits_b.push(rep_2);

    let (domain_a, domain_b) = ([0x0a; 16], [0x0b; 16]);
    TwoCellShards {
        shard_a: CellSearchShardOutcome::Available {
            cell_domain_id: domain_a,
            hits: hits_a,
            checkpoint_digest: Some([0x11; 32]),
        },
        shard_b: CellSearchShardOutcome::Available {
            cell_domain_id: domain_b,
            hits: hits_b,
            checkpoint_digest: Some([0x22; 32]),
        },
        expected_order: vec![b_only, a_shared_1, a_shared_2, a_only],
        domain_a,
        domain_b,
    }
}

#[test]
fn sem_checkpoint_001_federated_aggregator_deduplicates_across_cells_with_arrival_independence() {
    let TwoCellShards {
        shard_a,
        shard_b,
        expected_order,
        domain_a,
        domain_b,
    } = build_two_cell_federated_shards();

    let report_forward =
        aggregate_federated_candidates(&[shard_a.clone(), shard_b.clone()], 10, false)
            .expect("aggregate A then B");
    let report_reversed =
        aggregate_federated_candidates(&[shard_b.clone(), shard_a.clone()], 10, false)
            .expect("aggregate B then A");
    let report_replayed =
        aggregate_federated_candidates(&[shard_a.clone(), shard_b, shard_a], 10, false)
            .expect("aggregate idempotent replay");

    assert_eq!(report_forward, report_reversed);
    assert_eq!(report_forward.candidates, report_replayed.candidates);
    assert!(!report_forward.is_partial());
    assert_eq!(report_forward.failed_shards.len(), 0);
    assert!(report_forward.idempotent_read);
    assert_eq!(
        report_forward.achieved_consistency,
        SearchAchievedConsistency::CheckpointBoundedSnapshot
    );
    assert_eq!(
        report_forward
            .candidates
            .iter()
            .map(|c| c.event_id)
            .collect::<Vec<_>>(),
        expected_order
    );
    assert_eq!(report_forward.candidates[1].admitted_at_ms, 1_100);
    assert_eq!(
        report_forward.candidates[1].observed_by_cells,
        vec![domain_a, domain_b]
    );
    assert!(report_forward.candidates[2].retracted);
    assert_eq!(
        report_forward.candidates[2].observed_by_cells,
        vec![domain_a, domain_b]
    );
}

#[test]
fn dist_fail_001_federated_remote_failures_fail_closed_by_default_and_surface_explicit_partial_downgrade()
 {
    let root = Root::new("fed-fail");
    let cell_a = new_cell_fixture(&root, 140);
    let cap_a = issue_capability(&cell_a, 0xc1, CapabilityRights::SEMANTIC_APPEND);
    let event_a = append_assertion_at(
        &cell_a,
        &cap_a,
        1,
        AssertionMode::Inference,
        "text/plain",
        "local-survivor",
        1_000,
    );
    let search_a =
        SearchService::open(root.path(), Arc::clone(&cell_a.semantic)).expect("open search A");
    let hits_a = search_a
        .search_assertions(&AssertionSelector::new(10))
        .expect("hits A");

    let shards = [
        CellSearchShardOutcome::Available {
            cell_domain_id: [0x01; 16],
            hits: hits_a,
            checkpoint_digest: Some([0xaa; 32]),
        },
        CellSearchShardOutcome::Failed {
            cell_domain_id: [0x03; 16],
            reason: RemoteSearchFailureReason::Unavailable,
            retryable: true,
        },
        CellSearchShardOutcome::Failed {
            cell_domain_id: [0x02; 16],
            reason: RemoteSearchFailureReason::DeadlineExceeded { budget_ms: 250 },
            retryable: true,
        },
        CellSearchShardOutcome::Failed {
            cell_domain_id: [0x04; 16],
            reason: RemoteSearchFailureReason::Uncertain,
            retryable: false,
        },
    ];

    let expected_failed = vec![
        FailedShardSummary {
            cell_domain_id: [0x02; 16],
            reason: RemoteSearchFailureReason::DeadlineExceeded { budget_ms: 250 },
            retryable: true,
        },
        FailedShardSummary {
            cell_domain_id: [0x03; 16],
            reason: RemoteSearchFailureReason::Unavailable,
            retryable: true,
        },
        FailedShardSummary {
            cell_domain_id: [0x04; 16],
            reason: RemoteSearchFailureReason::Uncertain,
            retryable: false,
        },
    ];

    let err = aggregate_federated_candidates(&shards, 10, false)
        .expect_err("must fail closed when partial is disallowed");
    match err {
        SearchError::RemoteShardFailed(failed) => assert_eq!(failed, expected_failed),
        other => panic!("unexpected error variant: {other}"),
    }

    let partial = aggregate_federated_candidates(&shards, 10, true)
        .expect("partial federated aggregation allowed");
    assert!(partial.is_partial());
    assert_eq!(
        partial.achieved_consistency,
        SearchAchievedConsistency::FederatedPartialEventual
    );
    assert_eq!(partial.failed_shards, expected_failed);
    assert_eq!(partial.available_shard_count, 1);
    assert_eq!(partial.candidates.len(), 1);
    assert_eq!(partial.candidates[0].event_id, event_a);
    assert_eq!(
        partial.candidates[0].achieved_consistency,
        SearchAchievedConsistency::FederatedPartialEventual
    );
    assert!(
        partial
            .enforce_not_control_authoritative(ControlConsumerKind::IrreversibleAction)
            .is_err()
    );
    assert!(
        partial.candidates[0]
            .enforce_not_control_authoritative(ControlConsumerKind::CapabilityDecision)
            .is_err()
    );
}

#[test]
fn federated_aggregator_rejects_zero_limit_and_conflicting_canonical_payload_for_same_event_id() {
    assert!(matches!(
        aggregate_federated_candidates(&[], 0, false),
        Err(SearchError::InvalidLimit)
    ));

    let hit_1 = AssertionHit {
        event_id: SemanticEventId::from_bytes([0x42; 32]),
        log_seq: 1,
        scope: CapabilityTarget::Namespace(NamespaceId::from_bytes([0x10; 16])),
        issuer: PrincipalId::from_bytes([0x20; 16]),
        assertion_mode: AssertionMode::Inference,
        content_digest: [0xaa; 32],
        content_media_type: String::from("text/plain"),
        admitted_at_ms: 1_000,
        retracted: false,
    };
    let mut hit_2_corrupt = hit_1.clone();
    hit_2_corrupt.log_seq = 9;
    hit_2_corrupt.content_digest = [0xbb; 32];

    let shards = [
        CellSearchShardOutcome::Available {
            cell_domain_id: [0x01; 16],
            hits: vec![hit_1],
            checkpoint_digest: None,
        },
        CellSearchShardOutcome::Available {
            cell_domain_id: [0x02; 16],
            hits: vec![hit_2_corrupt],
            checkpoint_digest: None,
        },
    ];
    assert!(matches!(
        aggregate_federated_candidates(&shards, 10, true),
        Err(SearchError::CorruptRecord(_))
    ));
}

#[test]
fn strict_zero_write_invariant_across_checkpoint_paginated_verification_and_federated_workloads() {
    let root = Root::new("zero-write-w65");
    let fixture = new_cell_fixture(&root, 170);
    let cap = issue_capability(
        &fixture,
        0xd1,
        CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::SEMANTIC_RETRACT),
    );

    let a1 = append_assertion_at(
        &fixture,
        &cap,
        1,
        AssertionMode::Inference,
        "text/plain",
        "zero-write-1",
        1_000,
    );
    let a2 = append_assertion_at(
        &fixture,
        &cap,
        2,
        AssertionMode::FactFromTool,
        "text/plain",
        "zero-write-2",
        1_100,
    );
    append_verification_pass(&fixture, &cap, 3, a1, 1_150);
    let checkpoint = fixture
        .semantic
        .export_signed_vector_checkpoint(
            &fixture.identity,
            &fixture.store_signer,
            [0x99; 32],
            1_200,
        )
        .expect("export checkpoint");
    append_retraction_withdraw(&fixture, &cap, 4, a2, 1_300);

    let db_path = root.path().join("semantic-authority.db");
    let inspector =
        Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("open ro");
    let snapshot_before = snapshot_sqlite_state(&inspector);
    let files_before = list_dir_names(root.path());

    let search =
        SearchService::open(root.path(), Arc::clone(&fixture.semantic)).expect("open search");
    let vk_bytes = fixture.store_verifying_key_bytes();

    let paged = search
        .search_assertions_paginated(&PaginatedAssertionSelector::new(AssertionSelector::new(1)))
        .expect("paginated");
    assert_eq!(paged.items.len(), 1);

    let ckpt_page = search
        .search_candidates_at_checkpoint(
            fixture.issuer_domain_id(),
            &checkpoint,
            &vk_bytes,
            &AssertionSelector::new(10),
            None,
        )
        .expect("checkpoint search");
    assert_eq!(ckpt_page.items.len(), 2);

    let verified =
        verify_candidate_against_checkpoint(&search, &ckpt_page.items[0], &checkpoint, &vk_bytes)
            .expect("verify candidate a1");
    assert_eq!(verified.event_id, a1);
    assert!(matches!(
        verify_candidate_against_checkpoint(&search, &ckpt_page.items[1], &checkpoint, &vk_bytes),
        Err(SearchError::CandidateRetracted { event_id }) if event_id == a2
    ));

    let index = search.build_index().expect("build index");
    assert_eq!(
        index
            .query_candidates(fixture.issuer_domain_id(), &AssertionSelector::new(10))
            .expect("index candidates")
            .len(),
        2
    );
    let _ = search.trust_view(a1).expect("trust view");
    let fed = aggregate_federated_candidates(
        &[CellSearchShardOutcome::Available {
            cell_domain_id: fixture.issuer_domain_id(),
            hits: index.entries().to_vec(),
            checkpoint_digest: Some(checkpoint.checkpoint_digest),
        }],
        10,
        false,
    )
    .expect("federated");
    assert_eq!(fed.candidates.len(), 2);

    assert_eq!(snapshot_before, snapshot_sqlite_state(&inspector));
    assert_eq!(files_before, list_dir_names(root.path()));
}

fn snapshot_sqlite_state(connection: &Connection) -> (i64, Vec<(String, i64)>) {
    let data_version: i64 = connection
        .pragma_query_value(None, "data_version", |row| row.get(0))
        .expect("data_version");
    let mut stmt = connection
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .expect("list tables");
    let tables: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .expect("table rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect tables");
    let counts = tables
        .into_iter()
        .map(|table| {
            let count: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |row| {
                    row.get(0)
                })
                .expect("count table");
            (table, count)
        })
        .collect();
    (data_version, counts)
}

fn list_dir_names(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .expect("read root dir")
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}
