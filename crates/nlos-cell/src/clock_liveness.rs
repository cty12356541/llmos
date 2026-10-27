//! Cross-host receiver-local monotonic liveness windows, link clock-skew
//! quarantine guards, and per-Cell monotonic checkpoint trail ordering
//! (`ADR-0026` §2.3, `ADR-0019` Trigger 7 #1 & #2, `[MODEL-TIME-001]`).
//!
//! Across distinct physical machines, OS wall clocks (`SystemTime::now()`)
//! cannot be assumed synchronized or comparable. This module closes the two
//! cross-host wall-clock traps identified in `ADR-0019` Trigger 7:
//! 1. **Heartbeat liveness (`Trigger 7 #1`)**: [`CrossHostHeartbeatEnvelope`]
//!    carries `(cell_id, node_boot_generation, epoch, heartbeat_seq,
//!    measured_rtt_ms_opt, audit_wall_hint_opt)` and is evaluated by
//!    [`ReceiverMonotonicLivenessTracker`] (and [`CellDirectory`] cross-host
//!    helpers) **exclusively** against the receiver's local monotonic arrival
//!    instant (`silence_ms = now_receiver_mono - last_received_at_mono`).
//!    Sender wall-clock hints ([`AuditWallTimestampHint`]) intentionally omit
//!    [`Ord`] and [`PartialOrd`] so they can never participate in liveness or
//!    ordering comparisons. Out-of-order or replayed `(node_boot_generation,
//!    epoch, heartbeat_seq)` watermarks are rejected fail-closed, and exact
//!    duplicate packets never refresh `last_received_at_mono`.
//!    Peer liveness remains a discovery heuristic ([`HeuristicPeerLiveness`]),
//!    **never** a fencing or lease-unlock oracle (`[LEASE-FENCE-001]`).
//! 2. **Link clock-skew & monotonic-regression quarantine**: if an observed
//!    handshake/RPC round-trip sample violates `rtt_ms / 2 <= max_clock_skew_ms`
//!    (`rtt_ms.div_ceil(2) > max_clock_skew_ms`) or if the local monotonic
//!    counter regresses, [`CrossHostLinkClockGuard`] and
//!    [`PartitionAutonomyGate::observe_cross_host_clock_sample`] automatically
//!    transition the control-plane link into fail-closed
//!    [`ControlPlaneLinkState::Partitioned`] (`ClockSkewQuarantined`).
//! 3. **Per-Cell monotonic checkpoint trails (`Trigger 7 #2`)**:
//!    [`MonotonicCheckpointTrail`] and [`CellDirectory`] cross-host checkpoint
//!    methods order and verify `ReconciliationCheckpoint` records strictly by
//!    `(CellIdentity, node_boot_generation, epoch, checkpoint_seq)` with
//!    hash-chained SHA-256 digests (`MONOTONIC_CHECKPOINT_CHAIN_DOMAIN`) and
//!    wall-clock-free filenames, eliminating any reliance on cross-machine
//!    incomparable `(recorded_ms, os_process_id, sequence)` timestamps.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::Write;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use nlos_types::{Generation, SchedulerDomainId};
use sha2::{Digest, Sha256};

use crate::{
    CellDirectory, CellEpoch, CellFence, CellFencingToken, CellIdentity, CheckpointFact,
    ControlPlaneLinkState, FederationError, PartitionAutonomyGate,
};

/// Domain separator for [`CrossHostHeartbeatEnvelope`] digests.
pub const CROSS_HOST_HEARTBEAT_DOMAIN: &[u8] = b"llmos/cell-cross-host-heartbeat/v1";

/// Domain separator for [`MonotonicCheckpointEntry`] hash-chain digests.
pub const MONOTONIC_CHECKPOINT_CHAIN_DOMAIN: &[u8] = b"llmos/cell-monotonic-checkpoint-chain/v1";

/// Diagnostic [`ControlPlaneLinkState::Partitioned`] reason code emitted when
/// `measured_rtt_ms / 2 > max_clock_skew_ms` violates the conservative skew
/// envelope (`ADR-0026` §2.3).
pub const REASON_CLOCK_SKEW_RTT_EXCEEDED: u16 = 0xC101;

/// Diagnostic [`ControlPlaneLinkState::Partitioned`] reason code emitted when a
/// local monotonic clock reading regresses below a previously observed tick.
pub const REASON_LOCAL_MONOTONIC_REGRESSION: u16 = 0xC102;

const MONOTONIC_CHECKPOINTS_DIR: &str = "monotonic-checkpoints";
const MONOTONIC_CHECKPOINT_EXT: &str = "mchk";
const MAX_ROUTING_SUMMARY_BYTES: usize = 512;

static STAGING_NONCE: AtomicU64 = AtomicU64::new(1);

/// Receiver-local monotonic instant in milliseconds (`[MODEL-TIME-001]`).
///
/// Values come strictly from the local machine's monotonic clock (never from a
/// remote peer's `SystemTime` or heartbeat payload).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MonotonicRecvInstantMs(u64);

impl MonotonicRecvInstantMs {
    /// Wraps a local monotonic millisecond tick.
    #[must_use]
    pub const fn new(ms: u64) -> Self {
        Self(ms)
    }

    /// Raw local monotonic millisecond tick.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Computes `self - earlier`, failing closed if the local monotonic clock
    /// regressed (`self < earlier`).
    ///
    /// # Errors
    ///
    /// Returns [`ClockLivenessError::LocalMonotonicRegression`] when
    /// `self < earlier`.
    pub const fn checked_elapsed_since(self, earlier: Self) -> Result<u64, ClockLivenessError> {
        if self.0 < earlier.0 {
            return Err(ClockLivenessError::LocalMonotonicRegression {
                previous_mono_ms: earlier.0,
                observed_mono_ms: self.0,
            });
        }
        Ok(self.0 - earlier.0)
    }
}

/// Opaque sender wall-clock hint carried solely for human audit rendering
/// (`[MODEL-TIME-001]`).
///
/// Deliberately omits [`Ord`] and [`PartialOrd`] so cross-host code cannot
/// order events, judge liveness, or compute expirations from remote wall-clock
/// timestamps.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AuditWallTimestampHint(u64);

impl AuditWallTimestampHint {
    /// Wraps an opaque Unix-epoch millisecond hint for audit logs only.
    #[must_use]
    pub const fn new(unix_epoch_ms: u64) -> Self {
        Self(unix_epoch_ms)
    }

    /// Extracts the raw audit display hint (`u64` ms since Unix epoch).
    #[must_use]
    pub const fn audit_display_ms(self) -> u64 {
        self.0
    }
}

/// Strictly ordered per-Cell heartbeat watermark `(node_boot_generation, epoch,
/// heartbeat_seq)` (`ADR-0019` Trigger 7 #1).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CrossHostHeartbeatWatermark {
    /// Sender Cell boot generation.
    pub node_boot_generation: Generation,
    /// Sender Cell control-plane epoch.
    pub epoch: CellEpoch,
    /// Strictly increasing per-`(boot_generation, epoch)` heartbeat sequence.
    pub heartbeat_seq: NonZeroU64,
}

impl CrossHostHeartbeatWatermark {
    /// Builds a watermark from `(node_boot_generation, epoch, heartbeat_seq)`.
    #[must_use]
    pub const fn new(
        node_boot_generation: Generation,
        epoch: CellEpoch,
        heartbeat_seq: NonZeroU64,
    ) -> Self {
        Self {
            node_boot_generation,
            epoch,
            heartbeat_seq,
        }
    }
}

/// Cross-host heartbeat wire envelope evaluated exclusively against
/// receiver-local monotonic silence windows (`ADR-0026` §2.3, `ADR-0019`
/// Trigger 7 #1).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CrossHostHeartbeatEnvelope {
    cell_id: CellIdentity,
    watermark: CrossHostHeartbeatWatermark,
    fencing_token: CellFencingToken,
    measured_rtt_ms_opt: Option<u64>,
    audit_wall_hint_opt: Option<AuditWallTimestampHint>,
    routing_summary: String,
    envelope_digest: [u8; 32],
}

