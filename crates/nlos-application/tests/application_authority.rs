//! B-APPLICATION-001 authority tests: the durable Application/Installation
//! authority — normal install, reinstall/idempotent replay, restart replay,
//! authority-first refusals (unverified receipt reference, installation
//! preceding verification, idempotency conflict, disabled application),
//! current-digest tracking, and the DDL trigger guards.

mod support;

use std::sync::atomic::{AtomicU64, Ordering};

use nlos_application::{
    ActiveTaskActivityProbe, ApplicationAuthorityError, CompatibilityWindow,
    DisableApplicationRequest, InstallApplicationRequest, RegisterBackgroundTaskRequest,
    RegisterProcessBindingRequest, RollbackApplicationRequest, UninstallApplicationRequest,
    UpdateApplicationRequest, UpdateDecision, derive_application_id, derive_installation_id,
    pack_package_version,
};
use nlos_types::{Generation, IdempotencyKey, PackageId, ProcessId, ReceiptId, TaskId};
use rusqlite::Connection;
use support::{
    TestStack, authority_database, background_task_registered,
    background_task_registration_replayed, disable_replayed, disabled, installed, open_authority,
    process_binding_registered, process_binding_registration_replayed, replayed, rollback_replayed,
    rolled_back, uninstall_replayed, uninstalled, update_replayed, updated,
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

fn assert_background_task_registration_counts(stack: &TestStack, registrations: i64) {
    let database = authority_database(stack.root.root());
    assert_eq!(
        raw_count(
            &database,
            "SELECT COUNT(*) FROM application_background_task_registrations"
        ),
        registrations,
        "unexpected application_background_task_registrations row count"
    );
}

fn assert_process_binding_counts(stack: &TestStack, bindings: i64) {
    let database = authority_database(stack.root.root());
    assert_eq!(
        raw_count(
            &database,
            "SELECT COUNT(*) FROM application_process_bindings"
        ),
        bindings,
        "unexpected application_process_bindings row count"
    );
}

fn task_id(seed: u8) -> TaskId {
    TaskId::from_bytes([seed; 16])
}

fn process_id(seed: u8) -> ProcessId {
    ProcessId::from_bytes([seed; 16])
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

    // Disabled can only transition to uninstalled or the forward-roll
    // rollback-to-installed (with a generation advance); bare re-enable
    // is illegal.
    raw.execute("UPDATE applications SET status=2", [])
        .expect("legal disable");
    assert!(
        raw.execute("UPDATE applications SET status=1", []).is_err(),
        "re-enabling a disabled application without a generation advance is illegal"
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
        "uninstalled is terminal except the forward-roll rollback with a generation advance"
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
        .is_err(),
        "re-enabling without a fresh generation is illegal"
    );
    assert!(
        raw.execute(
            "UPDATE applications SET status=1, current_installation_generation=1",
            []
        )
        .is_err(),
        "the generation never decreases, not even on rollback"
    );
    raw.execute(
        "UPDATE applications
         SET status=1,
             current_installation_generation=current_installation_generation+1,
             updated_at_ms=updated_at_ms+1",
        [],
    )
    .expect("the forward roll re-enables onto a fresh generation");

    // The guarded authority still serves reads; durable state is untouched.
    let view = authority
        .inspect_application(verified.package_id)
        .expect("inspect after tamper sweep")
        .expect("exists");
    assert_eq!(view.status, nlos_application::ApplicationStatus::Installed);
    assert_eq!(view.current_installation_generation.get(), 3);
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
            compatibility_window: CompatibilityWindow::SameMajor,
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
            compatibility_window: CompatibilityWindow::SameMajor,
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
                compatibility_window: CompatibilityWindow::SameMajor,
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
                compatibility_window: CompatibilityWindow::SameMajor,
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
                compatibility_window: CompatibilityWindow::SameMajor,
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
                compatibility_window: CompatibilityWindow::SameMajor,
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
                compatibility_window: CompatibilityWindow::SameMajor,
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
                compatibility_window: CompatibilityWindow::SameMajor,
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

/// Same-major minor/patch advance is accepted under
/// [`CompatibilityWindow::SameMajor`].
#[test]
fn update_compat_accepts_same_major() {
    let stack = TestStack::new(&label("update-compat-accept"), 0x35);
    let first = stack.verify_package(0x41, pack_package_version(1, 0, 0), key(0xF0), 1_000);
    let second = stack.verify_package(0x41, pack_package_version(1, 2, 3), key(0xF1), 2_000);
    assert_ne!(first.manifest_digest, second.manifest_digest);
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
    assert_eq!(receipt.package_version, pack_package_version(1, 2, 3));
    assert_counts(&stack, 1, 2);
}

/// Cross-major updates fail closed with zero durable state.
#[test]
fn update_compat_rejects_cross_major() {
    let stack = TestStack::new(&label("update-compat-reject"), 0x36);
    let first = stack.verify_package(0x41, pack_package_version(1, 0, 0), key(0xF0), 1_000);
    let second = stack.verify_package(0x41, pack_package_version(2, 0, 0), key(0xF1), 2_000);
    let authority = open_authority(stack.root.root());
    installed(&authority, &stack.artifacts, first.receipt_id, 0x01, 2_000);

    let error = authority
        .update_application(
            &stack.artifacts,
            UpdateApplicationRequest {
                package_id: first.package_id,
                package_verification_receipt_id: second.receipt_id,
                idempotency_key: key(0x02),
                updated_at_ms: 3_000,
                compatibility_window: CompatibilityWindow::SameMajor,
            },
        )
        .expect_err("cross-major update must be refused");
    assert!(matches!(
        error,
        ApplicationAuthorityError::UpdateCompatibilityViolation {
            package_id,
            current_version,
            target_version,
            compatibility_window: CompatibilityWindow::SameMajor,
        } if package_id == first.package_id
            && current_version == pack_package_version(1, 0, 0)
            && target_version == pack_package_version(2, 0, 0)
    ));
    assert_counts(&stack, 1, 1);
}

/// Idempotent replay bypasses the compatibility gate and never re-advances.
#[test]
fn update_compat_replay_bypasses_compatibility_gate() {
    let stack = TestStack::new(&label("update-compat-replay"), 0x37);
    let first = stack.verify_package(0x41, pack_package_version(1, 0, 0), key(0xF0), 1_000);
    let second = stack.verify_package(0x41, pack_package_version(1, 1, 0), key(0xF1), 2_000);
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
        2
    );
    assert_counts(&stack, 1, 2);
}

/// Same-major+minor patch advance is accepted under
/// [`CompatibilityWindow::SameMinor`].
#[test]
fn update_compat_same_minor_accepts_patch_bump() {
    let stack = TestStack::new(&label("update-compat-same-minor-accept"), 0x38);
    let first = stack.verify_package(0x41, pack_package_version(1, 2, 0), key(0xF0), 1_000);
    let second = stack.verify_package(0x41, pack_package_version(1, 2, 5), key(0xF1), 2_000);
    assert_ne!(first.manifest_digest, second.manifest_digest);
    let authority = open_authority(stack.root.root());
    installed(&authority, &stack.artifacts, first.receipt_id, 0x01, 2_000);
    let receipt = authority
        .update_application(
            &stack.artifacts,
            UpdateApplicationRequest {
                package_id: first.package_id,
                package_verification_receipt_id: second.receipt_id,
                idempotency_key: key(0x02),
                updated_at_ms: 3_000,
                compatibility_window: CompatibilityWindow::SameMinor,
            },
        )
        .expect("same-minor patch bump must succeed");
    let updated_receipt = match receipt {
        UpdateDecision::Updated(receipt) => receipt,
        UpdateDecision::Replayed(receipt) => {
            panic!("fresh key cannot replay an update, got {receipt:?}")
        }
    };
    assert_eq!(
        updated_receipt.package_version,
        pack_package_version(1, 2, 5)
    );
    assert_counts(&stack, 1, 2);
}

/// Cross-minor updates fail closed under [`CompatibilityWindow::SameMinor`].
#[test]
fn update_compat_same_minor_rejects_cross_minor() {
    let stack = TestStack::new(&label("update-compat-same-minor-reject"), 0x39);
    let first = stack.verify_package(0x41, pack_package_version(1, 2, 0), key(0xF0), 1_000);
    let second = stack.verify_package(0x41, pack_package_version(1, 3, 0), key(0xF1), 2_000);
    let authority = open_authority(stack.root.root());
    installed(&authority, &stack.artifacts, first.receipt_id, 0x01, 2_000);

    let error = authority
        .update_application(
            &stack.artifacts,
            UpdateApplicationRequest {
                package_id: first.package_id,
                package_verification_receipt_id: second.receipt_id,
                idempotency_key: key(0x02),
                updated_at_ms: 3_000,
                compatibility_window: CompatibilityWindow::SameMinor,
            },
        )
        .expect_err("cross-minor update must be refused");
    assert!(matches!(
        error,
        ApplicationAuthorityError::UpdateCompatibilityViolation {
            package_id,
            current_version,
            target_version,
            compatibility_window: CompatibilityWindow::SameMinor,
        } if package_id == first.package_id
            && current_version == pack_package_version(1, 2, 0)
            && target_version == pack_package_version(1, 3, 0)
    ));
    assert_counts(&stack, 1, 1);
}

/// Idempotent replay bypasses the same-minor compatibility gate.
#[test]
fn update_compat_same_minor_replay_bypasses_compatibility_gate() {
    let stack = TestStack::new(&label("update-compat-same-minor-replay"), 0x3B);
    let first = stack.verify_package(0x41, pack_package_version(1, 2, 0), key(0xF0), 1_000);
    let second = stack.verify_package(0x41, pack_package_version(1, 2, 1), key(0xF1), 2_000);
    let authority = open_authority(stack.root.root());
    installed(&authority, &stack.artifacts, first.receipt_id, 0x01, 2_000);
    let receipt = authority
        .update_application(
            &stack.artifacts,
            UpdateApplicationRequest {
                package_id: first.package_id,
                package_verification_receipt_id: second.receipt_id,
                idempotency_key: key(0x02),
                updated_at_ms: 3_000,
                compatibility_window: CompatibilityWindow::SameMinor,
            },
        )
        .expect("update must succeed");
    let updated_receipt = match receipt {
        UpdateDecision::Updated(receipt) => receipt,
        UpdateDecision::Replayed(receipt) => {
            panic!("fresh key cannot replay an update, got {receipt:?}")
        }
    };

    let replay = authority
        .update_application(
            &stack.artifacts,
            UpdateApplicationRequest {
                package_id: first.package_id,
                package_verification_receipt_id: second.receipt_id,
                idempotency_key: key(0x02),
                updated_at_ms: 3_000,
                compatibility_window: CompatibilityWindow::SameMinor,
            },
        )
        .expect("update must replay");
    let replay_receipt = match replay {
        UpdateDecision::Replayed(receipt) => receipt,
        UpdateDecision::Updated(receipt) => {
            panic!("expected Replayed, got Updated {receipt:?}")
        }
    };
    assert_eq!(replay_receipt, updated_receipt);
    assert_eq!(
        authority
            .inspect_application(first.package_id)
            .expect("inspect")
            .expect("exists")
            .current_installation_generation
            .get(),
        2
    );
    assert_counts(&stack, 1, 2);
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

/// 正常回退（disabled）：update 到 gen 2 后 disable，rollback 前滚——旧内容
/// （gen 1 receipt）装进全新 gen 3、状态 CAS disabled→installed；同 key 重放
/// 返回原回执，代际历史保持稠密单调。
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
    assert_eq!(receipt.to_generation.get(), 3);
    assert_eq!(receipt.rollback_at_ms, 5_000);

    let view = authority
        .inspect_application(first.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(view.status, nlos_application::ApplicationStatus::Installed);
    assert_eq!(view.current_installation_generation.get(), 3);
    assert_eq!(view.package_manifest_digest, first.manifest_digest);
    assert_eq!(view.updated_at_ms, 5_000);

    // The forward roll committed a fresh installation receipt at gen 3
    // whose content is the gen 1 receipt, bitwise.
    let listed = authority
        .list_installations(gen1.application_id)
        .expect("list");
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[2].installation_generation.get(), 3);
    assert_eq!(
        listed[2].package_manifest_digest,
        gen1.package_manifest_digest
    );
    assert_eq!(listed[2].package_version, gen1.package_version);
    assert_eq!(listed[2].entry_count, gen1.entry_count);
    assert_eq!(
        listed[2].package_verification_receipt_id,
        gen1.package_verification_receipt_id
    );
    assert_eq!(listed[2].installer_principal, gen1.installer_principal);
    assert_eq!(listed[2].installed_at_ms, 5_000);

    let read_back = authority
        .inspect_rollback_receipt(key(0x0A))
        .expect("rollback readback")
        .expect("rollback receipt exists");
    assert_eq!(read_back, receipt);

    let replay = rollback_replayed(&authority, first.package_id, 0x0A, 5_000);
    assert_eq!(replay, receipt);
    assert_counts(&stack, 1, 3);
    assert_disable_counts(&stack, 1);
    assert_rollback_counts(&stack, 1);
}

/// 正常回退（uninstalled）：update 到 gen 2 后 uninstall，rollback 前滚到
/// gen 3 并恢复 gen 1 内容、CAS uninstalled→installed；同 key 重放返回原回执。
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
    assert_eq!(receipt.to_generation.get(), 3);

    let view = authority
        .inspect_application(first.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(view.status, nlos_application::ApplicationStatus::Installed);
    assert_eq!(view.current_installation_generation.get(), 3);
    assert_eq!(view.package_manifest_digest, gen1.package_manifest_digest);

    let replay = rollback_replayed(&authority, first.package_id, 0x0A, 5_000);
    assert_eq!(replay, receipt);
    assert_counts(&stack, 1, 3);
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

    assert_counts(&stack, 2, 4);
    assert_rollback_counts(&stack, 1);
    let view = authority
        .inspect_application(verified.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(view.status, nlos_application::ApplicationStatus::Installed);
    assert_eq!(view.current_installation_generation.get(), 3);
}

/// 前滚多跳（D1）：回滚后 update 通道仍然打开——每次生命周期动作都落在
/// 全新代际 + 全新 receipt，`UNIQUE(application_id, generation)` 不可能再撞。
#[test]
fn rollback_forward_roll_keeps_update_channel_open_across_hops() {
    let stack = TestStack::new(&label("rollback-forward-update"), 0x5A);
    let v1 = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let v2 = stack.verify_package(0x41, 2, key(0xF1), 2_000);
    let v3 = stack.verify_package(0x41, 3, key(0xF2), 2_500);
    let authority = open_authority(stack.root.root());
    let gen1 = installed(&authority, &stack.artifacts, v1.receipt_id, 0x01, 3_000);
    let gen2 = updated(
        &authority,
        &stack.artifacts,
        v1.package_id,
        v2.receipt_id,
        0x02,
        4_000,
    );
    disabled(&authority, v1.package_id, 0x0B, 5_000);

    let first = rolled_back(&authority, v1.package_id, 0x0A, 6_000);
    assert_eq!(first.from_generation, gen2.installation_generation);
    assert_eq!(first.to_generation.get(), 3);

    // The one-way gate is gone: a fresh update after the rollback commits
    // a receipt at generation 4 (the rewound-generation UNIQUE collision
    // the old semantics hit is impossible by construction now).
    let gen4 = updated(
        &authority,
        &stack.artifacts,
        v1.package_id,
        v3.receipt_id,
        0x03,
        7_000,
    );
    assert_eq!(gen4.installation_generation.get(), 4);
    assert_eq!(gen4.package_manifest_digest, v3.manifest_digest);

    // Second hop: disable + roll back again lands on generation 5 and
    // restores the generation-3 content (v1); the second disable takes a
    // fresh receipt at generation 4 (D2: no per-application PK collision).
    disabled(&authority, v1.package_id, 0x0C, 8_000);
    let second = rolled_back(&authority, v1.package_id, 0x0D, 9_000);
    assert_eq!(second.from_generation.get(), 4);
    assert_eq!(second.to_generation.get(), 5);
    let view = authority
        .inspect_application(v1.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(view.status, nlos_application::ApplicationStatus::Installed);
    assert_eq!(view.current_installation_generation.get(), 5);
    assert_eq!(view.package_manifest_digest, gen1.package_manifest_digest);

    // Dense, strictly monotonic history: five receipts, five generations.
    let listed = authority
        .list_installations(gen1.application_id)
        .expect("list");
    let generations = listed
        .iter()
        .map(|receipt| receipt.installation_generation.get())
        .collect::<Vec<_>>();
    assert_eq!(generations, vec![1, 2, 3, 4, 5]);
    let digests = listed
        .iter()
        .map(|receipt| receipt.package_manifest_digest)
        .collect::<Vec<_>>();
    assert_eq!(
        digests,
        vec![
            v1.manifest_digest,
            v2.manifest_digest,
            v1.manifest_digest,
            v3.manifest_digest,
            v1.manifest_digest,
        ]
    );
    assert_counts(&stack, 1, 5);
    assert_disable_counts(&stack, 2);
    assert_rollback_counts(&stack, 2);
}

/// 回滚 → disable → 再回滚（D2 核心）：第二次 disable 落在新代际的新
/// receipt，不再撞 `application_disable_receipts` 的主键；第二次回滚恢复
/// 当前代之前最近一代的内容。
#[test]
fn rollback_disable_rollback_cycle_never_collides() {
    let stack = TestStack::new(&label("rollback-disable-cycle"), 0x5B);
    let v1 = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let v2 = stack.verify_package(0x41, 2, key(0xF1), 2_000);
    let authority = open_authority(stack.root.root());
    installed(&authority, &stack.artifacts, v1.receipt_id, 0x01, 3_000);
    updated(
        &authority,
        &stack.artifacts,
        v1.package_id,
        v2.receipt_id,
        0x02,
        4_000,
    );
    disabled(&authority, v1.package_id, 0x0B, 5_000);
    let first = rolled_back(&authority, v1.package_id, 0x0A, 6_000);
    assert_eq!(first.to_generation.get(), 3);

    // Disable at the forward-roll generation: a second durable disable
    // receipt for the same application (pre-fix: raw PK collision).
    let disable = disabled(&authority, v1.package_id, 0x0C, 7_000);
    assert_eq!(disable.application_generation.get(), 3);

    // Roll back again: generation 4 restores the generation-2 content (v2)
    // — the receipt of the most recent generation before the current one.
    let second = rolled_back(&authority, v1.package_id, 0x0D, 8_000);
    assert_eq!(second.from_generation.get(), 3);
    assert_eq!(second.to_generation.get(), 4);
    let view = authority
        .inspect_application(v1.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(view.current_installation_generation.get(), 4);
    assert_eq!(view.package_manifest_digest, v2.manifest_digest);

    assert_counts(&stack, 1, 4);
    assert_disable_counts(&stack, 2);
    assert_rollback_counts(&stack, 2);
}

/// 回滚 → uninstall → 再回滚 → 终态 uninstall（D2）：uninstall receipt 按
/// (application, generation) 记账，可重复落账；终态后同 key 重放收敛。
#[test]
fn rollback_uninstall_cycles_end_terminal() {
    let stack = TestStack::new(&label("rollback-uninstall-cycle"), 0x5C);
    let v1 = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let v2 = stack.verify_package(0x41, 2, key(0xF1), 2_000);
    let authority = open_authority(stack.root.root());
    installed(&authority, &stack.artifacts, v1.receipt_id, 0x01, 3_000);
    updated(
        &authority,
        &stack.artifacts,
        v1.package_id,
        v2.receipt_id,
        0x02,
        4_000,
    );
    let uninstall_one = uninstalled(&authority, v1.package_id, 0x0B, 5_000);
    assert_eq!(uninstall_one.application_generation.get(), 2);

    let first = rolled_back(&authority, v1.package_id, 0x0A, 6_000);
    assert_eq!(first.to_generation.get(), 3);
    let uninstall_two = uninstalled(&authority, v1.package_id, 0x0C, 7_000);
    assert_eq!(uninstall_two.application_generation.get(), 3);

    let second = rolled_back(&authority, v1.package_id, 0x0D, 8_000);
    assert_eq!(second.from_generation.get(), 3);
    assert_eq!(second.to_generation.get(), 4);
    let uninstall_three = uninstalled(&authority, v1.package_id, 0x0E, 9_000);
    assert_eq!(uninstall_three.application_generation.get(), 4);

    let view = authority
        .inspect_application(v1.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(
        view.status,
        nlos_application::ApplicationStatus::Uninstalled
    );
    assert_eq!(view.current_installation_generation.get(), 4);
    assert_eq!(view.package_manifest_digest, v2.manifest_digest);

    // Terminal-state replay converges on the original receipt.
    assert_eq!(
        uninstall_replayed(&authority, v1.package_id, 0x0E, 9_000),
        uninstall_three
    );
    assert_counts(&stack, 1, 4);
    assert_uninstall_counts(&stack, 3);
    assert_rollback_counts(&stack, 2);
}

/// 回执链可审计：每次回滚 = 恰好一条新 installation receipt（内容逐字段
/// 继承被恢复代，installation id 派生自 (key, application, 新代)）+ 一条
/// rollback receipt（target 指向新代）；同 key 重放不新增任何行。
#[test]
fn rollback_receipt_chain_is_auditable() {
    let stack = TestStack::new(&label("rollback-audit"), 0x5D);
    let v1 = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let v2 = stack.verify_package(0x41, 2, key(0xF1), 2_000);
    let authority = open_authority(stack.root.root());
    let gen1 = installed(&authority, &stack.artifacts, v1.receipt_id, 0x01, 3_000);
    updated(
        &authority,
        &stack.artifacts,
        v1.package_id,
        v2.receipt_id,
        0x02,
        4_000,
    );
    disabled(&authority, v1.package_id, 0x0B, 5_000);
    let first = rolled_back(&authority, v1.package_id, 0x0A, 6_000);
    disabled(&authority, v1.package_id, 0x0C, 7_000);
    let second = rolled_back(&authority, v1.package_id, 0x0D, 8_000);

    assert_eq!(first.from_generation.get(), 2);
    assert_eq!(first.to_generation.get(), 3);
    assert_eq!(second.from_generation.get(), 3);
    assert_eq!(second.to_generation.get(), 4);

    let listed = authority
        .list_installations(gen1.application_id)
        .expect("list");
    assert_eq!(listed.len(), 4);
    let generations = listed
        .iter()
        .map(|receipt| receipt.installation_generation.get())
        .collect::<Vec<_>>();
    assert_eq!(generations, vec![1, 2, 3, 4]);

    // Each forward-roll receipt inherits the restored generation's
    // content bitwise, under its own installation id, key, and timestamp.
    let restored = [(&listed[2], &listed[0]), (&listed[3], &listed[1])];
    for (rolled, source) in restored {
        assert_eq!(rolled.package_id, source.package_id);
        assert_eq!(
            rolled.package_manifest_digest,
            source.package_manifest_digest
        );
        assert_eq!(rolled.package_version, source.package_version);
        assert_eq!(rolled.entry_count, source.entry_count);
        assert_eq!(
            rolled.package_verification_receipt_id,
            source.package_verification_receipt_id
        );
        assert_eq!(rolled.installer_principal, source.installer_principal);
        assert_ne!(rolled.installation_id, source.installation_id);
        assert_ne!(rolled.idempotency_key, source.idempotency_key);
    }
    assert_eq!(
        listed[2].installation_id,
        derive_installation_id(
            key(0x0A),
            gen1.application_id,
            listed[2].installation_generation
        )
    );
    assert_eq!(
        listed[3].installation_id,
        derive_installation_id(
            key(0x0D),
            gen1.application_id,
            listed[3].installation_generation
        )
    );
    assert_eq!(listed[2].installed_at_ms, 6_000);
    assert_eq!(listed[3].installed_at_ms, 8_000);

    // One rollback receipt per command, each target naming the new
    // generation; replay adds no rows anywhere.
    assert_eq!(
        authority
            .inspect_rollback_receipt(key(0x0A))
            .expect("inspect")
            .expect("exists"),
        first
    );
    assert_eq!(
        authority
            .inspect_rollback_receipt(key(0x0D))
            .expect("inspect")
            .expect("exists"),
        second
    );
    assert_eq!(
        rollback_replayed(&authority, v1.package_id, 0x0A, 6_000),
        first
    );
    assert_eq!(
        rollback_replayed(&authority, v1.package_id, 0x0D, 8_000),
        second
    );
    assert_counts(&stack, 1, 4);
    assert_disable_counts(&stack, 2);
    assert_rollback_counts(&stack, 2);
}

/// 回滚 key 与既有安装命令 key 冲突：typed IdempotencyConflict、零
/// durable 变化（不是裸 `SQLite` `UNIQUE` 错误）。
#[test]
fn rollback_key_bound_to_an_installation_command_conflicts_typed() {
    let stack = TestStack::new(&label("rollback-key-conflict"), 0x5E);
    let v1 = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let v2 = stack.verify_package(0x41, 2, key(0xF1), 2_000);
    let authority = open_authority(stack.root.root());
    installed(&authority, &stack.artifacts, v1.receipt_id, 0x01, 3_000);
    updated(
        &authority,
        &stack.artifacts,
        v1.package_id,
        v2.receipt_id,
        0x02,
        4_000,
    );
    disabled(&authority, v1.package_id, 0x0B, 5_000);
    let error = authority
        .rollback_application(RollbackApplicationRequest {
            package_id: v1.package_id,
            idempotency_key: key(0x01),
            rollback_at_ms: 6_000,
        })
        .expect_err("the install command's key is already bound");
    assert!(matches!(
        error,
        ApplicationAuthorityError::IdempotencyConflict
    ));
    assert_counts(&stack, 1, 2);
    assert_rollback_counts(&stack, 0);
}

/// Builds, by hand, the v8-era database the upgrade test upgrades: one
/// application with receipts at generations 1..3 that then lived through
/// the legacy generation step-back (3 → 2, disabled first), leaving the
/// rewound row below its own receipt history (the durable D1 residue).
// One auditable block contains the complete v8-era DDL fixture.
#[allow(clippy::too_many_lines)]
fn seed_v8_database_with_legacy_rollback(
    root: &std::path::Path,
    application_id: nlos_types::ApplicationId,
    package_id: nlos_types::PackageId,
) {
    let d1 = [0x11; 32];
    let d2 = [0x12; 32];
    let d3 = [0x13; 32];
    {
        let raw = Connection::open(authority_database(root)).expect("open raw");
        raw.execute_batch(
            "CREATE TABLE applications (
                application_id BLOB PRIMARY KEY NOT NULL CHECK(length(application_id)=16),
                package_id BLOB NOT NULL UNIQUE CHECK(length(package_id)=16),
                package_manifest_digest BLOB NOT NULL CHECK(length(package_manifest_digest)=32),
                current_installation_generation INTEGER NOT NULL
                    CHECK(current_installation_generation >= 1),
                status INTEGER NOT NULL CHECK(status IN (1, 2, 3)),
                created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
                updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms)
            ) STRICT;
            CREATE TRIGGER applications_monotonic_generation
            BEFORE UPDATE ON applications
            WHEN NEW.current_installation_generation < OLD.current_installation_generation
                AND NOT (
                    NEW.status = 1
                    AND OLD.status IN (2, 3)
                    AND NEW.current_installation_generation
                        = OLD.current_installation_generation - 1
                )
            BEGIN
                SELECT RAISE(ABORT, 'application installation generation is monotonic');
            END;
            CREATE TRIGGER applications_frozen_identity
            BEFORE UPDATE ON applications
            WHEN NEW.application_id != OLD.application_id OR NEW.package_id != OLD.package_id
            BEGIN
                SELECT RAISE(ABORT, 'application identity is frozen');
            END;
            CREATE TRIGGER applications_legal_status_transition
            BEFORE UPDATE ON applications
            WHEN (OLD.status = 3
                    AND NOT (
                        NEW.status = 1
                        AND NEW.current_installation_generation
                            = OLD.current_installation_generation - 1
                    ))
                OR (OLD.status = 2 AND NEW.status NOT IN (1, 3))
                OR (OLD.status = 2 AND NEW.status = 1
                    AND NEW.current_installation_generation
                        != OLD.current_installation_generation - 1)
                OR (OLD.status = 1 AND NEW.status = 1
                    AND NEW.current_installation_generation
                        <= OLD.current_installation_generation)
                OR NEW.status NOT IN (1, 2, 3)
            BEGIN
                SELECT RAISE(ABORT, 'application status transition is not legal');
            END;
            CREATE TRIGGER applications_no_delete
            BEFORE DELETE ON applications BEGIN
                SELECT RAISE(ABORT, 'application row is durable');
            END;
            CREATE TABLE installation_receipts (
                installation_id BLOB PRIMARY KEY NOT NULL CHECK(length(installation_id)=16),
                idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key)=16),
                application_id BLOB NOT NULL CHECK(length(application_id)=16),
                installation_generation INTEGER NOT NULL CHECK(installation_generation >= 1),
                package_id BLOB NOT NULL CHECK(length(package_id)=16),
                package_manifest_digest BLOB NOT NULL CHECK(length(package_manifest_digest)=32),
                package_version INTEGER NOT NULL CHECK(package_version >= 0),
                entry_count INTEGER NOT NULL CHECK(entry_count >= 1),
                package_verification_receipt_id BLOB NOT NULL
                    CHECK(length(package_verification_receipt_id)=16),
                installer_principal BLOB NOT NULL CHECK(length(installer_principal)=16),
                installed_at_ms INTEGER NOT NULL CHECK(installed_at_ms >= 0),
                UNIQUE(application_id, installation_generation)
            ) STRICT;
            CREATE TRIGGER installation_receipts_immutable_update
            BEFORE UPDATE ON installation_receipts BEGIN
                SELECT RAISE(ABORT, 'installation receipt is immutable');
            END;
            CREATE TRIGGER installation_receipts_no_delete
            BEFORE DELETE ON installation_receipts BEGIN
                SELECT RAISE(ABORT, 'installation receipt is durable');
            END;
            CREATE TRIGGER installation_receipts_generation_bounds
            AFTER INSERT ON installation_receipts
            WHEN NEW.installation_generation != (
                SELECT current_installation_generation FROM applications
                WHERE application_id = NEW.application_id
            )
            BEGIN
                SELECT RAISE(ABORT, 'installation receipt exceeds the application generation');
            END;
            CREATE TABLE application_disable_receipts (
                application_id BLOB PRIMARY KEY NOT NULL CHECK(length(application_id)=16),
                idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key)=16),
                application_generation INTEGER NOT NULL CHECK(application_generation >= 1),
                disabled_at_ms INTEGER NOT NULL CHECK(disabled_at_ms >= 0)
            ) STRICT;
            CREATE TRIGGER application_disable_receipts_state_bounds
            AFTER INSERT ON application_disable_receipts
            WHEN (SELECT status FROM applications
                  WHERE application_id = NEW.application_id) != 2
                OR NEW.application_generation != (
                    SELECT current_installation_generation FROM applications
                    WHERE application_id = NEW.application_id
                )
            BEGIN
                SELECT RAISE(ABORT, 'disable receipt bounds');
            END;
            CREATE TABLE application_uninstall_receipts (
                application_id BLOB PRIMARY KEY NOT NULL CHECK(length(application_id)=16),
                idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key)=16),
                application_generation INTEGER NOT NULL CHECK(application_generation >= 1),
                uninstalled_at_ms INTEGER NOT NULL CHECK(uninstalled_at_ms >= 0)
            ) STRICT;
            CREATE TABLE application_rollback_receipts (
                idempotency_key BLOB PRIMARY KEY NOT NULL CHECK(length(idempotency_key)=16),
                application_id BLOB NOT NULL CHECK(length(application_id)=16),
                from_generation INTEGER NOT NULL CHECK(from_generation >= 2),
                to_generation INTEGER NOT NULL
                    CHECK(to_generation >= 1 AND from_generation = to_generation + 1),
                rollback_at_ms INTEGER NOT NULL CHECK(rollback_at_ms >= 0)
            ) STRICT;
            CREATE TRIGGER application_rollback_receipts_state_bounds
            AFTER INSERT ON application_rollback_receipts
            WHEN (SELECT status FROM applications
                  WHERE application_id = NEW.application_id) != 1
                OR NEW.to_generation != (
                    SELECT current_installation_generation FROM applications
                    WHERE application_id = NEW.application_id
                )
            BEGIN
                SELECT RAISE(ABORT, 'rollback receipt bounds');
            END;
            PRAGMA user_version=8;",
        )
        .expect("seed v8 schema");

        let app = application_id.as_bytes().as_slice();
        let pkg = package_id.as_bytes().as_slice();
        let receipt = |raw: &Connection,
                       installation: u8,
                       generation: i64,
                       digest: &[u8; 32],
                       version: i64,
                       at_ms: i64| {
            raw.execute(
                "INSERT INTO installation_receipts (
                    installation_id, idempotency_key, application_id,
                    installation_generation, package_id, package_manifest_digest,
                    package_version, entry_count, package_verification_receipt_id,
                    installer_principal, installed_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?9, ?10)",
                rusqlite::params![
                    [installation; 16].as_slice(),
                    [installation; 16].as_slice(),
                    app,
                    generation,
                    pkg,
                    digest.as_slice(),
                    version,
                    [0x99_u8; 16].as_slice(),
                    [0xC0_u8; 16].as_slice(),
                    at_ms,
                ],
            )
            .expect("seed receipt");
        };
        raw.execute(
            "INSERT INTO applications (
                application_id, package_id, package_manifest_digest,
                current_installation_generation, status, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, 1, 1, 1000, 2000)",
            rusqlite::params![app, pkg, d1.as_slice()],
        )
        .expect("seed application");
        receipt(&raw, 0xA1, 1, &d1, 1, 2_000);
        raw.execute(
            "UPDATE applications SET current_installation_generation = 2,
                 package_manifest_digest = ?1, updated_at_ms = 3000
             WHERE application_id = ?2",
            rusqlite::params![d2.as_slice(), app],
        )
        .expect("seed advance 2");
        receipt(&raw, 0xA2, 2, &d2, 2, 3_000);
        raw.execute(
            "UPDATE applications SET current_installation_generation = 3,
                 package_manifest_digest = ?1, updated_at_ms = 4000
             WHERE application_id = ?2",
            rusqlite::params![d3.as_slice(), app],
        )
        .expect("seed advance 3");
        receipt(&raw, 0xA3, 3, &d3, 3, 4_000);
        raw.execute(
            "UPDATE applications SET status = 2, updated_at_ms = 5000
             WHERE application_id = ?1",
            rusqlite::params![app],
        )
        .expect("seed disable");
        raw.execute(
            "INSERT INTO application_disable_receipts (
                application_id, idempotency_key, application_generation, disabled_at_ms
             ) VALUES (?1, ?2, 3, 5000)",
            rusqlite::params![app, [0x04_u8; 16].as_slice()],
        )
        .expect("seed disable receipt");
        // The legacy generation step-back: 3 -> 2 under the v4 semantics.
        raw.execute(
            "UPDATE applications SET status = 1, current_installation_generation = 2,
                 package_manifest_digest = ?1, updated_at_ms = 6000
             WHERE application_id = ?2",
            rusqlite::params![d2.as_slice(), app],
        )
        .expect("seed legacy rollback");
        raw.execute(
            "INSERT INTO application_rollback_receipts (
                idempotency_key, application_id, from_generation, to_generation,
                rollback_at_ms
             ) VALUES (?1, ?2, 3, 2, 6000)",
            rusqlite::params![[0x05_u8; 16].as_slice(), app],
        )
        .expect("seed legacy rollback receipt");
    }
}

