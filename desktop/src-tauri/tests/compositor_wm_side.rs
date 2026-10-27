//! Stage D (`W70-D-COMPOSITOR` / `D-COMPOSITOR-WM`) 桌面合成器、多用户会话/工作区隔离、
//! 窗口/表面耦合生命周期、`[DUI-LAUNCH-001]` 应用启动防混淆与 `[DUI-INPUT-001]` 防伪输入路由集成测试。

use llmos_desktop_lib::compositor::{
    ApplicationLauncher, CompositorError, DeclaredWindowSurfaceSpec, DesktopCompositor,
    FLOATING_BORDER_RADIUS_PX, InstalledApplicationCatalogEntry, LaunchIdentity, LauncherError,
    RectPx, TASKBAR_HEIGHT_PX, TILE_GAP_PX, TILED_BORDER_RADIUS_PX, WINDOW_TITLEBAR_HEIGHT_PX,
    WorkstationHostShell, WorkstationLayoutMode, Z0_BACKDROP_Z, Z1_TASK_SPACE_Z, Z2_WINDOW_Z_MAX,
    Z2_WINDOW_Z_MIN, Z3_FLYOUT_Z, Z3_PALETTE_Z, Z3_TASKBAR_Z, Z4_SECURITY_SCREEN_Z,
    clamp_floating_rect, compute_stage_rect, compute_taskbar_rect,
};
use llmos_desktop_lib::dto::compositor_snapshot_dto;
use llmos_desktop_lib::input_router::{
    InputEventEnvelope, InputOrigin, InputRejectionReason, InputSource, InputTarget,
    Z4_ARMING_WINDOW_MS,
};
use llmos_desktop_lib::session::{
    DesktopSessionManager, DesktopSessionState, SessionError, UserProfile, WorkspaceState,
    WorkstationLocale, WorkstationTheme,
};
use llmos_desktop_lib::window_lifecycle::{
    CoupledWindowSurface, SurfaceLifecycle, WindowLifecycle, WindowLifecycleError,
};
use nlos_types::{ApplicationId, InstallationId, PackageId, ProcessId};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ============================================================================
// 1. DesktopSession & 多用户 Workspace 容器隔离与私密防钓鱼印章边界 ([DUI-SESSION-001])
// ============================================================================

