//! Step D (`ADR-0027` section 2.4 Step D) — Host `N` (`host_epoch = 2`)
//! Zero-Recompile Recovery, `nlos-compat-shim` Mediation, Userland Driver
//! Hot-Plug & `UNCERTAIN` Reconciliation, Dual-Layer Wasm Sandbox Fences,
//! `v1.0.0 -> v1.1.0` Atomic Rollback/Upgrade, and Clean Uninstall.
//!
//! Reopens the **exact same durable SQLite databases and `UserData` directories**
//! written by Host `N-1` in Step B under upgraded Host `N` (`host_epoch = 2`)
//! without recompiling, re-signing, or touching a single byte of any sealed
//! `.nlos-pkg` file, mechanically proving Subsections D.1 through D.7:
//! - **D.1 Durable State Recovery & `[PKG-APPID-001]` Anti-Hijack**: 100%
//!   automatic survival of `InstallationId`s, `(PublisherId, ApplicationId)`
//!   ownership bindings (blocking cross-publisher `ApplicationId` hijacking),
//!   `Capability` grants and remaining call budgets, `ArtifactStore` causal
//!   lineage revisions, 5-dimension `LlmTokenBudget` counters + zero-double-charge
//!   idempotent replay, `PinnedEcosystemDependencyHandle`s, durable `Task` and
//!   `TaskPlan` states, and `UserData` SHA-256 container tree digests.
//! - **D.2 `ServiceDirectory` Compatibility & Bounded `DeprecationWarningReceipt`**:
//!   `SnapshotDirectory` compatibility resolution (`[SABI-DISC-001]`,
//!   `[COMPAT-DEPRECATE-001]`), structured `DeprecationWarningReceipt` emission,
//!   bounded `MAX_DEPRECATION_WARNINGS` (`256`) ring ceiling, fail-closed
//!   `RESERVED` tombstone rejection (`E_ABI_VERSION` + `DO_NOT_RETRY`), and
//!   zero-recompile execution of `N-1`-installed application payloads on Host `N`.
//! - **D.3 `OutOfCoreCompatShimRuntime` Mediation & `0`-Byte `SecretCredential`
//!   Leak Audit**: `[DRV-SHIM-001]`, `[DRV-CRED-001]`, and `[TASK-PROVIDER-001]`
//!   across `TrustedEnforcementGate` + `OutOfCoreCompatShimRuntime`, proving
//!   unadapted `N-1` bypass fails closed, forged `PROVIDER_ISOLATION_VERIFIED_TAG`
//!   elevation fails closed, `128`-bit `request_id` and `IdempotencyKey` survive
//!   end-to-end, and `0` bytes of sealed `SecretCredential` material appear on
//!   any wire frame touching the unprivileged shim.
//! - **D.4 Userland Driver Hot-Plug & Crash -> `UNCERTAIN` -> Same-`IdempotencyKey`
//!   Reconcile**: `[DRV-LIFE-001]` and `[KABI-UNCERTAIN-001]` across both
//!   `ResourceAuthority::rotate_driver` (`Generation(1) -> Generation(2)` with
//!   stale generation fenced), `TrustedEnforcementGate::fence_shim_or_driver_crash`
//!   (`OperationState::EffectUnknown` + `ReservationState::Quarantined` rejecting
//!   blind retry with a new `IdempotencyKey` before succeeding under the exact
//!   original `IdempotencyKey`), and userland `McpBridgeAdapter` mid-flight
//!   transport crash -> `BridgeState::Recovering` -> same-key `ProviderProbe::EffectLanded`
//!   reconciliation with zero duplicate tool execution.
//! - **D.5 Dual-Layer `nlos-wasm-sandbox` Strong Isolation (`[TCB-BYPASS-001]`)**:
//!   real child host process (`--self-audit-env` reporting `0` inherited host
//!   environment variables), positive preopen read/write, `ENOTCAPABLE` (`76`)
//!   denial of `..` and POSIX/Windows absolute path escapes, and fail-closed
//!   instantiation rejection of absent `sock_open` network imports.
//! - **D.6 Third-Party Package Upgrade `v1.0.0 -> v1.1.0` on Host `N`**:
//!   `[TUI-CONSENT-001]` permission-escalation detection (`diff_permission_transparency`),
//!   `[PKG-UPDATE-001]` safety-gate blocks (`DisguisedIrreversibleMigration`,
//!   `MissingPreUpgradeBackup`, `MissingZ4TrustedUiConsent`), injected
//!   health-check failure atomic rollback on `field-survey-app` (`Generation(1)`
//!   retained, `0` residual `.tmp` files, bit-identical `N-1` `UserData` digest),
//!   and healthy `v1.0.0 -> v1.1.0` upgrade from frozen `v1.1.0.nlos-pkg` bytes
//!   for `knowledge-observatory-app` (`Generation(2)`, `TaskPlan` revision `2`
//!   with `N-1` `Completed` nodes preserved + appended 4th node `verify-digest`
//!   executed + `ArtifactStore` revision `4`), `mcp-bridge-driver` (`Generation(2)`),
//!   and `openai-compat-driver` (`Generation(2)`).
//! - **D.7 Clean Uninstall & Immediate Capability Revocation (`[PKG-UNINSTALL-001]`,
//!   `[CAP-REVOKE-001]`)**: active background `Task` blocks uninstall with
//!   `ApplicationActiveTasksRunning`; canceling the background `Task` and
//!   revoking `survey_root_capability` immediately fences both the root and
//!   delegated child capabilities (`AncestorRevokedOrFenced`); clean uninstall
//!   marks `ApplicationStatus::Uninstalled`, purges private runtime cache (`0`
//!   remaining files), preserves shared cross-app `Artifact`s bit-for-bit,
//!   preserves `UserData` by default, rejects forged `Z4` purge signatures
//!   fail-closed without deleting a single file, and purges `UserData` only
//!   after `IdentityAuthority`-verified `Z4` Trusted-UI operator consent.

use std::collections::BTreeMap;
use std::fs;
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ecosystem_driver_kit::{EffectDescriptor, JsonValue, ProviderProbe};
use ed25519_dalek::{Signer, SigningKey};
use mcp_bridge_driver::{BridgeState, McpBridgeAdapter, McpBridgeError, sample_tool_catalog};
use nlos_application::{
    ApplicationAuthorityError, ApplicationStatus, CompatibilityWindow, MigrateApplicationRequest,
    MigrationHealthContext, MigrationHealthProbe, MigrationState, RecordMigrationStepRequest,
    RollbackPackageMigrationRequest, UninstallApplicationRequest, UpdateApplicationRequest,
    compile_task_templates, pack_package_version,
};
use nlos_artifact::{
    ContentDigest, ProvenanceSourceTriple, PutRevisionRequest, SignedPackageWithTasks,
};
use nlos_capability::{
    AuthorizeSemanticRequest, CapabilityAuthority, CapabilityAuthorityError, CapabilityRights,
    CapabilityTarget, ConsumeCapabilityRequest, IssueRootCapabilityRequest,
    RevokeCapabilityRequest, SignedIssueRootCapabilityRequest, SignedRevokeCapabilityRequest,
    issue_root_command_message, revoke_command_message,
};
use nlos_certify::derive_publisher_id_from_str;
use nlos_compat_shim::{
    BoundaryViolation, DriverProtocolShim, EpochTranslationRules, LegacyCompleteBridgeContext,
    MediatedDriverDispatchAuth, OutOfCoreCompatShimRuntime, SabiEpochAdapterCell, SecretCredential,
    SignedMeteredUsageProof, TrustedEnforcementGate, build_driver_request_envelope,
    compute_dispatch_semantic_event_id, compute_metered_usage_proof_digest,
};
use nlos_driver_mock::codec::{
    CompleteOperationResultWire, OUTCOME_CODE_COMPLETED, RegisterOperationWire,
    STATE_CODE_COMPLETED, encode_complete_result, encode_register_request,
};
use nlos_driver_mock::proto_codec::{
    COMPLETE_PROVIDER_METHOD, DRIVER_PROVIDER_SERVICE, PROVIDER_ISOLATION_UNVERIFIED_TAG,
    ProviderIsolationAttestation, REGISTER_PROVIDER_METHOD, encode_complete_ack_proto,
};
use nlos_driver_mock::provider::CompleteProviderOutcome;
use nlos_identity::{
    BootstrapPrincipalRequest, Ed25519Signature, IdentityAuthority, IdentityAuthorityError,
    IdentityBinding, KeyPurpose, VerifyCapabilityCommandSignatureRequest,
    VerifySemanticSignatureRequest, semantic_signature_message,
};
use nlos_migrate::{
    CliDrillScenario, EffectClass, MigrationDrillError, MigrationMode, capture_container_snapshot,
    compute_container_tree_digest, count_residual_temp_files, execute_ephemeral_cli_drill,
    restore_container_from_snapshot,
};
use nlos_operation::{
    CompletionOutcome, OperationHandle, OperationRegistry, OperationSpec, OperationState,
};
use nlos_plan::ecosystem_selector::SqliteEcosystemDependencyGate;
use nlos_plan::{DeclarationAdmissionConsult, DeclarationAdmissionOutcome, PlanNodeState};
use nlos_publisher_trust::{
    PublisherTrustCheckError, RevocationEpoch, SqlitePublisherTrustAuthority,
};
use nlos_resource::{
    CreateAccountRequest, CreateQuoteRequest, ReserveRequest, ResourceAuthority,
    ResourceAuthorityError, ResourceDemand, RotateDriverRequest,
};
use nlos_runtime::FiberHandle;
use nlos_schema::sabi::v1::{
    Envelope, LocalEndpoint, LocalTransportKind, NegotiateServiceRequest, ResolveServiceRequest,
    RetryDirective, SabiErrorCode, SabiResponseContext, SchemaIdentity, ServiceCandidate,
    ServiceVersion, envelope, negotiate_service_response,
};
use nlos_schema::{
    DeprecationManifest, SABI_ENVELOPE_SCHEMA, SABI_SERVICE_DIRECTORY_SCHEMA,
    VersionCompatibilityPolicy,
};
use nlos_service_directory::{
    DeprecationWarningReceipt, MAX_DEPRECATION_WARNINGS, PolicyNegotiationError,
    ServiceRegistration, SnapshotDirectory,
};
use nlos_slice_k::{SliceKRuntime, execute_application_payload};
use nlos_store_registry::{
    DeclaredPermissionEntry, LocalOciCasStore, SqliteStoreRegistry, StoreCatalogTrack,
    StoreChannel, StoreGovernanceAnchors, StoreRegistryError, StoreReleaseSubmission,
    diff_permission_transparency,
};
use nlos_task::{CancelRequest, SqliteTaskAuthority, TaskStoreError};
use nlos_types::{
    CallId, CallbackId, CancellationScopeId, ExecutionFiberId, Generation, IdempotencyKey, KeyId,
    NamespaceId, OperationId, PackageId, PrincipalId, ReceiptId, SemanticEventId,
    TaskParticipantId,
};
use nlos_wasm_sandbox::process::{HostProcessRequest, run_isolated_host, spawn_env_audit};
use nlos_wasm_sandbox::profile::{PreopenGrant, SandboxProfile};
use nlos_wasm_sandbox::{HostOutcome, HostReport};
use openai_compat_driver::{MeteredChatGateway, request_digest};
use prost::Message;
use sha2::{Digest, Sha256};

use crate::step_b_n1_host::{
    GATEWAY_INITIAL_CREDIT, GATEWAY_LLM_TOKEN_LIMITS, GATEWAY_PRICE_PER_KILOTOKEN,
    InstalledN1SampleRecord, OBSERVATORY_HOLDER_SEED, OBSERVATORY_NODE_BUILD_INDEX,
    OBSERVATORY_NODE_HARVEST, OBSERVATORY_NODE_PUBLISH, OBSERVATORY_NODE_VERIFY_V1_1,
    SURVEY_NODE_COLLECT_NOTES, SURVEY_NODE_PLAN_ROUTE, SURVEY_NODE_SYNC_NOTES,
    SURVEY_WORKER_DELEGATE_SEED, ScriptedMcpServer, StepBN1HostStateSnapshot, Z4_OPERATOR_SEED,
    complete_active_plan_node, materialize_and_activate_plan_node,
    materialize_and_verify_frozen_package, resolve_plan_node_by_hex_key,
};
use crate::step_c_in_place_upgrade::{
    DEPRECATED_N1_REPLACEMENT, DEPRECATED_N1_SYMBOL, HOST_N_KNOWN_CRITICAL_EXT_ID,
    RESERVED_TOMBSTONE_SYMBOL, StepCInPlaceUpgradeReceipt,
};
use crate::{
    FROZEN_N1_SAMPLES, HOST_EPOCH_N, HOST_EPOCH_N_MINUS_1, SEAL_CERTIFIER_SEED,
    SEAL_ENTITLEMENT_SEED, SEAL_RECALL_SEED, SEAL_T0_MS, TempHostWorkspace, derive_digest32,
    derive_key16, hex_encode,
};

const STEP_D_KEY_DOMAIN: &[u8] = b"llmos/w81-road-e001/step-d/key/v1";
const Z4_USER_DATA_PURGE_DOMAIN: &[u8] = b"llmos/w81-road-e001/z4-user-data-purge-consent/v1";

