//! Integration & Negative-Bypass Verification Suite for `W72-D-SETTINGS-CTRL`
//! (`D-CONTROL-SETTINGS-A11Y` Slice 1: 12-Domain SystemControlCenter, 10-Layer
//! Ownership Tree, `SHARED_THREAD_LIKE` Badges, Stage C Cell 3-Family Leases &
//! Migration, Headlamp Action Pruning & 5+1 Statutory Locks, Blast-Radius Diff
//! & 5s Two-Step Arming, Data-Loss Receipt, `ControlCommandId` Recovery,
//! 4-Layer Settings CAS + Watch, Anti-Hijack `ContentTypeAssociation`, and
//! Zero-Secret `KeychainReference` Vault).

use llmos_desktop_lib::error::ErrorCode;
use llmos_desktop_lib::settings_keychain::{
    CellConnectivityPostureDto, CellLeaseFamilyDto, ControlCenterDomainKind,
    ControlCommandReconcileStatusDto, DevicePermissionPostureDto, FaultDomainPostureDto,
    HardwarePermissionDomainDto, OwnershipHierarchyLayer, PackageAssociationProposalDto,
    PartitionAutonomyGateDto, ProcessMigrationControlRequestDto, SettingsKeychainState,
    SettingsLayerKindDto, StatutoryLockKindDto, TWO_STEP_ARMING_WINDOW_MS, TWO_STEP_MIN_GUARD_MS,
    UndoClassDto, UpdateSettingCasRequestDto, acquire_keychain_use_by_reference_handle_core,
    arm_destructive_control_command_core, bind_content_type_association_core,
    confirm_destructive_control_command_core, drain_settings_watch_events_core,
    enroll_keychain_secret_reference_core, inspect_settings_layer_core,
    inspect_system_control_center_core, list_content_type_associations_v2_core,
    list_keychain_references_v2_core, lock_desktop_session_and_wipe_caches_core,
    preview_blast_radius_package_core, reconcile_control_command_status_core,
    request_process_migration_core, revoke_content_type_association_core,
    revoke_keychain_reference_core, set_hardware_permission_kill_switch_core,
    submit_package_association_proposal_core, unlock_desktop_session_core,
    update_setting_with_cas_core, validate_and_apply_node_verb_core,
};