impl CrossHostHeartbeatEnvelope {
    /// Constructs a canonical, digest-bound [`CrossHostHeartbeatEnvelope`].
    ///
    /// # Errors
    ///
    /// Returns [`ClockLivenessError::MalformedHeartbeatEnvelope`] if
    /// `routing_summary` is empty, contains control/newline characters, or
    /// exceeds `MAX_ROUTING_SUMMARY_BYTES`.
    pub fn new(
        cell_id: CellIdentity,
        watermark: CrossHostHeartbeatWatermark,
        fencing_token: CellFencingToken,
        measured_rtt_ms_opt: Option<u64>,
        audit_wall_hint_opt: Option<AuditWallTimestampHint>,
        routing_summary: impl Into<String>,
    ) -> Result<Self, ClockLivenessError> {
        let routing_summary = routing_summary.into();
        validate_single_line(&routing_summary, MAX_ROUTING_SUMMARY_BYTES).ok_or(
            ClockLivenessError::MalformedHeartbeatEnvelope("invalid routing_summary"),
        )?;
        let envelope_digest = Self::compute_digest(
            cell_id,
            watermark,
            fencing_token,
            measured_rtt_ms_opt,
            audit_wall_hint_opt,
            &routing_summary,
        );
        Ok(Self {
            cell_id,
            watermark,
            fencing_token,
            measured_rtt_ms_opt,
            audit_wall_hint_opt,
            routing_summary,
            envelope_digest,
        })
    }

    /// Returns a copy with an overridden digest (for testing tamper rejection).
    #[must_use]
    pub fn with_digest(mut self, envelope_digest: [u8; 32]) -> Self {
        self.envelope_digest = envelope_digest;
        self
    }

    /// Computes the domain-separated SHA-256 digest of all envelope fields.
    #[must_use]
    pub fn compute_digest(
        cell_id: CellIdentity,
        watermark: CrossHostHeartbeatWatermark,
        fencing_token: CellFencingToken,
        measured_rtt_ms_opt: Option<u64>,
        audit_wall_hint_opt: Option<AuditWallTimestampHint>,
        routing_summary: &str,
    ) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(CROSS_HOST_HEARTBEAT_DOMAIN);
        hasher.update([0x00]);
        hasher.update(cell_id.as_bytes());
        hasher.update(watermark.node_boot_generation.get().to_le_bytes());
        hasher.update(watermark.epoch.get().to_le_bytes());
        hasher.update(watermark.heartbeat_seq.get().to_le_bytes());
        hasher.update(fencing_token.get().to_le_bytes());
        match measured_rtt_ms_opt {
            Some(rtt_ms) => {
                hasher.update([1u8]);
                hasher.update(rtt_ms.to_le_bytes());
            }
            None => hasher.update([0u8]),
        }
        match audit_wall_hint_opt {
            Some(hint) => {
                hasher.update([1u8]);
                hasher.update(hint.audit_display_ms().to_le_bytes());
            }
            None => hasher.update([0u8]),
        }
        let summary_bytes = routing_summary.as_bytes();
        let summary_len = u64::try_from(summary_bytes.len()).unwrap_or(u64::MAX);
        hasher.update(summary_len.to_le_bytes());
        hasher.update(summary_bytes);
        hasher.finalize().into()
    }

    /// Verifies the envelope's single-line bounds and SHA-256 digest.
    #[must_use]
    pub fn verify_integrity(&self) -> bool {
        if validate_single_line(&self.routing_summary, MAX_ROUTING_SUMMARY_BYTES).is_none() {
            return false;
        }
        let expected = Self::compute_digest(
            self.cell_id,
            self.watermark,
            self.fencing_token,
            self.measured_rtt_ms_opt,
            self.audit_wall_hint_opt,
            &self.routing_summary,
        );
        self.envelope_digest == expected
    }

    /// Sender Cell identity.
    #[must_use]
    pub const fn cell_id(&self) -> CellIdentity {
        self.cell_id
    }

    /// Monotonic `(node_boot_generation, epoch, heartbeat_seq)` watermark.
    #[must_use]
    pub const fn watermark(&self) -> CrossHostHeartbeatWatermark {
        self.watermark
    }

    /// Sender boot generation.
    #[must_use]
    pub const fn node_boot_generation(&self) -> Generation {
        self.watermark.node_boot_generation
    }

    /// Sender Cell epoch.
    #[must_use]
    pub const fn epoch(&self) -> CellEpoch {
        self.watermark.epoch
    }

    /// Sender heartbeat sequence number.
    #[must_use]
    pub const fn heartbeat_seq(&self) -> NonZeroU64 {
        self.watermark.heartbeat_seq
    }

    /// Sender fencing token carried on the heartbeat.
    #[must_use]
    pub const fn fencing_token(&self) -> CellFencingToken {
        self.fencing_token
    }

    /// Optional measured round-trip time in milliseconds observed on the link.
    #[must_use]
    pub const fn measured_rtt_ms(&self) -> Option<u64> {
        self.measured_rtt_ms_opt
    }

    /// Optional opaque wall-clock hint for human audit logs only.
    #[must_use]
    pub const fn audit_wall_hint(&self) -> Option<AuditWallTimestampHint> {
        self.audit_wall_hint_opt
    }

    /// Routing summary advertised by the sender.
    #[must_use]
    pub fn routing_summary(&self) -> &str {
        &self.routing_summary
    }

    /// Canonical SHA-256 envelope digest.
    #[must_use]
    pub const fn envelope_digest(&self) -> [u8; 32] {
        self.envelope_digest
    }
}

/// Discovery-only peer liveness judgment derived from receiver-local monotonic
/// silence windows (`[LEASE-FENCE-001]`, `ADR-0026` §2.3).
///
/// Explicitly named `Heuristic*` because heartbeat silence or freshness is a
/// routing/discovery hint and MUST NEVER be used alone to unlock quarantined
/// leases or skip fencing tokens.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeuristicPeerLiveness {
    /// Latest valid advancing heartbeat arrived within `max_silence_ms` on the
    /// receiver's local monotonic clock.
    HeuristicAlive {
        /// Receiver-local monotonic silence elapsed since the last advancing
        /// heartbeat.
        silence_ms: u64,
    },
    /// Receiver-local monotonic silence exceeded `max_silence_ms`.
    HeuristicSuspectSilence {
        /// Receiver-local monotonic silence elapsed since the last advancing
        /// heartbeat.
        silence_ms: u64,
        /// Configured silence threshold.
        max_silence_ms: u64,
    },
    /// Peer link violated `rtt_ms / 2 <= max_clock_skew_ms` and is quarantined
    /// for conservative clock-skew breach.
    HeuristicClockSkewQuarantined {
        /// Offending round-trip measurement in milliseconds.
        measured_rtt_ms: u64,
        /// Configured `max_clock_skew_ms`.
        max_clock_skew_ms: NonZeroU64,
    },
}

impl HeuristicPeerLiveness {
    /// Returns `true` only when the discovery heuristic considers the peer
    /// reachable within its silence and skew envelopes.
    #[must_use]
    pub const fn is_heuristic_alive(&self) -> bool {
        matches!(self, Self::HeuristicAlive { .. })
    }
}

///Tracked receiver-local state for one peer Cell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackedPeerHeartbeatState {
    cell_id: CellIdentity,
    high_watermark: CrossHostHeartbeatWatermark,
    fencing_token: CellFencingToken,
    last_received_at_mono: MonotonicRecvInstantMs,
    last_measured_rtt_ms: Option<u64>,
    last_audit_wall_hint: Option<AuditWallTimestampHint>,
    routing_summary: String,
    last_envelope_digest: [u8; 32],
    skew_quarantined_rtt_ms: Option<u64>,
}

impl TrackedPeerHeartbeatState {
    /// Peer Cell identity.
    #[must_use]
    pub const fn cell_id(&self) -> CellIdentity {
        self.cell_id
    }

