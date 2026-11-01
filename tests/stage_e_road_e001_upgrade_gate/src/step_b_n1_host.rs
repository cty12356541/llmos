//! Step B (`ADR-0027` section 2.4 Step B) — Host `N-1` (`host_epoch = 1`)
//! Durable Installation, Execution, and State Persistence.
//!
//! Boots a real Host `N-1` environment over [`TempHostWorkspace`], installs all
//! 4 frozen `v1.0.0.nlos-pkg` packages directly from their sealed bytes (`0`
//! recompilation, `0` re-signing), enforces `[PLAN-DEPENDENCY-001]` immutable
//! dependency pinning and `[SEARCH-AUTH-002]` (`0x1F`) non-authoritative search
//! firewalls, grants `Z4`-consented attenuated `Capability` handles, executes
//! multi-node `TaskPlan` DAGs, `McpBridgeAdapter` JSON-RPC 2.0 tool calls,
//! `MeteredChatGateway` 5-dimension `LlmTokenBudget` accounting + SSE stream
//! aggregation, content-addressed `ArtifactStore` causal lineage revisions,
//! durable background `Task` registration, and `UserData` container writes, and
//! finally drops all `N-1` handles cleanly before the Step C in-place upgrade.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::Path;

use ecosystem_driver_kit::{EffectDescriptor, JsonValue, parse_effect_declarations};
use ed25519_dalek::{Signer, SigningKey};
use mcp_bridge_driver::{
    BridgeState, CallOutcome, InitializeResult, JsonRpcResponse, McpBridgeAdapter, McpBridgeError,
    McpTransport, RequestId, ToolCallOutcome, sample_tool_catalog,
};
use nlos_application::{InstallationReceipt, RegisterBackgroundTaskRequest};
use nlos_artifact::{
    ArtifactStore, ContentDigest, CreateArtifactSpec, PackageFile, PackageFileEntry,
    PackageManifest, PackageVerificationReceipt, ProvenanceSourceTriple, PutRevisionRequest,
    SignedPackage, SignedPackageWithTasks, VerifyPackageRequest, VerifyPackageWithTasksRequest,
    derive_artifact_id,
};
use nlos_capability::{
    CapabilityHandle, CapabilityRights, CapabilityTarget, ConsumeCapabilityRequest,
    DelegateCapabilityRequest, IssueRootCapabilityRequest, SignedDelegateCapabilityRequest,
    SignedIssueRootCapabilityRequest, delegate_command_message, issue_root_command_message,
};
use nlos_identity::{
    BootstrapPrincipalRequest, IdentityBinding, KeyPurpose, VerifySemanticSignatureRequest,
};
use nlos_migrate::compute_container_tree_digest;
use nlos_plan::ecosystem_selector::{
    BindNodeDependenciesRequest, CrossEcosystemEntitySource, CrossEcosystemLookup,
    EcosystemEntitySnapshot, EcosystemGateError, PinnedEcosystemDependencyHandle,
    PlanDependencyEntityKind, ResolveNodeDependencyRequest, SelectorProvenance, SelectorVersionPin,
    SqliteEcosystemDependencyGate, TypedDependencySelector, TypedDependencySelectorProposal,
    parse_version_pin,
};
use nlos_plan::{
    MaterializationAdmission, MaterializationAdmissionVerdict, MaterializationRequest,
    MaterializationResolution, MaterializationResolutionDecision, NodeTransitionRequest,
    PlanNodeState, PlanStoreError, SqlitePlanAuthority,
};
use nlos_publisher_trust::PublisherId;
use nlos_resource::{
    DriverRecord, LlmTokenBudgetRecord, LlmTokenLimits, RegisterDriverRequest, ResourceAuthority,
};
use nlos_schema::SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL;
use nlos_search::{ControlConsumerKind, SearchError, SearchUseRestrictions};
use nlos_slice_k::{AutoOrphanGc, SliceKRuntime, execute_application_payload};
use nlos_task::TaskSpec;
use nlos_types::{
    ApplicationId, ArtifactId, Generation, IdempotencyKey, NamespaceId, PackageId, PrincipalId,
    ResourceAccountId, SemanticEventId, TaskId, TaskNodeId, TaskPlanId,
};
use openai_compat_driver::{
    ChatCompletionRequest, ChatMessage, ChatRole, DONE_SENTINEL, MeteredChatGateway, SseParser,
    StreamAggregator, request_digest,
};

use crate::step_a_frozen_seal::{
    ParsedStoreSealRecord, StepAFrozenSealReceipt, VerifiedFrozenSample,
    provision_store_governance_catalog,
};
use crate::{
    HOST_EPOCH_N_MINUS_1, SEAL_T0_MS, SampleKind, TempHostWorkspace, derive_digest32, derive_key16,
    hex_decode_exact, hex_encode,
};

/// `knowledge-observatory-app` `v1.0.0` DAG node keys (`harvest -> build-index -> publish`).
pub const OBSERVATORY_NODE_HARVEST: &str = "41a1c2d3e4f5061728394a5b6c7d8e01";
/// `knowledge-observatory-app` `v1.0.0` `build-index` node key.
pub const OBSERVATORY_NODE_BUILD_INDEX: &str = "42b2c3d4e5f60718293a4b5c6d7e8f02";
/// `knowledge-observatory-app` `v1.0.0` `publish-digest` node key.
pub const OBSERVATORY_NODE_PUBLISH: &str = "43c3d4e5f60718293a4b5c6d7e8f0303";
/// `knowledge-observatory-app` `v1.1.0` appended `verify-digest` node key.
pub const OBSERVATORY_NODE_VERIFY_V1_1: &str = "44d4e5f60718293a4b5c6d7e8f030404";

/// `field-survey-app` `v1.0.0` DAG node keys (`plan-route -> collect-notes -> sync-notes`).
pub const SURVEY_NODE_PLAN_ROUTE: &str = "51a1c2d3e4f5061728394a5b6c7d8e01";
/// `field-survey-app` `v1.0.0` `collect-notes` node key.
pub const SURVEY_NODE_COLLECT_NOTES: &str = "52b2c3d4e5f60718293a4b5c6d7e8f02";
/// `field-survey-app` `v1.0.0` `sync-notes` node key.
pub const SURVEY_NODE_SYNC_NOTES: &str = "53c3d4e5f60718293a4b5c6d7e8f0303";
/// `field-survey-app` `v1.1.0` appended `review-notes` node key.
pub const SURVEY_NODE_REVIEW_V1_1: &str = "54d4e5f60718293a4b5c6d7e8f030404";

/// Deterministic `Z4` Trusted-UI operator signing key seed (`[0x74; 32]`).
pub const Z4_OPERATOR_SEED: [u8; 32] = [0x74; 32];
/// Deterministic `field-survey-app` capability holder key seed (`[0x75; 32]`).
pub const SURVEY_HOLDER_SEED: [u8; 32] = [0x75; 32];
/// Deterministic delegated worker key seed (`[0x76; 32]`).
pub const SURVEY_WORKER_DELEGATE_SEED: [u8; 32] = [0x76; 32];
/// Deterministic `knowledge-observatory-app` capability holder key seed (`[0x77; 32]`).
pub const OBSERVATORY_HOLDER_SEED: [u8; 32] = [0x77; 32];

/// Five-dimension `LlmTokenLimits` provisioned on `N-1` and recovered on `N`.
pub const GATEWAY_LLM_TOKEN_LIMITS: LlmTokenLimits = LlmTokenLimits {
    max_prompt_tokens: 16_384,
    max_completion_tokens: 8_192,
    max_total_tokens: 32_768,
    max_api_calls: 16,
    max_cost_microcredits: 120_000,
};

/// Initial credit provisioned for the `MeteredChatGateway` account (`500_000`).
pub const GATEWAY_INITIAL_CREDIT: u64 = 500_000;

/// Per-kilotoken price in microcredits configured on `MeteredChatGateway` (`2_500`).
pub const GATEWAY_PRICE_PER_KILOTOKEN: u64 = 2_500;

/// Domain separator for deterministic Step B keys and digests.
pub const STEP_B_KEY_DOMAIN: &[u8] = b"llmos/w81-road-e001/step-b/key/v1";

/// Per-sample durable installation record captured on Host `N-1`.
#[derive(Clone, Debug)]
pub struct InstalledN1SampleRecord {
    /// Sample label (`knowledge-observatory-app`, etc.).
    pub label: String,
    /// Sample category (`Application` vs `Driver`).
    pub kind: SampleKind,
    /// `PackageId` from frozen `v1.0.0.nlos-pkg`.
    pub package_id: PackageId,
    /// Runtime `ApplicationId` in `ApplicationAuthority`.
    pub runtime_application_id: ApplicationId,
    /// Certify `ApplicationId` registered in `SqliteStoreRegistry`.
    pub certify_application_id: ApplicationId,
    /// Publisher identity registered in `SqlitePublisherTrustAuthority` & `SqliteStoreRegistry`.
    pub publisher_id: PublisherId,
    /// Signer `PrincipalId` bootstrapped into `IdentityAuthority`.
    pub signer_principal: PrincipalId,
    /// Durable `PackageVerificationReceipt` for `v1.0.0`.
    pub verification_v1_0_0: PackageVerificationReceipt,
    /// Durable `InstallationReceipt` (`Generation(1)`) on Host `N-1`.
    pub installation_v1_0_0: InstallationReceipt,
    /// Optional `TaskPlanId` created by templated application installation.
    pub plan_id: Option<TaskPlanId>,
    /// Optional `DriverRecord` registered in `ResourceAuthority` (`Generation(1)`).
    pub driver_record: Option<DriverRecord>,
    /// Durable `UserData` SHA-256 container tree digest recorded at end of Step B.
    pub user_data_tree_digest: [u8; 32],
    /// Frozen `v1.0.0` `PackageFile` retained for read-only structural assertions.
    pub pkg_v1_0_0: PackageFile,
    /// Frozen `v1.1.0` `PackageFile` retained for Step D.6 `v1.0.0 -> v1.1.0` upgrade on Host `N`.
    pub pkg_v1_1_0: PackageFile,
}

