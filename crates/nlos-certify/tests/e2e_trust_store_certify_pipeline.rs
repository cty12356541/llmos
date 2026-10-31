//! Stage E `W79-INT` (`M-E3`) cross-crate end-to-end integration test spanning:
//! - `nlos-publisher-trust` (`SqlitePublisherTrustAuthority`, 3-tier PKI under
//!   `SigningKeyCustodyVault`, `SignedRevocationList`, `ApplicationId` anti-hijack),
//! - `nlos-certify` (4-gate automated conformance pipeline & custody-signed
//!   `StoreCertificationReceipt`),
//! - `nlos-store-registry` (`LocalOciCasStore`, `SqliteStoreRegistry` multi-component
//!   onboarding, `0b1111` `Candidate -> Stable` promotion, offline
//!   `SignedEntitlementToken` redemption `[NLOS-DEFER-001]`, and Level 3
//!   `RecallSeverity::QuarantineInstallation` cross-authority cascade),
//! - `nlos-plan::ecosystem_selector` (`[PLAN-DEPENDENCY-001]` 7-class pinned
//!   selectors, anti-floating `"latest"` & `UnpinnedSearchResult` fail-closed
//!   gates, `[PLAN-LAZY-001]` G3 `WaitingAuthorization -> Materializing`
//!   conditioned admission, and post-recall `WaitingAuthorization` quarantine
//!   hold), and
//! - core kernel/platform authorities (`IdentityAuthority`, `ArtifactStore`,
//!   `ApplicationAuthority`, `CapabilityAuthority`, `SqliteTaskAuthority`,
//!   `SqlitePlanAuthority`).

#![forbid(unsafe_code)]
#![allow(clippy::too_many_lines)]

use std::collections::BTreeMap;
use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_application::{
    ApplicationAuthority, ApplicationStatus, InstallApplicationRequest, derive_application_id,
    pack_package_version,
};
use nlos_artifact::{
    ArtifactStore, ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageManifest,
    PackageManifestEntry, ProvenanceSourceTriple, PutRevisionDecision, PutRevisionRequest,
    SignedPackage, VerifyPackageRequest, package_manifest_message,
};
use nlos_capability::custody::SqliteCustodyStore;
use nlos_capability::{
    CapabilityAuthority, CapabilityAuthorityError, CapabilityHandle, CapabilityRights,
    CapabilityTarget, ConsumeCapabilityRequest, CustodySignRequest, CustodyTier,
    IssueRootCapabilityRequest, ProvisionCustodyKeyRequest, SignedIssueRootCapabilityRequest,
    SigningKeyCustodyVault, issue_root_command_message,
};
use nlos_certify::{
    CertifyPipelineConfig, GateId, GateStatus, LoadedCertifyPackage, PublisherSubmissionProof,
    StoreCertifierSigner, SurfaceProfile, derive_publisher_id_from_str, run_certification_pipeline,
    verify_store_certification_receipt,
};
use nlos_identity::{
    BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose,
    VerifySemanticSignatureRequest, semantic_signature_message,
};
use nlos_plan::ecosystem_selector::{
    AuthorizationCheckVerdict, AuthorizeNodeMaterializationRequest, BindNodeDependenciesRequest,
    ConditionedGateDenialReason, ConditionedGateOutcome, ConditionedMaterializationDecision,
    CrossEcosystemEntitySource, CrossEcosystemLookup, EcosystemEntitySnapshot, EcosystemGateError,
    FanoutGateVerdict, NamespaceGateVerdict, NodeExecutionGateConsult,
    PLAN_DEPENDENCY_ENTITY_KIND_COUNT, PinnedDependencyDecision, PinnedEcosystemDependencyHandle,
    PlanDependencyEntityKind, ResolveNodeDependencyRequest, ResourceContractVerdict,
    SelectorProvenance, SelectorVersionPin, SqliteEcosystemDependencyGate, TypedDependencySelector,
    TypedDependencySelectorProposal, parse_version_pin,
};
use nlos_plan::{
    AdmissionConsult, AdmissionConsultOutcome, ApplyPlanRevisionRequest, FanoutCondition,
    MaterializationAdmission, MaterializationRejection, NamespaceCondition, NodeConditions,
    NodeTransitionRequest, PlanNodeDeclaration, PlanNodeKind, PlanNodeState,
    ResourceContractCondition, SqlitePlanAuthority,
};
use nlos_publisher_trust::{
    CrlImportDecision, IntermediateCertificateIssueSpec, LeafCertificateIssueSpec,
    OfflinePlatformRootCa, PublisherId, PublisherTrustCheckError, PublisherTrustTier,
    PublisherTrustVerifier, RevocationEpoch, RevocationListSpec, SignedRevocationList,
    SqlitePublisherTrustAuthority,
};
use nlos_store_registry::{
    CERTIFICATION_ATTESTATION_SIGNING_DOMAIN, DeclaredPermissionEntry,
    DirectEd25519RevocationAuthorizer, EffectClass, EntitlementRedeemDecision,
    LocalLaunchDisposition, LocalOciCasStore, NLOS_OCI_CONFIG_MEDIA_TYPE, OciLayerDescriptor,
    OciPackageLayout, REQUIRED_CERTIFICATION_GATES_MASK, RecallApplyDecision,
    RecallEnforcementAuthorities, RecallSeverity, RedeemEntitlementRequest,
    RegisterGrantedCapabilityRequest, SignedCertificationAttestation, SignedEntitlementToken,
    SignedRecallAdvisory, SqliteStoreRegistry, StoreCatalogTrack, StoreChannel, StoreComponentKind,
    StoreComponentSlot, StoreGovernanceAnchors, StorePromotionDecision, StoreRegistryError,
    StoreReleaseSubmission, UnsignedStoreReleaseSubmission, compute_entitlement_token_digest,
    compute_recall_advisory_digest, compute_release_submission_digest,
};
use nlos_task::{
    MaterializationAdmissionFacts, SqliteTaskAuthority, TASK_PROFILE_10K, TaskStoreError,
};
use nlos_types::{
    ApplicationId, ArtifactId, ControlDomainId, Generation, IdempotencyKey, KeyId, NamespaceId,
    PackageId, PrincipalId, SemanticEventId, TaskNodeId, TaskPlanId,
};

static NEXT_E2E_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempE2eWorkspace(PathBuf);

impl TempE2eWorkspace {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after UNIX_EPOCH")
            .as_nanos();
        let seq = NEXT_E2E_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "nlos-w79-e2e-{label}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create E2E temp workspace");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempE2eWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn bootstrap_principal(
    identity: &IdentityAuthority,
    seed: u8,
    valid_from_ms: u64,
    valid_until_ms: u64,
) -> (SigningKey, IdentityBinding, [u8; 32]) {
    let raw_seed = [seed; 32];
    let signing_key = SigningKey::from_bytes(&raw_seed);
    let binding = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: signing_key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: valid_from_ms,
            key_valid_until_ms: valid_until_ms,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
            created_at_ms: valid_from_ms,
        })
        .expect("bootstrap principal")
        .binding();
    (signing_key, binding, raw_seed)
}

