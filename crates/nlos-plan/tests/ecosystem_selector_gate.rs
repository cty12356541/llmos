//! Stage E `W79-E4` integration and adversarial falsification battery for
//! `[PLAN-DEPENDENCY-001]` 7-class pinned ecosystem dependency selectors (`U-8`)
//! and `[PLAN-LAZY-001]` G3 `WaitingAuthorization` + `Namespace` /
//! `ResourceContract` / `Fanout` conditioned materialization gate (`U-7`,
//! `ADR-0027` section 5.2, `RISK-E-05`).

#![forbid(unsafe_code)]
#![allow(clippy::too_many_lines)]

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_capability::{
    BootstrapCustodyPrincipalRequest, CapabilityAuthority, CapabilityHandle, CapabilityRights,
    CapabilityTarget, CustodySignRequest, CustodyTier, IssueRootCapabilityRequest,
    RevokeCapabilityRequest, SignedRevokeCapabilityRequest, SigningKeyCustodyVault,
    revoke_command_message,
};
use nlos_identity::{IdentityAuthority, IdentityBinding, KeyPurpose};
use nlos_plan::ecosystem_selector::{
    AuthorizationCheckVerdict, AuthorizeNodeMaterializationRequest, BindNodeDependenciesRequest,
    ConditionedGateDenialReason, ConditionedGateOutcome, ConditionedMaterializationDecision,
    CrossEcosystemEntitySource, CrossEcosystemLookup, ECOSYSTEM_GATE_DB_FILENAME,
    ECOSYSTEM_GATE_SCHEMA_VERSION, EcosystemEntitySnapshot, EcosystemGateError, FanoutGateVerdict,
    NamespaceGateVerdict, NodeExecutionGateConsult, PLAN_DEPENDENCY_ENTITY_KIND_COUNT,
    PinnedDependencyDecision, PinnedEcosystemDependencyHandle, PlanDependencyEntityKind,
    ResolveNodeDependencyRequest, ResourceContractVerdict, SelectorProvenance, SelectorVersionPin,
    SqliteEcosystemDependencyGate, TypedDependencySelector, TypedDependencySelectorProposal,
    parse_version_pin,
};
use nlos_plan::{
    AdmissionConsult, AdmissionConsultOutcome, ApplyPlanRevisionRequest, FanoutCondition,
    MaterializationAdmission, MaterializationAdmissionVerdict, MaterializationRejection,
    MaterializationRequest, MaterializationResolution, NamespaceCondition, NodeConditions,
    NodeTransitionRequest, PlanNodeDeclaration, PlanNodeKind, PlanNodeState, PlanStoreError,
    ResourceContractCondition, SqlitePlanAuthority,
};
use nlos_task::{
    MaterializationAdmissionFacts, ScaleProfile, SqliteTaskAuthority, TASK_PROFILE_10K,
    TaskStoreError,
};
use nlos_types::{IdempotencyKey, NamespaceId, PrincipalId, TaskNodeId, TaskPlanId};
use rusqlite::Connection;

static ZERO_WORKING_SET: ScaleProfile = ScaleProfile {
    profile_id: "task-e4-zero-working-set",
    max_task_nodes: 100_000,
    max_task_registrations: 100_000,
    max_active_working_set: 0,
    reclaim_threshold_ratio: None,
};

static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(1);

struct TempFixtureDir(PathBuf);

impl TempFixtureDir {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after UNIX_EPOCH")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "nlos-plan-e4-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Deterministic in-memory cross-ecosystem registry backing all seven
/// `[PLAN-DEPENDENCY-001]` entity classes with live recall & drift injection.
#[derive(Default)]
struct RegistryEntitySource {
    entries: Mutex<BTreeMap<(PlanDependencyEntityKind, [u8; 16]), CrossEcosystemLookup>>,
}

impl RegistryEntitySource {
    fn new_with_seven_defaults() -> Self {
        let source = Self::default();
        for kind in PlanDependencyEntityKind::ALL {
            let code = kind.encode();
            source.upsert(
                kind,
                [code; 16],
                CrossEcosystemLookup::Found(EcosystemEntitySnapshot {
                    pinned_version_tag: format!("v1.{code}.0"),
                    pinned_generation: u64::from(code),
                    content_digest: [code.wrapping_add(0x40); 32],
                }),
            );
        }
        source
    }

    fn upsert(
        &self,
        kind: PlanDependencyEntityKind,
        entity_id: [u8; 16],
        state: CrossEcosystemLookup,
    ) {
        self.entries
            .lock()
            .expect("registry lock")
            .insert((kind, entity_id), state);
    }
}

impl CrossEcosystemEntitySource for RegistryEntitySource {
    type Error = Infallible;

    fn supports_kind(&self, _kind: PlanDependencyEntityKind) -> bool {
        true
    }

    fn resolve_pinned(
        &self,
        selector: &TypedDependencySelector,
    ) -> Result<CrossEcosystemLookup, Infallible> {
        let map = self.entries.lock().expect("registry lock");
        Ok(map
            .get(&(selector.kind(), selector.entity_id()))
            .cloned()
            .unwrap_or(CrossEcosystemLookup::NotFound))
    }

    fn verify_handle_live(
        &self,
        handle: &PinnedEcosystemDependencyHandle,
    ) -> Result<CrossEcosystemLookup, Infallible> {
        let map = self.entries.lock().expect("registry lock");
        Ok(map
            .get(&(handle.kind, handle.entity_id))
            .cloned()
            .unwrap_or(CrossEcosystemLookup::NotFound))
    }
}

/// Production-shaped `AdmissionConsult` adapter over `SqliteTaskAuthority`.
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

/// Real `CapabilityAuthority`-backed G3 execution gate consult enforcing
/// live capability verification, namespace scope, resource quota ceilings,
/// and downstream fanout payer accountability.
#[derive(Clone)]
struct RealAuthorityExecutionGate<'a> {
    capability_authority: &'a CapabilityAuthority,
    capability_handle: Option<CapabilityHandle>,
    allowed_namespaces: Vec<NamespaceId>,
    resource_ceiling: ResourceContractCondition,
    payer_principal: Option<PrincipalId>,
    payer_max_fanout_budget: u64,
}

