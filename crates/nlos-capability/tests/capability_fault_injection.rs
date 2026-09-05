//! B-CAPABILITY-004 (lane W15-N): kill-window / fault-injection matrix minimum
//! prefix for `CapabilityAuthority` durable write paths — `issue_root`,
//! `delegate`, and `consume` (call-limit ledger).
//!
//! Harness follows the established `nlos-store-fault` discipline: process-global
//! fault state serialized by `FAULT_LOCK`, only the Capability authority connection
//! routes through the shim via a `SQLite` URI filename (see header deviation note);
//! Identity stays on the plain VFS. Row counts, typed `SQLite` error chains,
//! `PRAGMA integrity_check`, and byte-equal replay convergence are asserted per
//! scenario.
//!
//! **Fault-VFS plumbing deviation (documented harness constraint)**: unlike
//! `SemanticAuthority`, `CapabilityAuthority` has no `open_with_vfs` constructor
//! and the workspace forbids `unsafe`, so the shim is routed through a `SQLite`
//! **URI filename**: `CapabilityAuthority::open` passes
//! `root.join("capability-authority.db")` through unchanged, so a root of
//! `file:<db>?vfs=<shim>&tail=` routes that connection through the registered
//! fault VFS. A RAII sandbox process CWD contains the junk directory that
//! `create_dir_all(root)` creates for the literal URI path.
//!
//! Matrix (minimum prefix — four fault classes × three write lanes):
//! - W1 pre-commit `IOERR` on `issue_root` / `consume`;
//! - W2 pre-commit ENOSPC (`SQLITE_FULL`) on `issue_root` / `delegate`;
//! - W3 commit-point `PowerLossAfter` invisible direction (modeled lost write):
//!   phantom rows wholly absent after reopen, same-key redo byte-equal;
//! - W4 replay storm — same request replayed 3+ times plus once after reopen.
//!
//! **Crash semantics disclaimer**: `PowerLossAfter` models a successful write
//! response whose bytes never reach durable storage; kill-9 / torn-WAL sweeps
//! are out of scope for this minimum prefix.

#![allow(deprecated)]
#![allow(clippy::large_types_passed_by_value)] // fault matrix helpers mirror channel precedent

use std::error::Error as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use ed25519_dalek::{Signer, SigningKey};
use nlos_capability::{
    CapabilityAuthority, CapabilityAuthorityError, CapabilityConsumptionDecision,
    CapabilityIssueDecision, CapabilityRecord, CapabilityRights, CapabilityTarget,
    ConsumeCapabilityRequest, DelegateCapabilityRequest, IssueRootCapabilityRequest,
};
use nlos_identity::{
    BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose,
    VerifySemanticSignatureRequest, semantic_signature_message,
};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_types::{IdempotencyKey, NamespaceId, SemanticEventId};
use rusqlite::Connection;

const VFS_NAME: &str = "nlos-capability-fault";

static FAULT_LOCK: Mutex<()> = Mutex::new(());
static NEXT: AtomicU64 = AtomicU64::new(1);

fn fault_lock() -> MutexGuard<'static, ()> {
    FAULT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CapabilityCounts {
    heads: i64,
    versions: i64,
    issue_receipts: i64,
    revocation_receipts: i64,
    consumption_rows: i64,
}

const EMPTY: CapabilityCounts = CapabilityCounts {
    heads: 0,
    versions: 0,
    issue_receipts: 0,
    revocation_receipts: 0,
    consumption_rows: 0,
};

const ONE_ISSUED: CapabilityCounts = CapabilityCounts {
    heads: 1,
    versions: 1,
    issue_receipts: 1,
    revocation_receipts: 0,
    consumption_rows: 0,
};

const TWO_ISSUED: CapabilityCounts = CapabilityCounts {
    heads: 2,
    versions: 2,
    issue_receipts: 2,
    revocation_receipts: 0,
    consumption_rows: 0,
};

const ONE_ISSUED_ONE_CONSUMED: CapabilityCounts = CapabilityCounts {
    heads: 1,
    versions: 1,
    issue_receipts: 1,
    revocation_receipts: 0,
    consumption_rows: 1,
};

struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "nlos-capability-fault-{label}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&base).expect("create test root");
        Self(base)
    }

    fn base(&self) -> &Path {
        &self.0
    }

    fn database(&self) -> PathBuf {
        self.0.join("capability-authority.db")
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct SandboxCwd {
    previous: PathBuf,
    directory: PathBuf,
}

impl SandboxCwd {
    fn new(label: &str) -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "nlos-capability-fault-cwd-{label}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create sandbox cwd");
        let previous = std::env::current_dir().expect("capture previous cwd");
        std::env::set_current_dir(&directory).expect("enter sandbox cwd");
        Self {
            previous,
            directory,
        }
    }
}

impl Drop for SandboxCwd {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn fault_root(base: &Path) -> String {
    let database = base.join("capability-authority.db");
    let uri_path = database.to_string_lossy().replace('\\', "/");
    let trimmed = uri_path.trim_start_matches('/');
    format!("file:///{trimmed}?vfs={VFS_NAME}&tail=")
}

fn open_fault(base: &Path) -> CapabilityAuthority {
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
    CapabilityAuthority::open(fault_root(base)).expect("open capability authority via fault vfs")
}

fn reopen(base: &Path) -> CapabilityAuthority {
    CapabilityAuthority::open(base).expect("reopen capability authority on plain vfs")
}

fn bootstrap(identity: &IdentityAuthority, seed: u8) -> (SigningKey, IdentityBinding) {
    let signing = signing_key(seed);
    let binding = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: signing.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms: 10_000,
            idempotency_key: key(seed.wrapping_add(3)),
            created_at_ms: 0,
        })
        .unwrap()
        .binding();
    (signing, binding)
}

fn root_request(
    issuer: IdentityBinding,
    holder: IdentityBinding,
    seed: u8,
    call_limit: Option<u64>,
) -> IssueRootCapabilityRequest {
    IssueRootCapabilityRequest {
        issuer_key_id: issuer.key_id,
        holder_key_id: holder.key_id,
        target: CapabilityTarget::Namespace(NamespaceId::from_bytes([0x44; 16])),
        rights: CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::DELEGATE),
        purpose_digest: None,
        valid_from_ms: 1_000,
        valid_until_ms: 9_000,
        delegation_depth_remaining: 3,
        call_limit,
        idempotency_key: key(seed),
        issued_at_ms: 500,
    }
}

fn delegate_request(
    parent: CapabilityRecord,
    delegator: IdentityBinding,
    recipient: IdentityBinding,
    seed: u8,
) -> DelegateCapabilityRequest {
    DelegateCapabilityRequest {
        parent: parent.handle,
        delegator_key_id: delegator.key_id,
        recipient_key_id: recipient.key_id,
        target: parent.target,
        rights: CapabilityRights::SEMANTIC_APPEND,
        purpose_digest: parent.purpose_digest,
        valid_from_ms: parent.valid_from_ms,
        valid_until_ms: parent.valid_until_ms,
        delegation_depth_remaining: 2,
        call_limit: Some(5),
        idempotency_key: key(seed),
        delegated_at_ms: 1_100,
    }
}

fn consume_request(
    identity: &IdentityAuthority,
    holder_key: &SigningKey,
    holder: IdentityBinding,
    record: CapabilityRecord,
    idempotency_key: IdempotencyKey,
    at_ms: u64,
) -> ConsumeCapabilityRequest {
    let event_id = SemanticEventId::from_bytes([0x66; 32]);
    let signature = holder_key
        .sign(&semantic_signature_message(event_id))
        .to_bytes();
    let signer = identity
        .verify_semantic_signature(VerifySemanticSignatureRequest {
            event_id,
            issuer: holder.principal_id,
            control_domain_id: holder.control_domain_id,
            key_id: holder.key_id,
            signature,
            admitted_at_ms: at_ms,
        })
        .unwrap();
    ConsumeCapabilityRequest {
        handle: record.handle,
        signer,
        target: record.target,
        required_right: CapabilityRights::SEMANTIC_APPEND,
        purpose_digest: record.purpose_digest,
        idempotency_key,
        consumed_at_ms: at_ms,
    }
}

fn capability_counts(database: &Path) -> CapabilityCounts {
    let connection = Connection::open(database).expect("open capability for row counts");
    let query = |sql: &str| -> i64 {
        connection
            .query_row(sql, [], |row| row.get(0))
            .expect("count query")
    };
    CapabilityCounts {
        heads: query("SELECT COUNT(*) FROM capability_heads"),
        versions: query("SELECT COUNT(*) FROM capability_versions"),
        issue_receipts: query("SELECT COUNT(*) FROM capability_issue_receipts"),
        revocation_receipts: query("SELECT COUNT(*) FROM capability_revocation_receipts"),
        consumption_rows: query("SELECT COUNT(*) FROM capability_consumption_rows"),
    }
}

