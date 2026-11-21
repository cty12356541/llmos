//! Two-tier materialization scheduler, minimal form (W31-F, B4-6; v0.5
//! §25.2.2 两层形态——「足以支撑 benchmark 的两层」，not a full OS
//! scheduler; 行 4503 `[SCALE-MATERIALIZE-001]` window shaping, 行 4538
//! `[SCHED-BACKPRESSURE-001]` posture).
//!
//! The two tiers compose the W31-A gate; they own no second state
//! machine and no second write face:
//!
//! - **Global tier** ([`MaterializationScheduler::select`]): a pure
//!   eligibility scan over durable plan state. A node is a candidate
//!   when its state can still await materialization (`DECLARED` through
//!   `WAITING_*`/`REHYDRATING`), it carries no pending gate round, and
//!   every dependency of its pinned declared-revision shape is
//!   `COMPLETED`. Candidates are selected in **ready-FIFO** order —
//!   `(first_declared_at_ms, TaskNodeId)` — bounded by the window's
//!   free seats. The policy is deliberately deterministic and
//!   wall-clock-free: priority/deadline/locality shaping is controller
//!   policy (MUST NOT here); a node also only becomes a candidate once
//!   its dependencies complete, so arrival-order selection over ready
//!   nodes is the minimal topological-ready priority.
//! - **Worker tier** ([`MaterializationScheduler::drive`]): maps each
//!   selection to the W31-A gate — `request_materialization` →
//!   admission consult ([`AdmissionConsult`]) → `resolve_materialization`.
//!   A durable `PENDING` round (a crashed pre-restart drive) is
//!   **adopted** and resolved under its original exactly-once key; a
//!   fresh round's key is derived as
//!   `digest(SCHEDULER_REQUEST_KEY_DOMAIN, node_id, retry_round)` with
//!   the retry round read from the node's durable request history, so
//!   keys are restart-stable and per-round fresh.
//!
//! **Window semantics**: the window bounds concurrent materialization
//! seats = pending gate rounds + nodes in the W31-A window band
//! (`MATERIALIZING/ACTIVE/CHECKPOINTED/REHYDRATING`; `EVICTED` and
//! terminal states released their seats). Each admission rejection
//! shrinks the window by one seat (floor 1, so sustained pressure keeps
//! a durable one-probe-per-pass rejection trail instead of going
//! silent) — but only while the window is running: a controller-set `0`
//! is a full stop that outlives any rejection trail, so the shrink
//! path never revives it. Approvals never grow the window — the only
//! growth path is the controller lever
//! [`MaterializationScheduler::set_window`]. Pending rounds always
//! drive to resolution regardless of budget: they already hold seats,
//! and resolving them is crash convergence, not a new materialization.
//!
//! **Inspect surface**: recent decisions (selected/skipped with typed
//! reasons, approved/rejected, consult failures, typed gate refusals)
//! are a bounded in-memory readback
//! ([`MaterializationScheduler::decisions`]). The honest minimal-form
//! choice: the *durable* audit trail is the plan authority's own rows
//! (request rows and vouchers via the W31-A inspect faces) — a durable
//! decision log would duplicate that authority, so the diagnostic trail
//! is memory-only and lost on restart, by design.
//!
//! **Consult boundary**: the Worker tier consults admission through the
//! [`AdmissionConsult`] trait, keeping `nlos-task` out of this crate's
//! public face (it remains a dev-dependency for the test wiring). The
//! 1:1 mapping from `SqliteTaskAuthority::answer_plan_materialization`
//! onto [`AdmissionConsultOutcome`] is assembler wiring (slice-k); the
//! G3 battery and the scheduler battery both carry it inline.
//!
//! The scheduler selects **for materialization only** — dispatch
//! decisions (派发决策) stay with the controller; nothing here runs,
//! authorizes, or bypasses the gate.
//!
//! Known limitation (registered in the lane evidence): a node cancelled
//! while its gate round is `PENDING` leaves a dead round that still
//! counts as a seat; cancel-vs-in-flight-round reconciliation is
//! controller/W31-C territory. Storage failures abort the pass
//! (`PlanStoreError::Sqlite`); typed gate refusals are per-node
//! [`SchedulerDecision::DriveRefused`] entries.

use std::collections::HashMap;

use nlos_types::{IdempotencyKey, ReceiptId, TaskNodeId, TaskPlanId};
use rusqlite::{Connection, OptionalExtension, params};

use crate::PlanStoreError;
use crate::materialization::{
    REQUEST_COLUMNS, decode_request_row, raw_request_row, unresolved_dependencies,
};
use crate::model::{
    MaterializationAdmission, MaterializationAdmissionVerdict, MaterializationRejection,
    MaterializationRequest, MaterializationResolution, MaterializationResolutionDecision,
    PlanNodeState, SCHEDULER_REQUEST_KEY_DOMAIN, decode_state,
};
use crate::store::{
    SqlitePlanAuthority, decode_node_row, decode_u64, digest16, encode_u64, load_plan_head,
    raw_node_row,
};

