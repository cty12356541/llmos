//! Integration tests for C-LEASE cross-epoch reconciliation protocol:
//! - `[LEASE-RECON-001]` Typed non-interchangeable reconciliation receipts across all 3 lease families
//! - `[LEASE-FENCE-001]` Fencing token monotonicity and `FenceBarrierAck` digest verification
//! - `[LEASE-CAPACITY-002]` Capacity lease cleanup attestation before pool return
//! - `[LEASE-DEVICE-001]` Exclusive device hardware reset + zeroization and `reset_generation` bump before return
//! - `[LEASE-MIGRATE-001]` Cross-node lease migration via source drain/return + new lease minting
//! - `[LEASE-WAL-001]` Local lease WAL loss quarantine and refusal of guessed remaining balances

#![allow(clippy::similar_names, clippy::too_many_lines)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use nlos_cell::{CellAuthority, CellFencingToken};
use nlos_lease::{
    CapacityLeaseGrantor, CapacityLeaseState, CapacityMigrationError, CapacityReconcileError,
    CapacityReconciliationReceipt, CleanupAttestation, DeviceMigrationError, DeviceReconcileError,
    DeviceReconciliationReceipt, DeviceResetReceipt, ExclusiveDeviceLeaseGrantor,
    ExclusiveDeviceLeaseState, FenceBarrierAck, QuotaLeaseGrantor, QuotaLeaseState,
    QuotaMigrationError, QuotaReconcileError, QuotaReconciliationReceipt, QuotaWalLossError,
    SqliteLeaseStore,
};
use nlos_types::{
    CapacityLeaseId, DeviceId, ExclusiveDeviceLeaseId, Generation, QuotaLeaseId, ReceiptId,
    SchedulerDomainId,
};

static CELL_AUTHORITY_LOCK: Mutex<Option<Arc<CellAuthority>>> = Mutex::new(None);
static NEXT_TEST_SEQ: AtomicU64 = AtomicU64::new(1);

const GATEWAY_ID: [u8; 16] = [0xA5; 16];

