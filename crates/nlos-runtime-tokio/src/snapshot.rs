//! The ADR-0009 decision-2 / ADR-0012 decision-2 B path, promoted from the
//! minimal prefix's bare marker: handler-entry input snapshot + idempotent
//! re-execution, semantics mirroring B-TASK-006O at fiber granularity.
//!
//! The durable face is the process authority's `fiber_entry_snapshots` slot
//! (ADR-0012 keeps the snapshot inside the B-PROCESS-001 authority family —
//! no new authority): latest-only per invocation, each write overwriting the
//! binding's single slot, garbage-collected when the fiber reaches terminal
//! state. Retention has no TTL and no expiry window: the snapshot is either
//! consumed by its recovery or disappears with the terminal state.
//!
//! Crash-window semantics are the kill-window matrix of the existing
//! registration precedents (ADR-0007/0008): the snapshot row commits in one
//! WAL/FULL transaction, so the durable states are exactly
//! `[absent | complete]`; a crash between the snapshot commit and the
//! handler's progress restores to the snapshot and re-executes the handler
//! from its entry, with the re-execution's durable interactions converged by
//! the existing idempotent dedup. Intermediate progress inside the handler
//! is honestly declared lost (B-TASK-006O semantics).

use nlos_process::{FiberEntrySnapshotRecord, ProcessAuthority};
use nlos_runtime::{FiberHandle, RuntimeError};
use nlos_wait::BindingId;

use crate::channel_wait::ChannelWaitError;
use crate::replay::ResumeRejection;
use crate::wake::is_terminal;
use crate::{TokioRuntimeAdapter, lock_unpoisoned};
use std::sync::atomic::Ordering;

/// The B-path contract of a fiber incarnation (ADR-0009 decision 2,
/// implemented per ADR-0012 decision 2): handler-level input snapshot +
/// idempotent re-execution, recovery to the handler entry, for fibers not
/// reshaped into the [`crate::ResumableBinding`] re-entrant form.
pub trait SnapshotResumable {
    /// The binding (logical fiber identity) this incarnation re-executes.
    #[must_use]
    fn binding(&self) -> BindingId;

    /// The durable process identity owning this binding.
    #[must_use]
    fn process_id(&self) -> nlos_types::ProcessId;

    /// The durable fiber incarnation this invocation belongs to; every
    /// durable side of the B path CAS's against it, so a stale incarnation
    /// fails closed with zero side effect (ADR-0012 generation gate).
    #[must_use]
    fn expected_incarnation(&self) -> nlos_types::Generation;

    /// The handler-entry input of the current invocation (opaque bytes),
    /// recorded as the entry snapshot.
    #[must_use]
    fn handler_input(&self) -> Vec<u8>;

    /// Re-executes the handler from its entry with the restored input.
    ///
    /// Must be idempotent across re-executions: the exactly-once boundary is
    /// carried by the existing durable dedup (registration uniqueness,
    /// effect idempotency), never by this contract. Durable interactions the
    /// re-execution performs are its own registrations — a resumed fiber is
    /// driven back to its wait point by its own code re-registering through
    /// the normal runtime entries.
    ///
    /// # Errors
    ///
    /// The incarnation's own failure, propagated verbatim as
    /// [`ChannelWaitError::ResumeRejected`].
    fn resume_from_entry(&self, input: &[u8]) -> Result<(), ResumeRejection>;
}

/// The outcome of one [`TokioRuntimeAdapter::resume_from_snapshot`] call.
#[derive(Debug)]
pub struct SnapshotResumeReport {
    /// The binding the snapshot was restored for.
    pub binding: BindingId,
    /// The snapshot consumed (the binding's latest entry input), `None` for
    /// a terminal or already-cancelled fiber — the terminal no-op analog of
    /// the A-path's empty report, zero durable side effect.
    pub restored: Option<FiberEntrySnapshotRecord>,
}

impl TokioRuntimeAdapter {
    /// Durably records the handler-entry input snapshot for this fiber's
    /// binding (B path, ADR-0012 decision 2). Latest-only per invocation:
    /// every call overwrites the binding's single snapshot slot.
    ///
    /// Gate order, fail-closed: runtime shutdown →
    /// [`RuntimeError::ShuttingDown`]; stale or unknown fiber handle →
    /// [`RuntimeError::InvalidGeneration`]; terminal or already-cancelled
    /// fiber → `Ok(None)` with zero durable side effect (the terminal GC,
    /// not a fresh snapshot, is what a terminal fiber's state calls for);
    /// then the process authority's write — which CAS's the presented
    /// incarnation against the binding's current registration, so a stale
    /// incarnation fails closed
    /// ([`ChannelWaitError::StaleFiberIncarnation`], zero side effect).
    ///
    /// # Errors
    ///
    /// Returns [`ChannelWaitError::Runtime`] for shutdown and stale/unknown
    /// fiber handles and [`ChannelWaitError::ProcessAuthority`] (including
    /// the stale-incarnation CAS) for the durable write.
    pub fn snapshot_handler_entry(
        &self,
        handle: FiberHandle,
        process: &ProcessAuthority,
        snapshot: &impl SnapshotResumable,
    ) -> Result<Option<FiberEntrySnapshotRecord>, ChannelWaitError> {
        if self.inner.shutdown.load(Ordering::Acquire) {
            return Err(RuntimeError::ShuttingDown.into());
        }
        let record = self.record_for(handle)?;
        if is_terminal(*lock_unpoisoned(&record.state)) || record.scope.is_cancelled() {
            return Ok(None);
        }
        let fiber = nlos_types::ExecutionFiberId::from_bytes(*snapshot.binding().as_bytes());
        process.inspect_active_process_binding(snapshot.process_id())?;
        let decision =
            process.write_fiber_entry_snapshot(nlos_process::WriteFiberEntrySnapshotRequest {
                process_id: snapshot.process_id(),
                binding: fiber,
                expected_incarnation_generation: snapshot.expected_incarnation(),
                handler_input: snapshot.handler_input(),
                written_at_ms: crate::channel_wait::now_millis(),
            })?;
        Ok(Some(decision.record().clone()))
    }

