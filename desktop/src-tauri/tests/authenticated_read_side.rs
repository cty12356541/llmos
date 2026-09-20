//! W32-A 只读半集成证据:GUI 后端命令层经 ADR-0011 认证入口完成真实
//! dispatch,并验证(1) InspectHealth 回执形态;(2) 认证入口与 plain
//! 入口同命令 receipt 字节一致(B-TASK-006L 契约的桌面侧活体复验);
//! (3) 未接线 inspector 的类型化 NotFound 失败面;(4) 不存在端点的
//! 类型化 Handshake 错误码。

#![cfg(all(unix, feature = "dev-fixture"))]

use llmos_desktop_lib::devfixture::DevFixture;
use llmos_desktop_lib::dto::OutcomeDto;
use llmos_desktop_lib::error::ErrorCode;
use llmos_desktop_lib::ipc::dispatch_read;
use nlos_system_control::control::{ControlCommand, ControlReceipt, receipt_to_hex};

fn write_key_file(fixture: &DevFixture, label: &str) -> String {
    let path = std::env::temp_dir().join(format!(
        "llmosdt-test-key-{label}-{}.key",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    fixture.write_key_file(&path).expect("write key file");
    path.display().to_string()
}

#[tokio::test]
async fn authenticated_inspect_health_matches_plain_entry_bytes() {
    let mut fixture = DevFixture::spawn("t1").expect("fixture");
    let key_file = write_key_file(&fixture, "t1");
    let _loops = fixture.serve_forever();

    // 1) GUI 后端:认证入口 dispatch。
    let gui = dispatch_read(
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
    .expect("authenticated dispatch");
    assert_eq!(gui.control_command_id_hex, "c0".repeat(16));
    let OutcomeDto::Inspected {
        worker_state,
        durable_escalated,
        durable_unacknowledged_escalated,
        alerts,
        ..
    } = &gui.outcome
    else {
        panic!("expected inspection outcome, got {:?}", gui.outcome);
    };
    assert_eq!(worker_state, "BackingOff");
    assert_eq!(*durable_escalated, 1);
    assert_eq!(*durable_unacknowledged_escalated, 1);
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].plan_id_hex, fixture.plan_id_hex());
    assert!(!gui.receipt_hex.is_empty());

    // 2) 同命令经 plain 入口(dispatch_over_socket,与 CLI 同一路径):
    //    receipt 字节一致。
    let plain: ControlReceipt = nlos_system_control::control::dispatch_over_socket(
        fixture.socket_plain(),
        &ControlCommand::InspectHealth,
        None,
        None,
    )
    .await
    .expect("plain dispatch");
    assert_eq!(gui.receipt_hex, receipt_to_hex(&plain));

    let _ = std::fs::remove_file(&key_file);
}

#[tokio::test]
async fn inspect_task_by_plan_id_round_trips_the_escalated_alert() {
    let mut fixture = DevFixture::spawn("t2").expect("fixture");
    let key_file = write_key_file(&fixture, "t2");
    let _loops = fixture.serve_forever();

    let plan_id =
        nlos_system_control::control::parse_hex_id(fixture.plan_id_hex()).expect("fixture plan id");
    let receipt = dispatch_read(
        fixture
            .socket_authenticated()
            .display()
            .to_string()
            .as_str(),
        fixture.principal_hex(),
        &key_file,
        ControlCommand::InspectTask { plan_id },
    )
    .await
    .expect("task dispatch");
    let OutcomeDto::Inspected { alerts, .. } = &receipt.outcome else {
        panic!("expected inspection outcome, got {:?}", receipt.outcome);
    };
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].total_failures, 1);

    let _ = std::fs::remove_file(&key_file);
}

#[tokio::test]
async fn unwired_inspectors_surface_typed_not_found_failures() {
    let mut fixture = DevFixture::spawn("t3").expect("fixture");
    let key_file = write_key_file(&fixture, "t3");
    let _loops = fixture.serve_forever();

    let receipt = dispatch_read(
        fixture
            .socket_authenticated()
            .display()
            .to_string()
            .as_str(),
        fixture.principal_hex(),
        &key_file,
        ControlCommand::InspectProcess {
            process_id: [0x77; 16],
        },
    )
    .await
    .expect("process dispatch itself succeeds");
    // 回执成功穿越认证 GET 信封;失败是 receipt 内的类型化 SabiFailure
    // (客户端 inspector 未接线),而不是命令错误。
    let OutcomeDto::Failure {
        code, safe_message, ..
    } = &receipt.outcome
    else {
        panic!("expected failure outcome, got {:?}", receipt.outcome);
    };
    assert!(code.contains("NOT_FOUND"), "code was {code}");
    assert!(safe_message.contains("not wired"));

    let _ = std::fs::remove_file(&key_file);
}

#[tokio::test]
async fn missing_endpoint_maps_to_typed_handshake_error() {
    let mut fixture = DevFixture::spawn("t4").expect("fixture");
    let key_file = write_key_file(&fixture, "t4");
    let _loops = fixture.serve_forever();

    let missing = std::env::temp_dir().join("llmosdt-missing-endpoint.sock");
    let error = dispatch_read(
        missing.display().to_string().as_str(),
        fixture.principal_hex(),
        &key_file,
        ControlCommand::InspectHealth,
    )
    .await
    .expect_err("dispatch must fail");
    assert_eq!(error.code, ErrorCode::Handshake);

    let _ = std::fs::remove_file(&key_file);
}

/// GUI `parity_check` 命令的全链探针:GUI(认证入口)receipt hex 与真实
/// `system-control-cli` 二进制(plain 入口)stdout 的 `RECEIPT <hex>` 逐字节
/// 相等。`#[ignore]`:需要预先 `cargo build -p nlos-system-control` 并以
/// `LLMOS_SYSTEM_CONTROL_CLI=<二进制路径>` 提供路径(本机验证方式见
/// b-gui-001 证据;W32-C 将把它钉进仓库级测试)。
#[tokio::test]
#[ignore = "set LLMOS_SYSTEM_CONTROL_CLI to the prebuilt system-control-cli binary"]
async fn cli_binary_receipt_matches_authenticated_entry() {
    let cli = std::env::var("LLMOS_SYSTEM_CONTROL_CLI").expect("LLMOS_SYSTEM_CONTROL_CLI");
    let mut fixture = DevFixture::spawn("t5").expect("fixture");
    let key_file = write_key_file(&fixture, "t5");
    let _loops = fixture.serve_forever();

    let gui = dispatch_read(
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
    .expect("authenticated dispatch");

    // spawn_blocking:current_thread 测试运行时不能被同步 output() 阻塞,
    // 否则 plain accept 循环停摆、CLI connect 超时。
    let cli_socket = fixture.socket_plain().to_path_buf();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(&cli)
            .arg(&cli_socket)
            .arg("inspect-health")
            .output()
    })
    .await
    .expect("join blocking")
    .expect("run cli");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let cli_hex = stdout
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("RECEIPT "))
        .unwrap_or_default()
        .to_owned();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(gui.receipt_hex, cli_hex);

    let _ = std::fs::remove_file(&key_file);
}