impl NodeExecutionGateConsult for RealAuthorityExecutionGate<'_> {
    type Error = String;

    fn check_authorization(
        &self,
        _plan_id: TaskPlanId,
        _node_id: TaskNodeId,
        pinned_handles: &[PinnedEcosystemDependencyHandle],
        at_ms: u64,
    ) -> Result<AuthorizationCheckVerdict, String> {
        let Some(handle) = self.capability_handle else {
            return Ok(AuthorizationCheckVerdict::Denied {
                reason: "node execution lacks an assigned capability handle".to_owned(),
            });
        };
        match self.capability_authority.inspect_active(handle, at_ms) {
            Ok(record) => {
                if !record.rights.contains(CapabilityRights::SEMANTIC_APPEND) {
                    return Ok(AuthorizationCheckVerdict::Denied {
                        reason: "capability lacks SEMANTIC_APPEND execution right".to_owned(),
                    });
                }
                if pinned_handles.is_empty() {
                    return Ok(AuthorizationCheckVerdict::Denied {
                        reason: "expected non-empty pinned ecosystem handles for node".to_owned(),
                    });
                }
                Ok(AuthorizationCheckVerdict::Authorized {
                    capability_or_grant_id: *record.handle.capability_id.as_bytes(),
                })
            }
            Err(err) => Ok(AuthorizationCheckVerdict::Denied {
                reason: format!("CapabilityAuthority rejected handle: {err}"),
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
                    reason: format!("namespace {ns:?} is outside authorized capability scope"),
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
                reason: format!(
                    "requested (cpu={}, mem_mib={}, io={}) exceeds ceiling (cpu={}, mem_mib={}, io={})",
                    resource_contract.cpu_shares,
                    resource_contract.memory_mib,
                    resource_contract.io_weight,
                    self.resource_ceiling.cpu_shares,
                    self.resource_ceiling.memory_mib,
                    self.resource_ceiling.io_weight,
                ),
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
        let Some(payer_principal_id) = self.payer_principal else {
            return Ok(FanoutGateVerdict::Denied {
                reason: "missing downstream fanout fee payer principal (MSG-PAY-001)".to_owned(),
            });
        };
        if effective_downstream_fanout > self.payer_max_fanout_budget {
            return Ok(FanoutGateVerdict::Denied {
                reason: format!(
                    "effective downstream fanout {effective_downstream_fanout} exceeds payer budget {}",
                    self.payer_max_fanout_budget
                ),
            });
        }
        Ok(FanoutGateVerdict::Approved { payer_principal_id })
    }
}

fn conditioned_node(
    key: u8,
    dependency_keys: Vec<[u8; 16]>,
    conditions: Option<NodeConditions>,
) -> PlanNodeDeclaration {
    PlanNodeDeclaration {
        node_key: [key; 16],
        kind: PlanNodeKind::AgentRole,
        binding_digest: [key; 32],
        dependency_keys,
        input_selectors_digest: [key.wrapping_add(1); 32],
        output_contract_digest: [key.wrapping_add(2); 32],
        policy_digest: [key.wrapping_add(3); 32],
        resource_ceiling_digest: [key.wrapping_add(4); 32],
        conditions,
    }
}

fn default_conditions(ns_a: NamespaceId, max_fanout: u64) -> NodeConditions {
    NodeConditions {
        namespace: NamespaceCondition {
            namespaces: vec![ns_a],
        },
        resource_contract: ResourceContractCondition {
            cpu_shares: 1_000,
            memory_mib: 512,
            io_weight: 100,
        },
        fanout: FanoutCondition {
            max_downstream_fanout: max_fanout,
        },
    }
}

fn node_id_for_key(
    plan_authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_key: [u8; 16],
) -> TaskNodeId {
    plan_authority
        .list_plan_nodes(plan_id)
        .expect("list plan nodes")
        .into_iter()
        .find(|n| n.node_key == node_key)
        .expect("node exists")
        .node_id
}

fn seven_class_proposals() -> Vec<TypedDependencySelectorProposal> {
    PlanDependencyEntityKind::ALL
        .into_iter()
        .map(|kind| {
            let code = kind.encode();
            let pin = if code % 2 == 1 {
                parse_version_pin(&format!("v1.{code}.0")).expect("valid exact tag")
            } else {
                parse_version_pin(&format!("gen:{code}")).expect("valid exact generation")
            };
            let provenance = if code % 2 == 1 {
                SelectorProvenance::DeclaredPinnedManifest
            } else {
                SelectorProvenance::TrustedUserSelection
            };
            let expected_content_digest = if code <= 4 {
                Some([code.wrapping_add(0x40); 32])
            } else {
                None
            };
            TypedDependencySelectorProposal {
                kind,
                entity_id: [code; 16],
                version_or_tag: pin,
                expected_content_digest,
                provenance,
            }
        })
        .collect()
}

fn bind_and_resolve_all_seven(
    gate: &SqliteEcosystemDependencyGate,
    plan_authority: &SqlitePlanAuthority,
    source: &RegistryEntitySource,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    key_seed: u8,
) -> Vec<PinnedEcosystemDependencyHandle> {
    let selectors = gate
        .bind_node_dependency_proposals(
            plan_authority,
            BindNodeDependenciesRequest {
                plan_id,
                node_id,
                proposals: seven_class_proposals(),
                idempotency_key: IdempotencyKey::from_bytes([key_seed; 16]),
                bound_at_ms: 1_100,
            },
        )
        .expect("bind 7-class proposals");
    assert_eq!(selectors.len(), PLAN_DEPENDENCY_ENTITY_KIND_COUNT);

    let mut handles = Vec::with_capacity(PLAN_DEPENDENCY_ENTITY_KIND_COUNT);
    for (idx, kind) in PlanDependencyEntityKind::ALL.into_iter().enumerate() {
        let code = kind.encode();
        let step_seed = key_seed.wrapping_add(u8::try_from(idx + 1).expect("idx fits u8"));
        let decision = gate
            .resolve_node_dependency(
                plan_authority,
                source,
                ResolveNodeDependencyRequest {
                    plan_id,
                    node_id,
                    kind,
                    entity_id: [code; 16],
                    idempotency_key: IdempotencyKey::from_bytes([step_seed; 16]),
                    resolved_at_ms: 1_200 + u64::from(code),
                },
            )
            .expect("resolve pinned dependency");
        assert!(matches!(decision, PinnedDependencyDecision::Resolved(_)));
        handles.push(decision.into_handle());
    }
    handles
}

fn bootstrap_custody_operator(
    vault: &SigningKeyCustodyVault,
    identity: &IdentityAuthority,
    seed: u8,
) -> IdentityBinding {
    let (_slot, binding) = vault
        .bootstrap_principal_under_custody(
            identity,
            BootstrapCustodyPrincipalRequest {
                principal_profile_digest: [seed.wrapping_add(1); 32],
                control_domain_policy_digest: [seed.wrapping_add(2); 32],
                authority_id: [seed.wrapping_add(3); 16],
                key_purpose: KeyPurpose::SemanticSigning,
                custody_tier: CustodyTier::OperatorFileSealed,
                valid_from_ms: 0,
                valid_until_ms: 1_000_000,
                seed: [seed; 32],
                bootstrap_idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(4); 16]),
                custody_binding_idempotency_key: IdempotencyKey::from_bytes(
                    [seed.wrapping_add(5); 16],
                ),
                provisioned_at_ms: 100,
            },
        )
        .expect("bootstrap custody principal");
    binding
}

fn issue_namespace_capability(
    vault: &SigningKeyCustodyVault,
    capability_authority: &CapabilityAuthority,
    identity: &IdentityAuthority,
    issuer: &IdentityBinding,
    holder: &IdentityBinding,
    namespace: NamespaceId,
    key_seed: u8,
) -> CapabilityHandle {
    let (decision, _audit) = vault
        .issue_root_capability_under_custody(
            capability_authority,
            identity,
            IssueRootCapabilityRequest {
                issuer_key_id: issuer.key_id,
                holder_key_id: holder.key_id,
                target: CapabilityTarget::Namespace(namespace),
                rights: CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::DELEGATE),
                purpose_digest: None,
                valid_from_ms: 500,
                valid_until_ms: 500_000,
                delegation_depth_remaining: 2,
                call_limit: Some(64),
                idempotency_key: IdempotencyKey::from_bytes([key_seed; 16]),
                issued_at_ms: 600,
            },
        )
        .expect("issue root capability under custody");
    decision.record().handle
}

fn revoke_capability_under_custody(
    vault: &SigningKeyCustodyVault,
    capability_authority: &CapabilityAuthority,
    identity: &IdentityAuthority,
    revoker: &IdentityBinding,
    handle: CapabilityHandle,
    key_seed: u8,
    revoked_at_ms: u64,
) {
    let command = RevokeCapabilityRequest {
        handle,
        revoker_key_id: revoker.key_id,
        idempotency_key: IdempotencyKey::from_bytes([key_seed; 16]),
        revoked_at_ms,
    };
    let digest = revoke_command_message(command);
    let audit = vault
        .sign_digest_under_custody(CustodySignRequest {
            key_id: revoker.key_id,
            expected_rotation_epoch: None,
            domain_tag: b"nlos/capability/command/revoke/v1",
            message_digest: digest,
            signed_at_ms: revoked_at_ms,
        })
        .expect("custody sign revoke command");
    capability_authority
        .revoke_signed(
            identity,
            SignedRevokeCapabilityRequest {
                command,
                signer: revoker.principal_id,
                signature: audit.signature,
            },
        )
        .expect("revoke capability");
}

