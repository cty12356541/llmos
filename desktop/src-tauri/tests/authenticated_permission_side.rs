//! W32-D 可信权限 UI 最小版集成证据(授权/预算/成本可见):
//! (1) `InspectResource` 经认证入口 + 真实 `ResourceAuthorityInspector`
//!     组装的有界成本事实与资源权威的 `inspect_cost_receipt` 投影逐字段
//!     一致(「与 authority 一致」的活体断言);
//! (2) 一致性自检核心:同一结清预留两次独立认证派发,渲染事实与直接
//!     复检逐字段一致且 receipt hex 相等(结清事实不可变);
//! (3) 未接线形态(inspector=None,与 CLI 同形)回执为类型化 NOT_FOUND,
//!     且与 plain 入口(CLI 同路)receipt 字节一致——预算/成本视图在未
//!     配置 resource_root 时不伪造任何数据。

#![cfg(all(unix, feature = "dev-fixture"))]

use llmos_desktop_lib::devfixture::DevFixture;
use llmos_desktop_lib::dto::{OutcomeDto, fact_check_dto};
use llmos_desktop_lib::ipc::{dispatch_control, dispatch_cost_inspect};
use nlos_resource::ResourceAuthority;
use nlos_system_control::control::{ControlCommand, ControlReceipt, parse_hex_id, receipt_to_hex};
use nlos_types::ReservationId;

