//! Cross-hardware-clock-domain conservative skew horizon envelope, shrunk holder
//! monotonic budget, delayed grantor expiry floor, multi-endpoint fence barrier
//! coverage, and cryptographic receipt gating (`ADR-0026` §2.3, `ADR-0019`
//! R2.3 #2, `[MODEL-TIME-001]`, `[RESCROW-TIME-001]`, `[LEASE-TTL-001]`,
//! `[LEASE-FENCE-001]`, `[LEASE-LOSS-001]`, `[LEASE-RENEW-001]`).
//!
//! # Time-Domain Separation
//!
//! Cross-host wall clocks (`NTP` / `SystemTime` / `WallReading`) are **audit
//! hints only** ([`AuditWallTimeHint`], deliberately omitting `Ord` and
//! `PartialOrd`). Lease liveliness, shrunk holder budgets, and grantor sweep
//! horizons operate strictly on node-local monotonic readings
//! ([`MonotonicInstantMs`], [`MonotonicDurationMs`]) plus conservative skew
//! bounds ([`ClockSkewPolicy`]):
//!
//! - **Validity precondition**: `ttl_grant > 2 * max_clock_skew + safety_margin`.
//! - **Holder shrunk budget**: `ttl_holder = ttl_grant - 2 * max_clock_skew - safety_margin`,
//!   anchored at holder-local `t_req_holder_mono` recorded **before** sending
//!   the grant/renewal request (`t_req_holder <= t_grant_grantor` in physical
//!   time). Once `now_holder_mono - t_req_holder_mono >= ttl_holder`, the
//!   holder transitions `ACTIVE -> FENCED` locally and refuses any new local
//!   `reserve`, `CommitPermit`, or irreversible `EffectPermit`.
//! - **Grantor delayed expiry floor**: anchored at grantor-local commit instant
//!   `t_grant_grantor_mono`, the grantor refuses declaring an unreachable
//!   holder's horizon elapsed until `elapsed_grantor_mono > ttl_grant + max_clock_skew`.
//!   Even after that horizon elapses, unsettled `QuotaLease`, `CapacityLease`,
//!   and `ExclusiveDeviceLease` records move strictly into **`QUARANTINED`** —
//!   never unlocking `AVAILABLE` / `pool_remaining` / `DeviceLeaseHead(FREE)`
//!   until either a verified Ed25519-signed holder receipt arrives or the
//!   authority `(epoch, fencing_token)` has strictly advanced **and** durable
//!   `FenceBarrierAck`s cover **100%** of [`MultiEndpointFenceScope`] (plus
//!   family-specific physical cleanup proofs).

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::Arc;

use nlos_cell::{
    CellAdmitError, CellAuthority, CellEpoch, CellError, CellFence, CellFencingToken, CellIdentity,
};
use nlos_clock::{AuthorityClock, NowRequest, Reading};
use nlos_net::{IdentityPeerVerifier, PeerDigestSigner, PeerIdentityError, VerifiedPeerKey};
use nlos_types::{
    CapacityLeaseId, DeviceId, ExclusiveDeviceLeaseId, Generation, IdempotencyKey, KeyId,
    PrincipalId, QuotaLeaseId, ReceiptId,
};
use sha2::{Digest, Sha256};

use crate::cross_host_store::{StoredHorizonRow, StoredLeaseFamily, StoredSignedCloseReceiptRow};
use crate::reconcile::{
    verify_capacity_fence_ack, verify_device_fence_ack, verify_quota_fence_ack,
};
use crate::{
    CapacityLeaseGrant, CapacityLeaseGrantError, CapacityLeaseGrantor, CapacityLeaseReturnError,
    CapacityLeaseState, CapacityReconcileError, CapacityReconciliationReceipt,
    DeviceReconcileError, DeviceReconciliationReceipt, DeviceResetReceipt,
    ExclusiveDeviceLeaseGrant, ExclusiveDeviceLeaseGrantError, ExclusiveDeviceLeaseGrantor,
    ExclusiveDeviceLeaseReturnError, ExclusiveDeviceLeaseState, FenceBarrierAck, FenceScope,
    HostAttachReceipt, LeaseStoreError, QuotaLeaseGrant, QuotaLeaseGrantError, QuotaLeaseGrantor,
    QuotaLeaseLedgerError, QuotaLeaseState, QuotaReconcileError, QuotaReconciliationReceipt,
    SqliteLeaseStore,
};

/// Domain separator for holder-signed [`SignedLeaseCloseReceipt`] Ed25519 digests.
pub const SIGNED_CLOSE_RECEIPT_DOMAIN: &[u8] = b"llmos/lease/signed-close-receipt/v1";

/// Domain separator for gateway-signed [`SignedFenceBarrierAck`] Ed25519 digests.
pub const SIGNED_FENCE_BARRIER_ACK_DOMAIN: &[u8] = b"llmos/lease/signed-fence-barrier-ack/v1";

/// Domain separator for supervisor-signed [`SignedSupervisorCancellationReceipt`] digests.
pub const SIGNED_SUPERVISOR_CANCEL_DOMAIN: &[u8] = b"llmos/lease/signed-supervisor-cancel/v1";

/// Node-local monotonic instant in milliseconds (`[MODEL-TIME-001]`, `ADR-0026` §2.3).
///
/// Only comparable against monotonic readings captured on the **same** hardware
/// node and boot incarnation; never derived from `SystemTime` or `NTP`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MonotonicInstantMs(u64);

impl MonotonicInstantMs {
    /// Wraps a node-local monotonic millisecond reading.
    #[must_use]
    pub const fn new(ms: u64) -> Self {
        Self(ms)
    }

    /// Raw node-local monotonic millisecond tick.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Checked addition of a monotonic duration.
    #[must_use]
    pub const fn checked_add(self, duration: MonotonicDurationMs) -> Option<Self> {
        match self.0.checked_add(duration.0) {
            Some(sum) => Some(Self(sum)),
            None => None,
        }
    }

    /// Computes monotonic elapsed milliseconds since `earlier` on the same node.
    ///
    /// # Errors
    ///
    /// Fails closed with [`QuarantinedClockViolation::LocalMonotonicRollback`]
    /// if `self < earlier`.
    pub const fn elapsed_since(
        self,
        earlier: Self,
    ) -> Result<MonotonicDurationMs, QuarantinedClockViolation> {
        if self.0 < earlier.0 {
            return Err(QuarantinedClockViolation::LocalMonotonicRollback {
                previous: earlier,
                observed: self,
            });
        }
        Ok(MonotonicDurationMs(self.0 - earlier.0))
    }
}

/// Node-local monotonic duration in milliseconds (`ADR-0026` §2.3).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MonotonicDurationMs(u64);

impl MonotonicDurationMs {
    /// Wraps a duration in milliseconds.
    #[must_use]
    pub const fn new(ms: u64) -> Self {
        Self(ms)
    }

    /// Raw millisecond count.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Non-authoritative wall-clock timestamp hint (`ms` since Unix epoch) carried
/// strictly for audit diagnostics (`[MODEL-TIME-001]`, `[RESCROW-TIME-001]`).
///
/// Deliberately omits `Ord` and `PartialOrd` so wall-clock hints can never be
/// compared across hosts to decide lease expiry, fencing, or ordering.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AuditWallTimeHint(u64);

impl AuditWallTimeHint {
    /// Wraps an unverified Unix-epoch millisecond hint for audit logging only.
    #[must_use]
    pub const fn new(unix_epoch_ms: u64) -> Self {
        Self(unix_epoch_ms)
    }

    /// Raw diagnostic wall-clock hint (`ms`).
    #[must_use]
    pub const fn unix_epoch_ms(self) -> u64 {
        self.0
    }
}

/// Conservative cross-host clock-skew envelope policy (`ADR-0026` §2.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockSkewPolicy {
    /// Configured upper bound on one-way clock skew / uncertainty (`ms`, `>= 1`).
    pub max_clock_skew_ms: NonZeroU64,
    /// Strictly positive safety separation margin (`ms`, `>= 1`) between holder
    /// local fencing and grantor conservative expiry evaluation.
    pub safety_margin_ms: NonZeroU64,
}

impl ClockSkewPolicy {
    /// Constructs a conservative clock-skew policy.
    #[must_use]
    pub const fn new(max_clock_skew_ms: NonZeroU64, safety_margin_ms: NonZeroU64) -> Self {
        Self {
            max_clock_skew_ms,
            safety_margin_ms,
        }
    }

    /// Returns `2 * max_clock_skew_ms + safety_margin_ms`, which `ttl_grant_ms`
    /// must strictly exceed (`ttl_grant > 2 * max_clock_skew + safety_margin`).
    #[must_use]
    pub const fn minimum_exclusive_ttl_floor_ms(self) -> Option<u64> {
        let Some(two_skew) = self.max_clock_skew_ms.get().checked_mul(2) else {
            return None;
        };
        two_skew.checked_add(self.safety_margin_ms.get())
    }

    /// Validates `ttl_grant_ms > 2 * max_clock_skew_ms + safety_margin_ms` and
    /// computes the shrunk holder monotonic budget
    /// `ttl_holder = ttl_grant_ms - 2 * max_clock_skew_ms - safety_margin_ms`.
    ///
    /// # Errors
    ///
    /// Returns [`ClockHorizonError::InvalidLeaseHorizon`] when `ttl_grant_ms` is
    /// less than or equal to `2 * max_clock_skew_ms + safety_margin_ms` or
    /// overflows `u64`.
    pub const fn validate_and_compute_holder_budget(
        self,
        ttl_grant_ms: u64,
    ) -> Result<MonotonicDurationMs, ClockHorizonError> {
        let Some(floor) = self.minimum_exclusive_ttl_floor_ms() else {
            return Err(ClockHorizonError::HorizonArithmeticOverflow);
        };
        if ttl_grant_ms <= floor {
            return Err(ClockHorizonError::InvalidLeaseHorizon {
                ttl_grant_ms,
                max_clock_skew_ms: self.max_clock_skew_ms.get(),
                safety_margin_ms: self.safety_margin_ms.get(),
                required_strictly_greater_than_ms: floor,
            });
        }
        Ok(MonotonicDurationMs::new(ttl_grant_ms - floor))
    }

    /// Computes the grantor conservative expiry wait floor
    /// `ttl_grant_ms + max_clock_skew_ms` (`ADR-0026` §2.3).
    ///
    /// The grantor may only sweep an unreachable holder's lease into
    /// `QUARANTINED` once `elapsed_grantor_mono > ttl_grant_ms + max_clock_skew_ms`
    /// (strictly greater).
    ///
    /// # Errors
    ///
    /// Fails closed if `ttl_grant_ms` violates [`Self::validate_and_compute_holder_budget`]
    /// or `ttl_grant_ms + max_clock_skew_ms` overflows `u64`.
    pub const fn grantor_expiry_wait_floor(
        self,
        ttl_grant_ms: u64,
    ) -> Result<MonotonicDurationMs, ClockHorizonError> {
        let Some(floor) = self.minimum_exclusive_ttl_floor_ms() else {
            return Err(ClockHorizonError::HorizonArithmeticOverflow);
        };
        if ttl_grant_ms <= floor {
            return Err(ClockHorizonError::InvalidLeaseHorizon {
                ttl_grant_ms,
                max_clock_skew_ms: self.max_clock_skew_ms.get(),
                safety_margin_ms: self.safety_margin_ms.get(),
                required_strictly_greater_than_ms: floor,
            });
        }
        let Some(wait_floor) = ttl_grant_ms.checked_add(self.max_clock_skew_ms.get()) else {
            return Err(ClockHorizonError::HorizonArithmeticOverflow);
        };
        Ok(MonotonicDurationMs::new(wait_floor))
    }

    /// Verifies that a measured control-plane round-trip sample satisfies
    /// `ceil(measured_rtt_ms / 2) <= max_clock_skew_ms`.
    ///
    /// # Errors
    ///
    /// Fails closed with [`QuarantinedClockViolation::RttHalfExceedsMaxClockSkew`]
    /// when `ceil(measured_rtt_ms / 2) > max_clock_skew_ms`.
    pub const fn verify_rtt_sample(
        self,
        measured_rtt_ms: u64,
    ) -> Result<(), QuarantinedClockViolation> {
        let one_way_upper_bound_ms = measured_rtt_ms.div_ceil(2);
        if one_way_upper_bound_ms > self.max_clock_skew_ms.get() {
            return Err(QuarantinedClockViolation::RttHalfExceedsMaxClockSkew {
                measured_rtt_ms,
                one_way_upper_bound_ms,
                max_clock_skew_ms: self.max_clock_skew_ms.get(),
            });
        }
        Ok(())
    }
}

/// Fail-closed clock or RTT skew violation forcing immediate quarantine
/// (`ADR-0026` §2.3, `ADR-0019` Trigger 7 #2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QuarantinedClockViolation {
    /// Local monotonic clock reading regressed below a previously observed value.
    LocalMonotonicRollback {
        /// Previously observed local monotonic instant.
        previous: MonotonicInstantMs,
        /// Regressed local monotonic instant.
        observed: MonotonicInstantMs,
    },
    /// Durable `AuthorityClock` logical tick regressed below its committed watermark.
    AuthorityClockTickRollback {
        /// Previously committed logical tick reading.
        previous_tick: u64,
        /// Observed regressed logical tick reading.
        observed_tick: u64,
    },
    /// Durable `AuthorityClock` failed closed (`SQLite` corruption / CAS loss).
    AuthorityClockFailure(String),
    /// Node boot generation regressed below the bound incarnation.
    BootGenerationRegression {
        /// Bound boot generation.
        expected: Generation,
        /// Observed regressed boot generation.
        observed: Generation,
    },
    /// `ceil(measured_rtt_ms / 2) > max_clock_skew_ms` invalidated the skew bound.
    RttHalfExceedsMaxClockSkew {
        /// Measured round-trip latency (`ms`).
        measured_rtt_ms: u64,
        /// `ceil(measured_rtt_ms / 2)` (`ms`).
        one_way_upper_bound_ms: u64,
        /// Configured `max_clock_skew_ms`.
        max_clock_skew_ms: u64,
    },
}

impl fmt::Display for QuarantinedClockViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LocalMonotonicRollback { previous, observed } => write!(
                formatter,
                "local monotonic clock regressed: observed {} ms < previous {} ms",
                observed.get(),
                previous.get()
            ),
            Self::AuthorityClockTickRollback {
                previous_tick,
                observed_tick,
            } => write!(
                formatter,
                "AuthorityClock logical tick regressed: observed {observed_tick} < previous {previous_tick}"
            ),
            Self::AuthorityClockFailure(reason) => {
                write!(formatter, "AuthorityClock fail-closed error: {reason}")
            }
            Self::BootGenerationRegression { expected, observed } => write!(
                formatter,
                "node boot generation regressed: observed {} < expected {}",
                observed.get(),
                expected.get()
            ),
            Self::RttHalfExceedsMaxClockSkew {
                measured_rtt_ms,
                one_way_upper_bound_ms,
                max_clock_skew_ms,
            } => write!(
                formatter,
                "RTT/2 ({one_way_upper_bound_ms} ms from RTT {measured_rtt_ms} ms) > max_clock_skew ({max_clock_skew_ms} ms)"
            ),
        }
    }
}

impl Error for QuarantinedClockViolation {}

/// Local monotonic + [`AuthorityClock`] continuity guard (`ADR-0011`, `ADR-0026` §2.3).
///
/// Couples process-local [`MonotonicInstantMs`] sampling with durable
/// [`AuthorityClock`] logical watermarks and `node_boot_generation`. Any
/// rollback or storage error latches a sticky [`QuarantinedClockViolation`].
pub struct AuthorityClockHorizonGuard {
    clock: Arc<AuthorityClock>,
    boot_generation: Generation,
    last_monotonic: Option<MonotonicInstantMs>,
    last_authority_tick: u64,
    latched_violation: Option<QuarantinedClockViolation>,
}

impl fmt::Debug for AuthorityClockHorizonGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthorityClockHorizonGuard")
            .field("boot_generation", &self.boot_generation)
            .field("last_monotonic", &self.last_monotonic)
            .field("last_authority_tick", &self.last_authority_tick)
            .field("latched_violation", &self.latched_violation)
            .finish_non_exhaustive()
    }
}

impl AuthorityClockHorizonGuard {
    /// Opens a guard anchored to `clock`'s durable watermark and `boot_generation`.
    ///
    /// # Errors
    ///
    /// Fails closed if `AuthorityClock::inspect` fails.
    pub fn open(
        clock: Arc<AuthorityClock>,
        boot_generation: Generation,
    ) -> Result<Self, QuarantinedClockViolation> {
        let initial = clock
            .inspect()
            .map_err(|err| QuarantinedClockViolation::AuthorityClockFailure(err.to_string()))?;
        Ok(Self {
            clock,
            boot_generation,
            last_monotonic: None,
            last_authority_tick: initial.as_u64(),
            latched_violation: None,
        })
    }

    /// Sticky violation, if any clock or boot-generation anomaly was detected.
    #[must_use]
    pub fn latched_violation(&self) -> Option<&QuarantinedClockViolation> {
        self.latched_violation.as_ref()
    }

