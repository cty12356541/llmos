//! Deterministic socket / TLS failure classification into
//! [`nlos_cell::partition::RemoteCallOutcome`] and blind-retry reconciliation
//! guard (`[DIST-FAIL-001]`, `ADR-0026` §2.4).
//!
//! Transport errors are NEVER collapsed into untyped string failures: whether a
//! remote side effect may have executed depends strictly on
//! `(WriteProgressPhase, RemoteCallContract.side_effecting)`.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Mutex, PoisonError};

use nlos_cell::{
    CellEpoch, CellIdentity, RemoteCallContract, RemoteCallOutcome, RemoteRetryRefusal,
};
use nlos_types::IdempotencyKey;

/// Exact boundary tracking whether any byte of the business request frame has
/// entered the TLS/socket write path (`ADR-0026` §2.4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteProgressPhase {
    /// Zero bytes of the business request frame were handed to the socket/TLS
    /// stream (e.g. unresolved route, `SingleHostDesktop` block, TCP connect
    /// refusal/timeout, mTLS rejection, or 5-axis Exporter handshake failure).
    BeforeFirstPayloadByte,
    /// Request frame transmission entered the TLS/TCP write path or completed
    /// before response receipt; remote execution CANNOT be ruled out.
    AfterFirstPayloadByte,
}

/// Structured wire-level failure category observed on the cross-host socket.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketFailureKind {
    /// `ClusterProfile::SingleHostDesktop` (`N = 1`) refused network dial.
    SingleHostProfileDisabled,
    /// `EndpointResolver` had no routing hint for the target `CellIdentity`.
    EndpointUnresolved,
    /// TCP `connect()` failed (`ECONNREFUSED`, `ENETUNREACH`, `EHOSTUNREACH`,
    /// `EADDRNOTAVAIL`).
    ConnectRefusedOrUnreachable,
    /// TCP `connect()` timed out before SYN-ACK completion.
    ConnectTimeout,
    /// TLS 1.3 mTLS handshake or X.509 principal verification failed.
    TlsHandshakeRejected,
    /// 5-axis TLS Exporter `CellFence` challenge-response failed closed.
    PeerFenceHandshakeRejected,
    /// Socket/TLS write timed out.
    WriteTimeout,
    /// Socket/TLS read timed out (including TCP half-open silent blackhole).
    ReadTimeout,
    /// Peer or middlebox sent TCP `RST` / `ECONNRESET` / `WSAECONNRESET` /
    /// `BrokenPipe` / `ConnectionAborted`.
    ConnectionReset,
    /// Peer closed TCP stream abruptly (`UnexpectedEof` or TLS truncation
    /// without `close_notify`).
    UnexpectedEofOrTlsTruncation,
    /// Other `std::io::ErrorKind` surfaced by the OS network stack.
    TransportIoOther(io::ErrorKind),
}

impl SocketFailureKind {
    /// Maps a `std::io::Error` encountered after connection establishment into
    /// its deterministic [`SocketFailureKind`].
    #[must_use]
    pub fn from_stream_io_error(error: &io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe => Self::ConnectionReset,
            io::ErrorKind::UnexpectedEof => Self::UnexpectedEofOrTlsTruncation,
            io::ErrorKind::TimedOut => Self::ReadTimeout,
            io::ErrorKind::ConnectionRefused
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::AddrNotAvailable => Self::ConnectRefusedOrUnreachable,
            other => Self::TransportIoOther(other),
        }
    }
}

/// Deterministically classifies a cross-host socket/TLS failure into
/// [`RemoteCallOutcome`] (`[DIST-FAIL-001]`, `ADR-0026` §2.4 table).
///
/// - `BeforeFirstPayloadByte` -> ALWAYS [`RemoteCallOutcome::Unavailable`]
///   (`SafeToRetryWithSameKey` within retry budget, else
///   `DoNotRetryBudgetExhausted`).
/// - `AfterFirstPayloadByte` + `side_effecting == false` ->
///   [`RemoteCallOutcome::DeadlineExceededUncertain`] with
///   `side_effecting: false` (`SafeToRetryWithSameKey` within retry budget).
/// - `AfterFirstPayloadByte` + `side_effecting == true` ->
///   [`RemoteCallOutcome::DeadlineExceededUncertain`] with
///   `side_effecting: true` and MANDATORY
///   [`RemoteRetryDirective::ReconcileBeforeRetry`].
#[must_use]
pub const fn classify_transport_failure<T>(
    contract: &RemoteCallContract,
    phase: WriteProgressPhase,
    _kind: SocketFailureKind,
    elapsed_ticks: u64,
) -> RemoteCallOutcome<T> {
    match phase {
        WriteProgressPhase::BeforeFirstPayloadByte => contract.classify_pre_dispatch_unavailable(),
        WriteProgressPhase::AfterFirstPayloadByte => {
            contract.classify_inflight_timeout(elapsed_ticks)
        }
    }
}

/// Client-side reconciliation fence tracker enforcing that any `IdempotencyKey`
/// whose side-effecting call entered `DeadlineExceededUncertain` (`ReconcileBeforeRetry`)
/// CANNOT be retried blindly until explicitly reconciled (`[DIST-FAIL-001]`).
#[derive(Debug, Default)]
pub struct PendingReconciliationTracker {
    uncertain_keys: Mutex<BTreeMap<IdempotencyKey, SocketFailureKind>>,
}

