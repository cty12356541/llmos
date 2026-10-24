//! Durable `SQLite` (`WAL + FULL`) persistence, crash/reopen recovery,
//! immutability triggers, and fail-closed corruption checks for all three
//! `nlos-lease` families (`QuotaLeaseGrantor`, `CapacityLeaseGrantor`,
//! `ExclusiveDeviceLeaseGrantor`).
//!
//! One `#[test]` so this file claims the process-scoped Cell only once and
//! drives epoch advances deterministically.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_cell::CellAuthority;
use nlos_lease::{
    CapacityLeaseGrantor, CapacityLeaseState, DeviceResetReceipt, ExclusiveDeviceLeaseGrantor,
    ExclusiveDeviceLeaseState, HostAttachReceipt, LeaseInstant, LeaseStoreError, QuotaLeaseGrantor,
    QuotaLeaseLedgerError, QuotaLeaseState, SqliteLeaseStore,
};
use nlos_types::{
    CapacityLeaseId, DeviceId, ExclusiveDeviceLeaseId, Generation, QuotaLeaseId, ReceiptId,
    SchedulerDomainId,
};
use rusqlite::Connection;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "nlos-lease-durable-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn durable_lease_store_persists_all_three_families_and_enforces_invariants() {
    let authority = CellAuthority::claim(SchedulerDomainId::from_bytes([0x62; 16]))
        .expect("claim process-scoped cell")
        .into_shared();
    let temp = TempDir::new("w62");

    quota_durable_lifecycle_quarantine_and_ttl_survive_reopen(&authority, temp.path());
    capacity_durable_lifecycle_and_attach_survive_reopen(&authority, temp.path());
    device_durable_reset_receipts_and_triggers_survive_reopen(&authority, temp.path());
    unified_single_db_all_three_families_coexist_across_reopen(&authority, temp.path());
    corrupt_schema_and_tampered_rows_fail_closed_on_reopen(&authority, temp.path());
}

fn quota_durable_lifecycle_quarantine_and_ttl_survive_reopen(
    authority: &Arc<CellAuthority>,
    root: &Path,
) {
    let db_path = root.join("quota-durable.db");
    let fence_e1 = authority.fence();

    let lease_spend_close = QuotaLeaseId::from_bytes([0x11; 16]);
    let lease_cancel = QuotaLeaseId::from_bytes([0x12; 16]);
    let lease_quarantined = QuotaLeaseId::from_bytes([0x13; 16]);
    let lease_ttl = QuotaLeaseId::from_bytes([0x14; 16]);

    let fence_e2 = {
        let mut grantor =
            QuotaLeaseGrantor::open_durable(&db_path, Arc::clone(authority), 200).expect("open");
        assert_eq!(grantor.available(), 200);

        // 1. Grant + activate + spend high-water + begin_close + finalize_close
        grantor
            .grant(&fence_e1, lease_spend_close, 50, None)
            .expect("grant 1");
        grantor
            .admit_operation(&fence_e1, lease_spend_close)
            .expect("activate 1");
        let spent = grantor
            .report_spend(&fence_e1, lease_spend_close, 20)
            .expect("spend 1");
        assert_eq!(spent.spent(), 20);
        assert_eq!(spent.remaining(), 30);
        grantor
            .begin_close(&fence_e1, lease_spend_close)
            .expect("begin_close 1");
        grantor
            .finalize_close(&fence_e1, lease_spend_close)
            .expect("ack_close 1");
        assert_eq!(grantor.available(), 180);

        // 2. Grant + cancel_issued
        grantor
            .grant(&fence_e1, lease_cancel, 30, None)
            .expect("grant 2");
        assert_eq!(grantor.available(), 150);
        grantor
            .cancel_issued(&fence_e1, lease_cancel)
            .expect("cancel 2");
        assert_eq!(grantor.available(), 180);

        // 3. Grant + activate + spend partial -> then epoch advance quarantines it!
        grantor
            .grant(&fence_e1, lease_quarantined, 40, None)
            .expect("grant 3");
        grantor
            .activate(&fence_e1, lease_quarantined)
            .expect("activate 3");
        grantor
            .report_usage(&fence_e1, lease_quarantined, 15)
            .expect("spend 3");
        assert_eq!(grantor.available(), 140);

        let fence_e2 = grantor
            .advance_epoch_and_quarantine()
            .expect("advance_epoch_and_quarantine");
        assert_eq!(
            grantor.query(lease_quarantined).expect("query 3").state(),
            QuotaLeaseState::Quarantined
        );
        assert_eq!(
            grantor.available(),
            140,
            "quarantined funds never return to available"
        );

        // 4. Under epoch 2, grant a TTL-bound lease, spend 10, and sweep_expired into FENCED
        grantor
            .grant(&fence_e2, lease_ttl, 30, Some(LeaseInstant::new(50)))
            .expect("grant ttl");
        grantor
            .activate(&fence_e2, lease_ttl)
            .expect("activate ttl");
        grantor
            .report_usage(&fence_e2, lease_ttl, 10)
            .expect("spend ttl");
        assert_eq!(grantor.available(), 110);
        assert_eq!(
            grantor.sweep_expired(LeaseInstant::new(50)),
            vec![lease_ttl]
        );
        assert_eq!(
            grantor.query(lease_ttl).expect("query ttl").state(),
            QuotaLeaseState::Fenced
        );
        assert_eq!(grantor.available(), 110, "sweep does not refund");
        fence_e2
    };

    assert_quota_reopen_and_settle_fenced(
        &db_path,
        authority,
        &fence_e2,
        lease_spend_close,
        lease_cancel,
        lease_quarantined,
        lease_ttl,
    );
    assert_quota_second_reopen_and_triggers(&db_path, authority, lease_ttl);
}

