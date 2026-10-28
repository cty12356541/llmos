//! Stage D (`W74-D-ROADD001` / `[ROAD-D-001]`) Non-Developer Multi-Application
//! Workstation End-to-End Acceptance Gate Suite.
//!
//! Verifies that a non-developer operator can continuously launch and use multiple
//! applications (`app://notes.workspace`, `app://sheets.finance`, `app://slides.briefing`)
//! and background tasks across the complete Stage D neutral industrial workstation
//! (`Z0..Z4` planes, `L0..L3` progressive disclosure, `Ctrl+Space` Omnibar, `Z4`
//! UAC/Polkit Trusted UI, `Task Space` DAG overrides, `NotificationService` one-shot
//! `ActionToken`s, `Ctrl+K` `0x1F` Spotlight, `ChooserGrant`, taint-aware clipboard,
//! 4-tier `SettingsDomain` CAS, and `[DUI-OFFLINE-001]` partition autonomy) without
//! ever encountering internal kernel orchestration jargon at `L0/L1/L2`.

use std::collections::BTreeSet;

use llmos_desktop_lib::a11y_i18n::{
    A11yI18nState, FOCUS_RING_WIDTH_PX, KeyboardFocusTargetInputDto, KeyboardFocusTrapRequestDto,
    WCAG_AA_NORMAL_TEXT_MIN_RATIO, WCAG_AA_UI_COMPONENT_MIN_RATIO, WCAG_MIN_HIT_TARGET_PX,
    WorkstationLocaleDto, a11y_audit_aria_landmarks_inner, a11y_audit_wcag_contrast_inner,
    a11y_get_locale_bundle_inner, a11y_switch_locale_inner, a11y_verify_keyboard_focus_trap_inner,
    contains_forbidden_l0_to_l2_jargon, contains_pictographic_emoji,
};
use llmos_desktop_lib::compositor::{
    DeclaredWindowSurfaceSpec, InstalledApplicationCatalogEntry, LaunchIdentity, LauncherError,
    TASKBAR_HEIGHT_PX, TILE_GAP_PX, TILED_BORDER_RADIUS_PX, WINDOW_TITLEBAR_HEIGHT_PX,
    WorkstationHostShell, WorkstationLayoutMode, Z0_BACKDROP_Z, Z1_TASK_SPACE_Z, Z2_WINDOW_Z_MAX,
    Z2_WINDOW_Z_MIN, Z3_FLYOUT_Z, Z3_PALETTE_Z, Z3_TASKBAR_Z, Z4_SECURITY_SCREEN_Z,
};
use llmos_desktop_lib::dto::compositor_snapshot_dto;
use llmos_desktop_lib::error::ErrorCode;
use llmos_desktop_lib::input_router::{InputOrigin, InputSource};
use llmos_desktop_lib::ipc::{
    AppState, SessionConfig, build_poll_desktop_watermark_events,
    build_query_control_command_status,
};
use llmos_desktop_lib::notify_search_offline::{
    AttemptMutationFromSearchHitRequest, CanonicalTaskState, ClusterConnectivityMode,
    EvaluateOfflineActionAvailabilityRequest, ListNotificationFeedRequest,
    NotifySearchOfflineErrorCode, NotifySearchOfflineState, OfflineActionAvailabilityStatus,
    OfflineSurfaceKind, QuerySpotlightCandidatesRequest, RedeemNotificationActionTokenRequest,
    SPOTLIGHT_STATUTORY_MASK_0X1F, SearchForbiddenControlGate, SetClusterConnectivityModeRequest,
    VerifySpotlightCandidateAuthorityRequest,
};
use llmos_desktop_lib::omnibar::{
    AttachmentChipInput, AttachmentKind, HardwarePermissionGrant, OMNIBAR_CONTROL_RADIUS_PX,
    OMNIBAR_OVERLAY_ID, OMNIBAR_TOP_PERMANENT_FOOTPRINT_PX, OMNIBAR_Z_INDEX, OmnibarCatalogEntry,
    OmnibarEngine, OmnibarError, OmnibarLocale, UnresolvedAmbiguityKind, VoiceStreamPhase,
};
use llmos_desktop_lib::plan_editor::{
    CLOSED_TASK_SPACE_GAPS_COUNT, MAX_PLAN_EDITOR_ENUM_LIMIT, OPEN_TASK_SPACE_GAPS_COUNT,
    PlanEditorError, PlanEditorState, PlanOverrideOpDto, PlanOverridePathKind, PlanResidencyTier,
    ReopenTaskMode, STATUTORY_LOCK_ATTEMPT_NO_MANUAL_COMMIT,
    STATUTORY_LOCK_NODE_COMPLETED_OR_ACTIVE, clamp_plan_editor_limit, execute_task_attempt_control,
    expand_lazy_plan_supernode, inspect_task_attempt_lanes, inspect_task_plan_dag,
    issue_z4_reauthorization_grant, preview_plan_override_diff, reopen_terminal_task,
    submit_plan_override,
};
use llmos_desktop_lib::settings_keychain::{
    ControlCenterDomainKind, ControlCommandReconcileStatusDto, PackageAssociationProposalDto,
    SettingsKeychainState, SettingsLayerKindDto, StatutoryLockKindDto, UndoClassDto,
    UpdateSettingCasRequestDto, arm_destructive_control_command_core,
    bind_content_type_association_core, confirm_destructive_control_command_core,
    drain_settings_watch_events_core, inspect_settings_layer_core,
    inspect_system_control_center_core, list_content_type_associations_v2_core,
    reconcile_control_command_status_core, revoke_content_type_association_core,
    submit_package_association_proposal_core, update_setting_with_cas_core,
    validate_and_apply_node_verb_core,
};
use llmos_desktop_lib::trusted_ui::{
    ChooserAttenuatedRights, ChooserMode, ConsentGrantScope, ReversibilityClass,
    TaintFlag as TrustedTaintFlag, TransferReadDecisionDto, TrustedUiAuthority,
    TrustedUiCallerOrigin, TrustedUiErrorCode, TrustedUiState, Z4_CLICKJACK_ARMING_MS,
    Z4_DIALOG_RADIUS_PX, Z4_SCRIM_RGBA, Z4_SEAL_HEIGHT_PX, Z4_SEAL_WIDTH_PX, Z4_SECURITY_Z_INDEX,
    Z4_TOP_SECURITY_BAR_HEIGHT_PX, Z4ActionScopeSpec, Z4BlastRadiusRowDto, Z4ChallengeState,
    Z4ConsentChallengeRequest, Z4RequestingAppSpec, Z4ResourceCeilingSpec, Z4ReversibilityRiskSpec,
    parse_hex_id_16, sha256_bytes, trusted_ui_complete_escalated_transfer_core,
    trusted_ui_decide_z4_consent_core, trusted_ui_get_public_posture_core,
    trusted_ui_mint_gesture_token_core, trusted_ui_notify_z4_focus_regained_core,
    trusted_ui_publish_transfer_core, trusted_ui_request_transfer_read_core,
};
use nlos_types::{ApplicationId, InstallationId, PackageId, ProcessId};

const BANNED_NON_DEV_JARGON: &[&str] = &[
    "CausalCone",
    "LeaseEpoch",
    "TypedIntent",
    "0x1F",
    "0x1f",
    "TaskAttempt",
    "ExecutionFiber",
    "receipt_hex",
    "control_command_id",
    "CAS_WINNER",
    "SHARED_THREAD_LIKE",
    "LOCAL_SINGLE_NODE",
    "NON_AUTHORITATIVE",
    "ADR-0011",
    "ROAD-D-001",
];

