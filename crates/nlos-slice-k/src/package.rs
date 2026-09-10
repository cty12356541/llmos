//! Package and task fixtures over the landed authorities: bootstrap a
//! publisher principal, publish the package payload artifact, sign and
//! verify the package envelope, install it as an application, then register
//! the Task/Attempt pair the fiber will run under.

use ed25519_dalek::{Signer, SigningKey};
use nlos_application::{
    BackgroundTaskRegistrationReceipt, InstallApplicationRequest, InstallDecision,
    InstallationReceipt, ProcessBindingReceipt, RegisterBackgroundTaskDecision,
    RegisterBackgroundTaskRequest, RegisterProcessBindingDecision, RegisterProcessBindingRequest,
    UninstallApplicationRequest, UninstallDecision, UninstallReceipt,
};
use nlos_artifact::{
    CollectOrphanBlobsDecision, CollectOrphanBlobsRequest, ContentDigest, CreateArtifactSpec,
    PackageEntryRole, PackageManifest, PackageManifestEntry, PackageVerificationReceipt,
    ProvenanceSourceTriple, PutRevisionRequest, SignedPackage, VerifyPackageRequest,
    package_manifest_message,
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityBinding, KeyPurpose};
use nlos_process::{
    CreateIsolationDomainRequest, ProcessBindingRecord, RegisterDelegatedProcessRequest,
};
use nlos_types::{
    ArtifactId, Generation, IdempotencyKey, PackageId, PrincipalId, ProcessId, TaskAttemptId,
    TaskId,
};

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