    /// Highest accepted `(node_boot_generation, epoch, heartbeat_seq)`.
    #[must_use]
    pub const fn high_watermark(&self) -> CrossHostHeartbeatWatermark {
        self.high_watermark
    }

    /// Latest fencing token bound to `high_watermark`.
    #[must_use]
    pub const fn fencing_token(&self) -> CellFencingToken {
        self.fencing_token
    }

    /// Receiver-local monotonic instant when `high_watermark` was accepted.
    #[must_use]
    pub const fn last_received_at_mono(&self) -> MonotonicRecvInstantMs {
        self.last_received_at_mono
    }

    /// Latest measured RTT in milliseconds, if supplied.
    #[must_use]
    pub const fn last_measured_rtt_ms(&self) -> Option<u64> {
        self.last_measured_rtt_ms
    }

    /// Latest opaque sender wall-clock hint (audit-only).
    #[must_use]
    pub const fn last_audit_wall_hint(&self) -> Option<AuditWallTimestampHint> {
        self.last_audit_wall_hint
    }

    /// Latest routing summary advertised by the peer.
    #[must_use]
    pub fn routing_summary(&self) -> &str {
        &self.routing_summary
    }
}

/// Outcome of processing a [`CrossHostHeartbeatEnvelope`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrossHostHeartbeatDecision {
    /// Strictly newer watermark accepted and `last_received_at_mono` updated.
    Accepted {
        /// Accepted watermark.
        watermark: CrossHostHeartbeatWatermark,
        /// Receiver-local monotonic arrival instant recorded.
        received_at_mono: MonotonicRecvInstantMs,
    },
    /// Byte-identical replay of the current `high_watermark`; acknowledged
    /// idempotently **without** refreshing `last_received_at_mono` so replayed
    /// packets cannot mask a silent peer.
    DuplicateIgnored {
        /// Current watermark on record.
        watermark: CrossHostHeartbeatWatermark,
        /// Original receiver-local arrival instant preserved unchanged.
        preserved_received_at_mono: MonotonicRecvInstantMs,
    },
}

/// Fail-closed errors across cross-host heartbeat evaluation, link clock
/// monitoring, and monotonic checkpoint trails.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClockLivenessError {
    /// Local monotonic clock regressed below a previously observed instant.
    LocalMonotonicRegression {
        /// Previously observed local monotonic ms.
        previous_mono_ms: u64,
        /// Regressed local monotonic ms supplied by caller.
        observed_mono_ms: u64,
    },
    /// Heartbeat envelope failed structural or SHA-256 integrity checks.
    MalformedHeartbeatEnvelope(&'static str),
    /// Heartbeat `(node_boot_generation, epoch, heartbeat_seq)` regressed or
    /// replayed with conflicting payload.
    StaleOrReplayedWatermark {
        /// Peer Cell identity.
        cell_id: CellIdentity,
        /// Highest accepted watermark on the receiver.
        committed_watermark: CrossHostHeartbeatWatermark,
        /// Rejected incoming watermark.
        presented_watermark: CrossHostHeartbeatWatermark,
    },
    /// Measured link RTT violated `rtt_ms / 2 <= max_clock_skew_ms`.
    ClockSkewRttExceeded {
        /// Peer or link RTT in milliseconds.
        measured_rtt_ms: u64,
        /// Configured conservative `max_clock_skew_ms`.
        max_clock_skew_ms: NonZeroU64,
    },
    /// Per-Cell monotonic checkpoint coordinate regressed, skipped a sequence
    /// number within the same `(boot_generation, epoch)`, or conflicted with an
    /// already committed entry.
    CheckpointOrderViolation {
        /// Owning Cell identity.
        cell: CellIdentity,
        /// Diagnostic reason.
        reason: &'static str,
    },
    /// Per-Cell monotonic checkpoint file or SHA-256 chain failed verification.
    CheckpointChainTampered {
        /// Owning Cell identity.
        cell: CellIdentity,
        /// Diagnostic reason.
        reason: &'static str,
    },
    /// Underlying federation directory I/O or validation error.
    Federation(FederationError),
}

impl From<FederationError> for ClockLivenessError {
    fn from(error: FederationError) -> Self {
        Self::Federation(error)
    }
}

impl fmt::Display for ClockLivenessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LocalMonotonicRegression {
                previous_mono_ms,
                observed_mono_ms,
            } => write!(
                formatter,
                "receiver local monotonic clock regressed: observed {observed_mono_ms}ms < previous {previous_mono_ms}ms"
            ),
            Self::MalformedHeartbeatEnvelope(reason) => {
                write!(formatter, "malformed CrossHostHeartbeatEnvelope: {reason}")
            }
            Self::StaleOrReplayedWatermark {
                cell_id,
                committed_watermark,
                presented_watermark,
            } => write!(
                formatter,
                "stale or conflicting CrossHostHeartbeatEnvelope for {cell_id:?}: presented {presented_watermark:?} <= committed {committed_watermark:?}"
            ),
            Self::ClockSkewRttExceeded {
                measured_rtt_ms,
                max_clock_skew_ms,
            } => write!(
                formatter,
                "cross-host RTT {measured_rtt_ms}ms violates rtt/2 <= max_clock_skew_ms ({}ms)",
                max_clock_skew_ms.get()
            ),
            Self::CheckpointOrderViolation { cell, reason } => write!(
                formatter,
                "monotonic checkpoint order violation for {cell:?}: {reason}"
            ),
            Self::CheckpointChainTampered { cell, reason } => write!(
                formatter,
                "monotonic checkpoint chain tampered for {cell:?}: {reason}"
            ),
            Self::Federation(error) => write!(formatter, "federation directory error: {error}"),
        }
    }
}

impl Error for ClockLivenessError {}

/// Checks the conservative half-RTT skew bound `rtt_ms / 2 <= max_clock_skew_ms`
/// using exact ceiling division (`rtt_ms.div_ceil(2) <= max_clock_skew_ms.get()`,
/// algebraically equivalent to `rtt_ms <= 2 * max_clock_skew_ms.get()` without
/// odd-millisecond truncation).
#[must_use]
pub const fn rtt_exceeds_max_clock_skew(
    measured_rtt_ms: u64,
    max_clock_skew_ms: NonZeroU64,
) -> bool {
    measured_rtt_ms.div_ceil(2) > max_clock_skew_ms.get()
}

/// Receiver-local monotonic liveness tracker for cross-host Cell peers
/// (`ADR-0026` §2.3, `ADR-0019` Trigger 7 #1).
#[derive(Clone, Debug)]
pub struct ReceiverMonotonicLivenessTracker {
    max_silence_ms: u64,
    max_clock_skew_ms: NonZeroU64,
    last_receiver_mono: Option<MonotonicRecvInstantMs>,
    peers: BTreeMap<CellIdentity, TrackedPeerHeartbeatState>,
}

impl ReceiverMonotonicLivenessTracker {
    /// Creates a new tracker parameterized by receiver-local `max_silence_ms`
    /// and conservative `max_clock_skew_ms`.
    #[must_use]
    pub const fn new(max_silence_ms: u64, max_clock_skew_ms: NonZeroU64) -> Self {
        Self {
            max_silence_ms,
            max_clock_skew_ms,
            last_receiver_mono: None,
            peers: BTreeMap::new(),
        }
    }

    /// Configured receiver-local silence threshold in milliseconds.
    #[must_use]
    pub const fn max_silence_ms(&self) -> u64 {
        self.max_silence_ms
    }

    /// Configured conservative `max_clock_skew_ms`.
    #[must_use]
    pub const fn max_clock_skew_ms(&self) -> NonZeroU64 {
        self.max_clock_skew_ms
    }

    /// Highest receiver-local monotonic timestamp observed so far.
    #[must_use]
    pub const fn last_receiver_mono(&self) -> Option<MonotonicRecvInstantMs> {
        self.last_receiver_mono
    }

