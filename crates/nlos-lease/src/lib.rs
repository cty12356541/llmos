//! Single-Cell `QuotaLease` / `CapacityLease` / `ExclusiveDeviceLease` grant
//! **冻结声明（F1 / 2026-10-05）**：本 crate 当前在 workspace 内零生产消费方
//! （`cargo tree -i` 复核）。处置裁定：**冻结保留**——能力与测试不作废，待接线
//! （接线前提见下行）。冻结期内禁止视为已装配能力引用；解冻 = 出现首个生产
//! 消费方或显式接线计划落地。登记：docs/management/stage-c-progress.md 2026-10-05 F1 段。

//! 接线前提：slice-k/daemon 装配 `CellAuthority` 与 lease 三族 admit/release 面（W38 lease-admit 前片已指向该路径）。
//! 2026-10-05 W48：接线计划已落地（slice-k `CellHost` 装配 + nlos-slice-k→nlos-lease 依赖边）——解冻条件达成；生产调用方为 `CellHost` 的 quota 面。
//! 2026-10-05 W49：ADR-0021——三族 grantor 改持共享权威 `Arc<CellAuthority>`（`CellAuthority::into_shared`），
//! capacity/device 族随 W49 进入单进程装配；quota 族仍是唯一 epoch 推进入口。
//! 2026-10-24 W62：三族租约 `SQLite WAL + FULL` 持久化与崩溃恢复（`SqliteLeaseStore` / `open_durable`）。
//! 2026-10-24 W63：三族租约跨纪元对账回执（`LEASE-RECON-001`、`LEASE-FENCE-001`）、跨节点迁移（`LEASE-MIGRATE-001`、`LEASE-CAPACITY-002`、`LEASE-DEVICE-001`）与本地 WAL 丢失隔离（`LEASE-WAL-001`）。
//! (C-LEASE slices).
//!
//! Control-plane prepaid transfer for one Cell:
//! - `QuotaLease` (`LEASE-GRANT-001`): `AVAILABLE` → node `LEASE`; face value
//!   deducted from the available pool before issue.
//! - `CapacityLease` (`LEASE-CAPACITY-001` prefix): source pool →
//!   `GLOBAL_RESERVED`; `amount` deducted before issue.
//! - `ExclusiveDeviceLease` (`LEASE-DEVICE-001`): FREE `DeviceLeaseHead` →
//!   `DEVICE_RESERVED`; device claimed before issue. The return leg is
//!   reset-gated: `DEVICE_RESERVED → RESETTING` declares the holder's
//!   reset+zeroization, and only an accepted `DeviceResetReceipt`
//!   (`RESETTING → RETURNED`) moves the head back to FREE and bumps the head
//!   reset generation.
//!
//! Stale epoch is a typed fail-closed reject for all three families.
//!
//! `QuotaLease` also tracks a node-local monotonic usage high-water
//! (`LEASE-SPEND-001`, `LEASE-REPORT-001`), `ACTIVE → CLOSING → CLOSED`
//! (`LEASE-CLOSE-001`), and idempotent `ISSUED` cancel (`LEASE-PREACTIVE-001`).
//! The high-water moves face value straight from local remaining to spent;
//! this slice has no held-reservation bucket.
//!
//! `QuotaLease` TTL (`LEASE-TTL-001`): a grant may bind a logical
//! `LeaseInstant` deadline (the caller sweeps with `now`; this crate reads no
//! wall clock). `sweep_expired` moves expired `ISSUED` / `ACTIVE` / `CLOSING`
//! leases into `FENCED` — no new use — and `settle_fenced` returns the unspent
//! remainder once. `None` deadlines never expire.
//!
//! Fence admit is delegated to [`CellAuthority::admit`]; this crate does not
//! re-implement the fail-closed fence checks.
//!
//! `CapacityLease` additionally has idempotent
//! `GLOBAL_RESERVED → TARGET_PREPARED`, the host attach receipt edge
//! `TARGET_PREPARED → ACTIVE` ([`HostAttachReceipt`]: idempotency key, axes
//! bound to the committed lease, full `amount` only), and
//! `GLOBAL_RESERVED`/`TARGET_PREPARED`/`ACTIVE → RETURNING → RETURNED`
//! (`LEASE-PREACTIVE-001`). `RETURNING` does not refund; `RETURNED` refunds
//! `amount` once, whether the lease passed through `ACTIVE` or not — capacity
//! is returned whole, not consumed.
//!
//! `QuotaLease` loss (`LEASE-LOSS-001`): advancing the Cell epoch moves
//! `ISSUED` / `ACTIVE` / `CLOSING` / un-settled `FENCED` leases into
//! `QUARANTINED` without returning their face value to `AVAILABLE`. `CLOSED`
//! and `CANCELLED` stay put.
//!
//! Each grantor may be opened in-memory ([`QuotaLeaseGrantor::open`]) or bound
//! to a durable `SQLite` `WAL + FULL` store ([`QuotaLeaseGrantor::open_durable`],
//! [`CapacityLeaseGrantor::open_durable`],
//! [`ExclusiveDeviceLeaseGrantor::open_durable`]).

mod reconcile;
mod store;

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use nlos_cell::{
    CellAdmitError, CellAuthority, CellEpoch, CellError, CellFence, CellFencingToken, CellIdentity,
};
use nlos_types::{
    CapacityLeaseId, DeviceId, ExclusiveDeviceLeaseId, Generation, QuotaLeaseId, ReceiptId,
};

pub use reconcile::{
    CapacityMigrationError, CapacityMigrationOutcome, CapacityReconcileError,
    CapacityReconciliationReceipt, CleanupAttestation, DeviceMigrationError,
    DeviceMigrationOutcome, DeviceReconcileError, DeviceReconciliationReceipt, FenceBarrierAck,
    QuotaMigrationError, QuotaMigrationOutcome, QuotaReconcileError, QuotaReconciliationReceipt,
    QuotaWalLossError,
};
pub use store::{LeaseStoreError, SCHEMA_VERSION, SqliteLeaseStore};

/// Fence scope for a single-Cell grant: the Cell identity itself.
///
/// Cross-cell / multi-gateway scopes are out of scope for this slice.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FenceScope {
    cell: CellIdentity,
}

impl FenceScope {
    /// Single-Cell fence scope keyed by Cell identity.
    #[must_use]
    pub const fn cell(cell: CellIdentity) -> Self {
        Self { cell }
    }

    /// Cell identity that defines this scope.
    #[must_use]
    pub const fn as_cell(self) -> CellIdentity {
        self.cell
    }
}

/// Monotonic logical instant for lease TTL deadlines (`LEASE-TTL-001`).
///
/// An opaque tick counter supplied by the caller; this crate reads no wall
/// clock. Ordering is the only meaning: a lease whose deadline is less than
/// or equal to the presented `now` is expired.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LeaseInstant(u64);

impl LeaseInstant {
    /// Builds an instant from its tick form.
    #[must_use]
    pub const fn new(tick: u64) -> Self {
        Self(tick)
    }

    /// Returns the tick form.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// `QuotaLease` state for the single-Cell prefix.
///
/// Spec chain used here: `ISSUED → ACTIVE → CLOSING → CLOSED`,
/// `ISSUED → CANCELLED`, TTL expiry
/// `ISSUED`/`ACTIVE`/`CLOSING → FENCED → CLOSED` (settle), and non-terminal
/// → `QUARANTINED` on epoch advance.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum QuotaLeaseState {
    /// Prepaid and not yet admitted on the node (`ISSUED`).
    Issued,
    /// Node-local sub-ledger is open (`ACTIVE`).
    Active,
    /// New usage is frozen; remainder is not back in `AVAILABLE` (`CLOSING`).
    Closing,
    /// Close ACK applied; unspent face value returned (`CLOSED`).
    Closed,
    /// Pre-active cancel; full face value returned (`CANCELLED`).
    Cancelled,
    /// TTL deadline reached (`FENCED`): no new use is admitted and only the
    /// settle path (`FENCED → CLOSED`) returns the unspent remainder.
    Fenced,
    /// Unreconciled face value frozen after epoch advance (`QUARANTINED`).
    Quarantined,
}

/// A single-Cell `QuotaLease` grant snapshot (v0.5 §12 `QuotaLease` fields
/// used by this slice, plus the `LEASE-TTL-001` deadline).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuotaLeaseGrant {
    pub(crate) lease_id: QuotaLeaseId,
    pub(crate) cell: CellIdentity,
    pub(crate) node_boot_generation: Generation,
    pub(crate) face_value: u64,
    pub(crate) remaining: u64,
    pub(crate) spent: u64,
    pub(crate) returned: u64,
    pub(crate) epoch: CellEpoch,
    pub(crate) fencing_token: CellFencingToken,
    pub(crate) fence_scope: FenceScope,
    pub(crate) expires_at: Option<LeaseInstant>,
    pub(crate) state: QuotaLeaseState,
}

impl QuotaLeaseGrant {
    /// Stable lease identity.
    #[must_use]
    pub const fn lease_id(self) -> QuotaLeaseId {
        self.lease_id
    }

    /// Target Cell (`node_id` in §12 `QuotaLease`).
    #[must_use]
    pub const fn cell(self) -> CellIdentity {
        self.cell
    }

    /// Node boot generation bound into the grant.
    #[must_use]
    pub const fn node_boot_generation(self) -> Generation {
        self.node_boot_generation
    }

    /// Prepaid face value deducted at grant time.
    #[must_use]
    pub const fn face_value(self) -> u64 {
        self.face_value
    }

    /// Local remainder not yet spent or returned.
    #[must_use]
    pub const fn remaining(self) -> u64 {
        self.remaining
    }

    /// Monotonic usage high-water already consumed from the face value.
    #[must_use]
    pub const fn spent(self) -> u64 {
        self.spent
    }

    /// Face value returned to the control-plane pool.
    #[must_use]
    pub const fn returned(self) -> u64 {
        self.returned
    }

    /// Epoch bound into the grant.
    #[must_use]
    pub const fn epoch(self) -> CellEpoch {
        self.epoch
    }

    /// Fencing token bound into the grant.
    #[must_use]
    pub const fn fencing_token(self) -> CellFencingToken {
        self.fencing_token
    }

    /// Fence scope for this grant.
    #[must_use]
    pub const fn fence_scope(self) -> FenceScope {
        self.fence_scope
    }

    /// TTL deadline bound at grant time (`LEASE-TTL-001`), if any. `None`
    /// never expires. A lease is expired once the deadline is less than or
    /// equal to the sweep's `now`.
    #[must_use]
    pub const fn expires_at(self) -> Option<LeaseInstant> {
        self.expires_at
    }

    /// Lease state after grant.
    #[must_use]
    pub const fn state(self) -> QuotaLeaseState {
        self.state
    }
}

/// Single-Cell `QuotaLease` grantor (in-memory or durable `SQLite`).
///
/// Holds the control-plane AVAILABLE pool and the shared Cell authority
/// used for fence admit. This family remains the epoch-advance entry of the
/// assembly (`advance_epoch_and_quarantine`): sharing the authority (ADR-0021)
/// mints readers, not a second writer.
#[derive(Debug)]
pub struct QuotaLeaseGrantor {
    pub(crate) authority: Arc<CellAuthority>,
    pub(crate) available: u64,
    pub(crate) leases: HashMap<QuotaLeaseId, QuotaLeaseGrant>,
    pub(crate) reconciliation_receipts: HashMap<QuotaLeaseId, Vec<QuotaReconciliationReceipt>>,
    pub(crate) store: Option<Arc<SqliteLeaseStore>>,
}