fn ingest_composite_oci_layout(
    cas: &LocalOciCasStore,
    package_dir: &Path,
) -> (OciPackageLayout, Vec<StoreComponentSlot>) {
    let manifest_bytes =
        fs::read(package_dir.join("package.manifest")).expect("read package.manifest");
    let worker_bytes = fs::read(package_dir.join("assistant.bin")).expect("read assistant.bin");
    let seed_bytes = fs::read(package_dir.join("assets/assistant-profile.txt"))
        .expect("read assets/assistant-profile.txt");
    let sync_bytes = fs::read(package_dir.join("tasks/draft-plan-binding.txt"))
        .expect("read tasks/draft-plan-binding.txt");
    let driver_bytes = b"nlos-starter-driver-shim-v1".to_vec();
    let ui_surface_bytes = b"nlos-starter-declarative-duis-v1".to_vec();

    let config_desc =
        OciLayerDescriptor::from_layer_bytes(NLOS_OCI_CONFIG_MEDIA_TYPE, &manifest_bytes)
            .expect("valid OCI config descriptor");

    let raw_components: [(&str, StoreComponentKind, &str, &str, Vec<u8>); 5] = [
        (
            "assistant-driver-exec",
            StoreComponentKind::Executable,
            "assistant.bin",
            "application/vnd.nlos.component.executable.v1+bin",
            worker_bytes,
        ),
        (
            "assistant-driver",
            StoreComponentKind::Driver,
            "drivers/assistant-bridge.drv",
            "application/vnd.nlos.component.driver.v1+bin",
            driver_bytes,
        ),
        (
            "assistant-surface",
            StoreComponentKind::UiSurface,
            "surfaces/assistant-card.duis",
            "application/vnd.nlos.component.ui-surface.v1+cbor",
            ui_surface_bytes,
        ),
        (
            "assistant-sync",
            StoreComponentKind::BackgroundService,
            "tasks/draft-plan-binding.txt",
            "application/vnd.nlos.component.service.v1+bin",
            sync_bytes,
        ),
        (
            "assistant-profile-seed",
            StoreComponentKind::ReadOnlySeedTemplate,
            "assets/assistant-profile.txt",
            "application/vnd.nlos.component.seed.v1+text",
            seed_bytes,
        ),
    ];

    let mut owned_layers = Vec::with_capacity(5);
    let mut slots = Vec::with_capacity(5);
    for (slot_name, kind, entrypoint, media_type, payload) in raw_components {
        let desc =
            OciLayerDescriptor::from_layer_bytes(media_type, &payload).expect("valid OCI layer");
        slots.push(StoreComponentSlot {
            slot_name: slot_name.to_owned(),
            kind,
            entrypoint: entrypoint.to_owned(),
            content_sha256: desc.sha256_digest,
            size_bytes: desc.size_bytes,
        });
        owned_layers.push((desc, payload));
    }

    let borrowed_layers: Vec<(OciLayerDescriptor, &[u8])> = owned_layers
        .iter()
        .map(|(desc, bytes)| (desc.clone(), bytes.as_slice()))
        .collect();
    let layout = cas
        .write_package_layout(&config_desc, &manifest_bytes, &borrowed_layers)
        .expect("write 5-component OCI package layout");
    (layout, slots)
}

#[allow(clippy::too_many_arguments)]
fn build_signed_store_submission(
    package_id: PackageId,
    package_version: u64,
    manifest_digest: [u8; 32],
    oci_manifest_digest: [u8; 32],
    application_ids: Vec<ApplicationId>,
    components: Vec<StoreComponentSlot>,
    declared_permissions: Vec<DeclaredPermissionEntry>,
    publisher_id: PublisherId,
    publisher_key: &SigningKey,
    submitted_at_ms: u64,
) -> StoreReleaseSubmission {
    let digest = compute_release_submission_digest(&UnsignedStoreReleaseSubmission {
        package_id,
        package_version,
        manifest_digest,
        oci_manifest_digest,
        application_ids: &application_ids,
        components: &components,
        declared_permissions: &declared_permissions,
        publisher_id,
    });
    let signature = publisher_key.sign(&digest).to_bytes();
    StoreReleaseSubmission {
        package_id,
        package_version,
        manifest_digest,
        oci_manifest_digest,
        application_ids,
        components,
        declared_permissions,
        publisher_id,
        publisher_signing_key: publisher_key.verifying_key().to_bytes(),
        publisher_signature: signature,
        submitted_at_ms,
    }
}

/// Live `CrossEcosystemEntitySource` backed directly by `SqliteStoreRegistry`
/// for `PlanDependencyEntityKind::Package` (with optional simulated offline
/// cache mode to also exercise G3 capability/quarantine fencing independently)
/// and deterministic pinned entries for the remaining six ecosystem entity
/// classes (`Driver`, `UiSurface`, `Model`, `Runtime`, `Skill`, `PolicyBundle`).
struct LiveStoreEcosystemSource<'a> {
    store_registry: &'a SqliteStoreRegistry,
    tracked_package_id: PackageId,
    tracked_package_version: u64,
    tracked_version_tag: String,
    use_stale_offline_package_cache: bool,
    stale_package_digest: [u8; 32],
    companion_entities: BTreeMap<(PlanDependencyEntityKind, [u8; 16]), EcosystemEntitySnapshot>,
}

impl<'a> LiveStoreEcosystemSource<'a> {
    fn new(
        store_registry: &'a SqliteStoreRegistry,
        tracked_package_id: PackageId,
        tracked_package_version: u64,
        tracked_version_tag: &str,
        oci_manifest_digest: [u8; 32],
    ) -> Self {
        let mut companion_entities = BTreeMap::new();
        for kind in PlanDependencyEntityKind::ALL {
            if kind == PlanDependencyEntityKind::Package {
                continue;
            }
            let code = kind.encode();
            companion_entities.insert(
                (kind, [code; 16]),
                EcosystemEntitySnapshot {
                    pinned_version_tag: format!("v1.{code}.0"),
                    pinned_generation: u64::from(code),
                    content_digest: [code.wrapping_add(0x50); 32],
                },
            );
        }
        Self {
            store_registry,
            tracked_package_id,
            tracked_package_version,
            tracked_version_tag: tracked_version_tag.to_owned(),
            use_stale_offline_package_cache: false,
            stale_package_digest: oci_manifest_digest,
            companion_entities,
        }
    }

    fn lookup_entity(
        &self,
        kind: PlanDependencyEntityKind,
        entity_id: [u8; 16],
    ) -> Result<CrossEcosystemLookup, StoreRegistryError> {
        if kind == PlanDependencyEntityKind::Package {
            if entity_id != *self.tracked_package_id.as_bytes() {
                return Ok(CrossEcosystemLookup::NotFound);
            }
            if self.use_stale_offline_package_cache {
                return Ok(CrossEcosystemLookup::Found(EcosystemEntitySnapshot {
                    pinned_version_tag: self.tracked_version_tag.clone(),
                    pinned_generation: self.tracked_package_version,
                    content_digest: self.stale_package_digest,
                }));
            }
            let Some(record) = self
                .store_registry
                .inspect_release(self.tracked_package_id, self.tracked_package_version)?
            else {
                return Ok(CrossEcosystemLookup::NotFound);
            };
            return match record.channel {
                StoreChannel::Stable => Ok(CrossEcosystemLookup::Found(EcosystemEntitySnapshot {
                    pinned_version_tag: self.tracked_version_tag.clone(),
                    pinned_generation: record.package_version,
                    content_digest: record.oci_manifest_digest,
                })),
                StoreChannel::Recalled => Ok(CrossEcosystemLookup::RecalledOrRevoked {
                    reason: "SqliteStoreRegistry release channel is Recalled".to_owned(),
                }),
                StoreChannel::Candidate => Ok(CrossEcosystemLookup::NotFound),
            };
        }
        Ok(self
            .companion_entities
            .get(&(kind, entity_id))
            .cloned()
            .map_or(CrossEcosystemLookup::NotFound, CrossEcosystemLookup::Found))
    }
}

impl CrossEcosystemEntitySource for LiveStoreEcosystemSource<'_> {
    type Error = StoreRegistryError;

    fn supports_kind(&self, _kind: PlanDependencyEntityKind) -> bool {
        true
    }

    fn resolve_pinned(
        &self,
        selector: &TypedDependencySelector,
    ) -> Result<CrossEcosystemLookup, Self::Error> {
        self.lookup_entity(selector.kind(), selector.entity_id())
    }

    fn verify_handle_live(
        &self,
        handle: &PinnedEcosystemDependencyHandle,
    ) -> Result<CrossEcosystemLookup, Self::Error> {
        self.lookup_entity(handle.kind, handle.entity_id)
    }
}

struct TaskAuthorityAdmission<'a>(&'a SqliteTaskAuthority);