    /// Samples and durably advances [`AuthorityClock::now`] alongside
    /// `observed_mono`, latching fail-closed quarantine on any rollback.
    ///
    /// # Errors
    ///
    /// Returns [`QuarantinedClockViolation`] on boot-generation regression,
    /// local monotonic rollback, `AuthorityClock` watermark rollback, or
    /// `AuthorityClock` storage failure.
    pub fn sample_verified_instant(
        &mut self,
        tick_key: IdempotencyKey,
        observed_mono: MonotonicInstantMs,
        presented_boot_generation: Generation,
    ) -> Result<(MonotonicInstantMs, Reading), QuarantinedClockViolation> {
        if let Some(violation) = &self.latched_violation {
            return Err(violation.clone());
        }
        if presented_boot_generation < self.boot_generation {
            let v = QuarantinedClockViolation::BootGenerationRegression {
                expected: self.boot_generation,
                observed: presented_boot_generation,
            };
            self.latched_violation = Some(v.clone());
            return Err(v);
        }
        if let Some(prev) = self.last_monotonic
            && observed_mono < prev
        {
            let v = QuarantinedClockViolation::LocalMonotonicRollback {
                previous: prev,
                observed: observed_mono,
            };
            self.latched_violation = Some(v.clone());
            return Err(v);
        }
        let decision = self
            .clock
            .now(NowRequest {
                idempotency_key: tick_key,
            })
            .map_err(|err| {
                let v = QuarantinedClockViolation::AuthorityClockFailure(err.to_string());
                self.latched_violation = Some(v.clone());
                v
            })?;
        let watermark = self.clock.inspect().map_err(|err| {
            let v = QuarantinedClockViolation::AuthorityClockFailure(err.to_string());
            self.latched_violation = Some(v.clone());
            v
        })?;
        if watermark.as_u64() < self.last_authority_tick {
            let v = QuarantinedClockViolation::AuthorityClockTickRollback {
                previous_tick: self.last_authority_tick,
                observed_tick: watermark.as_u64(),
            };
            self.latched_violation = Some(v.clone());
            return Err(v);
        }
        self.last_monotonic = Some(observed_mono);
        self.last_authority_tick = watermark.as_u64();
        Ok((observed_mono, decision.reading()))
    }

    /// Verifies an externally reported `AuthorityClock` tick watermark against
    /// this guard's monotonic expectation, latching [`QuarantinedClockViolation`]
    /// on any regression.
    ///
    /// # Errors
    ///
    /// Returns [`QuarantinedClockViolation::AuthorityClockTickRollback`] if
    /// `observed_watermark_tick < last_authority_tick`.
    pub fn observe_authority_watermark_tick(
        &mut self,
        observed_watermark_tick: u64,
    ) -> Result<(), QuarantinedClockViolation> {
        if let Some(violation) = &self.latched_violation {
            return Err(violation.clone());
        }
        if observed_watermark_tick < self.last_authority_tick {
            let v = QuarantinedClockViolation::AuthorityClockTickRollback {
                previous_tick: self.last_authority_tick,
                observed_tick: observed_watermark_tick,
            };
            self.latched_violation = Some(v.clone());
            return Err(v);
        }
        self.last_authority_tick = observed_watermark_tick;
        Ok(())
    }
}

/// Lease family tag across cross-host horizon and barrier tracking.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CrossHostLeaseFamily {
    /// `QuotaLease` (`[LEASE-GRANT-001]`).
    Quota,
    /// `CapacityLease` (`[LEASE-CAPACITY-001]`).
    Capacity,
    /// `ExclusiveDeviceLease` (`[LEASE-DEVICE-001]`).
    ExclusiveDevice,
}

impl CrossHostLeaseFamily {
    pub(crate) const fn to_stored(self) -> StoredLeaseFamily {
        match self {
            Self::Quota => StoredLeaseFamily::Quota,
            Self::Capacity => StoredLeaseFamily::Capacity,
            Self::ExclusiveDevice => StoredLeaseFamily::Device,
        }
    }

    pub(crate) const fn from_stored(stored: StoredLeaseFamily) -> Self {
        match stored {
            StoredLeaseFamily::Quota => Self::Quota,
            StoredLeaseFamily::Capacity => Self::Capacity,
            StoredLeaseFamily::Device => Self::ExclusiveDevice,
        }
    }

    pub(crate) const fn wire_tag(self) -> u8 {
        match self {
            Self::Quota => 1,
            Self::Capacity => 2,
            Self::ExclusiveDevice => 3,
        }
    }

    pub(crate) const fn from_wire_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::Quota),
            2 => Some(Self::Capacity),
            3 => Some(Self::ExclusiveDevice),
            _ => None,
        }
    }
}

/// Multi-endpoint / multi-gateway enforcement scope extending single-Cell
/// [`FenceScope`] (`[LEASE-FENCE-001]`, `ADR-0026` §2.3).
///
/// Barrier-based reconciliation of a `QUARANTINED` lease requires **100%** of
/// `required_gateways` to contribute a verified [`FenceBarrierAck`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MultiEndpointFenceScope {
    scope_cell: CellIdentity,
    required_gateways: BTreeSet<[u8; 16]>,
}

impl MultiEndpointFenceScope {
    /// Lifts a single-Cell [`FenceScope`] into a 1-endpoint
    /// `MultiEndpointFenceScope` whose sole required gateway is
    /// `*scope.as_cell().as_bytes()`.
    #[must_use]
    pub fn from_single_cell(scope: FenceScope) -> Self {
        let mut required_gateways = BTreeSet::new();
        required_gateways.insert(*scope.as_cell().as_bytes());
        Self {
            scope_cell: scope.as_cell(),
            required_gateways,
        }
    }

    /// Constructs a multi-gateway enforcement scope bound to `scope_cell`.
    ///
    /// # Errors
    ///
    /// Fails closed if `gateways` is empty or contains an all-zero `[0u8; 16]`
    /// gateway identifier.
    pub fn with_gateways<I>(
        scope_cell: CellIdentity,
        gateways: I,
    ) -> Result<Self, ClockHorizonError>
    where
        I: IntoIterator<Item = [u8; 16]>,
    {
        let mut set = BTreeSet::new();
        for gw in gateways {
            if gw == [0u8; 16] {
                return Err(ClockHorizonError::InvalidMultiEndpointScope(
                    "zero gateway_id is forbidden in MultiEndpointFenceScope",
                ));
            }
            set.insert(gw);
        }
        if set.is_empty() {
            return Err(ClockHorizonError::InvalidMultiEndpointScope(
                "MultiEndpointFenceScope requires at least one gateway endpoint",
            ));
        }
        Ok(Self {
            scope_cell,
            required_gateways: set,
        })
    }

    /// Underlying single-Cell [`FenceScope`] anchor.
    #[must_use]
    pub const fn as_single_fence_scope(&self) -> FenceScope {
        FenceScope::cell(self.scope_cell)
    }

    /// Nominal `CellIdentity` of the holder scope.
    #[must_use]
    pub const fn scope_cell(&self) -> CellIdentity {
        self.scope_cell
    }

    /// Canonical sorted set of all required enforcement gateway identifiers.
    #[must_use]
    pub const fn required_gateways(&self) -> &BTreeSet<[u8; 16]> {
        &self.required_gateways
    }

    /// Ordered vector of gateway identifiers still missing a verified
    /// [`FenceBarrierAck`].
    #[must_use]
    pub fn missing_gateways(&self, acked: &BTreeMap<[u8; 16], FenceBarrierAck>) -> Vec<[u8; 16]> {
        self.required_gateways
            .iter()
            .filter(|gw| !acked.contains_key(*gw))
            .copied()
            .collect()
    }

    /// Returns `true` iff **100%** of `required_gateways` have a verified
    /// [`FenceBarrierAck`].
    #[must_use]
    pub fn is_fully_covered(&self, acked: &BTreeMap<[u8; 16], FenceBarrierAck>) -> bool {
        self.required_gateways
            .iter()
            .all(|gw| acked.contains_key(gw))
    }
}

impl From<FenceScope> for MultiEndpointFenceScope {
    fn from(scope: FenceScope) -> Self {
        Self::from_single_cell(scope)
    }
}

/// Verified physical-cleanup proof category (`ADR-0019` R2.3 #2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum PhysicalCleanupProofKind {
    /// Verified `CleanupAttestation` proving remote capacity processes terminated.
    CapacityCleanupAttestation = 1,
    /// Verified `DeviceResetReceipt` + zeroization proof for an exclusive device.
    DeviceResetZeroization = 2,
    /// Signed remote host supervisor / gateway cancellation attestation.
    SignedSupervisorCancellation = 3,
}

impl PhysicalCleanupProofKind {
    pub(crate) const fn from_u8(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::CapacityCleanupAttestation),
            2 => Some(Self::DeviceResetZeroization),
            3 => Some(Self::SignedSupervisorCancellation),
            _ => None,
        }
    }
}

/// Explicit distinction between ledger/authority fencing vs verified remote
/// physical side-effect cleanup (`ADR-0019` R2.3 #2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuarantineDisposition {
    /// Lease is active or cleanly settled by the holder without quarantine.
    ActiveOrSettled,
    /// Ledger face value / capacity / device head is quarantined and old
    /// `CellFence` authority is fenced at gateways, **without** falsely claiming
    /// remote physical execution has terminated.
    LedgerQuarantinedFenceOnly,
    /// Remote physical cleanup was cryptographically / structurally verified by
    /// an explicit cleanup attestation, device reset zeroization receipt, or
    /// signed supervisor cancellation receipt.
    PhysicallyCleanedVerified {
        /// Proof family that unlocked physical-cleanup verification.
        proof_kind: PhysicalCleanupProofKind,
        /// 32-byte digest of the verified physical cleanup proof.
        proof_digest: [u8; 32],
    },
}

impl QuarantineDisposition {
    pub(crate) const fn encode(self) -> (u8, Option<u8>, Option<[u8; 32]>) {
        match self {
            Self::ActiveOrSettled => (0, None, None),
            Self::LedgerQuarantinedFenceOnly => (1, None, None),
            Self::PhysicallyCleanedVerified {
                proof_kind,
                proof_digest,
            } => (2, Some(proof_kind as u8), Some(proof_digest)),
        }
    }

    pub(crate) fn decode(
        code: u8,
        kind: Option<u8>,
        digest: Option<[u8; 32]>,
    ) -> Result<Self, LeaseStoreError> {
        match (code, kind, digest) {
            (0, None, None) => Ok(Self::ActiveOrSettled),
            (1, None, None) => Ok(Self::LedgerQuarantinedFenceOnly),
            (2, Some(raw_kind), Some(proof_digest)) => {
                let proof_kind = PhysicalCleanupProofKind::from_u8(raw_kind)
                    .ok_or(LeaseStoreError::CorruptRecord("unknown cleanup_proof_kind"))?;
                Ok(Self::PhysicallyCleanedVerified {
                    proof_kind,
                    proof_digest,
                })
            }
            _ => Err(LeaseStoreError::CorruptRecord(
                "inconsistent QuarantineDisposition columns",
            )),
        }
    }
}

/// Ed25519-signed idempotent holder close receipt for a cross-host `QuotaLease`
/// (`ADR-0026` §2.3, `[LEASE-CLOSE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignedLeaseCloseReceipt {
    /// Unique idempotency key for this close receipt.
    pub receipt_id: ReceiptId,
    /// Closed `QuotaLeaseId`.
    pub lease_id: QuotaLeaseId,
    /// Holder `CellIdentity`.
    pub holder_cell: CellIdentity,
    /// Holder `node_boot_generation`.
    pub holder_boot_generation: Generation,
    /// Bound `CellEpoch`.
    pub epoch: CellEpoch,
    /// Bound `CellFencingToken`.
    pub fencing_token: CellFencingToken,
    /// Issued `face_value`.
    pub face_value: u64,
    /// Final monotonic spent high-water committed on the holder.
    pub final_high_water_spent: u64,
    /// Unspent remainder returned to `AVAILABLE` (`face_value - final_high_water_spent`).
    pub returned_unspent: u64,
    /// Signing `PrincipalId` of the holder Cell.
    pub signer_principal_id: PrincipalId,
    /// Signing custody `KeyId`.
    pub signer_key_id: KeyId,
    /// Signing custody key generation (`rotation_epoch`).
    pub signer_key_generation: Generation,
    /// Ed25519 signature over [`Self::signing_digest`] under [`SIGNED_CLOSE_RECEIPT_DOMAIN`].
    pub ed25519_signature: [u8; 64],
}

impl SignedLeaseCloseReceipt {
    /// Signs a [`SignedLeaseCloseReceipt`] for `grant` after verifying exact
    /// conservation (`final_high_water_spent + returned_unspent == grant.face_value()`).
    ///
    /// # Errors
    ///
    /// Fails closed on conservation mismatch or custody signing failure.
    pub fn sign(
        grant: &QuotaLeaseGrant,
        receipt_id: ReceiptId,
        final_high_water_spent: u64,
        returned_unspent: u64,
        signer: &dyn PeerDigestSigner,
        signed_at_audit_hint: AuditWallTimeHint,
    ) -> Result<Self, ClockHorizonError> {
        if final_high_water_spent.checked_add(returned_unspent) != Some(grant.face_value()) {
            return Err(ClockHorizonError::CloseReceiptConservationMismatch {
                face_value: grant.face_value(),
                final_high_water_spent,
                returned_unspent,
            });
        }
        let signer_principal_id = signer.principal_id();
        let signer_key_id = signer.key_id();
        let signer_key_generation = signer.key_generation();
        let digest = compute_close_receipt_digest(
            receipt_id,
            grant.lease_id(),
            grant.cell(),
            grant.node_boot_generation(),
            grant.epoch(),
            grant.fencing_token(),
            grant.face_value(),
            final_high_water_spent,
            returned_unspent,
            signer_principal_id,
            signer_key_id,
            signer_key_generation,
        );
        let ed25519_signature = signer
            .sign_domain_digest(
                SIGNED_CLOSE_RECEIPT_DOMAIN,
                digest,
                signed_at_audit_hint.unix_epoch_ms(),
            )
            .map_err(ClockHorizonError::PeerSignatureRejected)?;
        Ok(Self {
            receipt_id,
            lease_id: grant.lease_id(),
            holder_cell: grant.cell(),
            holder_boot_generation: grant.node_boot_generation(),
            epoch: grant.epoch(),
            fencing_token: grant.fencing_token(),
            face_value: grant.face_value(),
            final_high_water_spent,
            returned_unspent,
            signer_principal_id,
            signer_key_id,
            signer_key_generation,
            ed25519_signature,
        })
    }

    /// Canonical `SHA-256` digest over all close-receipt accounting and fence axes.
    #[must_use]
    pub fn signing_digest(&self) -> [u8; 32] {
        compute_close_receipt_digest(
            self.receipt_id,
            self.lease_id,
            self.holder_cell,
            self.holder_boot_generation,
            self.epoch,
            self.fencing_token,
            self.face_value,
            self.final_high_water_spent,
            self.returned_unspent,
            self.signer_principal_id,
            self.signer_key_id,
            self.signer_key_generation,
        )
    }

    /// Verifies conservation (`final_high_water_spent + returned_unspent == face_value`)
    /// and strict Ed25519 signature validity via `verifier`.
    ///
    /// # Errors
    ///
    /// Fails closed if conservation fails or `verifier` rejects the key/signature.
    pub fn verify(
        &self,
        verifier: &dyn IdentityPeerVerifier,
        verified_at_ms: u64,
    ) -> Result<VerifiedPeerKey, ClockHorizonError> {
        if self
            .final_high_water_spent
            .checked_add(self.returned_unspent)
            != Some(self.face_value)
        {
            return Err(ClockHorizonError::CloseReceiptConservationMismatch {
                face_value: self.face_value,
                final_high_water_spent: self.final_high_water_spent,
                returned_unspent: self.returned_unspent,
            });
        }
        verifier
            .verify_peer_digest(
                self.signer_principal_id,
                self.signer_key_id,
                self.signer_key_generation,
                self.signing_digest(),
                self.ed25519_signature,
                verified_at_ms,
            )
            .map_err(ClockHorizonError::PeerSignatureRejected)
    }

    pub(crate) fn to_stored_row(self) -> StoredSignedCloseReceiptRow {
        StoredSignedCloseReceiptRow {
            lease_id: self.lease_id,
            receipt_id: self.receipt_id,
            holder_cell: self.holder_cell,
            holder_boot_generation: self.holder_boot_generation,
            epoch: self.epoch,
            fencing_token: self.fencing_token,
            face_value: self.face_value,
            final_high_water_spent: self.final_high_water_spent,
            returned_unspent: self.returned_unspent,
            receipt_digest: self.signing_digest(),
            signer_principal_id: self.signer_principal_id,
            signer_key_id: self.signer_key_id,
            signer_key_generation: self.signer_key_generation,
            ed25519_signature: self.ed25519_signature,
        }
    }

