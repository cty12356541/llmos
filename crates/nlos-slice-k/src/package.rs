//! Package and task fixtures over the landed authorities: bootstrap a
//! publisher principal, publish the package payload artifact, sign and
//! verify the package envelope, install it as an application, then register
//! the Task/Attempt pair the fiber will run under.
//!
//! Since W60 this module also carries the install→plan head segment: the
//! task-templated install face binds the application authority's install
//! commit to the plan authority's declaration face — the signed `tasks`
//! segment is compiled ([`compile_task_templates`],
//! `[PLAN-OVERRIDE-001]`) and applied as one plan revision behind the
//! apply-time declaration-admission consult, under the idempotency key
//! derived from `(application_id, installation_generation)`.

use ed25519_dalek::{Signer, SigningKey};
use nlos_application::{
    BackgroundTaskRegistrationReceipt, InstallApplicationRequest, InstallDecision,
    InstallationReceipt, ProcessBindingReceipt, RegisterBackgroundTaskDecision,
    RegisterBackgroundTaskRequest, RegisterProcessBindingDecision, RegisterProcessBindingRequest,
    TaskTemplateError, UninstallApplicationRequest, UninstallDecision, UninstallReceipt,
    compile_task_templates,
};
use nlos_artifact::{
    CollectOrphanBlobsDecision, CollectOrphanBlobsRequest, ContentDigest, CreateArtifactSpec,
    PackageEntryRole, PackageManifest, PackageManifestEntry, PackageVerificationReceipt,
    ProvenanceSourceTriple, PutRevisionRequest, SignedPackage, SignedPackageWithTasks,
    VerifyPackageRequest, package_manifest_message, package_manifest_with_tasks_message,
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityBinding, KeyPurpose};
use nlos_plan::{
    DeclarationAdmissionConsult, DeclarationAdmissionOutcome, PlanRevisionDecision,
    PlanRevisionReceipt,
};
use nlos_process::{
    CreateIsolationDomainRequest, ProcessBindingRecord, RegisterDelegatedProcessRequest,
};
use nlos_task::{SqliteTaskAuthority, TaskStoreError};
use nlos_types::{
    ApplicationId, ArtifactId, Generation, IdempotencyKey, PackageId, PrincipalId, ProcessId,
    TaskAttemptId, TaskId, TaskPlanId,
};
use sha2::{Digest, Sha256};