/// v8 → v9 升级：手工构造经历过旧语义回退（3→2，receipt 1..3 已在）的
/// v8 数据库；升级后旧 receipt 逐字保留，偏斜行（row 代落后于 receipt 最大
/// 代）的 update 与 rollback 都跳到"最大 receipt 代 + 1"，旧 D1 残留不再
/// 以裸 UNIQUE 错误暴露；第二次 disable/回滚落在全新代际。
#[test]
fn v9_upgrade_preserves_legacy_rows_and_reopens_skewed_history() {
    let stack = TestStack::new(&label("v9-upgrade"), 0x5F);
    let package_id = nlos_types::PackageId::from_bytes([0x51; 16]);
    let application_id = derive_application_id(package_id);
    let d2 = [0x12; 32];
    let d3 = [0x13; 32];
    seed_v8_database_with_legacy_rollback(stack.root.root(), application_id, package_id);

    // Reopening runs the v9 migration; the legacy state survives bitwise.
    let authority = open_authority(stack.root.root());
    assert_eq!(
        raw_count(
            &authority_database(stack.root.root()),
            "PRAGMA user_version"
        ),
        9,
        "the v9 migration must have run"
    );
    let view = authority
        .inspect_application(package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(view.current_installation_generation.get(), 2);
    assert_eq!(
        view.package_manifest_digest,
        nlos_artifact::ContentDigest::from_bytes(d2)
    );
    let legacy = authority
        .inspect_rollback_receipt(key(0x05))
        .expect("inspect")
        .expect("legacy rollback receipt survives");
    assert_eq!(legacy.application_id, application_id);
    assert_eq!(legacy.from_generation.get(), 3);
    assert_eq!(legacy.to_generation.get(), 2);
    assert_eq!(legacy.rollback_at_ms, 6_000);
    assert_counts(&stack, 1, 3);
    assert_disable_counts(&stack, 1);
    assert_rollback_counts(&stack, 1);

    // Skew repaired forward: the update lands at max receipt generation + 1
    // (4), not the rewound row generation + 1 (3, already recorded).
    let target = stack.verify_package(0x51, 4, key(0xF0), 6_500);
    let update = updated(
        &authority,
        &stack.artifacts,
        package_id,
        target.receipt_id,
        0x06,
        7_000,
    );
    assert_eq!(update.installation_generation.get(), 4);
    assert_eq!(update.package_manifest_digest, target.manifest_digest);

    // The second disable and a rollback land on fresh generations with
    // fresh receipts (D2 on upgraded data).
    disabled(&authority, package_id, 0x07, 8_000);
    let rollback = rolled_back(&authority, package_id, 0x08, 9_000);
    assert_eq!(rollback.from_generation.get(), 4);
    assert_eq!(rollback.to_generation.get(), 5);
    let view = authority
        .inspect_application(package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(view.status, nlos_application::ApplicationStatus::Installed);
    assert_eq!(view.current_installation_generation.get(), 5);
    assert_eq!(
        view.package_manifest_digest,
        nlos_artifact::ContentDigest::from_bytes(d3),
        "the rollback restores the generation before the current one (d3)"
    );

    assert_counts(&stack, 1, 5);
    assert_disable_counts(&stack, 2);
    assert_rollback_counts(&stack, 2);
}

struct MockTaskProbe {
    count: u64,
}

impl ActiveTaskActivityProbe for MockTaskProbe {
    fn outstanding_task_count(&self, _package_id: PackageId) -> u64 {
        self.count
    }
}

#[test]
fn uninstall_with_active_tasks_is_refused_with_zero_state() {
    let stack = TestStack::new(&label("uninstall-active-tasks"), 0x4D);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());
    installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );
    let probe = MockTaskProbe { count: 2 };
    let error = authority
        .uninstall_application_with_activity_gate(
            UninstallApplicationRequest {
                package_id: verified.package_id,
                idempotency_key: key(0x0A),
                uninstalled_at_ms: 3_000,
            },
            &probe,
        )
        .expect_err("active tasks must block fresh uninstall");
    assert!(matches!(
        error,
        ApplicationAuthorityError::ApplicationActiveTasksRunning {
            package_id,
            active_task_count: 2,
        } if package_id == verified.package_id
    ));
    assert_uninstall_counts(&stack, 0);
    uninstalled(&authority, verified.package_id, 0x0A, 3_000);
}