impl QuotaLeaseGrantor {
    /// Opens an in-memory grantor bound to the process's shared Cell authority
    /// ([`CellAuthority::into_shared`], ADR-0021) with an initial AVAILABLE
    /// pool.
    #[must_use]
    pub fn open(authority: Arc<CellAuthority>, available: u64) -> Self {
        Self {
            authority,
            available,
            leases: HashMap::new(),
            reconciliation_receipts: HashMap::new(),
            store: None,
        }
    }

    /// Alias for [`Self::open`].
    #[must_use]
    pub fn new(authority: Arc<CellAuthority>, available: u64) -> Self {
        Self::open(authority, available)
    }

    /// Opens a durable `QuotaLeaseGrantor` backed by `SQLite` (`WAL + FULL`)
    /// at `path`, seeding `available` on first creation or rehydrating full
    /// pool/lease state on reopen.
    ///
    /// # Errors
    ///
    /// Fails closed on storage failure, unsupported schema version, or
    /// corrupted/tampered pool/lease records.
    pub fn open_durable(
        path: impl AsRef<Path>,
        authority: Arc<CellAuthority>,
        available: u64,
    ) -> Result<Self, LeaseStoreError> {
        let store = Arc::new(SqliteLeaseStore::open(path)?);
        Self::open_with_store(store, authority, available)
    }

    /// Opens a durable `QuotaLeaseGrantor` over a shared [`SqliteLeaseStore`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::open_durable`].
    pub fn open_with_store(
        store: Arc<SqliteLeaseStore>,
        authority: Arc<CellAuthority>,
        available: u64,
    ) -> Result<Self, LeaseStoreError> {
        let (rehydrated_available, leases, reconciliation_receipts) =
            store.open_or_rehydrate_quota(&authority, available)?;
        Ok(Self {
            authority,
            available: rehydrated_available,
            leases,
            reconciliation_receipts,
            store: Some(store),
        })
    }

    /// Remaining control-plane AVAILABLE for this Cell.
    #[must_use]
    pub const fn available(&self) -> u64 {
        self.available
    }

    /// Grants a `QuotaLease` against a presented Cell fence.
    ///
    /// `LEASE-GRANT-001`: `face_value` is deducted from AVAILABLE before the
    /// lease is issued. Fail-closed fence checks run via
    /// [`CellAuthority::admit`] before any deduct. An identical
    /// `(lease_id, face_value, expires_at)` returns the committed lease and
    /// does not deduct again. A different face value for the same id is
    /// [`QuotaLeaseGrantError::ConflictingFace`]; a different TTL deadline is
    /// [`QuotaLeaseGrantError::ConflictingTtl`]. The grantor reads no clock:
    /// a deadline already in the past simply fences the lease on the first
    /// [`Self::sweep_expired`].
    ///
    /// # Errors
    ///
    /// Typed reject: [`QuotaLeaseGrantError`].
    pub fn grant(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
        face_value: u64,
        expires_at: Option<LeaseInstant>,
    ) -> Result<QuotaLeaseGrant, QuotaLeaseGrantError> {
        if let Some(existing) = self.leases.get(&lease_id).copied() {
            if existing.face_value == face_value && existing.expires_at == expires_at {
                return Ok(existing);
            }
            self.authority.admit(presented)?;
            if existing.face_value != face_value {
                return Err(QuotaLeaseGrantError::ConflictingFace {
                    existing: existing.face_value,
                    requested: face_value,
                });
            }
            return Err(QuotaLeaseGrantError::ConflictingTtl {
                existing: existing.expires_at,
                requested: expires_at,
            });
        }
        self.authority.admit(presented)?;
        if face_value > self.available {
            return Err(QuotaLeaseGrantError::InsufficientAvailable {
                requested: face_value,
                available: self.available,
            });
        }
        let next_available = self.available - face_value;
        let cell = self.authority.identity();
        let grant = QuotaLeaseGrant {
            lease_id,
            cell,
            node_boot_generation: self.authority.node_boot_generation(),
            face_value,
            remaining: face_value,
            spent: 0,
            returned: 0,
            epoch: self.authority.epoch(),
            fencing_token: self.authority.fencing_token(),
            fence_scope: FenceScope::cell(cell),
            expires_at,
            state: QuotaLeaseState::Issued,
        };
        if let Some(store) = &self.store {
            store.persist_quota_grant(&grant, self.available, next_available)?;
        }
        self.available = next_available;
        self.leases.insert(lease_id, grant);
        Ok(grant)
    }

    /// Reads the committed lease snapshot.
    ///
    /// # Errors
    ///
    /// [`QuotaLeaseLedgerError::UnknownLease`] when `lease_id` was never issued.
    pub fn query(&self, lease_id: QuotaLeaseId) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
        self.leases
            .get(&lease_id)
            .copied()
            .ok_or(QuotaLeaseLedgerError::UnknownLease)
    }

    /// Moves `ISSUED → ACTIVE`. A second call returns the committed lease.
    ///
    /// # Errors
    ///
    /// Typed reject: [`QuotaLeaseLedgerError`].
    pub fn activate(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
    ) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
        self.authority.admit(presented)?;
        let existing = self.query(lease_id)?;
        match existing.state {
            QuotaLeaseState::Issued => {
                let next = QuotaLeaseGrant {
                    state: QuotaLeaseState::Active,
                    ..existing
                };
                if let Some(store) = &self.store {
                    store.persist_quota_state_transition(
                        lease_id,
                        QuotaLeaseState::Issued,
                        &next,
                        None,
                    )?;
                }
                self.leases.insert(lease_id, next);
                Ok(next)
            }
            QuotaLeaseState::Active => Ok(existing),
            state => Err(QuotaLeaseLedgerError::ActivateRequiresIssued { state }),
        }
    }

    /// Alias for [`Self::activate`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::activate`].
    pub fn admit_operation(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
    ) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
        self.activate(presented, lease_id)
    }

    /// Advances the monotonic usage high-water inside the issued face value.
    ///
    /// `LEASE-SPEND-001` / `LEASE-REPORT-001`: the same high-water is
    /// idempotent. A lower mark is a typed regression. A mark above
    /// `face_value` is refused, so the lease cannot expand. `CLOSING` accepts
    /// only the already-committed mark.
    ///
    /// # Errors
    ///
    /// Typed reject: [`QuotaLeaseLedgerError`].
    pub fn report_usage(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
        high_water: u64,
    ) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
        self.authority.admit(presented)?;
        let existing = self.query(lease_id)?;
        match existing.state {
            QuotaLeaseState::Active => {
                let mut next = existing;
                apply_quota_high_water(&mut next, high_water)?;
                if next.spent != existing.spent {
                    if let Some(store) = &self.store {
                        store.persist_quota_usage(&existing, &next)?;
                    }
                    self.leases.insert(lease_id, next);
                }
                Ok(next)
            }
            QuotaLeaseState::Closing => closing_high_water(&existing, high_water),
            state => Err(QuotaLeaseLedgerError::NotActive { state }),
        }
    }

    /// Alias for [`Self::report_usage`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::report_usage`].
    pub fn report_spend(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
        high_water: u64,
    ) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
        self.report_usage(presented, lease_id, high_water)
    }

    /// `ACTIVE → CLOSING`. Does not return remainder to `AVAILABLE`.
    ///
    /// # Errors
    ///
    /// Typed reject: [`QuotaLeaseLedgerError`].
    pub fn begin_close(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
    ) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
        self.authority.admit(presented)?;
        let existing = self.query(lease_id)?;
        match existing.state {
            QuotaLeaseState::Active => {
                let next = QuotaLeaseGrant {
                    state: QuotaLeaseState::Closing,
                    ..existing
                };
                if let Some(store) = &self.store {
                    store.persist_quota_state_transition(
                        lease_id,
                        QuotaLeaseState::Active,
                        &next,
                        None,
                    )?;
                }
                self.leases.insert(lease_id, next);
                Ok(next)
            }
            QuotaLeaseState::Closing => Ok(existing),
            state => Err(QuotaLeaseLedgerError::NotActive { state }),
        }
    }

    /// `CLOSING → CLOSED`. Returns only the unspent remainder to `AVAILABLE`.
    ///
    /// A second ACK returns the committed lease and does not refund again.
    ///
    /// # Errors
    ///
    /// Typed reject: [`QuotaLeaseLedgerError`].
    pub fn ack_close(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
    ) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
        self.authority.admit(presented)?;
        let existing = self.query(lease_id)?;
        match existing.state {
            QuotaLeaseState::Closing => {
                let refund = existing.remaining;
                let next_available = self.available + refund;
                let next = QuotaLeaseGrant {
                    returned: existing.returned + refund,
                    remaining: 0,
                    state: QuotaLeaseState::Closed,
                    ..existing
                };
                if let Some(store) = &self.store {
                    store.persist_quota_state_transition(
                        lease_id,
                        QuotaLeaseState::Closing,
                        &next,
                        Some((self.available, next_available)),
                    )?;
                }
                self.leases.insert(lease_id, next);
                self.available = next_available;
                Ok(next)
            }
            QuotaLeaseState::Closed => Ok(existing),
            state => Err(QuotaLeaseLedgerError::NotClosing { state }),
        }
    }

    /// Alias for [`Self::ack_close`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::ack_close`].
    pub fn finalize_close(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
    ) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
        self.ack_close(presented, lease_id)
    }

    /// Idempotent `ISSUED → CANCELLED`. Refunds the full face value once.
    ///
    /// A lease that has left `ISSUED` is refused: this slice has no fence
    /// proof that would let an active lease cancel.
    ///
    /// # Errors
    ///
    /// Typed reject: [`QuotaLeaseLedgerError`].
    pub fn cancel(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
    ) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
        self.authority.admit(presented)?;
        let existing = self.query(lease_id)?;
        match existing.state {
            QuotaLeaseState::Issued => {
                let refund = existing.face_value;
                let next_available = self.available + refund;
                let next = QuotaLeaseGrant {
                    remaining: 0,
                    returned: refund,
                    spent: 0,
                    state: QuotaLeaseState::Cancelled,
                    ..existing
                };
                if let Some(store) = &self.store {
                    store.persist_quota_state_transition(
                        lease_id,
                        QuotaLeaseState::Issued,
                        &next,
                        Some((self.available, next_available)),
                    )?;
                }
                self.leases.insert(lease_id, next);
                self.available = next_available;
                Ok(next)
            }
            QuotaLeaseState::Cancelled => Ok(existing),
            state => Err(QuotaLeaseLedgerError::CancelRequiresNeverActive { state }),
        }
    }

    /// Alias for [`Self::cancel`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::cancel`].
    pub fn cancel_issued(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
    ) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
        self.cancel(presented, lease_id)
    }

    /// Sweeps TTL-expired leases into `FENCED` (`LEASE-TTL-001`).
    ///
    /// A lease is expired when its deadline is less than or equal to `now`;
    /// `None` deadlines never expire. `ISSUED`, `ACTIVE`, and `CLOSING`
    /// leases move to `FENCED`: no new use is admitted afterwards, and only
    /// [`Self::settle_fenced`] returns the unspent remainder. Terminal states
    /// are untouched. Returns the ids fenced by this sweep, sorted by lease
    /// id; an idempotent re-sweep returns an empty vector.
    ///
    /// This is a grantor-local compaction pass on the caller's logical clock:
    /// no fence is presented and no refund happens here.
    pub fn sweep_expired(&mut self, now: LeaseInstant) -> Vec<QuotaLeaseId> {
        let mut to_fence: Vec<(QuotaLeaseId, QuotaLeaseState)> = self
            .leases
            .values()
            .filter(|lease| lease.expires_at.is_some_and(|deadline| deadline <= now))
            .filter_map(|lease| match lease.state {
                QuotaLeaseState::Issued | QuotaLeaseState::Active | QuotaLeaseState::Closing => {
                    Some((lease.lease_id, lease.state))
                }
                QuotaLeaseState::Closed
                | QuotaLeaseState::Cancelled
                | QuotaLeaseState::Fenced
                | QuotaLeaseState::Quarantined => None,
            })
            .collect();
        to_fence.sort_unstable_by_key(|(id, _)| *id);
        if to_fence.is_empty() {
            return Vec::new();
        }
        if let Some(store) = &self.store
            && store.persist_quota_sweep(&to_fence).is_err()
        {
            return Vec::new();
        }
        let mut fenced = Vec::with_capacity(to_fence.len());
        for (lease_id, _) in to_fence {
            if let Some(lease) = self.leases.get_mut(&lease_id) {
                lease.state = QuotaLeaseState::Fenced;
                fenced.push(lease_id);
            }
        }
        fenced
    }

    /// `FENCED → CLOSED`: settles an expired lease and returns the unspent
    /// remainder to `AVAILABLE` once.
    ///
    /// The spent high-water stays consumed; only `remaining` comes back. A
    /// second settle returns the committed lease and does not refund again.
    /// A lease that is neither `FENCED` nor `CLOSED` is refused — the settle
    /// window also closes at epoch loss, where `LEASE-LOSS-001` quarantine
    /// freezes all unreconciled face value.
    ///
    /// # Errors
    ///
    /// Typed reject: [`QuotaLeaseLedgerError`].
    pub fn settle_fenced(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
    ) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
        self.authority.admit(presented)?;
        let existing = self.query(lease_id)?;
        match existing.state {
            QuotaLeaseState::Fenced => {
                let refund = existing.remaining;
                let next_available = self.available + refund;
                let next = QuotaLeaseGrant {
                    returned: existing.returned + refund,
                    remaining: 0,
                    state: QuotaLeaseState::Closed,
                    ..existing
                };
                if let Some(store) = &self.store {
                    store.persist_quota_state_transition(
                        lease_id,
                        QuotaLeaseState::Fenced,
                        &next,
                        Some((self.available, next_available)),
                    )?;
                }
                self.leases.insert(lease_id, next);
                self.available = next_available;
                Ok(next)
            }
            QuotaLeaseState::Closed => Ok(existing),
            state => Err(QuotaLeaseLedgerError::NotFenced { state }),
        }
    }

    /// Advances the Cell epoch and quarantines unreconciled leases.
    ///
    /// `LEASE-LOSS-001`: `ISSUED`, `ACTIVE`, `CLOSING`, and un-settled
    /// `FENCED` leases bound to the previous epoch become `QUARANTINED`.
    /// Their face value is not returned to `AVAILABLE`. `CLOSED` and
    /// `CANCELLED` are already reconciled.
    ///
    /// # Errors
    ///
    /// [`CellError::GenerationExhausted`] when the epoch cannot advance, or
    /// [`CellError::DataDirUnavailable`] / [`CellError::DataDirCorrupt`] when
    /// the durable store fails to persist the quarantine.
    pub fn advance_epoch_and_quarantine(&mut self) -> Result<CellFence, CellError> {
        let next_epoch = self
            .authority
            .epoch()
            .checked_next()
            .ok_or(CellError::GenerationExhausted)?;
        let next_token = self
            .authority
            .fencing_token()
            .checked_next()
            .ok_or(CellError::GenerationExhausted)?;

        let mut to_quarantine: Vec<(QuotaLeaseId, QuotaLeaseState)> = self
            .leases
            .values()
            .filter(|lease| lease.epoch < next_epoch)
            .filter_map(|lease| match lease.state {
                QuotaLeaseState::Issued
                | QuotaLeaseState::Active
                | QuotaLeaseState::Closing
                | QuotaLeaseState::Fenced => Some((lease.lease_id, lease.state)),
                QuotaLeaseState::Closed
                | QuotaLeaseState::Cancelled
                | QuotaLeaseState::Quarantined => None,
            })
            .collect();
        to_quarantine.sort_unstable_by_key(|(id, _)| *id);

        if let Some(store) = &self.store {
            store
                .persist_quota_epoch_advance(next_epoch, next_token, &to_quarantine)
                .map_err(LeaseStoreError::into_cell_error)?;
        }

        let fence = self.authority.advance_epoch()?;
        for (lease_id, _) in to_quarantine {
            if let Some(lease) = self.leases.get_mut(&lease_id) {
                lease.state = QuotaLeaseState::Quarantined;
            }
        }
        Ok(fence)
    }
}

