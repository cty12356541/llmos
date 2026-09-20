//! W32-E 资源监控(Resource Monitor 最小版)集成证据:OpenMetrics 消费面。
//! (1) 三条既有只读导出命令(ExportMetrics / ExportSemanticMetrics /
//!     ExportResourceMetrics)经 ADR-0011 认证入口真实派发,回执携带
//!     OpenMetrics 文本,artifact/semantic/resource 三恢复域目录逐族在场,
//!     且取值与夹具权威健康事实一致(不发明指标);
//! (2) 同一批命令经 plain 入口(CLI 同路)派发,receipt 字节一致——GUI
//!     监控消费的回执面与 CLI parity 契约同源。无任何新控制路径。

#![cfg(all(unix, feature = "dev-fixture"))]

use llmos_desktop_lib::devfixture::DevFixture;
use llmos_desktop_lib::dto::OutcomeDto;
use llmos_desktop_lib::ipc::dispatch_control;
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

async fn export_metrics_text(
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
    .expect("metrics export dispatch")
}

#[tokio::test]
async fn metrics_exports_carry_three_recovery_domains_over_authenticated_entry() {
    let mut fixture = DevFixture::spawn("m1").expect("fixture");
    let key_file = write_key_file(&fixture, "m1");
    let _loops = fixture.serve_forever();

    let artifact = export_metrics_text(&fixture, &key_file, ControlCommand::ExportMetrics).await;
    let OutcomeDto::MetricsExported {
        openmetrics_text: artifact_text,
    } = &artifact.outcome
    else {
        panic!(
            "expected artifact metrics export, got {:?}",
            artifact.outcome
        );
    };
    // 取值与夹具权威健康事实一致(RecoveryWorkerHealth 直读),非发明。
    assert!(artifact_text.contains("# TYPE nlos_artifact_recovery_worker_state gauge"));
    assert!(artifact_text.contains("nlos_artifact_recovery_worker_state{state=\"backing_off\"} 1"));
    assert!(artifact_text.contains("nlos_artifact_recovery_cycles_total 4"));
    assert!(artifact_text.contains("nlos_artifact_recovery_plans_inspected_total 3"));
    assert!(artifact_text.contains("nlos_artifact_recovery_plans_finalized_total 2"));
    assert!(artifact_text.contains("nlos_artifact_recovery_durable_escalated 1"));

    let semantic =
        export_metrics_text(&fixture, &key_file, ControlCommand::ExportSemanticMetrics).await;
    let OutcomeDto::MetricsExported {
        openmetrics_text: semantic_text,
    } = &semantic.outcome
    else {
        panic!(
            "expected semantic metrics export, got {:?}",
            semantic.outcome
        );
    };
    assert!(semantic_text.contains("nlos_semantic_recovery_plans_inspected_total 0"));
    assert!(semantic_text.contains("nlos_semantic_recovery_plans_finalized_total 0"));
    assert!(semantic_text.contains("nlos_semantic_recovery_durable_escalated 0"));
    assert!(semantic_text.contains("nlos_semantic_recovery_domain_faulted 0"));

    let resource =
        export_metrics_text(&fixture, &key_file, ControlCommand::ExportResourceMetrics).await;
    let OutcomeDto::MetricsExported {
        openmetrics_text: resource_text,
    } = &resource.outcome
    else {
        panic!(
            "expected resource metrics export, got {:?}",
            resource.outcome
        );
    };
    assert!(resource_text.contains("nlos_resource_recovery_plans_inspected_total 0"));
    assert!(resource_text.contains("nlos_resource_recovery_plans_finalized_total 0"));
    assert!(resource_text.contains("nlos_resource_recovery_durable_escalated 0"));
    assert!(resource_text.contains("nlos_resource_recovery_domain_faulted 0"));

    let _ = std::fs::remove_file(&key_file);
}

#[tokio::test]
async fn metrics_export_receipts_match_plain_entry_bytes() {
    let mut fixture = DevFixture::spawn("m2").expect("fixture");
    let key_file = write_key_file(&fixture, "m2");
    let _loops = fixture.serve_forever();

    for command in [
        ControlCommand::ExportMetrics,
        ControlCommand::ExportSemanticMetrics,
        ControlCommand::ExportResourceMetrics,
    ] {
        let gui = export_metrics_text(&fixture, &key_file, command.clone()).await;
        let plain: ControlReceipt = nlos_system_control::control::dispatch_over_socket(
            fixture.socket_plain(),
            &command,
            None,
            None,
        )
        .await
        .expect("plain dispatch");
        assert_eq!(
            gui.receipt_hex,
            receipt_to_hex(&plain),
            "authenticated vs plain receipt bytes diverged for {command:?}"
        );
    }

    let _ = std::fs::remove_file(&key_file);
}
