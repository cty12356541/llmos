//! IP-agnostic nominal `CellIdentity` endpoint resolution guarded by monotonic
//! `(node_boot_generation, epoch, heartbeat_seq)` routing watermarks
//! (`[MODEL-ID-002]`, `[DIST-NAME-001]`, `ADR-0026` §2.2 & §2.3).
//!
//! `SocketAddr` (`IP:port`) is strictly an ephemeral routing hint that may
//! rotate across DHCP renewals, NAT rebindings, or process restarts. It NEVER
//! defines or overrides [`CellIdentity`], and out-of-order / replayed routing
//! hints cannot overwrite newer registrations.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Mutex, PoisonError};

use nlos_cell::{CellEpoch, CellIdentity};
use nlos_types::{Generation, PrincipalId};

/// Strictly ordered monotonic version tuple guarding ephemeral `SocketAddr`
/// routing hints (`ADR-0026` §2.2 & §2.3 item 4).
///
/// Lexicographic order: `(node_boot_generation, epoch, heartbeat_seq)`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EndpointRoutingVersion {
    /// Monotonic process boot incarnation (`Generation >= 1`).
    pub node_boot_generation: Generation,
    /// Monotonic Cell control-plane epoch (`CellEpoch >= 1`).
    pub epoch: CellEpoch,
    /// Monotonic per-boot/epoch heartbeat sequence (`>= 1`).
    pub heartbeat_seq: u64,
}

impl EndpointRoutingVersion {
    /// Constructs a routing watermark tuple.
    #[must_use]
    pub const fn new(
        node_boot_generation: Generation,
        epoch: CellEpoch,
        heartbeat_seq: u64,
    ) -> Self {
        Self {
            node_boot_generation,
            epoch,
            heartbeat_seq,
        }
    }
}

/// Ephemeral routing hint mapping an opaque [`CellIdentity`] to its latest
/// observed [`SocketAddr`] (`[DIST-NAME-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EndpointHintRecord {
    /// Opaque 16-byte nominal Cell identity (never encodes IP or port).
    pub cell: CellIdentity,
    /// Ephemeral transport address hint (`IPv4` or `IPv6` + port).
    pub addr_hint: SocketAddr,
    /// Monotonic routing version guarding against stale gossip overwrite.
    pub version: EndpointRoutingVersion,
    /// Receiver's local monotonic clock reading (`ns`) when this hint was
    /// observed (`ADR-0026` §2.3: never remote wall-clock `SystemTime`).
    pub observed_at_mono_ns: u64,
    /// Optional authenticated `PrincipalId` pinned for `cell`, preventing
    /// sibling-node `CellIdentity` masquerade across DHCP/IP reassignment.
    pub expected_principal: Option<PrincipalId>,
}

impl EndpointHintRecord {
    /// Builds a new [`EndpointHintRecord`] without an explicit `PrincipalId` pin.
    #[must_use]
    pub const fn new(
        cell: CellIdentity,
        addr_hint: SocketAddr,
        version: EndpointRoutingVersion,
        observed_at_mono_ns: u64,
    ) -> Self {
        Self {
            cell,
            addr_hint,
            version,
            observed_at_mono_ns,
            expected_principal: None,
        }
    }

    /// Binds an authoritative `PrincipalId` expectation onto this routing hint.
    #[must_use]
    pub const fn with_expected_principal(mut self, principal: PrincipalId) -> Self {
        self.expected_principal = Some(principal);
        self
    }
}

/// Outcome of a valid [`EndpointResolver::upsert_hint`] update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointUpdateDecision {
    /// First routing hint recorded for this `CellIdentity`.
    Inserted,
    /// Newer `EndpointRoutingVersion` replaced the prior hint (including DHCP
    /// IP or port rotation).
    Advanced {
        /// Prior socket address hint.
        previous_addr: SocketAddr,
        /// Prior routing version.
        previous_version: EndpointRoutingVersion,
    },
    /// Exact idempotent replay of the current `(version, addr_hint)`.
    IdempotentReplay,
}

