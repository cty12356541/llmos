//! B-APPLICATION-001 authority tests: the durable Application/Installation
//! authority — normal install, reinstall/idempotent replay, restart replay,
//! authority-first refusals (unverified receipt reference, installation
//! preceding verification, idempotency conflict, disabled application),
//! current-digest tracking, and the DDL trigger guards.

mod support;

use std::sync::atomic::{AtomicU64, Ordering};

use nlos_application::{
    ApplicationAuthorityError, DisableApplicationRequest, InstallApplicationRequest,
    RollbackApplicationRequest, UninstallApplicationRequest, UpdateApplicationRequest,
    derive_application_id, derive_installation_id,
};
use nlos_types::{Generation, IdempotencyKey, ReceiptId};
use rusqlite::Connection;
use support::{
    TestStack, authority_database, disable_replayed, disabled, installed, open_authority, replayed,
    rollback_replayed, rolled_back, uninstall_replayed, uninstalled, update_replayed, updated,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn label(name: &str) -> String {
    format!(
        "authority-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn raw_count(database: &std::path::Path, sql: &str) -> i64 {
    let connection = Connection::open(database).expect("open raw reader");
    connection
        .query_row(sql, [], |row| row.get(0))
        .expect("count rows")
}

fn assert_counts(stack: &TestStack, applications: i64, receipts: i64) {
    let database = authority_database(stack.root.root());
    assert_eq!(
        raw_count(&database, "SELECT COUNT(*) FROM applications"),
        applications,
        "unexpected applications row count"
    );
    assert_eq!(
        raw_count(&database, "SELECT COUNT(*) FROM installation_receipts"),
        receipts,
        "unexpected installation_receipts row count"
    );
}

fn assert_disable_counts(stack: &TestStack, disable_receipts: i64) {
    let database = authority_database(stack.root.root());
    assert_eq!(
        raw_count(
            &database,
            "SELECT COUNT(*) FROM application_disable_receipts"
        ),
        disable_receipts,
        "unexpected application_disable_receipts row count"
    );
}

fn assert_uninstall_counts(stack: &TestStack, uninstall_receipts: i64) {
    let database = authority_database(stack.root.root());
    assert_eq!(
        raw_count(
            &database,
            "SELECT COUNT(*) FROM application_uninstall_receipts"
        ),
        uninstall_receipts,
        "unexpected application_uninstall_receipts row count"
    );
}

fn assert_rollback_counts(stack: &TestStack, rollback_receipts: i64) {
    let database = authority_database(stack.root.root());
    assert_eq!(
        raw_count(
            &database,
            "SELECT COUNT(*) FROM application_rollback_receipts"
        ),
        rollback_receipts,
        "unexpected application_rollback_receipts row count"
    );
}

/// 正常安装：verified receipt → application singleton（gen 1, installed）+
/// immutable installation receipt；authority 派生 Id 与 API 返回一致；
/// inspect/list 只读回读逐字段一致。
#[test]
fn install_fresh_package_creates_application_generation_one() {
    let stack = TestStack::new(&label("fresh"), 0x21);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());

    let receipt = installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );
    assert_eq!(receipt.package_verification_receipt_id, verified.receipt_id);
    assert_eq!(receipt.package_id, verified.package_id);
    assert_eq!(receipt.package_manifest_digest, verified.manifest_digest);
    assert_eq!(receipt.package_version, verified.package_version);
    assert_eq!(receipt.entry_count, verified.entry_count);
    assert_eq!(receipt.installer_principal, verified.signer);
    assert_eq!(receipt.installed_at_ms, 2_000);
    assert_eq!(receipt.installation_generation, Generation::INITIAL);

    // Authority-derived identities match the derivation functions.
    let application_id = derive_application_id(verified.package_id);
    assert_eq!(receipt.application_id, application_id);
    assert_eq!(
        receipt.installation_id,
        derive_installation_id(key(0x01), application_id, Generation::INITIAL)
    );

    let view = authority
        .inspect_application(verified.package_id)
        .expect("inspect application")
        .expect("application exists after install");
    assert_eq!(view.application_id, application_id);
    assert_eq!(view.package_id, verified.package_id);
    assert_eq!(view.package_manifest_digest, verified.manifest_digest);
    assert_eq!(view.current_installation_generation, Generation::INITIAL);
    assert_eq!(view.status, nlos_application::ApplicationStatus::Installed);
    assert_eq!(view.created_at_ms, 2_000);
    assert_eq!(view.updated_at_ms, 2_000);

    let read_back = authority
        .inspect_installation(receipt.installation_id)
        .expect("inspect installation");
    assert_eq!(read_back, receipt);

    let installations = authority
        .list_installations(application_id)
        .expect("list installations");
    assert_eq!(installations, vec![receipt.clone()]);

    // Unknown reads are legitimate empty outcomes, not errors.
    assert!(
        authority
            .inspect_application(nlos_types::PackageId::from_bytes([0xEE; 16]))
            .expect("inspect unknown")
            .is_none()
    );
    assert!(
        authority
            .list_installations(nlos_types::ApplicationId::from_bytes([0xEE; 16]))
            .expect("list unknown")
            .is_empty()
    );
    assert!(matches!(
        authority.inspect_installation(nlos_types::InstallationId::from_bytes([0xEE; 16])),
        Err(ApplicationAuthorityError::InstallationNotFound(_))
    ));
}