fn assert_quota_reopen_and_settle_fenced(
    db_path: &Path,
    authority: &Arc<CellAuthority>,
    fence_e2: &nlos_cell::CellFence,
    lease_spend_close: QuotaLeaseId,
    lease_cancel: QuotaLeaseId,
    lease_quarantined: QuotaLeaseId,
    lease_ttl: QuotaLeaseId,
) {
    let mut reopened =
        QuotaLeaseGrantor::open_durable(db_path, Arc::clone(authority), 200).expect("reopen");
    assert_eq!(reopened.available(), 110);

    let closed = reopened.query(lease_spend_close).expect("query closed");
    assert_eq!(closed.state(), QuotaLeaseState::Closed);
    assert_eq!(closed.spent(), 20);
    assert_eq!(closed.returned(), 30);
    assert_eq!(closed.remaining(), 0);

    let cancelled = reopened.query(lease_cancel).expect("query cancelled");
    assert_eq!(cancelled.state(), QuotaLeaseState::Cancelled);
    assert_eq!(cancelled.returned(), 30);

    let quarantined = reopened
        .query(lease_quarantined)
        .expect("query quarantined");
    assert_eq!(quarantined.state(), QuotaLeaseState::Quarantined);
    assert_eq!(quarantined.spent(), 15);
    assert_eq!(quarantined.remaining(), 25);
    assert_eq!(quarantined.returned(), 0);
    assert_eq!(
        reopened.ack_close(fence_e2, lease_quarantined),
        Err(QuotaLeaseLedgerError::NotClosing {
            state: QuotaLeaseState::Quarantined,
        })
    );

    // Re-sweeping after restart is idempotent (already FENCED)
    assert_eq!(
        reopened.sweep_expired(LeaseInstant::new(60)),
        Vec::<QuotaLeaseId>::new()
    );
    let fenced = reopened.query(lease_ttl).expect("query fenced");
    assert_eq!(fenced.state(), QuotaLeaseState::Fenced);

    // Settle the FENCED lease after restart -> returns unspent 20 to available!
    let settled = reopened
        .settle_fenced(fence_e2, lease_ttl)
        .expect("settle fenced after reopen");
    assert_eq!(settled.state(), QuotaLeaseState::Closed);
    assert_eq!(settled.spent(), 10);
    assert_eq!(settled.returned(), 20);
    assert_eq!(settled.remaining(), 0);
    assert_eq!(reopened.available(), 130);

    // Idempotent replay of settle_fenced and grant after reopen
    assert_eq!(
        reopened
            .settle_fenced(fence_e2, lease_ttl)
            .expect("idempotent settle"),
        settled
    );
    assert_eq!(reopened.available(), 130);
    assert_eq!(
        reopened
            .grant(fence_e2, lease_ttl, 30, Some(LeaseInstant::new(50)))
            .expect("idempotent grant replay"),
        settled
    );
    assert_eq!(reopened.available(), 130);
}