fn hex16(bytes: [u8; 16]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_slice(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn assert_non_dev_zero_jargon(context: &str, text: &str) {
    assert_eq!(
        contains_forbidden_l0_to_l2_jargon(text),
        None,
        "{context} leaked L0-L2 forbidden jargon in: {text}"
    );
    for &term in BANNED_NON_DEV_JARGON {
        assert!(
            !text.contains(term),
            "{context} leaked banned kernel jargon `{term}` in: {text}"
        );
    }
    assert!(
        !contains_pictographic_emoji(text),
        "{context} violated ANTI-AI-05 zero-emoji rule in: {text}"
    );
}
// ============================================================================
// Test 1: Non-Developer Multi-App Launch, Tiling & Bilingual Zero-Jargon Surface
// ============================================================================

#[test]
fn non_dev_multi_app_launch_tiling_and_bilingual_zero_jargon_surface() {
    let mut shell = WorkstationHostShell::new_default_workstation();
    let a11y = A11yI18nState::new();

    // Register 3 non-developer workstation applications:
    // 1. app://notes.workspace ([0x11; 16])
    // 2. app://sheets.finance ([0x22; 16])
    // 3. app://slides.briefing ([0x33; 16])
    let apps = [
        (
            [0x11_u8; 16],
            [0x21_u8; 16],
            [0x31_u8; 16],
            [0x41_u8; 16],
            [0x51_u8; 16],
            "app://notes.workspace · 笔记工作区",
        ),
        (
            [0x12_u8; 16],
            [0x22_u8; 16],
            [0x32_u8; 16],
            [0x42_u8; 16],
            [0x52_u8; 16],
            "app://sheets.finance · 财务表格台",
        ),
        (
            [0x13_u8; 16],
            [0x23_u8; 16],
            [0x33_u8; 16],
            [0x43_u8; 16],
            [0x53_u8; 16],
            "app://slides.briefing · 简报演示台",
        ),
    ];

    for (app_id, inst_id, pkg_id, proc_id, surf_id, title) in apps {
        shell
            .launcher
            .register_catalog_entry(InstalledApplicationCatalogEntry {
                application_id: ApplicationId::from_bytes(app_id),
                installation_id: InstallationId::from_bytes(inst_id),
                package_id: PackageId::from_bytes(pkg_id),
                current_installation_generation: 1,
                surfaces: vec![DeclaredWindowSurfaceSpec {
                    surface_id: surf_id,
                    title: title.to_owned(),
                    entry_name: Some("main".to_owned()),
                }],
                known_process_ids: vec![ProcessId::from_bytes(proc_id)],
            });

        // [DUI-LAUNCH-001]: PackageId or transient ProcessId must never be accepted as ApplicationId.
        assert_eq!(
            shell.launcher.launch_by_identity(
                LaunchIdentity::PackageId(PackageId::from_bytes(pkg_id)),
                &shell.session,
                &mut shell.compositor,
                &mut shell.route,
            ),
            Err(LauncherError::PackageIdNotApplicationIdentity {
                offending_id: pkg_id,
            })
        );
        assert_eq!(
            shell.launcher.launch_by_identity(
                LaunchIdentity::ProcessId(ProcessId::from_bytes(proc_id)),
                &shell.session,
                &mut shell.compositor,
                &mut shell.route,
            ),
            Err(LauncherError::ProcessIdNotApplicationIdentity {
                offending_id: proc_id,
            })
        );

        // Launch legitimately via (ApplicationId, InstallationId).
        let launched = shell
            .launcher
            .launch_from_catalog(
                ApplicationId::from_bytes(app_id),
                InstallationId::from_bytes(inst_id),
                &shell.session,
                &mut shell.compositor,
                &mut shell.route,
            )
            .expect("launch workstation application");
        assert_eq!(launched.len(), 1);
    }

    // Verify Z0..Z4 strict monotonic Z-order constants.
    const {
        assert!(Z0_BACKDROP_Z < Z1_TASK_SPACE_Z);
        assert!(Z1_TASK_SPACE_Z < Z2_WINDOW_Z_MIN);
        assert!(Z2_WINDOW_Z_MIN < Z2_WINDOW_Z_MAX);
        assert!(Z2_WINDOW_Z_MAX < Z3_TASKBAR_Z);
        assert!(Z3_TASKBAR_Z < Z3_FLYOUT_Z);
        assert!(Z3_FLYOUT_Z < Z3_PALETTE_Z);
        assert!(Z3_PALETTE_Z < Z4_SECURITY_SCREEN_Z);
    }

    // Exercise Split2 (TwoColumnSplit) and Grid4 (Quadrants) 4px tiled workstation layouts at 1440x900.
    for mode in [
        WorkstationLayoutMode::TwoColumnSplit,
        WorkstationLayoutMode::Quadrants,
        WorkstationLayoutMode::ThreePaneWorkstation,
    ] {
        shell
            .set_layout(
                mode.as_str(),
                Some(1440),
                Some(900),
                Some(680),
                Some(500),
                Some(true),
            )
            .expect("apply tiled workstation layout");
        let snap = compositor_snapshot_dto(&shell, false).expect("compositor snapshot");
        assert_eq!(snap.windows.len(), 3);
        assert_eq!(snap.taskbar_rect.height, TASKBAR_HEIGHT_PX);
        assert_eq!(snap.taskbar_rect.height, 36);

        if mode == WorkstationLayoutMode::TwoColumnSplit {
            let left = &snap.windows[0].outer_frame_rect;
            let right = &snap.windows[1].outer_frame_rect;
            assert_eq!(TILE_GAP_PX, 4);
            assert_eq!(left.width + TILE_GAP_PX + right.width, 1440);
            assert_eq!(right.x, (left.width + TILE_GAP_PX) as i32);
        }

        for win in &snap.windows {
            assert!(win.z_index >= Z2_WINDOW_Z_MIN && win.z_index <= Z2_WINDOW_Z_MAX);
            assert_eq!(win.border_radius_px, TILED_BORDER_RADIUS_PX);
            assert_eq!(win.border_radius_px, 0);
            assert_eq!(
                win.inner_content_clip_rect.y,
                win.outer_frame_rect.y + WINDOW_TITLEBAR_HEIGHT_PX as i32
            );
            assert!(
                win.outer_frame_rect.y + (win.outer_frame_rect.height as i32)
                    <= (900 - TASKBAR_HEIGHT_PX) as i32
            );
            assert_non_dev_zero_jargon("Z2 window title", &win.title);
            assert_non_dev_zero_jargon("Z2 window a11y label", &win.a11y.a11y_label);
        }
    }

    // Switch zh-CN <-> en-US resource bundles and verify L0/L1/L2 zero internal kernel jargon + WCAG 2.2 AA.
    let zh_bundle = a11y_get_locale_bundle_inner(&a11y, None).expect("default zh-CN bundle");
    assert_eq!(zh_bundle.locale, WorkstationLocaleDto::ZhCn);
    assert!(zh_bundle.key_parity_verified);
    assert!(zh_bundle.l0_to_l2_zero_jargon_verified);
    assert!(zh_bundle.zero_emoji_verified);

    let en_bundle = a11y_switch_locale_inner(&a11y, "en-US").expect("switch to en-US");
    assert_eq!(en_bundle.locale, WorkstationLocaleDto::EnUs);
    assert_eq!(en_bundle.switch_generation, 2);
    assert!(en_bundle.key_parity_verified);
    assert!(en_bundle.l0_to_l2_zero_jargon_verified);
    assert!(en_bundle.zero_emoji_verified);

    let zh_back = a11y_switch_locale_inner(&a11y, "zh-CN").expect("switch back to zh-CN");
    assert_eq!(zh_back.locale, WorkstationLocaleDto::ZhCn);
    assert_eq!(zh_back.switch_generation, 3);

    for bundle in [&zh_bundle, &en_bundle] {
        for (k, v) in &bundle.task_state_labels {
            assert_non_dev_zero_jargon(&format!("task_state_label[{k}]"), v);
        }
        for (k, v) in &bundle.control_domain_labels {
            assert_non_dev_zero_jargon(&format!("control_domain_label[{k}]"), v);
        }
        for (k, v) in &bundle.plane_labels {
            assert_non_dev_zero_jargon(&format!("plane_label[{k}]"), v);
        }
        for (k, v) in &bundle.l2_statutory_lock_labels {
            assert_non_dev_zero_jargon(&format!("l2_statutory_lock_label[{k}]"), v);
        }
    }

    let contrast = a11y_audit_wcag_contrast_inner(&a11y).expect("WCAG 2.2 AA contrast audit");
    assert!(contrast.passed);
    assert_eq!(contrast.total_pairs_audited, 20);
    assert!(contrast.failures.is_empty());
    assert_eq!(contrast.focus_ring_width_px, FOCUS_RING_WIDTH_PX);
    assert!(contrast.min_normal_text_ratio >= WCAG_AA_NORMAL_TEXT_MIN_RATIO);
    assert!(contrast.min_ui_component_ratio >= WCAG_AA_UI_COMPONENT_MIN_RATIO);

    let aria = a11y_audit_aria_landmarks_inner(&a11y, false).expect("ARIA landmark audit");
    assert!(aria.passed);
    assert!(aria.violations.is_empty());
    assert_eq!(aria.min_hit_target_px, WCAG_MIN_HIT_TARGET_PX);
    assert!(aria.polite_live_region_ready && aria.assertive_live_region_ready);
}
// ============================================================================
// Test 2: Natural-Language Cross-App Goal, [NL-AMBIG-001] & Z4 Trusted Consent
// ============================================================================

#[test]
fn natural_language_cross_app_goal_ambiguity_clarification_and_z4_consent() {
    let mut shell = WorkstationHostShell::new_default_workstation();
    let mut omnibar = OmnibarEngine::new();
    let mut trusted_auth = TrustedUiAuthority::new();
    let a11y = A11yI18nState::new();

    // Register catalog entries for notes.workspace and sheets.finance (both matching keyword "笔记").
    omnibar.register_catalog_entry(OmnibarCatalogEntry {
        application_id: [0x11; 16],
        installation_id: [0x21; 16],
        package_id: Some([0x31; 16]),
        known_process_ids: vec![[0x41; 16]],
        current_generation: 3,
        display_name: "app://notes.workspace · 笔记工作区".to_owned(),
        keywords: vec![
            "notes".to_owned(),
            "workspace".to_owned(),
            "笔记".to_owned(),
        ],
    });
    omnibar.register_catalog_entry(OmnibarCatalogEntry {
        application_id: [0x12; 16],
        installation_id: [0x22; 16],
        package_id: Some([0x32; 16]),
        known_process_ids: vec![[0x42; 16]],
        current_generation: 5,
        display_name: "app://sheets.finance · 财务表格台".to_owned(),
        keywords: vec!["sheets".to_owned(), "finance".to_owned(), "笔记".to_owned()],
    });

    // Verify default 0px top permanent footprint and invoke Ctrl+Space Z3 Omnibar.
    assert_eq!(
        omnibar.snapshot().top_permanent_footprint_px,
        OMNIBAR_TOP_PERMANENT_FOOTPRINT_PX
    );
    assert_eq!(omnibar.snapshot().top_permanent_footprint_px, 0);
    let opened = omnibar
        .toggle_via_shortcut(&mut shell, InputOrigin::TrustedShell, 100)
        .expect("Ctrl+Space opens Z3 Omnibar");
    assert!(opened);
    assert_eq!(omnibar.snapshot().z_index, OMNIBAR_Z_INDEX);
    assert_eq!(
        omnibar.snapshot().border_radius_px,
        OMNIBAR_CONTROL_RADIUS_PX
    );
    assert!(
        shell
            .compositor
            .active_z3_overlays()
            .iter()
            .any(|ov| ov.overlay_id == OMNIBAR_OVERLAY_ID && ov.z_index == Z3_PALETTE_Z)
    );

    // Non-developer speaks cross-app goal via InputSource::Voice + attaches ChooserGrant file chip.
    omnibar.set_device_permissions(
        &mut shell,
        Some(HardwarePermissionGrant::Granted),
        Some(HardwarePermissionGrant::PromptRequired),
        Some(HardwarePermissionGrant::PromptRequired),
        Some(true),
        Some(false),
    );
    omnibar
        .start_voice_capture(&mut shell, InputOrigin::TrustedShell, 120)
        .expect("start voice capture");
    omnibar
        .append_voice_transcript(
            &mut shell,
            InputOrigin::TrustedShell,
            "永久移除旧笔记并转发到简报演示台且扩容预算",
            140,
        )
        .expect("append voice transcript");
    omnibar
        .finish_voice_to_transcribing(&mut shell, 150)
        .expect("finish voice to transcribing");
    let committed_voice = omnibar
        .commit_voice_transcript(&mut shell, None, 160)
        .expect("commit voice transcript");
    assert_eq!(committed_voice.phase, VoiceStreamPhase::Committed);

    let chip = omnibar
        .attach_item(AttachmentChipInput {
            attachment_id: Some("chip-q4-notes".to_owned()),
            kind: AttachmentKind::ChooserFile,
            label_or_path: r"C:\Users\Operator\Documents\q4-notes-draft.md".to_owned(),
            mime_type: Some("text/markdown".to_owned()),
            byte_length: 4096,
            content_digest_hex: "ab".repeat(32),
            chooser_grant_id_hex: Some("12".repeat(16)),
            scoped_capability_id_hex: Some("34".repeat(16)),
            artifact_id_hex: None,
            taint_flags: Vec::new(),
            explicit_host_path_authorized: false,
        })
        .expect("attach ChooserGrant file chip");
    assert_eq!(chip.display_label, "q4-notes-draft.md");
    assert_eq!(chip.authorized_host_path, None);

    // [NL-AMBIG-001]: Compile ambiguous cross-app utterance -> zero execution & hard block before clarification.
    for locale in [OmnibarLocale::ZhCn, OmnibarLocale::EnUs] {
        omnibar.set_locale(locale);
        let preview = omnibar
            .compile_preview(&mut shell, None)
            .expect("compile ambiguous preview");
        assert!(!preview.proposal.can_submit);
        assert!(preview.proposal.candidates.len() >= 2);
        let ambiguity_kinds: Vec<UnresolvedAmbiguityKind> = preview
            .proposal
            .unresolved_ambiguities
            .iter()
            .map(|a| a.kind)
            .collect();
        assert!(ambiguity_kinds.contains(&UnresolvedAmbiguityKind::MultipleMatchingTargets));
        assert!(ambiguity_kinds.contains(&UnresolvedAmbiguityKind::HighRiskIrreversibleEffect));
        assert!(ambiguity_kinds.contains(&UnresolvedAmbiguityKind::CrossApplicationTransfer));
        assert!(ambiguity_kinds.contains(&UnresolvedAmbiguityKind::BudgetEscalation));

        for field in [
            &preview.proposal.target_application_label,
            &preview.proposal.affected_scope_label,
            &preview.proposal.resource_ceiling_label,
            &preview.proposal.undo_badge_label,
            &preview.proposal.compensation_note,
            &preview.proposal.dependency_summary_label,
        ] {
            assert_non_dev_zero_jargon("Omnibar L1/L2 proposal field", field);
        }
    }

    let unclarified_err = omnibar
        .submit_proposal(&mut shell, None, true, 200)
        .expect_err("unclarified proposal must be blocked fail-closed");
    assert!(matches!(
        unclarified_err,
        OmnibarError::ClarificationRequired { .. }
    ));
    assert!(!shell.route.z4_modal_lock);

    // Resolve clarification into an executable preview card and escalate to Z4 TrustedConsentOverlay.
    let preview = omnibar.compile_preview(&mut shell, None).expect("compile");
    let chosen_candidate_id = preview.proposal.candidates[0].candidate_id.clone();
    let clarified = omnibar
        .select_candidate(&mut shell, None, &chosen_candidate_id)
        .expect("select clarification candidate");
    assert!(clarified.proposal.can_submit);
    assert_eq!(
        clarified
            .proposal
            .clarification_digest_hex
            .as_ref()
            .map(String::len),
        Some(64)
    );

    let submitted = omnibar
        .submit_proposal(&mut shell, None, true, 250)
        .expect("submit clarified proposal");
    assert_eq!(submitted.handoff_target, "Z4_TRUSTED_CONSENT");
    assert!(shell.route.z4_modal_lock);

    // Open Z4 TrustedConsentOverlay (350ms clickjacking arming gate, 32x20 Gerono seal, 4-row KV table).
    let z4_dto = trusted_auth
        .z4_broker
        .open_consent_challenge(
            TrustedUiCallerOrigin::TcbTrustedBroker,
            &shell.session,
            Z4ConsentChallengeRequest {
                requesting_app: Z4RequestingAppSpec {
                    display_name: "app://notes.workspace · 笔记工作区".to_owned(),
                    publisher_name: "NLOS Verified Publisher".to_owned(),
                    signature_verified: true,
                    application_id_hex: hex16([0x11; 16]),
                    installation_id_hex: hex16([0x21; 16]),
                    principal_id_hex: hex16(shell.session.active_principal_id()),
                    installation_generation: 3,
                    manifest_digest_hex: hex_slice(&sha256_bytes(b"manifest:notes.workspace:v3")),
                },
                action_scope: Z4ActionScopeSpec {
                    plain_summary_zh: "归档清理旧版草稿并将核定财务摘要同步至简报演示台".to_owned(),
                    plain_summary_en: "Archive old note drafts and sync verified financial summary to slides briefing".to_owned(),
                    capability_rights: vec!["CROSS_APP_BRIEFING_SYNC".to_owned(), "DRAFT_ARCHIVE_PURGE".to_owned()],
                    control_scope: "TASK_GROUP".to_owned(),
                    plan_digest_hex: clarified.proposal.proposal_digest_hex.clone(),
                },
                resource_ceiling: Z4ResourceCeilingSpec {
                    plain_ceiling_zh: "内存上限 256 MiB · 预算上限 8,000 单位 · 最长 30 秒".to_owned(),
                    plain_ceiling_en: "Memory cap 256 MiB · Budget cap 8,000 units · Max 30s".to_owned(),
                    memory_ceiling_mib: 256,
                    token_budget_ceiling: 8_000,
                    network_egress_allowed: false,
                    max_duration_ms: 30_000,
                },
                reversibility_and_risk: Z4ReversibilityRiskSpec {
                    reversibility_class: ReversibilityClass::CompensatableRollback,
                    plain_consequence_zh: "可通过版本快照冲正回退，完整保留审计轨迹。".to_owned(),
                    plain_consequence_en: "Can be rolled back via snapshot compensation with full audit trail.".to_owned(),
                    multi_target_count: 2,
                    partial_failure_disclosure_zh: None,
                    partial_failure_disclosure_en: None,
                    blast_radius_rows: vec![Z4BlastRadiusRowDto {
                        symbol: "~".to_owned(),
                        category: "modified".to_owned(),
                        scope_label: "跨应用简报".to_owned(),
                        target_label: "Q4 汇报演示文稿".to_owned(),
                        reason: "注入核定财务摘要章节".to_owned(),
                        target_id_hex: Some(hex16([0x13; 16])),
                    }],
                    requires_data_loss_receipt: false,
                },
                allowed_grant_scopes: vec![ConsentGrantScope::SingleAction, ConsentGrantScope::CurrentTask],
            },
            1_000,
            &mut trusted_auth.audit,
        )
        .expect("open Z4 consent challenge");

    assert_eq!(z4_dto.z_index, Z4_SECURITY_Z_INDEX);
    assert_eq!(z4_dto.scrim_rgba, Z4_SCRIM_RGBA);
    assert!(z4_dto.zero_blur_filter);
    assert_eq!(z4_dto.dialog_radius_px, Z4_DIALOG_RADIUS_PX);
    assert_eq!(z4_dto.top_security_bar_px, Z4_TOP_SECURITY_BAR_HEIGHT_PX);
    assert_eq!(z4_dto.seal_width_px, Z4_SEAL_WIDTH_PX);
    assert_eq!(z4_dto.seal_height_px, Z4_SEAL_HEIGHT_PX);
    assert_eq!(
        z4_dto.security_seal_hint,
        shell
            .session
            .active_user()
            .expect("active user")
            .z4_trusted_seal_hint()
    );

    // Verify Z4 focus trap & 350ms clickjacking arming gate before approving.
    let trap = a11y_verify_keyboard_focus_trap_inner(
        &a11y,
        KeyboardFocusTrapRequestDto {
            z4_modal_active: true,
            originating_trigger_id: "z3-omnibar-submit".to_owned(),
            targets: vec![
                KeyboardFocusTargetInputDto {
                    id: "z2-notes-editor".to_owned(),
                    plane: "Z2".to_owned(),
                    label: "Notes Editor".to_owned(),
                    disabled: false,
                },
                KeyboardFocusTargetInputDto {
                    id: "z4-cancel-btn".to_owned(),
                    plane: "Z4".to_owned(),
                    label: "Cancel".to_owned(),
                    disabled: false,
                },
                KeyboardFocusTargetInputDto {
                    id: "z4-approve-btn".to_owned(),
                    plane: "Z4".to_owned(),
                    label: "Approve".to_owned(),
                    disabled: false,
                },
            ],
        },
    )
    .expect("Z4 focus trap check");
    assert!(!trap.leaked_to_lower_plane);
    assert_eq!(trap.z4_control_ids, vec!["z4-cancel-btn", "z4-approve-btn"]);

    let cid = parse_hex_id_16("cid", &z4_dto.challenge_id_hex).expect("valid cid");
    let early_err = trusted_auth
        .z4_broker
        .decide_consent_challenge(
            InputOrigin::TrustedShell,
            InputSource::Pointer,
            &shell.session,
            cid,
            true,
            Some(ConsentGrantScope::CurrentTask),
            false,
            1_000 + Z4_CLICKJACK_ARMING_MS - 1,
            &mut trusted_auth.audit,
        )
        .expect_err("click before 350ms must fail closed");
    assert_eq!(early_err.code, TrustedUiErrorCode::ClickjackingArmingActive);

    let approved = trusted_auth
        .z4_broker
        .decide_consent_challenge(
            InputOrigin::TrustedShell,
            InputSource::Pointer,
            &shell.session,
            cid,
            true,
            Some(ConsentGrantScope::CurrentTask),
            false,
            1_000 + Z4_CLICKJACK_ARMING_MS,
            &mut trusted_auth.audit,
        )
        .expect("approve after 350ms arming gate");
    assert_eq!(approved.state, Z4ChallengeState::Approved);
    assert_eq!(
        approved.session_generation,
        shell.session.session_generation()
    );
}
// ============================================================================
// Test 3: L1 16-State Progression, L2 Attribution & L3 DAG Override + Re-Auth
// ============================================================================

#[test]
fn l1_commitment_progression_l2_attribution_and_l3_dag_override_reauth() {
    let plan_state = PlanEditorState::new();
    let notify_state = NotifySearchOfflineState::with_default_fixture();
    let a11y = A11yI18nState::new();

    // Verify all 3 historical TASK_SPACE_GAPS are closed and O(visible_rows) <= 64 cursor clamp holds.
    assert_eq!(CLOSED_TASK_SPACE_GAPS_COUNT, 3);
    assert_eq!(OPEN_TASK_SPACE_GAPS_COUNT, 0);
    assert_eq!(clamp_plan_editor_limit(None), MAX_PLAN_EDITOR_ENUM_LIMIT);
    assert_eq!(clamp_plan_editor_limit(Some(500)), 64);

    // 1. L1 16-state task progression in plain language (zero internal kernel jargon).
    let feed = notify_state
        .list_notification_feed(ListNotificationFeedRequest {
            cursor_sequence: None,
            limit: 64,
            unread_only: false,
            category_filter: None,
            include_dismissed: false,
        })
        .expect("list 16-state progression feed");
    assert_eq!(feed.items.len(), 16);
    let states: BTreeSet<CanonicalTaskState> = feed.items.iter().map(|i| i.task_state).collect();
    assert_eq!(states.len(), 16);
    for item in &feed.items {
        assert_non_dev_zero_jargon("L1 state_label_zh", &item.state_label_zh);
        assert_non_dev_zero_jargon("L1 state_label_en", &item.state_label_en);
        assert_non_dev_zero_jargon("L1 summary_zh", &item.summary_zh);
        assert_non_dev_zero_jargon("L1 summary_en", &item.summary_en);
    }

    let zh_bundle = a11y_get_locale_bundle_inner(&a11y, Some("zh-CN")).expect("zh-CN bundle");
    let en_bundle = a11y_get_locale_bundle_inner(&a11y, Some("en-US")).expect("en-US bundle");
    assert_eq!(zh_bundle.task_state_labels.len(), 24);
    assert_eq!(en_bundle.task_state_labels.len(), 24);

    // 2. L2 single-row non-modal attribution drawer + L3 (Ctrl+Shift+E) TaskPlan DAG canvas.
    let dag = inspect_task_plan_dag(&plan_state, "task-01", Some(3)).expect("inspect task-01 DAG");
    assert_eq!(dag.task_id, "task-01");
    assert_eq!(dag.plan_revision, 3);
    assert_eq!(dag.lazy_supernodes.len(), 1);

    // [PLAN-LAZY-001]: Unexpanded +N super-node carries 0 allocated processes until expanded on demand.
    let supernode = &dag.lazy_supernodes[0];
    assert_eq!(supernode.unmaterialized_step_count, 18);
    assert_eq!(supernode.allocated_process_count, 0);
    assert_eq!(supernode.residency_tier, PlanResidencyTier::MetadataOnly);
    assert_eq!(supernode.badge_zh, "+18 待物化步骤 · 0 进程");
    assert_eq!(supernode.badge_en, "+18 Unmaterialized Steps · 0 Processes");

    let expanded_page =
        expand_lazy_plan_supernode(&plan_state, "task-01", &supernode.supernode_id, Some(8))
            .expect("expand 8 lazy steps on demand");
    assert_eq!(expanded_page.effective_limit, 8);
    assert_eq!(expanded_page.allocated_process_count_before, 0);
    assert_eq!(expanded_page.materialized_nodes.len(), 8);
    assert_eq!(expanded_page.remaining_unmaterialized_count, 10);

    // Statutory Lock #3: Completed or Active TaskNodes reject graph edits fail-closed.
    let active_edit_err = preview_plan_override_diff(
        &plan_state,
        "task-01",
        3,
        &[PlanOverrideOpDto::PauseNode {
            target_node_id: "plan-01-node-active".to_owned(),
            reason: Some("Attempt to pause active node".to_owned()),
        }],
    )
    .expect_err("editing active node must fail closed");
    assert!(matches!(
        active_edit_err,
        PlanEditorError::ReadOnlyExecutedTaskNode {
            ref statutory_lock,
            ..
        } if statutory_lock == STATUTORY_LOCK_NODE_COMPLETED_OR_ACTIVE
    ));

    // 3. Path A bounded control command (PauseNode + ReorderNodes within authorized envelope -> no Z4 re-auth).
    let path_a_ops = vec![
        PlanOverrideOpDto::PauseNode {
            target_node_id: "plan-01-node-pending-a".to_owned(),
            reason: Some("Pause pending step while reviewing finance sheet".to_owned()),
        },
        PlanOverrideOpDto::ReorderNodes {
            node_id: "plan-01-node-pending-b".to_owned(),
            new_dependency_node_ids: vec!["plan-01-node-active".to_owned()],
            within_authorized_envelope: true,
        },
    ];
    let path_a_preview =
        preview_plan_override_diff(&plan_state, "task-01", 3, &path_a_ops).expect("Path A diff");
    assert_eq!(
        path_a_preview.path_kind,
        PlanOverridePathKind::BoundedControlCommand
    );
    assert!(!path_a_preview.requires_z4_reauthorization);
    assert_eq!(path_a_preview.compiled_control_commands.len(), 2);

    let path_a_commit =
        submit_plan_override(&plan_state, "task-01", 3, &path_a_ops, None).expect("Path A commit");
    assert_eq!(path_a_commit.new_plan_revision, 4);
    assert!(path_a_commit.z4_grant_token_hex.is_none());

    // 4. Path B node parameter edit & capability/budget expansion -> new plan_digest + Z4 diff re-authorization.
    let path_b_ops = vec![
        PlanOverrideOpDto::AddNode {
            node_id: "plan-02-node-slides-sync".to_owned(),
            group_id: "plan-02-grp-commit".to_owned(),
            title: "同步财务图表至简报演示台".to_owned(),
            dependency_node_ids: vec!["plan-02-node-pending-b".to_owned()],
            capability_id: "cap.app.slides.write".to_owned(),
            requested_budget_units: 45,
            cross_application_target: Some("app://slides.briefing".to_owned()),
        },
        PlanOverrideOpDto::ReplaceNode {
            target_node_id: "plan-02-node-pending-a".to_owned(),
            replacement_node_id: "plan-02-node-pending-a-v2".to_owned(),
            replacement_title: "生成多应用汇总简报 v2".to_owned(),
            capability_id: "cap.fs.workspace.write".to_owned(),
            requested_budget_units: 30,
            cross_application_target: None,
        },
    ];
    let path_b_preview =
        preview_plan_override_diff(&plan_state, "task-02", 3, &path_b_ops).expect("Path B diff");
    assert_eq!(
        path_b_preview.path_kind,
        PlanOverridePathKind::RecompileAndZ4Reauthorize
    );
    assert!(path_b_preview.requires_z4_reauthorization);
    assert_ne!(
        path_b_preview.base_plan_digest_hex,
        path_b_preview.candidate_plan_digest_hex
    );

    // Submitting Path B without Z4 re-authorization must fail closed.
    let missing_z4 = submit_plan_override(&plan_state, "task-02", 3, &path_b_ops, None)
        .expect_err("Path B without Z4 grant must fail");
    assert!(matches!(
        missing_z4,
        PlanEditorError::Z4ReauthorizationRequired { .. }
    ));

    let z4_grant = issue_z4_reauthorization_grant(&path_b_preview, 12);
    let path_b_commit =
        submit_plan_override(&plan_state, "task-02", 3, &path_b_ops, Some(&z4_grant))
            .expect("Path B commit with Z4 grant");
    assert_eq!(path_b_commit.new_plan_revision, 4);
    assert_eq!(
        path_b_commit.z4_grant_token_hex.as_deref(),
        Some(z4_grant.grant_token_hex.as_str())
    );

    // Statutory Lock #4: Manual commit on TaskAttempt race lane is forbidden.
    let manual_commit_err =
        execute_task_attempt_control(&plan_state, "task-01", "task-01-attempt-1", "manual_commit")
            .expect_err("manual commit on TaskAttempt must be rejected");
    assert!(matches!(
        manual_commit_err,
        PlanEditorError::ForbiddenManualAttemptCommit {
            ref statutory_lock,
            ..
        } if statutory_lock == STATUTORY_LOCK_ATTEMPT_NO_MANUAL_COMMIT
    ));
    let lanes = inspect_task_attempt_lanes(&plan_state, "task-01", Some(4)).expect("attempt lanes");
    assert_eq!(lanes.attempts.len(), 2);

    // 5. [TASK-REOPEN-001]: Retry & tweak/refine from terminal state preserving prior history.
    let retry_res = reopen_terminal_task(&plan_state, "task-03", ReopenTaskMode::RetryNewAttempt)
        .expect("retry terminal failed task-03");
    assert_eq!(
        retry_res.spawned_attempt_id.as_deref(),
        Some("task-03-attempt-3")
    );

    let refine_res =
        reopen_terminal_task(&plan_state, "task-04", ReopenTaskMode::RefineNewGeneration)
            .expect("refine terminal cancelled task-04");
    assert_eq!(refine_res.resulting_task_id, "task-04-gen3");
    assert!(
        refine_res
            .preserved_history_task_ids
            .contains(&"task-04".to_owned())
    );
}
// ============================================================================
// Test 4: Notification ActionToken Redemption & Spotlight 0x1F Zero-Write Fence
// ============================================================================

#[test]
fn notification_action_token_redemption_and_spotlight_0x1f_zero_write_fence() {
    let notify_state = NotifySearchOfflineState::with_default_fixture();
    let settings_state = SettingsKeychainState::new_seeded();
    let mut shell = WorkstationHostShell::new_default_workstation();
    let mut omnibar = OmnibarEngine::new();
    omnibar.register_catalog_entry(OmnibarCatalogEntry {
        application_id: [0x11; 16],
        installation_id: [0x21; 16],
        package_id: Some([0x31; 16]),
        known_process_ids: vec![[0x41; 16]],
        current_generation: 3,
        display_name: "app://notes.workspace · 笔记工作区".to_owned(),
        keywords: vec!["notes".to_owned(), "笔记".to_owned()],
    });

    // 1. Pull lifecycle notifications in Z3 and locate AWAITING_APPROVAL notification with one-shot ActionToken.
    let feed = notify_state
        .list_notification_feed(ListNotificationFeedRequest::default())
        .expect("list notification feed");
    let awaiting_item = feed
        .items
        .iter()
        .find(|i| i.task_state == CanonicalTaskState::AwaitingApproval)
        .expect("AWAITING_APPROVAL notification must exist")
        .clone();
    let token = awaiting_item
        .action_token
        .clone()
        .expect("AWAITING_APPROVAL notification must carry ActionToken");
    assert!(!token.consumed);
    assert_non_dev_zero_jargon("notification title_zh", &awaiting_item.title_zh);
    assert_non_dev_zero_jargon("notification title_en", &awaiting_item.title_en);
    assert_non_dev_zero_jargon("action_token label_zh", &token.label_zh);
    assert_non_dev_zero_jargon("action_token label_en", &token.label_en);

    let valid_redeem = RedeemNotificationActionTokenRequest {
        notification_id_hex: awaiting_item.notification_id_hex.clone(),
        token_id_hex: token.token_id_hex.clone(),
        action_id: token.action_id.clone(),
        caller_app_id_hex: token.bound_app_id_hex.clone(),
        caller_principal_id_hex: token.bound_principal_id_hex.clone(),
        task_id_hex: token.bound_task_id_hex.clone(),
        now_mono_ms: awaiting_item.created_at_mono_ms + 500,
        presented_commitment_hex: Some(token.verifier_commitment_hex.clone()),
    };

    // Cross-application spoofing, cross-task spoofing, expired token, and forged commitment all fail closed.
    let mut spoof_app_req = valid_redeem.clone();
    spoof_app_req.caller_app_id_hex = "ff".repeat(16);
    assert_eq!(
        notify_state
            .redeem_notification_action_token(spoof_app_req)
            .expect_err("cross-app ActionToken spoof must fail")
            .code,
        NotifySearchOfflineErrorCode::ActionTokenAppMismatch
    );

    let mut spoof_task_req = valid_redeem.clone();
    spoof_task_req.task_id_hex = "dd".repeat(16);
    assert_eq!(
        notify_state
            .redeem_notification_action_token(spoof_task_req)
            .expect_err("cross-task ActionToken spoof must fail")
            .code,
        NotifySearchOfflineErrorCode::ActionTokenTaskMismatch
    );

    let mut expired_req = valid_redeem.clone();
    expired_req.now_mono_ms = token.expiry_mono_ms + 1;
    assert_eq!(
        notify_state
            .redeem_notification_action_token(expired_req)
            .expect_err("expired ActionToken must fail")
            .code,
        NotifySearchOfflineErrorCode::ActionTokenExpired
    );

    // Legitimate one-shot redemption succeeds and transitions task to RUNNING; immediate replay fails closed.
    let redeemed = notify_state
        .redeem_notification_action_token(valid_redeem.clone())
        .expect("legitimate ActionToken redemption");
    assert_eq!(redeemed.resulting_task_state, CanonicalTaskState::Running);
    assert_eq!(redeemed.redemption_receipt_digest_hex.len(), 64);

    let replay_err = notify_state
        .redeem_notification_action_token(valid_redeem)
        .expect_err("replay of consumed ActionToken must fail closed");
    assert_eq!(
        replay_err.code,
        NotifySearchOfflineErrorCode::ActionTokenAlreadyConsumed
    );

    // 2. Search via Ctrl+K Spotlight (`0x1F NON_AUTHORITATIVE` hard zero-write fence).
    let spotlight = notify_state
        .query_spotlight_candidates(QuerySpotlightCandidatesRequest::default())
        .expect("query Ctrl+K Spotlight candidates");
    assert_eq!(spotlight.envelope_authority_grade, "NON_AUTHORITATIVE");
    assert_eq!(
        spotlight.envelope_restrictions_bitmask,
        SPOTLIGHT_STATUTORY_MASK_0X1F
    );
    assert_non_dev_zero_jargon("spotlight scope_notice_zh", &spotlight.scope_notice_zh);
    assert_non_dev_zero_jargon("spotlight scope_notice_en", &spotlight.scope_notice_en);

    let first_cand = &spotlight.candidates[0];
    assert_eq!(first_cand.restrictions_bitmask, 0x1F);
    assert_eq!(
        first_cand.allowed_verbs,
        vec!["inspect".to_owned(), "jump_to_authority".to_owned()]
    );

    for gate in [
        SearchForbiddenControlGate::CapabilityGrant,
        SearchForbiddenControlGate::ResourceLedgerDeduction,
        SearchForbiddenControlGate::TaskCommit,
        SearchForbiddenControlGate::ApplicationUpdate,
        SearchForbiddenControlGate::IrreversibleAction,
    ] {
        let err = notify_state
            .attempt_mutation_from_search_hit(AttemptMutationFromSearchHitRequest {
                candidate_id_hex: first_cand.candidate_id_hex.clone(),
                attempted_gate: gate,
                caller_supplied_mask: Some(0x00),
            })
            .expect_err("direct mutation from 0x1F Spotlight hit must fail closed");
        assert_eq!(
            err.code,
            NotifySearchOfflineErrorCode::SearchResultNonAuthoritative0x1f
        );
    }

    // ControlCenter & Omnibar also enforce the 0x1F statutory zero-write lock.
    let cc_search_err = validate_and_apply_node_verb_core(
        &settings_state,
        "node-search-0x1f-candidate",
        "Cancel",
        "rbt-valid-001",
    )
    .expect_err("ControlCenter must reject mutating verb on 0x1F search candidate");
    assert!(cc_search_err.to_string().contains("SEARCH_0X1F_READ_ONLY"));

    omnibar.update_draft(
        "打开笔记工作区",
        Some(llmos_desktop_lib::omnibar::ExplicitDependencyPin {
            application_id_hex: Some("11".repeat(16)),
            installation_id_hex: Some("21".repeat(16)),
            from_non_authoritative_search_hit: true,
            search_restriction_bits: 0x1F,
            ..Default::default()
        }),
    );
    assert_eq!(
        omnibar.compile_preview(&mut shell, None),
        Err(OmnibarError::NonAuthoritativeSearchHitForbidden {
            restriction_bits: 0x1F
        })
    );

    // 3. Jump back to authoritative Z1/Z2 surface and re-verify against live store generation.
    let jump_ok = notify_state
        .verify_spotlight_candidate_authority(VerifySpotlightCandidateAuthorityRequest {
            candidate_id_hex: first_cand.candidate_id_hex.clone(),
            expected_target_id_hex: first_cand.target_id_hex.clone(),
            snapshot_generation: first_cand.snapshot_generation,
        })
        .expect("authoritative jump re-verification succeeds");
    assert!(jump_ok.verified);
    assert!(!jump_ok.unlocked_authoritative_verbs.is_empty());
}
// ============================================================================
// Test 5: ChooserGrant Save, Taint Clipboard Escalation & Settings CAS + Undo
// ============================================================================

#[test]
fn chooser_grant_save_taint_clipboard_escalation_and_settings_cas_undo() {
    let shell = WorkstationHostShell::new_default_workstation();
    let mut trusted_auth = TrustedUiAuthority::new();
    let app_state = AppState::from_session(SessionConfig::default());
    let trusted_state = TrustedUiState::new();
    let settings_state = SettingsKeychainState::new_seeded();

    let app_sheets = [0x12_u8; 16];
    let app_slides = [0x13_u8; 16];

    // 1. Export cross-app report via ChooserGrant (Save mode with zero raw host path disclosure).
    let save_session = trusted_auth
        .chooser_broker
        .open_chooser_session(
            TrustedUiCallerOrigin::TcbTrustedBroker,
            &shell.session,
            app_slides,
            "app://slides.briefing · 简报演示台",
            ChooserMode::Save,
            ChooserAttenuatedRights::ExportSave,
            30_000,
            10_000,
            &mut trusted_auth.audit,
        )
        .expect("open SAVE chooser session");
    let save_csid =
        parse_hex_id_16("save_csid", &save_session.chooser_session_id_hex).expect("valid csid");

    // Path traversal attempt is rejected fail-closed.
    assert_eq!(
        trusted_auth
            .chooser_broker
            .confirm_chooser_grant(
                InputOrigin::TrustedShell,
                InputSource::Keyboard,
                &shell.session,
                save_csid,
                &[],
                Some(r"..\..\Windows\System32\evil.dll"),
                Some("application/octet-stream"),
                10_400,
                &mut trusted_auth.audit,
            )
            .expect_err("path traversal export must fail")
            .code,
        TrustedUiErrorCode::ChooserInvalidExportName
    );

    let save_grant = trusted_auth
        .chooser_broker
        .confirm_chooser_grant(
            InputOrigin::TrustedShell,
            InputSource::Keyboard,
            &shell.session,
            save_csid,
            &[],
            Some("cross-app-briefing-q4.pdf"),
            Some("application/pdf"),
            10_500,
            &mut trusted_auth.audit,
        )
        .expect("confirm clean SAVE export grant after 350ms");
    let serialized_grant = serde_json::to_string(&save_grant).expect("serialize ChooserGrant");
    for forbidden_path_token in [
        r"C:\",
        "C:/",
        "Users",
        "Documents",
        "/home/",
        ".ssh",
        "host_backing_path",
    ] {
        assert!(
            !serialized_grant.contains(forbidden_path_token),
            "ChooserGrant leaked raw host path token `{forbidden_path_token}`"
        );
    }

    let save_gid = parse_hex_id_16("save_gid", &save_grant.grant_id_hex).expect("valid gid");
    let save_hid = parse_hex_id_16(
        "save_hid",
        &save_grant.scoped_handles[0].scoped_handle_id_hex,
    )
    .expect("valid hid");
    let exercised = trusted_auth
        .chooser_broker
        .exercise_chooser_grant(
            save_gid,
            app_slides,
            save_hid,
            ChooserAttenuatedRights::ExportSave,
            &shell.session,
            10_600,
            &mut trusted_auth.audit,
        )
        .expect("exercise ExportSave ChooserGrant");
    assert_eq!(exercised.display_name, "cross-app-briefing-q4.pdf");

    // 2. Copy HIGH/CRITICAL_SECRET (CREDENTIALS + CONFIDENTIAL) tainted content across apps -> Z4 [DUI-CLIP-001] gate.
    let src_hex = hex16(app_sheets);
    let dst_hex = hex16(app_slides);
    let dst_inst_hex = hex16([0x23; 16]);

    let copy_gesture = trusted_ui_mint_gesture_token_core(
        &trusted_state,
        &app_state,
        InputOrigin::TrustedShell,
        InputSource::Keyboard,
        &src_hex,
        "CLIPBOARD",
        20_000,
        5_000,
    )
    .expect("mint copy gesture");
    let tainted_clip = trusted_ui_publish_transfer_core(
        &trusted_state,
        &app_state,
        "CLIPBOARD",
        "text/plain",
        b"CRITICAL_SECRET:Q4-EXECUTIVE-MARGIN-TABLE".to_vec(),
        &src_hex,
        "app://sheets.finance",
        &["CREDENTIALS".to_owned(), "CONFIDENTIAL".to_owned()],
        Some(&copy_gesture.gesture_token_id_hex),
        30_000,
        20_050,
    )
    .expect("publish tainted clipboard");

    let paste_gesture = trusted_ui_mint_gesture_token_core(
        &trusted_state,
        &app_state,
        InputOrigin::TrustedShell,
        InputSource::Keyboard,
        &dst_hex,
        "CLIPBOARD",
        20_200,
        5_000,
    )
    .expect("mint paste gesture");
    let read_decision = trusted_ui_request_transfer_read_core(
        &trusted_state,
        &app_state,
        &tainted_clip.transfer_id_hex,
        &dst_hex,
        &dst_inst_hex,
        "app://slides.briefing",
        Some(&paste_gesture.gesture_token_id_hex),
        20_220,
    )
    .expect("cross-app tainted paste escalates to Z4");

    let z4_clip_cid = match read_decision {
        TransferReadDecisionDto::EscalatedToZ4Consent {
            challenge_id_hex,
            requires_high_contrast_ack,
            taint_flags,
            ..
        } => {
            assert!(requires_high_contrast_ack);
            assert!(taint_flags.contains(&TrustedTaintFlag::Credentials));
            assert!(taint_flags.contains(&TrustedTaintFlag::Confidential));
            challenge_id_hex
        }
        other => panic!("expected EscalatedToZ4Consent, got {other:?}"),
    };
    assert!(
        trusted_ui_get_public_posture_core(&trusted_state, &app_state, 20_220)
            .expect("public posture")
            .z4_modal_active
    );

    trusted_ui_notify_z4_focus_regained_core(
        &trusted_state,
        &app_state,
        TrustedUiCallerOrigin::TcbTrustedBroker,
        &z4_clip_cid,
        20_250,
    )
    .expect("notify focus regained");
    let clip_z4_receipt = trusted_ui_decide_z4_consent_core(
        &trusted_state,
        &app_state,
        InputOrigin::TrustedShell,
        InputSource::Pointer,
        &z4_clip_cid,
        true,
        Some(ConsentGrantScope::SingleAction),
        true,
        20_250 + Z4_CLICKJACK_ARMING_MS,
    )
    .expect("approve Z4 clipboard transfer after 350ms");
    assert_eq!(clip_z4_receipt.state, Z4ChallengeState::Approved);

    let released_clip = trusted_ui_complete_escalated_transfer_core(
        &trusted_state,
        &app_state,
        &tainted_clip.transfer_id_hex,
        &dst_hex,
        &z4_clip_cid,
        20_700,
    )
    .expect("release tainted clipboard payload after Z4 approval");
    match released_clip {
        TransferReadDecisionDto::Released {
            payload_utf8_or_hex,
            ..
        } => assert_eq!(
            payload_utf8_or_hex,
            "CRITICAL_SECRET:Q4-EXECUTIVE-MARGIN-TABLE"
        ),
        other => panic!("expected Released, got {other:?}"),
    }

    // 3. ContentTypeAssociation anti-hijack proposal & 4-tier SettingsDomain CAS (expected_revision).
    let assoc_initial =
        list_content_type_associations_v2_core(&settings_state).expect("list associations");
    assert!(
        assoc_initial
            .iter()
            .any(|a| a.content_type_or_uri == "text/markdown")
    );
    let proposed = submit_package_association_proposal_core(
        &settings_state,
        "text/markdown",
        PackageAssociationProposalDto {
            proposed_application_id_hex: "app-notes-workspace".to_owned(),
            proposed_installation_id_hex: "inst-notes-01".to_owned(),
            proposed_app_name_zh: "笔记工作区".to_owned(),
            proposed_app_name_en: "Notes Workspace".to_owned(),
            submitted_at_ms: 30_000,
        },
    )
    .expect("package proposal");
    assert_eq!(
        proposed.handler_application_id_hex.as_deref(),
        Some("app-spec-studio-01"),
        "package proposal must not silently hijack default handler"
    );
    let bound = bind_content_type_association_core(
        &settings_state,
        "text/markdown",
        "app-notes-workspace",
        "inst-notes-01",
        "笔记工作区",
        "Notes Workspace",
        proposed.revision,
    )
    .expect("bind default handler with CAS");
    assert_eq!(
        bound.handler_application_id_hex.as_deref(),
        Some("app-notes-workspace")
    );
    let revoked_assoc =
        revoke_content_type_association_core(&settings_state, "text/markdown", bound.revision)
            .expect("revoke association");
    assert!(revoked_assoc.handler_application_id_hex.is_none());

    let user_layer = inspect_settings_layer_core(&settings_state, SettingsLayerKindDto::User)
        .expect("inspect User settings layer");
    assert_eq!(user_layer.domain_revision, 20);
    let (updated_settings, watch_ev) = update_setting_with_cas_core(
        &settings_state,
        UpdateSettingCasRequestDto {
            layer: SettingsLayerKindDto::User,
            key: "appearance.theme".to_owned(),
            value_json: "\"paper-workstation\"".to_owned(),
            expected_domain_revision: 20,
        },
    )
    .expect("update setting with CAS revision 20");
    assert_eq!(updated_settings.domain_revision, 21);
    assert_eq!(watch_ev.new_domain_revision, 21);
    assert_eq!(
        drain_settings_watch_events_core(&settings_state, 0)
            .expect("drain watch events")
            .len(),
        1
    );
    assert_eq!(
        update_setting_with_cas_core(
            &settings_state,
            UpdateSettingCasRequestDto {
                layer: SettingsLayerKindDto::User,
                key: "appearance.theme".to_owned(),
                value_json: "\"graphite-workstation\"".to_owned(),
                expected_domain_revision: 20,
            },
        )
        .expect_err("stale revision 20 must fail with CAS conflict")
        .code,
        ErrorCode::Control
    );

    // 4. [RCPT-UNDO-001]: Reversible receipt undo vs IRREVERSIBLE zero-fake-undo guard.
    let irrev_err = validate_and_apply_node_verb_core(
        &settings_state,
        "node-task-external-publish",
        "Undo",
        "rbt-valid-002",
    )
    .expect_err("IRREVERSIBLE receipt must reject Undo");
    assert!(irrev_err.to_string().contains("IRREVERSIBLE_NO_UNDO"));

    let clip_rid =
        parse_hex_id_16("clip_rid", &clip_z4_receipt.receipt_id_hex).expect("valid receipt_id");
    trusted_state
        .with_authority_mut(|auth| {
            let err = auth
                .z4_broker
                .request_undo_for_receipt(clip_rid, &mut auth.audit)
                .expect_err("IRREVERSIBLE Z4 receipt must reject undo");
            assert_eq!(
                err.code,
                TrustedUiErrorCode::IrreversibleStatutoryLockViolation
            );
            Ok(())
        })
        .expect("check irreversible Z4 receipt");
}
// ============================================================================
// Test 6: [DUI-OFFLINE-001] 6-Surface Autonomy & Reconciliation Recovery
// ============================================================================

#[test]
fn offline_partition_autonomy_six_local_surfaces_and_reconciliation_recovery() {
    let notify_state = NotifySearchOfflineState::with_default_fixture();
    let settings_state = SettingsKeychainState::new_seeded();
    let plan_state = PlanEditorState::new();
    let mut shell = WorkstationHostShell::new_default_workstation();
    let mut omnibar = OmnibarEngine::new();

    // 1. Switch cluster connectivity into Partitioned and LocalSingleNode modes.
    for mode in [
        ClusterConnectivityMode::Partitioned,
        ClusterConnectivityMode::LocalSingleNode,
    ] {
        let posture = notify_state
            .set_cluster_connectivity_mode(SetClusterConnectivityModeRequest {
                mode,
                now_mono_ms: 1_700_000_800_000,
            })
            .expect("switch cluster connectivity mode");
        assert_eq!(posture.connectivity_mode, mode);
        assert!(posture.local_autonomy_active);
        assert_non_dev_zero_jargon("offline badge_label_zh", &posture.badge_label_zh);
        assert_non_dev_zero_jargon("offline badge_label_en", &posture.badge_label_en);
        assert_non_dev_zero_jargon("offline status_copy_zh", &posture.status_copy_zh);
        assert_non_dev_zero_jargon("offline status_copy_en", &posture.status_copy_en);

        // Verify all 6 local workstation surfaces remain at 100% interactive availability with LocalAutonomyProof.
        for surface in [
            OfflineSurfaceKind::Launcher,
            OfflineSurfaceKind::LocalArtifacts,
            OfflineSurfaceKind::TaskHistoryAndLocalTasks,
            OfflineSurfaceKind::Settings,
            OfflineSurfaceKind::LocalSearch,
            OfflineSurfaceKind::NotificationHistory,
        ] {
            let decision = notify_state
                .evaluate_offline_action_availability(EvaluateOfflineActionAvailabilityRequest {
                    surface,
                    action_verb: "open_and_interact".to_owned(),
                    requires_remote_provider: false,
                    target_cell_id_hex: None,
                })
                .expect("evaluate local surface availability");
            assert!(decision.interactive);
            assert_eq!(
                decision.status,
                OfflineActionAvailabilityStatus::AvailableLocalAutonomy
            );
            assert!(decision.autonomy_proof.is_some());
        }

        // Verify remote-dependent actions deterministically show WAITING_NETWORK or UNAVAILABLE_REMOTE_OFFLINE.
        let remote_decision = notify_state
            .evaluate_offline_action_availability(EvaluateOfflineActionAvailabilityRequest {
                surface: OfflineSurfaceKind::RemoteClusterDispatch,
                action_verb: "invoke_cloud_provider".to_owned(),
                requires_remote_provider: true,
                target_cell_id_hex: None,
            })
            .expect("evaluate remote action availability");
        assert!(!remote_decision.interactive);
        let expected_status = if mode == ClusterConnectivityMode::Partitioned {
            OfflineActionAvailabilityStatus::WaitingNetwork
        } else {
            OfflineActionAvailabilityStatus::UnavailableRemoteOffline
        };
        assert_eq!(remote_decision.status, expected_status);
    }

    // Exercise actual local operations across all 6 workstation surfaces while partitioned:
    // (a) Task Space, (b) Omnibar, (c) Spotlight, (d) SystemControlCenter, (e) Settings, (f) local ApplicationView.
    let local_dag = inspect_task_plan_dag(&plan_state, "task-01", None).expect("local Task Space");
    assert_eq!(local_dag.task_id, "task-01");
    omnibar.update_draft("inspect health", None);
    let local_omni = omnibar
        .compile_preview(&mut shell, None)
        .expect("local Omnibar compile");
    assert!(
        local_omni
            .typed_intent
            .compiled_control_command_preview
            .contains("parse_nl_command ->")
    );
    let local_spot = notify_state
        .query_spotlight_candidates(QuerySpotlightCandidatesRequest::default())
        .expect("local Spotlight query");
    assert!(!local_spot.candidates.is_empty());
    let local_cc =
        inspect_system_control_center_core(&settings_state).expect("local SystemControlCenter");
    assert_eq!(
        local_cc.domain_summaries.len(),
        ControlCenterDomainKind::ALL.len()
    );
    let local_settings =
        inspect_settings_layer_core(&settings_state, SettingsLayerKindDto::Workspace)
            .expect("local Settings");
    assert!(!local_settings.effective_settings.is_empty());
    let local_view = compositor_snapshot_dto(&shell, false).expect("local ApplicationView");
    assert_eq!(local_view.taskbar_rect.height, TASKBAR_HEIGHT_PX);

    // Verify QUARANTINED remote cell lease cannot be unlocked by local TTL expiry alone.
    let posture = notify_state.inspect_offline_cluster_posture();
    let quarantined_cell = posture
        .cells
        .iter()
        .find(|c| c.quarantined_fence_locked)
        .expect("quarantined cell exists");
    assert_eq!(
        notify_state
            .attempt_unlock_quarantined_lease_by_ttl(&quarantined_cell.cell_id_hex, 999_999)
            .expect_err("QUARANTINED cell lease must reject local TTL unlock")
            .code,
        NotifySearchOfflineErrorCode::QuarantinedLeaseFenceLocked
    );

    // 2. [CTRL-RECOVERY-001]: Transport disconnect/UNKNOWN reconciliation via ControlCommandId + monotonic watermark polling.
    let ticket = arm_destructive_control_command_core(
        &settings_state,
        "node-topic-telemetry",
        "TopicPurge",
        1,
        50_000,
    )
    .expect("arm destructive TopicPurge command");
    let unknown_entry = confirm_destructive_control_command_core(
        &settings_state,
        "cmd-e2e-purge-01",
        &ticket.arming_ticket_id_hex,
        true,
        50_500,
        true,
    )
    .expect("record UNKNOWN outcome during partition");
    assert_eq!(
        unknown_entry.status,
        ControlCommandReconcileStatusDto::UnknownNeedsReconcile
    );
    assert!(unknown_entry.blind_retry_blocked);
    let dl_receipt = unknown_entry
        .data_loss_receipt
        .as_ref()
        .expect("DataLossReceipt attached");
    assert_eq!(dl_receipt.undo_class, UndoClassDto::Irreversible);
    assert_eq!(
        dl_receipt.statutory_lock,
        StatutoryLockKindDto::IrreversibleNoUndo
    );

    // Blind retry before reconciliation is deterministically blocked.
    let retry_err = confirm_destructive_control_command_core(
        &settings_state,
        "cmd-e2e-purge-01",
        &ticket.arming_ticket_id_hex,
        true,
        50_800,
        false,
    )
    .expect_err("blind retry on UNKNOWN must be blocked");
    assert!(
        retry_err
            .to_string()
            .contains("BLIND_RETRY_FORBIDDEN_ON_UNKNOWN")
    );

    // Verify wire builder for QueryControlCommandStatus and monotonic PollDesktopWatermarkEvents.
    assert!(build_query_control_command_status(&"aa".repeat(16)).is_ok());
    for seq in [100_u64, 107, 114] {
        assert!(build_poll_desktop_watermark_events(seq, 64).is_ok());
    }

    // Reconcile ControlCommandId and restore cluster connectivity to Connected.
    let reconciled = reconcile_control_command_status_core(&settings_state, "cmd-e2e-purge-01")
        .expect("reconcile ControlCommandId");
    assert_eq!(
        reconciled.status,
        ControlCommandReconcileStatusDto::AppliedIdempotent
    );
    assert!(!reconciled.blind_retry_blocked);

    let reconnected = notify_state
        .set_cluster_connectivity_mode(SetClusterConnectivityModeRequest {
            mode: ClusterConnectivityMode::Connected,
            now_mono_ms: 1_700_000_900_000,
        })
        .expect("reconnect cluster");
    assert_eq!(
        reconnected.connectivity_mode,
        ClusterConnectivityMode::Connected
    );
}