    pub(crate) const fn from_stored_row(row: StoredSignedCloseReceiptRow) -> Self {
        Self {
            receipt_id: row.receipt_id,
            lease_id: row.lease_id,
            holder_cell: row.holder_cell,
            holder_boot_generation: row.holder_boot_generation,
            epoch: row.epoch,
            fencing_token: row.fencing_token,
            face_value: row.face_value,
            final_high_water_spent: row.final_high_water_spent,
            returned_unspent: row.returned_unspent,
            signer_principal_id: row.signer_principal_id,
            signer_key_id: row.signer_key_id,
            signer_key_generation: row.signer_key_generation,
            ed25519_signature: row.ed25519_signature,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn compute_close_receipt_digest(
    receipt_id: ReceiptId,
    lease_id: QuotaLeaseId,
    holder_cell: CellIdentity,
    holder_boot_generation: Generation,
    epoch: CellEpoch,
    fencing_token: CellFencingToken,
    face_value: u64,
    final_high_water_spent: u64,
    returned_unspent: u64,
    signer_principal_id: PrincipalId,
    signer_key_id: KeyId,
    signer_key_generation: Generation,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(SIGNED_CLOSE_RECEIPT_DOMAIN);
    hasher.update(receipt_id.as_bytes());
    hasher.update(lease_id.as_bytes());
    hasher.update(holder_cell.as_bytes());
    hasher.update(holder_boot_generation.get().to_be_bytes());
    hasher.update(epoch.get().to_be_bytes());
    hasher.update(fencing_token.get().to_be_bytes());
    hasher.update(face_value.to_be_bytes());
    hasher.update(final_high_water_spent.to_be_bytes());
    hasher.update(returned_unspent.to_be_bytes());
    hasher.update(signer_principal_id.as_bytes());
    hasher.update(signer_key_id.as_bytes());
    hasher.update(signer_key_generation.get().to_be_bytes());
    hasher.finalize().into()
}

/// Ed25519-signed enforcement gateway [`FenceBarrierAck`] (`[LEASE-FENCE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignedFenceBarrierAck {
    /// Lease family this barrier ACK fences.
    pub family: CrossHostLeaseFamily,
    /// Underlying canonical `FenceBarrierAck`.
    pub ack: FenceBarrierAck,
    /// Gateway / authority `PrincipalId`.
    pub signer_principal_id: PrincipalId,
    /// Gateway custody `KeyId`.
    pub signer_key_id: KeyId,
    /// Gateway custody key generation (`rotation_epoch`).
    pub signer_key_generation: Generation,
    /// Ed25519 signature over [`Self::signing_digest`] under [`SIGNED_FENCE_BARRIER_ACK_DOMAIN`].
    pub ed25519_signature: [u8; 64],
}

impl SignedFenceBarrierAck {
    /// Signs a [`FenceBarrierAck`] using an enforcement gateway's [`PeerDigestSigner`].
    ///
    /// # Errors
    ///
    /// Fails closed if `ack.verify_digest()` is false or custody signing fails.
    pub fn sign(
        family: CrossHostLeaseFamily,
        ack: FenceBarrierAck,
        signer: &dyn PeerDigestSigner,
        signed_at_audit_hint: AuditWallTimeHint,
    ) -> Result<Self, ClockHorizonError> {
        if !ack.verify_digest() {
            return Err(ClockHorizonError::InvalidFenceBarrierAck(
                "FenceBarrierAck digest invalid before signing",
            ));
        }
        let signer_principal_id = signer.principal_id();
        let signer_key_id = signer.key_id();
        let signer_key_generation = signer.key_generation();
        let digest = compute_signed_barrier_digest(
            family,
            &ack,
            signer_principal_id,
            signer_key_id,
            signer_key_generation,
        );
        let ed25519_signature = signer
            .sign_domain_digest(
                SIGNED_FENCE_BARRIER_ACK_DOMAIN,
                digest,
                signed_at_audit_hint.unix_epoch_ms(),
            )
            .map_err(ClockHorizonError::PeerSignatureRejected)?;
        Ok(Self {
            family,
            ack,
            signer_principal_id,
            signer_key_id,
            signer_key_generation,
            ed25519_signature,
        })
    }

    /// Canonical `SHA-256` digest signed by the enforcement gateway.
    #[must_use]
    pub fn signing_digest(&self) -> [u8; 32] {
        compute_signed_barrier_digest(
            self.family,
            &self.ack,
            self.signer_principal_id,
            self.signer_key_id,
            self.signer_key_generation,
        )
    }

    /// Verifies both `FenceBarrierAck::verify_digest` and the gateway's Ed25519
    /// custody signature.
    ///
    /// # Errors
    ///
    /// Fails closed on tampered `ack_digest` or invalid/revoked peer signature.
    pub fn verify(
        &self,
        verifier: &dyn IdentityPeerVerifier,
        verified_at_ms: u64,
    ) -> Result<VerifiedPeerKey, ClockHorizonError> {
        if !self.ack.verify_digest() {
            return Err(ClockHorizonError::InvalidFenceBarrierAck(
                "FenceBarrierAck digest verification failed",
            ));
        }
        verifier
            .verify_peer_digest(
                self.signer_principal_id,
                self.signer_key_id,
                self.signer_key_generation,
                self.signing_digest(),
                self.ed25519_signature,
                verified_at_ms,
            )
            .map_err(ClockHorizonError::PeerSignatureRejected)
    }
}

fn compute_signed_barrier_digest(
    family: CrossHostLeaseFamily,
    ack: &FenceBarrierAck,
    signer_principal_id: PrincipalId,
    signer_key_id: KeyId,
    signer_key_generation: Generation,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(SIGNED_FENCE_BARRIER_ACK_DOMAIN);
    hasher.update([family.wire_tag()]);
    hasher.update(ack.lease_id());
    hasher.update(ack.fence_scope().as_cell().as_bytes());
    hasher.update(ack.epoch().get().to_be_bytes());
    hasher.update(ack.fencing_token().get().to_be_bytes());
    hasher.update(ack.gateway_id());
    hasher.update(ack.observed_high_water().to_be_bytes());
    hasher.update(ack.ack_digest());
    hasher.update(signer_principal_id.as_bytes());
    hasher.update(signer_key_id.as_bytes());
    hasher.update(signer_key_generation.get().to_be_bytes());
    hasher.finalize().into()
}

/// Signed remote supervisor / gateway cancellation attestation proving that
/// remote execution under a quarantined lease was physically terminated
/// (`ADR-0019` R2.3 #2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignedSupervisorCancellationReceipt {
    /// Lease family whose remote execution was terminated.
    pub family: CrossHostLeaseFamily,
    /// 16-byte lease identifier.
    pub lease_id: [u8; 16],
    /// Remote holder `CellIdentity`.
    pub holder_cell: CellIdentity,
    /// Remote holder `node_boot_generation`.
    pub holder_boot_generation: Generation,
    /// Terminated lease `CellEpoch`.
    pub epoch: CellEpoch,
    /// Terminated lease `CellFencingToken`.
    pub fencing_token: CellFencingToken,
    /// Signing supervisor `PrincipalId`.
    pub signer_principal_id: PrincipalId,
    /// Signing custody `KeyId`.
    pub signer_key_id: KeyId,
    /// Signing custody key generation.
    pub signer_key_generation: Generation,
    /// Ed25519 signature over [`Self::signing_digest`].
    pub ed25519_signature: [u8; 64],
}

impl SignedSupervisorCancellationReceipt {
    /// Signs a supervisor physical-cancellation attestation.
    ///
    /// # Errors
    ///
    /// Fails closed if custody signing fails.
    #[allow(clippy::too_many_arguments)]
    pub fn sign(
        family: CrossHostLeaseFamily,
        lease_id: [u8; 16],
        holder_cell: CellIdentity,
        holder_boot_generation: Generation,
        epoch: CellEpoch,
        fencing_token: CellFencingToken,
        signer: &dyn PeerDigestSigner,
        signed_at_audit_hint: AuditWallTimeHint,
    ) -> Result<Self, ClockHorizonError> {
        let signer_principal_id = signer.principal_id();
        let signer_key_id = signer.key_id();
        let signer_key_generation = signer.key_generation();
        let mut candidate = Self {
            family,
            lease_id,
            holder_cell,
            holder_boot_generation,
            epoch,
            fencing_token,
            signer_principal_id,
            signer_key_id,
            signer_key_generation,
            ed25519_signature: [0u8; 64],
        };
        let digest = candidate.signing_digest();
        candidate.ed25519_signature = signer
            .sign_domain_digest(
                SIGNED_SUPERVISOR_CANCEL_DOMAIN,
                digest,
                signed_at_audit_hint.unix_epoch_ms(),
            )
            .map_err(ClockHorizonError::PeerSignatureRejected)?;
        Ok(candidate)
    }

    /// Canonical `SHA-256` digest over all cancellation attestation axes.
    #[must_use]
    pub fn signing_digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(SIGNED_SUPERVISOR_CANCEL_DOMAIN);
        hasher.update([self.family.wire_tag()]);
        hasher.update(self.lease_id);
        hasher.update(self.holder_cell.as_bytes());
        hasher.update(self.holder_boot_generation.get().to_be_bytes());
        hasher.update(self.epoch.get().to_be_bytes());
        hasher.update(self.fencing_token.get().to_be_bytes());
        hasher.update(self.signer_principal_id.as_bytes());
        hasher.update(self.signer_key_id.as_bytes());
        hasher.update(self.signer_key_generation.get().to_be_bytes());
        hasher.finalize().into()
    }

    /// Verifies the Ed25519 signature via `verifier`.
    ///
    /// # Errors
    ///
    /// Fails closed if `verifier` rejects the key or signature.
    pub fn verify(
        &self,
        verifier: &dyn IdentityPeerVerifier,
        verified_at_ms: u64,
    ) -> Result<VerifiedPeerKey, ClockHorizonError> {
        verifier
            .verify_peer_digest(
                self.signer_principal_id,
                self.signer_key_id,
                self.signer_key_generation,
                self.signing_digest(),
                self.ed25519_signature,
                verified_at_ms,
            )
            .map_err(ClockHorizonError::PeerSignatureRejected)
    }
}

/// Holder-side lease payload wrapped inside [`HolderClockBoundedLease`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HolderLeaseGrantKind {
    /// Cross-host `QuotaLease` with local monotonic spend tracking.
    Quota {
        /// Grantor-committed snapshot.
        grant: QuotaLeaseGrant,
        /// Monotonic local reserved/spent high-water (`<= grant.face_value()`).
        local_spent: u64,
    },
    /// Cross-host `CapacityLease`.
    Capacity {
        /// Grantor-committed snapshot.
        grant: CapacityLeaseGrant,
    },
    /// Cross-host `ExclusiveDeviceLease`.
    ExclusiveDevice {
        /// Grantor-committed snapshot.
        grant: ExclusiveDeviceLeaseGrant,
    },
}

/// Holder-local state under the shrunk monotonic survival budget
/// (`ADR-0026` §2.3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HolderHorizonState {
    /// `now_holder_mono - t_req_holder_mono < ttl_holder`: local reserve and
    /// commit/effect permits are admitted.
    Active,
    /// `now_holder_mono - t_req_holder_mono >= ttl_holder`: shrunk budget
    /// exhausted; **zero** new local reserve, `CommitPermit`, or irreversible
    /// `EffectPermit` may be admitted.
    FencedByShrunkHorizon,
    /// Holder signed its final close/return receipt and closed local use.
    ClosedOrReturned,
    /// Clock rollback or `RTT/2 > max_clock_skew` quarantined local execution.
    QuarantinedClockViolation(QuarantinedClockViolation),
}

/// Short-lived local commit permit minted while [`HolderClockBoundedLease`] is
/// strictly inside its shrunk monotonic budget (`ADR-0026` §2.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HolderCommitPermit {
    /// Bound 16-byte lease identifier.
    pub lease_id: [u8; 16],
    /// Bound `CellEpoch`.
    pub epoch: CellEpoch,
    /// Bound `CellFencingToken`.
    pub fencing_token: CellFencingToken,
    /// Caller-supplied monotonic permit sequence.
    pub permit_seq: u64,
    /// Remaining shrunk monotonic survival budget (`ms`, `>= 1`) at mint time.
    pub remaining_shrunk_budget_ms: u64,
}

/// Short-lived local irreversible-effect permit minted while
/// [`HolderClockBoundedLease`] is strictly inside its shrunk monotonic budget.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HolderEffectPermit {
    /// Bound 16-byte lease identifier.
    pub lease_id: [u8; 16],
    /// Bound `CellEpoch`.
    pub epoch: CellEpoch,
    /// Bound `CellFencingToken`.
    pub fencing_token: CellFencingToken,
    /// Bound irreversible effect digest (`SHA-256`).
    pub effect_digest: [u8; 32],
    /// Remaining shrunk monotonic survival budget (`ms`, `>= 1`) at mint time.
    pub remaining_shrunk_budget_ms: u64,
}

/// Fail-closed holder-side admission errors (`ADR-0026` §2.3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HolderLeaseAdmitError {
    /// Horizon configuration or renewal parameters are invalid.
    Horizon(ClockHorizonError),
    /// Shrunk holder budget (`ttl_holder`) has elapsed (`ACTIVE -> FENCED`).
    ShrunkBudgetExpired {
        /// Elapsed milliseconds since `t_req_holder_mono`.
        elapsed_ms: u64,
        /// Shrunk holder budget `ttl_holder` (`ms`).
        ttl_holder_ms: u64,
    },
    /// Holder lease was already closed or returned.
    AlreadyClosedOrReturned,
    /// Clock rollback or RTT skew violation quarantined the holder lease.
    ClockViolation(QuarantinedClockViolation),
    /// Operation is valid only for `QuotaLease`.
    WrongLeaseFamily {
        /// Actual family held.
        actual: CrossHostLeaseFamily,
    },
    /// Local quota reservation exceeds `face_value`.
    QuotaExceedsFaceValue {
        /// Requested reservation delta.
        requested_delta: u64,
        /// Current local spent high-water.
        current_spent: u64,
        /// Frozen `face_value`.
        face_value: u64,
    },
    /// Renewal attempted to alter `face_value` (`[LEASE-RENEW-001]` violation)
    /// or regressed `renewal_seq` / `t_req_holder_mono`.
    InvalidRenewal {
        /// Static diagnostic reason.
        reason: &'static str,
    },
}

impl fmt::Display for HolderLeaseAdmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Horizon(err) => write!(formatter, "holder lease horizon error: {err}"),
            Self::ShrunkBudgetExpired {
                elapsed_ms,
                ttl_holder_ms,
            } => write!(
                formatter,
                "holder shrunk monotonic budget expired: elapsed {elapsed_ms} ms >= ttl_holder {ttl_holder_ms} ms"
            ),
            Self::AlreadyClosedOrReturned => {
                formatter.write_str("holder lease is already closed or returned")
            }
            Self::ClockViolation(v) => write!(formatter, "holder clock quarantine: {v}"),
            Self::WrongLeaseFamily { actual } => {
                write!(formatter, "wrong holder lease family: {actual:?}")
            }
            Self::QuotaExceedsFaceValue {
                requested_delta,
                current_spent,
                face_value,
            } => write!(
                formatter,
                "local quota reserve ({current_spent} + {requested_delta}) exceeds face_value {face_value}"
            ),
            Self::InvalidRenewal { reason } => write!(formatter, "invalid lease renewal: {reason}"),
        }
    }
}

impl Error for HolderLeaseAdmitError {}

/// Holder-side lease enforcer anchored at `t_req_holder_mono` with shrunk
/// survival budget `ttl_holder = ttl_grant - 2 * max_clock_skew - safety_margin`
/// (`ADR-0026` §2.3, `[LEASE-TTL-001]`, `[LEASE-RENEW-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HolderClockBoundedLease {
    kind: HolderLeaseGrantKind,
    policy: ClockSkewPolicy,
    t_req_holder_mono: MonotonicInstantMs,
    ttl_grant_ms: u64,
    ttl_holder: MonotonicDurationMs,
    last_observed_mono: MonotonicInstantMs,
    renewal_seq: u64,
    state: HolderHorizonState,
}

impl HolderClockBoundedLease {
    /// Wraps a cross-host [`QuotaLeaseGrant`] anchored at `t_req_holder_mono`
    /// (recorded **before** sending the grant RPC) and verified at
    /// `t_grant_recv_holder_mono`.
    ///
    /// # Errors
    ///
    /// Fails closed if `ttl_grant_ms <= 2 * max_clock_skew + safety_margin`, if
    /// `t_grant_recv_holder_mono < t_req_holder_mono`, or if `RTT / 2 > max_clock_skew`.
    pub fn new_quota(
        grant: QuotaLeaseGrant,
        policy: ClockSkewPolicy,
        t_req_holder_mono: MonotonicInstantMs,
        t_grant_recv_holder_mono: MonotonicInstantMs,
        ttl_grant_ms: u64,
    ) -> Result<Self, ClockHorizonError> {
        Self::new_internal(
            HolderLeaseGrantKind::Quota {
                local_spent: grant.spent(),
                grant,
            },
            policy,
            t_req_holder_mono,
            t_grant_recv_holder_mono,
            ttl_grant_ms,
        )
    }

    /// Wraps a cross-host [`CapacityLeaseGrant`] under the shrunk holder budget.
    ///
    /// # Errors
    ///
    /// Same as [`Self::new_quota`].
    pub fn new_capacity(
        grant: CapacityLeaseGrant,
        policy: ClockSkewPolicy,
        t_req_holder_mono: MonotonicInstantMs,
        t_grant_recv_holder_mono: MonotonicInstantMs,
        ttl_grant_ms: u64,
    ) -> Result<Self, ClockHorizonError> {
        Self::new_internal(
            HolderLeaseGrantKind::Capacity { grant },
            policy,
            t_req_holder_mono,
            t_grant_recv_holder_mono,
            ttl_grant_ms,
        )
    }

    /// Wraps a cross-host [`ExclusiveDeviceLeaseGrant`] under the shrunk holder budget.
    ///
    /// # Errors
    ///
    /// Same as [`Self::new_quota`].
    pub fn new_exclusive_device(
        grant: ExclusiveDeviceLeaseGrant,
        policy: ClockSkewPolicy,
        t_req_holder_mono: MonotonicInstantMs,
        t_grant_recv_holder_mono: MonotonicInstantMs,
        ttl_grant_ms: u64,
    ) -> Result<Self, ClockHorizonError> {
        Self::new_internal(
            HolderLeaseGrantKind::ExclusiveDevice { grant },
            policy,
            t_req_holder_mono,
            t_grant_recv_holder_mono,
            ttl_grant_ms,
        )
    }

    fn new_internal(
        kind: HolderLeaseGrantKind,
        policy: ClockSkewPolicy,
        t_req_holder_mono: MonotonicInstantMs,
        t_grant_recv_holder_mono: MonotonicInstantMs,
        ttl_grant_ms: u64,
    ) -> Result<Self, ClockHorizonError> {
        let ttl_holder = policy.validate_and_compute_holder_budget(ttl_grant_ms)?;
        let rtt = t_grant_recv_holder_mono
            .elapsed_since(t_req_holder_mono)
            .map_err(ClockHorizonError::ClockViolation)?;
        policy
            .verify_rtt_sample(rtt.get())
            .map_err(ClockHorizonError::ClockViolation)?;

        let mut holder = Self {
            kind,
            policy,
            t_req_holder_mono,
            ttl_grant_ms,
            ttl_holder,
            last_observed_mono: t_req_holder_mono,
            renewal_seq: 0,
            state: HolderHorizonState::Active,
        };
        let _ = holder.refresh_state(t_grant_recv_holder_mono);
        Ok(holder)
    }

    /// Lease family of this holder handle.
    #[must_use]
    pub const fn family(&self) -> CrossHostLeaseFamily {
        match self.kind {
            HolderLeaseGrantKind::Quota { .. } => CrossHostLeaseFamily::Quota,
            HolderLeaseGrantKind::Capacity { .. } => CrossHostLeaseFamily::Capacity,
            HolderLeaseGrantKind::ExclusiveDevice { .. } => CrossHostLeaseFamily::ExclusiveDevice,
        }
    }

    /// Wrapped grant snapshot and local spend state.
    #[must_use]
    pub const fn kind(&self) -> HolderLeaseGrantKind {
        self.kind
    }

    /// Shrunk holder survival budget `ttl_holder` (`ms`).
    #[must_use]
    pub const fn ttl_holder(&self) -> MonotonicDurationMs {
        self.ttl_holder
    }