/// Complete durable Host `N-1` state snapshot produced by Step B and verified
/// post-upgrade in Step C and Step D.
#[derive(Clone, Debug)]
pub struct StepBN1HostStateSnapshot {
    /// Host major epoch under which Step B executed (`1`).
    pub host_epoch: u32,
    /// All 4 installed frozen `v1.0.0` samples keyed by `label`.
    pub installed_samples: BTreeMap<String, InstalledN1SampleRecord>,
    /// `Z4` Trusted-UI operator `IdentityBinding` in `runtime.identity`.
    pub z4_operator_binding: IdentityBinding,
    /// `field-survey-app` capability holder `IdentityBinding`.
    pub survey_holder_binding: IdentityBinding,
    /// `field-survey-app` delegated worker `IdentityBinding`.
    pub survey_worker_binding: IdentityBinding,
    /// `knowledge-observatory-app` capability holder `IdentityBinding`.
    pub observatory_holder_binding: IdentityBinding,
    /// Shared target `NamespaceId` scoped onto issued `N-1` capabilities.
    pub target_namespace: NamespaceId,
    /// Purpose digest bound onto `field-survey-app` capabilities.
    pub survey_purpose_digest: [u8; 32],
    /// Root `CapabilityHandle` issued to `field-survey-app` on `N-1` (revoked during Step D.7 uninstall).
    pub survey_root_capability: CapabilityHandle,
    /// Attenuated child `CapabilityHandle` delegated to `field-survey-app` worker on `N-1`.
    pub survey_delegated_capability: CapabilityHandle,
    /// Remaining calls on `survey_delegated_capability` at end of Step B (`3` after `1` call of `4`).
    pub survey_delegated_remaining_calls: u64,
    /// Root `CapabilityHandle` issued to `knowledge-observatory-app` on `N-1` (survives into Host `N`).
    pub observatory_root_capability: CapabilityHandle,
    /// Remaining calls on `observatory_root_capability` at end of Step B (`7` after `1` call of `8`).
    pub observatory_remaining_calls: u64,
    /// Immutable `PinnedEcosystemDependencyHandle`s persisted in `SqliteEcosystemDependencyGate` on `N-1`.
    pub pinned_dependency_handles: Vec<PinnedEcosystemDependencyHandle>,
    /// `knowledge-observatory-app` output lineage `ArtifactId` (head revision `3` on `N-1`).
    pub observatory_output_artifact_id: ArtifactId,
    /// `knowledge-observatory-app` revision `3` `ContentDigest` on `N-1`.
    pub observatory_rev3_digest: ContentDigest,
    /// `field-survey-app` output lineage `ArtifactId` (head revision `3` on `N-1`).
    pub survey_output_artifact_id: ArtifactId,
    /// `field-survey-app` revision `3` `ContentDigest` on `N-1`.
    pub survey_rev3_digest: ContentDigest,
    /// Cross-application shared corpus `ArtifactId` (must remain 100% intact after `field-survey-app` uninstall).
    pub shared_corpus_artifact_id: ArtifactId,
    /// Cross-application shared corpus `ContentDigest`.
    pub shared_corpus_digest: ContentDigest,
    /// Active background `TaskId` registered for `field-survey-app` on `N-1`.
    pub survey_active_bg_task_id: TaskId,
    /// Provisioned `ResourceAccountId` inside `llm-gateway` `ResourceAuthority`.
    pub llm_account_id: ResourceAccountId,
    /// First `N-1` completion `request_digest` (replayed on Host `N` to verify 0 double-charge).
    pub observatory_llm_request_digest: [u8; 32],
    /// Durable 5-dimension `LlmTokenBudgetRecord` snapshot at end of Step B.
    pub llm_budget_after_n1: LlmTokenBudgetRecord,
    /// Aggregated streamed SSE text produced by `openai-compat-driver` on `N-1`.
    pub streamed_sse_completion_text: String,
    /// Count of `McpBridgeAdapter` tool calls executed on `N-1` (`2`).
    pub mcp_calls_executed_on_n1: usize,
}