#[test]
fn desktop_session_lifecycle_multi_user_workspace_isolation_and_seal_privacy() {
    let alice_id = [0x11; 16];
    let bob_id = [0x22; 16];
    let alice_ws_main = [0xA1; 16];
    let alice_ws_research = [0xA2; 16];
    let bob_ws_main = [0xB1; 16];

    let alice = UserProfile::with_isolated_scopes(
        alice_id,
        "Alice Operator",
        "玄武-42-ALICE-SEAL",
        WorkstationLocale::ZhCn,
        WorkstationTheme::GraphiteWorkstation,
    )
    .expect("create alice profile");
    let bob = UserProfile::with_isolated_scopes(
        bob_id,
        "Bob Engineer",
        "朱雀-99-BOB-SEAL",
        WorkstationLocale::EnUs,
        WorkstationTheme::PaperWorkstation,
    )
    .expect("create bob profile");

    assert_ne!(alice.data_container_scope_id, bob.data_container_scope_id);
    assert_ne!(alice.keychain_namespace_id, bob.keychain_namespace_id);
    assert_ne!(alice.data_container_scope_id, alice.keychain_namespace_id);

    // 碰撞作用域的用户档案必须被拒绝注册。
    let colliding_profile = UserProfile::new(
        [0x33; 16],
        "Mallory",
        "白虎-00",
        WorkstationLocale::EnUs,
        WorkstationTheme::System,
        alice.data_container_scope_id,
        [0x34; 16],
    )
    .expect("construct profile");

    let mut session = DesktopSessionManager::new_locked(alice, alice_ws_main, "Alice 主工作区")
        .expect("init locked session");
    assert_eq!(session.state(), DesktopSessionState::Locked);
    assert_eq!(session.session_generation(), 1);

    let collision_err = session
        .register_user_with_default_workspace(colliding_profile, [0xC1; 16], "Mallory WS")
        .expect_err("colliding data_container_scope_id must be rejected");
    assert!(matches!(
        collision_err,
        SessionError::ContainerScopeCollision { .. }
    ));

    session
        .register_user_with_default_workspace(bob, bob_ws_main, "Bob Main")
        .expect("register bob");

    // LOCKED 态拒绝签发瞬态能力凭证。
    let locked_grant_err = session
        .issue_transient_grant([0x71; 16], alice_ws_main)
        .expect_err("LOCKED session must reject transient grant");
    assert!(matches!(
        locked_grant_err,
        SessionError::SessionNotActive {
            state: DesktopSessionState::Locked
        }
    ));

    // LOCKED -> ACTIVE（解锁保持当前代际 1）。
    assert_eq!(session.unlock().expect("unlock"), 1);
    assert_eq!(session.state(), DesktopSessionState::Active);

    // 签发瞬态会话凭证并在当前代际校验通过。
    let grant_gen1 = session
        .issue_transient_grant([0x71; 16], alice_ws_main)
        .expect("issue grant in ACTIVE gen 1");
    assert_eq!(grant_gen1.issued_session_generation, 1);
    session
        .validate_transient_grant([0x71; 16], alice_ws_main)
        .expect("grant valid in gen 1");

    // 创建第二个工作区并验证 CREATED -> ACTIVE <-> ARCHIVED -> CLOSED 准入锁。
    let created_ws = session
        .create_workspace(alice_ws_research, "研发工作区", false)
        .expect("create research ws");
    assert_eq!(created_ws.state, WorkspaceState::Created);
    assert!(!created_ws.state.can_admit_work());

    session
        .activate_workspace(alice_ws_research)
        .expect("activate research ws");
    assert_eq!(
        session.active_workspace().expect("active ws").workspace_id,
        alice_ws_research
    );

    session
        .archive_workspace(alice_ws_research)
        .expect("archive research ws");
    let archived_grant_err = session
        .issue_transient_grant([0x72; 16], alice_ws_research)
        .expect_err("ARCHIVED workspace must lock new work admission");
    assert!(matches!(
        archived_grant_err,
        SessionError::WorkspaceAdmissionLocked {
            state: WorkspaceState::Archived,
            ..
        }
    ));
    session
        .activate_workspace(alice_ws_main)
        .expect("switch back to alice main ws");

    // 装入合成器壳以同时验证防钓鱼印章隔离与跨代际输入丢弃。
    let mut shell = WorkstationHostShell::new(session, DesktopCompositor::new(1440, 900));
    shell
        .launcher
        .register_catalog_entry(InstalledApplicationCatalogEntry {
            application_id: ApplicationId::from_bytes([0x51; 16]),
            installation_id: InstallationId::from_bytes([0x61; 16]),
            package_id: PackageId::from_bytes([0x81; 16]),
            current_installation_generation: 1,
            surfaces: vec![DeclaredWindowSurfaceSpec {
                surface_id: [0x91; 16],
                title: "终端控制台".to_owned(),
                entry_name: Some("main".to_owned()),
            }],
            known_process_ids: vec![],
        });
    let opened = shell
        .launcher
        .launch_from_catalog(
            ApplicationId::from_bytes([0x51; 16]),
            InstallationId::from_bytes([0x61; 16]),
            &shell.session,
            &mut shell.compositor,
            &mut shell.route,
        )
        .expect("launch window in alice ws");
    let alice_win_id = opened[0];

    // 公共快照 JSON 绝不包含 security_seal_hint 字段或私密印章明文！
    let public_snap = compositor_snapshot_dto(&shell, false).expect("public snapshot");
    let public_json = serde_json::to_string(&public_snap).expect("serialize public snapshot");
    assert!(!public_json.contains("securitySealHint"));
    assert!(!public_json.contains("玄武-42-ALICE-SEAL"));
    assert!(public_snap.z4_trusted_context.is_none());

    // 仅 Z4 可信投影显式请求时才携带防钓鱼印章。
    let z4_snap = compositor_snapshot_dto(&shell, true).expect("z4 trusted snapshot");
    let z4_ctx = z4_snap
        .z4_trusted_context
        .expect("z4 trusted context present");
    assert_eq!(z4_ctx.security_seal_hint, "玄武-42-ALICE-SEAL");
    assert_eq!(z4_ctx.arming_window_ms, Z4_ARMING_WINDOW_MS);

    // ACTIVE -> LOCKED：session_generation 1 -> 2，旧瞬态凭证立即失效，在途旧会话输入立即丢弃。
    let gen_after_lock = shell
        .transition_session("lock", None, Some(1_000))
        .expect("lock session");
    assert_eq!(gen_after_lock, 2);
    assert_eq!(shell.session.state(), DesktopSessionState::Locked);
    assert!(shell.route.focused_window_id.is_none());

    // 在途携带旧 session_generation=1 的事件被丢弃。
    let stale_session_in_flight = shell
        .dispatch_input(&InputEventEnvelope {
            source: InputSource::Keyboard,
            origin: InputOrigin::TrustedShell,
            target: InputTarget::Z2Surface {
                window_id: alice_win_id,
                surface_id: [0x91; 16],
                target_surface_generation: 1,
            },
            event_session_generation: 1,
            now_mono_ms: 1_010,
        })
        .expect_err("stale session generation 1 event must be rejected");
    assert_eq!(
        stale_session_in_flight.reason,
        InputRejectionReason::LockedOrStaleSession
    );

    // LOCKED 态下即便携带最新代际 2，发往 Z2 的输入也一律被拒绝。
    let locked_z2_attempt = shell
        .dispatch_input(&InputEventEnvelope {
            source: InputSource::Pointer,
            origin: InputOrigin::TrustedShell,
            target: InputTarget::Z2Surface {
                window_id: alice_win_id,
                surface_id: [0x91; 16],
                target_surface_generation: 1,
            },
            event_session_generation: 2,
            now_mono_ms: 1_020,
        })
        .expect_err("Z2 input during LOCKED session must be rejected");
    assert_eq!(
        locked_z2_attempt.reason,
        InputRejectionReason::LockedOrStaleSession
    );
    assert_eq!(shell.route.counters.rejected_locked_session_events, 2);

    // LOCKED 态下可信 Z4SessionUnlock 允许通过。
    let unlock_routed = shell
        .dispatch_input(&InputEventEnvelope {
            source: InputSource::Keyboard,
            origin: InputOrigin::TrustedShell,
            target: InputTarget::Z4SessionUnlock,
            event_session_generation: 2,
            now_mono_ms: 1_030,
        })
        .expect("trusted Z4 session unlock allowed while LOCKED");
    assert_eq!(unlock_routed.routed_plane, "Z4");

    // 解锁后检验旧凭证已被撤销（GrantRevokedOrStale）。
    shell
        .transition_session("unlock", None, Some(1_040))
        .expect("unlock to ACTIVE gen 2");
    let revoked_err = shell
        .session
        .validate_transient_grant([0x71; 16], alice_ws_main)
        .expect_err("gen-1 grant must be revoked after lock");
    assert!(matches!(
        revoked_err,
        SessionError::GrantRevokedOrStale {
            issued_session_generation: 1,
            current_session_generation: 2,
            ..
        }
    ));

    // ACTIVE -> SUSPENDED -> ACTIVE：suspend 递增代际 2 -> 3。
    assert_eq!(
        shell
            .transition_session("suspend", None, Some(2_000))
            .expect("suspend"),
        3
    );
    assert_eq!(shell.session.state(), DesktopSessionState::Suspended);
    assert_eq!(
        shell
            .transition_session("resume", None, Some(2_100))
            .expect("resume"),
        3
    );
    assert_eq!(shell.session.state(), DesktopSessionState::Active);

    // 切换用户 Alice -> Bob：递增代际 3 -> 4，且严格阻断跨用户访问 Alice 的工作区！
    assert_eq!(
        shell
            .transition_session("switch_user", Some(bob_id), Some(3_000))
            .expect("switch to bob"),
        4
    );
    assert_eq!(shell.session.active_principal_id(), bob_id);

    let cross_inspect = shell
        .session
        .inspect_workspace_for_active_user(alice_ws_main)
        .expect_err("Bob must not inspect Alice workspace");
    assert!(matches!(
        cross_inspect,
        SessionError::CrossUserWorkspaceAccessDenied { .. }
    ));

    let cross_activate = shell
        .session
        .activate_workspace(alice_ws_main)
        .expect_err("Bob must not activate Alice workspace");
    assert!(matches!(
        cross_activate,
        SessionError::CrossUserWorkspaceAccessDenied { .. }
    ));

    // Bob 的公共快照只暴露 Bob 的工作区，且不暴露 Alice 的窗口或 Bob 的印章。
    let bob_snap = compositor_snapshot_dto(&shell, false).expect("bob snapshot");
    assert_eq!(bob_snap.active_user.principal_id_hex, hex(&bob_id));
    assert_eq!(bob_snap.workspaces.len(), 1);
    assert_eq!(bob_snap.workspaces[0].workspace_id_hex, hex(&bob_ws_main));
    assert!(bob_snap.windows.is_empty());

    // ACTIVE -> CLOSED（终态，代际 4 -> 5，后续任何转换返回 AlreadyClosed）。
    assert_eq!(
        shell
            .transition_session("close", None, Some(4_000))
            .expect("close session"),
        5
    );
    assert_eq!(shell.session.state(), DesktopSessionState::Closed);
    let closed_err = shell
        .session
        .unlock()
        .expect_err("CLOSED session must reject unlock");
    assert_eq!(closed_err, SessionError::AlreadyClosed);
}

// ============================================================================
// 2. WindowLifecycle <-> SurfaceLifecycle 耦合、5 平面 Z 序、StageRect 钳制与遮挡门禁
// ============================================================================