    /// Grantor-issued nominal `ttl_grant_ms`.
    #[must_use]
    pub const fn ttl_grant_ms(&self) -> u64 {
        self.ttl_grant_ms
    }

    /// Holder request-send monotonic anchor `t_req_holder_mono`.
    #[must_use]
    pub const fn t_req_holder_mono(&self) -> MonotonicInstantMs {
        self.t_req_holder_mono
    }

    /// Monotonic renewal sequence (`0` at initial grant).
    #[must_use]
    pub const fn renewal_seq(&self) -> u64 {
        self.renewal_seq
    }

    /// Last observed holder state without advancing local clock.
    #[must_use]
    pub fn state(&self) -> &HolderHorizonState {
        &self.state
    }

    /// Evaluates holder survival window at `now_holder_mono`, automatically
    /// transitioning `ACTIVE -> FENCED` once
    /// `now_holder_mono - t_req_holder_mono >= ttl_holder` or latching
    /// `QuarantinedClockViolation` on local monotonic regression.
    pub fn refresh_state(&mut self, now_holder_mono: MonotonicInstantMs) -> HolderHorizonState {
        if now_holder_mono < self.last_observed_mono {
            let violation = QuarantinedClockViolation::LocalMonotonicRollback {
                previous: self.last_observed_mono,
                observed: now_holder_mono,
            };
            self.state = HolderHorizonState::QuarantinedClockViolation(violation);
            return self.state.clone();
        }
        self.last_observed_mono = now_holder_mono;

        if matches!(
            self.state,
            HolderHorizonState::QuarantinedClockViolation(_) | HolderHorizonState::ClosedOrReturned
        ) {
            return self.state.clone();
        }

        let elapsed = now_holder_mono.get() - self.t_req_holder_mono.get();
        if elapsed >= self.ttl_holder.get() {
            self.state = HolderHorizonState::FencedByShrunkHorizon;
        }
        self.state.clone()
    }

    /// Latches an explicit [`QuarantinedClockViolation`] (e.g. from
    /// [`AuthorityClockHorizonGuard`]) onto this holder lease.
    pub fn quarantine_on_clock_violation(&mut self, violation: QuarantinedClockViolation) {
        self.state = HolderHorizonState::QuarantinedClockViolation(violation);
    }

    /// Evaluates a control-plane RTT sample against `max_clock_skew_ms`,
    /// latching fail-closed quarantine if `ceil(measured_rtt_ms / 2) > max_clock_skew_ms`.
    ///
    /// # Errors
    ///
    /// Returns [`QuarantinedClockViolation`] when the RTT sample violates the
    /// configured skew bound.
    pub fn observe_rtt_sample(
        &mut self,
        measured_rtt_ms: u64,
    ) -> Result<(), QuarantinedClockViolation> {
        if let Err(violation) = self.policy.verify_rtt_sample(measured_rtt_ms) {
            self.state = HolderHorizonState::QuarantinedClockViolation(violation.clone());
            return Err(violation);
        }
        Ok(())
    }

    fn require_active_remaining_budget(
        &mut self,
        now_holder_mono: MonotonicInstantMs,
    ) -> Result<u64, HolderLeaseAdmitError> {
        match self.refresh_state(now_holder_mono) {
            HolderHorizonState::Active => {
                let elapsed = now_holder_mono.get() - self.t_req_holder_mono.get();
                Ok(self.ttl_holder.get() - elapsed)
            }
            HolderHorizonState::FencedByShrunkHorizon => {
                let elapsed = now_holder_mono.get() - self.t_req_holder_mono.get();
                Err(HolderLeaseAdmitError::ShrunkBudgetExpired {
                    elapsed_ms: elapsed,
                    ttl_holder_ms: self.ttl_holder.get(),
                })
            }
            HolderHorizonState::ClosedOrReturned => {
                Err(HolderLeaseAdmitError::AlreadyClosedOrReturned)
            }
            HolderHorizonState::QuarantinedClockViolation(v) => {
                Err(HolderLeaseAdmitError::ClockViolation(v))
            }
        }
    }

    /// Admits a local quota spend / reservation delta strictly inside the
    /// shrunk survival window (`now_holder_mono - t_req_holder_mono < ttl_holder`).
    ///
    /// # Errors
    ///
    /// Refuses fail-closed if the shrunk budget elapsed (`FENCED`), a clock/RTT
    /// violation occurred, the lease is not a `QuotaLease`, or `local_spent + delta > face_value`.
    pub fn admit_local_quota_reserve(
        &mut self,
        now_holder_mono: MonotonicInstantMs,
        delta_units: u64,
    ) -> Result<u64, HolderLeaseAdmitError> {
        let _remaining_ms = self.require_active_remaining_budget(now_holder_mono)?;
        match &mut self.kind {
            HolderLeaseGrantKind::Quota { grant, local_spent } => {
                let next_spent = local_spent.checked_add(delta_units).ok_or(
                    HolderLeaseAdmitError::QuotaExceedsFaceValue {
                        requested_delta: delta_units,
                        current_spent: *local_spent,
                        face_value: grant.face_value(),
                    },
                )?;
                if next_spent > grant.face_value() {
                    return Err(HolderLeaseAdmitError::QuotaExceedsFaceValue {
                        requested_delta: delta_units,
                        current_spent: *local_spent,
                        face_value: grant.face_value(),
                    });
                }
                *local_spent = next_spent;
                Ok(next_spent)
            }
            other => Err(HolderLeaseAdmitError::WrongLeaseFamily {
                actual: match other {
                    HolderLeaseGrantKind::Quota { .. } => CrossHostLeaseFamily::Quota,
                    HolderLeaseGrantKind::Capacity { .. } => CrossHostLeaseFamily::Capacity,
                    HolderLeaseGrantKind::ExclusiveDevice { .. } => {
                        CrossHostLeaseFamily::ExclusiveDevice
                    }
                },
            }),
        }
    }

    /// Mints a short-lived local [`HolderCommitPermit`] iff the holder lease is
    /// strictly inside its shrunk survival window (`ADR-0026` §2.3).
    ///
    /// # Errors
    ///
    /// Fails closed if `FENCED`, closed, or quarantined by a clock violation.
    pub fn admit_commit_permit(
        &mut self,
        now_holder_mono: MonotonicInstantMs,
        permit_seq: u64,
    ) -> Result<HolderCommitPermit, HolderLeaseAdmitError> {
        let remaining_shrunk_budget_ms = self.require_active_remaining_budget(now_holder_mono)?;
        let (lease_id, epoch, fencing_token) = self.lease_axes();
        Ok(HolderCommitPermit {
            lease_id,
            epoch,
            fencing_token,
            permit_seq,
            remaining_shrunk_budget_ms,
        })
    }

    /// Mints a short-lived local [`HolderEffectPermit`] for an irreversible
    /// side effect iff the holder lease is strictly inside its shrunk survival
    /// window (`ADR-0026` §2.3).
    ///
    /// # Errors
    ///
    /// Fails closed if `FENCED`, closed, or quarantined by a clock violation.
    pub fn admit_irreversible_effect_permit(
        &mut self,
        now_holder_mono: MonotonicInstantMs,
        effect_digest: [u8; 32],
    ) -> Result<HolderEffectPermit, HolderLeaseAdmitError> {
        let remaining_shrunk_budget_ms = self.require_active_remaining_budget(now_holder_mono)?;
        let (lease_id, epoch, fencing_token) = self.lease_axes();
        Ok(HolderEffectPermit {
            lease_id,
            epoch,
            fencing_token,
            effect_digest,
            remaining_shrunk_budget_ms,
        })
    }

    /// Applies a verified grantor [`QuotaLeaseGrant`] renewal response
    /// (`[LEASE-RENEW-001]`): re-anchors `t_req_holder_mono = t_renew_req_holder_mono`
    /// and updates `ttl_holder` while verifying `face_value` is **unchanged**.
    ///
    /// # Errors
    ///
    /// Fails closed if the lease is closed or clock-quarantined, if `confirmed_face_value`
    /// differs from the original `face_value`, if `new_renewal_seq <= renewal_seq`,
    /// or if RTT/horizon bounds fail.
    pub fn apply_verified_renewal(
        &mut self,
        t_renew_req_holder_mono: MonotonicInstantMs,
        t_renew_recv_holder_mono: MonotonicInstantMs,
        new_ttl_grant_ms: u64,
        new_renewal_seq: u64,
        confirmed_face_value: u64,
    ) -> Result<(), HolderLeaseAdmitError> {
        if let HolderHorizonState::QuarantinedClockViolation(v) = &self.state {
            return Err(HolderLeaseAdmitError::ClockViolation(v.clone()));
        }
        if matches!(self.state, HolderHorizonState::ClosedOrReturned) {
            return Err(HolderLeaseAdmitError::AlreadyClosedOrReturned);
        }
        let HolderLeaseGrantKind::Quota { grant, .. } = &self.kind else {
            return Err(HolderLeaseAdmitError::WrongLeaseFamily {
                actual: self.family(),
            });
        };
        if confirmed_face_value != grant.face_value() {
            return Err(HolderLeaseAdmitError::InvalidRenewal {
                reason: "LEASE-RENEW-001 forbids implicit face_value mutation during renewal",
            });
        }
        if new_renewal_seq <= self.renewal_seq {
            return Err(HolderLeaseAdmitError::InvalidRenewal {
                reason: "renewal_seq must strictly advance",
            });
        }
        if t_renew_req_holder_mono < self.t_req_holder_mono
            || t_renew_req_holder_mono < self.last_observed_mono
        {
            let v = QuarantinedClockViolation::LocalMonotonicRollback {
                previous: self.last_observed_mono,
                observed: t_renew_req_holder_mono,
            };
            self.state = HolderHorizonState::QuarantinedClockViolation(v.clone());
            return Err(HolderLeaseAdmitError::ClockViolation(v));
        }
        let rtt = t_renew_recv_holder_mono
            .elapsed_since(t_renew_req_holder_mono)
            .map_err(|v| {
                self.state = HolderHorizonState::QuarantinedClockViolation(v.clone());
                HolderLeaseAdmitError::ClockViolation(v)
            })?;
        self.policy.verify_rtt_sample(rtt.get()).map_err(|v| {
            self.state = HolderHorizonState::QuarantinedClockViolation(v.clone());
            HolderLeaseAdmitError::ClockViolation(v)
        })?;
        let new_ttl_holder = self
            .policy
            .validate_and_compute_holder_budget(new_ttl_grant_ms)
            .map_err(HolderLeaseAdmitError::Horizon)?;

        self.t_req_holder_mono = t_renew_req_holder_mono;
        self.ttl_grant_ms = new_ttl_grant_ms;
        self.ttl_holder = new_ttl_holder;
        self.renewal_seq = new_renewal_seq;
        self.state = HolderHorizonState::Active;
        let _ = self.refresh_state(t_renew_recv_holder_mono);
        Ok(())
    }

    /// Freezes local quota usage (`ClosedOrReturned`) and signs an Ed25519
    /// [`SignedLeaseCloseReceipt`] committing `final_high_water_spent` and
    /// `returned_unspent = face_value - final_high_water_spent`.
    ///
    /// # Errors
    ///
    /// Fails closed on monotonic rollback, non-Quota family, or custody signing
    /// failure.
    pub fn sign_quota_close_receipt(
        &mut self,
        now_holder_mono: MonotonicInstantMs,
        receipt_id: ReceiptId,
        signer: &dyn PeerDigestSigner,
        signed_at_audit_hint: AuditWallTimeHint,
    ) -> Result<SignedLeaseCloseReceipt, HolderLeaseAdmitError> {
        let _ = self.refresh_state(now_holder_mono);
        if let HolderHorizonState::QuarantinedClockViolation(v) = &self.state {
            return Err(HolderLeaseAdmitError::ClockViolation(v.clone()));
        }
        let HolderLeaseGrantKind::Quota { grant, local_spent } = self.kind else {
            return Err(HolderLeaseAdmitError::WrongLeaseFamily {
                actual: self.family(),
            });
        };
        let returned_unspent = grant.face_value() - local_spent;
        let signer_principal_id = signer.principal_id();
        let signer_key_id = signer.key_id();
        let signer_key_generation = signer.key_generation();
        let digest = compute_close_receipt_digest(
            receipt_id,
            grant.lease_id(),
            grant.cell(),
            grant.node_boot_generation(),
            grant.epoch(),
            grant.fencing_token(),
            grant.face_value(),
            local_spent,
            returned_unspent,
            signer_principal_id,
            signer_key_id,
            signer_key_generation,
        );
        let ed25519_signature = signer
            .sign_domain_digest(
                SIGNED_CLOSE_RECEIPT_DOMAIN,
                digest,
                signed_at_audit_hint.unix_epoch_ms(),
            )
            .map_err(|err| {
                HolderLeaseAdmitError::Horizon(ClockHorizonError::PeerSignatureRejected(err))
            })?;
        self.state = HolderHorizonState::ClosedOrReturned;
        Ok(SignedLeaseCloseReceipt {
            receipt_id,
            lease_id: grant.lease_id(),
            holder_cell: grant.cell(),
            holder_boot_generation: grant.node_boot_generation(),
            epoch: grant.epoch(),
            fencing_token: grant.fencing_token(),
            face_value: grant.face_value(),
            final_high_water_spent: local_spent,
            returned_unspent,
            signer_principal_id,
            signer_key_id,
            signer_key_generation,
            ed25519_signature,
        })
    }

    /// Marks a `Capacity` or `ExclusiveDevice` holder handle as locally
    /// returned/closed so no further local `CommitPermit` / `EffectPermit` can
    /// be minted.
    pub fn mark_returned_locally(&mut self) {
        if !matches!(self.state, HolderHorizonState::QuarantinedClockViolation(_)) {
            self.state = HolderHorizonState::ClosedOrReturned;
        }
    }

    const fn lease_axes(&self) -> ([u8; 16], CellEpoch, CellFencingToken) {
        match self.kind {
            HolderLeaseGrantKind::Quota { grant, .. } => (
                *grant.lease_id().as_bytes(),
                grant.epoch(),
                grant.fencing_token(),
            ),
            HolderLeaseGrantKind::Capacity { grant } => (
                *grant.capacity_lease_id().as_bytes(),
                grant.capacity_epoch(),
                grant.fencing_token(),
            ),
            HolderLeaseGrantKind::ExclusiveDevice { grant } => (
                *grant.device_lease_id().as_bytes(),
                grant.exclusivity_epoch(),
                grant.fencing_token(),
            ),
        }
    }
}

/// Durable grantor-side horizon metadata tracked for each cross-host lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantorHorizonRecord {
    /// Lease family.
    pub family: CrossHostLeaseFamily,
    /// 16-byte lease identifier.
    pub lease_id: [u8; 16],
    /// Remote holder `CellIdentity`.
    pub holder_cell: CellIdentity,
    /// Remote holder `node_boot_generation`.
    pub holder_boot_generation: Generation,
    /// Grantor-local monotonic commit instant `t_grant_grantor_mono` (or latest
    /// renewal commit instant).
    pub grantor_commit_mono: MonotonicInstantMs,
    /// Nominal granted TTL (`ms`).
    pub ttl_grant_ms: u64,
    /// Conservative clock-skew policy bound to this lease.
    pub policy: ClockSkewPolicy,
    /// Logical `AuthorityClock` expiry tick bound for protocol admission.
    pub authority_expiry_tick: u64,
    /// Monotonic renewal counter (`0` on initial grant).
    pub renewal_seq: u64,
    /// Multi-endpoint enforcement scope requiring 100% barrier coverage.
    pub multi_endpoint_scope: MultiEndpointFenceScope,
    /// Honest ledger vs physical cleanup disposition (`ADR-0019` R2.3 #2).
    pub disposition: QuarantineDisposition,
}

impl GrantorHorizonRecord {
    pub(crate) fn to_stored_row(&self) -> StoredHorizonRow {
        let (disposition_code, cleanup_proof_kind, cleanup_proof_digest) =
            self.disposition.encode();
        StoredHorizonRow {
            family: self.family.to_stored(),
            lease_id: self.lease_id,
            holder_cell: self.holder_cell,
            holder_boot_generation: self.holder_boot_generation,
            grantor_commit_mono_ms: self.grantor_commit_mono.get(),
            ttl_grant_ms: self.ttl_grant_ms,
            max_clock_skew_ms: self.policy.max_clock_skew_ms,
            safety_margin_ms: self.policy.safety_margin_ms,
            authority_expiry_tick: self.authority_expiry_tick,
            renewal_seq: self.renewal_seq,
            multi_endpoint_gateways: self
                .multi_endpoint_scope
                .required_gateways()
                .iter()
                .copied()
                .collect(),
            disposition_code,
            cleanup_proof_kind,
            cleanup_proof_digest,
        }
    }

    pub(crate) fn from_stored_row(row: StoredHorizonRow) -> Result<Self, LeaseStoreError> {
        let multi_endpoint_scope =
            MultiEndpointFenceScope::with_gateways(row.holder_cell, row.multi_endpoint_gateways)
                .map_err(|_| {
                    LeaseStoreError::CorruptRecord("corrupt MultiEndpointFenceScope in SQLite")
                })?;
        let disposition = QuarantineDisposition::decode(
            row.disposition_code,
            row.cleanup_proof_kind,
            row.cleanup_proof_digest,
        )?;
        Ok(Self {
            family: CrossHostLeaseFamily::from_stored(row.family),
            lease_id: row.lease_id,
            holder_cell: row.holder_cell,
            holder_boot_generation: row.holder_boot_generation,
            grantor_commit_mono: MonotonicInstantMs::new(row.grantor_commit_mono_ms),
            ttl_grant_ms: row.ttl_grant_ms,
            policy: ClockSkewPolicy::new(row.max_clock_skew_ms, row.safety_margin_ms),
            authority_expiry_tick: row.authority_expiry_tick,
            renewal_seq: row.renewal_seq,
            multi_endpoint_scope,
            disposition,
        })
    }
}

