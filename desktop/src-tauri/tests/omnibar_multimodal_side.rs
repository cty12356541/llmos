//! Stage D (`W71-D-OMNIBAR` / `D-OMNIBAR-MULTIMODAL`) 按需 `Z3` 命令与意图面板、
//! 渐进式自然语言编译器、`[NL-AMBIG-001]` 强制澄清硬门、`[NL-BIND-001]` 依赖核验与多模态权限状态机测试。

use llmos_desktop_lib::compositor::{WorkstationHostShell, Z3_PALETTE_Z};
use llmos_desktop_lib::error::ErrorCode;
use llmos_desktop_lib::input_router::{InputOrigin, InputRejectionReason};
use llmos_desktop_lib::ipc::{AppState, SessionConfig};
use llmos_desktop_lib::omnibar::{
    ACTION_PROPOSAL_SCHEMA_ID, AttachmentChipInput, AttachmentKind, CompiledIntentKind,
    ExplicitDependencyPin, HardwarePermissionGrant, OMNIBAR_CONTROL_RADIUS_PX, OMNIBAR_OVERLAY_ID,
    OMNIBAR_TASKBAR_TRIGGER_HEIGHT_PX, OMNIBAR_TOP_PERMANENT_FOOTPRINT_PX, OMNIBAR_Z_INDEX,
    OmnibarCatalogEntry, OmnibarCloseReason, OmnibarEngine, OmnibarError, OmnibarLocale,
    OmnibarOpenSource, ProposalUndoClass, TYPED_INTENT_SCHEMA_ID, TYPED_INTENT_SCHEMA_VERSION,
    TaintFlag, UnresolvedAmbiguityKind, VoiceSeveranceReason, VoiceStreamPhase,
    omnibar_attach_item_core, omnibar_clear_draft_core, omnibar_compile_preview_core,
    omnibar_get_snapshot_core, omnibar_select_candidate_core, omnibar_set_device_permission_core,
    omnibar_submit_proposal_core, omnibar_toggle_palette_core, omnibar_update_draft_core,
    omnibar_voice_action_core,
};

const BANNED_L1_L2_JARGON: &[&str] = &[
    "Z1",
    "Z2",
    "Z3",
    "Z4",
    "TypedIntent",
    "0x1F",
    "0x1f",
    "Quota",
    "quota",
    "ROAD-D-001",
    "ADR-0011",
    "TaskAttempt",
    "ExecutionFiber",
    "Cell",
    "receipt_hex",
    "control_command_id",
    "CAS_WINNER",
    "SHARED_THREAD_LIKE",
    "LOCAL_SINGLE_NODE",
];

fn assert_zero_jargon(label: &str, text: &str) {
    for &banned in BANNED_L1_L2_JARGON {
        assert!(
            !text.contains(banned),
            "{label} leaked banned architecture jargon '{banned}' in '{text}'"
        );
    }
}

fn sample_catalog_editor() -> OmnibarCatalogEntry {
    OmnibarCatalogEntry {
        application_id: [0x41; 16],
        installation_id: [0x51; 16],
        package_id: Some([0x61; 16]),
        known_process_ids: vec![[0x71; 16]],
        current_generation: 3,
        display_name: "文档工作台".to_owned(),
        keywords: vec![
            "document".to_owned(),
            "editor".to_owned(),
            "笔记".to_owned(),
        ],
    }
}

fn sample_catalog_sheets() -> OmnibarCatalogEntry {
    OmnibarCatalogEntry {
        application_id: [0x42; 16],
        installation_id: [0x52; 16],
        package_id: Some([0x62; 16]),
        known_process_ids: vec![[0x72; 16]],
        current_generation: 5,
        display_name: "表格分析台".to_owned(),
        keywords: vec!["sheet".to_owned(), "report".to_owned(), "笔记".to_owned()],
    }
}

