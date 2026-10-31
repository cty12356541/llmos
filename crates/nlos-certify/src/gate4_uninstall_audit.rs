//! Gate 4: Zero-residue uninstall, immediate capability revocation, shared
//! artifact preservation, and `Z4`-gated `UserDataContainer` retention audit
//! (`[PKG-UNINSTALL-001]` & `[CAP-REVOKE-001]`, `ADR-0027` §3.4).
//!
//! Drives real `IdentityAuthority`, `CapabilityAuthority`, `ArtifactStore`,
//! `SqliteTaskAuthority`, and `ApplicationAuthority` in an isolated sandbox
//! through the complete lifecycle:
//! `Install -> Grant Capabilities -> Start Background Tasks -> Populate Cache + Shared Artifact + UserDataContainer -> Uninstall`
//! and verifies:
//! 1. Active background task guard (`uninstall_application_with_task_activity_gate`
//!    refuses uninstall with `ApplicationActiveTasksRunning` and zero dirty
//!    state while tasks are `Active`; succeeds cleanly once tasks are cancelled).
//! 2. Zero orphan active background tasks post-uninstall.
//! 3. Immediate revocation of every `CapabilityId` granted to the application
//!    principal (`consume` fails closed with `CapabilityRevoked` /
//!    `GenerationFenceConflict`).
//! 4. Application private `cache` directory and derived cache entries are 100%
//!    emptied (`0` residual cache files).
//! 5. Cross-application shared `Artifact` in `ArtifactStore` remains intact with
//!    verifiable content digest.
//! 6. Application-owned `UserDataContainer` is **100% preserved by default**
//!    (bitwise identical SHA-256 tree digest), and is purged **if and only if**
//!    an explicit `IdentityAuthority`-verified `Z4` user-data destruction
//!    confirmation receipt ([`Z4UserDataPurgeConsentReceipt`]) is presented.

use std::fs;
use std::path::Path;

use ed25519_dalek::{Signer, SigningKey};
use nlos_application::{
    ApplicationAuthority, ApplicationAuthorityError, ApplicationStatus, InstallApplicationRequest,
    RegisterBackgroundTaskRequest, UninstallApplicationRequest, UninstallDecision,
};
use nlos_artifact::{
    ArtifactStore, ContentDigest, CreateArtifactSpec, ProvenanceSourceTriple, PutRevisionRequest,
};
use nlos_capability::{
    CapabilityAuthority, CapabilityAuthorityError, CapabilityHandle, CapabilityRights,
    CapabilityTarget, ConsumeCapabilityRequest, IssueRootCapabilityRequest,
    RevokeCapabilityRequest, SignedIssueRootCapabilityRequest, SignedRevokeCapabilityRequest,
    issue_root_command_message, revoke_command_message,
};
use nlos_identity::{
    BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose,
    VerifyCapabilityCommandSignatureRequest, VerifySemanticSignatureRequest,
    semantic_signature_message,
};
use nlos_migrate::compute_container_tree_digest;
use nlos_task::{CancelRequest, SqliteTaskAuthority, TaskId, TaskSpec};
use nlos_types::{
    ApplicationId, ArtifactId, Generation, IdempotencyKey, NamespaceId, PackageId, PrincipalId,
    SemanticEventId,
};
use sha2::{Digest, Sha256};

use crate::gate3_update_rollback::{
    EphemeralSandboxDir, load_real_package_entries, materialize_verified_package,
};
use crate::model::{
    CertifyError, CertifyFinding, CertifySeverity, GateExecutionSummary, GateId,
    LoadedCertifyPackage,
};

const Z4_USER_DATA_PURGE_DOMAIN: &[u8] = b"nlos/certify/z4-userdata-purge-consent/v1\0";

/// Fault-injection / testing knobs for Gate 4.
#[derive(Clone, Copy, Debug, Default)]
pub struct Gate4DrillOptions {
    /// Simulates skipping capability revocation during uninstall to prove Gate 4
    /// fails closed with `[CAP-REVOKE-001]` `BLOCKER`.
    pub simulate_unrevoked_capability: bool,
    /// Simulates leaving residual files in the application's private cache
    /// directory to prove Gate 4 fails closed with `[PKG-UNINSTALL-001]` `BLOCKER`.
    pub simulate_residual_private_cache: bool,
    /// Simulates deleting `UserDataContainer` during default uninstall without
    /// a signed `Z4` confirmation receipt to prove Gate 4 fails closed.
    pub simulate_unconsented_user_data_deletion: bool,
}

/// Signed `Z4` Trusted-UI operator confirmation receipt authorizing irreversible
/// destruction of an uninstalled application's `UserDataContainer`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Z4UserDataPurgeConsentReceipt {
    /// Operator principal registered in `IdentityAuthority`.
    pub operator_principal: PrincipalId,
    /// Target `ApplicationId` whose `UserDataContainer` is authorized for purge.
    pub application_id: ApplicationId,
    /// Target `PackageId`.
    pub package_id: PackageId,
    /// Exact SHA-256 tree digest of the `UserDataContainer` being destroyed.
    pub expected_container_tree_digest: [u8; 32],
    /// Domain-separated 32-byte signing digest.
    pub message_digest: [u8; 32],
    /// Ed25519 signature over `message_digest`.
    pub ed25519_signature: [u8; 64],
    /// Timestamp (ms since Unix epoch) when the operator confirmed destruction.
    pub consented_at_ms: u64,
}