fn apply_quota_high_water(
    lease: &mut QuotaLeaseGrant,
    high_water: u64,
) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
    if high_water < lease.spent {
        return Err(QuotaLeaseLedgerError::UsageRegression {
            reported: high_water,
            current: lease.spent,
        });
    }
    if high_water > lease.face_value {
        return Err(QuotaLeaseLedgerError::UsageExceedsFace {
            reported: high_water,
            face_value: lease.face_value,
        });
    }
    lease.spent = high_water;
    lease.remaining = lease.face_value - lease.spent;
    Ok(*lease)
}

fn closing_high_water(
    lease: &QuotaLeaseGrant,
    high_water: u64,
) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
    if high_water < lease.spent {
        return Err(QuotaLeaseLedgerError::UsageRegression {
            reported: high_water,
            current: lease.spent,
        });
    }
    if high_water == lease.spent {
        return Ok(*lease);
    }
    Err(QuotaLeaseLedgerError::NewReserveForbidden)
}

/// Typed fail-closed rejects for a `QuotaLease` grant attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotaLeaseGrantError {
    /// Presented epoch is older than the grantor's current epoch.
    StaleEpoch {
        /// Epoch on the presentation.
        presented: CellEpoch,
        /// Epoch currently held by the grantor.
        current: CellEpoch,
    },
    /// Presented epoch is not current and is not older (fail closed).
    EpochMismatch {
        /// Epoch on the presentation.
        presented: CellEpoch,
        /// Epoch currently held by the grantor.
        current: CellEpoch,
    },
    /// Presented boot generation does not match the grantor.
    BootGenerationMismatch {
        /// Boot generation on the presentation.
        presented: Generation,
        /// Boot generation currently held by the grantor.
        current: Generation,
    },
    /// Presented identity is not this Cell.
    IdentityMismatch {
        /// Identity on the presentation.
        presented: CellIdentity,
        /// Identity of this grantor.
        current: CellIdentity,
    },
    /// Epoch matches but the fencing token does not.
    FencingTokenMismatch {
        /// Token on the presentation.
        presented: CellFencingToken,
        /// Token currently held by the grantor.
        current: CellFencingToken,
    },
    /// AVAILABLE is strictly less than the requested `face_value`.
    InsufficientAvailable {
        /// Requested `face_value`.
        requested: u64,
        /// Remaining AVAILABLE before the attempt.
        available: u64,
    },
    /// The same `lease_id` was already issued with a different face value.
    ConflictingFace {
        /// Face value on the committed lease.
        existing: u64,
        /// Face value on this grant attempt.
        requested: u64,
    },
    /// The same `lease_id` was already issued with a different TTL deadline.
    ConflictingTtl {
        /// Deadline on the committed lease (`None` never expires).
        existing: Option<LeaseInstant>,
        /// Deadline on this grant attempt (`None` never expires).
        requested: Option<LeaseInstant>,
    },
    /// Durable `SQLite` store rejected or failed to persist the grant.
    Storage(&'static str),
}

impl From<CellAdmitError> for QuotaLeaseGrantError {
    fn from(error: CellAdmitError) -> Self {
        match error {
            CellAdmitError::StaleEpoch { presented, current } => {
                Self::StaleEpoch { presented, current }
            }
            CellAdmitError::EpochMismatch { presented, current } => {
                Self::EpochMismatch { presented, current }
            }
            CellAdmitError::BootGenerationMismatch { presented, current } => {
                Self::BootGenerationMismatch { presented, current }
            }
            CellAdmitError::IdentityMismatch { presented, current } => {
                Self::IdentityMismatch { presented, current }
            }
            CellAdmitError::FencingTokenMismatch { presented, current } => {
                Self::FencingTokenMismatch { presented, current }
            }
        }
    }
}

impl From<LeaseStoreError> for QuotaLeaseGrantError {
    fn from(error: LeaseStoreError) -> Self {
        Self::Storage(error.reason())
    }
}

impl fmt::Display for QuotaLeaseGrantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleEpoch { presented, current } => write!(
                formatter,
                "stale Cell epoch for QuotaLease grant: presented {} < current {}",
                presented.get(),
                current.get()
            ),
            Self::EpochMismatch { presented, current } => write!(
                formatter,
                "Cell epoch mismatch for QuotaLease grant: presented {} != current {}",
                presented.get(),
                current.get()
            ),
            Self::BootGenerationMismatch { presented, current } => write!(
                formatter,
                "Cell boot generation mismatch for QuotaLease grant: presented {} != current {}",
                presented.get(),
                current.get()
            ),
            Self::IdentityMismatch { presented, current } => write!(
                formatter,
                "Cell identity mismatch for QuotaLease grant: presented {presented:?} != current {current:?}"
            ),
            Self::FencingTokenMismatch { presented, current } => write!(
                formatter,
                "Cell fencing token mismatch for QuotaLease grant: presented {} != current {}",
                presented.get(),
                current.get()
            ),
            Self::InsufficientAvailable {
                requested,
                available,
            } => write!(
                formatter,
                "insufficient AVAILABLE for QuotaLease grant: requested {requested} > available {available}"
            ),
            Self::ConflictingFace {
                existing,
                requested,
            } => write!(
                formatter,
                "conflicting QuotaLease face value: existing {existing} != requested {requested}"
            ),
            Self::ConflictingTtl {
                existing,
                requested,
            } => write!(
                formatter,
                "conflicting QuotaLease TTL deadline: existing {existing:?} != requested {requested:?}"
            ),
            Self::Storage(reason) => {
                write!(formatter, "QuotaLease durable store failure: {reason}")
            }
        }
    }
}

impl Error for QuotaLeaseGrantError {}

/// Typed fail-closed rejects for `QuotaLease` activate, usage, close, cancel,
/// and TTL settle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotaLeaseLedgerError {
    /// Presented fence was rejected by [`CellAuthority::admit`].
    Fence(QuotaLeaseGrantError),
    /// No lease with this id has been issued.
    UnknownLease,
    /// Activate was asked of a lease that is neither `ISSUED` nor `ACTIVE`.
    ActivateRequiresIssued {
        /// State of the committed lease.
        state: QuotaLeaseState,
    },
    /// Usage was asked of a lease that is not `ACTIVE` (and not an idempotent
    /// `CLOSING` replay).
    NotActive {
        /// State of the committed lease.
        state: QuotaLeaseState,
    },
    /// `CLOSING` refused a high-water above the committed mark.
    NewReserveForbidden,
    /// Reported usage is below the committed high-water.
    UsageRegression {
        /// Mark on this report.
        reported: u64,
        /// Committed high-water.
        current: u64,
    },
    /// Reported usage is above the issued face value.
    UsageExceedsFace {
        /// Mark on this report.
        reported: u64,
        /// Issued face value.
        face_value: u64,
    },
    /// Close ACK was asked of a lease that is neither `CLOSING` nor `CLOSED`.
    NotClosing {
        /// State of the committed lease.
        state: QuotaLeaseState,
    },
    /// Cancel was asked of a lease that has left `ISSUED`.
    CancelRequiresNeverActive {
        /// State of the committed lease.
        state: QuotaLeaseState,
    },
    /// Settle was asked of a lease that is neither `FENCED` nor `CLOSED`.
    NotFenced {
        /// State of the committed lease.
        state: QuotaLeaseState,
    },
    /// Durable `SQLite` store rejected or failed to persist the transition.
    Storage(&'static str),
}