#[test]
fn omnibar_default_zero_footprint_ctrl_space_taskbar_and_draft_preservation() {
    let mut shell = WorkstationHostShell::new_default_workstation();
    let mut engine = OmnibarEngine::new();

    let init = engine.snapshot();
    assert!(!init.open);
    assert_eq!(init.z_index, OMNIBAR_Z_INDEX);
    assert_eq!(init.z_index, Z3_PALETTE_Z);
    assert_eq!(init.z_index, 460);
    assert_eq!(init.border_radius_px, OMNIBAR_CONTROL_RADIUS_PX);
    assert_eq!(init.border_radius_px, 4);
    assert_eq!(
        init.taskbar_trigger_height_px,
        OMNIBAR_TASKBAR_TRIGGER_HEIGHT_PX
    );
    assert_eq!(
        init.top_permanent_footprint_px,
        OMNIBAR_TOP_PERMANENT_FOOTPRINT_PX
    );
    assert_eq!(init.top_permanent_footprint_px, 0);

    // 不可信应用表面尝试伪造 Ctrl+Space 唤起 Z3 命令面板必须被拒绝。
    let spoof_err = engine
        .toggle_via_shortcut(
            &mut shell,
            InputOrigin::UntrustedApplicationSurface {
                surface_id: [0x99; 16],
                application_id: [0x88; 16],
            },
            100,
        )
        .expect_err("untrusted spoof must be rejected");
    assert!(matches!(
        spoof_err,
        OmnibarError::InputRejected {
            reason: InputRejectionReason::UntrustedSystemSpoof,
            ..
        }
    ));
    assert!(!engine.snapshot().open);

    // 可信宿主按 Ctrl+Space 唤起命令面板，挂载于 Z3 palette 层 (z-index: 460)。
    let opened = engine
        .toggle_via_shortcut(&mut shell, InputOrigin::TrustedShell, 120)
        .expect("trusted Ctrl+Space opens omnibar");
    assert!(opened);
    assert!(engine.snapshot().open);
    assert_eq!(
        engine.snapshot().last_open_source,
        Some(OmnibarOpenSource::KeyboardShortcut)
    );
    assert!(
        shell
            .compositor
            .active_z3_overlays()
            .iter()
            .any(|ov| ov.overlay_id == OMNIBAR_OVERLAY_ID && ov.z_index == 460)
    );

    // 录入草稿与附件后按 Escape 与点击外部收起：草稿与附件必须完整保留。
    engine.update_draft("整理本周评审会议纪要", None);
    engine
        .attach_item(AttachmentChipInput {
            attachment_id: Some("shot-1".to_owned()),
            kind: AttachmentKind::Screenshot,
            label_or_path: "review-board.png".to_owned(),
            mime_type: Some("image/png".to_owned()),
            byte_length: 2048,
            content_digest_hex: "ab".repeat(32),
            chooser_grant_id_hex: None,
            scoped_capability_id_hex: None,
            artifact_id_hex: None,
            taint_flags: Vec::new(),
            explicit_host_path_authorized: false,
        })
        .expect("attach screenshot");

    engine.close_palette(&mut shell, OmnibarCloseReason::Escape);
    let after_esc = engine.snapshot();
    assert!(!after_esc.open);
    assert_eq!(
        after_esc.last_close_reason,
        Some(OmnibarCloseReason::Escape)
    );
    assert_eq!(after_esc.draft_utterance, "整理本周评审会议纪要");
    assert_eq!(after_esc.attachments.len(), 1);

    // 通过底部 36px 任务栏的 26px 紧凑触发槽重新打开，然后外部点击收起。
    engine
        .open_via_taskbar_slot(&mut shell, InputOrigin::TrustedShell, 200)
        .expect("taskbar slot opens omnibar");
    assert_eq!(
        engine.snapshot().last_open_source,
        Some(OmnibarOpenSource::TaskbarTrigger)
    );
    engine.close_palette(&mut shell, OmnibarCloseReason::OutsideClick);
    assert_eq!(engine.snapshot().draft_utterance, "整理本周评审会议纪要");
    assert_eq!(engine.snapshot().attachments.len(), 1);

    // 只有显式 clear_draft 才清空草稿与附件。
    engine.clear_draft();
    assert!(engine.snapshot().draft_utterance.is_empty());
    assert!(engine.snapshot().attachments.is_empty());
}

