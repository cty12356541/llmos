//! Stage D (`D-NOTIFY-SEARCH-OFFLINE` / `W72-D-NOTIFY-SEARCH`) 宿主侧集成验收测试：
//! 16 态通知流与一次性 `ActionToken` 核销、`Ctrl+K` 搜索 `0x1F` 只读非权威硬件锁与权威跳转重核验、
//! 以及 `[DUI-OFFLINE-001]` 本机离线自治证明与 `QUARANTINED` 租约围栏锁。

use std::collections::BTreeSet;

use llmos_desktop_lib::notify_search_offline::{
    AttemptMutationFromSearchHitRequest, CanonicalTaskState, ClusterConnectivityMode,
    DismissNotificationItemRequest, EvaluateOfflineActionAvailabilityRequest,
    ListNotificationFeedRequest, NotifySearchOfflineErrorCode, NotifySearchOfflineState,
    OfflineActionAvailabilityStatus, OfflineSurfaceKind, QuerySpotlightCandidatesRequest,
    RedeemNotificationActionTokenRequest, SPOTLIGHT_STATUTORY_MASK_0X1F,
    SearchForbiddenControlGate, SetClusterConnectivityModeRequest,
    VerifySpotlightCandidateAuthorityRequest,
};

const BANNED_L1_L2_JARGON: &[&str] = &[
    "0x1F",
    "0x1f",
    "NON_AUTHORITATIVE",
    "TaskAttempt",
    "ExecutionFiber",
    "TypedIntent",
    "checkpoint",
    "ROAD-D-001",
    "ADR-0011",
    "CAS_WINNER",
    "SHARED_THREAD_LIKE",
    "LOCAL_SINGLE_NODE",
    "receipt_hex",
    "control_command_id",
];

#[test]
fn test_notification_feed_covers_all_16_canonical_task_states_and_cursor_pagination() {
    let state = NotifySearchOfflineState::with_default_fixture();

    // 1. 拉取全量 16 条通知并验证覆盖全部 16 个权威任务状态
    let full_page = state
        .list_notification_feed(ListNotificationFeedRequest {
            cursor_sequence: None,
            limit: 64,
            unread_only: false,
            category_filter: None,
            include_dismissed: false,
        })
        .expect("full notification feed should succeed");

    assert_eq!(full_page.items.len(), 16);
    assert!(!full_page.has_more);
    assert_eq!(full_page.next_cursor_sequence, None);

    let observed_states: BTreeSet<CanonicalTaskState> =
        full_page.items.iter().map(|i| i.task_state).collect();
    for canonical in CanonicalTaskState::ALL {
        assert!(
            observed_states.contains(&canonical),
            "missing canonical task state in notification feed: {:?}",
            canonical
        );
    }

    // 2. 验证游标分页：每页 6 条，共 3 页（6 + 6 + 4 = 16），序号严格递减且无重复
    let mut collected_ids = BTreeSet::new();
    let mut cursor = None;
    let mut page_lengths = Vec::new();
    let mut last_seq = u64::MAX;

    loop {
        let page = state
            .list_notification_feed(ListNotificationFeedRequest {
                cursor_sequence: cursor,
                limit: 6,
                unread_only: false,
                category_filter: None,
                include_dismissed: false,
            })
            .expect("paginated notification feed should succeed");

        page_lengths.push(page.items.len());
        for item in &page.items {
            assert!(
                item.sequence < last_seq,
                "notification feed must be strictly descending by sequence"
            );
            last_seq = item.sequence;
            assert!(
                collected_ids.insert(item.notification_id_hex.clone()),
                "duplicate notification_id_hex across pages"
            );
        }

        if !page.has_more {
            break;
        }
        cursor = page.next_cursor_sequence;
        assert!(
            cursor.is_some(),
            "has_more=true must provide next_cursor_sequence"
        );
    }

    assert_eq!(page_lengths, vec![6, 6, 4]);
    assert_eq!(collected_ids.len(), 16);

    // 3. 验证单条通知检视 (`IPC-D8` `InspectNotification`) 与归档忽略 (`dismiss_notification_item`)
    let newest = &full_page.items[0];
    let inspected = state
        .inspect_notification_item(&newest.notification_id_hex)
        .expect("inspect_notification_item should succeed");
    assert_eq!(inspected.sequence, newest.sequence);

    let initial_unread = full_page.total_unread_count;
    let dismiss_receipt = state
        .dismiss_notification_item(DismissNotificationItemRequest {
            notification_id_hex: newest.notification_id_hex.clone(),
            now_mono_ms: 1_700_000_500_000,
        })
        .expect("dismiss_notification_item should succeed");
    assert!(dismiss_receipt.dismissed);
    assert_eq!(dismiss_receipt.ack_watermark_sequence, newest.sequence);
    assert_eq!(dismiss_receipt.updated_unread_count, initial_unread - 1);
}