/// The consult answer for one materialization candidate: the Task-side
/// admission facts an approval records, or the typed denial a rejection
/// records (the W31-A consult half, boundary-mapped).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdmissionConsultOutcome {
    Admitted(MaterializationAdmission),
    Denied(MaterializationRejection),
}

/// The Worker tier's admission boundary. One consult answers "does one
/// more materialization fit the tier?" for a candidate node; `Err`
/// denotes a failed consult (transport/storage posture), not a denial —
/// the round stays `PENDING` and the next pass adopts it.
pub trait AdmissionConsult {
    /// The consult's own failure type (diagnostics stay with the
    /// implementation; the scheduler records only the fact).
    type Error;

    /// Consults the Task-side admission for one candidate.
    ///
    /// `other_declared_task_nodes` is the plan authority's persisted
    /// declared-TaskNode count excluding the candidate (the consult
    /// projects `+1`), the W31-A convention.
    ///
    /// # Errors
    ///
    /// `Err` denotes a failed consult (transport/storage posture), not
    /// a denial — a denial is the `Ok(AdmissionConsultOutcome::Denied)`
    /// answer.
    fn consult_materialization(
        &self,
        other_declared_task_nodes: u64,
    ) -> Result<AdmissionConsultOutcome, Self::Error>;
}

/// Why the Global tier did not select one scanned node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectionSkipReason {
    /// At least one declared dependency is not `COMPLETED` (named,
    /// deterministically sorted).
    DependenciesNotReady { unresolved: Vec<TaskNodeId> },
    /// The node's state cannot await materialization (holds a window
    /// seat, is terminal, or was cancelled).
    NotAwaitingMaterialization { current: PlanNodeState },
    /// The node is a candidate but the window has no free seat.
    WindowExhausted,
}

/// One Global-tier selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectionEntry {
    pub node_id: TaskNodeId,
    pub kind: SelectionKind,
}

/// What the Global tier decided a selection is: adopt a crashed
/// in-flight gate round, or open a fresh one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionKind {
    /// Resume the node's durable `PENDING` gate round under its
    /// original exactly-once key (crash-window convergence).
    AdoptPendingGateRound { request_key: IdempotencyKey },
    /// Open a new gate round (Worker tier derives the key).
    NewGateRound,
}

/// One Global-tier skip.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkipEntry {
    pub node_id: TaskNodeId,
    pub reason: SelectionSkipReason,
}

/// The Global tier's selection report: what one pass will drive, in
/// ready-FIFO scan order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectionReport {
    pub plan_id: TaskPlanId,
    /// The window bound this report was computed under.
    pub window: u64,
    /// Pending gate rounds + window-band nodes at scan time.
    pub seats_in_use: u64,
    pub selections: Vec<SelectionEntry>,
    pub skips: Vec<SkipEntry>,
}

/// One inspectable scheduler decision (in-memory diagnostic trail; the
/// durable audit trail is the plan authority's request rows and
/// vouchers).
#[derive(Debug)]
pub enum SchedulerDecision {
    /// Global tier selected the node (pass scan order).
    Selected {
        node_id: TaskNodeId,
        kind: SelectionKind,
    },
    /// Global tier skipped the node with a typed reason.
    Skipped {
        node_id: TaskNodeId,
        reason: SelectionSkipReason,
    },
    /// Worker tier: the gate approved the round; the node is
    /// `MATERIALIZING` with the named voucher.
    Approved {
        node_id: TaskNodeId,
        request_key: IdempotencyKey,
        voucher_id: ReceiptId,
    },
    /// Worker tier: admission denied; the window shrank.
    Rejected {
        node_id: TaskNodeId,
        request_key: IdempotencyKey,
        reason: MaterializationRejection,
    },
    /// Worker tier: the consult itself failed; the round stays
    /// `PENDING` for the next pass to adopt.
    ConsultFailed {
        node_id: TaskNodeId,
        request_key: IdempotencyKey,
    },
    /// Worker tier: the gate refused the drive with a typed error (the
    /// durable state converged by gate semantics).
    DriveRefused {
        node_id: TaskNodeId,
        error: PlanStoreError,
    },
}

/// One decision-trail entry.
#[derive(Debug)]
pub struct SchedulerDecisionRecord {
    pub pass: u64,
    pub at_ms: u64,
    pub decision: SchedulerDecision,
}