    /// Looks up tracked state for `cell_id`.
    #[must_use]
    pub fn peer_state(&self, cell_id: CellIdentity) -> Option<&TrackedPeerHeartbeatState> {
        self.peers.get(&cell_id)
    }

    fn advance_receiver_mono(
        &mut self,
        now_receiver_mono: MonotonicRecvInstantMs,
    ) -> Result<(), ClockLivenessError> {
        if let Some(prev) = self.last_receiver_mono {
            let _ = now_receiver_mono.checked_elapsed_since(prev)?;
        }
        self.last_receiver_mono = Some(now_receiver_mono);
        Ok(())
    }

    /// Processes an incoming [`CrossHostHeartbeatEnvelope`] stamped with the
    /// receiver's local monotonic arrival instant `received_at_mono`.
    ///
    /// Sender `audit_wall_hint` is stored verifiably for audit rendering only
    /// and is **never** read for ordering or silence calculations.
    ///
    /// # Errors
    ///
    /// Fails closed on receiver monotonic regression, corrupted envelope
    /// digest, stale/conflicting `(node_boot_generation, epoch, heartbeat_seq)`
    /// watermark, or `measured_rtt_ms / 2 > max_clock_skew_ms` (after marking
    /// the peer `HeuristicClockSkewQuarantined`).
    pub fn observe_heartbeat(
        &mut self,
        envelope: &CrossHostHeartbeatEnvelope,
        received_at_mono: MonotonicRecvInstantMs,
    ) -> Result<CrossHostHeartbeatDecision, ClockLivenessError> {
        self.advance_receiver_mono(received_at_mono)?;
        if !envelope.verify_integrity() {
            return Err(ClockLivenessError::MalformedHeartbeatEnvelope(
                "envelope digest or routing_summary verification failed",
            ));
        }

        if let Some(existing) = self.peers.get(&envelope.cell_id) {
            if envelope.watermark == existing.high_watermark {
                if envelope.envelope_digest == existing.last_envelope_digest {
                    return Ok(CrossHostHeartbeatDecision::DuplicateIgnored {
                        watermark: existing.high_watermark,
                        preserved_received_at_mono: existing.last_received_at_mono,
                    });
                }
                return Err(ClockLivenessError::StaleOrReplayedWatermark {
                    cell_id: envelope.cell_id,
                    committed_watermark: existing.high_watermark,
                    presented_watermark: envelope.watermark,
                });
            }
            if envelope.watermark < existing.high_watermark {
                return Err(ClockLivenessError::StaleOrReplayedWatermark {
                    cell_id: envelope.cell_id,
                    committed_watermark: existing.high_watermark,
                    presented_watermark: envelope.watermark,
                });
            }
            if envelope.node_boot_generation() == existing.high_watermark.node_boot_generation
                && envelope.epoch() == existing.high_watermark.epoch
                && envelope.fencing_token < existing.fencing_token
            {
                return Err(ClockLivenessError::MalformedHeartbeatEnvelope(
                    "fencing_token regressed within identical (boot_generation, epoch)",
                ));
            }
        }

        let skew_violation_rtt = envelope
            .measured_rtt_ms_opt
            .filter(|&rtt| rtt_exceeds_max_clock_skew(rtt, self.max_clock_skew_ms));

        let next_state = TrackedPeerHeartbeatState {
            cell_id: envelope.cell_id,
            high_watermark: envelope.watermark,
            fencing_token: envelope.fencing_token,
            last_received_at_mono: received_at_mono,
            last_measured_rtt_ms: envelope.measured_rtt_ms_opt,
            last_audit_wall_hint: envelope.audit_wall_hint_opt,
            routing_summary: envelope.routing_summary.clone(),
            last_envelope_digest: envelope.envelope_digest,
            skew_quarantined_rtt_ms: skew_violation_rtt,
        };
        self.peers.insert(envelope.cell_id, next_state);

        if let Some(measured_rtt_ms) = skew_violation_rtt {
            return Err(ClockLivenessError::ClockSkewRttExceeded {
                measured_rtt_ms,
                max_clock_skew_ms: self.max_clock_skew_ms,
            });
        }

        Ok(CrossHostHeartbeatDecision::Accepted {
            watermark: envelope.watermark,
            received_at_mono,
        })
    }

    /// Evaluates discovery-only [`HeuristicPeerLiveness`] for `cell_id` at
    /// `now_receiver_mono` without consulting any wall clock.
    ///
    /// # Errors
    ///
    /// Fails closed with [`ClockLivenessError::LocalMonotonicRegression`] if
    /// `now_receiver_mono` regresses below the receiver's previously observed
    /// monotonic watermark.
    pub fn evaluate_peer(
        &mut self,
        cell_id: CellIdentity,
        now_receiver_mono: MonotonicRecvInstantMs,
    ) -> Result<Option<HeuristicPeerLiveness>, ClockLivenessError> {
        self.advance_receiver_mono(now_receiver_mono)?;
        let Some(peer) = self.peers.get(&cell_id) else {
            return Ok(None);
        };
        if let Some(measured_rtt_ms) = peer.skew_quarantined_rtt_ms {
            return Ok(Some(HeuristicPeerLiveness::HeuristicClockSkewQuarantined {
                measured_rtt_ms,
                max_clock_skew_ms: self.max_clock_skew_ms,
            }));
        }
        let silence_ms = now_receiver_mono.checked_elapsed_since(peer.last_received_at_mono)?;
        if silence_ms <= self.max_silence_ms {
            Ok(Some(HeuristicPeerLiveness::HeuristicAlive { silence_ms }))
        } else {
            Ok(Some(HeuristicPeerLiveness::HeuristicSuspectSilence {
                silence_ms,
                max_silence_ms: self.max_silence_ms,
            }))
        }
    }

    /// Enumerates all tracked peers whose discovery-heuristic status is
    /// [`HeuristicPeerLiveness::HeuristicAlive`] at `now_receiver_mono`, sorted
    /// deterministically by [`CellIdentity`].
    ///
    /// # Errors
    ///
    /// Fails closed on receiver-local monotonic regression.
    pub fn heuristic_live_peers(
        &mut self,
        now_receiver_mono: MonotonicRecvInstantMs,
    ) -> Result<Vec<TrackedPeerHeartbeatState>, ClockLivenessError> {
        self.advance_receiver_mono(now_receiver_mono)?;
        let mut alive = Vec::new();
        for peer in self.peers.values() {
            if peer.skew_quarantined_rtt_ms.is_some() {
                continue;
            }
            let silence_ms = now_receiver_mono.checked_elapsed_since(peer.last_received_at_mono)?;
            if silence_ms <= self.max_silence_ms {
                alive.push(peer.clone());
            }
        }
        Ok(alive)
    }
}

/// Clock-health state of a cross-host control-plane link (`ADR-0026` §2.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkClockHealthState {
    /// Link RTT and local monotonic watermarks are within the conservative skew
    /// envelope.
    Healthy {
        /// Configured conservative `max_clock_skew_ms`.
        max_clock_skew_ms: NonZeroU64,
        /// Highest local monotonic tick observed.
        last_mono_tick: u64,
        /// Latest measured RTT in milliseconds, if any.
        last_rtt_ms: Option<u64>,
    },
    /// Link exceeded `rtt_ms / 2 <= max_clock_skew_ms` and was quarantined into
    /// fail-closed `Partitioned`.
    ClockSkewQuarantined {
        /// Offending RTT measurement in milliseconds.
        measured_rtt_ms: u64,
        /// Configured conservative `max_clock_skew_ms`.
        max_clock_skew_ms: NonZeroU64,
        /// Local monotonic tick at which quarantine triggered.
        quarantined_at_tick: u64,
    },
    /// Local monotonic clock regressed and the link was quarantined into
    /// fail-closed `Partitioned`.
    MonotonicRegressionQuarantined {
        /// Previously observed local monotonic tick.
        previous_tick: u64,
        /// Regressed local monotonic tick.
        regressed_tick: u64,
    },
}