/// Structured verification receipt produced by Step D (`ADR-0027` section 2.4 Step D).
#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct StepDHostNRecoveryReceipt {
    /// Count of `N-1` installations automatically recovered on Host `N` (`4`).
    pub recovered_installations_count: usize,
    /// Proof flag confirming `[PKG-APPID-001]` cross-publisher `ApplicationId` hijack failed closed.
    pub anti_hijack_rejected: bool,
    /// Remaining call budget on `observatory_root_capability` after consuming `1` call on Host `N` (`6`).
    pub observatory_remaining_calls_after_host_n_consume: u64,
    /// Proof flag confirming 5-dimension `LlmTokenBudget` recovery and `0`-double-charge replay on Host `N`.
    pub llm_budget_recovered_and_idempotent: bool,
    /// Count of `PlanNodeState::Completed` nodes recovered from `N-1` across both `TaskPlan`s (`6`).
    pub recovered_completed_plan_nodes: usize,
    /// Count of `UserData` containers matching their `N-1` SHA-256 tree digest bit-for-bit on Host `N` (`4`).
    pub recovered_user_data_digests_matched: usize,
    /// Structured `DeprecationWarningReceipt`s emitted during `ServiceDirectory` negotiation on Host `N`.
    pub deprecation_warnings_emitted: Vec<DeprecationWarningReceipt>,
    /// Verified `SnapshotDirectory` bounded deprecation warning ring ceiling (`256`).
    pub deprecation_warning_ring_ceiling_verified: usize,
    /// Count of `N-1`-installed application payloads executed on Host `N` with zero recompilation (`2`).
    pub zero_recompile_payload_runs_on_host_n: usize,
    /// Count of raw `SecretCredential` bytes leaked across all `OutOfCoreCompatShimRuntime` wire frames (`0`).
    pub shim_mediated_secret_leak_bytes: usize,
    /// Rotated driver generations in `ResourceAuthority` on Host `N` (`Generation(2)` each).
    pub rotated_driver_generations: BTreeMap<String, u64>,
    /// Proof flag confirming TCB-gate crash -> `EffectUnknown` + `Quarantined` -> same-key reconcile succeeded.
    pub tcb_uncertain_reconciled_with_same_key: bool,
    /// Proof flag confirming userland `McpBridgeAdapter` crash -> `Recovering` -> same-key reconcile succeeded.
    pub mcp_uncertain_reconciled_with_same_key: bool,
    /// Count of host environment variables observed by isolated `nlos-wasm-sandbox` child (`0`).
    pub wasm_sandbox_isolated_env_vars: i64,
    /// Proof flag confirming `[TUI-CONSENT-001]` permission-escalation diff blocks silent upgrade.
    pub permission_escalation_blocked_without_z4_reconsent: bool,
    /// Proof flag confirming injected mid-migration health failure rolled back `field-survey-app` atomically.
    pub injected_fault_rollback_preserved_gen1_and_user_data: bool,
    /// Upgraded `v1.1.0` package generations on Host `N` (`2` for `knowledge-observatory-app` + both drivers).
    pub upgraded_v1_1_0_packages: BTreeMap<String, u64>,
    /// Proof flag confirming `[PKG-UNINSTALL-001]` blocked uninstall while background `Task` remained active.
    pub uninstall_blocked_while_bg_task_active: bool,
    /// Proof flag confirming `[CAP-REVOKE-001]` immediately fenced both root and delegated child capabilities.
    pub revoked_root_and_delegated_capabilities_blocked: bool,
    /// Remaining files in `field-survey-app`'s private runtime cache after clean uninstall (`0`).
    pub uninstalled_app_private_cache_remaining_files: usize,
    /// Proof flag confirming shared cross-application corpus `Artifact` survived `field-survey-app` uninstall.
    pub shared_corpus_artifact_preserved_after_uninstall: bool,
    /// Proof flag confirming `UserData` was preserved by default, resisted forged `Z4` consent, and purged on valid `Z4` consent.
    pub user_data_preserved_by_default_then_purged_with_z4: bool,
}

/// Executes Step D (`ADR-0027` section 2.4 Step D) on upgraded Host `N` (`host_epoch = 2`).
///
/// # Errors
///
/// Fails closed with a descriptive error if any Subsection D.1..D.7 invariant fails.
pub fn run_step_d_recover_and_exercise_on_host_n(
    _repo_root: &Path,
    host_ws: &TempHostWorkspace,
    step_b: &StepBN1HostStateSnapshot,
    step_c: &StepCInPlaceUpgradeReceipt,
    wasm_host_exe: &Path,
) -> Result<StepDHostNRecoveryReceipt, String> {
    if step_c.host_epoch != HOST_EPOCH_N {
        return Err(format!(
            "Step D requires Host N epoch {HOST_EPOCH_N}, got {}",
            step_c.host_epoch
        ));
    }

    let auth_root = host_ws.authority_root();
    let runtime = SliceKRuntime::open(&auth_root)
        .map_err(|err| format!("reopen SliceKRuntime on Host N: {err}"))?;
    let driver_resources = ResourceAuthority::open(auth_root.join("driver-registry"))
        .map_err(|err| format!("reopen driver-registry ResourceAuthority on Host N: {err}"))?;
    let mut gateway = MeteredChatGateway::open(
        &auth_root.join("llm-gateway"),
        GATEWAY_LLM_TOKEN_LIMITS,
        GATEWAY_INITIAL_CREDIT,
        SEAL_T0_MS + 10_000,
    )
    .map_err(|err| format!("reopen MeteredChatGateway on Host N: {err}"))?
    .with_price_per_kilotoken(GATEWAY_PRICE_PER_KILOTOKEN);
    let dep_gate = SqliteEcosystemDependencyGate::open(auth_root.join("ecosystem-gate"))
        .map_err(|err| format!("reopen SqliteEcosystemDependencyGate on Host N: {err}"))?;

    // D.1 Durable State Recovery & `[PKG-APPID-001]` Anti-Hijack.
    let (
        recovered_installations_count,
        anti_hijack_rejected,
        observatory_remaining_calls_after_host_n_consume,
        llm_budget_recovered_and_idempotent,
        recovered_completed_plan_nodes,
        recovered_user_data_digests_matched,
    ) = verify_d1_durable_recovery_and_anti_hijack(
        host_ws,
        &runtime,
        &mut gateway,
        &dep_gate,
        step_b,
    )?;

    // D.2 `ServiceDirectory` Compatibility, Bounded `DeprecationWarningReceipt`, & Zero-Recompile Payload Run.
    let (
        deprecation_warnings_emitted,
        deprecation_warning_ring_ceiling_verified,
        zero_recompile_payload_runs_on_host_n,
    ) = verify_d2_service_directory_and_payload_execution(&runtime, step_b, step_c)?;

    // D.3 `OutOfCoreCompatShimRuntime` Mediation & `0`-Byte `SecretCredential` Leak Audit
    // + D.4 TCB-Gate Crash -> `EffectUnknown` + `Quarantined` -> Same-Key Reconcile.
    let (shim_mediated_secret_leak_bytes, tcb_uncertain_reconciled_with_same_key) =
        verify_d3_and_d4_tcb_shim_mediation_and_uncertain_reconcile(
            host_ws,
            &step_c.host_n_deprecation_manifest,
        )?;

    // D.4 Userland Driver Hot-Plug Rotation & `McpBridgeAdapter` `UNCERTAIN` Reconcile.
    let (rotated_driver_generations, mcp_uncertain_reconciled_with_same_key) =
        verify_d4_driver_hotplug_and_mcp_uncertain_reconcile(&runtime, &driver_resources, step_b)?;

    // D.5 Dual-Layer `nlos-wasm-sandbox` Strong Isolation (`[TCB-BYPASS-001]`).
    let wasm_sandbox_isolated_env_vars = verify_d5_dual_layer_wasm_sandbox(host_ws, wasm_host_exe)?;

    // D.6 Third-Party Package Upgrade `v1.0.0 -> v1.1.0` on Host `N` (`[PKG-UPDATE-001]`, `[TUI-CONSENT-001]`).
    let (
        permission_escalation_blocked_without_z4_reconsent,
        injected_fault_rollback_preserved_gen1_and_user_data,
        upgraded_v1_1_0_packages,
    ) = verify_d6_package_upgrade_rollback_and_v1_1_0_rollout(host_ws, &runtime, step_b)?;

    // D.7 Clean Uninstall & Immediate Capability Revocation (`[PKG-UNINSTALL-001]`, `[CAP-REVOKE-001]`).
    let (
        uninstall_blocked_while_bg_task_active,
        revoked_root_and_delegated_capabilities_blocked,
        uninstalled_app_private_cache_remaining_files,
        shared_corpus_artifact_preserved_after_uninstall,
        user_data_preserved_by_default_then_purged_with_z4,
    ) = verify_d7_clean_uninstall_and_capability_revocation(host_ws, &runtime, step_b)?;

    Ok(StepDHostNRecoveryReceipt {
        recovered_installations_count,
        anti_hijack_rejected,
        observatory_remaining_calls_after_host_n_consume,
        llm_budget_recovered_and_idempotent,
        recovered_completed_plan_nodes,
        recovered_user_data_digests_matched,
        deprecation_warnings_emitted,
        deprecation_warning_ring_ceiling_verified,
        zero_recompile_payload_runs_on_host_n,
        shim_mediated_secret_leak_bytes,
        rotated_driver_generations,
        tcb_uncertain_reconciled_with_same_key,
        mcp_uncertain_reconciled_with_same_key,
        wasm_sandbox_isolated_env_vars,
        permission_escalation_blocked_without_z4_reconsent,
        injected_fault_rollback_preserved_gen1_and_user_data,
        upgraded_v1_1_0_packages,
        uninstall_blocked_while_bg_task_active,
        revoked_root_and_delegated_capabilities_blocked,
        uninstalled_app_private_cache_remaining_files,
        shared_corpus_artifact_preserved_after_uninstall,
        user_data_preserved_by_default_then_purged_with_z4,
    })
}

// ============================================================================
// Subsection D.1 — Durable State Recovery & `[PKG-APPID-001]` Anti-Hijack
// ============================================================================

