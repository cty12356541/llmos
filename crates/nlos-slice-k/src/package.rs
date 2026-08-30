//! Package and task fixtures over the landed authorities: bootstrap a
//! publisher principal, publish the package payload artifact, sign and
//! verify the package envelope, install it as an application, then register
//! the Task/Attempt pair the fiber will run under.

use ed25519_dalek::{Signer, SigningKey};
use nlos_application::{InstallApplicationRequest, InstallDecision, InstallationReceipt};
use nlos_artifact::{
    ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageManifest, PackageManifestEntry,
    PackageVerificationReceipt, PutRevisionRequest, SignedPackage, VerifyPackageRequest,
    package_manifest_message,
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityBinding, KeyPurpose};
use nlos_types::{ArtifactId, PackageId, PrincipalId, TaskAttemptId, TaskId};

use crate::error::SliceKResult;
use crate::runtime::{SliceKRuntime, initial_generation, seeded_key};

/// A package producer bootstrapped into the runtime's identity authority.
pub struct Publisher {
    pub principal_id: PrincipalId,
    pub signing: SigningKey,
    #[allow(dead_code)]
    binding: IdentityBinding,
}

/// The signed package plus the payload artifact it binds.
pub struct PublishedPackage {
    pub package_id: PackageId,
    pub manifest: PackageManifest,
    pub signed: SignedPackage,
    pub payload_artifact: ArtifactId,
    pub payload_digest: ContentDigest,
}

impl SliceKRuntime {
    /// Bootstraps one `SemanticSigning` publisher principal whose key is
    /// valid on `[0, u64::MAX)` ms so real wall-clock verification
    /// timestamps are accepted.
    ///
    /// # Errors
    ///
    /// Propagates the identity authority's bootstrap error.
    pub fn bootstrap_publisher(&self, seed: u8) -> SliceKResult<Publisher> {
        let key = SigningKey::from_bytes(&[seed; 32]);
        let binding = self
            .identity
            .bootstrap_principal(BootstrapPrincipalRequest {
                principal_profile_digest: [seed.wrapping_add(1); 32],
                control_domain_policy_digest: [seed.wrapping_add(2); 32],
                public_key: key.verifying_key().to_bytes(),
                key_purpose: KeyPurpose::SemanticSigning,
                key_valid_from_ms: 0,
                key_valid_until_ms: i64::MAX as u64,
                idempotency_key: seeded_key(seed, 3),
                created_at_ms: 0,
            })?
            .binding();
        Ok(Publisher {
            principal_id: binding.principal_id,
            signing: key,
            binding,
        })
    }

    /// Creates the payload artifact (head revision 1) and signs a
    /// one-entry package manifest binding its digest.
    ///
    /// # Errors
    ///
    /// Propagates artifact-authority and clock errors.
    pub fn publish_signed_package(
        &self,
        publisher: &Publisher,
        seed: u8,
        payload: &[u8],
    ) -> SliceKResult<PublishedPackage> {
        let artifact_id = ArtifactId::from_bytes([seed.wrapping_add(10); 16]);
        let created_at_ms = self.wall_now_ms(seeded_key(seed, 12))?;
        self.artifacts.create_artifact(CreateArtifactSpec {
            artifact_id,
            idempotency_key: seeded_key(seed, 11),
            content_type: "application/octet-stream".to_string(),
            application_id: None,
            owner: None,
            created_at_ms,
        })?;
        self.artifacts.put_revision(PutRevisionRequest {
            artifact_id,
            expected_head_revision: 0,
            bytes: payload,
            created_at_ms,
        })?;
        let payload_digest = ContentDigest::of_bytes(payload);
        let manifest = PackageManifest {
            package_id: PackageId::from_bytes([seed; 16]),
            version: 1,
            entries: vec![PackageManifestEntry {
                name: "payload".to_string(),
                artifact_id,
                digest: payload_digest,
                role: PackageEntryRole::Data,
            }],
        };
        let signed = SignedPackage {
            signature: publisher
                .signing
                .sign(&package_manifest_message(&manifest))
                .to_bytes(),
            signer: publisher.principal_id,
            manifest: manifest.clone(),
        };
        Ok(PublishedPackage {
            package_id: manifest.package_id,
            manifest,
            signed,
            payload_artifact: artifact_id,
            payload_digest,
        })
    }