#[test]
fn uninstall_replay_bypasses_active_task_gate() {
    let stack = TestStack::new(&label("uninstall-replay-active-tasks"), 0x4E);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());
    installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );
    let receipt = uninstalled(&authority, verified.package_id, 0x0A, 3_000);
    let probe = MockTaskProbe { count: 1 };
    let replay = authority
        .uninstall_application_with_activity_gate(
            UninstallApplicationRequest {
                package_id: verified.package_id,
                idempotency_key: key(0x0A),
                uninstalled_at_ms: 3_000,
            },
            &probe,
        )
        .expect("replay must succeed despite active tasks");
    assert!(matches!(
        replay,
        nlos_application::UninstallDecision::Replayed(r) if r == receipt
    ));
}

#[test]
fn rollback_with_active_tasks_is_refused_with_zero_state() {
    let stack = TestStack::new(&label("rollback-active-tasks"), 0x4F);
    let first = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let second = stack.verify_package(0x41, 2, key(0xF1), 2_000);
    let authority = open_authority(stack.root.root());
    installed(&authority, &stack.artifacts, first.receipt_id, 0x01, 2_000);
    updated(
        &authority,
        &stack.artifacts,
        first.package_id,
        second.receipt_id,
        0x02,
        3_000,
    );
    disabled(&authority, first.package_id, 0x0B, 4_000);
    let probe = MockTaskProbe { count: 3 };
    let error = authority
        .rollback_application_with_activity_gate(
            RollbackApplicationRequest {
                package_id: first.package_id,
                idempotency_key: key(0x0A),
                rollback_at_ms: 5_000,
            },
            &probe,
        )
        .expect_err("active tasks must block fresh rollback");
    assert!(matches!(
        error,
        ApplicationAuthorityError::ApplicationActiveTasksRunning {
            package_id,
            active_task_count: 3,
        } if package_id == first.package_id
    ));
    assert_rollback_counts(&stack, 0);
}