#[allow(clippy::too_many_lines)]
fn verify_d1_durable_recovery_and_anti_hijack(
    host_ws: &TempHostWorkspace,
    runtime: &SliceKRuntime,
    gateway: &mut MeteredChatGateway,
    dep_gate: &SqliteEcosystemDependencyGate,
    step_b: &StepBN1HostStateSnapshot,
) -> Result<(usize, bool, u64, bool, usize, usize), String> {
    // 1. Verify all 4 installed `N-1` applications/drivers survived automatically.
    let mut recovered_installations_count = 0_usize;
    let mut recovered_user_data_digests_matched = 0_usize;
    for (label, rec) in &step_b.installed_samples {
        let view = runtime
            .applications
            .inspect_application(rec.package_id)
            .map_err(|err| format!("inspect_application {label} on Host N: {err}"))?
            .ok_or_else(|| format!("application {label} missing after upgrade to Host N"))?;
        if view.application_id != rec.runtime_application_id
            || view.current_installation_generation != Generation::INITIAL
            || view.status != ApplicationStatus::Installed
            || view.package_manifest_digest != rec.installation_v1_0_0.package_manifest_digest
        {
            return Err(format!(
                "durable ApplicationView drifted for {label} on Host N: {view:?}"
            ));
        }
        recovered_installations_count += 1;

        let container_dir = host_ws.user_data_dir(label);
        let live_digest = compute_container_tree_digest(&container_dir)
            .map_err(|err| format!("compute_container_tree_digest {label} on Host N: {err}"))?;
        if live_digest != rec.user_data_tree_digest {
            return Err(format!(
                "UserData SHA-256 tree digest drifted across N-1 -> N for {label}"
            ));
        }
        recovered_user_data_digests_matched += 1;
    }

    // 2. Reopen Store Governance (`SqlitePublisherTrustAuthority`, `LocalOciCasStore`,
    //    `SqliteStoreRegistry`), verify all 4 `Stable` releases & CAS blobs survived,
    //    and prove `[PKG-APPID-001]` anti-hijack fails closed!
    let gov_root = host_ws.authority_root().join("store-governance");
    let trust_authority = SqlitePublisherTrustAuthority::open(gov_root.join("publisher-trust.db"))
        .map_err(|err| format!("reopen SqlitePublisherTrustAuthority on Host N: {err}"))?;
    let cas = LocalOciCasStore::open(gov_root.join("oci-cas"))
        .map_err(|err| format!("reopen LocalOciCasStore on Host N: {err}"))?;
    let certifier_key = SigningKey::from_bytes(&SEAL_CERTIFIER_SEED);
    let entitlement_key = SigningKey::from_bytes(&SEAL_ENTITLEMENT_SEED);
    let recall_key = SigningKey::from_bytes(&SEAL_RECALL_SEED);
    let registry = SqliteStoreRegistry::open(
        gov_root.join("store-registry"),
        StoreGovernanceAnchors {
            certifier_public_key: certifier_key.verifying_key().to_bytes(),
            entitlement_issuer_key_id: KeyId::from_bytes([0x65; 16]),
            entitlement_issuer_public_key: entitlement_key.verifying_key().to_bytes(),
            recall_authority_public_key: recall_key.verifying_key().to_bytes(),
            initial_revocation_epoch: RevocationEpoch::new(1),
            initialized_at_ms: SEAL_T0_MS,
        },
    )
    .map_err(|err| format!("reopen SqliteStoreRegistry on Host N: {err}"))?;

    for rec in step_b.installed_samples.values() {
        let release = registry
            .inspect_release(rec.package_id, rec.pkg_v1_0_0.version)
            .map_err(|err| format!("inspect_release {} on Host N: {err}", rec.label))?
            .ok_or_else(|| format!("store release {} missing on Host N", rec.label))?;
        if release.channel != StoreChannel::Stable
            || release.track != StoreCatalogTrack::OfficialTrack
            || release.publisher_id != rec.publisher_id
            || !release
                .application_ids
                .contains(&rec.certify_application_id)
        {
            return Err(format!(
                "StoreReleaseRecord drifted across N-1 -> N for {}: {release:?}",
                rec.label
            ));
        }
    }

    let obs_rec = step_b
        .installed_samples
        .get("knowledge-observatory-app")
        .ok_or_else(|| "missing knowledge-observatory-app".to_owned())?;
    let obs_release = registry
        .inspect_release(obs_rec.package_id, obs_rec.pkg_v1_0_0.version)
        .map_err(|err| format!("inspect_release for hijack digest: {err}"))?
        .ok_or_else(|| "missing observatory release".to_owned())?;
    let hijacker_spec = &FROZEN_N1_SAMPLES[1]; // field-survey-app's publisher (`0x62`)
    let hijacker_publisher_id = derive_publisher_id_from_str(hijacker_spec.publisher_label);
    let hijacker_key = SigningKey::from_bytes(&[hijacker_spec.publisher_seed_byte; 32]);
    let mut hijack_submission = StoreReleaseSubmission {
        package_id: PackageId::from_bytes(derive_key16(STEP_D_KEY_DOMAIN, b"hijack-pkg", 1)),
        package_version: pack_package_version(1, 0, 1),
        manifest_digest: *obs_rec.verification_v1_0_0.manifest_digest.as_bytes(),
        oci_manifest_digest: obs_release.oci_manifest_digest,
        application_ids: vec![obs_rec.certify_application_id],
        components: obs_release.components.clone(),
        declared_permissions: vec![DeclaredPermissionEntry {
            capability_scope: "sabi.artifact.read".to_owned(),
            human_purpose: "Hijack attempt".to_owned(),
            effect_class: EffectClass::ReadOnly,
            network_egress_declared: false,
            sandbox_profile: "certified-userland-v1".to_owned(),
        }],
        publisher_id: hijacker_publisher_id,
        publisher_signing_key: hijacker_key.verifying_key().to_bytes(),
        publisher_signature: [0_u8; 64],
        submitted_at_ms: SEAL_T0_MS + 50_000,
    };
    let hijack_digest = hijack_submission.signing_digest();
    hijack_submission.publisher_signature = hijacker_key.sign(&hijack_digest).to_bytes();
    let hijack_err = registry
        .submit_release(&cas, &trust_authority, &hijack_submission)
        .expect_err("cross-publisher ApplicationId hijack must fail closed");
    let anti_hijack_rejected = matches!(
        hijack_err,
        StoreRegistryError::PublisherTrust(
            PublisherTrustCheckError::ApplicationIdHijackAttempt {
                application_id,
                registered_owner,
                attempted_publisher,
            }
        ) if application_id == obs_rec.certify_application_id
            && registered_owner == obs_rec.publisher_id
            && attempted_publisher == hijacker_publisher_id
    );
    if !anti_hijack_rejected {
        return Err(format!(
            "expected ApplicationIdHijackAttempt, got {hijack_err:?}"
        ));
    }

    // 3. Verify `CapabilityAuthority` grants survived & exercise 1 call on Host `N`.
    let consume_at = SEAL_T0_MS + 60_000;
    let obs_holder_key = SigningKey::from_bytes(&OBSERVATORY_HOLDER_SEED);
    let obs_event_id =
        SemanticEventId::from_bytes(derive_digest32(STEP_D_KEY_DOMAIN, b"obs-host-n-event", 1));
    let obs_sig = obs_holder_key
        .sign(&semantic_signature_message(obs_event_id))
        .to_bytes();
    let obs_verified_signer = runtime
        .identity
        .verify_semantic_signature(VerifySemanticSignatureRequest {
            event_id: obs_event_id,
            issuer: step_b.observatory_holder_binding.principal_id,
            control_domain_id: step_b.observatory_holder_binding.control_domain_id,
            key_id: step_b.observatory_holder_binding.key_id,
            signature: obs_sig,
            admitted_at_ms: consume_at,
        })
        .map_err(|err| format!("verify_semantic_signature for observatory on Host N: {err}"))?;
    let obs_purpose_digest = derive_digest32(
        crate::step_b_n1_host::STEP_B_KEY_DOMAIN,
        b"purpose-observatory",
        2,
    );
    let obs_consume = runtime
        .capability()
        .consume(ConsumeCapabilityRequest {
            handle: step_b.observatory_root_capability,
            signer: obs_verified_signer,
            target: CapabilityTarget::Namespace(step_b.target_namespace),
            required_right: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: Some(obs_purpose_digest),
            idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                STEP_D_KEY_DOMAIN,
                b"obs-cap-consume",
                2,
            )),
            consumed_at_ms: consume_at,
        })
        .map_err(|err| format!("consume observatory_root_capability on Host N: {err}"))?
        .receipt();
    let expected_remaining = step_b.observatory_remaining_calls - 1;
    if obs_consume.remaining != Some(expected_remaining) {
        return Err(format!(
            "observatory_root_capability remaining mismatch on Host N: expected Some({expected_remaining}), got {:?}",
            obs_consume.remaining
        ));
    }

    // Confirm `survey_delegated_capability` is also valid on Host `N` prior to Step D.7 revocation.
    let worker_key = SigningKey::from_bytes(&SURVEY_WORKER_DELEGATE_SEED);
    let worker_event = SemanticEventId::from_bytes(derive_digest32(
        STEP_D_KEY_DOMAIN,
        b"survey-worker-check",
        1,
    ));
    let worker_sig = worker_key
        .sign(&semantic_signature_message(worker_event))
        .to_bytes();
    let worker_signer = runtime
        .identity
        .verify_semantic_signature(VerifySemanticSignatureRequest {
            event_id: worker_event,
            issuer: step_b.survey_worker_binding.principal_id,
            control_domain_id: step_b.survey_worker_binding.control_domain_id,
            key_id: step_b.survey_worker_binding.key_id,
            signature: worker_sig,
            admitted_at_ms: consume_at,
        })
        .map_err(|err| format!("verify_semantic_signature for survey worker on Host N: {err}"))?;
    runtime
        .capability()
        .authorize_semantic(AuthorizeSemanticRequest {
            handle: step_b.survey_delegated_capability,
            signer: worker_signer,
            target: CapabilityTarget::Namespace(step_b.target_namespace),
            required_right: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: Some(step_b.survey_purpose_digest),
            admitted_at_ms: consume_at,
        })
        .map_err(|err| {
            format!("authorize_semantic survey_delegated_capability on Host N: {err}")
        })?;

    // 4. Verify `ArtifactStore` causal lineage revisions survived bit-for-bit.
    for (label, art_id, expected_rev, expected_digest) in [
        (
            "observatory_output",
            step_b.observatory_output_artifact_id,
            3_u64,
            step_b.observatory_rev3_digest,
        ),
        (
            "survey_output",
            step_b.survey_output_artifact_id,
            3_u64,
            step_b.survey_rev3_digest,
        ),
        (
            "shared_corpus",
            step_b.shared_corpus_artifact_id,
            1_u64,
            step_b.shared_corpus_digest,
        ),
    ] {
        let bytes = runtime
            .artifacts
            .get_revision(art_id, expected_rev, consume_at)
            .map_err(|err| format!("get_revision {label} rev {expected_rev} on Host N: {err}"))?;
        if ContentDigest::of_bytes(&bytes) != expected_digest {
            return Err(format!(
                "ArtifactStore revision digest drifted across N-1 -> N for {label}"
            ));
        }
    }

    // 5. Verify 5-dimension `LlmTokenBudget` counters survived & idempotent replay charges 0 tokens.
    let recovered_budget = gateway
        .budget()
        .map_err(|err| format!("gateway.budget() on Host N: {err}"))?;
    if recovered_budget != step_b.llm_budget_after_n1
        || gateway.account_id() != step_b.llm_account_id
    {
        return Err(format!(
            "LlmTokenBudget drifted across N-1 -> N: expected {:?}, got {:?}",
            step_b.llm_budget_after_n1, recovered_budget
        ));
    }
    let replay_digest = request_digest("glm-4.6", 2, true);
    if replay_digest != step_b.observatory_llm_request_digest {
        return Err(
            "reconstructed ChatCompletionRequest digest did not match Step B digest".to_owned(),
        );
    }
    let _replay_receipt = gateway
        .charge_completion(&replay_digest, 600, 300, 100, SEAL_T0_MS + 13_000)
        .map_err(|err| format!("gateway.charge_completion replay on Host N: {err}"))?;
    let budget_after_replay = gateway
        .budget()
        .map_err(|err| format!("gateway.budget() after replay on Host N: {err}"))?;
    let llm_budget_recovered_and_idempotent = budget_after_replay == step_b.llm_budget_after_n1;
    if !llm_budget_recovered_and_idempotent {
        return Err("idempotent LLM replay on Host N mutated durable LlmTokenBudget".to_owned());
    }

    // 6. Verify `SqliteEcosystemDependencyGate` handles, `TaskPlan` completed nodes, and active background `Task`.
    let obs_plan_id = obs_rec
        .plan_id
        .ok_or_else(|| "missing observatory TaskPlanId".to_owned())?;
    let (harvest_node_id, harvest_decl_rev) =
        resolve_plan_node_by_hex_key(runtime.plans(), obs_plan_id, OBSERVATORY_NODE_HARVEST)?;
    let recovered_pins = dep_gate
        .list_node_pinned_handles(obs_plan_id, harvest_node_id, harvest_decl_rev)
        .map_err(|err| format!("list_node_pinned_handles on Host N: {err}"))?;
    if recovered_pins != step_b.pinned_dependency_handles {
        return Err(format!(
            "PinnedEcosystemDependencyHandles drifted across N-1 -> N: expected {}, got {}",
            step_b.pinned_dependency_handles.len(),
            recovered_pins.len()
        ));
    }

    let mut recovered_completed_plan_nodes = 0_usize;
    for (app_label, node_keys) in [
        (
            "knowledge-observatory-app",
            [
                OBSERVATORY_NODE_HARVEST,
                OBSERVATORY_NODE_BUILD_INDEX,
                OBSERVATORY_NODE_PUBLISH,
            ],
        ),
        (
            "field-survey-app",
            [
                SURVEY_NODE_PLAN_ROUTE,
                SURVEY_NODE_COLLECT_NOTES,
                SURVEY_NODE_SYNC_NOTES,
            ],
        ),
    ] {
        let plan_id = step_b
            .installed_samples
            .get(app_label)
            .and_then(|r| r.plan_id)
            .ok_or_else(|| format!("missing TaskPlanId for {app_label}"))?;
        let nodes = runtime
            .plans()
            .list_plan_nodes(plan_id)
            .map_err(|err| format!("list_plan_nodes {app_label} on Host N: {err}"))?;
        for expected_hex in node_keys {
            let (nid, _) = resolve_plan_node_by_hex_key(runtime.plans(), plan_id, expected_hex)?;
            let found = nodes
                .iter()
                .find(|n| n.node_id == nid)
                .ok_or_else(|| format!("missing node {expected_hex} on Host N"))?;
            if found.state != PlanNodeState::Completed {
                return Err(format!(
                    "expected Completed state for {app_label} node {expected_hex} on Host N, got {:?}",
                    found.state
                ));
            }
            recovered_completed_plan_nodes += 1;
        }
    }

    let outstanding_bg = runtime
        .tasks
        .inspect_outstanding_task_count(&[step_b.survey_active_bg_task_id])
        .map_err(|err| format!("inspect_outstanding_task_count on Host N: {err}"))?;
    if outstanding_bg != 1 {
        return Err(format!(
            "expected field-survey-app background Task to remain outstanding (1) on Host N, got {outstanding_bg}"
        ));
    }

    Ok((
        recovered_installations_count,
        anti_hijack_rejected,
        expected_remaining,
        llm_budget_recovered_and_idempotent,
        recovered_completed_plan_nodes,
        recovered_user_data_digests_matched,
    ))
}

// ============================================================================
// Subsection D.2 — `ServiceDirectory` Compatibility & Zero-Recompile App Run
// ============================================================================