/// Tuple describing one pinned `N-1` entity `(entity_id, kind, version_tag, content_digest)`.
pub type PinnedEntityCatalogEntry = ([u8; 16], PlanDependencyEntityKind, &'static str, [u8; 32]);

/// Deterministic `CrossEcosystemEntitySource` resolving the 4 pinned `N-1`
/// entities (`Package`, `Tool`, `Model`, `Artifact`) for `SqliteEcosystemDependencyGate`.
pub struct HostCrossEcosystemSource {
    entries: BTreeMap<(PlanDependencyEntityKind, [u8; 16]), EcosystemEntitySnapshot>,
}

impl HostCrossEcosystemSource {
    /// Builds the pinned `N-1` entity catalog for `knowledge-observatory-app`.
    #[must_use]
    pub fn for_observatory_n1(
        mcp_driver_pkg_id: PackageId,
        mcp_driver_manifest_digest: [u8; 32],
        seed_artifact_id: ArtifactId,
        seed_digest: [u8; 32],
    ) -> (Self, [PinnedEntityCatalogEntry; 4]) {
        let tool_id = derive_key16(STEP_B_KEY_DOMAIN, b"mcp-tool-search-kb", 1);
        let tool_digest = derive_digest32(STEP_B_KEY_DOMAIN, b"mcp-tool-search-kb-schema", 1);
        let model_id = derive_key16(STEP_B_KEY_DOMAIN, b"openai-compat-glm-4-6", 2);
        let model_digest = derive_digest32(STEP_B_KEY_DOMAIN, b"openai-compat-glm-4-6-weights", 2);

        let specs = [
            (
                *mcp_driver_pkg_id.as_bytes(),
                PlanDependencyEntityKind::Package,
                "v1.0.0",
                mcp_driver_manifest_digest,
            ),
            (
                tool_id,
                PlanDependencyEntityKind::Tool,
                "mcp-tool-v1.0.0",
                tool_digest,
            ),
            (
                model_id,
                PlanDependencyEntityKind::Model,
                "glm-4.6-2026-01",
                model_digest,
            ),
            (
                *seed_artifact_id.as_bytes(),
                PlanDependencyEntityKind::Artifact,
                "rev:1",
                seed_digest,
            ),
        ];

        let mut entries = BTreeMap::new();
        for (entity_id, kind, tag, digest) in specs {
            entries.insert(
                (kind, entity_id),
                EcosystemEntitySnapshot {
                    pinned_version_tag: tag.to_owned(),
                    pinned_generation: 1,
                    content_digest: digest,
                },
            );
        }
        (Self { entries }, specs)
    }
}

impl CrossEcosystemEntitySource for HostCrossEcosystemSource {
    type Error = String;

    fn supports_kind(&self, kind: PlanDependencyEntityKind) -> bool {
        matches!(
            kind,
            PlanDependencyEntityKind::Package
                | PlanDependencyEntityKind::Tool
                | PlanDependencyEntityKind::Model
                | PlanDependencyEntityKind::Artifact
        )
    }

    fn resolve_pinned(
        &self,
        selector: &TypedDependencySelector,
    ) -> Result<CrossEcosystemLookup, Self::Error> {
        Ok(self
            .entries
            .get(&(selector.kind(), selector.entity_id()))
            .cloned()
            .map_or(CrossEcosystemLookup::NotFound, CrossEcosystemLookup::Found))
    }

    fn verify_handle_live(
        &self,
        handle: &PinnedEcosystemDependencyHandle,
    ) -> Result<CrossEcosystemLookup, Self::Error> {
        Ok(self
            .entries
            .get(&(handle.kind, handle.entity_id))
            .cloned()
            .map_or(CrossEcosystemLookup::NotFound, CrossEcosystemLookup::Found))
    }
}

/// Deterministic in-memory MCP JSON-RPC 2.0 server transport used to exercise
/// `mcp_bridge_driver::McpBridgeAdapter` on both Host `N-1` and Host `N`.
pub struct ScriptedMcpServer {
    outgoing: VecDeque<Vec<u8>>,
    server_protocol_version: &'static str,
    drop_tool_call_replies: bool,
    drop_next_tool_call_replies: usize,
    executed_tools: Vec<String>,
}

impl ScriptedMcpServer {
    /// Creates a scripted MCP server advertising `server_protocol_version`.
    #[must_use]
    pub const fn new(server_protocol_version: &'static str) -> Self {
        Self {
            outgoing: VecDeque::new(),
            server_protocol_version,
            drop_tool_call_replies: false,
            drop_next_tool_call_replies: 0,
            executed_tools: Vec::new(),
        }
    }

    /// Configures whether `tools/call` responses are dropped to simulate a mid-flight transport crash.
    pub fn set_drop_tool_call_replies(&mut self, drop_replies: bool) {
        self.drop_tool_call_replies = drop_replies;
    }

    /// Arms a one-shot drop of the very next `tools/call` reply to simulate a mid-flight transport crash.
    pub fn arm_drop_next_tool_call_reply(&mut self) {
        self.drop_next_tool_call_replies = 1;
    }

    /// Names of tools executed on the server side.
    #[must_use]
    pub fn executed_tools(&self) -> &[String] {
        &self.executed_tools
    }

    fn handle_incoming(&mut self, frame: &[u8]) {
        let text = String::from_utf8_lossy(frame);
        let trimmed = text.trim_end_matches(['\n', '\r']);
        let Ok(val) = JsonValue::parse(trimmed) else {
            return;
        };
        let method = val.get("method").and_then(JsonValue::as_str).unwrap_or("");
        let Some(raw_id) = val.get("id").cloned() else {
            return;
        };
        let Some(req_id) = RequestId::from_json(&raw_id) else {
            return;
        };

        match method {
            "initialize" => {
                let init = InitializeResult {
                    protocol_version: self.server_protocol_version.to_owned(),
                    server_name: "nlos-road-e001-mcp-server".to_owned(),
                    server_version: "1.0.0".to_owned(),
                    tools_capability: true,
                }
                .to_json();
                self.push_response(&JsonRpcResponse::success(req_id, init));
            }
            "tools/list" => {
                let tools: Vec<_> = sample_tool_catalog()
                    .into_iter()
                    .map(|(info, _)| info.to_json())
                    .collect();
                let payload = JsonValue::object([("tools", JsonValue::Array(tools))]);
                self.push_response(&JsonRpcResponse::success(req_id, payload));
            }
            "tools/call" => {
                let name = val
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(JsonValue::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                self.executed_tools.push(name.clone());
                let drop_this = if self.drop_next_tool_call_replies > 0 {
                    self.drop_next_tool_call_replies -= 1;
                    true
                } else {
                    self.drop_tool_call_replies
                };
                if !drop_this {
                    let result = ToolCallOutcome {
                        text: format!("mcp-ok:{name}"),
                        is_error: false,
                        structured: None,
                    }
                    .to_json();
                    self.push_response(&JsonRpcResponse::success(req_id, result));
                }
            }
            _ => {}
        }
    }

    fn push_response(&mut self, response: &JsonRpcResponse) {
        self.outgoing
            .push_back(format!("{}\n", response.to_json_string()).into_bytes());
    }
}

impl McpTransport for ScriptedMcpServer {
    fn send_frame(&mut self, frame: &[u8]) -> Result<(), McpBridgeError> {
        self.handle_incoming(frame);
        Ok(())
    }

    fn recv_frame(&mut self) -> Result<Vec<u8>, McpBridgeError> {
        self.outgoing
            .pop_front()
            .ok_or_else(|| McpBridgeError::Transport("simulated mid-flight EOF/crash".to_owned()))
    }
}

/// Materializes all payload entries of a frozen `PackageFile` into `runtime.artifacts`,
/// bootstraps its embedded `SignerDescriptor` into `runtime.identity`, and verifies
/// its cryptographic signature (`verify_package` or `verify_package_with_tasks`)
/// **without rebuilding or re-signing any byte**.
///
/// # Errors
///
/// Fails closed if principal bootstrap, artifact creation/put, or package
/// signature/digest verification fails.
pub fn materialize_and_verify_frozen_package(
    runtime: &SliceKRuntime,
    pkg: &PackageFile,
    phase_tag: &[u8],
) -> Result<
    (
        PrincipalId,
        PackageManifest,
        Option<SignedPackageWithTasks>,
        PackageVerificationReceipt,
    ),
    String,
> {
    let signer_principal = runtime
        .identity
        .bootstrap_principal(pkg.descriptor.bootstrap_request())
        .map_err(|err| format!("bootstrap signer principal: {err}"))?
        .binding()
        .principal_id;

    let at_ms = runtime
        .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
            STEP_B_KEY_DOMAIN,
            phase_tag,
            0,
        )))
        .map_err(|err| format!("wall_now_ms: {err}"))?;

    for (idx, entry) in pkg.entries.iter().enumerate() {
        let ordinal = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        let artifact_id = ArtifactId::from_bytes(entry.artifact_id);
        runtime
            .artifacts
            .create_artifact(CreateArtifactSpec {
                artifact_id,
                idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                    STEP_B_KEY_DOMAIN,
                    phase_tag,
                    ordinal,
                )),
                content_type: "application/octet-stream".to_owned(),
                application_id: None,
                owner: None,
                created_at_ms: at_ms,
            })
            .map_err(|err| format!("create_artifact {}: {err}", entry.name))?;
        runtime
            .artifacts
            .put_revision(PutRevisionRequest {
                artifact_id,
                expected_head_revision: 0,
                bytes: &entry.payload,
                created_at_ms: at_ms,
                provenance: ProvenanceSourceTriple {
                    source_a: entry.artifact_id,
                    source_b: *pkg.package_id.as_bytes(),
                    source_digest: ContentDigest::from_bytes(entry.digest),
                },
            })
            .map_err(|err| format!("put_revision {}: {err}", entry.name))?;
    }

    let manifest = PackageManifest {
        package_id: pkg.package_id,
        version: pkg.version,
        entries: pkg
            .entries
            .iter()
            .map(PackageFileEntry::manifest_entry)
            .collect(),
    };
    let verify_key = IdempotencyKey::from_bytes(derive_key16(STEP_B_KEY_DOMAIN, phase_tag, 900));

    if pkg.tasks.is_empty() {
        let signed = SignedPackage {
            manifest: manifest.clone(),
            signer: signer_principal,
            signature: pkg.signature,
        };
        let decision = runtime
            .artifacts
            .verify_package(
                &runtime.identity,
                VerifyPackageRequest {
                    signed: &signed,
                    idempotency_key: verify_key,
                    verified_at_ms: at_ms,
                },
            )
            .map_err(|err| format!("verify_package: {err}"))?;
        Ok((signer_principal, manifest, None, decision.receipt().clone()))
    } else {
        let signed_templated = SignedPackageWithTasks {
            manifest: manifest.clone(),
            tasks: pkg.tasks.clone(),
            signer: signer_principal,
            signature: pkg.signature,
        };
        let decision = runtime
            .artifacts
            .verify_package_with_tasks(
                &runtime.identity,
                VerifyPackageWithTasksRequest {
                    signed: &signed_templated,
                    idempotency_key: verify_key,
                    verified_at_ms: at_ms,
                },
            )
            .map_err(|err| format!("verify_package_with_tasks: {err}"))?;
        Ok((
            signer_principal,
            manifest,
            Some(signed_templated),
            decision.receipt().clone(),
        ))
    }
}

/// Resolves a declared `TaskNodeId` and its `declared_revision` by 32-hex `node_key`.
///
/// # Errors
///
/// Returns an error if `node_key_hex` is malformed or absent from `plan_id`.
pub fn resolve_plan_node_by_hex_key(
    plans: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_key_hex: &str,
) -> Result<(TaskNodeId, u64), String> {
    let expected_key = hex_decode_exact::<16>(node_key_hex, "task node_key")?;
    for record in plans
        .list_plan_nodes(plan_id)
        .map_err(|err| format!("list_plan_nodes: {err}"))?
    {
        if record.node_key == expected_key {
            return Ok((record.node_id, record.declared_revision));
        }
    }
    Err(format!(
        "node_key {node_key_hex} not found in plan {}",
        hex_encode(plan_id.as_bytes())
    ))
}

/// Advances `(plan_id, node_id)` through `request_materialization -> resolve_materialization -> Active`.
///
/// # Errors
///
/// Propagates any `SqlitePlanAuthority` or clock failure.
pub fn materialize_and_activate_plan_node(
    runtime: &SliceKRuntime,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    declared_revision: u64,
    phase_tag: &[u8],
) -> Result<(), String> {
    let req_key = IdempotencyKey::from_bytes(derive_key16(STEP_B_KEY_DOMAIN, phase_tag, 1));
    let req_at = runtime
        .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
            STEP_B_KEY_DOMAIN,
            phase_tag,
            2,
        )))
        .map_err(|err| format!("wall_now_ms: {err}"))?;
    runtime
        .plans()
        .request_materialization(MaterializationRequest {
            plan_id,
            node_id,
            idempotency_key: req_key,
            requested_at_ms: req_at,
        })
        .map_err(|err| format!("request_materialization: {err}"))?;

    let projected_nodes = runtime
        .plans()
        .inspect_declared_task_node_count()
        .map_err(|err| format!("inspect_declared_task_node_count: {err}"))?;
    let resolved_at_ms = runtime
        .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
            STEP_B_KEY_DOMAIN,
            phase_tag,
            3,
        )))
        .map_err(|err| format!("wall_now_ms: {err}"))?;
    let decision = runtime
        .plans()
        .resolve_materialization(MaterializationResolution {
            request_key: req_key,
            verdict: MaterializationAdmissionVerdict::Approved(MaterializationAdmission {
                profile_id: "road-e001-workstation".to_owned(),
                projected_task_nodes: projected_nodes,
                projected_active_working_set: 1,
            }),
            resolved_at_ms,
        })
        .map_err(|err| format!("resolve_materialization: {err}"))?;
    if !matches!(
        decision,
        MaterializationResolutionDecision::Approved(_)
            | MaterializationResolutionDecision::ReplayedApproved(_)
    ) {
        return Err(format!(
            "expected Approved materialization, got {decision:?}"
        ));
    }

    let active_at = runtime
        .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
            STEP_B_KEY_DOMAIN,
            phase_tag,
            4,
        )))
        .map_err(|err| format!("wall_now_ms: {err}"))?;
    runtime
        .plans()
        .record_node_transition(NodeTransitionRequest {
            plan_id,
            node_id,
            from_state: PlanNodeState::Materializing,
            to_state: PlanNodeState::Active,
            expected_declared_revision: declared_revision,
            idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                STEP_B_KEY_DOMAIN,
                phase_tag,
                5,
            )),
            transitioned_at_ms: active_at,
        })
        .map_err(|err| format!("record_node_transition(Active): {err}"))?;
    Ok(())
}