/// Fail-closed rejection when an out-of-order, replayed, or conflicting routing
/// hint attempts to overwrite a newer or equal [`EndpointRoutingVersion`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EndpointStaleError {
    /// Target Cell identity.
    pub cell: CellIdentity,
    /// Currently authoritative routing watermark in the resolver.
    pub existing_version: EndpointRoutingVersion,
    /// Currently recorded socket address hint.
    pub existing_addr: SocketAddr,
    /// Stale or conflicting watermark presented by the caller.
    pub presented_version: EndpointRoutingVersion,
    /// Socket address hint carried on the rejected update.
    pub presented_addr: SocketAddr,
}

impl fmt::Display for EndpointStaleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "stale or conflicting endpoint routing hint for {:?}: presented ({:?} -> {}) <= existing ({:?} -> {})",
            self.cell,
            self.presented_version,
            self.presented_addr,
            self.existing_version,
            self.existing_addr
        )
    }
}

impl Error for EndpointStaleError {}

/// Transport-neutral resolver mapping nominal [`CellIdentity`] to ephemeral
/// [`EndpointHintRecord`]s (`[DIST-NAME-001]`).
pub trait EndpointResolver: Send + Sync {
    /// Resolves the current routing hint for `cell`, if known.
    fn resolve(&self, cell: CellIdentity) -> Option<EndpointHintRecord>;

    /// Inserts or advances the routing hint for `hint.cell`.
    ///
    /// # Errors
    ///
    /// Returns [`EndpointStaleError`] if `hint.version < existing.version`, if
    /// `hint.version.heartbeat_seq == 0`, or if `hint.version == existing.version`
    /// with a conflicting `addr_hint` or regressing `observed_at_mono_ns`.
    #[allow(clippy::result_large_err)]
    fn upsert_hint(
        &self,
        hint: EndpointHintRecord,
    ) -> Result<EndpointUpdateDecision, EndpointStaleError>;

    /// Evicts a cached hint ONLY if its version is `<= max_version`, preventing
    /// slow failure-detector callbacks from erasing a freshly rotated hint.
    fn invalidate_if_version_at_most(
        &self,
        cell: CellIdentity,
        max_version: EndpointRoutingVersion,
    ) -> bool;

    /// Returns a deterministic snapshot sorted by `CellIdentity`.
    fn snapshot(&self) -> Vec<EndpointHintRecord>;
}

/// Thread-safe in-memory [`EndpointResolver`] enforcing strict `(node_boot_generation,
/// epoch, heartbeat_seq)` monotonicity (`[DIST-NAME-001]`).
#[derive(Debug, Default)]
pub struct MonotonicEndpointResolver {
    entries: Mutex<BTreeMap<CellIdentity, EndpointHintRecord>>,
}

impl MonotonicEndpointResolver {
    /// Creates an empty [`MonotonicEndpointResolver`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl EndpointResolver for MonotonicEndpointResolver {
    fn resolve(&self, cell: CellIdentity) -> Option<EndpointHintRecord> {
        let guard = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        guard.get(&cell).copied()
    }

