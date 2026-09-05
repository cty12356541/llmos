//! B-SEMANTIC-009 (lane W15-S): kill-window / fault-injection matrix minimum
//! prefix for Assertion and Judgment durable admission write paths.
//!
//! Harness follows the established `nlos-store-fault` discipline: process-global
//! fault state serialized by `FAULT_LOCK`, only the Semantic authority connection
//! routes through the shim via [`SemanticAuthority::open_with_vfs`]; Identity,
//! Process, and Capability stay on the plain VFS. Row counts, typed `SQLite`
//! error chains, `PRAGMA integrity_check`, and byte-equal replay convergence are
//! asserted per scenario.
//!
//! Matrix (minimum prefix — four fault classes × two admission lanes):
//! - W1 pre-commit `IOERR` on `append_assertion` / `append_judgment`;
//! - W2 pre-commit ENOSPC (`SQLITE_FULL`) on the same two entries;
//! - W3 commit-point `PowerLossAfter` invisible direction (modeled lost write):
//!   phantom admission wholly absent after reopen, same-key redo byte-equal;
//! - W4 replay storm — same request replayed 3+ times plus once after reopen.
//!
//! **Crash semantics disclaimer**: `PowerLossAfter` models a successful write
//! response whose bytes never reach durable storage; it is not a hardware
//! power-cut measurement. Kill-9 / torn-WAL sweeps are out of scope for this
//! minimum prefix.

#![allow(deprecated)]