impl From<CellAdmitError> for QuotaLeaseLedgerError {
    fn from(error: CellAdmitError) -> Self {
        Self::Fence(QuotaLeaseGrantError::from(error))
    }
}

impl From<LeaseStoreError> for QuotaLeaseLedgerError {
    fn from(error: LeaseStoreError) -> Self {
        Self::Storage(error.reason())
    }
}

impl fmt::Display for QuotaLeaseLedgerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fence(error) => write!(formatter, "QuotaLease fence rejected: {error}"),
            Self::UnknownLease => write!(formatter, "unknown QuotaLease"),
            Self::ActivateRequiresIssued { state } => {
                write!(
                    formatter,
                    "QuotaLease activate requires ISSUED, found {state:?}"
                )
            }
            Self::NotActive { state } => {
                write!(
                    formatter,
                    "QuotaLease usage requires ACTIVE, found {state:?}"
                )
            }
            Self::NewReserveForbidden => {
                write!(
                    formatter,
                    "QuotaLease CLOSING forbids a higher usage high-water"
                )
            }
            Self::UsageRegression { reported, current } => write!(
                formatter,
                "QuotaLease usage high-water regressed: reported {reported} < current {current}"
            ),
            Self::UsageExceedsFace {
                reported,
                face_value,
            } => write!(
                formatter,
                "QuotaLease usage exceeds face value: reported {reported} > face {face_value}"
            ),
            Self::NotClosing { state } => {
                write!(
                    formatter,
                    "QuotaLease close ACK requires CLOSING, found {state:?}"
                )
            }
            Self::CancelRequiresNeverActive { state } => {
                write!(
                    formatter,
                    "QuotaLease cancel requires a lease that was never ACTIVE, found {state:?}"
                )
            }
            Self::NotFenced { state } => {
                write!(
                    formatter,
                    "QuotaLease settle requires FENCED, found {state:?}"
                )
            }
            Self::Storage(reason) => {
                write!(formatter, "QuotaLease durable store failure: {reason}")
            }
        }
    }
}

impl Error for QuotaLeaseLedgerError {}

/// `CapacityLease` state for the single-Cell prefix.
///
/// Spec chain used here: `GLOBAL_RESERVED → TARGET_PREPARED → ACTIVE` (host
/// attach receipt) and `GLOBAL_RESERVED`/`TARGET_PREPARED`/`ACTIVE →
/// RETURNING → RETURNED`. Reclaim stays out of this slice.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CapacityLeaseState {
    /// Issued after `amount` deduct from the source pool
    /// (`LEASE-CAPACITY-001` `GLOBAL_RESERVED`).
    GlobalReserved,
    /// Pre-active prepare. Source pool is unchanged (`TARGET_PREPARED`).
    TargetPrepared,
    /// Host attach receipt accepted; the capacity is in host hands
    /// (`ACTIVE`). Source pool is unchanged.
    Active,
    /// Return started. Source pool is unchanged (`RETURNING`).
    Returning,
    /// Holder fenced during revoke/migration; requires `CapacityReconciliationReceipt`
    /// (`FenceBarrierAck` + `CleanupAttestation`) before returning (`FENCED`).
    Fenced,
    /// Holder lost or cleanup proof missing; capacity stays frozen (`QUARANTINED`).
    Quarantined,
    /// Source authority accepted the return or reconciliation. `amount` is back
    /// in the pool (`RETURNED`).
    Returned,
}

/// A single-Cell `CapacityLease` grant snapshot (v0.5 §12 fields used by this
/// slice: id, amount, target node/boot, capacity epoch, fencing token, state).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapacityLeaseGrant {
    pub(crate) capacity_lease_id: CapacityLeaseId,
    pub(crate) target_node: CellIdentity,
    pub(crate) target_node_boot_generation: Generation,
    pub(crate) amount: u64,
    pub(crate) capacity_epoch: CellEpoch,
    pub(crate) fencing_token: CellFencingToken,
    pub(crate) fence_scope: FenceScope,
    pub(crate) state: CapacityLeaseState,
}

impl CapacityLeaseGrant {
    /// Stable capacity lease identity.
    #[must_use]
    pub const fn capacity_lease_id(self) -> CapacityLeaseId {
        self.capacity_lease_id
    }

    /// Target Cell (`target_node` in §12 `CapacityLease`).
    #[must_use]
    pub const fn target_node(self) -> CellIdentity {
        self.target_node
    }

    /// Target node boot generation bound into the grant.
    #[must_use]
    pub const fn target_node_boot_generation(self) -> Generation {
        self.target_node_boot_generation
    }

    /// Capacity amount deducted at grant time.
    #[must_use]
    pub const fn amount(self) -> u64 {
        self.amount
    }

    /// Capacity epoch bound into the grant (Cell epoch for this single-Cell
    /// fence slice).
    #[must_use]
    pub const fn capacity_epoch(self) -> CellEpoch {
        self.capacity_epoch
    }

    /// Fencing token bound into the grant.
    #[must_use]
    pub const fn fencing_token(self) -> CellFencingToken {
        self.fencing_token
    }

    /// Fence scope for this grant.
    #[must_use]
    pub const fn fence_scope(self) -> FenceScope {
        self.fence_scope
    }

    /// Lease state after grant.
    #[must_use]
    pub const fn state(self) -> CapacityLeaseState {
        self.state
    }
}

/// Single-Cell `CapacityLease` grantor (in-memory or durable `SQLite`).
///
/// Holds the source capacity pool remaining, the committed lease records
/// with their host attach receipts, and the shared Cell authority used for
/// fence admit.
#[derive(Debug)]
pub struct CapacityLeaseGrantor {
    pub(crate) authority: Arc<CellAuthority>,
    pub(crate) pool_remaining: u64,
    pub(crate) leases: HashMap<CapacityLeaseId, CapacityLeaseGrant>,
    pub(crate) attach_receipts: HashMap<CapacityLeaseId, HostAttachReceipt>,
    pub(crate) reconciliation_receipts: HashMap<CapacityLeaseId, CapacityReconciliationReceipt>,
    pub(crate) store: Option<Arc<SqliteLeaseStore>>,
}

impl CapacityLeaseGrantor {
    /// Opens an in-memory grantor bound to the process's shared Cell authority
    /// ([`CellAuthority::into_shared`], ADR-0021) with an initial source
    /// capacity pool.
    #[must_use]
    pub fn open(authority: Arc<CellAuthority>, pool_remaining: u64) -> Self {
        Self {
            authority,
            pool_remaining,
            leases: HashMap::new(),
            attach_receipts: HashMap::new(),
            reconciliation_receipts: HashMap::new(),
            store: None,
        }
    }

    /// Alias for [`Self::open`].
    #[must_use]
    pub fn new(authority: Arc<CellAuthority>, pool_remaining: u64) -> Self {
        Self::open(authority, pool_remaining)
    }

    /// Opens a durable `CapacityLeaseGrantor` backed by `SQLite` (`WAL + FULL`)
    /// at `path`, seeding `pool_remaining` on first creation or rehydrating
    /// full pool/lease/attach-receipt state on reopen.
    ///
    /// # Errors
    ///
    /// Fails closed on storage failure, unsupported schema version, or
    /// corrupted/tampered pool/lease records.
    pub fn open_durable(
        path: impl AsRef<Path>,
        authority: Arc<CellAuthority>,
        pool_remaining: u64,
    ) -> Result<Self, LeaseStoreError> {
        let store = Arc::new(SqliteLeaseStore::open(path)?);
        Self::open_with_store(store, authority, pool_remaining)
    }

    /// Opens a durable `CapacityLeaseGrantor` over a shared [`SqliteLeaseStore`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::open_durable`].
    pub fn open_with_store(
        store: Arc<SqliteLeaseStore>,
        authority: Arc<CellAuthority>,
        pool_remaining: u64,
    ) -> Result<Self, LeaseStoreError> {
        let (rehydrated_pool, leases, attach_receipts, reconciliation_receipts) =
            store.open_or_rehydrate_capacity(&authority, pool_remaining)?;
        Ok(Self {
            authority,
            pool_remaining: rehydrated_pool,
            leases,
            attach_receipts,
            reconciliation_receipts,
            store: Some(store),
        })
    }

    /// Remaining source capacity pool for this Cell.
    #[must_use]
    pub const fn pool_remaining(&self) -> u64 {
        self.pool_remaining
    }

    /// Grants a `CapacityLease` against a presented Cell fence.
    ///
    /// `LEASE-CAPACITY-001` prefix: `amount` is deducted from the source pool
    /// before the lease enters [`CapacityLeaseState::GlobalReserved`].
    /// Fail-closed fence checks run via [`CellAuthority::admit`] before any
    /// deduct.
    ///
    /// # Errors
    ///
    /// Typed reject: [`CapacityLeaseGrantError`].
    pub fn grant(
        &mut self,
        presented: &CellFence,
        capacity_lease_id: CapacityLeaseId,
        amount: u64,
    ) -> Result<CapacityLeaseGrant, CapacityLeaseGrantError> {
        if let Some(existing) = self.leases.get(&capacity_lease_id).copied() {
            if existing.amount == amount {
                return Ok(existing);
            }
            self.authority.admit(presented)?;
            return Err(CapacityLeaseGrantError::ConflictingAmount {
                existing: existing.amount,
                requested: amount,
            });
        }
        self.authority.admit(presented)?;
        if amount > self.pool_remaining {
            return Err(CapacityLeaseGrantError::InsufficientCapacity {
                requested: amount,
                available: self.pool_remaining,
            });
        }
        let next_pool = self.pool_remaining - amount;
        let target_node = self.authority.identity();
        let grant = CapacityLeaseGrant {
            capacity_lease_id,
            target_node,
            target_node_boot_generation: self.authority.node_boot_generation(),
            amount,
            capacity_epoch: self.authority.epoch(),
            fencing_token: self.authority.fencing_token(),
            fence_scope: FenceScope::cell(target_node),
            state: CapacityLeaseState::GlobalReserved,
        };
        if let Some(store) = &self.store {
            store.persist_capacity_grant(&grant, self.pool_remaining, next_pool)?;
        }
        self.pool_remaining = next_pool;
        self.leases.insert(capacity_lease_id, grant);
        Ok(grant)
    }

    /// Reads the committed capacity lease snapshot.
    ///
    /// # Errors
    ///
    /// [`CapacityLeaseReturnError::UnknownLease`] when the id was never issued.
    pub fn query(
        &self,
        capacity_lease_id: CapacityLeaseId,
    ) -> Result<CapacityLeaseGrant, CapacityLeaseReturnError> {
        self.leases
            .get(&capacity_lease_id)
            .copied()
            .ok_or(CapacityLeaseReturnError::UnknownLease)
    }