fn assert_quota_second_reopen_and_triggers(
    db_path: &Path,
    authority: &Arc<CellAuthority>,
    lease_ttl: QuotaLeaseId,
) {
    let reopened_again =
        QuotaLeaseGrantor::open_durable(db_path, Arc::clone(authority), 200).expect("reopen 2");
    assert_eq!(reopened_again.available(), 130);
    assert_eq!(
        reopened_again.query(lease_ttl).expect("query ttl").state(),
        QuotaLeaseState::Closed
    );

    let conn = Connection::open(db_path).expect("raw open");
    assert!(
        conn.execute(
            "UPDATE quota_leases SET state = 'ACTIVE' WHERE state = 'CLOSED'",
            [],
        )
        .is_err(),
        "trigger must reject updating CLOSED quota_leases"
    );
    assert!(
        conn.execute(
            "UPDATE quota_leases SET state = 'CLOSED' WHERE state = 'QUARANTINED'",
            [],
        )
        .is_err(),
        "trigger must reject updating QUARANTINED quota_leases"
    );
    assert!(
        conn.execute("DELETE FROM quota_leases", []).is_err(),
        "trigger must reject deleting quota_leases"
    );
}

fn capacity_durable_lifecycle_and_attach_survive_reopen(
    authority: &Arc<CellAuthority>,
    root: &Path,
) {
    let db_path = root.join("capacity-durable.db");
    let fence = authority.fence();

    let lease_full = CapacityLeaseId::from_bytes([0x21; 16]);
    let lease_returning = CapacityLeaseId::from_bytes([0x22; 16]);
    let receipt_id = ReceiptId::from_bytes([0x31; 16]);

    let committed_receipt = {
        let mut grantor =
            CapacityLeaseGrantor::open_durable(&db_path, Arc::clone(authority), 100).expect("open");
        assert_eq!(grantor.pool_remaining(), 100);

        let reserved = grantor
            .grant(&fence, lease_full, 40)
            .expect("grant lease_full");
        assert_eq!(reserved.state(), CapacityLeaseState::GlobalReserved);
        assert_eq!(grantor.pool_remaining(), 60);

        let prepared = grantor
            .prepare_target(&fence, lease_full)
            .expect("prepare lease_full");
        assert_eq!(prepared.state(), CapacityLeaseState::TargetPrepared);

        let receipt = HostAttachReceipt::new(
            receipt_id,
            lease_full,
            prepared.target_node(),
            prepared.target_node_boot_generation(),
            prepared.capacity_epoch(),
            prepared.fencing_token(),
            prepared.amount(),
        );
        let active = grantor
            .activate_host(&fence, receipt)
            .expect("activate lease_full");
        assert_eq!(active.state(), CapacityLeaseState::Active);

        // Second lease enters RETURNING before crash/drop
        grantor
            .grant(&fence, lease_returning, 25)
            .expect("grant lease_returning");
        grantor
            .begin_return(&fence, lease_returning)
            .expect("begin_return lease_returning");
        assert_eq!(grantor.pool_remaining(), 35);
        receipt
    };

    // Reopen and verify ACTIVE + HostAttachReceipt and RETURNING -> RETURNED settlement
    {
        let mut reopened = CapacityLeaseGrantor::open_durable(&db_path, Arc::clone(authority), 100)
            .expect("reopen");
        assert_eq!(reopened.pool_remaining(), 35);

        let active = reopened.query(lease_full).expect("query lease_full");
        assert_eq!(active.state(), CapacityLeaseState::Active);
        assert_eq!(reopened.attach_receipt(lease_full), Some(committed_receipt));

        // Idempotent replay of activate_host after reopen
        assert_eq!(
            reopened
                .activate_host(&fence, committed_receipt)
                .expect("idempotent attach replay"),
            active
        );

        // Complete return of both leases across reopen
        let ret_second = reopened
            .finalize_return(&fence, lease_returning)
            .expect("ack_return lease_returning");
        assert_eq!(ret_second.state(), CapacityLeaseState::Returned);
        assert_eq!(reopened.pool_remaining(), 60);

        reopened
            .begin_return(&fence, lease_full)
            .expect("begin_return lease_full");
        assert_eq!(reopened.pool_remaining(), 60);
        let ret_first = reopened
            .ack_return(&fence, lease_full)
            .expect("ack_return lease_full");
        assert_eq!(ret_first.state(), CapacityLeaseState::Returned);
        assert_eq!(reopened.pool_remaining(), 100);
    }

    assert_capacity_second_reopen_and_triggers(&db_path, authority, lease_full, committed_receipt);
}