/// 重装（fresh key）推进一代并落第二条 immutable receipt；同 key 重放返回
/// 原 receipt 不双跳（generation 与 receipt 计数均不变）；同 key 不同请求
/// 形状为 typed `IdempotencyConflict`。
#[test]
fn reinstall_advances_generation_and_replays_idempotently() {
    let stack = TestStack::new(&label("reinstall"), 0x22);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());

    let first = installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );
    let second = installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x02,
        3_000,
    );
    assert_eq!(
        second.installation_generation.get(),
        2,
        "a fresh key advances exactly one generation"
    );
    assert_eq!(
        second.installation_id,
        derive_installation_id(
            key(0x02),
            first.application_id,
            second.installation_generation
        )
    );
    assert_eq!(second.application_id, first.application_id);
    assert_eq!(
        authority
            .list_installations(first.application_id)
            .expect("list"),
        vec![first.clone(), second.clone()]
    );
    assert_eq!(
        authority
            .inspect_application(verified.package_id)
            .expect("inspect")
            .expect("exists")
            .current_installation_generation
            .get(),
        2
    );

    // Same-key replay returns the original receipt without a double-jump.
    let replay = replayed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x02,
        3_000,
    );
    assert_eq!(replay, second);
    assert_eq!(
        authority
            .inspect_application(verified.package_id)
            .expect("inspect")
            .expect("exists")
            .current_installation_generation
            .get(),
        2,
        "replay never advances the generation"
    );
    assert_counts(&stack, 1, 2);

    // Same key, different request shape: typed conflict, zero state change.
    // A different timestamp under the same key:
    let conflict = authority.install_application(
        &stack.artifacts,
        InstallApplicationRequest {
            package_verification_receipt_id: verified.receipt_id,
            idempotency_key: key(0x02),
            installed_at_ms: 9_000,
        },
    );
    assert!(
        matches!(
            conflict,
            Err(ApplicationAuthorityError::IdempotencyConflict)
        ),
        "same key with a different timestamp must conflict"
    );
    // A different verification receipt under the same key (a second verify
    // command yields its own receipt id):
    let second_receipt = stack.verify_package(0x41, 1, key(0xF1), 2_000);
    assert_ne!(second_receipt.receipt_id, verified.receipt_id);
    let conflict = authority.install_application(
        &stack.artifacts,
        InstallApplicationRequest {
            package_verification_receipt_id: second_receipt.receipt_id,
            idempotency_key: key(0x02),
            installed_at_ms: 3_000,
        },
    );
    assert!(matches!(
        conflict,
        Err(ApplicationAuthorityError::IdempotencyConflict)
    ));
    assert_counts(&stack, 1, 2);
}

/// 重启 replay：全部权威状态 durable；重开后同 key 重放逐字节相等、只读
/// 回读一致、fresh key 从 durable 代际稠密续推。
#[test]
fn replay_survives_restart() {
    let stack = TestStack::new(&label("restart"), 0x23);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let first = {
        let authority = open_authority(stack.root.root());
        installed(
            &authority,
            &stack.artifacts,
            verified.receipt_id,
            0x01,
            2_000,
        )
    };

    // Fresh authority instance over the same durable store (artifact store
    // reopened too — the readback path must work across restarts).
    let reopened_artifacts =
        nlos_artifact::ArtifactStore::open(stack.root.root().join("art")).expect("reopen art");
    let authority = open_authority(stack.root.root());
    let replay = replayed(
        &authority,
        &reopened_artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );
    assert_eq!(replay, first);
    assert_eq!(
        authority
            .inspect_application(verified.package_id)
            .expect("inspect")
            .expect("durable application")
            .current_installation_generation,
        Generation::INITIAL,
        "replays after reopen advance nothing"
    );

    let next = installed(
        &authority,
        &reopened_artifacts,
        verified.receipt_id,
        0x02,
        3_000,
    );
    assert_eq!(next.installation_generation.get(), 2);
    assert_counts(&stack, 1, 2);
}

/// 未验证包拒绝（authority-first FINALIZED 门）：引用 artifact authority
/// 不存在的 verified receipt → typed 拒绝、零部分状态。
#[test]
fn unverified_receipt_reference_is_refused_with_zero_state() {
    let stack = TestStack::new(&label("unverified"), 0x24);
    let authority = open_authority(stack.root.root());
    let ghost = ReceiptId::from_bytes([0x99; 16]);

    let error = authority
        .install_application(
            &stack.artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: ghost,
                idempotency_key: key(0x01),
                installed_at_ms: 2_000,
            },
        )
        .expect_err("an unverified package reference must be refused");
    assert!(
        matches!(
            error,
            ApplicationAuthorityError::PackageVerificationReceiptNotFound(id) if id == ghost
        ),
        "typed fail-closed, got {error}"
    );
    assert_counts(&stack, 0, 0);
    assert!(
        authority
            .inspect_application(nlos_types::PackageId::from_bytes([0x41; 16]))
            .expect("inspect")
            .is_none(),
        "zero partial state"
    );

    // A real verified package installs fine afterwards: the refusal left no
    // durable scar.
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let receipt = installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );
    assert_eq!(receipt.installation_generation, Generation::INITIAL);
    assert_counts(&stack, 1, 1);
}

/// 安装时间早于验证时间：digest 绑定第 7 式 typed 拒绝、零部分状态。
#[test]
fn installation_preceding_verification_is_refused() {
    let stack = TestStack::new(&label("precedes"), 0x25);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 5_000);
    let authority = open_authority(stack.root.root());

    let error = authority
        .install_application(
            &stack.artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: verified.receipt_id,
                idempotency_key: key(0x01),
                installed_at_ms: 4_999,
            },
        )
        .expect_err("installation must not precede verification");
    assert!(
        matches!(
            error,
            ApplicationAuthorityError::InstallationPrecedesVerification {
                verified_at_ms: 5_000,
                installed_at_ms: 4_999,
            }
        ),
        "typed fail-closed, got {error}"
    );
    assert_counts(&stack, 0, 0);

    // Equal timestamps are legal (the binding is >=).
    installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        5_000,
    );
    assert_counts(&stack, 1, 1);
}

/// disabled 状态机拒绝重装：raw SQL 合法转移 installed→disabled 后，新
/// key 安装为 typed `ApplicationDisabled`，状态与代际纹丝不动。
#[test]
fn disabled_application_refuses_reinstall() {
    let stack = TestStack::new(&label("disabled"), 0x26);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());
    let first = installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );

    // The legal installed→disabled transition (the future policy engine's
    // durable act; generation untouched).
    let raw = Connection::open(authority_database(stack.root.root())).expect("raw connection");
    raw.execute(
        "UPDATE applications SET status=2 WHERE application_id=?1",
        [first.application_id.as_bytes().as_slice()],
    )
    .expect("legal disable transition");
    drop(raw);

    let error = authority
        .install_application(
            &stack.artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: verified.receipt_id,
                idempotency_key: key(0x02),
                installed_at_ms: 3_000,
            },
        )
        .expect_err("a disabled application must refuse new installations");
    assert!(matches!(
        error,
        ApplicationAuthorityError::ApplicationDisabled { application_id }
            if application_id == first.application_id
    ));
    assert_counts(&stack, 1, 1);
    let view = authority
        .inspect_application(verified.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(view.status, nlos_application::ApplicationStatus::Disabled);
    assert_eq!(view.current_installation_generation.get(), 1);
}

