//! Pluggable Stage C `Cell` topology, lease, and controlled process-migration
//! inspection/execution seam (`IPC-D7`, ADR-0025 Decision 2 & Decision 4).
//!
//! Keeps zero coupling or edits to `nlos-cell` / `nlos-lease` while exposing
//! deterministic bounded projections (`ListCells`, `InspectCellLeases`) and
//! controlled `RequestProcessMigration` execution over SABI `SystemControl`.

use std::sync::Mutex;

use nlos_schema::MAX_SYSTEM_CONTROL_ENUM_LIMIT;
use nlos_schema::sabi::v1::{
    CellLeaseKind, CellLeaseLockState, CellReachabilityState, PartitionAutonomyGateState,
    RetryDirective, SabiErrorCode, SabiFailure,
};
use nlos_types::ReceiptId;

use crate::executor_receipt::derive_executor_receipt_id;

/// Default local single-node `CellId` reported by [`LocalSingleNodeCellInspector`].
pub const DEFAULT_LOCAL_CELL_ID: [u8; 16] = [0xCE; 16];

/// Bounded summary row of one Stage C `Cell` (`IPC-D7`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CellSummaryRecord {
    pub cell_id: [u8; 16],
    pub cell_label: String,
    pub reachability: CellReachabilityState,
    pub partition_autonomy_gate: PartitionAutonomyGateState,
    pub current_epoch: u64,
    pub active_lease_count: u64,
    pub expected_generation_or_revision: u64,
}

/// Bounded cursor page returned by [`CellTopologyInspector::list_cells`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CellPage {
    pub cells: Vec<CellSummaryRecord>,
    pub next_cursor: Option<[u8; 16]>,
    pub truncated: bool,
}

/// Bounded lease row of one `Cell` (`Quota` / `Capacity` / `ExclusiveDevice`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CellLeaseRecord {
    pub lease_id: [u8; 16],
    pub cell_id: [u8; 16],
    pub kind: CellLeaseKind,
    pub lock_state: CellLeaseLockState,
    pub holder_id: [u8; 16],
    pub remaining_holder_ttl_ms: u64,
    pub lease_epoch: u64,
    pub clock_skew_horizon_ms: u64,
}

/// Bounded lease inspection page returned by [`CellLeaseInspector::inspect_cell_leases`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CellLeasePage {
    pub cell_id: [u8; 16],
    pub leases: Vec<CellLeaseRecord>,
    pub truncated: bool,
}

/// Alias used by `ControlOutcome::CellsListed`.
pub type CellListPage = CellPage;

/// Alias used by `ControlOutcome::CellLeasesInspected`.
pub type CellLeaseInspectPage = CellLeasePage;

/// Controlled `RequestProcessMigration` request handed to [`CellTopologyInspector`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessMigrationControlRequest {
    pub process_id: [u8; 16],
    pub destination_cell_id: [u8; 16],
    pub expected_generation_or_revision: u64,
    pub issuer_principal_id: [u8; 16],
    pub idempotency_key: [u8; 16],
    pub requested_at_ms: i64,
}

/// Pluggable Stage C `Cell` topology & migration seam (`IPC-D7`).
pub trait CellTopologyInspector: Send + Sync {
    /// Lists cells in deterministic ascending `cell_id` order after `cursor`,
    /// bounded to `limit <= 64`.
    ///
    /// # Errors
    ///
    /// Returns a crossing-safe [`SabiFailure`] when unwired or cursor/bounds fail.
    fn list_cells(&self, cursor: Option<[u8; 16]>, limit: usize) -> Result<CellPage, SabiFailure>;

    /// Executes a controlled `RequestProcessMigration` under CAS fencing.
    ///
    /// # Errors
    ///
    /// Returns a crossing-safe [`SabiFailure`] when destination cell is unknown,
    /// partitioned/quarantined, or migration is disabled on a single-node host.
    fn request_process_migration(
        &self,
        request: ProcessMigrationControlRequest,
    ) -> Result<ReceiptId, SabiFailure>;
}

