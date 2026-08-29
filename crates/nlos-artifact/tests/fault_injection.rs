//! B-ARTIFACT-001 fault-injection: durability invariants of `ArtifactStore`
//! under the `nlos-store-fault` VFS plus kill-9 child processes, mirroring
//! the established `nlos-task` harness.
//!
//! **Crash semantics disclaimer**: kill-9 simulates a *process* crash (the
//! OS page cache survives); machine power loss is covered by
//! `FaultMode::PowerLossAfter`. Children synchronize through piped `READY`
//! markers, never sleeps. The fault state is process-global, so every test
//! in this binary holds `FAULT_LOCK` for its entire duration.
//!
//! The VFS shim intercepts only `SQLite` I/O; blob writes are ordinary
//! filesystem I/O. Metadata-phase injection therefore models the crash
//! window *after* the blob commit: the blob is durable, the metadata
//! transaction fails or vanishes, and no phantom revision may appear.

use std::error::Error as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_artifact::{
    ArtifactError, ArtifactStore, ContentDigest, PackageEntryRole, PackageVerificationDecision,
    PutRevisionDecision, VerifyPackageRequest,
};
use nlos_store_fault::{FaultCode, FaultMode};
use support::{
    TestStoreDir, artifact_id, artifact_spec, bytes, entry, manifest, put, sign_package,
    test_identity,
};

mod support;

const VFS_NAME: &str = "nlos-artifact-fault";

static FAULT_LOCK: Mutex<()> = Mutex::new(());

fn fault_lock() -> MutexGuard<'static, ()> {
    FAULT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn open_shim(root: &Path) -> ArtifactStore {
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
    ArtifactStore::open_with_vfs(root, Some(VFS_NAME)).expect("open via fault vfs")
}

fn error_chain(error: &ArtifactError) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(" <- ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn assert_integrity(root: &Path) {
    let connection =
        rusqlite::Connection::open(root.join("metadata.db")).expect("open for integrity check");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run integrity_check");
    assert_eq!(result, "ok", "integrity_check must pass");
}

// ---------------------------------------------------------------------------
// kill-9 child-process harness (current_exe + env var + READY markers)
// ---------------------------------------------------------------------------

fn spawn_child(scenario: &str, root: &Path) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_child_helper", "--nocapture"])
        .env("NLOS_ARTIFACT_CRASH_CHILD_SCENARIO", scenario)
        .env("NLOS_ARTIFACT_CRASH_CHILD_ROOT", root)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn crash child")
}

fn await_marker(child: &mut Child) {
    let stdout = child.stdout.take().expect("piped stdout");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // The libtest harness prints its own banner lines before the
        // helper's marker; scan until the marker (or EOF on early death).
        let mut lines = BufReader::new(stdout).lines();
        let mut seen = false;
        for line in lines.by_ref() {
            match line {
                Ok(line) if line.starts_with("READY") => {
                    seen = true;
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = sender.send(Err(error.to_string()));
                    return;
                }
            }
        }
        let _ = sender.send(if seen {
            Ok(())
        } else {
            Err("child exited without READY".to_string())
        });
    });
    match receiver.recv_timeout(Duration::from_mins(1)) {
        Ok(Ok(())) => {}
        other => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not report READY: {other:?}");
        }
    }
}

fn kill_and_reap(child: &mut Child) {
    child.kill().expect("force-terminate child");
    let status = child.wait().expect("wait child");
    assert!(
        !status.success(),
        "killed child must not exit cleanly: {status}"
    );
}

fn announce(marker: &str) {
    println!("{marker}");
    std::io::stdout().flush().expect("flush marker");
}