#[test]
fn test_action_token_one_shot_redemption_and_replay_rejection() {
    let state = NotifySearchOfflineState::with_default_fixture();
    let page = state
        .list_notification_feed(ListNotificationFeedRequest::default())
        .expect("list_notification_feed should succeed");

    // 选取 AWAITING_APPROVAL 状态的通知（带有一次性 approve_task_step ActionToken）
    let awaiting_item = page
        .items
        .iter()
        .find(|i| i.task_state == CanonicalTaskState::AwaitingApproval)
        .expect("AWAITING_APPROVAL notification must exist")
        .clone();
    let token = awaiting_item
        .action_token
        .clone()
        .expect("AWAITING_APPROVAL notification must carry inline ActionToken");
    assert!(!token.consumed);
    assert!(
        awaiting_item
            .allowed_verbs
            .contains(&"redeem_action_token".to_string())
    );

    let redeem_req = RedeemNotificationActionTokenRequest {
        notification_id_hex: awaiting_item.notification_id_hex.clone(),
        token_id_hex: token.token_id_hex.clone(),
        action_id: token.action_id.clone(),
        caller_app_id_hex: token.bound_app_id_hex.clone(),
        caller_principal_id_hex: token.bound_principal_id_hex.clone(),
        task_id_hex: token.bound_task_id_hex.clone(),
        now_mono_ms: awaiting_item.created_at_mono_ms + 1_000,
        presented_commitment_hex: Some(token.verifier_commitment_hex.clone()),
    };

    // 首次核销：成功消费并推进状态至 RUNNING
    let receipt = state
        .redeem_notification_action_token(redeem_req.clone())
        .expect("first ActionToken redemption should succeed");
    assert_eq!(receipt.resulting_task_state, CanonicalTaskState::Running);
    assert_eq!(receipt.redemption_receipt_digest_hex.len(), 64);
    assert!(
        !receipt
            .remaining_allowed_verbs
            .contains(&"redeem_action_token".to_string()),
        "redeemed notification must prune redeem_action_token from allowed_verbs"
    );

    // 立即重放同一令牌：必须 fail-closed 报错 ACTION_TOKEN_ALREADY_CONSUMED
    let replay_err = state
        .redeem_notification_action_token(redeem_req)
        .expect_err("second redemption of the same ActionToken must fail closed");
    assert_eq!(
        replay_err.code,
        NotifySearchOfflineErrorCode::ActionTokenAlreadyConsumed
    );

    let audit = state.get_security_audit();
    assert_eq!(audit.action_token_redemptions_succeeded, 1);
    assert_eq!(audit.action_token_replay_rejections, 1);
}