fn complete_upstream_node(
    plan_authority: &SqlitePlanAuthority,
    task_authority: &SqliteTaskAuthority,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    key_base: u8,
) {
    let mat_key = IdempotencyKey::from_bytes([key_base; 16]);
    plan_authority
        .request_materialization(MaterializationRequest {
            plan_id,
            node_id,
            idempotency_key: mat_key,
            requested_at_ms: 1_500,
        })
        .expect("request upstream materialization");
    let other_declared = plan_authority
        .inspect_declared_task_node_count()
        .expect("declared count")
        .saturating_sub(1);
    let facts = task_authority
        .answer_plan_materialization(other_declared)
        .expect("task admits upstream");
    plan_authority
        .resolve_materialization(MaterializationResolution {
            request_key: mat_key,
            verdict: MaterializationAdmissionVerdict::Approved(MaterializationAdmission {
                profile_id: facts.profile_id.to_owned(),
                projected_task_nodes: facts.projected_task_nodes,
                projected_active_working_set: facts.projected_active_working_set,
            }),
            resolved_at_ms: 1_600,
        })
        .expect("resolve upstream materialization");

    plan_authority
        .record_node_transition(NodeTransitionRequest {
            plan_id,
            node_id,
            from_state: PlanNodeState::Materializing,
            to_state: PlanNodeState::Active,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([key_base.wrapping_add(1); 16]),
            transitioned_at_ms: 1_700,
        })
        .expect("upstream Materializing -> Active");
    plan_authority
        .record_node_transition(NodeTransitionRequest {
            plan_id,
            node_id,
            from_state: PlanNodeState::Active,
            to_state: PlanNodeState::Completed,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([key_base.wrapping_add(2); 16]),
            transitioned_at_ms: 1_800,
        })
        .expect("upstream Active -> Completed");
}

#[test]
fn seven_class_pinned_dependency_resolution_idempotency_restart_and_schema_isolation() {
    let fixture = TempFixtureDir::new("seven-class-pinned");
    let plan_db_path = fixture.path().join("plan-authority.sqlite3");
    let gate_dir = fixture.path().join("gate");
    let ns = NamespaceId::from_bytes([0x11; 16]);

    let receipt_ids: Vec<_> = {
        let plan_authority =
            SqlitePlanAuthority::open(&plan_db_path).expect("open SqlitePlanAuthority");
        let gate = SqliteEcosystemDependencyGate::open(&gate_dir).expect("open ecosystem gate");
        let source = RegistryEntitySource::new_with_seven_defaults();

        let rev = plan_authority
            .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
                plan_id: None,
                nodes: vec![conditioned_node(
                    0xA1,
                    Vec::new(),
                    Some(default_conditions(ns, 4)),
                )],
                idempotency_key: IdempotencyKey::from_bytes([0x01; 16]),
                applied_at_ms: 1_000,
            })
            .expect("apply plan revision")
            .receipt();
        let node_id = node_id_for_key(&plan_authority, rev.plan_id, [0xA1; 16]);

        let handles =
            bind_and_resolve_all_seven(&gate, &plan_authority, &source, rev.plan_id, node_id, 0x10);
        assert_eq!(handles.len(), 7);

        // Byte-identical idempotent replay returns `Replayed` with identical handle.
        let replay = gate
            .resolve_node_dependency(
                &plan_authority,
                &source,
                ResolveNodeDependencyRequest {
                    plan_id: rev.plan_id,
                    node_id,
                    kind: PlanDependencyEntityKind::Package,
                    entity_id: [1; 16],
                    idempotency_key: IdempotencyKey::from_bytes([0x11; 16]),
                    resolved_at_ms: 1_201,
                },
            )
            .expect("idempotent replay");
        assert_eq!(
            replay,
            PinnedDependencyDecision::Replayed(handles[0].clone())
        );

        // Replaying the same idempotency key with mutated timestamp/entity fails closed.
        let conflict = gate
            .resolve_node_dependency(
                &plan_authority,
                &source,
                ResolveNodeDependencyRequest {
                    plan_id: rev.plan_id,
                    node_id,
                    kind: PlanDependencyEntityKind::Package,
                    entity_id: [1; 16],
                    idempotency_key: IdempotencyKey::from_bytes([0x11; 16]),
                    resolved_at_ms: 9_999,
                },
            )
            .expect_err("parameter tampering must trigger IdempotencyConflict");
        assert!(matches!(conflict, EcosystemGateError::IdempotencyConflict));

        handles
            .into_iter()
            .map(|h| (rev.plan_id, node_id, h))
            .collect()
    };

    // Verify `SqlitePlanAuthority` stayed strictly at `SCHEMA_VERSION == 9` and
    // `ecosystem-dependency-gate.db` is at `ECOSYSTEM_GATE_SCHEMA_VERSION == 1` with immutable rows.
    {
        let plan_conn = Connection::open(&plan_db_path).expect("open raw plan sqlite");
        let plan_ver: i64 = plan_conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read plan user_version");
        assert_eq!(
            plan_ver, 9,
            "SqlitePlanAuthority schema must remain untouched at v9"
        );

        let gate_db_file = gate_dir.join(ECOSYSTEM_GATE_DB_FILENAME);
        let gate_conn = Connection::open(&gate_db_file).expect("open raw gate sqlite");
        let gate_ver: i64 = gate_conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read gate user_version");
        assert_eq!(gate_ver, ECOSYSTEM_GATE_SCHEMA_VERSION);

        assert!(
            gate_conn
                .execute(
                    "UPDATE pinned_ecosystem_dependency_handles SET pinned_generation = 99",
                    [],
                )
                .is_err(),
            "pinned_ecosystem_dependency_handles must reject UPDATE via trigger"
        );
        assert!(
            gate_conn
                .execute("DELETE FROM pinned_ecosystem_dependency_handles", [])
                .is_err(),
            "pinned_ecosystem_dependency_handles must reject DELETE via trigger"
        );
    }

    // Reopen after process drop and verify durable readback across all 7 entity classes.
    let reopened_gate = SqliteEcosystemDependencyGate::open(&gate_dir).expect("reopen gate");
    let (plan_id, node_id, _) = &receipt_ids[0];
    let listed = reopened_gate
        .list_node_pinned_handles(*plan_id, *node_id, 1)
        .expect("list pinned handles after reopen");
    assert_eq!(listed.len(), 7);
    for ((_, _, original), persisted) in receipt_ids.iter().zip(listed.iter()) {
        assert_eq!(persisted, original);
        let single = reopened_gate
            .inspect_pinned_handle(original.receipt_id)
            .expect("inspect single handle")
            .expect("handle exists");
        assert_eq!(&single, original);
    }
}