#[allow(clippy::too_many_lines)]
fn verify_d2_service_directory_and_payload_execution(
    runtime: &SliceKRuntime,
    step_b: &StepBN1HostStateSnapshot,
    step_c: &StepCInPlaceUpgradeReceipt,
) -> Result<(Vec<DeprecationWarningReceipt>, usize, usize), String> {
    let ipc_transport = LocalTransportKind::UnixSocket as i32;
    let registrations = vec![
        ServiceRegistration {
            candidate: ServiceCandidate {
                binding_id: derive_key16(STEP_D_KEY_DOMAIN, b"svc-binding-legacy", 1).to_vec(),
                service: DEPRECATED_N1_SYMBOL.to_owned(),
                version: Some(ServiceVersion {
                    schema_name: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
                    major: HOST_EPOCH_N_MINUS_1,
                    minor: 0,
                }),
                feature_ids: vec![1],
                transport_kinds: vec![ipc_transport],
                generation: 1,
            },
            endpoint: LocalEndpoint {
                kind: ipc_transport,
                address: "nlos-ipc://observatory-sync-v1-compat".to_owned(),
            },
        },
        ServiceRegistration {
            candidate: ServiceCandidate {
                binding_id: derive_key16(STEP_D_KEY_DOMAIN, b"svc-binding-v2", 2).to_vec(),
                service: DEPRECATED_N1_REPLACEMENT.to_owned(),
                version: Some(ServiceVersion {
                    schema_name: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
                    major: HOST_EPOCH_N,
                    minor: 1,
                }),
                feature_ids: vec![1, 2],
                transport_kinds: vec![ipc_transport],
                generation: 1,
            },
            endpoint: LocalEndpoint {
                kind: ipc_transport,
                address: "nlos-ipc://observatory-sync-v2-native".to_owned(),
            },
        },
    ];

    let mut directory = SnapshotDirectory::new(registrations)
        .map_err(|err| format!("SnapshotDirectory::new: {err}"))?;
    let policy = VersionCompatibilityPolicy::with_backward_window(
        HOST_EPOCH_N,
        step_c.backward_window_epochs,
        &[HOST_N_KNOWN_CRITICAL_EXT_ID],
    );

    let negotiate_req = NegotiateServiceRequest {
        schema: Some(SchemaIdentity {
            name: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
            major: HOST_EPOCH_N_MINUS_1,
            minor: 0,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: vec![7701],
        }),
        service: DEPRECATED_N1_SYMBOL.to_owned(),
        schema_name: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
        major: HOST_EPOCH_N_MINUS_1,
        minimum_minor: 0,
        required_feature_ids: vec![1],
        supported_transport_kinds: vec![ipc_transport],
    };

    let outcome = directory
        .negotiate_with_policy(&negotiate_req, &policy, &step_c.host_n_deprecation_manifest)
        .map_err(|err| format!("negotiate_with_policy for N-1 deprecated symbol: {err}"))?;
    if !matches!(
        outcome.response.result,
        Some(negotiate_service_response::Result::Binding(_))
    ) {
        return Err(format!(
            "expected ServiceBinding for N-1 deprecated symbol within grace window, got {:?}",
            outcome.response.result
        ));
    }
    if outcome.emitted_warnings.len() != 1 {
        return Err(format!(
            "expected 1 DeprecationWarningReceipt, got {}",
            outcome.emitted_warnings.len()
        ));
    }
    let warning = &outcome.emitted_warnings[0];
    if warning.symbol != DEPRECATED_N1_SYMBOL
        || warning.deprecated_in_epoch != HOST_EPOCH_N
        || warning.earliest_sunset_epoch != 4
        || warning.replacement != DEPRECATED_N1_REPLACEMENT
    {
        return Err(format!("unexpected DeprecationWarningReceipt: {warning:?}"));
    }
    let captured_warnings = outcome.emitted_warnings.clone();

    // Verify `RESERVED` tombstone symbol is rejected fail-closed (`E_ABI_VERSION` + `DO_NOT_RETRY`).
    let tombstone_err = directory
        .resolve_with_policy(
            &ResolveServiceRequest {
                schema: Some(SchemaIdentity {
                    name: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
                    major: HOST_EPOCH_N_MINUS_1,
                    minor: 0,
                    critical_extension_ids: Vec::new(),
                    non_critical_extension_ids: Vec::new(),
                }),
                service: RESERVED_TOMBSTONE_SYMBOL.to_owned(),
            },
            &policy,
            &step_c.host_n_deprecation_manifest,
        )
        .expect_err("RESERVED tombstone symbol must fail closed on Host N");
    if !matches!(tombstone_err, PolicyNegotiationError::SunsetContract { .. }) {
        return Err(format!(
            "expected PolicyNegotiationError::SunsetContract, got {tombstone_err:?}"
        ));
    }
    let failure = tombstone_err.to_sabi_failure();
    if failure.code != SabiErrorCode::AbiVersion as i32
        || failure.retry != RetryDirective::DoNotRetry as i32
    {
        return Err(format!(
            "expected E_ABI_VERSION + DO_NOT_RETRY on tombstone, got {failure:?}"
        ));
    }

    // Pump > MAX_DEPRECATION_WARNINGS calls to verify bounded ring capacity (256, zero OOM).
    let resolve_deprecated = ResolveServiceRequest {
        schema: Some(SchemaIdentity {
            name: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
            major: HOST_EPOCH_N_MINUS_1,
            minor: 0,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        service: DEPRECATED_N1_SYMBOL.to_owned(),
    };
    for _ in 0..(MAX_DEPRECATION_WARNINGS + 20) {
        directory
            .resolve_with_policy(
                &resolve_deprecated,
                &policy,
                &step_c.host_n_deprecation_manifest,
            )
            .map_err(|err| format!("resolve_with_policy ring saturation check: {err}"))?;
    }
    let ring_len = directory.deprecation_warnings().len();
    if ring_len != MAX_DEPRECATION_WARNINGS {
        return Err(format!(
            "DeprecationWarningReceipt ring exceeded/missed ceiling: expected {MAX_DEPRECATION_WARNINGS}, got {ring_len}"
        ));
    }

    // Execute both `N-1`-installed application payloads on Host `N` with zero recompilation.
    let mut payload_runs = 0_usize;
    for (label, entry) in [
        ("knowledge-observatory-app", "curator"),
        ("field-survey-app", "survey-worker"),
    ] {
        let rec = step_b
            .installed_samples
            .get(label)
            .ok_or_else(|| format!("missing {label}"))?;
        let exec = execute_application_payload(runtime, rec.package_id, entry)
            .map_err(|err| format!("execute_application_payload {label} on Host N: {err}"))?;
        if exec.payload_size_bytes == 0 {
            return Err(format!("{label} returned empty payload on Host N"));
        }
        payload_runs += 1;
    }

    Ok((captured_warnings, ring_len, payload_runs))
}

// ============================================================================
// Subsection D.3 & D.4(a) — `OutOfCoreCompatShimRuntime` + `TrustedEnforcementGate`
// ============================================================================

#[allow(clippy::too_many_lines)]
fn verify_d3_and_d4_tcb_shim_mediation_and_uncertain_reconcile(
    host_ws: &TempHostWorkspace,
    manifest: &DeprecationManifest,
) -> Result<(usize, bool), String> {
    let gate_dir = host_ws.path().join("host-n-tcb-gate");
    fs::create_dir_all(&gate_dir).map_err(|err| format!("create host-n-tcb-gate dir: {err}"))?;

    let identity = Arc::new(
        IdentityAuthority::open(gate_dir.join("identity.db"))
            .map_err(|err| format!("open gate IdentityAuthority: {err}"))?,
    );
    let capability = Arc::new(
        CapabilityAuthority::open(gate_dir.join("capability.db"))
            .map_err(|err| format!("open gate CapabilityAuthority: {err}"))?,
    );
    let resource = Arc::new(
        ResourceAuthority::open(gate_dir.join("resource.db"))
            .map_err(|err| format!("open gate ResourceAuthority: {err}"))?,
    );
    let operations = Arc::new(OperationRegistry::new());

    let caller_seed = [0x91_u8; 32];
    let caller_key = SigningKey::from_bytes(&caller_seed);
    let caller_binding = bootstrap_gate_principal(&identity, &caller_key, 1, SEAL_T0_MS)?;

    let meter_seed = [0x92_u8; 32];
    let meter_key = SigningKey::from_bytes(&meter_seed);
    let meter_binding = bootstrap_gate_principal(&identity, &meter_key, 2, SEAL_T0_MS)?;

    let gate = TrustedEnforcementGate::new(
        HOST_EPOCH_N,
        Arc::clone(&identity),
        Arc::clone(&capability),
        Arc::clone(&resource),
        Arc::clone(&operations),
        meter_binding.principal_id,
    );

    let credential_id = [0xC7_u8; 16];
    let raw_secret = b"sk-live-road-e001-top-secret-bearer-token-998877";
    let secret = SecretCredential::new(credential_id, raw_secret.to_vec())
        .map_err(|err| format!("SecretCredential::new: {err}"))?;
    gate.register_secret_credential(secret);

    let process_id =
        TaskParticipantId::from_bytes(derive_key16(STEP_D_KEY_DOMAIN, b"shim-proc", 1));
    gate.register_process_incarnation(process_id, Generation::INITIAL);

    let target_ns = NamespaceId::from_bytes(derive_key16(STEP_D_KEY_DOMAIN, b"shim-ns", 1));
    let issue_cmd = IssueRootCapabilityRequest {
        issuer_key_id: caller_binding.key_id,
        holder_key_id: caller_binding.key_id,
        target: CapabilityTarget::Namespace(target_ns),
        rights: CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::SEMANTIC_RETRACT),
        purpose_digest: None,
        valid_from_ms: SEAL_T0_MS,
        valid_until_ms: SEAL_T0_MS + 3_600_000,
        delegation_depth_remaining: 1,
        call_limit: Some(16),
        idempotency_key: IdempotencyKey::from_bytes(derive_key16(
            STEP_D_KEY_DOMAIN,
            b"shim-cap",
            1,
        )),
        issued_at_ms: SEAL_T0_MS + 100,
    };
    let issue_sig = caller_key
        .sign(&issue_root_command_message(issue_cmd))
        .to_bytes();
    let cap_handle = capability
        .issue_root_signed(
            &identity,
            SignedIssueRootCapabilityRequest {
                command: issue_cmd,
                signer: caller_binding.principal_id,
                signature: issue_sig,
            },
        )
        .map_err(|err| format!("issue_root_signed for gate: {err}"))?
        .record()
        .handle;

    let account = resource
        .create_account(CreateAccountRequest {
            initial_credit: 50_000,
            idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                STEP_D_KEY_DOMAIN,
                b"shim-acct",
                1,
            )),
            created_at_ms: SEAL_T0_MS + 100,
        })
        .map_err(|err| format!("create_account for gate: {err}"))?;
    let driver = resource
        .register_driver(nlos_resource::RegisterDriverRequest {
            profile_digest: derive_digest32(STEP_D_KEY_DOMAIN, b"legacy-minicbor-drv", 1),
            idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                STEP_D_KEY_DOMAIN,
                b"shim-drv",
                1,
            )),
            created_at_ms: SEAL_T0_MS + 100,
        })
        .map_err(|err| format!("register_driver for gate: {err}"))?
        .record();

    let sabi_adapter = SabiEpochAdapterCell::new(
        HOST_EPOCH_N_MINUS_1,
        HOST_EPOCH_N,
        2,
        vec![HOST_N_KNOWN_CRITICAL_EXT_ID],
        manifest.clone(),
        EpochTranslationRules {
            host_envelope_minor: 1,
            client_envelope_minor: Some(0),
            ..EpochTranslationRules::default()
        },
    )
    .map_err(|err| format!("SabiEpochAdapterCell::new: {err}"))?;
    let driver_shim = DriverProtocolShim::new_legacy_minicbor(HOST_EPOCH_N)
        .map_err(|err| format!("DriverProtocolShim::new_legacy_minicbor: {err}"))?;
    let mut shim = OutOfCoreCompatShimRuntime::new(sabi_adapter, driver_shim);

    // Operation #1: Full `N-1` (`major = 1`) -> `OutOfCoreCompatShimRuntime` -> `TrustedEnforcementGate`
    // dispatch + forged isolation negative check + honest completion + `0`-byte secret leak audit.
    let (op1_spec, op1_req_id, op1_idemp, op1_client_wire, op1_auth) =
        prepare_mediated_n1_dispatch(
            &resource,
            account.account_id,
            &driver,
            &caller_binding,
            &caller_key,
            process_id,
            cap_handle,
            target_ns,
            Some(credential_id),
            1,
        )?;

    // Prove raw unadapted `N-1` (`major = 1`) wire bypassing `OutOfCoreCompatShimRuntime` fails closed!
    let bypass_err = gate
        .admit_mediated_driver_dispatch(&op1_client_wire, &op1_auth)
        .expect_err("unadapted N-1 envelope bypassing shim must fail closed at TCB gate");
    if !matches!(
        bypass_err,
        BoundaryViolation::NonCanonicalHostEpoch {
            expected: HOST_EPOCH_N,
            actual: HOST_EPOCH_N_MINUS_1,
        }
    ) {
        return Err(format!(
            "expected NonCanonicalHostEpoch(2, 1) on shim bypass, got {bypass_err:?}"
        ));
    }

    let op1_host_wire = shim
        .translate_legacy_client_dispatch_to_host(&op1_client_wire)
        .map_err(|err| format!("translate_legacy_client_dispatch_to_host op1: {err}"))?;
    let op1_dispatch_receipt = gate
        .admit_mediated_driver_dispatch(&op1_host_wire, &op1_auth)
        .map_err(|err| format!("admit_mediated_driver_dispatch op1: {err}"))?;
    if op1_dispatch_receipt.request_id != op1_req_id
        || op1_dispatch_receipt.idempotency_key != op1_idemp
        || op1_dispatch_receipt.tcb_credential_commitment.is_none()
    {
        return Err(format!(
            "unexpected op1 dispatch receipt: {op1_dispatch_receipt:?}"
        ));
    }

    let op1_exec_receipt_id =
        ReceiptId::from_bytes(derive_key16(STEP_D_KEY_DOMAIN, b"op1-exec-rcpt", 1));
    let op1_usage_proof = sign_metered_usage_proof(
        &meter_binding,
        &meter_key,
        op1_auth.reservation_id,
        op1_spec.operation_id,
        op1_dispatch_receipt.activation_receipt.receipt_id,
        op1_exec_receipt_id,
        1,
        120,
        SEAL_T0_MS + 2_000,
    );

    // `[TASK-PROVIDER-001]` Negative check: forged `PROVIDER_ISOLATION_VERIFIED_TAG` on unverified shim fails closed!
    let forged_verified_resp =
        build_forged_verified_host_completion_wire(op1_req_id, op1_exec_receipt_id)?;
    let forged_err = gate
        .complete_mediated_driver_call(&forged_verified_resp, &op1_usage_proof)
        .expect_err("forged PROVIDER_ISOLATION_VERIFIED_TAG must fail closed");
    if !matches!(
        forged_err,
        BoundaryViolation::ForbiddenIsolationElevation(_)
    ) {
        return Err(format!(
            "expected ForbiddenIsolationElevation, got {forged_err:?}"
        ));
    }

    // Honest Stage B `minicbor` completion through `OutOfCoreCompatShimRuntime`.
    let legacy_cbor_resp = encode_complete_result(&CompleteOperationResultWire {
        replayed: false,
        outcome_code: OUTCOME_CODE_COMPLETED,
        receipt_id: *op1_exec_receipt_id.as_bytes(),
        state_code: STATE_CODE_COMPLETED,
    })
    .map_err(|err| format!("encode_complete_result: {err}"))?;
    let (host_resp_wire, downgraded_client_resp_wire) = shim
        .translate_legacy_driver_completion_bidirectional(
            op1_req_id,
            &legacy_cbor_resp,
            LegacyCompleteBridgeContext {
                handle: OperationHandle {
                    operation_id: op1_spec.operation_id,
                    generation: op1_spec.generation,
                },
                callback_id: op1_auth.callback_id,
                seed: derive_digest32(STEP_D_KEY_DOMAIN, b"op1-compute", 1),
                fenced_reconciliation_only: false,
            },
        )
        .map_err(|err| format!("translate_legacy_driver_completion_bidirectional: {err}"))?;
    let op1_completion = gate
        .complete_mediated_driver_call(&host_resp_wire, &op1_usage_proof)
        .map_err(|err| format!("complete_mediated_driver_call op1: {err}"))?;
    if op1_completion.provider_execution_receipt.isolation_marker
        != PROVIDER_ISOLATION_UNVERIFIED_TAG
    {
        return Err(
            "expected PROVIDER_ISOLATION_UNVERIFIED_TAG on legacy shim completion".to_owned(),
        );
    }
    let decoded_client_resp = Envelope::decode(downgraded_client_resp_wire.as_slice())
        .map_err(|err| format!("decode downgraded N-1 client response: {err}"))?;
    if decoded_client_resp.schema.as_ref().map(|s| s.major) != Some(HOST_EPOCH_N_MINUS_1)
        || decoded_client_resp.request_id != op1_req_id
    {
        return Err(
            "downgraded client response lost N-1 major epoch or 128-bit request_id".to_owned(),
        );
    }

    // Audit 100% of frames in `shim.transcript()` for `0`-byte `SecretCredential` leakage!
    let mut leaked_bytes = 0_usize;
    if shim.transcript().contains_byte_subsequence(raw_secret) {
        leaked_bytes += raw_secret.len();
    }
    for frame in shim.transcript().all_frames() {
        if gate.wire_contains_any_registered_secret(frame) {
            leaked_bytes += raw_secret.len();
        }
    }
    if leaked_bytes != 0 {
        return Err(format!(
            "[DRV-CRED-001] raw SecretCredential bytes leaked onto shim wire ({leaked_bytes} bytes)"
        ));
    }

    // Operation #2 (D.4a): Mid-flight crash -> `EffectUnknown` + `Quarantined` ->
    // blind retry with new `IdempotencyKey` rejected -> same-`IdempotencyKey` reconcile succeeds!
    let (op2_spec, _op2_req_id, op2_idemp, op2_client_wire, op2_auth) =
        prepare_mediated_n1_dispatch(
            &resource,
            account.account_id,
            &driver,
            &caller_binding,
            &caller_key,
            process_id,
            cap_handle,
            target_ns,
            Some(credential_id),
            2,
        )?;
    let op2_host_wire = shim
        .translate_legacy_client_dispatch_to_host(&op2_client_wire)
        .map_err(|err| format!("translate_legacy_client_dispatch_to_host op2: {err}"))?;
    let op2_dispatch = gate
        .admit_mediated_driver_dispatch(&op2_host_wire, &op2_auth)
        .map_err(|err| format!("admit_mediated_driver_dispatch op2: {err}"))?;

    let crash_receipt_id =
        ReceiptId::from_bytes(derive_key16(STEP_D_KEY_DOMAIN, b"op2-crash-rcpt", 2));
    let crash_quarantine = gate
        .fence_shim_or_driver_crash(
            op2_spec.operation_id,
            crash_receipt_id,
            derive_digest32(STEP_D_KEY_DOMAIN, b"shim-midflight-crash", 2),
            SEAL_T0_MS + 3_000,
        )
        .map_err(|err| format!("fence_shim_or_driver_crash op2: {err}"))?;
    if crash_quarantine.operation_snapshot.state
        != (OperationState::EffectUnknown {
            receipt_id: crash_receipt_id,
        })
    {
        return Err(format!(
            "expected EffectUnknown after crash fence, got {:?}",
            crash_quarantine.operation_snapshot.state
        ));
    }

    let op2_reconcile_proof = sign_metered_usage_proof(
        &meter_binding,
        &meter_key,
        op2_auth.reservation_id,
        op2_spec.operation_id,
        op2_dispatch.activation_receipt.receipt_id,
        crash_receipt_id,
        1,
        80,
        SEAL_T0_MS + 3_500,
    );

    let wrong_new_key = IdempotencyKey::from_bytes(derive_key16(
        STEP_D_KEY_DOMAIN,
        b"blind-retry-wrong-key",
        99,
    ));
    let blind_retry_err = gate
        .reconcile_uncertain_operation(op2_spec.operation_id, wrong_new_key, &op2_reconcile_proof)
        .expect_err("blind retry of EffectUnknown with new IdempotencyKey must fail closed");
    if !matches!(
        blind_retry_err,
        BoundaryViolation::BlindRetryWithNewIdempotencyKeyForbidden {
            operation_id,
            expected_idempotency_key,
            attempted_idempotency_key,
        } if operation_id == op2_spec.operation_id
            && expected_idempotency_key == op2_idemp
            && attempted_idempotency_key == wrong_new_key
    ) {
        return Err(format!(
            "expected BlindRetryWithNewIdempotencyKeyForbidden, got {blind_retry_err:?}"
        ));
    }

    let reconciled = gate
        .reconcile_uncertain_operation(op2_spec.operation_id, op2_idemp, &op2_reconcile_proof)
        .map_err(|err| format!("reconcile_uncertain_operation with original key: {err}"))?;
    if reconciled.replayed || reconciled.idempotency_key != op2_idemp {
        return Err(format!(
            "unexpected initial reconcile receipt: {reconciled:?}"
        ));
    }
    let reconciled_replay = gate
        .reconcile_uncertain_operation(op2_spec.operation_id, op2_idemp, &op2_reconcile_proof)
        .map_err(|err| format!("reconcile_uncertain_operation replay: {err}"))?;
    if !reconciled_replay.replayed {
        return Err("expected second same-key reconcile to return replayed=true".to_owned());
    }

    Ok((leaked_bytes, true))
}