#[test]
fn progressive_compilation_l1_l2_zero_jargon_and_l3_typed_intent_fidelity() {
    let mut shell = WorkstationHostShell::new_default_workstation();
    let mut engine = OmnibarEngine::new();
    engine.register_catalog_entry(sample_catalog_editor());

    for locale in [OmnibarLocale::ZhCn, OmnibarLocale::EnUs] {
        engine.set_locale(locale);
        engine.update_draft("查看文档工作台最近保存纪要", None);
        let bundle = engine
            .compile_preview(&mut shell, None)
            .expect("compile preview");

        assert_eq!(bundle.typed_intent.schema_id, TYPED_INTENT_SCHEMA_ID);
        assert_eq!(
            bundle.typed_intent.schema_version,
            TYPED_INTENT_SCHEMA_VERSION
        );
        assert_eq!(
            bundle.typed_intent.intent_kind,
            CompiledIntentKind::InspectReadonly
        );
        assert_eq!(
            bundle.proposal.proposal_schema_id,
            ACTION_PROPOSAL_SCHEMA_ID
        );
        assert_eq!(bundle.proposal.undo_class, ProposalUndoClass::Lossless);
        match locale {
            OmnibarLocale::ZhCn => assert_eq!(bundle.proposal.undo_badge_label, "可无损撤销"),
            OmnibarLocale::EnUs => {
                assert_eq!(bundle.proposal.undo_badge_label, "Losslessly Reversible")
            }
        }

        // L1 / L2 全部面向普通用户的可见文本严禁泄露任何内部架构行话 (ANTI-AI-07)。
        for field in [
            &bundle.proposal.target_application_label,
            &bundle.proposal.affected_scope_label,
            &bundle.proposal.resource_ceiling_label,
            &bundle.proposal.undo_badge_label,
            &bundle.proposal.compensation_note,
            &bundle.proposal.dependency_summary_label,
        ] {
            assert_zero_jargon("L1/L2 proposal field", field);
        }

        // L3 诊断字段必须完整携带 TypedIntent / ControlScope / IdempotencyKey / ResolvedHandle。
        assert_eq!(bundle.typed_intent.intent_id_hex.len(), 32);
        assert_eq!(bundle.typed_intent.idempotency_key_hex.len(), 32);
        assert_eq!(bundle.proposal.proposal_digest_hex.len(), 64);
        let resolved_app = bundle
            .typed_intent
            .resolved_application
            .as_ref()
            .expect("resolved application handle");
        assert_eq!(resolved_app.application_id_hex, "41".repeat(16));
        assert_eq!(resolved_app.installation_id_hex, "51".repeat(16));
        assert_eq!(resolved_app.generation, 3);
    }

    // 受限语法系统控制命令应与 parse_nl_command 同构映射。
    engine.update_draft("inspect health", None);
    let nl_bundle = engine
        .compile_preview(&mut shell, None)
        .expect("compile system control nl command");
    assert_eq!(
        nl_bundle.typed_intent.intent_kind,
        CompiledIntentKind::ControlOperation
    );
    assert!(
        nl_bundle
            .typed_intent
            .compiled_control_command_preview
            .contains("parse_nl_command ->")
    );
}