/// Child entry point. Runs only when spawned by a parent test with the
/// scenario environment set; a no-op in the normal test run.
#[test]
fn crash_child_helper() {
    let (Ok(scenario), Ok(root)) = (
        std::env::var("NLOS_ARTIFACT_CRASH_CHILD_SCENARIO"),
        std::env::var("NLOS_ARTIFACT_CRASH_CHILD_ROOT"),
    ) else {
        return;
    };
    let root = PathBuf::from(root);
    match scenario.as_str() {
        "commit-then-park" => {
            let store = ArtifactStore::open(&root).expect("open");
            store.create_artifact(artifact_spec(0x30)).expect("create");
            store
                .put_revision(put(artifact_id(0x30), 0, &bytes(0xc1, 512)))
                .expect("put");
            announce("READY");
            let _keeper = store;
            loop {
                std::thread::park();
            }
        }
        "mid-metadata-tx" => {
            let store = ArtifactStore::open(&root).expect("open");
            store.create_artifact(artifact_spec(0x31)).expect("create");
            // Simulate the middle of the metadata phase: a writer
            // transaction has dirtied the durable head row but has not
            // committed when the process dies.
            let raw = rusqlite::Connection::open(root.join("metadata.db")).expect("raw");
            raw.execute_batch("BEGIN IMMEDIATE").expect("begin mid-tx");
            raw.execute(
                "UPDATE artifacts SET created_at_ms = created_at_ms + 100",
                [],
            )
            .expect("mid-tx write");
            announce("READY");
            let _keepers = (store, raw);
            loop {
                std::thread::park();
            }
        }
        "plant-orphan-blob" => {
            // Model the window "after rename, before metadata commit": the
            // blob is fully durable at its content address, but no revision
            // row references it when the process dies.
            let store = ArtifactStore::open(&root).expect("open");
            store.create_artifact(artifact_spec(0x32)).expect("create");
            let payload = bytes(0xc2, 512);
            let digest = ContentDigest::of_bytes(&payload);
            let hex = digest.to_hex();
            let shard = root.join("artifacts/blobs").join(&hex[..2]);
            fs::create_dir_all(&shard).expect("shard dir");
            let file = fs::File::create(shard.join(&hex)).expect("create blob");
            file.sync_all().expect("sync blob");
            announce("READY");
            let _keeper = store;
            loop {
                std::thread::park();
            }
        }
        "plant-orphan-tmp" => {
            // Model the window "before rename": a partial tmp file exists,
            // nothing else.
            let store = ArtifactStore::open(&root).expect("open");
            store.create_artifact(artifact_spec(0x33)).expect("create");
            fs::write(root.join("artifacts/tmp/partial.0.tmp"), b"half").expect("plant tmp");
            announce("READY");
            let _keeper = store;
            loop {
                std::thread::park();
            }
        }
        other => panic!("unknown crash child scenario {other}"),
    }
}

// ---------------------------------------------------------------------------
// Row 1: hard I/O error during the metadata commit — no half state
// ---------------------------------------------------------------------------

/// 元数据提交期硬 I/O 错误：blob 已持久化（提交协议第一阶段先于元数据），
/// `FailWritesAfter { 0, IoErr }` 使元数据事务显式失败；重开后无幻影
/// revision，blob 被 recover 识别为孤儿（列出、不删除）；disarm 后同一
/// 请求成功提交。此行同时覆盖“rename 后、metadata commit 前崩溃”窗口。
#[test]
fn fault_io_error_during_metadata_commit_leaves_orphan_blob_no_revision() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let directory = TestStoreDir::new("ioerr-metadata");
    let store = open_shim(directory.root());
    store.create_artifact(artifact_spec(0x40)).expect("create");

    let payload = bytes(0xd1, 512);
    let digest = ContentDigest::of_bytes(&payload);
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = store
        .put_revision(put(artifact_id(0x40), 0, &payload))
        .expect_err("metadata commit must fail under injected I/O error");
    assert!(
        matches!(error, ArtifactError::Sqlite(_)),
        "expected a storage error, got {error}"
    );
    let chain = error_chain(&error).to_lowercase();
    assert!(
        chain.contains("i/o") || chain.contains("ioerr"),
        "error chain must name the I/O condition, got: {chain}"
    );
    assert!(nlos_store_fault::writes_observed() > 0);
    nlos_store_fault::disarm();

    // No half state: the blob is durable (phase 1), but no revision or head
    // may reference it (phase 2 never committed).
    assert!(directory.artifact_blob(digest).is_file());
    assert_eq!(store.resolve_head(artifact_id(0x40)).expect("head"), None);
    assert!(
        store
            .list_revisions(artifact_id(0x40))
            .expect("list")
            .is_empty()
    );
    assert!(matches!(
        store.get_revision(artifact_id(0x40), 1),
        Err(ArtifactError::RevisionNotFound { .. })
    ));
    let report = store.recover().expect("recover");
    assert_eq!(report.orphan_blobs, vec![digest]);
    assert!(report.missing_blobs.is_empty());

    // After the fault clears the same request commits cleanly; the existing
    // blob makes phase 1 an idempotent no-op.
    let decision = store
        .put_revision(put(artifact_id(0x40), 0, &payload))
        .expect("put after disarm");
    assert!(matches!(decision, PutRevisionDecision::Committed(_)));
    assert_eq!(
        store
            .get_revision(artifact_id(0x40), 1)
            .expect("get after recovery"),
        payload
    );
    assert!(
        store
            .recover()
            .expect("clean recover")
            .orphan_blobs
            .is_empty()
    );
    assert_integrity(directory.root());
}

