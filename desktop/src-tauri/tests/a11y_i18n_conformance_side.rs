//! Integration & Negative-Bypass Conformance Suite for `W73-D-A11Y-I18N`
//! (`D-CONTROL-SETTINGS-A11Y` Slice 2: `WCAG 2.2 AA` Contrast, ARIA Semantic Tree,
//! Keyboard Navigation, `Z4` Focus Trap, Reduced Motion & `zh-CN / en-US` Localization).

use llmos_desktop_lib::a11y_i18n::{
    A11yI18nState, FOCUS_RING_WIDTH_PX, KeyboardFocusTargetInputDto, KeyboardFocusTrapRequestDto,
    LOCALE_BUNDLE_VERSION, LOCALE_SCHEMA_REVISION, WCAG_AA_NORMAL_TEXT_MIN_RATIO,
    WCAG_AA_UI_COMPONENT_MIN_RATIO, WCAG_MIN_HIT_TARGET_PX, WorkstationLocaleDto,
    a11y_audit_aria_landmarks_inner, a11y_audit_wcag_contrast_inner, a11y_get_locale_bundle_inner,
    a11y_set_reduced_motion_override_inner, a11y_switch_locale_inner,
    a11y_verify_keyboard_focus_trap_inner, compute_contrast_ratio_hex,
    contains_forbidden_l0_to_l2_jargon, contains_pictographic_emoji,
};
use llmos_desktop_lib::error::ErrorCode;

#[test]
fn versioned_bilingual_resource_bundles_key_parity_and_runtime_switching() {
    let state = A11yI18nState::new();

    // Default locale is zh-CN at switch_generation 1
    let zh = a11y_get_locale_bundle_inner(&state, None).expect("get default zh-CN bundle");
    assert_eq!(zh.locale, WorkstationLocaleDto::ZhCn);
    assert_eq!(zh.bundle_version, LOCALE_BUNDLE_VERSION);
    assert_eq!(zh.schema_revision, LOCALE_SCHEMA_REVISION);
    assert_eq!(zh.switch_generation, 1);
    assert!(zh.key_parity_verified);
    assert_eq!(zh.task_state_labels.len(), 24);
    assert_eq!(zh.control_domain_labels.len(), 12);
    assert_eq!(zh.plane_labels.len(), 8);
    assert_eq!(zh.l2_statutory_lock_labels.len(), 6);

    // Switch to en-US atomically
    let en = a11y_switch_locale_inner(&state, "en-US").expect("switch to en-US");
    assert_eq!(en.locale, WorkstationLocaleDto::EnUs);
    assert_eq!(en.switch_generation, 2);
    assert!(en.key_parity_verified);
    assert_eq!(en.task_state_labels.len(), 24);
    assert_eq!(en.control_domain_labels.len(), 12);
    assert_eq!(
        en.task_state_labels
            .get("WAITING_CONSENT")
            .map(String::as_str),
        Some("Approval Required")
    );
    assert_eq!(
        zh.task_state_labels
            .get("WAITING_CONSENT")
            .map(String::as_str),
        Some("待您确认权限或费用上限")
    );

    // Invalid locale is rejected fail-closed with ErrorCode::Config
    let bad = a11y_switch_locale_inner(&state, "fr-FR");
    assert!(bad.is_err());
    assert_eq!(bad.unwrap_err().code, ErrorCode::Config);
}

#[test]
fn l0_to_l2_zero_kernel_jargon_and_zero_emoji_invariants() {
    let state = A11yI18nState::new();
    for locale_code in ["zh-CN", "en-US"] {
        let bundle = a11y_get_locale_bundle_inner(&state, Some(locale_code))
            .expect("bundle lookup should succeed");
        assert!(
            bundle.l0_to_l2_zero_jargon_verified,
            "locale {locale_code} violated L0/L1/L2 zero-jargon guard"
        );
        assert!(
            bundle.zero_emoji_verified,
            "locale {locale_code} violated ANTI-AI-05 zero-emoji guard"
        );
    }

    // Negative checks for jargon detector & false-positive resistance on "Cancelled"
    assert_eq!(contains_forbidden_l0_to_l2_jargon("Cancelled"), None);
    assert_eq!(
        contains_forbidden_l0_to_l2_jargon("Search mask 0x1F preview"),
        Some("0x1F")
    );
    assert_eq!(
        contains_forbidden_l0_to_l2_jargon("TaskAttempt race lane"),
        Some("TaskAttempt")
    );
    assert_eq!(
        contains_forbidden_l0_to_l2_jargon("Remaining Quota limit"),
        Some("Quota")
    );
    assert!(!contains_pictographic_emoji("已完成 100% · Ready"));
    assert!(contains_pictographic_emoji("Completed \u{1F680}"));
}

#[test]
fn wcag_22_aa_relative_luminance_contrast_across_graphite_and_paper_themes() {
    let state = A11yI18nState::new();
    let report = a11y_audit_wcag_contrast_inner(&state).expect("contrast audit should succeed");

    assert!(
        report.passed,
        "all 20 Graphite & Paper pairs must pass WCAG 2.2 AA"
    );
    assert_eq!(report.total_pairs_audited, 20);
    assert!(report.failures.is_empty());
    assert_eq!(report.focus_ring_width_px, FOCUS_RING_WIDTH_PX);
    assert!(report.min_normal_text_ratio >= WCAG_AA_NORMAL_TEXT_MIN_RATIO);
    assert!(report.min_ui_component_ratio >= WCAG_AA_UI_COMPONENT_MIN_RATIO);

    let graphite_primary = compute_contrast_ratio_hex("#ecedee", "#18191c").expect("valid hex");
    assert!(graphite_primary >= 14.5);
    let paper_primary = compute_contrast_ratio_hex("#111827", "#ffffff").expect("valid hex");
    assert!(paper_primary >= 17.5);

    assert!(compute_contrast_ratio_hex("not-a-hex", "#18191c").is_err());
}