#[test]
fn coupled_window_surface_lifecycle_five_plane_z_order_stage_clamping_and_occlusion() {
    // 2.1 双轨耦合状态机校验
    let pair = CoupledWindowSurface::new_created();
    assert_eq!(pair.window, WindowLifecycle::Created);
    assert_eq!(pair.surface, SurfaceLifecycle::Created);
    assert!(!pair.can_accept_input());
    assert_eq!(pair.minimize(), Err(WindowLifecycleError::NotVisible));

    let visible = pair.show().expect("Created -> Visible / Presented");
    assert_eq!(visible.window, WindowLifecycle::Visible);
    assert_eq!(visible.surface, SurfaceLifecycle::Presented);
    assert!(visible.can_accept_input());

    let minimized = visible.minimize().expect("Visible -> Minimized / Hidden");
    assert_eq!(minimized.window, WindowLifecycle::Minimized);
    assert_eq!(minimized.surface, SurfaceLifecycle::Hidden);
    assert!(!minimized.can_accept_input());

    let restored = minimized
        .present()
        .expect("Minimized -> Visible / Presented");
    assert!(restored.can_accept_input());

    let closing = restored
        .begin_close()
        .expect("Visible -> Closing immediately hides surface");
    assert_eq!(closing.window, WindowLifecycle::Closing);
    assert_eq!(closing.surface, SurfaceLifecycle::Hidden);
    assert!(!closing.can_accept_input());
    assert_eq!(closing.show(), Err(WindowLifecycleError::ClosingInProgress));

    let closed = closing.finish_close().expect("Closing -> Closed");
    assert_eq!(closed.window, WindowLifecycle::Closed);
    assert_eq!(closed.surface, SurfaceLifecycle::Closed);
    assert_eq!(closed.show(), Err(WindowLifecycleError::AlreadyClosed));

    // 2.2 五平面严格单调 Z 序常量不变量
    const {
        assert!(Z0_BACKDROP_Z < Z1_TASK_SPACE_Z);
        assert!(Z1_TASK_SPACE_Z < Z2_WINDOW_Z_MIN);
        assert!(Z2_WINDOW_Z_MIN < Z2_WINDOW_Z_MAX);
        assert!(Z2_WINDOW_Z_MAX < Z3_TASKBAR_Z);
        assert!(Z3_TASKBAR_Z < Z3_FLYOUT_Z);
        assert!(Z3_FLYOUT_Z < Z3_PALETTE_Z);
        assert!(Z3_PALETTE_Z < Z4_SECURITY_SCREEN_Z);
    }

    // 2.3 多分辨率与全部 5 种布局模式下的 StageRect 边界钳制与子表面裁剪
    let mut shell = WorkstationHostShell::new_default_workstation();
    for idx in 1_u8..=4 {
        shell
            .launcher
            .register_catalog_entry(InstalledApplicationCatalogEntry {
                application_id: ApplicationId::from_bytes([idx; 16]),
                installation_id: InstallationId::from_bytes([idx.wrapping_add(0x20); 16]),
                package_id: PackageId::from_bytes([idx.wrapping_add(0x40); 16]),
                current_installation_generation: 1,
                surfaces: vec![DeclaredWindowSurfaceSpec {
                    surface_id: [idx.wrapping_add(0x60); 16],
                    title: format!("工作站面板-{idx}"),
                    entry_name: Some("main".to_owned()),
                }],
                known_process_ids: vec![],
            });
        shell
            .launcher
            .launch_from_catalog(
                ApplicationId::from_bytes([idx; 16]),
                InstallationId::from_bytes([idx.wrapping_add(0x20); 16]),
                &shell.session,
                &mut shell.compositor,
                &mut shell.route,
            )
            .expect("launch sample app");
    }

    let resolutions = [
        (1024_u32, 768_u32),
        (1440, 900),
        (1920, 1080),
        (2560, 1440),
        (3840, 2160),
    ];
    let modes = [
        WorkstationLayoutMode::ThreePaneWorkstation,
        WorkstationLayoutMode::TwoColumnSplit,
        WorkstationLayoutMode::Quadrants,
        WorkstationLayoutMode::WorkspaceMaximize,
        WorkstationLayoutMode::Floating,
    ];

    for (vw, vh) in resolutions {
        let expected_stage = compute_stage_rect(vw, vh);
        let expected_taskbar = compute_taskbar_rect(vw, vh);
        assert_eq!(expected_stage.height, vh - TASKBAR_HEIGHT_PX);
        assert_eq!(expected_taskbar.y, (vh - TASKBAR_HEIGHT_PX) as i32);
        assert_eq!(expected_taskbar.height, TASKBAR_HEIGHT_PX);

        // 越界浮动矩形钳制：负坐标与超大宽高均不得越过标题栏顶部或侵入底部 36px 任务栏。
        let clamped_neg = clamp_floating_rect(RectPx::new(-999, -500, 9999, 9999), vw, vh);
        assert!(clamped_neg.x >= 0);
        assert!(clamped_neg.y >= 0);
        assert!(clamped_neg.width <= vw);
        assert!(clamped_neg.bottom() <= (vh - TASKBAR_HEIGHT_PX) as i32);
        assert!(!clamped_neg.intersects(&expected_taskbar));

        for mode in modes {
            shell
                .set_layout(
                    mode.as_str(),
                    Some(vw),
                    Some(vh),
                    Some(680),
                    Some(500),
                    Some(true),
                )
                .expect("set layout mode");
            let snap = compositor_snapshot_dto(&shell, false).expect("snapshot");
            assert_eq!(snap.windows.len(), 4);

            if mode == WorkstationLayoutMode::ThreePaneWorkstation {
                let z1 = snap
                    .z1_task_space_rect
                    .expect("Z1 task space visible in ThreePaneWorkstation");
                assert!(z1.y + (z1.height as i32) <= (vh - TASKBAR_HEIGHT_PX) as i32);
            }

            if mode == WorkstationLayoutMode::TwoColumnSplit {
                let left = &snap.windows[0].outer_frame_rect;
                let right = &snap.windows[1].outer_frame_rect;
                assert_eq!(left.width + TILE_GAP_PX + right.width, vw);
                assert_eq!(right.x, (left.width + TILE_GAP_PX) as i32);
            }

            for win in &snap.windows {
                assert!(win.z_index >= Z2_WINDOW_Z_MIN && win.z_index <= Z2_WINDOW_Z_MAX);
                assert!(win.outer_frame_rect.x >= 0);
                assert!(win.outer_frame_rect.y >= 0);
                assert!(
                    win.outer_frame_rect.y + (win.outer_frame_rect.height as i32)
                        <= (vh - TASKBAR_HEIGHT_PX) as i32,
                    "window outer rect must never cover bottom 36px taskbar in mode {:?} at {vw}x{vh}",
                    mode
                );
                let expected_radius = if mode == WorkstationLayoutMode::Floating {
                    FLOATING_BORDER_RADIUS_PX
                } else {
                    TILED_BORDER_RADIUS_PX
                };
                assert_eq!(win.border_radius_px, expected_radius);

                // 子表面裁剪严格位于 30px 标题栏下方与 1px 左右/底边框之内。
                assert_eq!(win.inner_content_clip_rect.x, win.outer_frame_rect.x + 1);
                assert_eq!(
                    win.inner_content_clip_rect.y,
                    win.outer_frame_rect.y + WINDOW_TITLEBAR_HEIGHT_PX as i32
                );
                assert_eq!(
                    win.inner_content_clip_rect.width,
                    win.outer_frame_rect.width - 2
                );
                assert_eq!(
                    win.inner_content_clip_rect.height,
                    win.outer_frame_rect.height - (WINDOW_TITLEBAR_HEIGHT_PX + 1)
                );
                assert_eq!(win.a11y.a11y_role, "region");
                assert!(win.a11y.a11y_label.starts_with("Window: 工作站面板-"));
                assert!(!win.a11y.hidden);
            }
        }
    }

    // 2.4 Z3 浮层局部相交遮挡与 Z4 全屏安全锁遮挡校验
    shell
        .set_layout(
            "two-column-split",
            Some(1440),
            Some(900),
            None,
            Some(500),
            Some(false),
        )
        .expect("reset 1440x900 two-column-split");
    // 左列 x=0..718，右列 x=722..1440。放置一个仅在右半侧的 Z3 Flyout（x=900..1300）。
    shell
        .set_z3_overlay(
            "tray-network",
            "flyout",
            true,
            Some(RectPx::new(900, 500, 400, 260)),
        )
        .expect("activate right-side Z3 flyout");
    let snap_z3 = compositor_snapshot_dto(&shell, false).expect("snapshot with Z3 flyout");
    assert_eq!(snap_z3.active_z3_overlays[0].z_index, Z3_FLYOUT_Z);
    assert!(
        !snap_z3.windows[0].occluded_by_system_plane,
        "left-column window does not intersect right-side flyout"
    );
    assert!(
        snap_z3.windows[1].occluded_by_system_plane,
        "right-column window intersects Z3 flyout and must be marked occluded"
    );

    // 激活 Z4 安全模态锁后，所有窗口（含左列）全部进入 occluded_by_system_plane = true。
    shell.set_z4_modal(true, 5_000);
    let snap_z4 = compositor_snapshot_dto(&shell, false).expect("snapshot with Z4 lock");
    assert!(
        snap_z4
            .windows
            .iter()
            .all(|win| win.occluded_by_system_plane)
    );

    shell.set_z4_modal(false, 5_500);
    shell
        .set_z3_overlay("tray-network", "flyout", false, None)
        .expect("dismiss Z3 flyout");
    let snap_clear = compositor_snapshot_dto(&shell, false).expect("cleared snapshot");
    assert!(
        snap_clear
            .windows
            .iter()
            .all(|win| !win.occluded_by_system_plane)
    );
}