/// Typed readback of one executed pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchedulerPassSummary {
    /// Dense pass counter (diagnostic; restarts reset it).
    pub pass: u64,
    pub plan_id: TaskPlanId,
    pub window_before: u64,
    pub window_after: u64,
    pub selected: u64,
    pub skipped: u64,
    pub approved: u64,
    pub rejected: u64,
}

/// Default bound of the in-memory decision trail.
pub const DEFAULT_DECISION_LOG_CAPACITY: usize = 1_024;

/// Integer `node_state` discriminants that satisfy
/// `state_can_await_materialization`: `DECLARED` (`1`),
/// `BLOCKED_DEPENDENCY` (`2`), `ELIGIBLE` (`3`),
/// `WAITING_AUTHORIZATION` (`4`), `WAITING_RESOURCE` (`5`), and
/// `REHYDRATING` (`10`).
pub const AWAITING_MATERIALIZATION_STATES: [i64; 6] = [1, 2, 3, 4, 5, 10];

/// Integer `node_state` discriminants occupying a W31-A window-band
/// seat: `MATERIALIZING` (`6`), `ACTIVE` (`7`), `CHECKPOINTED` (`8`), and
/// `REHYDRATING` (`10`).
pub const ACTIVE_BAND_STATES: [i64; 4] = [6, 7, 8, 10];

/// Covering-index query used by [`MaterializationScheduler::select_bounded`]
/// to count active window-band seats via `plan_nodes_state_order`.
pub const BOUNDED_BAND_SEATS_SQL: &str = "SELECT COUNT(*) FROM plan_nodes
     WHERE plan_id = ?1 AND node_state IN (6, 7, 8, 10)";

/// Single-state streaming cursor query used by
/// [`MaterializationScheduler::select_bounded`]. Fixing both `plan_id`
/// and `node_state` lets `SQLite` walk `plan_nodes_state_order` in
/// `(first_declared_at_ms, task_node_id)` order with zero
/// `USE TEMP B-TREE FOR ORDER BY` and zero full-table scan.
pub const BOUNDED_AWAITING_CURSOR_SQL: &str =
    "SELECT task_node_id, first_declared_at_ms, declared_revision
     FROM plan_nodes
     WHERE plan_id = ?1 AND node_state = ?2
     ORDER BY first_declared_at_ms, task_node_id";

const BOUNDED_PENDING_REQUESTS_SQL: &str = "SELECT task_node_id, idempotency_key
     FROM plan_materialization_requests
     WHERE plan_id = ?1 AND status = 1";

const BOUNDED_PENDING_NODE_STATE_SQL: &str = "SELECT node_state, first_declared_at_ms
     FROM plan_nodes
     WHERE plan_id = ?1 AND task_node_id = ?2";

const UNRESOLVED_DEPENDENCIES_SQL: &str = "SELECT edges.dependency_node_id, nodes.node_state
     FROM plan_revision_edges AS edges
     LEFT JOIN plan_nodes AS nodes
       ON nodes.plan_id = edges.plan_id
      AND nodes.task_node_id = edges.dependency_node_id
     WHERE edges.plan_id = ?1 AND edges.revision = ?2
       AND edges.dependent_node_id = ?3
     ORDER BY edges.dependency_node_id";

/// The shrink floor: sustained rejections keep one selection per pass
/// so pressure stays observable as a durable probe instead of a silent
/// full stop (full stop is a controller decision, `set_window(0)` —
/// and a controller-set zero survives the rejection trail; the shrink
/// path applies the floor only to a running window).
const WINDOW_SHRINK_FLOOR: u64 = 1;

fn state_can_await_materialization(state: PlanNodeState) -> bool {
    matches!(
        state,
        PlanNodeState::Declared
            | PlanNodeState::BlockedDependency
            | PlanNodeState::Eligible
            | PlanNodeState::WaitingAuthorization
            | PlanNodeState::WaitingResource
            | PlanNodeState::Rehydrating
    )
}

fn scheduler_request_key(node_id: TaskNodeId, retry_round: u64) -> IdempotencyKey {
    IdempotencyKey::from_bytes(digest16(
        SCHEDULER_REQUEST_KEY_DOMAIN,
        &[node_id.as_bytes(), &retry_round.to_be_bytes()],
    ))
}

/// The identity of one drive pass, carried into its decision records.
#[derive(Clone, Copy, Debug)]
struct PassMarker {
    pass: u64,
    at_ms: u64,
}

/// The W31-F two-tier materialization scheduler. In-memory policy state
/// (window, pass counter, decision trail) over the durable plan
/// authority; every effect lands through the W31-A gate faces.
#[derive(Debug)]
pub struct MaterializationScheduler {
    window: u64,
    next_pass: u64,
    decision_log: Vec<SchedulerDecisionRecord>,
    decision_log_capacity: usize,
}

