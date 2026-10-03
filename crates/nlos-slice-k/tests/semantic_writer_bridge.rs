//! W49 semantic write-bridge tests: the payload lane's terminal operation
//! receipt becomes exactly one admitted Semantic assertion under the
//! runtime's dedicated writer principal — queryable, replay-exact, guarded
//! by the root capability's `authorize_semantic`, and fail-closed typed
//! when the capability is revoked or the writer key file is corrupt. The
//! W51 consume-ledger wiring (one budget charge per admission, exhaustion
//! fail-closed) has its own file: `semantic_consume_ledger.rs`.
//!
//! Fixture discipline mirrors `payload_execution.rs` (one package identity,
//! single `executable` entry); this file's idempotency/clock band is
//! `0x73 + 50..57` (reinstall band `0x74`), disjoint from every documented
//! slice-k helper band.

use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::Signer;
use nlos_artifact::{
    ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageManifest, PackageManifestEntry,
    ProvenanceSourceTriple, PutRevisionRequest, SignedPackage, VerifyPackageRequest,
    derive_artifact_id, package_manifest_message,
};
use nlos_capability::{
    CapabilityAuthorityError, CapabilityRights, CapabilityTarget, RevokeCapabilityRequest,
    SignedRevokeCapabilityRequest, revoke_command_message,
};
use nlos_semantic::AdmissionDurability;
use nlos_slice_k::{
    PayloadExecution, Publisher, SliceKError, SliceKRuntime, application_namespace,
    execute_application_payload, seeded_key,
};
use nlos_types::{ArtifactId, PackageId, ReceiptId};

const PACKAGE_BYTES: [u8; 16] = [
    0x4d, 0x21, 0xc8, 0x5f, 0x6a, 0x93, 0x77, 0x0e, 0xb2, 0xd4, 0x41, 0x9f, 0x08, 0xce, 0x63, 0x2d,
];
const ENTRY: &str = "sample-driver";
const SEED: u8 = 0x73;
const SEED_REINSTALL: u8 = 0x74;

const PAYLOAD_A: &[u8] = b"semantic bridge payload A - executable bytes v1\n";
const PAYLOAD_B: &[u8] = b"semantic bridge payload B - executable bytes v2\n";

struct TempDir {
    root: std::path::PathBuf,
}

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

impl TempDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-slice-k-semantic-{label}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create temp root");
        Self { root }
    }

    fn root(&self) -> &std::path::Path {
        &self.root
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove semantic temp root: {error}"),
        }
    }
}

fn package_id() -> PackageId {
    PackageId::from_bytes(PACKAGE_BYTES)
}