#[test]
fn catalog_launcher_identity_rejection_and_generation_advance() {
    let mut shell = WorkstationHostShell::new_default_workstation();
    let app_id = ApplicationId::from_bytes([0x61; 16]);
    let inst_v1 = InstallationId::from_bytes([0x62; 16]);
    let inst_v2 = InstallationId::from_bytes([0x63; 16]);
    let pkg_id = PackageId::from_bytes([0x64; 16]);
    let proc_id = ProcessId::from_bytes([0x65; 16]);

    let standalone_launcher = ApplicationLauncher::new();
    assert!(matches!(
        standalone_launcher.launch_by_package_id_rejected(pkg_id),
        Err(LauncherError::PackageIdNotApplicationIdentity { .. })
    ));

    shell
        .launcher
        .register_catalog_entry(InstalledApplicationCatalogEntry {
            application_id: app_id,
            installation_id: inst_v1,
            package_id: pkg_id,
            current_installation_generation: 1,
            surfaces: vec![DeclaredWindowSurfaceSpec {
                surface_id: [0x66; 16],
                title: "目录应用".to_owned(),
                entry_name: None,
            }],
            known_process_ids: vec![proc_id],
        });

    assert_eq!(
        shell.launcher.launch_by_identity(
            LaunchIdentity::PackageId(pkg_id),
            &shell.session,
            &mut shell.compositor,
            &mut shell.route,
        ),
        Err(LauncherError::PackageIdNotApplicationIdentity {
            offending_id: *pkg_id.as_bytes()
        })
    );
    assert_eq!(
        shell.launcher.launch_by_identity(
            LaunchIdentity::ProcessId(proc_id),
            &shell.session,
            &mut shell.compositor,
            &mut shell.route,
        ),
        Err(LauncherError::ProcessIdNotApplicationIdentity {
            offending_id: *proc_id.as_bytes()
        })
    );

    let win_id = shell
        .launcher
        .launch_by_identity(
            LaunchIdentity::ApplicationInstallation {
                application_id: app_id,
                installation_id: inst_v1,
            },
            &shell.session,
            &mut shell.compositor,
            &mut shell.route,
        )
        .expect("launch catalog app")[0];

    let marked = shell.launcher.advance_application_generation(
        app_id,
        2,
        Some(inst_v2),
        &mut shell.compositor,
        &mut shell.route,
    );
    assert_eq!(marked, 1);
    let focus_err = shell
        .window_action(win_id, "focus", None, None, None)
        .expect_err("stale window refuses focus");
    assert!(matches!(
        focus_err,
        CompositorError::StaleWindowRequiresReloadOrClose { .. }
    ));
}

// ============================================================================
// 3–5. 真实 ApplicationAuthority + ArtifactStore + IdentityAuthority 端到端集成测试
// ============================================================================

#[cfg(feature = "dev-fixture")]
mod authority_backed {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ed25519_dalek::Signer;
    use llmos_desktop_lib::error::ErrorCode;
    use llmos_desktop_lib::ipc::{
        AppState, CompositorInputDispatchInput, RectPxInput, SessionConfig,
        compositor_get_snapshot_core, compositor_launch_application_core,
        compositor_route_input_core, compositor_session_transition_core,
        compositor_set_layout_core, compositor_set_z3_overlay_core, compositor_set_z4_modal_core,
        compositor_window_action_core, compositor_workspace_action_core,
    };
    use nlos_application::{
        ApplicationAuthority, CompatibilityWindow, InstallApplicationRequest, InstallDecision,
        PackageSurfaceDeclaration, PackageSurfaceKind, RegisterProcessBindingRequest,
        RegisterSurfacesDecision, RegisterSurfacesRequest, UpdateApplicationRequest,
        UpdateDecision,
    };
    use nlos_artifact::{
        ArtifactStore, ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageManifest,
        PackageManifestEntry, ProvenanceSourceTriple, PutRevisionRequest, SignedPackage,
        VerifyPackageRequest, package_manifest_message,
    };
    use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
    use nlos_types::{ArtifactId, IdempotencyKey, PrincipalId};

    static NEXT_TEST_SEQ: AtomicU64 = AtomicU64::new(1);

    fn unique_temp_root(tag: &str) -> PathBuf {
        let seq = NEXT_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "llmosdt-w70-comp-{tag}-{}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    struct SignedPackageHarness {
        root: PathBuf,
        authority_root: PathBuf,
        artifacts: ArtifactStore,
        identity: IdentityAuthority,
        authority: ApplicationAuthority,
        signing_key: ed25519_dalek::SigningKey,
        principal: PrincipalId,
    }

    impl SignedPackageHarness {
        fn new(tag: &str, seed: u8) -> Self {
            let root = unique_temp_root(tag);
            let authority_root = root.join("app-authority");
            std::fs::create_dir_all(&authority_root).expect("create authority root");
            let artifacts = ArtifactStore::open(root.join("artifacts")).expect("open artifacts");
            let identity = IdentityAuthority::open(root.join("identity")).expect("open identity");
            let authority =
                ApplicationAuthority::open(&authority_root).expect("open application authority");
            let signing_key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
            let binding = identity
                .bootstrap_principal(BootstrapPrincipalRequest {
                    principal_profile_digest: [seed.wrapping_add(1); 32],
                    control_domain_policy_digest: [seed.wrapping_add(2); 32],
                    public_key: signing_key.verifying_key().to_bytes(),
                    key_purpose: KeyPurpose::SemanticSigning,
                    key_valid_from_ms: 0,
                    key_valid_until_ms: 100_000,
                    idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
                    created_at_ms: 0,
                })
                .expect("bootstrap principal")
                .binding();
            Self {
                root,
                authority_root,
                artifacts,
                identity,
                authority,
                signing_key,
                principal: binding.principal_id,
            }
        }