#[test]
fn twelve_domain_control_center_and_ten_layer_system_informer_tree_projection() {
    let state = SettingsKeychainState::new_seeded();
    let snapshot = inspect_system_control_center_core(&state)
        .expect("inspect_system_control_center_core should succeed");

    // 1. All 12 authoritative control domains are projected in domain_summaries.
    assert_eq!(snapshot.domain_summaries.len(), 12);
    for expected_domain in ControlCenterDomainKind::ALL {
        assert!(
            snapshot
                .domain_summaries
                .iter()
                .any(|d| d.domain == expected_domain),
            "missing domain summary for {expected_domain:?}"
        );
    }

    // 2. All 10 System Informer ownership layers (depths 0..=9) are present.
    let expected_layers = [
        OwnershipHierarchyLayer::L1SystemDevice,
        OwnershipHierarchyLayer::L2WorkspaceTenant,
        OwnershipHierarchyLayer::L3Application,
        OwnershipHierarchyLayer::L4Task,
        OwnershipHierarchyLayer::L5TaskGroup,
        OwnershipHierarchyLayer::L6TaskNode,
        OwnershipHierarchyLayer::L7TaskAttempt,
        OwnershipHierarchyLayer::L8Process,
        OwnershipHierarchyLayer::L9AgentInstance,
        OwnershipHierarchyLayer::L10ExecutionFiber,
    ];
    for (expected_depth, layer) in expected_layers.into_iter().enumerate() {
        let node = snapshot
            .ownership_tree
            .iter()
            .find(|n| n.layer == layer)
            .unwrap_or_else(|| panic!("missing layer {layer:?} in ownership_tree"));
        assert_eq!(usize::from(node.depth), expected_depth);
        // Verify Nomad triple-watermark invariants (guaranteed <= allocated <= ceiling <= 100)
        assert!(node.watermark.guaranteed_floor_pct <= node.watermark.allocated_reserved_pct);
        assert!(node.watermark.allocated_reserved_pct <= node.watermark.burst_ceiling_pct);
        assert!(node.watermark.burst_ceiling_pct <= 100);
    }

    // 3. Verify SHARED_THREAD_LIKE fault-domain propagation across PID 4820 ([TASK-AGENT-001], [TASK-ISO-005]).
    assert!(snapshot.shared_thread_like_group_count >= 1);
    for shared_id in [
        "node-proc-worker-4820",
        "node-agent-scanner-a",
        "node-agent-scanner-b",
        "node-fiber-ast-parser-01",
    ] {
        let node = snapshot
            .ownership_tree
            .iter()
            .find(|n| n.node_id == shared_id)
            .expect("shared PID node must exist");
        assert_eq!(node.host_pid, Some(4820));
        assert_eq!(
            node.fault_domain_posture,
            FaultDomainPostureDto::SharedThreadLike,
            "node {shared_id} sharing PID 4820 must carry SHARED_THREAD_LIKE badge"
        );
        assert!(
            !node.shared_fate_peer_ids.is_empty(),
            "node {shared_id} must list shared fate peer IDs"
        );
    }

    // 4. Verify hardware permission kill-switch toggle in SystemControlCenter.
    let updated_perm = set_hardware_permission_kill_switch_core(
        &state,
        HardwarePermissionDomainDto::Microphone,
        DevicePermissionPostureDto::RevokedKillSwitch,
    )
    .expect("hardware kill-switch should succeed");
    assert_eq!(
        updated_perm.posture,
        DevicePermissionPostureDto::RevokedKillSwitch
    );
    assert_eq!(updated_perm.active_holder_count, 0);
}