impl AdmissionConsult for TaskAuthorityAdmission<'_> {
    type Error = TaskStoreError;

    fn consult_materialization(
        &self,
        other_declared_task_nodes: u64,
    ) -> Result<AdmissionConsultOutcome, TaskStoreError> {
        match self
            .0
            .answer_plan_materialization(other_declared_task_nodes)
        {
            Ok(MaterializationAdmissionFacts {
                profile_id,
                projected_task_nodes,
                projected_active_working_set,
            }) => Ok(AdmissionConsultOutcome::Admitted(
                MaterializationAdmission {
                    profile_id: profile_id.to_owned(),
                    projected_task_nodes,
                    projected_active_working_set,
                },
            )),
            Err(TaskStoreError::WorkingSetAdmissionDenied {
                profile_id,
                active_count,
                max_active_working_set,
            }) => Ok(AdmissionConsultOutcome::Denied(
                MaterializationRejection::WorkingSetFull {
                    profile_id: profile_id.to_owned(),
                    active_count,
                    max_active_working_set,
                },
            )),
            Err(TaskStoreError::TaskNodeAdmissionDenied {
                profile_id,
                task_count,
                max_task_nodes,
            }) => Ok(AdmissionConsultOutcome::Denied(
                MaterializationRejection::TaskNodeCapExceeded {
                    profile_id: profile_id.to_owned(),
                    task_count,
                    max_task_nodes,
                },
            )),
            Err(other) => Err(other),
        }
    }
}

/// Real `CapabilityAuthority` + `ApplicationAuthority` G3 execution gate consult.
struct RealRuntimeExecutionGate<'a> {
    capabilities: &'a CapabilityAuthority,
    applications: &'a ApplicationAuthority,
    package_id: PackageId,
    capability_handle: CapabilityHandle,
    allowed_namespaces: Vec<NamespaceId>,
    resource_ceiling: ResourceContractCondition,
    payer_principal: PrincipalId,
    payer_max_fanout_budget: u64,
}

impl NodeExecutionGateConsult for RealRuntimeExecutionGate<'_> {
    type Error = String;

    fn check_authorization(
        &self,
        _plan_id: TaskPlanId,
        _node_id: TaskNodeId,
        pinned_handles: &[PinnedEcosystemDependencyHandle],
        at_ms: u64,
    ) -> Result<AuthorizationCheckVerdict, String> {
        let app_record = self
            .applications
            .inspect_application(self.package_id)
            .map_err(|e| format!("ApplicationAuthority inspect failed: {e}"))?;
        let Some(app) = app_record else {
            return Ok(AuthorizationCheckVerdict::Denied {
                reason: "target package is not installed in ApplicationAuthority".to_owned(),
            });
        };
        if app.status != ApplicationStatus::Installed {
            return Ok(AuthorizationCheckVerdict::Denied {
                reason: format!(
                    "ApplicationAuthority installation quarantined or inactive: {:?}",
                    app.status
                ),
            });
        }
        match self
            .capabilities
            .inspect_active(self.capability_handle, at_ms)
        {
            Ok(record) => {
                if !record.rights.contains(CapabilityRights::SEMANTIC_APPEND) {
                    return Ok(AuthorizationCheckVerdict::Denied {
                        reason: "capability lacks SEMANTIC_APPEND execution right".to_owned(),
                    });
                }
                if pinned_handles.is_empty() {
                    return Ok(AuthorizationCheckVerdict::Denied {
                        reason: "missing pinned ecosystem dependency handles".to_owned(),
                    });
                }
                Ok(AuthorizationCheckVerdict::Authorized {
                    capability_or_grant_id: *record.handle.capability_id.as_bytes(),
                })
            }
            Err(err) => Ok(AuthorizationCheckVerdict::Denied {
                reason: format!("CapabilityAuthority rejected execution handle: {err}"),
            }),
        }
    }

    fn check_namespace_scope(
        &self,
        _plan_id: TaskPlanId,
        _node_id: TaskNodeId,
        namespace: &NamespaceCondition,
        _at_ms: u64,
    ) -> Result<NamespaceGateVerdict, String> {
        for ns in &namespace.namespaces {
            if !self.allowed_namespaces.contains(ns) {
                return Ok(NamespaceGateVerdict::Denied {
                    denied_namespace: *ns,
                    reason: format!("namespace {ns:?} outside capability scope"),
                });
            }
        }
        Ok(NamespaceGateVerdict::Permitted)
    }

    fn check_resource_contract(
        &self,
        _plan_id: TaskPlanId,
        _node_id: TaskNodeId,
        resource_contract: &ResourceContractCondition,
        _at_ms: u64,
    ) -> Result<ResourceContractVerdict, String> {
        if resource_contract.cpu_shares > self.resource_ceiling.cpu_shares
            || resource_contract.memory_mib > self.resource_ceiling.memory_mib
            || resource_contract.io_weight > self.resource_ceiling.io_weight
        {
            return Ok(ResourceContractVerdict::Exceeded {
                reason: "node resource contract exceeds runtime quota ceiling".to_owned(),
            });
        }
        Ok(ResourceContractVerdict::WithinQuota)
    }

    fn check_fanout_and_payer(
        &self,
        _plan_id: TaskPlanId,
        _node_id: TaskNodeId,
        _fanout: &FanoutCondition,
        _dag_downstream_fanout: u64,
        effective_downstream_fanout: u64,
        _at_ms: u64,
    ) -> Result<FanoutGateVerdict, String> {
        if effective_downstream_fanout > self.payer_max_fanout_budget {
            return Ok(FanoutGateVerdict::Denied {
                reason: "effective downstream fanout exceeds payer budget".to_owned(),
            });
        }
        Ok(FanoutGateVerdict::Approved {
            payer_principal_id: self.payer_principal,
        })
    }
}

fn conditioned_plan_node(key: u8, ns: NamespaceId) -> PlanNodeDeclaration {
    PlanNodeDeclaration {
        node_key: [key; 16],
        kind: PlanNodeKind::AgentRole,
        binding_digest: [key; 32],
        dependency_keys: Vec::new(),
        input_selectors_digest: [key.wrapping_add(1); 32],
        output_contract_digest: [key.wrapping_add(2); 32],
        policy_digest: [key.wrapping_add(3); 32],
        resource_ceiling_digest: [key.wrapping_add(4); 32],
        conditions: Some(NodeConditions {
            namespace: NamespaceCondition {
                namespaces: vec![ns],
            },
            resource_contract: ResourceContractCondition {
                cpu_shares: 1_000,
                memory_mib: 512,
                io_weight: 100,
            },
            fanout: FanoutCondition {
                max_downstream_fanout: 4,
            },
        }),
    }
}