/// Pluggable Stage C `Cell` lease inspection seam (`IPC-D7`).
pub trait CellLeaseInspector: Send + Sync {
    /// Inspects `Quota`, `Capacity`, and `ExclusiveDevice` leases for `cell_id`
    /// (or the local primary cell when `cell_id` is `None`), bounded to `limit <= 64`.
    ///
    /// # Errors
    ///
    /// Returns a crossing-safe [`SabiFailure`] when the target cell is absent.
    fn inspect_cell_leases(
        &self,
        cell_id: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<CellLeasePage, SabiFailure>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RecordedMigration {
    idempotency_key: [u8; 16],
    process_id: [u8; 16],
    destination_cell_id: [u8; 16],
    expected_generation: u64,
    receipt_id: ReceiptId,
}

/// Deterministic default/fixture `Cell` topology & lease inspector (`IPC-D7`).
///
/// Ships a canonical single-node local cell (`OnlineLocal` + `FullAutonomy`)
/// with representative `Quota`, `Capacity`, and `ExclusiveDevice` (`Quarantined`
/// capable) lease projections, and supports multi-cell / partition / migration
/// configuration in integration tests without touching `nlos-cell` or `nlos-lease`.
pub struct LocalSingleNodeCellInspector {
    cells: Vec<CellSummaryRecord>,
    leases: Vec<CellLeaseRecord>,
    allow_migration: bool,
    migrations: Mutex<Vec<RecordedMigration>>,
}

impl Default for LocalSingleNodeCellInspector {
    fn default() -> Self {
        Self::new_single_node(DEFAULT_LOCAL_CELL_ID)
    }
}

impl LocalSingleNodeCellInspector {
    /// Creates a deterministic single-node workstation cell fixture.
    #[must_use]
    pub fn new_single_node(local_cell_id: [u8; 16]) -> Self {
        let leases = vec![
            CellLeaseRecord {
                lease_id: [0xC1; 16],
                cell_id: local_cell_id,
                kind: CellLeaseKind::Quota,
                lock_state: CellLeaseLockState::Active,
                holder_id: [0x11; 16],
                remaining_holder_ttl_ms: 30_000,
                lease_epoch: 1,
                clock_skew_horizon_ms: 100,
            },
            CellLeaseRecord {
                lease_id: [0xC2; 16],
                cell_id: local_cell_id,
                kind: CellLeaseKind::Capacity,
                lock_state: CellLeaseLockState::Expiring,
                holder_id: [0x12; 16],
                remaining_holder_ttl_ms: 4_500,
                lease_epoch: 2,
                clock_skew_horizon_ms: 100,
            },
            CellLeaseRecord {
                lease_id: [0xC3; 16],
                cell_id: local_cell_id,
                kind: CellLeaseKind::ExclusiveDevice,
                lock_state: CellLeaseLockState::Quarantined,
                holder_id: [0x13; 16],
                remaining_holder_ttl_ms: 0,
                lease_epoch: 3,
                clock_skew_horizon_ms: 250,
            },
        ];
        let cells = vec![CellSummaryRecord {
            cell_id: local_cell_id,
            cell_label: "local-workstation-cell".to_owned(),
            reachability: CellReachabilityState::OnlineLocal,
            partition_autonomy_gate: PartitionAutonomyGateState::FullAutonomy,
            current_epoch: 3,
            active_lease_count: 3,
            expected_generation_or_revision: 3,
        }];
        Self {
            cells,
            leases,
            allow_migration: false,
            migrations: Mutex::new(Vec::new()),
        }
    }

    /// Creates a multi-cell topology & lease fixture with explicit migration gate.
    #[must_use]
    pub fn with_topology(
        mut cells: Vec<CellSummaryRecord>,
        mut leases: Vec<CellLeaseRecord>,
        allow_migration: bool,
    ) -> Self {
        cells.sort_by_key(|cell| cell.cell_id);
        leases.sort_by_key(|lease| (lease.cell_id, lease.lease_id));
        Self {
            cells,
            leases,
            allow_migration,
            migrations: Mutex::new(Vec::new()),
        }
    }
}

impl CellTopologyInspector for LocalSingleNodeCellInspector {
    fn list_cells(&self, cursor: Option<[u8; 16]>, limit: usize) -> Result<CellPage, SabiFailure> {
        let clamped = limit.clamp(1, MAX_SYSTEM_CONTROL_ENUM_LIMIT);
        let filtered: Vec<&CellSummaryRecord> = self
            .cells
            .iter()
            .filter(|cell| cursor.is_none_or(|after| cell.cell_id > after))
            .collect();
        let truncated = filtered.len() > clamped;
        let cells: Vec<CellSummaryRecord> = filtered.into_iter().take(clamped).cloned().collect();
        let next_cursor = if truncated {
            cells.last().map(|last| last.cell_id)
        } else {
            None
        };
        Ok(CellPage {
            cells,
            next_cursor,
            truncated,
        })
    }

    fn request_process_migration(
        &self,
        request: ProcessMigrationControlRequest,
    ) -> Result<ReceiptId, SabiFailure> {
        if let Ok(guard) = self.migrations.lock()
            && let Some(existing) = guard
                .iter()
                .find(|entry| entry.idempotency_key == request.idempotency_key)
        {
            if existing.process_id == request.process_id
                && existing.destination_cell_id == request.destination_cell_id
                && existing.expected_generation == request.expected_generation_or_revision
            {
                return Ok(existing.receipt_id);
            }
            return Err(SabiFailure {
                code: SabiErrorCode::Conflict.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "idempotency key reused with divergent migration parameters"
                    .to_owned(),
            });
        }

        let Some(dest) = self
            .cells
            .iter()
            .find(|cell| cell.cell_id == request.destination_cell_id)
        else {
            return Err(SabiFailure {
                code: SabiErrorCode::NotFound.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "destination cell was not found".to_owned(),
            });
        };

        if !self.allow_migration {
            return Err(SabiFailure {
                code: SabiErrorCode::NotSupported.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "single-node cell topology has no peer migration target enabled"
                    .to_owned(),
            });
        }

        if matches!(
            dest.reachability,
            CellReachabilityState::Partitioned | CellReachabilityState::Offline
        ) || matches!(
            dest.partition_autonomy_gate,
            PartitionAutonomyGateState::FencedReadOnly | PartitionAutonomyGateState::Quarantined
        ) {
            return Err(SabiFailure {
                code: SabiErrorCode::Fenced.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "destination cell partition autonomy gate forbids process migration"
                    .to_owned(),
            });
        }

        let receipt_id = derive_executor_receipt_id(
            b"llmos/cell-process-migration/v1\0",
            &[
                &request.process_id,
                &request.destination_cell_id,
                &request.expected_generation_or_revision.to_be_bytes(),
                &request.idempotency_key,
            ],
        );

        if let Ok(mut guard) = self.migrations.lock() {
            guard.push(RecordedMigration {
                idempotency_key: request.idempotency_key,
                process_id: request.process_id,
                destination_cell_id: request.destination_cell_id,
                expected_generation: request.expected_generation_or_revision,
                receipt_id,
            });
        }
        Ok(receipt_id)
    }
}

impl CellLeaseInspector for LocalSingleNodeCellInspector {
    fn inspect_cell_leases(
        &self,
        cell_id: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<CellLeasePage, SabiFailure> {
        let resolved_cell_id = match cell_id {
            Some(explicit) => explicit,
            None => self
                .cells
                .first()
                .map_or(DEFAULT_LOCAL_CELL_ID, |cell| cell.cell_id),
        };
        if !self
            .cells
            .iter()
            .any(|cell| cell.cell_id == resolved_cell_id)
        {
            return Err(SabiFailure {
                code: SabiErrorCode::NotFound.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "requested cell was not found".to_owned(),
            });
        }
        let clamped = limit.clamp(1, MAX_SYSTEM_CONTROL_ENUM_LIMIT);
        let matching: Vec<&CellLeaseRecord> = self
            .leases
            .iter()
            .filter(|lease| lease.cell_id == resolved_cell_id)
            .collect();
        let truncated = matching.len() > clamped;
        let leases = matching.into_iter().take(clamped).cloned().collect();
        Ok(CellLeasePage {
            cell_id: resolved_cell_id,
            leases,
            truncated,
        })
    }
}
