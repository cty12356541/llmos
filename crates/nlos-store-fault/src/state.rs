//! Process-global fault state machine (pure safe code).
//!
//! VFS callbacks are global C function pointers and cannot carry a Rust
//! handle, so the armed mode lives in atomics, exactly like the screenpipe
//! precedent. All ordering is `Acquire`/`Release`; the shim only ever reads
//! or saturates these counters.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use crate::{FaultCode, FaultMode};

pub(crate) const KIND_DISABLED: u8 = 0;
pub(crate) const KIND_FAIL_WRITES: u8 = 1;
pub(crate) const KIND_POWER_LOSS: u8 = 2;

pub(crate) const CODE_IOERR: u8 = 1;
pub(crate) const CODE_FULL: u8 = 2;

static KIND: AtomicU8 = AtomicU8::new(KIND_DISABLED);
static CODE: AtomicU8 = AtomicU8::new(CODE_IOERR);
static REMAINING: AtomicU64 = AtomicU64::new(0);
pub(crate) static WRITES_OBSERVED: AtomicU64 = AtomicU64::new(0);

/// Outcome of one `xWrite` consult.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WriteDecision {
    /// Forward to the real `xWrite`.
    Passthrough,
    /// Do not write; return this hard `SQLite` error code to the caller.
    Fail(u8),
    /// Pretend success without touching disk (power loss).
    Drop,
}

pub(crate) fn arm(mode: FaultMode) {
    // Publish payload fields before the kind so a concurrent callback never
    // observes the new kind with a stale payload.
    match mode {
        FaultMode::Disabled => KIND.store(KIND_DISABLED, Ordering::Release),
        FaultMode::FailWritesAfter { remaining, code } => {
            let code = match code {
                FaultCode::IoErr => CODE_IOERR,
                FaultCode::Full => CODE_FULL,
            };
            CODE.store(code, Ordering::Relaxed);
            REMAINING.store(remaining, Ordering::Relaxed);
            WRITES_OBSERVED.store(0, Ordering::Relaxed);
            KIND.store(KIND_FAIL_WRITES, Ordering::Release);
        }
        FaultMode::PowerLossAfter { remaining } => {
            REMAINING.store(remaining, Ordering::Relaxed);
            WRITES_OBSERVED.store(0, Ordering::Relaxed);
            KIND.store(KIND_POWER_LOSS, Ordering::Release);
        }
    }
}

pub(crate) fn decide_write() -> WriteDecision {
    let kind = KIND.load(Ordering::Acquire);
    if kind == KIND_DISABLED {
        return WriteDecision::Passthrough;
    }
    WRITES_OBSERVED.fetch_add(1, Ordering::Relaxed);
    let mut remaining = REMAINING.load(Ordering::Acquire);
    loop {
        if remaining == 0 {
            return match kind {
                KIND_FAIL_WRITES => WriteDecision::Fail(CODE.load(Ordering::Relaxed)),
                _ => WriteDecision::Drop,
            };
        }
        match REMAINING.compare_exchange_weak(
            remaining,
            remaining - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return WriteDecision::Passthrough,
            Err(current) => remaining = current,
        }
    }
}

/// `xSync`/`xTruncate` are only intercepted after a simulated power loss;
/// write failures must surface at `xWrite` so `SQLite` reports them.
pub(crate) fn sync_or_truncate_dropped() -> bool {
    KIND.load(Ordering::Acquire) == KIND_POWER_LOSS && REMAINING.load(Ordering::Acquire) == 0
}