fn assert_counts(database: &Path, expected: CapabilityCounts) {
    assert_eq!(
        capability_counts(database),
        expected,
        "unexpected durable capability row set"
    );
}

fn assert_integrity(database: &Path) {
    let connection = Connection::open(database).expect("open for integrity check");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run integrity_check");
    assert_eq!(
        result, "ok",
        "capability database must pass integrity_check"
    );
}

fn assert_sqlite_error_chain(error: &CapabilityAuthorityError, needles: &[&str]) {
    match error {
        CapabilityAuthorityError::Sqlite(sqlite) => {
            let mut chain = sqlite.to_string().to_lowercase();
            let mut source = sqlite.source();
            while let Some(cause) = source {
                chain.push_str(" <- ");
                chain.push_str(&cause.to_string().to_lowercase());
                source = cause.source();
            }
            assert!(
                needles.iter().any(|needle| chain.contains(needle)),
                "error chain must name injected condition, got: {chain}"
            );
        }
        other => panic!("expected Sqlite error, got {other}"),
    }
}

fn issued(
    decision: CapabilityIssueDecision,
) -> (CapabilityRecord, nlos_capability::CapabilityIssueReceipt) {
    match decision {
        CapabilityIssueDecision::Issued(record, receipt) => (record, receipt),
        CapabilityIssueDecision::Replayed(_, _) => panic!("expected Issued, got Replayed"),
    }
}

fn replayed_issue(decision: CapabilityIssueDecision) -> nlos_capability::CapabilityIssueReceipt {
    match decision {
        CapabilityIssueDecision::Replayed(_, receipt)
        | CapabilityIssueDecision::Issued(_, receipt) => receipt,
    }
}

fn consumed(
    decision: CapabilityConsumptionDecision,
) -> nlos_capability::CapabilityConsumptionReceipt {
    match decision {
        CapabilityConsumptionDecision::Consumed(receipt) => receipt,
        CapabilityConsumptionDecision::Replayed(_) => panic!("expected Consumed, got Replayed"),
    }
}

fn replayed_consume(
    decision: CapabilityConsumptionDecision,
) -> nlos_capability::CapabilityConsumptionReceipt {
    match decision {
        CapabilityConsumptionDecision::Replayed(receipt)
        | CapabilityConsumptionDecision::Consumed(receipt) => receipt,
    }
}

fn issue_root(
    capability: &CapabilityAuthority,
    identity: &IdentityAuthority,
    request: IssueRootCapabilityRequest,
) -> Result<CapabilityIssueDecision, CapabilityAuthorityError> {
    capability.issue_root(identity, request)
}

fn delegate(
    capability: &CapabilityAuthority,
    identity: &IdentityAuthority,
    request: DelegateCapabilityRequest,
) -> Result<CapabilityIssueDecision, CapabilityAuthorityError> {
    capability.delegate(identity, request)
}

fn consume(
    capability: &CapabilityAuthority,
    request: ConsumeCapabilityRequest,
) -> Result<CapabilityConsumptionDecision, CapabilityAuthorityError> {
    capability.consume(request)
}

fn fixture_with_parent(
    seed: u8,
) -> (
    TestRoot,
    IdentityAuthority,
    CapabilityAuthority,
    IdentityBinding,
    IdentityBinding,
    CapabilityRecord,
) {
    let root = TestRoot::new("parent-fixture");
    let identity = IdentityAuthority::open(root.base()).expect("open identity");
    let (delegator_key, delegator) = bootstrap(&identity, seed);
    let (_, recipient) = bootstrap(&identity, seed.wrapping_add(10));
    let capability = reopen(root.base());
    let parent = issue_root(
        &capability,
        &identity,
        root_request(delegator, delegator, seed.wrapping_add(1), Some(10)),
    )
    .unwrap()
    .record();
    let _ = delegator_key;
    (root, identity, capability, delegator, recipient, parent)
}