/// 当前 manifest digest 跟随最新代际：同 package 新版本 verified receipt
/// 作为下一代安装推进 current digest；历史 receipt 保持各自 digest。
#[test]
fn current_digest_tracks_latest_generation() {
    let stack = TestStack::new(&label("digest"), 0x27);
    let first = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let second = stack.verify_package(0x41, 2, key(0xF1), 2_000);
    assert_ne!(first.manifest_digest, second.manifest_digest);
    let authority = open_authority(stack.root.root());

    let gen1 = installed(&authority, &stack.artifacts, first.receipt_id, 0x01, 2_000);
    assert_eq!(
        authority
            .inspect_application(first.package_id)
            .expect("inspect")
            .expect("exists")
            .package_manifest_digest,
        first.manifest_digest
    );

    let gen2 = installed(&authority, &stack.artifacts, second.receipt_id, 0x02, 3_000);
    assert_eq!(gen2.installation_generation.get(), 2);
    assert_eq!(gen2.package_manifest_digest, second.manifest_digest);
    assert_eq!(gen2.package_version, 2);
    let view = authority
        .inspect_application(first.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(view.package_manifest_digest, second.manifest_digest);
    assert_eq!(view.current_installation_generation.get(), 2);
    assert_eq!(
        authority
            .list_installations(view.application_id)
            .expect("list"),
        vec![gen1, gen2]
    );
}

/// DDL trigger 守卫：receipt 不可变/不可删、application 代际不可减、身份
/// 冻结、application 行不可删、receipt 只能落在当前代际、非法状态转移
/// abort（disabled 终态；未知状态；disable 同时动代际）。
#[test]
#[allow(clippy::too_many_lines)] // One linear tamper sweep over the full guard surface.
fn trigger_guards_abort_raw_tampering() {
    let stack = TestStack::new(&label("guards"), 0x28);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());
    let first = installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );
    installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x02,
        3_000,
    );
    let database = authority_database(stack.root.root());
    let raw = Connection::open(&database).expect("raw connection");

    // Installation receipts are immutable and durable.
    assert!(
        raw.execute("UPDATE installation_receipts SET installed_at_ms=99", [])
            .is_err(),
        "an installation receipt can never be rewritten"
    );
    assert!(
        raw.execute(
            "UPDATE installation_receipts SET package_manifest_digest=x'0000000000000000000000000000000000000000000000000000000000000000'",
            []
        )
        .is_err()
    );
    assert!(
        raw.execute("DELETE FROM installation_receipts", [])
            .is_err(),
        "an installation receipt is durable"
    );

    // The generation is monotonic and the identity is frozen.
    assert!(
        raw.execute(
            "UPDATE applications SET current_installation_generation=1",
            []
        )
        .is_err(),
        "the generation can never decrease"
    );
    assert!(
        raw.execute(
            "UPDATE applications SET application_id=x'CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC'",
            []
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "UPDATE applications SET package_id=x'CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC'",
            []
        )
        .is_err()
    );
    // The application row is durable (physical delete is out of scope).
    assert!(raw.execute("DELETE FROM applications", []).is_err());

    // A receipt can only exist at the application's current generation.
    let future = raw.execute(
        "INSERT INTO installation_receipts (
            installation_id, idempotency_key, application_id,
            installation_generation, package_id, package_manifest_digest,
            package_version, entry_count, package_verification_receipt_id,
            installer_principal, installed_at_ms
         ) VALUES (
            x'BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB',
            x'BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB',
            ?1, 99, ?2, ?3, 1, 1, ?4, ?5, 3
         )",
        rusqlite::params![
            first.application_id.as_bytes().as_slice(),
            verified.package_id.as_bytes().as_slice(),
            verified.manifest_digest.as_bytes().as_slice(),
            verified.receipt_id.as_bytes().as_slice(),
            verified.signer.as_bytes().as_slice(),
        ],
    );
    assert!(
        future.is_err(),
        "a receipt can never record a generation beyond the current one"
    );
    // Same generation but an unknown application is refused by the FK too.
    assert!(
        raw.execute(
            "INSERT INTO installation_receipts (
                installation_id, idempotency_key, application_id,
                installation_generation, package_id, package_manifest_digest,
                package_version, entry_count, package_verification_receipt_id,
                installer_principal, installed_at_ms
             ) VALUES (
                x'BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB',
                x'BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB',
                x'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA', 1, ?1, ?2, 1, 1, ?3, ?4, 3
             )",
            rusqlite::params![
                verified.package_id.as_bytes().as_slice(),
                verified.manifest_digest.as_bytes().as_slice(),
                verified.receipt_id.as_bytes().as_slice(),
                verified.signer.as_bytes().as_slice(),
            ],
        )
        .is_err()
    );

    // Disabled can only transition to uninstalled or rollback-to-installed
    // (with a one-step generation decrease); bare re-enable is illegal.
    raw.execute("UPDATE applications SET status=2", [])
        .expect("legal disable");
    assert!(
        raw.execute("UPDATE applications SET status=1", []).is_err(),
        "re-enabling a disabled application without generation rollback is illegal"
    );
    assert!(
        raw.execute(
            "UPDATE applications SET status=3, current_installation_generation=current_installation_generation+1",
            []
        )
        .is_err(),
        "uninstall must not move the generation"
    );
    raw.execute(
        "UPDATE applications SET status=3, updated_at_ms=updated_at_ms+1",
        [],
    )
    .expect("disabled may transition to uninstalled");
    assert!(
        raw.execute("UPDATE applications SET status=1", []).is_err(),
        "uninstalled is terminal except rollback with generation decrease"
    );
    assert!(
        raw.execute("UPDATE applications SET status=2", []).is_err(),
        "uninstalled cannot return to disabled"
    );
    assert!(
        raw.execute("UPDATE applications SET status=3", []).is_err(),
        "unknown statuses are illegal"
    );
    assert!(
        raw.execute(
            "UPDATE applications SET status=1, current_installation_generation=2",
            []
        )
        .is_err()
    );

    // The guarded authority still serves reads; durable state is untouched.
    let view = authority
        .inspect_application(verified.package_id)
        .expect("inspect after tamper sweep")
        .expect("exists");
    assert_eq!(
        view.status,
        nlos_application::ApplicationStatus::Uninstalled
    );
    assert_eq!(view.current_installation_generation.get(), 2);
    assert_eq!(
        authority
            .inspect_installation(first.installation_id)
            .expect("receipt survives"),
        first
    );
    assert_counts(&stack, 1, 2);
}