/// Outcome of evaluating an unreachable holder's lease against the grantor's
/// conservative delayed-expiry floor (`ADR-0026` §2.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrantorHorizonStatus {
    /// `elapsed_grantor_mono <= ttl_grant_ms + max_clock_skew_ms`: the grantor
    /// **must wait** and refuses to declare the lease expired yet.
    StillWithinSkewEnvelope {
        /// Elapsed milliseconds on grantor's monotonic clock since `grantor_commit_mono`.
        elapsed_ms: u64,
        /// Conservative floor `ttl_grant_ms + max_clock_skew_ms` that `elapsed_ms`
        /// must strictly exceed.
        required_strictly_greater_than_ms: u64,
        /// Minimum additional milliseconds (`>= 1`) before sweep is permitted.
        remaining_wait_ms: u64,
    },
    /// `elapsed_grantor_mono > ttl_grant_ms + max_clock_skew_ms`: lease moved
    /// strictly into **`QUARANTINED`** (`AVAILABLE` / `pool_remaining` / `FREE`
    /// untouched until cryptographic receipt or epoch + 100% barrier proof).
    QuarantinedAfterSkewHorizon {
        /// Quarantine disposition (`LedgerQuarantinedFenceOnly` until physical
        /// cleanup proof arrives).
        disposition: QuarantineDisposition,
    },
    /// Lease was already terminal (`CLOSED`, `CANCELLED`, `RETURNED`).
    AlreadySettled,
}

/// Coverage snapshot of [`FenceBarrierAck`]s collected for a lease's
/// [`MultiEndpointFenceScope`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FenceScopeCoverageStatus {
    /// Count of verified gateway ACKs recorded so far.
    pub acked_count: usize,
    /// Total gateways required by [`MultiEndpointFenceScope`].
    pub required_count: usize,
    /// Ordered list of gateways still missing (`empty` iff 100% covered).
    pub missing_gateways: Vec<[u8; 16]>,
}

impl FenceScopeCoverageStatus {
    /// Returns `true` iff 100% of required gateways have acknowledged.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.missing_gateways.is_empty()
    }
}

/// Fail-closed errors surfaced by [`ClockSkewPolicy`] and
/// [`GrantorClockBoundedCoordinator`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClockHorizonError {
    /// `ttl_grant_ms <= 2 * max_clock_skew_ms + safety_margin_ms` (`ADR-0026` §2.3).
    InvalidLeaseHorizon {
        /// Requested `ttl_grant_ms`.
        ttl_grant_ms: u64,
        /// Configured `max_clock_skew_ms`.
        max_clock_skew_ms: u64,
        /// Configured `safety_margin_ms`.
        safety_margin_ms: u64,
        /// Exclusive floor `2 * max_clock_skew_ms + safety_margin_ms`.
        required_strictly_greater_than_ms: u64,
    },
    /// Arithmetic overflow computing horizon bounds.
    HorizonArithmeticOverflow,
    /// Invalid [`MultiEndpointFenceScope`] configuration.
    InvalidMultiEndpointScope(&'static str),
    /// Clock rollback or `RTT/2 > max_clock_skew` violation.
    ClockViolation(QuarantinedClockViolation),
    /// Authority `CellFence` check failed.
    AuthorityFence(CellAdmitError),
    /// `CellAuthority` epoch advance error.
    CellAuthority(CellError),
    /// Caller `CellIdentity` does not match the lease's bound holder `CellIdentity`.
    HolderCellImpersonation {
        /// Authenticated caller `CellIdentity`.
        caller_cell: CellIdentity,
        /// Bound lease holder `CellIdentity`.
        expected_holder_cell: CellIdentity,
    },
    /// Signer `PrincipalId` does not match the `PrincipalId` pinned for `holder_cell`.
    HolderPrincipalMismatch {
        /// Holder `CellIdentity`.
        holder_cell: CellIdentity,
        /// Pinned `PrincipalId`.
        expected_principal: PrincipalId,
        /// Presented `PrincipalId`.
        presented_principal: PrincipalId,
    },
    /// Peer Ed25519 signature or custody verification failed.
    PeerSignatureRejected(PeerIdentityError),
    /// Close receipt failed `final_high_water_spent + returned_unspent == face_value`.
    CloseReceiptConservationMismatch {
        /// Lease `face_value`.
        face_value: u64,
        /// Reported spent high-water.
        final_high_water_spent: u64,
        /// Reported returned remainder.
        returned_unspent: u64,
    },
    /// Conflicting close receipt replayed for an already closed lease.
    ConflictingCloseReceipt {
        /// Committed `ReceiptId`.
        existing: ReceiptId,
        /// Requested `ReceiptId`.
        requested: ReceiptId,
    },
    /// Unknown cross-host lease horizon record.
    UnknownLeaseHorizon {
        /// Lease family.
        family: CrossHostLeaseFamily,
        /// 16-byte lease identifier.
        lease_id: [u8; 16],
    },
    /// Unknown `DeviceId` head on coordinator.
    UnknownDeviceHead(DeviceId),
    /// Conflicting parameters replayed on an existing cross-host grant.
    ConflictingCrossHostGrant(&'static str),
    /// Invalid `FenceBarrierAck` or gateway not in [`MultiEndpointFenceScope`].
    InvalidFenceBarrierAck(&'static str),
    /// Barrier-only reconciliation attempted before `(epoch, fencing_token)`
    /// strictly advanced past the quarantined lease's era (`ADR-0026` §2.3).
    EpochAdvanceRequiredForBarrierReconcile {
        /// Lease's bound `CellEpoch`.
        lease_epoch: CellEpoch,
        /// Coordinator's current `CellEpoch`.
        current_epoch: CellEpoch,
        /// Lease's bound `CellFencingToken`.
        lease_token: CellFencingToken,
        /// Coordinator's current `CellFencingToken`.
        current_token: CellFencingToken,
    },
    /// Barrier-only reconciliation attempted with `< 100%` gateway coverage on
    /// [`MultiEndpointFenceScope`] (`[LEASE-FENCE-001]`).
    IncompleteMultiEndpointFenceBarrier {
        /// Count of gateways acknowledged so far.
        acked_count: usize,
        /// Required gateway count.
        required_count: usize,
        /// Gateways still missing `FenceBarrierAck`.
        missing_gateways: Vec<[u8; 16]>,
    },
    ///Underlying `QuotaLease` grant error.
    QuotaGrant(QuotaLeaseGrantError),
    /// Underlying `QuotaLease` ledger error.
    QuotaLedger(QuotaLeaseLedgerError),
    /// Underlying `QuotaLease` reconcile error.
    QuotaReconcile(QuotaReconcileError),
    /// Underlying `CapacityLease` grant error.
    CapacityGrant(CapacityLeaseGrantError),
    /// Underlying `CapacityLease` return/attach error.
    CapacityReturn(CapacityLeaseReturnError),
    /// Underlying `CapacityLease` reconcile error.
    CapacityReconcile(CapacityReconcileError),
    /// Underlying `ExclusiveDeviceLease` grant error.
    DeviceGrant(ExclusiveDeviceLeaseGrantError),
    /// Underlying `ExclusiveDeviceLease` return/reset error.
    DeviceReturn(ExclusiveDeviceLeaseReturnError),
    /// Underlying `ExclusiveDeviceLease` reconcile error.
    DeviceReconcile(DeviceReconcileError),
    /// Durable `SQLite` storage error.
    Storage(&'static str),
}

impl From<CellAdmitError> for ClockHorizonError {
    fn from(err: CellAdmitError) -> Self {
        Self::AuthorityFence(err)
    }
}

impl From<LeaseStoreError> for ClockHorizonError {
    fn from(err: LeaseStoreError) -> Self {
        Self::Storage(err.reason())
    }
}

impl fmt::Display for ClockHorizonError {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLeaseHorizon {
                ttl_grant_ms,
                max_clock_skew_ms,
                safety_margin_ms,
                required_strictly_greater_than_ms,
            } => write!(
                formatter,
                "invalid cross-host lease horizon: ttl_grant ({ttl_grant_ms} ms) must exceed 2*max_clock_skew ({max_clock_skew_ms} ms) + safety_margin ({safety_margin_ms} ms) = {required_strictly_greater_than_ms} ms"
            ),
            Self::HorizonArithmeticOverflow => {
                formatter.write_str("clock horizon arithmetic overflow")
            }
            Self::InvalidMultiEndpointScope(reason) => {
                write!(formatter, "invalid multi-endpoint fence scope: {reason}")
            }
            Self::ClockViolation(v) => write!(formatter, "{v}"),
            Self::AuthorityFence(e) => write!(formatter, "authority fence rejected: {e}"),
            Self::CellAuthority(e) => write!(formatter, "CellAuthority error: {e}"),
            Self::HolderCellImpersonation {
                caller_cell,
                expected_holder_cell,
            } => write!(
                formatter,
                "holder Cell impersonation rejected: caller {caller_cell:?} != lease holder {expected_holder_cell:?}"
            ),
            Self::HolderPrincipalMismatch {
                holder_cell,
                expected_principal,
                presented_principal,
            } => write!(
                formatter,
                "holder PrincipalId mismatch for {holder_cell:?}: presented {presented_principal:?} != pinned {expected_principal:?}"
            ),
            Self::PeerSignatureRejected(e) => write!(formatter, "peer signature rejected: {e}"),
            Self::CloseReceiptConservationMismatch {
                face_value,
                final_high_water_spent,
                returned_unspent,
            } => write!(
                formatter,
                "SignedLeaseCloseReceipt conservation mismatch: {final_high_water_spent} + {returned_unspent} != {face_value}"
            ),
            Self::ConflictingCloseReceipt {
                existing,
                requested,
            } => write!(
                formatter,
                "conflicting SignedLeaseCloseReceipt: existing {existing:?} != requested {requested:?}"
            ),
            Self::UnknownLeaseHorizon { family, lease_id } => {
                write!(
                    formatter,
                    "unknown cross-host {family:?} lease {lease_id:02x?}"
                )
            }
            Self::UnknownDeviceHead(id) => write!(formatter, "unknown DeviceId head {id:?}"),
            Self::ConflictingCrossHostGrant(reason) => {
                write!(formatter, "conflicting cross-host grant: {reason}")
            }
            Self::InvalidFenceBarrierAck(reason) => {
                write!(formatter, "invalid FenceBarrierAck: {reason}")
            }
            Self::EpochAdvanceRequiredForBarrierReconcile {
                lease_epoch,
                current_epoch,
                lease_token,
                current_token,
            } => write!(
                formatter,
                "barrier reconciliation requires strictly advanced (epoch, token): current ({}, {}) <= lease ({}, {})",
                current_epoch.get(),
                current_token.get(),
                lease_epoch.get(),
                lease_token.get()
            ),
            Self::IncompleteMultiEndpointFenceBarrier {
                acked_count,
                required_count,
                ..
            } => write!(
                formatter,
                "incomplete MultiEndpointFenceScope barrier coverage: {acked_count}/{required_count} gateways acknowledged"
            ),
            Self::QuotaGrant(e) => write!(formatter, "{e}"),
            Self::QuotaLedger(e) => write!(formatter, "{e}"),
            Self::QuotaReconcile(e) => write!(formatter, "{e}"),
            Self::CapacityGrant(e) => write!(formatter, "{e}"),
            Self::CapacityReturn(e) => write!(formatter, "{e}"),
            Self::CapacityReconcile(e) => write!(formatter, "{e}"),
            Self::DeviceGrant(e) => write!(formatter, "{e}"),
            Self::DeviceReturn(e) => write!(formatter, "{e}"),
            Self::DeviceReconcile(e) => write!(formatter, "{e}"),
            Self::Storage(reason) => write!(formatter, "cross-host lease store error: {reason}"),
        }
    }
}

impl Error for ClockHorizonError {}

/// Parameters for granting a cross-host lease with conservative skew bounds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CrossHostGrantParams {
    /// Authenticated remote holder `CellIdentity`.
    pub holder_cell: CellIdentity,
    /// Authenticated remote holder `node_boot_generation`.
    pub holder_boot_generation: Generation,
    /// Grantor-local monotonic instant at commit.
    pub grantor_commit_mono: MonotonicInstantMs,
    /// Nominal granted TTL (`ms`), validated against `policy`.
    pub ttl_grant_ms: u64,
    /// Conservative clock-skew envelope policy.
    pub policy: ClockSkewPolicy,
    /// Logical `AuthorityClock` expiry tick bound for protocol admission.
    pub authority_expiry_tick: u64,
    /// Multi-endpoint gateway scope requiring 100% barrier coverage.
    pub multi_endpoint_scope: MultiEndpointFenceScope,
}

/// Grantor-side three-family lease coordinator enforcing conservative clock-skew
/// expiry floors, `QUARANTINED`-only sweeps, 100% [`MultiEndpointFenceScope`]
/// barrier coverage, and honest [`QuarantineDisposition`] tracking
/// (`ADR-0026` §2.3, `ADR-0019` R2.3 #2).
#[derive(Debug)]
pub struct GrantorClockBoundedCoordinator {
    authority: Arc<CellAuthority>,
    quota: QuotaLeaseGrantor,
    capacity: CapacityLeaseGrantor,
    devices: BTreeMap<DeviceId, ExclusiveDeviceLeaseGrantor>,
    store: Option<Arc<SqliteLeaseStore>>,
    horizons: BTreeMap<(CrossHostLeaseFamily, [u8; 16]), GrantorHorizonRecord>,
    barrier_acks: BTreeMap<(CrossHostLeaseFamily, [u8; 16]), BTreeMap<[u8; 16], FenceBarrierAck>>,
    close_receipts: BTreeMap<QuotaLeaseId, SignedLeaseCloseReceipt>,
    pinned_holder_principals: BTreeMap<CellIdentity, PrincipalId>,
    last_grantor_mono: Option<MonotonicInstantMs>,
}

impl GrantorClockBoundedCoordinator {
    /// Opens an in-memory three-family cross-host lease coordinator.
    #[must_use]
    pub fn open(
        authority: Arc<CellAuthority>,
        available_quota: u64,
        initial_capacity: u64,
        device_ids: &[DeviceId],
    ) -> Self {
        let mut devices = BTreeMap::new();
        for &device_id in device_ids {
            devices.insert(
                device_id,
                ExclusiveDeviceLeaseGrantor::open(Arc::clone(&authority), device_id),
            );
        }
        Self {
            quota: QuotaLeaseGrantor::open(Arc::clone(&authority), available_quota),
            capacity: CapacityLeaseGrantor::open(Arc::clone(&authority), initial_capacity),
            devices,
            authority,
            store: None,
            horizons: BTreeMap::new(),
            barrier_acks: BTreeMap::new(),
            close_receipts: BTreeMap::new(),
            pinned_holder_principals: BTreeMap::new(),
            last_grantor_mono: None,
        }
    }

    /// Opens a durable `SQLite` (`WAL + FULL`) three-family cross-host lease
    /// coordinator at `path`, rehydrating all pools, device heads, horizons,
    /// `FenceBarrierAck` sets, and signed close receipts on reopen.
    ///
    /// # Errors
    ///
    /// Fails closed on `SQLite` durability failure or tampered/corrupt state.
    pub fn open_durable(
        path: impl AsRef<Path>,
        authority: Arc<CellAuthority>,
        available_quota: u64,
        initial_capacity: u64,
        device_ids: &[DeviceId],
    ) -> Result<Self, LeaseStoreError> {
        let store = Arc::new(SqliteLeaseStore::open(path)?);
        Self::open_with_store(
            store,
            authority,
            available_quota,
            initial_capacity,
            device_ids,
        )
    }

    /// Opens a durable coordinator over an existing shared [`SqliteLeaseStore`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::open_durable`].
    pub fn open_with_store(
        store: Arc<SqliteLeaseStore>,
        authority: Arc<CellAuthority>,
        available_quota: u64,
        initial_capacity: u64,
        device_ids: &[DeviceId],
    ) -> Result<Self, LeaseStoreError> {
        let quota = QuotaLeaseGrantor::open_with_store(
            Arc::clone(&store),
            Arc::clone(&authority),
            available_quota,
        )?;
        let capacity = CapacityLeaseGrantor::open_with_store(
            Arc::clone(&store),
            Arc::clone(&authority),
            initial_capacity,
        )?;
        let mut devices = BTreeMap::new();
        for &device_id in device_ids {
            let grantor = ExclusiveDeviceLeaseGrantor::open_with_store(
                Arc::clone(&store),
                Arc::clone(&authority),
                device_id,
            )?;
            devices.insert(device_id, grantor);
        }
        let rehydrated = store.ensure_and_rehydrate_cross_host()?;
        let mut horizons = BTreeMap::new();
        for (_, row) in rehydrated.horizons {
            let rec = GrantorHorizonRecord::from_stored_row(row)?;
            horizons.insert((rec.family, rec.lease_id), rec);
        }
        let mut barrier_acks = BTreeMap::new();
        for ((fam, id), acks) in rehydrated.barrier_acks {
            barrier_acks.insert((CrossHostLeaseFamily::from_stored(fam), id), acks);
        }
        let mut close_receipts = BTreeMap::new();
        for (lease_id, row) in rehydrated.close_receipts {
            close_receipts.insert(lease_id, SignedLeaseCloseReceipt::from_stored_row(row));
        }
        let last_grantor_mono = horizons.values().map(|h| h.grantor_commit_mono).max();

        Ok(Self {
            authority,
            quota,
            capacity,
            devices,
            store: Some(store),
            horizons,
            barrier_acks,
            close_receipts,
            pinned_holder_principals: BTreeMap::new(),
            last_grantor_mono,
        })
    }

    /// Shared `CellAuthority` governing this coordinator.
    #[must_use]
    pub const fn authority(&self) -> &Arc<CellAuthority> {
        &self.authority
    }

    /// Current authority `CellFence`.
    #[must_use]
    pub fn current_fence(&self) -> CellFence {
        self.authority.fence()
    }

    /// Underlying durable store, if opened durably.
    #[must_use]
    pub const fn store(&self) -> Option<&Arc<SqliteLeaseStore>> {
        self.store.as_ref()
    }

    /// Read-only view of the `QuotaLeaseGrantor`.
    #[must_use]
    pub const fn quota(&self) -> &QuotaLeaseGrantor {
        &self.quota
    }

    /// Read-only view of the `CapacityLeaseGrantor`.
    #[must_use]
    pub const fn capacity(&self) -> &CapacityLeaseGrantor {
        &self.capacity
    }

    /// Read-only view of the `ExclusiveDeviceLeaseGrantor` for `device_id`.
    #[must_use]
    pub fn device(&self, device_id: DeviceId) -> Option<&ExclusiveDeviceLeaseGrantor> {
        self.devices.get(&device_id)
    }