impl MaterializationScheduler {
    /// Creates a scheduler with the given materialization window and
    /// the default decision-trail bound.
    #[must_use]
    pub fn new(window: u64) -> Self {
        Self::with_decision_log_capacity(window, DEFAULT_DECISION_LOG_CAPACITY)
    }

    /// Creates a scheduler with an explicit decision-trail bound
    /// (`0` disables the trail).
    #[must_use]
    pub fn with_decision_log_capacity(window: u64, decision_log_capacity: usize) -> Self {
        Self {
            window,
            next_pass: 0,
            decision_log: Vec::new(),
            decision_log_capacity,
        }
    }

    /// The current materialization window.
    #[must_use]
    pub fn window(&self) -> u64 {
        self.window
    }

    /// The controller lever: sets the window. This is the only growth
    /// path — passes never grow it — and may fully stop selection
    /// (`0`); a full stop survives the rejection trail (the shrink
    /// path never revives a controller-set zero), and pending gate
    /// rounds still drive to resolution.
    pub fn set_window(&mut self, window: u64) {
        self.window = window;
    }

    /// The bounded in-memory decision trail, oldest first.
    #[must_use]
    pub fn decisions(&self) -> &[SchedulerDecisionRecord] {
        &self.decision_log
    }

    /// **Global tier**: scans one plan's durable state and selects
    /// which nodes to materialize within the window. Pure read; records
    /// nothing.
    ///
    /// # Errors
    ///
    /// Fails typed on unknown plans
    /// ([`PlanStoreError::PlanNotFound`]) or storage failure.
    pub fn select(
        &self,
        authority: &SqlitePlanAuthority,
        plan_id: TaskPlanId,
    ) -> Result<SelectionReport, PlanStoreError> {
        let connection = authority.lock()?;
        if load_plan_head(&connection, plan_id)?.is_none() {
            return Err(PlanStoreError::PlanNotFound(plan_id));
        }

        let mut statement = connection.prepare(
            "SELECT plan_id, task_node_id, node_key, node_kind, declared_revision,
                    node_digest, node_state, transition_count,
                    residency_tier, residency_transition_count,
                    first_declared_at_ms, updated_at_ms
             FROM plan_nodes WHERE plan_id = ?1
             ORDER BY first_declared_at_ms, task_node_id",
        )?;
        let nodes = statement
            .query_map(params![plan_id.as_bytes().as_slice()], raw_node_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(PlanStoreError::from)?
            .into_iter()
            .map(decode_node_row)
            .collect::<Result<Vec<_>, _>>()?;

        let mut pending: HashMap<TaskNodeId, IdempotencyKey> = HashMap::new();
        {
            let mut statement = connection.prepare(&format!(
                "SELECT {REQUEST_COLUMNS}
                 FROM plan_materialization_requests
                 WHERE plan_id = ?1 AND status = 1"
            ))?;
            let rows =
                statement.query_map(params![plan_id.as_bytes().as_slice()], raw_request_row)?;
            for row in rows {
                let record = decode_request_row(row?)?;
                pending.insert(record.node_id, record.idempotency_key);
            }
        }

        let band_seats: i64 = connection.query_row(
            "SELECT COUNT(*) FROM plan_nodes
             WHERE plan_id = ?1 AND node_state IN (6, 7, 8, 10)",
            params![plan_id.as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        let pending_seats = u64::try_from(pending.len()).unwrap_or(u64::MAX);
        let seats_in_use = decode_u64(band_seats)?.saturating_add(pending_seats);
        let mut budget = self.window.saturating_sub(seats_in_use);

        let mut selections = Vec::new();
        let mut skips = Vec::new();
        for node in nodes {
            if let Some(request_key) = pending.get(&node.node_id) {
                if !state_can_await_materialization(node.state) {
                    skips.push(SkipEntry {
                        node_id: node.node_id,
                        reason: SelectionSkipReason::NotAwaitingMaterialization {
                            current: node.state,
                        },
                    });
                    continue;
                }
                selections.push(SelectionEntry {
                    node_id: node.node_id,
                    kind: SelectionKind::AdoptPendingGateRound {
                        request_key: *request_key,
                    },
                });
                continue;
            }
            if !state_can_await_materialization(node.state) {
                skips.push(SkipEntry {
                    node_id: node.node_id,
                    reason: SelectionSkipReason::NotAwaitingMaterialization {
                        current: node.state,
                    },
                });
                continue;
            }
            let unresolved = unresolved_dependencies(&connection, &node)?;
            if !unresolved.is_empty() {
                skips.push(SkipEntry {
                    node_id: node.node_id,
                    reason: SelectionSkipReason::DependenciesNotReady { unresolved },
                });
                continue;
            }
            if budget == 0 {
                skips.push(SkipEntry {
                    node_id: node.node_id,
                    reason: SelectionSkipReason::WindowExhausted,
                });
                continue;
            }
            budget -= 1;
            selections.push(SelectionEntry {
                node_id: node.node_id,
                kind: SelectionKind::NewGateRound,
            });
        }

        Ok(SelectionReport {
            plan_id,
            window: self.window,
            seats_in_use,
            selections,
            skips,
        })
    }

    /// **Global tier (bounded)**: ready-FIFO selection over awaiting states
    /// using the `plan_nodes_state_order` index, stopping as soon as the
    /// window budget reaches zero and all remaining pending gate rounds are
    /// resolved.
    ///
    /// Unlike [`Self::select`], this bounded path does **not** perform a
    /// full-table scan over non-awaiting (`MATERIALIZING`, `ACTIVE`,
    /// `CHECKPOINTED`, `EVICTED`, or terminal) nodes and does **not**
    /// enumerate `SelectionSkipReason::NotAwaitingMaterialization` (or
    /// post-budget `WindowExhausted` / `DependenciesNotReady`) entries for
    /// unvisited rows. `skips` contains only skips observed during the
    /// bounded scan and pending-round resolution; callers requiring a
    /// full-plan skip diagnostic inventory should use [`Self::select`].
    ///
    /// Under any plan topology, `selections` (including ready-FIFO order
    /// and `AdoptPendingGateRound` entries) and `seats_in_use` match
    /// [`Self::select`] identically.
    ///
    /// # Errors
    ///
    /// Fails typed on unknown plans
    /// ([`PlanStoreError::PlanNotFound`]) or storage failure.
    pub fn select_bounded(
        &self,
        authority: &SqlitePlanAuthority,
        plan_id: TaskPlanId,
    ) -> Result<SelectionReport, PlanStoreError> {
        let connection = authority.lock()?;
        if load_plan_head(&connection, plan_id)?.is_none() {
            return Err(PlanStoreError::PlanNotFound(plan_id));
        }
        let plan_bytes = plan_id.as_bytes().as_slice();

        let mut remaining_pending: HashMap<TaskNodeId, IdempotencyKey> = HashMap::new();
        {
            let mut statement = connection.prepare_cached(BOUNDED_PENDING_REQUESTS_SQL)?;
            let mut rows = statement.query(params![plan_bytes])?;
            while let Some(row) = rows.next()? {
                let node_id =
                    TaskNodeId::from_bytes(fixed16_col(row, 0, "materialization node id")?);
                let request_key = IdempotencyKey::from_bytes(fixed16_col(
                    row,
                    1,
                    "materialization idempotency key",
                )?);
                remaining_pending.insert(node_id, request_key);
            }
        }

        let band_seats: i64 =
            connection.query_row(BOUNDED_BAND_SEATS_SQL, params![plan_bytes], |row| {
                row.get(0)
            })?;
        let pending_seats = u64::try_from(remaining_pending.len()).unwrap_or(u64::MAX);
        let seats_in_use = decode_u64(band_seats)?.saturating_add(pending_seats);
        let mut budget = self.window.saturating_sub(seats_in_use);

        let mut selections = Vec::new();
        let mut skips = Vec::new();

        if budget > 0 {
            scan_awaiting_bounded(
                &connection,
                plan_id,
                &mut remaining_pending,
                &mut budget,
                &mut selections,
                &mut skips,
            )?;
        }

        if !remaining_pending.is_empty() {
            resolve_remaining_pending(
                &connection,
                plan_id,
                &remaining_pending,
                &mut selections,
                &mut skips,
            )?;
        }

        Ok(SelectionReport {
            plan_id,
            window: self.window,
            seats_in_use,
            selections,
            skips,
        })
    }

    /// **Worker tier**: drives one selection report through the W31-A
    /// gate (request → consult → resolve per selection), records the
    /// decision trail, and shrinks the window on every admission
    /// rejection.
    ///
    /// # Errors
    ///
    /// Storage failures abort the pass
    /// ([`PlanStoreError::Sqlite`]); every typed gate refusal is a
    /// per-node [`SchedulerDecision::DriveRefused`] entry instead.
    pub fn drive<C: AdmissionConsult>(
        &mut self,
        authority: &SqlitePlanAuthority,
        report: &SelectionReport,
        admission: &C,
        at_ms: u64,
    ) -> Result<SchedulerPassSummary, PlanStoreError> {
        let marker = PassMarker {
            pass: self.next_pass,
            at_ms,
        };
        self.next_pass = self.next_pass.saturating_add(1);
        let mut summary = SchedulerPassSummary {
            pass: marker.pass,
            plan_id: report.plan_id,
            window_before: self.window,
            window_after: self.window,
            selected: 0,
            skipped: 0,
            approved: 0,
            rejected: 0,
        };

        for skip in &report.skips {
            self.record(
                marker,
                SchedulerDecision::Skipped {
                    node_id: skip.node_id,
                    reason: skip.reason.clone(),
                },
            );
            summary.skipped += 1;
        }
        for entry in &report.selections {
            summary.selected += 1;
            self.record(
                marker,
                SchedulerDecision::Selected {
                    node_id: entry.node_id,
                    kind: entry.kind,
                },
            );
            match entry.kind {
                SelectionKind::AdoptPendingGateRound { request_key } => {
                    self.consult_and_resolve(
                        authority,
                        entry.node_id,
                        request_key,
                        admission,
                        marker,
                        &mut summary,
                    )?;
                }
                SelectionKind::NewGateRound => {
                    let history = authority
                        .inspect_node_materialization_requests(report.plan_id, entry.node_id)?;
                    let retry_round = u64::try_from(history.len()).unwrap_or(u64::MAX);
                    let request_key = scheduler_request_key(entry.node_id, retry_round);
                    match authority.request_materialization(MaterializationRequest {
                        plan_id: report.plan_id,
                        node_id: entry.node_id,
                        idempotency_key: request_key,
                        requested_at_ms: marker.at_ms,
                    }) {
                        Ok(_) => self.consult_and_resolve(
                            authority,
                            entry.node_id,
                            request_key,
                            admission,
                            marker,
                            &mut summary,
                        )?,
                        Err(PlanStoreError::Sqlite(error)) => {
                            return Err(PlanStoreError::Sqlite(error));
                        }
                        Err(error) => self.record(
                            marker,
                            SchedulerDecision::DriveRefused {
                                node_id: entry.node_id,
                                error,
                            },
                        ),
                    }
                }
            }
        }

        summary.window_after = self.window;
        Ok(summary)
    }

    /// One pass: the Global tier selects, the Worker tier drives.
    ///
    /// # Errors
    ///
    /// Same surface as [`MaterializationScheduler::select`] and
    /// [`MaterializationScheduler::drive`].
    pub fn run_pass<C: AdmissionConsult>(
        &mut self,
        authority: &SqlitePlanAuthority,
        plan_id: TaskPlanId,
        admission: &C,
        at_ms: u64,
    ) -> Result<SchedulerPassSummary, PlanStoreError> {
        let report = self.select(authority, plan_id)?;
        self.drive(authority, &report, admission, at_ms)
    }

    /// One bounded pass: the Global tier selects via
    /// [`MaterializationScheduler::select_bounded`], the Worker tier
    /// drives.
    ///
    /// # Errors
    ///
    /// Same surface as [`MaterializationScheduler::select_bounded`] and
    /// [`MaterializationScheduler::drive`].
    pub fn run_pass_bounded<C: AdmissionConsult>(
        &mut self,
        authority: &SqlitePlanAuthority,
        plan_id: TaskPlanId,
        admission: &C,
        at_ms: u64,
    ) -> Result<SchedulerPassSummary, PlanStoreError> {
        let report = self.select_bounded(authority, plan_id)?;
        self.drive(authority, &report, admission, at_ms)
    }

    /// Consults admission for one gate round and resolves it with the
    /// verdict. A failed consult leaves the round `PENDING` (adopted by
    /// the next pass); a rejection shrinks the window; typed gate
    /// refusals become decision-trail entries.
    ///
    /// # Errors
    ///
    /// Storage failures abort the pass
    /// ([`PlanStoreError::Sqlite`]); typed gate refusals are recorded
    /// per node instead.
    fn consult_and_resolve<C: AdmissionConsult>(
        &mut self,
        authority: &SqlitePlanAuthority,
        node_id: TaskNodeId,
        request_key: IdempotencyKey,
        admission: &C,
        marker: PassMarker,
        summary: &mut SchedulerPassSummary,
    ) -> Result<(), PlanStoreError> {
        let other_declared = authority
            .inspect_declared_task_node_count()?
            .saturating_sub(1);
        let verdict = match admission.consult_materialization(other_declared) {
            Ok(AdmissionConsultOutcome::Admitted(admission)) => {
                MaterializationAdmissionVerdict::Approved(admission)
            }
            Ok(AdmissionConsultOutcome::Denied(reason)) => {
                MaterializationAdmissionVerdict::Rejected(reason)
            }
            Err(_) => {
                self.record(
                    marker,
                    SchedulerDecision::ConsultFailed {
                        node_id,
                        request_key,
                    },
                );
                return Ok(());
            }
        };

        match authority.resolve_materialization(MaterializationResolution {
            request_key,
            verdict,
            resolved_at_ms: marker.at_ms,
        }) {
            Ok(
                MaterializationResolutionDecision::Approved(approval)
                | MaterializationResolutionDecision::ReplayedApproved(approval),
            ) => {
                self.record(
                    marker,
                    SchedulerDecision::Approved {
                        node_id,
                        request_key,
                        voucher_id: approval.voucher.voucher_id,
                    },
                );
                summary.approved += 1;
            }
            Ok(
                MaterializationResolutionDecision::Rejected(record)
                | MaterializationResolutionDecision::ReplayedRejected(record),
            ) => {
                let reason = record
                    .rejection
                    .expect("rejected rows carry their typed reason");
                self.record(
                    marker,
                    SchedulerDecision::Rejected {
                        node_id,
                        request_key,
                        reason: reason.clone(),
                    },
                );
                // A rejection shrinks a *running* window by one seat,
                // floored at 1 (the durable probe). A controller-set
                // `0` is a full stop: reviving it to the floor would
                // override the controller's stop decision —
                // `set_window` is the only growth path, in either
                // direction.
                if self.window > 0 {
                    self.window = self.window.saturating_sub(1).max(WINDOW_SHRINK_FLOOR);
                }
                summary.rejected += 1;
            }
            Err(PlanStoreError::Sqlite(error)) => return Err(PlanStoreError::Sqlite(error)),
            Err(error) => self.record(marker, SchedulerDecision::DriveRefused { node_id, error }),
        }
        Ok(())
    }

    fn record(&mut self, marker: PassMarker, decision: SchedulerDecision) {
        if self.decision_log_capacity == 0 {
            return;
        }
        self.decision_log.push(SchedulerDecisionRecord {
            pass: marker.pass,
            at_ms: marker.at_ms,
            decision,
        });
        while self.decision_log.len() > self.decision_log_capacity {
            self.decision_log.remove(0);
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct AwaitingCursorRow {
    node_id: TaskNodeId,
    first_declared_at_ms: u64,
    declared_revision: u64,
}

fn fixed16_col(
    row: &rusqlite::Row<'_>,
    index: usize,
    field: &'static str,
) -> Result<[u8; 16], PlanStoreError> {
    let bytes = row
        .get_ref(index)?
        .as_blob()
        .map_err(|_| PlanStoreError::CorruptRecord(field))?;
    bytes
        .try_into()
        .map_err(|_| PlanStoreError::CorruptRecord(field))
}

fn next_cursor_row(
    rows: &mut rusqlite::Rows<'_>,
) -> Result<Option<AwaitingCursorRow>, PlanStoreError> {
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let node_id = TaskNodeId::from_bytes(fixed16_col(row, 0, "node id")?);
    let first_declared_at_ms = decode_u64(row.get::<_, i64>(1)?)?;
    let declared_revision = decode_u64(row.get::<_, i64>(2)?)?;
    Ok(Some(AwaitingCursorRow {
        node_id,
        first_declared_at_ms,
        declared_revision,
    }))
}

fn unresolved_dependencies_for(
    connection: &Connection,
    plan_id: TaskPlanId,
    declared_revision: u64,
    node_id: TaskNodeId,
) -> Result<Vec<TaskNodeId>, PlanStoreError> {
    let mut statement = connection.prepare_cached(UNRESOLVED_DEPENDENCIES_SQL)?;
    let mut rows = statement.query(params![
        plan_id.as_bytes().as_slice(),
        encode_u64(declared_revision)?,
        node_id.as_bytes().as_slice(),
    ])?;
    let mut unresolved = Vec::new();
    while let Some(row) = rows.next()? {
        let dependency_id = TaskNodeId::from_bytes(fixed16_col(row, 0, "dependency node id")?);
        let state: Option<i64> = row.get(1)?;
        let state = state.ok_or(PlanStoreError::CorruptRecord("dependency node row missing"))?;
        if decode_state(state)? != PlanNodeState::Completed {
            unresolved.push(dependency_id);
        }
    }
    Ok(unresolved)
}

fn scan_awaiting_bounded(
    connection: &Connection,
    plan_id: TaskPlanId,
    remaining_pending: &mut HashMap<TaskNodeId, IdempotencyKey>,
    budget: &mut u64,
    selections: &mut Vec<SelectionEntry>,
    skips: &mut Vec<SkipEntry>,
) -> Result<(), PlanStoreError> {
    let plan_bytes = plan_id.as_bytes().as_slice();
    let mut statements = Vec::with_capacity(AWAITING_MATERIALIZATION_STATES.len());
    for _ in AWAITING_MATERIALIZATION_STATES {
        statements.push(connection.prepare(BOUNDED_AWAITING_CURSOR_SQL)?);
    }
    let mut cursors = Vec::with_capacity(AWAITING_MATERIALIZATION_STATES.len());
    for (statement, state_code) in statements.iter_mut().zip(AWAITING_MATERIALIZATION_STATES) {
        cursors.push(statement.query(params![plan_bytes, state_code])?);
    }
    let mut heads: [Option<AwaitingCursorRow>; AWAITING_MATERIALIZATION_STATES.len()] =
        [None; AWAITING_MATERIALIZATION_STATES.len()];
    for (cursor, head) in cursors.iter_mut().zip(heads.iter_mut()) {
        *head = next_cursor_row(cursor)?;
    }

    while *budget > 0 {
        let Some(min_idx) = heads
            .iter()
            .enumerate()
            .filter_map(|(idx, head)| head.map(|row| (idx, row)))
            .min_by(|(_, a), (_, b)| {
                a.first_declared_at_ms
                    .cmp(&b.first_declared_at_ms)
                    .then_with(|| a.node_id.cmp(&b.node_id))
            })
            .map(|(idx, _)| idx)
        else {
            break;
        };
        let Some(node) = heads[min_idx].take() else {
            break;
        };

        if let Some(request_key) = remaining_pending.remove(&node.node_id) {
            selections.push(SelectionEntry {
                node_id: node.node_id,
                kind: SelectionKind::AdoptPendingGateRound { request_key },
            });
        } else {
            let unresolved = unresolved_dependencies_for(
                connection,
                plan_id,
                node.declared_revision,
                node.node_id,
            )?;
            if unresolved.is_empty() {
                *budget -= 1;
                selections.push(SelectionEntry {
                    node_id: node.node_id,
                    kind: SelectionKind::NewGateRound,
                });
            } else {
                skips.push(SkipEntry {
                    node_id: node.node_id,
                    reason: SelectionSkipReason::DependenciesNotReady { unresolved },
                });
            }
        }

        if *budget == 0 {
            break;
        }
        heads[min_idx] = next_cursor_row(&mut cursors[min_idx])?;
    }
    Ok(())
}

fn resolve_remaining_pending(
    connection: &Connection,
    plan_id: TaskPlanId,
    remaining_pending: &HashMap<TaskNodeId, IdempotencyKey>,
    selections: &mut Vec<SelectionEntry>,
    skips: &mut Vec<SkipEntry>,
) -> Result<(), PlanStoreError> {
    let mut statement = connection.prepare_cached(BOUNDED_PENDING_NODE_STATE_SQL)?;
    let plan_bytes = plan_id.as_bytes().as_slice();
    let mut adoptable: Vec<(u64, TaskNodeId, IdempotencyKey)> = Vec::new();
    let mut non_awaiting: Vec<(u64, TaskNodeId, PlanNodeState)> = Vec::new();

    for (&node_id, &request_key) in remaining_pending {
        let Some((state_code, declared_at_ms)) = statement
            .query_row(params![plan_bytes, node_id.as_bytes().as_slice()], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })
            .optional()?
        else {
            continue;
        };
        let state = decode_state(state_code)?;
        let declared_at_ms = decode_u64(declared_at_ms)?;
        if state_can_await_materialization(state) {
            adoptable.push((declared_at_ms, node_id, request_key));
        } else {
            non_awaiting.push((declared_at_ms, node_id, state));
        }
    }

    adoptable.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    for (_, node_id, request_key) in adoptable {
        selections.push(SelectionEntry {
            node_id,
            kind: SelectionKind::AdoptPendingGateRound { request_key },
        });
    }

    non_awaiting.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    for (_, node_id, current) in non_awaiting {
        skips.push(SkipEntry {
            node_id,
            reason: SelectionSkipReason::NotAwaitingMaterialization { current },
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ACTIVE_BAND_STATES, AWAITING_MATERIALIZATION_STATES, state_can_await_materialization,
    };
    use crate::model::{PlanNodeState, decode_state};

    #[test]
    fn awaiting_and_band_state_codes_match_state_machine() {
        let mut expected_awaiting = Vec::new();
        let mut expected_band = Vec::new();
        for code in 1..=13_i64 {
            let state = decode_state(code).expect("valid state code");
            if state_can_await_materialization(state) {
                expected_awaiting.push(code);
            }
            if matches!(
                state,
                PlanNodeState::Materializing
                    | PlanNodeState::Active
                    | PlanNodeState::Checkpointed
                    | PlanNodeState::Rehydrating
            ) {
                expected_band.push(code);
            }
        }
        assert_eq!(
            expected_awaiting.as_slice(),
            AWAITING_MATERIALIZATION_STATES.as_slice()
        );
        assert_eq!(expected_band.as_slice(), ACTIVE_BAND_STATES.as_slice());
    }
}
