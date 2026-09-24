//! W32-B 写入半集成证据:GUI 的授权动作经同一编译点
//! (`build_control_command`)构造真实 `ControlCommand`,经 ADR-0011 认证
//! 入口派发到开发夹具的真实权威,验证:
//! (1) ack-recovery-alert 真实生效(TaskAuthority CAS mutation,回执携带
//!     权威 receipt 引用,后续 InspectHealth 观察到 acknowledged);
//! (2) CAS 预期不符 → 回执内类型化 CONFLICT 失败(与成功形态可区分);
//! (3) pause/kill 族在未接线执行器的夹具上真实穿越 submit 信封,得到
//!     类型化 NOT_FOUND(executor 未接线)失败回执,而非命令错误;
//! (4) 语义/资源域动作路由到各自 ledger(夹具只有 artifact escalated
//!     计划 → 类型化 NOT_FOUND)。

#![cfg(all(unix, feature = "dev-fixture"))]

use llmos_desktop_lib::devfixture::DevFixture;
use llmos_desktop_lib::dto::OutcomeDto;
use llmos_desktop_lib::dto::ReceiptDto;
use llmos_desktop_lib::error::DesktopError;
use llmos_desktop_lib::ipc::{ControlAction, build_control_command, dispatch_control};
use nlos_system_control::control::{ControlCommand, receipt_to_hex};

