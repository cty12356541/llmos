//! Shared fixtures for nlos-application integration tests: a temporary
//! store root, a bootstrapped identity authority, an artifact store with
//! published content, and a verified signed package whose entries bind to
//! artifact heads (the `nlos-artifact` test-support shape, trimmed to what
//! the installation authority consumes).

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::{Signer, SigningKey};
use nlos_application::{
    ApplicationAuthority, DisableApplicationRequest, DisableDecision, DisableReceipt,
    InstallApplicationRequest, InstallDecision, RegisterBackgroundTaskDecision,
    RegisterBackgroundTaskRequest, RegisterProcessBindingDecision, RegisterProcessBindingRequest,
    RollbackApplicationRequest, RollbackDecision, RollbackReceipt, UninstallApplicationRequest,
    UninstallDecision, UninstallReceipt, UpdateApplicationRequest, UpdateDecision,
};
use nlos_artifact::{
    ArtifactStore, ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageManifest,
    PackageManifestEntry, ProvenanceSourceTriple, PutRevisionRequest, SignedPackage,
    VerifyPackageRequest, package_manifest_message,
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose};
use nlos_types::{
    ApplicationId, ArtifactId, IdempotencyKey, PackageId, PrincipalId, ProcessId, TaskId,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

/// Unique temporary store root, removed recursively on drop.
pub struct TestStoreDir {
    root: PathBuf,
}

impl TestStoreDir {
    pub fn new(name: &str) -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-application-test-{name}-{}-{sequence}",
            std::process::id()
        ));
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for TestStoreDir {
    fn drop(&mut self) {
        // Best-effort: Windows releases SQLite handles asynchronously, so
        // an eager remove can race an in-flight close (os error 32).
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A freshly bootstrapped identity authority with one `SemanticSigning`
/// principal whose key is valid on `[0, 10_000)` ms (mirroring the
/// `nlos-identity` test bootstrap).
pub struct TestIdentity {
    pub authority: IdentityAuthority,
    pub key: SigningKey,
    pub binding: IdentityBinding,
    _dir: TestStoreDir,
}

pub fn test_identity(name: &str, seed: u8) -> TestIdentity {
    let dir = TestStoreDir::new(name);
    let authority = IdentityAuthority::open(dir.root()).expect("open identity authority");
    let key = SigningKey::from_bytes(&[seed; 32]);
    let binding = authority
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
        .expect("bootstrap principal")
        .binding();
    TestIdentity {
        authority,
        key,
        binding,
        _dir: dir,
    }
}

/// An artifact store under `<root>/art` plus an identity authority under
/// `<root>/id`, both on the default VFS. The application authority is the
/// only store the fault matrix routes through the fault VFS.
pub struct TestStack {
    pub root: TestStoreDir,
    pub artifacts: ArtifactStore,
    pub identity: TestIdentity,
}

impl TestStack {
    pub fn new(name: &str, seed: u8) -> Self {
        let root = TestStoreDir::new(name);
        let artifacts = ArtifactStore::open(root.root().join("art")).expect("open artifact store");
        let identity = test_identity(&format!("{name}-identity"), seed);
        Self {
            root,
            artifacts,
            identity,
        }
    }

    /// Publishes one artifact revision and returns its head digest.
    pub fn publish_artifact(&self, seed: u8, payload: &[u8]) -> (ArtifactId, ContentDigest) {
        let spec = CreateArtifactSpec {
            artifact_id: ArtifactId::from_bytes([seed; 16]),
            idempotency_key: IdempotencyKey::from_bytes([0xa0 + seed; 16]),
            content_type: "application/octet-stream".to_string(),
            application_id: Some(ApplicationId::from_bytes([0xb0 + seed; 16])),
            owner: Some(format!("user-{seed}")),
            created_at_ms: 1_000 + u64::from(seed),
        };
        self.artifacts
            .create_artifact(spec.clone())
            .expect("create artifact");
        self.artifacts
            .put_revision(PutRevisionRequest {
                artifact_id: spec.artifact_id,
                expected_head_revision: 0,
                bytes: payload,
                created_at_ms: 5_000,
                provenance: ProvenanceSourceTriple {
                    source_a: [0xc0_u8.wrapping_add(seed); 16],
                    source_b: [0xd0_u8.wrapping_add(seed); 16],
                    source_digest: ContentDigest::from_bytes([0xe0_u8.wrapping_add(seed); 32]),
                },
            })
            .expect("put revision");
        (spec.artifact_id, ContentDigest::of_bytes(payload))
    }

    /// Builds, signs, and verifies one minimal package whose single entry
    /// binds the published artifact's head. `verified_at_ms` must fall in
    /// the identity key's validity window.
    pub fn verify_package(
        &self,
        package_seed: u8,
        version: u64,
        idempotency_key: IdempotencyKey,
        verified_at_ms: u64,
    ) -> nlos_artifact::PackageVerificationReceipt {
        let (artifact_id, digest) = self.publish_artifact(0x30, b"payload-of-the-package");
        let manifest = PackageManifest {
            package_id: PackageId::from_bytes([package_seed; 16]),
            version,
            entries: vec![PackageManifestEntry {
                name: "main".to_string(),
                artifact_id,
                digest,
                role: PackageEntryRole::Executable,
            }],
        };
        let message = package_manifest_message(&manifest);
        let signed = SignedPackage {
            manifest,
            signer: self.identity.binding.principal_id,
            signature: self.identity.key.sign(&message).to_bytes(),
        };
        let decision = self
            .artifacts
            .verify_package(
                &self.identity.authority,
                VerifyPackageRequest {
                    signed: &signed,
                    idempotency_key,
                    verified_at_ms,
                },
            )
            .expect("verify package");
        decision.receipt().clone()
    }
}

/// The application authority over the plain default VFS root.
pub fn open_authority(root: &Path) -> ApplicationAuthority {
    ApplicationAuthority::open(root).expect("open application authority")
}

/// The application authority's database path under a test root.
pub fn authority_database(root: &Path) -> PathBuf {
    root.join("application-authority.db")
}

/// Runs one install and unwraps the fresh-install branch.
pub fn installed(
    authority: &ApplicationAuthority,
    artifacts: &ArtifactStore,
    receipt_id: nlos_types::ReceiptId,
    key: u8,
    at_ms: u64,
) -> nlos_application::InstallationReceipt {
    match authority
        .install_application(
            artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: receipt_id,
                idempotency_key: IdempotencyKey::from_bytes([key; 16]),
                installed_at_ms: at_ms,
            },
        )
        .expect("install must succeed")
    {
        InstallDecision::Installed(receipt) => receipt,
        InstallDecision::Replayed(receipt) => {
            panic!("fresh key cannot replay, got {receipt:?}")
        }
    }
}

/// Runs one install and unwraps the replay branch.
pub fn replayed(
    authority: &ApplicationAuthority,
    artifacts: &ArtifactStore,
    receipt_id: nlos_types::ReceiptId,
    key: u8,
    at_ms: u64,
) -> nlos_application::InstallationReceipt {
    match authority
        .install_application(
            artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: receipt_id,
                idempotency_key: IdempotencyKey::from_bytes([key; 16]),
                installed_at_ms: at_ms,
            },
        )
        .expect("install must replay")
    {
        InstallDecision::Replayed(receipt) => receipt,
        InstallDecision::Installed(receipt) => {
            panic!("expected Replayed, got Installed {receipt:?}")
        }
    }
}

