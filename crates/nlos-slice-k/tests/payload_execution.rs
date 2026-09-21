//! Kernel-side payload-execution lane (handover #2 first slice, W35-P2 /
//! `C-APP-PAYLOAD` 前片): an installed application's manifest-declared
//! `executable` entry is really executed through the public
//! `nlos-driver-mock` provider face — the payload bytes are read back
//! from the artifact authority, consumed as the completion seed, and the
//! terminal outcome plus every boundary receipt is durable and
//! inspectable. This file closes W33-H §2 boundary 1 from the kernel
//! side; no runtime beyond the existing driver face is invented here.
//!
//! Fixture discipline: one package identity whose single manifest entry
//! `sample-driver` carries role `executable`; the entry artifact id is
//! the public `derive_artifact_id(package, version, name)` derivation,
//! so the lane resolves exactly the bytes the signed manifest bound.
//! Idempotency/clock keys live in the `0x71 + 40..47` band, disjoint
//! from every slice-k helper band (`+3`, `+10..22`, `+30/31`,
//! `+110..114`).

use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::Signer;
use nlos_artifact::{
    ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageManifest, PackageManifestEntry,
    ProvenanceSourceTriple, PutRevisionRequest, SignedPackage, VerifyPackageRequest,
    derive_artifact_id, package_manifest_message,
};
use nlos_driver_mock::ProviderError;
use nlos_operation::{CompletionOutcome, OperationError};
use nlos_slice_k::{
    PayloadExecution, Publisher, SliceKError, SliceKRuntime, execute_application_payload,
    payload_execution_seed, seeded_key,
};
use nlos_store::StoreError;
use nlos_types::{ArtifactId, PackageId, ReceiptId};

const PACKAGE_BYTES: [u8; 16] = [
    0x3b, 0x7f, 0x16, 0x2e, 0x8a, 0x44, 0xc9, 0x0d, 0x51, 0xf6, 0x83, 0x2c, 0xb0, 0x97, 0xe4, 0x58,
];
const ENTRY: &str = "sample-driver";
/// Fixture seed band: bootstrap/verify/install helpers consume
/// `0x71 + 3/13/14/15/16/21/22` internally; this file's own keys live at
/// `0x71 + 40..47` and collide with nothing.
const SEED: u8 = 0x71;
/// Second install band: slice-k's install helper derives its idempotency
/// key from the seed alone, so the version-2 reinstall of the same
/// package identity must come from a disjoint band (keys `[0x82; 16]`
/// etc., disjoint from every `0x71 + n` band above).
const SEED_REINSTALL: u8 = 0x72;

const PAYLOAD_A: &[u8] = b"sample payload A - executable bytes v1\n";
const PAYLOAD_B: &[u8] = b"sample payload B - executable bytes, different content\n";

struct TempDir {
    root: std::path::PathBuf,
}

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

impl TempDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-slice-k-payload-{label}-{}-{sequence}",
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
            Err(error) => panic!("remove payload temp root: {error}"),
        }
    }
}

fn package_id() -> PackageId {
    PackageId::from_bytes(PACKAGE_BYTES)
}

/// Materializes the executable entry artifact and verifies a one-entry
/// (no tasks) package bound to it, returning the durable verification
/// receipt id — the same public pipeline the CLI verify half rides.
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
    let receipt = verified_payload_package(&runtime, &signer, 1, PAYLOAD_A, 40, 44);
    runtime
        .install_verified_package_by_id(receipt, SEED)
        .expect("install");
    let execution = execute_application_payload(&runtime, package_id(), ENTRY)
        .expect("execute the installed executable payload");
    (dir, runtime, execution)
}

fn expected_outcome(execution: &PayloadExecution, payload: &[u8]) -> CompletionOutcome {
    nlos_driver_mock::derive_provider_outcome(
        execution.operation,
        execution.callback_id,
        &payload_execution_seed(payload),
    )
}

#[test]
fn executable_payload_executes_through_driver_face_with_real_receipts() {
    let (_dir, runtime, execution) = installed_execution("receipts");

    assert!(!execution.register_replayed);
    assert!(!execution.dispatch_replayed);
    assert!(!execution.complete_replayed);
    assert_eq!(execution.entry_name, ENTRY);
    assert_eq!(execution.installation_generation.get(), 1);
    assert_eq!(execution.payload_digest, ContentDigest::of_bytes(PAYLOAD_A));
    assert_eq!(
        execution.payload_size_bytes,
        u64::try_from(PAYLOAD_A.len()).expect("size fits")
    );
    assert_eq!(execution.payload_revision, 1);

    // The payload bytes were consumed: the terminal outcome is the
    // deterministic provider outcome under the payload-derived seed.
    assert_eq!(execution.outcome, expected_outcome(&execution, PAYLOAD_A));

    // Three distinct, non-zero boundary receipts from the authority.
    let zero = ReceiptId::from_bytes([0; 16]);
    assert_ne!(execution.admission_receipt_id, zero);
    assert_ne!(execution.preparation_receipt_id, zero);
    assert_ne!(execution.activation_receipt_id, zero);
    assert_ne!(
        execution.admission_receipt_id,
        execution.preparation_receipt_id
    );
    assert_ne!(
        execution.preparation_receipt_id,
        execution.activation_receipt_id
    );

    // The terminal state is durable and inspectable on the same store the
    // runtime's fiber lane uses.
    assert!(execution.terminal_state.is_terminal());
    let snapshot = runtime
        .operations
        .inspect(execution.operation)
        .expect("inspect executed operation");
    assert_eq!(snapshot.state, execution.terminal_state);
    assert_eq!(snapshot.handle, execution.operation);

    let report = execution.report_lines().join("\n");
    assert!(
        report.contains(ENTRY),
        "report must name the entry:\n{report}"
    );
    assert!(
        report.contains("admission="),
        "report must carry receipts:\n{report}"
    );
}