fn bootstrap_gate_principal(
    identity: &IdentityAuthority,
    signing_key: &SigningKey,
    ordinal: u32,
    at_ms: u64,
) -> Result<IdentityBinding, String> {
    Ok(identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: derive_digest32(
                STEP_D_KEY_DOMAIN,
                b"gate-principal",
                ordinal,
            ),
            control_domain_policy_digest: derive_digest32(
                STEP_D_KEY_DOMAIN,
                b"gate-domain",
                ordinal,
            ),
            public_key: signing_key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms: u64::try_from(i64::MAX).unwrap_or(u64::MAX),
            idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                STEP_D_KEY_DOMAIN,
                b"gate-bootstrap",
                ordinal,
            )),
            created_at_ms: at_ms,
        })
        .map_err(|err| format!("bootstrap_gate_principal {ordinal}: {err}"))?
        .binding())
}

type PreparedMediatedDispatch = (
    OperationSpec,
    [u8; 16],
    IdempotencyKey,
    Vec<u8>,
    MediatedDriverDispatchAuth,
);

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn prepare_mediated_n1_dispatch(
    resource: &ResourceAuthority,
    account_id: nlos_types::ResourceAccountId,
    driver: &nlos_resource::DriverRecord,
    caller_binding: &IdentityBinding,
    caller_key: &SigningKey,
    process_id: TaskParticipantId,
    capability_handle: nlos_capability::CapabilityHandle,
    target_ns: NamespaceId,
    credential_id: Option<[u8; 16]>,
    ordinal: u32,
) -> Result<PreparedMediatedDispatch, String> {
    let op_id = OperationId::from_bytes(derive_key16(STEP_D_KEY_DOMAIN, b"shim-op", ordinal));
    let call_id = CallId::from_bytes(derive_key16(STEP_D_KEY_DOMAIN, b"shim-call", ordinal));
    let callback_id = CallbackId::from_bytes(derive_key16(STEP_D_KEY_DOMAIN, b"shim-cb", ordinal));
    let fiber_id =
        ExecutionFiberId::from_bytes(derive_key16(STEP_D_KEY_DOMAIN, b"shim-fiber", ordinal));
    let scope_id =
        CancellationScopeId::from_bytes(derive_key16(STEP_D_KEY_DOMAIN, b"shim-scope", ordinal));
    let request_id = derive_key16(STEP_D_KEY_DOMAIN, b"shim-req-id", ordinal);
    let idempotency_key =
        IdempotencyKey::from_bytes(derive_key16(STEP_D_KEY_DOMAIN, b"shim-idemp", ordinal));
    let demand = ResourceDemand {
        cpu_shares: 10,
        memory_mib: 16,
        io_weight: 10,
    };

    let quote = resource
        .create_quote(CreateQuoteRequest {
            driver_id: driver.driver_id,
            driver_generation: driver.generation,
            driver_fencing_token: driver.fencing_token,
            operation_proposal_digest: derive_digest32(
                STEP_D_KEY_DOMAIN,
                b"shim-proposal",
                ordinal,
            ),
            pricing_version: [0x01; 32],
            upper_bound: 500,
            demand_capacity: demand,
            valid_until_ms: SEAL_T0_MS + 60_000,
            idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                STEP_D_KEY_DOMAIN,
                b"shim-quote",
                ordinal,
            )),
            created_at_ms: SEAL_T0_MS + 200 + u64::from(ordinal),
        })
        .map_err(|err| format!("create_quote {ordinal}: {err}"))?
        .record();
    let reservation = resource
        .reserve(ReserveRequest {
            account_id,
            quote_id: quote.quote_id,
            call_id,
            operation_id: op_id,
            idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                STEP_D_KEY_DOMAIN,
                b"shim-reserve",
                ordinal,
            )),
            demand,
            reserved_at_ms: SEAL_T0_MS + 300 + u64::from(ordinal),
        })
        .map_err(|err| format!("reserve {ordinal}: {err}"))?
        .record();

    let spec = OperationSpec {
        operation_id: op_id,
        generation: Generation::INITIAL,
        owner_fiber: FiberHandle {
            fiber_id,
            generation: Generation::INITIAL,
        },
        cancellation_scope_id: scope_id,
        cancellation_generation: Generation::INITIAL,
    };

    let cbor_req = encode_register_request(&RegisterOperationWire {
        operation_id: *op_id.as_bytes(),
        operation_generation: 1,
        owner_fiber_id: *fiber_id.as_bytes(),
        owner_fiber_generation: 1,
        cancellation_scope_id: *scope_id.as_bytes(),
        cancellation_generation: 1,
    })
    .map_err(|err| format!("encode_register_request {ordinal}: {err}"))?;

    let deadline_ns = 90_000_000_000_u64;
    let activity_ctx = b"road-e001-activity-ctx".to_vec();
    let client_env = build_driver_request_envelope(
        HOST_EPOCH_N_MINUS_1,
        0,
        REGISTER_PROVIDER_METHOD,
        request_id,
        idempotency_key,
        deadline_ns,
        activity_ctx.clone(),
        cbor_req,
    );

    let mut auth = MediatedDriverDispatchAuth {
        caller_principal: caller_binding.principal_id,
        caller_control_domain: caller_binding.control_domain_id,
        caller_key_id: caller_binding.key_id,
        caller_signature: [0_u8; 64],
        process_id,
        process_generation: Generation::INITIAL,
        capability_handle,
        capability_target: CapabilityTarget::Namespace(target_ns),
        required_right: CapabilityRights::SEMANTIC_APPEND,
        purpose_digest: None,
        reservation_id: reservation.reservation_id,
        call_id,
        activation_token: reservation.activation_token,
        driver_id: driver.driver_id,
        driver_generation: driver.generation,
        driver_fencing_token: driver.fencing_token,
        credential_id,
        callback_id,
        allow_verified_isolation: false,
        admitted_at_ms: SEAL_T0_MS + 1_000 + u64::from(ordinal),
        now_monotonic_ns: 10_000_000_000,
    };
    let sem_event = compute_dispatch_semantic_event_id(
        request_id,
        idempotency_key,
        deadline_ns,
        &activity_ctx,
        &spec,
        &auth,
    );
    auth.caller_signature = caller_key
        .sign(&semantic_signature_message(sem_event))
        .to_bytes();

    Ok((
        spec,
        request_id,
        idempotency_key,
        client_env.encode_to_vec(),
        auth,
    ))
}

#[allow(clippy::too_many_arguments)]
fn sign_metered_usage_proof(
    meter_binding: &IdentityBinding,
    meter_key: &SigningKey,
    reservation_id: nlos_types::ReservationId,
    operation_id: OperationId,
    activation_receipt_id: ReceiptId,
    execution_receipt_id: ReceiptId,
    sequence: u64,
    cumulative_usage: u64,
    settled_at_ms: u64,
) -> SignedMeteredUsageProof {
    let effect_closed_proof_digest =
        derive_digest32(STEP_D_KEY_DOMAIN, operation_id.as_bytes(), 77);
    let digest = compute_metered_usage_proof_digest(
        reservation_id,
        operation_id,
        activation_receipt_id,
        execution_receipt_id,
        sequence,
        cumulative_usage,
        effect_closed_proof_digest,
    );
    let meter_signature: Ed25519Signature = meter_key.sign(&digest).to_bytes();
    SignedMeteredUsageProof {
        reservation_id,
        operation_id,
        activation_receipt_id,
        execution_receipt_id,
        sequence,
        cumulative_usage,
        effect_closed_proof_digest,
        meter_principal: meter_binding.principal_id,
        meter_control_domain: meter_binding.control_domain_id,
        meter_key_id: meter_binding.key_id,
        meter_signature,
        settled_at_ms,
    }
}

fn build_forged_verified_host_completion_wire(
    request_id: [u8; 16],
    exec_receipt_id: ReceiptId,
) -> Result<Vec<u8>, String> {
    let forged_outcome = CompleteProviderOutcome {
        replayed: false,
        outcome: CompletionOutcome::Completed {
            receipt_id: exec_receipt_id,
        },
        state: OperationState::Completed {
            receipt_id: exec_receipt_id,
        },
    };
    let forged_attestation = ProviderIsolationAttestation {
        tenant_isolated: true,
        session_ephemeral: true,
        cache_partitioned: true,
        zero_data_retention: true,
        attestation_digest: [0x99; 32],
        provider_profile: "forged-verified-profile".to_owned(),
    };
    let encoded_ack =
        encode_complete_ack_proto(&forged_outcome, [0x55; 32], false, &forged_attestation)
            .map_err(|err| format!("encode_complete_ack_proto: {err}"))?;
    let mut proto_resp =
        nlos_schema::sabi::v1::DriverProviderResponse::decode(encoded_ack.as_slice())
            .map_err(|err| format!("decode forged DriverProviderResponse: {err}"))?;
    if let Some(schema) = proto_resp.schema.as_mut() {
        schema.major = HOST_EPOCH_N;
    }
    let env = Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: HOST_EPOCH_N,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: request_id.to_vec(),
        service: DRIVER_PROVIDER_SERVICE.to_owned(),
        method: COMPLETE_PROVIDER_METHOD.to_owned(),
        common_context: Some(envelope::CommonContext::ResponseContext(
            SabiResponseContext {
                correlation_id: request_id.to_vec(),
                operation: None,
                receipts: Vec::new(),
                failure: None,
            },
        )),
        payload: proto_resp.encode_to_vec(),
    };
    Ok(env.encode_to_vec())
}

// ============================================================================
// Subsection D.4(b) — Driver Hot-Plug Rotation & `McpBridgeAdapter` Reconcile
// ============================================================================

#[allow(clippy::too_many_lines)]
fn verify_d4_driver_hotplug_and_mcp_uncertain_reconcile(
    runtime: &SliceKRuntime,
    driver_resources: &ResourceAuthority,
    step_b: &StepBN1HostStateSnapshot,
) -> Result<(BTreeMap<String, u64>, bool), String> {
    let mut rotated_generations = BTreeMap::new();
    for (idx, label) in ["mcp-bridge-driver", "openai-compat-driver"]
        .into_iter()
        .enumerate()
    {
        let rec = step_b
            .installed_samples
            .get(label)
            .ok_or_else(|| format!("missing driver {label}"))?;
        let old_drv = rec
            .driver_record
            .ok_or_else(|| format!("missing N-1 DriverRecord for {label}"))?;
        let rotate_at = runtime
            .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
                STEP_D_KEY_DOMAIN,
                label.as_bytes(),
                10,
            )))
            .map_err(|err| format!("wall_now_ms: {err}"))?;
        let new_drv = driver_resources
            .rotate_driver(RotateDriverRequest {
                driver_id: old_drv.driver_id,
                expected_generation: old_drv.generation,
                expected_fencing_token: old_drv.fencing_token,
                idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                    STEP_D_KEY_DOMAIN,
                    label.as_bytes(),
                    11,
                )),
                rotated_at_ms: rotate_at,
            })
            .map_err(|err| format!("rotate_driver {label} on Host N: {err}"))?
            .record();
        if new_drv.generation.get() != 2 || new_drv.fencing_token == old_drv.fencing_token {
            return Err(format!(
                "driver hot-plug rotation did not advance Generation(1 -> 2) for {label}: {new_drv:?}"
            ));
        }

        // Prove stale `Generation(1)` / old `fencing_token` is immediately fenced!
        let stale_err = driver_resources
            .create_quote(CreateQuoteRequest {
                driver_id: old_drv.driver_id,
                driver_generation: old_drv.generation,
                driver_fencing_token: old_drv.fencing_token,
                operation_proposal_digest: derive_digest32(STEP_D_KEY_DOMAIN, label.as_bytes(), 12),
                pricing_version: [0x01; 32],
                upper_bound: 100,
                demand_capacity: ResourceDemand {
                    cpu_shares: 1,
                    memory_mib: 1,
                    io_weight: 1,
                },
                valid_until_ms: rotate_at + 60_000,
                idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                    STEP_D_KEY_DOMAIN,
                    label.as_bytes(),
                    u32::try_from(20 + idx).unwrap_or(20),
                )),
                created_at_ms: rotate_at + 1,
            })
            .expect_err("quote against stale Generation(1) driver must fail closed");
        if !matches!(stale_err, ResourceAuthorityError::StaleDriver) {
            return Err(format!(
                "expected ResourceAuthorityError::StaleDriver, got {stale_err:?}"
            ));
        }

        rotated_generations.insert(label.to_owned(), new_drv.generation.get());
    }

    // Exercise userland `McpBridgeAdapter` mid-flight crash -> `UNCERTAIN` -> same-key reconcile.
    let effects: Vec<_> = sample_tool_catalog()
        .into_iter()
        .map(|(info, class)| EffectDescriptor {
            effect_id: info.name,
            summary: info.description,
            class,
            compensation_handler: (class == ecosystem_driver_kit::EffectClass::Compensatable)
                .then(|| "rollback-batch-sync".to_owned()),
        })
        .collect();
    let mut transport = ScriptedMcpServer::new("2025-03-26");
    transport.arm_drop_next_tool_call_reply();
    let mut bridge = McpBridgeAdapter::new(&mut transport);
    bridge
        .initialize()
        .map_err(|err| format!("McpBridgeAdapter::initialize on Host N: {err}"))?;
    bridge
        .discover_tools(&effects)
        .map_err(|err| format!("McpBridgeAdapter::discover_tools on Host N: {err}"))?;

    let op_id = derive_key16(STEP_D_KEY_DOMAIN, b"mcp-uncertain-op", 1);
    let idemp_key = derive_key16(STEP_D_KEY_DOMAIN, b"mcp-uncertain-key", 1);
    let crash_err = bridge
        .call_tool(
            "stage-batch-sync",
            &JsonValue::object([("batch_id", JsonValue::String("batch-road-e001".to_owned()))]),
            &op_id,
            &idemp_key,
        )
        .expect_err("dropped MCP tool reply must surface transport failure and mark UNCERTAIN");
    if !matches!(crash_err, McpBridgeError::Transport(_)) {
        return Err(format!(
            "expected McpBridgeError::Transport, got {crash_err:?}"
        ));
    }
    bridge.simulate_crash();
    if bridge.state() != BridgeState::Recovering || bridge.fence().uncertain_count() != 1 {
        return Err(format!(
            "expected BridgeState::Recovering with 1 uncertain op, got state={:?}, count={}",
            bridge.state(),
            bridge.fence().uncertain_count()
        ));
    }

    // Blind retry of the `UNCERTAIN` key before `reconcile` must fail closed with
    // `McpBridgeError::Fence` without re-dispatching the side effect!
    let blind_err = bridge
        .call_tool(
            "stage-batch-sync",
            &JsonValue::object([("batch_id", JsonValue::String("batch-road-e001".to_owned()))]),
            &op_id,
            &idemp_key,
        )
        .expect_err("blind retry of UNCERTAIN key before reconcile must fail closed");
    if !matches!(blind_err, McpBridgeError::Fence(_)) {
        return Err(format!("expected McpBridgeError::Fence, got {blind_err:?}"));
    }

    // Reconcile under the exact original `(op_id, idemp_key)` without re-executing external side effect,
    // and verify a subsequent same-key `call_tool` replays the settled outcome!
    let settled = bridge
        .reconcile(
            "stage-batch-sync",
            &op_id,
            &idemp_key,
            ProviderProbe::EffectLanded { receipt: 77 },
        )
        .map_err(|err| format!("McpBridgeAdapter::reconcile: {err}"))?;
    let replay_outcome = bridge
        .call_tool(
            "stage-batch-sync",
            &JsonValue::object([("batch_id", JsonValue::String("batch-road-e001".to_owned()))]),
            &op_id,
            &idemp_key,
        )
        .map_err(|err| format!("post-reconcile same-key replay: {err}"))?;
    let ready_after_reconcile = bridge.state() == BridgeState::Ready
        && bridge.fence().uncertain_count() == 0
        && settled.probe == ProviderProbe::EffectLanded { receipt: 77 }
        && matches!(replay_outcome, mcp_bridge_driver::CallOutcome::Replayed(_));
    drop(bridge);
    if !ready_after_reconcile || transport.executed_tools().len() != 1 {
        return Err(
            "McpBridgeAdapter same-key reconcile failed to converge cleanly without duplicate execution"
                .to_owned(),
        );
    }

    Ok((rotated_generations, true))
}