/// Computes the domain-separated 32-byte digest signed by a `Z4` operator to
/// authorize `UserDataContainer` destruction.
#[must_use]
pub fn z4_user_data_purge_digest(
    application_id: ApplicationId,
    package_id: PackageId,
    expected_container_tree_digest: [u8; 32],
    consented_at_ms: u64,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(Z4_USER_DATA_PURGE_DOMAIN);
    hasher.update(application_id.as_bytes());
    hasher.update(package_id.as_bytes());
    hasher.update(expected_container_tree_digest);
    hasher.update(consented_at_ms.to_be_bytes());
    hasher.finalize().into()
}

/// Constructs and signs a [`Z4UserDataPurgeConsentReceipt`].
#[must_use]
pub fn sign_z4_user_data_purge_consent(
    operator_signing_key: &SigningKey,
    operator_principal: PrincipalId,
    application_id: ApplicationId,
    package_id: PackageId,
    expected_container_tree_digest: [u8; 32],
    consented_at_ms: u64,
) -> Z4UserDataPurgeConsentReceipt {
    let message_digest = z4_user_data_purge_digest(
        application_id,
        package_id,
        expected_container_tree_digest,
        consented_at_ms,
    );
    let ed25519_signature = operator_signing_key.sign(&message_digest).to_bytes();
    Z4UserDataPurgeConsentReceipt {
        operator_principal,
        application_id,
        package_id,
        expected_container_tree_digest,
        message_digest,
        ed25519_signature,
        consented_at_ms,
    }
}

/// Outcome of [`purge_user_data_container_if_z4_confirmed`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserDataRetentionDisposition {
    /// No `Z4` confirmation presented; `UserDataContainer` preserved 100% intact.
    PreservedByDefault {
        /// Preserved SHA-256 tree digest.
        tree_digest: [u8; 32],
    },
    /// Valid `IdentityAuthority`-verified `Z4` receipt presented; container purged.
    PurgedWithVerifiedZ4Consent {
        /// Remaining file count (`0`).
        remaining_files: usize,
    },
}

/// Enforces the `[PKG-UNINSTALL-001]` `UserDataContainer` retention law:
/// preserves `user_data_container` untouched when `z4_consent` is `None`, fails
/// closed without deleting a single file if `z4_consent` is invalid/forged/stale,
/// and purges `user_data_container` only when `z4_consent` passes
/// `IdentityAuthority` signature and digest verification.
///
/// # Errors
///
/// Returns [`CertifyError::Authority`] (with zero files deleted) if `z4_consent`
/// is supplied but fails cryptographic or target-binding verification.
pub fn purge_user_data_container_if_z4_confirmed(
    identity: &IdentityAuthority,
    user_data_container: &Path,
    application_id: ApplicationId,
    package_id: PackageId,
    z4_consent: Option<&Z4UserDataPurgeConsentReceipt>,
    verified_at_ms: u64,
) -> Result<UserDataRetentionDisposition, CertifyError> {
    let current_digest = compute_container_tree_digest(user_data_container)
        .map_err(|e| CertifyError::Authority(format!("compute UserDataContainer digest: {e}")))?;

    let Some(receipt) = z4_consent else {
        return Ok(UserDataRetentionDisposition::PreservedByDefault {
            tree_digest: current_digest,
        });
    };

    if receipt.application_id != application_id {
        return Err(CertifyError::Authority(
            "Z4 purge consent application_id mismatch".to_owned(),
        ));
    }
    if receipt.package_id != package_id {
        return Err(CertifyError::Authority(
            "Z4 purge consent package_id mismatch".to_owned(),
        ));
    }
    if receipt.expected_container_tree_digest != current_digest {
        return Err(CertifyError::Authority(
            "Z4 purge consent container tree digest does not match current UserDataContainer"
                .to_owned(),
        ));
    }
    let expected_msg = z4_user_data_purge_digest(
        application_id,
        package_id,
        current_digest,
        receipt.consented_at_ms,
    );
    if receipt.message_digest != expected_msg {
        return Err(CertifyError::Authority(
            "Z4 purge consent preimage digest mismatch".to_owned(),
        ));
    }

    identity
        .verify_capability_command_signature(VerifyCapabilityCommandSignatureRequest {
            message_digest: expected_msg,
            principal: receipt.operator_principal,
            signature: receipt.ed25519_signature,
            verified_at_ms,
        })
        .map_err(|e| {
            CertifyError::Authority(format!(
                "IdentityAuthority rejected Z4 UserDataContainer purge signature: {e}"
            ))
        })?;

    clear_directory_files(user_data_container)?;
    let remaining_files = count_all_files(user_data_container)?;
    Ok(UserDataRetentionDisposition::PurgedWithVerifiedZ4Consent { remaining_files })
}