/// 幂等重做收敛：同 (key, application, generation) 派生同一 installation
/// id —— 幻影丢失后同 key 重做逐字节落回同一 receipt（见派生函数单测）。
/// 这里验证安装路径上 generation 推进与 receipt 的 co-life 计数恒等。
#[test]
fn generation_and_receipts_stay_in_lockstep() {
    let stack = TestStack::new(&label("lockstep"), 0x29);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());

    for seed in 1..=4_u8 {
        installed(
            &authority,
            &stack.artifacts,
            verified.receipt_id,
            seed,
            1_000 + u64::from(seed),
        );
        assert_counts(&stack, 1, i64::from(seed));
        assert_eq!(
            authority
                .inspect_application(verified.package_id)
                .expect("inspect")
                .expect("exists")
                .current_installation_generation
                .get(),
            u64::from(seed),
            "generation == receipt count at every step"
        );
    }

    // A distinct verification command for the same package under a fresh
    // key yields its own verified receipt (receipt ids derive from the
    // verification idempotency key); a fresh install key over it is a new
    // installation command and advances one more generation.
    let again = stack.verify_package(0x41, 1, key(0xF2), 2_000);
    assert_ne!(again.receipt_id, verified.receipt_id);
    assert_eq!(again.manifest_digest, verified.manifest_digest);
    let receipt = installed(&authority, &stack.artifacts, again.receipt_id, 0x05, 6_000);
    assert_eq!(receipt.installation_generation.get(), 5);
    let replay = replayed(&authority, &stack.artifacts, again.receipt_id, 0x05, 6_000);
    assert_eq!(replay, receipt);
    assert_counts(&stack, 1, 5);
}

/// 正常停用：disable API 单事务落 immutable disable receipt 并 CAS
/// installed→disabled（代际不动）；同 key 重放返回原回执不产生新事实；
/// 只读回读逐字段一致。
#[test]
fn disable_installed_application_replays_idempotently() {
    let stack = TestStack::new(&label("disable"), 0x2A);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());
    let installation = installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );

    let receipt = disabled(&authority, verified.package_id, 0x0A, 4_000);
    assert_eq!(receipt.application_id, installation.application_id);
    assert_eq!(
        receipt.application_generation,
        Generation::INITIAL,
        "disable never moves the generation"
    );
    assert_eq!(receipt.idempotency_key, key(0x0A));
    assert_eq!(receipt.disabled_at_ms, 4_000);

    let view = authority
        .inspect_application(verified.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(view.status, nlos_application::ApplicationStatus::Disabled);
    assert_eq!(
        view.current_installation_generation,
        Generation::INITIAL,
        "the generation is untouched by the transition"
    );
    assert_eq!(
        view.updated_at_ms, 4_000,
        "the row records the disable as its last update"
    );

    let read_back = authority
        .inspect_disable_receipt(verified.package_id)
        .expect("disable readback")
        .expect("disable receipt exists");
    assert_eq!(read_back, receipt);

    let replay = disable_replayed(&authority, verified.package_id, 0x0A, 4_000);
    assert_eq!(replay, receipt);
    assert_counts(&stack, 1, 1);
    assert_disable_counts(&stack, 1);
}

/// 停用拒绝全表：未知 package（ApplicationNotFound）、早于当前安装时间
/// （DisablePrecedesInstallation）、同 key 异形（IdempotencyConflict，
/// replay-first：异 package 也先撞 key）、终态异键（ApplicationAlready
/// Disabled）；全部 typed 且零 durable 状态变化。
#[test]
fn disable_refusals_are_typed_and_leave_zero_state() {
    let stack = TestStack::new(&label("disable-refusals"), 0x2B);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());

    let error = authority
        .disable_application(DisableApplicationRequest {
            package_id: verified.package_id,
            idempotency_key: key(0x0A),
            disabled_at_ms: 3_000,
        })
        .expect_err("nothing was ever installed");
    assert!(matches!(
        error,
        ApplicationAuthorityError::ApplicationNotFound { package_id }
            if package_id == verified.package_id
    ));

    installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );

    let error = authority
        .disable_application(DisableApplicationRequest {
            package_id: verified.package_id,
            idempotency_key: key(0x0A),
            disabled_at_ms: 1_999,
        })
        .expect_err("disable must not precede its own installation");
    assert!(matches!(
        error,
        ApplicationAuthorityError::DisablePrecedesInstallation {
            installed_at_ms: 2_000,
            disabled_at_ms: 1_999,
        }
    ));
    assert_disable_counts(&stack, 0);

    let receipt = disabled(&authority, verified.package_id, 0x0A, 3_000);
    assert_disable_counts(&stack, 1);

    let error = authority
        .disable_application(DisableApplicationRequest {
            package_id: verified.package_id,
            idempotency_key: key(0x0A),
            disabled_at_ms: 9_000,
        })
        .expect_err("same key with a different timestamp must conflict");
    assert!(matches!(
        error,
        ApplicationAuthorityError::IdempotencyConflict
    ));

    // Replay-first ordering: the same key names the recorded fact, so even
    // an unknown package under it conflicts before any existence check.
    let error = authority
        .disable_application(DisableApplicationRequest {
            package_id: nlos_types::PackageId::from_bytes([0x42; 16]),
            idempotency_key: key(0x0A),
            disabled_at_ms: 3_000,
        })
        .expect_err("the key is bound to its original request shape");
    assert!(matches!(
        error,
        ApplicationAuthorityError::IdempotencyConflict
    ));

    let error = authority
        .disable_application(DisableApplicationRequest {
            package_id: verified.package_id,
            idempotency_key: key(0x0B),
            disabled_at_ms: 3_000,
        })
        .expect_err("a distinct command against the terminal state is refused");
    assert!(matches!(
        error,
        ApplicationAuthorityError::ApplicationAlreadyDisabled { application_id }
            if application_id == receipt.application_id
    ));

    assert_counts(&stack, 1, 1);
    assert_disable_counts(&stack, 1);
    let view = authority
        .inspect_application(verified.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(view.status, nlos_application::ApplicationStatus::Disabled);
    assert_eq!(view.current_installation_generation, Generation::INITIAL);
}