#[test]
fn floating_versions_and_unpinned_search_results_fail_closed_with_zero_durable_rows() {
    let fixture = TempFixtureDir::new("anti-floating-anti-search");
    let plan_authority =
        SqlitePlanAuthority::open(fixture.path().join("plan.sqlite3")).expect("open plan");
    let gate_dir = fixture.path().join("gate");
    let gate = SqliteEcosystemDependencyGate::open(&gate_dir).expect("open gate");
    let ns = NamespaceId::from_bytes([0x22; 16]);

    let rev = plan_authority
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: None,
            nodes: vec![conditioned_node(
                0xB1,
                Vec::new(),
                Some(default_conditions(ns, 2)),
            )],
            idempotency_key: IdempotencyKey::from_bytes([0x02; 16]),
            applied_at_ms: 1_000,
        })
        .expect("apply plan")
        .receipt();
    let node_id = node_id_for_key(&plan_authority, rev.plan_id, [0xB1; 16]);

    for (idx, bad_tag) in [
        "latest", "LATEST", "*", "head", "main", "canary", "nightly", "", "1.*", "2.x",
    ]
    .into_iter()
    .enumerate()
    {
        let key_byte = 0x30_u8.wrapping_add(u8::try_from(idx).expect("idx u8"));
        let err = gate
            .bind_node_dependency_proposals(
                &plan_authority,
                BindNodeDependenciesRequest {
                    plan_id: rev.plan_id,
                    node_id,
                    proposals: vec![TypedDependencySelectorProposal {
                        kind: PlanDependencyEntityKind::Package,
                        entity_id: [0x01; 16],
                        version_or_tag: SelectorVersionPin::ExactTag(bad_tag.to_owned()),
                        expected_content_digest: None,
                        provenance: SelectorProvenance::DeclaredPinnedManifest,
                    }],
                    idempotency_key: IdempotencyKey::from_bytes([key_byte; 16]),
                    bound_at_ms: 1_100,
                },
            )
            .expect_err("floating tag must fail closed");
        assert!(
            matches!(err, EcosystemGateError::FloatingVersionForbidden { .. }),
            "expected FloatingVersionForbidden for '{bad_tag}', got {err}"
        );
    }

    // `ExactGeneration(0)` is also forbidden.
    let gen_zero_err = gate
        .bind_node_dependency_proposals(
            &plan_authority,
            BindNodeDependenciesRequest {
                plan_id: rev.plan_id,
                node_id,
                proposals: vec![TypedDependencySelectorProposal {
                    kind: PlanDependencyEntityKind::Model,
                    entity_id: [0x04; 16],
                    version_or_tag: SelectorVersionPin::ExactGeneration(0),
                    expected_content_digest: None,
                    provenance: SelectorProvenance::DeclaredPinnedManifest,
                }],
                idempotency_key: IdempotencyKey::from_bytes([0x4E; 16]),
                bound_at_ms: 1_100,
            },
        )
        .expect_err("generation 0 must fail closed");
    assert!(matches!(
        gen_zero_err,
        EcosystemGateError::FloatingVersionForbidden { .. }
    ));

    // `SelectorProvenance::UnpinnedSearchResult` (`发现 != 授权`) must never pass directly.
    for (idx, kind) in PlanDependencyEntityKind::ALL.into_iter().enumerate() {
        let key_byte = 0x50_u8.wrapping_add(u8::try_from(idx).expect("idx u8"));
        let search_err = gate
            .bind_node_dependency_proposals(
                &plan_authority,
                BindNodeDependenciesRequest {
                    plan_id: rev.plan_id,
                    node_id,
                    proposals: vec![TypedDependencySelectorProposal {
                        kind,
                        entity_id: [kind.encode(); 16],
                        version_or_tag: SelectorVersionPin::ExactGeneration(3),
                        expected_content_digest: Some([0x99; 32]),
                        provenance: SelectorProvenance::UnpinnedSearchResult,
                    }],
                    idempotency_key: IdempotencyKey::from_bytes([key_byte; 16]),
                    bound_at_ms: 1_100,
                },
            )
            .expect_err("UnpinnedSearchResult must fail closed");
        assert!(matches!(
            search_err,
            EcosystemGateError::UnpinnedSearchResultForbidden { kind: k, .. } if k == kind
        ));
    }

    // Confirm zero durable rows were written across all rejected calls.
    let raw = Connection::open(gate_dir.join(ECOSYSTEM_GATE_DB_FILENAME)).expect("open raw gate");
    for table in [
        "node_dependency_binding_manifests",
        "node_declared_dependency_selectors",
        "pinned_ecosystem_dependency_handles",
    ] {
        let count: i64 = raw
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .expect("count rows");
        assert_eq!(
            count, 0,
            "rejected bindings must leave zero rows in {table}"
        );
    }
}

#[test]
fn pre_materialization_fence_blocks_unbound_unresolved_recalled_and_drifted_dependencies() {
    let fixture = TempFixtureDir::new("pre-mat-dependency-fence");
    let plan_authority =
        SqlitePlanAuthority::open(fixture.path().join("plan.sqlite3")).expect("open plan");
    let gate = SqliteEcosystemDependencyGate::open(fixture.path().join("gate")).expect("open gate");
    let task_authority = SqliteTaskAuthority::open_with_scale_profile(
        fixture.path().join("task.sqlite3"),
        &TASK_PROFILE_10K,
    )
    .expect("open task authority");
    let identity = IdentityAuthority::open(fixture.path().join("identity")).expect("open identity");
    let vault = SigningKeyCustodyVault::open_durable(fixture.path().join("custody"), [0x77; 32])
        .expect("open custody");
    let cap_auth =
        CapabilityAuthority::open(fixture.path().join("capability")).expect("open capability");

    let ns = NamespaceId::from_bytes([0x33; 16]);
    let operator = bootstrap_custody_operator(&vault, &identity, 0x21);
    let cap_handle =
        issue_namespace_capability(&vault, &cap_auth, &identity, &operator, &operator, ns, 0x22);
    let exec_gate = RealAuthorityExecutionGate {
        capability_authority: &cap_auth,
        capability_handle: Some(cap_handle),
        allowed_namespaces: vec![ns],
        resource_ceiling: ResourceContractCondition {
            cpu_shares: 4_000,
            memory_mib: 4_096,
            io_weight: 500,
        },
        payer_principal: Some(operator.principal_id),
        payer_max_fanout_budget: 8,
    };
    let task_consult = TaskAuthorityAdmission(&task_authority);
    let source = RegistryEntitySource::new_with_seven_defaults();

    let rev = plan_authority
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: None,
            nodes: vec![conditioned_node(
                0xC1,
                Vec::new(),
                Some(default_conditions(ns, 2)),
            )],
            idempotency_key: IdempotencyKey::from_bytes([0x03; 16]),
            applied_at_ms: 1_000,
        })
        .expect("apply plan")
        .receipt();
    let node_id = node_id_for_key(&plan_authority, rev.plan_id, [0xC1; 16]);

    // 1. Unbound node dependencies -> `NodeDependenciesUnbound`, node stays `Declared`.
    let unbound_err = gate
        .advance_and_authorize_materialization(
            &plan_authority,
            &source,
            &exec_gate,
            &task_consult,
            AuthorizeNodeMaterializationRequest {
                plan_id: rev.plan_id,
                node_id,
                idempotency_key: IdempotencyKey::from_bytes([0x61; 16]),
                at_ms: 2_000,
            },
        )
        .expect_err("unbound node dependencies must fail closed");
    assert!(matches!(
        unbound_err,
        EcosystemGateError::NodeDependenciesUnbound { .. }
    ));
    assert_eq!(
        plan_authority
            .inspect_node(rev.plan_id, node_id)
            .expect("inspect")
            .expect("node")
            .state,
        PlanNodeState::Declared
    );

    // 2. Bind 2 dependencies (`Package` and `Model`), resolve only `Package` -> `UnresolvedEcosystemDependencies`.
    gate.bind_node_dependency_proposals(
        &plan_authority,
        BindNodeDependenciesRequest {
            plan_id: rev.plan_id,
            node_id,
            proposals: vec![
                TypedDependencySelectorProposal {
                    kind: PlanDependencyEntityKind::Package,
                    entity_id: [1; 16],
                    version_or_tag: SelectorVersionPin::ExactTag("v1.1.0".to_owned()),
                    expected_content_digest: Some([0x41; 32]),
                    provenance: SelectorProvenance::DeclaredPinnedManifest,
                },
                TypedDependencySelectorProposal {
                    kind: PlanDependencyEntityKind::Model,
                    entity_id: [4; 16],
                    version_or_tag: SelectorVersionPin::ExactGeneration(4),
                    expected_content_digest: Some([0x44; 32]),
                    provenance: SelectorProvenance::TrustedUserSelection,
                },
            ],
            idempotency_key: IdempotencyKey::from_bytes([0x62; 16]),
            bound_at_ms: 2_050,
        },
    )
    .expect("bind 2 selectors");

    gate.resolve_node_dependency(
        &plan_authority,
        &source,
        ResolveNodeDependencyRequest {
            plan_id: rev.plan_id,
            node_id,
            kind: PlanDependencyEntityKind::Package,
            entity_id: [1; 16],
            idempotency_key: IdempotencyKey::from_bytes([0x63; 16]),
            resolved_at_ms: 2_100,
        },
    )
    .expect("resolve Package");

    let unresolved_err = gate
        .advance_and_authorize_materialization(
            &plan_authority,
            &source,
            &exec_gate,
            &task_consult,
            AuthorizeNodeMaterializationRequest {
                plan_id: rev.plan_id,
                node_id,
                idempotency_key: IdempotencyKey::from_bytes([0x64; 16]),
                at_ms: 2_150,
            },
        )
        .expect_err("partially unresolved ecosystem dependencies must fail closed");
    assert!(matches!(
        unresolved_err,
        EcosystemGateError::UnresolvedEcosystemDependencies {
            unresolved_count: 1,
            ..
        }
    ));

    // Resolve `Model` as well, then inject upstream store recall on `Package` before materializing!
    gate.resolve_node_dependency(
        &plan_authority,
        &source,
        ResolveNodeDependencyRequest {
            plan_id: rev.plan_id,
            node_id,
            kind: PlanDependencyEntityKind::Model,
            entity_id: [4; 16],
            idempotency_key: IdempotencyKey::from_bytes([0x65; 16]),
            resolved_at_ms: 2_200,
        },
    )
    .expect("resolve Model");

    // 3. Upstream recalls `Package` -> `PinnedDependencyRevokedOrRecalled`.
    source.upsert(
        PlanDependencyEntityKind::Package,
        [1; 16],
        CrossEcosystemLookup::RecalledOrRevoked {
            reason: "emergency CVE store recall".to_owned(),
        },
    );
    let recalled_err = gate
        .advance_and_authorize_materialization(
            &plan_authority,
            &source,
            &exec_gate,
            &task_consult,
            AuthorizeNodeMaterializationRequest {
                plan_id: rev.plan_id,
                node_id,
                idempotency_key: IdempotencyKey::from_bytes([0x66; 16]),
                at_ms: 2_250,
            },
        )
        .expect_err("recalled pinned dependency must block materialization");
    assert!(matches!(
        recalled_err,
        EcosystemGateError::PinnedDependencyRevokedOrRecalled {
            kind: PlanDependencyEntityKind::Package,
            ..
        }
    ));

    // 4. Restore `Package`, inject silent generation drift on `Model` -> `PinnedDependencyFenceDrifted`.
    source.upsert(
        PlanDependencyEntityKind::Package,
        [1; 16],
        CrossEcosystemLookup::Found(EcosystemEntitySnapshot {
            pinned_version_tag: "v1.1.0".to_owned(),
            pinned_generation: 1,
            content_digest: [0x41; 32],
        }),
    );
    source.upsert(
        PlanDependencyEntityKind::Model,
        [4; 16],
        CrossEcosystemLookup::Found(EcosystemEntitySnapshot {
            pinned_version_tag: "v1.4.0".to_owned(),
            pinned_generation: 5,
            content_digest: [0x44; 32],
        }),
    );
    let drifted_err = gate
        .advance_and_authorize_materialization(
            &plan_authority,
            &source,
            &exec_gate,
            &task_consult,
            AuthorizeNodeMaterializationRequest {
                plan_id: rev.plan_id,
                node_id,
                idempotency_key: IdempotencyKey::from_bytes([0x67; 16]),
                at_ms: 2_300,
            },
        )
        .expect_err("generation drift must block materialization");
    assert!(matches!(
        drifted_err,
        EcosystemGateError::PinnedDependencyFenceDrifted {
            kind: PlanDependencyEntityKind::Model,
            pinned_generation: 4,
            live_generation: 5,
            ..
        }
    ));
    assert_eq!(
        plan_authority
            .inspect_node(rev.plan_id, node_id)
            .expect("inspect")
            .expect("node")
            .state,
        PlanNodeState::Declared
    );
}