#[test]
fn nl_bind_001_rejects_latest_tag_0x1f_search_hit_identity_confusion_and_stale_gen() {
    let mut shell = WorkstationHostShell::new_default_workstation();
    let mut engine = OmnibarEngine::new();
    engine.register_catalog_entry(sample_catalog_editor());

    // 1. 拒绝可变符号标签 "latest"
    engine.update_draft(
        "打开文档工作台",
        Some(ExplicitDependencyPin {
            version_tag: Some("latest".to_owned()),
            ..ExplicitDependencyPin::default()
        }),
    );
    let latest_err = engine
        .compile_preview(&mut shell, None)
        .expect_err("mutable latest tag must fail");
    assert!(matches!(
        latest_err,
        OmnibarError::MutableLatestTagForbidden { .. }
    ));

    // 自然语言文本中直接要求 `@latest` 同样必须拒绝。
    engine.update_draft("run document-editor@latest on notes", None);
    assert!(matches!(
        engine.compile_preview(&mut shell, None),
        Err(OmnibarError::MutableLatestTagForbidden { .. })
    ));

    // 2. 拒绝非权威搜索命中 (0x1F) 直接充当授权执行依赖 (U-7 / U-8 / [NL-BIND-001])
    engine.update_draft(
        "打开文档工作台",
        Some(ExplicitDependencyPin {
            application_id_hex: Some("41".repeat(16)),
            installation_id_hex: Some("51".repeat(16)),
            from_non_authoritative_search_hit: true,
            search_restriction_bits: 0x1F,
            ..ExplicitDependencyPin::default()
        }),
    );
    let search_err = engine
        .compile_preview(&mut shell, None)
        .expect_err("0x1F search hit must not act as direct authority");
    assert_eq!(
        search_err,
        OmnibarError::NonAuthoritativeSearchHitForbidden {
            restriction_bits: 0x1F
        }
    );

    // 3. 拒绝将 PackageId 或瞬态 ProcessId 混充 ApplicationId ([DUI-LAUNCH-001])
    engine.update_draft(
        "启动应用",
        Some(ExplicitDependencyPin {
            raw_identifier_hex: Some("61".repeat(16)),
            ..ExplicitDependencyPin::default()
        }),
    );
    assert_eq!(
        engine.compile_preview(&mut shell, None),
        Err(OmnibarError::PackageIdNotApplicationIdentity {
            offending_id: [0x61; 16]
        })
    );

    engine.update_draft(
        "启动应用",
        Some(ExplicitDependencyPin {
            raw_identifier_hex: Some("71".repeat(16)),
            ..ExplicitDependencyPin::default()
        }),
    );
    assert_eq!(
        engine.compile_preview(&mut shell, None),
        Err(OmnibarError::ProcessIdNotApplicationIdentity {
            offending_id: [0x71; 16]
        })
    );

    // 4. 拒绝过期目标代际 (expected_generation < current_generation)
    engine.update_draft(
        "打开文档工作台",
        Some(ExplicitDependencyPin {
            application_id_hex: Some("41".repeat(16)),
            installation_id_hex: Some("51".repeat(16)),
            expected_generation: Some(2),
            ..ExplicitDependencyPin::default()
        }),
    );
    assert_eq!(
        engine.compile_preview(&mut shell, None),
        Err(OmnibarError::StaleTargetGeneration {
            application_id: [0x41; 16],
            requested_generation: 2,
            current_generation: 3,
        })
    );
}

#[test]
fn nl_ambig_001_hard_blocks_submission_before_selection_and_binds_clarification_after() {
    let mut shell = WorkstationHostShell::new_default_workstation();
    let mut engine = OmnibarEngine::new();
    engine.register_catalog_entry(sample_catalog_editor());
    engine.register_catalog_entry(sample_catalog_sheets());

    // 涵盖多匹配目标 ("笔记" 同时命中两个应用) + 不可逆删除 ("永久移除") + 跨应用流转 ("转发到") + 扩容预算 ("扩容预算")
    engine.update_draft("永久移除旧笔记并转发到归档且扩容预算", None);
    let preview = engine
        .compile_preview(&mut shell, None)
        .expect("compile ambiguous utterance");

    assert!(!preview.proposal.can_submit);
    assert!(preview.proposal.candidates.len() >= 2);
    let kinds: Vec<UnresolvedAmbiguityKind> = preview
        .proposal
        .unresolved_ambiguities
        .iter()
        .map(|a| a.kind)
        .collect();
    assert!(kinds.contains(&UnresolvedAmbiguityKind::MultipleMatchingTargets));
    assert!(kinds.contains(&UnresolvedAmbiguityKind::HighRiskIrreversibleEffect));
    assert!(kinds.contains(&UnresolvedAmbiguityKind::CrossApplicationTransfer));
    assert!(kinds.contains(&UnresolvedAmbiguityKind::BudgetEscalation));

    // 硬门拦截：未选定候选行前调用 submit_proposal 必须返回 ClarificationRequired。
    let blocked = engine
        .submit_proposal(&mut shell, None, true, 500)
        .expect_err("must block unclarified proposal");
    assert!(matches!(
        blocked,
        OmnibarError::ClarificationRequired { .. }
    ));
    assert!(!shell.route.z4_modal_lock);

    // 用户显式点选第一行候选解释后，硬门解锁并绑定 selected_candidate_id + clarification_digest_hex。
    let chosen_id = preview.proposal.candidates[0].candidate_id.clone();
    let clarified = engine
        .select_candidate(&mut shell, None, &chosen_id)
        .expect("select candidate");
    assert!(clarified.proposal.can_submit);
    assert_eq!(
        clarified.proposal.selected_candidate_id.as_deref(),
        Some(chosen_id.as_str())
    );
    let digest = clarified
        .proposal
        .clarification_digest_hex
        .as_ref()
        .expect("clarification digest");
    assert_eq!(digest.len(), 64);

    let submitted = engine
        .submit_proposal(&mut shell, None, true, 600)
        .expect("clarified proposal submits");
    assert_eq!(
        submitted.selected_candidate_id.as_deref(),
        Some(chosen_id.as_str())
    );
    assert_eq!(
        submitted.clarification_digest_hex.as_deref(),
        Some(digest.as_str())
    );
    assert_eq!(submitted.handoff_target, "Z4_TRUSTED_CONSENT");
    assert!(shell.route.z4_modal_lock);
}