fn assert_capacity_second_reopen_and_triggers(
    db_path: &Path,
    authority: &Arc<CellAuthority>,
    lease_full: CapacityLeaseId,
    committed_receipt: HostAttachReceipt,
) {
    let reopened_again =
        CapacityLeaseGrantor::open_durable(db_path, Arc::clone(authority), 100).expect("reopen 2");
    assert_eq!(reopened_again.pool_remaining(), 100);
    assert_eq!(
        reopened_again.query(lease_full).expect("query").state(),
        CapacityLeaseState::Returned
    );
    assert_eq!(
        reopened_again.attach_receipt(lease_full),
        Some(committed_receipt)
    );

    let conn = Connection::open(db_path).expect("raw open");
    assert!(
        conn.execute(
            "UPDATE capacity_leases SET state = 'ACTIVE' WHERE state = 'RETURNED'",
            [],
        )
        .is_err(),
        "trigger must reject updating RETURNED capacity_leases"
    );
    assert!(
        conn.execute(
            "UPDATE capacity_attach_receipts SET attached_amount = 999",
            []
        )
        .is_err(),
        "trigger must reject updating capacity_attach_receipts"
    );
    assert!(
        conn.execute("DELETE FROM capacity_attach_receipts", [])
            .is_err(),
        "trigger must reject deleting capacity_attach_receipts"
    );
}

fn device_durable_reset_receipts_and_triggers_survive_reopen(
    authority: &Arc<CellAuthority>,
    root: &Path,
) {
    let db_path = root.join("device-durable.db");
    let fence = authority.fence();
    let device_id = DeviceId::from_bytes([0x41; 16]);
    let lease_1 = ExclusiveDeviceLeaseId::from_bytes([0x42; 16]);
    let lease_2 = ExclusiveDeviceLeaseId::from_bytes([0x43; 16]);
    let receipt_id_1 = ReceiptId::from_bytes([0x51; 16]);
    let receipt_id_2 = ReceiptId::from_bytes([0x52; 16]);

    let receipt_1 = {
        let mut grantor =
            ExclusiveDeviceLeaseGrantor::open_durable(&db_path, Arc::clone(authority), device_id)
                .expect("open");
        assert!(grantor.is_free());
        assert_eq!(grantor.reset_generation(), Generation::INITIAL);

        let grant_1 = grantor.grant(&fence, lease_1).expect("grant 1");
        assert!(!grantor.is_free());
        assert_eq!(grant_1.state(), ExclusiveDeviceLeaseState::DeviceReserved);

        let resetting_1 = grantor
            .begin_device_reset(&fence, lease_1)
            .expect("declare reset 1");
        assert_eq!(resetting_1.state(), ExclusiveDeviceLeaseState::Resetting);

        let receipt_1 = DeviceResetReceipt::new(
            receipt_id_1,
            lease_1,
            grant_1.holder_node(),
            grant_1.holder_node_boot_generation(),
            grant_1.exclusivity_epoch(),
            grant_1.fencing_token(),
            grant_1.reset_generation(),
        );
        let returned_1 = grantor
            .finalize_device_reset(&fence, receipt_1)
            .expect("submit reset receipt 1");
        assert_eq!(returned_1.state(), ExclusiveDeviceLeaseState::Returned);
        assert!(grantor.is_free());
        assert_eq!(
            grantor.reset_generation(),
            Generation::INITIAL.checked_next().expect("gen 2")
        );

        // Claim second lease and move to Resetting before drop/restart
        let grant_2 = grantor.grant(&fence, lease_2).expect("grant 2");
        assert_eq!(
            grant_2.reset_generation(),
            Generation::INITIAL.checked_next().expect("gen 2")
        );
        grantor
            .declare_reset(&fence, lease_2)
            .expect("declare reset 2");
        receipt_1
    };

    // Reopen and verify lease_1 is Returned, receipt_1 is in log, lease_2 is Resetting, head is not free
    let receipt_2 = {
        let mut reopened =
            ExclusiveDeviceLeaseGrantor::open_durable(&db_path, Arc::clone(authority), device_id)
                .expect("reopen");
        assert!(!reopened.is_free());
        assert_eq!(
            reopened.reset_generation(),
            Generation::INITIAL.checked_next().expect("gen 2")
        );
        assert_eq!(reopened.reset_receipts(), &[receipt_1]);

        // Replay receipt_1 after restart is idempotent
        assert_eq!(
            reopened
                .submit_reset_receipt(&fence, receipt_1)
                .expect("idempotent receipt 1 replay")
                .state(),
            ExclusiveDeviceLeaseState::Returned
        );

        let resetting_2 = reopened.query(lease_2).expect("query lease_2");
        assert_eq!(resetting_2.state(), ExclusiveDeviceLeaseState::Resetting);

        let receipt_2 = DeviceResetReceipt::new(
            receipt_id_2,
            lease_2,
            resetting_2.holder_node(),
            resetting_2.holder_node_boot_generation(),
            resetting_2.exclusivity_epoch(),
            resetting_2.fencing_token(),
            resetting_2.reset_generation(),
        );
        let returned_2 = reopened
            .submit_reset_receipt(&fence, receipt_2)
            .expect("submit reset receipt 2");
        assert_eq!(returned_2.state(), ExclusiveDeviceLeaseState::Returned);
        assert!(reopened.is_free());
        assert_eq!(reopened.reset_receipts(), &[receipt_1, receipt_2]);
        receipt_2
    };

    assert_device_third_reopen_and_triggers(&db_path, authority, device_id, receipt_1, receipt_2);
}

