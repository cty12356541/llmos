//! W35-P11 authority-backed application-lifecycle executor tests: the two
//! arms drive the real `nlos-application` authority (disable through the
//! ungated reversible mark; uninstall through the W27-D task-activity
//! gate), produce deterministic receipt ids derived from the authority's
//! own durable receipt facts, and refuse fail-closed on CAS mismatches,
//! absent applications, outstanding task activity, and out-of-domain input.

#![cfg(feature = "application")]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::Signer;
use nlos_application::{
    ApplicationAuthority, ApplicationStatus, InstallApplicationRequest, InstallDecision,
    RegisterBackgroundTaskRequest,
};
use nlos_artifact::{
    ArtifactStore, ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageManifest,
    PackageManifestEntry, ProvenanceSourceTriple, PutRevisionRequest, SignedPackage,
    VerifyPackageRequest, package_manifest_message,
};
use nlos_commit_coordinator::{RecoveryWorkerHealth, RecoveryWorkerState};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_schema::sabi::v1::SabiRequestContext;
use nlos_system_control::application_lifecycle_executor::ApplicationAuthorityLifecycleExecutor;
use nlos_system_control::control::{ControlCommand, ControlOutcome, dispatch_in_process};
use nlos_system_control::{
    ApplicationCommandExecutor, ApplicationControlRequest, RecoveryHealthSource,
    RecoverySystemControl, SystemControlAuthorizer,
};
use nlos_task::SqliteTaskAuthority;
use nlos_types::{ArtifactId, Generation, IdempotencyKey, PackageId, TaskId};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);
impl TempDir {
    fn new(label: &str) -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "nlos-sc-w35p11-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct StubHealth(RecoveryWorkerHealth);
impl RecoveryHealthSource for StubHealth {
    fn recovery_health(&self) -> RecoveryWorkerHealth {
        self.0.clone()
    }
}

struct CapabilityPolicy;
impl SystemControlAuthorizer for CapabilityPolicy {
    fn authorize_get(
        &self,
        _: &SabiRequestContext,
        _: &nlos_schema::sabi::v1::GetSystemControlRequest,
    ) -> Result<(), &'static str> {
        Ok(())
    }
    fn authorize_submit(
        &self,
        _: &SabiRequestContext,
        _: &nlos_schema::sabi::v1::ControlCommand,
    ) -> Result<(), &'static str> {
        Ok(())
    }
}

const MONOTONIC_NOW_NS: u64 = 10;
const WALL_NOW_MS: i64 = 6_000;

/// One installed application over a real artifact store, identity
/// authority, application authority, and task authority: a verified signed
/// package is built through the real `verify_package` path and installed
/// through the real `install_application` (generation 1, `installed`).
struct ApplicationFixture {
    _root: TempDir,
    applications: ApplicationAuthority,
    tasks: SqliteTaskAuthority,
    package_id: PackageId,
    principal: nlos_types::PrincipalId,
}