    /// Restores the binding's latest entry snapshot and re-executes the
    /// handler from its entry (B-path resume, ADR-0012 decision 2).
    ///
    /// Gate order, fail-closed:
    ///
    /// 1. runtime shutdown → [`RuntimeError::ShuttingDown`];
    /// 2. stale or unknown fiber handle → [`RuntimeError::InvalidGeneration`];
    /// 3. terminal or already-cancelled fiber → `Ok` with `restored: None`
    ///    (zero action, not an error);
    /// 4. the ADR-0012 generation gate: the binding's current registered
    ///    incarnation must equal the resumer's expected one
    ///    ([`ChannelWaitError::StaleFiberIncarnation`] otherwise, zero side
    ///    effect);
    /// 5. the snapshot must exist for the binding
    ///    ([`ChannelWaitError::SnapshotUnavailable`] — nothing was recorded,
    ///    or the terminal GC consumed it; use the A path);
    /// 6. `resume_from_entry` re-executes the handler from its entry — a
    ///    rejection propagates as
    ///    [`ChannelWaitError::ResumeRejected`].
    ///
    /// # Errors
    ///
    /// Returns [`ChannelWaitError::Runtime`] for shutdown and stale/unknown
    /// fiber handles, [`ChannelWaitError::ProcessAuthority`] for the
    /// incarnation readback, [`ChannelWaitError::StaleFiberIncarnation`] for
    /// the generation gate, [`ChannelWaitError::SnapshotUnavailable`] for a
    /// missing snapshot, and [`ChannelWaitError::ResumeRejected`] when the
    /// re-execution fails.
    pub fn resume_from_snapshot(
        &self,
        handle: FiberHandle,
        process: &ProcessAuthority,
        snapshot: &impl SnapshotResumable,
    ) -> Result<SnapshotResumeReport, ChannelWaitError> {
        if self.inner.shutdown.load(Ordering::Acquire) {
            return Err(RuntimeError::ShuttingDown.into());
        }
        let record = self.record_for(handle)?;
        if is_terminal(*lock_unpoisoned(&record.state)) || record.scope.is_cancelled() {
            return Ok(SnapshotResumeReport {
                binding: snapshot.binding(),
                restored: None,
            });
        }

        let fiber = nlos_types::ExecutionFiberId::from_bytes(*snapshot.binding().as_bytes());
        process.inspect_active_process_binding(snapshot.process_id())?;
        let current = process.inspect_fiber_incarnation(snapshot.process_id(), fiber)?;
        if current.incarnation_generation != snapshot.expected_incarnation() {
            return Err(ChannelWaitError::StaleFiberIncarnation);
        }
        let restored = process
            .inspect_fiber_entry_snapshot(snapshot.process_id(), fiber)
            .map_err(|error| match error {
                nlos_process::ProcessAuthorityError::FiberSnapshotNotFound => {
                    ChannelWaitError::SnapshotUnavailable
                }
                other => ChannelWaitError::from(other),
            })?;
        snapshot.resume_from_entry(&restored.handler_input)?;
        Ok(SnapshotResumeReport {
            binding: snapshot.binding(),
            restored: Some(restored),
        })
    }

    /// Garbage-collects the binding's entry snapshot (the terminal GC of the
    /// latest-only retention policy). Returns whether a snapshot existed.
    /// Unlike the write/restore entries this deliberately does NOT gate on
    /// the fiber being live — GC is exactly what a terminal fiber's state
    /// calls for — but a stale or unknown handle is still rejected.
    ///
    /// # Errors
    ///
    /// Returns [`ChannelWaitError::Runtime`] for stale/unknown fiber handles
    /// and [`ChannelWaitError::ProcessAuthority`] for the durable delete.
    pub fn gc_handler_entry_snapshot(
        &self,
        handle: FiberHandle,
        process: &ProcessAuthority,
        snapshot: &impl SnapshotResumable,
    ) -> Result<bool, ChannelWaitError> {
        if self.inner.shutdown.load(Ordering::Acquire) {
            return Err(RuntimeError::ShuttingDown.into());
        }
        self.record_for(handle)?;
        let fiber = nlos_types::ExecutionFiberId::from_bytes(*snapshot.binding().as_bytes());
        Ok(process.gc_fiber_entry_snapshot(snapshot.process_id(), fiber)?)
    }
}