#[test]
fn headlamp_action_pruning_and_five_plus_one_statutory_non_tamperable_locks() {
    let state = SettingsKeychainState::new_seeded();

    // 0. Missing ReadBackingToken is rejected fail-closed.
    let err_no_token =
        validate_and_apply_node_verb_core(&state, "node-task-release-verify", "Pause", "")
            .expect_err("empty ReadBackingToken must fail");
    assert!(err_no_token.to_string().contains("READ_BACKING_REQUIRED"));

    // Lock 1: 0x1F Search Candidate is strictly read-only ([CONS-CTRL-001]).
    let err_search_write = validate_and_apply_node_verb_core(
        &state,
        "node-search-0x1f-candidate",
        "Cancel",
        "rbt-valid-001",
    )
    .expect_err("0x1F search candidate must reject mutating verbs");
    assert!(
        err_search_write
            .to_string()
            .contains("SEARCH_0X1F_READ_ONLY")
    );

    let ok_search_jump = validate_and_apply_node_verb_core(
        &state,
        "node-search-0x1f-candidate",
        "JumpToAuthoritativeObject",
        "rbt-valid-001",
    )
    .expect("JumpToAuthoritativeObject must be allowed on 0x1F search candidate");
    assert!(
        ok_search_jump
            .statutory_locks
            .contains(&StatutoryLockKindDto::Search0x1fReadOnly)
    );

    // Lock 2: IRREVERSIBLE receipt forbids Undo ([RCPT-UNDO-001]).
    let err_irreversible_undo = validate_and_apply_node_verb_core(
        &state,
        "node-task-external-publish",
        "Undo",
        "rbt-valid-002",
    )
    .expect_err("IRREVERSIBLE task must reject Undo");
    assert!(
        err_irreversible_undo
            .to_string()
            .contains("IRREVERSIBLE_NO_UNDO")
    );

    // Lock 3: COMPLETED or ACTIVE TaskNode forbids structural edits ([TASK-GRAPH-002]).
    let err_active_node_edit = validate_and_apply_node_verb_core(
        &state,
        "node-tasknode-ast-scan",
        "EditNodeSpec",
        "rbt-valid-003",
    )
    .expect_err("ACTIVE TaskNode must reject structural edits");
    assert!(
        err_active_node_edit
            .to_string()
            .contains("NODE_COMPLETED_OR_ACTIVE_LOCKED")
    );

    // Pending TaskNode allows EditNodeSpec.
    let ok_pending_node_edit = validate_and_apply_node_verb_core(
        &state,
        "node-tasknode-report-pack",
        "EditNodeSpec",
        "rbt-valid-003b",
    )
    .expect("PENDING TaskNode should allow EditNodeSpec");
    assert_eq!(ok_pending_node_edit.lifecycle_state, "PENDING");

    // Lock 4: TaskAttempt forbids manual commit ([TASK-COMMIT-001]).
    for forbidden_commit_verb in ["Commit", "ManualCommit", "PromoteDone"] {
        let err_attempt_commit = validate_and_apply_node_verb_core(
            &state,
            "node-attempt-ast-01",
            forbidden_commit_verb,
            "rbt-valid-004",
        )
        .expect_err("TaskAttempt must forbid manual commit");
        assert!(
            err_attempt_commit
                .to_string()
                .contains("TASK_ATTEMPT_NO_MANUAL_COMMIT")
        );
    }

    // Lock 5: Operation(EFFECT_UNKNOWN) forbids manual mark success ([TASK-EFFECT-003]).
    for forbidden_op_verb in ["MarkSuccess", "MarkNotExecuted", "ForceComplete"] {
        let err_op_manual = validate_and_apply_node_verb_core(
            &state,
            "node-op-ext-webhook",
            forbidden_op_verb,
            "rbt-valid-005",
        )
        .expect_err("EFFECT_UNKNOWN operation must forbid manual mark success");
        assert!(
            err_op_manual
                .to_string()
                .contains("OPERATION_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS")
        );
    }
    let ok_op_reconcile = validate_and_apply_node_verb_core(
        &state,
        "node-op-ext-webhook",
        "Reconcile",
        "rbt-valid-005b",
    )
    .expect("EFFECT_UNKNOWN operation allows Reconcile");
    assert_eq!(ok_op_reconcile.lifecycle_state, "RECONCILING");

    // Lock 6 (+1): Quarantined Cell Lease forbids local TTL unlock ([LEASE-TTL-001]).
    for forbidden_lease_verb in ["LocalTtlUnlock", "ForceUnlockLease", "ReleaseLease"] {
        let err_quarantined_unlock = validate_and_apply_node_verb_core(
            &state,
            "node-cell-quarantined-edge",
            forbidden_lease_verb,
            "rbt-valid-006",
        )
        .expect_err("Quarantined lease must reject local TTL unlock");
        assert!(
            err_quarantined_unlock
                .to_string()
                .contains("QUARANTINED_LEASE_FENCE")
        );
    }
}