#[test]
fn upstream_dag_unready_blocks_in_blocked_dependency_and_missing_conditions_fail_closed() {
    let fixture = TempFixtureDir::new("dag-unready-and-unconditioned");
    let plan_authority =
        SqlitePlanAuthority::open(fixture.path().join("plan.sqlite3")).expect("open plan");
    let gate = SqliteEcosystemDependencyGate::open(fixture.path().join("gate")).expect("open gate");
    let task_authority = SqliteTaskAuthority::open_with_scale_profile(
        fixture.path().join("task.sqlite3"),
        &TASK_PROFILE_10K,
    )
    .expect("open task");
    let identity = IdentityAuthority::open(fixture.path().join("identity")).expect("open identity");
    let vault = SigningKeyCustodyVault::open_durable(fixture.path().join("custody"), [0x88; 32])
        .expect("open custody");
    let cap_auth =
        CapabilityAuthority::open(fixture.path().join("capability")).expect("open capability");

    let ns = NamespaceId::from_bytes([0x44; 16]);
    let operator = bootstrap_custody_operator(&vault, &identity, 0x31);
    let cap_handle =
        issue_namespace_capability(&vault, &cap_auth, &identity, &operator, &operator, ns, 0x32);
    let exec_gate = RealAuthorityExecutionGate {
        capability_authority: &cap_auth,
        capability_handle: Some(cap_handle),
        allowed_namespaces: vec![ns],
        resource_ceiling: ResourceContractCondition {
            cpu_shares: 4_000,
            memory_mib: 4_096,
            io_weight: 500,
        },
        payer_principal: Some(operator.principal_id),
        payer_max_fanout_budget: 8,
    };
    let task_consult = TaskAuthorityAdmission(&task_authority);
    let source = RegistryEntitySource::new_with_seven_defaults();

    // Node A (upstream), Node B (depends on A, conditioned), Node C (no dependency, `conditions: None`).
    let rev = plan_authority
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: None,
            nodes: vec![
                conditioned_node(0xD1, Vec::new(), Some(default_conditions(ns, 2))),
                conditioned_node(0xD2, vec![[0xD1; 16]], Some(default_conditions(ns, 2))),
                conditioned_node(0xD3, Vec::new(), None),
            ],
            idempotency_key: IdempotencyKey::from_bytes([0x04; 16]),
            applied_at_ms: 1_000,
        })
        .expect("apply plan")
        .receipt();
    let node_b = node_id_for_key(&plan_authority, rev.plan_id, [0xD2; 16]);
    let node_c = node_id_for_key(&plan_authority, rev.plan_id, [0xD3; 16]);

    bind_and_resolve_all_seven(&gate, &plan_authority, &source, rev.plan_id, node_b, 0x70);
    bind_and_resolve_all_seven(&gate, &plan_authority, &source, rev.plan_id, node_c, 0x80);

    // Node B's upstream A is still `Declared` -> B transitions `Declared -> BlockedDependency` and fails closed.
    let unready_err = gate
        .advance_and_authorize_materialization(
            &plan_authority,
            &source,
            &exec_gate,
            &task_consult,
            AuthorizeNodeMaterializationRequest {
                plan_id: rev.plan_id,
                node_id: node_b,
                idempotency_key: IdempotencyKey::from_bytes([0x91; 16]),
                at_ms: 2_000,
            },
        )
        .expect_err("unready upstream DAG dependency must block");
    assert!(matches!(
        unready_err,
        EcosystemGateError::Plan(PlanStoreError::DependenciesNotReady { .. })
    ));
    assert_eq!(
        plan_authority
            .inspect_node(rev.plan_id, node_b)
            .expect("inspect B")
            .expect("B")
            .state,
        PlanNodeState::BlockedDependency
    );

    // Node C lacks structured `NodeConditions` -> fails closed before entering `WaitingAuthorization`.
    let uncond_err = gate
        .advance_and_authorize_materialization(
            &plan_authority,
            &source,
            &exec_gate,
            &task_consult,
            AuthorizeNodeMaterializationRequest {
                plan_id: rev.plan_id,
                node_id: node_c,
                idempotency_key: IdempotencyKey::from_bytes([0x92; 16]),
                at_ms: 2_050,
            },
        )
        .expect_err("unconditioned node must fail closed");
    assert!(matches!(
        uncond_err,
        EcosystemGateError::MissingStructuredNodeConditions { .. }
    ));
}

