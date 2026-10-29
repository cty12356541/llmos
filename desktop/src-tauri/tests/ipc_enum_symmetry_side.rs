//! Stage D (`D-IPC-ENUM` / `IPC-D1..D8` & `[UI-PARITY-001]`) cross-platform
//! end-to-end integration & four-entry receipt byte-symmetry suite.
//!
//! Runs on both Windows (Named Pipes) and Unix (Unix Domain Sockets) under
//! `--features dev-fixture`, proving:
//! 1. All 12 domains `List* + Inspect*` bounded cursor pagination (`limit <= 64`,
//!    `<= 128` for watermark events), `[PLAN-LAZY-001]` collapsed counts,
//!    Settings (`system/user/workspace/application`), `ContentTypeAssociation`,
//!    zero-secret `KeychainReference`, `ChooserGrant`, `Cell` topology &
//!    `Quarantined` `ExclusiveDevice` lease, and `NOT_FOUND` vs
//!    `STALE_GENERATION` carrying `current_generation`.
//! 2. `inspect_search` (`0x1F` verified + fail-closed rejection on tampered
//!    restriction bits), `inspect_notification`, `disable-application`, and
//!    `uninstall-application` end-to-end through `llmos_desktop_lib::ipc`.
//! 3. `PreviewControlBlastRadius` (`+ / ~ / -` descendant impacts + statutory
//!    lock dry-run surfacing) and `QueryControlCommandStatus`
//!    (`[CTRL-RECOVERY-001]` idempotent reconciliation).
//! 4. Deterministic protocol-level rejection of all **Five Statutory
//!    Non-Tamperable Locks** (`[UI-PARITY-001]`).
//! 5. Four-entry (`TRUSTED_GUI`, `NL_SHELL`, `CLI` subprocess, `STRUCTURED_API`)
//!    byte-identical `ControlReceipt::to_bytes` (`receipt_hex`) parity across
//!    both Stage D reads and explicit `ControlScope` / domain mutations.

#![cfg(all(any(unix, windows), feature = "dev-fixture"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use ed25519_dalek::SigningKey;
use nlos_clock::{AuthorityClock, NowRequest};
use nlos_commit_coordinator::{RecoveryWorkerHealth, RecoveryWorkerState};
use nlos_identity::{BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_ipc::handshake::transport::ServerHandshakeContext;
#[cfg(unix)]
use nlos_ipc::unix::UnixListenerAdapter;
#[cfg(windows)]
use nlos_ipc::windows::NamedPipeListenerAdapter;
use nlos_ipc::{OutboundResponse, PeerAuthorizer, PeerIdentity, TransportConfig, serve_one};
use nlos_schema::SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL;
use nlos_schema::sabi::v1::{
    CellLeaseKind, CellLeaseLockState, CellReachabilityState, ContextResidencyTier,
    ControlCommand as SabiWireCommand, DurableOperationState, ExchangeResponse,
    ExecutionFiberLifecycleState, ExecutionFiberPhase, GetSystemControlRequest,
    PartitionAutonomyGateState, PlanNodeKind, PlanNodeLifecycleState, SabiRequestContext,
    SettingsDomainKind, TaskAttemptLifecycleState, TaskGroupLifecycleState, TaskLifecycleState,
    TaskUndoClass,
};
use nlos_system_control::auth::{
    EndpointListener, authenticated_serve_one_control, command_wall_key, serve_validated,
};
use nlos_system_control::cell_inspector::{
    CellLeaseRecord, CellSummaryRecord, LocalSingleNodeCellInspector,
};
use nlos_system_control::control::{
    CONTROL_CAPABILITY_GENERATION, CONTROL_CAPABILITY_SLOT, ControlCommand,
    DurableOperationInspection, ExecutionFiberInspection, NotificationInspection, SearchInspection,
    TaskNodeInspection, TopicInspection, dispatch_in_process_with_services,
    dispatch_over_plain_endpoint_with_services, receipt_to_hex,
};
use nlos_system_control::desktop_enum::{
    ChooserGrantRecord, ContentTypeAssociationRecord, ExecutionFiberSummaryRecord,
    InMemoryDesktopControlSurface, InstalledApplicationSummaryRecord, KeychainReferenceRecord,
    NotificationSummaryRecord, ProcessSummaryRecord, ResourceReservationSummaryRecord,
    SearchCandidateRecord, SettingsDomainProjection, SettingsEntryRecord, TaskAttemptSummaryRecord,
    TaskGroupSummaryRecord, TaskSummaryRecord, TopicSummaryRecord, WorkspaceRecord,
};
use nlos_system_control::nl::parse_nl_command;
use nlos_system_control::{RecoveryHealthSource, RecoverySystemControl, SystemControlAuthorizer};
use nlos_task::SqliteTaskAuthority;
use nlos_types::IdempotencyKey;

use llmos_desktop_lib::dto::{ConfigSourceDto, OutcomeDto};
use llmos_desktop_lib::error::ErrorCode;
use llmos_desktop_lib::ipc::{
    AppState, ControlAction, SessionConfig, build_inspect_cell_leases,
    build_inspect_settings_domain, build_inspect_workspace, build_list_cells,
    build_list_chooser_grants, build_list_content_type_associations, build_list_execution_fibers,
    build_list_installed_applications, build_list_keychain_references, build_list_notifications,
    build_list_operations, build_list_processes, build_list_resource_reservations,
    build_list_task_attempts, build_list_task_groups, build_list_task_nodes, build_list_tasks,
    build_list_topics, build_list_workspaces, build_poll_desktop_watermark_events,
    build_preview_control_blast_radius, build_query_control_command_status, dispatch_app_state,
    inspect_notification_core, inspect_search_core, query_search_candidates_core,
    submit_control_with_id_core,
};

static NEXT_HARNESS_ID: AtomicU64 = AtomicU64::new(1);
static CLI_BINARY_PATH: OnceLock<PathBuf> = OnceLock::new();

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn ensure_cli_binary() -> &'static Path {
    CLI_BINARY_PATH
        .get_or_init(|| {
            if let Ok(explicit) = std::env::var("LLMOS_SYSTEM_CONTROL_CLI") {
                let candidate = PathBuf::from(explicit);
                if candidate.exists() {
                    return candidate;
                }
            }
            let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
            let workspace_root = manifest_dir
                .join("../..")
                .canonicalize()
                .expect("workspace root");
            let status = Command::new("cargo")
                .current_dir(&workspace_root)
                .args([
                    "build",
                    "--quiet",
                    "-p",
                    "nlos-system-control",
                    "--bin",
                    "system-control-cli",
                    "--features",
                    "cli",
                ])
                .status()
                .expect("invoke cargo build for system-control-cli");
            assert!(
                status.success(),
                "building system-control-cli failed with {status}"
            );
            let bin_name = if cfg!(windows) {
                "system-control-cli.exe"
            } else {
                "system-control-cli"
            };
            let target_dir = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
                || workspace_root.join("target"),
                |dir| {
                    let path = PathBuf::from(dir);
                    if path.is_absolute() {
                        path
                    } else {
                        workspace_root.join(path)
                    }
                },
            );
            let bin_path = target_dir.join("debug").join(bin_name);
            assert!(
                bin_path.exists(),
                "expected built CLI binary at {}",
                bin_path.display()
            );
            bin_path
        })
        .as_path()
}

struct CapabilityPolicy;

impl SystemControlAuthorizer for CapabilityPolicy {
    fn authorize_get(
        &self,
        context: &SabiRequestContext,
        _: &GetSystemControlRequest,
    ) -> Result<(), &'static str> {
        authorize_capability(context)
    }

    fn authorize_submit(
        &self,
        context: &SabiRequestContext,
        _: &SabiWireCommand,
    ) -> Result<(), &'static str> {
        authorize_capability(context)
    }
}

fn authorize_capability(context: &SabiRequestContext) -> Result<(), &'static str> {
    let expected = nlos_schema::sabi::v1::CapabilityHandle {
        slot: CONTROL_CAPABILITY_SLOT,
        generation: CONTROL_CAPABILITY_GENERATION,
    };
    if context.capability_handles.as_slice() == [expected] {
        Ok(())
    } else {
        Err("missing recovery operations capability")
    }
}

struct AllowPeer;

impl PeerAuthorizer for AllowPeer {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone)]
struct StubHealth(RecoveryWorkerHealth);

impl RecoveryHealthSource for StubHealth {
    fn recovery_health(&self) -> RecoveryWorkerHealth {
        self.0.clone()
    }
}

fn next_nonce(counter: &Arc<AtomicU64>) -> [u8; 32] {
    let value = counter.fetch_add(1, Ordering::Relaxed);
    let mut nonce = [0u8; 32];
    nonce[..8].copy_from_slice(&value.to_be_bytes());
    nonce
}

fn bind_endpoint(path: &Path) -> EndpointListener {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(path);
        UnixListenerAdapter::bind(path).expect("bind unix listener")
    }
    #[cfg(windows)]
    {
        NamedPipeListenerAdapter::bind(path, 8, TransportConfig::default())
            .expect("bind named pipe listener")
    }
}

#[cfg(unix)]
async fn accept_plain(
    listener: &mut EndpointListener,
) -> Result<(tokio::net::UnixStream, PeerIdentity), nlos_ipc::IpcError> {
    listener.accept(TransportConfig::default()).await
}

#[cfg(windows)]
async fn accept_plain(
    listener: &mut EndpointListener,
) -> Result<
    (
        tokio::net::windows::named_pipe::NamedPipeServer,
        PeerIdentity,
    ),
    nlos_ipc::IpcError,