fn write_key_file(fixture: &DevFixture, label: &str) -> String {
    let path = std::env::temp_dir().join(format!(
        "llmosdt-wtest-key-{label}-{}.key",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    fixture.write_key_file(&path).expect("write key file");
    path.display().to_string()
}

async fn submit(
    fixture: &DevFixture,
    key_file: &str,
    action: ControlAction,
) -> Result<ReceiptDto, DesktopError> {
    // 与 Tauri `submit_control` 命令同一构造路径;固定 id 便于断言回显。
    let command = build_control_command(action, [0xD1; 16])?;
    dispatch_control(
        fixture
            .socket_authenticated()
            .display()
            .to_string()
            .as_str(),
        fixture.principal_hex(),
        key_file,
        command,
    )
    .await
}

#[tokio::test]
async fn ack_recovery_alert_mutates_authority_and_echoes_receipt_reference() {
    let mut fixture = DevFixture::spawn("w1").expect("fixture");
    let key_file = write_key_file(&fixture, "w1");
    let _loops = fixture.serve_forever();

    let plan_id_hex = fixture.plan_id_hex().to_owned();
    // CAS 预期取自 inspect 状态:夹具告警 total_failures=1(escalated)。
    let receipt = submit(
        &fixture,
        &key_file,
        ControlAction::AckRecoveryAlert {
            plan_id_hex: plan_id_hex.clone(),
            expected_total_failures: 1,
            reason: "desktop W32-B: acknowledge escalated alert".to_owned(),
        },
    )
    .await
    .expect("authenticated ack dispatch");
    assert_eq!(receipt.control_command_id_hex, "d1".repeat(16));
    let OutcomeDto::Acknowledged { receipt_id_hex } = &receipt.outcome else {
        panic!("expected acknowledged outcome, got {:?}", receipt.outcome);
    };
    assert!(!receipt_id_hex.is_empty());

    // 真实生效观察:durable_unacknowledged_escalated 归零,告警带上回执引用。
    let inspected = dispatch_control(
        fixture
            .socket_authenticated()
            .display()
            .to_string()
            .as_str(),
        fixture.principal_hex(),
        &key_file,
        ControlCommand::InspectHealth,
    )
    .await
    .expect("inspect after ack");
    let OutcomeDto::Inspected {
        durable_unacknowledged_escalated,
        alerts,
        ..
    } = &inspected.outcome
    else {
        panic!("expected inspection outcome, got {:?}", inspected.outcome);
    };
    assert_eq!(*durable_unacknowledged_escalated, 0);
    assert_eq!(alerts.len(), 1);
    assert_eq!(
        alerts[0].acknowledged_receipt_id_hex.as_deref(),
        Some(receipt_id_hex.as_str())
    );

    let _ = std::fs::remove_file(&key_file);
}

#[tokio::test]
async fn cas_mismatch_surfaces_typed_conflict_failure_receipt() {
    let mut fixture = DevFixture::spawn("w2").expect("fixture");
    let key_file = write_key_file(&fixture, "w2");
    let _loops = fixture.serve_forever();

    let receipt = submit(
        &fixture,
        &key_file,
        ControlAction::AckRecoveryAlert {
            plan_id_hex: fixture.plan_id_hex().to_owned(),
            expected_total_failures: 999,
            reason: "stale expectation".to_owned(),
        },
    )
    .await
    .expect("dispatch itself crosses the authenticated exchange");
    // 派发成功穿越;失败是回执内的类型化 SabiFailure(CONFLICT),不是命令错误。
    let OutcomeDto::Failure { code, retry, .. } = &receipt.outcome else {
        panic!("expected failure outcome, got {:?}", receipt.outcome);
    };
    assert!(code.contains("CONFLICT"), "code was {code}");
    assert_eq!(retry, "RETRY_DIRECTIVE_DO_NOT_RETRY");

    let _ = std::fs::remove_file(&key_file);
}

#[tokio::test]
async fn operation_commands_dispatch_real_submits_with_typed_unwired_failures() {
    let mut fixture = DevFixture::spawn("w3").expect("fixture");
    let key_file = write_key_file(&fixture, "w3");
    let _loops = fixture.serve_forever();

    // pause/kill/throttle/reclaim 四个代表形态:真实 submit 信封穿越认证
    // 入口;夹具未接线 OperationCommandExecutor → 确定性类型化 NOT_FOUND。
    let actions = [
        ControlAction::PauseOperation {
            target_id_hex: "41".repeat(16),
            expected_revision: 3,
            reason: "pause probe".to_owned(),
        },
        ControlAction::KillOperation {
            target_id_hex: "42".repeat(16),
            expected_revision: 7,
            reason: "kill probe".to_owned(),
        },
        ControlAction::ThrottleOperation {
            target_id_hex: "43".repeat(16),
            expected_revision: 2,
            throttle_percent: 50,
            reason: "throttle probe".to_owned(),
        },
        ControlAction::ReclaimOperation {
            target_id_hex: "44".repeat(16),
            expected_revision: 9,
            reason: "reclaim probe".to_owned(),
        },
    ];
    for action in actions {
        let receipt = submit(&fixture, &key_file, action).await.expect("dispatch");
        assert_eq!(receipt.control_command_id_hex, "d1".repeat(16));
        let OutcomeDto::Failure {
            code, safe_message, ..
        } = &receipt.outcome
        else {
            panic!(
                "expected unwired failure outcome, got {:?}",
                receipt.outcome
            );
        };
        assert!(code.contains("NOT_FOUND"), "code was {code}");
        assert!(
            safe_message.contains("not wired"),
            "safe_message was {safe_message}"
        );
    }

    // 写路径 parity 前驱(W32-C 钉死的最小形态):同一条 pause-operation
    // 命令(字节同一)经认证入口与 plain 入口(CLI 同路)各派发一次,
    // 失败回执字节一致——与读侧 B-TASK-006L 契约同源。
    let pause = ControlAction::PauseOperation {
        target_id_hex: "41".repeat(16),
        expected_revision: 3,
        reason: "pause probe".to_owned(),
    };
    let gui = submit(&fixture, &key_file, pause.clone())
        .await
        .expect("authenticated pause");
    let plain = nlos_system_control::control::dispatch_over_socket(
        fixture.socket_plain(),
        &build_control_command(pause, [0xD1; 16]).expect("build pause"),
        None,
        None,
        None,
    )
    .await
    .expect("plain pause");
    assert_eq!(gui.receipt_hex, receipt_to_hex(&plain));

    let _ = std::fs::remove_file(&key_file);
}

#[tokio::test]
async fn semantic_and_resource_arms_route_to_their_own_ledgers() {
    let mut fixture = DevFixture::spawn("w4").expect("fixture");
    let key_file = write_key_file(&fixture, "w4");
    let _loops = fixture.serve_forever();

    // 夹具只有 artifact escalated 计划;同一 plan_id 在 semantic/resource
    // ledger 不存在 → 各自类型化 NOT_FOUND(域路由不串)。
    for action in [
        ControlAction::AckSemanticRecoveryAlert {
            plan_id_hex: fixture.plan_id_hex().to_owned(),
            expected_total_failures: 1,
            reason: "wrong ledger probe".to_owned(),
        },
        ControlAction::ResumeResourceRecovery {
            plan_id_hex: fixture.plan_id_hex().to_owned(),
            expected_total_failures: 1,
            reason: "wrong ledger probe".to_owned(),
        },
    ] {
        let receipt = submit(&fixture, &key_file, action).await.expect("dispatch");
        let OutcomeDto::Failure { code, .. } = &receipt.outcome else {
            panic!("expected failure outcome, got {:?}", receipt.outcome);
        };
        assert!(code.contains("NOT_FOUND"), "code was {code}");
    }

    let _ = std::fs::remove_file(&key_file);
}