fn assert_device_third_reopen_and_triggers(
    db_path: &Path,
    authority: &Arc<CellAuthority>,
    device_id: DeviceId,
    receipt_1: DeviceResetReceipt,
    receipt_2: DeviceResetReceipt,
) {
    let reopened_again =
        ExclusiveDeviceLeaseGrantor::open_durable(db_path, Arc::clone(authority), device_id)
            .expect("reopen 2");
    assert!(reopened_again.is_free());
    assert_eq!(
        reopened_again.reset_generation(),
        Generation::INITIAL
            .checked_next()
            .expect("gen 2")
            .checked_next()
            .expect("gen 3")
    );
    assert_eq!(reopened_again.reset_receipts(), &[receipt_1, receipt_2]);

    let conn = Connection::open(db_path).expect("raw open");
    assert!(
        conn.execute("DELETE FROM device_reset_receipts WHERE seq = 1", [])
            .is_err(),
        "trigger must reject DELETE on device_reset_receipts"
    );
    assert!(
        conn.execute(
            "UPDATE device_reset_receipts SET reset_generation = 99 WHERE seq = 1",
            [],
        )
        .is_err(),
        "trigger must reject UPDATE on device_reset_receipts"
    );
    assert!(
        conn.execute(
            "UPDATE device_leases SET state = 'RESETTING' WHERE state = 'RETURNED'",
            [],
        )
        .is_err(),
        "trigger must reject UPDATE on RETURNED device_leases"
    );
    assert!(
        conn.execute("DELETE FROM device_leases", []).is_err(),
        "trigger must reject DELETE on device_leases"
    );
}

fn unified_single_db_all_three_families_coexist_across_reopen(
    authority: &Arc<CellAuthority>,
    root: &Path,
) {
    let db_path = root.join("unified-leases.db");
    let fence = authority.fence();
    let device_id = DeviceId::from_bytes([0x61; 16]);
    let q_id = QuotaLeaseId::from_bytes([0x62; 16]);
    let c_id = CapacityLeaseId::from_bytes([0x63; 16]);
    let d_id = ExclusiveDeviceLeaseId::from_bytes([0x64; 16]);

    {
        let store = Arc::new(SqliteLeaseStore::open(&db_path).expect("open unified store"));
        let mut quota =
            QuotaLeaseGrantor::open_with_store(Arc::clone(&store), Arc::clone(authority), 500)
                .expect("open quota");
        let mut capacity =
            CapacityLeaseGrantor::open_with_store(Arc::clone(&store), Arc::clone(authority), 300)
                .expect("open capacity");
        let mut device =
            ExclusiveDeviceLeaseGrantor::open_with_store(store, Arc::clone(authority), device_id)
                .expect("open device");

        quota.grant(&fence, q_id, 120, None).expect("quota grant");
        capacity.grant(&fence, c_id, 80).expect("capacity grant");
        device.grant(&fence, d_id).expect("device grant");
    }

    {
        let quota = QuotaLeaseGrantor::open_durable(&db_path, Arc::clone(authority), 500)
            .expect("reopen quota");
        let capacity = CapacityLeaseGrantor::open_durable(&db_path, Arc::clone(authority), 300)
            .expect("reopen capacity");
        let device =
            ExclusiveDeviceLeaseGrantor::open_durable(&db_path, Arc::clone(authority), device_id)
                .expect("reopen device");

        assert_eq!(quota.available(), 380);
        assert_eq!(
            quota.query(q_id).expect("q_id").state(),
            QuotaLeaseState::Issued
        );
        assert_eq!(capacity.pool_remaining(), 220);
        assert_eq!(
            capacity.query(c_id).expect("c_id").state(),
            CapacityLeaseState::GlobalReserved
        );
        assert!(!device.is_free());
        assert_eq!(
            device.query(d_id).expect("d_id").state(),
            ExclusiveDeviceLeaseState::DeviceReserved
        );
    }
}