> {
    listener.accept(TransportConfig::default()).await
}

fn seed_twelve_domain_surface() -> (
    Arc<InMemoryDesktopControlSurface>,
    Arc<LocalSingleNodeCellInspector>,
) {
    let surface = Arc::new(InMemoryDesktopControlSurface::new_workstation_defaults());

    // 1. Tasks (>= 3)
    for (idx, tag) in [0x11_u8, 0x12, 0x13].into_iter().enumerate() {
        surface.upsert_task(TaskSummaryRecord {
            task_id: [tag; 16],
            plan_id: [tag; 16],
            application_id: Some([0x71; 16]),
            installation_generation: 2,
            plan_revision: 4,
            task_state: TaskLifecycleState::Running,
            undo_class: if idx == 0 {
                TaskUndoClass::Reversible
            } else {
                TaskUndoClass::Irreversible
            },
            reservation_id: Some([0x91; 16]),
            cell_id: Some([0xCE; 16]),
            updated_at_ms: 10_000 + u64::try_from(idx).unwrap_or(0),
            expected_generation_or_revision: 3,
        });
    }

    // 2. TaskGroups (>= 3, attached to task [0x11; 16] for blast-radius cascade)
    for (idx, tag) in [0x21_u8, 0x22, 0x23].into_iter().enumerate() {
        surface.upsert_task_group(TaskGroupSummaryRecord {
            group_id: [tag; 16],
            task_id: [0x11; 16],
            parent_group_id: (idx > 0).then_some([0x21; 16]),
            state: TaskGroupLifecycleState::Open,
            membership_generation: 2,
            state_seq: 1,
            depth: u32::try_from(idx).unwrap_or(0),
            cancel_epoch: 0,
            created_at_ms: 9_000,
            updated_at_ms: 9_500,
        });
    }

    // 3. TaskNodes (>= 3 hot/warm + 1 MetadataOnly + 1 Cold for [PLAN-LAZY-001] collapse)
    surface.upsert_plan_nodes(
        [0x11; 16],
        4,
        vec![
            TaskNodeInspection {
                plan_id: [0x11; 16],
                node_id: [0x31; 16],
                kind: PlanNodeKind::AgentRole,
                state: PlanNodeLifecycleState::Declared,
                declared_revision: 4,
                node_digest: vec![0xA1; 32],
                transition_count: 1,
                residency_tier: ContextResidencyTier::Hot,
                residency_transition_count: 1,
                first_declared_at_ms: 8_000,
                updated_at_ms: 8_100,
            },
            TaskNodeInspection {
                plan_id: [0x11; 16],
                node_id: [0x32; 16],
                kind: PlanNodeKind::Executable,
                state: PlanNodeLifecycleState::Active,
                declared_revision: 4,
                node_digest: vec![0xA2; 32],
                transition_count: 2,
                residency_tier: ContextResidencyTier::Warm,
                residency_transition_count: 1,
                first_declared_at_ms: 8_000,
                updated_at_ms: 8_200,
            },
            TaskNodeInspection {
                plan_id: [0x11; 16],
                node_id: [0x33; 16],
                kind: PlanNodeKind::AgentRole,
                state: PlanNodeLifecycleState::Completed,
                declared_revision: 4,
                node_digest: vec![0xA3; 32],
                transition_count: 3,
                residency_tier: ContextResidencyTier::Hot,
                residency_transition_count: 1,
                first_declared_at_ms: 8_000,
                updated_at_ms: 8_300,
            },
            TaskNodeInspection {
                plan_id: [0x11; 16],
                node_id: [0x34; 16],
                kind: PlanNodeKind::Executable,
                state: PlanNodeLifecycleState::Declared,
                declared_revision: 4,
                node_digest: vec![0xA4; 32],
                transition_count: 0,
                residency_tier: ContextResidencyTier::MetadataOnly,
                residency_transition_count: 1,
                first_declared_at_ms: 8_000,
                updated_at_ms: 8_400,
            },
            TaskNodeInspection {
                plan_id: [0x11; 16],
                node_id: [0x35; 16],
                kind: PlanNodeKind::Executable,
                state: PlanNodeLifecycleState::Declared,
                declared_revision: 4,
                node_digest: vec![0xA5; 32],
                transition_count: 0,
                residency_tier: ContextResidencyTier::Cold,
                residency_transition_count: 1,
                first_declared_at_ms: 8_000,
                updated_at_ms: 8_500,
            },
        ],
    );

    // 4. TaskAttempts (>= 3)
    for (idx, tag) in [0x41_u8, 0x42, 0x43].into_iter().enumerate() {
        surface.upsert_task_attempt(
            3,
            TaskAttemptSummaryRecord {
                attempt_id: [tag; 16],
                task_id: [0x11; 16],
                node_id: Some([0x32; 16]),
                state: if idx == 0 {
                    TaskAttemptLifecycleState::Committed
                } else {
                    TaskAttemptLifecycleState::Running
                },
                isolation_domain_id: [0x1D; 16],
                provider_isolation_unverified: idx == 2,
                cas_winner: idx == 0,
                expected_generation_or_revision: 3,
                started_at_ms: 9_100,
                updated_at_ms: 9_600,
            },
        );
    }

    // 5. ExecutionFibers (>= 3)
    for (idx, tag) in [0x51_u8, 0x52, 0x53].into_iter().enumerate() {
        surface.upsert_execution_fiber(
            2,
            ExecutionFiberSummaryRecord {
                fiber: ExecutionFiberInspection {
                    fiber_id: [tag; 16],
                    generation: 2,
                    state: ExecutionFiberLifecycleState::Running,
                    lifecycle_phase: ExecutionFiberPhase::Running,
                    active_cpu_ms: 120 + u64::try_from(idx).unwrap_or(0) * 10,
                    elapsed_wall_ms: 500,
                    scheduler_wait_ms: 15,
                    external_wait_ms: 40,
                    backpressure_wait_ms: 5,
                    suspended_ms: 0,
                },
                process_id: Some([0x81; 16]),
                shared_thread_like: idx == 1,
            },
        );
    }

    // 6. DurableOperations (>= 3, including EffectUnknown)
    for (tag, state) in [
        (0x61_u8, DurableOperationState::Registered),
        (0x62_u8, DurableOperationState::EffectUnknown),
        (0x63_u8, DurableOperationState::Completed),
    ] {
        surface.upsert_operation(DurableOperationInspection {
            operation_id: [tag; 16],
            generation: 2,
            state,
            cancel_epoch: 0,
            owner_fiber_id: [0x51; 16],
            owner_fiber_generation: 2,
            outcome_receipt_id: matches!(
                state,
                DurableOperationState::Completed | DurableOperationState::EffectUnknown
            )
            .then(|| vec![tag; 16]),
        });
    }

    // 7. Installed Applications (>= 3) + Settings + Associations + Keychain + ChooserGrants
    for tag in [0x71_u8, 0x72, 0x73] {
        surface.upsert_application(InstalledApplicationSummaryRecord {
            application_id: [tag; 16],
            installation_id: [tag.wrapping_add(0x04); 16],
            package_id: [tag; 16],
            package_manifest_digest: [tag; 32],
            current_installation_generation: 2,
            status: 1,
            created_at_ms: 5_000,
            updated_at_ms: 6_000,
        });
    }
    surface.upsert_settings_domain(SettingsDomainProjection {
        domain_kind: SettingsDomainKind::Workspace,
        scope_target_id: Some([0xD1; 16]),
        domain_revision: 2,
        entries: vec![SettingsEntryRecord {
            key: "workspace.grid".to_owned(),
            value_cbor: vec![0x02],
            revision: 2,
        }],
    });
    surface.upsert_settings_domain(SettingsDomainProjection {
        domain_kind: SettingsDomainKind::Application,
        scope_target_id: Some([0x71; 16]),
        domain_revision: 2,
        entries: vec![SettingsEntryRecord {
            key: "app.notifications".to_owned(),
            value_cbor: vec![0xF5],
            revision: 2,
        }],
    });
    for (tag, mime) in [
        (0xA1_u8, "text/markdown"),
        (0xA2_u8, "application/pdf"),
        (0xA3_u8, "image/png"),
    ] {
        surface.upsert_content_type_association(ContentTypeAssociationRecord {
            association_id: [tag; 16],
            content_type: mime.to_owned(),
            application_id: [0x71; 16],
            expected_generation_or_revision: 1,
        });
    }
    for (idx, tag) in [0xB1_u8, 0xB2, 0xB3].into_iter().enumerate() {
        surface.upsert_keychain_reference(KeychainReferenceRecord {
            keychain_ref_id: [tag; 16],
            owner_principal_id: [0x99; 16],
            label: format!("workstation-signing-ref-{idx}"),
            public_fingerprint: [tag; 32],
            expected_generation_or_revision: 1,
            hardware_backed: idx != 2,
        });
    }
    for (idx, tag) in [0xC1_u8, 0xC2, 0xC3].into_iter().enumerate() {
        surface.upsert_chooser_grant(ChooserGrantRecord {
            grant_id: [tag; 16],
            application_id: [0x71; 16],
            chooser_kind: u32::try_from(idx + 1).unwrap_or(1),
            resource_handle: [tag.wrapping_add(0x05); 16],
            expected_generation_or_revision: 1,
            expires_at_ms: 86_400_000,
        });
    }

    // 8. Workspaces (>= 3 including default [0xD0; 16])
    for (tag, rev, name) in [
        (0xD1_u8, 2_u64, "Engineering"),
        (0xD2_u8, 1_u64, "Operations"),
    ] {
        surface.upsert_workspace(WorkspaceRecord {
            workspace_id: [tag; 16],
            name: name.to_owned(),
            layout_preset: 2,
            active_surface_count: 3,
            focused_task_id: Some([0x11; 16]),
            expected_generation_or_revision: rev,
            updated_at_ms: 7_500,
        });
    }

    // 9. Processes (>= 3)
    for tag in [0x81_u8, 0x82, 0x83] {
        surface.upsert_process(ProcessSummaryRecord {
            process_id: [tag; 16],
            expected_generation_or_revision: 2,
            agent_instance_id: [tag.wrapping_add(1); 16],
            task_id: [0x11; 16],
            task_attempt_id: [0x41; 16],
            isolation_domain_id: [0x1D; 16],
            cell_id: Some([0xCE; 16]),
            lifecycle_state: 1,
        });
    }

    // 10. ResourceReservations (>= 3)
    for (tag, tier) in [
        (0x91_u8, ContextResidencyTier::Hot),
        (0x92_u8, ContextResidencyTier::Warm),
        (0x93_u8, ContextResidencyTier::Cold),
    ] {
        surface.upsert_resource_reservation(ResourceReservationSummaryRecord {
            reservation_id: [tag; 16],
            account_id: [0xAC; 16],
            actual_usage: 512,
            effective_granted: 1_024,
            requested_ceiling: 2_048,
            context_residency_tier: tier,
            expected_generation_or_revision: 2,
        });
    }

    // 11. Topics (>= 3) & Notifications (>= 3)
    for (idx, tag) in [0xE1_u8, 0xE2, 0xE3].into_iter().enumerate() {
        surface.upsert_topic(TopicSummaryRecord {
            topic: TopicInspection {
                topic_id: [tag; 16],
                channel_id: [0xCA; 16],
                channel_generation: 1,
                name: format!("system.events.{idx}").into_bytes(),
                active_subscriptions: 2,
                policy_digest: vec![tag; 32],
                created_at_ms: 4_000,
            },
            expected_generation_or_revision: 3,
            retained_messages: 12,
        });
    }
    for tag in [0xF1_u8, 0xF2, 0xF3] {
        surface.upsert_notification(NotificationSummaryRecord {
            notification: NotificationInspection {
                notification_id: [tag; 16],
                topic_id: [0xE1; 16],
                subscriber_key: [0x55; 16],
                next_sequence: 9,
                history_entry_count: 4,
                history_truncated: false,
                prune_watermark_sequence: 5,
            },
            expected_generation_or_revision: 9,
        });
    }

    // 12. Search (0x1F pinned + 3 candidates + 1 tampered scope for fail-closed bridge test)
    surface.upsert_search_scope(
        SearchInspection {
            scope_id: [0x5A; 32],
            consistency_class: 3,
            degradation_policy: 0,
            non_authoritative_restrictions_bits: 0x1F,
            admitted_event_count: 42,
            active_assertion_count: 38,
            retracted_event_count: 4,
            truncated: false,
        },
        vec![
            SearchCandidateRecord {
                candidate_id: [0xAA; 16],
                object_id: [0x11; 16],
                scope_id: [0x5A; 32],
                title_snippet: "Stage D architecture control task alpha".to_owned(),
                rank_score_milli: 980,
                non_authoritative_restrictions_bits: SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL,
            },
            SearchCandidateRecord {
                candidate_id: [0xAB; 16],
                object_id: [0x12; 16],
                scope_id: [0x5A; 32],
                title_snippet: "Stage D architecture control task beta".to_owned(),
                rank_score_milli: 910,
                non_authoritative_restrictions_bits: SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL,
            },
            SearchCandidateRecord {
                candidate_id: [0xAC; 16],
                object_id: [0x13; 16],
                scope_id: [0x5A; 32],
                title_snippet: "Stage D architecture control task gamma".to_owned(),
                rank_score_milli: 870,
                non_authoritative_restrictions_bits: SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL,
            },
        ],
        vec![],
    );
    surface.upsert_search_scope(
        SearchInspection {
            scope_id: [0x5B; 32],
            consistency_class: 3,
            degradation_policy: 0,
            non_authoritative_restrictions_bits: 0x0F, // tampered != 0x1F
            admitted_event_count: 1,
            active_assertion_count: 1,
            retracted_event_count: 0,
            truncated: false,
        },
        vec![],
        vec![],
    );

    // Cell topology (>= 3 cells + 3 leases including Quarantined ExclusiveDevice)
    let cells = Arc::new(LocalSingleNodeCellInspector::with_topology(
        vec![
            CellSummaryRecord {
                cell_id: [0xCE; 16],
                cell_label: "cell-primary-local".to_owned(),
                reachability: CellReachabilityState::OnlineLocal,
                partition_autonomy_gate: PartitionAutonomyGateState::FullAutonomy,
                current_epoch: 5,
                active_lease_count: 3,
                expected_generation_or_revision: 5,
            },
            CellSummaryRecord {
                cell_id: [0xCF; 16],
                cell_label: "cell-peer-ready".to_owned(),
                reachability: CellReachabilityState::OnlineRemote,
                partition_autonomy_gate: PartitionAutonomyGateState::FullAutonomy,
                current_epoch: 5,
                active_lease_count: 0,
                expected_generation_or_revision: 5,
            },
            CellSummaryRecord {
                cell_id: [0xD9; 16],
                cell_label: "cell-fenced-partition".to_owned(),
                reachability: CellReachabilityState::Partitioned,
                partition_autonomy_gate: PartitionAutonomyGateState::FencedReadOnly,
                current_epoch: 4,
                active_lease_count: 0,
                expected_generation_or_revision: 4,
            },
        ],
        vec![
            CellLeaseRecord {
                lease_id: [0xC1; 16],
                cell_id: [0xCE; 16],
                kind: CellLeaseKind::Quota,
                lock_state: CellLeaseLockState::Active,
                holder_id: [0x81; 16],
                remaining_holder_ttl_ms: 25_000,
                lease_epoch: 5,
                clock_skew_horizon_ms: 100,
            },
            CellLeaseRecord {
                lease_id: [0xC2; 16],
                cell_id: [0xCE; 16],
                kind: CellLeaseKind::Capacity,
                lock_state: CellLeaseLockState::Expiring,
                holder_id: [0x82; 16],
                remaining_holder_ttl_ms: 3_000,
                lease_epoch: 5,
                clock_skew_horizon_ms: 100,
            },
            CellLeaseRecord {
                lease_id: [0xC3; 16],
                cell_id: [0xCE; 16],
                kind: CellLeaseKind::ExclusiveDevice,
                lock_state: CellLeaseLockState::Quarantined,
                holder_id: [0x83; 16],
                remaining_holder_ttl_ms: 0,
                lease_epoch: 5,
                clock_skew_horizon_ms: 250,
            },
        ],
        true,
    ));

    (surface, cells)
}