/// Real TaskAuthority-backed activity gate fixtures: a task authority
/// database inside the test stack root, plus small wrappers over the
/// public task APIs the gate consults (register → `Active`, cancel →
/// terminal `Cancelled`).
fn open_task_authority(stack: &TestStack) -> nlos_task::SqliteTaskAuthority {
    nlos_task::SqliteTaskAuthority::open(stack.root.root().join("task-authority.sqlite3"))
        .expect("open task authority")
}

fn registered_active_task(tasks: &nlos_task::SqliteTaskAuthority, id: TaskId) {
    match tasks.register_task(nlos_task::TaskSpec {
        task_id: id,
        task_generation: Generation::INITIAL,
        registered_at_ms: 1_000,
        application_id: None,
        plan_revision: None,
    }) {
        Ok(
            nlos_task::TaskRegistrationDecision::Created(created)
            | nlos_task::TaskRegistrationDecision::Existing(created),
        ) => {
            assert_eq!(created, id);
        }
        Err(error) => panic!("register task {id:?}: {error}"),
    }
}

fn cancelled_task(tasks: &nlos_task::SqliteTaskAuthority, id: TaskId) {
    let decision = tasks
        .cancel_task(nlos_task::CancelRequest {
            task_id: id,
            idempotency_key: IdempotencyKey::from_bytes([id.as_bytes()[0] ^ 0x5E; 16]),
            requested_at_ms: 2_000,
        })
        .expect("cancel task");
    assert!(matches!(
        decision,
        nlos_task::CancelDecision::Applied { .. }
    ));
}