/// API 停用后的终态全表面：installed 时插入 disable receipt 被 state
/// bounds trigger abort；停用后 fresh key 重装 typed `ApplicationDisabled`
/// （代际/状态纹丝不动）；disable receipt 不可变、不可删、同 application
/// 第二条被 PRIMARY KEY 拒绝。
#[test]
fn api_disabled_application_refuses_reinstall_and_pins_receipt_guards() {
    let stack = TestStack::new(&label("api-disable"), 0x2C);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());
    let installation = installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );
    let database = authority_database(stack.root.root());
    let raw = Connection::open(&database).expect("raw connection");

    // The state-bounds guard: a disable receipt can only exist for an
    // application that is already disabled — inserting while installed
    // aborts even with a perfectly shaped row.
    assert!(
        raw.execute(
            "INSERT INTO application_disable_receipts (
                application_id, idempotency_key, application_generation, disabled_at_ms
             ) VALUES (?1, x'BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB', 1, 3_000)",
            [installation.application_id.as_bytes().as_slice()],
        )
        .is_err(),
        "a disable receipt can never precede the disable transition"
    );

    let receipt = disabled(&authority, verified.package_id, 0x0A, 3_000);

    // At most one disable receipt per application, ever (terminal status).
    assert!(
        raw.execute(
            "INSERT INTO application_disable_receipts (
                application_id, idempotency_key, application_generation, disabled_at_ms
             ) VALUES (?1, x'BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB', 1, 4_000)",
            [receipt.application_id.as_bytes().as_slice()],
        )
        .is_err(),
        "the DDL primary key encodes the terminality"
    );

    let error = authority
        .install_application(
            &stack.artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: verified.receipt_id,
                idempotency_key: key(0x02),
                installed_at_ms: 4_000,
            },
        )
        .expect_err("an api-disabled application must refuse new installations");
    assert!(matches!(
        error,
        ApplicationAuthorityError::ApplicationDisabled { application_id }
            if application_id == receipt.application_id
    ));

    assert!(
        raw.execute(
            "UPDATE application_disable_receipts SET disabled_at_ms=99",
            []
        )
        .is_err(),
        "a disable receipt can never be rewritten"
    );
    assert!(
        raw.execute("DELETE FROM application_disable_receipts", [])
            .is_err(),
        "a disable receipt is durable"
    );
    drop(raw);

    assert_counts(&stack, 1, 1);
    assert_disable_counts(&stack, 1);
    let view = authority
        .inspect_application(verified.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(view.status, nlos_application::ApplicationStatus::Disabled);
    assert_eq!(view.current_installation_generation, Generation::INITIAL);
    assert_eq!(
        authority
            .inspect_disable_receipt(verified.package_id)
            .expect("readback")
            .expect("durable"),
        receipt
    );
}