#[test]
fn real_capability_authority_revocation_holds_node_in_waiting_authorization() {
    let fixture = TempFixtureDir::new("real-capability-revoked");
    let plan_authority =
        SqlitePlanAuthority::open(fixture.path().join("plan.sqlite3")).expect("open plan");
    let gate = SqliteEcosystemDependencyGate::open(fixture.path().join("gate")).expect("open gate");
    let task_authority = SqliteTaskAuthority::open_with_scale_profile(
        fixture.path().join("task.sqlite3"),
        &TASK_PROFILE_10K,
    )
    .expect("open task");
    let identity = IdentityAuthority::open(fixture.path().join("identity")).expect("open identity");
    let vault = SigningKeyCustodyVault::open_durable(fixture.path().join("custody"), [0x99; 32])
        .expect("open custody");
    let cap_auth =
        CapabilityAuthority::open(fixture.path().join("capability")).expect("open capability");

    let ns = NamespaceId::from_bytes([0x55; 16]);
    let operator = bootstrap_custody_operator(&vault, &identity, 0x41);
    let revoked_cap =
        issue_namespace_capability(&vault, &cap_auth, &identity, &operator, &operator, ns, 0x42);

    // Revoke the capability in real `CapabilityAuthority` before materialization evaluation!
    revoke_capability_under_custody(
        &vault,
        &cap_auth,
        &identity,
        &operator,
        revoked_cap,
        0x43,
        1_500,
    );

    let rev = plan_authority
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: None,
            nodes: vec![conditioned_node(
                0xE1,
                Vec::new(),
                Some(default_conditions(ns, 2)),
            )],
            idempotency_key: IdempotencyKey::from_bytes([0x05; 16]),
            applied_at_ms: 1_000,
        })
        .expect("apply plan")
        .receipt();
    let node_id = node_id_for_key(&plan_authority, rev.plan_id, [0xE1; 16]);
    let source = RegistryEntitySource::new_with_seven_defaults();
    bind_and_resolve_all_seven(&gate, &plan_authority, &source, rev.plan_id, node_id, 0xA0);

    let exec_gate = RealAuthorityExecutionGate {
        capability_authority: &cap_auth,
        capability_handle: Some(revoked_cap),
        allowed_namespaces: vec![ns],
        resource_ceiling: ResourceContractCondition {
            cpu_shares: 4_000,
            memory_mib: 4_096,
            io_weight: 500,
        },
        payer_principal: Some(operator.principal_id),
        payer_max_fanout_budget: 8,
    };
    let task_consult = TaskAuthorityAdmission(&task_authority);

    let decision = gate
        .advance_and_authorize_materialization(
            &plan_authority,
            &source,
            &exec_gate,
            &task_consult,
            AuthorizeNodeMaterializationRequest {
                plan_id: rev.plan_id,
                node_id,
                idempotency_key: IdempotencyKey::from_bytes([0xB1; 16]),
                at_ms: 2_000,
            },
        )
        .expect("denied authorization returns durable receipt, not storage failure");

    let ConditionedMaterializationDecision::DeniedInWaitingAuthorization(receipt) = decision else {
        panic!("expected DeniedInWaitingAuthorization, got {decision:?}");
    };
    assert!(matches!(
        receipt.outcome,
        ConditionedGateOutcome::Denied(ConditionedGateDenialReason::AuthorizationDenied { .. })
    ));

    // Node MUST remain in `PlanNodeState::WaitingAuthorization` (NOT `Materializing`) with zero vouchers.
    let node_record = plan_authority
        .inspect_node(rev.plan_id, node_id)
        .expect("inspect node")
        .expect("node exists");
    assert_eq!(node_record.state, PlanNodeState::WaitingAuthorization);
    assert_eq!(
        plan_authority
            .inspect_node_materialization_requests(rev.plan_id, node_id)
            .expect("list materialization requests")
            .len(),
        0
    );

    // Byte-identical replay returns `ConditionedMaterializationDecision::Replayed`.
    let replay = gate
        .advance_and_authorize_materialization(
            &plan_authority,
            &source,
            &exec_gate,
            &task_consult,
            AuthorizeNodeMaterializationRequest {
                plan_id: rev.plan_id,
                node_id,
                idempotency_key: IdempotencyKey::from_bytes([0xB1; 16]),
                at_ms: 2_000,
            },
        )
        .expect("idempotent replay of denial");
    assert_eq!(
        replay,
        ConditionedMaterializationDecision::Replayed(receipt)
    );
}