fn fixture_with_issued(
    seed: u8,
) -> (
    TestRoot,
    IdentityAuthority,
    CapabilityAuthority,
    SigningKey,
    IdentityBinding,
    CapabilityRecord,
) {
    let root = TestRoot::new("issued-fixture");
    let identity = IdentityAuthority::open(root.base()).expect("open identity");
    let (holder_key, holder) = bootstrap(&identity, seed);
    let capability = reopen(root.base());
    let record = issue_root(
        &capability,
        &identity,
        root_request(holder, holder, seed.wrapping_add(1), Some(3)),
    )
    .unwrap()
    .record();
    (root, identity, capability, holder_key, holder, record)
}

#[test]
fn capability_precommit_ioerr_fails_typed_zero_phantom_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();

    {
        let _cwd = SandboxCwd::new("ioerr-issue");
        let root = TestRoot::new("ioerr-issue");
        let database = root.database();
        let identity = IdentityAuthority::open(root.base()).expect("open identity");
        let (_, holder) = bootstrap(&identity, 0x21);
        let capability = open_fault(root.base());
        let request = root_request(holder, holder, 0x01, Some(5));

        nlos_store_fault::arm(FaultMode::FailWritesAfter {
            remaining: 0,
            code: FaultCode::IoErr,
        });
        let error = issue_root(&capability, &identity, request).expect_err("issue under IOERR");
        assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
        assert!(nlos_store_fault::writes_observed() > 0);
        nlos_store_fault::disarm();
        assert_counts(&database, EMPTY);
        assert_integrity(&database);

        let (_, receipt) = issued(issue_root(&capability, &identity, request).unwrap());
        assert_counts(&database, ONE_ISSUED);
        assert_eq!(
            replayed_issue(issue_root(&capability, &identity, request).unwrap()),
            receipt
        );
        assert_integrity(&database);
    }

    {
        let _cwd = SandboxCwd::new("ioerr-consume");
        let (root, identity, capability, holder_key, holder, record) = fixture_with_issued(0x22);
        let database = root.database();
        drop(capability);
        let capability = open_fault(root.base());
        let request = consume_request(&identity, &holder_key, holder, record, key(0x02), 2_000);

        nlos_store_fault::arm(FaultMode::FailWritesAfter {
            remaining: 0,
            code: FaultCode::IoErr,
        });
        let error = consume(&capability, request).expect_err("consume under IOERR");
        assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
        nlos_store_fault::disarm();
        assert_counts(&database, ONE_ISSUED);
        assert_integrity(&database);

        drop(capability);
        let capability = reopen(root.base());
        let receipt = consumed(consume(&capability, request).unwrap());
        assert_counts(&database, ONE_ISSUED_ONE_CONSUMED);
        assert_eq!(
            replayed_consume(consume(&capability, request).unwrap()),
            receipt
        );
        assert_integrity(&database);
    }
}

#[test]
fn capability_precommit_enospc_fails_typed_zero_phantom_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();

    {
        let _cwd = SandboxCwd::new("full-issue");
        let root = TestRoot::new("full-issue");
        let database = root.database();
        let identity = IdentityAuthority::open(root.base()).expect("open identity");
        let (_, holder) = bootstrap(&identity, 0x31);
        let capability = open_fault(root.base());
        let request = root_request(holder, holder, 0x03, None);

        nlos_store_fault::arm(FaultMode::FailWritesAfter {
            remaining: 0,
            code: FaultCode::Full,
        });
        let error = issue_root(&capability, &identity, request).expect_err("issue under ENOSPC");
        assert_sqlite_error_chain(&error, &["full"]);
        nlos_store_fault::disarm();
        assert_counts(&database, EMPTY);

        let (_, receipt) = issued(issue_root(&capability, &identity, request).unwrap());
        assert_counts(&database, ONE_ISSUED);
        assert_eq!(
            replayed_issue(issue_root(&capability, &identity, request).unwrap()),
            receipt
        );
        assert_integrity(&database);
    }

    {
        let _cwd = SandboxCwd::new("full-delegate");
        let (root, identity, capability, delegator, recipient, parent) = fixture_with_parent(0x32);
        let database = root.database();
        drop(capability);
        let capability = open_fault(root.base());
        let request = delegate_request(parent, delegator, recipient, 0x04);

        nlos_store_fault::arm(FaultMode::FailWritesAfter {
            remaining: 0,
            code: FaultCode::Full,
        });
        let error = delegate(&capability, &identity, request).expect_err("delegate under ENOSPC");
        assert_sqlite_error_chain(&error, &["full"]);
        nlos_store_fault::disarm();
        assert_counts(&database, ONE_ISSUED);
        assert_integrity(&database);

        drop(capability);
        let capability = reopen(root.base());
        let (_, receipt) = issued(delegate(&capability, &identity, request).unwrap());
        assert_counts(&database, TWO_ISSUED);
        assert_eq!(
            replayed_issue(delegate(&capability, &identity, request).unwrap()),
            receipt
        );
        assert_integrity(&database);
    }
}