#[test]
fn test_action_token_rejects_expired_cross_app_cross_principal_and_forged_commitment() {
    let state = NotifySearchOfflineState::with_default_fixture();
    let page = state
        .list_notification_feed(ListNotificationFeedRequest::default())
        .expect("list_notification_feed should succeed");

    let item = page
        .items
        .iter()
        .find(|i| i.task_state == CanonicalTaskState::WaitingHuman)
        .expect("WAITING_HUMAN notification must exist")
        .clone();
    let token = item
        .action_token
        .clone()
        .expect("WAITING_HUMAN notification must have ActionToken");

    let base_req = RedeemNotificationActionTokenRequest {
        notification_id_hex: item.notification_id_hex.clone(),
        token_id_hex: token.token_id_hex.clone(),
        action_id: token.action_id.clone(),
        caller_app_id_hex: token.bound_app_id_hex.clone(),
        caller_principal_id_hex: token.bound_principal_id_hex.clone(),
        task_id_hex: token.bound_task_id_hex.clone(),
        now_mono_ms: item.created_at_mono_ms + 500,
        presented_commitment_hex: Some(token.verifier_commitment_hex.clone()),
    };

    // 1. 过期令牌拦截 (`ACTION_TOKEN_EXPIRED`)
    let mut expired_req = base_req.clone();
    expired_req.now_mono_ms = token.expiry_mono_ms + 1;
    let err_expired = state
        .redeem_notification_action_token(expired_req)
        .expect_err("expired token must be rejected");
    assert_eq!(
        err_expired.code,
        NotifySearchOfflineErrorCode::ActionTokenExpired
    );

    // 2. 跨应用伪造拦截 (`ACTION_TOKEN_APP_MISMATCH`)
    let mut wrong_app_req = base_req.clone();
    wrong_app_req.caller_app_id_hex = "ffffffffffffffffffffffffffffffff".to_string();
    let err_app = state
        .redeem_notification_action_token(wrong_app_req)
        .expect_err("cross-app token redemption must be rejected");
    assert_eq!(
        err_app.code,
        NotifySearchOfflineErrorCode::ActionTokenAppMismatch
    );

    // 3. 跨主体 Principal 伪造拦截 (`ACTION_TOKEN_PRINCIPAL_MISMATCH`)
    let mut wrong_principal_req = base_req.clone();
    wrong_principal_req.caller_principal_id_hex = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".to_string();
    let err_principal = state
        .redeem_notification_action_token(wrong_principal_req)
        .expect_err("cross-principal token redemption must be rejected");
    assert_eq!(
        err_principal.code,
        NotifySearchOfflineErrorCode::ActionTokenPrincipalMismatch
    );

    // 4. 跨任务 ID 伪造拦截 (`ACTION_TOKEN_TASK_MISMATCH`)
    let mut wrong_task_req = base_req.clone();
    wrong_task_req.task_id_hex = "dddddddddddddddddddddddddddddddd".to_string();
    let err_task = state
        .redeem_notification_action_token(wrong_task_req)
        .expect_err("mismatched task_id must be rejected");
    assert_eq!(
        err_task.code,
        NotifySearchOfflineErrorCode::ActionTokenTaskMismatch
    );

    // 5. 伪造密码学承诺拦截 (`ACTION_TOKEN_INVALID_PROOF`)
    let mut wrong_proof_req = base_req.clone();
    wrong_proof_req.presented_commitment_hex = Some("0".repeat(64));
    let err_proof = state
        .redeem_notification_action_token(wrong_proof_req)
        .expect_err("forged commitment proof must be rejected");
    assert_eq!(
        err_proof.code,
        NotifySearchOfflineErrorCode::ActionTokenInvalidProof
    );

    let audit = state.get_security_audit();
    assert_eq!(audit.action_token_expired_rejections, 1);
    assert_eq!(audit.action_token_app_mismatch_rejections, 1);
    assert_eq!(audit.action_token_principal_mismatch_rejections, 1);
    assert_eq!(audit.action_token_task_mismatch_rejections, 1);
    assert_eq!(audit.action_token_invalid_proof_rejections, 1);
    assert_eq!(audit.action_token_redemptions_succeeded, 0);
}