/// Detailed execution output of Gate 4.
#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct Gate4Outcome {
    /// True iff active background tasks blocked premature uninstall with zero dirty state.
    pub active_task_guard_verified: bool,
    /// Number of granted capabilities verified as immediately revoked post-uninstall.
    pub revoked_capabilities_verified: usize,
    /// Residual files found in the application's private cache post-uninstall (`0` required).
    pub residual_private_cache_files: usize,
    /// True iff the cross-application shared `Artifact` remained intact and digest-verified.
    pub shared_artifact_preserved: bool,
    /// True iff `UserDataContainer` remained bitwise identical by default and passed `Z4` gating.
    pub user_data_preserved_by_default_and_z4_gated: bool,
    /// Structured findings emitted by Gate 4.
    pub findings: Vec<CertifyFinding>,
    /// Cryptographically committed summary for receipt inclusion.
    pub summary: GateExecutionSummary,
}

/// Runs Gate 4 zero-residue uninstall & capability revocation audit with
/// default certification options.
///
/// # Errors
///
/// Returns [`CertifyError`] only on unexpected host temp-directory I/O failures.
pub fn run_gate4_uninstall_audit(pkg: &LoadedCertifyPackage) -> Result<Gate4Outcome, CertifyError> {
    run_gate4_uninstall_audit_with_options(pkg, Gate4DrillOptions::default())
}

