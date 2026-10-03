//! W51 capability consume-ledger tests: the semantic write bridge charges
//! the application's root-capability budget exactly once per bridged
//! admission (consume-before-append), re-runs and reopens charge nothing
//! (the consume idempotency key replays), an exhausted budget fails the
//! bridge closed with the typed `CallLimitExhausted` error and zero
//! semantic writes, and the remaining budget stays observable through the
//! runtime's read-only surface. The cross-domain derivation claim (the
//! consume key never equals an event-input key of the same operation
//! tuple) is unit-tested inside `src/semantic_writer.rs`.
//!
//! Fixture discipline mirrors `semantic_writer_bridge.rs` (one package
//! identity, single `executable` entry); this file's idempotency/clock
//! band is `0x75 + 60..67`, disjoint from every documented slice-k helper
//! band.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::Signer;
use nlos_artifact::{
    ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageManifest, PackageManifestEntry,
    ProvenanceSourceTriple, PutRevisionRequest, SignedPackage, VerifyPackageRequest,
    derive_artifact_id, package_manifest_message,
};
use nlos_capability::CapabilityAuthorityError;
use nlos_operation::{CompletionOutcome, OperationState};
use nlos_slice_k::{
    OperationReceiptFact, SEMANTIC_WRITER_ROOT_CALL_LIMIT, SemanticWriter, SemanticWriterKey,
    SliceKError, SliceKRuntime, execute_application_payload, seeded_key,
};
use nlos_types::{
    ApplicationId, ArtifactId, CallbackId, CapabilityId, Generation, OperationId, PackageId,
    ReceiptId,
};
use rusqlite::Connection;

const PACKAGE_BYTES: [u8; 16] = [
    0x2a, 0x64, 0x1f, 0x0c, 0x93, 0xb7, 0x58, 0xe0, 0x4d, 0xaf, 0x1b, 0x36, 0xc8, 0x55, 0x07, 0x91,
];
const ENTRY: &str = "sample-driver";
const SEED: u8 = 0x75;

const PAYLOAD: &[u8] = b"consume ledger payload - executable bytes v1\n";

struct TempDir {
    root: std::path::PathBuf,
}

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

impl TempDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-slice-k-consume-{label}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create temp root");
        Self { root }
    }

    fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove consume temp root: {error}"),
        }
    }
}

fn package_id() -> PackageId {
    PackageId::from_bytes(PACKAGE_BYTES)
}

fn installed_runtime(label: &str) -> (TempDir, SliceKRuntime, ApplicationId) {
    let dir = TempDir::new(label);
    let runtime = SliceKRuntime::open(dir.root()).expect("open runtime");
    let signer = runtime.bootstrap_publisher(SEED).expect("publisher");
    let artifact_id = ArtifactId::from_bytes(derive_artifact_id(package_id(), 1, ENTRY));
    let at_ms = runtime
        .wall_now_ms(seeded_key(SEED, 60))
        .expect("entry publish clock");
    runtime
        .artifacts
        .create_artifact(CreateArtifactSpec {
            artifact_id,
            idempotency_key: seeded_key(SEED, 61),
            content_type: "application/octet-stream".to_string(),
            application_id: None,
            owner: None,
            created_at_ms: at_ms,
        })
        .expect("create entry artifact");
    runtime
        .artifacts
        .put_revision(PutRevisionRequest {
            artifact_id,
            expected_head_revision: 0,
            bytes: PAYLOAD,
            created_at_ms: at_ms,
            provenance: ProvenanceSourceTriple {
                source_a: *artifact_id.as_bytes(),
                source_b: PACKAGE_BYTES,
                source_digest: ContentDigest::of_bytes(PAYLOAD),
            },
        })
        .expect("put entry revision");
    let manifest = PackageManifest {
        package_id: package_id(),
        version: 1,
        entries: vec![PackageManifestEntry {
            name: ENTRY.to_string(),
            artifact_id,
            digest: ContentDigest::of_bytes(PAYLOAD),
            role: PackageEntryRole::Executable,
        }],
    };
    let envelope = SignedPackage {
        signature: signer
            .signing
            .sign(&package_manifest_message(&manifest))
            .to_bytes(),
        signer: signer.principal_id,
        manifest,
    };
    let receipt = runtime
        .artifacts
        .verify_package(
            &runtime.identity,
            VerifyPackageRequest {
                signed: &envelope,
                idempotency_key: seeded_key(SEED, 62),
                verified_at_ms: runtime
                    .wall_now_ms(seeded_key(SEED, 63))
                    .expect("verify clock"),
            },
        )
        .expect("verify payload package")
        .receipt()
        .receipt_id;
    runtime
        .install_verified_package_by_id(receipt, SEED)
        .expect("install");
    let application_id = runtime
        .applications
        .inspect_application(package_id())
        .expect("application read")
        .expect("installed application exists")
        .application_id;
    (dir, runtime, application_id)
}