struct SymmetryHarness {
    root: PathBuf,
    plain_endpoint: PathBuf,
    app_state: AppState,
    clock: Arc<AuthorityClock>,
    tasks: Arc<SqliteTaskAuthority>,
    health: StubHealth,
    surface: Arc<InMemoryDesktopControlSurface>,
    cells: Arc<LocalSingleNodeCellInspector>,
    auth_loop: tokio::task::JoinHandle<()>,
    plain_loop: tokio::task::JoinHandle<()>,
}

impl SymmetryHarness {
    fn spawn(tag: &str) -> Self {
        let id = NEXT_HARNESS_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("llmos-d-ipc-{tag}-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create test root");

        let seed = [0x57_u8; 32];
        let identity = Arc::new(IdentityAuthority::open(root.join("identity")).expect("identity"));
        let public_key = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
        let BootstrapDecision::Created(binding) = identity
            .bootstrap_principal(BootstrapPrincipalRequest {
                principal_profile_digest: [0xA1; 32],
                control_domain_policy_digest: [0xA2; 32],
                public_key,
                key_purpose: KeyPurpose::SemanticSigning,
                key_valid_from_ms: 0,
                key_valid_until_ms: 4_102_444_800_000,
                idempotency_key: IdempotencyKey::from_bytes([0xA3; 16]),
                created_at_ms: 0,
            })
            .expect("bootstrap principal")
        else {
            panic!("fresh identity authority must bootstrap principal");
        };

        let key_file = root.join("operator.key");
        std::fs::write(&key_file, hex(&seed)).expect("write operator key");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&key_file).unwrap().permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&key_file, perms).unwrap();
        }

        let clock = Arc::new(AuthorityClock::open(root.join("clock")).expect("clock"));
        let tasks = Arc::new(SqliteTaskAuthority::open(root.join("tasks.sqlite3")).expect("tasks"));
        let health = StubHealth(RecoveryWorkerHealth {
            state: RecoveryWorkerState::Running,
            completed_cycles: 12,
            ..RecoveryWorkerHealth::default()
        });
        let (surface, cells) = seed_twelve_domain_surface();

        #[cfg(unix)]
        let (auth_endpoint, plain_endpoint) = (root.join("auth.sock"), root.join("plain.sock"));
        #[cfg(windows)]
        let (auth_endpoint, plain_endpoint) = (
            PathBuf::from(format!(
                r"\\.\pipe\llmos-dipc-auth-{}-{id}",
                std::process::id()
            )),
            PathBuf::from(format!(
                r"\\.\pipe\llmos-dipc-plain-{}-{id}",
                std::process::id()
            )),
        );

        let cli_bin = ensure_cli_binary().to_path_buf();
        let handshake =
            Arc::new(ServerHandshakeContext::new(&auth_endpoint, 32).expect("handshake ctx"));
        let mut auth_listener = bind_endpoint(&auth_endpoint);
        let mut plain_listener = bind_endpoint(&plain_endpoint);

        let auth_tasks = Arc::clone(&tasks);
        let auth_clock = Arc::clone(&clock);
        let auth_health = health.clone();
        let auth_surface = Arc::clone(&surface);
        let auth_cells = Arc::clone(&cells);
        let auth_identity = Arc::clone(&identity);
        let nonce_counter = Arc::new(AtomicU64::new(1));

        let auth_loop = tokio::spawn(async move {
            let policy = CapabilityPolicy;
            let gate = AllowPeer;
            loop {
                let control =
                    RecoverySystemControl::new(auth_tasks.as_ref(), &auth_health, &policy)
                        .with_task_node_source(auth_surface.as_ref())
                        .with_execution_fiber_source(auth_surface.as_ref())
                        .with_topic_source(auth_surface.as_ref())
                        .with_operation_source(auth_surface.as_ref())
                        .with_operation_executor(auth_surface.as_ref())
                        .with_application_executor(auth_surface.as_ref())
                        .with_desktop_enumeration_source(auth_surface.as_ref())
                        .with_desktop_mutation_executor(auth_surface.as_ref())
                        .with_reconciliation_source(auth_surface.as_ref())
                        .with_command_status_ledger(auth_surface.command_ledger())
                        .with_cell_topology_inspector(auth_cells.as_ref())
                        .with_cell_lease_inspector(auth_cells.as_ref());
                let nonce = next_nonce(&nonce_counter);
                match authenticated_serve_one_control(
                    &mut auth_listener,
                    TransportConfig::default(),
                    &control,
                    auth_identity.as_ref(),
                    auth_clock.as_ref(),
                    handshake.as_ref(),
                    &gate,
                    10,
                    move || nonce,
                )
                .await
                {
                    Ok(outcome) => {
                        let _ = outcome.served();
                    }
                    Err(nlos_ipc::handshake::HandshakeError::Transport(
                        nlos_ipc::IpcError::Timeout(nlos_ipc::IoOperation::Accept),
                    )) => continue,
                    Err(_) => {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                }
            }
        });

        let plain_tasks = Arc::clone(&tasks);
        let plain_clock = Arc::clone(&clock);
        let plain_health = health.clone();
        let plain_surface = Arc::clone(&surface);
        let plain_cells = Arc::clone(&cells);

        let plain_loop = tokio::spawn(async move {
            loop {
                let (stream, peer) = match accept_plain(&mut plain_listener).await {
                    Ok(connection) => connection,
                    Err(nlos_ipc::IpcError::Timeout(nlos_ipc::IoOperation::Accept)) => continue,
                    Err(_) => {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        continue;
                    }
                };
                let tasks_ref = Arc::clone(&plain_tasks);
                let clock_ref = Arc::clone(&plain_clock);
                let health_ref = plain_health.clone();
                let surface_ref = Arc::clone(&plain_surface);
                let cells_ref = Arc::clone(&plain_cells);
                let _ = serve_one(
                    stream,
                    TransportConfig::default(),
                    peer,
                    &AllowPeer,
                    move |validated| async move {
                        let policy = CapabilityPolicy;
                        let control =
                            RecoverySystemControl::new(tasks_ref.as_ref(), &health_ref, &policy)
                                .with_task_node_source(surface_ref.as_ref())
                                .with_execution_fiber_source(surface_ref.as_ref())
                                .with_topic_source(surface_ref.as_ref())
                                .with_operation_source(surface_ref.as_ref())
                                .with_operation_executor(surface_ref.as_ref())
                                .with_application_executor(surface_ref.as_ref())
                                .with_desktop_enumeration_source(surface_ref.as_ref())
                                .with_desktop_mutation_executor(surface_ref.as_ref())
                                .with_reconciliation_source(surface_ref.as_ref())
                                .with_command_status_ledger(surface_ref.command_ledger())
                                .with_cell_topology_inspector(cells_ref.as_ref())
                                .with_cell_lease_inspector(cells_ref.as_ref());
                        let response =
                            serve_validated(&control, clock_ref.as_ref(), 10, validated.envelope());
                        Ok(OutboundResponse::Typed(ExchangeResponse {
                            envelope: Some(response),
                        }))
                    },
                )
                .await;
            }
        });

        let config = SessionConfig {
            socket_path: Some(auth_endpoint.to_string_lossy().into_owned()),
            principal_hex: Some(hex(binding.principal_id.as_bytes())),
            key_file: Some(key_file.to_string_lossy().into_owned()),
            cli_socket: Some(plain_endpoint.to_string_lossy().into_owned()),
            cli_path: Some(cli_bin.to_string_lossy().into_owned()),
            resource_root: None,
            application_root: None,
            source: ConfigSourceDto::Session,
        };
        let app_state = AppState::with_desktop_surface(config, Arc::clone(&surface));

        Self {
            root,
            plain_endpoint,
            app_state,
            clock,
            tasks,
            health,
            surface,
            cells,
            auth_loop,
            plain_loop,
        }
    }

    fn dispatch_structured_api(&self, command: &ControlCommand) -> String {
        let policy = CapabilityPolicy;
        let control = RecoverySystemControl::new(self.tasks.as_ref(), &self.health, &policy)
            .with_task_node_source(self.surface.as_ref())
            .with_execution_fiber_source(self.surface.as_ref())
            .with_topic_source(self.surface.as_ref())
            .with_operation_source(self.surface.as_ref())
            .with_operation_executor(self.surface.as_ref())
            .with_application_executor(self.surface.as_ref())
            .with_desktop_enumeration_source(self.surface.as_ref())
            .with_desktop_mutation_executor(self.surface.as_ref())
            .with_reconciliation_source(self.surface.as_ref())
            .with_command_status_ledger(self.surface.command_ledger())
            .with_cell_topology_inspector(self.cells.as_ref())
            .with_cell_lease_inspector(self.cells.as_ref());
        let correlation = command.control_command_id();
        let wall_ms = i64::try_from(
            self.clock
                .wall_now(NowRequest {
                    idempotency_key: command_wall_key(&correlation),
                })
                .expect("clock wall")
                .reading()
                .as_u64(),
        )
        .unwrap_or(i64::MAX);
        let receipt = dispatch_in_process_with_services(
            &control,
            command,
            10,
            wall_ms,
            Some(self.surface.as_ref()),
            Some(self.surface.as_ref()),
            Some(self.surface.as_ref()),
            Some(self.surface.as_ref()),
            Some(self.surface.as_ref()),
        )
        .expect("structured api dispatch");
        receipt_to_hex(&receipt)
    }

    async fn dispatch_nl_shell(&self, nl_utterance: &str) -> String {
        let command = parse_nl_command(nl_utterance).expect("parse NL utterance");
        let receipt = dispatch_over_plain_endpoint_with_services(
            &self.plain_endpoint,
            &command,
            Some(self.surface.as_ref()),
            Some(self.surface.as_ref()),
            Some(self.surface.as_ref()),
            Some(self.surface.as_ref()),
            Some(self.surface.as_ref()),
        )
        .await
        .expect("nl shell plain endpoint dispatch");
        receipt_to_hex(&receipt)
    }

    async fn dispatch_cli_subprocess(&self, cli_args: &[&str]) -> String {
        let cli_bin = ensure_cli_binary().to_path_buf();
        let endpoint = self.plain_endpoint.clone();
        let owned_args: Vec<String> = cli_args.iter().map(|s| (*s).to_owned()).collect();
        tokio::task::spawn_blocking(move || {
            let output = Command::new(&cli_bin)
                .arg(&endpoint)
                .args(&owned_args)
                .output()
                .expect("spawn system-control-cli");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            stdout
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("RECEIPT "))
                .unwrap_or_else(|| {
                    panic!(
                        "CLI output lacked RECEIPT line (status={:?}, stdout={stdout:?}, stderr={stderr:?})",
                        output.status.code()
                    )
                })
                .trim()
                .to_owned()
        })
        .await
        .expect("join cli subprocess")
    }
}