    /// `GLOBAL_RESERVED → TARGET_PREPARED`. Does not refund the source pool.
    ///
    /// A second call on `TARGET_PREPARED` returns the committed lease.
    ///
    /// # Errors
    ///
    /// Typed reject: [`CapacityLeaseReturnError`].
    pub fn prepare(
        &mut self,
        presented: &CellFence,
        capacity_lease_id: CapacityLeaseId,
    ) -> Result<CapacityLeaseGrant, CapacityLeaseReturnError> {
        self.authority.admit(presented)?;
        let existing = self.query(capacity_lease_id)?;
        match existing.state {
            CapacityLeaseState::GlobalReserved => {
                let next = CapacityLeaseGrant {
                    state: CapacityLeaseState::TargetPrepared,
                    ..existing
                };
                if let Some(store) = &self.store {
                    store.persist_capacity_state_transition(
                        capacity_lease_id,
                        CapacityLeaseState::GlobalReserved,
                        CapacityLeaseState::TargetPrepared,
                        None,
                    )?;
                }
                self.leases.insert(capacity_lease_id, next);
                Ok(next)
            }
            CapacityLeaseState::TargetPrepared => Ok(existing),
            state => Err(CapacityLeaseReturnError::NotGlobalReserved { state }),
        }
    }

    /// Alias for [`Self::prepare`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::prepare`].
    pub fn prepare_target(
        &mut self,
        presented: &CellFence,
        capacity_lease_id: CapacityLeaseId,
    ) -> Result<CapacityLeaseGrant, CapacityLeaseReturnError> {
        self.prepare(presented, capacity_lease_id)
    }

    /// `TARGET_PREPARED → ACTIVE`: accepts the host attach receipt.
    ///
    /// The receipt id is the idempotency key: replaying the identical
    /// committed receipt returns the committed lease. Any other receipt for
    /// the same lease is a typed conflict. Every receipt axis must bind the
    /// committed lease (target node, boot generation, capacity epoch,
    /// fencing token, and the full `amount`). The accepted receipt is stored
    /// once per lease and never rewritten. The source pool is unchanged.
    ///
    /// # Errors
    ///
    /// Typed reject: [`CapacityLeaseReturnError`].
    pub fn activate(
        &mut self,
        presented: &CellFence,
        receipt: HostAttachReceipt,
    ) -> Result<CapacityLeaseGrant, CapacityLeaseReturnError> {
        self.authority.admit(presented)?;
        let lease = self
            .leases
            .get(&receipt.capacity_lease_id)
            .copied()
            .ok_or(CapacityLeaseReturnError::UnknownLease)?;
        match lease.state {
            CapacityLeaseState::TargetPrepared => {
                if !attach_receipt_binds_lease(&receipt, &lease) {
                    return Err(CapacityLeaseReturnError::AttachReceiptMismatch { receipt });
                }
                let committed = CapacityLeaseGrant {
                    state: CapacityLeaseState::Active,
                    ..lease
                };
                if let Some(store) = &self.store {
                    store.persist_capacity_activate(receipt.capacity_lease_id, &receipt)?;
                }
                self.leases.insert(receipt.capacity_lease_id, committed);
                self.attach_receipts
                    .insert(receipt.capacity_lease_id, receipt);
                Ok(committed)
            }
            CapacityLeaseState::Active => {
                let existing = self
                    .attach_receipts
                    .get(&receipt.capacity_lease_id)
                    .copied()
                    .ok_or(CapacityLeaseReturnError::UnknownLease)?;
                if existing == receipt {
                    return Ok(lease);
                }
                Err(CapacityLeaseReturnError::ConflictingAttachReceipt {
                    existing: existing.receipt_id,
                    requested: receipt.receipt_id,
                })
            }
            state => Err(CapacityLeaseReturnError::AttachRequiresTargetPrepared { state }),
        }
    }

    /// Alias for [`Self::activate`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::activate`].
    pub fn activate_host(
        &mut self,
        presented: &CellFence,
        receipt: HostAttachReceipt,
    ) -> Result<CapacityLeaseGrant, CapacityLeaseReturnError> {
        self.activate(presented, receipt)
    }

    /// Committed host attach receipt for `capacity_lease_id`, when the lease
    /// has reached `ACTIVE`.
    #[must_use]
    pub fn attach_receipt(&self, capacity_lease_id: CapacityLeaseId) -> Option<HostAttachReceipt> {
        self.attach_receipts.get(&capacity_lease_id).copied()
    }

    /// `GLOBAL_RESERVED`, `TARGET_PREPARED`, or `ACTIVE` → `RETURNING`. Does
    /// not refund the source pool.
    ///
    /// Symmetric return semantics: an `ACTIVE` lease returns through the same
    /// `RETURNING → RETURNED` edge as a pre-active one. Capacity is returned
    /// whole, not consumed — this family has no usage accounting, so
    /// `RETURNED` refunds the full `amount` whether the lease passed through
    /// `ACTIVE` or not.
    ///
    /// # Errors
    ///
    /// Typed reject: [`CapacityLeaseReturnError`].
    pub fn begin_return(
        &mut self,
        presented: &CellFence,
        capacity_lease_id: CapacityLeaseId,
    ) -> Result<CapacityLeaseGrant, CapacityLeaseReturnError> {
        self.authority.admit(presented)?;
        let existing = self.query(capacity_lease_id)?;
        match existing.state {
            prev @ (CapacityLeaseState::GlobalReserved
            | CapacityLeaseState::TargetPrepared
            | CapacityLeaseState::Active) => {
                let next = CapacityLeaseGrant {
                    state: CapacityLeaseState::Returning,
                    ..existing
                };
                if let Some(store) = &self.store {
                    store.persist_capacity_state_transition(
                        capacity_lease_id,
                        prev,
                        CapacityLeaseState::Returning,
                        None,
                    )?;
                }
                self.leases.insert(capacity_lease_id, next);
                Ok(next)
            }
            CapacityLeaseState::Returning => Ok(existing),
            state @ (CapacityLeaseState::Fenced
            | CapacityLeaseState::Quarantined
            | CapacityLeaseState::Returned) => Err(CapacityLeaseReturnError::NotReserved { state }),
        }
    }

    /// `RETURNING → RETURNED`. Refunds `amount` to the source pool once,
    /// whether the lease was returned pre-active or from `ACTIVE`.
    ///
    /// # Errors
    ///
    /// Typed reject: [`CapacityLeaseReturnError`].
    pub fn ack_return(
        &mut self,
        presented: &CellFence,
        capacity_lease_id: CapacityLeaseId,
    ) -> Result<CapacityLeaseGrant, CapacityLeaseReturnError> {
        self.authority.admit(presented)?;
        let existing = self.query(capacity_lease_id)?;
        match existing.state {
            CapacityLeaseState::Returning => {
                let next_pool = self.pool_remaining + existing.amount;
                let next = CapacityLeaseGrant {
                    state: CapacityLeaseState::Returned,
                    ..existing
                };
                if let Some(store) = &self.store {
                    store.persist_capacity_state_transition(
                        capacity_lease_id,
                        CapacityLeaseState::Returning,
                        CapacityLeaseState::Returned,
                        Some((self.pool_remaining, next_pool)),
                    )?;
                }
                self.leases.insert(capacity_lease_id, next);
                self.pool_remaining = next_pool;
                Ok(next)
            }
            CapacityLeaseState::Returned => Ok(existing),
            state @ (CapacityLeaseState::GlobalReserved
            | CapacityLeaseState::TargetPrepared
            | CapacityLeaseState::Active
            | CapacityLeaseState::Fenced
            | CapacityLeaseState::Quarantined) => {
                Err(CapacityLeaseReturnError::NotReturning { state })
            }
        }
    }

    /// Alias for [`Self::ack_return`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::ack_return`].
    pub fn finalize_return(
        &mut self,
        presented: &CellFence,
        capacity_lease_id: CapacityLeaseId,
    ) -> Result<CapacityLeaseGrant, CapacityLeaseReturnError> {
        self.ack_return(presented, capacity_lease_id)
    }
}

pub(crate) fn attach_receipt_binds_lease(
    receipt: &HostAttachReceipt,
    lease: &CapacityLeaseGrant,
) -> bool {
    receipt.capacity_lease_id == lease.capacity_lease_id
        && receipt.target_node == lease.target_node
        && receipt.target_node_boot_generation == lease.target_node_boot_generation
        && receipt.capacity_epoch == lease.capacity_epoch
        && receipt.fencing_token == lease.fencing_token
        && receipt.attached_amount == lease.amount
}

/// Immutable host attach receipt that moves a prepared capacity lease into
/// `ACTIVE` (`TARGET_PREPARED → ACTIVE`).
///
/// The target host builds this receipt once the reserved capacity is
/// attached and submits it to the grantor. `receipt_id` is the idempotency
/// key: replaying the identical committed receipt returns the committed
/// lease, while any other receipt for the same lease is a typed conflict.
/// `attached_amount` must repeat the full lease `amount` — a partial attach
/// is fail-closed. An accepted receipt is stored once per lease and never
/// rewritten.
///
/// Like [`CellFence::present`], the constructor takes every axis
/// explicitly: this is a host-side attestation the grantor validates, not a
/// value the grantor mints.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostAttachReceipt {
    pub(crate) receipt_id: ReceiptId,
    pub(crate) capacity_lease_id: CapacityLeaseId,
    pub(crate) target_node: CellIdentity,
    pub(crate) target_node_boot_generation: Generation,
    pub(crate) capacity_epoch: CellEpoch,
    pub(crate) fencing_token: CellFencingToken,
    pub(crate) attached_amount: u64,
}

impl HostAttachReceipt {
    /// Builds the receipt for `capacity_lease_id` under an idempotency key.
    ///
    /// Every axis must repeat the fence the lease was issued under; the
    /// grantor fail-closes on any drift.
    #[must_use]
    pub const fn new(
        receipt_id: ReceiptId,
        capacity_lease_id: CapacityLeaseId,
        target_node: CellIdentity,
        target_node_boot_generation: Generation,
        capacity_epoch: CellEpoch,
        fencing_token: CellFencingToken,
        attached_amount: u64,
    ) -> Self {
        Self {
            receipt_id,
            capacity_lease_id,
            target_node,
            target_node_boot_generation,
            capacity_epoch,
            fencing_token,
            attached_amount,
        }
    }

    /// Idempotency key of this receipt.
    #[must_use]
    pub const fn receipt_id(self) -> ReceiptId {
        self.receipt_id
    }

    /// Capacity lease this receipt attaches.
    #[must_use]
    pub const fn capacity_lease_id(self) -> CapacityLeaseId {
        self.capacity_lease_id
    }

    /// Target host Cell on the receipt.
    #[must_use]
    pub const fn target_node(self) -> CellIdentity {
        self.target_node
    }

    /// Target node boot generation on the receipt.
    #[must_use]
    pub const fn target_node_boot_generation(self) -> Generation {
        self.target_node_boot_generation
    }

    /// Capacity epoch on the receipt.
    #[must_use]
    pub const fn capacity_epoch(self) -> CellEpoch {
        self.capacity_epoch
    }

    /// Fencing token on the receipt.
    #[must_use]
    pub const fn fencing_token(self) -> CellFencingToken {
        self.fencing_token
    }

    /// Attached capacity amount on the receipt.
    #[must_use]
    pub const fn attached_amount(self) -> u64 {
        self.attached_amount
    }
}