#[test]
fn voice_stream_routes_via_focus_router_projects_tray_and_severs_on_z4_or_lock() {
    let mut shell = WorkstationHostShell::new_default_workstation();
    let mut engine = OmnibarEngine::new();

    // 初始请求麦克风：PromptRequired -> RequestingMicPermission，任务栏麦克风指示灯仍为 false。
    let req_snap = engine
        .start_voice_capture(&mut shell, InputOrigin::TrustedShell, 100)
        .expect("request mic");
    assert_eq!(req_snap.phase, VoiceStreamPhase::RequestingMicPermission);
    assert_eq!(
        engine.snapshot().taskbar_tray_indicators,
        (false, false, false)
    );

    // 授予麦克风与屏幕采集权限：自动进入 Capturing，L0 托盘指示器立即点亮 (true, false, true)。
    let perms = engine.set_device_permissions(
        &mut shell,
        Some(HardwarePermissionGrant::Granted),
        Some(HardwarePermissionGrant::PromptRequired),
        Some(HardwarePermissionGrant::Granted),
        Some(false),
        Some(true),
    );
    assert_eq!(perms.taskbar_tray_projection(), (true, false, true));
    assert_eq!(engine.voice_snapshot().phase, VoiceStreamPhase::Capturing);

    // 不可信表面尝试注入语音分片必须被拒绝，但不破坏宿主自身状态审计。
    let spoof_voice = engine
        .append_voice_transcript(
            &mut shell,
            InputOrigin::UntrustedApplicationSurface {
                surface_id: [0xAA; 16],
                application_id: [0xBB; 16],
            },
            "malicious command",
            150,
        )
        .expect_err("untrusted voice spoof rejected");
    assert!(matches!(
        spoof_voice,
        OmnibarError::InputRejected {
            reason: InputRejectionReason::UntrustedSystemSpoof,
            ..
        }
    ));

    engine
        .append_voice_transcript(
            &mut shell,
            InputOrigin::TrustedShell,
            "汇总季度成本报表",
            160,
        )
        .expect("append voice chunk");

    // 挂载 Z4 安全模态屏时：活跃语音流与屏幕采集必须立即被物理切断！
    shell.set_z4_modal(true, 200);
    engine.sync_with_host_shell(&mut shell);
    let severed_z4 = engine.voice_snapshot();
    assert_eq!(severed_z4.phase, VoiceStreamPhase::Cancelled);
    assert_eq!(
        severed_z4.last_severance_reason,
        Some(VoiceSeveranceReason::Z4ModalActive)
    );
    assert_eq!(severed_z4.severed_count, 1);
    assert_eq!(
        engine.snapshot().taskbar_tray_indicators,
        (false, false, false)
    );

    // Z4 期间尝试重新启动语音采集必须被 Z4ModalSevered 拒绝。
    let z4_reject = engine
        .start_voice_capture(&mut shell, InputOrigin::TrustedShell, 220)
        .expect_err("Z4 blocks voice capture");
    assert!(matches!(
        z4_reject,
        OmnibarError::InputRejected {
            reason: InputRejectionReason::Z4ModalSevered,
            ..
        }
    ));

    // 解除 Z4 后完整走通 Capturing -> Transcribing -> Committed，再验证会话锁屏自动切断。
    shell.set_z4_modal(false, 600);
    engine
        .start_voice_capture(&mut shell, InputOrigin::TrustedShell, 610)
        .expect("restart voice");
    engine
        .append_voice_transcript(
            &mut shell,
            InputOrigin::TrustedShell,
            "查看工作区运行状态",
            620,
        )
        .expect("append chunk");
    let transcribing = engine
        .finish_voice_to_transcribing(&mut shell, 630)
        .expect("finish to transcribing");
    assert_eq!(transcribing.phase, VoiceStreamPhase::Transcribing);
    assert!(!transcribing.device_permissions.mic_active);
    let committed = engine
        .commit_voice_transcript(&mut shell, None, 640)
        .expect("commit transcript");
    assert_eq!(committed.phase, VoiceStreamPhase::Committed);
    assert_eq!(engine.snapshot().draft_utterance, "查看工作区运行状态");

    // 再次开启采集并触发会话锁屏 (LOCKED)：必须立即切断为 Cancelled (SessionNotActive)。
    engine
        .start_voice_capture(&mut shell, InputOrigin::TrustedShell, 700)
        .expect("start before lock");
    assert!(engine.voice_snapshot().device_permissions.mic_active);
    shell
        .transition_session("lock", None, Some(710))
        .expect("lock session");
    engine.sync_with_host_shell(&mut shell);
    assert_eq!(engine.voice_snapshot().phase, VoiceStreamPhase::Cancelled);
    assert_eq!(
        engine.voice_snapshot().last_severance_reason,
        Some(VoiceSeveranceReason::SessionNotActive)
    );
    assert_eq!(engine.voice_snapshot().severed_count, 2);
}