/// Transitions an `Active` plan node into `Completed`.
///
/// # Errors
///
/// Propagates any `SqlitePlanAuthority` or clock failure.
pub fn complete_active_plan_node(
    runtime: &SliceKRuntime,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    declared_revision: u64,
    phase_tag: &[u8],
) -> Result<(), String> {
    let completed_at = runtime
        .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
            STEP_B_KEY_DOMAIN,
            phase_tag,
            6,
        )))
        .map_err(|err| format!("wall_now_ms: {err}"))?;
    runtime
        .plans()
        .record_node_transition(NodeTransitionRequest {
            plan_id,
            node_id,
            from_state: PlanNodeState::Active,
            to_state: PlanNodeState::Completed,
            expected_declared_revision: declared_revision,
            idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                STEP_B_KEY_DOMAIN,
                phase_tag,
                7,
            )),
            transitioned_at_ms: completed_at,
        })
        .map_err(|err| format!("record_node_transition(Completed): {err}"))?;
    Ok(())
}

/// Reads a materialized read-only seed or service entry directly from `ArtifactStore`
/// (`[PKG-SEED-001]`).
///
/// # Errors
///
/// Returns an error if revision `1` of the derived `ArtifactId` cannot be read.
pub fn read_materialized_package_entry(
    artifacts: &ArtifactStore,
    package_id: PackageId,
    package_version: u64,
    entry_name: &str,
    now_ms: u64,
) -> Result<Vec<u8>, String> {
    let artifact_id =
        ArtifactId::from_bytes(derive_artifact_id(package_id, package_version, entry_name));
    artifacts
        .get_revision(artifact_id, 1, now_ms)
        .map_err(|err| format!("read materialized entry {entry_name:?}: {err}"))
}

/// Executes Step B (`ADR-0027` section 2.4 Step B) on Host `N-1` (`host_epoch = 1`)
/// and cleanly closes all `N-1` handles before returning [`StepBN1HostStateSnapshot`].
///
/// # Errors
///
/// Fails closed if any installation, dependency pin, capability issuance, driver
/// execution, `TaskPlan` transition, or `UserData` digest check fails.
#[allow(clippy::too_many_lines)]
pub fn run_step_b_install_and_run_on_n1_host(
    repo_root: &Path,
    host_ws: &TempHostWorkspace,
    step_a: &StepAFrozenSealReceipt,
) -> Result<StepBN1HostStateSnapshot, String> {
    let auth_root = host_ws.authority_root();
    fs::create_dir_all(&auth_root).map_err(|err| format!("create authority_root: {err}"))?;

    // 1. Provision durable Store Governance under `<authority_root>/store-governance`
    //    so `(PublisherId, ApplicationId)` ownership bindings and Stable releases
    //    persist across the `N-1 -> N` host upgrade.
    let expected_seals: Vec<ParsedStoreSealRecord> = step_a
        .samples
        .iter()
        .map(|s| s.seal_record.clone())
        .collect();
    let (trust_gov, cas_gov, registry_gov, _inter_gov) = provision_store_governance_catalog(
        repo_root,
        &auth_root.join("store-governance"),
        Some(&expected_seals),
    )?;
    drop(registry_gov);
    drop(cas_gov);
    drop(trust_gov);

    // 2. Boot Host `N-1` core authorities over `host_ws.authority_root()`.
    let runtime = SliceKRuntime::open(&auth_root)
        .map_err(|err| format!("SliceKRuntime::open on N-1: {err}"))?;
    let driver_resources = ResourceAuthority::open(auth_root.join("driver-registry"))
        .map_err(|err| format!("ResourceAuthority::open(driver-registry) on N-1: {err}"))?;
    let gateway = MeteredChatGateway::open(
        &auth_root.join("llm-gateway"),
        GATEWAY_LLM_TOKEN_LIMITS,
        GATEWAY_INITIAL_CREDIT,
        SEAL_T0_MS + 10_000,
    )
    .map_err(|err| format!("MeteredChatGateway::open on N-1: {err}"))?
    .with_price_per_kilotoken(GATEWAY_PRICE_PER_KILOTOKEN);
    let dep_gate = SqliteEcosystemDependencyGate::open(auth_root.join("ecosystem-gate"))
        .map_err(|err| format!("SqliteEcosystemDependencyGate::open on N-1: {err}"))?;

    // 3. Install all 4 frozen `v1.0.0.nlos-pkg` packages directly from Step A's
    //    verified frozen bytes (`0` rebuild, `0` re-sign).
    let mut installed_samples = BTreeMap::new();
    for sample in &step_a.samples {
        let record = install_one_frozen_v1_sample(&runtime, &driver_resources, host_ws, sample)?;
        installed_samples.insert(sample.spec.label.to_owned(), record);
    }

    // 4. Execute payloads & read materialized seed/service artifacts on Host `N-1`.
    for label in ["knowledge-observatory-app", "field-survey-app"] {
        let rec = installed_samples
            .get(label)
            .ok_or_else(|| format!("missing installed app {label}"))?;
        let spec = step_a
            .samples
            .iter()
            .find(|s| s.spec.label == label)
            .ok_or_else(|| format!("missing spec {label}"))?
            .spec;
        let payload_exec =
            execute_application_payload(&runtime, rec.package_id, spec.executable_entry)
                .map_err(|err| format!("execute_application_payload {label} on N-1: {err}"))?;
        if payload_exec.payload_size_bytes == 0 {
            return Err(format!(
                "{label} executable payload returned 0 bytes on N-1"
            ));
        }
        let read_ms = runtime
            .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
                STEP_B_KEY_DOMAIN,
                label.as_bytes(),
                50,
            )))
            .map_err(|err| format!("wall_now_ms: {err}"))?;
        let seed_bytes = read_materialized_package_entry(
            &runtime.artifacts,
            rec.package_id,
            rec.pkg_v1_0_0.version,
            spec.seed_entry,
            read_ms,
        )?;
        if seed_bytes.is_empty() {
            return Err(format!("{label} seed entry {:?} is empty", spec.seed_entry));
        }
        if let Some(svc_entry) = spec.service_entry {
            let svc_bytes = read_materialized_package_entry(
                &runtime.artifacts,
                rec.package_id,
                rec.pkg_v1_0_0.version,
                svc_entry,
                read_ms,
            )?;
            if svc_bytes.is_empty() {
                return Err(format!("{label} service entry {svc_entry:?} is empty"));
            }
        }
    }

    // 5. Enforce `[PLAN-DEPENDENCY-001]` & `[SEARCH-AUTH-002]` (`0x1F`) on Host `N-1`,
    //    then pin 4 immutable `PinnedEcosystemDependencyHandle`s (`Package`, `Tool`,
    //    `Model`, `Artifact`) onto `knowledge-observatory-app`'s `harvest` node.
    verify_search_and_floating_pin_firewalls()?;
    let pinned_dependency_handles =
        pin_observatory_dependencies_on_n1(&runtime, &dep_gate, &installed_samples)?;

    // 6. Bootstrap `Z4` operator + application principals in `runtime.identity`,
    //    issue root & attenuated delegated `Capability` handles in `runtime.capability()`,
    //    and consume 1 authorized call on each so remaining call counters persist durably.
    let (
        z4_operator_binding,
        survey_holder_binding,
        survey_worker_binding,
        observatory_holder_binding,
        target_namespace,
        survey_purpose_digest,
        survey_root_capability,
        survey_delegated_capability,
        survey_delegated_remaining_calls,
        observatory_root_capability,
        observatory_remaining_calls,
    ) = issue_and_exercise_n1_capabilities(&runtime)?;

    // 7. Drive `mcp-bridge-driver` (`McpBridgeAdapter`) using the `"mcp-effect-declarations"`
    //    seed read directly from `runtime.artifacts`, plus `[DRV-NEGO-001]` negative check.
    let mcp_rec = installed_samples
        .get("mcp-bridge-driver")
        .ok_or_else(|| "missing mcp-bridge-driver".to_owned())?;
    let mcp_read_ms = runtime
        .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
            STEP_B_KEY_DOMAIN,
            b"mcp-seed-read",
            1,
        )))
        .map_err(|err| format!("wall_now_ms: {err}"))?;
    let mcp_effects_raw = read_materialized_package_entry(
        &runtime.artifacts,
        mcp_rec.package_id,
        mcp_rec.pkg_v1_0_0.version,
        "mcp-effect-declarations",
        mcp_read_ms,
    )?;
    let mcp_effects_text = String::from_utf8(mcp_effects_raw)
        .map_err(|err| format!("mcp-effect-declarations UTF-8: {err}"))?;
    let mcp_effects: Vec<EffectDescriptor> = parse_effect_declarations(&mcp_effects_text)
        .map_err(|err| format!("parse_effect_declarations: {err}"))?;
    let mcp_calls_executed_on_n1 = exercise_mcp_bridge_on_n1(&mcp_effects)?;

    // 8. Drive `openai-compat-driver` (`ChatCompletionRequest` + `SseParser` /
    //    `StreamAggregator` + `MeteredChatGateway` 5-D `LlmTokenBudget` + same-digest replay).
    let (observatory_llm_request_digest, streamed_sse_completion_text, llm_budget_after_n1) =
        exercise_openai_compat_gateway_on_n1(&gateway)?;

    // 9. Execute `knowledge-observatory-app` (`harvest -> build-index -> publish-digest`)
    //    and `field-survey-app` (`plan-route -> collect-notes -> sync-notes`) `TaskPlan`
    //    DAGs, producing 3 causal-lineage `ArtifactStore` revisions per app plus 1 shared
    //    cross-app corpus `Artifact`.
    let (observatory_output_artifact_id, observatory_rev3_digest) = execute_app_three_node_dag(
        &runtime,
        installed_samples
            .get("knowledge-observatory-app")
            .ok_or_else(|| "missing observatory".to_owned())?,
        [
            ("harvest", OBSERVATORY_NODE_HARVEST),
            ("build-index", OBSERVATORY_NODE_BUILD_INDEX),
            ("publish-digest", OBSERVATORY_NODE_PUBLISH),
        ],
        b"obs-n1-dag",
    )?;
    let (survey_output_artifact_id, survey_rev3_digest) = execute_app_three_node_dag(
        &runtime,
        installed_samples
            .get("field-survey-app")
            .ok_or_else(|| "missing survey".to_owned())?,
        [
            ("plan-route", SURVEY_NODE_PLAN_ROUTE),
            ("collect-notes", SURVEY_NODE_COLLECT_NOTES),
            ("sync-notes", SURVEY_NODE_SYNC_NOTES),
        ],
        b"srv-n1-dag",
    )?;

    let (shared_corpus_artifact_id, shared_corpus_digest) =
        create_shared_cross_app_corpus_artifact(&runtime)?;

    // 10. Register a durable `Active` background `Task` for `field-survey-app` so it
    //     persists across `N-1 -> N` and enforces `[PKG-UNINSTALL-001]` active-task gating.
    let survey_rec = installed_samples
        .get("field-survey-app")
        .ok_or_else(|| "missing field-survey-app".to_owned())?;
    let survey_active_bg_task_id = register_active_background_task_on_n1(&runtime, survey_rec)?;

    // 11. Populate `UserData` containers for all 4 samples + private cache for
    //     `field-survey-app`, recording deterministic SHA-256 container tree digests.
    for (label, rec) in &mut installed_samples {
        let container_dir = host_ws.user_data_dir(label);
        fs::create_dir_all(container_dir.join("records"))
            .map_err(|err| format!("create UserData dir for {label}: {err}"))?;
        let state_payload = format!(
            "schema=nlos.userdata.v1\nsample={label}\nhost_epoch={HOST_EPOCH_N_MINUS_1}\ninstallation_id={}\ngeneration={}\n",
            hex_encode(rec.installation_v1_0_0.installation_id.as_bytes()),
            rec.installation_v1_0_0.installation_generation.get()
        );
        fs::write(
            container_dir.join("records/state-v1.txt"),
            state_payload.as_bytes(),
        )
        .map_err(|err| format!("write UserData state-v1.txt for {label}: {err}"))?;
        fs::write(
            container_dir.join("records/journal-0001.log"),
            format!("committed_on_epoch_1:{label}:ok\n").as_bytes(),
        )
        .map_err(|err| format!("write UserData journal for {label}: {err}"))?;
        rec.user_data_tree_digest = compute_container_tree_digest(&container_dir)
            .map_err(|err| format!("compute_container_tree_digest for {label}: {err}"))?;
    }

    let survey_cache = host_ws.private_cache_dir("field-survey-app");
    fs::create_dir_all(survey_cache.join("tiles"))
        .map_err(|err| format!("create private cache dir: {err}"))?;
    fs::write(
        survey_cache.join("tiles/tile-00.cache"),
        b"ephemeral-tile-00",
    )
    .map_err(|err| format!("write cache tile-00: {err}"))?;
    fs::write(survey_cache.join("index.cache"), b"ephemeral-cache-index")
        .map_err(|err| format!("write cache index: {err}"))?;

    let llm_account_id = gateway.account_id();

    // 12. Cleanly close all Host `N-1` database handles before Step C in-place upgrade.
    drop(dep_gate);
    drop(gateway);
    drop(driver_resources);
    drop(runtime);

    Ok(StepBN1HostStateSnapshot {
        host_epoch: HOST_EPOCH_N_MINUS_1,
        installed_samples,
        z4_operator_binding,
        survey_holder_binding,
        survey_worker_binding,
        observatory_holder_binding,
        target_namespace,
        survey_purpose_digest,
        survey_root_capability,
        survey_delegated_capability,
        survey_delegated_remaining_calls,
        observatory_root_capability,
        observatory_remaining_calls,
        pinned_dependency_handles,
        observatory_output_artifact_id,
        observatory_rev3_digest,
        survey_output_artifact_id,
        survey_rev3_digest,
        shared_corpus_artifact_id,
        shared_corpus_digest,
        survey_active_bg_task_id,
        llm_account_id,
        observatory_llm_request_digest,
        llm_budget_after_n1,
        streamed_sse_completion_text,
        mcp_calls_executed_on_n1,
    })
}