#[test]
fn namespace_resource_contract_fanout_and_task_gates_each_hold_node_in_waiting_authorization() {
    let fixture = TempFixtureDir::new("four-condition-denials");
    let plan_authority =
        SqlitePlanAuthority::open(fixture.path().join("plan.sqlite3")).expect("open plan");
    let gate = SqliteEcosystemDependencyGate::open(fixture.path().join("gate")).expect("open gate");
    let task_ok = SqliteTaskAuthority::open_with_scale_profile(
        fixture.path().join("task-ok.sqlite3"),
        &TASK_PROFILE_10K,
    )
    .expect("open task 10k");
    let task_zero = SqliteTaskAuthority::open_with_scale_profile(
        fixture.path().join("task-zero.sqlite3"),
        &ZERO_WORKING_SET,
    )
    .expect("open task zero working set");
    let identity = IdentityAuthority::open(fixture.path().join("identity")).expect("open identity");
    let vault = SigningKeyCustodyVault::open_durable(fixture.path().join("custody"), [0xAA; 32])
        .expect("open custody");
    let cap_auth =
        CapabilityAuthority::open(fixture.path().join("capability")).expect("open capability");

    let allowed_ns = NamespaceId::from_bytes([0x61; 16]);
    let forbidden_ns = NamespaceId::from_bytes([0x62; 16]);
    let operator = bootstrap_custody_operator(&vault, &identity, 0x51);
    let active_cap = issue_namespace_capability(
        &vault, &cap_auth, &identity, &operator, &operator, allowed_ns, 0x52,
    );
    let source = RegistryEntitySource::new_with_seven_defaults();

    // Declare 5 test nodes (+ 3 downstream fanout children depending on `node_fanout_dag`):
    // - `0x11` (`node_ns`): requests `[allowed_ns, forbidden_ns]`
    // - `0x12` (`node_res`): requests excessive `ResourceContractCondition`
    // - `0x13` (`node_fanout_dag`): declares `max_downstream_fanout = 2`, but 3 children (`0x21..=0x23`) depend on it!
    // - `0x14` (`node_fanout_payer`): DAG fanout = 0, `max_downstream_fanout = 2`, tested with missing fee payer
    // - `0x15` (`node_task_cap`): passes all 4 G3 condition gates, denied by `ZERO_WORKING_SET` Task authority
    let over_quota_resources = ResourceContractCondition {
        cpu_shares: 8_000,
        memory_mib: 16_384,
        io_weight: 900,
    };
    let rev = plan_authority
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: None,
            nodes: vec![
                conditioned_node(
                    0x11,
                    Vec::new(),
                    Some(NodeConditions {
                        namespace: NamespaceCondition {
                            namespaces: vec![allowed_ns, forbidden_ns],
                        },
                        ..default_conditions(allowed_ns, 2)
                    }),
                ),
                conditioned_node(
                    0x12,
                    Vec::new(),
                    Some(NodeConditions {
                        resource_contract: over_quota_resources,
                        ..default_conditions(allowed_ns, 2)
                    }),
                ),
                conditioned_node(0x13, Vec::new(), Some(default_conditions(allowed_ns, 2))),
                conditioned_node(
                    0x21,
                    vec![[0x13; 16]],
                    Some(default_conditions(allowed_ns, 1)),
                ),
                conditioned_node(
                    0x22,
                    vec![[0x13; 16]],
                    Some(default_conditions(allowed_ns, 1)),
                ),
                conditioned_node(
                    0x23,
                    vec![[0x13; 16]],
                    Some(default_conditions(allowed_ns, 1)),
                ),
                conditioned_node(0x14, Vec::new(), Some(default_conditions(allowed_ns, 2))),
                conditioned_node(0x15, Vec::new(), Some(default_conditions(allowed_ns, 2))),
            ],
            idempotency_key: IdempotencyKey::from_bytes([0x06; 16]),
            applied_at_ms: 1_000,
        })
        .expect("apply plan")
        .receipt();

    let node_ns = node_id_for_key(&plan_authority, rev.plan_id, [0x11; 16]);
    let node_res = node_id_for_key(&plan_authority, rev.plan_id, [0x12; 16]);
    let node_fanout_dag = node_id_for_key(&plan_authority, rev.plan_id, [0x13; 16]);
    let node_fanout_payer = node_id_for_key(&plan_authority, rev.plan_id, [0x14; 16]);
    let node_task_cap = node_id_for_key(&plan_authority, rev.plan_id, [0x15; 16]);

    bind_and_resolve_all_seven(&gate, &plan_authority, &source, rev.plan_id, node_ns, 0x10);
    bind_and_resolve_all_seven(&gate, &plan_authority, &source, rev.plan_id, node_res, 0x20);
    bind_and_resolve_all_seven(
        &gate,
        &plan_authority,
        &source,
        rev.plan_id,
        node_fanout_dag,
        0x30,
    );
    bind_and_resolve_all_seven(
        &gate,
        &plan_authority,
        &source,
        rev.plan_id,
        node_fanout_payer,
        0x40,
    );
    bind_and_resolve_all_seven(
        &gate,
        &plan_authority,
        &source,
        rev.plan_id,
        node_task_cap,
        0x50,
    );

    let standard_gate = RealAuthorityExecutionGate {
        capability_authority: &cap_auth,
        capability_handle: Some(active_cap),
        allowed_namespaces: vec![allowed_ns],
        resource_ceiling: ResourceContractCondition {
            cpu_shares: 4_000,
            memory_mib: 4_096,
            io_weight: 500,
        },
        payer_principal: Some(operator.principal_id),
        payer_max_fanout_budget: 8,
    };
    let task_ok_consult = TaskAuthorityAdmission(&task_ok);

    // (a) `NamespaceCondition` out of scope -> held in `WaitingAuthorization`.
    let ns_dec = gate
        .advance_and_authorize_materialization(
            &plan_authority,
            &source,
            &standard_gate,
            &task_ok_consult,
            AuthorizeNodeMaterializationRequest {
                plan_id: rev.plan_id,
                node_id: node_ns,
                idempotency_key: IdempotencyKey::from_bytes([0xC1; 16]),
                at_ms: 2_000,
            },
        )
        .expect("namespace gate evaluation");
    assert!(matches!(
        ns_dec.audit_receipt().outcome,
        ConditionedGateOutcome::Denied(ConditionedGateDenialReason::NamespaceOutOfScope {
            denied_namespace,
            ..
        }) if denied_namespace == forbidden_ns
    ));
    assert_eq!(
        plan_authority
            .inspect_node(rev.plan_id, node_ns)
            .expect("inspect")
            .expect("node")
            .state,
        PlanNodeState::WaitingAuthorization
    );

    // (b) `ResourceContractCondition` exceeded -> held in `WaitingAuthorization`.
    let res_dec = gate
        .advance_and_authorize_materialization(
            &plan_authority,
            &source,
            &standard_gate,
            &task_ok_consult,
            AuthorizeNodeMaterializationRequest {
                plan_id: rev.plan_id,
                node_id: node_res,
                idempotency_key: IdempotencyKey::from_bytes([0xC2; 16]),
                at_ms: 2_100,
            },
        )
        .expect("resource gate evaluation");
    assert!(matches!(
        res_dec.audit_receipt().outcome,
        ConditionedGateOutcome::Denied(ConditionedGateDenialReason::ResourceContractExceeded {
            requested,
            ..
        }) if requested == over_quota_resources
    ));
    assert_eq!(
        plan_authority
            .inspect_node(rev.plan_id, node_res)
            .expect("inspect")
            .expect("node")
            .state,
        PlanNodeState::WaitingAuthorization
    );

    // (c1) `FanoutCondition`: DAG has 3 downstream dependents > `declared_max_fanout = 2` -> held in `WaitingAuthorization`.
    let fanout_dag_dec = gate
        .advance_and_authorize_materialization(
            &plan_authority,
            &source,
            &standard_gate,
            &task_ok_consult,
            AuthorizeNodeMaterializationRequest {
                plan_id: rev.plan_id,
                node_id: node_fanout_dag,
                idempotency_key: IdempotencyKey::from_bytes([0xC3; 16]),
                at_ms: 2_200,
            },
        )
        .expect("fanout DAG gate evaluation");
    assert!(matches!(
        fanout_dag_dec.audit_receipt().outcome,
        ConditionedGateOutcome::Denied(ConditionedGateDenialReason::FanoutOrPayerDenied {
            declared_max_fanout: 2,
            dag_downstream_fanout: 3,
            effective_downstream_fanout: 3,
            ..
        })
    ));
    assert_eq!(
        plan_authority
            .inspect_node(rev.plan_id, node_fanout_dag)
            .expect("inspect")
            .expect("node")
            .state,
        PlanNodeState::WaitingAuthorization
    );

    // (c2) `FanoutCondition`: missing fee payer -> held in `WaitingAuthorization`.
    let no_payer_gate = RealAuthorityExecutionGate {
        payer_principal: None,
        ..standard_gate.clone()
    };
    let fanout_payer_dec = gate
        .advance_and_authorize_materialization(
            &plan_authority,
            &source,
            &no_payer_gate,
            &task_ok_consult,
            AuthorizeNodeMaterializationRequest {
                plan_id: rev.plan_id,
                node_id: node_fanout_payer,
                idempotency_key: IdempotencyKey::from_bytes([0xC4; 16]),
                at_ms: 2_300,
            },
        )
        .expect("fanout payer gate evaluation");
    assert!(matches!(
        fanout_payer_dec.audit_receipt().outcome,
        ConditionedGateOutcome::Denied(ConditionedGateDenialReason::FanoutOrPayerDenied {
            declared_max_fanout: 2,
            dag_downstream_fanout: 0,
            effective_downstream_fanout: 2,
            ..
        })
    ));
    assert_eq!(
        plan_authority
            .inspect_node(rev.plan_id, node_fanout_payer)
            .expect("inspect")
            .expect("node")
            .state,
        PlanNodeState::WaitingAuthorization
    );

    // (d) Task `ScaleProfile` zero-working-set denial -> held in `WaitingAuthorization` with `TaskMaterializationRejected`.
    let task_zero_consult = TaskAuthorityAdmission(&task_zero);
    let task_dec = gate
        .advance_and_authorize_materialization(
            &plan_authority,
            &source,
            &standard_gate,
            &task_zero_consult,
            AuthorizeNodeMaterializationRequest {
                plan_id: rev.plan_id,
                node_id: node_task_cap,
                idempotency_key: IdempotencyKey::from_bytes([0xC5; 16]),
                at_ms: 2_400,
            },
        )
        .expect("task zero working-set evaluation");
    assert!(matches!(
        task_dec.audit_receipt().outcome,
        ConditionedGateOutcome::Denied(ConditionedGateDenialReason::TaskMaterializationRejected(
            MaterializationRejection::WorkingSetFull { .. }
        ))
    ));
    assert_eq!(
        plan_authority
            .inspect_node(rev.plan_id, node_task_cap)
            .expect("inspect")
            .expect("node")
            .state,
        PlanNodeState::WaitingAuthorization
    );
}

