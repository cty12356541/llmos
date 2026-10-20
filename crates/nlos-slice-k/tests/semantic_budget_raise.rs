//! W53-B budget-raise tests: an exhausted per-application budget is
//! restored by `raise_semantic_writer_budget`'s revoke-and-reissue
//! orchestration — the bridge recovers on the fresh root (a new budget of
//! exactly `new_limit`, drawn down by subsequent admissions), the same
//! raise replays as a typed no-op, the revoked old root's handle stays
//! generation-fenced while the bridge keeps admitting through the active
//! root, a decrease is an ordinary raise, the mid-raise window (revoke
//! committed, issue never ran) fails the bridge and budget closed until a
//! raise converges it, and a raise survives reopen through the durable
//! writer-root registry.
//!
//! Fixture discipline mirrors `semantic_consume_ledger.rs` (one package
//! identity, single `executable` entry, injected small-budget writers via
//! the documented `SemanticWriterKey` test surface); this file's
//! idempotency/clock band is `0x76 + 70..77`, disjoint from every
//! documented slice-k helper band.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::Signer;
use nlos_artifact::{
    ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageManifest, PackageManifestEntry,
    ProvenanceSourceTriple, PutRevisionRequest, SignedPackage, VerifyPackageRequest,
    derive_artifact_id, package_manifest_message,
};
use nlos_capability::{
    CapabilityAuthorityError, RevokeCapabilityRequest, SignedRevokeCapabilityRequest,
    revoke_command_message,
};
use nlos_operation::{CompletionOutcome, OperationState};
use nlos_slice_k::{
    OperationReceiptFact, SEMANTIC_WRITER_ROOT_CALL_LIMIT, SemanticWriter, SemanticWriterKey,
    SliceKError, SliceKRuntime, seeded_key,
};
use nlos_types::{
    ApplicationId, ArtifactId, CallbackId, Generation, OperationId, PackageId, ReceiptId,
};

const PACKAGE_BYTES: [u8; 16] = [
    0x71, 0x0b, 0x3e, 0xd8, 0x49, 0xa2, 0x66, 0x1f, 0x9c, 0x03, 0xb7, 0x5d, 0xe4, 0x88, 0x12, 0xaf,
];
const ENTRY: &str = "sample-driver";
const SEED: u8 = 0x76;
/// The deterministic seed of every injected small-budget writer in this
/// file (the writer whose raises these tests drive).
const WRITER_SEED: [u8; 32] = [0x76; 32];

const PAYLOAD: &[u8] = b"budget raise payload - executable bytes v1\n";

struct TempDir {
    root: std::path::PathBuf,
}

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