/// 正常更新：installed 状态下新 verified package（manifest 变化）推进
/// 一代并落 immutable installation receipt；authority 派生 Id 与 inspect/
/// list 只读回读逐字段一致。
#[test]
fn update_installed_application_advances_generation() {
    let stack = TestStack::new(&label("update"), 0x31);
    let first = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let second = stack.verify_package(0x41, 2, key(0xF1), 2_000);
    assert_ne!(first.manifest_digest, second.manifest_digest);
    let authority = open_authority(stack.root.root());

    let gen1 = installed(&authority, &stack.artifacts, first.receipt_id, 0x01, 2_000);
    let gen2 = updated(
        &authority,
        &stack.artifacts,
        first.package_id,
        second.receipt_id,
        0x02,
        3_000,
    );
    assert_eq!(gen2.installation_generation.get(), 2);
    assert_eq!(gen2.package_manifest_digest, second.manifest_digest);
    assert_eq!(gen2.package_version, 2);
    assert_eq!(gen2.package_verification_receipt_id, second.receipt_id);
    assert_eq!(
        gen2.installation_id,
        derive_installation_id(key(0x02), gen1.application_id, gen2.installation_generation)
    );

    let view = authority
        .inspect_application(first.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(view.status, nlos_application::ApplicationStatus::Installed);
    assert_eq!(view.package_manifest_digest, second.manifest_digest);
    assert_eq!(view.current_installation_generation.get(), 2);
    assert_eq!(view.updated_at_ms, 3_000);

    let read_back = authority
        .inspect_installation(gen2.installation_id)
        .expect("inspect installation");
    assert_eq!(read_back, gen2);
    assert_eq!(
        authority
            .list_installations(gen1.application_id)
            .expect("list"),
        vec![gen1, gen2.clone()]
    );
}

/// 更新幂等 replay：同 key 重放返回原 receipt 不双跳；同 key 不同请求
/// 形状为 typed `IdempotencyConflict`。
#[test]
fn update_replays_idempotently_and_conflicts_on_shape_mismatch() {
    let stack = TestStack::new(&label("update-replay"), 0x32);
    let first = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let second = stack.verify_package(0x41, 2, key(0xF1), 2_000);
    let authority = open_authority(stack.root.root());
    installed(&authority, &stack.artifacts, first.receipt_id, 0x01, 2_000);
    let receipt = updated(
        &authority,
        &stack.artifacts,
        first.package_id,
        second.receipt_id,
        0x02,
        3_000,
    );

    let replay = update_replayed(
        &authority,
        &stack.artifacts,
        first.package_id,
        second.receipt_id,
        0x02,
        3_000,
    );
    assert_eq!(replay, receipt);
    assert_eq!(
        authority
            .inspect_application(first.package_id)
            .expect("inspect")
            .expect("exists")
            .current_installation_generation
            .get(),
        2,
        "replay never advances the generation"
    );
    assert_counts(&stack, 1, 2);

    let conflict = authority.update_application(
        &stack.artifacts,
        UpdateApplicationRequest {
            package_id: first.package_id,
            package_verification_receipt_id: second.receipt_id,
            idempotency_key: key(0x02),
            updated_at_ms: 9_000,
        },
    );
    assert!(matches!(
        conflict,
        Err(ApplicationAuthorityError::IdempotencyConflict)
    ));

    let third = stack.verify_package(0x41, 3, key(0xF2), 4_000);
    let conflict = authority.update_application(
        &stack.artifacts,
        UpdateApplicationRequest {
            package_id: first.package_id,
            package_verification_receipt_id: third.receipt_id,
            idempotency_key: key(0x02),
            updated_at_ms: 3_000,
        },
    );
    assert!(matches!(
        conflict,
        Err(ApplicationAuthorityError::IdempotencyConflict)
    ));
    assert_counts(&stack, 1, 2);
}

/// 更新重启 replay：重开后同 key 重放逐字节相等、fresh key 从 durable
/// 代际稠密续推。
#[test]
fn update_replay_survives_restart() {
    let stack = TestStack::new(&label("update-restart"), 0x33);
    let first = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let second = stack.verify_package(0x41, 2, key(0xF1), 2_000);
    let gen1 = {
        let authority = open_authority(stack.root.root());
        installed(&authority, &stack.artifacts, first.receipt_id, 0x01, 2_000)
    };

    let reopened_artifacts =
        nlos_artifact::ArtifactStore::open(stack.root.root().join("art")).expect("reopen art");
    let authority = open_authority(stack.root.root());
    let gen2 = updated(
        &authority,
        &reopened_artifacts,
        first.package_id,
        second.receipt_id,
        0x02,
        3_000,
    );
    assert_eq!(gen2.installation_generation.get(), 2);

    let replay = update_replayed(
        &authority,
        &reopened_artifacts,
        first.package_id,
        second.receipt_id,
        0x02,
        3_000,
    );
    assert_eq!(replay, gen2);

    let third = stack.verify_package(0x41, 3, key(0xF3), 4_000);
    let gen3 = updated(
        &authority,
        &reopened_artifacts,
        first.package_id,
        third.receipt_id,
        0x03,
        5_000,
    );
    assert_eq!(gen3.installation_generation.get(), 3);
    assert_eq!(
        authority
            .list_installations(gen1.application_id)
            .expect("list")
            .len(),
        3
    );
}

/// 更新拒绝全表：未安装（ApplicationNotFound）、disabled
/// （ApplicationDisabled）、manifest 未变（UpdateManifestUnchanged）、
/// package 身份不符（PackageIdentityMismatch）、早于验证时间
/// （InstallationPrecedesVerification）、未验证 receipt
/// （PackageVerificationReceiptNotFound）；全部 typed 且零 durable 变化。
#[test]
#[allow(clippy::too_many_lines)] // One linear refusal sweep over the update surface.
fn update_refusals_are_typed_and_leave_zero_state() {
    let stack = TestStack::new(&label("update-refusals"), 0x34);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());

    let error = authority
        .update_application(
            &stack.artifacts,
            UpdateApplicationRequest {
                package_id: verified.package_id,
                package_verification_receipt_id: verified.receipt_id,
                idempotency_key: key(0x01),
                updated_at_ms: 2_000,
            },
        )
        .expect_err("update requires a prior install");
    assert!(matches!(
        error,
        ApplicationAuthorityError::ApplicationNotFound { package_id }
            if package_id == verified.package_id
    ));
    assert_counts(&stack, 0, 0);

    installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );

    let newer = stack.verify_package(0x41, 2, key(0xF6), 5_000);
    let error = authority
        .update_application(
            &stack.artifacts,
            UpdateApplicationRequest {
                package_id: verified.package_id,
                package_verification_receipt_id: newer.receipt_id,
                idempotency_key: key(0x02),
                updated_at_ms: 4_999,
            },
        )
        .expect_err("update must not precede verification");
    assert!(matches!(
        error,
        ApplicationAuthorityError::InstallationPrecedesVerification {
            verified_at_ms: 5_000,
            installed_at_ms: 4_999,
        }
    ));

    let error = authority
        .update_application(
            &stack.artifacts,
            UpdateApplicationRequest {
                package_id: verified.package_id,
                package_verification_receipt_id: verified.receipt_id,
                idempotency_key: key(0x02),
                updated_at_ms: 2_000,
            },
        )
        .expect_err("same manifest is not an update");
    assert!(matches!(
        error,
        ApplicationAuthorityError::UpdateManifestUnchanged { package_id, .. }
            if package_id == verified.package_id
    ));
    assert_counts(&stack, 1, 1);

    let other_package = stack.verify_package(0x42, 1, key(0xF4), 2_000);
    let error = authority
        .update_application(
            &stack.artifacts,
            UpdateApplicationRequest {
                package_id: verified.package_id,
                package_verification_receipt_id: other_package.receipt_id,
                idempotency_key: key(0x03),
                updated_at_ms: 3_000,
            },
        )
        .expect_err("verified package must match the named package");
    assert!(matches!(
        error,
        ApplicationAuthorityError::PackageIdentityMismatch { expected, actual }
            if expected == verified.package_id && actual == other_package.package_id
    ));

    let ghost = ReceiptId::from_bytes([0x99; 16]);
    let error = authority
        .update_application(
            &stack.artifacts,
            UpdateApplicationRequest {
                package_id: verified.package_id,
                package_verification_receipt_id: ghost,
                idempotency_key: key(0x04),
                updated_at_ms: 3_000,
            },
        )
        .expect_err("unverified receipt");
    assert!(matches!(
        error,
        ApplicationAuthorityError::PackageVerificationReceiptNotFound(id) if id == ghost
    ));

    disabled(&authority, verified.package_id, 0x0A, 4_000);
    let disabled_target = stack.verify_package(0x41, 3, key(0xF5), 3_000);
    let error = authority
        .update_application(
            &stack.artifacts,
            UpdateApplicationRequest {
                package_id: verified.package_id,
                package_verification_receipt_id: disabled_target.receipt_id,
                idempotency_key: key(0x05),
                updated_at_ms: 5_000,
            },
        )
        .expect_err("disabled application must refuse updates");
    assert!(matches!(
        error,
        ApplicationAuthorityError::ApplicationDisabled { .. }
    ));
    assert_counts(&stack, 1, 1);
    assert_disable_counts(&stack, 1);
}

/// 正常卸载（installed）：uninstall API 单事务落 immutable uninstall
/// receipt 并 CAS installed→uninstalled（代际不动）；同 key 重放返回原回执。
#[test]
fn uninstall_installed_application_replays_idempotently() {
    let stack = TestStack::new(&label("uninstall"), 0x3A);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());
    let installation = installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );

    let receipt = uninstalled(&authority, verified.package_id, 0x0A, 4_000);
    assert_eq!(receipt.application_id, installation.application_id);
    assert_eq!(
        receipt.application_generation,
        Generation::INITIAL,
        "uninstall never moves the generation"
    );
    assert_eq!(receipt.idempotency_key, key(0x0A));
    assert_eq!(receipt.uninstalled_at_ms, 4_000);

    let view = authority
        .inspect_application(verified.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(
        view.status,
        nlos_application::ApplicationStatus::Uninstalled
    );
    assert_eq!(
        view.current_installation_generation,
        Generation::INITIAL,
        "the generation is untouched by the transition"
    );
    assert_eq!(
        view.updated_at_ms, 4_000,
        "the row records the uninstall as its last update"
    );

    let read_back = authority
        .inspect_uninstall_receipt(verified.package_id)
        .expect("uninstall readback")
        .expect("uninstall receipt exists");
    assert_eq!(read_back, receipt);

    let replay = uninstall_replayed(&authority, verified.package_id, 0x0A, 4_000);
    assert_eq!(replay, receipt);
    assert_counts(&stack, 1, 1);
    assert_uninstall_counts(&stack, 1);
}