fn seven_class_proposals_for_package(
    package_id: PackageId,
    oci_manifest_digest: [u8; 32],
) -> Vec<TypedDependencySelectorProposal> {
    PlanDependencyEntityKind::ALL
        .into_iter()
        .map(|kind| {
            if kind == PlanDependencyEntityKind::Package {
                TypedDependencySelectorProposal {
                    kind,
                    entity_id: *package_id.as_bytes(),
                    version_or_tag: parse_version_pin("v1.0.0").expect("exact v1.0.0 tag"),
                    expected_content_digest: Some(oci_manifest_digest),
                    provenance: SelectorProvenance::DeclaredPinnedManifest,
                }
            } else {
                let code = kind.encode();
                TypedDependencySelectorProposal {
                    kind,
                    entity_id: [code; 16],
                    version_or_tag: SelectorVersionPin::ExactGeneration(u64::from(code)),
                    expected_content_digest: Some([code.wrapping_add(0x50); 32]),
                    provenance: SelectorProvenance::TrustedUserSelection,
                }
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn bind_and_resolve_seven_classes(
    gate: &SqliteEcosystemDependencyGate,
    plan_authority: &SqlitePlanAuthority,
    source: &LiveStoreEcosystemSource<'_>,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    package_id: PackageId,
    oci_manifest_digest: [u8; 32],
    idem_seed: u8,
    at_ms: u64,
) -> Vec<PinnedEcosystemDependencyHandle> {
    let selectors = gate
        .bind_node_dependency_proposals(
            plan_authority,
            BindNodeDependenciesRequest {
                plan_id,
                node_id,
                proposals: seven_class_proposals_for_package(package_id, oci_manifest_digest),
                idempotency_key: IdempotencyKey::from_bytes([idem_seed; 16]),
                bound_at_ms: at_ms,
            },
        )
        .expect("bind 7-class proposals");
    assert_eq!(selectors.len(), PLAN_DEPENDENCY_ENTITY_KIND_COUNT);

    let mut handles = Vec::with_capacity(PLAN_DEPENDENCY_ENTITY_KIND_COUNT);
    for (idx, selector) in selectors.iter().enumerate() {
        let step_byte = idem_seed.wrapping_add(u8::try_from(idx + 1).expect("idx fits u8"));
        let decision = gate
            .resolve_node_dependency(
                plan_authority,
                source,
                ResolveNodeDependencyRequest {
                    plan_id,
                    node_id,
                    kind: selector.kind(),
                    entity_id: selector.entity_id(),
                    idempotency_key: IdempotencyKey::from_bytes([step_byte; 16]),
                    resolved_at_ms: at_ms + 10 + u64::try_from(idx).expect("idx fits u64"),
                },
            )
            .expect("resolve pinned ecosystem dependency");
        assert!(matches!(decision, PinnedDependencyDecision::Resolved(_)));
        handles.push(decision.into_handle());
    }
    handles
}

#[test]
fn e2e_publisher_pki_four_gate_certify_oci_store_entitlement_plan_selector_and_level3_recall() {
    let ws = TempE2eWorkspace::new("m-e3-pipeline");
    let t0: u64 = 1_761_868_800_000;

    // =========================================================================
    // STEP 1: Real `SqlitePublisherTrustAuthority` + `SigningKeyCustodyVault`
    //         3-tier PKI (`OfflinePlatformRootCa -> StoreIntermediateCertificate
    //         -> CertifiedPublisher` leaf) + `SignedRevocationList` @ Epoch 1.
    // =========================================================================
    let custody_store =
        SqliteCustodyStore::open(ws.path().join("custody")).expect("open SqliteCustodyStore");
    let vault =
        SigningKeyCustodyVault::with_store(custody_store, [0xA7; 32]).expect("open KEK vault");

    let root_seed = [0x11; 32];
    let root_ca = OfflinePlatformRootCa::issue_self_signed(
        [0x10; 16],
        KeyId::from_bytes([0x12; 16]),
        &root_seed,
        t0 - 10_000,
        t0 + 86_400_000,
    )
    .expect("issue offline root CA");

    let certified_ca_key_id = KeyId::from_bytes([0x21; 16]);
    let certified_ca_slot = vault
        .provision_key_slot(ProvisionCustodyKeyRequest {
            key_id: certified_ca_key_id,
            principal_id: PrincipalId::from_bytes([0x22; 16]),
            control_domain_id: ControlDomainId::from_bytes([0x23; 16]),
            authority_id: [0x24; 16],
            key_purpose: KeyPurpose::SemanticSigning,
            custody_tier: CustodyTier::PlatformKeychainSealed,
            rotation_epoch: 1,
            valid_from_ms: t0 - 10_000,
            valid_until_ms: t0 + 86_400_000,
            seed: [0x25; 32],
            provisioned_at_ms: t0 - 10_000,
        })
        .expect("provision StoreIntermediateCertificate custody key slot");

    let certified_ca = root_ca
        .issue_intermediate_certificate(
            &root_seed,
            IntermediateCertificateIssueSpec {
                serial_id: [0x26; 16],
                intermediate_key_id: certified_ca_key_id,
                intermediate_public_key: certified_ca_slot.public_key,
                max_issuable_tier: PublisherTrustTier::CertifiedPublisher,
                not_before_ms: t0 - 10_000,
                expires_at_ms: t0 + 86_400_000,
            },
        )
        .expect("issue CertifiedPublisher StoreIntermediateCertificate");

    let trust_authority = SqlitePublisherTrustAuthority::open(ws.path().join("publisher-trust.db"))
        .expect("open SqlitePublisherTrustAuthority");
    trust_authority
        .anchor_offline_root_ca(&root_ca, t0 - 5_000)
        .expect("anchor offline root CA");
    trust_authority
        .register_intermediate_certificate(&certified_ca, t0 - 5_000)
        .expect("register intermediate CA");

    let (initial_crl, _) = SignedRevocationList::issue_with_intermediate_custody(
        &certified_ca,
        &vault,
        Some(1),
        RevocationListSpec {
            revocation_epoch: RevocationEpoch::new(1),
            issued_at_ms: t0,
            fresh_until_ms: t0 + 3_600_000,
            revoked_certificates: Vec::new(),
            revoked_publishers: Vec::new(),
        },
    )
    .expect("issue initial SignedRevocationList under intermediate CA custody");
    assert_eq!(
        trust_authority
            .import_signed_revocation_list(&initial_crl)
            .expect("import initial CRL"),
        CrlImportDecision::Advanced(RevocationEpoch::new(1))
    );

    let legit_publisher = derive_publisher_id_from_str("nlos.certified.starter-assistant");
    let legit_sk = SigningKey::from_bytes(&[0x31; 32]);
    let (legit_leaf, _) = certified_ca
        .issue_leaf_certificate_under_custody(
            &vault,
            Some(1),
            t0 + 100,
            LeafCertificateIssueSpec {
                serial_id: [0x32; 16],
                publisher_id: legit_publisher,
                trust_tier: PublisherTrustTier::CertifiedPublisher,
                leaf_key_id: KeyId::from_bytes([0x33; 16]),
                leaf_public_key: legit_sk.verifying_key().to_bytes(),
                not_before_ms: t0 - 1_000,
                expires_at_ms: t0 + 86_400_000,
            },
        )
        .expect("issue legitimate CertifiedPublisher leaf certificate");
    trust_authority
        .register_leaf_certificate(&legit_leaf, t0 + 100)
        .expect("register legitimate leaf certificate");

    let malicious_publisher = derive_publisher_id_from_str("nlos.certified.impostor");
    let malicious_sk = SigningKey::from_bytes(&[0x41; 32]);
    let (malicious_leaf, _) = certified_ca
        .issue_leaf_certificate_under_custody(
            &vault,
            Some(1),
            t0 + 200,
            LeafCertificateIssueSpec {
                serial_id: [0x42; 16],
                publisher_id: malicious_publisher,
                trust_tier: PublisherTrustTier::CertifiedPublisher,
                leaf_key_id: KeyId::from_bytes([0x43; 16]),
                leaf_public_key: malicious_sk.verifying_key().to_bytes(),
                not_before_ms: t0 - 1_000,
                expires_at_ms: t0 + 86_400_000,
            },
        )
        .expect("issue second CertifiedPublisher leaf certificate");
    trust_authority
        .register_leaf_certificate(&malicious_leaf, t0 + 200)
        .expect("register second leaf certificate");

    // =========================================================================
    // STEP 2: Four-Gate Certification & `StoreCertificationReceipt` Issuance
    //         (`nlos-certify`) wired directly to `&trust_authority` +
    //         cross-publisher `ApplicationIdHijackAttempt` rejection.
    // =========================================================================
    let root = repo_root();
    let v1_dir = root.join("examples/starter-assistant-app/package");
    let v2_dir = root.join("examples/starter-assistant-app/package-v1.1.0");
    let loaded_v1 = LoadedCertifyPackage::load(&v1_dir).expect("load starter-assistant-app v1.0.0");
    let package_id = loaded_v1.package_id;
    let application_id = loaded_v1.application_id;
    let runtime_application_id = derive_application_id(package_id);
    let package_version = pack_package_version(1, 0, 0);

    let certifier_key_id = KeyId::from_bytes([0x51; 16]);
    let certifier_slot = vault
        .provision_key_slot(ProvisionCustodyKeyRequest {
            key_id: certifier_key_id,
            principal_id: PrincipalId::from_bytes([0x52; 16]),
            control_domain_id: ControlDomainId::from_bytes([0x53; 16]),
            authority_id: [0x54; 16],
            key_purpose: KeyPurpose::SemanticSigning,
            custody_tier: CustodyTier::OperatorFileSealed,
            rotation_epoch: 1,
            valid_from_ms: t0 - 10_000,
            valid_until_ms: t0 + 86_400_000,
            seed: [0x55; 32],
            provisioned_at_ms: t0 - 10_000,
        })
        .expect("provision store certifier key slot in SigningKeyCustodyVault");
    let certifier_signer = StoreCertifierSigner::CustodyVault {
        vault: &vault,
        key_id: certifier_key_id,
        expected_rotation_epoch: Some(1),
    };

    let legit_proof = PublisherSubmissionProof::for_package(legit_publisher, &legit_sk, &loaded_v1);
    let certify_config =
        CertifyPipelineConfig::new(legit_proof, SurfaceProfile::ApplicationPlatform, t0 + 1_000);
    let report = run_certification_pipeline(
        &v1_dir,
        Some(&v2_dir),
        &certify_config,
        &trust_authority,
        &certifier_signer,
    )
    .expect("run 4-gate certification pipeline against real SqlitePublisherTrustAuthority");

    assert!(
        report.all_gates_passed(),
        "4/4 certification gates must pass: {}",
        report.render_text()
    );
    assert!(report.certified());
    assert_eq!(report.gate3.healthy_activated_generation, Some(2));
    assert_eq!(
        report.gate3.baseline_container_tree_digest,
        report.gate3.health_fail_restored_tree_digest
    );
    assert_eq!(
        report.gate3.baseline_container_tree_digest,
        report.gate3.mid_step_fault_restored_tree_digest
    );
    assert!(report.gate4.active_task_guard_verified);
    assert_eq!(report.gate4.residual_private_cache_files, 0);

    let receipt = report
        .receipt
        .as_ref()
        .expect("issued StoreCertificationReceipt");
    verify_store_certification_receipt(receipt, Some(&certifier_slot.public_key), t0 + 1_000)
        .expect("verify custody-signed StoreCertificationReceipt");
    assert_eq!(receipt.publisher_id, legit_publisher);
    assert_eq!(receipt.trust_tier, PublisherTrustTier::CertifiedPublisher);
    assert_eq!(receipt.revocation_epoch, RevocationEpoch::new(1));

    // Malicious second CertifiedPublisher attempting to claim/certify the same
    // ApplicationId MUST fail closed with ApplicationIdHijackAttempt in both
    // `SqlitePublisherTrustAuthority` and `run_certification_pipeline`!
    assert_eq!(
        trust_authority.verify_or_bind_application_owner(
            malicious_publisher,
            PublisherTrustTier::CertifiedPublisher,
            application_id,
            t0 + 1_500,
        ),
        Err(PublisherTrustCheckError::ApplicationIdHijackAttempt {
            application_id,
            registered_owner: legit_publisher,
            attempted_publisher: malicious_publisher,
        })
    );
    let hijack_proof =
        PublisherSubmissionProof::for_package(malicious_publisher, &malicious_sk, &loaded_v1);
    let hijack_config = CertifyPipelineConfig::new(
        hijack_proof,
        SurfaceProfile::ApplicationPlatform,
        t0 + 1_500,
    );
    let hijack_report = run_certification_pipeline(
        &v1_dir,
        Some(&v2_dir),
        &hijack_config,
        &trust_authority,
        &certifier_signer,
    )
    .expect("run certification pipeline for hijack attempt");
    assert!(!hijack_report.certified());
    assert!(hijack_report.receipt.is_none());
    assert!(
        hijack_report
            .receipt_rejection_reason
            .as_deref()
            .is_some_and(|msg| msg.contains("PKG-APPID-001")),
        "expected PKG-APPID-001 hijack rejection, got {:?}",
        hijack_report.receipt_rejection_reason
    );

    // =========================================================================
    // STEP 3: OCI CAS Ingestion, Store Onboarding, 0b1111 `Candidate -> Stable`
    //         Promotion, Offline `SignedEntitlementToken` Redemption, Real
    //         Package Installation & Runtime `CapabilityHandle` Registration.
    // =========================================================================
    let cas = LocalOciCasStore::open(ws.path().join("oci-cas")).expect("open LocalOciCasStore");
    let entitlement_key_id = KeyId::from_bytes([0x61; 16]);
    let entitlement_key = SigningKey::from_bytes(&[0x62; 32]);
    let recall_key = SigningKey::from_bytes(&[0x63; 32]);
    let store_anchors = StoreGovernanceAnchors {
        certifier_public_key: certifier_slot.public_key,
        entitlement_issuer_key_id: entitlement_key_id,
        entitlement_issuer_public_key: entitlement_key.verifying_key().to_bytes(),
        recall_authority_public_key: recall_key.verifying_key().to_bytes(),
        initial_revocation_epoch: RevocationEpoch::new(1),
        initialized_at_ms: t0,
    };
    let store_registry = SqliteStoreRegistry::open(ws.path().join("store-db"), store_anchors)
        .expect("open SqliteStoreRegistry");

    let (oci_layout, component_slots) = ingest_composite_oci_layout(&cas, &v1_dir);
    assert_eq!(
        cas.verify_stored_layout(&oci_layout.oci_manifest_digest)
            .expect("verify OCI CAS layout"),
        oci_layout
    );
    assert_eq!(
        store_registry
            .inspect_release(package_id, package_version)
            .expect("inspect before submission"),
        None,
        "OCI CAS ingestion alone must not grant store catalog visibility ([PKG-OCI-001])"
    );

    let declared_permissions = vec![DeclaredPermissionEntry {
        capability_scope: "sabi.artifact.scoped_chooser".to_owned(),
        human_purpose: "Read and summarize user-selected workspace notes".to_owned(),
        effect_class: EffectClass::ReadOnly,
        network_egress_declared: false,
        sandbox_profile: "wasm-dual-layer-v1".to_owned(),
    }];
    let manifest_digest_bytes = *receipt.manifest_digest.as_bytes();
    let release_submission = build_signed_store_submission(
        package_id,
        package_version,
        manifest_digest_bytes,
        oci_layout.oci_manifest_digest,
        vec![application_id, runtime_application_id],
        component_slots,
        declared_permissions,
        legit_publisher,
        &legit_sk,
        t0 + 2_000,
    );
    let candidate_record = store_registry
        .submit_release(&cas, &trust_authority, &release_submission)
        .expect("onboard Candidate composite release with real SqlitePublisherTrustAuthority");
    assert_eq!(candidate_record.channel, StoreChannel::Candidate);
    assert_eq!(candidate_record.track, StoreCatalogTrack::OfficialTrack);
    assert_eq!(candidate_record.components.len(), 5);

    // Malicious publisher hijack attempt at the Store Registry boundary also
    // fails closed with zero committed rows!
    let hijack_pkg_id = PackageId::from_bytes([0x9F; 16]);
    let hijack_store_sub = build_signed_store_submission(
        hijack_pkg_id,
        package_version,
        manifest_digest_bytes,
        oci_layout.oci_manifest_digest,
        vec![application_id],
        candidate_record.components.clone(),
        Vec::new(),
        malicious_publisher,
        &malicious_sk,
        t0 + 2_100,
    );
    let hijack_store_err = store_registry
        .submit_release(&cas, &trust_authority, &hijack_store_sub)
        .expect_err("store submission hijacking bound ApplicationId must fail closed");
    assert_eq!(
        match hijack_store_err {
            StoreRegistryError::PublisherTrust(inner) => Some(inner),
            _ => None,
        },
        Some(PublisherTrustCheckError::ApplicationIdHijackAttempt {
            application_id,
            registered_owner: legit_publisher,
            attempted_publisher: malicious_publisher,
        })
    );
    assert_eq!(
        store_registry
            .inspect_release(hijack_pkg_id, package_version)
            .expect("inspect rejected hijack package"),
        None
    );

    // Derive the 4-gate bitmask directly from `StoreCertificationReceipt` and
    // sign `SignedCertificationAttestation` inside `SigningKeyCustodyVault`.
    let mut gates_mask: u8 = 0;
    for gate_summary in &receipt.gate_summaries {
        if gate_summary.status == GateStatus::Pass {
            let bit = match gate_summary.gate {
                GateId::Gate1ProtocolAndEffects => 0b0001,
                GateId::Gate2LeastPrivilegeAndSupplyChain => 0b0010,
                GateId::Gate3TransactionalUpdateAndRollback => 0b0100,
                GateId::Gate4ZeroResidueUninstall => 0b1000,
            };
            gates_mask |= bit;
        }
    }
    assert_eq!(gates_mask, REQUIRED_CERTIFICATION_GATES_MASK);

    let mut attestation = SignedCertificationAttestation {
        package_id,
        application_id,
        package_version,
        manifest_digest: manifest_digest_bytes,
        oci_manifest_digest: oci_layout.oci_manifest_digest,
        publisher_id: legit_publisher,
        passed_gates_mask: gates_mask,
        issued_at_ms: t0 + 2_500,
        expires_at_ms: t0 + 86_400_000,
        certifier_public_key: certifier_slot.public_key,
        ed25519_signature: [0u8; 64],
    };
    let attestation_sig_audit = vault
        .sign_digest_under_custody(CustodySignRequest {
            key_id: certifier_key_id,
            expected_rotation_epoch: Some(1),
            domain_tag: CERTIFICATION_ATTESTATION_SIGNING_DOMAIN,
            message_digest: attestation.signing_digest(),
            signed_at_ms: t0 + 2_500,
        })
        .expect("sign SignedCertificationAttestation under certifier custody slot");
    attestation.ed25519_signature = attestation_sig_audit.signature;

    let promo = store_registry
        .promote_candidate_to_stable(
            &attestation,
            IdempotencyKey::from_bytes([0x71; 16]),
            t0 + 2_600,
        )
        .expect("promote 4-gate certified release from Candidate to Stable");
    assert!(matches!(promo, StorePromotionDecision::Promoted(_)));
    assert_eq!(
        store_registry
            .list_official_stable_releases()
            .expect("list official stable releases")
            .len(),
        1
    );

    // Open real kernel/platform authorities for entitlement redemption,
    // installation, capability grant, and execution gating.
    let identity =
        IdentityAuthority::open(ws.path().join("identity")).expect("open IdentityAuthority");
    let artifacts = ArtifactStore::open(ws.path().join("artifact")).expect("open ArtifactStore");
    let applications = ApplicationAuthority::open(ws.path().join("application"))
        .expect("open ApplicationAuthority");
    let capabilities =
        CapabilityAuthority::open(ws.path().join("capability")).expect("open CapabilityAuthority");

    let (operator_key, operator_binding, operator_seed) =
        bootstrap_principal(&identity, 0x81, t0 - 10_000, t0 + 86_400_000);
    let (runtime_key, runtime_binding, _runtime_seed) =
        bootstrap_principal(&identity, 0x82, t0 - 10_000, t0 + 86_400_000);

    // Redeem offline Ed25519 `SignedEntitlementToken` (`[NLOS-DEFER-001]`).
    let token_id = [0x72; 16];
    let token_epoch = RevocationEpoch::new(1);
    let token_digest = compute_entitlement_token_digest(
        &token_id,
        runtime_binding.principal_id,
        application_id,
        StoreChannel::Stable,
        1,
        t0 + 2_700,
        t0 + 3_600_000,
        token_epoch,
        entitlement_key_id,
    );
    let entitlement_token = SignedEntitlementToken {
        token_id,
        holder_principal: runtime_binding.principal_id,
        application_id,
        allowed_channel: StoreChannel::Stable,
        max_seat_or_install_count: 1,
        issued_at_ms: t0 + 2_700,
        expires_at_ms: t0 + 3_600_000,
        min_revocation_epoch: token_epoch,
        issuer_key_id: entitlement_key_id,
        ed25519_signature: entitlement_key.sign(&token_digest).to_bytes(),
    };
    let redeem_decision = store_registry
        .verify_and_redeem_entitlement(&RedeemEntitlementRequest {
            token: entitlement_token,
            presenter_principal: runtime_binding.principal_id,
            requested_application_id: application_id,
            package_id,
            package_version,
            idempotency_key: IdempotencyKey::from_bytes([0x73; 16]),
            now_ms: t0 + 2_800,
        })
        .expect("redeem offline SignedEntitlementToken");
    assert!(matches!(
        redeem_decision,
        EntitlementRedeemDecision::Granted(_)
    ));
    assert_eq!(redeem_decision.receipt().seat_ordinal, 1);
    assert_eq!(redeem_decision.receipt().remaining_seats, 0);

    // Materialize and install the verified package in `ArtifactStore` +
    // `ApplicationAuthority`, then grant and register runtime `CapabilityHandle`.
    let worker_artifact_id = ArtifactId::from_bytes([0x74; 16]);
    let worker_bytes = fs::read(v1_dir.join("assistant.bin")).expect("read assistant.bin");
    artifacts
        .create_artifact(CreateArtifactSpec {
            artifact_id: worker_artifact_id,
            idempotency_key: IdempotencyKey::from_bytes([0x75; 16]),
            content_type: "application/octet-stream".to_owned(),
            application_id: Some(application_id),
            owner: Some("w79-e2e".to_owned()),
            created_at_ms: t0 + 2_900,
        })
        .expect("create worker artifact");
    let worker_rev = artifacts
        .put_revision(PutRevisionRequest {
            artifact_id: worker_artifact_id,
            expected_head_revision: 0,
            bytes: &worker_bytes,
            created_at_ms: t0 + 2_950,
            provenance: ProvenanceSourceTriple {
                source_a: [0x01; 16],
                source_b: [0x02; 16],
                source_digest: ContentDigest::of_bytes(&worker_bytes),
            },
        })
        .expect("put worker artifact revision");
    let worker_digest = match worker_rev {
        PutRevisionDecision::Committed(rec) | PutRevisionDecision::Replayed(rec) => rec.digest,
    };

    let artifact_pkg_manifest = PackageManifest {
        package_id,
        version: package_version,
        entries: vec![PackageManifestEntry {
            name: "assistant.bin".to_owned(),
            artifact_id: worker_artifact_id,
            digest: worker_digest,
            role: PackageEntryRole::Executable,
        }],
    };
    let manifest_msg = package_manifest_message(&artifact_pkg_manifest);
    let signed_artifact_pkg = SignedPackage {
        manifest: artifact_pkg_manifest,
        signer: operator_binding.principal_id,
        signature: operator_key.sign(&manifest_msg).to_bytes(),
    };
    let verified_pkg = artifacts
        .verify_package(
            &identity,
            VerifyPackageRequest {
                signed: &signed_artifact_pkg,
                idempotency_key: IdempotencyKey::from_bytes([0x76; 16]),
                verified_at_ms: t0 + 3_000,
            },
        )
        .expect("verify signed package in ArtifactStore");
    let install_receipt = applications
        .install_application(
            &artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: verified_pkg.receipt().receipt_id,
                idempotency_key: IdempotencyKey::from_bytes([0x77; 16]),
                installed_at_ms: t0 + 3_100,
            },
        )
        .expect("install application in ApplicationAuthority")
        .receipt();
    assert_eq!(install_receipt.application_id, runtime_application_id);
    assert_ne!(
        *package_id.as_bytes(),
        *runtime_application_id.as_bytes(),
        "[PKG-SEP-001] PackageId and runtime ApplicationId must remain distinct"
    );
    assert_eq!(
        applications
            .inspect_application(package_id)
            .expect("inspect installed application")
            .expect("installed record")
            .status,
        ApplicationStatus::Installed
    );

    let app_namespace = NamespaceId::from_bytes([0x78; 16]);
    let target_ns = CapabilityTarget::Namespace(app_namespace);
    let issue_cap_cmd = IssueRootCapabilityRequest {
        issuer_key_id: operator_binding.key_id,
        holder_key_id: runtime_binding.key_id,
        target: target_ns,
        rights: CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::DELEGATE),
        purpose_digest: None,
        valid_from_ms: t0 - 1_000,
        valid_until_ms: t0 + 86_400_000,
        delegation_depth_remaining: 2,
        call_limit: Some(32),
        idempotency_key: IdempotencyKey::from_bytes([0x79; 16]),
        issued_at_ms: t0 + 3_200,
    };
    let issue_cap_msg = issue_root_command_message(issue_cap_cmd);
    let runtime_cap_handle = capabilities
        .issue_root_signed(
            &identity,
            SignedIssueRootCapabilityRequest {
                command: issue_cap_cmd,
                signer: operator_binding.principal_id,
                signature: operator_key.sign(&issue_cap_msg).to_bytes(),
            },
        )
        .expect("issue runtime CapabilityHandle")
        .record()
        .handle;

    store_registry
        .register_granted_capability(
            &capabilities,
            &RegisterGrantedCapabilityRequest {
                handle: runtime_cap_handle,
                package_id,
                package_version,
                application_id: runtime_application_id,
                installation_id: Some(install_receipt.installation_id),
                registered_at_ms: t0 + 3_300,
            },
        )
        .expect("register granted CapabilityHandle in SqliteStoreRegistry");

    let consume_capability = |handle: CapabilityHandle, idem_byte: u8, at_ms: u64| {
        let event_id = SemanticEventId::from_bytes([idem_byte; 32]);
        let sem_msg = semantic_signature_message(event_id);
        let verified_signer = identity
            .verify_semantic_signature(VerifySemanticSignatureRequest {
                event_id,
                issuer: runtime_binding.principal_id,
                control_domain_id: runtime_binding.control_domain_id,
                key_id: runtime_binding.key_id,
                signature: runtime_key.sign(&sem_msg).to_bytes(),
                admitted_at_ms: at_ms,
            })
            .expect("verify runtime semantic signature");
        capabilities.consume(ConsumeCapabilityRequest {
            handle,
            signer: verified_signer,
            target: target_ns,
            required_right: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: None,
            idempotency_key: IdempotencyKey::from_bytes([idem_byte; 16]),
            consumed_at_ms: at_ms,
        })
    };
    consume_capability(runtime_cap_handle, 0x7A, t0 + 3_400)
        .expect("baseline capability consumption succeeds before recall");

    // =========================================================================
    // STEP 4: Plan 7-Class Ecosystem Selector Locking, G3 Conditioned
    //         Materialization (`WaitingAuthorization -> Materializing`), and
    //         Level 3 `RecallSeverity::QuarantineInstallation` Cascade.
    // =========================================================================
    let plan_authority = SqlitePlanAuthority::open(ws.path().join("plan.sqlite3"))
        .expect("open SqlitePlanAuthority");
    let task_authority = SqliteTaskAuthority::open_with_scale_profile(
        ws.path().join("task.sqlite3"),
        &TASK_PROFILE_10K,
    )
    .expect("open SqliteTaskAuthority");
    let ecosystem_gate = SqliteEcosystemDependencyGate::open(ws.path().join("ecosystem-gate"))
        .expect("open SqliteEcosystemDependencyGate");

    let plan_rev = plan_authority
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: None,
            nodes: vec![
                conditioned_plan_node(0xA1, app_namespace),
                conditioned_plan_node(0xA2, app_namespace),
            ],
            idempotency_key: IdempotencyKey::from_bytes([0x90; 16]),
            applied_at_ms: t0 + 4_000,
        })
        .expect("apply plan revision with pre-recall and post-recall nodes")
        .receipt();
    let plan_nodes = plan_authority
        .list_plan_nodes(plan_rev.plan_id)
        .expect("list plan nodes");
    let node_pre_recall = plan_nodes
        .iter()
        .find(|n| n.node_key == [0xA1; 16])
        .expect("node 0xA1")
        .node_id;
    let node_post_recall = plan_nodes
        .iter()
        .find(|n| n.node_key == [0xA2; 16])
        .expect("node 0xA2")
        .node_id;

    // 4a. Reject floating `"latest"` (`FloatingVersionForbidden`) and
    //     `SelectorProvenance::UnpinnedSearchResult` (`UnpinnedSearchResultForbidden`).
    assert!(matches!(
        parse_version_pin("latest"),
        Err(EcosystemGateError::FloatingVersionForbidden { .. })
    ));
    let floating_err = ecosystem_gate
        .bind_node_dependency_proposals(
            &plan_authority,
            BindNodeDependenciesRequest {
                plan_id: plan_rev.plan_id,
                node_id: node_pre_recall,
                proposals: vec![TypedDependencySelectorProposal {
                    kind: PlanDependencyEntityKind::Package,
                    entity_id: *package_id.as_bytes(),
                    version_or_tag: SelectorVersionPin::ExactTag("latest".to_owned()),
                    expected_content_digest: Some(oci_layout.oci_manifest_digest),
                    provenance: SelectorProvenance::DeclaredPinnedManifest,
                }],
                idempotency_key: IdempotencyKey::from_bytes([0x91; 16]),
                bound_at_ms: t0 + 4_100,
            },
        )
        .expect_err("floating 'latest' version pin must fail closed");
    assert!(matches!(
        floating_err,
        EcosystemGateError::FloatingVersionForbidden { .. }
    ));

    let search_err = ecosystem_gate
        .bind_node_dependency_proposals(
            &plan_authority,
            BindNodeDependenciesRequest {
                plan_id: plan_rev.plan_id,
                node_id: node_pre_recall,
                proposals: vec![TypedDependencySelectorProposal {
                    kind: PlanDependencyEntityKind::Package,
                    entity_id: *package_id.as_bytes(),
                    version_or_tag: SelectorVersionPin::ExactTag("v1.0.0".to_owned()),
                    expected_content_digest: Some(oci_layout.oci_manifest_digest),
                    provenance: SelectorProvenance::UnpinnedSearchResult,
                }],
                idempotency_key: IdempotencyKey::from_bytes([0x92; 16]),
                bound_at_ms: t0 + 4_150,
            },
        )
        .expect_err("UnpinnedSearchResult provenance must fail closed");
    assert!(matches!(
        search_err,
        EcosystemGateError::UnpinnedSearchResultForbidden {
            kind: PlanDependencyEntityKind::Package,
            ..
        }
    ));

    // 4b. Bind and resolve all 7 ecosystem entity classes (with `Package` backed
    //     live by `SqliteStoreRegistry`) for both nodes while release is `Stable`,
    //     and materialize `node_pre_recall` (`WaitingAuthorization -> Materializing`).
    let mut live_source = LiveStoreEcosystemSource::new(
        &store_registry,
        package_id,
        package_version,
        "v1.0.0",
        oci_layout.oci_manifest_digest,
    );
    let pre_handles = bind_and_resolve_seven_classes(
        &ecosystem_gate,
        &plan_authority,
        &live_source,
        plan_rev.plan_id,
        node_pre_recall,
        package_id,
        oci_layout.oci_manifest_digest,
        0xA0,
        t0 + 4_200,
    );
    assert_eq!(pre_handles[0].kind, PlanDependencyEntityKind::Package);
    assert_eq!(pre_handles[0].entity_id, *package_id.as_bytes());
    assert_eq!(
        pre_handles[0].content_digest,
        oci_layout.oci_manifest_digest
    );

    bind_and_resolve_seven_classes(
        &ecosystem_gate,
        &plan_authority,
        &live_source,
        plan_rev.plan_id,
        node_post_recall,
        package_id,
        oci_layout.oci_manifest_digest,
        0xB0,
        t0 + 4_300,
    );
    // Advance `node_post_recall` to `WaitingAuthorization` prior to recall so we
    // verify both live recall fencing and capability/quarantine fencing hold it
    // strictly in `PlanNodeState::WaitingAuthorization`.
    plan_authority
        .record_node_transition(NodeTransitionRequest {
            plan_id: plan_rev.plan_id,
            node_id: node_post_recall,
            from_state: PlanNodeState::Declared,
            to_state: PlanNodeState::Eligible,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([0xBC; 16]),
            transitioned_at_ms: t0 + 4_350,
        })
        .expect("advance post-recall node Declared -> Eligible");
    plan_authority
        .record_node_transition(NodeTransitionRequest {
            plan_id: plan_rev.plan_id,
            node_id: node_post_recall,
            from_state: PlanNodeState::Eligible,
            to_state: PlanNodeState::WaitingAuthorization,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([0xBD; 16]),
            transitioned_at_ms: t0 + 4_360,
        })
        .expect("advance post-recall node Eligible -> WaitingAuthorization");

    let exec_gate = RealRuntimeExecutionGate {
        capabilities: &capabilities,
        applications: &applications,
        package_id,
        capability_handle: runtime_cap_handle,
        allowed_namespaces: vec![app_namespace],
        resource_ceiling: ResourceContractCondition {
            cpu_shares: 4_000,
            memory_mib: 4_096,
            io_weight: 500,
        },
        payer_principal: runtime_binding.principal_id,
        payer_max_fanout_budget: 8,
    };
    let task_consult = TaskAuthorityAdmission(&task_authority);

    assert_eq!(
        store_registry
            .evaluate_local_launch_gate(package_id, package_version)
            .expect("baseline local launch gate before recall"),
        LocalLaunchDisposition::Allowed
    );
    let pre_decision = ecosystem_gate
        .advance_and_authorize_materialization(
            &plan_authority,
            &live_source,
            &exec_gate,
            &task_consult,
            AuthorizeNodeMaterializationRequest {
                plan_id: plan_rev.plan_id,
                node_id: node_pre_recall,
                idempotency_key: IdempotencyKey::from_bytes([0xC1; 16]),
                at_ms: t0 + 4_400,
            },
        )
        .expect("healthy certified package passes G3 conditioned materialization");
    assert!(matches!(
        pre_decision,
        ConditionedMaterializationDecision::Materializing { .. }
    ));
    assert_eq!(
        plan_authority
            .inspect_node(plan_rev.plan_id, node_pre_recall)
            .expect("inspect pre-recall node")
            .expect("node exists")
            .state,
        PlanNodeState::Materializing
    );

    // 4c. Issue Level 3 `RecallSeverity::QuarantineInstallation` (`RevocationEpoch(2)`)
    //     `SignedRecallAdvisory`: atomically marks release `StoreChannel::Recalled`,
    //     revokes `runtime_cap_handle` in `CapabilityAuthority`, and disables/quarantines
    //     the installation in `ApplicationAuthority` (`ApplicationStatus::Disabled`).
    let recall_advisory_id = [0xD1; 16];
    let recall_epoch = RevocationEpoch::new(2);
    let recall_reason = "Critical supply-chain compromise requiring Level 3 QuarantineInstallation";
    let recall_digest = compute_recall_advisory_digest(
        &recall_advisory_id,
        package_id,
        runtime_application_id,
        Some(package_version),
        RecallSeverity::QuarantineInstallation,
        recall_epoch,
        recall_reason,
        t0 + 5_000,
    );
    let signed_recall = SignedRecallAdvisory {
        advisory_id: recall_advisory_id,
        package_id,
        application_id: runtime_application_id,
        target_version: Some(package_version),
        severity: RecallSeverity::QuarantineInstallation,
        revocation_epoch: recall_epoch,
        reason: recall_reason.to_owned(),
        issued_at_ms: t0 + 5_000,
        authority_signature: recall_key.sign(&recall_digest).to_bytes(),
    };

    let revocation_authorizer = DirectEd25519RevocationAuthorizer::from_seed(
        operator_binding.principal_id,
        operator_binding.key_id,
        &operator_seed,
    );
    let enforcement = RecallEnforcementAuthorities {
        identity: &identity,
        capabilities: &capabilities,
        revocation_authorizer: &revocation_authorizer,
        applications: Some(&applications),
    };
    let recall_decision = store_registry
        .apply_recall_advisory(
            &signed_recall,
            IdempotencyKey::from_bytes([0xD2; 16]),
            t0 + 5_100,
            Some(&enforcement),
        )
        .expect("apply Level 3 QuarantineInstallation recall advisory");
    assert!(matches!(recall_decision, RecallApplyDecision::Applied(_)));
    let recall_receipt = recall_decision.receipt();
    assert_eq!(recall_receipt.revoked_capability_count, 1);
    assert!(recall_receipt.application_quarantined);
    assert_eq!(
        store_registry
            .current_store_revocation_epoch()
            .expect("store epoch"),
        RevocationEpoch::new(2)
    );
    assert_eq!(
        store_registry
            .inspect_release(package_id, package_version)
            .expect("inspect recalled release")
            .expect("release exists")
            .channel,
        StoreChannel::Recalled
    );
    assert!(matches!(
        store_registry.evaluate_local_launch_gate(package_id, package_version),
        Err(StoreRegistryError::ReleaseRecalled {
            severity: RecallSeverity::QuarantineInstallation,
            ..
        })
    ));
    assert_eq!(
        applications
            .inspect_application(package_id)
            .expect("inspect quarantined app")
            .expect("app exists")
            .status,
        ApplicationStatus::Disabled
    );

    // Revoked capability fails closed at both current generation (`CapabilityRevoked`)
    // and pre-recall generation (`GenerationFenceConflict`).
    let gen_2 = Generation::new(NonZeroU64::new(2).expect("non-zero"));
    let current_gen_handle = CapabilityHandle {
        capability_id: runtime_cap_handle.capability_id,
        generation: gen_2,
    };
    assert!(matches!(
        consume_capability(current_gen_handle, 0xD3, t0 + 5_200),
        Err(CapabilityAuthorityError::CapabilityRevoked)
    ));
    assert!(matches!(
        consume_capability(runtime_cap_handle, 0xD4, t0 + 5_200),
        Err(CapabilityAuthorityError::GenerationFenceConflict)
    ));

    // 4d. Attempting materialization on `node_post_recall` is blocked BOTH by:
    //     (i) the live `SqliteStoreRegistry` recall freshness fence
    //         (`PinnedDependencyRevokedOrRecalled`), AND
    //     (ii) the G3 runtime authorization fence (`DeniedInWaitingAuthorization`)
    //          even if an offline node's catalog cache has not refreshed yet —
    //     strictly holding `node_post_recall` in `PlanNodeState::WaitingAuthorization`!
    let live_recall_err = ecosystem_gate
        .advance_and_authorize_materialization(
            &plan_authority,
            &live_source,
            &exec_gate,
            &task_consult,
            AuthorizeNodeMaterializationRequest {
                plan_id: plan_rev.plan_id,
                node_id: node_post_recall,
                idempotency_key: IdempotencyKey::from_bytes([0xE1; 16]),
                at_ms: t0 + 5_300,
            },
        )
        .expect_err("live Recalled store channel must block materialization");
    assert!(matches!(
        live_recall_err,
        EcosystemGateError::PinnedDependencyRevokedOrRecalled {
            kind: PlanDependencyEntityKind::Package,
            ..
        }
    ));
    assert_eq!(
        plan_authority
            .inspect_node(plan_rev.plan_id, node_post_recall)
            .expect("inspect post-recall node after live fence")
            .expect("node exists")
            .state,
        PlanNodeState::WaitingAuthorization
    );

    live_source.use_stale_offline_package_cache = true;
    let quarantined_decision = ecosystem_gate
        .advance_and_authorize_materialization(
            &plan_authority,
            &live_source,
            &exec_gate,
            &task_consult,
            AuthorizeNodeMaterializationRequest {
                plan_id: plan_rev.plan_id,
                node_id: node_post_recall,
                idempotency_key: IdempotencyKey::from_bytes([0xE2; 16]),
                at_ms: t0 + 5_400,
            },
        )
        .expect("revoked capability + quarantined app records durable G3 denial receipt");
    let ConditionedMaterializationDecision::DeniedInWaitingAuthorization(denial_audit) =
        quarantined_decision
    else {
        panic!("expected DeniedInWaitingAuthorization, got {quarantined_decision:?}");
    };
    assert!(matches!(
        denial_audit.outcome,
        ConditionedGateOutcome::Denied(ConditionedGateDenialReason::AuthorizationDenied { .. })
    ));
    assert_eq!(
        plan_authority
            .inspect_node(plan_rev.plan_id, node_post_recall)
            .expect("inspect post-recall node after G3 denial")
            .expect("node exists")
            .state,
        PlanNodeState::WaitingAuthorization
    );
    assert_eq!(
        plan_authority
            .inspect_node_materialization_requests(plan_rev.plan_id, node_post_recall)
            .expect("inspect materialization requests on quarantined node")
            .len(),
        0,
        "quarantined node held in WaitingAuthorization must emit zero materialization requests"
    );
}