fn installed_execution(label: &str) -> (TempDir, SliceKRuntime, ApplicationId) {
    let (dir, runtime, application_id) = installed_runtime(label);
    // Touch the payload lane once so the application row and bridge side
    // exist; callers assert the budget from a known charge count.
    let _ = execute_application_payload(&runtime, package_id(), ENTRY)
        .expect("execute the installed executable payload");
    (dir, runtime, application_id)
}

/// Durable consumption-row count of one capability (the same test-only
/// ledger inspection `nlos-capability`'s own tests use).
fn consumption_rows(root: &Path, capability_id: CapabilityId) -> i64 {
    let connection = Connection::open(root.join("capability").join("capability-authority.db"))
        .expect("open capability ledger");
    connection
        .query_row(
            "SELECT COUNT(*) FROM capability_consumption_rows WHERE capability_id=?1",
            [capability_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("count consumption rows")
}

/// A fabricated terminal-receipt fact for `operation_byte` under one
/// application: the bridge's admission gates (identity, execution fence,
/// capability, content, lineage) are all writer- and derivation-sourced,
/// so driving the ledger does not require the payload lane.
fn fact(application_id: ApplicationId, operation_byte: u8) -> OperationReceiptFact<'static> {
    OperationReceiptFact {
        application_id,
        package_id: package_id(),
        package_version: 1,
        installation_generation: Generation::INITIAL,
        entry_name: ENTRY,
        artifact_id: ArtifactId::from_bytes([0x7A; 16]),
        payload_revision: 1,
        payload_digest: [0x7B; 32],
        payload_size_bytes: 8,
        operation_id: OperationId::from_bytes([operation_byte; 16]),
        operation_generation: Generation::INITIAL,
        callback_id: CallbackId::from_bytes([0x7C; 16]),
        outcome: CompletionOutcome::Completed {
            receipt_id: ReceiptId::from_bytes([0x7D; 16]),
        },
        terminal_state: OperationState::Completed {
            receipt_id: ReceiptId::from_bytes([0x7D; 16]),
        },
    }
}

#[test]
fn bridge_success_charges_the_budget_exactly_once() {
    let (dir, runtime, application_id) = installed_runtime("charge-once");

    // Before the first run the full budget is observable — the read-only
    // surface replays the writer's deterministic root issuance and reads
    // the unspent call limit.
    assert_eq!(
        runtime
            .semantic_writer_budget(application_id)
            .expect("budget before first run"),
        Some(SEMANTIC_WRITER_ROOT_CALL_LIMIT)
    );

    let execution = execute_application_payload(&runtime, package_id(), ENTRY)
        .expect("execute the installed executable payload");
    assert!(!execution.semantic_replayed);

    // Exactly one charge: the budget dropped by one, one durable
    // consumption row exists for the application's root capability, and
    // the charged event is in the semantic store.
    assert_eq!(
        runtime
            .semantic_writer_budget(application_id)
            .expect("budget after first run"),
        Some(SEMANTIC_WRITER_ROOT_CALL_LIMIT - 1)
    );
    let capability = runtime
        .semantic_writer()
        .application_capability(&runtime, application_id)
        .expect("root capability replays");
    assert_eq!(capability.call_limit, Some(SEMANTIC_WRITER_ROOT_CALL_LIMIT));
    assert_eq!(
        consumption_rows(dir.root(), capability.handle.capability_id),
        1,
        "one bridged admission must write exactly one consumption row"
    );
    assert!(
        runtime
            .semantic()
            .inspect_event(execution.semantic_event_id)
            .is_ok(),
        "the charged admission's event is in the store"
    );
    drop(runtime);
}

#[test]
fn rerun_and_reopen_replays_charge_nothing() {
    let (dir, runtime, application_id) = installed_execution("replay-free");
    let capability = runtime
        .semantic_writer()
        .application_capability(&runtime, application_id)
        .expect("root capability");
    assert_eq!(
        consumption_rows(dir.root(), capability.handle.capability_id),
        1
    );

    // An exact re-run replays the same event AND the same consumption
    // receipt: zero new rows, budget unchanged.
    let second = execute_application_payload(&runtime, package_id(), ENTRY)
        .expect("exact re-execution replays");
    assert!(second.semantic_replayed);
    assert_eq!(
        consumption_rows(dir.root(), capability.handle.capability_id),
        1,
        "a replayed run must not charge again"
    );
    assert_eq!(
        runtime
            .semantic_writer_budget(application_id)
            .expect("budget after replay"),
        Some(SEMANTIC_WRITER_ROOT_CALL_LIMIT - 1)
    );

    // Crash-recovery shape: drop and reopen the same root, re-run — the
    // reopened writer derives the identical consume key, so still zero
    // new rows.
    drop(runtime);
    let reopened = SliceKRuntime::open(dir.root()).expect("reopen the same root");
    let third = execute_application_payload(&reopened, package_id(), ENTRY)
        .expect("re-execution after reopen replays");
    assert!(third.semantic_replayed);
    assert_eq!(
        consumption_rows(dir.root(), capability.handle.capability_id),
        1,
        "a reopened re-run must not charge again"
    );
    assert_eq!(
        reopened
            .semantic_writer_budget(application_id)
            .expect("budget after reopen replay"),
        Some(SEMANTIC_WRITER_ROOT_CALL_LIMIT - 1)
    );
}

#[test]
fn exhausted_budget_fails_closed_typed_with_zero_semantic_writes() {
    let (dir, runtime, application_id) = installed_runtime("exhausted");
    // A dedicated writer with a 2-charge budget over the same runtime and
    // application namespace: the documented test-injection surface of
    // [`SemanticWriterKey`] (production roots always carry
    // [`SEMANTIC_WRITER_ROOT_CALL_LIMIT`]).
    let key = SemanticWriterKey::from_seed_with_root_call_limit_for_tests([0x75; 32], 2);
    let writer = SemanticWriter::open_with_key(&runtime, &key).expect("injected writer");
    let handle = writer
        .application_capability(&runtime, application_id)
        .expect("injected root capability")
        .handle;
    assert_eq!(
        runtime.capability().call_limit_remaining(handle).unwrap(),
        Some(2)
    );

    let first = writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xA1))
        .expect("first charge admits");
    assert!(!first.replayed);
    let second = writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xB2))
        .expect("second charge admits");
    assert!(!second.replayed);
    assert_eq!(second.log_seq, first.log_seq + 1);
    assert_eq!(
        runtime.capability().call_limit_remaining(handle).unwrap(),
        Some(0),
        "the exhausted budget is queryable at zero"
    );

    // The third distinct operation under the same capability fails closed
    // with the typed exhaustion error — before any semantic write.
    let refusal = writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xC3))
        .expect_err("exhausted budget must fail the bridge closed");
    assert!(
        matches!(
            &refusal,
            SliceKError::Capability(CapabilityAuthorityError::CallLimitExhausted)
        ),
        "expected the typed CallLimitExhausted refusal, got: {refusal}"
    );
    assert_eq!(
        consumption_rows(dir.root(), handle.capability_id),
        2,
        "the refused third charge must leave the ledger at two rows"
    );

    // Zero partial state in the semantic store: the log position did not
    // move for the refused operation — the next admission (under the
    // production writer's separate full budget, same namespace) lands
    // exactly one past the second, never one past a phantom third.
    let production = runtime
        .semantic_writer()
        .append_operation_receipt(&runtime, &fact(application_id, 0xD4))
        .expect("the production writer's budget is unaffected");
    assert!(!production.replayed);
    assert_eq!(
        production.log_seq,
        second.log_seq + 1,
        "the refused append must have written no semantic event"
    );

    // A retry of an already-charged operation stays free at zero budget:
    // the consume replays under the same operation-tuple key, the append
    // replays under the same event id.
    let retry = writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xB2))
        .expect("retry after exhaustion replays free");
    assert!(retry.replayed);
    assert_eq!(retry.log_seq, second.log_seq);
    assert_eq!(
        consumption_rows(dir.root(), handle.capability_id),
        2,
        "a replayed retry must not charge again"
    );
    assert_eq!(
        runtime.capability().call_limit_remaining(handle).unwrap(),
        Some(0)
    );
}