/// Typed fail-closed rejects for a `CapacityLease` grant attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapacityLeaseGrantError {
    /// Presented epoch is older than the grantor's current epoch.
    StaleEpoch {
        /// Epoch on the presentation.
        presented: CellEpoch,
        /// Epoch currently held by the grantor.
        current: CellEpoch,
    },
    /// Presented epoch is not current and is not older (fail closed).
    EpochMismatch {
        /// Epoch on the presentation.
        presented: CellEpoch,
        /// Epoch currently held by the grantor.
        current: CellEpoch,
    },
    /// Presented boot generation does not match the grantor.
    BootGenerationMismatch {
        /// Boot generation on the presentation.
        presented: Generation,
        /// Boot generation currently held by the grantor.
        current: Generation,
    },
    /// Presented identity is not this Cell.
    IdentityMismatch {
        /// Identity on the presentation.
        presented: CellIdentity,
        /// Identity of this grantor.
        current: CellIdentity,
    },
    /// Epoch matches but the fencing token does not.
    FencingTokenMismatch {
        /// Token on the presentation.
        presented: CellFencingToken,
        /// Token currently held by the grantor.
        current: CellFencingToken,
    },
    /// Source pool remaining is strictly less than the requested `amount`.
    InsufficientCapacity {
        /// Requested `amount`.
        requested: u64,
        /// Remaining source pool before the attempt.
        available: u64,
    },
    /// The same capacity lease id was already issued with a different amount.
    ConflictingAmount {
        /// Amount on the committed lease.
        existing: u64,
        /// Amount on this grant attempt.
        requested: u64,
    },
    /// Durable `SQLite` store rejected or failed to persist the grant.
    Storage(&'static str),
}

impl From<CellAdmitError> for CapacityLeaseGrantError {
    fn from(error: CellAdmitError) -> Self {
        match error {
            CellAdmitError::StaleEpoch { presented, current } => {
                Self::StaleEpoch { presented, current }
            }
            CellAdmitError::EpochMismatch { presented, current } => {
                Self::EpochMismatch { presented, current }
            }
            CellAdmitError::BootGenerationMismatch { presented, current } => {
                Self::BootGenerationMismatch { presented, current }
            }
            CellAdmitError::IdentityMismatch { presented, current } => {
                Self::IdentityMismatch { presented, current }
            }
            CellAdmitError::FencingTokenMismatch { presented, current } => {
                Self::FencingTokenMismatch { presented, current }
            }
        }
    }
}

impl From<LeaseStoreError> for CapacityLeaseGrantError {
    fn from(error: LeaseStoreError) -> Self {
        Self::Storage(error.reason())
    }
}

impl fmt::Display for CapacityLeaseGrantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleEpoch { presented, current } => write!(
                formatter,
                "stale Cell epoch for CapacityLease grant: presented {} < current {}",
                presented.get(),
                current.get()
            ),
            Self::EpochMismatch { presented, current } => write!(
                formatter,
                "Cell epoch mismatch for CapacityLease grant: presented {} != current {}",
                presented.get(),
                current.get()
            ),
            Self::BootGenerationMismatch { presented, current } => write!(
                formatter,
                "Cell boot generation mismatch for CapacityLease grant: presented {} != current {}",
                presented.get(),
                current.get()
            ),
            Self::IdentityMismatch { presented, current } => write!(
                formatter,
                "Cell identity mismatch for CapacityLease grant: presented {presented:?} != current {current:?}"
            ),
            Self::FencingTokenMismatch { presented, current } => write!(
                formatter,
                "Cell fencing token mismatch for CapacityLease grant: presented {} != current {}",
                presented.get(),
                current.get()
            ),
            Self::InsufficientCapacity {
                requested,
                available,
            } => write!(
                formatter,
                "insufficient CapacityLease source pool: requested {requested} > available {available}"
            ),
            Self::ConflictingAmount {
                existing,
                requested,
            } => write!(
                formatter,
                "conflicting CapacityLease amount: existing {existing} != requested {requested}"
            ),
            Self::Storage(reason) => {
                write!(formatter, "CapacityLease durable store failure: {reason}")
            }
        }
    }
}

impl Error for CapacityLeaseGrantError {}

/// Typed fail-closed rejects for `CapacityLease` prepare, attach, and return.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapacityLeaseReturnError {
    /// Presented fence was rejected by [`CellAuthority::admit`].
    Fence(CapacityLeaseGrantError),
    /// No capacity lease with this id has been issued.
    UnknownLease,
    /// Return was asked of a lease that is neither `GLOBAL_RESERVED`,
    /// `TARGET_PREPARED`, `ACTIVE`, nor `RETURNING`.
    NotReserved {
        /// State of the committed lease.
        state: CapacityLeaseState,
    },
    /// Prepare was asked of a lease that is neither `GLOBAL_RESERVED` nor
    /// `TARGET_PREPARED`.
    NotGlobalReserved {
        /// State of the committed lease.
        state: CapacityLeaseState,
    },
    /// Return ACK was asked of a lease that is neither `RETURNING` nor
    /// `RETURNED`.
    NotReturning {
        /// State of the committed lease.
        state: CapacityLeaseState,
    },
    /// Attach receipt was submitted for a lease that is neither
    /// `TARGET_PREPARED` nor `ACTIVE`.
    AttachRequiresTargetPrepared {
        /// State of the committed lease.
        state: CapacityLeaseState,
    },
    /// Attach receipt does not bind the committed lease on the target node,
    /// boot, epoch, token, or full-`amount` axis. Compare the receipt against
    /// [`CapacityLeaseGrantor::query`] to find the drifting axis.
    AttachReceiptMismatch {
        /// Receipt as submitted.
        receipt: HostAttachReceipt,
    },
    /// A different attach receipt is already committed for this lease.
    ConflictingAttachReceipt {
        /// Idempotency key already committed.
        existing: ReceiptId,
        /// Idempotency key on this attempt.
        requested: ReceiptId,
    },
    /// Durable `SQLite` store rejected or failed to persist the transition.
    Storage(&'static str),
}

impl From<CellAdmitError> for CapacityLeaseReturnError {
    fn from(error: CellAdmitError) -> Self {
        Self::Fence(CapacityLeaseGrantError::from(error))
    }
}

impl From<LeaseStoreError> for CapacityLeaseReturnError {
    fn from(error: LeaseStoreError) -> Self {
        Self::Storage(error.reason())
    }
}

impl fmt::Display for CapacityLeaseReturnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fence(error) => write!(formatter, "CapacityLease fence rejected: {error}"),
            Self::UnknownLease => write!(formatter, "unknown CapacityLease"),
            Self::NotReserved { state } => write!(
                formatter,
                "CapacityLease return requires GLOBAL_RESERVED, TARGET_PREPARED, or ACTIVE, found {state:?}"
            ),
            Self::NotGlobalReserved { state } => write!(
                formatter,
                "CapacityLease prepare requires GLOBAL_RESERVED, found {state:?}"
            ),
            Self::NotReturning { state } => write!(
                formatter,
                "CapacityLease return ACK requires RETURNING, found {state:?}"
            ),
            Self::AttachRequiresTargetPrepared { state } => write!(
                formatter,
                "CapacityLease attach requires TARGET_PREPARED, found {state:?}"
            ),
            Self::AttachReceiptMismatch { receipt } => write!(
                formatter,
                "CapacityLease attach receipt does not bind the committed lease: {receipt:?}"
            ),
            Self::ConflictingAttachReceipt {
                existing,
                requested,
            } => write!(
                formatter,
                "CapacityLease attach receipt conflict: committed {existing:?} != requested {requested:?}"
            ),
            Self::Storage(reason) => {
                write!(formatter, "CapacityLease durable store failure: {reason}")
            }
        }
    }
}

impl Error for CapacityLeaseReturnError {}

/// `ExclusiveDeviceLease` state across the reset-gated return.
///
/// Spec chain used here: `DEVICE_RESERVED → RESETTING → RETURNED`. The return
/// leg is unlocked only by an accepted reset+zeroization receipt. The full
/// `HOLDER_PREPARED` / `ACTIVE` chain stays deferred.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ExclusiveDeviceLeaseState {
    /// Issued after FREE→reserved claim on the `DeviceLeaseHead`
    /// (`LEASE-DEVICE-001` `DEVICE_RESERVED`).
    DeviceReserved,
    /// Reset+zeroization declared; the receipt is not accepted yet. The head
    /// is not FREE (`RESETTING`).
    Resetting,
    /// Holder is being fenced; requires `DeviceReconciliationReceipt` to clear
    /// the head (`FENCING`).
    Fencing,
    /// Holder lost or zeroization unverified; head stays occupied (`QUARANTINED`).
    Quarantined,
    /// Reset+zeroization receipt accepted; the head is back to FREE
    /// (`RETURNED`).
    Returned,
}

/// A single-Cell `ExclusiveDeviceLease` grant snapshot (v0.5 §12 fields used by
/// this slice: id, device, exclusivity epoch, holder node/boot, fencing token,
/// state; plus the slice-local `reset_generation` head fence).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExclusiveDeviceLeaseGrant {
    pub(crate) device_lease_id: ExclusiveDeviceLeaseId,
    pub(crate) device_id: DeviceId,
    pub(crate) holder_node: CellIdentity,
    pub(crate) holder_node_boot_generation: Generation,
    pub(crate) exclusivity_epoch: CellEpoch,
    pub(crate) fencing_token: CellFencingToken,
    pub(crate) fence_scope: FenceScope,
    pub(crate) reset_generation: Generation,
    pub(crate) state: ExclusiveDeviceLeaseState,
}

impl ExclusiveDeviceLeaseGrant {
    /// Stable exclusive device lease identity.
    #[must_use]
    pub const fn device_lease_id(self) -> ExclusiveDeviceLeaseId {
        self.device_lease_id
    }

    /// Device identity bound into the grant.
    #[must_use]
    pub const fn device_id(self) -> DeviceId {
        self.device_id
    }

    /// Holder Cell (`holder_node` in §12 `ExclusiveDeviceLease`).
    #[must_use]
    pub const fn holder_node(self) -> CellIdentity {
        self.holder_node
    }

    /// Holder node boot generation bound into the grant.
    #[must_use]
    pub const fn holder_node_boot_generation(self) -> Generation {
        self.holder_node_boot_generation
    }

    /// Exclusivity epoch bound into the grant (Cell epoch for this single-Cell
    /// fence slice).
    #[must_use]
    pub const fn exclusivity_epoch(self) -> CellEpoch {
        self.exclusivity_epoch
    }

    /// Fencing token bound into the grant.
    #[must_use]
    pub const fn fencing_token(self) -> CellFencingToken {
        self.fencing_token
    }

    /// Fence scope for this grant.
    #[must_use]
    pub const fn fence_scope(self) -> FenceScope {
        self.fence_scope
    }

    /// `DeviceLeaseHead` reset generation bound at claim time. The
    /// reset+zeroization receipt must repeat it; a receipt from another head
    /// era is fenced out.
    #[must_use]
    pub const fn reset_generation(self) -> Generation {
        self.reset_generation
    }

    /// Lease state after grant.
    #[must_use]
    pub const fn state(self) -> ExclusiveDeviceLeaseState {
        self.state
    }
}