// ---------------------------------------------------------------------------
// Row 2: disk-full (ENOSPC) during the metadata commit fails closed
// ---------------------------------------------------------------------------

/// disk-full：`FailWritesAfter { 0, Full }` 使元数据事务以 `SQLITE_FULL`
/// 显式失败（错误链含 full）；不产生半截元数据（head 不变、revision 表
/// 无行）；disarm 后同一 put 成功。blob 写入期的 ENOSPC 由
/// `blob.rs` 的 OS 错误码分类单测与下行权限失败集成测试共同覆盖
/// （macOS 无可写 `/dev/full`，无法无挂载地制造真实整盘写满）。
#[test]
fn fault_disk_full_during_metadata_commit_fails_closed() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let directory = TestStoreDir::new("full-metadata");
    let store = open_shim(directory.root());
    store.create_artifact(artifact_spec(0x41)).expect("create");

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = store
        .put_revision(put(artifact_id(0x41), 0, &bytes(0xd2, 256)))
        .expect_err("metadata commit must fail under injected disk-full");
    assert!(
        matches!(error, ArtifactError::Sqlite(_)),
        "expected a storage error, got {error}"
    );
    let chain = error_chain(&error).to_lowercase();
    assert!(
        chain.contains("full"),
        "error chain must name the disk-full condition, got: {chain}"
    );
    assert!(nlos_store_fault::writes_observed() > 0);
    nlos_store_fault::disarm();

    assert_eq!(store.resolve_head(artifact_id(0x41)).expect("head"), None);
    assert!(
        store
            .list_revisions(artifact_id(0x41))
            .expect("list")
            .is_empty()
    );

    let decision = store
        .put_revision(put(artifact_id(0x41), 0, &bytes(0xd2, 256)))
        .expect("put after disarm");
    assert!(matches!(decision, PutRevisionDecision::Committed(_)));
    assert_integrity(directory.root());
}

/// blob 写入期失败（tmp 目录只读 → `File::create` 被拒）：类型化 I/O
/// 错误，且第一阶段失败时元数据阶段根本不执行 —— 无 revision、head 不
/// 变、tmp 目录无残留。
#[cfg(unix)]
#[test]
fn fault_blob_write_failure_commits_no_metadata() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TestStoreDir::new("blob-write-failure");
    let store = ArtifactStore::open(directory.root()).expect("open");
    store.create_artifact(artifact_spec(0x42)).expect("create");

    let tmp = directory.root().join("artifacts/tmp");
    let original = fs::metadata(&tmp).expect("stat tmp").permissions();
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o555)).expect("read-only tmp");

    let error = store
        .put_revision(put(artifact_id(0x42), 0, &bytes(0xd3, 256)))
        .expect_err("blob write must fail");
    fs::set_permissions(&tmp, original).expect("restore tmp permissions");

    assert!(
        matches!(error, ArtifactError::Io(_)),
        "expected a typed I/O error, got {error}"
    );
    assert_eq!(store.resolve_head(artifact_id(0x42)).expect("head"), None);
    assert!(
        store
            .list_revisions(artifact_id(0x42))
            .expect("list")
            .is_empty()
    );
    assert_eq!(
        fs::read_dir(&tmp).expect("read tmp").count(),
        0,
        "failed blob commit must not leave tmp residue"
    );
}

// ---------------------------------------------------------------------------
// Row 3: silent write loss — phantom revision must not survive reopen
// ---------------------------------------------------------------------------

