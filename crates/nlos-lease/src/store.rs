//! Durable `SQLite` (`WAL + FULL`) storage backend for single-Cell
//! `QuotaLease`, `CapacityLease`, and `ExclusiveDeviceLease` grantors.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_cell::{CellAuthority, CellEpoch, CellError, CellFencingToken, CellIdentity};
use nlos_types::{
    CapacityLeaseId, DeviceId, ExclusiveDeviceLeaseId, Generation, QuotaLeaseId, ReceiptId,
    SchedulerDomainId,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::reconcile::{
    verify_capacity_fence_ack, verify_cleanup_attestation, verify_device_fence_ack,
    verify_quota_fence_ack,
};
use crate::{
    CapacityLeaseGrant, CapacityLeaseState, CapacityReconciliationReceipt, CleanupAttestation,
    DeviceReconciliationReceipt, DeviceResetReceipt, ExclusiveDeviceLeaseGrant,
    ExclusiveDeviceLeaseState, FenceBarrierAck, FenceScope, HostAttachReceipt, LeaseInstant,
    QuotaLeaseGrant, QuotaLeaseState, QuotaReconciliationReceipt, attach_receipt_binds_lease,
    receipt_binds_lease,
};

/// Durable schema version for `SqliteLeaseStore`.
pub const SCHEMA_VERSION: i64 = 1;

const EXPECTED_TABLE_COUNT: i64 = 11;
const EXPECTED_TRIGGER_COUNT: i64 = 22;

/// Fail-closed typed errors of the durable `SQLite` lease store.
#[derive(Debug)]
pub enum LeaseStoreError {
    /// Storage failure surfaced by `SQLite`.
    Sqlite(rusqlite::Error),
    /// Filesystem failure outside `SQLite` (directory creation).
    Io(std::io::Error),
    /// `SQLite` cannot provide `WAL` + `FULL` durability on this platform.
    DurabilityUnavailable {
        /// Journal mode reported by `SQLite`.
        journal_mode: String,
        /// Synchronous level reported by `SQLite`.
        synchronous: i64,
    },
    /// The stored `user_version` is unknown to this build.
    SchemaVersionUnsupported(i64),
    /// A durable invariant is violated: missing pool/head row, conservation
    /// drift, identity mismatch, or tampered lease/receipt record.
    CorruptRecord(&'static str),
    /// The store writer lock is poisoned.
    LockPoisoned,
    /// The shared `CellAuthority` refused an epoch catch-up on reopen.
    Cell(CellError),
}

impl LeaseStoreError {
    /// Static diagnostic classification used by copyable grant/ledger errors.
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::Sqlite(_) => "sqlite storage failure",
            Self::Io(_) => "filesystem I/O failure",
            Self::DurabilityUnavailable { .. } => "WAL/FULL durability unavailable",
            Self::SchemaVersionUnsupported(_) => "unsupported lease store schema version",
            Self::CorruptRecord(reason) => reason,
            Self::LockPoisoned => "lease store writer lock poisoned",
            Self::Cell(_) => "cell authority epoch catch-up failed",
        }
    }

    pub(crate) fn into_cell_error(self) -> CellError {
        match self {
            Self::Cell(err) => err,
            Self::LockPoisoned => CellError::LockPoisoned,
            Self::CorruptRecord(_) | Self::SchemaVersionUnsupported(_) => CellError::DataDirCorrupt,
            Self::Sqlite(_) | Self::Io(_) | Self::DurabilityUnavailable { .. } => {
                CellError::DataDirUnavailable
            }
        }
    }
}

impl fmt::Display for LeaseStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite lease store failure: {error}"),
            Self::Io(error) => write!(formatter, "lease store I/O failure: {error}"),
            Self::DurabilityUnavailable {
                journal_mode,
                synchronous,
            } => write!(
                formatter,
                "WAL/FULL durability unavailable: journal_mode={journal_mode}, synchronous={synchronous}"
            ),
            Self::SchemaVersionUnsupported(version) => {
                write!(
                    formatter,
                    "unsupported lease store schema version {version}"
                )
            }
            Self::CorruptRecord(reason) => write!(formatter, "corrupt lease record: {reason}"),
            Self::LockPoisoned => formatter.write_str("lease store writer lock is poisoned"),
            Self::Cell(error) => write!(formatter, "cell authority catch-up failed: {error}"),
        }
    }
}