impl TempDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-slice-k-raise-{label}-{}-{sequence}",
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
            Err(error) => panic!("remove raise temp root: {error}"),
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
        .wall_now_ms(seeded_key(SEED, 70))
        .expect("entry publish clock");
    runtime
        .artifacts
        .create_artifact(CreateArtifactSpec {
            artifact_id,
            idempotency_key: seeded_key(SEED, 71),
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
                idempotency_key: seeded_key(SEED, 72),
                verified_at_ms: runtime
                    .wall_now_ms(seeded_key(SEED, 73))
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

/// An injected writer with a small root budget over the same runtime and
/// application namespace (the documented `SemanticWriterKey` test surface;
/// production roots always carry `SEMANTIC_WRITER_ROOT_CALL_LIMIT`).
fn small_writer(runtime: &SliceKRuntime, limit: u64) -> SemanticWriter {
    SemanticWriter::open_with_key(
        runtime,
        &SemanticWriterKey::from_seed_with_root_call_limit_for_tests(WRITER_SEED, limit),
    )
    .expect("injected writer")
}

/// A fabricated terminal-receipt fact (same shape as
/// `semantic_consume_ledger.rs`: the bridge's gates are writer- and
/// derivation-sourced, so driving the budget needs no payload lane).
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
fn exhausted_budget_raise_restores_the_bridge_with_the_raised_budget() {
    let (_dir, runtime, application_id) = installed_runtime("raise");
    let writer = small_writer(&runtime, 2);

    let first = writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xA1))
        .expect("first charge admits");
    let second = writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xB2))
        .expect("second charge admits");
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
    let old_handle = writer
        .application_capability(&runtime, application_id)
        .expect("original root replays")
        .handle;

    // The raise: revoke the exhausted root, reissue the identical grant
    // with a 4-charge budget, register the new root.
    let raise = writer
        .raise_application_capability(&runtime, application_id, 4)
        .expect("raise the budget");
    assert!(raise.issued, "a fresh raise issues a new root");
    assert_eq!(raise.revoked, Some(old_handle));
    assert_eq!(raise.capability.call_limit, Some(4));
    assert_ne!(raise.capability.handle, old_handle);
    assert_eq!(
        runtime
            .capability()
            .call_limit_remaining(raise.capability.handle)
            .expect("new root remaining"),
        Some(4),
        "the new root carries a fresh budget of exactly new_limit"
    );

    // The bridge recovers immediately: the previously refused operation
    // admits now, the log continues past the last pre-raise admission, and
    // the fresh budget draws down as new admissions charge it.
    let third = writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xC3))
        .expect("the raised budget re-opens the bridge");
    assert!(!third.replayed);
    assert_eq!(third.log_seq, second.log_seq + 1);
    assert_eq!(first.log_seq + 2, third.log_seq);
    let fourth = writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xD4))
        .expect("fourth charge admits");
    assert_eq!(fourth.log_seq, third.log_seq + 1);
    let _fifth = writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xE5))
        .expect("fifth charge admits");
    let _sixth = writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xF6))
        .expect("sixth charge admits");
    assert_eq!(
        runtime
            .capability()
            .call_limit_remaining(raise.capability.handle)
            .expect("remaining after four charges"),
        Some(0)
    );
    let refusal = writer
        .append_operation_receipt(&runtime, &fact(application_id, 0x97))
        .expect_err("the raised budget exhausts again, fail-closed");
    assert!(
        matches!(
            &refusal,
            SliceKError::Capability(CapabilityAuthorityError::CallLimitExhausted)
        ),
        "expected the typed CallLimitExhausted refusal, got: {refusal}"
    );

    // Writer scoping: the runtime's own writer root for the same
    // application namespace is untouched by the injected writer's raise.
    assert_eq!(
        runtime
            .semantic_writer_budget(application_id)
            .expect("runtime writer budget is unaffected"),
        Some(SEMANTIC_WRITER_ROOT_CALL_LIMIT)
    );
}

#[test]
fn same_limit_raise_replays_as_a_typed_noop() {
    let (_dir, runtime, application_id) = installed_runtime("replay");
    let writer = small_writer(&runtime, 2);

    let raise = writer
        .raise_application_capability(&runtime, application_id, 4)
        .expect("raise to 4");
    assert!(raise.issued);
    let raised_handle = raise.capability.handle;
    assert_eq!(
        runtime
            .capability()
            .call_limit_remaining(raised_handle)
            .expect("remaining"),
        Some(4)
    );
    writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xA1))
        .expect("one charge draws the raised budget down");
    assert_eq!(
        runtime
            .capability()
            .call_limit_remaining(raised_handle)
            .expect("remaining after one charge"),
        Some(3)
    );

    // Replaying the same raise is a typed no-op: the active root already
    // carries exactly new_limit, so nothing is revoked or issued, the
    // handle stays the same, and the drawn-down budget is not reset.
    let replay = writer
        .raise_application_capability(&runtime, application_id, 4)
        .expect("same-limit replay");
    assert!(!replay.issued);
    assert_eq!(replay.revoked, None);
    assert_eq!(replay.capability.handle, raised_handle);
    assert_eq!(replay.capability.call_limit, Some(4));
    assert_eq!(
        runtime
            .capability()
            .call_limit_remaining(raised_handle)
            .expect("remaining is unchanged by the replay"),
        Some(3)
    );

    // The bridge keeps admitting on the same root after the replay.
    let second = writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xB2))
        .expect("admission after replay");
    assert!(!second.replayed);
    assert_eq!(
        runtime
            .capability()
            .call_limit_remaining(raised_handle)
            .expect("remaining after second charge"),
        Some(2)
    );
}