        fn build_and_verify_package(
            &self,
            package_seed: u8,
            version: u64,
            payload: &[u8],
            verify_idem: [u8; 16],
            verified_at_ms: u64,
        ) -> nlos_artifact::PackageVerificationReceipt {
            let ver_tag = u8::try_from(version).unwrap_or(1);
            let mut art_bytes = [package_seed; 16];
            art_bytes[15] = ver_tag;
            let artifact_id = ArtifactId::from_bytes(art_bytes);

            let mut create_idem = [0xA0_u8.wrapping_add(package_seed); 16];
            create_idem[15] = ver_tag;

            self.artifacts
                .create_artifact(CreateArtifactSpec {
                    artifact_id,
                    idempotency_key: IdempotencyKey::from_bytes(create_idem),
                    content_type: "application/octet-stream".to_owned(),
                    application_id: None,
                    owner: Some("w70-compositor-test".to_owned()),
                    created_at_ms: 1_000,
                })
                .expect("create artifact");
            self.artifacts
                .put_revision(PutRevisionRequest {
                    artifact_id,
                    expected_head_revision: 0,
                    bytes: payload,
                    created_at_ms: 2_000,
                    provenance: ProvenanceSourceTriple {
                        source_a: [0xC1; 16],
                        source_b: [0xD1; 16],
                        source_digest: ContentDigest::of_bytes(payload),
                    },
                })
                .expect("put revision");

            let manifest = PackageManifest {
                package_id: PackageId::from_bytes([package_seed; 16]),
                version,
                entries: vec![PackageManifestEntry {
                    name: "main".to_owned(),
                    artifact_id,
                    digest: ContentDigest::of_bytes(payload),
                    role: PackageEntryRole::Executable,
                }],
            };
            let msg = package_manifest_message(&manifest);
            let signed = SignedPackage {
                manifest,
                signer: self.principal,
                signature: self.signing_key.sign(&msg).to_bytes(),
            };
            self.artifacts
                .verify_package(
                    &self.identity,
                    VerifyPackageRequest {
                        signed: &signed,
                        idempotency_key: IdempotencyKey::from_bytes(verify_idem),
                        verified_at_ms,
                    },
                )
                .expect("verify signed package")
                .receipt()
                .clone()
        }
    }

    impl Drop for SignedPackageHarness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn application_launcher_enforces_dui_launch_001_against_real_application_authority() {
        let harness = SignedPackageHarness::new("launch-001", 0x41);
        let verified_v1 = harness.build_and_verify_package(
            0x77,
            1,
            b"workstation-ide-surface-v1",
            [0x11; 16],
            5_000,
        );
        let install_v1 = match harness
            .authority
            .install_application(
                &harness.artifacts,
                InstallApplicationRequest {
                    package_verification_receipt_id: verified_v1.receipt_id,
                    idempotency_key: IdempotencyKey::from_bytes([0x12; 16]),
                    installed_at_ms: 6_000,
                },
            )
            .expect("install v1")
        {
            InstallDecision::Installed(receipt) => receipt,
            InstallDecision::Replayed(receipt) => receipt,
        };

        let surface_id = [0x31; 16];
        match harness
            .authority
            .register_surfaces(&RegisterSurfacesRequest {
                package_id: verified_v1.package_id,
                declared_manifest_digest: install_v1.package_manifest_digest,
                surfaces: vec![PackageSurfaceDeclaration {
                    surface_id,
                    kind: PackageSurfaceKind::Window,
                    title: "代码审计工作台".to_owned(),
                    entry_name: Some("main".to_owned()),
                }],
                registrant_principal: install_v1.installer_principal,
                idempotency_key: IdempotencyKey::from_bytes([0x13; 16]),
                registered_at_ms: 6_200,
            })
            .expect("register surfaces")
        {
            RegisterSurfacesDecision::Registered(rows) => assert_eq!(rows.len(), 1),
            RegisterSurfacesDecision::Replayed(_) => panic!("unexpected replay"),
        }

        let bound_process_id = ProcessId::from_bytes([0x88; 16]);
        harness
            .authority
            .register_process_binding(RegisterProcessBindingRequest {
                package_id: verified_v1.package_id,
                process_id: bound_process_id,
                registrant_principal: install_v1.installer_principal,
                idempotency_key: IdempotencyKey::from_bytes([0x14; 16]),
                registered_at_ms: 6_300,
            })
            .expect("register process binding");

        let mut shell = WorkstationHostShell::new_default_workstation();

        // 1) 显式选择器以 PackageId 或 ProcessId 启动 -> 确定性拒绝
        let pkg_selector_err = shell
            .launcher
            .launch_by_identity(
                LaunchIdentity::PackageId(verified_v1.package_id),
                &shell.session,
                &mut shell.compositor,
                &mut shell.route,
            )
            .expect_err("PackageId launch must be rejected");
        assert_eq!(
            pkg_selector_err,
            LauncherError::PackageIdNotApplicationIdentity {
                offending_id: *verified_v1.package_id.as_bytes()
            }
        );

        let pid_selector_err = shell
            .launcher
            .launch_by_identity(
                LaunchIdentity::ProcessId(bound_process_id),
                &shell.session,
                &mut shell.compositor,
                &mut shell.route,
            )
            .expect_err("ProcessId launch must be rejected");
        assert_eq!(
            pid_selector_err,
            LauncherError::ProcessIdNotApplicationIdentity {
                offending_id: *bound_process_id.as_bytes()
            }
        );

        // 2) 把 PackageId 伪装传入 launch_from_authority 的 application_id 或 installation_id -> 确定性拒绝
        let spoof_pkg_as_app = shell
            .launcher
            .launch_from_authority(
                &harness.authority,
                ApplicationId::from_bytes(*verified_v1.package_id.as_bytes()),
                install_v1.installation_id,
                &shell.session,
                &mut shell.compositor,
                &mut shell.route,
            )
            .expect_err("PackageId disguised as ApplicationId must fail");
        assert_eq!(
            spoof_pkg_as_app,
            LauncherError::PackageIdNotApplicationIdentity {
                offending_id: *verified_v1.package_id.as_bytes()
            }
        );

        // 3) 把已登记 ProcessId 伪装传入 launch_from_authority -> 确定性拒绝
        let spoof_pid_as_app = shell
            .launcher
            .launch_from_authority(
                &harness.authority,
                ApplicationId::from_bytes(*bound_process_id.as_bytes()),
                install_v1.installation_id,
                &shell.session,
                &mut shell.compositor,
                &mut shell.route,
            )
            .expect_err("ProcessId disguised as ApplicationId must fail");
        assert_eq!(
            spoof_pid_as_app,
            LauncherError::ProcessIdNotApplicationIdentity {
                offending_id: *bound_process_id.as_bytes()
            }
        );

        let spoof_pid_as_inst = shell
            .launcher
            .launch_from_authority(
                &harness.authority,
                install_v1.application_id,
                InstallationId::from_bytes(*bound_process_id.as_bytes()),
                &shell.session,
                &mut shell.compositor,
                &mut shell.route,
            )
            .expect_err("ProcessId disguised as InstallationId must fail");
        assert_eq!(
            spoof_pid_as_inst,
            LauncherError::ProcessIdNotApplicationIdentity {
                offending_id: *bound_process_id.as_bytes()
            }
        );