/// 静默丢写（断电模型）：`PowerLossAfter { 0 }` 下 put “报告成功”但元数据
/// 写入从未落盘；杀掉持有 wal-index 的连接后重开，幻影 revision 不可见
/// （head 仍为无、revision 表无行），blob 成为可识别孤儿；同一请求可重做
/// 且重开后真实持久。
#[test]
fn fault_power_loss_phantom_revision_invisible_after_reopen() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let directory = TestStoreDir::new("power-loss");
    let store = open_shim(directory.root());
    store.create_artifact(artifact_spec(0x43)).expect("create");

    let payload = bytes(0xd4, 512);
    let digest = ContentDigest::of_bytes(&payload);
    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = store
        .put_revision(put(artifact_id(0x43), 0, &payload))
        .expect("power loss drops metadata writes silently");
    assert!(matches!(phantom, PutRevisionDecision::Committed(_)));
    nlos_store_fault::disarm();

    // The surviving connection's wal-index references frames the disk never
    // saw; it must die first (as a real power loss would kill it).
    drop(store);

    let recovered = ArtifactStore::open(directory.root()).expect("reopen after power loss");
    assert_eq!(
        recovered.resolve_head(artifact_id(0x43)).expect("head"),
        None,
        "silently dropped metadata must not fabricate a head"
    );
    assert!(
        recovered
            .list_revisions(artifact_id(0x43))
            .expect("list")
            .is_empty()
    );
    assert!(matches!(
        recovered.get_revision(artifact_id(0x43), 1),
        Err(ArtifactError::RevisionNotFound { .. })
    ));
    let report = recovered.recover().expect("recover");
    assert_eq!(report.orphan_blobs, vec![digest]);
    assert!(report.missing_blobs.is_empty());
    assert_integrity(directory.root());

    // The lost decision is redoable and genuinely durable this time.
    let redone = recovered
        .put_revision(put(artifact_id(0x43), 0, &payload))
        .expect("redo after power loss");
    assert!(matches!(redone, PutRevisionDecision::Committed(_)));
    drop(recovered);
    let verified = ArtifactStore::open(directory.root()).expect("reopen after redo");
    assert_eq!(
        verified
            .get_revision(artifact_id(0x43), 1)
            .expect("redone revision"),
        payload
    );
}

// ---------------------------------------------------------------------------
// Row 4: kill-9 windows around the two-phase commit
// ---------------------------------------------------------------------------

/// commit 后崩溃等价：子进程完成 create + put 并提交返回后被强杀；重开后
/// revision 与 blob 完全可用，recover 无任何发现。
#[test]
fn fault_kill9_after_commit_fully_usable() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let directory = TestStoreDir::new("kill9-commit");
    let mut child = spawn_child("commit-then-park", directory.root());
    await_marker(&mut child);
    kill_and_reap(&mut child);

    let store = ArtifactStore::open(directory.root()).expect("reopen after kill");
    let head = store
        .resolve_head(artifact_id(0x30))
        .expect("resolve")
        .expect("head");
    assert_eq!(head.revision, 1);
    assert_eq!(head.digest, ContentDigest::of_bytes(&bytes(0xc1, 512)));
    assert_eq!(
        store
            .get_revision(artifact_id(0x30), 1)
            .expect("get committed revision"),
        bytes(0xc1, 512)
    );
    let report = store.recover().expect("recover");
    assert_eq!(report, nlos_artifact::RecoveryReport::default());
    assert_integrity(directory.root());
}

/// kill-9 等价：子进程在元数据事务未提交（已弄脏 head 行）时被强杀；
/// 重开后中断事务完全回滚，head 回到已提交值，recover 无发现。
#[test]
fn fault_kill9_mid_metadata_transaction_rolls_back() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let directory = TestStoreDir::new("kill9-mid-tx");
    let mut child = spawn_child("mid-metadata-tx", directory.root());
    await_marker(&mut child);
    kill_and_reap(&mut child);

    let store = ArtifactStore::open(directory.root()).expect("reopen after kill");
    assert_eq!(store.resolve_head(artifact_id(0x31)).expect("head"), None);
    assert!(
        store
            .list_revisions(artifact_id(0x31))
            .expect("list")
            .is_empty()
    );
    assert_eq!(
        store
            .inspect_artifact(artifact_id(0x31))
            .expect("inspect")
            .created_at_ms,
        1_000 + u64::from(0x31_u8),
        "mid-transaction dirt must be rolled back"
    );
    assert_eq!(
        store.recover().expect("recover"),
        nlos_artifact::RecoveryReport::default()
    );
    assert_integrity(directory.root());
}