impl Drop for SymmetryHarness {
    fn drop(&mut self) {
        self.auth_loop.abort();
        self.plain_loop.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn twelve_domains_cursor_pagination_bounds_and_stale_generation_distinction() {
    let h = SymmetryHarness::spawn("enum12");

    // Pre-wire pagination bound guards (`limit <= 64`, `limit <= 128` for watermark).
    assert_eq!(
        build_list_tasks(None, 0).unwrap_err().code,
        ErrorCode::Config
    );
    assert_eq!(
        build_list_tasks(None, 65).unwrap_err().code,
        ErrorCode::Config
    );
    assert_eq!(
        build_poll_desktop_watermark_events(0, 129)
            .unwrap_err()
            .code,
        ErrorCode::Config
    );

    // 1. Tasks: limit=2 -> page1 (2, truncated=true) -> page2 (1, truncated=false)
    let tasks_p1 = dispatch_app_state(&h.app_state, build_list_tasks(None, 2).unwrap())
        .await
        .unwrap();
    let OutcomeDto::TasksListed {
        tasks: t1,
        next_cursor_hex: Some(t_cur),
        truncated: true,
    } = tasks_p1.outcome
    else {
        panic!("expected truncated TasksListed page 1");
    };
    assert_eq!(t1.len(), 2);
    let tasks_p2 = dispatch_app_state(&h.app_state, build_list_tasks(Some(&t_cur), 2).unwrap())
        .await
        .unwrap();
    let OutcomeDto::TasksListed {
        tasks: t2,
        next_cursor_hex: None,
        truncated: false,
    } = tasks_p2.outcome
    else {
        panic!("expected terminal TasksListed page 2");
    };
    assert_eq!(t2.len(), 1);

    // 2. TaskGroups
    let groups_p1 =
        dispatch_app_state(&h.app_state, build_list_task_groups(None, None, 2).unwrap())
            .await
            .unwrap();
    let OutcomeDto::TaskGroupsListed {
        groups: g1,
        next_cursor_hex: Some(g_cur),
        truncated: true,
    } = groups_p1.outcome
    else {
        panic!("expected truncated TaskGroupsListed");
    };
    assert_eq!(g1.len(), 2);
    let groups_p2 = dispatch_app_state(
        &h.app_state,
        build_list_task_groups(None, Some(&g_cur), 2).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        groups_p2.outcome,
        OutcomeDto::TaskGroupsListed {
            truncated: false,
            next_cursor_hex: None,
            ..
        }
    ));

    // 3. TaskNodes (`[PLAN-LAZY-001]` collapsed counts + `NOT_FOUND` vs `STALE_GENERATION`)
    let plan_hex = "11".repeat(16);
    let nodes_p1 = dispatch_app_state(
        &h.app_state,
        build_list_task_nodes(&plan_hex, 4, None, 2).unwrap(),
    )
    .await
    .unwrap();
    let OutcomeDto::TaskNodesListed {
        current_plan_revision: 4,
        stale_generation: false,
        nodes: n1,
        metadata_only_collapsed_count: 1,
        cold_collapsed_count: 1,
        next_cursor_hex: Some(n_cur),
        truncated: true,
        ..
    } = nodes_p1.outcome
    else {
        panic!(
            "expected TaskNodesListed with collapsed counts, got {:?}",
            nodes_p1.outcome
        );
    };
    assert_eq!(n1.len(), 2);
    let nodes_p2 = dispatch_app_state(
        &h.app_state,
        build_list_task_nodes(&plan_hex, 4, Some(&n_cur), 2).unwrap(),
    )
    .await
    .unwrap();
    let OutcomeDto::TaskNodesListed {
        nodes: n2,
        next_cursor_hex: Some(n_cur2),
        truncated: true,
        stale_generation: false,
        ..
    } = nodes_p2.outcome
    else {
        panic!("expected middle TaskNodesListed page 2");
    };
    assert_eq!(n2.len(), 2);
    let nodes_p3 = dispatch_app_state(
        &h.app_state,
        build_list_task_nodes(&plan_hex, 4, Some(&n_cur2), 2).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        nodes_p3.outcome,
        OutcomeDto::TaskNodesListed {
            truncated: false,
            next_cursor_hex: None,
            stale_generation: false,
            ..
        }
    ));