impl PendingReconciliationTracker {
    /// Creates an empty reconciliation tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Checks whether `idempotency_key` is currently blocked awaiting
    /// reconciliation before dispatching a call attempt.
    ///
    /// # Errors
    ///
    /// Fails closed with
    /// [`RemoteRetryRefusal::UncertainSideEffectRequiresReconciliation`] if a
    /// prior side-effecting attempt on `idempotency_key` ended in an uncertain
    /// state and has not been reconciled.
    pub fn check_before_dispatch(
        &self,
        idempotency_key: IdempotencyKey,
    ) -> Result<(), RemoteRetryRefusal> {
        let guard = self
            .uncertain_keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if guard.contains_key(&idempotency_key) {
            return Err(
                RemoteRetryRefusal::UncertainSideEffectRequiresReconciliation { idempotency_key },
            );
        }
        Ok(())
    }

    /// Records an uncertain side-effecting outcome on `idempotency_key`.
    pub fn record_uncertain_side_effect(
        &self,
        idempotency_key: IdempotencyKey,
        failure_kind: SocketFailureKind,
    ) {
        let mut guard = self
            .uncertain_keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        guard.insert(idempotency_key, failure_kind);
    }

    /// Marks `idempotency_key` as reconciled after authoritative status query
    /// or epoch fence barrier convergence, re-enabling idempotent retry if
    /// needed.
    pub fn mark_reconciled(&self, idempotency_key: IdempotencyKey) -> bool {
        let mut guard = self
            .uncertain_keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        guard.remove(&idempotency_key).is_some()
    }

    /// Returns `true` if `idempotency_key` currently requires reconciliation.
    #[must_use]
    pub fn requires_reconciliation(&self, idempotency_key: IdempotencyKey) -> bool {
        let guard = self
            .uncertain_keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        guard.contains_key(&idempotency_key)
    }
}

/// Deterministic multi-shard fanout outcome aggregator (`ADR-0026` §2.4 row 4).
///
/// Sorts per-shard outcomes canonically by `CellIdentity` before feeding shard
/// status tags into [`RemoteCallContract::classify_partial`], producing an
/// order-independent domain-separated `SHA-256` `uncertainty_digest`
/// (`REMOTE_CALL_PARTIAL_UNCERTAINTY_DOMAIN`).
#[must_use]
pub fn aggregate_fanout_outcomes<T>(
    contract: &RemoteCallContract,
    mut shard_outcomes: Vec<(CellIdentity, RemoteCallOutcome<T>)>,
    shards_idempotent: bool,
) -> RemoteCallOutcome<Vec<(CellIdentity, T)>> {
    shard_outcomes.sort_by_key(|(cell, _)| *cell);

    let mut completed = Vec::new();
    let mut unavailable_count: u32 = 0;
    let mut uncertain_count: u32 = 0;
    let mut max_observed_epoch = CellEpoch::INITIAL;
    let mut any_replayed = false;
    let mut shard_evidence = Vec::with_capacity(shard_outcomes.len() * 21);

    for (cell, outcome) in shard_outcomes {
        shard_evidence.extend_from_slice(cell.as_bytes());
        match outcome {
            RemoteCallOutcome::Succeeded {
                value,
                observed_epoch,
                idempotency_replayed,
            } => {
                shard_evidence.push(0x01);
                shard_evidence.extend_from_slice(&observed_epoch.get().to_be_bytes()[4..]);
                if observed_epoch > max_observed_epoch {
                    max_observed_epoch = observed_epoch;
                }
                any_replayed |= idempotency_replayed;
                completed.push((cell, value));
            }
            RemoteCallOutcome::Unavailable { .. } => {
                unavailable_count = unavailable_count.saturating_add(1);
                shard_evidence.push(0x02);
                shard_evidence.extend_from_slice(&0u32.to_be_bytes());
            }
            RemoteCallOutcome::DeadlineExceededUncertain { side_effecting, .. } => {
                uncertain_count = uncertain_count.saturating_add(1);
                shard_evidence.push(if side_effecting { 0x03 } else { 0x04 });
                shard_evidence.extend_from_slice(&0u32.to_be_bytes());
            }
            RemoteCallOutcome::Partial {
                uncertainty_digest, ..
            } => {
                uncertain_count = uncertain_count.saturating_add(1);
                shard_evidence.push(0x05);
                shard_evidence.extend_from_slice(&uncertainty_digest[..4]);
            }
        }
    }

    let completed_shards = u32::try_from(completed.len()).unwrap_or(u32::MAX);
    let failed_shards = unavailable_count.saturating_add(uncertain_count);

    if failed_shards == 0 {
        return contract.classify_succeeded(completed, max_observed_epoch, any_replayed);
    }
    if completed_shards == 0 && uncertain_count == 0 {
        return contract.classify_pre_dispatch_unavailable();
    }

    // If any failed shard was an in-flight uncertain side-effecting call, force
    // non-idempotent classification so `ReconcileBeforeRetry` is attached.
    let effective_shards_idempotent =
        shards_idempotent && !(contract.side_effecting && uncertain_count > 0);
    contract.classify_partial(
        completed_shards,
        failed_shards,
        if completed.is_empty() {
            None
        } else {
            Some(completed)
        },
        effective_shards_idempotent,
        &shard_evidence,
    )
}