#[test]
fn test_spotlight_0x1f_statutory_lock_blocks_all_five_mutation_gates_even_with_tampered_mask() {
    let state = NotifySearchOfflineState::with_default_fixture();
    let resp = state
        .query_spotlight_candidates(QuerySpotlightCandidatesRequest::default())
        .expect("query_spotlight_candidates should succeed");

    assert_eq!(resp.envelope_authority_grade, "NON_AUTHORITATIVE");
    assert_eq!(
        resp.envelope_restrictions_bitmask,
        SPOTLIGHT_STATUTORY_MASK_0X1F
    );
    assert!(!resp.candidates.is_empty());

    for cand in &resp.candidates {
        assert_eq!(cand.authority_grade, "NON_AUTHORITATIVE");
        assert_eq!(cand.restrictions_bitmask, 0x1F);
        assert_eq!(
            cand.allowed_verbs,
            vec!["inspect".to_string(), "jump_to_authority".to_string()]
        );
        assert_eq!(
            cand.l3_diagnostics.authority_tag,
            "NON_AUTHORITATIVE · mask=0x1F"
        );
    }

    let first_candidate_id = resp.candidates[0].candidate_id_hex.clone();
    let all_gates = [
        SearchForbiddenControlGate::CapabilityGrant,
        SearchForbiddenControlGate::ResourceLedgerDeduction,
        SearchForbiddenControlGate::TaskCommit,
        SearchForbiddenControlGate::ApplicationUpdate,
        SearchForbiddenControlGate::IrreversibleAction,
    ];

    for gate in all_gates {
        let err = state
            .attempt_mutation_from_search_hit(AttemptMutationFromSearchHitRequest {
                candidate_id_hex: first_candidate_id.clone(),
                attempted_gate: gate,
                // 即使恶意调用方尝试传入 0x00 覆盖掩码，法定锁 #1 也必须强制拦截
                caller_supplied_mask: Some(0x00),
            })
            .expect_err("mutation from search hit must always fail closed");

        assert_eq!(
            err.code,
            NotifySearchOfflineErrorCode::SearchResultNonAuthoritative0x1f
        );
    }

    let audit = state.get_security_audit();
    assert_eq!(audit.search_0x1f_mutation_rejections, 5);
    assert_eq!(audit.search_forbid_capability_grant_hits, 1);
    assert_eq!(audit.search_forbid_ledger_deduction_hits, 1);
    assert_eq!(audit.search_forbid_task_commit_hits, 1);
    assert_eq!(audit.search_forbid_app_update_hits, 1);
    assert_eq!(audit.search_forbid_irreversible_action_hits, 1);
}

#[test]
fn test_spotlight_authoritative_jump_reverification_and_retracted_or_stale_rejection() {
    let state = NotifySearchOfflineState::with_default_fixture();
    let resp = state
        .query_spotlight_candidates(QuerySpotlightCandidatesRequest::default())
        .expect("query_spotlight_candidates should succeed");

    let cand = &resp.candidates[0];

    // 1. 正常跳转重核验：成功返回权威视图信息与权威存储允许的动作动词
    let verify_ok = state
        .verify_spotlight_candidate_authority(VerifySpotlightCandidateAuthorityRequest {
            candidate_id_hex: cand.candidate_id_hex.clone(),
            expected_target_id_hex: cand.target_id_hex.clone(),
            snapshot_generation: cand.snapshot_generation,
        })
        .expect("authoritative verification should succeed for fresh candidate");
    assert!(verify_ok.verified);
    assert_eq!(verify_ok.verification_digest_hex.len(), 64);
    assert!(!verify_ok.unlocked_authoritative_verbs.is_empty());

    // 2. 权威存储代数演进后，持旧 snapshot_generation 跳转：必须拒绝并报 STALE_GENERATION
    let new_gen = state
        .advance_authority_generation(&cand.target_id_hex)
        .expect("advance_authority_generation should find target");
    assert_eq!(new_gen, 2);

    let err_stale = state
        .verify_spotlight_candidate_authority(VerifySpotlightCandidateAuthorityRequest {
            candidate_id_hex: cand.candidate_id_hex.clone(),
            expected_target_id_hex: cand.target_id_hex.clone(),
            snapshot_generation: cand.snapshot_generation,
        })
        .expect_err("stale search snapshot generation must be rejected on jump");
    assert_eq!(
        err_stale.code,
        NotifySearchOfflineErrorCode::StaleGeneration
    );

    // 3. 权威记录被撤回 (`retracted`) 后跳转：必须拒绝并报 AUTHORITY_RECORD_RETRACTED
    assert!(state.retract_authority_record(&cand.target_id_hex));
    let err_retracted = state
        .verify_spotlight_candidate_authority(VerifySpotlightCandidateAuthorityRequest {
            candidate_id_hex: cand.candidate_id_hex.clone(),
            expected_target_id_hex: cand.target_id_hex.clone(),
            snapshot_generation: new_gen,
        })
        .expect_err("retracted authority record must be rejected on jump");
    assert_eq!(
        err_retracted.code,
        NotifySearchOfflineErrorCode::AuthorityRecordRetracted
    );

    let audit = state.get_security_audit();
    assert_eq!(audit.search_authority_verifications_succeeded, 1);
    assert_eq!(audit.search_stale_generation_rejections, 1);
    assert_eq!(audit.search_authority_retracted_rejections, 1);
}