/// Runs one disable and unwraps the fresh-disable branch.
pub fn disabled(
    authority: &ApplicationAuthority,
    package_id: nlos_types::PackageId,
    key: u8,
    at_ms: u64,
) -> DisableReceipt {
    match authority
        .disable_application(DisableApplicationRequest {
            package_id,
            idempotency_key: IdempotencyKey::from_bytes([key; 16]),
            disabled_at_ms: at_ms,
        })
        .expect("disable must succeed")
    {
        DisableDecision::Disabled(receipt) => receipt,
        DisableDecision::Replayed(receipt) => {
            panic!("fresh key cannot replay a disable, got {receipt:?}")
        }
    }
}

/// Runs one disable and unwraps the replay branch.
pub fn disable_replayed(
    authority: &ApplicationAuthority,
    package_id: nlos_types::PackageId,
    key: u8,
    at_ms: u64,
) -> DisableReceipt {
    match authority
        .disable_application(DisableApplicationRequest {
            package_id,
            idempotency_key: IdempotencyKey::from_bytes([key; 16]),
            disabled_at_ms: at_ms,
        })
        .expect("disable must replay")
    {
        DisableDecision::Replayed(receipt) => receipt,
        DisableDecision::Disabled(receipt) => {
            panic!("expected Replayed, got Disabled {receipt:?}")
        }
    }
}