/// 正常卸载（disabled）：disabled 状态可经 uninstall 进入终态 uninstalled；
/// 代际不动；同 key 重放返回原回执。
#[test]
fn uninstall_disabled_application_replays_idempotently() {
    let stack = TestStack::new(&label("uninstall-disabled"), 0x3B);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());
    let installation = installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );
    disabled(&authority, verified.package_id, 0x0B, 3_000);

    let receipt = uninstalled(&authority, verified.package_id, 0x0A, 4_000);
    assert_eq!(receipt.application_id, installation.application_id);
    assert_eq!(
        receipt.application_generation,
        Generation::INITIAL,
        "uninstall from disabled never moves the generation"
    );

    let view = authority
        .inspect_application(verified.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(
        view.status,
        nlos_application::ApplicationStatus::Uninstalled
    );
    assert_eq!(view.current_installation_generation, Generation::INITIAL);

    let replay = uninstall_replayed(&authority, verified.package_id, 0x0A, 4_000);
    assert_eq!(replay, receipt);
    assert_counts(&stack, 1, 1);
    assert_disable_counts(&stack, 1);
    assert_uninstall_counts(&stack, 1);
}

/// 更新后代际卸载：update 推进到 gen 2 后 uninstall 回执记录 gen 2（代际不动）。
#[test]
fn update_then_uninstall_records_current_generation() {
    let stack = TestStack::new(&label("uninstall-after-update"), 0x3C);
    let first = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let second = stack.verify_package(0x41, 2, key(0xF1), 2_000);
    let authority = open_authority(stack.root.root());
    let gen1 = installed(&authority, &stack.artifacts, first.receipt_id, 0x01, 2_000);
    updated(
        &authority,
        &stack.artifacts,
        first.package_id,
        second.receipt_id,
        0x02,
        3_000,
    );

    let receipt = uninstalled(&authority, first.package_id, 0x0A, 5_000);
    assert_eq!(receipt.application_id, gen1.application_id);
    assert_eq!(
        receipt.application_generation.get(),
        2,
        "the uninstall receipt pins the generation at uninstall time"
    );

    let view = authority
        .inspect_application(first.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(
        view.status,
        nlos_application::ApplicationStatus::Uninstalled
    );
    assert_eq!(view.current_installation_generation.get(), 2);
    assert_eq!(view.updated_at_ms, 5_000);
    assert_uninstall_counts(&stack, 1);
}

/// 卸载拒绝全表：未知 package（ApplicationNotFound）、早于当前更新时间
/// （UninstallPrecedesLastUpdate）、同 key 异形（IdempotencyConflict，
/// replay-first：异 package 也先撞 key）、终态异键（ApplicationAlready
/// Uninstalled）；全部 typed 且零 durable 状态变化。
#[test]
fn uninstall_refusals_are_typed_and_leave_zero_state() {
    let stack = TestStack::new(&label("uninstall-refusals"), 0x3D);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());

    let error = authority
        .uninstall_application(UninstallApplicationRequest {
            package_id: verified.package_id,
            idempotency_key: key(0x0A),
            uninstalled_at_ms: 3_000,
        })
        .expect_err("nothing was ever installed");
    assert!(matches!(
        error,
        ApplicationAuthorityError::ApplicationNotFound { package_id }
            if package_id == verified.package_id
    ));

    installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );

    let error = authority
        .uninstall_application(UninstallApplicationRequest {
            package_id: verified.package_id,
            idempotency_key: key(0x0A),
            uninstalled_at_ms: 1_999,
        })
        .expect_err("uninstall must not precede its own installation");
    assert!(matches!(
        error,
        ApplicationAuthorityError::UninstallPrecedesLastUpdate {
            last_updated_at_ms: 2_000,
            uninstalled_at_ms: 1_999,
        }
    ));
    assert_uninstall_counts(&stack, 0);

    let receipt = uninstalled(&authority, verified.package_id, 0x0A, 3_000);
    assert_uninstall_counts(&stack, 1);

    let error = authority
        .uninstall_application(UninstallApplicationRequest {
            package_id: verified.package_id,
            idempotency_key: key(0x0A),
            uninstalled_at_ms: 9_000,
        })
        .expect_err("same key with a different timestamp must conflict");
    assert!(matches!(
        error,
        ApplicationAuthorityError::IdempotencyConflict
    ));

    // Replay-first ordering: the same key names the recorded fact, so even
    // an unknown package under it conflicts before any existence check.
    let error = authority
        .uninstall_application(UninstallApplicationRequest {
            package_id: nlos_types::PackageId::from_bytes([0x42; 16]),
            idempotency_key: key(0x0A),
            uninstalled_at_ms: 3_000,
        })
        .expect_err("the key is bound to its original request shape");
    assert!(matches!(
        error,
        ApplicationAuthorityError::IdempotencyConflict
    ));

    let error = authority
        .uninstall_application(UninstallApplicationRequest {
            package_id: verified.package_id,
            idempotency_key: key(0x0B),
            uninstalled_at_ms: 3_000,
        })
        .expect_err("a distinct command against the terminal state is refused");
    assert!(matches!(
        error,
        ApplicationAuthorityError::ApplicationAlreadyUninstalled { application_id }
            if application_id == receipt.application_id
    ));

    assert_counts(&stack, 1, 1);
    assert_uninstall_counts(&stack, 1);
    let view = authority
        .inspect_application(verified.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(
        view.status,
        nlos_application::ApplicationStatus::Uninstalled
    );
    assert_eq!(view.current_installation_generation, Generation::INITIAL);
}