fn with_cell_authority<F, R>(f: F) -> R
where
    F: FnOnce(&Arc<CellAuthority>) -> R,
{
    let mut guard = CELL_AUTHORITY_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let authority = guard
        .get_or_insert_with(|| {
            CellAuthority::claim(SchedulerDomainId::from_bytes([0x63; 16]))
                .expect("claim process-scoped cell authority once")
                .into_shared()
        })
        .clone();
    f(&authority)
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = NEXT_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-lease-reconcile-{label}-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
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
fn quota_reconcile_full_and_partial_conserves_pool_and_replays_idempotently() {
    with_cell_authority(|authority| {
        let mut grantor = QuotaLeaseGrantor::open(Arc::clone(authority), 1_000);
        let fence_e1 = authority.fence();

        let lease_id = QuotaLeaseId::from_bytes([0x01; 16]);
        grantor
            .grant(&fence_e1, lease_id, 400, None)
            .expect("grant quota lease");
        grantor
            .activate(&fence_e1, lease_id)
            .expect("activate quota lease");
        grantor
            .report_spend(&fence_e1, lease_id, 120)
            .expect("spend 120 before partition");
        assert_eq!(grantor.available(), 600);

        // Advance epoch to quarantine the active lease
        let fence_e2 = grantor
            .advance_epoch_and_quarantine()
            .expect("advance epoch and quarantine");
        let quarantined = grantor.query(lease_id).expect("query quarantined lease");
        assert_eq!(quarantined.state(), QuotaLeaseState::Quarantined);
        assert_eq!(grantor.available(), 600);

        // Step 1: Partial reconciliation (high-water 150, returned 100, remaining quarantined 150)
        let ack_1 = FenceBarrierAck::for_quota(&quarantined, GATEWAY_ID, 150);
        let receipt_1 = QuotaReconciliationReceipt::for_lease(
            ReceiptId::from_bytes([0x11; 16]),
            1,
            &quarantined,
            150,
            100,
            150,
            ack_1,
        );
        assert!(receipt_1.verify_conservation());

        let after_1 = grantor
            .reconcile_quarantined(&fence_e2, receipt_1)
            .expect("partial quarantine reconciliation");
        assert_eq!(after_1.state(), QuotaLeaseState::Quarantined);
        assert_eq!(after_1.spent(), 150);
        assert_eq!(after_1.returned(), 100);
        assert_eq!(after_1.remaining(), 150);
        assert_eq!(grantor.available(), 700);

        // Idempotent replay of seq 1 returns identical snapshot without double-crediting available
        let replay_1 = grantor
            .reconcile_quarantined(&fence_e2, receipt_1)
            .expect("idempotent replay of seq 1");
        assert_eq!(replay_1, after_1);
        assert_eq!(grantor.available(), 700);

        // Step 2: Final reconciliation closing the quarantined lease (high-water 200, returned 200, remaining 0)
        let ack_2 = FenceBarrierAck::for_quota(&after_1, GATEWAY_ID, 200);
        let receipt_2 = QuotaReconciliationReceipt::for_lease(
            ReceiptId::from_bytes([0x12; 16]),
            2,
            &after_1,
            200,
            200,
            0,
            ack_2,
        );
        let after_2 = grantor
            .reconcile_quarantined(&fence_e2, receipt_2)
            .expect("terminal quarantine reconciliation");
        assert_eq!(after_2.state(), QuotaLeaseState::Closed);
        assert_eq!(after_2.spent(), 200);
        assert_eq!(after_2.returned(), 200);
        assert_eq!(after_2.remaining(), 0);
        assert_eq!(
            after_2.spent() + after_2.returned() + after_2.remaining(),
            after_2.face_value()
        );
        assert_eq!(grantor.available(), 800);
        assert_eq!(grantor.reconciliation_receipts(lease_id).len(), 2);
    });
}

#[test]
fn quota_reconcile_rejects_invalid_barrier_digest_and_high_water_regression() {
    with_cell_authority(|authority| {
        let mut grantor = QuotaLeaseGrantor::open(Arc::clone(authority), 500);
        let fence_e1 = authority.fence();

        let lease_id = QuotaLeaseId::from_bytes([0x02; 16]);
        grantor
            .grant(&fence_e1, lease_id, 200, None)
            .expect("grant lease");
        grantor.activate(&fence_e1, lease_id).expect("activate");
        grantor
            .report_spend(&fence_e1, lease_id, 80)
            .expect("spend 80");

        let fence_e2 = grantor
            .advance_epoch_and_quarantine()
            .expect("quarantine lease");
        let quarantined = grantor.query(lease_id).expect("query quarantined");

        // 1. Tampered digest must be rejected fail-closed
        let tampered_ack =
            FenceBarrierAck::for_quota(&quarantined, GATEWAY_ID, 100).with_digest([0xFF; 32]);
        let bad_digest_receipt = QuotaReconciliationReceipt::for_lease(
            ReceiptId::from_bytes([0x21; 16]),
            1,
            &quarantined,
            100,
            100,
            0,
            tampered_ack,
        );
        let err = grantor
            .reconcile_quarantined(&fence_e2, bad_digest_receipt)
            .expect_err("tampered digest must fail");
        assert!(matches!(
            err,
            QuotaReconcileError::InvalidFenceBarrierAck { .. }
        ));

        // 2. Mismatched fencing token in FenceBarrierAck must be rejected
        let bogus_token = CellFencingToken::from_u64(quarantined.fencing_token().get() + 999)
            .expect("non-zero token");
        let wrong_token_ack = FenceBarrierAck::new(
            *lease_id.as_bytes(),
            quarantined.fence_scope(),
            quarantined.epoch(),
            bogus_token,
            GATEWAY_ID,
            100,
            FenceBarrierAck::compute_digest(
                *lease_id.as_bytes(),
                quarantined.fence_scope(),
                quarantined.epoch(),
                bogus_token,
                GATEWAY_ID,
                100,
            ),
        );
        let wrong_token_receipt = QuotaReconciliationReceipt::for_lease(
            ReceiptId::from_bytes([0x22; 16]),
            1,
            &quarantined,
            100,
            100,
            0,
            wrong_token_ack,
        );
        let err = grantor
            .reconcile_quarantined(&fence_e2, wrong_token_receipt)
            .expect_err("wrong fencing token in barrier must fail");
        assert!(matches!(
            err,
            QuotaReconcileError::InvalidFenceBarrierAck { .. }
        ));

        // 3. High-water regression (< 80 already spent) must be rejected
        let regressed_ack = FenceBarrierAck::for_quota(&quarantined, GATEWAY_ID, 60);
        let regressed_receipt = QuotaReconciliationReceipt::for_lease(
            ReceiptId::from_bytes([0x23; 16]),
            1,
            &quarantined,
            60,
            140,
            0,
            regressed_ack,
        );
        let err = grantor
            .reconcile_quarantined(&fence_e2, regressed_receipt)
            .expect_err("high-water regression must fail");
        assert!(matches!(
            err,
            QuotaReconcileError::HighWaterRegression {
                reported: 60,
                committed: 80
            }
        ));

        // 4. Conservation violation (settled_spent + returned + remaining != face_value) must fail
        let valid_ack = FenceBarrierAck::for_quota(&quarantined, GATEWAY_ID, 150);
        let bad_conservation = QuotaReconciliationReceipt::for_lease(
            ReceiptId::from_bytes([0x24; 16]),
            1,
            &quarantined,
            150,
            60, // 150 + 60 + 0 = 210 != 200
            0,
            valid_ack,
        );
        let err = grantor
            .reconcile_quarantined(&fence_e2, bad_conservation)
            .expect_err("conservation violation must fail");
        assert!(matches!(
            err,
            QuotaReconcileError::ConservationViolated { .. }
        ));
        assert_eq!(grantor.available(), 300);
    });
}

#[test]
fn capacity_reconcile_restores_pool_with_cleanup_attestation_and_fencing_barrier() {
    with_cell_authority(|authority| {
        let mut grantor = CapacityLeaseGrantor::open(Arc::clone(authority), 64);
        let fence = authority.fence();

        let lease_id = CapacityLeaseId::from_bytes([0x03; 16]);
        let lease = grantor
            .grant(&fence, lease_id, 16)
            .expect("grant capacity lease");
        assert_eq!(grantor.pool_remaining(), 48);

        let fenced = grantor
            .fence_lease(&fence, lease_id)
            .expect("fence capacity lease");
        assert_eq!(fenced.state(), CapacityLeaseState::Fenced);
        assert_eq!(
            grantor.pool_remaining(),
            48,
            "fencing must not restore pool capacity prematurely"
        );

        let barrier = FenceBarrierAck::for_capacity(&fenced, GATEWAY_ID);
        let cleanup = CleanupAttestation::for_lease(&fenced, 16, true);
        let receipt = CapacityReconciliationReceipt::for_lease(
            ReceiptId::from_bytes([0x31; 16]),
            &fenced,
            cleanup,
            barrier,
        );

        let returned = grantor
            .reconcile(&fence, receipt)
            .expect("reconcile fenced capacity lease");
        assert_eq!(returned.state(), CapacityLeaseState::Returned);
        assert_eq!(grantor.pool_remaining(), 64);
        assert_eq!(grantor.reconciliation_receipt(lease_id), Some(receipt));

        // Idempotent replay returns identical lease without double-crediting pool
        let replay = grantor
            .reconcile(&fence, receipt)
            .expect("idempotent capacity reconcile replay");
        assert_eq!(replay, returned);
        assert_eq!(grantor.pool_remaining(), 64);

        let _ = lease;
    });
}

#[test]
fn capacity_reconcile_rejects_unverified_cleanup_or_mismatched_barrier() {
    with_cell_authority(|authority| {
        let mut grantor = CapacityLeaseGrantor::open(Arc::clone(authority), 32);
        let fence = authority.fence();

        let lease_id = CapacityLeaseId::from_bytes([0x04; 16]);
        grantor
            .grant(&fence, lease_id, 8)
            .expect("grant capacity lease");
        let quarantined = grantor
            .quarantine_lease(&fence, lease_id)
            .expect("quarantine capacity lease");
        assert_eq!(quarantined.state(), CapacityLeaseState::Quarantined);

        let valid_barrier = FenceBarrierAck::for_capacity(&quarantined, GATEWAY_ID);

        // 1. Cleanup with holder_processes_terminated == false must fail
        let unverified_cleanup = CleanupAttestation::for_lease(&quarantined, 8, false);
        let bad_receipt_1 = CapacityReconciliationReceipt::for_lease(
            ReceiptId::from_bytes([0x41; 16]),
            &quarantined,
            unverified_cleanup,
            valid_barrier,
        );
        let err = grantor
            .reconcile_quarantined(&fence, bad_receipt_1)
            .expect_err("unterminated holder processes must fail");
        assert!(matches!(
            err,
            CapacityReconcileError::InvalidCleanupAttestation { .. }
        ));

        // 2. Cleanup with wrong cleaned_amount must fail
        let partial_cleanup = CleanupAttestation::for_lease(&quarantined, 4, true);
        let bad_receipt_2 = CapacityReconciliationReceipt::for_lease(
            ReceiptId::from_bytes([0x42; 16]),
            &quarantined,
            partial_cleanup,
            valid_barrier,
        );
        let err = grantor
            .reconcile_quarantined(&fence, bad_receipt_2)
            .expect_err("partial cleaned amount must fail");
        assert!(matches!(
            err,
            CapacityReconcileError::InvalidCleanupAttestation { .. }
        ));

        // 3. Tampered FenceBarrierAck digest must fail
        let valid_cleanup = CleanupAttestation::for_lease(&quarantined, 8, true);
        let bad_barrier = valid_barrier.with_digest([0xEE; 32]);
        let bad_receipt_3 = CapacityReconciliationReceipt::for_lease(
            ReceiptId::from_bytes([0x43; 16]),
            &quarantined,
            valid_cleanup,
            bad_barrier,
        );
        let err = grantor
            .reconcile_quarantined(&fence, bad_receipt_3)
            .expect_err("tampered barrier digest must fail");
        assert!(matches!(
            err,
            CapacityReconcileError::InvalidFenceBarrierAck { .. }
        ));

        assert_eq!(grantor.pool_remaining(), 24);
    });
}

#[test]
fn device_reconcile_requires_hardware_reset_and_fencing_barrier_before_free() {
    with_cell_authority(|authority| {
        let device_id = DeviceId::from_bytes([0x05; 16]);
        let mut grantor = ExclusiveDeviceLeaseGrantor::open(Arc::clone(authority), device_id);
        let fence = authority.fence();

        let lease_id = ExclusiveDeviceLeaseId::from_bytes([0x05; 16]);
        grantor
            .grant(&fence, lease_id)
            .expect("grant exclusive device lease");
        let quarantined = grantor
            .quarantine_lease(&fence, lease_id)
            .expect("quarantine device lease");
        assert_eq!(quarantined.state(), ExclusiveDeviceLeaseState::Quarantined);
        assert!(!grantor.is_free());

        let barrier = FenceBarrierAck::for_device(&quarantined, GATEWAY_ID);
        let reset_receipt = DeviceResetReceipt::new(
            ReceiptId::from_bytes([0x50; 16]),
            lease_id,
            quarantined.holder_node(),
            quarantined.holder_node_boot_generation(),
            quarantined.exclusivity_epoch(),
            quarantined.fencing_token(),
            quarantined.reset_generation(),
        );

        // Stale/mismatched reset_generation on DeviceReconciliationReceipt must be rejected
        let stale_gen = quarantined
            .reset_generation()
            .checked_next()
            .expect("next gen");
        let stale_reconcile = DeviceReconciliationReceipt::new(
            ReceiptId::from_bytes([0x51; 16]),
            lease_id,
            device_id,
            quarantined.holder_node(),
            quarantined.holder_node_boot_generation(),
            quarantined.exclusivity_epoch(),
            quarantined.fencing_token(),
            stale_gen,
            true,
            true,
            true,
            reset_receipt,
            barrier,
        );
        let err = grantor
            .reconcile_quarantined(&fence, stale_reconcile)
            .expect_err("mismatched reset generation must fail");
        assert!(matches!(
            err,
            DeviceReconcileError::StaleResetGeneration { .. }
        ));

        // Valid reconciliation clears head to FREE and bumps reset_generation
        let valid_reconcile = DeviceReconciliationReceipt::for_lease(
            ReceiptId::from_bytes([0x52; 16]),
            &quarantined,
            true,
            true,
            true,
            reset_receipt,
            barrier,
        );
        let returned = grantor
            .reconcile_quarantined(&fence, valid_reconcile)
            .expect("reconcile quarantined device lease");
        assert_eq!(returned.state(), ExclusiveDeviceLeaseState::Returned);
        assert!(grantor.is_free());
        assert_eq!(
            grantor.reset_generation(),
            quarantined
                .reset_generation()
                .checked_next()
                .expect("next gen")
        );
        assert_eq!(
            grantor.reconciliation_receipt(lease_id),
            Some(valid_reconcile)
        );

        // Idempotent replay returns identical lease without bumping reset_generation again
        let gen_after = grantor.reset_generation();
        let replay = grantor
            .reconcile_quarantined(&fence, valid_reconcile)
            .expect("idempotent device reconcile replay");
        assert_eq!(replay, returned);
        assert_eq!(grantor.reset_generation(), gen_after);
    });
}

#[test]
fn device_reconcile_rejects_unverified_reset_flags_and_wrong_barrier_token() {
    with_cell_authority(|authority| {
        let device_id = DeviceId::from_bytes([0x06; 16]);
        let mut grantor = ExclusiveDeviceLeaseGrantor::open(Arc::clone(authority), device_id);
        let fence = authority.fence();

        let lease_id = ExclusiveDeviceLeaseId::from_bytes([0x06; 16]);
        grantor.grant(&fence, lease_id).expect("grant device lease");
        let fenced = grantor
            .fence_lease(&fence, lease_id)
            .expect("fence device lease");
        assert_eq!(fenced.state(), ExclusiveDeviceLeaseState::Fencing);

        let valid_barrier = FenceBarrierAck::for_device(&fenced, GATEWAY_ID);
        let reset_receipt = DeviceResetReceipt::new(
            ReceiptId::from_bytes([0x60; 16]),
            lease_id,
            fenced.holder_node(),
            fenced.holder_node_boot_generation(),
            fenced.exclusivity_epoch(),
            fenced.fencing_token(),
            fenced.reset_generation(),
        );

        // 1. endpoint_fenced_old_token == false must fail
        let no_endpoint_fence = DeviceReconciliationReceipt::for_lease(
            ReceiptId::from_bytes([0x61; 16]),
            &fenced,
            false,
            true,
            true,
            reset_receipt,
            valid_barrier,
        );
        let err = grantor
            .reconcile(&fence, no_endpoint_fence)
            .expect_err("unconfirmed old token rejection must fail");
        assert_eq!(err, DeviceReconcileError::OldFencingTokenNotRejected);

        // 2. queue_drained == false must fail
        let no_queue_drain = DeviceReconciliationReceipt::for_lease(
            ReceiptId::from_bytes([0x62; 16]),
            &fenced,
            true,
            false,
            true,
            reset_receipt,
            valid_barrier,
        );
        let err = grantor
            .reconcile(&fence, no_queue_drain)
            .expect_err("undrained queue must fail");
        assert_eq!(err, DeviceReconcileError::QueueNotDrained);

        // 3. vram_and_secrets_zeroized == false must fail
        let no_zeroize = DeviceReconciliationReceipt::for_lease(
            ReceiptId::from_bytes([0x63; 16]),
            &fenced,
            true,
            true,
            false,
            reset_receipt,
            valid_barrier,
        );
        let err = grantor
            .reconcile(&fence, no_zeroize)
            .expect_err("unzeroized VRAM must fail");
        assert_eq!(err, DeviceReconcileError::ZeroizationNotAttested);

        // 4. Tampered FenceBarrierAck must fail
        let bad_barrier = valid_barrier.with_digest([0xAB; 32]);
        let bad_barrier_receipt = DeviceReconciliationReceipt::for_lease(
            ReceiptId::from_bytes([0x64; 16]),
            &fenced,
            true,
            true,
            true,
            reset_receipt,
            bad_barrier,
        );
        let err = grantor
            .reconcile(&fence, bad_barrier_receipt)
            .expect_err("tampered barrier must fail");
        assert!(matches!(
            err,
            DeviceReconcileError::InvalidFenceBarrierAck { .. }
        ));
        assert!(!grantor.is_free());
    });
}

#[test]
fn durable_sqlite_store_persists_and_rehydrates_all_three_reconciliation_receipts() {
    with_cell_authority(|authority| {
        let temp = TempDir::new("durable-three-family");
        let db_path = temp.path().join("reconcile-all.db");
        let store = Arc::new(SqliteLeaseStore::open(&db_path).expect("open sqlite store"));

        let quota_id = QuotaLeaseId::from_bytes([0x71; 16]);
        let cap_id = CapacityLeaseId::from_bytes([0x72; 16]);
        let dev_id = DeviceId::from_bytes([0x73; 16]);
        let dev_lease_id = ExclusiveDeviceLeaseId::from_bytes([0x74; 16]);

        let (expected_q_receipt, expected_c_receipt, expected_d_receipt) = {
            let mut quota_grantor =
                QuotaLeaseGrantor::open_with_store(Arc::clone(&store), Arc::clone(authority), 500)
                    .expect("open quota grantor");
            let mut cap_grantor = CapacityLeaseGrantor::open_with_store(
                Arc::clone(&store),
                Arc::clone(authority),
                32,
            )
            .expect("open capacity grantor");
            let mut dev_grantor = ExclusiveDeviceLeaseGrantor::open_with_store(
                Arc::clone(&store),
                Arc::clone(authority),
                dev_id,
            )
            .expect("open device grantor");

            let fence_e1 = authority.fence();
            quota_grantor
                .grant(&fence_e1, quota_id, 200, None)
                .expect("grant quota");
            quota_grantor
                .activate(&fence_e1, quota_id)
                .expect("activate quota");
            quota_grantor
                .report_spend(&fence_e1, quota_id, 70)
                .expect("spend quota");

            cap_grantor
                .grant(&fence_e1, cap_id, 10)
                .expect("grant capacity");
            dev_grantor
                .grant(&fence_e1, dev_lease_id)
                .expect("grant device");

            let fence_e2 = quota_grantor
                .advance_epoch_and_quarantine()
                .expect("advance epoch and quarantine quota");
            let q_quarantined = quota_grantor.query(quota_id).expect("q_quarantined");
            let c_quarantined = cap_grantor
                .quarantine_lease(&fence_e2, cap_id)
                .expect("quarantine capacity");
            let d_quarantined = dev_grantor
                .quarantine_lease(&fence_e2, dev_lease_id)
                .expect("quarantine device");

            let q_ack = FenceBarrierAck::for_quota(&q_quarantined, GATEWAY_ID, 90);
            let q_receipt = QuotaReconciliationReceipt::for_lease(
                ReceiptId::from_bytes([0x75; 16]),
                1,
                &q_quarantined,
                90,
                110,
                0,
                q_ack,
            );
            quota_grantor
                .reconcile_quarantined(&fence_e2, q_receipt)
                .expect("reconcile durable quota");

            let c_ack = FenceBarrierAck::for_capacity(&c_quarantined, GATEWAY_ID);
            let c_cleanup = CleanupAttestation::for_lease(&c_quarantined, 10, true);
            let c_receipt = CapacityReconciliationReceipt::for_lease(
                ReceiptId::from_bytes([0x76; 16]),
                &c_quarantined,
                c_cleanup,
                c_ack,
            );
            cap_grantor
                .reconcile_quarantined(&fence_e2, c_receipt)
                .expect("reconcile durable capacity");

            let d_ack = FenceBarrierAck::for_device(&d_quarantined, GATEWAY_ID);
            let d_reset = DeviceResetReceipt::new(
                ReceiptId::from_bytes([0x77; 16]),
                dev_lease_id,
                d_quarantined.holder_node(),
                d_quarantined.holder_node_boot_generation(),
                d_quarantined.exclusivity_epoch(),
                d_quarantined.fencing_token(),
                d_quarantined.reset_generation(),
            );
            let d_receipt = DeviceReconciliationReceipt::for_lease(
                ReceiptId::from_bytes([0x78; 16]),
                &d_quarantined,
                true,
                true,
                true,
                d_reset,
                d_ack,
            );
            dev_grantor
                .reconcile_quarantined(&fence_e2, d_receipt)
                .expect("reconcile durable device");

            (q_receipt, c_receipt, d_receipt)
        };

        drop(store);

        // Reopen from disk and verify rehydrated state and reconciliation receipts
        let reopened = Arc::new(SqliteLeaseStore::open(&db_path).expect("reopen sqlite store"));
        let quota_grantor =
            QuotaLeaseGrantor::open_with_store(Arc::clone(&reopened), Arc::clone(authority), 500)
                .expect("rehydrate quota grantor");
        let cap_grantor =
            CapacityLeaseGrantor::open_with_store(Arc::clone(&reopened), Arc::clone(authority), 32)
                .expect("rehydrate capacity grantor");
        let dev_grantor = ExclusiveDeviceLeaseGrantor::open_with_store(
            Arc::clone(&reopened),
            Arc::clone(authority),
            dev_id,
        )
        .expect("rehydrate device grantor");

        let q_lease = quota_grantor.query(quota_id).expect("rehydrated quota");
        assert_eq!(q_lease.state(), QuotaLeaseState::Closed);
        assert_eq!(q_lease.spent(), 90);
        assert_eq!(q_lease.returned(), 110);
        assert_eq!(quota_grantor.available(), 410);
        assert_eq!(
            quota_grantor.reconciliation_receipts(quota_id),
            &[expected_q_receipt]
        );

        let c_lease = cap_grantor.query(cap_id).expect("rehydrated capacity");
        assert_eq!(c_lease.state(), CapacityLeaseState::Returned);
        assert_eq!(cap_grantor.pool_remaining(), 32);
        assert_eq!(
            cap_grantor.reconciliation_receipt(cap_id),
            Some(expected_c_receipt)
        );

        let d_lease = dev_grantor
            .query(dev_lease_id)
            .expect("rehydrated device lease");
        assert_eq!(d_lease.state(), ExclusiveDeviceLeaseState::Returned);
        assert!(dev_grantor.is_free());
        assert_eq!(
            dev_grantor.reconciliation_receipt(dev_lease_id),
            Some(expected_d_receipt)
        );
    });
}

#[test]
fn cross_node_quota_migration_requires_drained_source_and_forbids_partition_migration() {
    with_cell_authority(|authority| {
        let mut grantor = QuotaLeaseGrantor::open(Arc::clone(authority), 500);
        let fence = authority.fence();

        let source_id = QuotaLeaseId::from_bytes([0x81; 16]);
        let target_id = QuotaLeaseId::from_bytes([0x82; 16]);
        let source = grantor
            .grant(&fence, source_id, 200, None)
            .expect("grant source lease");
        grantor
            .activate(&fence, source_id)
            .expect("activate source lease");
        let active_source = grantor
            .report_spend(&fence, source_id, 50)
            .expect("spend 50 on source");

        let ack = FenceBarrierAck::for_quota(&active_source, GATEWAY_ID, 50);

        // Attempting to migrate while source is still Active (not drained) must fail
        let err = grantor
            .migrate_lease(&fence, source_id, &ack, target_id, 150, None)
            .expect_err("undrained source lease must not migrate");
        assert!(matches!(
            err,
            QuotaMigrationError::SourceNotDrainedOrClosed {
                state: QuotaLeaseState::Active,
                ..
            }
        ));

        // Drain source lease (ACTIVE -> CLOSING) and migrate remaining 150 units
        let closing_source = grantor
            .begin_close(&fence, source_id)
            .expect("begin_close source lease");
        let closing_ack = FenceBarrierAck::for_quota(&closing_source, GATEWAY_ID, 50);

        let outcome = grantor
            .migrate_lease(&fence, source_id, &closing_ack, target_id, 150, None)
            .expect("migrate drained quota lease");
        assert_eq!(outcome.closed_source.state(), QuotaLeaseState::Closed);
        assert_eq!(outcome.closed_source.returned(), 150);
        assert_eq!(outcome.target_grant.lease_id(), target_id);
        assert_eq!(outcome.target_grant.face_value(), 150);
        assert_eq!(grantor.available(), 300);

        // Quarantined lease (in partition) must fail migration with SourceQuarantinedInPartition
        let fence_e2 = grantor
            .advance_epoch_and_quarantine()
            .expect("quarantine target lease on partition");
        let quarantined_target = grantor.query(target_id).expect("quarantined target");
        let q_ack = FenceBarrierAck::for_quota(&quarantined_target, GATEWAY_ID, 0);
        let err = grantor
            .migrate_lease(
                &fence_e2,
                target_id,
                &q_ack,
                QuotaLeaseId::from_bytes([0x83; 16]),
                150,
                None,
            )
            .expect_err("partitioned quarantined lease must refuse direct migration");
        assert!(matches!(
            err,
            QuotaMigrationError::SourceQuarantinedInPartition { lease_id } if lease_id == target_id
        ));

        let _ = source;
    });
}

#[test]
fn cross_node_capacity_migration_requires_cleanup_and_mints_new_grant() {
    with_cell_authority(|authority| {
        let mut grantor = CapacityLeaseGrantor::open(Arc::clone(authority), 32);
        let fence = authority.fence();

        let source_id = CapacityLeaseId::from_bytes([0x91; 16]);
        let target_id = CapacityLeaseId::from_bytes([0x92; 16]);
        let source = grantor
            .grant(&fence, source_id, 12)
            .expect("grant source capacity lease");
        assert_eq!(grantor.pool_remaining(), 20);

        let barrier = FenceBarrierAck::for_capacity(&source, GATEWAY_ID);
        let cleanup = CleanupAttestation::for_lease(&source, 12, true);
        let reconcile_receipt = CapacityReconciliationReceipt::for_lease(
            ReceiptId::from_bytes([0x93; 16]),
            &source,
            cleanup,
            barrier,
        );

        // Requesting target amount > source amount must fail
        let err = grantor
            .migrate_lease(&fence, reconcile_receipt, target_id, 16)
            .expect_err("target exceeding source amount must fail");
        assert!(matches!(
            err,
            CapacityMigrationError::TargetExceedsSourceAmount {
                requested: 16,
                source_amount: 12
            }
        ));

        let outcome = grantor
            .migrate_lease(&fence, reconcile_receipt, target_id, 12)
            .expect("migrate capacity lease");
        assert_eq!(
            outcome.returned_source.state(),
            CapacityLeaseState::Returned
        );
        assert_eq!(outcome.target_grant.capacity_lease_id(), target_id);
        assert_eq!(outcome.target_grant.amount(), 12);
        assert_eq!(grantor.pool_remaining(), 20);
    });
}

#[test]
fn cross_node_device_migration_requires_epoch_bump_and_hardware_reset() {
    with_cell_authority(|authority| {
        let device_id = DeviceId::from_bytes([0xA1; 16]);
        let mut grantor = ExclusiveDeviceLeaseGrantor::open(Arc::clone(authority), device_id);
        let fence_e1 = authority.fence();

        let source_id = ExclusiveDeviceLeaseId::from_bytes([0xA2; 16]);
        let target_id = ExclusiveDeviceLeaseId::from_bytes([0xA3; 16]);
        let source = grantor
            .grant(&fence_e1, source_id)
            .expect("grant source device lease");

        let barrier = FenceBarrierAck::for_device(&source, GATEWAY_ID);
        let reset = DeviceResetReceipt::new(
            ReceiptId::from_bytes([0xA4; 16]),
            source_id,
            source.holder_node(),
            source.holder_node_boot_generation(),
            source.exclusivity_epoch(),
            source.fencing_token(),
            source.reset_generation(),
        );
        let reconcile_receipt = DeviceReconciliationReceipt::for_lease(
            ReceiptId::from_bytes([0xA5; 16]),
            &source,
            true,
            true,
            true,
            reset,
            barrier,
        );

        // Attempting device migration under the same epoch must fail ExclusivityEpochNotBumped
        let err = grantor
            .migrate_lease(&fence_e1, reconcile_receipt, target_id)
            .expect_err("device migration without epoch bump must fail");
        assert!(matches!(
            err,
            DeviceMigrationError::ExclusivityEpochNotBumped { .. }
        ));

        // Advance CellAuthority epoch and migrate device lease
        let fence_e2 = authority
            .advance_epoch()
            .expect("advance cell epoch for device migration");
        let outcome = grantor
            .migrate_lease(&fence_e2, reconcile_receipt, target_id)
            .expect("migrate device lease after epoch bump");
        assert_eq!(
            outcome.returned_source.state(),
            ExclusiveDeviceLeaseState::Returned
        );
        assert_eq!(outcome.target_grant.device_lease_id(), target_id);
        assert_eq!(outcome.target_grant.exclusivity_epoch(), fence_e2.epoch());
        assert!(outcome.target_grant.fencing_token() > source.fencing_token());
        assert!(!grantor.is_free());
    });
}

#[test]
fn wal_loss_fences_boot_generation_and_forbids_guessed_remaining_across_families() {
    with_cell_authority(|authority| {
        let mut quota_grantor = QuotaLeaseGrantor::open(Arc::clone(authority), 500);
        let mut cap_grantor = CapacityLeaseGrantor::open(Arc::clone(authority), 16);
        let dev_id = DeviceId::from_bytes([0xB1; 16]);
        let mut dev_grantor = ExclusiveDeviceLeaseGrantor::open(Arc::clone(authority), dev_id);

        let fence_e1 = authority.fence();
        let lost_boot = authority.node_boot_generation();
        let reconnected_boot = lost_boot.checked_next().expect("next boot generation");

        let q_id = QuotaLeaseId::from_bytes([0xB2; 16]);
        let c_id = CapacityLeaseId::from_bytes([0xB3; 16]);
        let d_id = ExclusiveDeviceLeaseId::from_bytes([0xB4; 16]);

        quota_grantor
            .grant(&fence_e1, q_id, 100, None)
            .expect("grant quota");
        cap_grantor
            .grant(&fence_e1, c_id, 4)
            .expect("grant capacity");
        dev_grantor.grant(&fence_e1, d_id).expect("grant device");

        // Stale/non-advancing boot generation on WAL loss must fail closed
        let err = quota_grantor
            .fence_boot_generation_on_wal_loss(lost_boot, lost_boot)
            .expect_err("non-advanced boot generation must fail");
        assert!(matches!(
            err,
            QuotaWalLossError::BootGenerationNotAdvanced { .. }
        ));

        // Node reconnects with higher boot generation after local WAL loss
        let (fence_e2, quarantined_q) = quota_grantor
            .handle_node_wal_loss(lost_boot, reconnected_boot)
            .expect("handle quota WAL loss");
        let quarantined_c = cap_grantor
            .fence_boot_generation_on_wal_loss(&fence_e2, lost_boot, reconnected_boot)
            .expect("handle capacity WAL loss");
        let quarantined_d = dev_grantor
            .fence_boot_generation_on_wal_loss(&fence_e2, lost_boot, reconnected_boot)
            .expect("handle device WAL loss");

        assert_eq!(quarantined_q, vec![q_id]);
        assert_eq!(quarantined_c, vec![c_id]);
        assert_eq!(quarantined_d, vec![d_id]);

        assert_eq!(
            quota_grantor.query(q_id).expect("q").state(),
            QuotaLeaseState::Quarantined
        );
        assert_eq!(
            cap_grantor.query(c_id).expect("c").state(),
            CapacityLeaseState::Quarantined
        );
        assert_eq!(
            dev_grantor.query(d_id).expect("d").state(),
            ExclusiveDeviceLeaseState::Quarantined
        );

        // Guessing remaining balance from face_value is unconditionally forbidden ([LEASE-WAL-001])
        let err = quota_grantor
            .refuse_guessed_remaining_on_wal_loss(q_id, 75)
            .expect_err("guessed remaining balance must be rejected on WAL loss");
        assert_eq!(
            err,
            QuotaWalLossError::GuessedRemainingForbidden {
                lease_id: q_id,
                face_value: 100,
                guessed_remaining: 75,
            }
        );

        let _ = Generation::INITIAL;
    });
}