fn install_one_frozen_v1_sample(
    runtime: &SliceKRuntime,
    driver_resources: &ResourceAuthority,
    host_ws: &TempHostWorkspace,
    sample: &VerifiedFrozenSample,
) -> Result<InstalledN1SampleRecord, String> {
    let phase = format!("install-v1-{}", sample.spec.label);
    let (signer_principal, _manifest, maybe_templated, verification_v1_0_0) =
        materialize_and_verify_frozen_package(runtime, &sample.pkg_v1_0_0, phase.as_bytes())?;

    let seed = 0x30_u8.wrapping_add(sample.spec.index.wrapping_mul(17));
    let (installation_v1_0_0, plan_id, driver_record) = match sample.spec.kind {
        SampleKind::MultiComponentApplication => {
            let signed_templated = maybe_templated.ok_or_else(|| {
                format!(
                    "expected templated tasks in frozen application {}",
                    sample.spec.label
                )
            })?;
            let templated_receipt = runtime
                .install_verified_templated_package_with_gc(
                    &verification_v1_0_0,
                    &signed_templated,
                    seed,
                    AutoOrphanGc::Enabled,
                )
                .map_err(|err| {
                    format!(
                        "install_verified_templated_package {}: {err}",
                        sample.spec.label
                    )
                })?;
            let plan_rev = templated_receipt.plan.receipt();
            if plan_rev.revision != 1 || plan_rev.declared_node_count != 3 {
                return Err(format!(
                    "unexpected initial TaskPlan revision for {}: rev={}, nodes={}",
                    sample.spec.label, plan_rev.revision, plan_rev.declared_node_count
                ));
            }
            (templated_receipt.installation, Some(plan_rev.plan_id), None)
        }
        SampleKind::UserlandDriver => {
            let installation = runtime
                .install_verified_package_with_gc(&verification_v1_0_0, seed, AutoOrphanGc::Enabled)
                .map_err(|err| format!("install_verified_package {}: {err}", sample.spec.label))?;
            let drv_at = runtime
                .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
                    STEP_B_KEY_DOMAIN,
                    phase.as_bytes(),
                    77,
                )))
                .map_err(|err| format!("wall_now_ms: {err}"))?;
            let drv_rec = driver_resources
                .register_driver(RegisterDriverRequest {
                    profile_digest: *verification_v1_0_0.manifest_digest.as_bytes(),
                    idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                        STEP_B_KEY_DOMAIN,
                        phase.as_bytes(),
                        78,
                    )),
                    created_at_ms: drv_at,
                })
                .map_err(|err| format!("register_driver {}: {err}", sample.spec.label))?
                .record();
            (installation, None, Some(drv_rec))
        }
    };

    if installation_v1_0_0.application_id != sample.runtime_application_id {
        return Err(format!(
            "runtime ApplicationId mismatch on install for {}",
            sample.spec.label
        ));
    }
    if installation_v1_0_0.installation_generation.get() != 1 {
        return Err(format!(
            "expected Generation(1) on N-1 install for {}, got {}",
            sample.spec.label,
            installation_v1_0_0.installation_generation.get()
        ));
    }

    fs::create_dir_all(host_ws.user_data_dir(sample.spec.label))
        .map_err(|err| format!("init user_data_dir {}: {err}", sample.spec.label))?;

    Ok(InstalledN1SampleRecord {
        label: sample.spec.label.to_owned(),
        kind: sample.spec.kind,
        package_id: sample.pkg_v1_0_0.package_id,
        runtime_application_id: sample.runtime_application_id,
        certify_application_id: sample.certify_application_id,
        publisher_id: sample.publisher_id,
        signer_principal,
        verification_v1_0_0,
        installation_v1_0_0,
        plan_id,
        driver_record,
        user_data_tree_digest: [0_u8; 32],
        pkg_v1_0_0: sample.pkg_v1_0_0.clone(),
        pkg_v1_1_0: sample.pkg_v1_1_0.clone(),
    })
}

fn verify_search_and_floating_pin_firewalls() -> Result<(), String> {
    if SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL != 0x1F {
        return Err(format!(
            "expected SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL == 0x1F, got {SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL:#x}"
        ));
    }
    let restrictions = SearchUseRestrictions::NON_AUTHORITATIVE;
    if !restrictions.is_strictly_non_authoritative() {
        return Err("SearchUseRestrictions::NON_AUTHORITATIVE must forbid all 5 gates".to_owned());
    }
    restrictions
        .enforce_not_control_authoritative(ControlConsumerKind::CandidateDiscovery)
        .map_err(|err| format!("CandidateDiscovery unexpectedly rejected: {err}"))?;

    for forbidden_consumer in [
        ControlConsumerKind::CapabilityDecision,
        ControlConsumerKind::ResourceAccountCommit,
        ControlConsumerKind::TaskHeadCommit,
        ControlConsumerKind::PackageUpdate,
        ControlConsumerKind::IrreversibleAction,
    ] {
        match restrictions.enforce_not_control_authoritative(forbidden_consumer) {
            Err(SearchError::ControlAuthoritativeUseForbidden(purpose))
                if purpose == forbidden_consumer => {}
            other => {
                return Err(format!(
                    "expected ControlAuthoritativeUseForbidden for {forbidden_consumer}, got {other:?}"
                ));
            }
        }
    }

    for floating in ["latest", "*", "^1.0.0", ">=1.0.0", "canary", "gen:0", ""] {
        if !matches!(
            parse_version_pin(floating),
            Err(EcosystemGateError::FloatingVersionForbidden { .. })
        ) {
            return Err(format!(
                "floating dependency pin {floating:?} must fail closed with FloatingVersionForbidden"
            ));
        }
    }

    Ok(())
}