#[test]
fn attachment_chips_enforce_chooser_grant_strip_host_paths_and_propagate_taint() {
    let mut shell = WorkstationHostShell::new_default_workstation();
    let mut engine = OmnibarEngine::new();
    engine.register_catalog_entry(sample_catalog_editor());

    // 1. 未携带 ChooserGrant 的原始宿主路径必须直接拒绝 (RawHostPathForbidden)。
    let raw_win_err = engine
        .attach_item(AttachmentChipInput {
            attachment_id: None,
            kind: AttachmentKind::ChooserFile,
            label_or_path: r"C:\Users\Alice\Documents\secret-budget.xlsx".to_owned(),
            mime_type: None,
            byte_length: 4096,
            content_digest_hex: "cd".repeat(32),
            chooser_grant_id_hex: None,
            scoped_capability_id_hex: None,
            artifact_id_hex: None,
            taint_flags: vec![TaintFlag::ConfidentialSecret],
            explicit_host_path_authorized: false,
        })
        .expect_err("raw Windows path without ChooserGrant must fail");
    assert_eq!(raw_win_err, OmnibarError::RawHostPathForbidden);

    // 2. 携带合法 ChooserGrant 但未显式授权路径透传时，仅保留 basename，绝不泄露宿主绝对路径。
    let safe_file = engine
        .attach_item(AttachmentChipInput {
            attachment_id: Some("file-1".to_owned()),
            kind: AttachmentKind::ChooserFile,
            label_or_path: r"C:\Users\Alice\Documents\secret-budget.xlsx".to_owned(),
            mime_type: Some(
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".to_owned(),
            ),
            byte_length: 4096,
            content_digest_hex: "cd".repeat(32),
            chooser_grant_id_hex: Some("12".repeat(16)),
            scoped_capability_id_hex: Some("34".repeat(16)),
            artifact_id_hex: None,
            taint_flags: vec![TaintFlag::ConfidentialSecret, TaintFlag::ExternalOrigin],
            explicit_host_path_authorized: false,
        })
        .expect("attach authorized ChooserGrant file");
    assert_eq!(safe_file.chip_border_radius_px, 4);
    assert_eq!(safe_file.display_label, "secret-budget.xlsx");
    assert_eq!(safe_file.authorized_host_path, None);
    let serialized = serde_json::to_string(&safe_file).expect("serialize chip");
    assert!(!serialized.contains(r"C:\Users\Alice"));

    // 3. 内容寻址 Artifact 引用芯片绑定 ArtifactId + ContentDigest。
    let artifact_chip = engine
        .attach_item(AttachmentChipInput {
            attachment_id: Some("art-1".to_owned()),
            kind: AttachmentKind::Artifact,
            label_or_path: "quarterly-summary.md".to_owned(),
            mime_type: Some("text/markdown".to_owned()),
            byte_length: 1024,
            content_digest_hex: "ef".repeat(32),
            chooser_grant_id_hex: None,
            scoped_capability_id_hex: None,
            artifact_id_hex: Some("56".repeat(16)),
            taint_flags: Vec::new(),
            explicit_host_path_authorized: false,
        })
        .expect("attach content-addressed artifact");
    assert_eq!(artifact_chip.chip_border_radius_px, 4);
    assert_eq!(
        artifact_chip.artifact_id_hex.as_deref(),
        Some("56565656565656565656565656565656")
    );

    // 4. 编译提案时附件 taint 标记必须单调传递至 ActionProposal，且触发 requires_z4_consent。
    engine.update_draft("整理附件内容生成摘要", None);
    let compiled = engine
        .compile_preview(&mut shell, None)
        .expect("compile with tainted attachments");
    assert!(
        compiled
            .proposal
            .effective_taint_flags
            .contains(&TaintFlag::ConfidentialSecret)
    );
    assert!(
        compiled
            .proposal
            .effective_taint_flags
            .contains(&TaintFlag::ExternalOrigin)
    );
    assert!(
        compiled
            .proposal
            .effective_taint_flags
            .contains(&TaintFlag::CrossApplication)
    );
    assert!(compiled.proposal.requires_z4_consent);
    assert_eq!(compiled.typed_intent.resolved_artifacts.len(), 1);
}