#[test]
fn stage_c_cell_three_family_lease_inspection_and_controlled_process_migration() {
    let state = SettingsKeychainState::new_seeded();
    let snapshot = inspect_system_control_center_core(&state).expect("snapshot");

    let primary_cell = snapshot
        .cells
        .iter()
        .find(|c| c.cell_id_hex == "cell-local-primary")
        .expect("primary cell must exist");
    assert_eq!(
        primary_cell.posture,
        CellConnectivityPostureDto::LocalSingleNode
    );
    assert_eq!(primary_cell.autonomy_gate, PartitionAutonomyGateDto::Open);

    // All 3 Stage C lease families (Quota, Capacity, ExclusiveDevice) are present on primary cell.
    let families: Vec<CellLeaseFamilyDto> = primary_cell.leases.iter().map(|l| l.family).collect();
    assert!(families.contains(&CellLeaseFamilyDto::QuotaExecutionLease));
    assert!(families.contains(&CellLeaseFamilyDto::CapacitySubpoolLease));
    assert!(families.contains(&CellLeaseFamilyDto::ExclusiveDeviceEpochLease));

    // 1. Stale generation / epoch is rejected with CasConflict.
    let stale_err = request_process_migration_core(
        &state,
        ProcessMigrationControlRequestDto {
            process_node_id: "node-proc-worker-4820".to_string(),
            source_cell_id_hex: "cell-local-primary".to_string(),
            target_cell_id_hex: "cell-secondary-compute".to_string(),
            expected_revision_or_generation: 999,
            read_backing_token: "rbt-mig-001".to_string(),
            closure_receipt_hex: None,
            fence_receipt_hex: None,
        },
    )
    .expect_err("stale generation must fail with CasConflict");
    assert_eq!(stale_err.code, ErrorCode::Control);
    assert!(
        stale_err
            .message
            .contains("CAS_CONFLICT: expected_revision=999, actual_revision=42")
    );

    // 2. Migrating into a Quarantined cell is rejected.
    let quarantined_target_err = request_process_migration_core(
        &state,
        ProcessMigrationControlRequestDto {
            process_node_id: "node-proc-worker-4820".to_string(),
            source_cell_id_hex: "cell-local-primary".to_string(),
            target_cell_id_hex: "cell-quarantined-edge".to_string(),
            expected_revision_or_generation: 42,
            read_backing_token: "rbt-mig-002".to_string(),
            closure_receipt_hex: None,
            fence_receipt_hex: None,
        },
    )
    .expect_err("migrating to quarantined cell must fail");
    assert!(
        quarantined_target_err
            .to_string()
            .contains("MIGRATION_TARGET_UNAVAILABLE")
    );

    // 3. Migrating out of a Quarantined cell without closure + fence receipts is rejected ([LEASE-TTL-001]).
    let quarantined_source_err = request_process_migration_core(
        &state,
        ProcessMigrationControlRequestDto {
            process_node_id: "node-cell-quarantined-edge".to_string(),
            source_cell_id_hex: "cell-quarantined-edge".to_string(),
            target_cell_id_hex: "cell-secondary-compute".to_string(),
            expected_revision_or_generation: 41,
            read_backing_token: "rbt-mig-003".to_string(),
            closure_receipt_hex: None,
            fence_receipt_hex: None,
        },
    )
    .expect_err("migrating out of quarantined cell without fence receipts must fail");
    assert!(
        quarantined_source_err
            .to_string()
            .contains("QUARANTINED_LEASE_FENCE")
    );

    // 4. Valid controlled migration from cell-local-primary to cell-secondary-compute succeeds.
    let mig_ok = request_process_migration_core(
        &state,
        ProcessMigrationControlRequestDto {
            process_node_id: "node-proc-worker-4820".to_string(),
            source_cell_id_hex: "cell-local-primary".to_string(),
            target_cell_id_hex: "cell-secondary-compute".to_string(),
            expected_revision_or_generation: 42,
            read_backing_token: "rbt-mig-004".to_string(),
            closure_receipt_hex: None,
            fence_receipt_hex: None,
        },
    )
    .expect("valid process migration should succeed");

    assert_eq!(mig_ok.new_generation, 43);
    assert_eq!(
        mig_ok.migrated_lease_ids,
        vec!["lease-quota-exec-001".to_string()]
    );
    assert!(mig_ok.receipt_id_hex.starts_with("rcpt_mig_"));
}