/// Fail-closed violation returned when [`PartitionAutonomyGate`] or
/// [`CrossHostLinkClockGuard`] quarantines a link on clock skew or monotonic
/// regression.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkClockViolation {
    /// `measured_rtt_ms / 2 > max_clock_skew_ms`.
    ClockSkewRttExceeded {
        /// Measured RTT in milliseconds.
        measured_rtt_ms: u64,
        /// Configured conservative `max_clock_skew_ms`.
        max_clock_skew_ms: NonZeroU64,
        /// Resulting fail-closed [`ControlPlaneLinkState`] (`Partitioned`).
        quarantined_state: ControlPlaneLinkState,
    },
    /// Local monotonic tick regressed below `previous_tick`.
    LocalMonotonicRegression {
        /// Previously observed monotonic tick.
        previous_tick: u64,
        /// Regressed monotonic tick supplied.
        regressed_tick: u64,
        /// Resulting fail-closed [`ControlPlaneLinkState`] (`Partitioned`).
        quarantined_state: ControlPlaneLinkState,
    },
}

impl fmt::Display for LinkClockViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClockSkewRttExceeded {
                measured_rtt_ms,
                max_clock_skew_ms,
                ..
            } => write!(
                formatter,
                "cross-host link quarantined: RTT {measured_rtt_ms}ms exceeds 2 * max_clock_skew_ms ({}ms)",
                max_clock_skew_ms.get().saturating_mul(2)
            ),
            Self::LocalMonotonicRegression {
                previous_tick,
                regressed_tick,
                ..
            } => write!(
                formatter,
                "cross-host link quarantined: local monotonic tick regressed ({regressed_tick} < {previous_tick})"
            ),
        }
    }
}

impl Error for LinkClockViolation {}

impl PartitionAutonomyGate {
    /// Observes a local monotonic tick and optional cross-host RTT measurement
    /// against `max_clock_skew_ms`.
    ///
    /// If `now_mono_tick` regresses below [`Self::last_observed_tick`] or if
    /// `measured_rtt_ms / 2 > max_clock_skew_ms`, this gate **immediately**
    /// transitions into fail-closed [`ControlPlaneLinkState::Partitioned`] with
    /// [`REASON_LOCAL_MONOTONIC_REGRESSION`] or
    /// [`REASON_CLOCK_SKEW_RTT_EXCEEDED`] before returning the typed
    /// [`LinkClockViolation`].
    ///
    /// # Errors
    ///
    /// Returns [`LinkClockViolation`] after quarantining the gate into
    /// `Partitioned`.
    pub fn observe_cross_host_clock_sample(
        &mut self,
        now_mono_tick: u64,
        measured_rtt_ms_opt: Option<u64>,
        max_clock_skew_ms: NonZeroU64,
    ) -> Result<ControlPlaneLinkState, LinkClockViolation> {
        let frozen_fence = self.state.authoritative_fence();
        if now_mono_tick < self.last_observed_tick {
            let previous_tick = self.last_observed_tick;
            let quarantined_state = ControlPlaneLinkState::Partitioned {
                frozen_fence,
                partitioned_since_tick: previous_tick,
                reason_code: REASON_LOCAL_MONOTONIC_REGRESSION,
            };
            self.state = quarantined_state;
            return Err(LinkClockViolation::LocalMonotonicRegression {
                previous_tick,
                regressed_tick: now_mono_tick,
                quarantined_state,
            });
        }

        self.last_observed_tick = now_mono_tick;

        if let Some(measured_rtt_ms) = measured_rtt_ms_opt
            && rtt_exceeds_max_clock_skew(measured_rtt_ms, max_clock_skew_ms)
        {
            let quarantined_state = ControlPlaneLinkState::Partitioned {
                frozen_fence,
                partitioned_since_tick: now_mono_tick,
                reason_code: REASON_CLOCK_SKEW_RTT_EXCEEDED,
            };
            self.state = quarantined_state;
            return Err(LinkClockViolation::ClockSkewRttExceeded {
                measured_rtt_ms,
                max_clock_skew_ms,
                quarantined_state,
            });
        }

        if let ControlPlaneLinkState::Connected { verified_fence, .. } = self.state {
            self.state = ControlPlaneLinkState::Connected {
                verified_fence,
                last_contact_tick: now_mono_tick,
            };
        }
        Ok(self.state)
    }
}

/// Paired [`PartitionAutonomyGate`] + [`LinkClockHealthState`] monitor that
/// enforces conservative clock-skew bounds and local monotonic non-regression.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CrossHostLinkClockGuard {
    gate: PartitionAutonomyGate,
    health: LinkClockHealthState,
    max_clock_skew_ms: NonZeroU64,
}

impl CrossHostLinkClockGuard {
    /// Creates a connected guard anchored at `verified_fence` and
    /// `initial_mono_tick`.
    #[must_use]
    pub const fn new_connected(
        verified_fence: CellFence,
        initial_mono_tick: u64,
        max_clock_skew_ms: NonZeroU64,
    ) -> Self {
        Self {
            gate: PartitionAutonomyGate::new_connected(verified_fence, initial_mono_tick),
            health: LinkClockHealthState::Healthy {
                max_clock_skew_ms,
                last_mono_tick: initial_mono_tick,
                last_rtt_ms: None,
            },
            max_clock_skew_ms,
        }
    }

    /// Underlying [`PartitionAutonomyGate`].
    #[must_use]
    pub const fn gate(&self) -> &PartitionAutonomyGate {
        &self.gate
    }

    /// Mutable reference to the underlying [`PartitionAutonomyGate`] for
    /// epoch-bumped reconnection convergence.
    #[must_use]
    pub const fn gate_mut(&mut self) -> &mut PartitionAutonomyGate {
        &mut self.gate
    }

    /// Current [`LinkClockHealthState`].
    #[must_use]
    pub const fn health(&self) -> LinkClockHealthState {
        self.health
    }

    /// Configured `max_clock_skew_ms`.
    #[must_use]
    pub const fn max_clock_skew_ms(&self) -> NonZeroU64 {
        self.max_clock_skew_ms
    }

    /// Observes a local monotonic tick and optional RTT measurement, failing
    /// closed into `Partitioned` + `ClockSkewQuarantined` /
    /// `MonotonicRegressionQuarantined` if violated.
    ///
    /// # Errors
    ///
    /// Returns [`LinkClockViolation`] when RTT half-bound or monotonic
    /// ordering is violated.
    pub fn observe_sample(
        &mut self,
        now_mono_tick: u64,
        measured_rtt_ms_opt: Option<u64>,
    ) -> Result<LinkClockHealthState, LinkClockViolation> {
        match self.gate.observe_cross_host_clock_sample(
            now_mono_tick,
            measured_rtt_ms_opt,
            self.max_clock_skew_ms,
        ) {
            Ok(_) => {
                if matches!(self.health, LinkClockHealthState::Healthy { .. }) {
                    self.health = LinkClockHealthState::Healthy {
                        max_clock_skew_ms: self.max_clock_skew_ms,
                        last_mono_tick: now_mono_tick,
                        last_rtt_ms: measured_rtt_ms_opt.or(match self.health {
                            LinkClockHealthState::Healthy { last_rtt_ms, .. } => last_rtt_ms,
                            _ => None,
                        }),
                    };
                }
                Ok(self.health)
            }
            Err(
                err @ LinkClockViolation::ClockSkewRttExceeded {
                    measured_rtt_ms, ..
                },
            ) => {
                self.health = LinkClockHealthState::ClockSkewQuarantined {
                    measured_rtt_ms,
                    max_clock_skew_ms: self.max_clock_skew_ms,
                    quarantined_at_tick: now_mono_tick,
                };
                Err(err)
            }
            Err(
                err @ LinkClockViolation::LocalMonotonicRegression {
                    previous_tick,
                    regressed_tick,
                    ..
                },
            ) => {
                self.health = LinkClockHealthState::MonotonicRegressionQuarantined {
                    previous_tick,
                    regressed_tick,
                };
                Err(err)
            }
        }
    }
}