    /// Pins the authoritative `PrincipalId` allowed to sign receipts on behalf
    /// of `holder_cell` (`[TCB-ENDPOINT-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed if a different `PrincipalId` was already pinned for `holder_cell`.
    pub fn pin_holder_principal(
        &mut self,
        holder_cell: CellIdentity,
        principal_id: PrincipalId,
    ) -> Result<(), ClockHorizonError> {
        if let Some(&existing) = self.pinned_holder_principals.get(&holder_cell)
            && existing != principal_id
        {
            return Err(ClockHorizonError::HolderPrincipalMismatch {
                holder_cell,
                expected_principal: existing,
                presented_principal: principal_id,
            });
        }
        self.pinned_holder_principals
            .insert(holder_cell, principal_id);
        Ok(())
    }

    /// Looks up the horizon record for `(family, lease_id)`.
    #[must_use]
    pub fn horizon_record(
        &self,
        family: CrossHostLeaseFamily,
        lease_id: [u8; 16],
    ) -> Option<&GrantorHorizonRecord> {
        self.horizons.get(&(family, lease_id))
    }

    /// Returns the honest [`QuarantineDisposition`] of `(family, lease_id)`.
    ///
    /// # Errors
    ///
    /// Returns [`ClockHorizonError::UnknownLeaseHorizon`] if unknown.
    pub fn quarantine_disposition(
        &self,
        family: CrossHostLeaseFamily,
        lease_id: [u8; 16],
    ) -> Result<QuarantineDisposition, ClockHorizonError> {
        self.horizons
            .get(&(family, lease_id))
            .map(|r| r.disposition)
            .ok_or(ClockHorizonError::UnknownLeaseHorizon { family, lease_id })
    }

    /// Returns current [`FenceScopeCoverageStatus`] for `(family, lease_id)`.
    ///
    /// # Errors
    ///
    /// Returns [`ClockHorizonError::UnknownLeaseHorizon`] if unknown.
    pub fn fence_barrier_coverage(
        &self,
        family: CrossHostLeaseFamily,
        lease_id: [u8; 16],
    ) -> Result<FenceScopeCoverageStatus, ClockHorizonError> {
        let rec = self
            .horizons
            .get(&(family, lease_id))
            .ok_or(ClockHorizonError::UnknownLeaseHorizon { family, lease_id })?;
        let empty = BTreeMap::new();
        let acked = self.barrier_acks.get(&(family, lease_id)).unwrap_or(&empty);
        let missing = rec.multi_endpoint_scope.missing_gateways(acked);
        Ok(FenceScopeCoverageStatus {
            acked_count: rec.multi_endpoint_scope.required_gateways().len() - missing.len(),
            required_count: rec.multi_endpoint_scope.required_gateways().len(),
            missing_gateways: missing,
        })
    }

    fn observe_grantor_mono(
        &mut self,
        now_grantor_mono: MonotonicInstantMs,
    ) -> Result<(), QuarantinedClockViolation> {
        if let Some(prev) = self.last_grantor_mono
            && now_grantor_mono < prev
        {
            return Err(QuarantinedClockViolation::LocalMonotonicRollback {
                previous: prev,
                observed: now_grantor_mono,
            });
        }
        self.last_grantor_mono = Some(now_grantor_mono);
        Ok(())
    }

    fn verify_grant_params(params: &CrossHostGrantParams) -> Result<(), ClockHorizonError> {
        let _ = params
            .policy
            .validate_and_compute_holder_budget(params.ttl_grant_ms)?;
        if params.multi_endpoint_scope.scope_cell() != params.holder_cell {
            return Err(ClockHorizonError::InvalidMultiEndpointScope(
                "MultiEndpointFenceScope.scope_cell must match holder_cell",
            ));
        }
        Ok(())
    }

    /// Grants and activates a cross-host `QuotaLease` bound to `params`.
    ///
    /// Replaying the identical `(lease_id, face_value, params)` is idempotent
    /// and never deducts `AVAILABLE` twice.
    ///
    /// # Errors
    ///
    /// Fails closed on invalid `ttl_grant_ms`, stale `CellFence`, insufficient
    /// `AVAILABLE`, grantor monotonic regression, or conflicting parameters.
    pub fn grant_quota_cross_host(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
        face_value: u64,
        params: CrossHostGrantParams,
    ) -> Result<(QuotaLeaseGrant, GrantorHorizonRecord), ClockHorizonError> {
        Self::verify_grant_params(&params)?;
        let id_bytes = *lease_id.as_bytes();
        if let Some(existing_rec) = self
            .horizons
            .get(&(CrossHostLeaseFamily::Quota, id_bytes))
            .cloned()
        {
            let existing_lease = self
                .quota
                .query(lease_id)
                .map_err(ClockHorizonError::QuotaLedger)?;
            if existing_lease.face_value() == face_value
                && existing_rec.holder_cell == params.holder_cell
                && existing_rec.holder_boot_generation == params.holder_boot_generation
                && existing_rec.ttl_grant_ms == params.ttl_grant_ms
                && existing_rec.policy == params.policy
                && existing_rec.multi_endpoint_scope == params.multi_endpoint_scope
            {
                return Ok((existing_lease, existing_rec));
            }
            return Err(ClockHorizonError::ConflictingCrossHostGrant(
                "conflicting QuotaLease grant parameters for existing lease_id",
            ));
        }

        self.observe_grantor_mono(params.grantor_commit_mono)
            .map_err(ClockHorizonError::ClockViolation)?;
        let _issued = self
            .quota
            .grant_for_holder(
                presented,
                params.holder_cell,
                params.holder_boot_generation,
                lease_id,
                face_value,
                None,
            )
            .map_err(ClockHorizonError::QuotaGrant)?;
        let active = self
            .quota
            .activate(presented, lease_id)
            .map_err(ClockHorizonError::QuotaLedger)?;

        let record = GrantorHorizonRecord {
            family: CrossHostLeaseFamily::Quota,
            lease_id: id_bytes,
            holder_cell: params.holder_cell,
            holder_boot_generation: params.holder_boot_generation,
            grantor_commit_mono: params.grantor_commit_mono,
            ttl_grant_ms: params.ttl_grant_ms,
            policy: params.policy,
            authority_expiry_tick: params.authority_expiry_tick,
            renewal_seq: 0,
            multi_endpoint_scope: params.multi_endpoint_scope,
            disposition: QuarantineDisposition::ActiveOrSettled,
        };
        if let Some(store) = &self.store {
            store.persist_cross_host_horizon(&record.to_stored_row())?;
        }
        self.horizons
            .insert((CrossHostLeaseFamily::Quota, id_bytes), record.clone());
        Ok((active, record))
    }

    /// Renews a cross-host `QuotaLease` (`[LEASE-RENEW-001]`): validates and
    /// commits `new_ttl_grant_ms` at `now_grantor_mono` and increments
    /// `renewal_seq` **without** increasing or altering `face_value`.
    ///
    /// # Errors
    ///
    /// Fails closed if `caller_cell != lease.cell()`, if the lease is not
    /// `ACTIVE`, if `new_ttl_grant_ms` violates the skew envelope, or if
    /// `now_grantor_mono` regressed.
    pub fn renew_quota_cross_host(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
        caller_cell: CellIdentity,
        now_grantor_mono: MonotonicInstantMs,
        new_ttl_grant_ms: u64,
        new_authority_expiry_tick: u64,
    ) -> Result<(QuotaLeaseGrant, GrantorHorizonRecord), ClockHorizonError> {
        self.authority.admit(presented)?;
        let id_bytes = *lease_id.as_bytes();
        let existing_lease = self
            .quota
            .query(lease_id)
            .map_err(ClockHorizonError::QuotaLedger)?;
        if existing_lease.cell() != caller_cell {
            return Err(ClockHorizonError::HolderCellImpersonation {
                caller_cell,
                expected_holder_cell: existing_lease.cell(),
            });
        }
        if existing_lease.state() != QuotaLeaseState::Active {
            return Err(ClockHorizonError::QuotaLedger(
                QuotaLeaseLedgerError::NotActive {
                    state: existing_lease.state(),
                },
            ));
        }
        let rec = self
            .horizons
            .get(&(CrossHostLeaseFamily::Quota, id_bytes))
            .cloned()
            .ok_or(ClockHorizonError::UnknownLeaseHorizon {
                family: CrossHostLeaseFamily::Quota,
                lease_id: id_bytes,
            })?;
        let _ = rec
            .policy
            .validate_and_compute_holder_budget(new_ttl_grant_ms)?;
        self.observe_grantor_mono(now_grantor_mono)
            .map_err(ClockHorizonError::ClockViolation)?;
        if now_grantor_mono < rec.grantor_commit_mono {
            return Err(ClockHorizonError::ClockViolation(
                QuarantinedClockViolation::LocalMonotonicRollback {
                    previous: rec.grantor_commit_mono,
                    observed: now_grantor_mono,
                },
            ));
        }

        let mut updated = rec;
        updated.grantor_commit_mono = now_grantor_mono;
        updated.ttl_grant_ms = new_ttl_grant_ms;
        updated.authority_expiry_tick = new_authority_expiry_tick;
        updated.renewal_seq = updated
            .renewal_seq
            .checked_add(1)
            .ok_or(ClockHorizonError::HorizonArithmeticOverflow)?;
        if let Some(store) = &self.store {
            store.persist_cross_host_horizon_update(&updated.to_stored_row())?;
        }
        self.horizons
            .insert((CrossHostLeaseFamily::Quota, id_bytes), updated.clone());
        Ok((existing_lease, updated))
    }

    /// Reports monotonic usage `high_water` for `lease_id` after verifying
    /// `caller_cell == lease.cell()`.
    ///
    /// # Errors
    ///
    /// Fails closed on cross-Cell impersonation, stale `CellFence`, regression,
    /// or `high_water > face_value`.
    pub fn report_quota_high_water_for_holder(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
        caller_cell: CellIdentity,
        high_water: u64,
    ) -> Result<QuotaLeaseGrant, ClockHorizonError> {
        let existing = self
            .quota
            .query(lease_id)
            .map_err(ClockHorizonError::QuotaLedger)?;
        if existing.cell() != caller_cell {
            return Err(ClockHorizonError::HolderCellImpersonation {
                caller_cell,
                expected_holder_cell: existing.cell(),
            });
        }
        self.quota
            .report_usage(presented, lease_id, high_water)
            .map_err(ClockHorizonError::QuotaLedger)
    }

    /// Grants and prepares (`GLOBAL_RESERVED -> TARGET_PREPARED`) a cross-host
    /// `CapacityLease` bound to `params`.
    ///
    /// # Errors
    ///
    /// Fails closed on invalid `ttl_grant_ms`, stale `CellFence`, insufficient
    /// capacity pool, or conflicting parameters.
    pub fn grant_capacity_cross_host(
        &mut self,
        presented: &CellFence,
        capacity_lease_id: CapacityLeaseId,
        amount: u64,
        params: CrossHostGrantParams,
    ) -> Result<(CapacityLeaseGrant, GrantorHorizonRecord), ClockHorizonError> {
        Self::verify_grant_params(&params)?;
        let id_bytes = *capacity_lease_id.as_bytes();
        if let Some(existing_rec) = self
            .horizons
            .get(&(CrossHostLeaseFamily::Capacity, id_bytes))
            .cloned()
        {
            let existing_lease = self
                .capacity
                .query(capacity_lease_id)
                .map_err(ClockHorizonError::CapacityReturn)?;
            if existing_lease.amount() == amount
                && existing_rec.holder_cell == params.holder_cell
                && existing_rec.holder_boot_generation == params.holder_boot_generation
                && existing_rec.ttl_grant_ms == params.ttl_grant_ms
                && existing_rec.policy == params.policy
                && existing_rec.multi_endpoint_scope == params.multi_endpoint_scope
            {
                return Ok((existing_lease, existing_rec));
            }
            return Err(ClockHorizonError::ConflictingCrossHostGrant(
                "conflicting CapacityLease grant parameters for existing capacity_lease_id",
            ));
        }

        self.observe_grantor_mono(params.grantor_commit_mono)
            .map_err(ClockHorizonError::ClockViolation)?;
        let _reserved = self
            .capacity
            .grant_for_holder(
                presented,
                params.holder_cell,
                params.holder_boot_generation,
                capacity_lease_id,
                amount,
            )
            .map_err(ClockHorizonError::CapacityGrant)?;
        let prepared = self
            .capacity
            .prepare(presented, capacity_lease_id)
            .map_err(ClockHorizonError::CapacityReturn)?;

        let record = GrantorHorizonRecord {
            family: CrossHostLeaseFamily::Capacity,
            lease_id: id_bytes,
            holder_cell: params.holder_cell,
            holder_boot_generation: params.holder_boot_generation,
            grantor_commit_mono: params.grantor_commit_mono,
            ttl_grant_ms: params.ttl_grant_ms,
            policy: params.policy,
            authority_expiry_tick: params.authority_expiry_tick,
            renewal_seq: 0,
            multi_endpoint_scope: params.multi_endpoint_scope,
            disposition: QuarantineDisposition::ActiveOrSettled,
        };
        if let Some(store) = &self.store {
            store.persist_cross_host_horizon(&record.to_stored_row())?;
        }
        self.horizons
            .insert((CrossHostLeaseFamily::Capacity, id_bytes), record.clone());
        Ok((prepared, record))
    }

    /// Attaches a remote target host (`TARGET_PREPARED -> ACTIVE`) after
    /// verifying `caller_cell == lease.target_node()`.
    ///
    /// # Errors
    ///
    /// Fails closed on cross-Cell impersonation, stale `CellFence`, or receipt
    /// binding mismatch.
    pub fn attach_capacity_for_holder(
        &mut self,
        presented: &CellFence,
        caller_cell: CellIdentity,
        receipt: HostAttachReceipt,
    ) -> Result<CapacityLeaseGrant, ClockHorizonError> {
        let existing = self
            .capacity
            .query(receipt.capacity_lease_id())
            .map_err(ClockHorizonError::CapacityReturn)?;
        if existing.target_node() != caller_cell {
            return Err(ClockHorizonError::HolderCellImpersonation {
                caller_cell,
                expected_holder_cell: existing.target_node(),
            });
        }
        self.capacity
            .activate(presented, receipt)
            .map_err(ClockHorizonError::CapacityReturn)
    }

    /// Cleanly returns a remote `CapacityLease` (`RETURNING -> RETURNED`) after
    /// verifying `caller_cell == lease.target_node()`.
    ///
    /// Refuses `QUARANTINED` or `FENCED` leases, which require reconciliation.
    ///
    /// # Errors
    ///
    /// Fails closed on cross-Cell impersonation, stale `CellFence`, or
    /// `QUARANTINED`/`FENCED` state.
    pub fn return_capacity_for_holder(
        &mut self,
        presented: &CellFence,
        capacity_lease_id: CapacityLeaseId,
        caller_cell: CellIdentity,
    ) -> Result<CapacityLeaseGrant, ClockHorizonError> {
        let existing = self
            .capacity
            .query(capacity_lease_id)
            .map_err(ClockHorizonError::CapacityReturn)?;
        if existing.target_node() != caller_cell {
            return Err(ClockHorizonError::HolderCellImpersonation {
                caller_cell,
                expected_holder_cell: existing.target_node(),
            });
        }
        if existing.state() != CapacityLeaseState::Returned {
            let _ = self
                .capacity
                .begin_return(presented, capacity_lease_id)
                .map_err(ClockHorizonError::CapacityReturn)?;
        }
        self.capacity
            .ack_return(presented, capacity_lease_id)
            .map_err(ClockHorizonError::CapacityReturn)
    }

    /// Grants a cross-host `ExclusiveDeviceLease` on `device_id` bound to `params`.
    ///
    /// # Errors
    ///
    /// Fails closed on invalid `ttl_grant_ms`, unknown `device_id`, occupied
    /// device head (`!is_free`), stale `CellFence`, or conflicting parameters.
    pub fn grant_device_cross_host(
        &mut self,
        presented: &CellFence,
        device_id: DeviceId,
        device_lease_id: ExclusiveDeviceLeaseId,
        params: CrossHostGrantParams,
    ) -> Result<(ExclusiveDeviceLeaseGrant, GrantorHorizonRecord), ClockHorizonError> {
        Self::verify_grant_params(&params)?;
        if !self.devices.contains_key(&device_id) {
            return Err(ClockHorizonError::UnknownDeviceHead(device_id));
        }
        let id_bytes = *device_lease_id.as_bytes();
        if let Some(existing_rec) = self
            .horizons
            .get(&(CrossHostLeaseFamily::ExclusiveDevice, id_bytes))
            .cloned()
        {
            let existing_lease = self
                .devices
                .get(&device_id)
                .ok_or(ClockHorizonError::UnknownDeviceHead(device_id))?
                .query(device_lease_id)
                .map_err(ClockHorizonError::DeviceReturn)?;
            if existing_lease.device_id() == device_id
                && existing_rec.holder_cell == params.holder_cell
                && existing_rec.holder_boot_generation == params.holder_boot_generation
                && existing_rec.ttl_grant_ms == params.ttl_grant_ms
                && existing_rec.policy == params.policy
                && existing_rec.multi_endpoint_scope == params.multi_endpoint_scope
            {
                return Ok((existing_lease, existing_rec));
            }
            return Err(ClockHorizonError::ConflictingCrossHostGrant(
                "conflicting ExclusiveDeviceLease grant parameters for existing device_lease_id",
            ));
        }

        self.observe_grantor_mono(params.grantor_commit_mono)
            .map_err(ClockHorizonError::ClockViolation)?;
        let grant = self
            .devices
            .get_mut(&device_id)
            .ok_or(ClockHorizonError::UnknownDeviceHead(device_id))?
            .grant_for_holder(
                presented,
                params.holder_cell,
                params.holder_boot_generation,
                device_lease_id,
            )
            .map_err(ClockHorizonError::DeviceGrant)?;

        let record = GrantorHorizonRecord {
            family: CrossHostLeaseFamily::ExclusiveDevice,
            lease_id: id_bytes,
            holder_cell: params.holder_cell,
            holder_boot_generation: params.holder_boot_generation,
            grantor_commit_mono: params.grantor_commit_mono,
            ttl_grant_ms: params.ttl_grant_ms,
            policy: params.policy,
            authority_expiry_tick: params.authority_expiry_tick,
            renewal_seq: 0,
            multi_endpoint_scope: params.multi_endpoint_scope,
            disposition: QuarantineDisposition::ActiveOrSettled,
        };
        if let Some(store) = &self.store {
            store.persist_cross_host_horizon(&record.to_stored_row())?;
        }
        self.horizons.insert(
            (CrossHostLeaseFamily::ExclusiveDevice, id_bytes),
            record.clone(),
        );
        Ok((grant, record))
    }