#[test]
fn terraform_blast_radius_preview_two_step_arming_data_loss_receipt_and_command_reconciliation() {
    let state = SettingsKeychainState::new_seeded();

    // 1. Preview TopicPurge blast radius (+ / ~ / - diff rows + 5s arming metadata).
    let preview =
        preview_blast_radius_package_core(&state, "node-topic-telemetry", "TopicPurge", 1)
            .expect("preview TopicPurge should succeed");
    assert!(preview.requires_two_step_arming);
    assert_eq!(preview.arming_window_ms, TWO_STEP_ARMING_WINDOW_MS);
    assert_eq!(preview.min_guard_ms, TWO_STEP_MIN_GUARD_MS);
    assert!(preview.requires_data_loss_ack);
    assert_eq!(preview.plus_create_count, 1);
    assert_eq!(preview.tilde_update_count, 1);
    assert_eq!(preview.minus_destroy_count, 1);
    assert_eq!(preview.estimated_purged_messages, 1_280);

    // 2. Step 1: Arm TopicPurge at t = 10_000ms.
    let ticket = arm_destructive_control_command_core(
        &state,
        "node-topic-telemetry",
        "TopicPurge",
        1,
        10_000,
    )
    .expect("arming TopicPurge should succeed");

    // 3. Clickjack guard (< 350ms) blocks premature second click at t = 10_100ms.
    let early_err = confirm_destructive_control_command_core(
        &state,
        "cmd-purge-001",
        &ticket.arming_ticket_id_hex,
        true,
        10_100,
        false,
    )
    .expect_err("click before 350ms guard must be rejected");
    assert!(early_err.to_string().contains("TWO_STEP_CLICKJACK_GUARD"));

    // 4. Missing data_loss_ack is rejected for TopicPurge.
    let no_ack_err = confirm_destructive_control_command_core(
        &state,
        "cmd-purge-001",
        &ticket.arming_ticket_id_hex,
        false,
        10_500,
        false,
    )
    .expect_err("TopicPurge without acknowledge_data_loss must be rejected");
    assert!(no_ack_err.to_string().contains("DATA_LOSS_ACK_REQUIRED"));

    // 5. Simulate transport UNKNOWN outcome at t = 10_600ms ([CTRL-RECOVERY-001]).
    let unknown_ledger = confirm_destructive_control_command_core(
        &state,
        "cmd-purge-001",
        &ticket.arming_ticket_id_hex,
        true,
        10_600,
        true,
    )
    .expect("simulated UNKNOWN outcome should record ledger entry");
    assert_eq!(
        unknown_ledger.status,
        ControlCommandReconcileStatusDto::UnknownNeedsReconcile
    );
    assert!(unknown_ledger.blind_retry_blocked);

    // Verify Data-Loss Receipt was generated and locked IRREVERSIBLE_NO_UNDO.
    let dl_receipt = unknown_ledger
        .data_loss_receipt
        .as_ref()
        .expect("TopicPurge must produce DataLossReceiptDto");
    assert!(dl_receipt.data_loss_acknowledged);
    assert_eq!(dl_receipt.purged_messages_count, 1_280);
    assert_eq!(dl_receipt.undo_class, UndoClassDto::Irreversible);
    assert_eq!(
        dl_receipt.statutory_lock,
        StatutoryLockKindDto::IrreversibleNoUndo
    );

    // 6. Blind retry with same command ID OR a fresh ticket on the same target is FORBIDDEN while UNKNOWN!
    let blind_retry_same_id = confirm_destructive_control_command_core(
        &state,
        "cmd-purge-001",
        "arm_any",
        true,
        11_000,
        false,
    )
    .expect_err("blind retry of UNKNOWN command ID must be blocked");
    assert!(
        blind_retry_same_id
            .to_string()
            .contains("BLIND_RETRY_FORBIDDEN_ON_UNKNOWN")
    );

    let ticket_2 = arm_destructive_control_command_core(
        &state,
        "node-topic-telemetry",
        "TopicPurge",
        1,
        11_000,
    )
    .expect("arming ticket 2");
    let blind_retry_new_id = confirm_destructive_control_command_core(
        &state,
        "cmd-purge-002",
        &ticket_2.arming_ticket_id_hex,
        true,
        11_500,
        false,
    )
    .expect_err("blind retry on target with un-reconciled UNKNOWN command must be blocked");
    assert!(
        blind_retry_new_id
            .to_string()
            .contains("BLIND_RETRY_FORBIDDEN_ON_UNKNOWN")
    );

    // 7. Reconcile status via reconcile_control_command_status_core ([CTRL-RECOVERY-001]).
    let reconciled = reconcile_control_command_status_core(&state, "cmd-purge-001")
        .expect("reconcile_control_command_status_core should succeed");
    assert_eq!(
        reconciled.status,
        ControlCommandReconcileStatusDto::AppliedIdempotent
    );
    assert!(!reconciled.blind_retry_blocked);

    // 8. Verify 5-second arming window expiration on ticket_2 at t = 17_000ms (> 11_000 + 5_000).
    let expired_err = confirm_destructive_control_command_core(
        &state,
        "cmd-purge-002",
        &ticket_2.arming_ticket_id_hex,
        true,
        17_000,
        false,
    )
    .expect_err("confirmation after 5000ms window must expire");
    assert!(expired_err.to_string().contains("TWO_STEP_ARMING_EXPIRED"));
}