#[allow(clippy::too_many_lines)]
fn pin_observatory_dependencies_on_n1(
    runtime: &SliceKRuntime,
    dep_gate: &SqliteEcosystemDependencyGate,
    installed_samples: &BTreeMap<String, InstalledN1SampleRecord>,
) -> Result<Vec<PinnedEcosystemDependencyHandle>, String> {
    let obs = installed_samples
        .get("knowledge-observatory-app")
        .ok_or_else(|| "missing knowledge-observatory-app".to_owned())?;
    let mcp = installed_samples
        .get("mcp-bridge-driver")
        .ok_or_else(|| "missing mcp-bridge-driver".to_owned())?;
    let plan_id = obs
        .plan_id
        .ok_or_else(|| "knowledge-observatory-app missing TaskPlanId".to_owned())?;
    let (harvest_node, _harvest_rev) =
        resolve_plan_node_by_hex_key(runtime.plans(), plan_id, OBSERVATORY_NODE_HARVEST)?;

    let seed_entry = obs
        .pkg_v1_0_0
        .entries
        .iter()
        .find(|e| e.name == "observatory-profile")
        .ok_or_else(|| "missing observatory-profile entry".to_owned())?;

    let (entity_source, specs) = HostCrossEcosystemSource::for_observatory_n1(
        mcp.package_id,
        *mcp.verification_v1_0_0.manifest_digest.as_bytes(),
        ArtifactId::from_bytes(seed_entry.artifact_id),
        seed_entry.digest,
    );

    // Negative assertion: unpinned search candidate (`发现 != 授权`) must fail closed before writing any row.
    let unpinned_attempt = dep_gate.bind_node_dependency_proposals(
        runtime.plans(),
        BindNodeDependenciesRequest {
            plan_id,
            node_id: harvest_node,
            proposals: vec![TypedDependencySelectorProposal {
                kind: PlanDependencyEntityKind::Package,
                entity_id: *mcp.package_id.as_bytes(),
                version_or_tag: SelectorVersionPin::ExactTag("v1.0.0".to_owned()),
                expected_content_digest: Some(*mcp.verification_v1_0_0.manifest_digest.as_bytes()),
                provenance: SelectorProvenance::UnpinnedSearchResult,
            }],
            idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                STEP_B_KEY_DOMAIN,
                b"unpinned-search-reject",
                1,
            )),
            bound_at_ms: SEAL_T0_MS + 11_000,
        },
    );
    if !matches!(
        unpinned_attempt,
        Err(EcosystemGateError::UnpinnedSearchResultForbidden { .. })
    ) {
        return Err(format!(
            "expected UnpinnedSearchResultForbidden, got {unpinned_attempt:?}"
        ));
    }

    let proposals: Vec<TypedDependencySelectorProposal> = specs
        .iter()
        .map(|(entity_id, kind, tag, digest)| {
            Ok(TypedDependencySelectorProposal {
                kind: *kind,
                entity_id: *entity_id,
                version_or_tag: parse_version_pin(tag)
                    .map_err(|err| format!("parse_version_pin({tag}): {err}"))?,
                expected_content_digest: Some(*digest),
                provenance: SelectorProvenance::DeclaredPinnedManifest,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    let bound = dep_gate
        .bind_node_dependency_proposals(
            runtime.plans(),
            BindNodeDependenciesRequest {
                plan_id,
                node_id: harvest_node,
                proposals,
                idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                    STEP_B_KEY_DOMAIN,
                    b"obs-harvest-bind",
                    1,
                )),
                bound_at_ms: SEAL_T0_MS + 11_100,
            },
        )
        .map_err(|err| format!("bind_node_dependency_proposals: {err}"))?;
    if bound.len() != 4 {
        return Err(format!(
            "expected 4 bound dependency selectors, got {}",
            bound.len()
        ));
    }

    let mut handles = Vec::with_capacity(specs.len());
    for (idx, (entity_id, kind, _tag, _digest)) in specs.iter().enumerate() {
        let ordinal = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        let handle = dep_gate
            .resolve_node_dependency(
                runtime.plans(),
                &entity_source,
                ResolveNodeDependencyRequest {
                    plan_id,
                    node_id: harvest_node,
                    kind: *kind,
                    entity_id: *entity_id,
                    idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                        STEP_B_KEY_DOMAIN,
                        b"obs-harvest-resolve",
                        ordinal,
                    )),
                    resolved_at_ms: SEAL_T0_MS + 11_200 + u64::from(ordinal),
                },
            )
            .map_err(|err| format!("resolve_node_dependency {kind}: {err}"))?
            .into_handle();
        handles.push(handle);
    }

    Ok(handles)
}

#[allow(clippy::too_many_lines, clippy::type_complexity)]
fn issue_and_exercise_n1_capabilities(
    runtime: &SliceKRuntime,
) -> Result<
    (
        IdentityBinding,
        IdentityBinding,
        IdentityBinding,
        IdentityBinding,
        NamespaceId,
        [u8; 32],
        CapabilityHandle,
        CapabilityHandle,
        u64,
        CapabilityHandle,
        u64,
    ),
    String,