use std::error::Error as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use ed25519_dalek::Signer;
use nlos_capability::{
    CapabilityAuthority, CapabilityRights, CapabilityTarget, IssueRootCapabilityRequest,
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_process::{
    CreateIsolationDomainRequest, ProcessAuthority, RegisterDelegatedProcessRequest,
};
use nlos_semantic::{
    AdmissionReceipt, AppendAssertionRequest, AppendDecision, AppendTypedEventRequest,
    AssertionMode, JudgmentRelation, LocalProcessRef, SemanticAuthority, SemanticAuthorityError,
    StoreSigner, StoreSignerError, TaintFlags, UnsignedAssertionEvent, UnsignedJudgmentEvent,
    content_digest, encode_unsigned_assertion_event, encode_unsigned_judgment_event,
    semantic_event_id,
};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_types::{
    Generation, IdempotencyKey, NamespaceId, ReceiptId, SemanticEventId, TaskAttemptId, TaskId,
};
use rusqlite::Connection;

const VFS_NAME: &str = "nlos-semantic-admission-fault";

static FAULT_LOCK: Mutex<()> = Mutex::new(());
static NEXT: AtomicU64 = AtomicU64::new(1);

fn fault_lock() -> MutexGuard<'static, ()> {
    FAULT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn register_fault_vfs() {
    nlos_store_fault::register(VFS_NAME).expect("register semantic admission fault VFS");
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AdmissionCounts {
    content_objects: i64,
    semantic_events: i64,
    event_signatures: i64,
    event_log: i64,
    admission_receipts: i64,
    semantic_outbox: i64,
}

const EMPTY: AdmissionCounts = AdmissionCounts {
    content_objects: 0,
    semantic_events: 0,
    event_signatures: 0,
    event_log: 0,
    admission_receipts: 0,
    semantic_outbox: 0,
};

const ONE_ASSERTION: AdmissionCounts = AdmissionCounts {
    content_objects: 1,
    semantic_events: 1,
    event_signatures: 1,
    event_log: 1,
    admission_receipts: 1,
    semantic_outbox: 1,
};

const TWO_ASSERTIONS: AdmissionCounts = AdmissionCounts {
    content_objects: 2,
    semantic_events: 2,
    event_signatures: 2,
    event_log: 2,
    admission_receipts: 2,
    semantic_outbox: 2,
};

const TWO_ASSERTIONS_ONE_JUDGMENT: AdmissionCounts = AdmissionCounts {
    content_objects: 2,
    semantic_events: 3,
    event_signatures: 3,
    event_log: 3,
    admission_receipts: 3,
    semantic_outbox: 3,
};

struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "nlos-semantic-admission-fault-{label}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&base).expect("create test root");
        Self(base)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn semantic_database(root: &Path) -> PathBuf {
    root.join("semantic-authority.db")
}

fn admission_counts(root: &Path) -> AdmissionCounts {
    let connection =
        Connection::open(semantic_database(root)).expect("open semantic for row counts");
    let query = |sql: &str| -> i64 {
        connection
            .query_row(sql, [], |row| row.get(0))
            .expect("count query")
    };
    AdmissionCounts {
        content_objects: query("SELECT COUNT(*) FROM content_objects"),
        semantic_events: query("SELECT COUNT(*) FROM semantic_events"),
        event_signatures: query("SELECT COUNT(*) FROM event_signatures"),
        event_log: query("SELECT COUNT(*) FROM event_log"),
        admission_receipts: query("SELECT COUNT(*) FROM admission_receipts"),
        semantic_outbox: query("SELECT COUNT(*) FROM semantic_outbox"),
    }
}

fn assert_counts(root: &Path, expected: AdmissionCounts) {
    assert_eq!(
        admission_counts(root),
        expected,
        "unexpected durable admission row set"
    );
}

fn assert_integrity(root: &Path) {
    let connection = Connection::open(semantic_database(root)).expect("open for integrity");
    let result: String = connection
        .pragma_query_value(None, "integrity_check", |row| row.get(0))
        .expect("integrity_check");
    assert_eq!(result, "ok", "semantic database must pass integrity_check");
}

fn assert_sqlite_error_chain(error: &SemanticAuthorityError, needles: &[&str]) {
    match error {
        SemanticAuthorityError::Sqlite(sqlite) => {
            let mut chain = sqlite.to_string().to_lowercase();
            let mut source = sqlite.source();
            while let Some(cause) = source {
                chain.push_str(" <- ");
                chain.push_str(&cause.to_string().to_lowercase());
                source = cause.source();
            }
            for needle in needles {
                assert!(
                    chain.contains(needle),
                    "error chain must name {needle:?}, got: {chain}"
                );
            }
        }
        other => panic!("expected Sqlite error, got {other}"),
    }
}

#[derive(Clone)]
struct TestSigner {
    key: ed25519_dalek::SigningKey,
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
    root: TestRoot,
    identity: IdentityAuthority,
    capability: CapabilityAuthority,
    process: ProcessAuthority,
    semantic: Option<SemanticAuthority>,
    issuer_key: ed25519_dalek::SigningKey,
    issuer: nlos_identity::IdentityBinding,
    process_binding: nlos_process::ProcessBindingRecord,
    capability_handle: nlos_capability::CapabilityHandle,
    capability_target: CapabilityTarget,
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

fn fixture(root: TestRoot, seed: u8, fault_semantic: bool) -> Fixture {
    register_fault_vfs();
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
    let capability_target = CapabilityTarget::Namespace(NamespaceId::from_bytes([0x44; 16]));
    let purpose_digest = Some([0x77; 32]);
    let capability_record = capability
        .issue_root(
            &identity,
            IssueRootCapabilityRequest {
                issuer_key_id: issuer.key_id,
                holder_key_id: issuer.key_id,
                target: capability_target,
                rights: CapabilityRights::SEMANTIC_APPEND,
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
    let semantic = if fault_semantic {
        Some(SemanticAuthority::open_with_vfs(root.path(), Some(VFS_NAME)).unwrap())
    } else {
        Some(SemanticAuthority::open(root.path()).unwrap())
    };
    Fixture {
        root,
        identity,
        capability,
        process,
        semantic,
        issuer_key,
        issuer,
        process_binding,
        capability_handle: capability_record.handle,
        capability_target,
        purpose_digest,
        store_signer: TestSigner {
            key: store_key,
            binding: store_binding,
        },
    }
}

fn semantic(fixture: &Fixture) -> &SemanticAuthority {
    fixture
        .semantic
        .as_ref()
        .expect("semantic authority must be open")
}

fn close_semantic(fixture: &mut Fixture) {
    fixture.semantic = None;
}

fn open_semantic_at(root: &Path, fault: bool) -> SemanticAuthority {
    if fault {
        SemanticAuthority::open_with_vfs(root, Some(VFS_NAME))
            .expect("open semantic through fault VFS")
    } else {
        SemanticAuthority::open(root).expect("open semantic on plain VFS")
    }
}

fn swap_semantic(fixture: &mut Fixture, fault: bool) {
    close_semantic(fixture);
    fixture.semantic = Some(open_semantic_at(fixture.root.path(), fault));
}

fn wal_sidecar(root: &Path, suffix: &str) -> PathBuf {
    let mut path = semantic_database(root).into_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn discard_stale_wal_sidecars(root: &Path) {
    for suffix in ["-wal", "-shm"] {
        let sidecar = wal_sidecar(root, suffix);
        if sidecar.exists() {
            fs::remove_file(sidecar).expect("remove stale WAL sidecar after fault session");
        }
    }
}

fn reset_semantic_storage(root: &Path) {
    let database = semantic_database(root);
    if database.exists() {
        fs::remove_file(&database).expect("remove semantic database");
    }
    discard_stale_wal_sidecars(root);
}

/// Drop the faulted Semantic connection and reopen on the plain VFS.
fn recover_semantic_after_power_loss(fixture: &mut Fixture, reset_if_empty_prefix: bool) {
    nlos_store_fault::disarm();
    close_semantic(fixture);
    if reset_if_empty_prefix {
        reset_semantic_storage(fixture.root.path());
    } else {
        discard_stale_wal_sidecars(fixture.root.path());
    }
    swap_semantic(fixture, false);
}

fn assertion_request(fixture: &Fixture, seed: u8) -> AppendAssertionRequest {
    let media_type = "text/plain".to_owned();
    let content_bytes = format!("fault-assertion-{seed}").into_bytes();
    let event = UnsignedAssertionEvent {
        scope: fixture.capability_target,
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
        content_digest: content_digest(&media_type, &content_bytes).unwrap(),
        assertion_mode: AssertionMode::Inference,
        execution_evidence_receipt_id: None,
        confidence_bp: Some(8_000),
        key_id: fixture.issuer.key_id,
    };
    let canonical_unsigned_event = encode_unsigned_assertion_event(&event).unwrap();
    let claimed_event_id = semantic_event_id(&canonical_unsigned_event);
    AppendAssertionRequest {
        canonical_unsigned_event,
        claimed_event_id,
        signature: fixture
            .issuer_key
            .sign(&nlos_identity::semantic_signature_message(claimed_event_id))
            .to_bytes(),
        capability: fixture.capability_handle,
        content_media_type: media_type,
        content_bytes,
        captured_inputs: Vec::new(),
        ingress_taint: TaintFlags::default(),
        authz_policy_digest: [0x99; 32],
        admission_limit_ms: Some(8_500),
        admitted_at_ms: 2_000,
    }
}

fn append_assertion(
    fixture: &Fixture,
    request: &AppendAssertionRequest,
) -> Result<AppendDecision, SemanticAuthorityError> {
    semantic(fixture).append_assertion(
        &fixture.identity,
        &fixture.capability,
        &fixture.process,
        &fixture.store_signer,
        request,
    )
}

fn admitted_assertion(fixture: &Fixture, seed: u8) -> (AppendAssertionRequest, AdmissionReceipt) {
    let request = assertion_request(fixture, seed);
    match append_assertion(fixture, &request).unwrap() {
        AppendDecision::Admitted(receipt) => (request, receipt),
        AppendDecision::Replayed(_) => panic!("fresh assertion cannot replay"),
    }
}

fn replayed_assertion(fixture: &Fixture, request: &AppendAssertionRequest) -> AdmissionReceipt {
    match append_assertion(fixture, request).unwrap() {
        AppendDecision::Replayed(receipt) => receipt,
        AppendDecision::Admitted(_) => panic!("expected replayed assertion"),
    }
}

fn commit_assertion_endpoints(fixture: &Fixture) -> (SemanticEventId, SemanticEventId) {
    let left = admitted_assertion(fixture, 0x11).1.event_id;
    let right = admitted_assertion(fixture, 0x12).1.event_id;
    (left, right)
}

fn judgment_request(
    fixture: &Fixture,
    seed: u8,
    source: SemanticEventId,
    target: SemanticEventId,
) -> AppendTypedEventRequest {
    let event = UnsignedJudgmentEvent {
        scope: fixture.capability_target,
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
        relation: JudgmentRelation::Entails,
        source,
        target,
        context_digest: Some([0xcc; 32]),
        evaluator_evidence_receipt_id: ReceiptId::from_bytes([seed.wrapping_add(0x40); 16]),
        confidence_bp: Some(9_000),
    };
    let canonical = encode_unsigned_judgment_event(&event).unwrap();
    let event_id = semantic_event_id(&canonical);
    AppendTypedEventRequest {
        canonical_unsigned_event: canonical,
        claimed_event_id: event_id,
        signature: fixture
            .issuer_key
            .sign(&nlos_identity::semantic_signature_message(event_id))
            .to_bytes(),
        capability: fixture.capability_handle,
        captured_inputs: Vec::new(),
        ingress_taint: TaintFlags::default(),
        authz_policy_digest: [0x33; 32],
        admission_limit_ms: Some(8_500),
        admitted_at_ms: 2_000,
    }
}

fn append_judgment(
    fixture: &Fixture,
    request: &AppendTypedEventRequest,
) -> Result<AppendDecision, SemanticAuthorityError> {
    semantic(fixture).append_judgment(
        &fixture.identity,
        &fixture.capability,
        &fixture.process,
        &fixture.store_signer,
        request,
    )
}

fn admitted_judgment(
    fixture: &Fixture,
    seed: u8,
    source: SemanticEventId,
    target: SemanticEventId,
) -> (AppendTypedEventRequest, AdmissionReceipt) {
    let request = judgment_request(fixture, seed, source, target);
    match append_judgment(fixture, &request).unwrap() {
        AppendDecision::Admitted(receipt) => (request, receipt),
        AppendDecision::Replayed(_) => panic!("fresh judgment cannot replay"),
    }
}

fn replayed_judgment(fixture: &Fixture, request: &AppendTypedEventRequest) -> AdmissionReceipt {
    match append_judgment(fixture, request).unwrap() {
        AppendDecision::Replayed(receipt) => receipt,
        AppendDecision::Admitted(_) => panic!("expected replayed judgment"),
    }
}

fn fixture_with_judgment_endpoints(seed: u8) -> (Fixture, SemanticEventId, SemanticEventId) {
    let plain = fixture(TestRoot::new("judgment-endpoints"), seed, false);
    let (left, right) = commit_assertion_endpoints(&plain);
    (plain, left, right)
}

#[test]
fn admission_precommit_ioerr_fails_typed_zero_phantom_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();

    {
        let fixture = fixture(TestRoot::new("ioerr-assertion"), 0x21, true);
        let request = assertion_request(&fixture, 0x01);
        nlos_store_fault::arm(FaultMode::FailWritesAfter {
            remaining: 0,
            code: FaultCode::IoErr,
        });
        let error = append_assertion(&fixture, &request).expect_err("assertion under IOERR");
        assert_sqlite_error_chain(&error, &["disk i/o"]);
        assert!(nlos_store_fault::writes_observed() > 0);
        nlos_store_fault::disarm();
        assert_counts(fixture.root.path(), EMPTY);
        assert_integrity(fixture.root.path());

        let (_, receipt) = admitted_assertion(&fixture, 0x01);
        assert_counts(fixture.root.path(), ONE_ASSERTION);
        assert_eq!(replayed_assertion(&fixture, &request), receipt);
        assert_integrity(fixture.root.path());
    }

    {
        let (mut fixture, left, right) = fixture_with_judgment_endpoints(0x22);
        swap_semantic(&mut fixture, true);
        let request = judgment_request(&fixture, 0x02, left, right);

        nlos_store_fault::arm(FaultMode::FailWritesAfter {
            remaining: 0,
            code: FaultCode::IoErr,
        });
        let error = append_judgment(&fixture, &request).expect_err("judgment under IOERR");
        assert_sqlite_error_chain(&error, &["disk i/o"]);
        nlos_store_fault::disarm();
        assert_counts(fixture.root.path(), TWO_ASSERTIONS);
        assert_integrity(fixture.root.path());

        swap_semantic(&mut fixture, false);
        let (_, receipt) = admitted_judgment(&fixture, 0x02, left, right);
        assert_counts(fixture.root.path(), TWO_ASSERTIONS_ONE_JUDGMENT);
        assert_eq!(replayed_judgment(&fixture, &request), receipt);
        assert_integrity(fixture.root.path());
    }
}

#[test]
fn admission_precommit_enospc_fails_typed_zero_phantom_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();

    {
        let fixture = fixture(TestRoot::new("full-assertion"), 0x31, true);
        let request = assertion_request(&fixture, 0x03);
        nlos_store_fault::arm(FaultMode::FailWritesAfter {
            remaining: 0,
            code: FaultCode::Full,
        });
        let error = append_assertion(&fixture, &request).expect_err("assertion under ENOSPC");
        assert_sqlite_error_chain(&error, &["full"]);
        nlos_store_fault::disarm();
        assert_counts(fixture.root.path(), EMPTY);

        let (_, receipt) = admitted_assertion(&fixture, 0x03);
        assert_counts(fixture.root.path(), ONE_ASSERTION);
        assert_eq!(replayed_assertion(&fixture, &request), receipt);
        assert_integrity(fixture.root.path());
    }

    {
        let (mut fixture, left, right) = fixture_with_judgment_endpoints(0x32);
        swap_semantic(&mut fixture, true);
        let request = judgment_request(&fixture, 0x04, left, right);

        nlos_store_fault::arm(FaultMode::FailWritesAfter {
            remaining: 0,
            code: FaultCode::Full,
        });
        let error = append_judgment(&fixture, &request).expect_err("judgment under ENOSPC");
        assert_sqlite_error_chain(&error, &["full"]);
        nlos_store_fault::disarm();
        assert_counts(fixture.root.path(), TWO_ASSERTIONS);

        swap_semantic(&mut fixture, false);
        let (_, receipt) = admitted_judgment(&fixture, 0x04, left, right);
        assert_counts(fixture.root.path(), TWO_ASSERTIONS_ONE_JUDGMENT);
        assert_eq!(replayed_judgment(&fixture, &request), receipt);
        assert_integrity(fixture.root.path());
    }
}

#[test]
fn admission_power_loss_invisible_commit_converges_for_assertion_and_judgment() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();

    {
        let mut fixture = fixture(TestRoot::new("pl-assertion"), 0x41, true);
        let request = assertion_request(&fixture, 0x05);
        nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
        let phantom = match append_assertion(&fixture, &request).unwrap() {
            AppendDecision::Admitted(receipt) => receipt,
            AppendDecision::Replayed(_) => panic!("phantom assertion cannot replay"),
        };
        assert!(nlos_store_fault::writes_observed() > 0);
        recover_semantic_after_power_loss(&mut fixture, true);
        assert_counts(fixture.root.path(), EMPTY);
        assert_integrity(fixture.root.path());

        let redo_fixture = fixture;
        let redo = admitted_assertion(&redo_fixture, 0x05).1;
        assert_eq!(redo, phantom);
        assert_eq!(replayed_assertion(&redo_fixture, &request), phantom);
        assert_counts(redo_fixture.root.path(), ONE_ASSERTION);
        assert_integrity(redo_fixture.root.path());
    }

    {
        let (mut fixture, left, right) = fixture_with_judgment_endpoints(0x42);
        swap_semantic(&mut fixture, true);
        let request = judgment_request(&fixture, 0x06, left, right);

        nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
        let phantom = match append_judgment(&fixture, &request).unwrap() {
            AppendDecision::Admitted(receipt) => receipt,
            AppendDecision::Replayed(_) => panic!("phantom judgment cannot replay"),
        };
        recover_semantic_after_power_loss(&mut fixture, false);
        assert_counts(fixture.root.path(), TWO_ASSERTIONS);
        assert_integrity(fixture.root.path());

        let redo = admitted_judgment(&fixture, 0x06, left, right).1;
        assert_eq!(redo, phantom);
        assert_eq!(replayed_judgment(&fixture, &request), phantom);
        assert_counts(fixture.root.path(), TWO_ASSERTIONS_ONE_JUDGMENT);
        assert_integrity(fixture.root.path());
    }
}

#[test]
fn admission_replay_storm_is_byte_equal_idempotent() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();

    {
        let mut fixture = fixture(TestRoot::new("storm-assertion"), 0x51, false);
        let (request, original) = admitted_assertion(&fixture, 0x07);
        for round in 0..3 {
            let replay = replayed_assertion(&fixture, &request);
            assert_eq!(replay, original, "assertion replay storm round {round}");
        }
        swap_semantic(&mut fixture, false);
        assert_eq!(replayed_assertion(&fixture, &request), original);
        assert_counts(fixture.root.path(), ONE_ASSERTION);
        assert_integrity(fixture.root.path());
    }

    {
        let (mut fixture, left, right) = fixture_with_judgment_endpoints(0x52);
        let (request, original) = admitted_judgment(&fixture, 0x08, left, right);
        for round in 0..3 {
            let replay = replayed_judgment(&fixture, &request);
            assert_eq!(replay, original, "judgment replay storm round {round}");
        }
        swap_semantic(&mut fixture, false);
        assert_eq!(replayed_judgment(&fixture, &request), original);
        assert_counts(fixture.root.path(), TWO_ASSERTIONS_ONE_JUDGMENT);
        assert_integrity(fixture.root.path());
    }
}