#[test]
fn full_green_conditioned_gate_transitions_waiting_authorization_to_materializing_and_survives_restart()
 {
    let fixture = TempFixtureDir::new("full-green-materializing");
    let plan_db_path = fixture.path().join("plan.sqlite3");
    let gate_dir = fixture.path().join("gate");
    let plan_authority = SqlitePlanAuthority::open(&plan_db_path).expect("open plan");
    let gate = SqliteEcosystemDependencyGate::open(&gate_dir).expect("open gate");
    let task_authority = SqliteTaskAuthority::open_with_scale_profile(
        fixture.path().join("task.sqlite3"),
        &TASK_PROFILE_10K,
    )
    .expect("open task");
    let identity = IdentityAuthority::open(fixture.path().join("identity")).expect("open identity");
    let vault = SigningKeyCustodyVault::open_durable(fixture.path().join("custody"), [0xBB; 32])
        .expect("open custody");
    let cap_auth =
        CapabilityAuthority::open(fixture.path().join("capability")).expect("open capability");

    let ns = NamespaceId::from_bytes([0x77; 16]);
    let operator = bootstrap_custody_operator(&vault, &identity, 0x61);
    let source = RegistryEntitySource::new_with_seven_defaults();

    // DAG: upstream `0xF1` -> target `0xF2` (`max_downstream_fanout = 2`) -> 2 downstream children `0xF3` & `0xF4`.
    let rev = plan_authority
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: None,
            nodes: vec![
                conditioned_node(0xF1, Vec::new(), Some(default_conditions(ns, 1))),
                conditioned_node(0xF2, vec![[0xF1; 16]], Some(default_conditions(ns, 2))),
                conditioned_node(0xF3, vec![[0xF2; 16]], Some(default_conditions(ns, 1))),
                conditioned_node(0xF4, vec![[0xF2; 16]], Some(default_conditions(ns, 1))),
            ],
            idempotency_key: IdempotencyKey::from_bytes([0x07; 16]),
            applied_at_ms: 1_000,
        })
        .expect("apply plan")
        .receipt();

    let node_upstream = node_id_for_key(&plan_authority, rev.plan_id, [0xF1; 16]);
    let node_target = node_id_for_key(&plan_authority, rev.plan_id, [0xF2; 16]);
    bind_and_resolve_all_seven(
        &gate,
        &plan_authority,
        &source,
        rev.plan_id,
        node_target,
        0xD0,
    );

    // Complete upstream `0xF1` so `node_target`'s DAG dependency readiness holds.
    complete_upstream_node(
        &plan_authority,
        &task_authority,
        rev.plan_id,
        node_upstream,
        0xE0,
    );

    let task_consult = TaskAuthorityAdmission(&task_authority);

    // Round 1: Attempt without capability -> node enters and waits in `PlanNodeState::WaitingAuthorization`.
    let unauth_gate = RealAuthorityExecutionGate {
        capability_authority: &cap_auth,
        capability_handle: None,
        allowed_namespaces: vec![ns],
        resource_ceiling: ResourceContractCondition {
            cpu_shares: 4_000,
            memory_mib: 4_096,
            io_weight: 500,
        },
        payer_principal: Some(operator.principal_id),
        payer_max_fanout_budget: 4,
    };
    let round1 = gate
        .advance_and_authorize_materialization(
            &plan_authority,
            &source,
            &unauth_gate,
            &task_consult,
            AuthorizeNodeMaterializationRequest {
                plan_id: rev.plan_id,
                node_id: node_target,
                idempotency_key: IdempotencyKey::from_bytes([0xF1; 16]),
                at_ms: 2_500,
            },
        )
        .expect("round 1 waits in WaitingAuthorization");
    assert!(matches!(
        round1,
        ConditionedMaterializationDecision::DeniedInWaitingAuthorization(_)
    ));
    assert_eq!(
        plan_authority
            .inspect_node(rev.plan_id, node_target)
            .expect("inspect")
            .expect("target")
            .state,
        PlanNodeState::WaitingAuthorization
    );

    // Round 2: Operator issues fresh active root capability in `CapabilityAuthority`; all 7 pinned
    // dependencies + Capability + Namespace + ResourceContract + Fanout (2 == 2) + Task 10K pass!
    let fresh_cap =
        issue_namespace_capability(&vault, &cap_auth, &identity, &operator, &operator, ns, 0x62);
    let authorized_gate = RealAuthorityExecutionGate {
        capability_handle: Some(fresh_cap),
        ..unauth_gate
    };

    let round2 = gate
        .advance_and_authorize_materialization(
            &plan_authority,
            &source,
            &authorized_gate,
            &task_consult,
            AuthorizeNodeMaterializationRequest {
                plan_id: rev.plan_id,
                node_id: node_target,
                idempotency_key: IdempotencyKey::from_bytes([0xF2; 16]),
                at_ms: 2_600,
            },
        )
        .expect("round 2 approves WaitingAuthorization -> Materializing");

    let ConditionedMaterializationDecision::Materializing {
        approval,
        audit_receipt,
    } = round2
    else {
        panic!("expected Materializing decision, got {round2:?}");
    };

    assert_eq!(
        approval.voucher.from_state,
        PlanNodeState::WaitingAuthorization
    );
    assert_eq!(approval.voucher.to_state, PlanNodeState::Materializing);
    assert_eq!(audit_receipt.dag_downstream_fanout, 2);
    assert_eq!(audit_receipt.effective_downstream_fanout, 2);
    assert_eq!(
        audit_receipt.outcome,
        ConditionedGateOutcome::Approved {
            materialization_voucher_id: approval.voucher.voucher_id,
            capability_or_grant_id: *fresh_cap.capability_id.as_bytes(),
            payer_principal_id: operator.principal_id,
        }
    );

    // Confirm durable state in `SqlitePlanAuthority` is now `Materializing` with 1 approved request.
    let target_record = plan_authority
        .inspect_node(rev.plan_id, node_target)
        .expect("inspect target")
        .expect("target exists");
    assert_eq!(target_record.state, PlanNodeState::Materializing);
    let mat_requests = plan_authority
        .inspect_node_materialization_requests(rev.plan_id, node_target)
        .expect("inspect materialization requests");
    assert_eq!(mat_requests.len(), 1);
    assert_eq!(
        mat_requests[0].approved_voucher_id,
        Some(approval.voucher.voucher_id)
    );

    // Byte-identical replay of Round 2 returns `ConditionedMaterializationDecision::Replayed`.
    let replay = gate
        .advance_and_authorize_materialization(
            &plan_authority,
            &source,
            &authorized_gate,
            &task_consult,
            AuthorizeNodeMaterializationRequest {
                plan_id: rev.plan_id,
                node_id: node_target,
                idempotency_key: IdempotencyKey::from_bytes([0xF2; 16]),
                at_ms: 2_600,
            },
        )
        .expect("replay approved round");
    assert_eq!(
        replay,
        ConditionedMaterializationDecision::Replayed(audit_receipt.clone())
    );

    // Close and reopen `SqliteEcosystemDependencyGate`: verify complete chronological audit trail
    // and raw SQLite trigger immutability on `conditioned_authorization_audit_receipts`.
    drop(gate);
    let reopened = SqliteEcosystemDependencyGate::open(&gate_dir).expect("reopen gate");
    let trail = reopened
        .list_node_authorization_audit_receipts(rev.plan_id, node_target)
        .expect("list audit trail");
    assert_eq!(trail.len(), 2);
    assert!(matches!(
        trail[0].outcome,
        ConditionedGateOutcome::Denied(ConditionedGateDenialReason::AuthorizationDenied { .. })
    ));
    assert_eq!(trail[1], audit_receipt);

    let raw_gate =
        Connection::open(gate_dir.join(ECOSYSTEM_GATE_DB_FILENAME)).expect("open raw gate");
    assert!(
        raw_gate
            .execute(
                "UPDATE conditioned_authorization_audit_receipts SET outcome_code = 1",
                [],
            )
            .is_err(),
        "audit receipts must reject UPDATE via trigger"
    );
    assert!(
        raw_gate
            .execute("DELETE FROM conditioned_authorization_audit_receipts", [])
            .is_err(),
        "audit receipts must reject DELETE via trigger"
    );
}