    #[allow(clippy::result_large_err)]
    fn upsert_hint(
        &self,
        hint: EndpointHintRecord,
    ) -> Result<EndpointUpdateDecision, EndpointStaleError> {
        let mut guard = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(&existing) = guard.get(&hint.cell) {
            if let (Some(exp_existing), Some(exp_presented)) =
                (existing.expected_principal, hint.expected_principal)
                && exp_existing != exp_presented
            {
                return Err(EndpointStaleError {
                    cell: hint.cell,
                    existing_version: existing.version,
                    existing_addr: existing.addr_hint,
                    presented_version: hint.version,
                    presented_addr: hint.addr_hint,
                });
            }
            if hint.version < existing.version {
                return Err(EndpointStaleError {
                    cell: hint.cell,
                    existing_version: existing.version,
                    existing_addr: existing.addr_hint,
                    presented_version: hint.version,
                    presented_addr: hint.addr_hint,
                });
            }
            if hint.version == existing.version {
                if hint.addr_hint == existing.addr_hint
                    && hint.observed_at_mono_ns >= existing.observed_at_mono_ns
                {
                    let mut merged = hint;
                    if merged.expected_principal.is_none() {
                        merged.expected_principal = existing.expected_principal;
                    }
                    guard.insert(hint.cell, merged);
                    return Ok(EndpointUpdateDecision::IdempotentReplay);
                }
                return Err(EndpointStaleError {
                    cell: hint.cell,
                    existing_version: existing.version,
                    existing_addr: existing.addr_hint,
                    presented_version: hint.version,
                    presented_addr: hint.addr_hint,
                });
            }
            let mut merged = hint;
            if merged.expected_principal.is_none() {
                merged.expected_principal = existing.expected_principal;
            }
            guard.insert(hint.cell, merged);
            return Ok(EndpointUpdateDecision::Advanced {
                previous_addr: existing.addr_hint,
                previous_version: existing.version,
            });
        }

        if hint.version.heartbeat_seq == 0 {
            return Err(EndpointStaleError {
                cell: hint.cell,
                existing_version: EndpointRoutingVersion::new(
                    hint.version.node_boot_generation,
                    hint.version.epoch,
                    1,
                ),
                existing_addr: hint.addr_hint,
                presented_version: hint.version,
                presented_addr: hint.addr_hint,
            });
        }
        guard.insert(hint.cell, hint);
        Ok(EndpointUpdateDecision::Inserted)
    }

    fn invalidate_if_version_at_most(
        &self,
        cell: CellIdentity,
        max_version: EndpointRoutingVersion,
    ) -> bool {
        let mut guard = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(&existing) = guard.get(&cell)
            && existing.version <= max_version
        {
            guard.remove(&cell);
            return true;
        }
        false
    }

    fn snapshot(&self) -> Vec<EndpointHintRecord> {
        let guard = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        guard.values().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use nlos_types::SchedulerDomainId;

    use super::*;

    #[test]
    fn conditional_invalidation_protects_freshly_rotated_routing_hints() {
        let cell = CellIdentity::from_domain(SchedulerDomainId::from_bytes([0xaa; 16]));
        let v1 = EndpointRoutingVersion::new(
            Generation::new(NonZeroU64::new(1).expect("nz")),
            CellEpoch::from_u64(1).expect("nz"),
            1,
        );
        let v2 = EndpointRoutingVersion::new(
            Generation::new(NonZeroU64::new(1).expect("nz")),
            CellEpoch::from_u64(2).expect("nz"),
            1,
        );
        let addr1: SocketAddr = "127.0.0.1:19001".parse().expect("addr");
        let addr2: SocketAddr = "127.0.0.1:19002".parse().expect("addr");

        let p_owner = PrincipalId::from_bytes([0x11; 16]);
        let p_imposter = PrincipalId::from_bytes([0x22; 16]);
        let resolver = MonotonicEndpointResolver::new();
        resolver
            .upsert_hint(
                EndpointHintRecord::new(cell, addr1, v1, 10).with_expected_principal(p_owner),
            )
            .expect("insert v1");
        // Conflicting PrincipalId for the same CellIdentity MUST fail closed even at higher version!
        assert!(
            resolver
                .upsert_hint(
                    EndpointHintRecord::new(cell, addr2, v2, 15)
                        .with_expected_principal(p_imposter),
                )
                .is_err()
        );
        resolver
            .upsert_hint(EndpointHintRecord::new(cell, addr2, v2, 20))
            .expect("advance v2 preserving pinned principal");
        assert_eq!(
            resolver.resolve(cell).expect("present").expected_principal,
            Some(p_owner)
        );

        // Stale failure-detector callback at v1 MUST NOT evict v2!
        assert!(!resolver.invalidate_if_version_at_most(cell, v1));
        assert_eq!(
            resolver.resolve(cell).expect("still present").addr_hint,
            addr2
        );

        // Current-version eviction at v2 succeeds.
        assert!(resolver.invalidate_if_version_at_most(cell, v2));
        assert!(resolver.resolve(cell).is_none());
    }
}
