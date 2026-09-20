//! Runtime-side batch-cancel linkage for process-domain cancel propagation
//! (B-PROCESS-003 §W16-003 / W27-C): [`TokioRuntimeAdapter::cancel_process_fibers`]
//! consumes the process authority's `propagate_cancel_to_fibers` decision and
//! drives the runtime's tree-cancel through the cancellation scopes of every
//! live fiber registered under the fenced `(process_id, process_generation)`.
//!
//! Division of the fence: the durable side invalidates fiber **incarnations**
//! (immutable receipts, fail-closed inspect/resume gates — process domain);
//! the runtime side cancels **scopes** (the runtime's only tree-cancel
//! mechanism per `[FIBER-CANCEL-001]`). One linkage call performs both in
//! that order, fail-closed: a rejected durable decision leaves zero runtime
//! side effect.

use nlos_process::{
    FiberCancelPropagationDecision, ProcessAuthority, PropagateCancelToFibersRequest,
};
use nlos_runtime::RuntimeError;

use crate::{ChannelWaitError, ScopeKey, TokioRuntimeAdapter, lock_unpoisoned, wake::is_terminal};

/// The outcome of one [`TokioRuntimeAdapter::cancel_process_fibers`] call:
/// the consumed durable propagation decision plus the runtime-side sweep
/// counts. Every counter describes what this one call observed, so an
/// idempotent re-invocation reports its own (usually smaller or already
/// terminal) view, never accumulated history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessFiberCancelReport {
    /// The durable propagation decision consumed by this call
    /// (`Propagated` on first execution, `Replayed` on an idempotent
    /// re-invocation with the same batch key), exactly as returned by
    /// [`ProcessAuthority::propagate_cancel_to_fibers`].
    pub decision: FiberCancelPropagationDecision,
    /// Live registry records under the fenced
    /// `(process_id, expected_process_generation)` at sweep time.
    pub matched_fibers: usize,
    /// Of the matched records, those already in a terminal state at sweep
    /// time — their terminal state is unique and is never rewritten by the
    /// batch (only the fiber's own lifecycle ever writes a terminal state).
    pub already_terminal: usize,
    /// Distinct scopes of the matched records driven to cancel. Cancel is
    /// idempotent: a scope already cancelled by an earlier batch counts
    /// here again on re-invocation.
    pub canceled_scopes: usize,
    /// Matched-record scopes that vanished between sweep and cancel — every
    /// referencing fiber record was reaped (joined/detached) mid-sweep, so
    /// there was nothing left to cancel. Benign by construction: the key
    /// was resolved from a live record under the registry lock.
    pub vanished_scopes: usize,
}

impl TokioRuntimeAdapter {
    /// Batch-cancel linkage (B-PROCESS-003 §W16-003): consumes the
    /// process-side cancel propagation for `request`'s fenced
    /// `(process_id, expected_process_generation)` and drives the runtime's
    /// tree-cancel through the cancellation scopes of every live fiber
    /// registered under that fence.
    ///
    /// Gate order, fail-closed:
    ///
    /// 1. the durable propagation — [`ProcessAuthority::propagate_cancel_to_fibers`]
    ///    performs (or idempotently replays) the batch invalidation; any
    ///    rejection (idempotency conflict, stale process fence, a process
    ///    that is not terminal at the presented generation) fails this call
    ///    with **zero runtime side effect**;
    /// 2. the runtime sweep — every live fiber record whose spawn-time
    ///    `(process_id, process_generation)` equals the fence matches; the
    ///    same process id at another generation, and other process ids, are
    ///    untouched;
    /// 3. tree-cancel via scopes — each distinct matched
    ///    `(scope id, cancellation generation)` is cancelled exactly like
    ///    [`RuntimeAdapter::cancel_scope`](nlos_runtime::RuntimeAdapter::cancel_scope),
    ///    so every non-terminal fiber in those scopes reaches the unique
    ///    terminal `Cancelled` through the runtime's existing biased-select
    ///    semantics, already-terminal fibers keep their own terminal state,
    ///    registered waits resolve `Cancelled` with durable rows left for
    ///    the durable side, and late callbacks neither resurrect a cancelled
    ///    fiber nor panic.
    ///
    /// Scope granularity (a deliberate mapping): the runtime's cancellation
    /// tree node is the scope, so cancel selection is fenced by process
    /// identity/generation while the cancel itself is scope-wholesale — a
    /// scope shared with fibers outside the fence is cancelled for all of
    /// them, and a fiber of the fenced process in a fresh scope stays live
    /// (its durable incarnation is still fenced by the process authority).
    ///
    /// Locking follows the adapter's global order: the sweep reads record
    /// state under the `fibers` registry lock (the same sanctioned edge as
    /// the channel-delivery resume path, closing no cycle), then the scope
    /// cancels run under the `scopes` lock after the registry guard is
    /// dropped — sequential, never nested. Complexity is one O(n) registry
    /// sweep plus an O(n·m) scope-key dedup (n matched fibers, m distinct
    /// scopes).
    ///
    /// # Errors
    ///
    /// Returns [`ChannelWaitError::ProcessAuthority`] when the durable
    /// propagation is rejected (stale fence, idempotency rebinding, or a
    /// non-terminal process) — with zero runtime side effect.
    pub fn cancel_process_fibers(
        &self,
        process: &ProcessAuthority,
        request: PropagateCancelToFibersRequest,
    ) -> Result<ProcessFiberCancelReport, ChannelWaitError> {
        // Fail-closed durable gate first: a rejected batch leaves the
        // runtime untouched.
        let decision = process.propagate_cancel_to_fibers(request)?;

        // Sweep: collect the distinct scope keys of every live record under
        // the fence, plus the matched/terminal counts. Record state is read
        // under the registry lock — the sanctioned delivery-resume edge.
        let (scope_keys, matched_fibers, already_terminal) = {
            let registry = lock_unpoisoned(&self.inner.fibers);
            let mut scope_keys: Vec<ScopeKey> = Vec::new();
            let mut matched_fibers = 0;
            let mut already_terminal = 0;
            for record in registry.values() {
                if record.process_id != request.process_id
                    || record.process_generation != request.expected_process_generation
                {
                    continue;
                }
                matched_fibers += 1;
                if is_terminal(*lock_unpoisoned(&record.state)) {
                    already_terminal += 1;
                }
                if !scope_keys.contains(&record.scope_key) {
                    scope_keys.push(record.scope_key);
                }
            }
            (scope_keys, matched_fibers, already_terminal)
        };

        // Tree-cancel via scopes, under one `scopes` guard taken only after
        // the registry guard is gone. A key that vanished mid-sweep had all
        // its records reaped — nothing left to cancel, counted as vanished.
        let scopes = lock_unpoisoned(&self.inner.scopes);
        let mut canceled_scopes = 0;
        let mut vanished_scopes = 0;
        for key in scope_keys {
            match scopes.cancel(key.id, key.generation) {
                Ok(()) => canceled_scopes += 1,
                Err(RuntimeError::InvalidGeneration) => vanished_scopes += 1,
                Err(other) => return Err(other.into()),
            }
        }

        Ok(ProcessFiberCancelReport {
            decision,
            matched_fibers,
            already_terminal,
            canceled_scopes,
            vanished_scopes,
        })
    }
}