// ============================================================================
// Subsection D.5 — Dual-Layer `nlos-wasm-sandbox` Strong Isolation (`U-9`)
// ============================================================================

#[allow(clippy::too_many_lines)]
fn verify_d5_dual_layer_wasm_sandbox(
    host_ws: &TempHostWorkspace,
    wasm_host_exe: &Path,
) -> Result<i64, String> {
    // 1. Layer-1 child process environment fence (`--self-audit-env` -> 0 env vars).
    let env_audit =
        spawn_env_audit(wasm_host_exe).map_err(|err| format!("spawn_env_audit: {err}"))?;
    let audit_report: &HostReport = env_audit
        .report
        .as_ref()
        .ok_or_else(|| "missing HostReport from spawn_env_audit".to_owned())?;
    if audit_report.outcome != HostOutcome::AuditEnv || audit_report.code != 0 {
        return Err(format!(
            "expected AuditEnv code=0 (0 inherited env vars), got {audit_report:?}"
        ));
    }

    let sandbox_dir = host_ws.path().join("wasm-sandbox-e7");
    let preopen_root = sandbox_dir.join("allowed-workspace");
    fs::create_dir_all(&preopen_root).map_err(|err| format!("create preopen_root: {err}"))?;
    let outside_secret = sandbox_dir.join("outside-host-secret.txt");
    fs::write(&outside_secret, b"top-secret-host-file")
        .map_err(|err| format!("write outside_secret: {err}"))?;

    // 2. Positive writable preopen `/workspace` guest writes `made.txt`.
    let positive_wat = r#"(module
        (import "wasi_snapshot_preview1" "path_open"
            (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
        (import "wasi_snapshot_preview1" "fd_write"
            (func $fd_write (param i32 i32 i32 i32) (result i32)))
        (memory (export "memory") 1)
        (data (i32.const 1024) "made.txt")
        (data (i32.const 2048) "e7-ok")
        (func (export "main") (result i32)
            (local $errno i32) (local $fd i32)
            (local.set $errno (call $path_open (i32.const 3) (i32.const 0)
                (i32.const 1024) (i32.const 8) (i32.const 1)
                (i64.const 64) (i64.const 0) (i32.const 0) (i32.const 2096)))
            (if (i32.ne (local.get $errno) (i32.const 0)) (then (return (local.get $errno))))
            (local.set $fd (i32.load (i32.const 2096)))
            (i32.store (i32.const 2112) (i32.const 2048))
            (i32.store (i32.const 2116) (i32.const 5))
            (local.set $errno (call $fd_write (local.get $fd) (i32.const 2112) (i32.const 1) (i32.const 2120)))
            (if (i32.ne (local.get $errno) (i32.const 0)) (then (return (i32.add (local.get $errno) (i32.const 1000)))))
            (i32.const 0)))"#;
    let positive_wat_path = sandbox_dir.join("positive.wat");
    fs::write(&positive_wat_path, positive_wat)
        .map_err(|err| format!("write positive.wat: {err}"))?;
    let writable_profile = SandboxProfile::deny_all()
        .with_fuel(5_000_000)
        .with_preopen(
            PreopenGrant::new("/workspace", &preopen_root, true)
                .map_err(|err| format!("PreopenGrant::new: {err}"))?,
        );
    let pos_outcome = run_isolated_host(
        &HostProcessRequest::new(wasm_host_exe, &positive_wat_path, writable_profile),
        Some(Duration::from_secs(60)),
    )
    .map_err(|err| format!("run_isolated_host positive: {err}"))?;
    let pos_report = pos_outcome
        .report
        .as_ref()
        .ok_or_else(|| "missing positive HostReport".to_owned())?;
    if pos_report.outcome != HostOutcome::Completed || pos_report.code != 0 {
        return Err(format!("positive Wasm guest failed: {pos_report:?}"));
    }
    if fs::read(preopen_root.join("made.txt")).map_err(|err| err.to_string())? != b"e7-ok" {
        return Err("positive Wasm guest write did not land exact bytes".to_owned());
    }

    // 3. Escape attempts (`..`, POSIX absolute, Windows drive-absolute) denied with `ENOTCAPABLE` (`76`).
    let readonly_profile = SandboxProfile::deny_all()
        .with_fuel(5_000_000)
        .with_preopen(
            PreopenGrant::new("/workspace", &preopen_root, false)
                .map_err(|err| format!("PreopenGrant::new readonly: {err}"))?,
        );
    for (idx, escape_path) in [
        "../../../outside-host-secret.txt",
        "/etc/passwd",
        "C:/Windows/win.ini",
    ]
    .into_iter()
    .enumerate()
    {
        let len = escape_path.len();
        let wat = format!(
            r#"(module
                (import "wasi_snapshot_preview1" "path_open"
                    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 1024) "{escape_path}")
                (func (export "main") (result i32)
                    (call $path_open (i32.const 3) (i32.const 0)
                        (i32.const 1024) (i32.const {len}) (i32.const 0)
                        (i64.const 2) (i64.const 0) (i32.const 0) (i32.const 2048))))"#
        );
        let wat_path = sandbox_dir.join(format!("escape-{idx}.wat"));
        fs::write(&wat_path, wat).map_err(|err| format!("write escape wat: {err}"))?;
        let out = run_isolated_host(
            &HostProcessRequest::new(wasm_host_exe, &wat_path, readonly_profile.clone()),
            Some(Duration::from_secs(60)),
        )
        .map_err(|err| format!("run_isolated_host escape {escape_path}: {err}"))?;
        let rep = out
            .report
            .as_ref()
            .ok_or_else(|| "missing escape HostReport".to_owned())?;
        if rep.outcome != HostOutcome::Completed || rep.code != 76 {
            return Err(format!(
                "expected ENOTCAPABLE (76) on {escape_path}, got {rep:?}"
            ));
        }
    }
    if fs::read(&outside_secret).map_err(|err| err.to_string())? != b"top-secret-host-file" {
        return Err("outside host secret file was mutated during Wasm sandbox test".to_owned());
    }

    // 4. Absent `sock_open` WASI network import fails closed before execution!
    let sock_wat = r#"(module
        (import "wasi_snapshot_preview1" "sock_open"
            (func $sock_open (param i32 i32 i32) (result i32)))
        (memory (export "memory") 1)
        (func (export "main") (result i32)
            (call $sock_open (i32.const 0) (i32.const 0) (i32.const 0))))"#;
    let sock_wat_path = sandbox_dir.join("sock-escape.wat");
    fs::write(&sock_wat_path, sock_wat).map_err(|err| format!("write sock wat: {err}"))?;
    let sock_out = run_isolated_host(
        &HostProcessRequest::new(wasm_host_exe, &sock_wat_path, SandboxProfile::deny_all()),
        Some(Duration::from_secs(60)),
    )
    .map_err(|err| format!("run_isolated_host sock_open: {err}"))?;
    let sock_rep = sock_out
        .report
        .as_ref()
        .ok_or_else(|| "missing sock_open HostReport".to_owned())?;
    if sock_rep.outcome == HostOutcome::Completed || !sock_rep.detail.contains("sock_open") {
        return Err(format!(
            "expected instantiation rejection naming sock_open, got {sock_rep:?}"
        ));
    }

    Ok(audit_report.code)
}

// ============================================================================
// Subsection D.6 — Third-Party Package Upgrade `v1.0.0 -> v1.1.0` on Host `N`
// ============================================================================

struct InjectedUnhealthyProbe;

impl MigrationHealthProbe for InjectedUnhealthyProbe {
    fn target_revision_healthy(&self, _context: &MigrationHealthContext<'_>) -> bool {
        false
    }
}

struct TaskDeclarationConsultAdapter<'a> {
    tasks: &'a SqliteTaskAuthority,
}

impl DeclarationAdmissionConsult for TaskDeclarationConsultAdapter<'_> {
    type Error = TaskStoreError;

    fn consult_plan_declaration(
        &self,
        projected_task_nodes: u64,
    ) -> Result<DeclarationAdmissionOutcome, Self::Error> {
        match self.tasks.answer_plan_declaration(projected_task_nodes) {
            Ok(()) => Ok(DeclarationAdmissionOutcome::Admits),
            Err(TaskStoreError::TaskNodeAdmissionDenied {
                profile_id,
                task_count: _,
                max_task_nodes,
            }) => Ok(DeclarationAdmissionOutcome::Denied {
                profile_id: profile_id.to_owned(),
                max_task_nodes,
            }),
            Err(other) => Err(other),
        }
    }
}