/// Wall-clock-free monotonic coordinate for cross-host
/// `ReconciliationCheckpoint` audit records (`ADR-0019` Trigger 7 #2).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MonotonicCheckpointCoord {
    /// Owning Cell identity.
    pub cell: CellIdentity,
    /// Owning Cell boot generation.
    pub node_boot_generation: Generation,
    /// Owning Cell epoch.
    pub epoch: CellEpoch,
    /// Strictly contiguous sequence (`1, 2, ...`) within `(cell,
    /// node_boot_generation, epoch)`.
    pub checkpoint_seq: NonZeroU64,
}

impl MonotonicCheckpointCoord {
    /// Builds a per-Cell monotonic checkpoint coordinate.
    #[must_use]
    pub const fn new(
        cell: CellIdentity,
        node_boot_generation: Generation,
        epoch: CellEpoch,
        checkpoint_seq: NonZeroU64,
    ) -> Self {
        Self {
            cell,
            node_boot_generation,
            epoch,
            checkpoint_seq,
        }
    }
}

/// Immutable, SHA-256 hash-chained checkpoint record ordered strictly by
/// [`MonotonicCheckpointCoord`] without any `SystemTime` or OS `pid` axis
/// (`ADR-0019` Trigger 7 #2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MonotonicCheckpointEntry {
    coord: MonotonicCheckpointCoord,
    fact: CheckpointFact,
    prev_chain_digest: [u8; 32],
    entry_chain_digest: [u8; 32],
    audit_wall_hint_opt: Option<AuditWallTimestampHint>,
}

impl MonotonicCheckpointEntry {
    /// Builds a hash-chained [`MonotonicCheckpointEntry`] from `coord`, `fact`,
    /// `prev_chain_digest` (`[0u8; 32]` for the genesis entry of a Cell trail),
    /// and an optional opaque `audit_wall_hint_opt`.
    ///
    /// # Errors
    ///
    /// Fails closed with [`ClockLivenessError::CheckpointOrderViolation`] if
    /// `fact.node_boot_generation()` or `fact.epoch()` disagrees with `coord`.
    pub fn new(
        coord: MonotonicCheckpointCoord,
        fact: CheckpointFact,
        prev_chain_digest: [u8; 32],
        audit_wall_hint_opt: Option<AuditWallTimestampHint>,
    ) -> Result<Self, ClockLivenessError> {
        if fact.node_boot_generation() != coord.node_boot_generation || fact.epoch() != coord.epoch
        {
            return Err(ClockLivenessError::CheckpointOrderViolation {
                cell: coord.cell,
                reason: "CheckpointFact boot_generation/epoch mismatch with MonotonicCheckpointCoord",
            });
        }
        let entry_chain_digest = Self::compute_chain_digest(
            coord,
            fact.fencing_token(),
            fact.digest(),
            prev_chain_digest,
        );
        Ok(Self {
            coord,
            fact,
            prev_chain_digest,
            entry_chain_digest,
            audit_wall_hint_opt,
        })
    }

    /// Computes the canonical SHA-256 chain digest over `(coord, fencing_token,
    /// fact_digest, prev_chain_digest)`. Note that `audit_wall_hint_opt` is
    /// deliberately excluded from ordering and chain authority axes, though
    /// serialized alongside for human display.
    #[must_use]
    pub fn compute_chain_digest(
        coord: MonotonicCheckpointCoord,
        fencing_token: CellFencingToken,
        fact_digest: &str,
        prev_chain_digest: [u8; 32],
    ) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(MONOTONIC_CHECKPOINT_CHAIN_DOMAIN);
        hasher.update([0x00]);
        hasher.update(coord.cell.as_bytes());
        hasher.update(coord.node_boot_generation.get().to_le_bytes());
        hasher.update(coord.epoch.get().to_le_bytes());
        hasher.update(coord.checkpoint_seq.get().to_le_bytes());
        hasher.update(fencing_token.get().to_le_bytes());
        hasher.update(prev_chain_digest);
        let digest_bytes = fact_digest.as_bytes();
        let digest_len = u64::try_from(digest_bytes.len()).unwrap_or(u64::MAX);
        hasher.update(digest_len.to_le_bytes());
        hasher.update(digest_bytes);
        hasher.finalize().into()
    }

    /// Verifies that `fact` matches `coord` and `entry_chain_digest` matches
    /// the canonical hash-chain preimage.
    #[must_use]
    pub fn verify_entry_digest(&self) -> bool {
        if self.fact.node_boot_generation() != self.coord.node_boot_generation
            || self.fact.epoch() != self.coord.epoch
        {
            return false;
        }
        let expected = Self::compute_chain_digest(
            self.coord,
            self.fact.fencing_token(),
            self.fact.digest(),
            self.prev_chain_digest,
        );
        self.entry_chain_digest == expected
    }

    /// Per-Cell monotonic coordinate.
    #[must_use]
    pub const fn coord(&self) -> MonotonicCheckpointCoord {
        self.coord
    }

    /// Underlying [`CheckpointFact`].
    #[must_use]
    pub const fn fact(&self) -> &CheckpointFact {
        &self.fact
    }

    /// Previous entry's `entry_chain_digest` (`[0u8; 32]` at genesis).
    #[must_use]
    pub const fn prev_chain_digest(&self) -> [u8; 32] {
        self.prev_chain_digest
    }

    /// This entry's SHA-256 chain digest.
    #[must_use]
    pub const fn entry_chain_digest(&self) -> [u8; 32] {
        self.entry_chain_digest
    }

    /// Optional opaque wall-clock display hint.
    #[must_use]
    pub const fn audit_wall_hint(&self) -> Option<AuditWallTimestampHint> {
        self.audit_wall_hint_opt
    }
}

/// Verified per-Cell monotonic checkpoint trail (`ADR-0019` Trigger 7 #2).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MonotonicCheckpointTrail {
    entries_by_cell: BTreeMap<CellIdentity, Vec<MonotonicCheckpointEntry>>,
}