/// rename 后、metadata commit 前崩溃窗口：blob 已在其内容地址持久化但无
/// 元数据引用；重开后无幻影 revision，recover 将 blob 列为孤儿且不删除
/// （GC 不在本切片）。
#[test]
fn fault_kill9_between_rename_and_metadata_commit_lists_orphan_blob() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let directory = TestStoreDir::new("kill9-orphan-blob");
    let mut child = spawn_child("plant-orphan-blob", directory.root());
    await_marker(&mut child);
    kill_and_reap(&mut child);

    let orphan_digest = ContentDigest::of_bytes(&bytes(0xc2, 512));
    let store = ArtifactStore::open(directory.root()).expect("reopen after kill");
    assert_eq!(store.resolve_head(artifact_id(0x32)).expect("head"), None);
    assert!(
        store
            .list_revisions(artifact_id(0x32))
            .expect("list")
            .is_empty()
    );
    let report = store.recover().expect("recover");
    assert_eq!(report.orphan_blobs, vec![orphan_digest]);
    assert!(report.missing_blobs.is_empty());
    assert!(
        directory.artifact_blob(orphan_digest).is_file(),
        "orphan blob is listed for GC, not deleted"
    );
    assert_integrity(directory.root());
}

/// rename 前崩溃窗口：tmp 残留是定义上的未提交写；recover 清理它，
/// 元数据与 blob 树保持一致。
#[test]
fn fault_kill9_before_rename_cleans_tmp_orphan() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let directory = TestStoreDir::new("kill9-orphan-tmp");
    let mut child = spawn_child("plant-orphan-tmp", directory.root());
    await_marker(&mut child);
    kill_and_reap(&mut child);

    let store = ArtifactStore::open(directory.root()).expect("reopen after kill");
    assert_eq!(store.resolve_head(artifact_id(0x33)).expect("head"), None);
    let report = store.recover().expect("recover");
    assert_eq!(report.removed_tmp_files, 1);
    assert_eq!(
        fs::read_dir(directory.root().join("artifacts/tmp"))
            .expect("read tmp")
            .count(),
        0
    );
    assert_integrity(directory.root());
}

// ---------------------------------------------------------------------------
// Row 5: hard I/O error during the package receipt commit — no half state
// ---------------------------------------------------------------------------

/// 元数据提交期硬 I/O 错误（B-ARTIFACT-003 receipt 写入）：签名与内容绑定
/// 校验已通过，`FailWritesAfter { 0, IoErr }` 使 receipt 插入事务显式失败；
/// 无半截 receipt（重开与 raw 计数均为 0），disarm 后同一请求原样成功
/// （幂等重做）。identity authority 使用默认 VFS，不受 shim 影响。
#[test]
fn fault_io_error_during_package_receipt_commit_leaves_no_receipt() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let directory = TestStoreDir::new("package-receipt-ioerr");
    let store = open_shim(directory.root());
    let identity = test_identity("package-receipt-ioerr", 0x58);
    store.create_artifact(artifact_spec(0x44)).expect("create");
    let payload = bytes(0xd5, 256);
    let digest = ContentDigest::of_bytes(&payload);
    store
        .put_revision(put(artifact_id(0x44), 0, &payload))
        .expect("put");

    let signed = sign_package(
        &identity,
        manifest(
            0x20,
            1,
            vec![entry(
                "only",
                artifact_id(0x44),
                digest,
                PackageEntryRole::Data,
            )],
        ),
    );
    let request = VerifyPackageRequest {
        signed: &signed,
        idempotency_key: nlos_types::IdempotencyKey::from_bytes([0x71; 16]),
        verified_at_ms: 5_000,
    };

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = store
        .verify_package(&identity.authority, request)
        .expect_err("receipt commit must fail under injected I/O error");
    nlos_store_fault::disarm();
    assert!(
        matches!(error, ArtifactError::Sqlite(_)),
        "expected a storage error, got {error}"
    );
    assert!(
        matches!(
            store.inspect_package_verification_receipt(nlos_types::ReceiptId::from_bytes([0; 16])),
            Err(ArtifactError::PackageVerificationReceiptNotFound(_))
        ),
        "no receipt id may be visible"
    );

    // No half state: no receipt row exists for this package at all.
    let raw = rusqlite::Connection::open(directory.root().join("metadata.db")).expect("raw open");
    let count: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM package_verification_receipts",
            [],
            |row| row.get(0),
        )
        .expect("count receipts");
    assert_eq!(count, 0);
    drop(raw);

    // After the fault clears the same request verifies cleanly.
    let decision = store
        .verify_package(&identity.authority, request)
        .expect("verify after disarm");
    assert!(matches!(decision, PackageVerificationDecision::Verified(_)));
    assert_integrity(directory.root());
}