#[test]
fn aria_semantic_landmarks_26px_hit_targets_and_dual_live_regions() {
    let state = A11yI18nState::new();

    // Standard workstation state (Z4 inactive)
    let std_audit = a11y_audit_aria_landmarks_inner(&state, false)
        .expect("ARIA audit with Z4=false should succeed");
    assert!(std_audit.passed);
    assert!(std_audit.violations.is_empty());
    assert_eq!(std_audit.min_hit_target_px, WCAG_MIN_HIT_TARGET_PX);
    assert!(std_audit.polite_live_region_ready);
    assert!(std_audit.assertive_live_region_ready);

    // Z4 modal active state: Z1..Z3 landmarks must become aria_hidden=true while Z4 is alertdialog + aria_modal=true
    a11y_switch_locale_inner(&state, "en-US").expect("switch to en-US");
    let z4_audit = a11y_audit_aria_landmarks_inner(&state, true)
        .expect("ARIA audit with Z4=true should succeed");
    assert!(z4_audit.passed);
    let z1_main = z4_audit
        .nodes
        .iter()
        .find(|n| n.node_id == "plane-z1-workspace-main")
        .expect("Z1 main node exists");
    assert!(
        z1_main.aria_hidden,
        "Z1 main must be aria-hidden when Z4 modal is active"
    );

    let z4_dialog = z4_audit
        .nodes
        .iter()
        .find(|n| n.node_id == "plane-z4-security-alertdialog")
        .expect("Z4 alertdialog exists");
    assert_eq!(z4_dialog.role, "alertdialog");
    assert_eq!(z4_dialog.aria_modal, Some(true));
    assert!(!z4_dialog.aria_hidden);
}

#[test]
fn z4_modal_focus_trap_strict_isolation_and_escape_restoration() {
    let state = A11yI18nState::new();
    let targets = vec![
        KeyboardFocusTargetInputDto {
            id: "z1-task-row-01".to_string(),
            plane: "Z1".to_string(),
            label: "Task Row 01".to_string(),
            disabled: false,
        },
        KeyboardFocusTargetInputDto {
            id: "z3-omnibar-trigger".to_string(),
            plane: "Z3".to_string(),
            label: "Command Palette Trigger".to_string(),
            disabled: false,
        },
        KeyboardFocusTargetInputDto {
            id: "z4-approve-btn".to_string(),
            plane: "Z4".to_string(),
            label: "Approve Authorization".to_string(),
            disabled: false,
        },
        KeyboardFocusTargetInputDto {
            id: "z4-deny-btn".to_string(),
            plane: "Z4".to_string(),
            label: "Deny Authorization".to_string(),
            disabled: false,
        },
    ];

    let report = a11y_verify_keyboard_focus_trap_inner(
        &state,
        KeyboardFocusTrapRequestDto {
            z4_modal_active: true,
            originating_trigger_id: "z3-omnibar-trigger".to_string(),
            targets,
        },
    )
    .expect("Z4 focus trap verification should succeed");

    assert!(report.z4_modal_active);
    assert!(!report.leaked_to_lower_plane);
    assert_eq!(
        report.z4_control_ids,
        vec!["z4-approve-btn".to_string(), "z4-deny-btn".to_string()]
    );
    assert_eq!(
        report.forward_tab_cycle,
        vec![
            "z4-approve-btn".to_string(),
            "z4-deny-btn".to_string(),
            "z4-approve-btn".to_string()
        ]
    );
    assert!(report.blocked_global_shortcuts_under_z4);
    assert!(report.blocked_lower_plane_focus_attempts >= 4);
    assert!(report.focus_restored_on_escape);
    assert_eq!(
        report.restored_focus_id.as_deref(),
        Some("z3-omnibar-trigger")
    );
}

#[test]
fn prefers_reduced_motion_and_user_override_freeze_gerono_seal_and_transitions() {
    let state = A11yI18nState::new();

    // 1. Standard motion with Z4 seal mounted: animated-canvas, 1 RAF loop, 80ms/140ms
    let std_posture = a11y_set_reduced_motion_override_inner(&state, Some(false), None, Some(true))
        .expect("standard motion posture");
    assert!(!std_posture.effective_reduced_motion);
    assert_eq!(std_posture.gerono_seal_mode, "animated-canvas");
    assert_eq!(std_posture.active_raf_loop_count, 1);
    assert_eq!(std_posture.fast_transition_duration_ms, 80);
    assert_eq!(std_posture.base_transition_duration_ms, 140);

    // 2. OS prefers-reduced-motion=true freezes Gerono seal to static closed curve & 0 RAF loops
    let os_reduced = a11y_set_reduced_motion_override_inner(&state, Some(true), None, Some(true))
        .expect("OS reduced motion posture");
    assert!(os_reduced.effective_reduced_motion);
    assert_eq!(os_reduced.gerono_seal_mode, "static-reduced-motion");
    assert_eq!(os_reduced.active_raf_loop_count, 0);
    assert_eq!(os_reduced.fast_transition_duration_ms, 0);
    assert_eq!(os_reduced.base_transition_duration_ms, 0);

    // 3. Explicit user override (true) forces reduced motion even when OS prefers-reduced-motion=false
    let user_reduced =
        a11y_set_reduced_motion_override_inner(&state, Some(false), Some(true), Some(true))
            .expect("user override reduced motion");
    assert!(user_reduced.effective_reduced_motion);
    assert_eq!(user_reduced.gerono_seal_mode, "static-reduced-motion");
    assert_eq!(user_reduced.active_raf_loop_count, 0);
}