        // 4) 唯有合法的 (ApplicationId, InstallationId) 允许激活窗口并获得焦点
        let launched = shell
            .launcher
            .launch_from_authority(
                &harness.authority,
                install_v1.application_id,
                install_v1.installation_id,
                &shell.session,
                &mut shell.compositor,
                &mut shell.route,
            )
            .expect("valid (ApplicationId, InstallationId) must launch");
        assert_eq!(launched.len(), 1);
        let win = shell.compositor.window(launched[0]).expect("opened window");
        assert_eq!(
            win.owner_application_id,
            *install_v1.application_id.as_bytes()
        );
        assert_eq!(win.installation_id, *install_v1.installation_id.as_bytes());
        assert_eq!(win.surface_id, surface_id);
        assert_eq!(win.surface_generation, 1);
        assert_eq!(win.lifecycle.window, WindowLifecycle::Visible);
        assert_eq!(win.lifecycle.surface, SurfaceLifecycle::Presented);
        assert_eq!(shell.route.focused_window_id, Some(win.window_id));
    }

    #[test]
    fn focus_and_input_route_fences_stale_generations_z4_modal_arming_and_spoofing() {
        let harness = SignedPackageHarness::new("input-route", 0x52);
        let verified_v1 =
            harness.build_and_verify_package(0x89, 1, b"telemetry-console-v1", [0x21; 16], 5_000);
        let install_v1 = match harness
            .authority
            .install_application(
                &harness.artifacts,
                InstallApplicationRequest {
                    package_verification_receipt_id: verified_v1.receipt_id,
                    idempotency_key: IdempotencyKey::from_bytes([0x22; 16]),
                    installed_at_ms: 6_000,
                },
            )
            .expect("install v1")
        {
            InstallDecision::Installed(receipt) => receipt,
            InstallDecision::Replayed(receipt) => receipt,
        };
        let surface_id = [0x42; 16];
        harness
            .authority
            .register_surfaces(&RegisterSurfacesRequest {
                package_id: verified_v1.package_id,
                declared_manifest_digest: install_v1.package_manifest_digest,
                surfaces: vec![PackageSurfaceDeclaration {
                    surface_id,
                    kind: PackageSurfaceKind::Window,
                    title: "遥测监控台 v1".to_owned(),
                    entry_name: Some("main".to_owned()),
                }],
                registrant_principal: install_v1.installer_principal,
                idempotency_key: IdempotencyKey::from_bytes([0x23; 16]),
                registered_at_ms: 6_200,
            })
            .expect("register v1 surface");

        let mut shell = WorkstationHostShell::new_default_workstation();
        let win_id = shell
            .launcher
            .launch_from_authority(
                &harness.authority,
                install_v1.application_id,
                install_v1.installation_id,
                &shell.session,
                &mut shell.compositor,
                &mut shell.route,
            )
            .expect("launch v1")[0];

        // 代际 1 正常输入通过。
        let ok_gen1 = shell
            .dispatch_input(&InputEventEnvelope {
                source: InputSource::Keyboard,
                origin: InputOrigin::TrustedShell,
                target: InputTarget::Z2Surface {
                    window_id: win_id,
                    surface_id,
                    target_surface_generation: 1,
                },
                event_session_generation: 1,
                now_mono_ms: 7_000,
            })
            .expect("gen 1 input accepted");
        assert_eq!(ok_gen1.routed_plane, "Z2");
        assert_eq!(shell.route.counters.accepted_events, 1);

        // 1) 不可信应用表面防伪拦截：伪造 Z3 快捷键、伪造 Z4 确认、伪造解锁、跨表面注入全部拦截！
        for spoof_target in [
            InputTarget::Z3SystemShortcut {
                shortcut: "Meta+K".to_owned(),
            },
            InputTarget::Z4SecurityAction {
                confirm: true,
                key: Some("Enter".to_owned()),
            },
            InputTarget::Z4SessionUnlock,
            InputTarget::Z2Surface {
                window_id: win_id,
                surface_id: [0xFE; 16], // 跨表面伪造
                target_surface_generation: 1,
            },
        ] {
            let err = shell
                .dispatch_input(&InputEventEnvelope {
                    source: InputSource::Automation,
                    origin: InputOrigin::UntrustedApplicationSurface {
                        surface_id,
                        application_id: *install_v1.application_id.as_bytes(),
                    },
                    target: spoof_target,
                    event_session_generation: 1,
                    now_mono_ms: 7_100,
                })
                .expect_err("untrusted spoof must be rejected");
            assert_eq!(err.reason, InputRejectionReason::UntrustedSystemSpoof);
        }
        assert_eq!(shell.route.counters.rejected_untrusted_spoof_events, 4);

        // 2) Z4 模态切断全部五类输入通道 + 350ms 武装防点击劫持门禁（0ms Escape 畅通）
        shell.set_z4_modal(true, 10_000);
        for src in [
            InputSource::Keyboard,
            InputSource::Pointer,
            InputSource::Voice,
            InputSource::DragDrop,
            InputSource::Automation,
        ] {
            let severed = shell
                .dispatch_input(&InputEventEnvelope {
                    source: src,
                    origin: InputOrigin::TrustedShell,
                    target: InputTarget::Z2Surface {
                        window_id: win_id,
                        surface_id,
                        target_surface_generation: 1,
                    },
                    event_session_generation: 1,
                    now_mono_ms: 10_050,
                })
                .expect_err("Z4 modal lock must sever all 5 Z2 input sources");
            assert_eq!(severed.reason, InputRejectionReason::Z4ModalSevered);
        }
        assert_eq!(shell.route.counters.rejected_z4_severed_events, 5);

        // +0ms 与 +349ms 的确认提交均被 350ms 武装窗拦截。
        for early_ts in [10_000_u64, 10_349] {
            let early = shell
                .dispatch_input(&InputEventEnvelope {
                    source: InputSource::Pointer,
                    origin: InputOrigin::TrustedShell,
                    target: InputTarget::Z4SecurityAction {
                        confirm: true,
                        key: Some("Enter".to_owned()),
                    },
                    event_session_generation: 1,
                    now_mono_ms: early_ts,
                })
                .expect_err("confirm before 350ms arming window must fail");
            assert_eq!(early.reason, InputRejectionReason::Z4ArmingWindowActive);
        }
        assert_eq!(shell.route.counters.rejected_z4_arming_guard_events, 2);

        // 0ms 即时 Escape 拒绝通道畅通无阻。
        let immediate_esc = shell
            .dispatch_input(&InputEventEnvelope {
                source: InputSource::Keyboard,
                origin: InputOrigin::TrustedShell,
                target: InputTarget::Z4SecurityAction {
                    confirm: true,
                    key: Some("Escape".to_owned()),
                },
                event_session_generation: 1,
                now_mono_ms: 10_000,
            })
            .expect("0ms Escape cancel must always succeed immediately");
        assert_eq!(immediate_esc.routed_plane, "Z4");

        // +350ms 到达武装阈值，确认放行。
        let armed_confirm = shell
            .dispatch_input(&InputEventEnvelope {
                source: InputSource::Pointer,
                origin: InputOrigin::TrustedShell,
                target: InputTarget::Z4SecurityAction {
                    confirm: true,
                    key: None,
                },
                event_session_generation: 1,
                now_mono_ms: 10_350,
            })
            .expect("350ms armed Z4 confirm must succeed");
        assert_eq!(armed_confirm.routed_plane, "Z4");
        shell.set_z4_modal(false, 10_400);

        // 3) 真实 ApplicationAuthority::update_application 推进代际 1 -> 2，旧窗口立即失焦并拒收输入！
        let verified_v2 =
            harness.build_and_verify_package(0x89, 2, b"telemetry-console-v2", [0x24; 16], 11_000);
        let install_v2 = match harness
            .authority
            .update_application(
                &harness.artifacts,
                UpdateApplicationRequest {
                    package_id: verified_v1.package_id,
                    package_verification_receipt_id: verified_v2.receipt_id,
                    idempotency_key: IdempotencyKey::from_bytes([0x25; 16]),
                    updated_at_ms: 11_500,
                    compatibility_window: CompatibilityWindow::SameMajor,
                },
            )
            .expect("update to v2")
        {
            UpdateDecision::Updated(receipt) => receipt,
            UpdateDecision::Replayed(receipt) => receipt,
        };
        assert_eq!(install_v2.installation_generation.get(), 2);

        harness
            .authority
            .register_surfaces(&RegisterSurfacesRequest {
                package_id: verified_v1.package_id,
                declared_manifest_digest: install_v2.package_manifest_digest,
                surfaces: vec![PackageSurfaceDeclaration {
                    surface_id,
                    kind: PackageSurfaceKind::Window,
                    title: "遥测监控台 v2".to_owned(),
                    entry_name: Some("main".to_owned()),
                }],
                registrant_principal: install_v2.installer_principal,
                idempotency_key: IdempotencyKey::from_bytes([0x26; 16]),
                registered_at_ms: 11_600,
            })
            .expect("register v2 surface");

        let marked = shell
            .sync_generations_from_authority(&harness.authority)
            .expect("sync generations from authority");
        assert_eq!(marked, 1);
        assert!(shell.route.focused_window_id.is_none());

        let stale_win = shell.compositor.window(win_id).expect("stale window");
        assert!(stale_win.stale_generation);
        assert!(stale_win.a11y.stale_readonly);
        assert_eq!(stale_win.surface_generation, 1);
        assert_eq!(stale_win.current_installation_generation, 2);

        // 尝试向过期窗口投递代际 1 或代际 2 输入均被 StaleOrUnpresentedSurface 拦截！
        for attempted_gen in [1_u64, 2] {
            let stale_err = shell
                .dispatch_input(&InputEventEnvelope {
                    source: InputSource::Pointer,
                    origin: InputOrigin::TrustedShell,
                    target: InputTarget::Z2Surface {
                        window_id: win_id,
                        surface_id,
                        target_surface_generation: attempted_gen,
                    },
                    event_session_generation: 1,
                    now_mono_ms: 12_000,
                })
                .expect_err("stale surface must reject input until reloaded");
            assert_eq!(
                stale_err.reason,
                InputRejectionReason::StaleOrUnpresentedSurface
            );
        }
        assert_eq!(shell.route.counters.rejected_stale_surface_events, 2);

        // 未重载前尝试聚焦过期窗口被拒绝。
        let focus_stale_err = shell
            .window_action(win_id, "focus", None, None, Some(&harness.authority))
            .expect_err("stale window must refuse focus before reload");
        assert!(matches!(
            focus_stale_err,
            CompositorError::StaleWindowRequiresReloadOrClose { .. }
        ));

        // 显式重载至代际 2 后恢复正常焦点与输入接收。
        shell
            .window_action(
                win_id,
                "reload",
                None,
                Some(install_v2.installation_id),
                Some(&harness.authority),
            )
            .expect("reload stale window to gen 2");

        let ok_gen2 = shell
            .dispatch_input(&InputEventEnvelope {
                source: InputSource::Keyboard,
                origin: InputOrigin::TrustedShell,
                target: InputTarget::Z2Surface {
                    window_id: win_id,
                    surface_id,
                    target_surface_generation: 2,
                },
                event_session_generation: 1,
                now_mono_ms: 12_500,
            })
            .expect("reloaded gen-2 window accepts input");
        assert_eq!(ok_gen2.routed_plane, "Z2");
    }