/// Runs Gate 4 with explicit [`Gate4DrillOptions`].
///
/// # Errors
///
/// Returns [`CertifyError`] only on unexpected host temp-directory I/O failures.
#[allow(clippy::too_many_lines)]
pub fn run_gate4_uninstall_audit_with_options(
    pkg: &LoadedCertifyPackage,
    options: Gate4DrillOptions,
) -> Result<Gate4Outcome, CertifyError> {
    let mut findings = Vec::new();
    let sandbox = EphemeralSandboxDir::new("gate4-uninstall")?;

    let private_cache_dir = sandbox.path().join("app_private_cache");
    let user_data_container = sandbox.path().join("user_data_container");
    let z4_drill_container = sandbox.path().join("user_data_z4_drill_copy");
    fs::create_dir_all(private_cache_dir.join("thumbnails")).map_err(|source| {
        CertifyError::Io {
            context: "create private_cache_dir".to_owned(),
            source,
        }
    })?;
    fs::create_dir_all(user_data_container.join("notes")).map_err(|source| CertifyError::Io {
        context: "create user_data_container".to_owned(),
        source,
    })?;
    fs::create_dir_all(z4_drill_container.join("notes")).map_err(|source| CertifyError::Io {
        context: "create z4_drill_container".to_owned(),
        source,
    })?;

    let identity = IdentityAuthority::open(sandbox.path().join("identity.db"))
        .map_err(|e| CertifyError::Authority(format!("open IdentityAuthority: {e}")))?;
    let capabilities = CapabilityAuthority::open(sandbox.path().join("capabilities"))
        .map_err(|e| CertifyError::Authority(format!("open CapabilityAuthority: {e}")))?;
    let artifacts = ArtifactStore::open(sandbox.path().join("artifacts"))
        .map_err(|e| CertifyError::Authority(format!("open ArtifactStore: {e}")))?;
    let tasks = SqliteTaskAuthority::open(sandbox.path().join("tasks.db"))
        .map_err(|e| CertifyError::Authority(format!("open SqliteTaskAuthority: {e}")))?;
    let applications = ApplicationAuthority::open(sandbox.path().join("applications.db"))
        .map_err(|e| CertifyError::Authority(format!("open ApplicationAuthority: {e}")))?;

    // Bootstrap three distinct principals: publisher, system/Z4 operator, and installed app principal.
    let publisher_key = SigningKey::from_bytes(&[0x61_u8; 32]);
    let publisher = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [0x11_u8; 32],
            control_domain_policy_digest: [0x12_u8; 32],
            public_key: publisher_key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 1_000,
            key_valid_until_ms: 9_000_000_000,
            idempotency_key: IdempotencyKey::from_bytes([0x01_u8; 16]),
            created_at_ms: 1_000,
        })
        .map_err(|e| CertifyError::Authority(format!("bootstrap publisher: {e}")))?
        .binding();

    let operator_key = SigningKey::from_bytes(&[0x62_u8; 32]);
    let operator = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [0x21_u8; 32],
            control_domain_policy_digest: [0x22_u8; 32],
            public_key: operator_key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 1_000,
            key_valid_until_ms: 9_000_000_000,
            idempotency_key: IdempotencyKey::from_bytes([0x02_u8; 16]),
            created_at_ms: 1_000,
        })
        .map_err(|e| CertifyError::Authority(format!("bootstrap operator: {e}")))?
        .binding();

    let app_key = SigningKey::from_bytes(&[0x63_u8; 32]);
    let app_principal = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [0x31_u8; 32],
            control_domain_policy_digest: [0x32_u8; 32],
            public_key: app_key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 1_000,
            key_valid_until_ms: 9_000_000_000,
            idempotency_key: IdempotencyKey::from_bytes([0x03_u8; 16]),
            created_at_ms: 1_000,
        })
        .map_err(|e| CertifyError::Authority(format!("bootstrap app principal: {e}")))?
        .binding();

    // 1. Install verified package.
    let entries = load_real_package_entries(pkg)?;
    let ver_receipt = materialize_verified_package(
        &identity,
        &artifacts,
        &publisher_key,
        publisher.principal_id,
        pkg.package_id,
        pkg.version_ordinal,
        &entries,
        0x15,
        2_000,
    )?;
    let installation = applications
        .install_application(
            &artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: ver_receipt.receipt_id,
                idempotency_key: IdempotencyKey::from_bytes([0x16_u8; 16]),
                installed_at_ms: 2_500,
            },
        )
        .map_err(|e| CertifyError::Authority(format!("install_application: {e}")))?
        .receipt();

    // 2. Grant multiple capabilities to `app_principal` and verify live pre-uninstall consumption.
    let target_ns = CapabilityTarget::Namespace(NamespaceId::from_bytes([0x91_u8; 16]));
    let task_a = TaskId::from_bytes([0xA1_u8; 16]);
    let task_b = TaskId::from_bytes([0xA2_u8; 16]);
    let target_task = CapabilityTarget::Task(task_a);

    let granted_handles = issue_app_capabilities(
        &identity,
        &capabilities,
        &operator_key,
        operator.principal_id,
        operator.key_id,
        app_principal.key_id,
        &[(target_ns, 0x21_u8), (target_task, 0x22_u8)],
        3_000,
    )?;

    // Exercise each granted capability once prior to uninstall.
    for (idx, (handle, target)) in granted_handles.iter().enumerate() {
        let seed = u8::try_from(0x30 + idx).unwrap_or(0x30);
        let signer = build_verified_semantic_signer(
            &identity,
            &app_key,
            app_principal.principal_id,
            app_principal.control_domain_id,
            app_principal.key_id,
            seed,
            3_100,
        )?;
        capabilities
            .consume(ConsumeCapabilityRequest {
                handle: *handle,
                signer,
                target: *target,
                required_right: CapabilityRights::SEMANTIC_APPEND,
                purpose_digest: None,
                idempotency_key: IdempotencyKey::from_bytes([seed; 16]),
                consumed_at_ms: 3_100,
            })
            .map_err(|e| CertifyError::Authority(format!("pre-uninstall consume failed: {e}")))?;
    }

    // 3. Register and start background tasks in `SqliteTaskAuthority` & `ApplicationAuthority`.
    for (idx, task_id) in [task_a, task_b].into_iter().enumerate() {
        tasks
            .register_task(TaskSpec {
                task_id,
                task_generation: Generation::INITIAL,
                registered_at_ms: 3_200,
                application_id: Some(installation.application_id),
                plan_revision: None,
            })
            .map_err(|e| CertifyError::Authority(format!("register_task: {e}")))?;
        let key_byte = u8::try_from(0x41 + idx).unwrap_or(0x41);
        applications
            .register_background_task(RegisterBackgroundTaskRequest {
                package_id: pkg.package_id,
                task_id,
                registrant_principal: publisher.principal_id,
                idempotency_key: IdempotencyKey::from_bytes([key_byte; 16]),
                registered_at_ms: 3_300,
            })
            .map_err(|e| CertifyError::Authority(format!("register_background_task: {e}")))?;
    }

    // 4. Write private cache, cross-app shared Artifact, and UserDataContainer.
    fs::write(
        private_cache_dir.join("query-index.cache"),
        b"ephemeral-derived-query-cache",
    )
    .map_err(|source| CertifyError::Io {
        context: "write query-index.cache".to_owned(),
        source,
    })?;
    fs::write(
        private_cache_dir.join("thumbnails/thumb-01.cache"),
        b"ephemeral-thumbnail-bytes",
    )
    .map_err(|source| CertifyError::Io {
        context: "write thumb-01.cache".to_owned(),
        source,
    })?;
    let cache_key = format!(
        "app-cache/{}",
        crate::json::hex_encode(pkg.package_id.as_bytes())
    );
    artifacts
        .put_cache_blob(&cache_key, b"derived-artifact-store-cache", 3_400)
        .map_err(|e| CertifyError::Authority(format!("put_cache_blob: {e}")))?;

    let shared_artifact_id = ArtifactId::from_bytes([0xE7_u8; 16]);
    let shared_payload = b"cross-application shared user document referenced by peer workspace app";
    let shared_expected_digest = ContentDigest::of_bytes(shared_payload);
    artifacts
        .create_artifact(CreateArtifactSpec {
            artifact_id: shared_artifact_id,
            idempotency_key: IdempotencyKey::from_bytes([0x51_u8; 16]),
            content_type: "text/plain".to_owned(),
            application_id: None,
            owner: Some("shared-workspace-domain".to_owned()),
            created_at_ms: 3_400,
        })
        .map_err(|e| CertifyError::Authority(format!("create shared artifact: {e}")))?;
    artifacts
        .put_revision(PutRevisionRequest {
            artifact_id: shared_artifact_id,
            expected_head_revision: 0,
            bytes: shared_payload,
            created_at_ms: 3_450,
            provenance: ProvenanceSourceTriple {
                source_a: [0xE7_u8; 16],
                source_b: [0x01_u8; 16],
                source_digest: shared_expected_digest,
            },
        })
        .map_err(|e| CertifyError::Authority(format!("put shared artifact revision: {e}")))?;

    for dir in [&user_data_container, &z4_drill_container] {
        fs::write(
            dir.join("notes/personal-notebook.md"),
            b"# User Notebook\nOwned by user; preserved across application uninstall.\n",
        )
        .map_err(|source| CertifyError::Io {
            context: "seed personal-notebook.md".to_owned(),
            source,
        })?;
        fs::write(
            dir.join("preferences.json"),
            b"{\"auto_save\":true,\"export_format\":\"markdown\"}\n",
        )
        .map_err(|source| CertifyError::Io {
            context: "seed preferences.json".to_owned(),
            source,
        })?;
    }
    let pre_uninstall_user_data_digest = compute_container_tree_digest(&user_data_container)
        .map_err(|e| CertifyError::Authority(format!("pre-uninstall tree digest: {e}")))?;

    // 5. Phase 1: Attempt uninstall while background tasks are still Active.
    let premature_uninstall = applications.uninstall_application_with_task_activity_gate(
        &tasks,
        UninstallApplicationRequest {
            package_id: pkg.package_id,
            idempotency_key: IdempotencyKey::from_bytes([0x61_u8; 16]),
            uninstalled_at_ms: 4_000,
        },
    );
    let mut active_task_guard_verified = matches!(
        premature_uninstall,
        Err(ApplicationAuthorityError::ApplicationActiveTasksRunning {
            active_task_count: 2,
            ..
        })
    );
    let mid_app = applications
        .inspect_application(pkg.package_id)
        .map_err(|e| CertifyError::Authority(format!("inspect mid_app: {e}")))?
        .ok_or_else(|| CertifyError::Authority("application missing after guard".to_owned()))?;
    if mid_app.status != ApplicationStatus::Installed {
        active_task_guard_verified = false;
    }
    if !active_task_guard_verified {
        findings.push(CertifyFinding {
            gate: GateId::Gate4ZeroResidueUninstall,
            rule_id: "PKG-UNINSTALL-001".to_owned(),
            severity: CertifySeverity::Blocker,
            subject: "gate4/active-task-guard".to_owned(),
            message:
                "uninstall did not fail closed with ApplicationActiveTasksRunning while background tasks were Active"
                    .to_owned(),
            remediation:
                "Enforce uninstall_application_with_task_activity_gate before transitioning application state."
                    .to_owned(),
        });
    }

    // 6. Phase 2: Cancel active background tasks, then execute clean uninstall + teardown.
    for (idx, task_id) in [task_a, task_b].into_iter().enumerate() {
        let cancel_seed = u8::try_from(0x71 + idx).unwrap_or(0x71);
        tasks
            .cancel_task(CancelRequest {
                task_id,
                idempotency_key: IdempotencyKey::from_bytes([cancel_seed; 16]),
                requested_at_ms: 4_200,
            })
            .map_err(|e| CertifyError::Authority(format!("cancel_task: {e}")))?;
    }

    let uninstall_res = applications
        .uninstall_application_with_task_activity_gate(
            &tasks,
            UninstallApplicationRequest {
                package_id: pkg.package_id,
                idempotency_key: IdempotencyKey::from_bytes([0x75_u8; 16]),
                uninstalled_at_ms: 4_500,
            },
        )
        .map_err(|e| CertifyError::Authority(format!("clean uninstall failed: {e}")))?;
    if !matches!(
        uninstall_res,
        UninstallDecision::Uninstalled(_) | UninstallDecision::Replayed(_)
    ) {
        findings.push(CertifyFinding {
            gate: GateId::Gate4ZeroResidueUninstall,
            rule_id: "PKG-UNINSTALL-001".to_owned(),
            severity: CertifySeverity::Blocker,
            subject: "gate4/uninstall-state".to_owned(),
            message: "application failed to transition to Uninstalled after tasks terminated"
                .to_owned(),
            remediation: "Complete terminal uninstall transition once active task count reaches 0."
                .to_owned(),
        });
    }

    // Revoke every granted capability handle unless fault-injected.
    let mut revocation_results = Vec::new();
    if options.simulate_unrevoked_capability {
        for (handle, target) in &granted_handles {
            revocation_results.push((*handle, handle.generation, *target));
        }
    } else {
        for (idx, (handle, target)) in granted_handles.iter().enumerate() {
            let rev_seed = u8::try_from(0x81 + idx).unwrap_or(0x81);
            let cmd = RevokeCapabilityRequest {
                handle: *handle,
                revoker_key_id: operator.key_id,
                idempotency_key: IdempotencyKey::from_bytes([rev_seed; 16]),
                revoked_at_ms: 4_600,
            };
            let msg = revoke_command_message(cmd);
            let sig = operator_key.sign(&msg).to_bytes();
            let rev_receipt = capabilities
                .revoke_signed(
                    &identity,
                    SignedRevokeCapabilityRequest {
                        command: cmd,
                        signer: operator.principal_id,
                        signature: sig,
                    },
                )
                .map_err(|e| CertifyError::Authority(format!("revoke_signed: {e}")))?
                .receipt();
            revocation_results.push((*handle, rev_receipt.resulting_generation, *target));
        }
    }

    // Sweep private cache directory and derived ArtifactStore cache entry unless fault-injected.
    if !options.simulate_residual_private_cache {
        clear_directory_files(&private_cache_dir)?;
        let _ = artifacts
            .evict_cache_blob(&cache_key)
            .map_err(|e| CertifyError::Authority(format!("evict_cache_blob: {e}")))?;
    }

    if options.simulate_unconsented_user_data_deletion {
        clear_directory_files(&user_data_container)?;
    } else {
        let _ = purge_user_data_container_if_z4_confirmed(
            &identity,
            &user_data_container,
            installation.application_id,
            pkg.package_id,
            None,
            4_700,
        )?;
    }

    // 7. Verify all 5 post-uninstall hard invariants!
    // Invariant (1): Zero orphan active background tasks.
    let outstanding_tasks = tasks
        .inspect_outstanding_task_count(&[task_a, task_b])
        .map_err(|e| CertifyError::Authority(format!("inspect_outstanding_task_count: {e}")))?;
    if outstanding_tasks != 0 {
        findings.push(CertifyFinding {
            gate: GateId::Gate4ZeroResidueUninstall,
            rule_id: "PKG-UNINSTALL-001".to_owned(),
            severity: CertifySeverity::Blocker,
            subject: "gate4/orphan-tasks".to_owned(),
            message: format!(
                "{outstanding_tasks} orphan active background task(s) survived application uninstall"
            ),
            remediation: "Cancel and fence all application background tasks prior to completing uninstall."
                .to_owned(),
        });
    }

    // Invariant (2): Immediate capability revocation (`[CAP-REVOKE-001]`).
    let mut revoked_capabilities_verified = 0_usize;
    for (idx, (old_handle, head_gen, target)) in revocation_results.iter().enumerate() {
        let seed_old = u8::try_from(0x91 + idx * 2).unwrap_or(0x91);
        let signer_old = build_verified_semantic_signer(
            &identity,
            &app_key,
            app_principal.principal_id,
            app_principal.control_domain_id,
            app_principal.key_id,
            seed_old,
            4_800,
        )?;
        let old_attempt = capabilities.consume(ConsumeCapabilityRequest {
            handle: *old_handle,
            signer: signer_old,
            target: *target,
            required_right: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: None,
            idempotency_key: IdempotencyKey::from_bytes([seed_old; 16]),
            consumed_at_ms: 4_800,
        });

        let seed_head = seed_old.wrapping_add(1);
        let signer_head = build_verified_semantic_signer(
            &identity,
            &app_key,
            app_principal.principal_id,
            app_principal.control_domain_id,
            app_principal.key_id,
            seed_head,
            4_800,
        )?;
        let head_attempt = capabilities.consume(ConsumeCapabilityRequest {
            handle: CapabilityHandle {
                capability_id: old_handle.capability_id,
                generation: *head_gen,
            },
            signer: signer_head,
            target: *target,
            required_right: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: None,
            idempotency_key: IdempotencyKey::from_bytes([seed_head; 16]),
            consumed_at_ms: 4_800,
        });

        let old_fenced = matches!(
            old_attempt,
            Err(CapabilityAuthorityError::GenerationFenceConflict
                | CapabilityAuthorityError::CapabilityRevoked)
        );
        let head_revoked = matches!(
            head_attempt,
            Err(CapabilityAuthorityError::CapabilityRevoked)
        );
        if old_fenced && head_revoked {
            revoked_capabilities_verified += 1;
        } else {
            findings.push(CertifyFinding {
                gate: GateId::Gate4ZeroResidueUninstall,
                rule_id: "CAP-REVOKE-001".to_owned(),
                severity: CertifySeverity::Blocker,
                subject: format!("gate4/capability-{idx}"),
                message:
                    "application capability remained consumable after uninstall instead of returning CapabilityRevoked"
                        .to_owned(),
                remediation:
                    "Synchronously revoke every CapabilityId granted to the uninstalled application principal."
                        .to_owned(),
            });
        }
    }

    // Invariant (3): Private cache directory & derived cache blob 100% emptied.
    let residual_dir_files = count_all_files(&private_cache_dir)?;
    let residual_store_cache = usize::from(
        artifacts
            .get_cache_blob(&cache_key)
            .map_err(|e| CertifyError::Authority(format!("get_cache_blob: {e}")))?
            .is_some(),
    );
    let residual_private_cache_files = residual_dir_files + residual_store_cache;
    if residual_private_cache_files != 0 {
        findings.push(CertifyFinding {
            gate: GateId::Gate4ZeroResidueUninstall,
            rule_id: "PKG-UNINSTALL-001".to_owned(),
            severity: CertifySeverity::Blocker,
            subject: "gate4/private-cache".to_owned(),
            message: format!(
                "uninstall left {residual_private_cache_files} residual private cache entry/file(s)"
            ),
            remediation:
                "Purge 100% of application-private cache directories and derived cache blobs during uninstall."
                    .to_owned(),
        });
    }

    // Invariant (4): Cross-application shared Artifact intact and digest-verified.
    let shared_readback = artifacts
        .get_revision(shared_artifact_id, 1, 5_000)
        .map_err(|e| CertifyError::Authority(format!("read shared artifact post-uninstall: {e}")));
    let shared_artifact_preserved = match shared_readback {
        Ok(bytes) => ContentDigest::of_bytes(&bytes) == shared_expected_digest,
        Err(_) => false,
    };
    if !shared_artifact_preserved {
        findings.push(CertifyFinding {
            gate: GateId::Gate4ZeroResidueUninstall,
            rule_id: "PKG-UNINSTALL-001".to_owned(),
            severity: CertifySeverity::Blocker,
            subject: "gate4/shared-artifact".to_owned(),
            message: "cross-application shared Artifact was corrupted or deleted during uninstall"
                .to_owned(),
            remediation:
                "Never delete shared Artifacts from ArtifactStore while external references remain."
                    .to_owned(),
        });
    }

    // Invariant (5): UserDataContainer 100% preserved by default + strictly Z4-gated purge.
    let post_uninstall_user_data_digest = compute_container_tree_digest(&user_data_container)
        .map_err(|e| CertifyError::Authority(format!("post-uninstall tree digest: {e}")))?;
    let default_preserved = post_uninstall_user_data_digest == pre_uninstall_user_data_digest
        && count_all_files(&user_data_container)? == 2;

    // Also drill Z4 purge gate on `z4_drill_container`: forged signature must fail closed,
    // valid Z4 operator signature must purge cleanly to 0 files.
    let mut forged_consent = sign_z4_user_data_purge_consent(
        &operator_key,
        operator.principal_id,
        installation.application_id,
        pkg.package_id,
        pre_uninstall_user_data_digest,
        5_100,
    );
    forged_consent.ed25519_signature[0] ^= 0x80;
    let forged_rejected = purge_user_data_container_if_z4_confirmed(
        &identity,
        &z4_drill_container,
        installation.application_id,
        pkg.package_id,
        Some(&forged_consent),
        5_200,
    )
    .is_err();
    let intact_after_forged = compute_container_tree_digest(&z4_drill_container)
        .map_err(|e| CertifyError::Authority(format!("z4 drill digest: {e}")))?
        == pre_uninstall_user_data_digest;

    let valid_z4 = sign_z4_user_data_purge_consent(
        &operator_key,
        operator.principal_id,
        installation.application_id,
        pkg.package_id,
        pre_uninstall_user_data_digest,
        5_300,
    );
    let valid_purged = matches!(
        purge_user_data_container_if_z4_confirmed(
            &identity,
            &z4_drill_container,
            installation.application_id,
            pkg.package_id,
            Some(&valid_z4),
            5_400,
        ),
        Ok(UserDataRetentionDisposition::PurgedWithVerifiedZ4Consent { remaining_files: 0 })
    );

    let user_data_preserved_by_default_and_z4_gated =
        default_preserved && forged_rejected && intact_after_forged && valid_purged;
    if !user_data_preserved_by_default_and_z4_gated {
        findings.push(CertifyFinding {
            gate: GateId::Gate4ZeroResidueUninstall,
            rule_id: "PKG-UNINSTALL-001".to_owned(),
            severity: CertifySeverity::Blocker,
            subject: "gate4/user-data-container".to_owned(),
            message: format!(
                "UserDataContainer retention invariant violated (default_preserved={default_preserved}, forged_z4_rejected={forged_rejected}, valid_z4_purged={valid_purged})"
            ),
            remediation:
                "Preserve UserDataContainer 100% intact on default uninstall and purge only after verifying a signed Z4 operator confirmation receipt."
                    .to_owned(),
        });
    }

    let mut extra = Vec::new();
    extra.extend_from_slice(&pre_uninstall_user_data_digest);
    extra.extend_from_slice(&(revoked_capabilities_verified as u64).to_be_bytes());
    extra.extend_from_slice(&(residual_private_cache_files as u64).to_be_bytes());

    let blocker_count = findings
        .iter()
        .filter(|f| f.severity == CertifySeverity::Blocker)
        .count();
    let summary_line = format!(
        "Gate 4 (task_guard={active_task_guard_verified}, caps_revoked={revoked_capabilities_verified}, cache_residue={residual_private_cache_files}, user_data_preserved={default_preserved}): {blocker_count} blocker(s)"
    );
    let summary = GateExecutionSummary::from_findings(
        GateId::Gate4ZeroResidueUninstall,
        &findings,
        &extra,
        summary_line,
    );

    Ok(Gate4Outcome {
        active_task_guard_verified,
        revoked_capabilities_verified,
        residual_private_cache_files,
        shared_artifact_preserved,
        user_data_preserved_by_default_and_z4_gated,
        findings,
        summary,
    })
}