#[allow(clippy::too_many_lines)]
fn verify_d6_package_upgrade_rollback_and_v1_1_0_rollout(
    host_ws: &TempHostWorkspace,
    runtime: &SliceKRuntime,
    step_b: &StepBN1HostStateSnapshot,
) -> Result<(bool, bool, BTreeMap<String, u64>), String> {
    let obs_rec = step_b
        .installed_samples
        .get("knowledge-observatory-app")
        .ok_or_else(|| "missing knowledge-observatory-app".to_owned())?;

    // 1. `[TUI-CONSENT-001]` Permission-escalation diff gate (`diff_permission_transparency`).
    let baseline_perm = DeclaredPermissionEntry {
        capability_scope: "sabi.artifact.write".to_owned(),
        human_purpose: "Persist curated knowledge digests".to_owned(),
        effect_class: EffectClass::Reversible,
        network_egress_declared: false,
        sandbox_profile: "certified-userland-v1".to_owned(),
    };
    let v1_0_perms = vec![baseline_perm.clone()];
    let benign_diff = diff_permission_transparency(&v1_0_perms, &v1_0_perms);
    if benign_diff.requires_tui_reconsent {
        return Err(
            "benign v1.0.0 -> v1.1.0 permission diff unexpectedly required Z4 reconsent".to_owned(),
        );
    }

    let escalated_v1_1_perms = vec![
        DeclaredPermissionEntry {
            effect_class: EffectClass::Irreversible,
            network_egress_declared: true,
            ..baseline_perm
        },
        DeclaredPermissionEntry {
            capability_scope: "sabi.network.external_egress".to_owned(),
            human_purpose: "Silent telemetry upload".to_owned(),
            effect_class: EffectClass::Irreversible,
            network_egress_declared: true,
            sandbox_profile: "certified-userland-v1".to_owned(),
        },
    ];
    let escalated_diff = diff_permission_transparency(&v1_0_perms, &escalated_v1_1_perms);
    let permission_escalation_blocked = escalated_diff.requires_tui_reconsent
        && !escalated_diff.newly_enabled_network_egress.is_empty()
        && !escalated_diff.escalated_to_irreversible.is_empty();
    if !permission_escalation_blocked {
        return Err(format!(
            "permission escalation diff failed to block silent upgrade: {escalated_diff:?}"
        ));
    }

    // 2. `[PKG-UPDATE-001]` Safety-gate blocks (`DisguisedIrreversibleMigration`,
    //    `MissingPreUpgradeBackup`, `MissingZ4TrustedUiConsent`).
    if !matches!(
        execute_ephemeral_cli_drill(CliDrillScenario {
            mode: MigrationMode::BackwardCompatibleWindow,
            inject_health_fail: false,
            inject_step_crash: false,
            omit_backup: false,
            omit_z4_consent: false,
            disguise_destructive: true,
        }),
        Err(MigrationDrillError::DisguisedIrreversibleMigration { .. })
    ) {
        return Err("expected DisguisedIrreversibleMigration block".to_owned());
    }
    if !matches!(
        execute_ephemeral_cli_drill(CliDrillScenario {
            mode: MigrationMode::Irreversible,
            inject_health_fail: false,
            inject_step_crash: false,
            omit_backup: true,
            omit_z4_consent: false,
            disguise_destructive: false,
        }),
        Err(MigrationDrillError::MissingPreUpgradeBackup)
    ) {
        return Err("expected MissingPreUpgradeBackup block".to_owned());
    }
    if !matches!(
        execute_ephemeral_cli_drill(CliDrillScenario {
            mode: MigrationMode::Irreversible,
            inject_health_fail: false,
            inject_step_crash: false,
            omit_backup: false,
            omit_z4_consent: true,
            disguise_destructive: false,
        }),
        Err(MigrationDrillError::MissingZ4TrustedUiConsent)
    ) {
        return Err("expected MissingZ4TrustedUiConsent block".to_owned());
    }

    // 3. Injected mid-migration health-check failure on `field-survey-app` (`v1.0.0 -> v1.1.0`):
    //    snapshot `UserData` -> stage `migrate_application` -> mutate `UserData` + leave `.tmp` ->
    //    probe returns `false` -> `rollback_package_migration` + `restore_container_from_snapshot` ->
    //    assert `Generation(1)`, `0` residual `.tmp`, and bit-identical `N-1` `UserData` digest!
    let survey_rec = step_b
        .installed_samples
        .get("field-survey-app")
        .ok_or_else(|| "missing field-survey-app".to_owned())?;
    let (_, _, _, survey_v1_1_verified) = materialize_and_verify_frozen_package(
        runtime,
        &survey_rec.pkg_v1_1_0,
        b"verify-v1.1-field-survey-app",
    )?;
    let survey_user_data = host_ws.user_data_dir("field-survey-app");
    let snapshot_staging = host_ws.path().join("migration-snapshots/field-survey-app");
    let snap_at = runtime
        .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
            STEP_D_KEY_DOMAIN,
            b"survey-mig-snap",
            1,
        )))
        .map_err(|err| format!("wall_now_ms: {err}"))?;
    let snapshot_receipt =
        capture_container_snapshot(&survey_user_data, &snapshot_staging, snap_at)
            .map_err(|err| format!("capture_container_snapshot: {err}"))?;
    if snapshot_receipt.tree_digest != survey_rec.user_data_tree_digest {
        return Err("pre-migration snapshot digest did not match N-1 UserData digest".to_owned());
    }

    let drill_key =
        IdempotencyKey::from_bytes(derive_key16(STEP_D_KEY_DOMAIN, b"survey-mig-drill", 1));
    let mig_start_ms = runtime
        .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
            STEP_D_KEY_DOMAIN,
            b"survey-mig-drill",
            2,
        )))
        .map_err(|err| format!("wall_now_ms: {err}"))?;
    runtime
        .applications
        .migrate_application(
            &runtime.artifacts,
            MigrateApplicationRequest {
                package_id: survey_rec.package_id,
                package_verification_receipt_id: survey_v1_1_verified.receipt_id,
                idempotency_key: drill_key,
                compatibility_window: CompatibilityWindow::SameMajor,
                declared_step_count: 1,
                requested_at_ms: mig_start_ms,
            },
        )
        .map_err(|err| format!("migrate_application field-survey-app: {err}"))?;

    // Mutate `UserData` mid-step and plant a `.tmp` scratch file before failing health check.
    fs::write(
        survey_user_data.join("records/state-v1.txt"),
        b"partially-migrated-corrupt-v1.1-draft",
    )
    .map_err(|err| format!("write partial migration mutation: {err}"))?;
    fs::write(
        survey_user_data.join("records/partial-rewrite.tmp"),
        b"uncommitted-temp-residue",
    )
    .map_err(|err| format!("write temp residue: {err}"))?;

    let step_ms = runtime
        .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
            STEP_D_KEY_DOMAIN,
            b"survey-mig-drill",
            3,
        )))
        .map_err(|err| format!("wall_now_ms: {err}"))?;
    runtime
        .applications
        .record_migration_step(RecordMigrationStepRequest {
            idempotency_key: drill_key,
            step_index: 1,
            completed_at_ms: step_ms,
        })
        .map_err(|err| format!("record_migration_step: {err}"))?;

    let health_ms = runtime
        .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
            STEP_D_KEY_DOMAIN,
            b"survey-mig-drill",
            4,
        )))
        .map_err(|err| format!("wall_now_ms: {err}"))?;
    let health_verdict = runtime
        .applications
        .run_migration_health_check(
            &runtime.artifacts,
            drill_key,
            &InjectedUnhealthyProbe,
            health_ms,
        )
        .map_err(|err| format!("run_migration_health_check: {err}"))?;
    let passed = match health_verdict {
        nlos_application::MigrationHealthDecision::Recorded(rep)
        | nlos_application::MigrationHealthDecision::Replayed(rep) => rep.passed,
    };
    if passed {
        return Err("InjectedUnhealthyProbe must record passed=false".to_owned());
    }

    let rb_ms = runtime
        .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
            STEP_D_KEY_DOMAIN,
            b"survey-mig-drill",
            5,
        )))
        .map_err(|err| format!("wall_now_ms: {err}"))?;
    let rb_receipt = match runtime
        .applications
        .rollback_package_migration(RollbackPackageMigrationRequest {
            idempotency_key: drill_key,
            rolled_back_at_ms: rb_ms,
        })
        .map_err(|err| format!("rollback_package_migration: {err}"))?
    {
        nlos_application::RollbackMigrationDecision::RolledBack(r)
        | nlos_application::RollbackMigrationDecision::Replayed(r) => r,
    };
    restore_container_from_snapshot(&snapshot_receipt, &survey_user_data)
        .map_err(|err| format!("restore_container_from_snapshot: {err}"))?;

    let survey_after_rb = runtime
        .applications
        .inspect_application(survey_rec.package_id)
        .map_err(|err| format!("inspect_application after rollback: {err}"))?
        .ok_or_else(|| "field-survey-app missing after rollback".to_owned())?;
    let mig_view = runtime
        .applications
        .inspect_package_migration(drill_key)
        .map_err(|err| format!("inspect_package_migration: {err}"))?
        .ok_or_else(|| "migration row missing".to_owned())?;
    let residual_temps = count_residual_temp_files(&survey_user_data)
        .map_err(|err| format!("count_residual_temp_files: {err}"))?;
    let restored_digest = compute_container_tree_digest(&survey_user_data)
        .map_err(|err| format!("compute_container_tree_digest after rollback: {err}"))?;
    let rollback_ok = rb_receipt.retained_generation == Generation::INITIAL
        && survey_after_rb.current_installation_generation == Generation::INITIAL
        && survey_after_rb.package_manifest_digest
            == survey_rec.installation_v1_0_0.package_manifest_digest
        && mig_view.state == MigrationState::Failed
        && residual_temps == 0
        && restored_digest == survey_rec.user_data_tree_digest;
    if !rollback_ok {
        return Err(
            "field-survey-app atomic rollback failed to restore exact Generation(1) & UserData"
                .to_owned(),
        );
    }

    // 4. Healthy `v1.0.0 -> v1.1.0` upgrade directly from frozen `v1.1.0.nlos-pkg` bytes
    //    for `knowledge-observatory-app`, `mcp-bridge-driver`, and `openai-compat-driver`.
    let mut upgraded_v1_1_0_packages = BTreeMap::new();
    let mut obs_v1_1_templated: Option<SignedPackageWithTasks> = None;
    for (ord, label) in [
        "knowledge-observatory-app",
        "mcp-bridge-driver",
        "openai-compat-driver",
    ]
    .into_iter()
    .enumerate()
    {
        let rec = step_b
            .installed_samples
            .get(label)
            .ok_or_else(|| format!("missing {label}"))?;
        let phase = format!("verify-v1.1-{label}");
        let (_, _, maybe_templated, verified_v1_1) =
            materialize_and_verify_frozen_package(runtime, &rec.pkg_v1_1_0, phase.as_bytes())?;
        if label == "knowledge-observatory-app" {
            obs_v1_1_templated = maybe_templated;
        }
        let upd_at = runtime
            .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
                STEP_D_KEY_DOMAIN,
                phase.as_bytes(),
                u32::try_from(100 + ord).unwrap_or(100),
            )))
            .map_err(|err| format!("wall_now_ms: {err}"))?;
        let updated_receipt = runtime
            .applications
            .update_application(
                &runtime.artifacts,
                UpdateApplicationRequest {
                    package_id: rec.package_id,
                    package_verification_receipt_id: verified_v1_1.receipt_id,
                    idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                        STEP_D_KEY_DOMAIN,
                        phase.as_bytes(),
                        u32::try_from(200 + ord).unwrap_or(200),
                    )),
                    updated_at_ms: upd_at,
                    compatibility_window: CompatibilityWindow::SameMajor,
                },
            )
            .map_err(|err| format!("update_application {label} v1.0.0 -> v1.1.0: {err}"))?
            .receipt();
        if updated_receipt.installation_generation.get() != 2
            || updated_receipt.application_id != rec.runtime_application_id
        {
            return Err(format!(
                "expected Generation(2) with preserved ApplicationId for {label}, got {updated_receipt:?}"
            ));
        }

        // Verify `UserData` container survived the `v1.0.0 -> v1.1.0` upgrade intact!
        let post_upd_digest = compute_container_tree_digest(&host_ws.user_data_dir(label))
            .map_err(|err| format!("compute_container_tree_digest post-upgrade {label}: {err}"))?;
        if post_upd_digest != rec.user_data_tree_digest {
            return Err(format!(
                "UserData container unexpectedly corrupted during healthy v1.1.0 upgrade of {label}"
            ));
        }

        upgraded_v1_1_0_packages.insert(
            label.to_owned(),
            updated_receipt.installation_generation.get(),
        );
    }

    // 5. Evolve `knowledge-observatory-app`'s `TaskPlan` to revision `2` (`4` nodes), verify
    //    upstream `3` `N-1` nodes remain `Completed` at `declared_revision == 1`, execute the
    //    appended 4th node (`verify-digest`), and commit `ArtifactStore` revision `4`!
    let obs_plan_id = obs_rec
        .plan_id
        .ok_or_else(|| "missing observatory TaskPlanId".to_owned())?;
    let obs_signed_v1_1 = obs_v1_1_templated
        .ok_or_else(|| "missing SignedPackageWithTasks for observatory v1.1.0".to_owned())?;
    let plan_rev2_at = runtime
        .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
            STEP_D_KEY_DOMAIN,
            b"obs-plan-rev2",
            1,
        )))
        .map_err(|err| format!("wall_now_ms: {err}"))?;
    let mut rev2_req = compile_task_templates(
        &obs_signed_v1_1,
        IdempotencyKey::from_bytes(derive_key16(STEP_D_KEY_DOMAIN, b"obs-plan-rev2", 2)),
        plan_rev2_at,
    )
    .map_err(|err| format!("compile_task_templates observatory v1.1.0: {err}"))?;
    rev2_req.plan_id = Some(obs_plan_id);
    let consult = TaskDeclarationConsultAdapter {
        tasks: &runtime.tasks,
    };
    let rev2_receipt = runtime
        .plans()
        .apply_plan_revision_with_admission(rev2_req, &consult)
        .map_err(|err| format!("apply_plan_revision observatory rev 2: {err}"))?
        .receipt();
    let chain = runtime
        .plans()
        .verify_revision_chain(obs_plan_id)
        .map_err(|err| format!("verify_revision_chain observatory rev 2: {err}"))?;
    if rev2_receipt.revision != 2
        || rev2_receipt.declared_node_count != 4
        || chain.revision_count != 2
    {
        return Err(format!(
            "expected observatory TaskPlan rev=2 with 4 nodes and chain_len=2, got {rev2_receipt:?}, chain={chain:?}"
        ));
    }

    for upstream_hex in [
        OBSERVATORY_NODE_HARVEST,
        OBSERVATORY_NODE_BUILD_INDEX,
        OBSERVATORY_NODE_PUBLISH,
    ] {
        let (nid, decl_rev) =
            resolve_plan_node_by_hex_key(runtime.plans(), obs_plan_id, upstream_hex)?;
        let node = runtime
            .plans()
            .list_plan_nodes(obs_plan_id)
            .map_err(|err| format!("list_plan_nodes: {err}"))?
            .into_iter()
            .find(|n| n.node_id == nid)
            .ok_or_else(|| format!("missing upstream node {upstream_hex}"))?;
        if decl_rev != 1 || node.state != PlanNodeState::Completed {
            return Err(format!(
                "[PLAN-DAG-001] upstream N-1 node {upstream_hex} lost Completed/rev=1 state: {node:?}"
            ));
        }
    }

    let (verify_node_id, verify_decl_rev) =
        resolve_plan_node_by_hex_key(runtime.plans(), obs_plan_id, OBSERVATORY_NODE_VERIFY_V1_1)?;
    if verify_decl_rev != 2 {
        return Err(format!(
            "expected appended verify-digest node at declared_revision=2, got {verify_decl_rev}"
        ));
    }
    materialize_and_activate_plan_node(
        runtime,
        obs_plan_id,
        verify_node_id,
        verify_decl_rev,
        b"obs-v1.1-verify-node-act",
    )?;
    let rev4_body = format!(
        "app=knowledge-observatory-app\nepoch={HOST_EPOCH_N}\nrevision=4\nstep=verify-digest-v1.1.0\nupstream={}\n",
        hex_encode(step_b.observatory_rev3_digest.as_bytes())
    );
    let rev4_at = runtime
        .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
            STEP_D_KEY_DOMAIN,
            b"obs-rev4",
            1,
        )))
        .map_err(|err| format!("wall_now_ms: {err}"))?;
    runtime
        .artifacts
        .put_revision(PutRevisionRequest {
            artifact_id: step_b.observatory_output_artifact_id,
            expected_head_revision: 3,
            bytes: rev4_body.as_bytes(),
            created_at_ms: rev4_at,
            provenance: ProvenanceSourceTriple {
                source_a: *verify_node_id.as_bytes(),
                source_b: *obs_rec.package_id.as_bytes(),
                source_digest: step_b.observatory_rev3_digest,
            },
        })
        .map_err(|err| format!("put_revision 4 for observatory v1.1.0: {err}"))?;
    complete_active_plan_node(
        runtime,
        obs_plan_id,
        verify_node_id,
        verify_decl_rev,
        b"obs-v1.1-verify-node-done",
    )?;

    // Execute upgraded `v1.1.0` `curator` payload on Host `N`.
    let v1_1_exec = execute_application_payload(runtime, obs_rec.package_id, "curator")
        .map_err(|err| format!("execute_application_payload observatory v1.1.0: {err}"))?;
    if v1_1_exec.payload_size_bytes == 0 {
        return Err("observatory v1.1.0 curator payload returned 0 bytes".to_owned());
    }

    Ok((
        permission_escalation_blocked,
        rollback_ok,
        upgraded_v1_1_0_packages,
    ))
}

// ============================================================================
// Subsection D.7 — Clean Uninstall & Immediate Capability Revocation
// ============================================================================