#[test]
fn app_state_core_command_handlers_roundtrip_end_to_end() {
    let state = AppState::from_session(SessionConfig::default());

    let s0 = omnibar_get_snapshot_core(&state).expect("initial snapshot");
    assert!(!s0.open);
    assert_eq!(s0.top_permanent_footprint_px, 0);

    let s1 = omnibar_toggle_palette_core(&state, "ctrl+space", 10).expect("open via shortcut");
    assert!(s1.open);

    let s2 = omnibar_set_device_permission_core(
        &state,
        Some(HardwarePermissionGrant::Granted),
        None,
        None,
        None,
        None,
    )
    .expect("grant mic");
    assert_eq!(
        s2.voice.device_permissions.mic_permission,
        HardwarePermissionGrant::Granted
    );

    omnibar_voice_action_core(&state, "start", None, 20).expect("start voice");
    omnibar_voice_action_core(&state, "append", Some("删除过期临时缓存".to_owned()), 30)
        .expect("append voice");
    let s3 = omnibar_voice_action_core(&state, "commit", None, 40).expect("commit voice");
    assert_eq!(s3.draft_utterance, "删除过期临时缓存");

    // 不可逆删除在未选择候选行前通过 core 提交必须返回 Control 错误码 (CLARIFICATION_REQUIRED)。
    let preview = omnibar_compile_preview_core(&state).expect("compile preview");
    assert!(!preview.proposal.can_submit);
    let err = omnibar_submit_proposal_core(&state, false, 50)
        .expect_err("ambiguous core submit must fail");
    assert_eq!(err.code, ErrorCode::Control);
    assert!(err.message.contains("CLARIFICATION_REQUIRED"));

    let cid = preview.proposal.candidates[0].candidate_id.clone();
    let clarified = omnibar_select_candidate_core(&state, &cid).expect("select candidate");
    assert!(clarified.proposal.can_submit);

    let submitted = omnibar_submit_proposal_core(&state, false, 60).expect("submit after clarify");
    assert_eq!(
        submitted.selected_candidate_id.as_deref(),
        Some(cid.as_str())
    );

    let cleared = omnibar_clear_draft_core(&state).expect("clear draft");
    assert!(cleared.draft_utterance.is_empty());

    let updated = omnibar_update_draft_core(
        &state,
        "查看系统健康".to_owned(),
        Some("zh-CN".to_owned()),
        None,
    )
    .expect("update draft");
    assert_eq!(updated.draft_utterance, "查看系统健康");

    let attached = omnibar_attach_item_core(
        &state,
        AttachmentChipInput {
            attachment_id: Some("shot-core".to_owned()),
            kind: AttachmentKind::Screenshot,
            label_or_path: "desk.png".to_owned(),
            mime_type: None,
            byte_length: 512,
            content_digest_hex: "aa".repeat(32),
            chooser_grant_id_hex: None,
            scoped_capability_id_hex: None,
            artifact_id_hex: None,
            taint_flags: Vec::new(),
            explicit_host_path_authorized: false,
        },
    )
    .expect("attach item");
    assert_eq!(attached.attachments.len(), 1);
}