fn tamper_task_state(stack: &TestStack, id: TaskId, state: i64) {
    let connection = Connection::open(stack.root.root().join("task-authority.sqlite3"))
        .expect("open raw task tamper");
    connection
        .execute(
            "UPDATE tasks SET task_state = ?1 WHERE task_id = ?2",
            rusqlite::params![state, id.as_bytes().as_slice()],
        )
        .expect("tamper task state");
}

#[test]
fn uninstall_task_activity_gate_opens_without_outstanding_tasks() {
    let stack = TestStack::new(&label("uninstall-task-activity-open"), 0x52);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());
    let tasks = open_task_authority(&stack);
    installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );

    // A registration-only task (no durable TaskAuthority row) is a
    // promise, not activity; a registered task that already reached its
    // terminal Cancelled state is not activity either. Both must leave
    // the real-query gate open — a registration counter would refuse.
    let principal = stack.identity.binding.principal_id;
    background_task_registered(
        &authority,
        verified.package_id,
        task_id(0xA1),
        principal,
        0x0B,
        3_000,
    );
    let cancelled_id = task_id(0xA2);
    background_task_registered(
        &authority,
        verified.package_id,
        cancelled_id,
        principal,
        0x0C,
        3_000,
    );
    registered_active_task(&tasks, cancelled_id);
    cancelled_task(&tasks, cancelled_id);

    let receipt = authority
        .uninstall_application_with_task_activity_gate(
            &tasks,
            UninstallApplicationRequest {
                package_id: verified.package_id,
                idempotency_key: key(0x0A),
                uninstalled_at_ms: 4_000,
            },
        )
        .expect("no outstanding task activity must allow uninstall");
    assert!(matches!(
        receipt,
        nlos_application::UninstallDecision::Uninstalled(_)
    ));
    assert_uninstall_counts(&stack, 1);
}