    #[test]
    fn tauri_ipc_compositor_commands_end_to_end_via_app_state() {
        let harness = SignedPackageHarness::new("ipc-e2e", 0x63);
        let verified_v1 =
            harness.build_and_verify_package(0x9A, 1, b"ipc-workbench-v1", [0x31; 16], 5_000);
        let install_v1 = match harness
            .authority
            .install_application(
                &harness.artifacts,
                InstallApplicationRequest {
                    package_verification_receipt_id: verified_v1.receipt_id,
                    idempotency_key: IdempotencyKey::from_bytes([0x32; 16]),
                    installed_at_ms: 6_000,
                },
            )
            .expect("install v1")
        {
            InstallDecision::Installed(receipt) => receipt,
            InstallDecision::Replayed(receipt) => receipt,
        };
        let surface_id = [0x53; 16];
        harness
            .authority
            .register_surfaces(&RegisterSurfacesRequest {
                package_id: verified_v1.package_id,
                declared_manifest_digest: install_v1.package_manifest_digest,
                surfaces: vec![PackageSurfaceDeclaration {
                    surface_id,
                    kind: PackageSurfaceKind::Window,
                    title: "IPC 工作台".to_owned(),
                    entry_name: Some("main".to_owned()),
                }],
                registrant_principal: install_v1.installer_principal,
                idempotency_key: IdempotencyKey::from_bytes([0x33; 16]),
                registered_at_ms: 6_100,
            })
            .expect("register surface");
        let bound_pid = ProcessId::from_bytes([0x79; 16]);
        harness
            .authority
            .register_process_binding(RegisterProcessBindingRequest {
                package_id: verified_v1.package_id,
                process_id: bound_pid,
                registrant_principal: install_v1.installer_principal,
                idempotency_key: IdempotencyKey::from_bytes([0x34; 16]),
                registered_at_ms: 6_150,
            })
            .expect("register process binding");

        let state = AppState::from_session(SessionConfig {
            application_root: Some(
                harness
                    .authority_root
                    .to_str()
                    .expect("utf8 path")
                    .to_owned(),
            ),
            ..SessionConfig::default()
        });

        // 1) compositor_get_snapshot
        let initial = compositor_get_snapshot_core(&state, Some(false)).expect("initial snapshot");
        assert_eq!(initial.session_state, "ACTIVE");
        assert_eq!(initial.session_generation, 1);
        assert!(initial.z4_trusted_context.is_none());

        // 2) compositor_launch_application 拒绝 PackageId 与 ProcessId，仅接受 (ApplicationId, InstallationId)
        let pkg_hex = hex(verified_v1.package_id.as_bytes());
        let pid_hex = hex(bound_pid.as_bytes());
        let app_hex = hex(install_v1.application_id.as_bytes());
        let inst_v1_hex = hex(install_v1.installation_id.as_bytes());

        let reject_pkg =
            compositor_launch_application_core(&state, None, None, Some(&pkg_hex), None)
                .expect_err("IPC launch by PackageId must fail");
        assert_eq!(reject_pkg.code, ErrorCode::Control);
        assert!(
            reject_pkg
                .message
                .contains("PACKAGE_ID_NOT_APPLICATION_IDENTITY")
        );

        let reject_pid = compositor_launch_application_core(
            &state,
            Some(&pid_hex),
            Some(&inst_v1_hex),
            None,
            None,
        )
        .expect_err("IPC launch by ProcessId disguised as ApplicationId must fail");
        assert_eq!(reject_pid.code, ErrorCode::Control);
        assert!(
            reject_pid
                .message
                .contains("PROCESS_ID_NOT_APPLICATION_IDENTITY")
        );

        let after_launch = compositor_launch_application_core(
            &state,
            Some(&app_hex),
            Some(&inst_v1_hex),
            None,
            None,
        )
        .expect("IPC launch with valid (ApplicationId, InstallationId)");
        assert_eq!(after_launch.windows.len(), 1);
        let win_hex = after_launch.windows[0].window_id_hex.clone();
        let surf_hex = after_launch.windows[0].surface_id_hex.clone();

        // 3) compositor_set_layout & compositor_window_action (float clamp + restore)
        let maxed = compositor_set_layout_core(
            &state,
            "workspace-maximize",
            Some(1920),
            Some(1080),
            None,
            None,
            Some(false),
        )
        .expect("maximize layout");
        assert_eq!(maxed.windows[0].outer_frame_rect.height, 1080 - 36);
        assert_eq!(maxed.taskbar_rect.y, 1080 - 36);

        let floated = compositor_window_action_core(
            &state,
            &win_hex,
            "float",
            Some(RectPxInput {
                x: -100,
                y: 950,
                width: 600,
                height: 400,
            }),
            None,
        )
        .expect("float window clamped above taskbar");
        assert_eq!(floated.windows[0].border_radius_px, 6);
        assert!(
            floated.windows[0].outer_frame_rect.y
                + (floated.windows[0].outer_frame_rect.height as i32)
                <= 1080 - 36
        );

        // 4) compositor_set_z3_overlay & compositor_set_z4_modal & compositor_route_input
        let with_palette = compositor_set_z3_overlay_core(
            &state,
            "command-palette",
            "palette",
            true,
            Some(RectPxInput {
                x: 0,
                y: 0,
                width: 1920,
                height: 400,
            }),
        )
        .expect("open command palette");
        assert_eq!(with_palette.active_z3_overlays[0].z_index, Z3_PALETTE_Z);

        let z4_armed = compositor_set_z4_modal_core(&state, true, 20_000).expect("arm Z4 modal");
        assert!(z4_armed.z4_trusted_context.is_some());
        assert!(z4_armed.windows[0].occluded_by_system_plane);

        let early_confirm = compositor_route_input_core(
            &state,
            &CompositorInputDispatchInput {
                source: "keyboard".to_owned(),
                origin_kind: Some("trusted_shell".to_owned()),
                origin_surface_id_hex: None,
                origin_application_id_hex: None,
                target_kind: "z4_security_action".to_owned(),
                window_id_hex: None,
                surface_id_hex: None,
                target_surface_generation: None,
                shortcut: None,
                confirm: Some(true),
                key: Some("Enter".to_owned()),
                event_session_generation: 1,
                now_mono_ms: 20_200,
            },
        )
        .expect("route early confirm");
        assert!(!early_confirm.accepted);
        assert_eq!(
            early_confirm.rejection_code.as_deref(),
            Some("Z4_ARMING_WINDOW_ACTIVE")
        );

        let valid_confirm = compositor_route_input_core(
            &state,
            &CompositorInputDispatchInput {
                source: "keyboard".to_owned(),
                origin_kind: Some("trusted_shell".to_owned()),
                origin_surface_id_hex: None,
                origin_application_id_hex: None,
                target_kind: "z4_security_action".to_owned(),
                window_id_hex: None,
                surface_id_hex: None,
                target_surface_generation: None,
                shortcut: None,
                confirm: Some(true),
                key: Some("Enter".to_owned()),
                event_session_generation: 1,
                now_mono_ms: 20_350,
            },
        )
        .expect("route valid confirm at 350ms");
        assert!(valid_confirm.accepted);
        assert_eq!(valid_confirm.routed_plane.as_deref(), Some("Z4"));

        compositor_set_z4_modal_core(&state, false, 20_400).expect("disarm Z4 modal");

        // 5) 磁盘 ApplicationAuthority 升级至代际 2 后，IPC 自动同步代际并拦截旧窗口输入
        let verified_v2 =
            harness.build_and_verify_package(0x9A, 2, b"ipc-workbench-v2", [0x35; 16], 21_000);
        let install_v2 = match harness
            .authority
            .update_application(
                &harness.artifacts,
                UpdateApplicationRequest {
                    package_id: verified_v1.package_id,
                    package_verification_receipt_id: verified_v2.receipt_id,
                    idempotency_key: IdempotencyKey::from_bytes([0x36; 16]),
                    updated_at_ms: 21_500,
                    compatibility_window: CompatibilityWindow::SameMajor,
                },
            )
            .expect("update v2")
        {
            UpdateDecision::Updated(receipt) => receipt,
            UpdateDecision::Replayed(receipt) => receipt,
        };
        harness
            .authority
            .register_surfaces(&RegisterSurfacesRequest {
                package_id: verified_v1.package_id,
                declared_manifest_digest: install_v2.package_manifest_digest,
                surfaces: vec![PackageSurfaceDeclaration {
                    surface_id,
                    kind: PackageSurfaceKind::Window,
                    title: "IPC 工作台 v2".to_owned(),
                    entry_name: Some("main".to_owned()),
                }],
                registrant_principal: install_v2.installer_principal,
                idempotency_key: IdempotencyKey::from_bytes([0x37; 16]),
                registered_at_ms: 21_600,
            })
            .expect("register v2 surface");

        let stale_routed = compositor_route_input_core(
            &state,
            &CompositorInputDispatchInput {
                source: "pointer".to_owned(),
                origin_kind: Some("trusted_shell".to_owned()),
                origin_surface_id_hex: None,
                origin_application_id_hex: None,
                target_kind: "z2_surface".to_owned(),
                window_id_hex: Some(win_hex.clone()),
                surface_id_hex: Some(surf_hex.clone()),
                target_surface_generation: Some(1),
                shortcut: None,
                confirm: None,
                key: None,
                event_session_generation: 1,
                now_mono_ms: 22_000,
            },
        )
        .expect("route stale input");
        assert!(!stale_routed.accepted);
        assert_eq!(
            stale_routed.rejection_code.as_deref(),
            Some("STALE_OR_UNPRESENTED_SURFACE")
        );

        let inst_v2_hex = hex(install_v2.installation_id.as_bytes());
        let reloaded =
            compositor_window_action_core(&state, &win_hex, "reload", None, Some(&inst_v2_hex))
                .expect("reload window via IPC");
        assert!(!reloaded.windows[0].stale_generation);
        assert_eq!(reloaded.windows[0].surface_generation, 2);

        // 6) compositor_workspace_action & compositor_session_transition
        let new_ws_hex = "d7".repeat(16);
        let ws_snap = compositor_workspace_action_core(&state, "create_and_activate", &new_ws_hex)
            .expect("create and activate workspace");
        assert_eq!(ws_snap.active_workspace.workspace_id_hex, new_ws_hex);

        let locked_snap = compositor_session_transition_core(&state, "lock", None, Some(25_000))
            .expect("lock via IPC");
        assert_eq!(locked_snap.session_state, "LOCKED");
        assert_eq!(locked_snap.session_generation, 2);
    }

    #[test]
    fn launcher_unit_can_be_constructed_standalone() {
        let launcher = ApplicationLauncher::new();
        let err = launcher
            .launch_by_package_id_rejected(PackageId::from_bytes([0xAB; 16]))
            .expect_err("reject package id");
        assert!(matches!(
            err,
            LauncherError::PackageIdNotApplicationIdentity { .. }
        ));
    }
}
