//! F3-c2 Windows named pipe 传输面集成证据(dev-fixture):
//! (1) GUI 后端命令层经 ADR-0011 认证入口(challenge-response over named
//!     pipe)完成真实 dispatch,InspectHealth 回执携带夹具权威事实;
//! (2) 认证入口与 plain 入口同命令 receipt 字节一致(B-TASK-006L 契约的
//!     Windows named pipe 侧活体复验;Unix 侧由 authenticated_*_side.rs
//!     对真实 CLI 二进制钉死,CLI 二进制本身仍 Unix-only);
//! (3) 写路径(ack-recovery-alert)真实穿越 named pipe 认证 submit 信封,
//!     TaskAuthority CAS mutation 生效;
//! (4) 不存在端点的类型化 Handshake 错误码(named pipe busy/NotFound
//!     重试耗尽 connect 预算后 fail-closed)。

#![cfg(all(windows, feature = "dev-fixture"))]

use llmos_desktop_lib::devfixture::DevFixture;
use llmos_desktop_lib::dto::OutcomeDto;
use llmos_desktop_lib::error::ErrorCode;
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

#[tokio::test]
async fn named_pipe_authenticated_inspect_health_round_trips() {
    let mut fixture = DevFixture::spawn("n1").expect("fixture");
    let key_file = write_key_file(&fixture, "n1");
    let _loops = fixture.serve_forever();

    let receipt = dispatch_control(
        fixture.endpoint_authenticated().as_str(),
        fixture.principal_hex(),
        &key_file,
        ControlCommand::InspectHealth,
    )
    .await
    .expect("authenticated dispatch over named pipe");
    let OutcomeDto::Inspected {
        worker_state,
        durable_escalated,
        durable_unacknowledged_escalated,
        alerts,
        ..
    } = &receipt.outcome
    else {
        panic!("expected inspection outcome, got {:?}", receipt.outcome);
    };
    assert_eq!(worker_state, "BackingOff");
    assert_eq!(*durable_escalated, 1);
    assert_eq!(*durable_unacknowledged_escalated, 1);
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].plan_id_hex, fixture.plan_id_hex());
    assert!(!receipt.receipt_hex.is_empty());

    let _ = std::fs::remove_file(&key_file);
}

#[tokio::test]
async fn named_pipe_authenticated_matches_plain_entry_bytes() {
    let mut fixture = DevFixture::spawn("n2").expect("fixture");
    let key_file = write_key_file(&fixture, "n2");
    let _loops = fixture.serve_forever();

    for command in [
        ControlCommand::InspectHealth,
        ControlCommand::ExportResourceMetrics,
        ControlCommand::InspectResource {
            reservation_id: [0x77; 16],
        },
    ] {
        let gui = dispatch_control(
            fixture.endpoint_authenticated().as_str(),
            fixture.principal_hex(),
            &key_file,
            command.clone(),
        )
        .await
        .expect("authenticated dispatch");
        let plain = fixture
            .dispatch_plain(&command, None, None, None)
            .await
            .expect("plain dispatch over named pipe");
        assert_eq!(
            gui.receipt_hex,
            receipt_to_hex(&plain),
            "authenticated vs plain receipt bytes diverged for {command:?}"
        );
    }

    let _ = std::fs::remove_file(&key_file);
}

#[tokio::test]
async fn named_pipe_write_mutation_reaches_the_authority() {
    let mut fixture = DevFixture::spawn("n3").expect("fixture");
    let key_file = write_key_file(&fixture, "n3");
    let _loops = fixture.serve_forever();

    // 与 Tauri submit_control 同一构造点(测试注入确定性命令 id)。
    let command = build_control_command(
        ControlAction::AckRecoveryAlert {
            plan_id_hex: fixture.plan_id_hex().to_owned(),
            expected_total_failures: 1,
            reason: "F3-c2 named pipe write probe".to_owned(),
        },
        [0xE1; 16],
    )
    .expect("build ack command");
    let receipt = dispatch_control(
        fixture.endpoint_authenticated().as_str(),
        fixture.principal_hex(),
        &key_file,
        command,
    )
    .await
    .expect("authenticated write dispatch over named pipe");
    assert_eq!(receipt.control_command_id_hex, "e1".repeat(16));
    let OutcomeDto::Acknowledged { receipt_id_hex } = &receipt.outcome else {
        panic!("expected acknowledged outcome, got {:?}", receipt.outcome);
    };
    assert!(!receipt_id_hex.is_empty());

    // 真实生效观察:durable_unacknowledged_escalated 归零。
    let inspected = dispatch_control(
        fixture.endpoint_authenticated().as_str(),
        fixture.principal_hex(),
        &key_file,
        ControlCommand::InspectHealth,
    )
    .await
    .expect("inspect after ack");
    let OutcomeDto::Inspected {
        durable_unacknowledged_escalated,
        ..
    } = &inspected.outcome
    else {
        panic!("expected inspection outcome, got {:?}", inspected.outcome);
    };
    assert_eq!(*durable_unacknowledged_escalated, 0);

    let _ = std::fs::remove_file(&key_file);
}

#[tokio::test]
async fn missing_pipe_endpoint_maps_to_typed_handshake_error() {
    let mut fixture = DevFixture::spawn("n4").expect("fixture");
    let key_file = write_key_file(&fixture, "n4");
    let _loops = fixture.serve_forever();

    // 不存在的管道名:nlos-ipc windows connect 以 busy/NotFound 重试耗尽
    // connect 预算后 fail-closed(连接失败属于握手传输面 → Handshake 码)。
    let missing = format!(r"\\.\pipe\llmosdt-missing-{}", std::process::id());
    let error = dispatch_control(
        missing.as_str(),
        fixture.principal_hex(),
        &key_file,
        ControlCommand::InspectHealth,
    )
    .await
    .expect_err("dispatch must fail");
    assert_eq!(error.code, ErrorCode::Handshake);

    let _ = std::fs::remove_file(&key_file);
}