impl ApplicationFixture {
    fn new(label: &str, seed: u8) -> Self {
        let root = TempDir::new(label);
        let artifacts = ArtifactStore::open(root.0.join("artifacts")).unwrap();
        let identity = IdentityAuthority::open(root.0.join("identity")).unwrap();
        let key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        let bootstrap = identity
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
        let artifact_id = ArtifactId::from_bytes([0x30; 16]);
        let payload: &[u8] = b"payload-of-the-package";
        artifacts
            .create_artifact(CreateArtifactSpec {
                artifact_id,
                idempotency_key: IdempotencyKey::from_bytes([0xA0 + seed; 16]),
                content_type: "application/octet-stream".to_string(),
                application_id: None,
                owner: Some(format!("user-{seed}")),
                created_at_ms: 1_000,
            })
            .unwrap();
        artifacts
            .put_revision(PutRevisionRequest {
                artifact_id,
                expected_head_revision: 0,
                bytes: payload,
                created_at_ms: 5_000,
                provenance: ProvenanceSourceTriple {
                    source_a: [0xC0_u8.wrapping_add(seed); 16],
                    source_b: [0xD0_u8.wrapping_add(seed); 16],
                    source_digest: ContentDigest::from_bytes([0xE0_u8.wrapping_add(seed); 32]),
                },
            })
            .unwrap();
        let package_id = PackageId::from_bytes([seed; 16]);
        let manifest = PackageManifest {
            package_id,
            version: 1,
            entries: vec![PackageManifestEntry {
                name: "main".to_string(),
                artifact_id,
                digest: ContentDigest::of_bytes(payload),
                role: PackageEntryRole::Executable,
            }],
        };
        let signed = SignedPackage {
            manifest: manifest.clone(),
            signer: bootstrap.principal_id,
            signature: key.sign(&package_manifest_message(&manifest)).to_bytes(),
        };
        let verified = artifacts
            .verify_package(
                &identity,
                VerifyPackageRequest {
                    signed: &signed,
                    idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(4); 16]),
                    verified_at_ms: 1_000,
                },
            )
            .unwrap();
        let applications = ApplicationAuthority::open(root.0.join("application")).unwrap();
        let tasks = SqliteTaskAuthority::open(root.0.join("task-authority.sqlite3")).unwrap();
        let InstallDecision::Installed(receipt) = applications
            .install_application(
                &artifacts,
                InstallApplicationRequest {
                    package_verification_receipt_id: verified.receipt().receipt_id,
                    idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(5); 16]),
                    installed_at_ms: 2_000,
                },
            )
            .unwrap()
        else {
            panic!("fresh key must install");
        };
        assert_eq!(receipt.installation_generation, Generation::INITIAL);
        Self {
            _root: root,
            applications,
            tasks,
            package_id,
            principal: bootstrap.principal_id,
        }
    }

    fn executor(&self) -> ApplicationAuthorityLifecycleExecutor<'_> {
        ApplicationAuthorityLifecycleExecutor::new(&self.applications, &self.tasks)
    }

    fn request(&self, cas: u64) -> ApplicationControlRequest {
        ApplicationControlRequest {
            package_id: self.package_id.into_bytes(),
            expected_generation_or_revision: cas,
            issuer_principal_id: [0x31; 16],
            idempotency_key: self.package_id.into_bytes(),
            requested_at_ms: WALL_NOW_MS,
        }
    }

    /// Registers one background task binding and its durable task row (the
    /// W27-D gate's live-activity input).
    fn register_active_background_task(&self, task_seed: u8) {
        let task_id = TaskId::from_bytes([task_seed; 16]);
        self.applications
            .register_background_task(RegisterBackgroundTaskRequest {
                package_id: self.package_id,
                task_id,
                registrant_principal: self.principal,
                idempotency_key: IdempotencyKey::from_bytes([task_seed; 16]),
                registered_at_ms: 3_000,
            })
            .unwrap();
        self.tasks
            .register_task(nlos_task::TaskSpec {
                task_id,
                task_generation: Generation::INITIAL,
                registered_at_ms: 1_000,
                application_id: None,
                plan_revision: None,
            })
            .unwrap();
    }

    fn cancel_background_task(&self, task_seed: u8) {
        let task_id = TaskId::from_bytes([task_seed; 16]);
        self.tasks
            .cancel_task(nlos_task::CancelRequest {
                task_id,
                idempotency_key: IdempotencyKey::from_bytes([task_seed ^ 0x5E; 16]),
                requested_at_ms: 2_000,
            })
            .unwrap();
    }
}

fn application_request(package_id: [u8; 16], cas: u64) -> ApplicationControlRequest {
    ApplicationControlRequest {
        package_id,
        expected_generation_or_revision: cas,
        issuer_principal_id: [0x31; 16],
        idempotency_key: package_id,
        requested_at_ms: WALL_NOW_MS,
    }
}

/// Dispatches one `ControlCommand` end-to-end through the shared handler
/// with the real application executor wired.
fn dispatch(
    tasks: &SqliteTaskAuthority,
    executor: &ApplicationAuthorityLifecycleExecutor<'_>,
    command: &ControlCommand,
) -> nlos_system_control::control::ControlReceipt {
    let health = StubHealth(RecoveryWorkerHealth {
        state: RecoveryWorkerState::Running,
        ..RecoveryWorkerHealth::default()
    });
    let control = RecoverySystemControl::new(tasks, &health, &CapabilityPolicy)
        .with_application_executor(executor);
    dispatch_in_process(
        &control,
        command,
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
        None,
        None,
        None,
    )
    .unwrap()
}

#[test]
fn disable_and_uninstall_drive_the_real_application_authority() {
    let fixture = ApplicationFixture::new("drive", 0x41);
    let executor = fixture.executor();

    let disable_id = executor
        .disable_application(fixture.request(1))
        .expect("disable drives the authority");
    // Durable replay re-derives the identical receipt id from the same
    // authority receipt facts.
    assert_eq!(
        executor.disable_application(fixture.request(1)).unwrap(),
        disable_id
    );
    let view = fixture
        .applications
        .inspect_application(fixture.package_id)
        .unwrap()
        .unwrap();
    assert_eq!(view.status, ApplicationStatus::Disabled);
    assert!(
        fixture
            .applications
            .inspect_disable_receipt(fixture.package_id)
            .unwrap()
            .is_some()
    );

    // The disable transition leaves the installation generation untouched,
    // so the uninstall CAS still addresses generation 1.
    let uninstall_id = executor
        .uninstall_application(fixture.request(1))
        .expect("uninstall drives the authority");
    assert_eq!(
        executor.uninstall_application(fixture.request(1)).unwrap(),
        uninstall_id
    );
    assert_ne!(disable_id, uninstall_id);
    let view = fixture
        .applications
        .inspect_application(fixture.package_id)
        .unwrap()
        .unwrap();
    assert_eq!(view.status, ApplicationStatus::Uninstalled);
    assert!(
        fixture
            .applications
            .inspect_uninstall_receipt(fixture.package_id)
            .unwrap()
            .is_some()
    );
}