impl MonotonicCheckpointTrail {
    /// Creates an empty trail verifier/accumulator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends or idempotently replays an entry for `entry.coord().cell`.
    ///
    /// Rules enforced per Cell:
    /// 1. Entry digest must verify (`verify_entry_digest()`).
    /// 2. Genesis entry for a Cell must start at `checkpoint_seq == 1` and
    ///    `prev_chain_digest == [0u8; 32]`.
    /// 3. Exact replay of the already committed coordinate + chain digest is
    ///    idempotent; conflicting payload at an existing coordinate fails
    ///    closed.
    /// 4. Within the same `(node_boot_generation, epoch)`, `checkpoint_seq`
    ///    must advance by exactly `+1` (`prev_seq + 1`), `fencing_token` must
    ///    not regress, and `prev_chain_digest` must equal the previous entry's
    ///    `entry_chain_digest`.
    /// 5. Across an advancing `(node_boot_generation, epoch)`, `checkpoint_seq`
    ///    must reset to `1` and `prev_chain_digest` must equal the previous
    ///    entry's `entry_chain_digest` so cross-epoch history stays cryptographically
    ///    linked without any wall-clock timestamp.
    ///
    /// # Errors
    ///
    /// Fails closed on any order gap, regression, conflict, or tampered chain
    /// digest.
    pub fn append(
        &mut self,
        entry: MonotonicCheckpointEntry,
    ) -> Result<&MonotonicCheckpointEntry, ClockLivenessError> {
        let cell = entry.coord.cell;
        if !entry.verify_entry_digest() {
            return Err(ClockLivenessError::CheckpointChainTampered {
                cell,
                reason: "entry_chain_digest mismatch",
            });
        }

        let chain = self.entries_by_cell.entry(cell).or_default();
        if let Some(existing_idx) = chain.iter().position(|e| e.coord == entry.coord) {
            if chain[existing_idx].entry_chain_digest == entry.entry_chain_digest
                && chain[existing_idx].fact == entry.fact
                && chain[existing_idx].prev_chain_digest == entry.prev_chain_digest
            {
                return Ok(&chain[existing_idx]);
            }
            return Err(ClockLivenessError::CheckpointOrderViolation {
                cell,
                reason: "conflicting checkpoint replay at identical MonotonicCheckpointCoord",
            });
        }

        match chain.last() {
            None => {
                if entry.coord.checkpoint_seq != NonZeroU64::MIN {
                    return Err(ClockLivenessError::CheckpointOrderViolation {
                        cell,
                        reason: "genesis monotonic checkpoint must start at checkpoint_seq == 1",
                    });
                }
                if entry.prev_chain_digest != [0u8; 32] {
                    return Err(ClockLivenessError::CheckpointChainTampered {
                        cell,
                        reason: "genesis monotonic checkpoint must have zero prev_chain_digest",
                    });
                }
            }
            Some(prev) => {
                let prev_era = (prev.coord.node_boot_generation, prev.coord.epoch);
                let next_era = (entry.coord.node_boot_generation, entry.coord.epoch);
                if next_era < prev_era {
                    return Err(ClockLivenessError::CheckpointOrderViolation {
                        cell,
                        reason: "checkpoint (node_boot_generation, epoch) regressed",
                    });
                }
                if entry.prev_chain_digest != prev.entry_chain_digest {
                    return Err(ClockLivenessError::CheckpointChainTampered {
                        cell,
                        reason: "checkpoint prev_chain_digest does not link to prior head",
                    });
                }
                if next_era == prev_era {
                    let expected_seq = prev.coord.checkpoint_seq.get().checked_add(1);
                    if Some(entry.coord.checkpoint_seq.get()) != expected_seq {
                        return Err(ClockLivenessError::CheckpointOrderViolation {
                            cell,
                            reason: "checkpoint_seq is non-contiguous within same (boot_generation, epoch)",
                        });
                    }
                    if entry.fact.fencing_token() < prev.fact.fencing_token() {
                        return Err(ClockLivenessError::CheckpointOrderViolation {
                            cell,
                            reason: "fencing_token regressed within same (boot_generation, epoch)",
                        });
                    }
                } else if entry.coord.checkpoint_seq != NonZeroU64::MIN {
                    return Err(ClockLivenessError::CheckpointOrderViolation {
                        cell,
                        reason: "new (boot_generation, epoch) era must start at checkpoint_seq == 1",
                    });
                }
            }
        }

        let idx = chain.len();
        chain.push(entry);
        chain
            .get(idx)
            .ok_or(ClockLivenessError::CheckpointChainTampered {
                cell,
                reason: "internal chain push index invariant",
            })
    }

    /// Returns the verified ordered checkpoint slice for `cell`.
    #[must_use]
    pub fn entries(&self, cell: CellIdentity) -> &[MonotonicCheckpointEntry] {
        self.entries_by_cell.get(&cell).map_or(&[], Vec::as_slice)
    }

    /// Returns the latest chain head digest for `cell` (`[0u8; 32]` if empty).
    #[must_use]
    pub fn head_chain_digest(&self, cell: CellIdentity) -> [u8; 32] {
        self.entries(cell)
            .last()
            .map_or([0u8; 32], MonotonicCheckpointEntry::entry_chain_digest)
    }
}

impl CellDirectory {
    /// Persists one wall-clock-free [`MonotonicCheckpointEntry`] under
    /// `<root>/monotonic-checkpoints/<cell>-b<boot>-e<epoch>-s<seq>.mchk` after
    /// verifying that it links monotonically onto `cell`'s existing trail
    /// (`ADR-0019` Trigger 7 #2).
    ///
    /// Idempotent when the identical entry is already committed on disk.
    ///
    /// # Errors
    ///
    /// Fails closed on existing trail corruption, non-monotonic coordinate or
    /// sequence gap, conflicting replay, or filesystem I/O failure.
    pub fn record_monotonic_checkpoint(
        &self,
        entry: &MonotonicCheckpointEntry,
    ) -> Result<MonotonicCheckpointEntry, ClockLivenessError> {
        if !entry.verify_entry_digest() {
            return Err(ClockLivenessError::CheckpointChainTampered {
                cell: entry.coord.cell,
                reason: "entry_chain_digest verification failed before write",
            });
        }
        let dir = self.root().join(MONOTONIC_CHECKPOINTS_DIR);
        fs::create_dir_all(&dir).map_err(|_| FederationError::DirectoryUnavailable)?;

        let mut trail = self.read_monotonic_checkpoint_trail(entry.coord.cell)?;
        let already_exists = trail
            .entries(entry.coord.cell)
            .iter()
            .any(|committed| committed.coord == entry.coord);
        let validated = trail.append(entry.clone())?.clone();
        if already_exists {
            return Ok(validated);
        }

        let target = monotonic_checkpoint_path(&dir, entry.coord);
        atomic_write_text(&target, &encode_monotonic_checkpoint(&validated))?;
        Ok(validated)
    }

    /// Reads and cryptographically verifies `cell`'s entire cross-host
    /// monotonic checkpoint trail from `<root>/monotonic-checkpoints/`, sorted
    /// exclusively by `(node_boot_generation, epoch, checkpoint_seq)` (zero
    /// `SystemTime` or `pid` ordering).
    ///
    /// # Errors
    ///
    /// Fails closed on any unparseable file, misfiled identity, sequence gap,
    /// or broken `prev_chain_digest` link.
    pub fn read_monotonic_checkpoint_trail(
        &self,
        cell: CellIdentity,
    ) -> Result<MonotonicCheckpointTrail, ClockLivenessError> {
        let dir = self.root().join(MONOTONIC_CHECKPOINTS_DIR);
        if !dir
            .try_exists()
            .map_err(|_| FederationError::DirectoryUnavailable)?
        {
            return Ok(MonotonicCheckpointTrail::new());
        }

        let prefix = format!("{}-", encode_hex(cell.as_bytes()));
        let mut raw_entries = Vec::new();
        let read_dir = fs::read_dir(&dir).map_err(|_| FederationError::DirectoryUnavailable)?;
        for entry_res in read_dir {
            let dir_entry = entry_res.map_err(|_| FederationError::DirectoryUnavailable)?;
            let path = dir_entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some(MONOTONIC_CHECKPOINT_EXT) {
                continue;
            }
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !file_name.starts_with(&prefix) {
                continue;
            }
            let text =
                fs::read_to_string(&path).map_err(|_| FederationError::DirectoryUnavailable)?;
            let parsed = decode_monotonic_checkpoint(&text, cell)?;
            let expected_name = monotonic_checkpoint_filename(parsed.coord);
            if file_name != expected_name {
                return Err(ClockLivenessError::CheckpointChainTampered {
                    cell,
                    reason: "monotonic checkpoint filename does not match encoded coord",
                });
            }
            raw_entries.push(parsed);
        }

        raw_entries.sort_by_key(MonotonicCheckpointEntry::coord);
        let mut trail = MonotonicCheckpointTrail::new();
        for item in raw_entries {
            trail.append(item)?;
        }
        Ok(trail)
    }
}

fn validate_single_line(text: &str, max_bytes: usize) -> Option<&str> {
    if text.is_empty() || text.len() > max_bytes {
        return None;
    }
    if text.chars().any(char::is_control) {
        return None;
    }
    Some(text)
}

fn monotonic_checkpoint_filename(coord: MonotonicCheckpointCoord) -> String {
    format!(
        "{}-b{:020}-e{:020}-s{:020}.{MONOTONIC_CHECKPOINT_EXT}",
        encode_hex(coord.cell.as_bytes()),
        coord.node_boot_generation.get(),
        coord.epoch.get(),
        coord.checkpoint_seq.get(),
    )
}

fn monotonic_checkpoint_path(dir: &Path, coord: MonotonicCheckpointCoord) -> PathBuf {
    dir.join(monotonic_checkpoint_filename(coord))
}