#[test]
fn capability_power_loss_invisible_commit_converges_for_issue_and_consume() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();

    {
        let _cwd = SandboxCwd::new("pl-issue");
        let root = TestRoot::new("pl-issue");
        let database = root.database();
        let identity = IdentityAuthority::open(root.base()).expect("open identity");
        let (_, holder) = bootstrap(&identity, 0x41);
        let capability = open_fault(root.base());
        let request = root_request(holder, holder, 0x05, Some(2));

        nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
        let phantom = match issue_root(&capability, &identity, request).unwrap() {
            CapabilityIssueDecision::Issued(record, receipt) => (record, receipt),
            CapabilityIssueDecision::Replayed(_, _) => panic!("phantom issue cannot replay"),
        };
        assert!(nlos_store_fault::writes_observed() > 0);
        nlos_store_fault::disarm();
        drop(capability);

        assert_counts(&database, EMPTY);
        assert_integrity(&database);

        let capability = reopen(root.base());
        let redo = issued(issue_root(&capability, &identity, request).unwrap());
        assert_eq!(redo.0.handle, phantom.0.handle);
        assert_eq!(redo.1, phantom.1);
        assert_eq!(
            replayed_issue(issue_root(&capability, &identity, request).unwrap()),
            phantom.1
        );
        assert_counts(&database, ONE_ISSUED);
        assert_integrity(&database);
    }

    {
        let _cwd = SandboxCwd::new("pl-consume");
        let (root, identity, capability, holder_key, holder, record) = fixture_with_issued(0x42);
        let database = root.database();
        drop(capability);
        let capability = open_fault(root.base());
        let request = consume_request(&identity, &holder_key, holder, record, key(0x06), 2_000);

        nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
        let phantom = consumed(consume(&capability, request).unwrap());
        nlos_store_fault::disarm();
        drop(capability);

        assert_counts(&database, ONE_ISSUED);
        assert_integrity(&database);

        let capability = reopen(root.base());
        let redo = consumed(consume(&capability, request).unwrap());
        assert_eq!(redo, phantom);
        assert_eq!(
            replayed_consume(consume(&capability, request).unwrap()),
            phantom
        );
        assert_counts(&database, ONE_ISSUED_ONE_CONSUMED);
        assert_integrity(&database);
    }
}

#[test]
fn capability_replay_storm_is_byte_equal_idempotent() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();

    {
        let root = TestRoot::new("storm-issue");
        let database = root.database();
        let identity = IdentityAuthority::open(root.base()).expect("open identity");
        let (_, holder) = bootstrap(&identity, 0x51);
        let capability = reopen(root.base());
        let request = root_request(holder, holder, 0x07, Some(4));
        let (_, original) = issued(issue_root(&capability, &identity, request).unwrap());

        for round in 0..3 {
            let replay = replayed_issue(issue_root(&capability, &identity, request).unwrap());
            assert_eq!(replay, original, "issue replay storm round {round}");
        }
        drop(capability);

        let capability = reopen(root.base());
        assert_eq!(
            replayed_issue(issue_root(&capability, &identity, request).unwrap()),
            original
        );
        assert_counts(&database, ONE_ISSUED);
        assert_integrity(&database);
    }

    {
        let (root, identity, capability, holder_key, holder, record) = fixture_with_issued(0x52);
        let database = root.database();
        let request = consume_request(&identity, &holder_key, holder, record, key(0x08), 2_000);
        let original = consumed(consume(&capability, request).unwrap());

        for round in 0..3 {
            let replay = replayed_consume(consume(&capability, request).unwrap());
            assert_eq!(replay, original, "consume replay storm round {round}");
        }
        drop(capability);

        let capability = reopen(root.base());
        assert_eq!(
            replayed_consume(consume(&capability, request).unwrap()),
            original
        );
        assert_counts(&database, ONE_ISSUED_ONE_CONSUMED);
        assert_integrity(&database);
    }
}