#[allow(clippy::too_many_arguments)]
fn issue_app_capabilities(
    identity: &IdentityAuthority,
    capabilities: &CapabilityAuthority,
    operator_key: &SigningKey,
    operator_principal: PrincipalId,
    operator_key_id: nlos_types::KeyId,
    holder_key_id: nlos_types::KeyId,
    targets: &[(CapabilityTarget, u8)],
    issued_at_ms: u64,
) -> Result<Vec<(CapabilityHandle, CapabilityTarget)>, CertifyError> {
    let mut out = Vec::with_capacity(targets.len());
    for &(target, seed) in targets {
        let command = IssueRootCapabilityRequest {
            issuer_key_id: operator_key_id,
            holder_key_id,
            target,
            rights: CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::SEMANTIC_ADJUDICATE),
            purpose_digest: None,
            valid_from_ms: 1_000,
            valid_until_ms: 9_000_000_000,
            delegation_depth_remaining: 1,
            call_limit: None,
            idempotency_key: IdempotencyKey::from_bytes([seed; 16]),
            issued_at_ms,
        };
        let msg = issue_root_command_message(command);
        let signature = operator_key.sign(&msg).to_bytes();
        let record = capabilities
            .issue_root_signed(
                identity,
                SignedIssueRootCapabilityRequest {
                    command,
                    signer: operator_principal,
                    signature,
                },
            )
            .map_err(|e| CertifyError::Authority(format!("issue_root_signed: {e}")))?
            .record();
        out.push((record.handle, target));
    }
    Ok(out)
}