> {
    let bootstrap = |seed_arr: [u8; 32],
                     tag: &[u8],
                     ord: u32|
     -> Result<(SigningKey, IdentityBinding), String> {
        let key = SigningKey::from_bytes(&seed_arr);
        let binding = runtime
            .identity
            .bootstrap_principal(BootstrapPrincipalRequest {
                principal_profile_digest: derive_digest32(STEP_B_KEY_DOMAIN, tag, ord),
                control_domain_policy_digest: derive_digest32(STEP_B_KEY_DOMAIN, tag, ord + 100),
                public_key: key.verifying_key().to_bytes(),
                key_purpose: KeyPurpose::SemanticSigning,
                key_valid_from_ms: 0,
                key_valid_until_ms: u64::try_from(i64::MAX).unwrap_or(u64::MAX),
                idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                    STEP_B_KEY_DOMAIN,
                    tag,
                    ord,
                )),
                created_at_ms: SEAL_T0_MS,
            })
            .map_err(|err| format!("bootstrap_principal: {err}"))?
            .binding();
        Ok((key, binding))
    };

    let (z4_key, z4_binding) = bootstrap(Z4_OPERATOR_SEED, b"z4-operator", 1)?;
    let (survey_holder_key, survey_holder_binding) =
        bootstrap(SURVEY_HOLDER_SEED, b"survey-holder", 2)?;
    let (survey_worker_key, survey_worker_binding) =
        bootstrap(SURVEY_WORKER_DELEGATE_SEED, b"survey-worker", 3)?;
    let (obs_holder_key, obs_holder_binding) =
        bootstrap(OBSERVATORY_HOLDER_SEED, b"observatory-holder", 4)?;

    let target_namespace = NamespaceId::from_bytes(derive_key16(STEP_B_KEY_DOMAIN, b"ns-eco", 1));
    let survey_purpose_digest = derive_digest32(STEP_B_KEY_DOMAIN, b"purpose-field-survey", 1);
    let obs_purpose_digest = derive_digest32(STEP_B_KEY_DOMAIN, b"purpose-observatory", 2);

    // Issue root capability to `field-survey-app` holder (`SEMANTIC_APPEND | SEMANTIC_RETRACT | DELEGATE`, call_limit = 8).
    let survey_root_cmd = IssueRootCapabilityRequest {
        issuer_key_id: z4_binding.key_id,
        holder_key_id: survey_holder_binding.key_id,
        target: CapabilityTarget::Namespace(target_namespace),
        rights: CapabilityRights::SEMANTIC_APPEND
            .union(CapabilityRights::SEMANTIC_RETRACT)
            .union(CapabilityRights::DELEGATE),
        purpose_digest: Some(survey_purpose_digest),
        valid_from_ms: SEAL_T0_MS,
        valid_until_ms: SEAL_T0_MS + 365 * 86_400_000,
        delegation_depth_remaining: 2,
        call_limit: Some(8),
        idempotency_key: IdempotencyKey::from_bytes(derive_key16(
            STEP_B_KEY_DOMAIN,
            b"cap-survey-root",
            1,
        )),
        issued_at_ms: SEAL_T0_MS + 12_000,
    };
    let survey_root_sig = z4_key
        .sign(&issue_root_command_message(survey_root_cmd))
        .to_bytes();
    let survey_root_rec = runtime
        .capability()
        .issue_root_signed(
            &runtime.identity,
            SignedIssueRootCapabilityRequest {
                command: survey_root_cmd,
                signer: z4_binding.principal_id,
                signature: survey_root_sig,
            },
        )
        .map_err(|err| format!("issue_root_signed(survey): {err}"))?
        .record();
    let survey_root_handle = survey_root_rec.handle;

    // Attenuate via `delegate_signed` to `survey_worker_binding` (`SEMANTIC_APPEND` only, call_limit = 4, depth = 0).
    let survey_del_cmd = DelegateCapabilityRequest {
        parent: survey_root_handle,
        delegator_key_id: survey_holder_binding.key_id,
        recipient_key_id: survey_worker_binding.key_id,
        target: CapabilityTarget::Namespace(target_namespace),
        rights: CapabilityRights::SEMANTIC_APPEND,
        purpose_digest: Some(survey_purpose_digest),
        valid_from_ms: SEAL_T0_MS,
        valid_until_ms: SEAL_T0_MS + 180 * 86_400_000,
        delegation_depth_remaining: 0,
        call_limit: Some(4),
        idempotency_key: IdempotencyKey::from_bytes(derive_key16(
            STEP_B_KEY_DOMAIN,
            b"cap-survey-delegate",
            1,
        )),
        delegated_at_ms: SEAL_T0_MS + 12_100,
    };
    let survey_del_sig = survey_holder_key
        .sign(&delegate_command_message(survey_del_cmd))
        .to_bytes();
    let survey_del_rec = runtime
        .capability()
        .delegate_signed(
            &runtime.identity,
            SignedDelegateCapabilityRequest {
                command: survey_del_cmd,
                signer: survey_holder_binding.principal_id,
                signature: survey_del_sig,
            },
        )
        .map_err(|err| format!("delegate_signed(survey_worker): {err}"))?
        .record();
    let survey_del_handle = survey_del_rec.handle;

    // Consume 1 SEMANTIC_APPEND call on `survey_del_handle` (`4 -> 3` remaining).
    let worker_event =
        SemanticEventId::from_bytes(derive_digest32(STEP_B_KEY_DOMAIN, b"worker-sem-ev", 1));
    let worker_sem_msg = nlos_identity::semantic_signature_message(worker_event);
    let worker_verified = runtime
        .identity
        .verify_semantic_signature(VerifySemanticSignatureRequest {
            event_id: worker_event,
            issuer: survey_worker_binding.principal_id,
            control_domain_id: survey_worker_binding.control_domain_id,
            key_id: survey_worker_binding.key_id,
            signature: survey_worker_key.sign(&worker_sem_msg).to_bytes(),
            admitted_at_ms: SEAL_T0_MS + 12_200,
        })
        .map_err(|err| format!("verify_semantic_signature(worker): {err}"))?;
    let worker_consume = runtime
        .capability()
        .consume(ConsumeCapabilityRequest {
            handle: survey_del_handle,
            signer: worker_verified,
            target: CapabilityTarget::Namespace(target_namespace),
            required_right: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: Some(survey_purpose_digest),
            idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                STEP_B_KEY_DOMAIN,
                b"cap-survey-worker-consume",
                1,
            )),
            consumed_at_ms: SEAL_T0_MS + 12_200,
        })
        .map_err(|err| format!("consume(survey_del_handle): {err}"))?
        .receipt();
    if worker_consume.remaining != Some(3) {
        return Err(format!(
            "expected remaining == Some(3) on survey_del_handle, got {:?}",
            worker_consume.remaining
        ));
    }

    // Issue root capability to `knowledge-observatory-app` holder (`SEMANTIC_APPEND | SEMANTIC_RETRACT`, call_limit = 8)
    // and consume 1 call (`8 -> 7` remaining) so Host `N` verifies exact durable counter continuity.
    let obs_root_cmd = IssueRootCapabilityRequest {
        issuer_key_id: z4_binding.key_id,
        holder_key_id: obs_holder_binding.key_id,
        target: CapabilityTarget::Namespace(target_namespace),
        rights: CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::SEMANTIC_RETRACT),
        purpose_digest: Some(obs_purpose_digest),
        valid_from_ms: SEAL_T0_MS,
        valid_until_ms: SEAL_T0_MS + 365 * 86_400_000,
        delegation_depth_remaining: 1,
        call_limit: Some(8),
        idempotency_key: IdempotencyKey::from_bytes(derive_key16(
            STEP_B_KEY_DOMAIN,
            b"cap-obs-root",
            1,
        )),
        issued_at_ms: SEAL_T0_MS + 12_300,
    };
    let obs_root_sig = z4_key
        .sign(&issue_root_command_message(obs_root_cmd))
        .to_bytes();
    let obs_root_rec = runtime
        .capability()
        .issue_root_signed(
            &runtime.identity,
            SignedIssueRootCapabilityRequest {
                command: obs_root_cmd,
                signer: z4_binding.principal_id,
                signature: obs_root_sig,
            },
        )
        .map_err(|err| format!("issue_root_signed(observatory): {err}"))?
        .record();
    let obs_root_handle = obs_root_rec.handle;

    let obs_event =
        SemanticEventId::from_bytes(derive_digest32(STEP_B_KEY_DOMAIN, b"obs-sem-ev", 1));
    let obs_sem_msg = nlos_identity::semantic_signature_message(obs_event);
    let obs_verified = runtime
        .identity
        .verify_semantic_signature(VerifySemanticSignatureRequest {
            event_id: obs_event,
            issuer: obs_holder_binding.principal_id,
            control_domain_id: obs_holder_binding.control_domain_id,
            key_id: obs_holder_binding.key_id,
            signature: obs_holder_key.sign(&obs_sem_msg).to_bytes(),
            admitted_at_ms: SEAL_T0_MS + 12_400,
        })
        .map_err(|err| format!("verify_semantic_signature(obs): {err}"))?;
    let obs_consume = runtime
        .capability()
        .consume(ConsumeCapabilityRequest {
            handle: obs_root_handle,
            signer: obs_verified,
            target: CapabilityTarget::Namespace(target_namespace),
            required_right: CapabilityRights::SEMANTIC_RETRACT,
            purpose_digest: Some(obs_purpose_digest),
            idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                STEP_B_KEY_DOMAIN,
                b"cap-obs-consume-n1",
                1,
            )),
            consumed_at_ms: SEAL_T0_MS + 12_400,
        })
        .map_err(|err| format!("consume(obs_root_handle): {err}"))?
        .receipt();
    if obs_consume.remaining != Some(7) {
        return Err(format!(
            "expected remaining == Some(7) on obs_root_handle, got {:?}",
            obs_consume.remaining
        ));
    }

    Ok((
        z4_binding,
        survey_holder_binding,
        survey_worker_binding,
        obs_holder_binding,
        target_namespace,
        survey_purpose_digest,
        survey_root_handle,
        survey_del_handle,
        worker_consume.remaining.unwrap_or(0),
        obs_root_handle,
        obs_consume.remaining.unwrap_or(0),
    ))
}

fn exercise_mcp_bridge_on_n1(effect_declarations: &[EffectDescriptor]) -> Result<usize, String> {
    // Negative check (`[DRV-NEGO-001]`): unsupported MCP protocol version fails closed.
    let mut bad_server = ScriptedMcpServer::new("1999-01-01");
    let mut bad_adapter = McpBridgeAdapter::new(&mut bad_server);
    if !matches!(
        bad_adapter.initialize(),
        Err(McpBridgeError::Negotiation(_))
    ) {
        return Err("expected McpBridgeError::Negotiation for unsupported MCP version".to_owned());
    }
    if bad_adapter.state() != BridgeState::Closed {
        return Err("expected BridgeState::Closed after negotiation rejection".to_owned());
    }

    // Positive handshake + tool discovery + 2 tool executions + same-key replay.
    let mut server = ScriptedMcpServer::new("2025-03-26");
    let mut adapter = McpBridgeAdapter::new(&mut server);
    adapter
        .initialize()
        .map_err(|err| format!("McpBridgeAdapter::initialize: {err}"))?;
    let discovered = adapter
        .discover_tools(effect_declarations)
        .map_err(|err| format!("McpBridgeAdapter::discover_tools: {err}"))?;
    if discovered.len() != 4 {
        return Err(format!(
            "expected 4 discovered MCP tools, got {}",
            discovered.len()
        ));
    }

    let op1 = derive_key16(STEP_B_KEY_DOMAIN, b"mcp-op", 1);
    let key1 = derive_key16(STEP_B_KEY_DOMAIN, b"mcp-key", 1);
    let args1 = JsonValue::object([("query", JsonValue::String("sabi-v1-compat".to_owned()))]);
    let out1 = adapter
        .call_tool("search-knowledge-base", &args1, &op1, &key1)
        .map_err(|err| format!("call_tool(search-knowledge-base): {err}"))?;
    if !matches!(out1, CallOutcome::Executed { .. }) {
        return Err(format!("expected CallOutcome::Executed, got {out1:?}"));
    }
    let replay1 = adapter
        .call_tool("search-knowledge-base", &args1, &op1, &key1)
        .map_err(|err| format!("replay call_tool(search-knowledge-base): {err}"))?;
    if !matches!(replay1, CallOutcome::Replayed(_)) {
        return Err(format!(
            "expected CallOutcome::Replayed on same key, got {replay1:?}"
        ));
    }

    let op2 = derive_key16(STEP_B_KEY_DOMAIN, b"mcp-op", 2);
    let key2 = derive_key16(STEP_B_KEY_DOMAIN, b"mcp-key", 2);
    let args2 = JsonValue::object([("note", JsonValue::String("ridge-waypoint-01".to_owned()))]);
    let out2 = adapter
        .call_tool("append-field-note", &args2, &op2, &key2)
        .map_err(|err| format!("call_tool(append-field-note): {err}"))?;
    if !matches!(out2, CallOutcome::Executed { .. }) {
        return Err(format!("expected CallOutcome::Executed, got {out2:?}"));
    }
    drop(adapter);

    if server.executed_tools().len() != 2 {
        return Err(format!(
            "expected exactly 2 server-side MCP tool executions (replay must not re-dispatch), got {}",
            server.executed_tools().len()
        ));
    }
    Ok(server.executed_tools().len())
}