impl Error for LeaseStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Cell(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for LeaseStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<CellError> for LeaseStoreError {
    fn from(error: CellError) -> Self {
        Self::Cell(error)
    }
}

/// Durable `SQLite` (`WAL + FULL`) store backing `QuotaLeaseGrantor`,
/// `CapacityLeaseGrantor`, and `ExclusiveDeviceLeaseGrantor`.
#[derive(Debug)]
pub struct SqliteLeaseStore {
    connection: Mutex<Connection>,
}

pub(crate) type QuotaRehydration = (
    u64,
    HashMap<QuotaLeaseId, QuotaLeaseGrant>,
    HashMap<QuotaLeaseId, Vec<QuotaReconciliationReceipt>>,
);

pub(crate) type CapacityRehydration = (
    u64,
    HashMap<CapacityLeaseId, CapacityLeaseGrant>,
    HashMap<CapacityLeaseId, HostAttachReceipt>,
    HashMap<CapacityLeaseId, CapacityReconciliationReceipt>,
);

pub(crate) type DeviceRehydration = (
    bool,
    Generation,
    HashMap<ExclusiveDeviceLeaseId, ExclusiveDeviceLeaseGrant>,
    Vec<DeviceResetReceipt>,
    HashMap<ExclusiveDeviceLeaseId, DeviceReconciliationReceipt>,
);

impl SqliteLeaseStore {
    /// Opens or initializes a durable `SQLite` lease store at `path` with
    /// `WAL + FULL` pragmas and DDL immutability triggers.
    ///
    /// If `path` is an existing directory, `<path>/leases.db` is opened;
    /// otherwise `path` is treated as the database file path and its parent
    /// directory is created if needed.
    ///
    /// # Errors
    ///
    /// Fails closed when `SQLite` cannot establish `WAL + FULL` durability,
    /// when the stored `user_version` is unsupported, or when the schema is
    /// partially initialized/corrupt.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LeaseStoreError> {
        let db_path = resolve_db_path(path.as_ref())?;
        let mut connection = Connection::open(db_path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;

        let journal_mode: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let synchronous: i64 =
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 {
            return Err(LeaseStoreError::DurabilityUnavailable {
                journal_mode,
                synchronous,
            });
        }

        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => migrate_v1(&mut connection)?,
            SCHEMA_VERSION => verify_schema_objects(&connection)?,
            other => return Err(LeaseStoreError::SchemaVersionUnsupported(other)),
        }

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, LeaseStoreError> {
        self.connection
            .lock()
            .map_err(|_| LeaseStoreError::LockPoisoned)
    }

    pub(crate) fn open_or_rehydrate_quota(
        &self,
        authority: &CellAuthority,
        initial_available: u64,
    ) -> Result<QuotaRehydration, LeaseStoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pool_row: Option<(Vec<u8>, i64, i64, i64, i64)> = tx
            .query_row(
                "SELECT cell_id, initial_available, available, last_epoch, last_fencing_token
                 FROM quota_pool_state WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;

        let Some((cell_bytes, raw_initial, raw_available, raw_epoch, raw_token)) = pool_row else {
            let orphan_count: i64 =
                tx.query_row("SELECT COUNT(*) FROM quota_leases", [], |row| row.get(0))?;
            if orphan_count != 0 {
                return Err(LeaseStoreError::CorruptRecord(
                    "orphan quota_leases without quota_pool_state",
                ));
            }
            tx.execute(
                "INSERT INTO quota_pool_state (
                    singleton, cell_id, initial_available, available, last_epoch, last_fencing_token
                 ) VALUES (1, ?1, ?2, ?3, ?4, ?5)",
                params![
                    authority.identity().as_bytes().as_slice(),
                    encode_u64(initial_available, "quota initial_available")?,
                    encode_u64(initial_available, "quota available")?,
                    encode_u64(authority.epoch().get(), "quota last_epoch")?,
                    encode_u64(authority.fencing_token().get(), "quota last_fencing_token")?,
                ],
            )?;
            tx.commit()?;
            return Ok((initial_available, HashMap::new(), HashMap::new()));
        };

        let stored_cell = decode_cell_identity(&cell_bytes, "quota_pool_state.cell_id")?;
        if stored_cell != authority.identity() {
            return Err(LeaseStoreError::CorruptRecord(
                "quota_pool_state cell identity mismatch",
            ));
        }
        let stored_initial = decode_u64(raw_initial, "quota_pool_state.initial_available")?;
        let stored_available = decode_u64(raw_available, "quota_pool_state.available")?;
        let stored_epoch = decode_epoch(raw_epoch, "quota_pool_state.last_epoch")?;
        let stored_token = decode_fencing_token(raw_token, "quota_pool_state.last_fencing_token")?;

        let (leases, reconciliation_receipts, net_deducted, max_lease_epoch) =
            load_and_validate_quota_leases(&tx, stored_cell)?;
        let total =
            stored_available
                .checked_add(net_deducted)
                .ok_or(LeaseStoreError::CorruptRecord(
                    "quota pool conservation overflow",
                ))?;
        if total != stored_initial {
            return Err(LeaseStoreError::CorruptRecord(
                "quota pool conservation invariant violated",
            ));
        }
        if stored_epoch.get() < max_lease_epoch {
            return Err(LeaseStoreError::CorruptRecord(
                "quota_pool_state last_epoch precedes committed lease epoch",
            ));
        }

        while authority.epoch() < stored_epoch {
            authority.advance_epoch()?;
        }
        if authority.epoch() == stored_epoch && authority.fencing_token() != stored_token {
            return Err(LeaseStoreError::CorruptRecord(
                "quota_pool_state fencing token mismatch",
            ));
        }
        if authority.epoch() > stored_epoch {
            tx.execute(
                "UPDATE quota_pool_state
                 SET last_epoch = ?1, last_fencing_token = ?2
                 WHERE singleton = 1",
                params![
                    encode_u64(authority.epoch().get(), "quota last_epoch")?,
                    encode_u64(authority.fencing_token().get(), "quota last_fencing_token")?,
                ],
            )?;
        }

        tx.commit()?;
        Ok((stored_available, leases, reconciliation_receipts))
    }

    pub(crate) fn persist_quota_grant(
        &self,
        grant: &QuotaLeaseGrant,
        expected_available: u64,
        next_available: u64,
    ) -> Result<(), LeaseStoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE quota_pool_state
             SET available = ?1,
                 last_epoch = CASE WHEN ?2 > last_epoch THEN ?2 ELSE last_epoch END,
                 last_fencing_token = CASE WHEN ?3 > last_fencing_token THEN ?3 ELSE last_fencing_token END
             WHERE singleton = 1 AND available = ?4",
            params![
                encode_u64(next_available, "quota next_available")?,
                encode_u64(grant.epoch.get(), "quota epoch")?,
                encode_u64(grant.fencing_token.get(), "quota fencing_token")?,
                encode_u64(expected_available, "quota expected_available")?,
            ],
        )?;
        if changed != 1 {
            return Err(LeaseStoreError::CorruptRecord("quota_pool_state CAS lost"));
        }
        let expires_at: Option<i64> = grant
            .expires_at
            .map(|inst| encode_u64(inst.get(), "quota expires_at"))
            .transpose()?;
        tx.execute(
            "INSERT INTO quota_leases (
                lease_id, cell_id, node_boot_generation, face_value, remaining,
                spent, returned, epoch, fencing_token, fence_scope_cell_id,
                expires_at, state
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                grant.lease_id.as_bytes().as_slice(),
                grant.cell.as_bytes().as_slice(),
                encode_u64(grant.node_boot_generation.get(), "node_boot_generation")?,
                encode_u64(grant.face_value, "face_value")?,
                encode_u64(grant.remaining, "remaining")?,
                encode_u64(grant.spent, "spent")?,
                encode_u64(grant.returned, "returned")?,
                encode_u64(grant.epoch.get(), "epoch")?,
                encode_u64(grant.fencing_token.get(), "fencing_token")?,
                grant.fence_scope.as_cell().as_bytes().as_slice(),
                expires_at,
                encode_quota_state(grant.state),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn persist_quota_state_transition(
        &self,
        lease_id: QuotaLeaseId,
        expected_state: QuotaLeaseState,
        next: &QuotaLeaseGrant,
        pool_update: Option<(u64, u64)>,
    ) -> Result<(), LeaseStoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE quota_leases
             SET remaining = ?1, spent = ?2, returned = ?3, state = ?4
             WHERE lease_id = ?5 AND state = ?6",
            params![
                encode_u64(next.remaining, "remaining")?,
                encode_u64(next.spent, "spent")?,
                encode_u64(next.returned, "returned")?,
                encode_quota_state(next.state),
                lease_id.as_bytes().as_slice(),
                encode_quota_state(expected_state),
            ],
        )?;
        if changed != 1 {
            return Err(LeaseStoreError::CorruptRecord(
                "quota_leases state CAS lost",
            ));
        }
        if let Some((expected_available, next_available)) = pool_update {
            let pool_changed = tx.execute(
                "UPDATE quota_pool_state SET available = ?1 WHERE singleton = 1 AND available = ?2",
                params![
                    encode_u64(next_available, "next_available")?,
                    encode_u64(expected_available, "expected_available")?,
                ],
            )?;
            if pool_changed != 1 {
                return Err(LeaseStoreError::CorruptRecord("quota_pool_state CAS lost"));
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn persist_quota_usage(
        &self,
        expected: &QuotaLeaseGrant,
        next: &QuotaLeaseGrant,
    ) -> Result<(), LeaseStoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE quota_leases
             SET spent = ?1, remaining = ?2
             WHERE lease_id = ?3 AND state = 'ACTIVE' AND spent = ?4",
            params![
                encode_u64(next.spent, "next.spent")?,
                encode_u64(next.remaining, "next.remaining")?,
                next.lease_id.as_bytes().as_slice(),
                encode_u64(expected.spent, "expected.spent")?,
            ],
        )?;
        if changed != 1 {
            return Err(LeaseStoreError::CorruptRecord(
                "quota_leases usage CAS lost",
            ));
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn persist_quota_sweep(
        &self,
        to_fence: &[(QuotaLeaseId, QuotaLeaseState)],
    ) -> Result<(), LeaseStoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (lease_id, expected_state) in to_fence {
            let changed = tx.execute(
                "UPDATE quota_leases SET state = 'FENCED' WHERE lease_id = ?1 AND state = ?2",
                params![
                    lease_id.as_bytes().as_slice(),
                    encode_quota_state(*expected_state),
                ],
            )?;
            if changed != 1 {
                return Err(LeaseStoreError::CorruptRecord(
                    "quota_leases sweep CAS lost",
                ));
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn persist_quota_epoch_advance(
        &self,
        next_epoch: CellEpoch,
        next_token: CellFencingToken,
        to_quarantine: &[(QuotaLeaseId, QuotaLeaseState)],
    ) -> Result<(), LeaseStoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE quota_pool_state
             SET last_epoch = ?1, last_fencing_token = ?2
             WHERE singleton = 1",
            params![
                encode_u64(next_epoch.get(), "next_epoch")?,
                encode_u64(next_token.get(), "next_token")?,
            ],
        )?;
        if changed != 1 {
            return Err(LeaseStoreError::CorruptRecord(
                "quota_pool_state epoch advance CAS lost",
            ));
        }
        for (lease_id, expected_state) in to_quarantine {
            let row_changed = tx.execute(
                "UPDATE quota_leases SET state = 'QUARANTINED' WHERE lease_id = ?1 AND state = ?2",
                params![
                    lease_id.as_bytes().as_slice(),
                    encode_quota_state(*expected_state),
                ],
            )?;
            if row_changed != 1 {
                return Err(LeaseStoreError::CorruptRecord(
                    "quota_leases quarantine CAS lost",
                ));
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn persist_quota_reconcile(
        &self,
        expected_state: QuotaLeaseState,
        next: &QuotaLeaseGrant,
        receipt: &QuotaReconciliationReceipt,
        pool_update: (u64, u64),
    ) -> Result<(), LeaseStoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO quota_reconciliation_receipts (
                lease_id, reconcile_seq, receipt_id, cell_id, node_boot_generation,
                epoch, fencing_token, face_value, high_water_spent, settled_spent,
                returned_to_available, remaining_quarantined, ack_gateway_id,
                ack_observed_high_water, ack_digest
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                receipt.lease_id.as_bytes().as_slice(),
                encode_u64(receipt.reconcile_seq, "reconcile_seq")?,
                receipt.receipt_id.as_bytes().as_slice(),
                receipt.cell.as_bytes().as_slice(),
                encode_u64(receipt.node_boot_generation.get(), "node_boot_generation")?,
                encode_u64(receipt.epoch.get(), "epoch")?,
                encode_u64(receipt.fencing_token.get(), "fencing_token")?,
                encode_u64(receipt.face_value, "face_value")?,
                encode_u64(receipt.high_water_spent, "high_water_spent")?,
                encode_u64(receipt.settled_spent, "settled_spent")?,
                encode_u64(receipt.returned_to_available, "returned_to_available")?,
                encode_u64(receipt.remaining_quarantined, "remaining_quarantined")?,
                receipt.fence_barrier_ack.gateway_id().as_slice(),
                encode_u64(
                    receipt.fence_barrier_ack.observed_high_water(),
                    "ack_observed_high_water"
                )?,
                receipt.fence_barrier_ack.ack_digest().as_slice(),
            ],
        )?;
        let changed = tx.execute(
            "UPDATE quota_leases
             SET remaining = ?1, spent = ?2, returned = ?3, state = ?4
             WHERE lease_id = ?5 AND state = ?6",
            params![
                encode_u64(next.remaining, "remaining")?,
                encode_u64(next.spent, "spent")?,
                encode_u64(next.returned, "returned")?,
                encode_quota_state(next.state),
                next.lease_id.as_bytes().as_slice(),
                encode_quota_state(expected_state),
            ],
        )?;
        if changed != 1 {
            return Err(LeaseStoreError::CorruptRecord(
                "quota_leases reconcile CAS lost",
            ));
        }
        let (expected_available, next_available) = pool_update;
        let pool_changed = tx.execute(
            "UPDATE quota_pool_state SET available = ?1 WHERE singleton = 1 AND available = ?2",
            params![
                encode_u64(next_available, "next_available")?,
                encode_u64(expected_available, "expected_available")?,
            ],
        )?;
        if pool_changed != 1 {
            return Err(LeaseStoreError::CorruptRecord("quota_pool_state CAS lost"));
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn open_or_rehydrate_capacity(
        &self,
        authority: &CellAuthority,
        initial_pool: u64,
    ) -> Result<CapacityRehydration, LeaseStoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pool_row: Option<(Vec<u8>, i64, i64)> = tx
            .query_row(
                "SELECT cell_id, initial_pool, pool_remaining
                 FROM capacity_pool_state WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;

        let Some((cell_bytes, raw_initial, raw_remaining)) = pool_row else {
            let orphan_count: i64 =
                tx.query_row("SELECT COUNT(*) FROM capacity_leases", [], |row| row.get(0))?;
            if orphan_count != 0 {
                return Err(LeaseStoreError::CorruptRecord(
                    "orphan capacity_leases without capacity_pool_state",
                ));
            }
            tx.execute(
                "INSERT INTO capacity_pool_state (singleton, cell_id, initial_pool, pool_remaining)
                 VALUES (1, ?1, ?2, ?3)",
                params![
                    authority.identity().as_bytes().as_slice(),
                    encode_u64(initial_pool, "capacity initial_pool")?,
                    encode_u64(initial_pool, "capacity pool_remaining")?,
                ],
            )?;
            tx.commit()?;
            return Ok((initial_pool, HashMap::new(), HashMap::new(), HashMap::new()));
        };

        let stored_cell = decode_cell_identity(&cell_bytes, "capacity_pool_state.cell_id")?;
        if stored_cell != authority.identity() {
            return Err(LeaseStoreError::CorruptRecord(
                "capacity_pool_state cell identity mismatch",
            ));
        }
        let stored_initial = decode_u64(raw_initial, "capacity_pool_state.initial_pool")?;
        let stored_remaining = decode_u64(raw_remaining, "capacity_pool_state.pool_remaining")?;

        let (leases, attach_receipts, reconciliation_receipts, reserved_total) =
            load_and_validate_capacity_leases(&tx, stored_cell)?;
        let total =
            stored_remaining
                .checked_add(reserved_total)
                .ok_or(LeaseStoreError::CorruptRecord(
                    "capacity pool conservation overflow",
                ))?;
        if total != stored_initial {
            return Err(LeaseStoreError::CorruptRecord(
                "capacity pool conservation invariant violated",
            ));
        }

        tx.commit()?;
        Ok((
            stored_remaining,
            leases,
            attach_receipts,
            reconciliation_receipts,
        ))
    }

    pub(crate) fn persist_capacity_grant(
        &self,
        grant: &CapacityLeaseGrant,
        expected_pool: u64,
        next_pool: u64,
    ) -> Result<(), LeaseStoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE capacity_pool_state
             SET pool_remaining = ?1
             WHERE singleton = 1 AND pool_remaining = ?2",
            params![
                encode_u64(next_pool, "capacity next_pool")?,
                encode_u64(expected_pool, "capacity expected_pool")?,
            ],
        )?;
        if changed != 1 {
            return Err(LeaseStoreError::CorruptRecord(
                "capacity_pool_state CAS lost",
            ));
        }
        tx.execute(
            "INSERT INTO capacity_leases (
                capacity_lease_id, target_node, target_node_boot_generation, amount,
                capacity_epoch, fencing_token, fence_scope_cell_id, state
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                grant.capacity_lease_id.as_bytes().as_slice(),
                grant.target_node.as_bytes().as_slice(),
                encode_u64(
                    grant.target_node_boot_generation.get(),
                    "target_node_boot_generation"
                )?,
                encode_u64(grant.amount, "amount")?,
                encode_u64(grant.capacity_epoch.get(), "capacity_epoch")?,
                encode_u64(grant.fencing_token.get(), "fencing_token")?,
                grant.fence_scope.as_cell().as_bytes().as_slice(),
                encode_capacity_state(grant.state),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn persist_capacity_state_transition(
        &self,
        capacity_lease_id: CapacityLeaseId,
        expected_state: CapacityLeaseState,
        next_state: CapacityLeaseState,
        pool_update: Option<(u64, u64)>,
    ) -> Result<(), LeaseStoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE capacity_leases
             SET state = ?1
             WHERE capacity_lease_id = ?2 AND state = ?3",
            params![
                encode_capacity_state(next_state),
                capacity_lease_id.as_bytes().as_slice(),
                encode_capacity_state(expected_state),
            ],
        )?;
        if changed != 1 {
            return Err(LeaseStoreError::CorruptRecord(
                "capacity_leases state CAS lost",
            ));
        }
        if let Some((expected_pool, next_pool)) = pool_update {
            let pool_changed = tx.execute(
                "UPDATE capacity_pool_state
                 SET pool_remaining = ?1
                 WHERE singleton = 1 AND pool_remaining = ?2",
                params![
                    encode_u64(next_pool, "capacity next_pool")?,
                    encode_u64(expected_pool, "capacity expected_pool")?,
                ],
            )?;
            if pool_changed != 1 {
                return Err(LeaseStoreError::CorruptRecord(
                    "capacity_pool_state CAS lost",
                ));
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn persist_capacity_activate(
        &self,
        capacity_lease_id: CapacityLeaseId,
        receipt: &HostAttachReceipt,
    ) -> Result<(), LeaseStoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE capacity_leases
             SET state = 'ACTIVE'
             WHERE capacity_lease_id = ?1 AND state = 'TARGET_PREPARED'",
            params![capacity_lease_id.as_bytes().as_slice()],
        )?;
        if changed != 1 {
            return Err(LeaseStoreError::CorruptRecord(
                "capacity_leases activate CAS lost",
            ));
        }
        tx.execute(
            "INSERT INTO capacity_attach_receipts (
                capacity_lease_id, receipt_id, target_node, target_node_boot_generation,
                capacity_epoch, fencing_token, attached_amount
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                receipt.capacity_lease_id.as_bytes().as_slice(),
                receipt.receipt_id.as_bytes().as_slice(),
                receipt.target_node.as_bytes().as_slice(),
                encode_u64(
                    receipt.target_node_boot_generation.get(),
                    "receipt.target_node_boot_generation"
                )?,
                encode_u64(receipt.capacity_epoch.get(), "receipt.capacity_epoch")?,
                encode_u64(receipt.fencing_token.get(), "receipt.fencing_token")?,
                encode_u64(receipt.attached_amount, "receipt.attached_amount")?,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn persist_capacity_reconcile(
        &self,
        capacity_lease_id: CapacityLeaseId,
        expected_state: CapacityLeaseState,
        receipt: &CapacityReconciliationReceipt,
        pool_update: (u64, u64),
    ) -> Result<(), LeaseStoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO capacity_reconciliation_receipts (
                capacity_lease_id, receipt_id, target_node, target_node_boot_generation,
                capacity_epoch, fencing_token, amount, cleaned_amount,
                holder_processes_terminated, cleanup_attestation_digest,
                ack_gateway_id, ack_observed_high_water, ack_digest
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                receipt.capacity_lease_id.as_bytes().as_slice(),
                receipt.receipt_id.as_bytes().as_slice(),
                receipt.target_node.as_bytes().as_slice(),
                encode_u64(
                    receipt.target_node_boot_generation.get(),
                    "target_node_boot_generation"
                )?,
                encode_u64(receipt.capacity_epoch.get(), "capacity_epoch")?,
                encode_u64(receipt.fencing_token.get(), "fencing_token")?,
                encode_u64(receipt.amount, "amount")?,
                encode_u64(
                    receipt.cleanup_attestation.cleaned_amount(),
                    "cleaned_amount"
                )?,
                i64::from(receipt.cleanup_attestation.holder_processes_terminated()),
                receipt.cleanup_attestation.attestation_digest().as_slice(),
                receipt.fence_barrier_ack.gateway_id().as_slice(),
                encode_u64(
                    receipt.fence_barrier_ack.observed_high_water(),
                    "ack_observed_high_water"
                )?,
                receipt.fence_barrier_ack.ack_digest().as_slice(),
            ],
        )?;
        let changed = tx.execute(
            "UPDATE capacity_leases
             SET state = 'RETURNED'
             WHERE capacity_lease_id = ?1 AND state = ?2",
            params![
                capacity_lease_id.as_bytes().as_slice(),
                encode_capacity_state(expected_state),
            ],
        )?;
        if changed != 1 {
            return Err(LeaseStoreError::CorruptRecord(
                "capacity_leases reconcile CAS lost",
            ));
        }
        let (expected_pool, next_pool) = pool_update;
        let pool_changed = tx.execute(
            "UPDATE capacity_pool_state
             SET pool_remaining = ?1
             WHERE singleton = 1 AND pool_remaining = ?2",
            params![
                encode_u64(next_pool, "capacity next_pool")?,
                encode_u64(expected_pool, "capacity expected_pool")?,
            ],
        )?;
        if pool_changed != 1 {
            return Err(LeaseStoreError::CorruptRecord(
                "capacity_pool_state CAS lost",
            ));
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn open_or_rehydrate_device(
        &self,
        authority: &CellAuthority,
        device_id: DeviceId,
    ) -> Result<DeviceRehydration, LeaseStoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let head_row: Option<(Vec<u8>, i64, i64)> = tx
            .query_row(
                "SELECT cell_id, is_free, reset_generation
                 FROM device_head_state WHERE device_id = ?1",
                params![device_id.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;

        let Some((cell_bytes, raw_free, raw_gen)) = head_row else {
            let orphan_leases: i64 = tx.query_row(
                "SELECT COUNT(*) FROM device_leases WHERE device_id = ?1",
                params![device_id.as_bytes().as_slice()],
                |row| row.get(0),
            )?;
            let orphan_receipts: i64 = tx.query_row(
                "SELECT COUNT(*) FROM device_reset_receipts WHERE device_id = ?1",
                params![device_id.as_bytes().as_slice()],
                |row| row.get(0),
            )?;
            if orphan_leases != 0 || orphan_receipts != 0 {
                return Err(LeaseStoreError::CorruptRecord(
                    "orphan device_leases or device_reset_receipts without device_head_state",
                ));
            }
            tx.execute(
                "INSERT INTO device_head_state (device_id, cell_id, is_free, reset_generation)
                 VALUES (?1, ?2, 1, 1)",
                params![
                    device_id.as_bytes().as_slice(),
                    authority.identity().as_bytes().as_slice(),
                ],
            )?;
            tx.commit()?;
            return Ok((
                true,
                Generation::INITIAL,
                HashMap::new(),
                Vec::new(),
                HashMap::new(),
            ));
        };

        let stored_cell = decode_cell_identity(&cell_bytes, "device_head_state.cell_id")?;
        if stored_cell != authority.identity() {
            return Err(LeaseStoreError::CorruptRecord(
                "device_head_state cell identity mismatch",
            ));
        }
        let is_free = match raw_free {
            0 => false,
            1 => true,
            _ => {
                return Err(LeaseStoreError::CorruptRecord(
                    "device_head_state invalid is_free",
                ));
            }
        };
        let reset_generation = decode_generation(raw_gen, "device_head_state.reset_generation")?;

        let (leases, reset_receipts, reconciliation_receipts) = load_and_validate_device_leases(
            &tx,
            device_id,
            stored_cell,
            is_free,
            reset_generation,
        )?;

        tx.commit()?;
        Ok((
            is_free,
            reset_generation,
            leases,
            reset_receipts,
            reconciliation_receipts,
        ))
    }

    pub(crate) fn persist_device_grant(
        &self,
        grant: &ExclusiveDeviceLeaseGrant,
    ) -> Result<(), LeaseStoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE device_head_state
             SET is_free = 0
             WHERE device_id = ?1 AND is_free = 1 AND reset_generation = ?2",
            params![
                grant.device_id.as_bytes().as_slice(),
                encode_u64(grant.reset_generation.get(), "grant.reset_generation")?,
            ],
        )?;
        if changed != 1 {
            return Err(LeaseStoreError::CorruptRecord(
                "device_head_state claim CAS lost",
            ));
        }
        tx.execute(
            "INSERT INTO device_leases (
                device_lease_id, device_id, holder_node, holder_node_boot_generation,
                exclusivity_epoch, fencing_token, fence_scope_cell_id, reset_generation, state
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                grant.device_lease_id.as_bytes().as_slice(),
                grant.device_id.as_bytes().as_slice(),
                grant.holder_node.as_bytes().as_slice(),
                encode_u64(
                    grant.holder_node_boot_generation.get(),
                    "holder_node_boot_generation"
                )?,
                encode_u64(grant.exclusivity_epoch.get(), "exclusivity_epoch")?,
                encode_u64(grant.fencing_token.get(), "fencing_token")?,
                grant.fence_scope.as_cell().as_bytes().as_slice(),
                encode_u64(grant.reset_generation.get(), "reset_generation")?,
                encode_device_state(grant.state),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn persist_device_declare_reset(
        &self,
        device_lease_id: ExclusiveDeviceLeaseId,
    ) -> Result<(), LeaseStoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE device_leases
             SET state = 'RESETTING'
             WHERE device_lease_id = ?1 AND state = 'DEVICE_RESERVED'",
            params![device_lease_id.as_bytes().as_slice()],
        )?;
        if changed != 1 {
            return Err(LeaseStoreError::CorruptRecord(
                "device_leases declare_reset CAS lost",
            ));
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn persist_device_reset_receipt(
        &self,
        device_id: DeviceId,
        receipt: &DeviceResetReceipt,
        expected_gen: Generation,
        next_gen: Generation,
    ) -> Result<(), LeaseStoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let lease_changed = tx.execute(
            "UPDATE device_leases
             SET state = 'RETURNED'
             WHERE device_lease_id = ?1 AND state = 'RESETTING'",
            params![receipt.device_lease_id.as_bytes().as_slice()],
        )?;
        if lease_changed != 1 {
            return Err(LeaseStoreError::CorruptRecord(
                "device_leases submit_reset_receipt CAS lost",
            ));
        }
        let next_seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM device_reset_receipts",
            [],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO device_reset_receipts (
                seq, receipt_id, device_lease_id, device_id, holder_node,
                holder_node_boot_generation, exclusivity_epoch, fencing_token, reset_generation
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                next_seq,
                receipt.receipt_id.as_bytes().as_slice(),
                receipt.device_lease_id.as_bytes().as_slice(),
                device_id.as_bytes().as_slice(),
                receipt.holder_node.as_bytes().as_slice(),
                encode_u64(
                    receipt.holder_node_boot_generation.get(),
                    "receipt.holder_node_boot_generation"
                )?,
                encode_u64(receipt.exclusivity_epoch.get(), "receipt.exclusivity_epoch")?,
                encode_u64(receipt.fencing_token.get(), "receipt.fencing_token")?,
                encode_u64(receipt.reset_generation.get(), "receipt.reset_generation")?,
            ],
        )?;
        let head_changed = tx.execute(
            "UPDATE device_head_state
             SET is_free = 1, reset_generation = ?1
             WHERE device_id = ?2 AND is_free = 0 AND reset_generation = ?3",
            params![
                encode_u64(next_gen.get(), "next_gen")?,
                device_id.as_bytes().as_slice(),
                encode_u64(expected_gen.get(), "expected_gen")?,
            ],
        )?;
        if head_changed != 1 {
            return Err(LeaseStoreError::CorruptRecord(
                "device_head_state reset_generation CAS lost",
            ));
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn persist_device_state_transition(
        &self,
        device_lease_id: ExclusiveDeviceLeaseId,
        expected_state: ExclusiveDeviceLeaseState,
        next_state: ExclusiveDeviceLeaseState,
    ) -> Result<(), LeaseStoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE device_leases
             SET state = ?1
             WHERE device_lease_id = ?2 AND state = ?3",
            params![
                encode_device_state(next_state),
                device_lease_id.as_bytes().as_slice(),
                encode_device_state(expected_state),
            ],
        )?;
        if changed != 1 {
            return Err(LeaseStoreError::CorruptRecord(
                "device_leases state transition CAS lost",
            ));
        }
        tx.commit()?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn persist_device_reconcile(
        &self,
        device_id: DeviceId,
        expected_state: ExclusiveDeviceLeaseState,
        receipt: &DeviceReconciliationReceipt,
        expected_gen: Generation,
        next_gen: Generation,
    ) -> Result<(), LeaseStoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO device_reconciliation_receipts (
                device_lease_id, receipt_id, device_id, holder_node,
                holder_node_boot_generation, exclusivity_epoch, fencing_token,
                reset_generation, reset_receipt_id, endpoint_fenced_old_token,
                queue_drained, vram_and_secrets_zeroized, ack_gateway_id,
                ack_observed_high_water, ack_digest
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                receipt.device_lease_id.as_bytes().as_slice(),
                receipt.receipt_id.as_bytes().as_slice(),
                device_id.as_bytes().as_slice(),
                receipt.holder_node.as_bytes().as_slice(),
                encode_u64(
                    receipt.holder_node_boot_generation.get(),
                    "holder_node_boot_generation"
                )?,
                encode_u64(receipt.exclusivity_epoch.get(), "exclusivity_epoch")?,
                encode_u64(receipt.fencing_token.get(), "fencing_token")?,
                encode_u64(receipt.reset_generation.get(), "reset_generation")?,
                receipt.reset_receipt.receipt_id().as_bytes().as_slice(),
                i64::from(receipt.endpoint_fenced_old_token),
                i64::from(receipt.queue_drained),
                i64::from(receipt.vram_and_secrets_zeroized),
                receipt.fence_barrier_ack.gateway_id().as_slice(),
                encode_u64(
                    receipt.fence_barrier_ack.observed_high_water(),
                    "ack_observed_high_water"
                )?,
                receipt.fence_barrier_ack.ack_digest().as_slice(),
            ],
        )?;
        let lease_changed = tx.execute(
            "UPDATE device_leases
             SET state = 'RETURNED'
             WHERE device_lease_id = ?1 AND state = ?2",
            params![
                receipt.device_lease_id.as_bytes().as_slice(),
                encode_device_state(expected_state),
            ],
        )?;
        if lease_changed != 1 {
            return Err(LeaseStoreError::CorruptRecord(
                "device_leases reconcile CAS lost",
            ));
        }
        let next_seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM device_reset_receipts",
            [],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO device_reset_receipts (
                seq, receipt_id, device_lease_id, device_id, holder_node,
                holder_node_boot_generation, exclusivity_epoch, fencing_token, reset_generation
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                next_seq,
                receipt.reset_receipt.receipt_id().as_bytes().as_slice(),
                receipt
                    .reset_receipt
                    .device_lease_id()
                    .as_bytes()
                    .as_slice(),
                device_id.as_bytes().as_slice(),
                receipt.reset_receipt.holder_node().as_bytes().as_slice(),
                encode_u64(
                    receipt.reset_receipt.holder_node_boot_generation().get(),
                    "reset_receipt.holder_node_boot_generation"
                )?,
                encode_u64(
                    receipt.reset_receipt.exclusivity_epoch().get(),
                    "reset_receipt.exclusivity_epoch"
                )?,
                encode_u64(
                    receipt.reset_receipt.fencing_token().get(),
                    "reset_receipt.fencing_token"
                )?,
                encode_u64(
                    receipt.reset_receipt.reset_generation().get(),
                    "reset_receipt.reset_generation"
                )?,
            ],
        )?;
        let head_changed = tx.execute(
            "UPDATE device_head_state
             SET is_free = 1, reset_generation = ?1
             WHERE device_id = ?2 AND is_free = 0 AND reset_generation = ?3",
            params![
                encode_u64(next_gen.get(), "next_gen")?,
                device_id.as_bytes().as_slice(),
                encode_u64(expected_gen.get(), "expected_gen")?,
            ],
        )?;
        if head_changed != 1 {
            return Err(LeaseStoreError::CorruptRecord(
                "device_head_state reconcile reset_generation CAS lost",
            ));
        }
        tx.commit()?;
        Ok(())
    }
}

fn resolve_db_path(path: &Path) -> Result<PathBuf, LeaseStoreError> {
    if path.to_string_lossy().starts_with("file:") {
        return Ok(path.to_path_buf());
    }
    if path.is_dir() {
        return Ok(path.join("leases.db"));
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(LeaseStoreError::Io)?;
    }
    Ok(path.to_path_buf())
}

fn count_schema_objects(connection: &Connection) -> Result<(i64, i64), LeaseStoreError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN (
            'quota_pool_state',
            'quota_leases',
            'quota_reconciliation_receipts',
            'capacity_pool_state',
            'capacity_leases',
            'capacity_attach_receipts',
            'capacity_reconciliation_receipts',
            'device_head_state',
            'device_leases',
            'device_reset_receipts',
            'device_reconciliation_receipts'
        )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'quota_pool_state_no_delete',
            'quota_pool_state_monotonic_epoch',
            'quota_leases_no_delete',
            'quota_leases_immutable_guard',
            'quota_reconciliation_receipts_no_update',
            'quota_reconciliation_receipts_no_delete',
            'capacity_pool_state_no_delete',
            'capacity_pool_state_immutable_identity',
            'capacity_leases_no_delete',
            'capacity_leases_immutable_guard',
            'capacity_attach_receipts_no_update',
            'capacity_attach_receipts_no_delete',
            'capacity_reconciliation_receipts_no_update',
            'capacity_reconciliation_receipts_no_delete',
            'device_head_state_no_delete',
            'device_head_state_monotonic_generation',
            'device_leases_no_delete',
            'device_leases_immutable_guard',
            'device_reset_receipts_no_update',
            'device_reset_receipts_no_delete',
            'device_reconciliation_receipts_no_update',
            'device_reconciliation_receipts_no_delete'
        )",
        [],
        |row| row.get(0),
    )?;
    Ok((table_count, trigger_count))
}