/// 正常回退（disabled）：update 到 gen 2 后 disable，rollback 单事务 CAS
/// disabled→installed 并代际 -1、恢复 gen 1 manifest；同 key 重放返回原回执。
#[test]
fn rollback_disabled_after_update_replays_idempotently() {
    let stack = TestStack::new(&label("rollback-disabled"), 0x4A);
    let first = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let second = stack.verify_package(0x41, 2, key(0xF1), 2_000);
    let authority = open_authority(stack.root.root());
    let gen1 = installed(&authority, &stack.artifacts, first.receipt_id, 0x01, 2_000);
    updated(
        &authority,
        &stack.artifacts,
        first.package_id,
        second.receipt_id,
        0x02,
        3_000,
    );
    disabled(&authority, first.package_id, 0x0B, 4_000);

    let receipt = rolled_back(&authority, first.package_id, 0x0A, 5_000);
    assert_eq!(receipt.application_id, gen1.application_id);
    assert_eq!(receipt.from_generation.get(), 2);
    assert_eq!(receipt.to_generation, Generation::INITIAL);
    assert_eq!(receipt.rollback_at_ms, 5_000);

    let view = authority
        .inspect_application(first.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(view.status, nlos_application::ApplicationStatus::Installed);
    assert_eq!(view.current_installation_generation, Generation::INITIAL);
    assert_eq!(view.package_manifest_digest, first.manifest_digest);
    assert_eq!(view.updated_at_ms, 5_000);

    let read_back = authority
        .inspect_rollback_receipt(key(0x0A))
        .expect("rollback readback")
        .expect("rollback receipt exists");
    assert_eq!(read_back, receipt);

    let replay = rollback_replayed(&authority, first.package_id, 0x0A, 5_000);
    assert_eq!(replay, receipt);
    assert_counts(&stack, 1, 2);
    assert_disable_counts(&stack, 1);
    assert_rollback_counts(&stack, 1);
}

/// 正常回退（uninstalled）：update 到 gen 2 后 uninstall，rollback 恢复
/// gen 1 manifest 并 CAS uninstalled→installed；同 key 重放返回原回执。
#[test]
fn rollback_uninstalled_after_update_replays_idempotently() {
    let stack = TestStack::new(&label("rollback-uninstalled"), 0x4B);
    let first = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let second = stack.verify_package(0x41, 2, key(0xF1), 2_000);
    let authority = open_authority(stack.root.root());
    let gen1 = installed(&authority, &stack.artifacts, first.receipt_id, 0x01, 2_000);
    updated(
        &authority,
        &stack.artifacts,
        first.package_id,
        second.receipt_id,
        0x02,
        3_000,
    );
    uninstalled(&authority, first.package_id, 0x0B, 4_000);

    let receipt = rolled_back(&authority, first.package_id, 0x0A, 5_000);
    assert_eq!(receipt.application_id, gen1.application_id);
    assert_eq!(receipt.from_generation.get(), 2);
    assert_eq!(receipt.to_generation, Generation::INITIAL);

    let view = authority
        .inspect_application(first.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(view.status, nlos_application::ApplicationStatus::Installed);
    assert_eq!(view.package_manifest_digest, gen1.package_manifest_digest);

    let replay = rollback_replayed(&authority, first.package_id, 0x0A, 5_000);
    assert_eq!(replay, receipt);
    assert_uninstall_counts(&stack, 1);
    assert_rollback_counts(&stack, 1);
}

/// 回退拒绝全表：未知 package、installed 状态、gen 1 无上一代、早于最后
/// 更新、同 key 异形、幂等 replay-first；全部 typed 且零 durable 变化。
#[test]
#[allow(clippy::too_many_lines)]
fn rollback_refusals_are_typed_and_leave_zero_state() {
    let stack = TestStack::new(&label("rollback-refusals"), 0x4C);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());

    let error = authority
        .rollback_application(RollbackApplicationRequest {
            package_id: verified.package_id,
            idempotency_key: key(0x0A),
            rollback_at_ms: 3_000,
        })
        .expect_err("nothing was ever installed");
    assert!(matches!(
        error,
        ApplicationAuthorityError::ApplicationNotFound { package_id }
            if package_id == verified.package_id
    ));

    installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );

    let error = authority
        .rollback_application(RollbackApplicationRequest {
            package_id: verified.package_id,
            idempotency_key: key(0x0A),
            rollback_at_ms: 3_000,
        })
        .expect_err("installed application cannot roll back");
    assert!(matches!(
        error,
        ApplicationAuthorityError::RollbackRequiresDisabledOrUninstalled {
            status: nlos_application::ApplicationStatus::Installed,
            ..
        }
    ));
    assert_rollback_counts(&stack, 0);

    let second = stack.verify_package(0x41, 2, key(0xF1), 2_000);
    updated(
        &authority,
        &stack.artifacts,
        verified.package_id,
        second.receipt_id,
        0x02,
        4_500,
    );
    disabled(&authority, verified.package_id, 0x0B, 5_000);

    let error = authority
        .rollback_application(RollbackApplicationRequest {
            package_id: verified.package_id,
            idempotency_key: key(0x0A),
            rollback_at_ms: 4_999,
        })
        .expect_err("rollback must not precede its own disable");
    assert!(matches!(
        error,
        ApplicationAuthorityError::RollbackPrecedesLastUpdate {
            last_updated_at_ms: 5_000,
            rollback_at_ms: 4_999,
        }
    ));

    let receipt = rolled_back(&authority, verified.package_id, 0x0A, 6_000);
    assert_rollback_counts(&stack, 1);

    let gen1_only = stack.verify_package(0x42, 1, key(0xF2), 1_000);
    installed(
        &authority,
        &stack.artifacts,
        gen1_only.receipt_id,
        0x03,
        2_000,
    );
    disabled(&authority, gen1_only.package_id, 0x0C, 3_000);
    let error = authority
        .rollback_application(RollbackApplicationRequest {
            package_id: gen1_only.package_id,
            idempotency_key: key(0x0D),
            rollback_at_ms: 4_000,
        })
        .expect_err("gen 1 has no previous generation");
    assert!(matches!(
        error,
        ApplicationAuthorityError::RollbackAtInitialGeneration { .. }
    ));

    let error = authority
        .rollback_application(RollbackApplicationRequest {
            package_id: verified.package_id,
            idempotency_key: key(0x0A),
            rollback_at_ms: 9_000,
        })
        .expect_err("same key with a different timestamp must conflict");
    assert!(matches!(
        error,
        ApplicationAuthorityError::IdempotencyConflict
    ));

    let error = authority
        .rollback_application(RollbackApplicationRequest {
            package_id: nlos_types::PackageId::from_bytes([0x42; 16]),
            idempotency_key: key(0x0A),
            rollback_at_ms: 6_000,
        })
        .expect_err("the key is bound to its original request shape");
    assert!(matches!(
        error,
        ApplicationAuthorityError::IdempotencyConflict
    ));

    let error = authority
        .rollback_application(RollbackApplicationRequest {
            package_id: verified.package_id,
            idempotency_key: key(0x0E),
            rollback_at_ms: 6_000,
        })
        .expect_err("installed application cannot roll back again");
    assert!(matches!(
        error,
        ApplicationAuthorityError::RollbackRequiresDisabledOrUninstalled {
            status: nlos_application::ApplicationStatus::Installed,
            application_id,
        } if application_id == receipt.application_id
    ));

    assert_counts(&stack, 2, 3);
    assert_rollback_counts(&stack, 1);
    let view = authority
        .inspect_application(verified.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(view.status, nlos_application::ApplicationStatus::Installed);
    assert_eq!(view.current_installation_generation, Generation::INITIAL);
}