#[test]
fn old_root_handle_is_generation_fenced_but_the_bridge_is_unaffected() {
    let (_dir, runtime, application_id) = installed_runtime("fence");
    let writer = small_writer(&runtime, 2);
    writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xA1))
        .expect("first charge");
    writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xB2))
        .expect("second charge");
    let old_handle = writer
        .application_capability(&runtime, application_id)
        .expect("original root")
        .handle;

    let raise = writer
        .raise_application_capability(&runtime, application_id, 4)
        .expect("raise");
    assert_eq!(raise.revoked, Some(old_handle));

    // The revoked root's stale handle fails closed on the authority's
    // generation fence: neither liveness inspection nor budget readback
    // can exercise it anymore.
    let at_ms = runtime
        .wall_now_ms(seeded_key(SEED, 74))
        .expect("probe clock");
    let fence = runtime
        .capability()
        .inspect_active(old_handle, at_ms)
        .expect_err("stale handle must not inspect as active");
    assert!(
        matches!(fence, CapabilityAuthorityError::GenerationFenceConflict),
        "expected the generation fence, got: {fence}"
    );
    let fence = runtime
        .capability()
        .call_limit_remaining(old_handle)
        .expect_err("stale handle must not read a budget");
    assert!(
        matches!(fence, CapabilityAuthorityError::GenerationFenceConflict),
        "expected the generation fence, got: {fence}"
    );

    // The bridge resolves the ACTIVE root, so it never steps on the stale
    // handle: the very next admission goes through on the new root.
    let third = writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xC3))
        .expect("the bridge is unaffected by the stale handle");
    assert!(!third.replayed);
    assert_eq!(
        runtime
            .capability()
            .call_limit_remaining(raise.capability.handle)
            .expect("new root remaining"),
        Some(3)
    );
}

#[test]
fn raise_can_decrease_the_budget() {
    let (_dir, runtime, application_id) = installed_runtime("decrease");
    let writer = small_writer(&runtime, 4);
    writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xA1))
        .expect("one charge of the 4-budget root");
    let old_handle = writer
        .application_capability(&runtime, application_id)
        .expect("original root")
        .handle;

    // A decrease is an operations decision, not a refusal: the new root
    // carries the smaller limit as its own fresh budget.
    let raise = writer
        .raise_application_capability(&runtime, application_id, 2)
        .expect("decrease the budget");
    assert!(raise.issued);
    assert_eq!(raise.revoked, Some(old_handle));
    assert_eq!(raise.capability.call_limit, Some(2));
    assert_eq!(
        runtime
            .capability()
            .call_limit_remaining(raise.capability.handle)
            .expect("decreased remaining"),
        Some(2)
    );

    writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xB2))
        .expect("first charge of the decreased root");
    writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xC3))
        .expect("second charge exhausts the decreased root");
    let refusal = writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xD4))
        .expect_err("the decreased budget fails the bridge closed");
    assert!(
        matches!(
            &refusal,
            SliceKError::Capability(CapabilityAuthorityError::CallLimitExhausted)
        ),
        "expected the typed CallLimitExhausted refusal, got: {refusal}"
    );
}