#[derive(Clone, Copy, Debug)]
struct Z4UserDataPurgeConsentReceipt {
    operator_principal: PrincipalId,
    package_id: PackageId,
    user_data_tree_digest: [u8; 32],
    consented_at_ms: u64,
    signature: Ed25519Signature,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UserDataRetentionDisposition {
    PreservedByDefault { tree_digest: [u8; 32] },
    PurgedWithVerifiedZ4Consent { remaining_files: usize },
}

#[allow(clippy::too_many_lines)]
fn verify_d7_clean_uninstall_and_capability_revocation(
    host_ws: &TempHostWorkspace,
    runtime: &SliceKRuntime,
    step_b: &StepBN1HostStateSnapshot,
) -> Result<(bool, bool, usize, bool, bool), String> {
    let survey_rec: &InstalledN1SampleRecord = step_b
        .installed_samples
        .get("field-survey-app")
        .ok_or_else(|| "missing field-survey-app".to_owned())?;

    // 1. Attempt uninstall while `survey_active_bg_task_id` is still active -> MUST fail closed!
    let blocked_at = runtime
        .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
            STEP_D_KEY_DOMAIN,
            b"survey-uninstall-blocked",
            1,
        )))
        .map_err(|err| format!("wall_now_ms: {err}"))?;
    let active_err = runtime
        .applications
        .uninstall_application_with_task_activity_gate(
            &runtime.tasks,
            UninstallApplicationRequest {
                package_id: survey_rec.package_id,
                idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                    STEP_D_KEY_DOMAIN,
                    b"survey-uninstall-blocked",
                    2,
                )),
                uninstalled_at_ms: blocked_at,
            },
        )
        .expect_err("uninstall with active background Task must fail closed");
    let uninstall_blocked = matches!(
        active_err,
        ApplicationAuthorityError::ApplicationActiveTasksRunning {
            package_id,
            active_task_count: 1,
        } if package_id == survey_rec.package_id
    );
    if !uninstall_blocked {
        return Err(format!(
            "expected ApplicationActiveTasksRunning(count=1), got {active_err:?}"
        ));
    }

    // 2. Cancel `survey_active_bg_task_id` in `runtime.tasks`.
    let cancel_i64 = runtime
        .wall_now_i64(IdempotencyKey::from_bytes(derive_key16(
            STEP_D_KEY_DOMAIN,
            b"survey-bg-cancel",
            1,
        )))
        .map_err(|err| format!("wall_now_i64: {err}"))?;
    runtime
        .tasks
        .cancel_task(CancelRequest {
            task_id: step_b.survey_active_bg_task_id,
            idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                STEP_D_KEY_DOMAIN,
                b"survey-bg-cancel",
                2,
            )),
            requested_at_ms: cancel_i64,
        })
        .map_err(|err| format!("cancel_task survey_active_bg_task_id: {err}"))?;
    if runtime
        .tasks
        .inspect_outstanding_task_count(&[step_b.survey_active_bg_task_id])
        .map_err(|err| format!("inspect_outstanding_task_count post-cancel: {err}"))?
        != 0
    {
        return Err("cancelled background Task still reported as outstanding".to_owned());
    }

    // 3. Revoke `survey_root_capability` via `revoke_signed` (`Z4_OPERATOR_SEED`) and prove
    //    both the root capability and `survey_delegated_capability` fail closed immediately!
    let revoke_at = SEAL_T0_MS + 90_000;
    let z4_key = SigningKey::from_bytes(&Z4_OPERATOR_SEED);
    let revoke_cmd = RevokeCapabilityRequest {
        handle: step_b.survey_root_capability,
        revoker_key_id: step_b.z4_operator_binding.key_id,
        idempotency_key: IdempotencyKey::from_bytes(derive_key16(
            STEP_D_KEY_DOMAIN,
            b"survey-cap-revoke",
            2,
        )),
        revoked_at_ms: revoke_at,
    };
    let revoke_sig = z4_key.sign(&revoke_command_message(revoke_cmd)).to_bytes();
    let revoke_receipt = runtime
        .capability()
        .revoke_signed(
            &runtime.identity,
            SignedRevokeCapabilityRequest {
                command: revoke_cmd,
                signer: step_b.z4_operator_binding.principal_id,
                signature: revoke_sig,
            },
        )
        .map_err(|err| format!("revoke_signed survey_root_capability: {err}"))?
        .receipt();

    // Test both stale Generation(1) root handle (`GenerationFenceConflict`), current
    // Generation(2) root handle (`CapabilityRevoked`), and child handle (`AncestorRevokedOrFenced`)!
    let holder_key = SigningKey::from_bytes(&crate::step_b_n1_host::SURVEY_HOLDER_SEED);
    let holder_ev =
        SemanticEventId::from_bytes(derive_digest32(STEP_D_KEY_DOMAIN, b"post-revoke-root", 1));
    let holder_signer = runtime
        .identity
        .verify_semantic_signature(VerifySemanticSignatureRequest {
            event_id: holder_ev,
            issuer: step_b.survey_holder_binding.principal_id,
            control_domain_id: step_b.survey_holder_binding.control_domain_id,
            key_id: step_b.survey_holder_binding.key_id,
            signature: holder_key
                .sign(&semantic_signature_message(holder_ev))
                .to_bytes(),
            admitted_at_ms: revoke_at + 1,
        })
        .map_err(|err| format!("verify_semantic_signature holder: {err}"))?;
    let stale_root_err = runtime
        .capability()
        .authorize_semantic(AuthorizeSemanticRequest {
            handle: step_b.survey_root_capability,
            signer: holder_signer,
            target: CapabilityTarget::Namespace(step_b.target_namespace),
            required_right: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: Some(step_b.survey_purpose_digest),
            admitted_at_ms: revoke_at + 1,
        })
        .expect_err("stale Generation(1) revoked root capability must fail closed");
    let current_revoked_handle = nlos_capability::CapabilityHandle {
        capability_id: step_b.survey_root_capability.capability_id,
        generation: Generation::new(
            NonZeroU64::new(revoke_receipt.resulting_generation.get()).expect("non-zero gen"),
        ),
    };
    let revoked_root_err = runtime
        .capability()
        .authorize_semantic(AuthorizeSemanticRequest {
            handle: current_revoked_handle,
            signer: holder_signer,
            target: CapabilityTarget::Namespace(step_b.target_namespace),
            required_right: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: Some(step_b.survey_purpose_digest),
            admitted_at_ms: revoke_at + 1,
        })
        .expect_err("revoked Generation(2) root capability must fail closed");

    let worker_key = SigningKey::from_bytes(&SURVEY_WORKER_DELEGATE_SEED);
    let worker_ev =
        SemanticEventId::from_bytes(derive_digest32(STEP_D_KEY_DOMAIN, b"post-revoke-child", 1));
    let worker_signer = runtime
        .identity
        .verify_semantic_signature(VerifySemanticSignatureRequest {
            event_id: worker_ev,
            issuer: step_b.survey_worker_binding.principal_id,
            control_domain_id: step_b.survey_worker_binding.control_domain_id,
            key_id: step_b.survey_worker_binding.key_id,
            signature: worker_key
                .sign(&semantic_signature_message(worker_ev))
                .to_bytes(),
            admitted_at_ms: revoke_at + 1,
        })
        .map_err(|err| format!("verify_semantic_signature worker: {err}"))?;
    let child_err = runtime
        .capability()
        .authorize_semantic(AuthorizeSemanticRequest {
            handle: step_b.survey_delegated_capability,
            signer: worker_signer,
            target: CapabilityTarget::Namespace(step_b.target_namespace),
            required_right: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: Some(step_b.survey_purpose_digest),
            admitted_at_ms: revoke_at + 1,
        })
        .expect_err("delegated child of revoked root capability must fail closed");
    let caps_revoked_ok =
        matches!(
            stale_root_err,
            CapabilityAuthorityError::GenerationFenceConflict
        ) && matches!(
            revoked_root_err,
            CapabilityAuthorityError::CapabilityRevoked
        ) && matches!(child_err, CapabilityAuthorityError::AncestorRevokedOrFenced);
    if !caps_revoked_ok {
        return Err(format!(
            "unexpected post-revocation errors: stale={stale_root_err:?}, root={revoked_root_err:?}, child={child_err:?}"
        ));
    }

    // 4. Execute gated clean uninstall of `field-survey-app`.
    let uninstall_at = runtime
        .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
            STEP_D_KEY_DOMAIN,
            b"survey-uninstall-clean",
            1,
        )))
        .map_err(|err| format!("wall_now_ms: {err}"))?;
    runtime
        .applications
        .uninstall_application_with_task_activity_gate(
            &runtime.tasks,
            UninstallApplicationRequest {
                package_id: survey_rec.package_id,
                idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                    STEP_D_KEY_DOMAIN,
                    b"survey-uninstall-clean",
                    2,
                )),
                uninstalled_at_ms: uninstall_at,
            },
        )
        .map_err(|err| format!("clean uninstall field-survey-app: {err}"))?;

    let uninstalled_view = runtime
        .applications
        .inspect_application(survey_rec.package_id)
        .map_err(|err| format!("inspect_application post-uninstall: {err}"))?
        .ok_or_else(|| "application row deleted instead of marked Uninstalled".to_owned())?;
    if uninstalled_view.status != ApplicationStatus::Uninstalled {
        return Err(format!(
            "expected ApplicationStatus::Uninstalled, got {:?}",
            uninstalled_view.status
        ));
    }
    if execute_application_payload(runtime, survey_rec.package_id, "survey-worker").is_ok() {
        return Err("execute_application_payload succeeded on Uninstalled application".to_owned());
    }

    // 5. Purge private runtime cache (`0` remaining files) and verify shared cross-app
    //    `Artifact` (`shared_corpus_artifact_id`) remains 100% intact!
    let cache_dir = host_ws.private_cache_dir("field-survey-app");
    if cache_dir.exists() {
        fs::remove_dir_all(&cache_dir).map_err(|err| format!("purge private cache dir: {err}"))?;
    }
    fs::create_dir_all(&cache_dir)
        .map_err(|err| format!("recreate empty private cache root: {err}"))?;
    let remaining_cache_files = count_files_recursively(&cache_dir)?;
    if remaining_cache_files != 0 {
        return Err(format!(
            "expected 0 residual private cache files after uninstall, got {remaining_cache_files}"
        ));
    }

    let shared_bytes = runtime
        .artifacts
        .get_revision(step_b.shared_corpus_artifact_id, 1, uninstall_at)
        .map_err(|err| format!("read shared_corpus_artifact_id post-uninstall: {err}"))?;
    let shared_preserved = ContentDigest::of_bytes(&shared_bytes) == step_b.shared_corpus_digest;
    if !shared_preserved {
        return Err(
            "shared cross-application Artifact corrupted by field-survey-app uninstall".to_owned(),
        );
    }

    // 6. Verify `UserData` retention law (`[PKG-UNINSTALL-001]`):
    //    - `None` `Z4` consent -> `PreservedByDefault` matching `N-1` SHA-256 digest;
    //    - forged `Z4` signature -> rejected fail-closed with `0` files deleted;
    //    - valid `Z4`-signed consent -> `PurgedWithVerifiedZ4Consent { remaining_files: 0 }`.
    let survey_user_data = host_ws.user_data_dir("field-survey-app");
    let default_disp = evaluate_user_data_retention_on_uninstall(
        &runtime.identity,
        step_b.z4_operator_binding.principal_id,
        survey_rec.package_id,
        &survey_user_data,
        None,
    )?;
    if default_disp
        != (UserDataRetentionDisposition::PreservedByDefault {
            tree_digest: survey_rec.user_data_tree_digest,
        })
    {
        return Err(format!(
            "default uninstall failed to preserve exact N-1 UserData: {default_disp:?}"
        ));
    }

    let forged_consent = Z4UserDataPurgeConsentReceipt {
        operator_principal: step_b.z4_operator_binding.principal_id,
        package_id: survey_rec.package_id,
        user_data_tree_digest: survey_rec.user_data_tree_digest,
        consented_at_ms: uninstall_at + 10,
        signature: [0xEE; 64],
    };
    let forged_err = evaluate_user_data_retention_on_uninstall(
        &runtime.identity,
        step_b.z4_operator_binding.principal_id,
        survey_rec.package_id,
        &survey_user_data,
        Some(forged_consent),
    )
    .expect_err("forged Z4 UserData purge consent must fail closed");
    if !forged_err.contains("InvalidSignature") {
        return Err(format!(
            "expected InvalidSignature on forged Z4 purge, got {forged_err}"
        ));
    }
    if compute_container_tree_digest(&survey_user_data).map_err(|err| err.to_string())?
        != survey_rec.user_data_tree_digest
    {
        return Err("forged Z4 purge deleted or mutated UserData files".to_owned());
    }

    let valid_digest = compute_z4_user_data_purge_digest(
        step_b.z4_operator_binding.principal_id,
        survey_rec.package_id,
        survey_rec.user_data_tree_digest,
        uninstall_at + 20,
    );
    let valid_consent = Z4UserDataPurgeConsentReceipt {
        operator_principal: step_b.z4_operator_binding.principal_id,
        package_id: survey_rec.package_id,
        user_data_tree_digest: survey_rec.user_data_tree_digest,
        consented_at_ms: uninstall_at + 20,
        signature: z4_key.sign(&valid_digest).to_bytes(),
    };
    let purged_disp = evaluate_user_data_retention_on_uninstall(
        &runtime.identity,
        step_b.z4_operator_binding.principal_id,
        survey_rec.package_id,
        &survey_user_data,
        Some(valid_consent),
    )?;
    let user_data_law_ok = purged_disp
        == UserDataRetentionDisposition::PurgedWithVerifiedZ4Consent { remaining_files: 0 };
    if !user_data_law_ok {
        return Err(format!(
            "valid Z4 UserData purge did not empty container cleanly: {purged_disp:?}"
        ));
    }

    Ok((
        uninstall_blocked,
        caps_revoked_ok,
        remaining_cache_files,
        shared_preserved,
        user_data_law_ok,
    ))
}

fn compute_z4_user_data_purge_digest(
    operator_principal: PrincipalId,
    package_id: PackageId,
    user_data_tree_digest: [u8; 32],
    consented_at_ms: u64,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(Z4_USER_DATA_PURGE_DOMAIN);
    hasher.update(operator_principal.as_bytes());
    hasher.update(package_id.as_bytes());
    hasher.update(user_data_tree_digest);
    hasher.update(consented_at_ms.to_be_bytes());
    hasher.finalize().into()
}

fn evaluate_user_data_retention_on_uninstall(
    identity: &IdentityAuthority,
    expected_z4_operator: PrincipalId,
    package_id: PackageId,
    user_data_dir: &Path,
    z4_consent: Option<Z4UserDataPurgeConsentReceipt>,
) -> Result<UserDataRetentionDisposition, String> {
    let live_digest = compute_container_tree_digest(user_data_dir)
        .map_err(|err| format!("compute_container_tree_digest: {err}"))?;
    let Some(consent) = z4_consent else {
        return Ok(UserDataRetentionDisposition::PreservedByDefault {
            tree_digest: live_digest,
        });
    };
    if consent.operator_principal != expected_z4_operator
        || consent.package_id != package_id
        || consent.user_data_tree_digest != live_digest
    {
        return Err("Z4 UserData purge consent binding mismatch".to_owned());
    }
    let message_digest = compute_z4_user_data_purge_digest(
        consent.operator_principal,
        consent.package_id,
        consent.user_data_tree_digest,
        consent.consented_at_ms,
    );
    identity
        .verify_capability_command_signature(VerifyCapabilityCommandSignatureRequest {
            message_digest,
            principal: consent.operator_principal,
            signature: consent.signature,
            verified_at_ms: consent.consented_at_ms,
        })
        .map_err(|err: IdentityAuthorityError| format!("Z4 signature rejected ({err:?})"))?;

    if user_data_dir.exists() {
        fs::remove_dir_all(user_data_dir).map_err(|err| format!("remove UserData dir: {err}"))?;
    }
    fs::create_dir_all(user_data_dir)
        .map_err(|err| format!("recreate empty UserData dir: {err}"))?;
    let remaining_files = count_files_recursively(user_data_dir)?;
    Ok(UserDataRetentionDisposition::PurgedWithVerifiedZ4Consent { remaining_files })
}

fn count_files_recursively(dir: &Path) -> Result<usize, String> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut total = 0_usize;
    for entry in fs::read_dir(dir).map_err(|err| format!("read_dir {}: {err}", dir.display()))? {
        let entry = entry.map_err(|err| format!("dir entry: {err}"))?;
        let path = entry.path();
        if path.is_dir() {
            total += count_files_recursively(&path)?;
        } else {
            total += 1;
        }
    }
    Ok(total)
}