/// Runs one update and unwraps the fresh-update branch.
pub fn updated(
    authority: &ApplicationAuthority,
    artifacts: &ArtifactStore,
    package_id: nlos_types::PackageId,
    receipt_id: nlos_types::ReceiptId,
    key: u8,
    at_ms: u64,
) -> nlos_application::InstallationReceipt {
    match authority
        .update_application(
            artifacts,
            UpdateApplicationRequest {
                package_id,
                package_verification_receipt_id: receipt_id,
                idempotency_key: IdempotencyKey::from_bytes([key; 16]),
                updated_at_ms: at_ms,
            },
        )
        .expect("update must succeed")
    {
        UpdateDecision::Updated(receipt) => receipt,
        UpdateDecision::Replayed(receipt) => {
            panic!("fresh key cannot replay an update, got {receipt:?}")
        }
    }
}

/// Runs one update and unwraps the replay branch.
pub fn update_replayed(
    authority: &ApplicationAuthority,
    artifacts: &ArtifactStore,
    package_id: nlos_types::PackageId,
    receipt_id: nlos_types::ReceiptId,
    key: u8,
    at_ms: u64,
) -> nlos_application::InstallationReceipt {
    match authority
        .update_application(
            artifacts,
            UpdateApplicationRequest {
                package_id,
                package_verification_receipt_id: receipt_id,
                idempotency_key: IdempotencyKey::from_bytes([key; 16]),
                updated_at_ms: at_ms,
            },
        )
        .expect("update must replay")
    {
        UpdateDecision::Replayed(receipt) => receipt,
        UpdateDecision::Updated(receipt) => {
            panic!("expected Replayed, got Updated {receipt:?}")
        }
    }
}

/// Runs one uninstall and unwraps the fresh-uninstall branch.
pub fn uninstalled(
    authority: &ApplicationAuthority,
    package_id: nlos_types::PackageId,
    key: u8,
    at_ms: u64,
) -> UninstallReceipt {
    match authority
        .uninstall_application(UninstallApplicationRequest {
            package_id,
            idempotency_key: IdempotencyKey::from_bytes([key; 16]),
            uninstalled_at_ms: at_ms,
        })
        .expect("uninstall must succeed")
    {
        UninstallDecision::Uninstalled(receipt) => receipt,
        UninstallDecision::Replayed(receipt) => {
            panic!("fresh key cannot replay an uninstall, got {receipt:?}")
        }
    }
}

/// Runs one uninstall and unwraps the replay branch.
pub fn uninstall_replayed(
    authority: &ApplicationAuthority,
    package_id: nlos_types::PackageId,
    key: u8,
    at_ms: u64,
) -> UninstallReceipt {
    match authority
        .uninstall_application(UninstallApplicationRequest {
            package_id,
            idempotency_key: IdempotencyKey::from_bytes([key; 16]),
            uninstalled_at_ms: at_ms,
        })
        .expect("uninstall must replay")
    {
        UninstallDecision::Replayed(receipt) => receipt,
        UninstallDecision::Uninstalled(receipt) => {
            panic!("expected Replayed, got Uninstalled {receipt:?}")
        }
    }
}

/// Runs one rollback and unwraps the fresh-rollback branch.
pub fn rolled_back(
    authority: &ApplicationAuthority,
    package_id: nlos_types::PackageId,
    key: u8,
    at_ms: u64,
) -> RollbackReceipt {
    match authority
        .rollback_application(RollbackApplicationRequest {
            package_id,
            idempotency_key: IdempotencyKey::from_bytes([key; 16]),
            rollback_at_ms: at_ms,
        })
        .expect("rollback must succeed")
    {
        RollbackDecision::RolledBack(receipt) => receipt,
        RollbackDecision::Replayed(receipt) => {
            panic!("fresh key cannot replay a rollback, got {receipt:?}")
        }
    }
}