#[test]
fn test_offline_autonomy_dui_offline_001_local_100_percent_interactive_and_remote_degradation() {
    let state = NotifySearchOfflineState::with_default_fixture();

    for offline_mode in [
        ClusterConnectivityMode::Partitioned,
        ClusterConnectivityMode::LocalSingleNode,
    ] {
        let posture = state
            .set_cluster_connectivity_mode(SetClusterConnectivityModeRequest {
                mode: offline_mode,
                now_mono_ms: 1_700_000_600_000,
            })
            .expect("switching connectivity mode should succeed");
        assert_eq!(posture.connectivity_mode, offline_mode);
        assert!(posture.local_autonomy_active);

        // 验证 6 大本机功能面在离线/单机模式下 100% 可交互，并附带 LocalAutonomyProof
        let local_surfaces = [
            OfflineSurfaceKind::Launcher,
            OfflineSurfaceKind::LocalArtifacts,
            OfflineSurfaceKind::TaskHistoryAndLocalTasks,
            OfflineSurfaceKind::Settings,
            OfflineSurfaceKind::LocalSearch,
            OfflineSurfaceKind::NotificationHistory,
        ];

        for surface in local_surfaces {
            let decision = state
                .evaluate_offline_action_availability(EvaluateOfflineActionAvailabilityRequest {
                    surface,
                    action_verb: "open_and_interact".to_string(),
                    requires_remote_provider: false,
                    target_cell_id_hex: None,
                })
                .expect("evaluating local surface availability should succeed");

            assert!(
                decision.interactive,
                "local surface {:?} must remain 100% interactive in {:?}",
                surface, offline_mode
            );
            assert_eq!(
                decision.status,
                OfflineActionAvailabilityStatus::AvailableLocalAutonomy
            );
            assert!(
                decision.autonomy_proof.is_some(),
                "local offline action must attach LocalAutonomyProofBadge"
            );
        }

        // 验证强依赖远端 Provider 的动作确定性降级为 WAITING_NETWORK 或 UNAVAILABLE_REMOTE_OFFLINE
        let remote_decision = state
            .evaluate_offline_action_availability(EvaluateOfflineActionAvailabilityRequest {
                surface: OfflineSurfaceKind::RemoteClusterDispatch,
                action_verb: "invoke_cloud_provider".to_string(),
                requires_remote_provider: true,
                target_cell_id_hex: None,
            })
            .expect("evaluating remote provider action should return deterministic decision");

        assert!(!remote_decision.interactive);
        let expected_status = if offline_mode == ClusterConnectivityMode::Partitioned {
            OfflineActionAvailabilityStatus::WaitingNetwork
        } else {
            OfflineActionAvailabilityStatus::UnavailableRemoteOffline
        };
        assert_eq!(remote_decision.status, expected_status);
    }
}