fn verify_schema_objects(connection: &Connection) -> Result<(), LeaseStoreError> {
    let (table_count, trigger_count) = count_schema_objects(connection)?;
    if table_count != EXPECTED_TABLE_COUNT || trigger_count != EXPECTED_TRIGGER_COUNT {
        return Err(LeaseStoreError::CorruptRecord("partial lease store schema"));
    }
    Ok(())
}

fn migrate_v1(connection: &mut Connection) -> Result<(), LeaseStoreError> {
    let (table_count, trigger_count) = count_schema_objects(connection)?;
    if table_count == EXPECTED_TABLE_COUNT && trigger_count == EXPECTED_TRIGGER_COUNT {
        connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 {
        return Err(LeaseStoreError::CorruptRecord("partial lease store schema"));
    }

    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(SCHEMA_V1_DDL)?;
    tx.commit()?;
    Ok(())
}

const SCHEMA_V1_DDL: &str = "
CREATE TABLE quota_pool_state (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK(singleton = 1),
    cell_id BLOB NOT NULL CHECK(length(cell_id) = 16),
    initial_available INTEGER NOT NULL CHECK(initial_available >= 0),
    available INTEGER NOT NULL CHECK(available >= 0 AND available <= initial_available),
    last_epoch INTEGER NOT NULL CHECK(last_epoch >= 1),
    last_fencing_token INTEGER NOT NULL CHECK(last_fencing_token >= 1)
) STRICT;

CREATE TABLE quota_leases (
    lease_id BLOB PRIMARY KEY NOT NULL CHECK(length(lease_id) = 16),
    cell_id BLOB NOT NULL CHECK(length(cell_id) = 16),
    node_boot_generation INTEGER NOT NULL CHECK(node_boot_generation >= 1),
    face_value INTEGER NOT NULL CHECK(face_value >= 0),
    remaining INTEGER NOT NULL CHECK(remaining >= 0),
    spent INTEGER NOT NULL CHECK(spent >= 0),
    returned INTEGER NOT NULL CHECK(returned >= 0),
    epoch INTEGER NOT NULL CHECK(epoch >= 1),
    fencing_token INTEGER NOT NULL CHECK(fencing_token >= 1),
    fence_scope_cell_id BLOB NOT NULL CHECK(length(fence_scope_cell_id) = 16 AND fence_scope_cell_id = cell_id),
    expires_at INTEGER CHECK(expires_at IS NULL OR expires_at >= 0),
    state TEXT NOT NULL CHECK(state IN ('ISSUED', 'ACTIVE', 'CLOSING', 'CLOSED', 'CANCELLED', 'FENCED', 'QUARANTINED')),
    CHECK(remaining + spent + returned = face_value)
) STRICT;

CREATE TABLE quota_reconciliation_receipts (
    lease_id BLOB NOT NULL CHECK(length(lease_id) = 16),
    reconcile_seq INTEGER NOT NULL CHECK(reconcile_seq >= 1),
    receipt_id BLOB NOT NULL UNIQUE CHECK(length(receipt_id) = 16),
    cell_id BLOB NOT NULL CHECK(length(cell_id) = 16),
    node_boot_generation INTEGER NOT NULL CHECK(node_boot_generation >= 1),
    epoch INTEGER NOT NULL CHECK(epoch >= 1),
    fencing_token INTEGER NOT NULL CHECK(fencing_token >= 1),
    face_value INTEGER NOT NULL CHECK(face_value >= 0),
    high_water_spent INTEGER NOT NULL CHECK(high_water_spent >= 0),
    settled_spent INTEGER NOT NULL CHECK(settled_spent = high_water_spent),
    returned_to_available INTEGER NOT NULL CHECK(returned_to_available >= 0),
    remaining_quarantined INTEGER NOT NULL CHECK(remaining_quarantined >= 0),
    ack_gateway_id BLOB NOT NULL CHECK(length(ack_gateway_id) = 16),
    ack_observed_high_water INTEGER NOT NULL CHECK(ack_observed_high_water = high_water_spent),
    ack_digest BLOB NOT NULL CHECK(length(ack_digest) = 32),
    CHECK(settled_spent + returned_to_available + remaining_quarantined = face_value),
    PRIMARY KEY (lease_id, reconcile_seq),
    FOREIGN KEY(lease_id) REFERENCES quota_leases(lease_id)
) STRICT;

CREATE TABLE capacity_pool_state (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK(singleton = 1),
    cell_id BLOB NOT NULL CHECK(length(cell_id) = 16),
    initial_pool INTEGER NOT NULL CHECK(initial_pool >= 0),
    pool_remaining INTEGER NOT NULL CHECK(pool_remaining >= 0 AND pool_remaining <= initial_pool)
) STRICT;

CREATE TABLE capacity_leases (
    capacity_lease_id BLOB PRIMARY KEY NOT NULL CHECK(length(capacity_lease_id) = 16),
    target_node BLOB NOT NULL CHECK(length(target_node) = 16),
    target_node_boot_generation INTEGER NOT NULL CHECK(target_node_boot_generation >= 1),
    amount INTEGER NOT NULL CHECK(amount >= 0),
    capacity_epoch INTEGER NOT NULL CHECK(capacity_epoch >= 1),
    fencing_token INTEGER NOT NULL CHECK(fencing_token >= 1),
    fence_scope_cell_id BLOB NOT NULL CHECK(length(fence_scope_cell_id) = 16 AND fence_scope_cell_id = target_node),
    state TEXT NOT NULL CHECK(state IN ('GLOBAL_RESERVED', 'TARGET_PREPARED', 'ACTIVE', 'RETURNING', 'FENCED', 'QUARANTINED', 'RETURNED'))
) STRICT;

CREATE TABLE capacity_attach_receipts (
    capacity_lease_id BLOB PRIMARY KEY NOT NULL CHECK(length(capacity_lease_id) = 16),
    receipt_id BLOB NOT NULL CHECK(length(receipt_id) = 16),
    target_node BLOB NOT NULL CHECK(length(target_node) = 16),
    target_node_boot_generation INTEGER NOT NULL CHECK(target_node_boot_generation >= 1),
    capacity_epoch INTEGER NOT NULL CHECK(capacity_epoch >= 1),
    fencing_token INTEGER NOT NULL CHECK(fencing_token >= 1),
    attached_amount INTEGER NOT NULL CHECK(attached_amount >= 0),
    FOREIGN KEY(capacity_lease_id) REFERENCES capacity_leases(capacity_lease_id)
) STRICT;

CREATE TABLE capacity_reconciliation_receipts (
    capacity_lease_id BLOB PRIMARY KEY NOT NULL CHECK(length(capacity_lease_id) = 16),
    receipt_id BLOB NOT NULL UNIQUE CHECK(length(receipt_id) = 16),
    target_node BLOB NOT NULL CHECK(length(target_node) = 16),
    target_node_boot_generation INTEGER NOT NULL CHECK(target_node_boot_generation >= 1),
    capacity_epoch INTEGER NOT NULL CHECK(capacity_epoch >= 1),
    fencing_token INTEGER NOT NULL CHECK(fencing_token >= 1),
    amount INTEGER NOT NULL CHECK(amount >= 0),
    cleaned_amount INTEGER NOT NULL CHECK(cleaned_amount = amount),
    holder_processes_terminated INTEGER NOT NULL CHECK(holder_processes_terminated = 1),
    cleanup_attestation_digest BLOB NOT NULL CHECK(length(cleanup_attestation_digest) = 32),
    ack_gateway_id BLOB NOT NULL CHECK(length(ack_gateway_id) = 16),
    ack_observed_high_water INTEGER NOT NULL CHECK(ack_observed_high_water = amount),
    ack_digest BLOB NOT NULL CHECK(length(ack_digest) = 32),
    FOREIGN KEY(capacity_lease_id) REFERENCES capacity_leases(capacity_lease_id)
) STRICT;

CREATE TABLE device_head_state (
    device_id BLOB PRIMARY KEY NOT NULL CHECK(length(device_id) = 16),
    cell_id BLOB NOT NULL CHECK(length(cell_id) = 16),
    is_free INTEGER NOT NULL CHECK(is_free IN (0, 1)),
    reset_generation INTEGER NOT NULL CHECK(reset_generation >= 1)
) STRICT;

CREATE TABLE device_leases (
    device_lease_id BLOB PRIMARY KEY NOT NULL CHECK(length(device_lease_id) = 16),
    device_id BLOB NOT NULL CHECK(length(device_id) = 16),
    holder_node BLOB NOT NULL CHECK(length(holder_node) = 16),
    holder_node_boot_generation INTEGER NOT NULL CHECK(holder_node_boot_generation >= 1),
    exclusivity_epoch INTEGER NOT NULL CHECK(exclusivity_epoch >= 1),
    fencing_token INTEGER NOT NULL CHECK(fencing_token >= 1),
    fence_scope_cell_id BLOB NOT NULL CHECK(length(fence_scope_cell_id) = 16 AND fence_scope_cell_id = holder_node),
    reset_generation INTEGER NOT NULL CHECK(reset_generation >= 1),
    state TEXT NOT NULL CHECK(state IN ('DEVICE_RESERVED', 'RESETTING', 'FENCING', 'QUARANTINED', 'RETURNED'))
) STRICT;

CREATE TABLE device_reset_receipts (
    seq INTEGER PRIMARY KEY NOT NULL CHECK(seq >= 1),
    receipt_id BLOB NOT NULL UNIQUE CHECK(length(receipt_id) = 16),
    device_lease_id BLOB NOT NULL UNIQUE CHECK(length(device_lease_id) = 16),
    device_id BLOB NOT NULL CHECK(length(device_id) = 16),
    holder_node BLOB NOT NULL CHECK(length(holder_node) = 16),
    holder_node_boot_generation INTEGER NOT NULL CHECK(holder_node_boot_generation >= 1),
    exclusivity_epoch INTEGER NOT NULL CHECK(exclusivity_epoch >= 1),
    fencing_token INTEGER NOT NULL CHECK(fencing_token >= 1),
    reset_generation INTEGER NOT NULL CHECK(reset_generation >= 1),
    FOREIGN KEY(device_lease_id) REFERENCES device_leases(device_lease_id)
) STRICT;

CREATE TABLE device_reconciliation_receipts (
    device_lease_id BLOB PRIMARY KEY NOT NULL CHECK(length(device_lease_id) = 16),
    receipt_id BLOB NOT NULL UNIQUE CHECK(length(receipt_id) = 16),
    device_id BLOB NOT NULL CHECK(length(device_id) = 16),
    holder_node BLOB NOT NULL CHECK(length(holder_node) = 16),
    holder_node_boot_generation INTEGER NOT NULL CHECK(holder_node_boot_generation >= 1),
    exclusivity_epoch INTEGER NOT NULL CHECK(exclusivity_epoch >= 1),
    fencing_token INTEGER NOT NULL CHECK(fencing_token >= 1),
    reset_generation INTEGER NOT NULL CHECK(reset_generation >= 1),
    reset_receipt_id BLOB NOT NULL UNIQUE CHECK(length(reset_receipt_id) = 16),
    endpoint_fenced_old_token INTEGER NOT NULL CHECK(endpoint_fenced_old_token = 1),
    queue_drained INTEGER NOT NULL CHECK(queue_drained = 1),
    vram_and_secrets_zeroized INTEGER NOT NULL CHECK(vram_and_secrets_zeroized = 1),
    ack_gateway_id BLOB NOT NULL CHECK(length(ack_gateway_id) = 16),
    ack_observed_high_water INTEGER NOT NULL CHECK(ack_observed_high_water = exclusivity_epoch),
    ack_digest BLOB NOT NULL CHECK(length(ack_digest) = 32),
    FOREIGN KEY(device_lease_id) REFERENCES device_leases(device_lease_id)
) STRICT;

CREATE TRIGGER quota_pool_state_no_delete
BEFORE DELETE ON quota_pool_state BEGIN
    SELECT RAISE(ABORT, 'quota_pool_state row is durable');
END;

CREATE TRIGGER quota_pool_state_monotonic_epoch
BEFORE UPDATE ON quota_pool_state
WHEN NEW.singleton != OLD.singleton
  OR NEW.cell_id != OLD.cell_id
  OR NEW.initial_available != OLD.initial_available
  OR NEW.last_epoch < OLD.last_epoch
  OR NEW.last_fencing_token < OLD.last_fencing_token
BEGIN
    SELECT RAISE(ABORT, 'quota_pool_state identity or monotonic epoch invariant violated');
END;

CREATE TRIGGER quota_leases_no_delete
BEFORE DELETE ON quota_leases BEGIN
    SELECT RAISE(ABORT, 'quota_leases rows are durable and cannot be deleted');
END;

CREATE TRIGGER quota_leases_immutable_guard
BEFORE UPDATE ON quota_leases
WHEN OLD.state IN ('CLOSED', 'CANCELLED')
  OR (OLD.state = 'QUARANTINED' AND NOT EXISTS (
      SELECT 1 FROM quota_reconciliation_receipts WHERE lease_id = OLD.lease_id
  ))
  OR NEW.lease_id != OLD.lease_id
  OR NEW.cell_id != OLD.cell_id
  OR NEW.node_boot_generation != OLD.node_boot_generation
  OR NEW.face_value != OLD.face_value
  OR NEW.epoch != OLD.epoch
  OR NEW.fencing_token != OLD.fencing_token
  OR NEW.fence_scope_cell_id != OLD.fence_scope_cell_id
  OR (OLD.expires_at IS NOT NULL AND (NEW.expires_at IS NULL OR NEW.expires_at != OLD.expires_at))
  OR (OLD.expires_at IS NULL AND NEW.expires_at IS NOT NULL)
  OR NEW.spent < OLD.spent
  OR NEW.returned < OLD.returned
  OR (NEW.state = 'CLOSED' AND NEW.remaining != 0)
  OR NOT (
      (OLD.state = 'ISSUED' AND NEW.state IN ('ACTIVE', 'CANCELLED', 'FENCED', 'QUARANTINED'))
      OR (OLD.state = 'ACTIVE' AND NEW.state IN ('ACTIVE', 'CLOSING', 'FENCED', 'QUARANTINED'))
      OR (OLD.state = 'CLOSING' AND NEW.state IN ('CLOSED', 'FENCED', 'QUARANTINED'))
      OR (OLD.state = 'FENCED' AND NEW.state IN ('CLOSED', 'QUARANTINED'))
      OR (OLD.state = 'QUARANTINED' AND NEW.state IN ('QUARANTINED', 'CLOSED'))
  )
BEGIN
    SELECT RAISE(ABORT, 'quota_leases terminal or monotonic invariant violated');
END;

CREATE TRIGGER quota_reconciliation_receipts_no_update
BEFORE UPDATE ON quota_reconciliation_receipts BEGIN
    SELECT RAISE(ABORT, 'quota_reconciliation_receipts rows are immutable');
END;

CREATE TRIGGER quota_reconciliation_receipts_no_delete
BEFORE DELETE ON quota_reconciliation_receipts BEGIN
    SELECT RAISE(ABORT, 'quota_reconciliation_receipts rows are durable');
END;

CREATE TRIGGER capacity_pool_state_no_delete
BEFORE DELETE ON capacity_pool_state BEGIN
    SELECT RAISE(ABORT, 'capacity_pool_state row is durable');
END;

CREATE TRIGGER capacity_pool_state_immutable_identity
BEFORE UPDATE ON capacity_pool_state
WHEN NEW.singleton != OLD.singleton
  OR NEW.cell_id != OLD.cell_id
  OR NEW.initial_pool != OLD.initial_pool
BEGIN
    SELECT RAISE(ABORT, 'capacity_pool_state identity invariant violated');
END;

CREATE TRIGGER capacity_leases_no_delete
BEFORE DELETE ON capacity_leases BEGIN
    SELECT RAISE(ABORT, 'capacity_leases rows are durable and cannot be deleted');
END;

CREATE TRIGGER capacity_leases_immutable_guard
BEFORE UPDATE ON capacity_leases
WHEN OLD.state = 'RETURNED'
  OR (OLD.state = 'QUARANTINED' AND NEW.state = 'RETURNED' AND NOT EXISTS (
      SELECT 1 FROM capacity_reconciliation_receipts WHERE capacity_lease_id = OLD.capacity_lease_id
  ))
  OR NEW.capacity_lease_id != OLD.capacity_lease_id
  OR NEW.target_node != OLD.target_node
  OR NEW.target_node_boot_generation != OLD.target_node_boot_generation
  OR NEW.amount != OLD.amount
  OR NEW.capacity_epoch != OLD.capacity_epoch
  OR NEW.fencing_token != OLD.fencing_token
  OR NEW.fence_scope_cell_id != OLD.fence_scope_cell_id
  OR NOT (
      (OLD.state = 'GLOBAL_RESERVED' AND NEW.state IN ('TARGET_PREPARED', 'RETURNING', 'FENCED', 'QUARANTINED'))
      OR (OLD.state = 'TARGET_PREPARED' AND NEW.state IN ('ACTIVE', 'RETURNING', 'FENCED', 'QUARANTINED'))
      OR (OLD.state = 'ACTIVE' AND NEW.state IN ('RETURNING', 'FENCED', 'QUARANTINED'))
      OR (OLD.state = 'RETURNING' AND NEW.state IN ('RETURNED', 'FENCED', 'QUARANTINED'))
      OR (OLD.state = 'FENCED' AND NEW.state IN ('QUARANTINED', 'RETURNED'))
      OR (OLD.state = 'QUARANTINED' AND NEW.state IN ('QUARANTINED', 'RETURNED'))
  )
BEGIN
    SELECT RAISE(ABORT, 'capacity_leases terminal or identity invariant violated');
END;

CREATE TRIGGER capacity_attach_receipts_no_update
BEFORE UPDATE ON capacity_attach_receipts BEGIN
    SELECT RAISE(ABORT, 'capacity_attach_receipts rows are immutable');
END;

CREATE TRIGGER capacity_attach_receipts_no_delete
BEFORE DELETE ON capacity_attach_receipts BEGIN
    SELECT RAISE(ABORT, 'capacity_attach_receipts rows are durable');
END;

CREATE TRIGGER capacity_reconciliation_receipts_no_update
BEFORE UPDATE ON capacity_reconciliation_receipts BEGIN
    SELECT RAISE(ABORT, 'capacity_reconciliation_receipts rows are immutable');
END;

CREATE TRIGGER capacity_reconciliation_receipts_no_delete
BEFORE DELETE ON capacity_reconciliation_receipts BEGIN
    SELECT RAISE(ABORT, 'capacity_reconciliation_receipts rows are durable');
END;

CREATE TRIGGER device_head_state_no_delete
BEFORE DELETE ON device_head_state BEGIN
    SELECT RAISE(ABORT, 'device_head_state row is durable');
END;

CREATE TRIGGER device_head_state_monotonic_generation
BEFORE UPDATE ON device_head_state
WHEN NEW.device_id != OLD.device_id
  OR NEW.cell_id != OLD.cell_id
  OR NEW.reset_generation < OLD.reset_generation
BEGIN
    SELECT RAISE(ABORT, 'device_head_state monotonic reset_generation invariant violated');
END;

CREATE TRIGGER device_leases_no_delete
BEFORE DELETE ON device_leases BEGIN
    SELECT RAISE(ABORT, 'device_leases rows are durable and cannot be deleted');
END;

CREATE TRIGGER device_leases_immutable_guard
BEFORE UPDATE ON device_leases
WHEN OLD.state = 'RETURNED'
  OR (OLD.state = 'QUARANTINED' AND NEW.state = 'RETURNED' AND NOT EXISTS (
      SELECT 1 FROM device_reconciliation_receipts WHERE device_lease_id = OLD.device_lease_id
  ))
  OR NEW.device_lease_id != OLD.device_lease_id
  OR NEW.device_id != OLD.device_id
  OR NEW.holder_node != OLD.holder_node
  OR NEW.holder_node_boot_generation != OLD.holder_node_boot_generation
  OR NEW.exclusivity_epoch != OLD.exclusivity_epoch
  OR NEW.fencing_token != OLD.fencing_token
  OR NEW.fence_scope_cell_id != OLD.fence_scope_cell_id
  OR NEW.reset_generation != OLD.reset_generation
  OR NOT (
      (OLD.state = 'DEVICE_RESERVED' AND NEW.state IN ('RESETTING', 'FENCING', 'QUARANTINED'))
      OR (OLD.state = 'RESETTING' AND NEW.state IN ('RETURNED', 'FENCING', 'QUARANTINED'))
      OR (OLD.state = 'FENCING' AND NEW.state IN ('QUARANTINED', 'RETURNED'))
      OR (OLD.state = 'QUARANTINED' AND NEW.state IN ('QUARANTINED', 'RETURNED'))
  )
BEGIN
    SELECT RAISE(ABORT, 'device_leases terminal or identity invariant violated');
END;

CREATE TRIGGER device_reset_receipts_no_update
BEFORE UPDATE ON device_reset_receipts BEGIN
    SELECT RAISE(ABORT, 'device_reset_receipts is append-only: UPDATE forbidden');
END;

CREATE TRIGGER device_reset_receipts_no_delete
BEFORE DELETE ON device_reset_receipts BEGIN
    SELECT RAISE(ABORT, 'device_reset_receipts is append-only: DELETE forbidden');
END;

CREATE TRIGGER device_reconciliation_receipts_no_update
BEFORE UPDATE ON device_reconciliation_receipts BEGIN
    SELECT RAISE(ABORT, 'device_reconciliation_receipts is append-only: UPDATE forbidden');
END;

CREATE TRIGGER device_reconciliation_receipts_no_delete
BEFORE DELETE ON device_reconciliation_receipts BEGIN
    SELECT RAISE(ABORT, 'device_reconciliation_receipts is append-only: DELETE forbidden');
END;

PRAGMA user_version = 1;
";

type LoadedQuotaLeases = (
    HashMap<QuotaLeaseId, QuotaLeaseGrant>,
    HashMap<QuotaLeaseId, Vec<QuotaReconciliationReceipt>>,
    u64,
    u64,
);

fn load_and_validate_quota_leases(
    tx: &Transaction<'_>,
    expected_cell: CellIdentity,
) -> Result<LoadedQuotaLeases, LeaseStoreError> {
    let mut stmt = tx.prepare(
        "SELECT lease_id, cell_id, node_boot_generation, face_value, remaining,
                spent, returned, epoch, fencing_token, fence_scope_cell_id,
                expires_at, state
         FROM quota_leases",
    )?;
    let mut rows = stmt.query([])?;
    let mut leases = HashMap::new();
    let mut net_deducted: u64 = 0;
    let mut max_epoch: u64 = 1;

    while let Some(row) = rows.next()? {
        let lease_id = QuotaLeaseId::from_bytes(decode_16(row.get(0)?, "quota_leases.lease_id")?);
        let cell = decode_cell_identity(&row.get::<_, Vec<u8>>(1)?, "quota_leases.cell_id")?;
        let scope_cell = decode_cell_identity(
            &row.get::<_, Vec<u8>>(9)?,
            "quota_leases.fence_scope_cell_id",
        )?;
        if cell != expected_cell || scope_cell != cell {
            return Err(LeaseStoreError::CorruptRecord(
                "quota_leases cell identity mismatch",
            ));
        }
        let node_boot_generation =
            decode_generation(row.get(2)?, "quota_leases.node_boot_generation")?;
        let face_value = decode_u64(row.get(3)?, "quota_leases.face_value")?;
        let remaining = decode_u64(row.get(4)?, "quota_leases.remaining")?;
        let spent = decode_u64(row.get(5)?, "quota_leases.spent")?;
        let returned = decode_u64(row.get(6)?, "quota_leases.returned")?;
        let epoch = decode_epoch(row.get(7)?, "quota_leases.epoch")?;
        let fencing_token = decode_fencing_token(row.get(8)?, "quota_leases.fencing_token")?;
        let raw_expires: Option<i64> = row.get(10)?;
        let expires_at = raw_expires
            .map(|val| decode_u64(val, "quota_leases.expires_at").map(LeaseInstant::new))
            .transpose()?;
        let state_str: String = row.get(11)?;
        let state = decode_quota_state(&state_str)?;

        validate_quota_lease_invariant(face_value, remaining, spent, returned, state)?;
        let lease_deducted =
            face_value
                .checked_sub(returned)
                .ok_or(LeaseStoreError::CorruptRecord(
                    "quota_leases returned exceeds face_value",
                ))?;
        net_deducted =
            net_deducted
                .checked_add(lease_deducted)
                .ok_or(LeaseStoreError::CorruptRecord(
                    "quota_leases net_deducted overflow",
                ))?;
        max_epoch = max_epoch.max(epoch.get());

        let grant = QuotaLeaseGrant {
            lease_id,
            cell,
            node_boot_generation,
            face_value,
            remaining,
            spent,
            returned,
            epoch,
            fencing_token,
            fence_scope: FenceScope::cell(scope_cell),
            expires_at,
            state,
        };
        leases.insert(lease_id, grant);
    }

    let reconciliation_receipts = load_quota_reconciliation_receipt_rows(tx, &leases)?;
    for lease in leases.values() {
        if lease.state == QuotaLeaseState::Quarantined && lease.returned > 0 {
            let Some(history) = reconciliation_receipts.get(&lease.lease_id) else {
                return Err(LeaseStoreError::CorruptRecord(
                    "QUARANTINED quota lease has non-zero returned without reconciliation receipt",
                ));
            };
            let Some(last) = history.last() else {
                return Err(LeaseStoreError::CorruptRecord(
                    "QUARANTINED quota lease has empty reconciliation history",
                ));
            };
            if last.returned_to_available() != lease.returned
                || last.settled_spent() != lease.spent
                || last.remaining_quarantined() != lease.remaining
            {
                return Err(LeaseStoreError::CorruptRecord(
                    "QUARANTINED quota lease accounting diverges from reconciliation receipt",
                ));
            }
        }
    }

    Ok((leases, reconciliation_receipts, net_deducted, max_epoch))
}

fn load_quota_reconciliation_receipt_rows(
    tx: &Transaction<'_>,
    leases: &HashMap<QuotaLeaseId, QuotaLeaseGrant>,
) -> Result<HashMap<QuotaLeaseId, Vec<QuotaReconciliationReceipt>>, LeaseStoreError> {
    let mut stmt = tx.prepare(
        "SELECT lease_id, reconcile_seq, receipt_id, cell_id, node_boot_generation,
                epoch, fencing_token, face_value, high_water_spent, settled_spent,
                returned_to_available, remaining_quarantined, ack_gateway_id,
                ack_observed_high_water, ack_digest
         FROM quota_reconciliation_receipts
         ORDER BY lease_id ASC, reconcile_seq ASC",
    )?;
    let mut rows = stmt.query([])?;
    let mut receipts: HashMap<QuotaLeaseId, Vec<QuotaReconciliationReceipt>> = HashMap::new();

    while let Some(row) = rows.next()? {
        let lease_id = QuotaLeaseId::from_bytes(decode_16(
            row.get(0)?,
            "quota_reconciliation_receipts.lease_id",
        )?);
        let reconcile_seq = decode_u64(row.get(1)?, "quota_reconciliation_receipts.reconcile_seq")?;
        let receipt_id = ReceiptId::from_bytes(decode_16(
            row.get(2)?,
            "quota_reconciliation_receipts.receipt_id",
        )?);
        let cell = decode_cell_identity(
            &row.get::<_, Vec<u8>>(3)?,
            "quota_reconciliation_receipts.cell_id",
        )?;
        let node_boot_generation = decode_generation(
            row.get(4)?,
            "quota_reconciliation_receipts.node_boot_generation",
        )?;
        let epoch = decode_epoch(row.get(5)?, "quota_reconciliation_receipts.epoch")?;
        let fencing_token =
            decode_fencing_token(row.get(6)?, "quota_reconciliation_receipts.fencing_token")?;
        let face_value = decode_u64(row.get(7)?, "quota_reconciliation_receipts.face_value")?;
        let high_water_spent = decode_u64(
            row.get(8)?,
            "quota_reconciliation_receipts.high_water_spent",
        )?;
        let settled_spent = decode_u64(row.get(9)?, "quota_reconciliation_receipts.settled_spent")?;
        let returned_to_available = decode_u64(
            row.get(10)?,
            "quota_reconciliation_receipts.returned_to_available",
        )?;
        let remaining_quarantined = decode_u64(
            row.get(11)?,
            "quota_reconciliation_receipts.remaining_quarantined",
        )?;
        let ack_gateway_id =
            decode_16(row.get(12)?, "quota_reconciliation_receipts.ack_gateway_id")?;
        let ack_observed_high_water = decode_u64(
            row.get(13)?,
            "quota_reconciliation_receipts.ack_observed_high_water",
        )?;
        let ack_digest = decode_32(row.get(14)?, "quota_reconciliation_receipts.ack_digest")?;

        let lease = leases.get(&lease_id).ok_or(LeaseStoreError::CorruptRecord(
            "quota_reconciliation_receipts references unknown lease",
        ))?;
        let ack = FenceBarrierAck::new(
            *lease_id.as_bytes(),
            lease.fence_scope,
            epoch,
            fencing_token,
            ack_gateway_id,
            ack_observed_high_water,
            ack_digest,
        );
        if verify_quota_fence_ack(&ack, lease, high_water_spent).is_err() {
            return Err(LeaseStoreError::CorruptRecord(
                "stored quota_reconciliation_receipts FenceBarrierAck invalid",
            ));
        }
        let receipt = QuotaReconciliationReceipt::new(
            receipt_id,
            lease_id,
            reconcile_seq,
            cell,
            node_boot_generation,
            epoch,
            fencing_token,
            face_value,
            high_water_spent,
            settled_spent,
            returned_to_available,
            remaining_quarantined,
            ack,
        );
        if !receipt.verify_conservation() {
            return Err(LeaseStoreError::CorruptRecord(
                "stored quota_reconciliation_receipts conservation violated",
            ));
        }
        receipts.entry(lease_id).or_default().push(receipt);
    }
    Ok(receipts)
}

fn validate_quota_lease_invariant(
    face_value: u64,
    remaining: u64,
    spent: u64,
    returned: u64,
    state: QuotaLeaseState,
) -> Result<(), LeaseStoreError> {
    let sum = remaining
        .checked_add(spent)
        .and_then(|acc| acc.checked_add(returned))
        .ok_or(LeaseStoreError::CorruptRecord(
            "quota_leases accounting overflow",
        ))?;
    if sum != face_value {
        return Err(LeaseStoreError::CorruptRecord(
            "quota_leases face_value conservation violated",
        ));
    }
    let state_valid = match state {
        QuotaLeaseState::Issued => spent == 0 && returned == 0 && remaining == face_value,
        QuotaLeaseState::Active | QuotaLeaseState::Closing | QuotaLeaseState::Fenced => {
            returned == 0
        }
        QuotaLeaseState::Quarantined => true,
        QuotaLeaseState::Closed => remaining == 0,
        QuotaLeaseState::Cancelled => remaining == 0 && spent == 0 && returned == face_value,
    };
    if !state_valid {
        return Err(LeaseStoreError::CorruptRecord(
            "quota_leases state accounting invariant violated",
        ));
    }
    Ok(())
}

type LoadedCapacityLeases = (
    HashMap<CapacityLeaseId, CapacityLeaseGrant>,
    HashMap<CapacityLeaseId, HostAttachReceipt>,
    HashMap<CapacityLeaseId, CapacityReconciliationReceipt>,
    u64,
);

type LoadedDeviceLeases = (
    HashMap<ExclusiveDeviceLeaseId, ExclusiveDeviceLeaseGrant>,
    Vec<DeviceResetReceipt>,
    HashMap<ExclusiveDeviceLeaseId, DeviceReconciliationReceipt>,
);

type LoadedDeviceLeaseRows = (
    HashMap<ExclusiveDeviceLeaseId, ExclusiveDeviceLeaseGrant>,
    Option<ExclusiveDeviceLeaseGrant>,
    u64,
);

fn load_and_validate_capacity_leases(
    tx: &Transaction<'_>,
    expected_cell: CellIdentity,
) -> Result<LoadedCapacityLeases, LeaseStoreError> {
    let (leases, reserved_total) = load_capacity_lease_rows(tx, expected_cell)?;
    let attach_receipts = load_capacity_attach_receipt_rows(tx, &leases)?;
    let reconciliation_receipts = load_capacity_reconciliation_receipt_rows(tx, &leases)?;

    for lease in leases.values() {
        if lease.state == CapacityLeaseState::Active
            && !attach_receipts.contains_key(&lease.capacity_lease_id)
        {
            return Err(LeaseStoreError::CorruptRecord(
                "ACTIVE capacity lease is missing its HostAttachReceipt",
            ));
        }
    }

    Ok((
        leases,
        attach_receipts,
        reconciliation_receipts,
        reserved_total,
    ))
}

#[allow(clippy::too_many_lines)]
fn load_capacity_reconciliation_receipt_rows(
    tx: &Transaction<'_>,
    leases: &HashMap<CapacityLeaseId, CapacityLeaseGrant>,
) -> Result<HashMap<CapacityLeaseId, CapacityReconciliationReceipt>, LeaseStoreError> {
    let mut stmt = tx.prepare(
        "SELECT capacity_lease_id, receipt_id, target_node, target_node_boot_generation,
                capacity_epoch, fencing_token, amount, cleaned_amount,
                holder_processes_terminated, cleanup_attestation_digest,
                ack_gateway_id, ack_observed_high_water, ack_digest
         FROM capacity_reconciliation_receipts",
    )?;
    let mut rows = stmt.query([])?;
    let mut receipts = HashMap::new();

    while let Some(row) = rows.next()? {
        let capacity_lease_id = CapacityLeaseId::from_bytes(decode_16(
            row.get(0)?,
            "capacity_reconciliation_receipts.capacity_lease_id",
        )?);
        let receipt_id = ReceiptId::from_bytes(decode_16(
            row.get(1)?,
            "capacity_reconciliation_receipts.receipt_id",
        )?);
        let target_node = decode_cell_identity(
            &row.get::<_, Vec<u8>>(2)?,
            "capacity_reconciliation_receipts.target_node",
        )?;
        let target_node_boot_generation = decode_generation(
            row.get(3)?,
            "capacity_reconciliation_receipts.target_node_boot_generation",
        )?;
        let capacity_epoch = decode_epoch(
            row.get(4)?,
            "capacity_reconciliation_receipts.capacity_epoch",
        )?;
        let fencing_token = decode_fencing_token(
            row.get(5)?,
            "capacity_reconciliation_receipts.fencing_token",
        )?;
        let amount = decode_u64(row.get(6)?, "capacity_reconciliation_receipts.amount")?;
        let cleaned_amount = decode_u64(
            row.get(7)?,
            "capacity_reconciliation_receipts.cleaned_amount",
        )?;
        let raw_term: i64 = row.get(8)?;
        let holder_processes_terminated = raw_term == 1;
        let attestation_digest = decode_32(
            row.get(9)?,
            "capacity_reconciliation_receipts.cleanup_attestation_digest",
        )?;
        let ack_gateway_id = decode_16(
            row.get(10)?,
            "capacity_reconciliation_receipts.ack_gateway_id",
        )?;
        let ack_observed_high_water = decode_u64(
            row.get(11)?,
            "capacity_reconciliation_receipts.ack_observed_high_water",
        )?;
        let ack_digest = decode_32(row.get(12)?, "capacity_reconciliation_receipts.ack_digest")?;

        let lease = leases
            .get(&capacity_lease_id)
            .ok_or(LeaseStoreError::CorruptRecord(
                "capacity_reconciliation_receipts references unknown lease",
            ))?;
        if lease.state != CapacityLeaseState::Returned {
            return Err(LeaseStoreError::CorruptRecord(
                "capacity_reconciliation_receipts references non-RETURNED lease",
            ));
        }

        let attestation = CleanupAttestation::new(
            capacity_lease_id,
            target_node,
            target_node_boot_generation,
            capacity_epoch,
            fencing_token,
            cleaned_amount,
            holder_processes_terminated,
            attestation_digest,
        );
        let ack = FenceBarrierAck::new(
            *capacity_lease_id.as_bytes(),
            lease.fence_scope,
            capacity_epoch,
            fencing_token,
            ack_gateway_id,
            ack_observed_high_water,
            ack_digest,
        );
        if verify_cleanup_attestation(&attestation, lease).is_err()
            || verify_capacity_fence_ack(&ack, lease).is_err()
        {
            return Err(LeaseStoreError::CorruptRecord(
                "stored capacity_reconciliation_receipts attestation or ack invalid",
            ));
        }

        receipts.insert(
            capacity_lease_id,
            CapacityReconciliationReceipt::new(
                receipt_id,
                capacity_lease_id,
                target_node,
                target_node_boot_generation,
                capacity_epoch,
                fencing_token,
                amount,
                attestation,
                ack,
            ),
        );
    }
    Ok(receipts)
}

fn load_capacity_lease_rows(
    tx: &Transaction<'_>,
    expected_cell: CellIdentity,
) -> Result<(HashMap<CapacityLeaseId, CapacityLeaseGrant>, u64), LeaseStoreError> {
    let mut stmt = tx.prepare(
        "SELECT capacity_lease_id, target_node, target_node_boot_generation, amount,
                capacity_epoch, fencing_token, fence_scope_cell_id, state
         FROM capacity_leases",
    )?;
    let mut rows = stmt.query([])?;
    let mut leases = HashMap::new();
    let mut reserved_total: u64 = 0;

    while let Some(row) = rows.next()? {
        let capacity_lease_id = CapacityLeaseId::from_bytes(decode_16(
            row.get(0)?,
            "capacity_leases.capacity_lease_id",
        )?);
        let target_node =
            decode_cell_identity(&row.get::<_, Vec<u8>>(1)?, "capacity_leases.target_node")?;
        let scope_cell = decode_cell_identity(
            &row.get::<_, Vec<u8>>(6)?,
            "capacity_leases.fence_scope_cell_id",
        )?;
        if target_node != expected_cell || scope_cell != target_node {
            return Err(LeaseStoreError::CorruptRecord(
                "capacity_leases target_node identity mismatch",
            ));
        }
        let target_node_boot_generation =
            decode_generation(row.get(2)?, "capacity_leases.target_node_boot_generation")?;
        let amount = decode_u64(row.get(3)?, "capacity_leases.amount")?;
        let capacity_epoch = decode_epoch(row.get(4)?, "capacity_leases.capacity_epoch")?;
        let fencing_token = decode_fencing_token(row.get(5)?, "capacity_leases.fencing_token")?;
        let state_str: String = row.get(7)?;
        let state = decode_capacity_state(&state_str)?;

        if state != CapacityLeaseState::Returned {
            reserved_total =
                reserved_total
                    .checked_add(amount)
                    .ok_or(LeaseStoreError::CorruptRecord(
                        "capacity_leases reserved_total overflow",
                    ))?;
        }

        leases.insert(
            capacity_lease_id,
            CapacityLeaseGrant {
                capacity_lease_id,
                target_node,
                target_node_boot_generation,
                amount,
                capacity_epoch,
                fencing_token,
                fence_scope: FenceScope::cell(scope_cell),
                state,
            },
        );
    }
    Ok((leases, reserved_total))
}

fn load_capacity_attach_receipt_rows(
    tx: &Transaction<'_>,
    leases: &HashMap<CapacityLeaseId, CapacityLeaseGrant>,
) -> Result<HashMap<CapacityLeaseId, HostAttachReceipt>, LeaseStoreError> {
    let mut receipt_stmt = tx.prepare(
        "SELECT capacity_lease_id, receipt_id, target_node, target_node_boot_generation,
                capacity_epoch, fencing_token, attached_amount
         FROM capacity_attach_receipts",
    )?;
    let mut receipt_rows = receipt_stmt.query([])?;
    let mut attach_receipts = HashMap::new();

    while let Some(row) = receipt_rows.next()? {
        let capacity_lease_id = CapacityLeaseId::from_bytes(decode_16(
            row.get(0)?,
            "capacity_attach_receipts.capacity_lease_id",
        )?);
        let receipt_id = ReceiptId::from_bytes(decode_16(
            row.get(1)?,
            "capacity_attach_receipts.receipt_id",
        )?);
        let target_node = decode_cell_identity(
            &row.get::<_, Vec<u8>>(2)?,
            "capacity_attach_receipts.target_node",
        )?;
        let target_node_boot_generation = decode_generation(
            row.get(3)?,
            "capacity_attach_receipts.target_node_boot_generation",
        )?;
        let capacity_epoch = decode_epoch(row.get(4)?, "capacity_attach_receipts.capacity_epoch")?;
        let fencing_token =
            decode_fencing_token(row.get(5)?, "capacity_attach_receipts.fencing_token")?;
        let attached_amount = decode_u64(row.get(6)?, "capacity_attach_receipts.attached_amount")?;

        let receipt = HostAttachReceipt::new(
            receipt_id,
            capacity_lease_id,
            target_node,
            target_node_boot_generation,
            capacity_epoch,
            fencing_token,
            attached_amount,
        );
        let lease = leases
            .get(&capacity_lease_id)
            .ok_or(LeaseStoreError::CorruptRecord(
                "attach receipt references unknown capacity lease",
            ))?;
        if !attach_receipt_binds_lease(&receipt, lease) {
            return Err(LeaseStoreError::CorruptRecord(
                "stored capacity attach receipt does not bind committed lease",
            ));
        }
        if matches!(
            lease.state,
            CapacityLeaseState::GlobalReserved | CapacityLeaseState::TargetPrepared
        ) {
            return Err(LeaseStoreError::CorruptRecord(
                "pre-active capacity lease cannot carry an attach receipt",
            ));
        }
        attach_receipts.insert(capacity_lease_id, receipt);
    }
    Ok(attach_receipts)
}

fn load_and_validate_device_leases(
    tx: &Transaction<'_>,
    expected_device_id: DeviceId,
    expected_cell: CellIdentity,
    head_is_free: bool,
    head_reset_generation: Generation,
) -> Result<LoadedDeviceLeases, LeaseStoreError> {
    let (leases, active_holder, returned_count) =
        load_device_lease_rows(tx, expected_device_id, expected_cell)?;
    let reset_receipts = load_device_reset_receipt_rows(tx, expected_device_id, &leases)?;
    let reconciliation_receipts =
        load_device_reconciliation_receipt_rows(tx, expected_device_id, &leases, &reset_receipts)?;

    if u64::try_from(reset_receipts.len()).ok() != Some(returned_count) {
        return Err(LeaseStoreError::CorruptRecord(
            "RETURNED device lease count does not match device_reset_receipts count",
        ));
    }
    if returned_count.checked_add(1) != Some(head_reset_generation.get()) {
        return Err(LeaseStoreError::CorruptRecord(
            "device_head_state reset_generation does not match receipt history",
        ));
    }
    match (head_is_free, active_holder) {
        (true, None) => {}
        (false, Some(active)) if active.reset_generation == head_reset_generation => {}
        _ => {
            return Err(LeaseStoreError::CorruptRecord(
                "device_head_state contradicts committed device leases",
            ));
        }
    }

    Ok((leases, reset_receipts, reconciliation_receipts))
}

#[allow(clippy::too_many_lines)]
fn load_device_reconciliation_receipt_rows(
    tx: &Transaction<'_>,
    expected_device_id: DeviceId,
    leases: &HashMap<ExclusiveDeviceLeaseId, ExclusiveDeviceLeaseGrant>,
    reset_receipts: &[DeviceResetReceipt],
) -> Result<HashMap<ExclusiveDeviceLeaseId, DeviceReconciliationReceipt>, LeaseStoreError> {
    let mut stmt = tx.prepare(
        "SELECT device_lease_id, receipt_id, device_id, holder_node,
                holder_node_boot_generation, exclusivity_epoch, fencing_token,
                reset_generation, reset_receipt_id, endpoint_fenced_old_token,
                queue_drained, vram_and_secrets_zeroized, ack_gateway_id,
                ack_observed_high_water, ack_digest
         FROM device_reconciliation_receipts
         WHERE device_id = ?1",
    )?;
    let mut rows = stmt.query(params![expected_device_id.as_bytes().as_slice()])?;
    let mut receipts = HashMap::new();

    while let Some(row) = rows.next()? {
        let device_lease_id = ExclusiveDeviceLeaseId::from_bytes(decode_16(
            row.get(0)?,
            "device_reconciliation_receipts.device_lease_id",
        )?);
        let receipt_id = ReceiptId::from_bytes(decode_16(
            row.get(1)?,
            "device_reconciliation_receipts.receipt_id",
        )?);
        let device_id = DeviceId::from_bytes(decode_16(
            row.get(2)?,
            "device_reconciliation_receipts.device_id",
        )?);
        let holder_node = decode_cell_identity(
            &row.get::<_, Vec<u8>>(3)?,
            "device_reconciliation_receipts.holder_node",
        )?;
        let holder_node_boot_generation = decode_generation(
            row.get(4)?,
            "device_reconciliation_receipts.holder_node_boot_generation",
        )?;
        let exclusivity_epoch = decode_epoch(
            row.get(5)?,
            "device_reconciliation_receipts.exclusivity_epoch",
        )?;
        let fencing_token =
            decode_fencing_token(row.get(6)?, "device_reconciliation_receipts.fencing_token")?;
        let reset_generation = decode_generation(
            row.get(7)?,
            "device_reconciliation_receipts.reset_generation",
        )?;
        let reset_receipt_id = ReceiptId::from_bytes(decode_16(
            row.get(8)?,
            "device_reconciliation_receipts.reset_receipt_id",
        )?);
        let endpoint_fenced_old_token = row.get::<_, i64>(9)? == 1;
        let queue_drained = row.get::<_, i64>(10)? == 1;
        let vram_and_secrets_zeroized = row.get::<_, i64>(11)? == 1;
        let ack_gateway_id = decode_16(
            row.get(12)?,
            "device_reconciliation_receipts.ack_gateway_id",
        )?;
        let ack_observed_high_water = decode_u64(
            row.get(13)?,
            "device_reconciliation_receipts.ack_observed_high_water",
        )?;
        let ack_digest = decode_32(row.get(14)?, "device_reconciliation_receipts.ack_digest")?;

        let lease = leases
            .get(&device_lease_id)
            .ok_or(LeaseStoreError::CorruptRecord(
                "device_reconciliation_receipts references unknown lease",
            ))?;
        if lease.state != ExclusiveDeviceLeaseState::Returned {
            return Err(LeaseStoreError::CorruptRecord(
                "device_reconciliation_receipts references non-RETURNED lease",
            ));
        }
        let reset_receipt = reset_receipts
            .iter()
            .find(|r| r.receipt_id == reset_receipt_id && r.device_lease_id == device_lease_id)
            .copied()
            .ok_or(LeaseStoreError::CorruptRecord(
                "device_reconciliation_receipts missing matching DeviceResetReceipt",
            ))?;
        let ack = FenceBarrierAck::new(
            *device_lease_id.as_bytes(),
            lease.fence_scope,
            exclusivity_epoch,
            fencing_token,
            ack_gateway_id,
            ack_observed_high_water,
            ack_digest,
        );
        if verify_device_fence_ack(&ack, lease).is_err()
            || !endpoint_fenced_old_token
            || !queue_drained
            || !vram_and_secrets_zeroized
        {
            return Err(LeaseStoreError::CorruptRecord(
                "stored device_reconciliation_receipts invalid",
            ));
        }
        receipts.insert(
            device_lease_id,
            DeviceReconciliationReceipt::new(
                receipt_id,
                device_lease_id,
                device_id,
                holder_node,
                holder_node_boot_generation,
                exclusivity_epoch,
                fencing_token,
                reset_generation,
                endpoint_fenced_old_token,
                queue_drained,
                vram_and_secrets_zeroized,
                reset_receipt,
                ack,
            ),
        );
    }
    Ok(receipts)
}

fn load_device_lease_rows(
    tx: &Transaction<'_>,
    expected_device_id: DeviceId,
    expected_cell: CellIdentity,
) -> Result<LoadedDeviceLeaseRows, LeaseStoreError> {
    let mut stmt = tx.prepare(
        "SELECT device_lease_id, device_id, holder_node, holder_node_boot_generation,
                exclusivity_epoch, fencing_token, fence_scope_cell_id, reset_generation, state
         FROM device_leases WHERE device_id = ?1",
    )?;
    let mut rows = stmt.query(params![expected_device_id.as_bytes().as_slice()])?;
    let mut leases = HashMap::new();
    let mut active_holder: Option<ExclusiveDeviceLeaseGrant> = None;
    let mut returned_count: u64 = 0;

    while let Some(row) = rows.next()? {
        let device_lease_id = ExclusiveDeviceLeaseId::from_bytes(decode_16(
            row.get(0)?,
            "device_leases.device_lease_id",
        )?);
        let device_id = DeviceId::from_bytes(decode_16(row.get(1)?, "device_leases.device_id")?);
        let holder_node =
            decode_cell_identity(&row.get::<_, Vec<u8>>(2)?, "device_leases.holder_node")?;
        let scope_cell = decode_cell_identity(
            &row.get::<_, Vec<u8>>(6)?,
            "device_leases.fence_scope_cell_id",
        )?;
        if device_id != expected_device_id
            || holder_node != expected_cell
            || scope_cell != holder_node
        {
            return Err(LeaseStoreError::CorruptRecord(
                "device_leases identity mismatch",
            ));
        }
        let holder_node_boot_generation =
            decode_generation(row.get(3)?, "device_leases.holder_node_boot_generation")?;
        let exclusivity_epoch = decode_epoch(row.get(4)?, "device_leases.exclusivity_epoch")?;
        let fencing_token = decode_fencing_token(row.get(5)?, "device_leases.fencing_token")?;
        let reset_generation = decode_generation(row.get(7)?, "device_leases.reset_generation")?;
        let state_str: String = row.get(8)?;
        let state = decode_device_state(&state_str)?;

        let grant = ExclusiveDeviceLeaseGrant {
            device_lease_id,
            device_id,
            holder_node,
            holder_node_boot_generation,
            exclusivity_epoch,
            fencing_token,
            fence_scope: FenceScope::cell(scope_cell),
            reset_generation,
            state,
        };
        match state {
            ExclusiveDeviceLeaseState::DeviceReserved
            | ExclusiveDeviceLeaseState::Resetting
            | ExclusiveDeviceLeaseState::Fencing
            | ExclusiveDeviceLeaseState::Quarantined => {
                if active_holder.is_some() {
                    return Err(LeaseStoreError::CorruptRecord(
                        "multiple non-returned device leases for the same device_id",
                    ));
                }
                active_holder = Some(grant);
            }
            ExclusiveDeviceLeaseState::Returned => {
                returned_count += 1;
            }
        }
        leases.insert(device_lease_id, grant);
    }
    Ok((leases, active_holder, returned_count))
}

fn load_device_reset_receipt_rows(
    tx: &Transaction<'_>,
    expected_device_id: DeviceId,
    leases: &HashMap<ExclusiveDeviceLeaseId, ExclusiveDeviceLeaseGrant>,
) -> Result<Vec<DeviceResetReceipt>, LeaseStoreError> {
    let mut receipt_stmt = tx.prepare(
        "SELECT receipt_id, device_lease_id, holder_node, holder_node_boot_generation,
                exclusivity_epoch, fencing_token, reset_generation
         FROM device_reset_receipts
         WHERE device_id = ?1
         ORDER BY seq ASC",
    )?;
    let mut receipt_rows = receipt_stmt.query(params![expected_device_id.as_bytes().as_slice()])?;
    let mut reset_receipts = Vec::new();

    while let Some(row) = receipt_rows.next()? {
        let receipt_id =
            ReceiptId::from_bytes(decode_16(row.get(0)?, "device_reset_receipts.receipt_id")?);
        let device_lease_id = ExclusiveDeviceLeaseId::from_bytes(decode_16(
            row.get(1)?,
            "device_reset_receipts.device_lease_id",
        )?);
        let holder_node = decode_cell_identity(
            &row.get::<_, Vec<u8>>(2)?,
            "device_reset_receipts.holder_node",
        )?;
        let holder_node_boot_generation = decode_generation(
            row.get(3)?,
            "device_reset_receipts.holder_node_boot_generation",
        )?;
        let exclusivity_epoch =
            decode_epoch(row.get(4)?, "device_reset_receipts.exclusivity_epoch")?;
        let fencing_token =
            decode_fencing_token(row.get(5)?, "device_reset_receipts.fencing_token")?;
        let reset_generation =
            decode_generation(row.get(6)?, "device_reset_receipts.reset_generation")?;

        let receipt = DeviceResetReceipt::new(
            receipt_id,
            device_lease_id,
            holder_node,
            holder_node_boot_generation,
            exclusivity_epoch,
            fencing_token,
            reset_generation,
        );
        let lease = leases
            .get(&device_lease_id)
            .ok_or(LeaseStoreError::CorruptRecord(
                "device_reset_receipts references unknown lease",
            ))?;
        if lease.state != ExclusiveDeviceLeaseState::Returned
            || !receipt_binds_lease(&receipt, lease)
        {
            return Err(LeaseStoreError::CorruptRecord(
                "device_reset_receipts row does not bind a RETURNED device lease",
            ));
        }
        reset_receipts.push(receipt);
    }
    Ok(reset_receipts)
}

fn encode_u64(value: u64, _field: &'static str) -> Result<i64, LeaseStoreError> {
    i64::try_from(value).map_err(|_| LeaseStoreError::CorruptRecord("u64 value exceeds SQLite i64"))
}

fn decode_u64(value: i64, _field: &'static str) -> Result<u64, LeaseStoreError> {
    u64::try_from(value).map_err(|_| LeaseStoreError::CorruptRecord("negative integer in SQLite"))
}

fn decode_16(bytes: Vec<u8>, _field: &'static str) -> Result<[u8; 16], LeaseStoreError> {
    bytes
        .try_into()
        .map_err(|_| LeaseStoreError::CorruptRecord("expected 16-byte nominal id"))
}

fn decode_32(bytes: Vec<u8>, _field: &'static str) -> Result<[u8; 32], LeaseStoreError> {
    bytes
        .try_into()
        .map_err(|_| LeaseStoreError::CorruptRecord("expected 32-byte digest"))
}

fn decode_cell_identity(
    bytes: &[u8],
    _field: &'static str,
) -> Result<CellIdentity, LeaseStoreError> {
    let arr: [u8; 16] = bytes
        .try_into()
        .map_err(|_| LeaseStoreError::CorruptRecord("expected 16-byte cell identity"))?;
    Ok(CellIdentity::from_domain(SchedulerDomainId::from_bytes(
        arr,
    )))
}

fn decode_generation(value: i64, field: &'static str) -> Result<Generation, LeaseStoreError> {
    let raw = decode_u64(value, field)?;
    let nz =
        NonZeroU64::new(raw).ok_or(LeaseStoreError::CorruptRecord("zero generation in SQLite"))?;
    Ok(Generation::new(nz))
}

fn decode_epoch(value: i64, field: &'static str) -> Result<CellEpoch, LeaseStoreError> {
    let raw = decode_u64(value, field)?;
    CellEpoch::from_u64(raw).ok_or(LeaseStoreError::CorruptRecord("zero epoch in SQLite"))
}

fn decode_fencing_token(
    value: i64,
    field: &'static str,
) -> Result<CellFencingToken, LeaseStoreError> {
    let raw = decode_u64(value, field)?;
    CellFencingToken::from_u64(raw).ok_or(LeaseStoreError::CorruptRecord(
        "zero fencing_token in SQLite",
    ))
}

const fn encode_quota_state(state: QuotaLeaseState) -> &'static str {
    match state {
        QuotaLeaseState::Issued => "ISSUED",
        QuotaLeaseState::Active => "ACTIVE",
        QuotaLeaseState::Closing => "CLOSING",
        QuotaLeaseState::Closed => "CLOSED",
        QuotaLeaseState::Cancelled => "CANCELLED",
        QuotaLeaseState::Fenced => "FENCED",
        QuotaLeaseState::Quarantined => "QUARANTINED",
    }
}