#[test]
fn four_layer_settings_domain_cas_precedence_and_watch_synchronization() {
    let state = SettingsKeychainState::new_seeded();

    // 1. Inspect User layer and verify 4-layer effective precedence (Application > Workspace > User > System).
    let user_snap = inspect_settings_layer_core(&state, SettingsLayerKindDto::User)
        .expect("inspect User settings layer");
    assert_eq!(user_snap.domain_revision, 20);
    assert_eq!(user_snap.active_theme, "graphite-workstation");

    let concurrency_eff = user_snap
        .effective_settings
        .iter()
        .find(|s| s.key == "control.background_concurrency_limit")
        .expect("concurrency setting must exist");
    // Defined in System (8), User (12), Workspace (16) -> Workspace wins with "16"!
    assert_eq!(concurrency_eff.effective_value_json, "16");
    assert_eq!(
        concurrency_eff.winning_layer,
        SettingsLayerKindDto::Workspace
    );

    // 2. System-policy-locked key cannot be overridden by User or Workspace layers.
    let policy_lock_err = update_setting_with_cas_core(
        &state,
        UpdateSettingCasRequestDto {
            layer: SettingsLayerKindDto::User,
            key: "security.mandatory_biometric_confirm".to_string(),
            value_json: "false".to_string(),
            expected_domain_revision: 20,
        },
    )
    .expect_err("system policy locked key must reject User override");
    assert!(policy_lock_err.to_string().contains("SYSTEM_POLICY_LOCKED"));

    // 3. Valid CAS update on User theme ("graphite-workstation" -> "paper-workstation").
    let (updated_snap, watch_ev) = update_setting_with_cas_core(
        &state,
        UpdateSettingCasRequestDto {
            layer: SettingsLayerKindDto::User,
            key: "appearance.theme".to_string(),
            value_json: "\"paper-workstation\"".to_string(),
            expected_domain_revision: 20,
        },
    )
    .expect("CAS update with matching revision 20 should succeed");
    assert_eq!(updated_snap.domain_revision, 21);
    assert_eq!(updated_snap.active_theme, "paper-workstation");
    assert_eq!(watch_ev.watch_seq, 1);
    assert_eq!(watch_ev.new_domain_revision, 21);
    assert_eq!(watch_ev.effective_theme, "paper-workstation");

    // 4. Stale expected_domain_revision (20) is rejected with CasConflict { expected: 20, actual: 21 }.
    let stale_cas_err = update_setting_with_cas_core(
        &state,
        UpdateSettingCasRequestDto {
            layer: SettingsLayerKindDto::User,
            key: "appearance.theme".to_string(),
            value_json: "\"graphite-workstation\"".to_string(),
            expected_domain_revision: 20,
        },
    )
    .expect_err("stale revision 20 must fail with CasConflict");
    assert_eq!(stale_cas_err.code, ErrorCode::Control);
    assert!(
        stale_cas_err
            .message
            .contains("CAS_CONFLICT: expected_revision=20, actual_revision=21")
    );

    // 5. Watch stream drains monotonically.
    let events = drain_settings_watch_events_core(&state, 0).expect("drain watch events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].key, "appearance.theme");
}

#[test]
fn content_type_association_anti_hijack_proposal_and_user_revocable_binding() {
    let state = SettingsKeychainState::new_seeded();
    let initial = list_content_type_associations_v2_core(&state).expect("list associations");
    let md_assoc = initial
        .iter()
        .find(|a| a.content_type_or_uri == "text/markdown")
        .expect("text/markdown association must exist");
    assert_eq!(
        md_assoc.handler_application_id_hex.as_deref(),
        Some("app-spec-studio-01")
    );
    assert_eq!(md_assoc.revision, 1);

    // 1. Package submits a proposal for text/markdown ([DUI-ASSOC-001]).
    // Must NOT hijack the existing default handler!
    let after_proposal = submit_package_association_proposal_core(
        &state,
        "text/markdown",
        PackageAssociationProposalDto {
            proposed_application_id_hex: "app-untrusted-md-viewer".to_string(),
            proposed_installation_id_hex: "inst-untrusted-md-01".to_string(),
            proposed_app_name_zh: "外部 Markdown 预览器".to_string(),
            proposed_app_name_en: "External Markdown Viewer".to_string(),
            submitted_at_ms: 1_760_000_500,
        },
    )
    .expect("package proposal submission should succeed");

    assert_eq!(
        after_proposal.handler_application_id_hex.as_deref(),
        Some("app-spec-studio-01"),
        "package proposal must NEVER silently overwrite active handler_application_id_hex"
    );
    assert!(after_proposal.pending_package_proposal.is_some());
    assert_eq!(after_proposal.revision, 2);

    // 2. User explicitly approves/binds new handler with CAS expected_revision = 2.
    let bound = bind_content_type_association_core(
        &state,
        "text/markdown",
        "app-untrusted-md-viewer",
        "inst-untrusted-md-01",
        "外部 Markdown 预览器",
        "External Markdown Viewer",
        2,
    )
    .expect("user explicit bind with matching revision should succeed");
    assert_eq!(
        bound.handler_application_id_hex.as_deref(),
        Some("app-untrusted-md-viewer")
    );
    assert!(bound.pending_package_proposal.is_none());
    assert_eq!(bound.revision, 3);

    // 3. User one-click revokes association back to system chooser prompt.
    let revoked = revoke_content_type_association_core(&state, "text/markdown", 3)
        .expect("one-click revoke should succeed");
    assert!(revoked.handler_application_id_hex.is_none());
    assert!(revoked.handler_installation_id_hex.is_none());
    assert!(!revoked.user_explicitly_bound);
    assert_eq!(revoked.revision, 4);
}

#[test]
fn keychain_reference_zero_secret_serialization_use_by_ref_and_session_lock_wipe() {
    let state = SettingsKeychainState::new_seeded();
    let secret_marker = "ULTRA_SECRET_RAW_BYTES_DO_NOT_LEAK_999";

    // 1. Enrolling without Z4 confirmation token is rejected.
    let no_z4_err = enroll_keychain_secret_reference_core(
        &state,
        "kref-custom-prod-key",
        "principal-workstation-operator",
        "生产集群部署密钥引用",
        "Production Cluster Deploy Key Reference",
        "ED25519_PRIVATE_KEY_REF",
        secret_marker.as_bytes(),
        vec!["node-app-code-analyzer".to_string()],
        true,
        "",
    )
    .expect_err("enrolling keychain reference without Z4 token must fail");
    assert!(no_z4_err.to_string().contains("Z4_CONFIRMATION_REQUIRED"));

    // 2. Enroll with valid Z4 token succeeds and returns only KeychainReferenceEntryDto.
    let enrolled = enroll_keychain_secret_reference_core(
        &state,
        "kref-custom-prod-key",
        "principal-workstation-operator",
        "生产集群部署密钥引用",
        "Production Cluster Deploy Key Reference",
        "ED25519_PRIVATE_KEY_REF",
        secret_marker.as_bytes(),
        vec!["node-app-code-analyzer".to_string()],
        true,
        "z4-token-verified-biometric-001",
    )
    .expect("enrolling with Z4 token should succeed");
    assert!(enrolled.public_fingerprint_hex.starts_with("fp_blake3_"));

    // 3. Prove ZERO secret bytes appear in any serialized JSON or Debug output ([DUI-SETTINGS-001]).
    let all_refs = list_keychain_references_v2_core(&state).expect("list keychain refs");
    let serialized_json = serde_json::to_string(&all_refs).expect("serialize keychain refs");
    let debug_dump = format!("{state:?} {all_refs:?}");
    assert!(
        !serialized_json.contains(secret_marker) && !serialized_json.contains("TOP_SECRET_"),
        "serialized KeychainReference JSON must NEVER contain raw secret bytes"
    );
    assert!(
        !debug_dump.contains(secret_marker) && !debug_dump.contains("TOP_SECRET_"),
        "Debug output must NEVER contain raw secret bytes"
    );
    assert!(debug_dump.contains("<REDACTED_TCB_VAULT_SECRET>"));

    // 4. Acquire use-by-reference handle for authorized app vs unauthorized app.
    let unauthorized_err = acquire_keychain_use_by_reference_handle_core(
        &state,
        "kref-custom-prod-key",
        "app-rogue-untrusted",
    )
    .expect_err("unauthorized application must be denied keychain handle");
    assert!(
        unauthorized_err
            .to_string()
            .contains("KEYCHAIN_UNAUTHORIZED_APP")
    );

    let handle = acquire_keychain_use_by_reference_handle_core(
        &state,
        "kref-custom-prod-key",
        "node-app-code-analyzer",
    )
    .expect("authorized application should acquire use-by-reference handle");
    assert!(handle.handle_id_hex.starts_with("khdl_"));
    assert_eq!(handle.session_generation, 100);

    // 5. Locking DesktopSession wipes all active credential handle caches and bumps session_generation ([DUI-SESSION-001]).
    let (new_session_gen, wiped_count) =
        lock_desktop_session_and_wipe_caches_core(&state).expect("session lock should succeed");
    assert_eq!(new_session_gen, 101);
    assert_eq!(wiped_count, 1);

    let locked_err = acquire_keychain_use_by_reference_handle_core(
        &state,
        "kref-custom-prod-key",
        "node-app-code-analyzer",
    )
    .expect_err("cannot acquire handle while session is locked");
    assert!(locked_err.to_string().contains("SESSION_LOCKED"));

    // 6. Unlock session and verify one-click credential revocation.
    let unlocked_gen = unlock_desktop_session_core(&state).expect("unlock session");
    assert_eq!(unlocked_gen, 102);

    let revoked_ref = revoke_keychain_reference_core(&state, "kref-custom-prod-key", 1)
        .expect("revoke_keychain_reference_core should succeed");
    assert!(revoked_ref.revoked);
    assert_eq!(revoked_ref.expected_generation_or_revision, 2);

    let after_revoke_err = acquire_keychain_use_by_reference_handle_core(
        &state,
        "kref-custom-prod-key",
        "node-app-code-analyzer",
    )
    .expect_err("revoked keychain reference must reject handle acquisition");
    assert!(
        after_revoke_err
            .to_string()
            .contains("KEYCHAIN_REF_REVOKED")
    );
}