    /// Submits a reset+zeroization [`DeviceResetReceipt`] from `caller_cell`,
    /// returning the device head to `FREE` and bumping `reset_generation`
    /// (`DEVICE_RESERVED -> RESETTING -> RETURNED`).
    ///
    /// Refuses `QUARANTINED` or `FENCING` device leases, which require full
    /// barrier + zeroization reconciliation.
    ///
    /// # Errors
    ///
    /// Fails closed on cross-Cell impersonation, stale `reset_generation`,
    /// stale `CellFence`, or `QUARANTINED`/`FENCING` state.
    pub fn submit_device_reset_for_holder(
        &mut self,
        presented: &CellFence,
        device_id: DeviceId,
        caller_cell: CellIdentity,
        receipt: DeviceResetReceipt,
    ) -> Result<ExclusiveDeviceLeaseGrant, ClockHorizonError> {
        let grantor = self
            .devices
            .get_mut(&device_id)
            .ok_or(ClockHorizonError::UnknownDeviceHead(device_id))?;
        let existing = grantor
            .query(receipt.device_lease_id())
            .map_err(ClockHorizonError::DeviceReturn)?;
        if existing.holder_node() != caller_cell {
            return Err(ClockHorizonError::HolderCellImpersonation {
                caller_cell,
                expected_holder_cell: existing.holder_node(),
            });
        }
        if existing.state() == ExclusiveDeviceLeaseState::DeviceReserved {
            let _ = grantor
                .declare_reset(presented, receipt.device_lease_id())
                .map_err(ClockHorizonError::DeviceReturn)?;
        }
        let returned = grantor
            .submit_reset_receipt(presented, receipt)
            .map_err(ClockHorizonError::DeviceReturn)?;
        Ok(returned)
    }

    /// Evaluates whether an unreachable holder's lease may be swept at
    /// `now_grantor_mono` (`ADR-0026` §2.3).
    ///
    /// - When `elapsed_grantor_mono <= ttl_grant_ms + max_clock_skew_ms`,
    ///   returns [`GrantorHorizonStatus::StillWithinSkewEnvelope`] and leaves
    ///   the lease untouched.
    /// - Once `elapsed_grantor_mono > ttl_grant_ms + max_clock_skew_ms`, moves
    ///   any unsettled lease strictly into **`QUARANTINED`** with
    ///   [`QuarantineDisposition::LedgerQuarantinedFenceOnly`] — **never**
    ///   auto-refunding `AVAILABLE` / `pool_remaining` / `DeviceLeaseHead(FREE)`.
    ///
    /// # Errors
    ///
    /// Fails closed on unknown lease, stale `CellFence`, grantor monotonic
    /// rollback (after quarantining the lease!), or storage failure.
    pub fn evaluate_unreachable_holder_horizon(
        &mut self,
        presented: &CellFence,
        family: CrossHostLeaseFamily,
        lease_id: [u8; 16],
        now_grantor_mono: MonotonicInstantMs,
    ) -> Result<GrantorHorizonStatus, ClockHorizonError> {
        self.authority.admit(presented)?;
        let rec = self
            .horizons
            .get(&(family, lease_id))
            .cloned()
            .ok_or(ClockHorizonError::UnknownLeaseHorizon { family, lease_id })?;

        if self.is_lease_terminal(family, lease_id)? {
            return Ok(GrantorHorizonStatus::AlreadySettled);
        }

        if let Err(rollback) = self.observe_grantor_mono(now_grantor_mono) {
            let _ = self.quarantine_unsettled_lease_internal(presented, family, lease_id)?;
            return Err(ClockHorizonError::ClockViolation(rollback));
        }
        if now_grantor_mono < rec.grantor_commit_mono {
            let rollback = QuarantinedClockViolation::LocalMonotonicRollback {
                previous: rec.grantor_commit_mono,
                observed: now_grantor_mono,
            };
            let _ = self.quarantine_unsettled_lease_internal(presented, family, lease_id)?;
            return Err(ClockHorizonError::ClockViolation(rollback));
        }

        let wait_floor = rec
            .policy
            .grantor_expiry_wait_floor(rec.ttl_grant_ms)?
            .get();
        let elapsed_ms = now_grantor_mono.get() - rec.grantor_commit_mono.get();
        if elapsed_ms <= wait_floor {
            return Ok(GrantorHorizonStatus::StillWithinSkewEnvelope {
                elapsed_ms,
                required_strictly_greater_than_ms: wait_floor,
                remaining_wait_ms: (wait_floor - elapsed_ms) + 1,
            });
        }

        let disposition = self.quarantine_unsettled_lease_internal(presented, family, lease_id)?;
        Ok(GrantorHorizonStatus::QuarantinedAfterSkewHorizon { disposition })
    }

    /// Sweeps all registered cross-host leases whose conservative wait floor
    /// `ttl_grant_ms + max_clock_skew_ms` has strictly elapsed at
    /// `now_grantor_mono` into **`QUARANTINED`** (`LedgerQuarantinedFenceOnly`).
    ///
    /// Returns the `(family, lease_id)` pairs transitioned to `QUARANTINED` by
    /// this sweep. Never unlocks `AVAILABLE` / `pool_remaining` / `FREE`.
    ///
    /// # Errors
    ///
    /// Fails closed on stale `CellFence`, grantor monotonic rollback, or
    /// durable storage failure.
    pub fn sweep_unreachable_horizons(
        &mut self,
        presented: &CellFence,
        now_grantor_mono: MonotonicInstantMs,
    ) -> Result<Vec<(CrossHostLeaseFamily, [u8; 16])>, ClockHorizonError> {
        self.authority.admit(presented)?;
        let keys: Vec<(CrossHostLeaseFamily, [u8; 16])> = self.horizons.keys().copied().collect();
        let mut quarantined_now = Vec::new();
        for (family, lease_id) in keys {
            if self.is_lease_terminal(family, lease_id)? {
                continue;
            }
            let already_quarantined = match family {
                CrossHostLeaseFamily::Quota => {
                    self.quota
                        .query(QuotaLeaseId::from_bytes(lease_id))
                        .map_err(ClockHorizonError::QuotaLedger)?
                        .state()
                        == QuotaLeaseState::Quarantined
                }
                CrossHostLeaseFamily::Capacity => {
                    self.capacity
                        .query(CapacityLeaseId::from_bytes(lease_id))
                        .map_err(ClockHorizonError::CapacityReturn)?
                        .state()
                        == CapacityLeaseState::Quarantined
                }
                CrossHostLeaseFamily::ExclusiveDevice => {
                    self.find_device_lease(ExclusiveDeviceLeaseId::from_bytes(lease_id))?
                        .state()
                        == ExclusiveDeviceLeaseState::Quarantined
                }
            };
            if already_quarantined {
                continue;
            }
            if matches!(
                self.evaluate_unreachable_holder_horizon(
                    presented,
                    family,
                    lease_id,
                    now_grantor_mono,
                )?,
                GrantorHorizonStatus::QuarantinedAfterSkewHorizon { .. }
            ) {
                quarantined_now.push((family, lease_id));
            }
        }
        Ok(quarantined_now)
    }

    /// Immediately quarantines an unsettled lease upon detecting a clock-skew
    /// or RTT violation (`ceil(rtt / 2) > max_clock_skew`, monotonic rollback,
    /// or `AuthorityClock` anomaly).
    ///
    /// # Errors
    ///
    /// Fails closed on unknown lease, stale `CellFence`, or storage failure.
    pub fn quarantine_on_clock_violation(
        &mut self,
        presented: &CellFence,
        family: CrossHostLeaseFamily,
        lease_id: [u8; 16],
        _violation: &QuarantinedClockViolation,
    ) -> Result<QuarantineDisposition, ClockHorizonError> {
        self.authority.admit(presented)?;
        if !self.horizons.contains_key(&(family, lease_id)) {
            return Err(ClockHorizonError::UnknownLeaseHorizon { family, lease_id });
        }
        if self.is_lease_terminal(family, lease_id)? {
            return self.quarantine_disposition(family, lease_id);
        }
        self.quarantine_unsettled_lease_internal(presented, family, lease_id)
    }

    /// Advances the Cell `(epoch, fencing_token)` and quarantines all unsettled
    /// cross-host `QuotaLease`, `CapacityLease`, and `ExclusiveDeviceLease`
    /// records (`[LEASE-LOSS-001]`) with
    /// [`QuarantineDisposition::LedgerQuarantinedFenceOnly`].
    ///
    /// # Errors
    ///
    /// Fails closed if `CellAuthority` generation is exhausted or durable
    /// persistence fails.
    pub fn advance_epoch_and_quarantine_all(&mut self) -> Result<CellFence, ClockHorizonError> {
        let current_fence = self.authority.fence();
        let keys: Vec<(CrossHostLeaseFamily, [u8; 16])> = self.horizons.keys().copied().collect();

        for (family, lease_id) in &keys {
            if *family != CrossHostLeaseFamily::Quota
                && !self.is_lease_terminal(*family, *lease_id)?
            {
                let _ =
                    self.quarantine_unsettled_lease_internal(&current_fence, *family, *lease_id)?;
            }
        }

        let new_fence = self
            .quota
            .advance_epoch_and_quarantine()
            .map_err(ClockHorizonError::CellAuthority)?;

        for (family, lease_id) in keys {
            if family == CrossHostLeaseFamily::Quota
                && let Ok(q) = self.quota.query(QuotaLeaseId::from_bytes(lease_id))
                && q.state() == QuotaLeaseState::Quarantined
            {
                self.mark_horizon_quarantined_fence_only(family, lease_id)?;
            }
        }
        Ok(new_fence)
    }

    fn is_lease_terminal(
        &self,
        family: CrossHostLeaseFamily,
        lease_id: [u8; 16],
    ) -> Result<bool, ClockHorizonError> {
        match family {
            CrossHostLeaseFamily::Quota => {
                let q = self
                    .quota
                    .query(QuotaLeaseId::from_bytes(lease_id))
                    .map_err(ClockHorizonError::QuotaLedger)?;
                Ok(matches!(
                    q.state(),
                    QuotaLeaseState::Closed | QuotaLeaseState::Cancelled
                ))
            }
            CrossHostLeaseFamily::Capacity => {
                let c = self
                    .capacity
                    .query(CapacityLeaseId::from_bytes(lease_id))
                    .map_err(ClockHorizonError::CapacityReturn)?;
                Ok(c.state() == CapacityLeaseState::Returned)
            }
            CrossHostLeaseFamily::ExclusiveDevice => {
                let d_id = ExclusiveDeviceLeaseId::from_bytes(lease_id);
                let grant = self.find_device_lease(d_id)?;
                Ok(grant.state() == ExclusiveDeviceLeaseState::Returned)
            }
        }
    }

    pub(crate) fn find_device_lease_opt(
        &self,
        device_lease_id: ExclusiveDeviceLeaseId,
    ) -> Option<ExclusiveDeviceLeaseGrant> {
        for grantor in self.devices.values() {
            if let Ok(grant) = grantor.query(device_lease_id) {
                return Some(grant);
            }
        }
        None
    }

    fn find_device_lease(
        &self,
        device_lease_id: ExclusiveDeviceLeaseId,
    ) -> Result<ExclusiveDeviceLeaseGrant, ClockHorizonError> {
        self.find_device_lease_opt(device_lease_id)
            .ok_or(ClockHorizonError::DeviceReturn(
                ExclusiveDeviceLeaseReturnError::UnknownLease,
            ))
    }

    fn find_device_id_for_lease(
        &self,
        device_lease_id: ExclusiveDeviceLeaseId,
    ) -> Result<DeviceId, ClockHorizonError> {
        for (&device_id, grantor) in &self.devices {
            if grantor.query(device_lease_id).is_ok() {
                return Ok(device_id);
            }
        }
        Err(ClockHorizonError::DeviceReturn(
            ExclusiveDeviceLeaseReturnError::UnknownLease,
        ))
    }

    fn quarantine_unsettled_lease_internal(
        &mut self,
        presented: &CellFence,
        family: CrossHostLeaseFamily,
        lease_id: [u8; 16],
    ) -> Result<QuarantineDisposition, ClockHorizonError> {
        match family {
            CrossHostLeaseFamily::Quota => {
                let _ = self
                    .quota
                    .quarantine_lease(presented, QuotaLeaseId::from_bytes(lease_id))
                    .map_err(ClockHorizonError::QuotaLedger)?;
            }
            CrossHostLeaseFamily::Capacity => {
                let _ = self
                    .capacity
                    .quarantine_lease(presented, CapacityLeaseId::from_bytes(lease_id))
                    .map_err(ClockHorizonError::CapacityReturn)?;
            }
            CrossHostLeaseFamily::ExclusiveDevice => {
                let d_lease_id = ExclusiveDeviceLeaseId::from_bytes(lease_id);
                let dev_id = self.find_device_id_for_lease(d_lease_id)?;
                let _ = self
                    .devices
                    .get_mut(&dev_id)
                    .ok_or(ClockHorizonError::UnknownDeviceHead(dev_id))?
                    .quarantine_lease(presented, d_lease_id)
                    .map_err(ClockHorizonError::DeviceReturn)?;
            }
        }
        self.mark_horizon_quarantined_fence_only(family, lease_id)
    }

    fn mark_horizon_quarantined_fence_only(
        &mut self,
        family: CrossHostLeaseFamily,
        lease_id: [u8; 16],
    ) -> Result<QuarantineDisposition, ClockHorizonError> {
        let mut rec = self
            .horizons
            .get(&(family, lease_id))
            .cloned()
            .ok_or(ClockHorizonError::UnknownLeaseHorizon { family, lease_id })?;
        if matches!(
            rec.disposition,
            QuarantineDisposition::PhysicallyCleanedVerified { .. }
        ) {
            return Ok(rec.disposition);
        }
        if rec.disposition != QuarantineDisposition::LedgerQuarantinedFenceOnly {
            rec.disposition = QuarantineDisposition::LedgerQuarantinedFenceOnly;
            if let Some(store) = &self.store {
                store.persist_cross_host_horizon_update(&rec.to_stored_row())?;
            }
            self.horizons.insert((family, lease_id), rec.clone());
        }
        Ok(rec.disposition)
    }

    fn update_horizon_disposition(
        &mut self,
        family: CrossHostLeaseFamily,
        lease_id: [u8; 16],
        next_disposition: QuarantineDisposition,
    ) -> Result<(), ClockHorizonError> {
        let mut rec = self
            .horizons
            .get(&(family, lease_id))
            .cloned()
            .ok_or(ClockHorizonError::UnknownLeaseHorizon { family, lease_id })?;
        if rec.disposition == next_disposition {
            return Ok(());
        }
        rec.disposition = next_disposition;
        if let Some(store) = &self.store {
            store.persist_cross_host_horizon_update(&rec.to_stored_row())?;
        }
        self.horizons.insert((family, lease_id), rec);
        Ok(())
    }

    /// Records a verified gateway [`SignedFenceBarrierAck`] into the durable
    /// [`MultiEndpointFenceScope`] accumulator for `(ack.family, ack.ack.lease_id())`.
    ///
    /// Idempotent when the identical ACK for `gateway_id` is replayed; fails
    /// closed on signature failure, unknown gateway, or conflicting ACK for the
    /// same `gateway_id`.
    ///
    /// # Errors
    ///
    /// Fails closed on signature rejection, unknown lease, gateway outside
    /// `MultiEndpointFenceScope`, or conflicting ACK digest.
    pub fn record_signed_fence_barrier_ack(
        &mut self,
        signed_ack: &SignedFenceBarrierAck,
        verifier: &dyn IdentityPeerVerifier,
        verified_at_ms: u64,
    ) -> Result<FenceScopeCoverageStatus, ClockHorizonError> {
        let _ = signed_ack.verify(verifier, verified_at_ms)?;
        self.record_fence_barrier_ack(signed_ack.family, signed_ack.ack)
    }

    /// Records a [`FenceBarrierAck`] into the durable [`MultiEndpointFenceScope`]
    /// accumulator for `(family, ack.lease_id())`.
    ///
    /// # Errors
    ///
    /// Fails closed if `ack` does not bind the committed lease axes, if
    /// `ack.gateway_id()` is not a member of `MultiEndpointFenceScope`, or if a
    /// conflicting ACK already exists for `ack.gateway_id()`.
    pub fn record_fence_barrier_ack(
        &mut self,
        family: CrossHostLeaseFamily,
        ack: FenceBarrierAck,
    ) -> Result<FenceScopeCoverageStatus, ClockHorizonError> {
        let lease_id = ack.lease_id();
        let rec = self
            .horizons
            .get(&(family, lease_id))
            .cloned()
            .ok_or(ClockHorizonError::UnknownLeaseHorizon { family, lease_id })?;

        if !rec
            .multi_endpoint_scope
            .required_gateways()
            .contains(&ack.gateway_id())
        {
            return Err(ClockHorizonError::InvalidFenceBarrierAck(
                "FenceBarrierAck gateway_id is not in MultiEndpointFenceScope",
            ));
        }

        match family {
            CrossHostLeaseFamily::Quota => {
                let q = self
                    .quota
                    .query(QuotaLeaseId::from_bytes(lease_id))
                    .map_err(ClockHorizonError::QuotaLedger)?;
                verify_quota_fence_ack(&ack, &q, ack.observed_high_water())
                    .map_err(ClockHorizonError::InvalidFenceBarrierAck)?;
                if ack.observed_high_water() > q.face_value()
                    || ack.observed_high_water() < q.spent()
                {
                    return Err(ClockHorizonError::InvalidFenceBarrierAck(
                        "Quota FenceBarrierAck observed_high_water outside [spent, face_value]",
                    ));
                }
            }
            CrossHostLeaseFamily::Capacity => {
                let c = self
                    .capacity
                    .query(CapacityLeaseId::from_bytes(lease_id))
                    .map_err(ClockHorizonError::CapacityReturn)?;
                verify_capacity_fence_ack(&ack, &c)
                    .map_err(ClockHorizonError::InvalidFenceBarrierAck)?;
            }
            CrossHostLeaseFamily::ExclusiveDevice => {
                let d = self.find_device_lease(ExclusiveDeviceLeaseId::from_bytes(lease_id))?;
                verify_device_fence_ack(&ack, &d)
                    .map_err(ClockHorizonError::InvalidFenceBarrierAck)?;
            }
        }

        if let Some(existing) = self
            .barrier_acks
            .get(&(family, lease_id))
            .and_then(|m| m.get(&ack.gateway_id()))
            .copied()
        {
            if existing == ack {
                return self.fence_barrier_coverage(family, lease_id);
            }
            return Err(ClockHorizonError::InvalidFenceBarrierAck(
                "conflicting FenceBarrierAck replayed for gateway_id",
            ));
        }

        if let Some(store) = &self.store {
            store.persist_cross_host_barrier_ack(family.to_stored(), &ack)?;
        }
        self.barrier_acks
            .entry((family, lease_id))
            .or_default()
            .insert(ack.gateway_id(), ack);
        self.fence_barrier_coverage(family, lease_id)
    }