#[test]
fn uninstall_task_activity_gate_refuses_while_task_active_then_converges() {
    let stack = TestStack::new(&label("uninstall-task-activity-closed"), 0x53);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());
    let tasks = open_task_authority(&stack);
    installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );
    let active_id = task_id(0xA1);
    background_task_registered(
        &authority,
        verified.package_id,
        active_id,
        stack.identity.binding.principal_id,
        0x0B,
        3_000,
    );
    registered_active_task(&tasks, active_id);

    let error = authority
        .uninstall_application_with_task_activity_gate(
            &tasks,
            UninstallApplicationRequest {
                package_id: verified.package_id,
                idempotency_key: key(0x0A),
                uninstalled_at_ms: 4_000,
            },
        )
        .expect_err("one durable Active task must block fresh uninstall");
    assert!(matches!(
        error,
        ApplicationAuthorityError::ApplicationActiveTasksRunning {
            package_id,
            active_task_count: 1,
        } if package_id == verified.package_id
    ));
    assert_uninstall_counts(&stack, 0);
    let view = authority
        .inspect_application(verified.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(view.status, nlos_application::ApplicationStatus::Installed);

    // The gate is a live query: once the task reaches its terminal state,
    // the same fresh command converges on the next key.
    cancelled_task(&tasks, active_id);
    let receipt = authority
        .uninstall_application_with_task_activity_gate(
            &tasks,
            UninstallApplicationRequest {
                package_id: verified.package_id,
                idempotency_key: key(0x0D),
                uninstalled_at_ms: 5_000,
            },
        )
        .expect("terminal task activity must re-open the gate");
    assert!(matches!(
        receipt,
        nlos_application::UninstallDecision::Uninstalled(_)
    ));
    assert_uninstall_counts(&stack, 1);
}