fn corrupt_schema_and_tampered_rows_fail_closed_on_reopen(
    authority: &Arc<CellAuthority>,
    root: &Path,
) {
    // 1. Unsupported PRAGMA user_version fails closed
    let bad_version_db = root.join("bad-version.db");
    {
        let _grantor = QuotaLeaseGrantor::open_durable(&bad_version_db, Arc::clone(authority), 100)
            .expect("create valid db");
        let conn = Connection::open(&bad_version_db).expect("open raw");
        conn.pragma_update(None, "user_version", 99)
            .expect("set unsupported user_version");
    }
    assert!(matches!(
        QuotaLeaseGrantor::open_durable(&bad_version_db, Arc::clone(authority), 100),
        Err(LeaseStoreError::SchemaVersionUnsupported(99))
    ));

    // 2. Tampered quota_pool_state available balance violating conservation fails closed
    let bad_quota_db = root.join("bad-quota.db");
    {
        let mut grantor =
            QuotaLeaseGrantor::open_durable(&bad_quota_db, Arc::clone(authority), 100)
                .expect("open quota");
        grantor
            .grant(
                &authority.fence(),
                QuotaLeaseId::from_bytes([0x81; 16]),
                40,
                None,
            )
            .expect("grant");
    }
    {
        let conn = Connection::open(&bad_quota_db).expect("raw open");
        conn.execute(
            "UPDATE quota_pool_state SET available = 99 WHERE singleton = 1",
            [],
        )
        .expect("tamper available");
    }
    assert!(matches!(
        QuotaLeaseGrantor::open_durable(&bad_quota_db, Arc::clone(authority), 100),
        Err(LeaseStoreError::CorruptRecord(_))
    ));

    // 3. Tampered capacity_pool_state violating conservation fails closed
    let bad_cap_db = root.join("bad-capacity.db");
    {
        let mut grantor =
            CapacityLeaseGrantor::open_durable(&bad_cap_db, Arc::clone(authority), 100)
                .expect("open capacity");
        grantor
            .grant(
                &authority.fence(),
                CapacityLeaseId::from_bytes([0x82; 16]),
                30,
            )
            .expect("grant capacity");
    }
    {
        let conn = Connection::open(&bad_cap_db).expect("raw open");
        conn.execute(
            "UPDATE capacity_pool_state SET pool_remaining = 10 WHERE singleton = 1",
            [],
        )
        .expect("tamper capacity pool");
    }
    assert!(matches!(
        CapacityLeaseGrantor::open_durable(&bad_cap_db, Arc::clone(authority), 100),
        Err(LeaseStoreError::CorruptRecord(_))
    ));

    // 4. Tampered device_head_state contradicting committed lease fails closed
    let bad_dev_db = root.join("bad-device.db");
    let device_id = DeviceId::from_bytes([0x83; 16]);
    {
        let mut grantor = ExclusiveDeviceLeaseGrantor::open_durable(
            &bad_dev_db,
            Arc::clone(authority),
            device_id,
        )
        .expect("open device");
        grantor
            .grant(
                &authority.fence(),
                ExclusiveDeviceLeaseId::from_bytes([0x84; 16]),
            )
            .expect("grant device");
    }
    {
        let conn = Connection::open(&bad_dev_db).expect("raw open");
        conn.execute("UPDATE device_head_state SET is_free = 1", [])
            .expect("tamper device head");
    }
    assert!(matches!(
        ExclusiveDeviceLeaseGrantor::open_durable(&bad_dev_db, Arc::clone(authority), device_id),
        Err(LeaseStoreError::CorruptRecord(_))
    ));
}