fn decode_quota_state(raw: &str) -> Result<QuotaLeaseState, LeaseStoreError> {
    match raw {
        "ISSUED" => Ok(QuotaLeaseState::Issued),
        "ACTIVE" => Ok(QuotaLeaseState::Active),
        "CLOSING" => Ok(QuotaLeaseState::Closing),
        "CLOSED" => Ok(QuotaLeaseState::Closed),
        "CANCELLED" => Ok(QuotaLeaseState::Cancelled),
        "FENCED" => Ok(QuotaLeaseState::Fenced),
        "QUARANTINED" => Ok(QuotaLeaseState::Quarantined),
        _ => Err(LeaseStoreError::CorruptRecord("unknown QuotaLeaseState")),
    }
}

const fn encode_capacity_state(state: CapacityLeaseState) -> &'static str {
    match state {
        CapacityLeaseState::GlobalReserved => "GLOBAL_RESERVED",
        CapacityLeaseState::TargetPrepared => "TARGET_PREPARED",
        CapacityLeaseState::Active => "ACTIVE",
        CapacityLeaseState::Returning => "RETURNING",
        CapacityLeaseState::Fenced => "FENCED",
        CapacityLeaseState::Quarantined => "QUARANTINED",
        CapacityLeaseState::Returned => "RETURNED",
    }
}