fn exercise_openai_compat_gateway_on_n1(
    gateway: &MeteredChatGateway,
) -> Result<([u8; 32], String, LlmTokenBudgetRecord), String> {
    let chat_req = ChatCompletionRequest {
        model: "glm-4.6".to_owned(),
        messages: vec![
            ChatMessage {
                role: ChatRole::System,
                content: "Summarize observatory & survey notes deterministically.".to_owned(),
            },
            ChatMessage {
                role: ChatRole::User,
                content: "Checkpoint N-1 baseline.".to_owned(),
            },
        ],
        max_tokens: Some(256),
        temperature: Some(0.0),
        stream: true,
        stream_options_include_usage: true,
    };
    let req_json = chat_req
        .to_json()
        .map_err(|err| format!("ChatCompletionRequest::to_json: {err}"))?
        .to_json_string();
    if !req_json.contains("\"model\":\"glm-4.6\"") || !req_json.contains("\"include_usage\":true") {
        return Err(format!(
            "unexpected ChatCompletionRequest JSON wire: {req_json}"
        ));
    }

    let sse_stream = format!(
        "data: {{\"id\":\"chatcmpl-n1\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"N-1 \"}},\"finish_reason\":null}}]}}\n\n\
         data: {{\"id\":\"chatcmpl-n1\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"baseline sealed\"}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: {DONE_SENTINEL}\n\n"
    );
    let mut parser = SseParser::new();
    let mut aggregator = StreamAggregator::new();
    let events = parser
        .push(sse_stream.as_bytes())
        .map_err(|err| format!("SseParser::push: {err}"))?;
    for event in &events {
        aggregator
            .fold_event(event)
            .map_err(|err| format!("StreamAggregator::fold_event: {err}"))?;
    }
    if !aggregator.done || aggregator.text != "N-1 baseline sealed" {
        return Err(format!(
            "unexpected SSE aggregation state: done={}, content={:?}",
            aggregator.done, aggregator.text
        ));
    }
    let aggregated_text = aggregator.text;

    let req_digest_1 = request_digest(&chat_req.model, chat_req.messages.len(), chat_req.stream);
    let receipt_1 = gateway
        .charge_completion(&req_digest_1, 600, 300, 100, SEAL_T0_MS + 13_000)
        .map_err(|err| format!("charge_completion #1: {err}"))?;
    // Idempotent replay under the exact same `request_digest` must not double-charge.
    let replay_1 = gateway
        .charge_completion(&req_digest_1, 600, 300, 100, SEAL_T0_MS + 13_000)
        .map_err(|err| format!("charge_completion #1 replay: {err}"))?;
    if replay_1.receipt_id != receipt_1.receipt_id {
        return Err(
            "same-digest MeteredChatGateway replay returned different receipt_id".to_owned(),
        );
    }

    let req_digest_2 = request_digest("glm-4.6", 3, false);
    let _receipt_2 = gateway
        .charge_completion(&req_digest_2, 800, 400, 200, SEAL_T0_MS + 13_100)
        .map_err(|err| format!("charge_completion #2: {err}"))?;

    let budget = gateway
        .budget()
        .map_err(|err| format!("gateway.budget(): {err}"))?;
    if budget.consumed_prompt_tokens != 1_400
        || budget.consumed_completion_tokens != 700
        || budget.consumed_total_tokens != 2_400
        || budget.consumed_api_calls != 2
    {
        return Err(format!(
            "unexpected 5-D LlmTokenBudget after N-1: {budget:?}"
        ));
    }

    Ok((req_digest_1, aggregated_text, budget))
}

#[allow(clippy::too_many_lines)]
fn execute_app_three_node_dag(
    runtime: &SliceKRuntime,
    app_rec: &InstalledN1SampleRecord,
    node_specs: [(&str, &str); 3],
    phase_prefix: &[u8],
) -> Result<(ArtifactId, ContentDigest), String> {
    let plan_id = app_rec
        .plan_id
        .ok_or_else(|| format!("{} missing TaskPlanId", app_rec.label))?;
    let (node_0, rev_0) = resolve_plan_node_by_hex_key(runtime.plans(), plan_id, node_specs[0].1)?;
    let (node_1, rev_1) = resolve_plan_node_by_hex_key(runtime.plans(), plan_id, node_specs[1].1)?;
    let (node_2, rev_2) = resolve_plan_node_by_hex_key(runtime.plans(), plan_id, node_specs[2].1)?;

    // Premature materialization of node 1 before node 0 completes must fail closed.
    let premature_at = runtime
        .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
            STEP_B_KEY_DOMAIN,
            phase_prefix,
            90,
        )))
        .map_err(|err| format!("wall_now_ms: {err}"))?;
    let premature = runtime
        .plans()
        .request_materialization(MaterializationRequest {
            plan_id,
            node_id: node_1,
            idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                STEP_B_KEY_DOMAIN,
                phase_prefix,
                91,
            )),
            requested_at_ms: premature_at,
        });
    if !matches!(premature, Err(PlanStoreError::DependenciesNotReady { .. })) {
        return Err(format!(
            "expected DependenciesNotReady for {} node {}, got {premature:?}",
            app_rec.label, node_specs[1].0
        ));
    }

    let output_artifact_id =
        ArtifactId::from_bytes(derive_key16(STEP_B_KEY_DOMAIN, phase_prefix, 100));
    let mut upstream_digest = app_rec.verification_v1_0_0.manifest_digest;

    for (idx, ((step_name, _hex), (node_id, decl_rev))) in node_specs
        .iter()
        .zip([(node_0, rev_0), (node_1, rev_1), (node_2, rev_2)])
        .enumerate()
    {
        let step_tag = format!("{}-{step_name}", String::from_utf8_lossy(phase_prefix));
        materialize_and_activate_plan_node(
            runtime,
            plan_id,
            node_id,
            decl_rev,
            step_tag.as_bytes(),
        )?;

        let rev_num = u64::try_from(idx + 1).unwrap_or(1);
        let write_at = runtime
            .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
                STEP_B_KEY_DOMAIN,
                step_tag.as_bytes(),
                20,
            )))
            .map_err(|err| format!("wall_now_ms: {err}"))?;
        if rev_num == 1 {
            runtime
                .artifacts
                .create_artifact(CreateArtifactSpec {
                    artifact_id: output_artifact_id,
                    idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                        STEP_B_KEY_DOMAIN,
                        step_tag.as_bytes(),
                        21,
                    )),
                    content_type: "text/markdown".to_owned(),
                    application_id: Some(app_rec.runtime_application_id),
                    owner: None,
                    created_at_ms: write_at,
                })
                .map_err(|err| format!("create output artifact for {}: {err}", app_rec.label))?;
        }
        let body = format!(
            "app={}\nepoch={HOST_EPOCH_N_MINUS_1}\nrevision={rev_num}\nstep={step_name}\nupstream={}\n",
            app_rec.label,
            hex_encode(upstream_digest.as_bytes()),
        );
        runtime
            .artifacts
            .put_revision(PutRevisionRequest {
                artifact_id: output_artifact_id,
                expected_head_revision: rev_num - 1,
                bytes: body.as_bytes(),
                created_at_ms: write_at,
                provenance: ProvenanceSourceTriple {
                    source_a: *node_id.as_bytes(),
                    source_b: *app_rec.package_id.as_bytes(),
                    source_digest: upstream_digest,
                },
            })
            .map_err(|err| format!("put_revision {rev_num} for {}: {err}", app_rec.label))?;

        complete_active_plan_node(runtime, plan_id, node_id, decl_rev, step_tag.as_bytes())?;
        upstream_digest = ContentDigest::of_bytes(body.as_bytes());
    }

    Ok((output_artifact_id, upstream_digest))
}

fn create_shared_cross_app_corpus_artifact(
    runtime: &SliceKRuntime,
) -> Result<(ArtifactId, ContentDigest), String> {
    let shared_id = ArtifactId::from_bytes(derive_key16(
        STEP_B_KEY_DOMAIN,
        b"shared-cross-app-corpus",
        1,
    ));
    let created_at = runtime
        .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
            STEP_B_KEY_DOMAIN,
            b"shared-cross-app-corpus",
            2,
        )))
        .map_err(|err| format!("wall_now_ms: {err}"))?;
    runtime
        .artifacts
        .create_artifact(CreateArtifactSpec {
            artifact_id: shared_id,
            idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                STEP_B_KEY_DOMAIN,
                b"shared-cross-app-corpus",
                3,
            )),
            content_type: "text/plain".to_owned(),
            application_id: None,
            owner: None,
            created_at_ms: created_at,
        })
        .map_err(|err| format!("create shared corpus artifact: {err}"))?;
    let payload = b"shared-ecosystem-corpus:observatory+field-survey:v1";
    runtime
        .artifacts
        .put_revision(PutRevisionRequest {
            artifact_id: shared_id,
            expected_head_revision: 0,
            bytes: payload,
            created_at_ms: created_at,
            provenance: ProvenanceSourceTriple {
                source_a: [0xA1; 16],
                source_b: [0xB2; 16],
                source_digest: ContentDigest::of_bytes(payload),
            },
        })
        .map_err(|err| format!("put shared corpus revision: {err}"))?;
    Ok((shared_id, ContentDigest::of_bytes(payload)))
}

fn register_active_background_task_on_n1(
    runtime: &SliceKRuntime,
    survey_rec: &InstalledN1SampleRecord,
) -> Result<TaskId, String> {
    let now_i64 = runtime
        .wall_now_i64(IdempotencyKey::from_bytes(derive_key16(
            STEP_B_KEY_DOMAIN,
            b"survey-bg-task",
            1,
        )))
        .map_err(|err| format!("wall_now_i64: {err}"))?;
    let bg_task_id =
        nlos_types::TaskId::from_bytes(derive_key16(STEP_B_KEY_DOMAIN, b"survey-bg-task-id", 1));
    let task_record = runtime
        .tasks
        .register_task(TaskSpec {
            task_id: bg_task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: now_i64,
            application_id: Some(survey_rec.runtime_application_id),
            plan_revision: None,
        })
        .map_err(|err| format!("tasks.register_task: {err}"))?;

    let reg_at_ms = runtime
        .wall_now_ms(IdempotencyKey::from_bytes(derive_key16(
            STEP_B_KEY_DOMAIN,
            b"survey-bg-task",
            3,
        )))
        .map_err(|err| format!("wall_now_ms: {err}"))?;
    runtime
        .applications
        .register_background_task(RegisterBackgroundTaskRequest {
            package_id: survey_rec.package_id,
            task_id: task_record.task_id(),
            registrant_principal: survey_rec.signer_principal,
            idempotency_key: IdempotencyKey::from_bytes(derive_key16(
                STEP_B_KEY_DOMAIN,
                b"survey-bg-task",
                4,
            )),
            registered_at_ms: reg_at_ms,
        })
        .map_err(|err| format!("applications.register_background_task: {err}"))?;

    Ok(task_record.task_id())
}