/// Install-time orphan-GC policy (W22-001): whether one install runs the
/// install-scoped orphan-blob GC pass ([`SliceKRuntime::install_orphan_gc`])
/// before the install authority call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoOrphanGc {
    /// Default: the install-scoped pass runs first, collecting pre-existing
    /// orphan blobs; its receipt is durably recorded and replayable.
    Enabled,
    /// Opt-out: no GC pass runs; pre-existing orphan blobs stay on disk.
    Disabled,
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
            provenance: provenance_triple(seed),
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
    /// immutable installation receipt. W22-001 default: one install-scoped
    /// orphan-blob GC pass ([`Self::install_orphan_gc`]) runs before the
    /// install authority call; pass [`AutoOrphanGc::Disabled`] via
    /// [`Self::install_verified_package_with_gc`] to skip it.
    ///
    /// # Errors
    ///
    /// Propagates artifact-authority (orphan GC), application-authority,
    /// and clock errors; a `Replayed` decision returns the durably recorded
    /// original receipt.
    pub fn install_verified_package(
        &self,
        verification: &PackageVerificationReceipt,
        seed: u8,
    ) -> SliceKResult<InstallationReceipt> {
        self.install_verified_package_with_gc(verification, seed, AutoOrphanGc::Enabled)
    }

    /// [`Self::install_verified_package`] with an explicit orphan-GC policy
    /// (W22-001): [`AutoOrphanGc::Enabled`] runs the install-scoped pass
    /// before the install authority call; [`AutoOrphanGc::Disabled`] skips
    /// it entirely and leaves pre-existing orphan blobs on disk.
    ///
    /// # Errors
    ///
    /// Propagates artifact-authority (orphan GC, `Enabled` only),
    /// application-authority, and clock errors; a `Replayed` decision
    /// returns the durably recorded original receipt.
    pub fn install_verified_package_with_gc(
        &self,
        verification: &PackageVerificationReceipt,
        seed: u8,
        orphan_gc: AutoOrphanGc,
    ) -> SliceKResult<InstallationReceipt> {
        match orphan_gc {
            AutoOrphanGc::Enabled => {
                self.install_orphan_gc(seed)?;
            }
            AutoOrphanGc::Disabled => {}
        }
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

    /// Installs the verified package referenced by an existing durable
    /// verification receipt id (authority-first readback), advancing the
    /// installation generation on reinstall. W22-001 default: the
    /// install-scoped orphan-blob GC pass ([`Self::install_orphan_gc`])
    /// runs before the install authority call; opt out via
    /// [`Self::install_verified_package_by_id_with_gc`].
    ///
    /// # Errors
    ///
    /// Propagates artifact-authority (orphan GC), application-authority,
    /// and clock errors; a `Replayed` decision returns the durably recorded
    /// original receipt.
    pub fn install_verified_package_by_id(
        &self,
        verification_receipt_id: nlos_types::ReceiptId,
        seed: u8,
    ) -> SliceKResult<InstallationReceipt> {
        self.install_verified_package_by_id_with_gc(
            verification_receipt_id,
            seed,
            AutoOrphanGc::Enabled,
        )
    }

    /// [`Self::install_verified_package_by_id`] with an explicit orphan-GC
    /// policy (W22-001), mirroring [`Self::install_verified_package_with_gc`].
    ///
    /// # Errors
    ///
    /// Propagates artifact-authority (orphan GC, `Enabled` only),
    /// application-authority, and clock errors; a `Replayed` decision
    /// returns the durably recorded original receipt.
    pub fn install_verified_package_by_id_with_gc(
        &self,
        verification_receipt_id: nlos_types::ReceiptId,
        seed: u8,
        orphan_gc: AutoOrphanGc,
    ) -> SliceKResult<InstallationReceipt> {
        match orphan_gc {
            AutoOrphanGc::Enabled => {
                self.install_orphan_gc(seed)?;
            }
            AutoOrphanGc::Disabled => {}
        }
        let installed_at_ms = self.wall_now_ms(seeded_key(seed, 15))?;
        match self.applications.install_application(
            &self.artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: verification_receipt_id,
                idempotency_key: seeded_key(seed, 16),
                installed_at_ms,
            },
        )? {
            InstallDecision::Installed(receipt) | InstallDecision::Replayed(receipt) => Ok(receipt),
        }
    }

    /// Runs one explicit conservative orphan-blob GC pass over this
    /// runtime's artifact store ([`ArtifactStore::collect_orphan_blobs`],
    /// B-ARTIFACT-004) under the manual-invocation keys
    /// `seeded_key(seed, 19/20)` (W17-001). Since W22-001 the install path
    /// runs its own independent install-scoped pass
    /// ([`Self::install_orphan_gc`], keys `seeded_key(seed, 21/22)`), so a
    /// manual pass and an install-time pass never alias each other's
    /// receipts.
    ///
    /// # Errors
    ///
    /// Propagates artifact-authority and clock errors.
    pub fn collect_orphan_blobs(&self, seed: u8) -> SliceKResult<CollectOrphanBlobsDecision> {
        self.orphan_gc_pass(seeded_key(seed, 19), seeded_key(seed, 20))
    }

    /// The install-scoped orphan-blob GC pass (W22-001): one conservative
    /// scan+collect of pre-existing orphan blobs, exactly-once under
    /// `seeded_key(seed, 21/22)`. The install path invokes this
    /// automatically before the install authority call
    /// ([`AutoOrphanGc::Enabled`]); invoking it again replays the durably
    /// recorded receipt unchanged — the readback path for the install-time
    /// run. After an `AutoOrphanGc::Disabled` install this performs the
    /// pass on demand instead (no key was consumed).
    ///
    /// # Errors
    ///
    /// Propagates artifact-authority and clock errors.
    pub fn install_orphan_gc(&self, seed: u8) -> SliceKResult<CollectOrphanBlobsDecision> {
        self.orphan_gc_pass(seeded_key(seed, 21), seeded_key(seed, 22))
    }

    fn orphan_gc_pass(
        &self,
        clock_key: IdempotencyKey,
        idempotency_key: IdempotencyKey,
    ) -> SliceKResult<CollectOrphanBlobsDecision> {
        let collected_at_ms = self.wall_now_ms(clock_key)?;
        Ok(self
            .artifacts
            .collect_orphan_blobs(CollectOrphanBlobsRequest {
                idempotency_key,
                collected_at_ms,
            })?)
    }

    /// Uninstalls one installed or disabled application (authority-first:
    /// terminal `installed|disabled → uninstalled` CAS mark), returning the
    /// immutable uninstall receipt.
    ///
    /// # Errors
    ///
    /// Propagates application-authority errors; a `Replayed` decision
    /// returns the durably recorded original receipt.
    pub fn uninstall_application(
        &self,
        package_id: PackageId,
        seed: u8,
    ) -> SliceKResult<UninstallReceipt> {
        let uninstalled_at_ms = self.wall_now_ms(seeded_key(seed, 17))?;
        match self
            .applications
            .uninstall_application(UninstallApplicationRequest {
                package_id,
                idempotency_key: seeded_key(seed, 18),
                uninstalled_at_ms,
            })? {
            UninstallDecision::Uninstalled(receipt) | UninstallDecision::Replayed(receipt) => {
                Ok(receipt)
            }
        }
    }

    /// Registers one background Task against an installed application
    /// ([`ApplicationAuthority::register_background_task`], B-APPLICATION-005).
    ///
    /// # Errors
    ///
    /// Propagates application-authority errors; a `Replayed` decision
    /// returns the durably recorded original receipt.
    pub fn register_background_task(
        &self,
        package_id: PackageId,
        task_id: TaskId,
        registrant_principal: PrincipalId,
        seed: u8,
    ) -> SliceKResult<BackgroundTaskRegistrationReceipt> {
        let registered_at_ms = self.wall_now_ms(seeded_key(seed, 30))?;
        match self
            .applications
            .register_background_task(RegisterBackgroundTaskRequest {
                package_id,
                task_id,
                registrant_principal,
                idempotency_key: seeded_key(seed, 31),
                registered_at_ms,
            })? {
            RegisterBackgroundTaskDecision::Registered(receipt)
            | RegisterBackgroundTaskDecision::Replayed(receipt) => Ok(receipt),
        }
    }

    /// Registers one Process binding against an installed application
    /// ([`ApplicationAuthority::register_process_binding`], B-APPLICATION-006).
    ///
    /// # Errors
    ///
    /// Propagates application-authority errors; a `Replayed` decision
    /// returns the durably recorded original receipt.
    pub fn register_process_binding(
        &self,
        package_id: PackageId,
        process_id: ProcessId,
        registrant_principal: PrincipalId,
        seed: u8,
    ) -> SliceKResult<ProcessBindingReceipt> {
        let registered_at_ms = self.wall_now_ms(seeded_key(seed, 32))?;
        match self
            .applications
            .register_process_binding(RegisterProcessBindingRequest {
                package_id,
                process_id,
                registrant_principal,
                idempotency_key: seeded_key(seed, 33),
                registered_at_ms,
            })? {
            RegisterProcessBindingDecision::Registered(receipt)
            | RegisterProcessBindingDecision::Replayed(receipt) => Ok(receipt),
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

    /// Materializes the Process of one attempt through the process
    /// authority: creates (or replays) the attempt's `IsolationDomain`,
    /// then registers (or replays) the durable delegated Process /
    /// `AgentInstance` binding against it. The returned record is the
    /// receipt every fiber spec of the chain takes its process/agent
    /// identities and generations from. Both steps are idempotent under
    /// their seeded keys, so reopening after a crash replays byte-identically.
    ///
    /// # Errors
    ///
    /// Propagates the process authority's fail-closed errors and clock errors.
    pub fn materialize_process(
        &self,
        seed: u8,
        task_id: TaskId,
        task_attempt_id: TaskAttemptId,
        attempt_generation: Generation,
    ) -> SliceKResult<ProcessBindingRecord> {
        let domain = self
            .process
            .create_isolation_domain(CreateIsolationDomainRequest {
                policy_digest: [seed.wrapping_add(111); 32],
                idempotency_key: seeded_key(seed, 110),
                created_at_ms: self.wall_now_ms(seeded_key(seed, 112))?,
            })?
            .record()
            .clone();
        let binding = self
            .process
            .register_delegated_process(RegisterDelegatedProcessRequest {
                task_id,
                task_attempt_id,
                attempt_generation,
                isolation_domain_id: domain.isolation_domain_id,
                isolation_domain_generation: domain.generation,
                isolation_domain_fencing_token: domain.fencing_token,
                idempotency_key: seeded_key(seed, 113),
                created_at_ms: self.wall_now_ms(seeded_key(seed, 114))?,
            })?
            .record()
            .clone();
        Ok(binding)
    }
}

/// Fixture provenance triple for slice `put_revision` calls.
#[must_use]
pub fn provenance_triple(seed: u8) -> ProvenanceSourceTriple {
    ProvenanceSourceTriple {
        source_a: [0xc0_u8.wrapping_add(seed); 16],
        source_b: [0xd0_u8.wrapping_add(seed); 16],
        source_digest: ContentDigest::from_bytes([0xe0_u8.wrapping_add(seed); 32]),
    }
}

/// Path of one digest-addressed blob under the slice runtime root
/// (`{root}/artifacts/artifacts/blobs/`, matching [`ArtifactStore::open`]
/// on `{root}/artifacts`).
#[must_use]
pub fn artifact_blob_path(root: &std::path::Path, digest: ContentDigest) -> std::path::PathBuf {
    let hex = digest.to_hex();
    root.join("artifacts")
        .join("artifacts")
        .join("blobs")
        .join(&hex[..2])
        .join(hex)
}

/// Plants a provable orphan blob (bytes on disk, no metadata row) for
/// slice fixtures simulating abandoned package writes.
///
/// # Errors
///
/// Propagates filesystem errors from directory creation or write.
pub fn plant_orphan_artifact_blob(
    root: &std::path::Path,
    tag: u8,
    len: usize,
) -> SliceKResult<(ContentDigest, std::path::PathBuf)> {
    let payload = fixture_bytes(tag, len);
    let digest = ContentDigest::of_bytes(&payload);
    let path = artifact_blob_path(root, digest);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &payload)?;
    Ok((digest, path))
}

/// Distinct payload bytes for fixtures (deterministic, seed-tagged).
#[must_use]
pub fn fixture_bytes(tag: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| tag ^ u8::try_from(index % 251).unwrap_or(0))
        .collect()
}