    // Stale plan revision vs absent plan_id
    let nodes_stale = dispatch_app_state(
        &h.app_state,
        build_list_task_nodes(&plan_hex, 1, None, 4).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        nodes_stale.outcome,
        OutcomeDto::StaleGeneration {
            expected_generation: 1,
            current_generation: 4,
            ..
        }
    ));
    let nodes_missing = dispatch_app_state(
        &h.app_state,
        build_list_task_nodes(&"9f".repeat(16), 4, None, 4).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        &nodes_missing.outcome,
        OutcomeDto::Failure { code, .. } if code == "SABI_ERROR_CODE_NOT_FOUND"
    ));

    // 4. TaskAttempts (pagination + stale_generation)
    let attempts_p1 = dispatch_app_state(
        &h.app_state,
        build_list_task_attempts(Some(&plan_hex), 3, None, 2).unwrap(),
    )
    .await
    .unwrap();
    let OutcomeDto::TaskAttemptsListed {
        stale_generation: false,
        attempts: a1,
        next_cursor_hex: Some(a_cur),
        truncated: true,
        ..
    } = attempts_p1.outcome
    else {
        panic!("expected truncated TaskAttemptsListed");
    };
    assert_eq!(a1.len(), 2);
    let attempts_p2 = dispatch_app_state(
        &h.app_state,
        build_list_task_attempts(Some(&plan_hex), 3, Some(&a_cur), 2).unwrap(),
    )
    .await
    .unwrap();
    let OutcomeDto::TaskAttemptsListed {
        attempts: a2,
        truncated: false,
        ..
    } = attempts_p2.outcome
    else {
        panic!("expected page 2 TaskAttemptsListed");
    };
    assert!(a2[0].provider_isolation_unverified);

    let attempts_stale = dispatch_app_state(
        &h.app_state,
        build_list_task_attempts(Some(&plan_hex), 1, None, 4).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        attempts_stale.outcome,
        OutcomeDto::StaleGeneration {
            expected_generation: 1,
            current_generation: 3,
            ..
        }
    ));

    // 5. ExecutionFibers & 6. DurableOperations (pagination + stale_generation)
    let proc_hex = "81".repeat(16);
    let fibers_p1 = dispatch_app_state(
        &h.app_state,
        build_list_execution_fibers(Some(&proc_hex), 2, None, 2).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        fibers_p1.outcome,
        OutcomeDto::ExecutionFibersListed {
            stale_generation: false,
            truncated: true,
            ..
        }
    ));
    let fibers_stale = dispatch_app_state(
        &h.app_state,
        build_list_execution_fibers(Some(&proc_hex), 1, None, 4).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        fibers_stale.outcome,
        OutcomeDto::StaleGeneration {
            expected_generation: 1,
            current_generation: 2,
            ..
        }
    ));

    let fiber_hex = "51".repeat(16);
    let ops_p1 = dispatch_app_state(
        &h.app_state,
        build_list_operations(Some(&fiber_hex), 2, None, 2).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        ops_p1.outcome,
        OutcomeDto::OperationsListed {
            stale_generation: false,
            truncated: true,
            ..
        }
    ));
    let ops_stale = dispatch_app_state(
        &h.app_state,
        build_list_operations(Some(&fiber_hex), 1, None, 4).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        ops_stale.outcome,
        OutcomeDto::StaleGeneration {
            expected_generation: 1,
            current_generation: 2,
            ..
        }
    ));

    // 7. Applications, SearchCandidates (0x1F), Settings (4 domains), Associations, Keychain (zero-secret), ChooserGrants
    let apps_p1 = dispatch_app_state(
        &h.app_state,
        build_list_installed_applications(None, 2).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        apps_p1.outcome,
        OutcomeDto::InstalledApplicationsListed {
            truncated: true,
            ..
        }
    ));

    let search_p1 =
        query_search_candidates_core(&h.app_state, &"5a".repeat(32), "architecture", 0, 2)
            .await
            .unwrap();
    let OutcomeDto::SearchCandidatesQueried {
        non_authoritative_restrictions_bits: 0x1f,
        candidates,
        truncated: true,
        ..
    } = search_p1.outcome
    else {
        panic!("expected SearchCandidatesQueried with 0x1F");
    };
    assert_eq!(candidates.len(), 2);

    for (kind, target) in [
        ("system", None),
        ("user", None),
        ("workspace", Some("d1".repeat(16))),
        ("application", Some("71".repeat(16))),
    ] {
        let settings = dispatch_app_state(
            &h.app_state,
            build_inspect_settings_domain(kind, target.as_deref(), 16).unwrap(),
        )
        .await
        .unwrap();
        assert!(matches!(
            settings.outcome,
            OutcomeDto::SettingsDomainInspected { .. }
        ));
    }

    let assoc_p1 = dispatch_app_state(
        &h.app_state,
        build_list_content_type_associations(None, 2).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        assoc_p1.outcome,
        OutcomeDto::ContentTypeAssociationsListed {
            truncated: true,
            ..
        }
    ));

    let keychain_p1 = dispatch_app_state(
        &h.app_state,
        build_list_keychain_references(None, 2).unwrap(),
    )
    .await
    .unwrap();
    let OutcomeDto::KeychainReferencesListed {
        references,
        truncated: true,
        ..
    } = keychain_p1.outcome
    else {
        panic!("expected KeychainReferencesListed");
    };
    assert_eq!(references.len(), 2);
    assert_eq!(references[0].public_fingerprint_hex.len(), 64);

    let grants_p1 = dispatch_app_state(&h.app_state, build_list_chooser_grants(None, 2).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        grants_p1.outcome,
        OutcomeDto::ChooserGrantsListed {
            truncated: true,
            ..
        }
    ));

    // 8. Workspaces (`ListWorkspaces` + `InspectWorkspace` NOT_FOUND vs STALE_GENERATION)
    let ws_p1 = dispatch_app_state(&h.app_state, build_list_workspaces(None, 2).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        ws_p1.outcome,
        OutcomeDto::WorkspacesListed {
            truncated: true,
            ..
        }
    ));

    let ws_ok = dispatch_app_state(
        &h.app_state,
        build_inspect_workspace(&"d1".repeat(16), 2).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        &ws_ok.outcome,
        OutcomeDto::WorkspaceInspected {
            name,
            expected_generation_or_revision: 2,
            ..
        } if name == "Engineering"
    ));

    let ws_stale = dispatch_app_state(
        &h.app_state,
        build_inspect_workspace(&"d1".repeat(16), 1).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        ws_stale.outcome,
        OutcomeDto::StaleGeneration {
            expected_generation: 1,
            current_generation: 2,
            ..
        }
    ));

    let ws_missing = dispatch_app_state(
        &h.app_state,
        build_inspect_workspace(&"df".repeat(16), 1).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        &ws_missing.outcome,
        OutcomeDto::Failure { code, .. } if code == "SABI_ERROR_CODE_NOT_FOUND"
    ));

    // 9. Processes, 10. ResourceReservations, 11. Topics & Notifications, 12. Cells & CellLeases
    let procs_p1 = dispatch_app_state(&h.app_state, build_list_processes(None, 2).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        procs_p1.outcome,
        OutcomeDto::ProcessesListed {
            truncated: true,
            ..
        }
    ));

    let res_p1 = dispatch_app_state(
        &h.app_state,
        build_list_resource_reservations(None, 2).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        res_p1.outcome,
        OutcomeDto::ResourceReservationsListed {
            truncated: true,
            ..
        }
    ));

    let topics_p1 = dispatch_app_state(&h.app_state, build_list_topics(None, 2).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        topics_p1.outcome,
        OutcomeDto::TopicsListed {
            truncated: true,
            ..
        }
    ));

    let notifs_p1 = dispatch_app_state(&h.app_state, build_list_notifications(None, 2).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        notifs_p1.outcome,
        OutcomeDto::NotificationsListed {
            truncated: true,
            ..
        }
    ));

    let cells_p1 = dispatch_app_state(&h.app_state, build_list_cells(None, 2).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        cells_p1.outcome,
        OutcomeDto::CellsListed {
            truncated: true,
            ..
        }
    ));

    let leases = dispatch_app_state(
        &h.app_state,
        build_inspect_cell_leases(Some(&"ce".repeat(16)), 16).unwrap(),
    )
    .await
    .unwrap();
    let OutcomeDto::CellLeasesInspected { leases, .. } = leases.outcome else {
        panic!("expected CellLeasesInspected");
    };
    assert_eq!(leases.len(), 3);
    assert!(leases.iter().any(|l| {
        l.kind == "CELL_LEASE_KIND_EXCLUSIVE_DEVICE"
            && l.lock_state == "CELL_LEASE_LOCK_STATE_QUARANTINED"
    }));

    // Watermark monotonic digest progression (`IPC-D5`)
    let wm_before = dispatch_app_state(
        &h.app_state,
        build_poll_desktop_watermark_events(0, 128).unwrap(),
    )
    .await
    .unwrap();
    let OutcomeDto::DesktopWatermarkPolled {
        current_watermark: mark0,
        ..
    } = wm_before.outcome
    else {
        panic!("expected watermark poll");
    };
    let _ = submit_control_with_id_core(
        &h.app_state,
        ControlAction::UpdateSettingsDomain {
            domain_kind: "system".to_owned(),
            scope: None,
            scope_target_id_hex: None,
            expected_domain_revision: 1,
            key: "desktop.theme".to_owned(),
            value_cbor_hex: "656c69676874".to_owned(),
            reason: "switch theme".to_owned(),
        },
        [0x88; 16],
    )
    .await
    .unwrap();
    let wm_after = dispatch_app_state(
        &h.app_state,
        build_poll_desktop_watermark_events(mark0, 128).unwrap(),
    )
    .await
    .unwrap();
    let OutcomeDto::DesktopWatermarkPolled {
        current_watermark: mark1,
        events: delta_events,
        ..
    } = wm_after.outcome
    else {
        panic!("expected watermark delta poll");
    };
    assert!(mark1 > mark0);
    assert_eq!(delta_events.len(), 1);
    assert_eq!(delta_events[0].state_digest_hex.len(), 64);
}