#[test]
fn midwindow_no_active_root_fails_closed_and_recovers() {
    let (_dir, runtime, application_id) = installed_runtime("window");
    let writer = small_writer(&runtime, 2);
    writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xA1))
        .expect("one charge");
    let handle = writer
        .application_capability(&runtime, application_id)
        .expect("current root")
        .handle;

    // Hand-construct the mid-raise window through the authority's public
    // revocation surface, signing with the injected writer's own seed: the
    // revoke half of a raise committed, the issue half never ran.
    let writer_key = ed25519_dalek::SigningKey::from_bytes(&WRITER_SEED);
    let revoke = RevokeCapabilityRequest {
        handle,
        revoker_key_id: writer.key_id(),
        idempotency_key: seeded_key(SEED, 75),
        revoked_at_ms: runtime
            .wall_now_ms(seeded_key(SEED, 76))
            .expect("window revocation clock"),
    };
    runtime
        .capability()
        .revoke_signed(
            &runtime.identity,
            SignedRevokeCapabilityRequest {
                command: revoke,
                signer: writer.principal_id(),
                signature: writer_key.sign(&revoke_command_message(revoke)).to_bytes(),
            },
        )
        .expect("hand revoke");

    // No active root: the bridge fails closed on the generation fence
    // before any semantic write, and the active-root resolution refuses
    // typed instead of returning a dead handle.
    let refusal = writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xB2))
        .expect_err("the window must fail the bridge closed");
    assert!(
        matches!(
            &refusal,
            SliceKError::Capability(CapabilityAuthorityError::GenerationFenceConflict)
        ),
        "expected the typed generation-fence refusal, got: {refusal}"
    );
    let refusal = writer
        .active_application_capability(&runtime, application_id)
        .expect_err("no active root resolves");
    assert!(
        matches!(
            &refusal,
            SliceKError::Capability(CapabilityAuthorityError::GenerationFenceConflict)
        ),
        "expected the typed generation-fence refusal, got: {refusal}"
    );

    // A raise converges out of the window: with nothing active there is
    // nothing to revoke, the issue proceeds, and the bridge recovers.
    let raise = writer
        .raise_application_capability(&runtime, application_id, 4)
        .expect("raise out of the window");
    assert!(raise.issued);
    assert_eq!(raise.revoked, None, "nothing was revoked by the recovery");
    assert_eq!(raise.capability.call_limit, Some(4));
    let second = writer
        .append_operation_receipt(&runtime, &fact(application_id, 0xB2))
        .expect("the bridge recovers out of the window");
    assert!(!second.replayed);
    assert_eq!(
        runtime
            .capability()
            .call_limit_remaining(raise.capability.handle)
            .expect("recovered remaining"),
        Some(3)
    );
}

#[test]
fn runtime_raise_persists_across_reopen() {
    let (dir, runtime, application_id) = installed_runtime("reopen");
    assert_eq!(
        runtime
            .semantic_writer_budget(application_id)
            .expect("production budget before raise"),
        Some(SEMANTIC_WRITER_ROOT_CALL_LIMIT)
    );

    // The runtime-level API raises the runtime's own writer root.
    let raise = runtime
        .raise_semantic_writer_budget(application_id, 5)
        .expect("runtime raise");
    assert!(raise.issued);
    assert_eq!(raise.capability.call_limit, Some(5));
    assert_eq!(
        runtime
            .semantic_writer_budget(application_id)
            .expect("raised budget readback"),
        Some(5)
    );
    assert!(
        dir.root().join("semantic-writer-roots").is_file(),
        "the raise registry file must exist after a raise"
    );
    drop(runtime);

    // Reopen resolves the raised root through the durable registry: the
    // budget, the bridge, and the same-limit replay all converge on it.
    let reopened = SliceKRuntime::open(dir.root()).expect("reopen the same root");
    assert_eq!(
        reopened
            .semantic_writer_budget(application_id)
            .expect("raised budget survives reopen"),
        Some(5)
    );
    let first = reopened
        .semantic_writer()
        .append_operation_receipt(&reopened, &fact(application_id, 0xA1))
        .expect("bridge after reopen charges the raised root");
    assert!(!first.replayed);
    assert_eq!(
        reopened
            .semantic_writer_budget(application_id)
            .expect("budget after the reopened charge"),
        Some(4)
    );
    let replay = reopened
        .raise_semantic_writer_budget(application_id, 5)
        .expect("same-limit raise after reopen");
    assert!(!replay.issued, "the reopened raise replays as a no-op");
    assert_eq!(replay.capability.handle, raise.capability.handle);
    assert_eq!(
        reopened
            .semantic_writer_budget(application_id)
            .expect("budget unchanged by the replay"),
        Some(4)
    );
}