#[test]
fn exact_reexecution_replays_every_receipt() {
    let (_dir, runtime, first) = installed_execution("replay");

    let second = execute_application_payload(&runtime, package_id(), ENTRY)
        .expect("re-execute the same payload");

    assert!(second.register_replayed);
    assert!(second.dispatch_replayed);
    assert!(second.complete_replayed);
    assert_eq!(second.operation, first.operation);
    assert_eq!(second.callback_id, first.callback_id);
    assert_eq!(second.admission_receipt_id, first.admission_receipt_id);
    assert_eq!(second.preparation_receipt_id, first.preparation_receipt_id);
    assert_eq!(second.activation_receipt_id, first.activation_receipt_id);
    assert_eq!(second.outcome, first.outcome);
    assert_eq!(second.terminal_state, first.terminal_state);
}

#[test]
fn payload_bytes_select_the_terminal_outcome() {
    // Same package identity and version, different executable bytes on a
    // fresh durable state: the outcome (and its receipt) must differ —
    // the bytes are inputs of the execution, not decoration.
    let (_dir_a, _runtime_a, execution_a) = installed_execution("bytes-a");
    let dir_b = TempDir::new("bytes-b");
    let runtime_b = SliceKRuntime::open(dir_b.root()).expect("open runtime b");
    let signer_b = runtime_b.bootstrap_publisher(SEED).expect("publisher b");
    let receipt_b = verified_payload_package(&runtime_b, &signer_b, 1, PAYLOAD_B, 40, 44);
    runtime_b
        .install_verified_package_by_id(receipt_b, SEED)
        .expect("install b");
    let execution_b =
        execute_application_payload(&runtime_b, package_id(), ENTRY).expect("execute payload b");

    assert_ne!(execution_a.payload_digest, execution_b.payload_digest);
    assert_ne!(execution_a.outcome, execution_b.outcome);
    assert_eq!(
        execution_b.outcome,
        expected_outcome(&execution_b, PAYLOAD_B)
    );
}

#[test]
fn mutated_payload_under_terminal_callback_is_typed_rejected() {
    let (_dir, runtime, execution) = installed_execution("mutation");

    // Publish a different head revision for the same entry artifact —
    // the payload bytes an unaware caller would now read back differ.
    let mutated = b"mutated payload bytes, post-execution rewrite\n";
    runtime
        .artifacts
        .put_revision(PutRevisionRequest {
            artifact_id: execution.artifact_id,
            expected_head_revision: 1,
            bytes: mutated,
            created_at_ms: runtime
                .wall_now_ms(seeded_key(SEED, 46))
                .expect("mutation clock"),
            provenance: ProvenanceSourceTriple {
                source_a: *execution.artifact_id.as_bytes(),
                source_b: PACKAGE_BYTES,
                source_digest: ContentDigest::of_bytes(mutated),
            },
        })
        .expect("mutate entry head");

    let refusal = execute_application_payload(&runtime, package_id(), ENTRY)
        .expect_err("mutated bytes must not silently re-derive");
    assert!(
        matches!(
            &refusal,
            SliceKError::Driver(ProviderError::Store(StoreError::Operation(
                OperationError::CallbackIdentityConflict
            )))
        ),
        "expected a typed callback-identity conflict, got: {refusal}"
    );
}

#[test]
fn fresh_installation_generation_executes_the_new_payload() {
    let (_dir, runtime, first) = installed_execution("generation");

    // A same-identity package at version 2 with new executable bytes:
    // verify + reinstall advances the installation generation, and the
    // lane derives a fresh operation identity over the new bytes.
    let signer = runtime.bootstrap_publisher(SEED).expect("publisher");
    let receipt_v2 = verified_payload_package(&runtime, &signer, 2, PAYLOAD_B, 42, 44);
    let installation_v2 = runtime
        .install_verified_package_by_id(receipt_v2, SEED_REINSTALL)
        .expect("reinstall at version 2");
    assert_eq!(installation_v2.installation_generation.get(), 2);

    let second = execute_application_payload(&runtime, package_id(), ENTRY)
        .expect("execute the new generation's payload");

    assert_eq!(second.installation_generation.get(), 2);
    assert_eq!(second.package_version, 2);
    assert_ne!(second.operation, first.operation);
    assert_ne!(second.callback_id, first.callback_id);
    assert!(!second.register_replayed);
    assert!(!second.complete_replayed);
    assert_eq!(second.payload_digest, ContentDigest::of_bytes(PAYLOAD_B));
    assert_eq!(second.outcome, expected_outcome(&second, PAYLOAD_B));
}

#[test]
fn non_installed_states_refuse_typed() {
    let dir = TempDir::new("refusal");
    let runtime = SliceKRuntime::open(dir.root()).expect("open runtime");

    let unknown = execute_application_payload(&runtime, package_id(), ENTRY)
        .expect_err("unknown package must refuse");
    assert!(matches!(unknown, SliceKError::PayloadState(_)), "{unknown}");

    let signer = runtime.bootstrap_publisher(SEED).expect("publisher");
    let receipt = verified_payload_package(&runtime, &signer, 1, PAYLOAD_A, 40, 44);
    runtime
        .install_verified_package_by_id(receipt, SEED)
        .expect("install");
    runtime
        .uninstall_application(package_id(), SEED)
        .expect("uninstall");
    let uninstalled = execute_application_payload(&runtime, package_id(), ENTRY)
        .expect_err("uninstalled application must refuse");
    assert!(
        matches!(uninstalled, SliceKError::PayloadState(reason) if reason.contains("not installed")),
        "{uninstalled}"
    );
}