fn decode_capacity_state(raw: &str) -> Result<CapacityLeaseState, LeaseStoreError> {
    match raw {
        "GLOBAL_RESERVED" => Ok(CapacityLeaseState::GlobalReserved),
        "TARGET_PREPARED" => Ok(CapacityLeaseState::TargetPrepared),
        "ACTIVE" => Ok(CapacityLeaseState::Active),
        "RETURNING" => Ok(CapacityLeaseState::Returning),
        "FENCED" => Ok(CapacityLeaseState::Fenced),
        "QUARANTINED" => Ok(CapacityLeaseState::Quarantined),
        "RETURNED" => Ok(CapacityLeaseState::Returned),
        _ => Err(LeaseStoreError::CorruptRecord("unknown CapacityLeaseState")),
    }
}

const fn encode_device_state(state: ExclusiveDeviceLeaseState) -> &'static str {
    match state {
        ExclusiveDeviceLeaseState::DeviceReserved => "DEVICE_RESERVED",
        ExclusiveDeviceLeaseState::Resetting => "RESETTING",
        ExclusiveDeviceLeaseState::Fencing => "FENCING",
        ExclusiveDeviceLeaseState::Quarantined => "QUARANTINED",
        ExclusiveDeviceLeaseState::Returned => "RETURNED",
    }
}

fn decode_device_state(raw: &str) -> Result<ExclusiveDeviceLeaseState, LeaseStoreError> {
    match raw {
        "DEVICE_RESERVED" => Ok(ExclusiveDeviceLeaseState::DeviceReserved),
        "RESETTING" => Ok(ExclusiveDeviceLeaseState::Resetting),
        "FENCING" => Ok(ExclusiveDeviceLeaseState::Fencing),
        "QUARANTINED" => Ok(ExclusiveDeviceLeaseState::Quarantined),
        "RETURNED" => Ok(ExclusiveDeviceLeaseState::Returned),
        _ => Err(LeaseStoreError::CorruptRecord(
            "unknown ExclusiveDeviceLeaseState",
        )),
    }
}
