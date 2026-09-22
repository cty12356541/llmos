//! Runtime-side platform-kill receipt consumption (B-PROCESS-003 §6/§7/§8
//! residual / W36 B-RUNTIME): [`TokioRuntimeAdapter::consume_platform_kill`]
//! reads the durable kill receipt `request_platform_kill` committed, then —
//! with the binding terminal — drives the W27-C batch-cancel linkage
//! ([`crate::TokioRuntimeAdapter::cancel_process_fibers`]) so the killed
//! process's live fibers reach the unique terminal `Cancelled` through
//! their cancellation scopes.
//!
//! Division of the fence (the W27-C family): the durable side owns the kill
//! evidence (the immutable receipt) and the incarnation invalidation; the
//! runtime side owns scope tree-cancel and the meter counters. One
//! consumption call performs receipt readback and linkage in that order,
//! fail-closed: a missing receipt leaves the linkage (and the runtime)
//! untouched, and a receipt whose binding is not yet terminal is rejected
//! by the W27-C durable gate with zero runtime side effect.

use std::sync::atomic::Ordering;

use nlos_process::{PlatformKillReceipt, ProcessAuthority, PropagateCancelToFibersRequest};

use crate::{ChannelWaitError, ProcessFiberCancelReport, TokioRuntimeAdapter};

/// The outcome of one [`TokioRuntimeAdapter::consume_platform_kill`] call:
/// the consumed durable kill receipt plus the W27-C sweep report the
/// consumption drove.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformKillConsumptionReport {
    /// The durable platform-kill receipt consumed by this call — the
    /// evidence that `request_platform_kill` signaled this exact
    /// `(process_id, process_generation)`.
    pub receipt: PlatformKillReceipt,
    /// The W27-C batch-cancel report driven by this consumption: the
    /// durable propagation decision plus the runtime-side sweep counts for
    /// the killed process's fibers.
    pub cancel: ProcessFiberCancelReport,
}

impl TokioRuntimeAdapter {
    /// Consumes the durable platform-kill receipt for `request`'s fenced
    /// `(process_id, expected_process_generation)` and — with the binding
    /// terminal — cancels that process's scopes through the W27-C path
    /// ([`Self::cancel_process_fibers`]).
    ///
    /// Gate order, fail-closed:
    ///
    /// 1. **receipt readback** —
    ///    [`ProcessAuthority::inspect_platform_kill_receipt`] scoped to the
    ///    presented `(process_id, expected_process_generation)`; no receipt
    ///    means no kill evidence, so the call fails with
    ///    [`ChannelWaitError::PlatformKillReceiptAbsent`] before any
    ///    durable propagation or runtime side effect;
    /// 2. **W27-C linkage** — `cancel_process_fibers` consumes the durable
    ///    cancel propagation (which requires the binding to be terminal at
    ///    the presented fence) and drives the runtime's scope tree-cancel;
    ///    every one of its rejection modes (non-terminal binding, stale
    ///    fence, idempotency rebinding) leaves the runtime untouched;
    /// 3. **meter linkage** — the consumption feeds the lock-free
    ///    [`crate::RuntimeHealth`] counters (`platform_kills_consumed_total`
    ///    here; the sweep counters inside the W27-C path), and the killed
    ///    fibers' own [`nlos_runtime::ActivationUsage`] dimensions close
    ///    through the existing terminal finalize seams.
    ///
    /// Consumption is idempotent in effect (the receipt is immutable, the
    /// propagation replays, scope cancel is idempotent), but every
    /// successful call meters again — the counters meter the linkage path,
    /// not the durable ledger.
    ///
    /// # Errors
    ///
    /// Returns [`ChannelWaitError::PlatformKillReceiptAbsent`] when no kill
    /// receipt exists for the presented fence (zero side effect),
    /// [`ChannelWaitError::ProcessAuthority`] when the readback or the W27-C
    /// durable gate fails (zero runtime side effect).
    pub fn consume_platform_kill(
        &self,
        process: &ProcessAuthority,
        request: PropagateCancelToFibersRequest,
    ) -> Result<PlatformKillConsumptionReport, ChannelWaitError> {
        // Fail-closed receipt gate first: without kill evidence the runtime
        // never touches the durable propagation or the scope registry.
        let receipt = process
            .inspect_platform_kill_receipt(request.process_id, request.expected_process_generation)?
            .ok_or(ChannelWaitError::PlatformKillReceiptAbsent {
                process_id: request.process_id,
            })?;

        // Binding terminal ⇒ runtime cancels the killed process's scopes
        // through the W27-C path (its own fail-closed durable gate).
        let cancel = self.cancel_process_fibers(process, request)?;

        self.inner
            .platform_kills_consumed
            .fetch_add(1, Ordering::Relaxed);

        Ok(PlatformKillConsumptionReport { receipt, cancel })
    }
}