#[test]
fn uninstall_task_activity_gate_fails_closed_on_task_store_error() {
    let stack = TestStack::new(&label("uninstall-task-activity-error"), 0x54);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());
    let tasks = open_task_authority(&stack);
    installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );
    let active_id = task_id(0xA1);
    background_task_registered(
        &authority,
        verified.package_id,
        active_id,
        stack.identity.binding.principal_id,
        0x0B,
        3_000,
    );
    registered_active_task(&tasks, active_id);
    tamper_task_state(&stack, active_id, 7);

    let error = authority
        .uninstall_application_with_task_activity_gate(
            &tasks,
            UninstallApplicationRequest {
                package_id: verified.package_id,
                idempotency_key: key(0x0A),
                uninstalled_at_ms: 4_000,
            },
        )
        .expect_err("an unreadable activity query must refuse fail-closed");
    assert!(matches!(
        error,
        ApplicationAuthorityError::TaskActivityQueryFailed {
            package_id,
            ..
        } if package_id == verified.package_id
    ));
    assert_uninstall_counts(&stack, 0);
    let view = authority
        .inspect_application(verified.package_id)
        .expect("inspect")
        .expect("exists");
    assert_eq!(view.status, nlos_application::ApplicationStatus::Installed);

    // The refusal poisoned nothing: once the stored state decodes again
    // (healed straight to the terminal Cancelled code, so the healed
    // query reports no outstanding activity), the same fresh command
    // converges.
    tamper_task_state(&stack, active_id, 1);
    let receipt = authority
        .uninstall_application_with_task_activity_gate(
            &tasks,
            UninstallApplicationRequest {
                package_id: verified.package_id,
                idempotency_key: key(0x0D),
                uninstalled_at_ms: 5_000,
            },
        )
        .expect("healed activity query must allow uninstall");
    assert!(matches!(
        receipt,
        nlos_application::UninstallDecision::Uninstalled(_)
    ));
}

#[test]
fn uninstall_task_activity_gate_replay_bypasses_task_activity_query() {
    let stack = TestStack::new(&label("uninstall-task-activity-replay"), 0x55);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());
    let tasks = open_task_authority(&stack);
    installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );
    let later_active = task_id(0xA1);
    background_task_registered(
        &authority,
        verified.package_id,
        later_active,
        stack.identity.binding.principal_id,
        0x0B,
        3_000,
    );

    // Gate consult happens while the task has no durable row: uninstall
    // commits its receipt under key 0x0A.
    let receipt = authority
        .uninstall_application_with_task_activity_gate(
            &tasks,
            UninstallApplicationRequest {
                package_id: verified.package_id,
                idempotency_key: key(0x0A),
                uninstalled_at_ms: 4_000,
            },
        )
        .expect("no durable task row yet: uninstall proceeds");
    let original = match receipt {
        nlos_application::UninstallDecision::Uninstalled(original) => original,
        other @ nlos_application::UninstallDecision::Replayed(_) => {
            panic!("fresh key must uninstall, got {other:?}")
        }
    };

    // Activity (and even an undecodable query) appears after the fact:
    // the durable receipt replays byte-equal without consulting the
    // task authority at all.
    registered_active_task(&tasks, later_active);
    tamper_task_state(&stack, later_active, 7);
    let replay = authority
        .uninstall_application_with_task_activity_gate(
            &tasks,
            UninstallApplicationRequest {
                package_id: verified.package_id,
                idempotency_key: key(0x0A),
                uninstalled_at_ms: 4_000,
            },
        )
        .expect("replay must bypass the activity query");
    assert!(matches!(
        replay,
        nlos_application::UninstallDecision::Replayed(r) if r == original
    ));
}

#[test]
fn rollback_task_activity_gate_refuses_then_converges() {
    let stack = TestStack::new(&label("rollback-task-activity"), 0x56);
    let first = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let second = stack.verify_package(0x41, 2, key(0xF1), 2_000);
    let authority = open_authority(stack.root.root());
    let tasks = open_task_authority(&stack);
    installed(&authority, &stack.artifacts, first.receipt_id, 0x01, 2_000);
    updated(
        &authority,
        &stack.artifacts,
        first.package_id,
        second.receipt_id,
        0x02,
        3_000,
    );
    // Background-task registration requires an installed application, so
    // the registration lands before the disable that makes rollback
    // admissible.
    let active_id = task_id(0xA1);
    background_task_registered(
        &authority,
        first.package_id,
        active_id,
        stack.identity.binding.principal_id,
        0x0C,
        4_000,
    );
    registered_active_task(&tasks, active_id);
    disabled(&authority, first.package_id, 0x0B, 5_000);

    let error = authority
        .rollback_application_with_task_activity_gate(
            &tasks,
            RollbackApplicationRequest {
                package_id: first.package_id,
                idempotency_key: key(0x0A),
                rollback_at_ms: 6_000,
            },
        )
        .expect_err("one durable Active task must block fresh rollback");
    assert!(matches!(
        error,
        ApplicationAuthorityError::ApplicationActiveTasksRunning {
            package_id,
            active_task_count: 1,
        } if package_id == first.package_id
    ));
    assert_rollback_counts(&stack, 0);

    cancelled_task(&tasks, active_id);
    let receipt = authority
        .rollback_application_with_task_activity_gate(
            &tasks,
            RollbackApplicationRequest {
                package_id: first.package_id,
                idempotency_key: key(0x0D),
                rollback_at_ms: 7_000,
            },
        )
        .expect("terminal task activity must re-open the rollback gate");
    match receipt {
        nlos_application::RollbackDecision::RolledBack(receipt) => {
            assert_eq!(
                receipt.from_generation,
                Generation::INITIAL.checked_next().unwrap()
            );
            assert_eq!(receipt.to_generation.get(), 3);
        }
        other @ nlos_application::RollbackDecision::Replayed(_) => {
            panic!("fresh key must roll back, got {other:?}")
        }
    }
}