/// Runs one rollback and unwraps the replay branch.
pub fn rollback_replayed(
    authority: &ApplicationAuthority,
    package_id: nlos_types::PackageId,
    key: u8,
    at_ms: u64,
) -> RollbackReceipt {
    match authority
        .rollback_application(RollbackApplicationRequest {
            package_id,
            idempotency_key: IdempotencyKey::from_bytes([key; 16]),
            rollback_at_ms: at_ms,
        })
        .expect("rollback must replay")
    {
        RollbackDecision::Replayed(receipt) => receipt,
        RollbackDecision::RolledBack(receipt) => {
            panic!("expected Replayed, got RolledBack {receipt:?}")
        }
    }
}

/// Runs one background-task registration and unwraps the fresh branch.
pub fn background_task_registered(
    authority: &ApplicationAuthority,
    package_id: PackageId,
    task_id: TaskId,
    registrant_principal: PrincipalId,
    key: u8,
    at_ms: u64,
) -> nlos_application::BackgroundTaskRegistrationReceipt {
    match authority
        .register_background_task(RegisterBackgroundTaskRequest {
            package_id,
            task_id,
            registrant_principal,
            idempotency_key: IdempotencyKey::from_bytes([key; 16]),
            registered_at_ms: at_ms,
        })
        .expect("background task registration must succeed")
    {
        RegisterBackgroundTaskDecision::Registered(receipt) => receipt,
        RegisterBackgroundTaskDecision::Replayed(receipt) => {
            panic!("fresh key cannot replay a registration, got {receipt:?}")
        }
    }
}

/// Runs one background-task registration and unwraps the replay branch.
pub fn background_task_registration_replayed(
    authority: &ApplicationAuthority,
    package_id: PackageId,
    task_id: TaskId,
    registrant_principal: PrincipalId,
    key: u8,
    at_ms: u64,
) -> nlos_application::BackgroundTaskRegistrationReceipt {
    match authority
        .register_background_task(RegisterBackgroundTaskRequest {
            package_id,
            task_id,
            registrant_principal,
            idempotency_key: IdempotencyKey::from_bytes([key; 16]),
            registered_at_ms: at_ms,
        })
        .expect("background task registration must replay")
    {
        RegisterBackgroundTaskDecision::Replayed(receipt) => receipt,
        RegisterBackgroundTaskDecision::Registered(receipt) => {
            panic!("expected Replayed, got Registered {receipt:?}")
        }
    }
}

/// Runs one process binding registration and unwraps the fresh branch.
pub fn process_binding_registered(
    authority: &ApplicationAuthority,
    package_id: PackageId,
    process_id: ProcessId,
    registrant_principal: PrincipalId,
    key: u8,
    at_ms: u64,
) -> nlos_application::ProcessBindingReceipt {
    match authority
        .register_process_binding(RegisterProcessBindingRequest {
            package_id,
            process_id,
            registrant_principal,
            idempotency_key: IdempotencyKey::from_bytes([key; 16]),
            registered_at_ms: at_ms,
        })
        .expect("process binding registration must succeed")
    {
        RegisterProcessBindingDecision::Registered(receipt) => receipt,
        RegisterProcessBindingDecision::Replayed(receipt) => {
            panic!("fresh key cannot replay a process binding, got {receipt:?}")
        }
    }
}

/// Runs one process binding registration and unwraps the replay branch.
pub fn process_binding_registration_replayed(
    authority: &ApplicationAuthority,
    package_id: PackageId,
    process_id: ProcessId,
    registrant_principal: PrincipalId,
    key: u8,
    at_ms: u64,
) -> nlos_application::ProcessBindingReceipt {
    match authority
        .register_process_binding(RegisterProcessBindingRequest {
            package_id,
            process_id,
            registrant_principal,
            idempotency_key: IdempotencyKey::from_bytes([key; 16]),
            registered_at_ms: at_ms,
        })
        .expect("process binding registration must replay")
    {
        RegisterProcessBindingDecision::Replayed(receipt) => receipt,
        RegisterProcessBindingDecision::Registered(receipt) => {
            panic!("expected Replayed, got Registered {receipt:?}")
        }
    }
}