fn encode_monotonic_checkpoint(entry: &MonotonicCheckpointEntry) -> String {
    let hint_line = entry
        .audit_wall_hint_opt
        .map_or_else(|| "none".to_string(), |h| h.audit_display_ms().to_string());
    format!(
        "v1\ncell={}\nboot={}\nepoch={}\nseq={}\ntoken={}\nprev={}\nchain={}\nhint={hint_line}\ndigest={}\n",
        encode_hex(entry.coord.cell.as_bytes()),
        entry.coord.node_boot_generation.get(),
        entry.coord.epoch.get(),
        entry.coord.checkpoint_seq.get(),
        entry.fact.fencing_token().get(),
        encode_hex(&entry.prev_chain_digest),
        encode_hex(&entry.entry_chain_digest),
        entry.fact.digest(),
    )
}

fn parse_monotonic_kv(
    text: &str,
    expected_cell: CellIdentity,
) -> Result<BTreeMap<&str, &str>, ClockLivenessError> {
    let mut lines = text.lines();
    if lines.next() != Some("v1") {
        return Err(ClockLivenessError::CheckpointChainTampered {
            cell: expected_cell,
            reason: "unsupported monotonic checkpoint version header",
        });
    }
    let mut kv = BTreeMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            return Err(ClockLivenessError::CheckpointChainTampered {
                cell: expected_cell,
                reason: "malformed key=value line in monotonic checkpoint",
            });
        };
        if kv.insert(k, v).is_some() {
            return Err(ClockLivenessError::CheckpointChainTampered {
                cell: expected_cell,
                reason: "duplicate key in monotonic checkpoint",
            });
        }
    }
    Ok(kv)
}

fn decode_monotonic_coord(
    kv: &BTreeMap<&str, &str>,
    expected_cell: CellIdentity,
) -> Result<(MonotonicCheckpointCoord, CellFencingToken), ClockLivenessError> {
    let cell_bytes = kv.get("cell").and_then(|s| decode_hex_16(s)).ok_or(
        ClockLivenessError::CheckpointChainTampered {
            cell: expected_cell,
            reason: "invalid cell hex",
        },
    )?;
    let cell = CellIdentity::from_domain(SchedulerDomainId::from_bytes(cell_bytes));
    if cell != expected_cell {
        return Err(ClockLivenessError::CheckpointChainTampered {
            cell: expected_cell,
            reason: "misfiled cell identity in monotonic checkpoint",
        });
    }
    let boot_raw: u64 = kv.get("boot").and_then(|s| s.parse().ok()).ok_or(
        ClockLivenessError::CheckpointChainTampered {
            cell: expected_cell,
            reason: "invalid boot generation",
        },
    )?;
    let node_boot_generation = NonZeroU64::new(boot_raw).map(Generation::new).ok_or(
        ClockLivenessError::CheckpointChainTampered {
            cell: expected_cell,
            reason: "zero boot generation",
        },
    )?;
    let epoch_raw: u64 = kv.get("epoch").and_then(|s| s.parse().ok()).ok_or(
        ClockLivenessError::CheckpointChainTampered {
            cell: expected_cell,
            reason: "invalid epoch",
        },
    )?;
    let epoch =
        CellEpoch::from_u64(epoch_raw).ok_or(ClockLivenessError::CheckpointChainTampered {
            cell: expected_cell,
            reason: "zero epoch",
        })?;
    let seq_raw: u64 = kv.get("seq").and_then(|s| s.parse().ok()).ok_or(
        ClockLivenessError::CheckpointChainTampered {
            cell: expected_cell,
            reason: "invalid checkpoint_seq",
        },
    )?;
    let checkpoint_seq =
        NonZeroU64::new(seq_raw).ok_or(ClockLivenessError::CheckpointChainTampered {
            cell: expected_cell,
            reason: "zero checkpoint_seq",
        })?;
    let token_raw: u64 = kv.get("token").and_then(|s| s.parse().ok()).ok_or(
        ClockLivenessError::CheckpointChainTampered {
            cell: expected_cell,
            reason: "invalid fencing_token",
        },
    )?;
    let fencing_token = CellFencingToken::from_u64(token_raw).ok_or(
        ClockLivenessError::CheckpointChainTampered {
            cell: expected_cell,
            reason: "zero fencing_token",
        },
    )?;
    Ok((
        MonotonicCheckpointCoord::new(cell, node_boot_generation, epoch, checkpoint_seq),
        fencing_token,
    ))
}

fn decode_monotonic_checkpoint(
    text: &str,
    expected_cell: CellIdentity,
) -> Result<MonotonicCheckpointEntry, ClockLivenessError> {
    let kv = parse_monotonic_kv(text, expected_cell)?;
    let (coord, fencing_token) = decode_monotonic_coord(&kv, expected_cell)?;

    let prev_chain_digest = kv.get("prev").and_then(|s| decode_hex_32(s)).ok_or(
        ClockLivenessError::CheckpointChainTampered {
            cell: expected_cell,
            reason: "invalid prev_chain_digest",
        },
    )?;
    let entry_chain_digest = kv.get("chain").and_then(|s| decode_hex_32(s)).ok_or(
        ClockLivenessError::CheckpointChainTampered {
            cell: expected_cell,
            reason: "invalid entry_chain_digest",
        },
    )?;
    let audit_wall_hint_opt = match kv.get("hint").copied() {
        Some("none") => None,
        Some(raw) => {
            let ms: u64 = raw
                .parse()
                .map_err(|_| ClockLivenessError::CheckpointChainTampered {
                    cell: expected_cell,
                    reason: "invalid hint integer",
                })?;
            Some(AuditWallTimestampHint::new(ms))
        }
        None => {
            return Err(ClockLivenessError::CheckpointChainTampered {
                cell: expected_cell,
                reason: "missing hint field",
            });
        }
    };
    let digest_str =
        kv.get("digest")
            .copied()
            .ok_or(ClockLivenessError::CheckpointChainTampered {
                cell: expected_cell,
                reason: "missing digest field",
            })?;
    let fact = CheckpointFact::new(
        coord.node_boot_generation,
        coord.epoch,
        fencing_token,
        digest_str,
    )
    .map_err(|_| ClockLivenessError::CheckpointChainTampered {
        cell: expected_cell,
        reason: "invalid CheckpointFact digest",
    })?;

    let rebuilt = MonotonicCheckpointEntry {
        coord,
        fact,
        prev_chain_digest,
        entry_chain_digest,
        audit_wall_hint_opt,
    };
    if !rebuilt.verify_entry_digest() {
        return Err(ClockLivenessError::CheckpointChainTampered {
            cell: expected_cell,
            reason: "stored entry_chain_digest failed SHA-256 verification",
        });
    }
    Ok(rebuilt)
}

fn atomic_write_text(target: &Path, content: &str) -> Result<(), FederationError> {
    let parent = target
        .parent()
        .ok_or(FederationError::DirectoryUnavailable)?;
    let nonce = STAGING_NONCE.fetch_add(1, AtomicOrdering::Relaxed);
    let temp = parent.join(format!(".mchk-stage-{}-{nonce}.tmp", std::process::id()));
    let write_res = (|| -> std::io::Result<()> {
        let mut file = fs::File::create(&temp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, target)?;
        Ok(())
    })();
    if write_res.is_err() {
        let _ = fs::remove_file(&temp);
        return Err(FederationError::DirectoryUnavailable);
    }
    Ok(())
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    out
}

fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn decode_hex_16(text: &str) -> Option<[u8; 16]> {
    if text.len() != 32 {
        return None;
    }
    let raw = text.as_bytes();
    let mut out = [0u8; 16];
    for (idx, slot) in out.iter_mut().enumerate() {
        let hi = decode_nibble(raw[idx * 2])?;
        let lo = decode_nibble(raw[idx * 2 + 1])?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

fn decode_hex_32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let raw = text.as_bytes();
    let mut out = [0u8; 32];
    for (idx, slot) in out.iter_mut().enumerate() {
        let hi = decode_nibble(raw[idx * 2])?;
        let lo = decode_nibble(raw[idx * 2 + 1])?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}