#[test]
fn background_task_registration_replays_idempotently() {
    let stack = TestStack::new(&label("bg-task-register"), 0x50);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());
    let install = installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );
    let principal = stack.identity.binding.principal_id;
    let receipt = background_task_registered(
        &authority,
        verified.package_id,
        task_id(0xA1),
        principal,
        0x0A,
        3_000,
    );
    assert_eq!(receipt.application_id, install.application_id);
    assert_eq!(receipt.application_generation, Generation::INITIAL);
    let listed = authority
        .inspect_background_tasks(verified.package_id)
        .expect("inspect");
    assert_eq!(listed, vec![receipt.clone()]);
    let replay = background_task_registration_replayed(
        &authority,
        verified.package_id,
        task_id(0xA1),
        principal,
        0x0A,
        3_000,
    );
    assert_eq!(replay, receipt);
    assert_background_task_registration_counts(&stack, 1);
}

#[test]
fn background_task_registration_refusals_are_typed_and_leave_zero_state() {
    let stack = TestStack::new(&label("bg-task-refusals"), 0x51);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());
    installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );
    let principal = stack.identity.binding.principal_id;
    let request = RegisterBackgroundTaskRequest {
        package_id: verified.package_id,
        task_id: task_id(0xA1),
        registrant_principal: principal,
        idempotency_key: key(0x0A),
        registered_at_ms: 3_000,
    };
    assert!(matches!(
        authority
            .register_background_task(RegisterBackgroundTaskRequest {
                package_id: PackageId::from_bytes([0xEE; 16]),
                ..request
            })
            .expect_err("unknown"),
        ApplicationAuthorityError::ApplicationNotFound { .. }
    ));
    disabled(&authority, verified.package_id, 0x0B, 4_000);
    assert!(matches!(
        authority
            .register_background_task(request)
            .expect_err("disabled"),
        ApplicationAuthorityError::ApplicationDisabled { .. }
    ));
    assert_background_task_registration_counts(&stack, 0);
}

#[test]
fn background_task_registration_duplicate_and_conflict_refusals() {
    let stack = TestStack::new(&label("bg-task-conflicts"), 0x52);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());
    installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );
    let principal = stack.identity.binding.principal_id;
    background_task_registered(
        &authority,
        verified.package_id,
        task_id(0xA1),
        principal,
        0x0A,
        3_000,
    );
    assert!(matches!(
        authority
            .register_background_task(RegisterBackgroundTaskRequest {
                package_id: verified.package_id,
                task_id: task_id(0xA1),
                registrant_principal: principal,
                idempotency_key: key(0x0B),
                registered_at_ms: 3_500,
            })
            .expect_err("duplicate"),
        ApplicationAuthorityError::BackgroundTaskAlreadyRegistered { .. }
    ));
    assert!(matches!(
        authority
            .register_background_task(RegisterBackgroundTaskRequest {
                package_id: verified.package_id,
                task_id: task_id(0xA2),
                registrant_principal: principal,
                idempotency_key: key(0x0C),
                registered_at_ms: 1_999,
            })
            .expect_err("too early"),
        ApplicationAuthorityError::RegistrationPrecedesLastUpdate { .. }
    ));
    assert_background_task_registration_counts(&stack, 1);
}

#[test]
fn process_binding_registration_replays_idempotently() {
    let stack = TestStack::new(&label("proc-bind-register"), 0x60);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());
    let install = installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );
    let principal = stack.identity.binding.principal_id;
    let receipt = process_binding_registered(
        &authority,
        verified.package_id,
        process_id(0xB1),
        principal,
        0x0A,
        3_000,
    );
    assert_eq!(receipt.application_id, install.application_id);
    assert_eq!(receipt.application_generation, Generation::INITIAL);
    let listed = authority
        .inspect_process_bindings(verified.package_id)
        .expect("inspect");
    assert_eq!(listed, vec![receipt.clone()]);
    let replay = process_binding_registration_replayed(
        &authority,
        verified.package_id,
        process_id(0xB1),
        principal,
        0x0A,
        3_000,
    );
    assert_eq!(replay, receipt);
    assert_process_binding_counts(&stack, 1);
}

#[test]
fn process_binding_registration_refusals_are_typed_and_leave_zero_state() {
    let stack = TestStack::new(&label("proc-bind-refusals"), 0x61);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());
    installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );
    let principal = stack.identity.binding.principal_id;
    let request = RegisterProcessBindingRequest {
        package_id: verified.package_id,
        process_id: process_id(0xB1),
        registrant_principal: principal,
        idempotency_key: key(0x0A),
        registered_at_ms: 3_000,
    };
    assert!(matches!(
        authority
            .register_process_binding(RegisterProcessBindingRequest {
                package_id: PackageId::from_bytes([0xEE; 16]),
                ..request
            })
            .expect_err("unknown"),
        ApplicationAuthorityError::ApplicationNotFound { .. }
    ));
    disabled(&authority, verified.package_id, 0x0B, 4_000);
    assert!(matches!(
        authority
            .register_process_binding(request)
            .expect_err("disabled"),
        ApplicationAuthorityError::ApplicationDisabled { .. }
    ));
    assert_process_binding_counts(&stack, 0);
}

#[test]
fn process_binding_registration_duplicate_and_conflict_refusals() {
    let stack = TestStack::new(&label("proc-bind-conflicts"), 0x62);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());
    installed(
        &authority,
        &stack.artifacts,
        verified.receipt_id,
        0x01,
        2_000,
    );
    let principal = stack.identity.binding.principal_id;
    process_binding_registered(
        &authority,
        verified.package_id,
        process_id(0xB1),
        principal,
        0x0A,
        3_000,
    );
    assert!(matches!(
        authority
            .register_process_binding(RegisterProcessBindingRequest {
                package_id: verified.package_id,
                process_id: process_id(0xB1),
                registrant_principal: principal,
                idempotency_key: key(0x0B),
                registered_at_ms: 3_500,
            })
            .expect_err("duplicate"),
        ApplicationAuthorityError::ProcessAlreadyRegistered { .. }
    ));
    assert!(matches!(
        authority
            .register_process_binding(RegisterProcessBindingRequest {
                package_id: verified.package_id,
                process_id: process_id(0xB2),
                registrant_principal: principal,
                idempotency_key: key(0x0C),
                registered_at_ms: 1_999,
            })
            .expect_err("too early"),
        ApplicationAuthorityError::RegistrationPrecedesLastUpdate { .. }
    ));
    assert_process_binding_counts(&stack, 1);
}