/// Single-Cell `ExclusiveDeviceLease` grantor (in-memory or durable `SQLite`).
///
/// Holds one `DeviceLeaseHead` (FREE or reserved) with its monotonic reset
/// generation, the committed lease history, and the immutable reset receipt
/// log, plus the shared Cell authority used for fence admit.
#[derive(Debug)]
pub struct ExclusiveDeviceLeaseGrantor {
    pub(crate) authority: Arc<CellAuthority>,
    pub(crate) device_id: DeviceId,
    pub(crate) free: bool,
    pub(crate) reset_generation: Generation,
    pub(crate) leases: HashMap<ExclusiveDeviceLeaseId, ExclusiveDeviceLeaseGrant>,
    pub(crate) reset_receipts: Vec<DeviceResetReceipt>,
    pub(crate) reconciliation_receipts:
        HashMap<ExclusiveDeviceLeaseId, DeviceReconciliationReceipt>,
    pub(crate) store: Option<Arc<SqliteLeaseStore>>,
}

impl ExclusiveDeviceLeaseGrantor {
    /// Opens an in-memory grantor bound to the process's shared Cell authority
    /// ([`CellAuthority::into_shared`], ADR-0021) with a FREE device head
    /// for `device_id` at reset generation [`Generation::INITIAL`].
    #[must_use]
    pub fn open(authority: Arc<CellAuthority>, device_id: DeviceId) -> Self {
        Self {
            authority,
            device_id,
            free: true,
            reset_generation: Generation::INITIAL,
            leases: HashMap::new(),
            reset_receipts: Vec::new(),
            reconciliation_receipts: HashMap::new(),
            store: None,
        }
    }

    /// Alias for [`Self::open`].
    #[must_use]
    pub fn new(authority: Arc<CellAuthority>, device_id: DeviceId) -> Self {
        Self::open(authority, device_id)
    }

    /// Opens a durable `ExclusiveDeviceLeaseGrantor` backed by `SQLite`
    /// (`WAL + FULL`) at `path`, seeding a `FREE` head for `device_id` on
    /// first creation or rehydrating full head/lease/reset-receipt state on
    /// reopen.
    ///
    /// # Errors
    ///
    /// Fails closed on storage failure, unsupported schema version, or
    /// corrupted/tampered head/lease/receipt records.
    pub fn open_durable(
        path: impl AsRef<Path>,
        authority: Arc<CellAuthority>,
        device_id: DeviceId,
    ) -> Result<Self, LeaseStoreError> {
        let store = Arc::new(SqliteLeaseStore::open(path)?);
        Self::open_with_store(store, authority, device_id)
    }

    /// Opens a durable `ExclusiveDeviceLeaseGrantor` over a shared
    /// [`SqliteLeaseStore`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::open_durable`].
    pub fn open_with_store(
        store: Arc<SqliteLeaseStore>,
        authority: Arc<CellAuthority>,
        device_id: DeviceId,
    ) -> Result<Self, LeaseStoreError> {
        let (free, reset_generation, leases, reset_receipts, reconciliation_receipts) =
            store.open_or_rehydrate_device(&authority, device_id)?;
        Ok(Self {
            authority,
            device_id,
            free,
            reset_generation,
            leases,
            reset_receipts,
            reconciliation_receipts,
            store: Some(store),
        })
    }

    /// Device identity this grantor manages.
    #[must_use]
    pub const fn device_id(&self) -> DeviceId {
        self.device_id
    }

    /// Whether the `DeviceLeaseHead` is still FREE.
    #[must_use]
    pub const fn is_free(&self) -> bool {
        self.free
    }

    /// Reset generation currently held by the `DeviceLeaseHead`.
    ///
    /// Bumps once per accepted reset receipt; receipts from an older head era
    /// are fenced out against it.
    #[must_use]
    pub const fn reset_generation(&self) -> Generation {
        self.reset_generation
    }

    /// Immutable reset receipt log in acceptance order. Receipts are appended
    /// on commit and never rewritten.
    #[must_use]
    pub fn reset_receipts(&self) -> &[DeviceResetReceipt] {
        &self.reset_receipts
    }

    /// Grants an `ExclusiveDeviceLease` against a presented Cell fence.
    ///
    /// `LEASE-DEVICE-001` prefix: the FREE head is claimed before the lease
    /// enters [`ExclusiveDeviceLeaseState::DeviceReserved`]. Fail-closed fence
    /// checks run via [`CellAuthority::admit`] before any claim. The head
    /// reset generation at claim time is bound into the grant. A
    /// `device_lease_id` already on record replays the committed lease and
    /// does not claim again.
    ///
    /// # Errors
    ///
    /// Typed reject: [`ExclusiveDeviceLeaseGrantError`].
    pub fn grant(
        &mut self,
        presented: &CellFence,
        device_lease_id: ExclusiveDeviceLeaseId,
    ) -> Result<ExclusiveDeviceLeaseGrant, ExclusiveDeviceLeaseGrantError> {
        if let Some(existing) = self.leases.get(&device_lease_id).copied() {
            return Ok(existing);
        }
        self.authority.admit(presented)?;
        if !self.free {
            return Err(ExclusiveDeviceLeaseGrantError::DeviceNotFree {
                device_id: self.device_id,
            });
        }
        let holder_node = self.authority.identity();
        let grant = ExclusiveDeviceLeaseGrant {
            device_lease_id,
            device_id: self.device_id,
            holder_node,
            holder_node_boot_generation: self.authority.node_boot_generation(),
            exclusivity_epoch: self.authority.epoch(),
            fencing_token: self.authority.fencing_token(),
            fence_scope: FenceScope::cell(holder_node),
            reset_generation: self.reset_generation,
            state: ExclusiveDeviceLeaseState::DeviceReserved,
        };
        if let Some(store) = &self.store {
            store.persist_device_grant(&grant)?;
        }
        self.free = false;
        self.leases.insert(device_lease_id, grant);
        Ok(grant)
    }

    /// Reads the committed device lease snapshot.
    ///
    /// # Errors
    ///
    /// [`ExclusiveDeviceLeaseReturnError::UnknownLease`] when `device_lease_id`
    /// was never issued.
    pub fn query(
        &self,
        device_lease_id: ExclusiveDeviceLeaseId,
    ) -> Result<ExclusiveDeviceLeaseGrant, ExclusiveDeviceLeaseReturnError> {
        self.leases
            .get(&device_lease_id)
            .copied()
            .ok_or(ExclusiveDeviceLeaseReturnError::UnknownLease)
    }

    /// `DEVICE_RESERVED → RESETTING`: the holder declares reset+zeroization
    /// intent. The head is not FREE yet.
    ///
    /// A second declare on `RESETTING` returns the committed lease.
    ///
    /// # Errors
    ///
    /// Typed reject: [`ExclusiveDeviceLeaseReturnError`].
    pub fn declare_reset(
        &mut self,
        presented: &CellFence,
        device_lease_id: ExclusiveDeviceLeaseId,
    ) -> Result<ExclusiveDeviceLeaseGrant, ExclusiveDeviceLeaseReturnError> {
        self.authority.admit(presented)?;
        let existing = self.query(device_lease_id)?;
        match existing.state {
            ExclusiveDeviceLeaseState::DeviceReserved => {
                let next = ExclusiveDeviceLeaseGrant {
                    state: ExclusiveDeviceLeaseState::Resetting,
                    ..existing
                };
                if let Some(store) = &self.store {
                    store.persist_device_declare_reset(device_lease_id)?;
                }
                self.leases.insert(device_lease_id, next);
                Ok(next)
            }
            ExclusiveDeviceLeaseState::Resetting => Ok(existing),
            state @ (ExclusiveDeviceLeaseState::Fencing
            | ExclusiveDeviceLeaseState::Quarantined
            | ExclusiveDeviceLeaseState::Returned) => {
                Err(ExclusiveDeviceLeaseReturnError::DeclareRequiresReserved { state })
            }
        }
    }

    /// Alias for [`Self::declare_reset`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::declare_reset`].
    pub fn begin_device_reset(
        &mut self,
        presented: &CellFence,
        device_lease_id: ExclusiveDeviceLeaseId,
    ) -> Result<ExclusiveDeviceLeaseGrant, ExclusiveDeviceLeaseReturnError> {
        self.declare_reset(presented, device_lease_id)
    }

    /// Accepts the reset+zeroization receipt and unblocks the return
    /// (`RESETTING → RETURNED`).
    ///
    /// The receipt id is the idempotency key: replaying the identical
    /// committed receipt returns the committed lease without touching the
    /// head again. Any other receipt for the same lease or key is a typed
    /// conflict. The receipt `reset_generation` must match the head fence
    /// (older-era receipts are fenced out) and every other axis must bind the
    /// committed lease. On accept, the receipt is appended to the immutable
    /// log, the head moves back to FREE, and the head reset generation bumps.
    ///
    /// # Errors
    ///
    /// Typed reject: [`ExclusiveDeviceLeaseReturnError`].
    pub fn submit_reset_receipt(
        &mut self,
        presented: &CellFence,
        receipt: DeviceResetReceipt,
    ) -> Result<ExclusiveDeviceLeaseGrant, ExclusiveDeviceLeaseReturnError> {
        self.authority.admit(presented)?;
        if let Some(committed) = self
            .reset_receipts
            .iter()
            .find(|logged| logged.receipt_id == receipt.receipt_id)
            .copied()
        {
            if committed == receipt {
                return self.query(committed.device_lease_id);
            }
            return Err(ExclusiveDeviceLeaseReturnError::ConflictingResetReceipt {
                existing: committed.receipt_id,
                requested: receipt.receipt_id,
            });
        }
        let lease = self
            .leases
            .get(&receipt.device_lease_id)
            .copied()
            .ok_or(ExclusiveDeviceLeaseReturnError::UnknownLease)?;
        match lease.state {
            ExclusiveDeviceLeaseState::Resetting => {
                if receipt.reset_generation != self.reset_generation {
                    return Err(ExclusiveDeviceLeaseReturnError::StaleResetGeneration {
                        receipt: receipt.reset_generation,
                        head: self.reset_generation,
                    });
                }
                if !receipt_binds_lease(&receipt, &lease) {
                    return Err(ExclusiveDeviceLeaseReturnError::ReceiptBindMismatch { receipt });
                }
                let next = self
                    .reset_generation
                    .checked_next()
                    .ok_or(ExclusiveDeviceLeaseReturnError::ResetGenerationExhausted)?;
                let returned = ExclusiveDeviceLeaseGrant {
                    state: ExclusiveDeviceLeaseState::Returned,
                    ..lease
                };
                if let Some(store) = &self.store {
                    store.persist_device_reset_receipt(
                        self.device_id,
                        &receipt,
                        self.reset_generation,
                        next,
                    )?;
                }
                self.leases.insert(receipt.device_lease_id, returned);
                self.reset_receipts.push(receipt);
                self.reset_generation = next;
                self.free = true;
                Ok(returned)
            }
            ExclusiveDeviceLeaseState::Returned => {
                let existing = self
                    .reset_receipts
                    .iter()
                    .find(|logged| logged.device_lease_id == receipt.device_lease_id)
                    .map(|logged| logged.receipt_id)
                    .ok_or(ExclusiveDeviceLeaseReturnError::UnknownLease)?;
                Err(ExclusiveDeviceLeaseReturnError::ConflictingResetReceipt {
                    existing,
                    requested: receipt.receipt_id,
                })
            }
            state @ (ExclusiveDeviceLeaseState::DeviceReserved
            | ExclusiveDeviceLeaseState::Fencing
            | ExclusiveDeviceLeaseState::Quarantined) => {
                Err(ExclusiveDeviceLeaseReturnError::ReceiptRequiresResetting { state })
            }
        }
    }

    /// Alias for [`Self::submit_reset_receipt`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::submit_reset_receipt`].
    pub fn finalize_device_reset(
        &mut self,
        presented: &CellFence,
        receipt: DeviceResetReceipt,
    ) -> Result<ExclusiveDeviceLeaseGrant, ExclusiveDeviceLeaseReturnError> {
        self.submit_reset_receipt(presented, receipt)
    }
}