use crate::error::{SliceKError, SliceKResult};
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

    /// Installs one verified task-templated package and applies its
    /// declared `tasks` segment as one plan revision — the install→plan
    /// head segment (W60, "装包即提案计划"): after the application
    /// authority's install commit, the signed segment is compiled
    /// ([`compile_task_templates`], `[PLAN-OVERRIDE-001]` — the segment is
    /// a declaration *source*, never a second dialect) and applied to this
    /// runtime's plan authority
    /// ([`SqlitePlanAuthority::apply_plan_revision_with_admission`])
    /// behind the apply-time declaration-admission consult, mapped from
    /// this runtime's task authority
    /// ([`SqliteTaskAuthority::answer_plan_declaration`], W31-G §8.2.4).
    ///
    /// **Idempotency (domain-separated derivation):** the plan revision's
    /// exactly-once key is
    /// [`install_plan_revision_key(application_id, installation_generation)`]
    /// — SHA-256 over `llmos/slice-k-install-plan-revision/v1`, the
    /// `ApplicationId`, and the installation generation. Same-generation
    /// replay (this call re-run, or the install authority replaying its
    /// receipt) replays the same plan revision (`Replayed`, zero new
    /// revisions); a new installation generation — a same-major reinstall
    /// or update carrying a newer verified package — derives a fresh key
    /// and applies a **new total revision of the same plan**: generation 1
    /// proposes the initial revision (the authority derives the
    /// `TaskPlanId` from the key), every later generation names the plan
    /// whose genesis revision was committed under the generation-1 key, so
    /// `[PLAN-DAG-001]` total-revision semantics and the G1
    /// execution-freeze apply automatically across updates.
    ///
    /// **Window semantics (honest, not atomic):** the install commit and
    /// the plan apply are two authority transactions. If the plan
    /// application fails after the install committed
    /// ([`SliceKError::Plan`] — a structurally refused revision, an
    /// admission denial, a consult failure — or
    /// [`SliceKError::InstallPlanState`] when an update generation finds
    /// no plan genesis), the installation fact stays durably committed
    /// and is **never rolled back**: the error names the refusal and the
    /// caller converges by replaying this call — the application
    /// authority replays its receipt (`Replayed`, same generation, hence
    /// the same derived plan key) and this wiring **still attempts the
    /// plan application** on the replay, so a transient cause removed
    /// before the replay converges to the applied revision with zero
    /// double effects. A permanent cause (for example a cyclic declared
    /// segment, or a Task-tier admission denial) keeps refusing typed;
    /// plan rollback for uninstall/rollback/migration generations is a
    /// registered follow-up, not implemented here.
    ///
    /// A package whose manifest declares no `tasks` segment is not
    /// installable through this face (verification of a templated package
    /// requires a non-empty segment): it installs through the legacy
    /// [`Self::install_verified_package`] family, which declares no plan
    /// revision — no declared tasks, no proposal.
    ///
    /// W22-001 default: the install-scoped orphan-blob GC pass runs
    /// before the install authority call; opt out via
    /// [`Self::install_verified_templated_package_with_gc`].
    ///
    /// # Errors
    ///
    /// Fails closed with [`SliceKError::InstallPlanState`] *before any
    /// durable write* when `verification` does not bind `signed` (digest
    /// or package-identity mismatch); otherwise propagates
    /// artifact-authority (orphan GC), application-authority, clock, and
    /// plan-authority errors typed — see the window semantics above for
    /// which failures leave the installation committed.
    pub fn install_verified_templated_package(
        &self,
        verification: &PackageVerificationReceipt,
        signed: &SignedPackageWithTasks,
        seed: u8,
    ) -> SliceKResult<TemplatedInstallReceipt> {
        self.install_verified_templated_package_with_gc(
            verification,
            signed,
            seed,
            AutoOrphanGc::Enabled,
        )
    }

    /// [`Self::install_verified_templated_package`] with an explicit
    /// orphan-GC policy (W22-001), mirroring
    /// [`Self::install_verified_package_with_gc`]: the install authority
    /// call and the plan-revision application are identical, only the
    /// install-scoped GC prefix is toggled.
    ///
    /// # Errors
    ///
    /// As [`Self::install_verified_templated_package`].
    pub fn install_verified_templated_package_with_gc(
        &self,
        verification: &PackageVerificationReceipt,
        signed: &SignedPackageWithTasks,
        seed: u8,
        orphan_gc: AutoOrphanGc,
    ) -> SliceKResult<TemplatedInstallReceipt> {
        // Verify-then-declare, pairing first (zero durable state on a
        // mismatch): the receipt must bind exactly this signed templated
        // package — same combined manifest digest, same package identity.
        // A mutated segment cannot reuse a good package's receipt.
        let templated_digest = ContentDigest::from_bytes(package_manifest_with_tasks_message(
            &signed.manifest,
            &signed.tasks,
        ));
        if verification.manifest_digest != templated_digest
            || verification.package_id != signed.manifest.package_id
        {
            return Err(SliceKError::InstallPlanState(
                "verification receipt does not bind this signed task-templated package",
            ));
        }
        match orphan_gc {
            AutoOrphanGc::Enabled => {
                self.install_orphan_gc(seed)?;
            }
            AutoOrphanGc::Disabled => {}
        }
        // The application authority call rides the legacy family's key
        // band (`seeded_key(seed, 15/16)`), so both faces agree on the
        // durable installation fact for one (receipt, seed) pair.
        let installed_at_ms = self.wall_now_ms(seeded_key(seed, 15))?;
        let installation = match self.applications.install_application(
            &self.artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: verification.receipt_id,
                idempotency_key: seeded_key(seed, 16),
                installed_at_ms,
            },
        )? {
            InstallDecision::Installed(receipt) | InstallDecision::Replayed(receipt) => receipt,
        };
        // Head segment: declare the signed segment as one plan revision.
        // Fail-closed typed — the committed installation is retained (see
        // the window semantics on the public face).
        let plan = self.apply_install_plan_revision(signed, &installation, seed)?;
        Ok(TemplatedInstallReceipt { installation, plan })
    }

    /// The plan application of one committed templated installation: the
    /// derived revision key, the generation→plan mapping (create at
    /// generation 1, total revision of the discovered plan at every later
    /// generation), the compiled proposal, and the gated apply.
    fn apply_install_plan_revision(
        &self,
        signed: &SignedPackageWithTasks,
        installation: &InstallationReceipt,
        seed: u8,
    ) -> SliceKResult<PlanRevisionDecision> {
        let idempotency_key = install_plan_revision_key(
            installation.application_id,
            installation.installation_generation,
        );
        let plan_id = if installation.installation_generation.get() == 1 {
            None
        } else {
            Some(self.discover_application_plan(installation.application_id)?)
        };
        // Deterministic per-seed observation time: a replay of this seed
        // takes the same durable clock reading, so the plan authority's
        // replay content-equality (nodes, key, timestamp) holds.
        let applied_at_ms = self.wall_now_ms(seeded_key(seed, 40))?;
        let mut request =
            compile_task_templates(signed, idempotency_key, applied_at_ms).map_err(|error| {
                match error {
                    TaskTemplateError::InvalidSegment(inner) => SliceKError::Artifact(inner),
                }
            })?;
        request.plan_id = plan_id;
        let consult = TaskAuthorityDeclarationConsult { tasks: &self.tasks };
        Ok(self
            .plans()
            .apply_plan_revision_with_admission(request, &consult)?)
    }

    /// Finds the plan of one application: the plan whose genesis revision
    /// (revision 1) was committed under the application's generation-1
    /// derived key. Plan rows are lifetime metadata, so the receipt scan
    /// is stable once the genesis exists; a miss is the typed
    /// no-genesis refusal (legacy install, install-window residue, or a
    /// replaced plan store — the documented convergence is replaying the
    /// application's generation-1 templated install).
    fn discover_application_plan(&self, application_id: ApplicationId) -> SliceKResult<TaskPlanId> {
        let genesis = install_plan_revision_key(application_id, Generation::INITIAL);
        for plan_id in self.plans().list_plan_ids()? {
            if let Some(receipt) = self.plans().inspect_plan_revision(plan_id, 1)?
                && receipt.idempotency_key == genesis
            {
                return Ok(plan_id);
            }
        }
        Err(SliceKError::InstallPlanState(
            "application has no plan genesis revision in the plan authority; \
             replay its generation-1 templated install to converge",
        ))
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

    /// Uninstalls one application through the production activity gate
    /// (W27-D, [`ApplicationAuthority::
    /// uninstall_application_with_task_activity_gate`]): inside the gate's
    /// own transaction the package's durably registered background Tasks are
    /// resolved and their liveness is queried on this runtime's task
    /// authority — a fresh uninstall is refused with
    /// `ApplicationActiveTasksRunning` while any registered Task is still
    /// outstanding (`Active`), and unanswerable queries fail closed. Durable
    /// replay under the same `seeded_key(seed, 17/18)` never consults the
    /// gate. The teardown lane (W30-D) drives the registered Tasks and
    /// Process bindings to their terminal states first, so this call is
    /// what proves the gate open.
    ///
    /// # Errors
    ///
    /// Propagates application-authority errors (including
    /// `ApplicationActiveTasksRunning` and `TaskActivityQueryFailed`); a
    /// `Replayed` decision returns the durably recorded original receipt.
    pub fn uninstall_application_gated_by_task_activity(
        &self,
        package_id: PackageId,
        seed: u8,
    ) -> SliceKResult<UninstallReceipt> {
        let uninstalled_at_ms = self.wall_now_ms(seeded_key(seed, 17))?;
        match self
            .applications
            .uninstall_application_with_task_activity_gate(
                &self.tasks,
                UninstallApplicationRequest {
                    package_id,
                    idempotency_key: seeded_key(seed, 18),
                    uninstalled_at_ms,
                },
            )? {
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
        self.register_task_and_attempt_for(seed, None, None)
    }

    /// [`Self::register_task_and_attempt`] with the schema-v44 declaration
    /// association (ADR-0016 决定 3, W29-A): the durable Task row carries
    /// `application_id` / `plan_revision` verbatim — the association the
    /// slice previously could only hold in its own orchestration layer
    /// (B-SLICE-K-001 §4 gap 1). The association is part of the
    /// declaration identity (a replay with a different association is
    /// refused by the task authority); verification of the reference
    /// happens at the materialization/permit boundaries, not here.
    ///
    /// # Errors
    ///
    /// Propagates task-authority and clock errors.
    pub fn register_task_and_attempt_for(
        &self,
        seed: u8,
        application_id: Option<nlos_types::ApplicationId>,
        plan_revision: Option<nlos_task::TaskPlanRevisionRef>,
    ) -> SliceKResult<(TaskId, TaskAttemptId, nlos_types::CancellationScopeId)> {
        let task_id = TaskId::from_bytes([seed.wrapping_add(20); 16]);
        let attempt_id = TaskAttemptId::from_bytes([seed.wrapping_add(21); 16]);
        let scope_id = nlos_types::CancellationScopeId::from_bytes([seed.wrapping_add(22); 16]);
        let registered_at_ms = self.wall_now_i64(seeded_key(seed, 23))?;
        self.tasks.register_task(nlos_task::TaskSpec {
            task_id,
            task_generation: initial_generation(),
            registered_at_ms,
            application_id,
            plan_revision,
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

/// The durable facts of one task-templated install (W60): the application
/// authority's immutable installation receipt (whichever branch —
/// `Installed` or `Replayed` — produced it) plus the plan authority's
/// decision for the declared `tasks` segment of the same call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemplatedInstallReceipt {
    /// The installation receipt the application authority committed (or
    /// replayed — the plan application runs on both branches, the
    /// convergence guarantee of the head segment's window semantics).
    pub installation: InstallationReceipt,
    /// The plan authority's decision for the segment's compiled proposal:
    /// `Applied` on the first execution of the derived revision key,
    /// `Replayed` on every same-generation replay (zero new revisions).
    pub plan: PlanRevisionDecision,
}

impl TemplatedInstallReceipt {
    /// The applied-or-replayed plan revision receipt, whichever branch.
    #[must_use]
    pub const fn plan_receipt(&self) -> &PlanRevisionReceipt {
        match &self.plan {
            PlanRevisionDecision::Applied(receipt) | PlanRevisionDecision::Replayed(receipt) => {
                receipt
            }
        }
    }
}

/// Domain of the install→plan revision key derivation (W60): the
/// derivation family of [`install_plan_revision_key`], disjoint from every
/// other idempotency-key domain by construction.
const INSTALL_PLAN_REVISION_KEY_DOMAIN: &[u8] = b"llmos/slice-k-install-plan-revision/v1";

/// Deterministic plan-revision idempotency key of one installation
/// generation: SHA-256 over [`INSTALL_PLAN_REVISION_KEY_DOMAIN`], the
/// `ApplicationId` (16 bytes), and the installation generation (u64
/// big-endian) — fixed-width framing, so no `(application, generation)`
/// pair ever aliases another and no seed participates (any seed replaying
/// the same install derives the same plan key).
///
/// This is the total-revision identity of the head segment: generation 1's
/// key creates the plan (the authority derives the `TaskPlanId` from it),
/// every later generation's key names one new total revision of that plan,
/// and a same-generation replay reuses the key bit-for-bit.
#[must_use]
pub fn install_plan_revision_key(
    application_id: ApplicationId,
    installation_generation: Generation,
) -> IdempotencyKey {
    let digest = Sha256::new()
        .chain_update(INSTALL_PLAN_REVISION_KEY_DOMAIN)
        .chain_update(application_id.as_bytes())
        .chain_update(installation_generation.get().to_be_bytes())
        .finalize();
    let mut key = [0_u8; 16];
    key.copy_from_slice(&digest[..16]);
    IdempotencyKey::from_bytes(key)
}

/// The Task-side half of the apply-time declaration consult (W36-P8; W31-G
/// §8.2.4), wired for the plan authority's gated apply: the task
/// authority's read-only
/// [`answer_plan_declaration`](SqliteTaskAuthority::answer_plan_declaration)
/// mapped onto [`DeclarationAdmissionConsult`] — the declaration twin of
/// the daemon's W31-A `TaskAuthorityMaterializationConsult` adapter. A
/// typed `TaskNodeAdmissionDenied` is the `Denied` answer (the tier's
/// window-shrink fact); every other Task error stays `Err` (a failed
/// consult — the gated apply fails closed, no revision without a verified
/// admission).
struct TaskAuthorityDeclarationConsult<'a> {
    tasks: &'a SqliteTaskAuthority,
}

impl DeclarationAdmissionConsult for TaskAuthorityDeclarationConsult<'_> {
    type Error = TaskStoreError;

    fn consult_plan_declaration(
        &self,
        projected_task_nodes: u64,
    ) -> Result<DeclarationAdmissionOutcome, TaskStoreError> {
        match self.tasks.answer_plan_declaration(projected_task_nodes) {
            Ok(()) => Ok(DeclarationAdmissionOutcome::Admits),
            Err(TaskStoreError::TaskNodeAdmissionDenied {
                profile_id,
                task_count: _,
                max_task_nodes,
            }) => Ok(DeclarationAdmissionOutcome::Denied {
                profile_id: profile_id.to_string(),
                max_task_nodes,
            }),
            Err(other) => Err(other),
        }
    }
}