    /// Verifies the signed package envelope through the artifact authority
    /// (signature + head binding), returning the durable verification
    /// receipt.
    ///
    /// # Errors
    ///
    /// Propagates artifact-authority errors (tamper, unknown signer, key
    /// revoked) and clock errors.
    pub fn verify_signed_package(
        &self,
        package: &PublishedPackage,
        seed: u8,
    ) -> SliceKResult<PackageVerificationReceipt> {
        let verified_at_ms = self.wall_now_ms(seeded_key(seed, 13))?;
        let decision = self.artifacts.verify_package(
            &self.identity,
            VerifyPackageRequest {
                signed: &package.signed,
                idempotency_key: seeded_key(seed, 14),
                verified_at_ms,
            },
        )?;
        Ok(decision.receipt().clone())
    }

    /// Installs the verified package (authority-first: the application
    /// authority reads the verification receipt back by id), returning the
    /// immutable installation receipt.
    ///
    /// # Errors
    ///
    /// Propagates application-authority errors; a `Replayed` decision
    /// returns the durably recorded original receipt.
    pub fn install_verified_package(
        &self,
        verification: &PackageVerificationReceipt,
        seed: u8,
    ) -> SliceKResult<InstallationReceipt> {
        let installed_at_ms = self.wall_now_ms(seeded_key(seed, 15))?;
        match self.applications.install_application(
            &self.artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: verification.receipt_id,
                idempotency_key: seeded_key(seed, 16),
                installed_at_ms,
            },
        )? {
            InstallDecision::Installed(receipt) | InstallDecision::Replayed(receipt) => Ok(receipt),
        }
    }

    /// Registers the `Task` + `TaskAttempt` pair of one chain, with the frozen
    /// empty-history snapshot bundle the permit CAS revalidates.
    ///
    /// # Errors
    ///
    /// Propagates task-authority and clock errors.
    pub fn register_task_and_attempt(
        &self,
        seed: u8,
    ) -> SliceKResult<(TaskId, TaskAttemptId, nlos_types::CancellationScopeId)> {
        let task_id = TaskId::from_bytes([seed.wrapping_add(20); 16]);
        let attempt_id = TaskAttemptId::from_bytes([seed.wrapping_add(21); 16]);
        let scope_id = nlos_types::CancellationScopeId::from_bytes([seed.wrapping_add(22); 16]);
        let registered_at_ms = self.wall_now_i64(seeded_key(seed, 23))?;
        self.tasks.register_task(nlos_task::TaskSpec {
            task_id,
            task_generation: initial_generation(),
            registered_at_ms,
        })?;
        self.tasks.register_attempt(nlos_task::AttemptSpec {
            task_id,
            attempt_id,
            attempt_generation: initial_generation(),
            snapshot: nlos_task::SnapshotBundle {
                snapshot_id: nlos_types::TaskSnapshotId::from_bytes([seed.wrapping_add(24); 16]),
                snapshot_digest: [seed.wrapping_add(25); 32],
                expected_head_commit_seq: 0,
                effect_history_root: nlos_task::empty_effect_history_root(),
                retry_fence_epoch: 0,
            },
            cancellation_scope_id: scope_id,
            cancellation_generation: initial_generation(),
            idempotency_key: seeded_key(seed, 26),
            registered_at_ms,
        })?;
        Ok((task_id, attempt_id, scope_id))
    }
}

/// Distinct payload bytes for fixtures (deterministic, seed-tagged).
#[must_use]
pub fn fixture_bytes(tag: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| tag ^ u8::try_from(index % 251).unwrap_or(0))
        .collect()
}
