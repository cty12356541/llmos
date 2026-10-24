//! W39-D / §28.4 Stage D 最小交付:Task Space 五层 inspect 的 desktop
//! 命令层接线(W33-F.3-③)。
//!
//! 既有 SABI v1.5 ControlCommand 面(InspectTaskGroup / TaskNode /
//! ExecutionFiber / Topic / Operation)与 DTO 投影已在;本测试钉死
//! desktop 侧命令构造器 + 认证入口派发 + plain 入口 receipt 字节一致。
//! 夹具未接 layer inspector 时回执为类型化 NOT_FOUND(「layer inspection
//! backend is not wired」)——与 InspectProcess 未接线先例同形,诚实失败
//! 面,不发明成功事实。

#![cfg(all(unix, feature = "dev-fixture"))]

use llmos_desktop_lib::devfixture::DevFixture;
use llmos_desktop_lib::dto::OutcomeDto;
use llmos_desktop_lib::ipc::{
    dispatch_control, layer_inspect_execution_fiber, layer_inspect_operation,
    layer_inspect_task_group, layer_inspect_task_node, layer_inspect_topic,
};
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

fn hex32(byte: u8) -> String {
    format!("{byte:02x}").repeat(16)
}

#[test]
fn layer_inspect_builders_compile_five_sabi_commands() {
    let group = layer_inspect_task_group(&hex32(0x91)).expect("group");
    assert_eq!(
        group,
        ControlCommand::InspectTaskGroup {
            group_id: [0x91; 16]
        }
    );

    let node = layer_inspect_task_node(&hex32(0xa1), &hex32(0xa2)).expect("node");
    assert_eq!(
        node,
        ControlCommand::InspectTaskNode {
            plan_id: [0xa1; 16],
            node_id: [0xa2; 16],
        }
    );

    let fiber = layer_inspect_execution_fiber(&hex32(0xb1), 7).expect("fiber");
    assert_eq!(
        fiber,
        ControlCommand::InspectExecutionFiber {
            fiber_id: [0xb1; 16],
            generation: 7,
        }
    );

    let topic = layer_inspect_topic(&hex32(0xc1)).expect("topic");
    assert_eq!(
        topic,
        ControlCommand::InspectTopic {
            topic_id: [0xc1; 16]
        }
    );

    let operation = layer_inspect_operation(&hex32(0xd1), 3).expect("operation");
    assert_eq!(
        operation,
        ControlCommand::InspectOperation {
            operation_id: [0xd1; 16],
            generation: 3,
        }
    );
}

#[test]
fn layer_inspect_builders_reject_zero_generation() {
    let err = layer_inspect_execution_fiber(&hex32(0xb1), 0).expect_err("gen 0");
    assert!(
        err.message.to_lowercase().contains("generation")
            || format!("{err:?}").to_lowercase().contains("generation"),
        "expected generation reject, got {err:?}"
    );
    let err = layer_inspect_operation(&hex32(0xd1), 0).expect_err("gen 0");
    assert!(
        err.message.to_lowercase().contains("generation")
            || format!("{err:?}").to_lowercase().contains("generation"),
        "expected generation reject, got {err:?}"
    );
}

async fn dispatch_auth(
    fixture: &DevFixture,
    key_file: &str,
    command: ControlCommand,
) -> llmos_desktop_lib::dto::ReceiptDto {
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
    .expect("authenticated layer inspect dispatch")
}

fn assert_typed_layer_read_failure(outcome: &OutcomeDto) {
    let OutcomeDto::Failure {
        code, safe_message, ..
    } = outcome
    else {
        panic!("expected typed failure outcome, got {outcome:?}");
    };
    assert!(
        code.contains("NOT_FOUND"),
        "expected NOT_FOUND code, got {code}"
    );
    // TaskGroup 走 TaskAuthority 直读(对象不存在 → recovery authority object
    // was not found);其余四层经 pluggable seam(未接线 → layer inspection
    // backend is not wired)。两种都是诚实类型化失败面,不发明成功事实。
    assert!(
        safe_message.contains("layer inspection backend is not wired")
            || safe_message.contains("requested recovery authority object was not found"),
        "expected typed layer/group miss message, got {safe_message}"
    );
}

#[tokio::test]
async fn five_layer_inspect_commands_surface_typed_unwired_failures_over_authenticated_entry() {
    let mut fixture = DevFixture::spawn("l1").expect("fixture");
    let key_file = write_key_file(&fixture, "l1");
    let _loops = fixture.serve_forever();

    let commands = [
        layer_inspect_task_group(&hex32(0x91)).expect("group"),
        layer_inspect_task_node(&hex32(0xa1), &hex32(0xa2)).expect("node"),
        layer_inspect_execution_fiber(&hex32(0xb1), 1).expect("fiber"),
        layer_inspect_topic(&hex32(0xc1)).expect("topic"),
        layer_inspect_operation(&hex32(0xd1), 1).expect("operation"),
    ];
    for command in commands {
        let receipt = dispatch_auth(&fixture, &key_file, command).await;
        assert_typed_layer_read_failure(&receipt.outcome);
        assert!(!receipt.receipt_hex.is_empty());
    }

    let _ = std::fs::remove_file(&key_file);
}

#[tokio::test]
async fn five_layer_inspect_receipts_match_plain_entry_bytes() {
    let mut fixture = DevFixture::spawn("l2").expect("fixture");
    let key_file = write_key_file(&fixture, "l2");
    let _loops = fixture.serve_forever();

    let commands = [
        layer_inspect_task_group(&hex32(0x91)).expect("group"),
        layer_inspect_task_node(&hex32(0xa1), &hex32(0xa2)).expect("node"),
        layer_inspect_execution_fiber(&hex32(0xb1), 1).expect("fiber"),
        layer_inspect_topic(&hex32(0xc1)).expect("topic"),
        layer_inspect_operation(&hex32(0xd1), 1).expect("operation"),
    ];
    for command in commands {
        let gui = dispatch_auth(&fixture, &key_file, command.clone()).await;
        let plain: ControlReceipt = nlos_system_control::control::dispatch_over_socket(
            fixture.socket_plain(),
            &command,
            None,
            None,
            None,
        )
        .await
        .expect("plain dispatch");
        assert_eq!(gui.receipt_hex, receipt_to_hex(&plain));
    }

    let _ = std::fs::remove_file(&key_file);
}