#[test]
fn test_quarantined_cell_lease_fence_rejects_local_ttl_unlock_and_blocks_dispatch() {
    let state = NotifySearchOfflineState::with_default_fixture();
    let posture = state.inspect_offline_cluster_posture();

    let quarantined_cell = posture
        .cells
        .iter()
        .find(|c| c.quarantined_fence_locked)
        .expect("fixture must contain a QUARANTINED fenced cell");

    // 1. 即使本地时钟经过任意长 TTL (`elapsed_local_ttl_ms = 999_999 >= ceiling`)，
    //    严禁仅凭本地 TTL 自动解锁 QUARANTINED 围栏（法定锁 #3）
    let unlock_err = state
        .attempt_unlock_quarantined_lease_by_ttl(&quarantined_cell.cell_id_hex, 999_999)
        .expect_err("QUARANTINED cell lease must never unlock by local TTL alone");
    assert_eq!(
        unlock_err.code,
        NotifySearchOfflineErrorCode::QuarantinedLeaseFenceLocked
    );

    // 2. 向 QUARANTINED 节点派发操作必须被法定围栏锁阻断
    let dispatch_decision = state
        .evaluate_offline_action_availability(EvaluateOfflineActionAvailabilityRequest {
            surface: OfflineSurfaceKind::TaskHistoryAndLocalTasks,
            action_verb: "dispatch_task".to_string(),
            requires_remote_provider: false,
            target_cell_id_hex: Some(quarantined_cell.cell_id_hex.clone()),
        })
        .expect("evaluating action on quarantined cell should return decision");
    assert!(!dispatch_decision.interactive);
    assert_eq!(
        dispatch_decision.status,
        OfflineActionAvailabilityStatus::BlockedQuarantinedFence
    );
    assert_eq!(
        dispatch_decision.statutory_lock_kind.as_deref(),
        Some("QUARANTINED_LEASE_FENCE")
    );

    let audit = state.get_security_audit();
    assert_eq!(audit.quarantined_lease_ttl_unlock_rejections, 1);
}

#[test]
fn test_progressive_disclosure_l1_l2_zero_jargon_and_l3_diagnostic_completeness() {
    let state = NotifySearchOfflineState::with_default_fixture();
    let feed = state
        .list_notification_feed(ListNotificationFeedRequest {
            limit: 64,
            ..ListNotificationFeedRequest::default()
        })
        .expect("feed should succeed");
    let spotlight = state
        .query_spotlight_candidates(QuerySpotlightCandidatesRequest::default())
        .expect("spotlight should succeed");
    let posture = state.inspect_offline_cluster_posture();

    let mut l1_l2_corpus = Vec::new();
    for item in &feed.items {
        l1_l2_corpus.push(item.title_zh.as_str());
        l1_l2_corpus.push(item.title_en.as_str());
        l1_l2_corpus.push(item.summary_zh.as_str());
        l1_l2_corpus.push(item.summary_en.as_str());
        l1_l2_corpus.push(item.state_label_zh.as_str());
        l1_l2_corpus.push(item.state_label_en.as_str());
        if let Some(tok) = &item.action_token {
            l1_l2_corpus.push(tok.label_zh.as_str());
            l1_l2_corpus.push(tok.label_en.as_str());
        }
        assert_eq!(item.l3_diagnostics.verifier_commitment_hex.len(), 64);
    }

    l1_l2_corpus.push(spotlight.scope_notice_zh.as_str());
    l1_l2_corpus.push(spotlight.scope_notice_en.as_str());
    for cand in &spotlight.candidates {
        l1_l2_corpus.push(cand.title_zh.as_str());
        l1_l2_corpus.push(cand.title_en.as_str());
        l1_l2_corpus.push(cand.snippet_zh.as_str());
        l1_l2_corpus.push(cand.snippet_en.as_str());
        l1_l2_corpus.push(cand.candidate_hint_zh.as_str());
        l1_l2_corpus.push(cand.candidate_hint_en.as_str());
        assert_eq!(
            cand.l3_diagnostics.authority_tag,
            "NON_AUTHORITATIVE · mask=0x1F"
        );
    }

    l1_l2_corpus.push(posture.badge_label_zh.as_str());
    l1_l2_corpus.push(posture.badge_label_en.as_str());
    l1_l2_corpus.push(posture.status_copy_zh.as_str());
    l1_l2_corpus.push(posture.status_copy_en.as_str());
    for cell in &posture.cells {
        l1_l2_corpus.push(cell.display_name_zh.as_str());
        l1_l2_corpus.push(cell.display_name_en.as_str());
        l1_l2_corpus.push(cell.status_label_zh.as_str());
        l1_l2_corpus.push(cell.status_label_en.as_str());
    }

    for text in l1_l2_corpus {
        for banned in BANNED_L1_L2_JARGON {
            assert!(
                !text.contains(banned),
                "L1/L2 user-facing copy '{}' must not contain internal jargon '{}'",
                text,
                banned
            );
        }
    }
}