fn build_verified_semantic_signer(
    identity: &IdentityAuthority,
    app_key: &SigningKey,
    principal_id: PrincipalId,
    control_domain_id: nlos_types::ControlDomainId,
    key_id: nlos_types::KeyId,
    seed: u8,
    at_ms: u64,
) -> Result<nlos_identity::VerifiedSemanticSigner, CertifyError> {
    let event_id = SemanticEventId::from_bytes([seed; 32]);
    let msg = semantic_signature_message(event_id);
    let signature = app_key.sign(&msg).to_bytes();
    identity
        .verify_semantic_signature(VerifySemanticSignatureRequest {
            event_id,
            issuer: principal_id,
            control_domain_id,
            key_id,
            signature,
            admitted_at_ms: at_ms,
        })
        .map_err(|e| CertifyError::Authority(format!("verify_semantic_signature: {e}")))
}

fn clear_directory_files(dir: &Path) -> Result<(), CertifyError> {
    if !dir.exists() {
        return Ok(());
    }
    let entries = fs::read_dir(dir).map_err(|source| CertifyError::Io {
        context: format!("read_dir {}", dir.display()),
        source,
    })?;
    for entry_res in entries {
        let entry = entry_res.map_err(|source| CertifyError::Io {
            context: format!("dir_entry {}", dir.display()),
            source,
        })?;
        let path = entry.path();
        let ft = entry.file_type().map_err(|source| CertifyError::Io {
            context: format!("file_type {}", path.display()),
            source,
        })?;
        if ft.is_dir() {
            fs::remove_dir_all(&path).map_err(|source| CertifyError::Io {
                context: format!("remove_dir_all {}", path.display()),
                source,
            })?;
        } else {
            fs::remove_file(&path).map_err(|source| CertifyError::Io {
                context: format!("remove_file {}", path.display()),
                source,
            })?;
        }
    }
    Ok(())
}

fn count_all_files(dir: &Path) -> Result<usize, CertifyError> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut total = 0_usize;
    let entries = fs::read_dir(dir).map_err(|source| CertifyError::Io {
        context: format!("read_dir {}", dir.display()),
        source,
    })?;
    for entry_res in entries {
        let entry = entry_res.map_err(|source| CertifyError::Io {
            context: format!("dir_entry {}", dir.display()),
            source,
        })?;
        let path = entry.path();
        let ft = entry.file_type().map_err(|source| CertifyError::Io {
            context: format!("file_type {}", path.display()),
            source,
        })?;
        if ft.is_dir() {
            total += count_all_files(&path)?;
        } else {
            total += 1;
        }
    }
    Ok(total)
}