fn verified_payload_package(
    runtime: &SliceKRuntime,
    signer: &Publisher,
    version: u64,
    payload: &[u8],
    key_offset: u8,
    clock_offset: u8,
) -> ReceiptId {
    let artifact_id = ArtifactId::from_bytes(derive_artifact_id(package_id(), version, ENTRY));
    let at_ms = runtime
        .wall_now_ms(seeded_key(SEED, clock_offset))
        .expect("entry publish clock");
    runtime
        .artifacts
        .create_artifact(CreateArtifactSpec {
            artifact_id,
            idempotency_key: seeded_key(SEED, key_offset),
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
            bytes: payload,
            created_at_ms: at_ms,
            provenance: ProvenanceSourceTriple {
                source_a: *artifact_id.as_bytes(),
                source_b: PACKAGE_BYTES,
                source_digest: ContentDigest::of_bytes(payload),
            },
        })
        .expect("put entry revision");
    let manifest = PackageManifest {
        package_id: package_id(),
        version,
        entries: vec![PackageManifestEntry {
            name: ENTRY.to_string(),
            artifact_id,
            digest: ContentDigest::of_bytes(payload),
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
    let decision = runtime
        .artifacts
        .verify_package(
            &runtime.identity,
            VerifyPackageRequest {
                signed: &envelope,
                idempotency_key: seeded_key(SEED, key_offset + 1),
                verified_at_ms: runtime
                    .wall_now_ms(seeded_key(SEED, clock_offset + 1))
                    .expect("verify clock"),
            },
        )
        .expect("verify payload package");
    decision.receipt().receipt_id
}

fn installed_execution(label: &str) -> (TempDir, SliceKRuntime, PayloadExecution) {
    let dir = TempDir::new(label);
    let runtime = SliceKRuntime::open(dir.root()).expect("open runtime");
    let signer = runtime.bootstrap_publisher(SEED).expect("publisher");
    let receipt = verified_payload_package(&runtime, &signer, 1, PAYLOAD_A, 50, 54);
    runtime
        .install_verified_package_by_id(receipt, SEED)
        .expect("install");
    let execution = execute_application_payload(&runtime, package_id(), ENTRY)
        .expect("execute the installed executable payload");
    (dir, runtime, execution)
}

#[test]
fn payload_execution_appends_one_queryable_semantic_assertion() {
    let (_dir, runtime, execution) = installed_execution("queryable");

    assert!(!execution.semantic_replayed);
    assert_ne!(
        execution.semantic_event_id.as_bytes(),
        &[0_u8; 32],
        "event id must be non-zero"
    );

    // The semantic library resolves the event: issuer is the runtime's
    // writer principal, scope is the application's policy namespace.
    let record = runtime
        .semantic()
        .inspect_event(execution.semantic_event_id)
        .expect("admitted event is inspectable");
    assert_eq!(record.issuer, runtime.semantic_writer().principal_id());
    assert_eq!(
        record.scope,
        CapabilityTarget::Namespace(application_namespace(execution.application_id))
    );
    assert_eq!(record.log_seq, execution.semantic_log_seq);

    // The admission receipt is durable and binds to the same event.
    let admission = runtime
        .semantic()
        .inspect_admission_receipt(execution.semantic_event_id)
        .expect("admission receipt is readable");
    assert_eq!(admission.event_id, execution.semantic_event_id);
    assert_eq!(
        admission.receipt_id,
        execution.semantic_admission_receipt_id
    );
    assert!(matches!(admission.durability, AdmissionDurability::Durable));

    // Every admit wrote one semantic_outbox row; nothing acknowledges it
    // here because this fixture runs no semantic-stream pump (the W50
    // consumer lane has its own tests in `semantic_stream.rs`).
    let outbox = runtime
        .semantic()
        .inspect_outbox(execution.semantic_event_id)
        .expect("outbox row exists per admit");
    assert_eq!(outbox.log_seq, execution.semantic_log_seq);
    assert_eq!(outbox.receipt_id, execution.semantic_admission_receipt_id);
    assert!(outbox.acknowledged_at_ms.is_none());

    // The root capability authorizes the namespace: SEMANTIC_APPEND only,
    // the finite W51 per-application budget (charged once per bridged
    // admission by the consume ledger), inspectable as active at admission
    // time — the authorize_semantic gate the admission above already
    // exercised for real.
    let writer = runtime.semantic_writer();
    let capability = writer
        .application_capability(&runtime, execution.application_id)
        .expect("root capability replays");
    assert!(
        capability
            .rights
            .contains(CapabilityRights::SEMANTIC_APPEND)
    );
    assert_eq!(
        capability.call_limit,
        Some(nlos_slice_k::SEMANTIC_WRITER_ROOT_CALL_LIMIT)
    );
    assert_eq!(capability.holder, writer.principal_id());
    assert_eq!(capability.issuer, writer.principal_id());
    assert_eq!(
        capability.target,
        CapabilityTarget::Namespace(application_namespace(execution.application_id))
    );
    runtime
        .capability()
        .inspect_active(capability.handle, admission.admitted_at_ms)
        .expect("root capability is active at admission time");
}

#[test]
fn exact_reexecution_replays_the_semantic_assertion_zero_new_events() {
    let (_dir, runtime, first) = installed_execution("replay");

    let second = execute_application_payload(&runtime, package_id(), ENTRY)
        .expect("re-execute the same payload");

    assert!(second.semantic_replayed);
    assert_eq!(second.semantic_event_id, first.semantic_event_id);
    assert_eq!(
        second.semantic_admission_receipt_id,
        first.semantic_admission_receipt_id
    );
    // Same EventId admitted once: the append-only log sequence cannot move.
    assert_eq!(second.semantic_log_seq, first.semantic_log_seq);
}

#[test]
fn fresh_installation_generation_admits_a_distinct_assertion() {
    let (_dir, runtime, first) = installed_execution("generation");

    let signer = runtime.bootstrap_publisher(SEED).expect("publisher");
    let receipt_v2 = verified_payload_package(&runtime, &signer, 2, PAYLOAD_B, 52, 54);
    let installation_v2 = runtime
        .install_verified_package_by_id(receipt_v2, SEED_REINSTALL)
        .expect("reinstall at version 2");
    assert_eq!(installation_v2.installation_generation.get(), 2);

    let second = execute_application_payload(&runtime, package_id(), ENTRY)
        .expect("execute the new generation's payload");

    assert!(!second.semantic_replayed);
    assert_ne!(second.semantic_event_id, first.semantic_event_id);
    assert_eq!(second.semantic_log_seq, first.semantic_log_seq + 1);
}

#[test]
fn writer_key_file_is_owner_only_and_reopen_is_idempotent() {
    let dir = TempDir::new("keyfile");
    let runtime = SliceKRuntime::open(dir.root()).expect("open runtime");
    let key_path = dir.root().join("keys").join("semantic-writer.key");
    assert!(key_path.is_file(), "writer key file must exist after open");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key_path)
            .expect("key file metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "writer key file must be 0600");
    }
    let public_key = runtime.semantic_writer().public_key();
    let principal = runtime.semantic_writer().principal_id();

    // Publish + install + execute so the reopen replays a real bridge side.
    let signer = runtime.bootstrap_publisher(SEED).expect("publisher");
    let receipt = verified_payload_package(&runtime, &signer, 1, PAYLOAD_A, 50, 54);
    runtime
        .install_verified_package_by_id(receipt, SEED)
        .expect("install");
    let first =
        execute_application_payload(&runtime, package_id(), ENTRY).expect("execute before reopen");
    drop(runtime);

    let reopened = SliceKRuntime::open(dir.root()).expect("reopen the same root");
    assert_eq!(reopened.semantic_writer().public_key(), public_key);
    assert_eq!(reopened.semantic_writer().principal_id(), principal);

    // The reopened writer still bridges: an exact re-execution replays the
    // identical semantic event (no second admission, no new log sequence).
    let second =
        execute_application_payload(&reopened, package_id(), ENTRY).expect("execute after reopen");
    assert!(second.semantic_replayed);
    assert_eq!(second.semantic_event_id, first.semantic_event_id);
    assert_eq!(second.semantic_log_seq, first.semantic_log_seq);
}

#[test]
fn corrupt_writer_key_file_fails_typed_at_open() {
    let dir = TempDir::new("corrupt");
    let keys = dir.root().join("keys");
    std::fs::create_dir_all(&keys).expect("keys dir");
    std::fs::write(keys.join("semantic-writer.key"), [0_u8; 47]).expect("bad key file");

    let Err(refusal) = SliceKRuntime::open(dir.root()) else {
        panic!("corrupt key file must refuse to open")
    };
    assert!(
        matches!(refusal, SliceKError::SemanticWriter(_)),
        "expected a typed semantic-writer refusal, got: {refusal}"
    );
}

#[test]
fn revoked_root_capability_fails_the_bridge_closed_typed() {
    let (dir, runtime, execution) = installed_execution("revoked");

    // The test owns the temp root, so it can reconstruct the writer key
    // from the documented key-file layout and drive the capability
    // authority's public revocation surface as the issuer/holder.
    let key_bytes = std::fs::read(dir.root().join("keys").join("semantic-writer.key"))
        .expect("read writer key file");
    let mut seed = [0_u8; 32];
    seed.copy_from_slice(&key_bytes[..32]);
    let writer_key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let handle = runtime
        .semantic_writer()
        .application_capability(&runtime, execution.application_id)
        .expect("capability before revocation")
        .handle;
    let revoke = RevokeCapabilityRequest {
        handle,
        revoker_key_id: runtime.semantic_writer().key_id(),
        idempotency_key: seeded_key(SEED, 56),
        revoked_at_ms: runtime
            .wall_now_ms(seeded_key(SEED, 57))
            .expect("revocation clock"),
    };
    runtime
        .capability()
        .revoke_signed(
            &runtime.identity,
            SignedRevokeCapabilityRequest {
                command: revoke,
                signer: runtime.semantic_writer().principal_id(),
                signature: writer_key.sign(&revoke_command_message(revoke)).to_bytes(),
            },
        )
        .expect("revoke the root capability");

    // A fresh operation under the same namespace must fail closed: the
    // run is not reported as executed without its semantic record. Since
    // W51 the refusal fires at the consume-before-append charge (the
    // same admission gates, one step earlier than the append's own
    // `authorize_semantic`). The revocation advanced the capability's
    // current generation, so the replayed issuance's handle is stale and
    // the typed refusal is the capability authority's generation fence.
    let signer = runtime.bootstrap_publisher(SEED).expect("publisher");
    let receipt_v2 = verified_payload_package(&runtime, &signer, 2, PAYLOAD_B, 52, 54);
    runtime
        .install_verified_package_by_id(receipt_v2, SEED_REINSTALL)
        .expect("reinstall at version 2");
    let refusal = execute_application_payload(&runtime, package_id(), ENTRY)
        .expect_err("revoked capability must fail the bridge closed");
    assert!(
        matches!(
            &refusal,
            SliceKError::Capability(CapabilityAuthorityError::GenerationFenceConflict)
        ),
        "expected a typed capability refusal through the consume charge, got: {refusal}"
    );
}