    fn verify_holder_principal_pin(
        &self,
        holder_cell: CellIdentity,
        signer_principal: PrincipalId,
    ) -> Result<(), ClockHorizonError> {
        if let Some(&pinned) = self.pinned_holder_principals.get(&holder_cell)
            && pinned != signer_principal
        {
            return Err(ClockHorizonError::HolderPrincipalMismatch {
                holder_cell,
                expected_principal: pinned,
                presented_principal: signer_principal,
            });
        }
        Ok(())
    }

    /// Path A (`ADR-0026` §2.3): Settles an `ACTIVE`, `CLOSING`, `FENCED`, or
    /// `QUARANTINED` cross-host `QuotaLease` upon verifying an Ed25519-signed
    /// [`SignedLeaseCloseReceipt`] from the holder.
    ///
    /// Idempotent when the identical receipt is replayed; never refunds
    /// `returned_unspent` twice.
    ///
    /// # Errors
    ///
    /// Fails closed on invalid signature, cross-Cell/Principal impersonation,
    /// high-water regression, conservation mismatch, or conflicting receipt.
    #[allow(clippy::too_many_lines)]
    pub fn settle_quota_with_signed_close_receipt(
        &mut self,
        presented: &CellFence,
        receipt: &SignedLeaseCloseReceipt,
        verifier: &dyn IdentityPeerVerifier,
        verified_at_ms: u64,
    ) -> Result<QuotaLeaseGrant, ClockHorizonError> {
        self.authority.admit(presented)?;
        let _verified_key = receipt.verify(verifier, verified_at_ms)?;

        if let Some(committed) = self.close_receipts.get(&receipt.lease_id).copied() {
            if committed == *receipt {
                return self
                    .quota
                    .query(receipt.lease_id)
                    .map_err(ClockHorizonError::QuotaLedger);
            }
            return Err(ClockHorizonError::ConflictingCloseReceipt {
                existing: committed.receipt_id,
                requested: receipt.receipt_id,
            });
        }

        let existing = self
            .quota
            .query(receipt.lease_id)
            .map_err(ClockHorizonError::QuotaLedger)?;
        if receipt.holder_cell != existing.cell() {
            return Err(ClockHorizonError::HolderCellImpersonation {
                caller_cell: receipt.holder_cell,
                expected_holder_cell: existing.cell(),
            });
        }
        self.verify_holder_principal_pin(existing.cell(), receipt.signer_principal_id)?;

        if receipt.holder_boot_generation != existing.node_boot_generation()
            || receipt.epoch != existing.epoch()
            || receipt.fencing_token != existing.fencing_token()
            || receipt.face_value != existing.face_value()
        {
            return Err(ClockHorizonError::QuotaReconcile(
                QuotaReconcileError::ReceiptBindMismatch,
            ));
        }

        let closed = match existing.state() {
            QuotaLeaseState::Active | QuotaLeaseState::Closing => {
                if existing.state() == QuotaLeaseState::Active {
                    let _ = self
                        .quota
                        .report_usage(presented, receipt.lease_id, receipt.final_high_water_spent)
                        .map_err(ClockHorizonError::QuotaLedger)?;
                    let _ = self
                        .quota
                        .begin_close(presented, receipt.lease_id)
                        .map_err(ClockHorizonError::QuotaLedger)?;
                } else if receipt.final_high_water_spent != existing.spent() {
                    return Err(ClockHorizonError::QuotaLedger(
                        QuotaLeaseLedgerError::NewReserveForbidden,
                    ));
                }
                self.quota
                    .ack_close(presented, receipt.lease_id)
                    .map_err(ClockHorizonError::QuotaLedger)?
            }
            QuotaLeaseState::Quarantined | QuotaLeaseState::Fenced => {
                let rec = self
                    .horizons
                    .get(&(CrossHostLeaseFamily::Quota, *receipt.lease_id.as_bytes()))
                    .ok_or(ClockHorizonError::UnknownLeaseHorizon {
                        family: CrossHostLeaseFamily::Quota,
                        lease_id: *receipt.lease_id.as_bytes(),
                    })?;
                let first_gw = rec
                    .multi_endpoint_scope
                    .required_gateways()
                    .iter()
                    .next()
                    .copied()
                    .ok_or(ClockHorizonError::InvalidMultiEndpointScope(
                        "empty MultiEndpointFenceScope",
                    ))?;
                let synthetic_ack =
                    FenceBarrierAck::for_quota(&existing, first_gw, receipt.final_high_water_spent);
                let next_seq =
                    self.quota.reconciliation_receipts(receipt.lease_id).len() as u64 + 1;
                let recon = QuotaReconciliationReceipt::for_lease(
                    receipt.receipt_id,
                    next_seq,
                    &existing,
                    receipt.final_high_water_spent,
                    receipt.returned_unspent,
                    0,
                    synthetic_ack,
                );
                self.quota
                    .reconcile_quarantined(presented, recon)
                    .map_err(ClockHorizonError::QuotaReconcile)?
            }
            state => {
                return Err(ClockHorizonError::QuotaLedger(
                    QuotaLeaseLedgerError::NotActive { state },
                ));
            }
        };

        if let Some(store) = &self.store {
            store.persist_cross_host_close_receipt(&receipt.to_stored_row())?;
        }
        self.close_receipts.insert(receipt.lease_id, *receipt);
        let id_bytes = *receipt.lease_id.as_bytes();
        if let Some(rec) = self.horizons.get(&(CrossHostLeaseFamily::Quota, id_bytes))
            && !matches!(
                rec.disposition,
                QuarantineDisposition::PhysicallyCleanedVerified { .. }
            )
        {
            self.update_horizon_disposition(
                CrossHostLeaseFamily::Quota,
                id_bytes,
                QuarantineDisposition::ActiveOrSettled,
            )?;
        }
        Ok(closed)
    }

    fn require_epoch_advanced_and_full_barrier(
        &self,
        family: CrossHostLeaseFamily,
        lease_id: [u8; 16],
        lease_epoch: CellEpoch,
        lease_token: CellFencingToken,
    ) -> Result<(), ClockHorizonError> {
        let current_epoch = self.authority.epoch();
        let current_token = self.authority.fencing_token();
        if current_epoch <= lease_epoch || current_token <= lease_token {
            return Err(ClockHorizonError::EpochAdvanceRequiredForBarrierReconcile {
                lease_epoch,
                current_epoch,
                lease_token,
                current_token,
            });
        }
        let coverage = self.fence_barrier_coverage(family, lease_id)?;
        if !coverage.is_complete() {
            return Err(ClockHorizonError::IncompleteMultiEndpointFenceBarrier {
                acked_count: coverage.acked_count,
                required_count: coverage.required_count,
                missing_gateways: coverage.missing_gateways,
            });
        }
        Ok(())
    }

    /// Path B (`ADR-0026` §2.3): Reconciles a `QUARANTINED` `QuotaLease` once
    /// `(epoch, fencing_token)` have strictly advanced AND **100%** of gateways
    /// in [`MultiEndpointFenceScope`] have durably acknowledged the barrier.
    ///
    /// Preserves [`QuarantineDisposition::LedgerQuarantinedFenceOnly`] unless a
    /// verified [`SignedSupervisorCancellationReceipt`] is supplied (`ADR-0019`
    /// R2.3 #2).
    ///
    /// # Errors
    ///
    /// Fails closed if `(epoch, token)` did not advance, if `< 100%` of
    /// `MultiEndpointFenceScope` gateways acknowledged, if `high_water_spent`
    /// undercounts any gateway's `observed_high_water`, or if conservation fails.
    pub fn reconcile_quarantined_quota_via_barrier(
        &mut self,
        presented: &CellFence,
        receipt: QuotaReconciliationReceipt,
    ) -> Result<QuotaLeaseGrant, ClockHorizonError> {
        self.authority.admit(presented)?;
        let id_bytes = *receipt.lease_id().as_bytes();
        let existing = self
            .quota
            .query(receipt.lease_id())
            .map_err(ClockHorizonError::QuotaLedger)?;
        if existing.state() == QuotaLeaseState::Closed
            && self
                .quota
                .reconciliation_receipts(receipt.lease_id())
                .contains(&receipt)
        {
            return Ok(existing);
        }

        self.require_epoch_advanced_and_full_barrier(
            CrossHostLeaseFamily::Quota,
            id_bytes,
            existing.epoch(),
            existing.fencing_token(),
        )?;

        if let Some(acks) = self
            .barrier_acks
            .get(&(CrossHostLeaseFamily::Quota, id_bytes))
        {
            let max_gw_hw = acks
                .values()
                .map(FenceBarrierAck::observed_high_water)
                .max()
                .unwrap_or(0);
            if receipt.high_water_spent() < max_gw_hw {
                return Err(ClockHorizonError::QuotaReconcile(
                    QuotaReconcileError::HighWaterRegression {
                        reported: receipt.high_water_spent(),
                        committed: max_gw_hw,
                    },
                ));
            }
            if acks.get(&receipt.fence_barrier_ack().gateway_id())
                != Some(&receipt.fence_barrier_ack())
            {
                return Err(ClockHorizonError::InvalidFenceBarrierAck(
                    "QuotaReconciliationReceipt fence_barrier_ack does not match recorded gateway ACK",
                ));
            }
        }

        self.quota
            .reconcile_quarantined(presented, receipt)
            .map_err(ClockHorizonError::QuotaReconcile)
    }

    /// Path B (`ADR-0026` §2.3): Reconciles a `QUARANTINED` `CapacityLease` once
    /// `(epoch, fencing_token)` have strictly advanced, **100%** of
    /// [`MultiEndpointFenceScope`] gateways acknowledged, and a verified
    /// [`CleanupAttestation`] (`holder_processes_terminated == true`) is bound.
    ///
    /// Upgrades [`QuarantineDisposition`] to
    /// [`QuarantineDisposition::PhysicallyCleanedVerified`] (`ADR-0019` R2.3 #2).
    ///
    /// # Errors
    ///
    /// Fails closed on un-advanced epoch, partial gateway barrier coverage, or
    /// missing/invalid `CleanupAttestation`.
    pub fn reconcile_quarantined_capacity_via_barrier(
        &mut self,
        presented: &CellFence,
        receipt: CapacityReconciliationReceipt,
    ) -> Result<CapacityLeaseGrant, ClockHorizonError> {
        self.authority.admit(presented)?;
        let id_bytes = *receipt.capacity_lease_id().as_bytes();
        let existing = self
            .capacity
            .query(receipt.capacity_lease_id())
            .map_err(ClockHorizonError::CapacityReturn)?;
        if existing.state() == CapacityLeaseState::Returned
            && self
                .capacity
                .reconciliation_receipt(receipt.capacity_lease_id())
                == Some(receipt)
        {
            return Ok(existing);
        }

        self.require_epoch_advanced_and_full_barrier(
            CrossHostLeaseFamily::Capacity,
            id_bytes,
            existing.capacity_epoch(),
            existing.fencing_token(),
        )?;

        if let Some(acks) = self
            .barrier_acks
            .get(&(CrossHostLeaseFamily::Capacity, id_bytes))
            && acks.get(&receipt.fence_barrier_ack().gateway_id())
                != Some(&receipt.fence_barrier_ack())
        {
            return Err(ClockHorizonError::InvalidFenceBarrierAck(
                "CapacityReconciliationReceipt fence_barrier_ack does not match recorded gateway ACK",
            ));
        }

        let returned = self
            .capacity
            .reconcile(presented, receipt)
            .map_err(ClockHorizonError::CapacityReconcile)?;
        self.update_horizon_disposition(
            CrossHostLeaseFamily::Capacity,
            id_bytes,
            QuarantineDisposition::PhysicallyCleanedVerified {
                proof_kind: PhysicalCleanupProofKind::CapacityCleanupAttestation,
                proof_digest: receipt.cleanup_attestation().attestation_digest(),
            },
        )?;
        Ok(returned)
    }

    /// Path B (`ADR-0026` §2.3): Reconciles a `QUARANTINED`
    /// `ExclusiveDeviceLease` once `(epoch, fencing_token)` have strictly
    /// advanced, **100%** of [`MultiEndpointFenceScope`] gateways acknowledged,
    /// and [`DeviceReconciliationReceipt`] proves queue drain + VRAM/secret
    /// zeroization + [`DeviceResetReceipt`].
    ///
    /// Upgrades [`QuarantineDisposition`] to
    /// [`QuarantineDisposition::PhysicallyCleanedVerified`] (`ADR-0019` R2.3 #2).
    ///
    /// # Errors
    ///
    /// Fails closed on un-advanced epoch, partial gateway barrier coverage, or
    /// missing zeroization / reset proof.
    pub fn reconcile_quarantined_device_via_barrier(
        &mut self,
        presented: &CellFence,
        receipt: DeviceReconciliationReceipt,
    ) -> Result<ExclusiveDeviceLeaseGrant, ClockHorizonError> {
        self.authority.admit(presented)?;
        let id_bytes = *receipt.device_lease_id().as_bytes();
        let dev_id = receipt.device_id();
        let existing = self
            .devices
            .get(&dev_id)
            .ok_or(ClockHorizonError::UnknownDeviceHead(dev_id))?
            .query(receipt.device_lease_id())
            .map_err(ClockHorizonError::DeviceReturn)?;
        if existing.state() == ExclusiveDeviceLeaseState::Returned
            && self
                .devices
                .get(&dev_id)
                .and_then(|g| g.reconciliation_receipt(receipt.device_lease_id()))
                == Some(receipt)
        {
            return Ok(existing);
        }

        self.require_epoch_advanced_and_full_barrier(
            CrossHostLeaseFamily::ExclusiveDevice,
            id_bytes,
            existing.exclusivity_epoch(),
            existing.fencing_token(),
        )?;

        if let Some(acks) = self
            .barrier_acks
            .get(&(CrossHostLeaseFamily::ExclusiveDevice, id_bytes))
            && acks.get(&receipt.fence_barrier_ack().gateway_id())
                != Some(&receipt.fence_barrier_ack())
        {
            return Err(ClockHorizonError::InvalidFenceBarrierAck(
                "DeviceReconciliationReceipt fence_barrier_ack does not match recorded gateway ACK",
            ));
        }

        let returned = self
            .devices
            .get_mut(&dev_id)
            .ok_or(ClockHorizonError::UnknownDeviceHead(dev_id))?
            .reconcile(presented, receipt)
            .map_err(ClockHorizonError::DeviceReconcile)?;

        let mut proof_hasher = Sha256::new();
        proof_hasher.update(b"llmos/lease/device-zeroization-proof/v1");
        proof_hasher.update(receipt.receipt_id().as_bytes());
        proof_hasher.update(receipt.reset_receipt().receipt_id().as_bytes());
        proof_hasher.update(receipt.device_lease_id().as_bytes());
        let proof_digest: [u8; 32] = proof_hasher.finalize().into();

        self.update_horizon_disposition(
            CrossHostLeaseFamily::ExclusiveDevice,
            id_bytes,
            QuarantineDisposition::PhysicallyCleanedVerified {
                proof_kind: PhysicalCleanupProofKind::DeviceResetZeroization,
                proof_digest,
            },
        )?;
        Ok(returned)
    }

    /// Records a verified [`SignedSupervisorCancellationReceipt`] upgrading
    /// [`QuarantineDisposition::LedgerQuarantinedFenceOnly`] to
    /// [`QuarantineDisposition::PhysicallyCleanedVerified`] (`ADR-0019` R2.3 #2).
    ///
    /// # Errors
    ///
    /// Fails closed if `verifier` rejects the supervisor signature or the
    /// receipt axes do not bind the committed lease.
    pub fn record_supervisor_physical_cancellation(
        &mut self,
        receipt: &SignedSupervisorCancellationReceipt,
        verifier: &dyn IdentityPeerVerifier,
        verified_at_ms: u64,
    ) -> Result<QuarantineDisposition, ClockHorizonError> {
        let _ = receipt.verify(verifier, verified_at_ms)?;
        let rec = self
            .horizons
            .get(&(receipt.family, receipt.lease_id))
            .cloned()
            .ok_or(ClockHorizonError::UnknownLeaseHorizon {
                family: receipt.family,
                lease_id: receipt.lease_id,
            })?;
        if receipt.holder_cell != rec.holder_cell
            || receipt.holder_boot_generation != rec.holder_boot_generation
        {
            return Err(ClockHorizonError::HolderCellImpersonation {
                caller_cell: receipt.holder_cell,
                expected_holder_cell: rec.holder_cell,
            });
        }
        let next = QuarantineDisposition::PhysicallyCleanedVerified {
            proof_kind: PhysicalCleanupProofKind::SignedSupervisorCancellation,
            proof_digest: receipt.signing_digest(),
        };
        self.update_horizon_disposition(receipt.family, receipt.lease_id, next)?;
        Ok(next)
    }
}