#[test]
fn uninstall_runs_the_w27d_activity_gate_before_committing() {
    let fixture = ApplicationFixture::new("gate", 0x42);
    let executor = fixture.executor();
    fixture.register_active_background_task(0xA1);

    // One durable Active task blocks the fresh uninstall: typed refusal,
    // zero durable state.
    let failure = executor
        .uninstall_application(fixture.request(1))
        .unwrap_err();
    assert_eq!(
        failure.code,
        i32::from(nlos_schema::sabi::v1::SabiErrorCode::State)
    );
    assert_eq!(
        failure.safe_message,
        "application still has outstanding task activity"
    );
    let view = fixture
        .applications
        .inspect_application(fixture.package_id)
        .unwrap()
        .unwrap();
    assert_eq!(view.status, ApplicationStatus::Installed);
    assert!(
        fixture
            .applications
            .inspect_uninstall_receipt(fixture.package_id)
            .unwrap()
            .is_none()
    );

    // The gate is a live query: once the task reaches its terminal state,
    // a fresh command converges.
    fixture.cancel_background_task(0xA1);
    executor
        .uninstall_application(fixture.request(1))
        .expect("terminal task activity must re-open the gate");
    let view = fixture
        .applications
        .inspect_application(fixture.package_id)
        .unwrap()
        .unwrap();
    assert_eq!(view.status, ApplicationStatus::Uninstalled);
}

#[test]
fn application_arms_refuse_cas_mismatch_absent_package_and_negative_wall() {
    let fixture = ApplicationFixture::new("refuse", 0x43);
    let executor = fixture.executor();

    let stale = executor.disable_application(fixture.request(2));
    let failure = stale.unwrap_err();
    assert_eq!(
        failure.code,
        i32::from(nlos_schema::sabi::v1::SabiErrorCode::Conflict)
    );
    assert_eq!(
        failure.retry,
        i32::from(nlos_schema::sabi::v1::RetryDirective::DoNotRetry)
    );
    assert_eq!(
        failure.safe_message,
        "disable CAS mismatch: expected revision is not the application installation generation"
    );

    let absent = executor.disable_application(application_request([0xFF; 16], 1));
    assert_eq!(
        absent.unwrap_err().code,
        i32::from(nlos_schema::sabi::v1::SabiErrorCode::NotFound)
    );

    let mut negative = fixture.request(1);
    negative.requested_at_ms = -1;
    let failure = executor.disable_application(negative).unwrap_err();
    assert_eq!(
        failure.code,
        i32::from(nlos_schema::sabi::v1::SabiErrorCode::InvalidArgument)
    );
    assert_eq!(
        failure.safe_message,
        "disable wall-clock reading must be non-negative"
    );

    // Every rejection above left the application installed.
    let view = fixture
        .applications
        .inspect_application(fixture.package_id)
        .unwrap()
        .unwrap();
    assert_eq!(view.status, ApplicationStatus::Installed);
}

#[test]
fn application_arms_end_to_end_through_the_shared_handler() {
    let fixture = ApplicationFixture::new("e2e", 0x44);
    let executor = fixture.executor();
    let direct_disable = executor.disable_application(fixture.request(1)).unwrap();
    let direct_uninstall = executor.uninstall_application(fixture.request(1)).unwrap();

    let disable = ControlCommand::DisableApplication {
        control_command_id: fixture.package_id.into_bytes(),
        package_id: fixture.package_id.into_bytes(),
        expected_generation_or_revision: 1,
        reason: "operator disables the application".to_owned(),
    };
    let receipt = dispatch(&fixture.tasks, &executor, &disable);
    let ControlOutcome::ApplicationDisabled { receipt_id } = receipt.outcome.unwrap() else {
        panic!("expected an application-disabled receipt");
    };
    assert_eq!(receipt_id, direct_disable.into_bytes().to_vec());

    let uninstall = ControlCommand::UninstallApplication {
        control_command_id: fixture.package_id.into_bytes(),
        package_id: fixture.package_id.into_bytes(),
        expected_generation_or_revision: 1,
        reason: "operator uninstalls the application".to_owned(),
    };
    let receipt = dispatch(&fixture.tasks, &executor, &uninstall);
    let ControlOutcome::ApplicationUninstalled { receipt_id } = receipt.outcome.unwrap() else {
        panic!("expected an application-uninstalled receipt");
    };
    assert_eq!(receipt_id, direct_uninstall.into_bytes().to_vec());
}