#[tokio::test]
async fn inspect_search_0x1f_fail_closed_inspect_notification_and_application_lifecycle_wiring() {
    let h = SymmetryHarness::spawn("search-notify-app");

    // 1. `inspect_search` succeeds when `0x1F` restriction bits are intact.
    let search_ok = inspect_search_core(&h.app_state, &"5a".repeat(32))
        .await
        .expect("inspect_search 0x1F valid");
    assert!(matches!(
        search_ok.outcome,
        OutcomeDto::SearchInspected {
            non_authoritative_restrictions_bits: 0x1f,
            admitted_event_count: 42,
            ..
        }
    ));

    // 2. `inspect_search` fails closed on the Tauri bridge when bits != 0x1F.
    let search_tampered = inspect_search_core(&h.app_state, &"5b".repeat(32))
        .await
        .expect_err("tampered search bits must fail closed");
    assert_eq!(search_tampered.code, ErrorCode::Control);
    assert!(search_tampered.message.contains("0x1F"));

    // 3. `inspect_notification` wired end-to-end.
    let notif = inspect_notification_core(&h.app_state, &"f1".repeat(16))
        .await
        .expect("inspect_notification");
    assert!(matches!(
        notif.outcome,
        OutcomeDto::NotificationInspected {
            next_sequence: 9,
            history_entry_count: 4,
            ..
        }
    ));

    // 4. `disable-application` -> `uninstall-application` end-to-end.
    let pkg_hex = "72".repeat(16);
    let disabled = submit_control_with_id_core(
        &h.app_state,
        ControlAction::DisableApplication {
            target_id_hex: pkg_hex.clone(),
            expected_revision: 2,
            scope: None,
            reason: "operator disables package 0x72".to_owned(),
        },
        [0x7A; 16],
    )
    .await
    .expect("disable-application");
    assert!(matches!(
        disabled.outcome,
        OutcomeDto::ApplicationDisabled { .. }
    ));

    let uninstalled = submit_control_with_id_core(
        &h.app_state,
        ControlAction::UninstallApplication {
            target_id_hex: pkg_hex,
            expected_revision: 3,
            scope: None,
            reason: "operator uninstalls package 0x72".to_owned(),
        },
        [0x7B; 16],
    )
    .await
    .expect("uninstall-application");
    assert!(matches!(
        uninstalled.outcome,
        OutcomeDto::ApplicationUninstalled { .. }
    ));
}