pub(crate) fn receipt_binds_lease(
    receipt: &DeviceResetReceipt,
    lease: &ExclusiveDeviceLeaseGrant,
) -> bool {
    receipt.device_lease_id == lease.device_lease_id
        && receipt.holder_node == lease.holder_node
        && receipt.holder_node_boot_generation == lease.holder_node_boot_generation
        && receipt.exclusivity_epoch == lease.exclusivity_epoch
        && receipt.fencing_token == lease.fencing_token
        && receipt.reset_generation == lease.reset_generation
}

/// Immutable reset+zeroization receipt that unblocks one
/// `ExclusiveDeviceLease` return (`RESETTING → RETURNED`).
///
/// The holder builds this receipt after the device reset+zeroization
/// completes and submits it to the grantor. `receipt_id` is the idempotency
/// key: replaying the identical committed receipt returns the committed
/// lease, while any other receipt for the same lease or key is a typed
/// conflict. `reset_generation` is the generation fence: it must repeat the
/// `DeviceLeaseHead` generation the lease was claimed under, so a receipt
/// from an older head era is rejected fail-closed. Accepted receipts are
/// appended to the grantor's immutable log and never rewritten.
///
/// Like [`CellFence::present`], the constructor takes every axis explicitly:
/// this is a holder-side attestation the grantor validates, not a value the
/// grantor mints.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceResetReceipt {
    pub(crate) receipt_id: ReceiptId,
    pub(crate) device_lease_id: ExclusiveDeviceLeaseId,
    pub(crate) holder_node: CellIdentity,
    pub(crate) holder_node_boot_generation: Generation,
    pub(crate) exclusivity_epoch: CellEpoch,
    pub(crate) fencing_token: CellFencingToken,
    pub(crate) reset_generation: Generation,
}

impl DeviceResetReceipt {
    /// Builds the receipt for `device_lease_id` under an idempotency key.
    ///
    /// Every axis must repeat the fence the lease was issued under; the
    /// grantor fail-closes on any drift.
    #[must_use]
    pub const fn new(
        receipt_id: ReceiptId,
        device_lease_id: ExclusiveDeviceLeaseId,
        holder_node: CellIdentity,
        holder_node_boot_generation: Generation,
        exclusivity_epoch: CellEpoch,
        fencing_token: CellFencingToken,
        reset_generation: Generation,
    ) -> Self {
        Self {
            receipt_id,
            device_lease_id,
            holder_node,
            holder_node_boot_generation,
            exclusivity_epoch,
            fencing_token,
            reset_generation,
        }
    }

    /// Idempotency key of this receipt.
    #[must_use]
    pub const fn receipt_id(self) -> ReceiptId {
        self.receipt_id
    }

    /// Device lease this receipt returns.
    #[must_use]
    pub const fn device_lease_id(self) -> ExclusiveDeviceLeaseId {
        self.device_lease_id
    }

    /// Holder Cell on the receipt.
    #[must_use]
    pub const fn holder_node(self) -> CellIdentity {
        self.holder_node
    }

    /// Holder node boot generation on the receipt.
    #[must_use]
    pub const fn holder_node_boot_generation(self) -> Generation {
        self.holder_node_boot_generation
    }

    /// Exclusivity epoch on the receipt.
    #[must_use]
    pub const fn exclusivity_epoch(self) -> CellEpoch {
        self.exclusivity_epoch
    }

    /// Fencing token on the receipt.
    #[must_use]
    pub const fn fencing_token(self) -> CellFencingToken {
        self.fencing_token
    }

    /// Head reset generation this receipt is fenced by.
    #[must_use]
    pub const fn reset_generation(self) -> Generation {
        self.reset_generation
    }
}

/// Typed fail-closed rejects for an `ExclusiveDeviceLease` grant attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExclusiveDeviceLeaseGrantError {
    /// Presented epoch is older than the grantor's current epoch.
    StaleEpoch {
        /// Epoch on the presentation.
        presented: CellEpoch,
        /// Epoch currently held by the grantor.
        current: CellEpoch,
    },
    /// Presented epoch is not current and is not older (fail closed).
    EpochMismatch {
        /// Epoch on the presentation.
        presented: CellEpoch,
        /// Epoch currently held by the grantor.
        current: CellEpoch,
    },
    /// Presented boot generation does not match the grantor.
    BootGenerationMismatch {
        /// Boot generation on the presentation.
        presented: Generation,
        /// Boot generation currently held by the grantor.
        current: Generation,
    },
    /// Presented identity is not this Cell.
    IdentityMismatch {
        /// Identity on the presentation.
        presented: CellIdentity,
        /// Identity of this grantor.
        current: CellIdentity,
    },
    /// Epoch matches but the fencing token does not.
    FencingTokenMismatch {
        /// Token on the presentation.
        presented: CellFencingToken,
        /// Token currently held by the grantor.
        current: CellFencingToken,
    },
    /// `DeviceLeaseHead` is not FREE; cannot install a second exclusive holder.
    DeviceNotFree {
        /// Device whose head is already reserved.
        device_id: DeviceId,
    },
    /// Durable `SQLite` store rejected or failed to persist the grant.
    Storage(&'static str),
}

impl From<CellAdmitError> for ExclusiveDeviceLeaseGrantError {
    fn from(error: CellAdmitError) -> Self {
        match error {
            CellAdmitError::StaleEpoch { presented, current } => {
                Self::StaleEpoch { presented, current }
            }
            CellAdmitError::EpochMismatch { presented, current } => {
                Self::EpochMismatch { presented, current }
            }
            CellAdmitError::BootGenerationMismatch { presented, current } => {
                Self::BootGenerationMismatch { presented, current }
            }
            CellAdmitError::IdentityMismatch { presented, current } => {
                Self::IdentityMismatch { presented, current }
            }
            CellAdmitError::FencingTokenMismatch { presented, current } => {
                Self::FencingTokenMismatch { presented, current }
            }
        }
    }
}

impl From<LeaseStoreError> for ExclusiveDeviceLeaseGrantError {
    fn from(error: LeaseStoreError) -> Self {
        Self::Storage(error.reason())
    }
}

impl fmt::Display for ExclusiveDeviceLeaseGrantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleEpoch { presented, current } => write!(
                formatter,
                "stale Cell epoch for ExclusiveDeviceLease grant: presented {} < current {}",
                presented.get(),
                current.get()
            ),
            Self::EpochMismatch { presented, current } => write!(
                formatter,
                "Cell epoch mismatch for ExclusiveDeviceLease grant: presented {} != current {}",
                presented.get(),
                current.get()
            ),
            Self::BootGenerationMismatch { presented, current } => write!(
                formatter,
                "Cell boot generation mismatch for ExclusiveDeviceLease grant: presented {} != current {}",
                presented.get(),
                current.get()
            ),
            Self::IdentityMismatch { presented, current } => write!(
                formatter,
                "Cell identity mismatch for ExclusiveDeviceLease grant: presented {presented:?} != current {current:?}"
            ),
            Self::FencingTokenMismatch { presented, current } => write!(
                formatter,
                "Cell fencing token mismatch for ExclusiveDeviceLease grant: presented {} != current {}",
                presented.get(),
                current.get()
            ),
            Self::DeviceNotFree { device_id } => write!(
                formatter,
                "ExclusiveDeviceLease DeviceLeaseHead not FREE for device {device_id:?}"
            ),
            Self::Storage(reason) => {
                write!(
                    formatter,
                    "ExclusiveDeviceLease durable store failure: {reason}"
                )
            }
        }
    }
}

impl Error for ExclusiveDeviceLeaseGrantError {}

/// Typed fail-closed rejects for the reset-gated `ExclusiveDeviceLease`
/// return.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExclusiveDeviceLeaseReturnError {
    /// Presented fence was rejected by [`CellAuthority::admit`].
    Fence(ExclusiveDeviceLeaseGrantError),
    /// No device lease with this id has been issued.
    UnknownLease,
    /// Reset declare was asked of a lease that already returned.
    DeclareRequiresReserved {
        /// State of the committed lease.
        state: ExclusiveDeviceLeaseState,
    },
    /// Receipt was submitted before reset was declared.
    ReceiptRequiresResetting {
        /// State of the committed lease.
        state: ExclusiveDeviceLeaseState,
    },
    /// Receipt `reset_generation` does not match the `DeviceLeaseHead` fence:
    /// a receipt from an older head era (or any other drift) fails closed.
    StaleResetGeneration {
        /// Generation bound into the receipt.
        receipt: Generation,
        /// Generation currently held by the head.
        head: Generation,
    },
    /// Receipt does not bind the committed lease on the holder, boot, epoch,
    /// or token axis. Compare the receipt against
    /// [`ExclusiveDeviceLeaseGrantor::query`] to find the drifting axis.
    ReceiptBindMismatch {
        /// Receipt as submitted.
        receipt: DeviceResetReceipt,
    },
    /// A different receipt is already committed for this lease or idempotency
    /// key.
    ConflictingResetReceipt {
        /// Idempotency key already committed.
        existing: ReceiptId,
        /// Idempotency key on this attempt.
        requested: ReceiptId,
    },
    /// Reset generation space exhausted; the head cannot fence another
    /// return.
    ResetGenerationExhausted,
    /// Durable `SQLite` store rejected or failed to persist the transition.
    Storage(&'static str),
}

impl From<CellAdmitError> for ExclusiveDeviceLeaseReturnError {
    fn from(error: CellAdmitError) -> Self {
        Self::Fence(ExclusiveDeviceLeaseGrantError::from(error))
    }
}

impl From<LeaseStoreError> for ExclusiveDeviceLeaseReturnError {
    fn from(error: LeaseStoreError) -> Self {
        Self::Storage(error.reason())
    }
}

impl fmt::Display for ExclusiveDeviceLeaseReturnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fence(error) => write!(formatter, "ExclusiveDeviceLease fence rejected: {error}"),
            Self::UnknownLease => write!(formatter, "unknown ExclusiveDeviceLease"),
            Self::DeclareRequiresReserved { state } => write!(
                formatter,
                "ExclusiveDeviceLease reset declare requires DEVICE_RESERVED or RESETTING, found {state:?}"
            ),
            Self::ReceiptRequiresResetting { state } => write!(
                formatter,
                "ExclusiveDeviceLease reset receipt requires RESETTING, found {state:?}"
            ),
            Self::StaleResetGeneration { receipt, head } => write!(
                formatter,
                "ExclusiveDeviceLease reset receipt generation fenced out: receipt {} != head {}",
                receipt.get(),
                head.get()
            ),
            Self::ReceiptBindMismatch { receipt } => write!(
                formatter,
                "ExclusiveDeviceLease reset receipt does not bind the committed lease: {receipt:?}"
            ),
            Self::ConflictingResetReceipt {
                existing,
                requested,
            } => write!(
                formatter,
                "ExclusiveDeviceLease reset receipt conflict: committed {existing:?} != requested {requested:?}"
            ),
            Self::ResetGenerationExhausted => write!(
                formatter,
                "ExclusiveDeviceLease DeviceLeaseHead reset generation space exhausted"
            ),
            Self::Storage(reason) => {
                write!(
                    formatter,
                    "ExclusiveDeviceLease durable store failure: {reason}"
                )
            }
        }
    }
}

impl Error for ExclusiveDeviceLeaseReturnError {}