fn write_key_file(fixture: &DevFixture, label: &str) -> String {
    let path = std::env::temp_dir().join(format!(
        "llmosdt-test-key-{label}-{}.key",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    fixture.write_key_file(&path).expect("write key file");
    path.display().to_string()
}

async fn cost_inspect(
    fixture: &DevFixture,
    key_file: &str,
    authority: Option<&ResourceAuthority>,
    reservation_id: [u8; 16],
) -> llmos_desktop_lib::dto::ReceiptDto {
    dispatch_cost_inspect(
        fixture
            .socket_authenticated()
            .display()
            .to_string()
            .as_str(),
        fixture.principal_hex(),
        key_file,
        authority,
        reservation_id,
    )
    .await
    .expect("cost inspect dispatch")
}

#[tokio::test]
async fn resource_cost_inspect_matches_authority_facts() {
    let mut fixture = DevFixture::spawn("p1").expect("fixture");
    let key_file = write_key_file(&fixture, "p1");
    let _loops = fixture.serve_forever();
    let facts = fixture.reservation_facts();
    let reservation_id = parse_hex_id(&facts.reservation_id_hex).expect("reservation id");
    let authority = ResourceAuthority::open(fixture.resource_root()).expect("reopen authority");

    let receipt = cost_inspect(&fixture, &key_file, Some(&authority), reservation_id).await;
    let OutcomeDto::ResourceInspected {
        reservation_id_hex,
        account_id_hex,
        upper_bound,
        usage_high_water,
        consumption_count,
    } = &receipt.outcome
    else {
        panic!("expected resource inspection, got {:?}", receipt.outcome);
    };
    assert_eq!(*reservation_id_hex, facts.reservation_id_hex);
    assert_eq!(*account_id_hex, facts.account_id_hex);
    assert_eq!(*upper_bound, facts.upper_bound);
    assert_eq!(*usage_high_water, facts.usage_high_water);
    assert_eq!(*consumption_count, facts.consumption_count);

    // 同一组事实再对资源权威的直接读数复核(inspect_cost_receipt 投影)。
    let cost = authority
        .inspect_cost_receipt(ReservationId::from_bytes(reservation_id))
        .expect("direct authority read");
    assert_eq!(*upper_bound, cost.upper_bound);
    assert_eq!(*usage_high_water, cost.finalization.high_water);
    assert_eq!(
        cost.consumptions.len() as u64,
        u64::from(*consumption_count),
        "GUI 回执的消费回执数必须等于权威消费回执表行数"
    );

    let _ = std::fs::remove_file(&key_file);
}

#[tokio::test]
async fn cost_fact_check_matches_across_two_dispatches() {
    let mut fixture = DevFixture::spawn("p2").expect("fixture");
    let key_file = write_key_file(&fixture, "p2");
    let _loops = fixture.serve_forever();
    let facts = fixture.reservation_facts();
    let reservation_id = parse_hex_id(&facts.reservation_id_hex).expect("reservation id");
    let authority = ResourceAuthority::open(fixture.resource_root()).expect("reopen authority");

    let first = cost_inspect(&fixture, &key_file, Some(&authority), reservation_id).await;
    let second = cost_inspect(&fixture, &key_file, Some(&authority), reservation_id).await;
    let check = fact_check_dto(&facts.reservation_id_hex, &first, &second);
    assert!(check.receipt_hex_matched, "receipt hex must be identical");
    assert!(
        check.rows.iter().all(|row| row.matched),
        "rows: {:?}",
        check.rows
    );
    assert!(check.matched);

    let _ = std::fs::remove_file(&key_file);
}

#[tokio::test]
async fn unwired_cost_inspect_stays_typed_not_found_and_matches_plain_entry() {
    let mut fixture = DevFixture::spawn("p3").expect("fixture");
    let key_file = write_key_file(&fixture, "p3");
    let _loops = fixture.serve_forever();
    let facts = fixture.reservation_facts();
    let reservation_id = parse_hex_id(&facts.reservation_id_hex).expect("reservation id");

    // 未配置 resource_root 的形态:inspector=None(与 CLI 相同),回执是
    // 诚实的类型化 NOT_FOUND,不是命令错误,更不是伪造的预算数据。
    let receipt = cost_inspect(&fixture, &key_file, None, reservation_id).await;
    let OutcomeDto::Failure {
        code, safe_message, ..
    } = &receipt.outcome
    else {
        panic!("expected unwired failure, got {:?}", receipt.outcome);
    };
    assert!(code.contains("NOT_FOUND"), "code was {code}");
    assert!(safe_message.contains("not wired"));

    // 与 plain 入口(CLI 同路)同命令同未接线形态:receipt 字节一致。
    let plain: ControlReceipt = nlos_system_control::control::dispatch_over_socket(
        fixture.socket_plain(),
        &ControlCommand::InspectResource { reservation_id },
        None,
        None,
    )
    .await
    .expect("plain dispatch");
    assert_eq!(receipt.receipt_hex, receipt_to_hex(&plain));

    // 一致性自检在未接线形态下也如实可比(两侧同形态失败回执)。
    let first = receipt;
    let second = cost_inspect(&fixture, &key_file, None, reservation_id).await;
    let check = fact_check_dto(&facts.reservation_id_hex, &first, &second);
    assert!(
        check.matched,
        "unwired shape must still self-check: {check:?}"
    );

    let _ = std::fs::remove_file(&key_file);
}

/// `dispatch_control`(CLI parity 路径)保持未接线形态:预算/成本接线
/// 不得泄漏进 parity 比对路径。
#[tokio::test]
async fn parity_dispatch_path_never_wires_the_resource_inspector() {
    let mut fixture = DevFixture::spawn("p4").expect("fixture");
    let key_file = write_key_file(&fixture, "p4");
    let _loops = fixture.serve_forever();
    let facts = fixture.reservation_facts();
    let reservation_id = parse_hex_id(&facts.reservation_id_hex).expect("reservation id");

    let receipt = dispatch_control(
        fixture
            .socket_authenticated()
            .display()
            .to_string()
            .as_str(),
        fixture.principal_hex(),
        &key_file,
        ControlCommand::InspectResource { reservation_id },
    )
    .await
    .expect("dispatch");
    assert!(
        matches!(&receipt.outcome, OutcomeDto::Failure { code, .. } if code.contains("NOT_FOUND")),
        "parity path must stay unwired, got {:?}",
        receipt.outcome
    );

    let _ = std::fs::remove_file(&key_file);
}