#[tokio::test]
async fn blast_radius_preview_and_idempotent_command_status_reconciliation() {
    let h = SymmetryHarness::spawn("blast-reconcile");
    let task_hex = "11".repeat(16);

    // `-` (`remove`) impact cascade across Task -> TaskGroup / TaskAttempt / Process / ExecutionFiber
    let kill_preview = dispatch_app_state(
        &h.app_state,
        build_preview_control_blast_radius("task", &task_hex, None, "kill", 32).unwrap(),
    )
    .await
    .unwrap();
    let OutcomeDto::BlastRadiusPreviewed {
        statutory_lock_blocked: false,
        impacts,
        ..
    } = kill_preview.outcome
    else {
        panic!("expected BlastRadiusPreviewed");
    };
    assert!(impacts.len() >= 10);
    assert!(
        impacts
            .iter()
            .all(|item| item.sign == "BLAST_RADIUS_IMPACT_SIGN_REMOVE")
    );
    assert!(
        impacts
            .iter()
            .any(|item| item.affected_scope == "CONTROL_SCOPE_TASK_GROUP")
    );
    assert!(
        impacts
            .iter()
            .any(|item| item.affected_scope == "CONTROL_SCOPE_EXECUTION_FIBER")
    );

    // `~` (`change`) impact on `pause`, `+` (`add`) impact on `migrate`
    let pause_preview = dispatch_app_state(
        &h.app_state,
        build_preview_control_blast_radius("task", &task_hex, None, "pause", 16).unwrap(),
    )
    .await
    .unwrap();
    let OutcomeDto::BlastRadiusPreviewed {
        impacts: change_impacts,
        ..
    } = pause_preview.outcome
    else {
        panic!("expected pause preview");
    };
    assert_eq!(change_impacts[0].sign, "BLAST_RADIUS_IMPACT_SIGN_CHANGE");

    let migrate_preview = dispatch_app_state(
        &h.app_state,
        build_preview_control_blast_radius("process", &"81".repeat(16), None, "migrate", 16)
            .unwrap(),
    )
    .await
    .unwrap();
    let OutcomeDto::BlastRadiusPreviewed {
        impacts: add_impacts,
        ..
    } = migrate_preview.outcome
    else {
        panic!("expected migrate preview");
    };
    assert_eq!(add_impacts[0].sign, "BLAST_RADIUS_IMPACT_SIGN_ADD");

    // Statutory lock dry-run preview surfaces `statutory_lock_blocked = true`
    let lock_preview = dispatch_app_state(
        &h.app_state,
        build_preview_control_blast_radius(
            "operation",
            &"62".repeat(16),
            None,
            "lock-override-unknown-operation",
            16,
        )
        .unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        lock_preview.outcome,
        OutcomeDto::BlastRadiusPreviewed {
            statutory_lock_blocked: true,
            ..
        }
    ));

    // `[CTRL-RECOVERY-001]` command status reconciliation: unknown -> completed -> rejected
    let unknown_status = dispatch_app_state(
        &h.app_state,
        build_query_control_command_status(&"ee".repeat(16)).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        &unknown_status.outcome,
        OutcomeDto::Failure { code, .. } if code == "SABI_ERROR_CODE_NOT_FOUND"
    ));

    let cmd_ok = [0x9A; 16];
    let ok_receipt = submit_control_with_id_core(
        &h.app_state,
        ControlAction::ScopedOperation {
            scope: "task-group".to_owned(),
            verb: "pause".to_owned(),
            target_id_hex: "21".repeat(16),
            expected_revision: 2,
            throttle_percent: None,
            reason: "pause group 0x21".to_owned(),
        },
        cmd_ok,
    )
    .await
    .unwrap();
    let OutcomeDto::OperationPaused { receipt_id_hex } = ok_receipt.outcome else {
        panic!("expected OperationPaused");
    };

    let reconciled_ok = dispatch_app_state(
        &h.app_state,
        build_query_control_command_status(&hex(&cmd_ok)).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        &reconciled_ok.outcome,
        OutcomeDto::CommandStatusQueried {
            state,
            receipt_id_hex: Some(rid),
            ..
        } if state == "CONTROL_COMMAND_LIFECYCLE_STATE_COMPLETED" && rid == &receipt_id_hex
    ));

    let cmd_bad_cas = [0x9B; 16];
    let _ = submit_control_with_id_core(
        &h.app_state,
        ControlAction::ScopedOperation {
            scope: "task-group".to_owned(),
            verb: "pause".to_owned(),
            target_id_hex: "22".repeat(16),
            expected_revision: 999,
            throttle_percent: None,
            reason: "stale cas".to_owned(),
        },
        cmd_bad_cas,
    )
    .await
    .unwrap();
    let reconciled_err = dispatch_app_state(
        &h.app_state,
        build_query_control_command_status(&hex(&cmd_bad_cas)).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        &reconciled_err.outcome,
        OutcomeDto::CommandStatusQueried {
            state,
            failure_code: Some(code),
            ..
        } if state == "CONTROL_COMMAND_LIFECYCLE_STATE_REJECTED" && code == "SABI_ERROR_CODE_CONFLICT"
    ));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn five_statutory_non_tamperable_locks_rejected_with_four_entry_byte_parity() {
    let h = SymmetryHarness::spawn("locks5");

    // Lock #1: `MutateSearchCandidate` on `0x1F` search candidate token
    let cand_id = [0xAA; 16];
    let cand_hex = hex(&cand_id);
    let scope_hex = "5a".repeat(32);
    let gui_l1 = submit_control_with_id_core(
        &h.app_state,
        ControlAction::MutateSearchCandidate {
            candidate_id_hex: cand_hex.clone(),
            scope_id_hex: scope_hex.clone(),
            restrictions_bits: 0x1F,
            expected_revision: 1,
            reason: "natural-language mutate search candidate probe".to_owned(),
        },
        cand_id,
    )
    .await
    .unwrap();
    assert!(matches!(
        &gui_l1.outcome,
        OutcomeDto::Failure { code, .. } if code == "SABI_ERROR_CODE_RIGHTS"
    ));
    let nl_l1 = h
        .dispatch_nl_shell(&format!(
            "mutate search candidate {cand_hex} {scope_hex} expecting 1"
        ))
        .await;
    let cli_l1 = h
        .dispatch_cli_subprocess(&[
            "mutate-search-candidate",
            &cand_hex,
            &cand_hex,
            &scope_hex,
            "1",
            "natural-language mutate search candidate probe",
        ])
        .await;
    let api_l1 = h.dispatch_structured_api(&ControlCommand::MutateSearchCandidate {
        control_command_id: cand_id,
        candidate_id: cand_id,
        scope_id: [0x5A; 32],
        non_authoritative_restrictions_bits: 0x1F,
        expected_generation_or_revision: 1,
        reason: "natural-language mutate search candidate probe".to_owned(),
    });
    assert_eq!(gui_l1.receipt_hex, nl_l1);
    assert_eq!(gui_l1.receipt_hex, cli_l1);
    assert_eq!(gui_l1.receipt_hex, api_l1);

    // Lock #2: `UndoReceipt` on `IRREVERSIBLE` undo class
    let irr_id = [0x12; 16];
    let irr_hex = hex(&irr_id);
    let gui_l2 = submit_control_with_id_core(
        &h.app_state,
        ControlAction::UndoReceipt {
            scope: Some("task".to_owned()),
            target_id_hex: irr_hex.clone(),
            undo_class: "irreversible".to_owned(),
            expected_revision: 3,
            reason: "natural-language undo receipt".to_owned(),
        },
        irr_id,
    )
    .await
    .unwrap();
    assert!(matches!(
        &gui_l2.outcome,
        OutcomeDto::Failure { code, .. } if code == "SABI_ERROR_CODE_STATE"
    ));
    let nl_l2 = h
        .dispatch_nl_shell(&format!(
            "undo receipt task {irr_hex} irreversible expecting 3"
        ))
        .await;
    let cli_l2 = h
        .dispatch_cli_subprocess(&[
            "undo-receipt",
            &irr_hex,
            "task",
            &irr_hex,
            "irreversible",
            "3",
            "natural-language undo receipt",
        ])
        .await;
    let api_l2 = h.dispatch_structured_api(&ControlCommand::UndoReceipt {
        control_command_id: irr_id,
        scope: nlos_schema::sabi::v1::ControlScope::Task,
        target_id: irr_id,
        receipt_undo_class: TaskUndoClass::Irreversible,
        expected_generation_or_revision: 3,
        reason: "natural-language undo receipt".to_owned(),
    });
    assert_eq!(gui_l2.receipt_hex, nl_l2);
    assert_eq!(gui_l2.receipt_hex, cli_l2);
    assert_eq!(gui_l2.receipt_hex, api_l2);

    // Lock #3: `MutateTaskNodeTopology` on `ACTIVE` and `COMPLETED` `TaskNode`
    for (node_tag, state_str, state_enum) in [
        (0x32_u8, "active", PlanNodeLifecycleState::Active),
        (0x33_u8, "completed", PlanNodeLifecycleState::Completed),
    ] {
        let node_id = [node_tag; 16];
        let node_hex = hex(&node_id);
        let plan_hex = "11".repeat(16);
        let gui_l3 = submit_control_with_id_core(
            &h.app_state,
            ControlAction::MutateTaskNodeTopology {
                plan_id_hex: plan_hex.clone(),
                node_id_hex: node_hex.clone(),
                observed_state: state_str.to_owned(),
                expected_plan_revision: 4,
                reason: "natural-language mutate node topology probe".to_owned(),
            },
            node_id,
        )
        .await
        .unwrap();
        assert!(matches!(
            &gui_l3.outcome,
            OutcomeDto::Failure { code, .. } if code == "SABI_ERROR_CODE_STATE"
        ));
        let nl_l3 = h
            .dispatch_nl_shell(&format!(
                "mutate node topology {plan_hex} {node_hex} {state_str} expecting 4"
            ))
            .await;
        let cli_l3 = h
            .dispatch_cli_subprocess(&[
                "mutate-task-node-topology",
                &node_hex,
                &plan_hex,
                &node_hex,
                state_str,
                "4",
                "natural-language mutate node topology probe",
            ])
            .await;
        let api_l3 = h.dispatch_structured_api(&ControlCommand::MutateTaskNodeTopology {
            control_command_id: node_id,
            plan_id: [0x11; 16],
            node_id,
            observed_node_state: state_enum,
            expected_generation_or_revision: 4,
            reason: "natural-language mutate node topology probe".to_owned(),
        });
        assert_eq!(gui_l3.receipt_hex, nl_l3);
        assert_eq!(gui_l3.receipt_hex, cli_l3);
        assert_eq!(gui_l3.receipt_hex, api_l3);
    }

    // Lock #4: `OverrideTaskAttemptCommit` manual commit-success override
    let attempt_id = [0x42; 16];
    let attempt_hex = hex(&attempt_id);
    let gui_l4 = submit_control_with_id_core(
        &h.app_state,
        ControlAction::OverrideTaskAttemptCommit {
            attempt_id_hex: attempt_hex.clone(),
            expected_generation: 3,
            reason: "natural-language override attempt commit probe".to_owned(),
        },
        attempt_id,
    )
    .await
    .unwrap();
    assert!(matches!(
        &gui_l4.outcome,
        OutcomeDto::Failure { code, .. } if code == "SABI_ERROR_CODE_RIGHTS"
    ));
    let nl_l4 = h
        .dispatch_nl_shell(&format!(
            "override attempt commit {attempt_hex} expecting 3"
        ))
        .await;
    let cli_l4 = h
        .dispatch_cli_subprocess(&[
            "override-task-attempt-commit",
            &attempt_hex,
            &attempt_hex,
            "3",
            "natural-language override attempt commit probe",
        ])
        .await;
    let api_l4 = h.dispatch_structured_api(&ControlCommand::OverrideTaskAttemptCommit {
        control_command_id: attempt_id,
        attempt_id,
        expected_generation_or_revision: 3,
        reason: "natural-language override attempt commit probe".to_owned(),
    });
    assert_eq!(gui_l4.receipt_hex, nl_l4);
    assert_eq!(gui_l4.receipt_hex, cli_l4);
    assert_eq!(gui_l4.receipt_hex, api_l4);

    // Lock #5: `OverrideEffectUnknownOperation` (`succeeded` and `not_happened`)
    for (op_cmd_tag, mark_succeeded, nl_tok, cli_bool_tok) in [
        (0x62_u8, true, "succeeded", "true"),
        (0x64_u8, false, "not_happened", "false"),
    ] {
        let op_id = [op_cmd_tag; 16];
        let op_hex = hex(&op_id);
        let gui_l5 = submit_control_with_id_core(
            &h.app_state,
            ControlAction::OverrideEffectUnknownOperation {
                operation_id_hex: op_hex.clone(),
                mark_succeeded,
                expected_generation: 2,
                reason: "natural-language override unknown operation probe".to_owned(),
            },
            op_id,
        )
        .await
        .unwrap();
        assert!(matches!(
            &gui_l5.outcome,
            OutcomeDto::Failure { code, .. } if code == "SABI_ERROR_CODE_STATE"
        ));
        let nl_l5 = h
            .dispatch_nl_shell(&format!(
                "override unknown operation {op_hex} as {nl_tok} expecting 2"
            ))
            .await;
        let cli_l5 = h
            .dispatch_cli_subprocess(&[
                "override-effect-unknown-operation",
                &op_hex,
                &op_hex,
                cli_bool_tok,
                "2",
                "natural-language override unknown operation probe",
            ])
            .await;
        let api_l5 = h.dispatch_structured_api(&ControlCommand::OverrideEffectUnknownOperation {
            control_command_id: op_id,
            operation_id: op_id,
            mark_succeeded,
            expected_generation_or_revision: 2,
            reason: "natural-language override unknown operation probe".to_owned(),
        });
        assert_eq!(gui_l5.receipt_hex, nl_l5);
        assert_eq!(gui_l5.receipt_hex, cli_l5);
        assert_eq!(gui_l5.receipt_hex, api_l5);
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn four_entry_receipt_hex_byte_parity_across_reads_and_scoped_mutations() {
    let h = SymmetryHarness::spawn("parity4-shared");

    // 1. Read parity: ListTasks
    let gui_tasks = dispatch_app_state(&h.app_state, build_list_tasks(None, 2).unwrap())
        .await
        .unwrap()
        .receipt_hex;
    let nl_tasks = h.dispatch_nl_shell("list tasks limit 2").await;
    let cli_tasks = h.dispatch_cli_subprocess(&["list-tasks", "2"]).await;
    let api_tasks = h.dispatch_structured_api(&ControlCommand::ListTasks {
        cursor: None,
        limit: 2,
    });
    assert_eq!(gui_tasks, nl_tasks);
    assert_eq!(gui_tasks, cli_tasks);
    assert_eq!(gui_tasks, api_tasks);

    // 2. Read parity: ListCells & InspectCellLeases
    let gui_cells = dispatch_app_state(&h.app_state, build_list_cells(None, 4).unwrap())
        .await
        .unwrap()
        .receipt_hex;
    let nl_cells = h.dispatch_nl_shell("列出 单元 限制 4").await;
    let cli_cells = h.dispatch_cli_subprocess(&["list-cells", "4"]).await;
    let api_cells = h.dispatch_structured_api(&ControlCommand::ListCells {
        cursor: None,
        limit: 4,
    });
    assert_eq!(gui_cells, nl_cells);
    assert_eq!(gui_cells, cli_cells);
    assert_eq!(gui_cells, api_cells);

    let cell_hex = "ce".repeat(16);
    let gui_leases = dispatch_app_state(
        &h.app_state,
        build_inspect_cell_leases(Some(&cell_hex), 8).unwrap(),
    )
    .await
    .unwrap()
    .receipt_hex;
    let nl_leases = h
        .dispatch_nl_shell(&format!("inspect cell leases {cell_hex} limit 8"))
        .await;
    let cli_leases = h
        .dispatch_cli_subprocess(&["inspect-cell-leases", &cell_hex, "8"])
        .await;
    let api_leases = h.dispatch_structured_api(&ControlCommand::InspectCellLeases {
        cell_id: Some([0xCE; 16]),
        limit: 8,
    });
    assert_eq!(gui_leases, nl_leases);
    assert_eq!(gui_leases, cli_leases);
    assert_eq!(gui_leases, api_leases);

    // 3. Read parity: PreviewControlBlastRadius
    let task_hex = "11".repeat(16);
    let gui_preview = dispatch_app_state(
        &h.app_state,
        build_preview_control_blast_radius("task", &task_hex, None, "kill", 16).unwrap(),
    )
    .await
    .unwrap()
    .receipt_hex;
    let nl_preview = h
        .dispatch_nl_shell(&format!("preview kill scope task {task_hex} limit 16"))
        .await;
    let cli_preview = h
        .dispatch_cli_subprocess(&["preview-blast-radius", "kill", "task", &task_hex, "16"])
        .await;
    let api_preview = h.dispatch_structured_api(&ControlCommand::PreviewControlBlastRadius {
        target_scope: nlos_schema::sabi::v1::ControlScope::Task,
        target_id: [0x11; 16],
        secondary_id: None,
        preview_action: 4,
        limit: 16,
    });
    assert_eq!(gui_preview, nl_preview);
    assert_eq!(gui_preview, cli_preview);
    assert_eq!(gui_preview, api_preview);

    // 4. Fresh-state 4-entry mutation parity across 4 independent identical harnesses:
    //    Explicit `ControlScope::TaskGroup` pause mutation!
    let group_id = [0x21; 16];
    let group_hex = hex(&group_id);
    let h_gui = SymmetryHarness::spawn("fresh-gui");
    let h_nl = SymmetryHarness::spawn("fresh-nl");
    let h_cli = SymmetryHarness::spawn("fresh-cli");
    let h_api = SymmetryHarness::spawn("fresh-api");

    let fresh_gui = submit_control_with_id_core(
        &h_gui.app_state,
        ControlAction::ScopedOperation {
            scope: "task-group".to_owned(),
            verb: "pause".to_owned(),
            target_id_hex: group_hex.clone(),
            expected_revision: 2,
            throttle_percent: None,
            reason: "natural-language pause operation".to_owned(),
        },
        group_id,
    )
    .await
    .unwrap();
    assert!(matches!(
        fresh_gui.outcome,
        OutcomeDto::OperationPaused { .. }
    ));
    let fresh_nl = h_nl
        .dispatch_nl_shell(&format!("pause scope task-group {group_hex} expecting 2"))
        .await;
    let fresh_cli = h_cli
        .dispatch_cli_subprocess(&[
            "scoped-operation",
            "pause",
            "task-group",
            &group_hex,
            &group_hex,
            "2",
            "natural-language pause operation",
        ])
        .await;
    let fresh_api = h_api.dispatch_structured_api(&ControlCommand::ScopedOperation {
        scope: nlos_schema::sabi::v1::ControlScope::TaskGroup,
        action: nlos_system_control::control::ScopedControlAction::Pause,
        control_command_id: group_id,
        target_id: group_id,
        expected_generation_or_revision: 2,
        reason: "natural-language pause operation".to_owned(),
    });
    assert_eq!(fresh_gui.receipt_hex, fresh_nl);
    assert_eq!(fresh_gui.receipt_hex, fresh_cli);
    assert_eq!(fresh_gui.receipt_hex, fresh_api);

    // 5. Controlled `RequestProcessMigration` (`IPC-D7` `[CELL-FENCE-001]`):
    //    succeeds to OnlineRemote peer `0xCF`, fails fenced (`FENCED`) to Partitioned `0xD9`,
    //    and replays byte-identically across all 4 entries!
    let proc_id = [0x81; 16];
    let proc_hex = hex(&proc_id);
    let peer_hex = "cf".repeat(16);
    let gui_mig = submit_control_with_id_core(
        &h.app_state,
        ControlAction::RequestProcessMigration {
            process_id_hex: proc_hex.clone(),
            destination_cell_id_hex: peer_hex.clone(),
            expected_revision: 2,
            reason: "natural-language process migration".to_owned(),
        },
        proc_id,
    )
    .await
    .unwrap();
    assert!(matches!(
        gui_mig.outcome,
        OutcomeDto::ProcessMigrationRequested { .. }
    ));
    let nl_mig = h
        .dispatch_nl_shell(&format!(
            "migrate process {proc_hex} to cell {peer_hex} expecting 2"
        ))
        .await;
    let cli_mig = h
        .dispatch_cli_subprocess(&[
            "migrate-process",
            &proc_hex,
            &proc_hex,
            &peer_hex,
            "2",
            "natural-language process migration",
        ])
        .await;
    let api_mig = h.dispatch_structured_api(&ControlCommand::RequestProcessMigration {
        control_command_id: proc_id,
        process_id: proc_id,
        destination_cell_id: [0xCF; 16],
        expected_generation_or_revision: 2,
        reason: "natural-language process migration".to_owned(),
    });
    assert_eq!(gui_mig.receipt_hex, nl_mig);
    assert_eq!(gui_mig.receipt_hex, cli_mig);
    assert_eq!(gui_mig.receipt_hex, api_mig);

    let fenced_mig = submit_control_with_id_core(
        &h.app_state,
        ControlAction::RequestProcessMigration {
            process_id_hex: "82".repeat(16),
            destination_cell_id_hex: "d9".repeat(16),
            expected_revision: 2,
            reason: "attempt fenced cell migration".to_owned(),
        },
        [0x8D; 16],
    )
    .await
    .unwrap();
    assert!(matches!(
        &fenced_mig.outcome,
        OutcomeDto::Failure { code, .. } if code == "SABI_ERROR_CODE_FENCED"
    ));

    // 6. QueryControlCommandStatus 4-entry byte parity on the completed migration command
    let gui_status = dispatch_app_state(
        &h.app_state,
        build_query_control_command_status(&proc_hex).unwrap(),
    )
    .await
    .unwrap()
    .receipt_hex;
    let nl_status = h
        .dispatch_nl_shell(&format!("query command status {proc_hex}"))
        .await;
    let cli_status = h
        .dispatch_cli_subprocess(&["query-command-status", &proc_hex])
        .await;
    let api_status = h.dispatch_structured_api(&ControlCommand::QueryControlCommandStatus {
        target_command_id: proc_id,
    });
    assert_eq!(gui_status, nl_status);
    assert_eq!(gui_status, cli_status);
    assert_eq!(gui_status, api_status);
}
